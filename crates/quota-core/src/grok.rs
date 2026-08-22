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
    http::{Header, JsonRequest, EMPTY_BODY_MARKER},
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
                // Length-delimited — recurse as a possible sub-message.
                //
                // The declared length comes off the wire and can be any u64, so
                // every step to the end offset is checked. Casting it to usize and
                // adding unchecked overflows the cursor: with overflow checks on
                // that is an immediate panic, and without them the sum wraps to a
                // value that passes the bounds test below and then panics on a
                // backwards slice. Either way a malformed frame takes down the
                // fetch, and because a fetch panic is classified non-transient it
                // would clear a working provider's cached window and suppress it
                // for the backoff — the provider would read as absent rather than
                // degraded. A length we cannot honour is simply malformed input,
                // so stop scanning and let the caller report a decode failure.
                let Some(len) = read_varint(bytes, &mut i) else {
                    break;
                };
                let Ok(len) = usize::try_from(len) else {
                    break;
                };
                let Some(end) = i.checked_add(len) else {
                    break;
                };
                if end > bytes.len() {
                    break;
                }
                scan_message(&bytes[i..end], &field_path, scan);
                i = end;
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
        // An empty HTTP 200 body (no data frame, no error trailer) is grok's
        // edge-limiter flap under rapid probing — the endpoint returns nothing
        // rather than a malformed payload. Classify it as Upstream (transient)
        // so the refresher serves the last-healthy window stale through the
        // flap instead of replacing a real 98%-used window with a degraded
        // entry (which the router reads as "quota signal: none"). A genuine
        // decode failure (garbled protobuf, missing reset) still degrades.
        return Err(FetchError::Upstream(
            "grok: empty gRPC-web response (transient edge limit)".to_string(),
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
        // DELIBERATE DIVERGENCE from "the percent is load-bearing, the reset is
        // optional", which holds everywhere a percent arrives in a NAMED field.
        // It does not hold here, because this response is an opaque protobuf with
        // no field names: the percent above is identified by shape alone (the
        // shallowest 32-bit float that happens to fall in 0..=100), so any
        // unrelated ratio or score in range can match it.
        //
        // The reset is what confirms the scan found the right message: it is
        // required at the exact path [1,5,1], which a coincidental value will not
        // occupy. Without it there is no evidence the float is a quota percent at
        // all, and emitting it would publish a number of unknown provenance as
        // this account's capacity.
        //
        // The asymmetry with the line below is intentional. A reset WITHOUT a
        // percent still proves the shape, so it yields 0% (no usage recorded); a
        // percent WITHOUT a reset proves nothing. Do not "restore" this window
        // when sweeping the reset-optional rule — the rule assumes an identified
        // percent, which is exactly what is missing here.
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
            raw_used_percent: None,
            resets_at: Some(resets_at),
            window_minutes,
            used_count: None,
            total_count: None,
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
            http: crate::http::provider_client(),
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
        crate::credential_source::report_vault_auth_failure(
            self.credential_source.as_ref(),
            capability,
            record_version,
            error,
        );
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
        let account_info = credential.account_info();
        let observed = Some(AccountObservation::new(
            canonical_account_id(credential.account_id.clone()),
            Some(record_version),
        ));
        let bearer = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(value) => value,
            Err(error) => return FetchAttempt::failure(observed, None, error),
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
            Ok(usage) => {
                FetchAttempt::success(observed, "vault", usage).with_account_info(account_info)
            }
            Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error),
        }
    }
}

/// Re-attribute the one failure this endpoint cannot distinguish on its own.
///
/// An empty HTTP 200 arrives both when the edge is flapping and when the
/// credential is dead, and it is classified transient so a flap keeps serving
/// the last healthy window. That is right when the token is good and wrong when
/// it is not: a dead credential retries forever and never reaches a verdict, so
/// no consumer and no operator is ever told to sign in again.
///
/// The expiry recorded at grant time settles it. It is consulted only here,
/// after the request has been made and only for this one ambiguous response, so
/// a token still working past its stated expiry is unaffected -- every other
/// outcome keeps the attribution the transport gave it.
fn disambiguate_empty_response(attempt: FetchAttempt, expired: bool) -> FetchAttempt {
    if !expired {
        return attempt;
    }
    let is_empty_body = matches!(
        &attempt.usage,
        Err(FetchError::Upstream(message)) if message.contains(EMPTY_BODY_MARKER)
    );
    if !is_empty_body {
        return attempt;
    }
    FetchAttempt::failure(
        attempt.observed,
        attempt.source,
        FetchError::CredentialUnusable(
            "xai token in opencode auth.json expired; the endpoint answers an expired \
             credential with an empty response. Sign in again to refresh it."
                .to_string(),
        ),
    )
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

        let entry = match opencode_auth::read_provider(OPENCODE_PROVIDER) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                return FetchAttempt::failure(
                    None,
                    None,
                    FetchError::NoSession("no xai entry in opencode auth.json".to_string()),
                );
            }
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        let expired = entry.is_expired(opencode_auth::now_ms());
        let access = match &entry {
            OpencodeAuth::Oauth { access, .. } => access.clone(),
            OpencodeAuth::Api { key } => key.clone(),
        };

        let attempt = self.fetch_local_bearer(&access).await;
        disambiguate_empty_response(attempt, expired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::Mutex;

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
            email: None,
            org_name: None,
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

    /// Serve one request and hand back what was sent, at this provider's path.
    ///
    /// Wraps the shared helper so the URL keeps the shape this provider
    /// builds against. The shared reader is used because a single socket read
    /// returns one TCP segment rather than the whole request, which makes any
    /// assertion about what was NOT sent pass without reading it.
    async fn serve_once(status: u16, body: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let (base, task) = crate::loopback::serve_once(status, body).await;
        (format!("{base}/usage"), task)
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
    fn empty_body_is_transient_upstream() {
        // grok's edge limiter returns an empty HTTP 200 (no data frame) under
        // rapid probing. This must classify as Upstream/transient so the
        // refresher serves the last-healthy window stale through the flap,
        // rather than Decode/non-transient which would replace a real 98%-used
        // window with a degraded entry the router reads as "no signal".
        assert!(matches!(normalize_usage(&[]), Err(FetchError::Upstream(_))));
        assert_eq!(
            crate::refresh::classify(&normalize_usage(&[]).unwrap_err()),
            crate::refresh::FetchClass::Transient
        );
    }

    /// Encode a protobuf varint, so a test can choose the value a scan will see.
    fn varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
        out
    }

    /// A frame carrying a percent at `[1,1]` and one varint at `[1,5,1]`, which
    /// is the path this decoder reads a reset timestamp from.
    fn frame_with_percent_and_varint_at_reset_path(percent: f32, value: u64) -> Vec<u8> {
        let mut inner: Vec<u8> = vec![0x0d];
        inner.extend_from_slice(&percent.to_le_bytes());
        let nested = {
            let mut field = vec![0x08];
            field.extend_from_slice(&varint(value));
            field
        };
        inner.push(0x2a);
        inner.push(nested.len() as u8);
        inner.extend_from_slice(&nested);

        let mut payload: Vec<u8> = vec![0x0a, inner.len() as u8];
        payload.extend_from_slice(&inner);
        let mut body: Vec<u8> = vec![0x00, 0x00, 0x00, 0x00, payload.len() as u8];
        body.extend_from_slice(&payload);
        body
    }

    /// This response has no schema, so a reset timestamp is identified by its
    /// field path plus a plausibility window on the value. The window is what
    /// stops an ordinary counter that happens to sit at that path from being
    /// read as a date -- and a counter is far more likely to be small or large
    /// than to land inside a four-hundred-million-second range.
    ///
    /// Driven with a value that violates the window rather than one inside it:
    /// an in-range value exercises the same code and can never show the check
    /// missing. The in-range case is kept as the control, so this cannot pass
    /// by rejecting everything.
    #[test]
    fn a_varint_outside_the_plausible_epoch_window_is_not_read_as_a_reset() {
        // Comfortably past the window: a counter, not a timestamp.
        let implausible = frame_with_percent_and_varint_at_reset_path(50.0, EPOCH_MAX + 1);

        // Pin the premise: the fixture really does put a varint where a reset
        // would be read from, so a rejection below is the window's doing and
        // not a fixture that never carried a candidate.
        let mut scan = Scan::default();
        scan_message(&implausible[5..], &[], &mut scan);
        assert_eq!(
            scan.varints.iter().filter(|v| v.path == [1, 5, 1]).count(),
            1,
            "fixture must carry exactly one varint at the reset path"
        );

        // With no plausible reset the window has no horizon, and this provider
        // deliberately degrades rather than emitting a percent alone.
        assert!(
            normalize_usage(&implausible).is_err(),
            "an implausible varint was accepted as a reset timestamp"
        );

        // Control: the identical shape with a plausible epoch does produce a
        // window, so the rejection above is about the value, not the shape.
        let plausible = frame_with_percent_and_varint_at_reset_path(50.0, EPOCH_MIN + 1);
        let usage = normalize_usage(&plausible).expect("a plausible epoch must decode");
        let primary = usage.primary.expect("a window is emitted");
        assert_eq!(primary.used_percent, 50.0);
        assert!(primary.resets_at.is_some());
    }

    #[test]
    fn a_percent_without_a_reset_degrades_rather_than_emitting_a_window() {
        // Everywhere a percent arrives in a NAMED field, a missing reset is carried
        // as absent and the window is still emitted. This provider is the deliberate
        // exception, and this test exists so a sweep of that rule cannot quietly
        // convert it: the percent here is identified by SHAPE (shallowest fixed32 in
        // 0..=100), so without the reset at its exact path there is no evidence the
        // float is a quota percent rather than any other in-range number.
        //
        // Frame payload: field 1 wiretype 2, wrapping field 1 wiretype 5 = the f32
        // 50.0. That lands a fixed32 at path [1,1], which the percent scan accepts.
        // No varint anywhere, so no reset is found.
        let inner: Vec<u8> = vec![0x0d, 0x00, 0x00, 0x48, 0x42];
        let mut payload: Vec<u8> = vec![0x0a, inner.len() as u8];
        payload.extend_from_slice(&inner);
        let mut body: Vec<u8> = vec![0x00, 0x00, 0x00, 0x00, payload.len() as u8];
        body.extend_from_slice(&payload);

        // Prove the fixture actually carries the percent, so this cannot pass by
        // failing to parse anything at all.
        let mut scan = Scan::default();
        scan_message(&payload, &[], &mut scan);
        assert_eq!(
            scan.fixed32.len(),
            1,
            "fixture must contain exactly one fixed32"
        );
        assert_eq!(scan.fixed32[0].path, vec![1, 1]);
        assert_eq!(scan.fixed32[0].value, 50.0);
        assert!(
            scan.varints.is_empty(),
            "fixture must contain no reset candidate"
        );

        assert!(
            matches!(normalize_usage(&body), Err(FetchError::Decode(_))),
            "a percent of unverified provenance must not reach the wire"
        );
    }

    #[test]
    fn oversized_length_varint_decodes_rather_than_panicking() {
        // A length-delimited field whose declared length is u64::MAX. Adding it to
        // the scan cursor unchecked panics on overflow when overflow checks are on,
        // and without them the sum wraps past the bounds test and panics on a
        // backwards slice — so this input took the fetch down in both profiles.
        // That matters beyond the crash: a fetch panic is classified non-transient,
        // so a working provider would lose its cached window and read as absent
        // rather than degraded.
        let body: Vec<u8> = vec![
            // frame header: flags 0, length 11
            0x00, 0x00, 0x00, 0x00, 0x0b, // field 1, wire type 2
            0x0a, // 10-byte varint = u64::MAX
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ];
        // Pin the frame length. An edit that miscounts these bytes makes the
        // declared length exceed the buffer, so the frame is rejected before the
        // scanner ever runs and the test passes without exercising anything.
        assert_eq!(body.len(), 16, "frame must be exactly 16 bytes");
        assert!(
            matches!(normalize_usage(&body), Err(FetchError::Decode(_))),
            "an unusable declared length is malformed input, not a transient edge \
             condition: it recurs on every fetch, so it must degrade with a reason"
        );
    }

    #[test]
    fn declared_length_past_the_buffer_decodes_rather_than_panicking() {
        // The neighbouring input class: a length that fits in usize but overruns
        // the remaining bytes. Same handling, no separate code path.
        let body: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x00,
            0x03, // field 1, wire type 2, declaring 200 bytes in a 2-byte payload
            0x0a, 0xc8, 0x01,
        ];
        assert_eq!(body.len(), 8, "frame must be exactly 8 bytes");
        assert!(matches!(normalize_usage(&body), Err(FetchError::Decode(_))));
    }

    /// The whole point of the re-attribution: an expired credential must reach a
    /// verdict rather than retrying behind a transient classification forever.
    #[test]
    fn expired_credential_turns_the_ambiguous_empty_body_into_a_verdict() {
        let attempt = FetchAttempt::failure(
            None,
            None,
            FetchError::Upstream("HTTP 200: empty response body".to_string()),
        );
        let out = disambiguate_empty_response(attempt, true);
        assert!(
            matches!(out.usage, Err(FetchError::CredentialUnusable(_))),
            "an empty body from an expired credential must be attributed to the \
             credential, not to the transport: {:?}",
            out.usage
        );
    }

    /// The guard's other side, and the one that keeps a working lane working: the
    /// same response with a live credential is still a flap, and must keep serving
    /// the last healthy window.
    #[test]
    fn live_credential_leaves_the_empty_body_transient() {
        let attempt = FetchAttempt::failure(
            None,
            None,
            FetchError::Upstream("HTTP 200: empty response body".to_string()),
        );
        let out = disambiguate_empty_response(attempt, false);
        assert!(
            matches!(out.usage, Err(FetchError::Upstream(_))),
            "an empty body with a live credential is an edge flap and must stay \
             transient: {:?}",
            out.usage
        );
    }

    /// Scoped to the one response that cannot be attributed on its own. An expired
    /// credential does not license re-attributing every other transport failure --
    /// a timeout is still a timeout, and degrading it would discard a healthy
    /// window on a network blip.
    #[test]
    fn expiry_does_not_re_attribute_other_transport_failures() {
        let attempt = FetchAttempt::failure(
            None,
            None,
            FetchError::Upstream("connection timed out".to_string()),
        );
        let out = disambiguate_empty_response(attempt, true);
        assert!(
            matches!(&out.usage, Err(FetchError::Upstream(message)) if message.contains("timed out")),
            "only the empty-body response is ambiguous; other transport errors keep \
             their attribution: {:?}",
            out.usage
        );
    }

    /// A success is a success. If a token works past its stated expiry the fetch
    /// stands -- the expiry is the issuer's claim at grant time, not proof.
    #[test]
    fn expiry_never_overrides_a_successful_fetch() {
        let attempt = FetchAttempt::success(None, "oauth", Usage::default());
        let out = disambiguate_empty_response(attempt, true);
        assert!(
            out.usage.is_ok(),
            "a token working past its stated expiry must keep its result: {:?}",
            out.usage
        );
    }
}
