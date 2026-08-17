//! StepFun usage fetcher — credential from `STEPFUN_TOKEN` (headless env path only).
//!
//! POST `QueryStepPlanRateLimit` with Oasis cookie auth. Username/password login
//! (RegisterDevice + SignInByPassword) is intentionally out of scope for v1.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `STEPFUN_TOKEN` available. Endpoint, request body `{}`, base headers, Cookie
//! (`Oasis-Token` + `Oasis-Webid`), and response fields ported from CodexBar
//! (`Sources/CodexBarCore/Providers/StepFun/StepFunUsageFetcher.swift:214-221,
//! 227-234, 393-401, 465-491, 141-158`; `StepFunSettingsReader.swift:6,20-24`).
//! `five_hour_usage_left_rate` / `weekly_usage_left_rate` are 0..1 fractions
//! (CodexBar: `(1.0 - left_rate) * 100` at lines 143-152). Reset times are Unix
//! seconds (string or integer JSON). Rides the live-proven `http.rs`.
//!
//! Webid fidelity: the `oasis-webid` header and the `Oasis-Webid=` cookie value
//! are derived from the token's JWT `device_id` claim (the refresh half of an
//! "access...refresh" pair, else a bare JWT), matching CodexBar's
//! `webID(forToken:)` (StepFunUsageFetcher.swift:357-400). The server ties the
//! webid to the session device, so a hard-coded webid risks rejection; deriving
//! it keeps the request faithful. A token with no `device_id` (an opaque
//! `STEPFUN_TOKEN`) falls back to the static `OASIS_WEB_ID`, i.e. the
//! pre-derivation behavior, so an opaque token never regresses.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "stepfun";
const TOKEN_ENV: &[&str] = &["STEPFUN_TOKEN"];
const API_URL: &str =
    "https://platform.stepfun.com/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit";
/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Fallback device identifier, used only when the token carries no `device_id`
/// claim.
const OASIS_WEB_ID: &str = "c8a1002d2c457e758785a9979832217c7c0b884c";
const OASIS_APP_ID: &str = "10300";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Deserialize)]
struct RateLimitResponse {
    status: Option<i64>,
    code: Option<i64>,
    message: Option<String>,
    #[serde(
        rename = "five_hour_usage_left_rate",
        deserialize_with = "deserialize_optional_flexible_f64",
        default
    )]
    five_hour_usage_left_rate: Option<f64>,
    #[serde(
        rename = "weekly_usage_left_rate",
        deserialize_with = "deserialize_optional_flexible_f64",
        default
    )]
    weekly_usage_left_rate: Option<f64>,
    #[serde(
        rename = "five_hour_usage_reset_time",
        deserialize_with = "deserialize_optional_flexible_i64",
        default
    )]
    five_hour_usage_reset_time: Option<i64>,
    #[serde(
        rename = "weekly_usage_reset_time",
        deserialize_with = "deserialize_optional_flexible_i64",
        default
    )]
    weekly_usage_reset_time: Option<i64>,
    #[serde(
        rename = "plan_family",
        deserialize_with = "deserialize_optional_flexible_f64",
        default
    )]
    plan_family: Option<f64>,
    #[serde(rename = "plan_credit_rate_limit", default)]
    plan_credit_rate_limit: Option<CreditRateLimit>,
}

#[derive(Debug, Deserialize)]
struct CreditRateLimit {
    #[serde(
        rename = "subscription_credit_left_rate",
        deserialize_with = "deserialize_optional_flexible_f64",
        default
    )]
    subscription_credit_left_rate: Option<f64>,
    #[serde(
        rename = "subscription_credit_reset_time",
        deserialize_with = "deserialize_optional_flexible_i64",
        default
    )]
    subscription_credit_reset_time: Option<i64>,
    #[serde(
        rename = "topup_credit_left_rate",
        deserialize_with = "deserialize_optional_flexible_f64",
        default
    )]
    topup_credit_left_rate: Option<f64>,
    #[serde(rename = "credit_buckets", default)]
    credit_buckets: Option<Vec<CreditBucket>>,
}

#[derive(Debug, Deserialize)]
struct CreditBucket {
    #[serde(
        rename = "credit_total",
        deserialize_with = "deserialize_optional_flexible_f64",
        default
    )]
    credit_total: Option<f64>,
    #[serde(
        rename = "credit_residual",
        deserialize_with = "deserialize_optional_flexible_f64",
        default
    )]
    credit_residual: Option<f64>,
}

impl RateLimitResponse {
    /// Whether this account is metered by a credit pool rather than by rolling
    /// windows.
    ///
    /// Two billing models run side by side: one meters rolling five-hour and
    /// weekly windows, the other a credit pool whose rate fields come back as
    /// zero with a reset time of `0` -- meaning "no window configured", not
    /// "fully consumed".
    ///
    /// Classified by the shape the payload carries, with `plan_family` only
    /// breaking a tie. A live window -- one stating a real reset time -- is
    /// decisive on its own, because the two mistakes are not equally cheap: a
    /// windowed account read as credit-metered loses the windows it is actually
    /// paced by, and they vanish from the wire entirely. The reverse mistake
    /// requires the upstream to state a live reset, in which case publishing
    /// that window is right regardless of how the plan is labelled.
    ///
    /// That ordering also means a future change to the family numbering cannot
    /// silently move a windowed account onto the credit reading.
    fn is_credit_plan(&self) -> bool {
        let has_live_window = self.five_hour_usage_reset_time.unwrap_or(0) > 0
            || self.weekly_usage_reset_time.unwrap_or(0) > 0;
        if has_live_window {
            return false;
        }

        let has_credit_pool = self.plan_credit_rate_limit.as_ref().is_some_and(|credit| {
            credit.subscription_credit_left_rate.is_some()
                || credit.topup_credit_left_rate.is_some()
        });
        if has_credit_pool {
            return true;
        }

        // Neither a live window nor a credit pool: nothing in the payload
        // decides, so fall back to the plan's own label.
        self.plan_family
            .is_some_and(|family| family.is_finite() && family == 2.0)
    }
}

impl CreditRateLimit {
    fn credit_left_rate(&self) -> Option<f64> {
        if let Some(buckets) = self
            .credit_buckets
            .as_ref()
            .filter(|buckets| !buckets.is_empty())
        {
            let balances: Option<Vec<(f64, f64)>> = buckets
                .iter()
                .map(|bucket| {
                    let total = bucket.credit_total?;
                    let residual = bucket.credit_residual?;
                    (total.is_finite() && residual.is_finite() && total > 0.0)
                        .then_some((total, residual))
                        .filter(|(_, residual)| *residual >= 0.0 && *residual <= total)
                })
                .collect();
            if let Some(balances) = balances {
                let total: f64 = balances.iter().map(|(total, _)| total).sum();
                let residual: f64 = balances.iter().map(|(_, residual)| residual).sum();
                if total > 0.0 {
                    return Some(residual / total);
                }
            }
        }
        self.subscription_credit_left_rate
            .or(self.topup_credit_left_rate)
    }

    fn reset_time(&self) -> Option<i64> {
        // If the reset time is absent or invalid, return no reset timestamp rather
        // than fabricating a dummy value; credit buckets only affect rate math.
        self.subscription_credit_reset_time
            .filter(|reset| *reset > 0)
    }
}

fn deserialize_optional_flexible_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(flexible_f64))
}

fn deserialize_optional_flexible_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(value.and_then(flexible_i64))
}

fn flexible_f64(value: serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn flexible_i64(value: serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Fraction remaining (0..1) → percent consumed, matching CodexBar `toUsageSnapshot`.
fn used_percent_from_left_rate(left_rate: f64) -> f64 {
    ((1.0 - left_rate) * 100.0).clamp(0.0, 100.0)
}

fn rate_window_from_left_and_reset(
    left_rate: Option<f64>,
    reset_secs: Option<i64>,
    window_minutes: i64,
) -> Option<RateWindow> {
    let left = left_rate?;
    // Credit plans report zero reset times for these legacy fields: no window is
    // configured, rather than a fully consumed window.
    let reset = reset_secs.filter(|&s| s > 0)?;
    let resets_at = env::epoch_to_iso8601(reset)?;
    Some(RateWindow {
        used_percent: used_percent_from_left_rate(left),
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
    })
}

fn credit_window(credit: Option<&CreditRateLimit>) -> Option<RateWindow> {
    let credit = credit?;
    let credit_rate = credit.credit_left_rate()?;
    if !credit_rate.is_finite() {
        return None;
    }
    Some(RateWindow {
        used_percent: ((1.0 - credit_rate) * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: credit.reset_time().and_then(env::epoch_to_iso8601),
        window_minutes: None,
        used_count: None,
        total_count: None,
    })
}

/// Normalize the rate-limit response body to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: RateLimitResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("stepfun rate limit not decodable: {e}")))?;

    if response.status != Some(1) {
        let msg = response
            .message
            .or_else(|| response.code.map(|c| c.to_string()))
            .unwrap_or_else(|| "unknown".to_string());
        return Err(FetchError::Upstream(format!("stepfun API error: {msg}")));
    }

    if response.is_credit_plan() {
        return Ok(Usage {
            primary: credit_window(response.plan_credit_rate_limit.as_ref()),
            secondary: None,
            tertiary: None,
            extra_rate_windows: None,
        });
    }

    let primary = rate_window_from_left_and_reset(
        response.five_hour_usage_left_rate,
        response.five_hour_usage_reset_time,
        300,
    );
    let secondary = rate_window_from_left_and_reset(
        response.weekly_usage_left_rate,
        response.weekly_usage_reset_time,
        10080,
    );

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The StepFun usage provider.
pub struct StepFunProvider {
    http: reqwest::Client,
}

impl StepFunProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for StepFunProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Derive the Oasis-Webid from a token, matching CodexBar's `webID(forToken:)`.
///
/// The token is either a bare JWT or an "access...refresh" pair; the `device_id`
/// lives in the refresh half, so the halves are scanned in reverse. A token with
/// no decodable `device_id` (e.g. an opaque `STEPFUN_TOKEN`) falls back to the
/// static [`OASIS_WEB_ID`], preserving the pre-derivation request shape.
fn webid_for_token(token: &str) -> String {
    for half in token.rsplit("...") {
        if let Some(device_id) = extract_device_id(half) {
            if !device_id.is_empty() {
                return device_id;
            }
        }
    }
    OASIS_WEB_ID.to_string()
}

/// Base64url-decode a JWT's payload (no signature verification) and return its
/// `device_id` claim, if any. Returns `None` for anything that is not a JWT with
/// a string `device_id`.
fn extract_device_id(jwt: &str) -> Option<String> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    // JWT payloads are unpadded base64url; strip any stray padding so the NO_PAD
    // engine accepts both padded and unpadded forms.
    let payload = payload.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("device_id")?.as_str().map(str::to_string)
}

#[async_trait]
impl UsageProvider for StepFunProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let token = env::first_env(TOKEN_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {TOKEN_ENV:?} is set")))?;

            // Derive the webid from the token's device_id so the header and the
            // Oasis-Webid cookie agree with the session (see webid_for_token).
            let webid = webid_for_token(&token);
            let cookie = format!("Oasis-Token={token}; Oasis-Webid={webid}");
            let body = b"{}".to_vec();

            let response = JsonRequest::post_json(API_URL, body)
                .header(Header::new("content-type", "application/json"))
                .header(Header::new("oasis-appid", OASIS_APP_ID))
                .header(Header::new("oasis-platform", "web"))
                .header(Header::new("oasis-webid", webid))
                .header(Header::new("User-Agent", USER_AGENT))
                .header(Header::new("Cookie", cookie))
                .timeout(REQUEST_TIMEOUT)
                .send(&self.http)
                .await?;

            let usage = normalize_usage(&response)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_five_hour_and_weekly_windows() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0.75,
            "weekly_usage_left_rate": 1,
            "five_hour_usage_reset_time": 1782135879,
            "weekly_usage_reset_time": "1782740679"
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T13:44:39Z"));
        assert_eq!(primary.window_minutes, Some(300));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 0.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-06-29T13:44:39Z"));
        assert_eq!(secondary.window_minutes, Some(10080));
    }

    #[test]
    fn converts_fraction_remaining_to_used_percent() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0.99781543,
            "weekly_usage_left_rate": 0.5,
            "five_hour_usage_reset_time": 1782135879,
            "weekly_usage_reset_time": 1782135879
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        let expected = ((1.0_f64 - 0.99781543) * 100.0).clamp(0.0, 100.0);
        assert!((primary.used_percent - expected).abs() < 1e-6);
        assert_eq!(usage.secondary.unwrap().used_percent, 50.0);
    }

    #[test]
    fn integer_left_rate_treated_as_fraction() {
        // CodexBar StepFunFlexibleNumber: `1` means 100% remaining → 0% used.
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 1,
            "weekly_usage_left_rate": 0,
            "five_hour_usage_reset_time": 1782135879,
            "weekly_usage_reset_time": 1782135879
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 0.0);
        assert_eq!(usage.secondary.unwrap().used_percent, 100.0);
    }

    #[test]
    fn drops_window_when_reset_missing_or_zero() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0.5,
            "weekly_usage_left_rate": 0.5,
            "five_hour_usage_reset_time": 0,
            "weekly_usage_reset_time": 1782135879
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_some());
    }

    #[test]
    fn zero_rate_windows_with_zero_resets_are_not_an_error() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0,
            "weekly_usage_left_rate": 0,
            "five_hour_usage_reset_time": 0,
            "weekly_usage_reset_time": 0
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn credit_plan_emits_credit_window_instead_of_legacy_windows() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0,
            "weekly_usage_left_rate": 0,
            "five_hour_usage_reset_time": 0,
            "weekly_usage_reset_time": 0,
            "plan_family": 2,
            "plan_credit_rate_limit": {
                "subscription_credit_left_rate": 0.75,
                "subscription_credit_reset_time": "1782135879",
                "credit_buckets": [{"next_reset_at": "1782000000"}]
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T13:44:39Z"));
        assert_eq!(primary.window_minutes, None);
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn credit_reset_without_subscription_reset_stays_absent() {
        let body = br#"{
            "status": 1,
            "plan_family": 2,
            "plan_credit_rate_limit": {
                "subscription_credit_left_rate": 0.5,
                "credit_buckets": [{"next_reset_at": 1782135879}]
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.unwrap().resets_at.is_none());
    }

    #[test]
    fn absent_plan_family_uses_zero_window_credit_heuristic() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0,
            "weekly_usage_left_rate": 0,
            "five_hour_usage_reset_time": 0,
            "weekly_usage_reset_time": 0,
            "plan_credit_rate_limit": {
                "subscription_credit_left_rate": 0.6,
                "subscription_credit_reset_time": 1782135879
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 40.0);
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn absent_plan_family_with_healthy_windows_keeps_legacy_usage() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0.8,
            "weekly_usage_left_rate": 0.6,
            "five_hour_usage_reset_time": 1782135879,
            "weekly_usage_reset_time": 1782135879,
            "plan_credit_rate_limit": {
                "subscription_credit_left_rate": 0.6,
                "subscription_credit_reset_time": 1782135879
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!((usage.primary.unwrap().used_percent - 20.0).abs() < 1e-9);
        assert!((usage.secondary.unwrap().used_percent - 40.0).abs() < 1e-9);
    }

    /// A credit pool is read as one even when the plan is labelled otherwise.
    ///
    /// The payload here states no live window and a real credit balance, so the
    /// only quota it describes is the pool. Deferring to the `plan_family`
    /// label would publish nothing at all for an account that has a stated
    /// balance, and an account with no windows on the wire is indistinguishable
    /// from one nobody configured.
    #[test]
    fn a_credit_pool_is_published_even_when_the_plan_is_labelled_windowed() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0,
            "weekly_usage_left_rate": 0,
            "five_hour_usage_reset_time": 0,
            "weekly_usage_reset_time": 0,
            "plan_family": 1,
            "plan_credit_rate_limit": {
                "subscription_credit_left_rate": 0.5,
                "subscription_credit_reset_time": 1782135879
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 50.0);
        assert!(usage.secondary.is_none());
    }

    /// With neither a live window nor a credit pool, nothing in the payload
    /// decides, and the plan's own label is the only evidence left.
    #[test]
    fn the_plan_label_decides_only_when_the_payload_states_neither() {
        let windowed = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0,
            "weekly_usage_left_rate": 0,
            "five_hour_usage_reset_time": 0,
            "weekly_usage_reset_time": 0,
            "plan_family": 1
        }"#;
        let usage = normalize_usage(windowed).unwrap();
        assert!(usage.primary.is_none(), "no window is configured yet");
        assert!(usage.secondary.is_none());

        let credit = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0,
            "weekly_usage_left_rate": 0,
            "five_hour_usage_reset_time": 0,
            "weekly_usage_reset_time": 0,
            "plan_family": 2
        }"#;
        let usage = normalize_usage(credit).unwrap();
        assert!(usage.primary.is_none(), "no credit pool is stated yet");
        assert!(usage.secondary.is_none());
    }

    /// A live window decides on its own, whatever the plan is labelled.
    ///
    /// This is the shape the two orderings disagree on. Trusting the label
    /// first would discard rolling windows the upstream says are live, and
    /// those are what the account is actually paced by.
    #[test]
    fn a_live_window_is_published_even_when_the_plan_is_labelled_credit() {
        let body = br#"{
            "status": 1,
            "five_hour_usage_left_rate": 0.8,
            "weekly_usage_left_rate": 0.6,
            "five_hour_usage_reset_time": 1782135879,
            "weekly_usage_reset_time": 1782740679,
            "plan_family": 2,
            "plan_credit_rate_limit": {
                "subscription_credit_left_rate": 0.5,
                "subscription_credit_reset_time": 1782135879
            }
        }"#;
        let usage = normalize_usage(body).unwrap();

        let primary = usage.primary.expect("the live five-hour window");
        assert!((primary.used_percent - 20.0).abs() < 1e-9);
        assert_eq!(primary.window_minutes, Some(300));
        let secondary = usage.secondary.expect("the live weekly window");
        assert!((secondary.used_percent - 40.0).abs() < 1e-9);
        assert_eq!(secondary.window_minutes, Some(10080));
    }

    #[test]
    fn api_non_success_is_upstream() {
        let body = br#"{ "status": 0, "message": "bad" }"#;
        assert!(matches!(
            normalize_usage(body),
            Err(FetchError::Upstream(_))
        ));
    }

    #[test]
    fn garbage_body_is_decode_error() {
        assert!(matches!(
            normalize_usage(b"not json"),
            Err(FetchError::Decode(_))
        ));
    }

    fn jwt_payload(payload: &str) -> String {
        use base64::Engine as _;
        // header.payload.sig with an unpadded base64url payload, like a real JWT.
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("hdr.{enc}.sig")
    }

    #[test]
    fn webid_derives_device_id_from_a_bare_jwt() {
        let jwt = jwt_payload(r#"{"device_id":"dev-123","sub":"x"}"#);
        assert_eq!(webid_for_token(&jwt), "dev-123");
    }

    #[test]
    fn webid_derives_device_id_from_the_refresh_half_of_a_pair() {
        let refresh = jwt_payload(r#"{"device_id":"refresh-dev"}"#);
        let token = format!("opaque-access...{refresh}");
        assert_eq!(webid_for_token(&token), "refresh-dev");
    }

    #[test]
    fn webid_falls_back_to_static_when_token_has_no_device_id() {
        // An opaque STEPFUN_TOKEN (no JWT, no device_id) keeps the pre-derivation
        // static webid, so an opaque token never regresses.
        assert_eq!(webid_for_token("opaque-token-no-jwt"), OASIS_WEB_ID);
        let jwt = jwt_payload(r#"{"sub":"no-device-id-here"}"#);
        assert_eq!(webid_for_token(&jwt), OASIS_WEB_ID);
    }

    #[test]
    fn webid_falls_back_when_device_id_is_empty() {
        let jwt = jwt_payload(r#"{"device_id":""}"#);
        assert_eq!(webid_for_token(&jwt), OASIS_WEB_ID);
    }
}
