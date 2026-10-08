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
//!     originally decoded is `{ "usage": KimiUsageDetail, "limits": [KimiRateLimit]? }`.
//!     The current decoder also accepts optional `usages` ratio pools without
//!     requiring a weekly counter.
//!   - `KimiModels.swift` `KimiUsageDetail`: `limit` (string-or-number, required),
//!     `used`, `remaining` (string-or-number, optional), `resetTime` with
//!     fallbacks `resetTime` / `resetAt` / `reset_time` / `reset_at` (first
//!     present wins). All numeric fields arrive as JSON strings OR numbers —
//!     parse both. The local `string_or_number` helper handles this; we copy it
//!     here rather than importing from `kimi.rs` (kimi.rs is a different product
//!     surface and stays untouched).
//!   - Coding ratio pools and reconciliation follow CodexBar v0.73.0
//!     (`8ab81e2eb`), `KimiModels.swift`, `KimiUsageFetcher.swift` and
//!     `KimiUsageSnapshot.swift::resolvedRatioWindow`. `limit_7d` stays primary,
//!     `limit_5h` stays secondary, and `limit_month_total` is a separate extra.
//!     Reliable counters win only when more exhausted and describing the same
//!     duration and reset (within two seconds, inclusive). Optional malformed
//!     pools are skipped without discarding usable counters or sibling pools.
//!
//! Window: primary, `window_minutes: Some(10080)` (weekly per CodexBar's
//! `weekly:` naming), `resets_at`: RFC3339 passthrough when the reset field is
//! present and parses as ISO8601 or epoch (accept both; see zai/kimi epoch
//! handling), omitted otherwise. Provider name on the wire: `kimi-for-coding`
//! (matches ALF's model-handle id).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde::Deserialize;

#[cfg(test)]
use crate::credential_source::VaultCapability;
use crate::credential_source::{CredentialSource, ScopedSnapshot};
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
/// The vault family whose deposit carries the web console session, which is a
/// different credential from the coding API key above. Read only for the
/// optional subscription extras, never enumerated as a lane.
pub(crate) const WEB_COOKIE_FAMILY: &str = "cookie:kimi.com";
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
    usage: Option<KimiUsageDetail>,
    // Keep optional pool schema drift outside the required-window decoder.
    usages: Option<serde_json::Value>,
    #[serde(default)]
    limits: Option<Vec<KimiRateLimit>>,
}

#[derive(Debug, Deserialize)]
struct KimiRateLimit {
    detail: Option<KimiUsageDetail>,
    window: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct KimiWindow {
    duration: i64,
    #[serde(rename = "timeUnit")]
    time_unit: String,
}

impl KimiWindow {
    fn duration_minutes(&self) -> Option<i64> {
        if self.duration <= 0 {
            return None;
        }
        let multiplier = match self.time_unit.as_str() {
            "TIME_UNIT_MINUTE" => 1,
            "TIME_UNIT_HOUR" => 60,
            "TIME_UNIT_DAY" => 24 * 60,
            _ => return None,
        };
        self.duration.checked_mul(multiplier)
    }
}

/// CodexBar's `KimiModels.swift::KimiRatioPool` reads only `used_ratio` and
/// `reset_time`: the ratio is already the used fraction, not a count to divide.
#[derive(Debug, Deserialize)]
struct KimiRatioPool {
    used_ratio: Option<f64>,
    reset_time: Option<String>,
}

#[derive(Default)]
struct CodingPools {
    session: Option<KimiRatioPool>,
    weekly: Option<KimiRatioPool>,
    monthly: Option<KimiRatioPool>,
}

fn decode_pool(
    value: Option<&serde_json::Value>,
    field: &'static str,
    refusals: &mut Vec<&'static str>,
) -> Option<KimiRatioPool> {
    let value = value.filter(|value| !value.is_null())?;
    let pool = serde_json::from_value::<KimiRatioPool>(value.clone()).ok();
    if pool.as_ref().is_some_and(|pool| {
        pool.used_ratio
            .is_some_and(|ratio| ratio.is_finite() && ratio >= 0.0)
    }) {
        pool
    } else {
        refusals.push(field);
        None
    }
}

fn decode_pools(value: Option<&serde_json::Value>) -> (CodingPools, Vec<&'static str>) {
    let mut refusals = Vec::new();
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return (CodingPools::default(), refusals);
    };
    let Some(object) = value.as_object() else {
        return (CodingPools::default(), vec!["usages"]);
    };
    let pools = CodingPools {
        session: decode_pool(object.get("limit_5h"), "usages.limit_5h", &mut refusals),
        weekly: decode_pool(object.get("limit_7d"), "usages.limit_7d", &mut refusals),
        monthly: decode_pool(
            object.get("limit_month_total"),
            "usages.limit_month_total",
            &mut refusals,
        ),
    };
    (pools, refusals)
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
        window_kind: None,
        used_percent,
        raw_used_percent: None,
        resets_at,
        window_minutes,
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    })
}

fn ratio_window(pool: &KimiRatioPool, minutes: Option<i64>) -> RateWindow {
    RateWindow {
        window_kind: None,
        used_percent: pool.used_ratio.unwrap_or_default().min(1.0) * 100.0,
        raw_used_percent: None,
        resets_at: pool.reset_time.as_deref().and_then(parse_reset),
        window_minutes: minutes,
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    }
}

/// A nonnegative used count remains reliable even above the limit. Only use
/// remaining as a fallback when it lies between zero and the total limit.
fn reliable_count_percent(detail: &KimiUsageDetail) -> Option<f64> {
    let limit = string_or_number(detail.limit.as_ref()).filter(|limit| *limit > 0)?;
    let used = string_or_number(detail.used.as_ref())
        .filter(|used| *used >= 0)
        .or_else(|| {
            string_or_number(detail.remaining.as_ref())
                .filter(|remaining| (0..=limit).contains(remaining))
                .map(|remaining| limit - remaining)
        })?;
    Some((used as f64 / limit as f64 * 100.0).clamp(0.0, 100.0))
}

fn reset_instant(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // Published legacy resets omit fractions; compare the original clock values
    // so a 2.9-second separation cannot become a two-second match by truncation.
    chrono::DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|date| date.to_utc())
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(&parse_reset(value)?)
                .ok()
                .map(|date| date.to_utc())
        })
}

fn resolved_ratio_window(
    pool: Option<&KimiRatioPool>,
    detail: Option<&KimiUsageDetail>,
    minutes: i64,
    count_window_minutes: Option<i64>,
) -> Option<RateWindow> {
    let pool = pool?;
    let window = ratio_window(pool, Some(minutes));
    if count_window_minutes == Some(minutes) {
        if let Some(detail) = detail {
            if let (Some(percent), Some(count_reset), Some(ratio_reset)) = (
                reliable_count_percent(detail),
                pick_reset_field(detail).and_then(reset_instant),
                pool.reset_time.as_deref().and_then(reset_instant),
            ) {
                if percent > window.used_percent
                    && (count_reset - ratio_reset).abs() <= chrono::Duration::seconds(2)
                {
                    let mut counter = window_from_detail(detail, count_window_minutes)?;
                    counter.used_percent = percent;
                    return Some(counter);
                }
            }
        }
    }
    Some(window)
}

/// Publish weekly usage in primary and five-hour usage in secondary, so
/// consumers keep the same slot identities whether readings are ratios or counts.
/// Optional malformed pools never discard sibling pools or count-based windows.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    normalize_with_pool_refusals(body).map(|(usage, _)| usage)
}

fn normalize_with_pool_refusals(body: &[u8]) -> Result<(Usage, Vec<&'static str>), FetchError> {
    let response: KimiCodeApiResponse =
        crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body)
            .map_err(|e| FetchError::Decode(format!("kimi coding usage not decodable: {e}")))?;

    let (pools, refusals) = decode_pools(response.usages.as_ref());
    let weekly = response.usage.as_ref();
    let primary = resolved_ratio_window(
        pools.weekly.as_ref(),
        weekly,
        WEEKLY_MINUTES,
        Some(WEEKLY_MINUTES),
    )
    .or_else(|| weekly.and_then(|detail| window_from_detail(detail, Some(WEEKLY_MINUTES))));
    let rate_limit = response.limits.as_ref().and_then(|limits| limits.first());
    let detail = rate_limit.and_then(|limit| limit.detail.as_ref());
    // Responses without window metadata use the existing five-hour fallback.
    // Supplied but unsupported metadata cannot establish a duration to compare.
    let count_minutes = match rate_limit.and_then(|limit| limit.window.as_ref()) {
        None => Some(FIVE_HOUR_MINUTES),
        Some(value) => serde_json::from_value::<KimiWindow>(value.clone())
            .ok()
            .and_then(|window| window.duration_minutes()),
    };
    let secondary = resolved_ratio_window(
        pools.session.as_ref(),
        detail,
        FIVE_HOUR_MINUTES,
        count_minutes,
    )
    .or_else(|| detail.and_then(|detail| window_from_detail(detail, count_minutes)));
    let monthly = pools.monthly.as_ref().map(|pool| {
        let mut window = ratio_window(pool, None);
        // The wire key explicitly names a month, but states no duration in days.
        window.window_kind = Some(cortexkit_provider_usage::window_kind::MONTHLY.to_string());
        crate::model::ExtraWindow {
            id: Some("kimi-monthly".to_string()),
            title: Some("Total usage".to_string()),
            window: Some(window),
        }
    });
    if primary.is_none() && pools.session.is_none() && monthly.is_none() {
        return Err(FetchError::Decode(
            "kimi coding usage missing valid weekly window".to_string(),
        ));
    }
    Ok((
        Usage {
            primary,
            secondary,
            tertiary: None,
            extra_rate_windows: monthly.map(|window| vec![window]),
        },
        refusals,
    ))
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
                            window_kind: None,
                            used_percent: round_2dp(used_percent),
                            raw_used_percent: None,
                            resets_at,
                            window_minutes: None,
                            used_count: None,
                            total_count: None,
                            regeneration: None,
                            breakdown: None,
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
                            window_kind: Some(
                                cortexkit_provider_usage::window_kind::WEEKLY.to_string(),
                            ),
                            used_percent: round_2dp(used_percent),
                            raw_used_percent: None,
                            resets_at,
                            window_minutes: Some(WEEKLY_MINUTES),
                            used_count: None,
                            total_count: None,
                            regeneration: None,
                            breakdown: None,
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
        Some(ref mut existing) => {
            // Coding pools are authoritative; web enrichment may supply the same
            // monthly id and must not duplicate or replace that reading.
            for extra in extras {
                if !existing.iter().any(|window| window.id == extra.id) {
                    existing.push(extra);
                }
            }
        }
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

/// Which `cookie:kimi.com` deposit in the installed snapshot to read, if any.
///
/// An account-suffixed deposit wins over the bare one, the same precedence the
/// cookie providers apply: the operator named an account.
fn web_cookie_deposit(snapshot: &ScopedSnapshot) -> Option<&str> {
    let mut bare = None;
    for row in &snapshot.rows {
        let id = row.credential_id.as_str();
        if id == WEB_COOKIE_FAMILY {
            bare = Some(id);
        } else if crate::vault_handles::handle_id_names_family(id, WEB_COOKIE_FAMILY) {
            return Some(id);
        }
    }
    bare
}

/// The `kimi-auth` value in a deposited `Cookie:` header.
fn kimi_auth_token(header: &str) -> Option<String> {
    crate::cookie_jar::CookieJar::from_header(header)
        .cookies
        .into_iter()
        .find(|cookie| cookie.name == "kimi-auth" && !cookie.value.is_empty())
        .map(|cookie| cookie.value)
}

/// The kimi-for-coding usage provider.
pub struct KimiForCodingProvider {
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    usage_url: String,
    pool_refusals: Mutex<HashMap<String, Vec<&'static str>>>,
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
            pool_refusals: Mutex::new(HashMap::new()),
        }
    }

    fn update_pool_refusals(&self, handle_id: &str, current: &[&'static str]) -> bool {
        let mut refusals = self
            .pool_refusals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = refusals
            .get(handle_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if previous == current {
            return false;
        }
        if current.is_empty() {
            refusals.remove(handle_id);
        } else {
            refusals.insert(handle_id.to_string(), current.to_vec());
        }
        true
    }

    fn normalize_for_handle(&self, handle_id: &str, body: &[u8]) -> Result<Usage, FetchError> {
        let (usage, refusals) = normalize_with_pool_refusals(body)?;
        if self.update_pool_refusals(handle_id, &refusals) {
            for field in refusals {
                eprintln!("{} warning: kimi-for-coding optional pool unreadable ({handle_id}): {field}; pool skipped", crate::LOG_TAG);
            }
        }
        Ok(usage)
    }

    fn report_auth_failure(
        &self,
        handle: &CredentialHandle,
        record_version: u64,
        error: &FetchError,
    ) {
        crate::credential_source::report_vault_auth_failure(
            self.credential_source.as_ref(),
            handle,
            record_version,
            error,
        );
    }

    /// The web console session token, when a `cookie:kimi.com` deposit holds one.
    ///
    /// The subscription extras come from the web console, which authenticates with
    /// a web session rather than with the coding API key that serves the usage
    /// endpoint. They are two separate credentials for two separate surfaces, and
    /// the key is not accepted by the console -- so sending it there produces a
    /// rejection on every fetch, and the extras it was meant to collect never
    /// arrive.
    ///
    /// The deposit is looked up in the snapshot the handle loader already holds
    /// and read by id, so it never becomes a slot of its own. Returning `None`
    /// when no deposit exists is what keeps the failure legible: the enrichment
    /// is skipped rather than attempted and swallowed, so a host without a Kimi
    /// web login does no console request at all.
    async fn web_enrichment_token(&self) -> Option<String> {
        let source = self.credential_source.as_ref()?;
        let snapshot = self.handle_loader.snapshot()?;
        let id = web_cookie_deposit(&snapshot)?;
        let mut credential = source
            .get_scoped(id, crate::credential_source::VAULT_READ_MIN_TTL_MS)
            .await
            .ok()?;
        let header = crate::credential_source::take_utf8_payload(&mut credential.payload).ok()?;
        kimi_auth_token(&header)
    }

    /// Fetch the optional subscription extras and fold them in.
    ///
    /// Best-effort by design: the usage windows are already resolved by the time
    /// this runs, and a console that is unreachable, logged out, or slow must not
    /// turn a good fetch into a degraded entry. The cost of skipping is two extra
    /// windows a consumer treats as optional; the cost of failing would be the
    /// provider's whole capacity signal.
    async fn merge_subscription_extras(&self, usage: &mut Usage) {
        let Some(web_token) = self.web_enrichment_token().await else {
            return;
        };
        if let Ok(stats_body) = subscription_stats_request(&web_token)
            .send(&self.http)
            .await
        {
            merge_extras(usage, parse_subscription_extras(&stats_body));
        }
    }

    async fn fetch_local_bearer(&self, bearer: &str) -> FetchAttempt {
        let result = usage_request(&self.usage_url, bearer)
            .send(&self.http)
            .await
            .and_then(|body| {
                self.normalize_for_handle(CredentialHandle::implicit().stable_id(), &body)
            });
        match result {
            Ok(mut usage) => {
                self.merge_subscription_extras(&mut usage).await;
                FetchAttempt::success(Some(AccountObservation::new(None, None)), "api", usage)
            }
            Err(error) => FetchAttempt::failure(None, None, error),
        }
    }

    async fn fetch_vault(&self, handle: &CredentialHandle) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match crate::credential_source::get_vault_credential(
            credential_source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
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
            .and_then(|body| self.normalize_for_handle(handle.stable_id(), &body));
        if let Err(error) = &result {
            self.report_auth_failure(handle, record_version, error);
        }
        match result {
            Ok(mut usage) => {
                self.merge_subscription_extras(&mut usage).await;
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
        // VAULT-ONLY CUSTODY. Third member of the class reported on insula#19,
        // and the one my hand sweep missed -- found by the fence written to stop
        // a fourth, on its first run.
        //
        // The local lane resolves no identity: its success builds
        // `AccountObservation::new(None, None)` explicitly. So an implicit lane
        // beside a vault one is a second slot for the same account that
        // deduplicates ONLY while the vault side is unlabelled too. Labelling the
        // vault record splits the row and publishes the local lane beside it.
        //
        // Latent here rather than visible: the `kimi-for-coding` vault record on
        // this host carries no label, so there is nothing yet to split.
        if self.credential_source.is_some() {
            let vault = self.handle_loader.kimi_for_coding_handles()?;
            if !vault.is_empty() {
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
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

    // CodexBar 8ab81e2eb, Tests/CodexBarTests/KimiRatioPoolTests.swift,
    // `ratio weekly and session retain established lane ordering`. The fixture
    // places weekly usage in primary and session usage in secondary without counts.
    const RATIO_LANES: &[u8] = br#"{"usages": {
        "limit_7d": {"used_ratio": 0.125, "reset_time": "2026-09-20T00:00:00Z"},
        "limit_5h": {"used_ratio": 0.625}
    }}"#;

    // CodexBar 8ab81e2eb, Tests/CodexBarTests/KimiContradictoryUsageTests.swift,
    // exact quota response from #4306, without credentials or account identifiers.
    // The session counter is exhausted while its matching ratio is zero, and
    // monthly usage is independent of both.
    const CONTRADICTORY_POOLS: &[u8] = br#"{
        "limits": [{ "window": { "duration": 300, "timeUnit": "TIME_UNIT_MINUTE" },
            "detail": { "limit": "100", "used": "100", "resetTime": "2026-10-06T13:23:46.915474Z" } }],
        "usages": {
            "limit_5h": { "used_ratio": 0, "reset_time": "2026-10-06T13:23:46Z" },
            "limit_month_total": { "used_ratio": 0.5531, "reset_time": "2026-10-22T14:26:29Z" },
            "limit_month_code": { "used_ratio": 0, "reset_time": "2026-10-22T14:26:29Z" }
        }
    }"#;

    fn mixed_weekly(counter_reset: &str, ratio_reset: &str, ratio: f64) -> Vec<u8> {
        // Based on the matching-nonzero fixture in KimiRatioPoolTests.swift
        // at CodexBar 8ab81e2eb. Vary the resets to distinguish matching clocks
        // within two seconds from unrelated reset periods.
        serde_json::to_vec(&serde_json::json!({
            "usage": {"limit": "100", "used": "19", "resetTime": counter_reset},
            "usages": {"limit_7d": {"used_ratio": ratio, "reset_time": ratio_reset}}
        }))
        .unwrap()
    }

    #[test]
    fn weekly_ratio_pool_decodes_upstream_fixture_in_primary() {
        let usage = normalize_usage(RATIO_LANES).unwrap();
        let weekly = usage.primary.unwrap();
        assert_eq!(weekly.used_percent, 12.5);
        assert_eq!(weekly.window_minutes, Some(WEEKLY_MINUTES));
        assert_eq!(weekly.window_kind, None);
        assert_eq!(weekly.resets_at.as_deref(), Some("2026-09-20T00:00:00Z"));
    }

    #[test]
    fn session_ratio_pool_decodes_upstream_fixture_in_secondary() {
        let usage = normalize_usage(RATIO_LANES).unwrap();
        let session = usage.secondary.unwrap();
        assert_eq!(session.used_percent, 62.5);
        assert_eq!(session.window_minutes, Some(FIVE_HOUR_MINUTES));
        assert_eq!(session.window_kind, None);
        assert_eq!(session.resets_at, None);
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn monthly_ratio_pool_decodes_upstream_fixture() {
        let usage = normalize_usage(CONTRADICTORY_POOLS).unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        let monthly = &extras[0];
        assert_eq!(monthly.id.as_deref(), Some("kimi-monthly"));
        assert_eq!(monthly.title.as_deref(), Some("Total usage"));
        let window = monthly.window.as_ref().unwrap();
        assert!((window.used_percent - 55.31).abs() < 0.00001);
        assert_eq!(window.resets_at.as_deref(), Some("2026-10-22T14:26:29Z"));
        assert_eq!(window.window_kind.as_deref(), Some("monthly"));
        assert_eq!(window.window_minutes, None);
    }

    #[test]
    fn same_duration_resets_one_second_apart_publish_higher_counter() {
        let body = mixed_weekly("2026-09-19T16:45:59Z", "2026-09-19T16:45:58Z", 0.1869);
        let primary = normalize_usage(&body).unwrap().primary.unwrap();
        assert_eq!(primary.used_percent, 19.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-19T16:45:59Z"));
    }

    #[test]
    fn same_duration_resets_three_seconds_apart_do_not_merge() {
        // Fractional clocks must also remain distinct just beyond two seconds.
        for reset in ["2026-09-19T16:46:02Z", "2026-09-19T16:46:01.001Z"] {
            let body = mixed_weekly(reset, "2026-09-19T16:45:59Z", 0.1);
            let primary = normalize_usage(&body).unwrap().primary.unwrap();
            assert_eq!(
                primary.used_percent, 10.0,
                "different reset periods must retain the ratio"
            );
            assert_eq!(primary.resets_at.as_deref(), Some("2026-09-19T16:45:59Z"));
            let decoded: KimiCodeApiResponse = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                window_from_detail(decoded.usage.as_ref().unwrap(), Some(WEEKLY_MINUTES))
                    .unwrap()
                    .used_percent,
                19.0
            );
        }
    }

    #[test]
    fn reset_tolerance_includes_exactly_two_seconds() {
        let body = mixed_weekly("2026-09-19T16:46:01Z", "2026-09-19T16:45:59Z", 0.1);
        assert_eq!(
            normalize_usage(&body)
                .unwrap()
                .primary
                .unwrap()
                .used_percent,
            19.0
        );
    }

    #[test]
    fn different_duration_does_not_merge_session_counter() {
        let body = br#"{
            "usage": {"limit": "100", "used": "19"},
            "limits": [{"window": {"duration": 120, "timeUnit": "TIME_UNIT_MINUTE"},
                "detail": {"limit": "100", "used": "80", "resetTime": "2026-09-19T14:45:59Z"}}],
            "usages": {"limit_5h": {"used_ratio": 0.1, "reset_time": "2026-09-19T14:45:59Z"}}
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 19.0);
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 10.0);
        assert_eq!(secondary.window_minutes, Some(300));
    }

    #[test]
    fn unparseable_reset_does_not_merge_counter() {
        for (counter_reset, ratio_reset) in [
            ("invalid", "2026-09-19T16:45:59Z"),
            ("2026-09-19T16:45:59Z", "invalid"),
        ] {
            let body = mixed_weekly(counter_reset, ratio_reset, 0.1);
            assert_eq!(
                normalize_usage(&body)
                    .unwrap()
                    .primary
                    .unwrap()
                    .used_percent,
                10.0
            );
        }
    }

    #[test]
    fn ratio_higher_than_counter_stays_authoritative() {
        let body = mixed_weekly("2026-09-19T16:45:59Z", "2026-09-19T16:45:58Z", 0.5);
        let primary = normalize_usage(&body).unwrap().primary.unwrap();
        assert_eq!(
            primary.used_percent, 50.0,
            "higher ratio must not be replaced by the counter"
        );
        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-19T16:45:58Z"));
    }

    #[test]
    fn monthly_pool_does_not_suppress_weekly_or_session_correction() {
        // Weekly fixture from KimiRatioPoolTests.swift at CodexBar 8ab81e2eb:
        // monthly usage must not suppress the higher matching weekly counter.
        let weekly = normalize_usage(
            br#"{
            "usage":{"limit":"100","used":"19","resetTime":"2026-09-19T16:45:59Z"},
            "usages":{"limit_7d":{"used_ratio":0,"reset_time":"2026-09-19T16:45:59Z"},
                      "limit_month_total":{"used_ratio":0.0313}}
        }"#,
        )
        .unwrap();
        assert_eq!(weekly.primary.unwrap().used_percent, 19.0);
        let session = normalize_usage(CONTRADICTORY_POOLS).unwrap();
        assert!(session.primary.is_none());
        assert_eq!(session.secondary.unwrap().used_percent, 100.0);
    }

    #[test]
    fn monthly_pool_publishes_separately_without_weekly_or_session() {
        // Monthly-only fixture from KimiRatioPoolTests.swift at CodexBar 8ab81e2eb:
        // no weekly or session reading exists to fill those slots.
        let usage =
            normalize_usage(br#"{"usages":{"limit_month_total":{"used_ratio":1.05}}}"#).unwrap();
        assert!(
            usage.primary.is_none(),
            "monthly pool must not become the weekly primary"
        );
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].id.as_deref(), Some("kimi-monthly"));
        assert_eq!(extras[0].window.as_ref().unwrap().used_percent, 100.0);
    }

    #[test]
    fn no_usages_retains_complete_legacy_fixture() {
        // Extend the existing string-valued weekly counter fixture with the
        // first limits entry, and pin every published field without ratio pools.
        let usage = normalize_usage(br#"{
            "usage": {"limit":"100","used":"25","remaining":"75","resetTime":"2026-07-01T12:00:00Z"},
            "limits": [{"detail":{"limit":"200","used":"50","resetTime":"2026-07-01T13:00:00Z"}}]
        }"#).unwrap();
        let expected_window = |minutes, reset: &str| RateWindow {
            window_kind: None,
            used_percent: 25.0,
            raw_used_percent: None,
            resets_at: Some(reset.to_string()),
            window_minutes: Some(minutes),
            used_count: None,
            total_count: None,
            regeneration: None,
            breakdown: None,
        };
        let expected = Usage {
            primary: Some(expected_window(WEEKLY_MINUTES, "2026-07-01T12:00:00Z")),
            secondary: Some(expected_window(FIVE_HOUR_MINUTES, "2026-07-01T13:00:00Z")),
            tertiary: None,
            extra_rate_windows: None,
        };
        assert_eq!(
            serde_json::to_value(usage).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        // Preserve the legacy rounding and overage behavior when no pools exist.
        assert_eq!(
            normalize_usage(br#"{"usage":{"limit":3,"used":4}}"#)
                .unwrap()
                .primary
                .unwrap()
                .used_percent,
            133.33
        );
    }

    #[test]
    fn malformed_pool_is_skipped_without_discarding_windows() {
        let base = serde_json::json!({
            "usage": {"limit": "100", "used": "25"},
            "usages": {
                "limit_5h": {"used_ratio": 0.625},
                "limit_7d": {"used_ratio": 0.125},
                "limit_month_total": {"used_ratio": 0.42}
            }
        });
        for field in ["limit_5h", "limit_7d", "limit_month_total"] {
            for malformed in [
                serde_json::json!(false),
                serde_json::json!({"used_ratio":"bad"}),
                serde_json::json!({"used_ratio":-0.5}),
                serde_json::json!({"used_ratio":0.2, "reset_time":{}}),
                serde_json::json!({}),
            ] {
                let mut body = base.clone();
                body["usages"][field] = malformed;
                let (usage, refusals) =
                    normalize_with_pool_refusals(&serde_json::to_vec(&body).unwrap()).unwrap();
                assert_eq!(
                    refusals,
                    vec![match field {
                        "limit_5h" => "usages.limit_5h",
                        "limit_7d" => "usages.limit_7d",
                        _ => "usages.limit_month_total",
                    }]
                );
                assert_eq!(
                    usage.primary.unwrap().used_percent,
                    if field == "limit_7d" { 25.0 } else { 12.5 }
                );
                assert_eq!(usage.secondary.is_some(), field != "limit_5h");
                assert_eq!(
                    usage.extra_rate_windows.is_some(),
                    field != "limit_month_total"
                );
            }
        }
        for usages in [
            serde_json::json!(null),
            serde_json::json!("bad"),
            serde_json::json!({}),
        ] {
            let mut body = base.clone();
            body["usages"] = usages;
            assert_eq!(
                normalize_usage(&serde_json::to_vec(&body).unwrap())
                    .unwrap()
                    .primary
                    .unwrap()
                    .used_percent,
                25.0
            );
        }
    }

    #[test]
    fn pool_refusals_change_once_and_are_scoped_to_handle() {
        let provider = KimiForCodingProvider::new();
        assert!(!provider.update_pool_refusals("a", &[]));
        assert!(provider.update_pool_refusals("a", &["usages.limit_7d"]));
        assert!(!provider.update_pool_refusals("a", &["usages.limit_7d"]));
        assert!(provider.update_pool_refusals("b", &["usages.limit_7d"]));
        assert!(provider.update_pool_refusals("a", &["usages.limit_5h"]));
        assert!(provider.update_pool_refusals("a", &[]));
        assert!(!provider.update_pool_refusals("a", &[]));
        assert!(provider.update_pool_refusals("a", &["usages.limit_5h"]));
        assert!(!provider.update_pool_refusals("b", &["usages.limit_7d"]));
    }

    #[test]
    fn legacy_limit_window_uses_stated_duration_and_units() {
        for (duration, unit, expected) in [
            (120, "TIME_UNIT_MINUTE", Some(120)),
            (5, "TIME_UNIT_HOUR", Some(300)),
            (1, "TIME_UNIT_DAY", Some(1440)),
            (0, "TIME_UNIT_MINUTE", None),
            (300, "TIME_UNIT_UNKNOWN", None),
            (i64::MAX, "TIME_UNIT_DAY", None),
        ] {
            let body = serde_json::json!({
                "usage": {"limit": 100, "used": 25},
                "limits": [{"window":{"duration":duration,"timeUnit":unit},
                    "detail":{"limit":100,"used":50}}]
            });
            let secondary = normalize_usage(&serde_json::to_vec(&body).unwrap())
                .unwrap()
                .secondary
                .unwrap();
            assert_eq!(secondary.used_percent, 50.0);
            assert_eq!(secondary.window_minutes, expected);
        }
    }

    #[test]
    fn unreliable_counter_does_not_override_ratio_and_overage_is_clamped() {
        for (counts, expected) in [
            (serde_json::json!({"used":-1}), 10.0),
            (serde_json::json!({"remaining":-1}), 10.0),
            (serde_json::json!({"remaining":101}), 10.0),
            (serde_json::json!({"used":150}), 100.0),
            (serde_json::json!({"used":-1,"remaining":20}), 80.0),
        ] {
            let mut body: serde_json::Value = serde_json::from_slice(&mixed_weekly(
                "2026-09-19T16:45:59Z",
                "2026-09-19T16:45:59Z",
                0.1,
            ))
            .unwrap();
            body["usage"].as_object_mut().unwrap().remove("used");
            body["usage"]
                .as_object_mut()
                .unwrap()
                .extend(counts.as_object().unwrap().clone());
            assert_eq!(
                normalize_usage(&serde_json::to_vec(&body).unwrap())
                    .unwrap()
                    .primary
                    .unwrap()
                    .used_percent,
                expected
            );
        }
    }

    #[test]
    fn equal_readings_keep_ratio_reset_metadata() {
        let body = mixed_weekly("2026-09-19T16:45:59Z", "2026-09-19T16:45:58Z", 0.19);
        let primary = normalize_usage(&body).unwrap().primary.unwrap();
        assert_eq!(primary.used_percent, 19.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-19T16:45:58Z"));
    }

    #[test]
    fn web_enrichment_cannot_duplicate_or_replace_coding_monthly_pool() {
        let mut usage = normalize_usage(CONTRADICTORY_POOLS).unwrap();
        merge_extras(
            &mut usage,
            parse_subscription_extras(
                br#"{
            "subscriptionBalance":{"amountUsedRatio":0.99},
            "ratelimitCode7d":{"ratio":0.2}
        }"#,
            ),
        );
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].id.as_deref(), Some("kimi-monthly"));
        assert!((extras[0].window.as_ref().unwrap().used_percent - 55.31).abs() < 0.00001);
        assert_eq!(extras[1].id.as_deref(), Some("kimi-code-7d"));
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
        assert_eq!(code_7d.window_kind.as_deref(), Some("weekly"));
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
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("kimi-for-coding", "oauth")]);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = KimiForCodingProvider::new_with_handle_loader(Some(source), loader);
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 1, "{handles:?}");
        assert_eq!(handles[0].stable_id(), "kimi-for-coding");
        assert!(handles[0].is_vault());
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
        // The prior account's USAGE must not survive an unverified identity. The
        // entry itself does survive, as a verdict carrying no account and no
        // windows: dropping it too publishes the ABSENT shape for a credential
        // this module reached and found unusable, which a consumer reads as "not
        // fetched yet" (insula#8, measured).
        let entry = next.entry.as_ref().expect("the verdict stays visible");
        assert!(entry.usage.is_none(), "no window may cross an identity");
        assert!(entry.account.is_none(), "and it attributes nothing");
        assert!(entry.error.is_some(), "the failure is stated, not implied");
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
        assert!(matches!(
            vault.usage,
            Err(FetchError::ProviderStatus(401, _))
        ));
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
            Err(FetchError::Unauthorized(message)) if message == "HTTP 401 (no response body)"
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

    /// Serves a fixed cookie header for any scoped id and records which ids
    /// were read, so a test can tell whether the enrichment touched the vault.
    struct WebCookieSource {
        read: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CredentialSource for WebCookieSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            Err(VaultGetError::FailClosed)
        }

        async fn get_scoped(
            &self,
            credential_id: &str,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            self.read.lock().unwrap().push(credential_id.to_string());
            Ok(credential(b"tracker=1; kimi-auth=web-token", 1))
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            _provider_status: u16,
            _record_version: u64,
        ) {
        }
    }

    fn enrichment_provider(rows: &[(&str, &str)]) -> (KimiForCodingProvider, Arc<WebCookieSource>) {
        let source = Arc::new(WebCookieSource {
            read: Mutex::new(Vec::new()),
        });
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(rows);
        let provider = KimiForCodingProvider::new_with_handle_loader(
            Some(Arc::clone(&source) as Arc<dyn CredentialSource>),
            loader,
        );
        (provider, source)
    }

    /// A `cookie:kimi.com` deposit supplies the console token, read by id
    /// without becoming a handle of this provider.
    #[tokio::test]
    async fn a_kimi_web_deposit_supplies_the_enrichment_token() {
        let (provider, source) = enrichment_provider(&[
            ("apikey:kimi-for-coding", "apikey"),
            ("cookie:kimi.com", "cookie"),
            ("cookie:kimi.com:ufuk", "cookie"),
        ]);
        assert_eq!(
            provider.web_enrichment_token().await.as_deref(),
            Some("web-token")
        );
        // The suffixed deposit outranks the bare one, as for the cookie providers.
        assert_eq!(*source.read.lock().unwrap(), vec!["cookie:kimi.com:ufuk"]);
        // Not a lane: the handles are the coding key's alone.
        let handles = provider.handles().unwrap();
        assert!(
            handles
                .iter()
                .all(|handle| handle.vault_credential_id() == Some("apikey:kimi-for-coding")),
            "the web deposit must not become a slot: {handles:?}"
        );
    }

    /// No deposit, no console request: the enrichment is skipped, and the vault
    /// is never asked.
    #[tokio::test]
    async fn without_a_kimi_web_deposit_the_enrichment_is_skipped() {
        let (provider, source) = enrichment_provider(&[("apikey:kimi-for-coding", "apikey")]);
        assert_eq!(provider.web_enrichment_token().await, None);
        assert!(source.read.lock().unwrap().is_empty());
    }

    /// Only a non-empty `kimi-auth` pair is a token.
    #[test]
    fn the_token_is_the_kimi_auth_value() {
        assert_eq!(
            kimi_auth_token("a=1; kimi-auth=t0k=; b=2").as_deref(),
            Some("t0k=")
        );
        assert_eq!(kimi_auth_token("a=1; kimi-auth="), None);
        assert_eq!(kimi_auth_token("a=1"), None);
    }
}
