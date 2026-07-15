//! Grok (xAI) usage fetcher — OAuth bearer from the opencode store, gRPC-web POST.
//!
//! Implicit-local auth: the opencode store `xai` entry is an OAuth token
//! (type/refresh/access/expires — like claude, NOT an inference api-key), so we
//! reuse `opencode_auth` exactly as anthropic does. Vault handles use the bare
//! bearer bytes served by the injected credential source. LIVE-PROVEN: probing
//! the real endpoint with this token
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

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;

use crate::credential_source::{CredentialSource, VaultCapability};
use crate::provider::{AccountObservation, CredentialHandle, FetchAttempt};
use crate::vault_handles::VaultHandleLoader;
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{RateWindow, Usage},
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

fn canonical_account_id(account_id: Option<String>) -> Option<String> {
    account_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn usage_request(url: &str, bearer: &str) -> JsonRequest {
    // An empty gRPC-web message: a single frame with flags=0 and length=0.
    let frame: Vec<u8> = vec![0, 0, 0, 0, 0];
    JsonRequest::post(url, frame)
        .timeout(REQUEST_TIMEOUT)
        .bearer(bearer)
        .header(Header::new("Origin", "https://grok.com"))
        .header(Header::new("Referer", "https://grok.com/?_s=usage"))
        .header(Header::new("Accept", "*/*"))
        .header(Header::new("Content-Type", CONTENT_TYPE))
        .header(Header::new("x-grpc-web", "1"))
        .header(Header::new("x-user-agent", "connect-es/2.1.1"))
}

/// The Grok usage provider.
pub struct GrokProvider {
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    usage_url: String,
}

impl GrokProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::from_env()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            credential_source,
            handle_loader,
            usage_url: USAGE_URL.to_string(),
        }
    }

    fn report_auth_failure(
        &self,
        capability: &VaultCapability,
        record_version: u64,
        error: &FetchError,
    ) {
        let FetchError::ProviderStatus(status @ (401 | 403)) = error else {
            return;
        };
        let Some(source) = self.credential_source.as_ref() else {
            return;
        };
        let source = Arc::clone(source);
        let capability = capability.clone();
        let status = *status;
        tokio::spawn(async move {
            source
                .report_auth_failure(&capability, status, record_version)
                .await;
        });
    }

    async fn fetch_local_bearer(&self, bearer: &str) -> FetchAttempt {
        let result = usage_request(&self.usage_url, bearer)
            .send(&self.http)
            .await
            .and_then(|body| normalize_usage(&body));
        match result {
            Ok(usage) => {
                FetchAttempt::success(Some(AccountObservation::new(None, None)), "oauth", usage)
            }
            Err(error) => FetchAttempt::failure(None, None, error),
        }
    }

    async fn fetch_vault(&self, capability: &VaultCapability) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match credential_source.get(capability, 120_000).await {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = credential.record_version;
        let observed = Some(AccountObservation::new(
            canonical_account_id(credential.account_id.clone()),
            Some(record_version),
        ));
        let bearer = match String::from_utf8(std::mem::take(&mut credential.payload)) {
            Ok(bearer) => bearer,
            Err(error) => {
                let mut payload = error.into_bytes();
                payload.fill(0);
                return FetchAttempt::failure(
                    observed,
                    None,
                    FetchError::Decode("vault credential payload is not valid UTF-8".to_string()),
                );
            }
        };

        let result = usage_request(&self.usage_url, &bearer)
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
            .map(|response| response.body)
            .and_then(|body| normalize_usage(&body));
        if let Err(error) = &result {
            self.report_auth_failure(capability, record_version, error);
        }
        match result {
            Ok(usage) => FetchAttempt::success(observed, "vault", usage),
            Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error),
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

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        let mut handles = vec![CredentialHandle::implicit()];
        if self.credential_source.is_some() {
            handles.extend(self.handle_loader.grok_handles()?);
        }
        Ok(handles)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if let Some(capability) = handle.vault_capability() {
            return self.fetch_vault(capability).await;
        }

        let access =
            match opencode_auth::read_provider(OPENCODE_PROVIDER).map_err(FetchError::NoSession) {
                Ok(Some(OpencodeAuth::Oauth { access, .. })) => access,
                Ok(Some(OpencodeAuth::Api { key })) => key,
                Ok(None) => {
                    return FetchAttempt::failure(
                        None,
                        None,
                        FetchError::NoSession("no xai entry in opencode auth.json".to_string()),
                    );
                }
                Err(error) => return FetchAttempt::failure(None, None, error),
            };
        self.fetch_local_bearer(&access).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::credential_source::{VaultCredential, VaultGetError};
    use crate::provider::CredentialResolution;
    use crate::refresh::{next_slot_after_attempt, Incarnation, ProviderSlot};

    type Reports = Arc<Mutex<Vec<(u16, u64)>>>;

    struct MockCredentialSource {
        get_result: Result<VaultCredential, VaultGetError>,
        reports: Reports,
    }

    #[async_trait]
    impl CredentialSource for MockCredentialSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            assert_eq!(min_ttl_ms, 120_000);
            self.get_result.clone()
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            provider_status: u16,
            record_version: u64,
        ) {
            self.reports
                .lock()
                .unwrap()
                .push((provider_status, record_version));
        }
    }

    fn source(
        get_result: Result<VaultCredential, VaultGetError>,
    ) -> (Arc<dyn CredentialSource>, Reports) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(MockCredentialSource {
                get_result,
                reports: Arc::clone(&reports),
            }),
            reports,
        )
    }

    fn credential(payload: &[u8], record_version: u64) -> VaultCredential {
        VaultCredential {
            payload: payload.to_vec(),
            expires_at_ms: None,
            record_version,
            account_id: Some("   ".to_string()),
            project_id: None,
        }
    }

    fn test_provider(source: Arc<dyn CredentialSource>, usage_url: String) -> GrokProvider {
        let mut provider = GrokProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(None)),
        );
        provider.usage_url = usage_url;
        provider
    }

    struct VaultOnlyProvider {
        provider: GrokProvider,
        handle: CredentialHandle,
    }

    #[async_trait]
    impl UsageProvider for VaultOnlyProvider {
        fn name(&self) -> &str {
            PROVIDER_NAME
        }

        fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
            Ok(vec![self.handle.clone()])
        }

        async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
            self.provider.fetch_handle(handle).await
        }
    }

    async fn serve_once(status: u16, body: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_string();
            let reason = if status == 200 { "OK" } else { "Unauthorized" };
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            request
        });
        (format!("http://{address}/usage"), task)
    }

    fn write_handles(body: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("ck-quota-grok-handles-{}.json", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        path
    }

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
    fn handles_include_mapped_vault_entries_when_source_is_wired() {
        let path = write_handles(
            r#"{"handles":{"oauth:xai":"ckh_grok","oauth:anthropic":"ckh_anthropic"}}"#,
        );
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = GrokProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(Some(path.clone()))),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0], CredentialHandle::implicit());
        assert_eq!(handles[1].stable_id(), "oauth:xai");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn vault_happy_path_uses_served_bearer_and_record_version() {
        let (url, request) = serve_once(200, decode_b64(LIVE_FIXTURE_B64)).await;
        let (source, _) = source(Ok(credential(b"grok-vault-token", 31)));
        let provider = test_provider(source, url);
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:xai",
                VaultCapability::new("ckh_grok"),
            ))
            .await;

        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt.observed.unwrap(),
            AccountObservation::new(None, Some(31))
        );
        assert_eq!(attempt.usage.unwrap().primary.unwrap().used_percent, 76.57);
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("authorization: bearer grok-vault-token"));
        assert!(request.contains("content-type: application/grpc-web+proto"));
    }

    #[tokio::test]
    async fn vault_happy_path_serves_one_unlabeled_entry() {
        let (url, _) = serve_once(200, decode_b64(LIVE_FIXTURE_B64)).await;
        let (source, _) = source(Ok(credential(b"grok-vault-token", 32)));
        let handle = CredentialHandle::vault("oauth:xai", VaultCapability::new("ckh_grok"));
        let registry = crate::Registry::new(vec![Box::new(VaultOnlyProvider {
            provider: test_provider(source, url),
            handle,
        })]);

        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let entries = registry.get_usage(Some(PROVIDER_NAME)).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].account, None);
        assert_eq!(
            entries[0]
                .usage
                .as_ref()
                .unwrap()
                .primary
                .as_ref()
                .unwrap()
                .used_percent,
            76.57
        );
    }

    #[tokio::test]
    async fn failed_get_is_unverified_and_clears_prior_observation() {
        let (source, _) = source(Err(VaultGetError::Transient));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:xai",
                VaultCapability::new("ckh_grok"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Unverified
        );

        let now = std::time::Instant::now();
        let cold = ProviderSlot::due_now(now, Incarnation::from_counter(1));
        let prior = next_slot_after_attempt(
            &cold,
            PROVIDER_NAME,
            FetchAttempt::success(
                Some(AccountObservation::new(
                    Some("prior-account".to_string()),
                    Some(1),
                )),
                "vault",
                Usage::default(),
            ),
            now,
            now,
        );
        let next = next_slot_after_attempt(&prior, PROVIDER_NAME, attempt, now, now);
        assert!(next.entry.is_none());
        assert!(next.label_in_flux);
        assert!(next.last_success_at.is_none());
    }

    #[tokio::test]
    async fn vault_401_reports_served_version_while_local_keeps_legacy_error() {
        let (vault_url, _) = serve_once(401, Vec::new()).await;
        let (source, reports) = source(Ok(credential(b"grok-vault-token", 52)));
        let mut provider = test_provider(Arc::clone(&source), vault_url);
        let vault = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:xai",
                VaultCapability::new("ckh_grok"),
            ))
            .await;
        assert!(matches!(vault.usage, Err(FetchError::ProviderStatus(401))));
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 52)]);

        reports.lock().unwrap().clear();
        let (local_url, _) = serve_once(401, Vec::new()).await;
        provider.usage_url = local_url;
        let local = provider.fetch_local_bearer("grok-local-token").await;
        assert!(matches!(
            local.usage,
            Err(FetchError::Unauthorized(message)) if message == "HTTP 401"
        ));
        tokio::task::yield_now().await;
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_utf8_vault_payload_is_a_verified_decode_failure() {
        let (source, _) = source(Ok(credential(&[0xff, 0xfe], 9)));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:xai",
                VaultCapability::new("ckh_grok"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Verified
        );
        assert_eq!(attempt.observed.unwrap().record_version, Some(9));
        assert!(matches!(attempt.usage, Err(FetchError::Decode(_))));
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
