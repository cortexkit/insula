//! MiMo usage — browser-cookie scrape of platform.xiaomimimo.com.
//!
//! VERIFICATION:
//! This port is fixture-verified against CodexBar source.
//! Ported from CodexBar:
//! - `Sources/CodexBarCore/Providers/MiMo/MiMoUsageFetcher.swift` (lines 88-92, 171-180, 188-193, 239-248, 280-289, 294-306)
//! - `Sources/CodexBarCore/Providers/MiMo/MiMoCookieImporter.swift` (lines 7-14, 106-109)
//! - `Sources/CodexBarCore/Providers/MiMo/MiMoUsageSnapshot.swift`
//!
//! No live proof is implied as there is no logged-in browser session on the build machine.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "mimo";
const DOMAIN: &str = "xiaomimimo.com";
const DETAIL_URL: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/detail";
const USAGE_URL: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Deserialize)]
struct MimoEnvelope<T> {
    code: i32,
    data: Option<T>,
}

#[derive(Deserialize)]
struct MimoDetail {
    #[serde(rename = "currentPeriodEnd")]
    current_period_end: Option<String>,
}

#[derive(Deserialize)]
struct MimoUsage {
    #[serde(rename = "monthUsage")]
    month_usage: Option<MimoMonthUsage>,
}

#[derive(Deserialize)]
struct MimoMonthUsage {
    items: Option<Vec<MimoUsageItem>>,
}

#[derive(Deserialize)]
struct MimoUsageItem {
    percent: Option<f64>,
    used: Option<f64>,
    limit: Option<f64>,
}

fn parse_reset_time(s: &str) -> Option<String> {
    let naive = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let dt = Utc.from_utc_datetime(&naive);
    Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Normalize the detail and usage JSON responses to [`Usage`]. Pure — unit-testable.
pub fn normalize(detail_json: &str, usage_json: &str) -> Result<Usage, FetchError> {
    let detail_envelope: MimoEnvelope<MimoDetail> = serde_json::from_str(detail_json)
        .map_err(|e| FetchError::Decode(format!("failed to parse detail JSON: {e}")))?;

    if detail_envelope.code == 401 || detail_envelope.code == 403 {
        return Err(FetchError::Unauthorized(format!(
            "mimo API returned code {}",
            detail_envelope.code
        )));
    }
    if detail_envelope.code != 0 {
        return Err(FetchError::Decode(format!(
            "mimo API returned non-zero code {}",
            detail_envelope.code
        )));
    }

    let detail_data = detail_envelope
        .data
        .ok_or_else(|| FetchError::Decode("mimo detail response missing data field".to_string()))?;

    let usage_envelope: MimoEnvelope<MimoUsage> = serde_json::from_str(usage_json)
        .map_err(|e| FetchError::Decode(format!("failed to parse usage JSON: {e}")))?;

    if usage_envelope.code == 401 || usage_envelope.code == 403 {
        return Err(FetchError::Unauthorized(format!(
            "mimo API returned code {}",
            usage_envelope.code
        )));
    }
    if usage_envelope.code != 0 {
        return Err(FetchError::Decode(format!(
            "mimo API returned non-zero code {}",
            usage_envelope.code
        )));
    }

    let usage_data = usage_envelope
        .data
        .ok_or_else(|| FetchError::Decode("mimo usage response missing data field".to_string()))?;

    let mut used_percent = None;
    if let Some(month_usage) = usage_data.month_usage {
        if let Some(items) = month_usage.items {
            if let Some(item) = items.first() {
                if let Some(p) = item.percent {
                    used_percent = Some((p * 100.0).clamp(0.0, 100.0));
                } else if let (Some(used), Some(limit)) = (item.used, item.limit) {
                    if limit > 0.0 {
                        used_percent = Some(((used / limit) * 100.0).clamp(0.0, 100.0));
                    }
                }
            }
        }
    }

    let resets_at = detail_data
        .current_period_end
        .and_then(|s| parse_reset_time(&s));

    let primary = match (used_percent, resets_at) {
        (Some(pct), Some(reset)) => Some(RateWindow {
            used_percent: pct,
            raw_used_percent: None,
            resets_at: Some(reset),
            window_minutes: Some(43200),
            used_count: None,
            total_count: None,
        }),
        _ => None,
    };

    if primary.is_none() {
        return Err(FetchError::Decode(
            "mimo: no valid usage window (missing percent or reset time)".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The MiMo usage provider.
pub struct MimoProvider {
    http: reqwest::Client,
}

impl MimoProvider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http }
    }
}

impl Default for MimoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for MimoProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let jar = browser_cookies::chrome_cookies_for(DOMAIN).map_err(|e| match e {
                browser_cookies::CookieError::NoStore
                | browser_cookies::CookieError::NoCookie
                | browser_cookies::CookieError::Unsupported => FetchError::NoSession(e.to_string()),
                browser_cookies::CookieError::NoKeychainKey(_)
                | browser_cookies::CookieError::Extract(_) => FetchError::Upstream(e.to_string()),
            })?;

            let has_token = jar.has_cookie_named(|n| n == "api-platform_serviceToken");
            let has_user_id = jar.has_cookie_named(|n| n == "userId");
            if !has_token || !has_user_id {
                return Err(FetchError::NoSession(
                    "missing required mimo session cookies (api-platform_serviceToken and userId)"
                        .to_string(),
                ));
            }

            // The usage response omits account metadata needed by normalization,
            // so validate and retain the detail response before fetching usage.
            let detail_req = JsonRequest::get(DETAIL_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Accept", "application/json, text/plain, */*"))
                .header(Header::new("Cookie", jar.header()))
                .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
                .header(Header::new("x-timeZone", "UTC+01:00"))
                .header(Header::new("Origin", "https://platform.xiaomimimo.com"))
                .header(Header::new(
                    "Referer",
                    "https://platform.xiaomimimo.com/#/console/balance",
                ))
                .header(Header::new("User-Agent", BROWSER_USER_AGENT));

            let detail_res = detail_req.send_raw(&self.http).await?;
            if (300..400).contains(&detail_res.status) || detail_res.status == 401 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    detail_res.status
                )));
            }
            if detail_res.status == 403 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    detail_res.status
                )));
            }
            if !(200..300).contains(&detail_res.status) {
                let excerpt: String = String::from_utf8_lossy(&detail_res.body)
                    .chars()
                    .take(200)
                    .collect();
                return Err(FetchError::Upstream(format!(
                    "HTTP {}: {excerpt}",
                    detail_res.status
                )));
            }

            let usage_req = JsonRequest::get(USAGE_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Accept", "application/json, text/plain, */*"))
                .header(Header::new("Cookie", jar.header()))
                .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
                .header(Header::new("x-timeZone", "UTC+01:00"))
                .header(Header::new("Origin", "https://platform.xiaomimimo.com"))
                .header(Header::new(
                    "Referer",
                    "https://platform.xiaomimimo.com/#/console/balance",
                ))
                .header(Header::new("User-Agent", BROWSER_USER_AGENT));

            let usage_res = usage_req.send_raw(&self.http).await?;
            if (300..400).contains(&usage_res.status) || usage_res.status == 401 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    usage_res.status
                )));
            }
            if usage_res.status == 403 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    usage_res.status
                )));
            }
            if !(200..300).contains(&usage_res.status) {
                let excerpt: String = String::from_utf8_lossy(&usage_res.body)
                    .chars()
                    .take(200)
                    .collect();
                return Err(FetchError::Upstream(format!(
                    "HTTP {}: {excerpt}",
                    usage_res.status
                )));
            }

            let detail_json = String::from_utf8_lossy(&detail_res.body);
            let usage_json = String::from_utf8_lossy(&usage_res.body);

            let usage = normalize(&detail_json, &usage_json)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DETAIL_HEALTHY: &str = r#"{
        "code": 0,
        "data": {
            "currentPeriodEnd": "2026-07-24 12:00:00"
        }
    }"#;

    const USAGE_HEALTHY: &str = r#"{
        "code": 0,
        "data": {
            "monthUsage": {
                "items": [
                    {
                        "percent": 0.45,
                        "used": 45.0,
                        "limit": 100.0
                    }
                ]
            }
        }
    }"#;

    const ENVELOPE_401: &str = r#"{
        "code": 401,
        "data": null
    }"#;

    const DETAIL_MISSING_RESET: &str = r#"{
        "code": 0,
        "data": {
            "currentPeriodEnd": null
        }
    }"#;

    const USAGE_COUNTS_ONLY: &str = r#"{
        "code": 0,
        "data": {
            "monthUsage": {
                "items": [
                    {
                        "percent": null,
                        "used": 30.0,
                        "limit": 120.0
                    }
                ]
            }
        }
    }"#;

    #[test]
    fn test_healthy_parse() {
        let usage = normalize(DETAIL_HEALTHY, USAGE_HEALTHY).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 45.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T12:00:00Z"));
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn test_unauthorized_envelope() {
        let res = normalize(ENVELOPE_401, USAGE_HEALTHY);
        assert!(matches!(res, Err(FetchError::Unauthorized(_))));

        let res2 = normalize(DETAIL_HEALTHY, ENVELOPE_401);
        assert!(matches!(res2, Err(FetchError::Unauthorized(_))));
    }

    #[test]
    fn test_missing_reset_drops_window() {
        let res = normalize(DETAIL_MISSING_RESET, USAGE_HEALTHY);
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }

    #[test]
    fn test_counts_only_parse() {
        let usage = normalize(DETAIL_HEALTHY, USAGE_COUNTS_ONLY).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T12:00:00Z"));
        assert_eq!(primary.window_minutes, Some(43200));
    }
}
