//! Grok (xAI) usage fetcher — OAuth bearer from the opencode store, gRPC-web POST.
//!
//! Auth: the opencode store `xai` entry is an OAuth token (type/refresh/access/
//! expires — like claude, NOT an inference api-key), so we reuse `opencode_auth`
//! exactly as anthropic does. LIVE-PROVEN: probing the real endpoint with this token
//! returns a real window.
//!
//! Fetch: `POST https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig`
//! as gRPC-web (`application/grpc-web+proto`, an empty 5-byte message frame). The
//! response is gRPC-web framed protobuf (NOT JSON) with no `.proto` schema available,
//! so — exactly like CodexBar — we scan the protobuf wire format generically: the
//! utilization percent is a `fixed32` float at the shallowest field path ending in
//! `1` within 0..=100, and the billing-period reset is a varint epoch-seconds at
//! path `[1,5,1]` (preferred) else the earliest future epoch varint.
//!
//! VERIFICATION: grok is LIVE-VERIFIED — the real gRPC-web fetch + protobuf decode
//! was proven end-to-end (HTTP 200, grpc-status:0, decoded usedPercent + a real
//! 2026-07-01 reset). The decode is a MINIMAL hand-rolled wire scan (read varints +
//! fixed32, track field paths) — deliberately NOT a prost/protobuf-codegen dependency
//! for what is a schema-less generic scan. Request shape + the percent/reset
//! selection heuristic are ported from CodexBar
//! `Sources/CodexBarCore/Providers/Grok/GrokWebBillingFetcher.swift:50-122,159-219`
//! (grpc-web framing, `scanProtobuf`, percent = shallowest `path.last==1` fixed32 in
//! 0..100, reset = future varint preferring path `[1,5,1]`). `tests/grok_live.rs`
//! is the ignored live proof; the unit test below decodes a REAL captured wire frame.

use std::time::Duration;

use async_trait::async_trait;

use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    opencode_auth::{self, OpencodeAuth},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "grok";
const OPENCODE_PROVIDER: &str = "xai";
const USAGE_URL: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
const CONTENT_TYPE: &str = "application/grpc-web+proto";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// gRPC-web epoch sanity window (CodexBar `:179`): only varints in this range are
/// treated as reset timestamps, so unrelated counters never look like dates.
const EPOCH_MIN: u64 = 1_700_000_000;
const EPOCH_MAX: u64 = 2_100_000_000;

// ---- minimal protobuf wire scan (schema-less) -------------------------------

/// A scalar field discovered by the scan, tagged with its nested field-number path.
struct Fixed32Field {
    path: Vec<u64>,
    value: f32,
    order: usize,
}
struct VarintField {
    path: Vec<u64>,
    value: u64,
}

#[derive(Default)]
struct Scan {
    fixed32: Vec<Fixed32Field>,
    varints: Vec<VarintField>,
    order: usize,
}

/// Read a base-128 varint, advancing `i`. Returns None on truncation.
fn read_varint(bytes: &[u8], i: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift = 0;
    while *i < bytes.len() {
        let byte = bytes[*i];
        *i += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

/// Recursively scan one protobuf message, recording varint + fixed32 fields with
/// their field-number `path`. Length-delimited fields are tried as sub-messages
/// (the scan simply yields nothing for genuine strings/bytes).
fn scan_message(bytes: &[u8], path: &[u64], scan: &mut Scan) {
    let mut i = 0;
    while i < bytes.len() {
        let field_start = i;
        let Some(key) = read_varint(bytes, &mut i) else {
            break;
        };
        if key == 0 {
            i = field_start + 1;
            continue;
        }
        let field_number = key >> 3;
        let wire_type = key & 0x07;
        let mut field_path = path.to_vec();
        field_path.push(field_number);

        match wire_type {
            0 => {
                // varint
                if let Some(value) = read_varint(bytes, &mut i) {
                    scan.varints.push(VarintField {
                        path: field_path,
                        value,
                    });
                } else {
                    break;
                }
            }
            5 => {
                // 32-bit fixed (interpret as f32, like CodexBar)
                if i + 4 > bytes.len() {
                    break;
                }
                let value =
                    f32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
                scan.order += 1;
                scan.fixed32.push(Fixed32Field {
                    path: field_path,
                    value,
                    order: scan.order,
                });
                i += 4;
            }
            1 => {
                // 64-bit fixed — skip
                if i + 8 > bytes.len() {
                    break;
                }
                i += 8;
            }
            2 => {
                // length-delimited — recurse as a possible sub-message
                let Some(len) = read_varint(bytes, &mut i) else {
                    break;
                };
                let len = len as usize;
                if i + len > bytes.len() {
                    break;
                }
                scan_message(&bytes[i..i + len], &field_path, scan);
                i += len;
            }
            _ => break,
        }
    }
}

/// Split a gRPC-web body into data-frame payloads (flag bit 0x80 clear). Trailer
/// frames (0x80 set) carry `grpc-status` and are returned separately as text.
fn grpc_web_frames(data: &[u8]) -> (Vec<Vec<u8>>, String) {
    let mut data_frames = Vec::new();
    let mut trailer = String::new();
    let mut i = 0;
    while i + 5 <= data.len() {
        let flags = data[i];
        let length = ((data[i + 1] as usize) << 24)
            | ((data[i + 2] as usize) << 16)
            | ((data[i + 3] as usize) << 8)
            | (data[i + 4] as usize);
        let start = i + 5;
        let end = start + length;
        if end > data.len() {
            break;
        }
        if flags & 0x80 == 0 {
            data_frames.push(data[start..end].to_vec());
        } else {
            trailer.push_str(&String::from_utf8_lossy(&data[start..end]));
        }
        i = end;
    }
    (data_frames, trailer)
}

/// `grpc-status` from a trailer block; `Some(0)` is success, non-zero an RPC error.
fn grpc_status(trailer: &str) -> Option<i64> {
    for line in trailer.split(['\r', '\n']) {
        let line = line.trim();
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("grpc-status:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Decode a gRPC-web protobuf billing response to [`Usage`]. Pure — unit-testable
/// against captured real wire bytes.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let (frames, trailer) = grpc_web_frames(body);
    if let Some(status) = grpc_status(&trailer) {
        if status != 0 {
            return Err(FetchError::Upstream(format!("grok grpc-status {status}")));
        }
    }
    if frames.is_empty() {
        return Err(FetchError::Decode(
            "grok: no gRPC-web data frame".to_string(),
        ));
    }

    let mut scan = Scan::default();
    for frame in &frames {
        scan_message(frame, &[], &mut scan);
    }

    // Percent: shallowest fixed32 whose path ends in field 1 and sits in 0..=100
    // (CodexBar `:168-175`).
    let percent = scan
        .fixed32
        .iter()
        .filter(|f| {
            f.path.last() == Some(&1) && f.value.is_finite() && (0.0..=100.0).contains(&f.value)
        })
        .min_by(|a, b| a.path.len().cmp(&b.path.len()).then(a.order.cmp(&b.order)))
        // The wire value is a 32-bit float; widening to f64 exposes its imprecision
        // (e.g. 76.57 → 76.58000183…). Round to 2 dp so the consumer gets a clean
        // percent rather than float noise.
        .map(|f| ((f.value as f64) * 100.0).round() / 100.0);

    // Reset: future epoch varint, preferring the billing-period-end path [1,5,1]
    // (CodexBar `:177-188`).
    let future: Vec<&VarintField> = scan
        .varints
        .iter()
        .filter(|v| (EPOCH_MIN..=EPOCH_MAX).contains(&v.value))
        .collect();
    let reset_epoch = future
        .iter()
        .filter(|v| v.path == [1, 5, 1])
        .map(|v| v.value)
        .min()
        .or_else(|| future.iter().map(|v| v.value).min());

    let Some(reset_epoch) = reset_epoch else {
        // No reset => no well-formed window; degrade rather than emit a bare percent.
        return Err(FetchError::Decode("grok: no reset timestamp".to_string()));
    };
    let resets_at = env::epoch_to_iso8601(reset_epoch as i64)
        .ok_or_else(|| FetchError::Decode("grok: reset epoch out of range".to_string()))?;

    // Window length from the billing period start [1,4,1] when present, else None.
    let window_minutes = scan
        .varints
        .iter()
        .filter(|v| v.path == [1, 4, 1] && (EPOCH_MIN..=EPOCH_MAX).contains(&v.value))
        .map(|v| v.value)
        .min()
        .filter(|&start| reset_epoch > start)
        .map(|start| ((reset_epoch - start) / 60) as i64);

    // Percent may legitimately be absent before any usage (CodexBar treats no-usage
    // as 0); only when there is also a reset (which we have here).
    let used_percent = percent.unwrap_or(0.0);

    Ok(Usage {
        primary: Some(RateWindow {
            used_percent,
            resets_at: Some(resets_at),
            window_minutes,
        }),
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

// ---- provider ---------------------------------------------------------------

/// The Grok usage provider.
pub struct GrokProvider {
    http: reqwest::Client,
}

impl GrokProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for GrokProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for GrokProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let auth = opencode_auth::read_provider(OPENCODE_PROVIDER)
            .map_err(FetchError::NoSession)?
            .ok_or_else(|| {
                FetchError::NoSession("no xai entry in opencode auth.json".to_string())
            })?;
        let access = match auth {
            OpencodeAuth::Oauth { access, .. } => access,
            OpencodeAuth::Api { key } => key,
        };

        // An empty gRPC-web message: a single frame with flags=0 and length=0.
        let frame: Vec<u8> = vec![0, 0, 0, 0, 0];
        let body = JsonRequest::post(USAGE_URL, frame)
            .timeout(REQUEST_TIMEOUT)
            .bearer(&access)
            .header(Header::new("Origin", "https://grok.com"))
            .header(Header::new("Referer", "https://grok.com/?_s=usage"))
            .header(Header::new("Accept", "*/*"))
            .header(Header::new("Content-Type", CONTENT_TYPE))
            .header(Header::new("x-grpc-web", "1"))
            .header(Header::new("x-user-agent", "connect-es/2.1.1"))
            .send(&self.http)
            .await?;

        let usage = normalize_usage(&body)?;
        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "oauth", usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A REAL captured gRPC-web response from GetGrokCreditsConfig (live wire bytes,
    /// base64). Decodes to 76.57% used with a 2026-07-01 billing reset over a 30-day
    /// period (43200 min). This is a historical capture: the live wire later moved to
    /// a 7-day billing period, which the decoder handles automatically because the
    /// window length is derived from the response's own period, not hardcoded — so
    /// this fixture's 43200 proves the derivation, it is not a pinned assumption.
    const LIVE_FIXTURE_B64: &str = "AAAAADwKOg3XI5lCEgAaACIGCICX89AGKgYIgLGR0gY6BwgBFeE6k0JCEggBEgYIgJfz0AYaBgiAsZHSBmIAaAGAAAAAD2dycGMtc3RhdHVzOjANCg==";

    fn decode_b64(s: &str) -> Vec<u8> {
        // Minimal base64 decode for the test fixture.
        const fn val(c: u8) -> i16 {
            match c {
                b'A'..=b'Z' => (c - b'A') as i16,
                b'a'..=b'z' => (c - b'a' + 26) as i16,
                b'0'..=b'9' => (c - b'0' + 52) as i16,
                b'+' => 62,
                b'/' => 63,
                _ => -1,
            }
        }
        let mut out = Vec::new();
        let mut buf: u32 = 0;
        let mut bits = 0;
        for &c in s.as_bytes() {
            if c == b'=' {
                break;
            }
            let v = val(c);
            if v < 0 {
                continue;
            }
            buf = (buf << 6) | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
        out
    }

    #[test]
    fn decodes_real_grok_wire_response() {
        let body = decode_b64(LIVE_FIXTURE_B64);
        let usage = normalize_usage(&body).unwrap();
        let primary = usage.primary.unwrap();
        // fixed32 at path [1,1] = 76.57, rounded to 2dp (no f32→f64 widening noise).
        assert_eq!(primary.used_percent, 76.57);
        // varint at [1,5,1] = 1782864000 = 2026-07-01T00:00:00Z.
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        // period start [1,4,1] = 2026-06-01 → 30-day window = 43200 min.
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn grpc_error_status_degrades() {
        // A trailer-only body with grpc-status:2 and no data frame.
        let trailer = b"grpc-status:2\r\n";
        let mut body = vec![0x80u8, 0, 0, 0, trailer.len() as u8];
        body.extend_from_slice(trailer);
        assert!(matches!(
            normalize_usage(&body),
            Err(FetchError::Upstream(_))
        ));
    }

    #[test]
    fn empty_body_is_decode_error() {
        assert!(matches!(normalize_usage(&[]), Err(FetchError::Decode(_))));
    }
}
