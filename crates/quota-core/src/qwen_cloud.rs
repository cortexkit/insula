//! Qwen Cloud token-plan usage — browser-cookie + console-gateway scrape.
//!
//! The token-plan quota is available only to an authenticated Qwen Cloud browser
//! session. Each scrape loads the token-plan page to obtain a fresh `SEC_TOKEN`,
//! then posts that token with the Chrome cookie jar through the ONE_CONSOLE
//! `IntlBroadScopeAspnGateway` gateway.
//!
//! VERIFICATION: fixture-verified from a live browser HAR capture of
//! `home.qwencloud.com`, not a CodexBar port (Qwen Cloud has no CodexBar
//! equivalent). The HAR verifies the
//! `zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage` endpoint via the
//! `IntlBroadScopeAspnGateway` console gateway, the Chrome cookie + per-page
//! `SEC_TOKEN` authentication, and the `per5HourPercentage`,
//! `per5HourResetTime`, `per1WeekPercentage`, and `per1WeekResetTime` response
//! fields. `per5HourPercentage` and `per1WeekPercentage` are interpreted as USED
//! fractions (0–1) from their field semantics and observed values. The shared
//! cookie transport in `browser_cookies.rs` is live-proven; the gateway call shape
//! is HAR-verified.
//!
//! Two hosts: the token-plan PAGE (`home.qwencloud.com`) is loaded only to extract
//! the per-session `SEC_TOKEN`; the quota call itself goes to the console data
//! gateway at `cs-data.qwencloud.com` (same-site, so the `qwencloud.com` cookies
//! apply), with `Origin: https://home.qwencloud.com`. Posting the quota action to
//! `home.qwencloud.com` instead returns an empty success — the gateway host matters.
//!
//! KNOWN LIMITATION (entitled view; no enforced signal in the console): the
//! `/usage` percentages are the *entitled* view — consumed usage divided by the
//! current tier cap from `/quota-config`. The console exposes NO enforced-cap,
//! absolute-used, or exhausted/limited field: the live `/usage`, `/quota-config`,
//! and `/subscription` field sets are identical whether a window is healthy or
//! walled (`/usage` returns only the four per*Percentage/per*ResetTime fields;
//! `/quota-config` returns every tier's `{five_hour, weekly}` cap; `/subscription`
//! returns specCode/remainingDays/status). Absolute consumed is derivable as
//! `percentage * quota-config[specCode].window`, but a stateless reader still
//! cannot tell which cap the inference edge enforces. Consequence: on a
//! mid-window plan upgrade, *if* the edge keeps enforcing the old cap until the
//! reset while the console already divides by the new one, the percentage would
//! read healthy while the edge 429s, and that desync is undetectable from any
//! console read (probing the inference edge is out: token-plan ToS forbids
//! automated calls). Whether the edge actually lags an upgrade is provider-
//! dependent and was NOT observed on 2026-07-21: the edge kept up, so post-upgrade
//! the weekly window was honestly healthy at ~26% (matching Alibaba's dashboard);
//! the 429 seen that day predated the upgrade and was genuine exhaustion, and the
//! post-upgrade failures were 403 model-entitlement on newly-onboarded models, not
//! quota. The limitation is therefore structural/latent — the console gives no
//! signal to detect an edge lag — so routing consumers that observe a 429 must
//! apply their own cooldown. This module reports the console's entitled figure,
//! the most accurate value the console provides.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies::{self, CookieError},
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "qwen-cloud";
const DOMAIN: &str = "qwencloud.com";
const TOKEN_PLAN_URL: &str =
    "https://home.qwencloud.com/billing/subscription/token-plan-individual";
const USAGE_URL: &str = "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const GATEWAY_PARAMS: &str = r#"{"Api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage","Data":{"cornerstoneParam":{"domain":"home.qwencloud.com","consoleSite":"QWENCLOUD","console":"ONE_CONSOLE","xsp_lang":"en-US","protocol":"V2","productCode":"p_efm"}},"V":"1.0"}"#;

const FIVE_HOUR_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;

#[derive(Debug, Deserialize)]
struct GatewayResponse {
    #[serde(rename = "successResponse")]
    success_response: Option<bool>,
    data: Option<GatewayData>,
}

#[derive(Debug, Deserialize)]
struct GatewayData {
    #[serde(rename = "DataV2")]
    data_v2: Option<DataV2>,
}

#[derive(Debug, Deserialize)]
struct DataV2 {
    data: Option<TokenPlanResult>,
}

#[derive(Debug, Deserialize)]
struct TokenPlanResult {
    msg: Option<String>,
    code: Option<String>,
    success: Option<bool>,
    data: Option<TokenPlanUsage>,
}

#[derive(Debug, Deserialize)]
struct TokenPlanUsage {
    #[serde(rename = "per5HourPercentage")]
    per_five_hour_percentage: Option<f64>,
    #[serde(rename = "per5HourResetTime")]
    per_five_hour_reset_time: Option<i64>,
    #[serde(rename = "per1WeekPercentage")]
    per_week_percentage: Option<f64>,
    #[serde(rename = "per1WeekResetTime")]
    per_week_reset_time: Option<i64>,
}

/// Extract the per-page CSRF token from Qwen Cloud's console configuration.
fn extract_sec_token(html: &str) -> Option<&str> {
    let remainder = html.split_once("SEC_TOKEN: \"")?.1;
    let token = remainder.split_once('"')?.0;
    (!token.is_empty()).then_some(token)
}

/// Convert the gateway's epoch-milliseconds reset value to the wire's UTC format.
fn epoch_ms_to_iso8601(epoch_ms: i64) -> Option<String> {
    (epoch_ms > 0)
        .then_some(epoch_ms / 1000)
        .and_then(env::epoch_to_iso8601)
}

/// Build one quota window from a used fraction. A percentage is load-bearing;
/// reset timestamps are preserved only when the gateway supplies a valid value.
fn window_from_fraction(
    used_fraction: Option<f64>,
    reset_epoch_ms: Option<i64>,
    window_minutes: i64,
) -> Option<RateWindow> {
    let used_percent = used_fraction.filter(|value| value.is_finite())? * 100.0;
    Some(RateWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: reset_epoch_ms.and_then(epoch_ms_to_iso8601),
        window_minutes: Some(window_minutes),
    })
}

fn degraded_response_error(result: Option<&TokenPlanResult>, reason: &str) -> FetchError {
    let detail = result
        .map(|result| {
            [result.code.as_deref(), result.msg.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": ")
        })
        .filter(|detail| !detail.is_empty());
    let suffix = detail
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    FetchError::Decode(format!("qwen-cloud {reason}{suffix}"))
}

/// Normalize the Qwen Cloud console-gateway response to [`Usage`].
///
/// This is pure so it can be fixture-tested without a browser session.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: GatewayResponse = serde_json::from_slice(body).map_err(|error| {
        FetchError::Decode(format!("qwen-cloud response not decodable: {error}"))
    })?;

    let result = response
        .data
        .as_ref()
        .and_then(|data| data.data_v2.as_ref())
        .and_then(|data_v2| data_v2.data.as_ref());

    if response.success_response != Some(true) {
        return Err(degraded_response_error(
            result,
            "gateway did not report success",
        ));
    }

    let result = result.ok_or_else(|| {
        degraded_response_error(None, "response is missing the token-plan result")
    })?;
    if result.success != Some(true) || result.code.as_deref() != Some("SUCCESS") {
        return Err(degraded_response_error(
            Some(result),
            "token-plan result did not report success",
        ));
    }

    let quota = result.data.as_ref().ok_or_else(|| {
        degraded_response_error(Some(result), "response is missing token-plan quota data")
    })?;
    let primary = window_from_fraction(
        quota.per_five_hour_percentage,
        quota.per_five_hour_reset_time,
        FIVE_HOUR_WINDOW_MINUTES,
    );
    let secondary = window_from_fraction(
        quota.per_week_percentage,
        quota.per_week_reset_time,
        WEEKLY_WINDOW_MINUTES,
    );
    if primary.is_none() && secondary.is_none() {
        return Err(FetchError::Decode(
            "qwen-cloud no quota windows found in response".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The Qwen Cloud token-plan usage provider.
pub struct QwenCloudProvider {
    http: reqwest::Client,
}

impl QwenCloudProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for QwenCloudProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for QwenCloudProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let jar = browser_cookies::chrome_cookies_for(DOMAIN).map_err(|error| match error {
                CookieError::NoStore | CookieError::NoCookie | CookieError::Unsupported => {
                    FetchError::NoSession(error.to_string())
                }
                CookieError::NoKeychainKey(_) | CookieError::Extract(_) => {
                    FetchError::Upstream(error.to_string())
                }
            })?;
            if !jar.has_cookie_named(|name| name == "login_qwencloud_ticket") {
                return Err(FetchError::NoSession(
                    "no Qwen Cloud login ticket in browser".to_string(),
                ));
            }

            let cookie_header = jar.header();
            let token_page = JsonRequest::get(TOKEN_PLAN_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Cookie", &cookie_header))
                .header(Header::new("User-Agent", BROWSER_USER_AGENT))
                .header(Header::new(
                    "Accept",
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ))
                .send(&self.http)
                .await?;
            let html = String::from_utf8_lossy(&token_page);
            let sec_token = extract_sec_token(&html).ok_or_else(|| {
                FetchError::Decode(
                    "qwen-cloud token-plan page did not include SEC_TOKEN".to_string(),
                )
            })?;

            let body = JsonRequest::post_form(
                USAGE_URL,
                &[
                    ("product", "sfm_bailian"),
                    ("action", "IntlBroadScopeAspnGateway"),
                    ("sec_token", sec_token),
                    ("region", "ap-southeast-1"),
                    ("params", GATEWAY_PARAMS),
                ],
            )
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", cookie_header))
            .header(Header::new("User-Agent", BROWSER_USER_AGENT))
            .header(Header::new("Origin", "https://home.qwencloud.com"))
            .header(Header::new("Referer", TOKEN_PLAN_URL))
            .send(&self.http)
            .await?;

            let usage = normalize_usage(&body)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HAR_RESPONSE: &str = r#"{
      "code": "200",
      "data": {
        "DataV2": {
          "ret": ["SUCCESS::接口调用成功"],
          "data": {
            "msg": "Success.",
            "code": "SUCCESS",
            "data": {
              "per5HourPercentage": 0.13117665718963334,
              "per1WeekResetTime": 1785074940000,
              "per5HourResetTime": 1784506140000,
              "per1WeekPercentage": 0.053834972282900004
            },
            "requestId": "...",
            "success": true
          }
        },
        "success": true,
        "httpStatus": 200,
        "api": "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage"
      },
      "successResponse": true
    }"#;

    #[test]
    fn normalizes_live_har_token_plan_response() {
        let usage = normalize_usage(HAR_RESPONSE.as_bytes()).unwrap();
        let primary = usage.primary.expect("five-hour quota window");
        assert!((primary.used_percent - 13.1177).abs() < 0.001);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-20T00:09:00Z"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.expect("weekly quota window");
        assert!((secondary.used_percent - 5.3835).abs() < 0.001);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-26T14:09:00Z"));
        assert_eq!(secondary.window_minutes, Some(10_080));
    }

    #[test]
    fn epoch_milliseconds_convert_to_utc() {
        assert_eq!(
            epoch_ms_to_iso8601(1_784_506_140_000).as_deref(),
            Some("2026-07-20T00:09:00Z")
        );
    }

    #[test]
    fn clamps_used_fraction_above_one() {
        let mut body: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        body["data"]["DataV2"]["data"]["data"]["per5HourPercentage"] = serde_json::json!(1.5);
        let usage = normalize_usage(body.to_string().as_bytes()).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 100.0);
    }

    #[test]
    fn non_success_gateway_response_is_decode_error() {
        let mut failed_gateway: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        failed_gateway["successResponse"] = serde_json::json!(false);
        assert!(matches!(
            normalize_usage(failed_gateway.to_string().as_bytes()),
            Err(FetchError::Decode(_))
        ));

        let mut failed_result: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        failed_result["data"]["DataV2"]["data"]["code"] = serde_json::json!("FAILED");
        assert!(matches!(
            normalize_usage(failed_result.to_string().as_bytes()),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn login_html_is_decode_error() {
        assert!(matches!(
            normalize_usage(b"<html><body>Qwen Cloud login</body></html>"),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn drops_window_when_its_percentage_is_missing() {
        let mut body: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        body["data"]["DataV2"]["data"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("per1WeekPercentage");
        let usage = normalize_usage(body.to_string().as_bytes()).unwrap();
        assert!(usage.primary.is_some());
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn extracts_sec_token_from_console_configuration() {
        assert_eq!(
            extract_sec_token(r#"window.ONE_CONSOLE_TOOL={SEC_TOKEN: "U19ojXS7pvhECD3W5IaVHA",};"#),
            Some("U19ojXS7pvhECD3W5IaVHA")
        );
        assert_eq!(extract_sec_token("window.ONE_CONSOLE_TOOL={};"), None);
    }
}
