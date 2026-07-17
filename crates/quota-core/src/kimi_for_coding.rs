//! Kimi $19 coding-plan usage fetcher — API-key lane, bare `Bearer` token.
//!
//! This is the third provider in the OAuth/API-key fork group. Unlike Kimi
//! (`kimi.rs`, which uses the JWT-driven web protocol and a browser fingerprint),
//! this lane is a clean Bearer-key GET against the dedicated coding-plan
//! endpoint, and the credential source is the same `CredentialSource` vault the
//! other API-key providers use (anthropic/grok use OAuth; kimi-for-coding uses an
//! API key).
//!
//! VERIFICATION: fixture-verified from CodexBar v0.43.0
//! `Sources/CodexBarCore/Providers/Kimi/`:
//!   - `KimiUsageFetcher.swift:15-48` (`fetchCodeAPIUsage`): GET
//!     `https://api.kimi.com/coding/v1/usages`, headers
//!     `Authorization: Bearer <api-key>`, `Accept: application/json`. 200 =
//!     parse; non-200 = error mapped by status (401/403 → Unauthorized).
//!   - `KimiUsageFetcher.swift:188-203` (`codeAPIUsageEndpoint`): path is
//!     `<base>/coding/v1/usages`. We hardcode the default base
//!     (`https://api.kimi.com`) and keep the path construction simple — the
//!     `/coding`/`/coding/v1` base-override forms are not used by this lane.
//!   - `KimiModels.swift:7-10` (`KimiCodeAPIUsageResponse`): the JSON shape we
//!     decode is `{ "usage": KimiUsageDetail, "limits": [KimiRateLimit]? }`.
//!   - `KimiModels.swift` `KimiUsageDetail`: `limit` (string-or-number, required),
//!     `used`, `remaining` (string-or-number, optional), `resetTime` with
//!     fallbacks `resetTime` / `resetAt` / `reset_time` / `reset_at` (first
//!     present wins). All numeric fields arrive as JSON strings OR numbers —
//!     parse both. The local `string_or_number` helper handles this; we copy it
//!     here rather than importing from `kimi.rs` (kimi.rs is a different product
//!     surface and stays untouched).
//!   - `KimiUsageFetcher.swift:204-...` (`parseCodeAPIUsage`): the `usage` object
//!     maps to the WEEKLY window; `limits[0].detail` is a secondary rate-limit
//!     detail that CodexBar surfaces as a secondary window. We SKIP `limits`
//!     for v1: CodexBar surfaces only the weekly as the primary window in its
//!     primary flow, and fabricating a second window from `limits` without a
//!     reset would violate the percent-required-reset-optional rule.
//!
//! Window: primary, `window_minutes: Some(10080)` (weekly per CodexBar's
//! `weekly:` naming), `resets_at`: RFC3339 passthrough when the reset field is
//! present and parses as ISO8601 or epoch (accept both; see zai/kimi epoch
//! handling), omitted otherwise. Provider name on the wire: `kimi-for-coding`
//! (matches ALF's model-handle id).

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;

use crate::credential_source::{CredentialSource, VaultCapability};
use crate::env;
use crate::provider::{AccountObservation, CredentialHandle, FetchAttempt};
use crate::vault_handles::VaultHandleLoader;
use crate::{
    http::{Header, JsonRequest},
    model::{RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "kimi-for-coding";
const USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";
const ENV_API_KEY: &str = "KIMI_CODE_API_KEY";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;

/// One `usage` object in the response. `limit` is required; `used` and
/// `remaining` are optional and arrive as JSON string OR number (CodexBar
/// `KimiUsageDetail`).
#[derive(Debug, Deserialize)]
struct KimiUsageDetail {
    limit: Option<serde_json::Value>,
    used: Option<serde_json::Value>,
    remaining: Option<serde_json::Value>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
    #[serde(rename = "resetAt")]
    reset_at: Option<String>,
    #[serde(rename = "reset_time")]
    reset_time_snake: Option<String>,
    #[serde(rename = "reset_at")]
    reset_at_snake: Option<String>,
}

/// The `KimiCodeAPIUsageResponse` body. `usage` is the primary weekly detail;
/// `limits[0].detail` is the secondary rate-limit detail (skipped for v1).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct KimiCodeApiResponse {
    usage: KimiUsageDetail,
    #[serde(default)]
    limits: Option<Vec<KimiRateLimit>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct KimiRateLimit {
    detail: Option<KimiUsageDetail>,
}

/// Parse a JSON string OR number into an `i64`. Returns `None` on any other
/// shape (null, object, bool, non-numeric string). Matches CodexBar's
/// `string-or-number` tolerance: e.g. `"100"` and `100` both parse.
fn string_or_number(value: Option<&serde_json::Value>) -> Option<i64> {
    match value? {
        serde_json::Value::String(text) => text.trim().parse::<i64>().ok(),
        serde_json::Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

/// Round to 2 decimal places (the consumer shows percent to 2dp; float noise
/// from a f64 multiply is undesirable).
fn round_2dp(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// Pick the first non-empty `reset*` field per CodexBar's `resetTime` fallbacks.
fn pick_reset_field(detail: &KimiUsageDetail) -> Option<&str> {
    detail
        .reset_time
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            detail
                .reset_at
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            detail
                .reset_time_snake
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            detail
                .reset_at_snake
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
}

/// Parse a `reset*` field as either an ISO8601/RFC3339 string or an epoch
/// (seconds or milliseconds — see zai/kimi epoch handling). Returns an
/// `..Z` UTC string on success.
fn parse_reset(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(epoch) = trimmed.parse::<f64>() {
        if epoch <= 0.0 {
            return None;
        }
        let secs = if epoch < 1e12 {
            epoch.round() as i64
        } else {
            (epoch / 1000.0).round() as i64
        };
        return env::epoch_to_iso8601(secs);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
    chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.fZ")
        .or_else(|_| chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%SZ"))
        .ok()
        .map(|dt| dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Decode the coding-API usage body to [`Usage`]. Pure — unit-testable.
///
/// Returns `Err(Decode(..))` if neither `used` nor `remaining` parses, or if
/// `limit` is absent / non-positive (no meaningful percent to emit).
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: KimiCodeApiResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("kimi coding usage not decodable: {e}")))?;

    let limit = string_or_number(response.usage.limit.as_ref())
        .ok_or_else(|| FetchError::Decode("kimi coding usage missing limit".to_string()))?;
    if limit <= 0 {
        return Err(FetchError::Decode(
            "kimi coding usage limit is non-positive".to_string(),
        ));
    }

    let used_percent = if let Some(used) = string_or_number(response.usage.used.as_ref()) {
        round_2dp((used as f64 / limit as f64) * 100.0)
    } else if let Some(remaining) = string_or_number(response.usage.remaining.as_ref()) {
        let used = (limit - remaining).max(0);
        round_2dp((used as f64 / limit as f64) * 100.0)
    } else {
        return Err(FetchError::Decode(
            "kimi coding usage missing used/remaining".to_string(),
        ));
    };

    let resets_at = pick_reset_field(&response.usage).and_then(parse_reset);

    Ok(Usage {
        primary: Some(RateWindow {
            used_percent,
            resets_at,
            window_minutes: Some(WEEKLY_MINUTES),
        }),
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

fn canonical_account_id(account_id: Option<String>) -> Option<String> {
    account_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn usage_request(url: &str, bearer: &str) -> JsonRequest {
    JsonRequest::get(url)
        .timeout(REQUEST_TIMEOUT)
        .bearer(bearer)
        .header(Header::new("Accept", "application/json"))
}

/// The kimi-for-coding usage provider.
pub struct KimiForCodingProvider {
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    usage_url: String,
}

impl KimiForCodingProvider {
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
        // Local lane: the env-var API key is a static token the user already
        // owns. Use the legacy `JsonRequest::send` so the byte-identical
        // `FetchError::Unauthorized("HTTP 401")` mapping stays intact (vault
        // requests get the status-first variant instead).
        let result = usage_request(&self.usage_url, bearer)
            .send(&self.http)
            .await
            .and_then(|body| normalize_usage(&body));
        match result {
            Ok(usage) => {
                FetchAttempt::success(Some(AccountObservation::new(None, None)), "api", usage)
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
        // apikey records have no account_id by contract (see CKCRED enumeration
        // contract: apikey:* has no refresh adapter and no account identity), so
        // the served observation is structurally None here.
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

impl Default for KimiForCodingProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for KimiForCodingProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        let mut handles = vec![CredentialHandle::implicit()];
        if self.credential_source.is_some() {
            handles.extend(self.handle_loader.kimi_for_coding_handles()?);
        }
        Ok(handles)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if let Some(capability) = handle.vault_capability() {
            return self.fetch_vault(capability).await;
        }

        // Implicit-local lane: `KIMI_CODE_API_KEY` (CodexBar
        // `KimiSettingsReader.swift:4`).
        let api_key = match std::env::var(ENV_API_KEY)
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            Some(key) => key,
            None => {
                return FetchAttempt::failure(
                    None,
                    None,
                    FetchError::NoSession(format!("{ENV_API_KEY} is not set")),
                );
            }
        };
        self.fetch_local_bearer(&api_key).await
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
            account_id: None,
            project_id: None,
        }
    }

    fn test_provider(
        source: Arc<dyn CredentialSource>,
        usage_url: String,
    ) -> KimiForCodingProvider {
        let mut provider = KimiForCodingProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(None)),
        );
        provider.usage_url = usage_url;
        provider
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
        (format!("http://{address}/usages"), task)
    }

    fn write_handles(body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ck-quota-kimi-for-coding-handles-{}.json",
            std::process::id()
        ));
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

    #[test]
    fn normalizes_full_shape_with_string_numerics_and_reset_time() {
        // Exact live shape (CodexBar fixture): `usage.limit`/`used`/`remaining`
        // are JSON strings; `resetTime` is the camelCase reset field.
        let body = br#"{
            "usage": {
                "limit": "100",
                "used": "25",
                "remaining": "75",
                "resetTime": "2026-07-01T12:00:00Z"
            },
            "limits": []
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-01T12:00:00Z"));
        assert_eq!(primary.window_minutes, Some(10_080));
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn normalizes_with_number_numerics_and_reset_at() {
        // `resetAt` is the second-priority field per CodexBar's reset fallbacks.
        let body = br#"{
            "usage": {
                "limit": 200,
                "used": 50,
                "remaining": 150,
                "resetAt": "2026-07-02T00:00:00Z"
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-02T00:00:00Z"));
        assert_eq!(primary.window_minutes, Some(10_080));
    }

    #[test]
    fn normalizes_with_snake_case_reset_at_fallback() {
        // `reset_at` (snake_case) is the third-priority field per CodexBar's
        // reset fallbacks.
        let body = br#"{
            "usage": {
                "limit": "100",
                "used": "10",
                "remaining": "90",
                "reset_at": "2026-07-09T06:56:36Z"
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 10.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-09T06:56:36Z"));
    }

    #[test]
    fn missing_reset_emits_window_without_resets_at() {
        // No reset field at all: percent-required-reset-optional rule means we
        // still emit the window but omit the reset.
        let body = br#"{
            "usage": { "limit": "50", "used": "5" }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 10.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.window_minutes, Some(10_080));
    }

    #[test]
    fn missing_used_and_remaining_is_decode_error() {
        let body = br#"{ "usage": { "limit": "100", "resetTime": "2026-07-01T12:00:00Z" } }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn zero_limit_is_decode_error_no_div_by_zero() {
        let body = br#"{ "usage": { "limit": "0", "used": "5" } }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn missing_limit_is_decode_error() {
        let body = br#"{ "usage": { "used": "5" } }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn garbage_body_is_decode_error() {
        let err = normalize_usage(b"not json").unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn epoch_seconds_reset_parses_to_iso8601() {
        let body = br#"{
            "usage": {
                "limit": "100",
                "used": "1",
                "remaining": "99",
                "resetTime": "1782135879"
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert_eq!(
            usage.primary.unwrap().resets_at.as_deref(),
            Some("2026-06-22T13:44:39Z")
        );
    }

    #[test]
    fn handles_no_vault_handle_returns_only_implicit_local() {
        // No env, no vault handle: the implicit-local lane is still exposed
        // so a `NoSession` degraded entry is produced by the scheduler.
        let provider = KimiForCodingProvider::new_with_handle_loader(
            None,
            Arc::new(VaultHandleLoader::new(None)),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles, vec![CredentialHandle::implicit()]);
    }

    #[test]
    fn handles_include_vault_entry_when_source_is_wired() {
        let path = write_handles(r#"{"handles":{"kimi-for-coding":"ckh_kimi"}}"#);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = KimiForCodingProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(Some(path.clone()))),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0], CredentialHandle::implicit());
        assert_eq!(handles[1].stable_id(), "kimi-for-coding");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn both_lanes_enumerate_two_fetch_units() {
        // env lane present + vault handle present → 2 fetch units
        // (implicit-local + vault). The scheduler's emission gate dedups them
        // to one unlabeled entry on read; this test just confirms the
        // enumeration surfaces both.
        let path = write_handles(r#"{"handles":{"kimi-for-coding":"ckh_kimi"}}"#);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = KimiForCodingProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(Some(path.clone()))),
        );
        // The implicit-local lane is always emitted whether or not the env var
        // is set (enumeration never reads the environment; only the fetch
        // does, degrading to NoSession when the key is absent), so no env
        // mutation is needed here.
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0], CredentialHandle::implicit());
        assert_eq!(handles[1].stable_id(), "kimi-for-coding");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn vault_happy_path_uses_served_bearer_and_record_version() {
        let body = br#"{
            "usage": { "limit": "100", "used": "30", "remaining": "70", "resetTime": "2026-07-01T12:00:00Z" }
        }"#
        .to_vec();
        let (url, request) = serve_once(200, body).await;
        let (source, _) = source(Ok(credential(b"kimi-coding-vault-token", 27)));
        let provider = test_provider(source, url);
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "kimi-for-coding",
                VaultCapability::new("ckh_kimi"),
            ))
            .await;

        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt.observed.unwrap(),
            AccountObservation::new(None, Some(27))
        );
        let primary = attempt.usage.unwrap().primary.unwrap();
        assert_eq!(primary.used_percent, 30.0);
        assert!(request
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer kimi-coding-vault-token"));
    }

    #[tokio::test]
    async fn failed_get_is_unverified_and_clears_prior_observation() {
        // F1 regression: a failed `credential.get` clears any prior
        // observation and is reported as Unverified, with `last_success_at`
        // cleared and `label_in_flux` set.
        let (source, _) = source(Err(VaultGetError::Transient));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "kimi-for-coding",
                VaultCapability::new("ckh_kimi"),
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
        // Vault 401 → `ProviderStatus(401)` + `report_auth_failure` carries
        // the served `record_version`. Local 401 → byte-identical
        // `Unauthorized("HTTP 401")` legacy string.
        let (vault_url, _) = serve_once(401, Vec::new()).await;
        let (source, reports) = source(Ok(credential(b"kimi-coding-vault-token", 44)));
        let mut provider = test_provider(Arc::clone(&source), vault_url);
        let vault = provider
            .fetch_handle(&CredentialHandle::vault(
                "kimi-for-coding",
                VaultCapability::new("ckh_kimi"),
            ))
            .await;
        assert!(matches!(vault.usage, Err(FetchError::ProviderStatus(401))));
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 44)]);

        reports.lock().unwrap().clear();
        let (local_url, _) = serve_once(401, Vec::new()).await;
        provider.usage_url = local_url;
        let local = provider.fetch_local_bearer("kimi-coding-local-token").await;
        assert!(matches!(
            local.usage,
            Err(FetchError::Unauthorized(message)) if message == "HTTP 401"
        ));
        tokio::task::yield_now().await;
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_utf8_vault_payload_is_a_verified_decode_failure() {
        let (source, _) = source(Ok(credential(&[0xff, 0xfe], 8)));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "kimi-for-coding",
                VaultCapability::new("ckh_kimi"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Verified
        );
        assert_eq!(attempt.observed.unwrap().record_version, Some(8));
        assert!(matches!(attempt.usage, Err(FetchError::Decode(_))));
    }
}
