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

use crate::browser_cookies;
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
const SUBSCRIPTION_STATS_URL: &str =
    "https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStats";
const ENV_API_KEY: &str = "KIMI_CODE_API_KEY";
/// The browser host carrying the web console session, which is a different
/// credential from the coding API key above.
const WEB_COOKIE_DOMAIN: &str = "kimi.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;
const FIVE_HOUR_MINUTES: i64 = 5 * 60;

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
/// `limits[0].detail` is the 5-hour rate-limit window (CodexBar v0.45.2
/// `parseCodeAPIUsage` line 184).
#[derive(Debug, Deserialize)]
struct KimiCodeApiResponse {
    usage: KimiUsageDetail,
    #[serde(default)]
    limits: Option<Vec<KimiRateLimit>>,
}

#[derive(Debug, Deserialize)]
struct KimiRateLimit {
    detail: Option<KimiUsageDetail>,
}

/// Response from `GetSubscriptionStats` — carries the monthly subscription
/// balance and the code-specific 7-day rate limit (CodexBar v0.45.2
/// `KimiSubscriptionStatsResponse`).
#[derive(Debug, Deserialize)]
struct SubscriptionStatsResponse {
    #[serde(rename = "subscriptionBalance")]
    subscription_balance: Option<SubscriptionBalance>,
    #[serde(rename = "ratelimitCode7d")]
    ratelimit_code_7d: Option<SubscriptionRateLimit>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionBalance {
    feature: Option<String>,
    #[serde(rename = "type")]
    balance_type: Option<String>,
    #[serde(rename = "amountUsedRatio")]
    amount_used_ratio: Option<f64>,
    #[serde(rename = "expireTime")]
    expire_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionRateLimit {
    ratio: Option<f64>,
    enabled: Option<bool>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
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

/// Build a `RateWindow` from a `KimiUsageDetail` (used/limit/remaining + reset).
/// Returns `None` when the detail has no parseable limit or neither used nor
/// remaining.
#[allow(clippy::question_mark)]
fn window_from_detail(detail: &KimiUsageDetail, window_minutes: Option<i64>) -> Option<RateWindow> {
    let limit = string_or_number(detail.limit.as_ref())?;
    if limit <= 0 {
        return None;
    }
    let used_percent = if let Some(used) = string_or_number(detail.used.as_ref()) {
        round_2dp((used as f64 / limit as f64) * 100.0)
    } else if let Some(remaining) = string_or_number(detail.remaining.as_ref()) {
        let used = (limit - remaining).max(0);
        round_2dp((used as f64 / limit as f64) * 100.0)
    } else {
        return None;
    };
    let resets_at = pick_reset_field(detail).and_then(parse_reset);
    Some(RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at,
        window_minutes,
        used_count: None,
        total_count: None,
    })
}

/// Decode the coding-API usage body to [`Usage`]. Pure — unit-testable.
///
/// Maps `usage` → primary (weekly/7d), `limits[0].detail` → secondary (5h).
/// The monthly + code-7d extras come from a separate `GetSubscriptionStats`
/// call and are merged by the caller.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: KimiCodeApiResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("kimi coding usage not decodable: {e}")))?;

    let primary = window_from_detail(&response.usage, Some(WEEKLY_MINUTES)).ok_or_else(|| {
        FetchError::Decode("kimi coding usage missing valid weekly window".to_string())
    })?;

    let secondary = response
        .limits
        .as_ref()
        .and_then(|limits| limits.first())
        .and_then(|rate_limit| rate_limit.detail.as_ref())
        .and_then(|detail| window_from_detail(detail, Some(FIVE_HOUR_MINUTES)));

    Ok(Usage {
        primary: Some(primary),
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// Parse the `GetSubscriptionStats` response into extra windows (monthly
/// subscription balance + code-specific 7-day rate limit). Best-effort:
/// returns an empty vec on any parse failure or missing data.
///
/// CodexBar v0.45.2 `KimiUsageSnapshot.toUsageSnapshot()`:
/// - monthly: `subscriptionBalance.amountUsedRatio * 100`, guarded by
///   feature == nil || "FEATURE_OMNI" and type == nil || "SUBSCRIPTION"
/// - code-7d: `ratelimitCode7d.ratio * 100`, guarded by enabled != false
fn parse_subscription_extras(body: &[u8]) -> Vec<crate::model::ExtraWindow> {
    let response: SubscriptionStatsResponse = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut extras = Vec::new();

    if let Some(balance) = response.subscription_balance {
        let feature_ok = balance
            .feature
            .as_deref()
            .is_none_or(|f| f == "FEATURE_OMNI");
        let type_ok = balance
            .balance_type
            .as_deref()
            .is_none_or(|t| t == "SUBSCRIPTION");
        if feature_ok && type_ok {
            if let Some(ratio) = balance.amount_used_ratio {
                if ratio.is_finite() {
                    let used_percent = (ratio * 100.0).clamp(0.0, 100.0);
                    let resets_at = balance.expire_time.as_deref().and_then(parse_reset);
                    extras.push(crate::model::ExtraWindow {
                        id: Some("kimi-monthly".to_string()),
                        title: Some("Monthly".to_string()),
                        window: Some(RateWindow {
                            used_percent: round_2dp(used_percent),
                            raw_used_percent: None,
                            resets_at,
                            window_minutes: None,
                            used_count: None,
                            total_count: None,
                        }),
                    });
                }
            }
        }
    }

    if let Some(limit) = response.ratelimit_code_7d {
        if limit.enabled != Some(false) {
            if let Some(ratio) = limit.ratio {
                if ratio.is_finite() {
                    let used_percent = (ratio * 100.0).clamp(0.0, 100.0);
                    let resets_at = limit.reset_time.as_deref().and_then(parse_reset);
                    extras.push(crate::model::ExtraWindow {
                        id: Some("kimi-code-7d".to_string()),
                        title: Some("Code 7-day".to_string()),
                        window: Some(RateWindow {
                            used_percent: round_2dp(used_percent),
                            raw_used_percent: None,
                            resets_at,
                            window_minutes: Some(WEEKLY_MINUTES),
                            used_count: None,
                            total_count: None,
                        }),
                    });
                }
            }
        }
    }

    extras
}

/// Merge subscription-stats extras into a `Usage` value.
fn merge_extras(usage: &mut Usage, extras: Vec<crate::model::ExtraWindow>) {
    if extras.is_empty() {
        return;
    }
    match usage.extra_rate_windows {
        Some(ref mut existing) => existing.extend(extras),
        None => usage.extra_rate_windows = Some(extras),
    }
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

fn subscription_stats_request(web_token: &str) -> JsonRequest {
    JsonRequest::post_json(SUBSCRIPTION_STATS_URL, b"{}".to_vec())
        .timeout(REQUEST_TIMEOUT)
        .bearer(web_token)
        .header(Header::new("Content-Type", "application/json"))
        .header(Header::new("Accept", "application/json"))
        .header(Header::new("Cookie", format!("kimi-auth={web_token}")))
        .header(Header::new("Origin", "https://www.kimi.com"))
        .header(Header::new("Referer", "https://www.kimi.com/code/console"))
}

/// The web console session token, when this host has one.
///
/// The subscription extras come from the web console, which authenticates with
/// a browser session rather than with the coding API key that serves the usage
/// endpoint. They are two separate credentials for two separate surfaces, and
/// the key is not accepted by the console -- so sending it there produces a
/// rejection on every fetch, and the extras it was meant to collect never
/// arrive.
///
/// Returning `None` when no browser session exists is what keeps the failure
/// legible: the enrichment is skipped rather than attempted and swallowed, so a
/// host without a Kimi browser login does no console request at all.
async fn web_enrichment_token() -> Option<String> {
    let jar = browser_cookies::chrome_cookies_for_async(WEB_COOKIE_DOMAIN)
        .await
        .ok()?;
    jar.header()
        .split("; ")
        .find_map(|pair| pair.strip_prefix("kimi-auth="))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

/// Fetch the optional subscription extras and fold them in.
///
/// Best-effort by design: the usage windows are already resolved by the time
/// this runs, and a console that is unreachable, logged out, or slow must not
/// turn a good fetch into a degraded entry. The cost of skipping is two extra
/// windows a consumer treats as optional; the cost of failing would be the
/// provider's whole capacity signal.
async fn merge_subscription_extras(http: &reqwest::Client, usage: &mut Usage) {
    let Some(web_token) = web_enrichment_token().await else {
        return;
    };
    if let Ok(stats_body) = subscription_stats_request(&web_token).send(http).await {
        merge_extras(usage, parse_subscription_extras(&stats_body));
    }
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
            Ok(mut usage) => {
                merge_subscription_extras(&self.http, &mut usage).await;
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
        // API-key credential records carry no account identity by contract — they
        // have no refresh adapter and nothing that resolves an account — so the
        // served observation is structurally None here, and this provider emits a
        // single unlabeled entry rather than a per-account one.
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
            Ok(mut usage) => {
                merge_subscription_extras(&self.http, &mut usage).await;
                FetchAttempt::success(observed, "vault", usage).with_account_info(account_info)
            }
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
            email: None,
            org_name: None,
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

    /// Serve one request and hand back what was sent, at this provider's path.
    ///
    /// Wraps the shared helper so the URL keeps the shape this provider
    /// builds against. The shared reader is used because a single socket read
    /// returns one TCP segment rather than the whole request, which makes any
    /// assertion about what was NOT sent pass without reading it.
    async fn serve_once(status: u16, body: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let (base, task) = crate::loopback::serve_once(status, body).await;
        (format!("{base}/usages"), task)
    }

    fn write_handles(body: &str) -> std::path::PathBuf {
        // Unique per call, not just per process: tests in one binary run in
        // parallel threads, and a shared path would let one test truncate or
        // delete the file another test is reading.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ck-quota-kimi-for-coding-handles-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
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
        // Pinned to the window guard rather than to the variant. Both
        // failure paths in this function report Decode, so a bare variant
        // check is also satisfied by the body failing to parse at all --
        // and a field becoming required is enough to move this input there,
        // leaving the test green while it exercises a different rule.
        assert!(
            matches!(&err, FetchError::Decode(m) if m.contains("missing valid weekly window")),
            "expected the window guard, got: {err}"
        );
    }

    #[test]
    fn zero_limit_is_decode_error_no_div_by_zero() {
        let body = br#"{ "usage": { "limit": "0", "used": "5" } }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(
            matches!(&err, FetchError::Decode(m) if m.contains("missing valid weekly window")),
            "expected the window guard, got: {err}"
        );
    }

    #[test]
    fn missing_limit_is_decode_error() {
        let body = br#"{ "usage": { "used": "5" } }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(
            matches!(&err, FetchError::Decode(m) if m.contains("missing valid weekly window")),
            "expected the window guard, got: {err}"
        );
    }

    /// A live console payload, captured from this host's browser session on
    /// 2026-08-07. LIVE-OBSERVED, not hand-written: the field spellings, the
    /// ratio scale (a fraction, not a percent) and the nanosecond-precision
    /// timestamps are all as the server sent them.
    const LIVE_SUBSCRIPTION_STATS: &[u8] = br#"{"ratelimitCode5h":{"ratio":0.11,"enabled":true,"resetTime":"2026-08-07T13:08:27.604046432Z"},"ratelimitCode7d":{"ratio":0.1621,"enabled":true,"resetTime":"2026-08-13T18:08:27.604046432Z"},"subscriptionBalance":{"id":"19f902a6-04c2-8306-8000-0000ff64c68f","feature":"FEATURE_OMNI","type":"SUBSCRIPTION","unit":"UNIT_CREDIT","amountUsedRatio":0.1915,"kimiCodeUsedRatio":0.1915,"expireTime":"2026-08-23T18:08:35Z","domain":"DOMAIN_NEXUS"}}"#;

    /// The console's own response yields both extras.
    ///
    /// Pinned against a live capture because the parser was written from the
    /// upstream reference and had never seen a real response: the extras were
    /// requested with the wrong credential, so every attempt was rejected and
    /// silently discarded, and this code path had never once produced a window
    /// in production.
    #[test]
    fn live_console_payload_yields_both_extras() {
        let extras = parse_subscription_extras(LIVE_SUBSCRIPTION_STATS);

        let ids: Vec<_> = extras.iter().filter_map(|e| e.id.as_deref()).collect();
        assert_eq!(ids, vec!["kimi-monthly", "kimi-code-7d"], "{extras:?}");

        // Ratios are fractions on the wire and percents on ours.
        let monthly = extras[0].window.as_ref().expect("monthly window");
        assert_eq!(monthly.used_percent, 19.15);
        assert_eq!(monthly.resets_at.as_deref(), Some("2026-08-23T18:08:35Z"));

        let code_7d = extras[1].window.as_ref().expect("7d window");
        assert_eq!(code_7d.used_percent, 16.21);
        assert_eq!(code_7d.window_minutes, Some(WEEKLY_MINUTES));
        assert!(code_7d.resets_at.is_some());
    }

    /// A disabled rate limit contributes no window.
    ///
    /// Without this the extras test above would pass against a parser that
    /// ignored `enabled` entirely, which would publish a window for a limit the
    /// account is not subject to.
    #[test]
    fn a_disabled_rate_limit_is_not_published() {
        let body = br#"{"ratelimitCode7d":{"ratio":0.5,"enabled":false}}"#;
        let extras = parse_subscription_extras(body);
        assert!(extras.is_empty(), "{extras:?}");
    }

    /// A non-subscription balance contributes no monthly window.
    ///
    /// The console returns other balance kinds against the same field, and
    /// treating one as the subscription would report an unrelated allowance as
    /// this account's monthly usage.
    #[test]
    fn a_non_subscription_balance_is_not_published_as_monthly() {
        let body = br#"{"subscriptionBalance":{"feature":"FEATURE_OMNI","type":"WALLET","amountUsedRatio":0.9}}"#;
        let extras = parse_subscription_extras(body);
        assert!(extras.is_empty(), "{extras:?}");
    }

    #[test]
    fn garbage_body_is_decode_error() {
        let err = normalize_usage(b"not json").unwrap_err();
        // The neighbouring failure path, pinned for the same reason: this
        // one must stay on the parse error and must not be satisfied by the
        // window guard.
        assert!(
            matches!(&err, FetchError::Decode(m) if m.contains("not decodable")),
            "expected the parse error, got: {err}"
        );
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
        // Fail-closed regression: a failed `credential.get` means the account
        // behind the handle is unverified this tick, so the slot clears any
        // prior observation (`last_success_at` reset, `label_in_flux` set)
        // instead of stale-serving a window that may belong to a different
        // account after a handle re-point.
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
