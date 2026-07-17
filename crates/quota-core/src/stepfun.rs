//! StepFun usage fetcher — credential from `STEPFUN_TOKEN` (headless env path only).
//!
//! POST `QueryStepPlanRateLimit` with Oasis cookie auth. Username/password login
//! (RegisterDevice + SignInByPassword) is intentionally out of scope for v1.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `STEPFUN_TOKEN` available. Endpoint, request body `{}`, base headers, Cookie
//! (`Oasis-Token` + fixed `Oasis-Webid`), and response fields ported from CodexBar
//! (`Sources/CodexBarCore/Providers/StepFun/StepFunUsageFetcher.swift:214-221,
//! 227-234, 393-401, 465-491, 141-158`; `StepFunSettingsReader.swift:6,20-24`).
//! `five_hour_usage_left_rate` / `weekly_usage_left_rate` are 0..1 fractions
//! (CodexBar: `(1.0 - left_rate) * 100` at lines 143-152). Reset times are Unix
//! seconds (string or integer JSON). Rides the live-proven `http.rs`.

use std::time::Duration;

use async_trait::async_trait;
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
    let reset = reset_secs.filter(|&s| s > 0)?;
    let resets_at = env::epoch_to_iso8601(reset)?;
    Some(RateWindow {
        used_percent: used_percent_from_left_rate(left),
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: Some(window_minutes),
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

#[async_trait]
impl UsageProvider for StepFunProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let token = env::first_env(TOKEN_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {TOKEN_ENV:?} is set")))?;

            let cookie = format!("Oasis-Token={token}; Oasis-Webid={OASIS_WEB_ID}");
            let body = b"{}".to_vec();

            let response = JsonRequest::post_json(API_URL, body)
                .header(Header::new("content-type", "application/json"))
                .header(Header::new("oasis-appid", OASIS_APP_ID))
                .header(Header::new("oasis-platform", "web"))
                .header(Header::new("oasis-webid", OASIS_WEB_ID))
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
}
