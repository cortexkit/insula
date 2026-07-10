//! Qoder credit usage — browser-cookie request to the member quota API.
//!
//! Chrome cookies for the exact `qoder.com` and `www.qoder.com` hosts are sent
//! with Qoder's browser headers. The base quota and optional shared quota are
//! merged into one primary percentage window.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! logged-in Qoder browser session. Endpoint and request headers, base/shared
//! quota merging, reset parsing, and snake/camel-case response aliases are from
//! CodexBar `Sources/CodexBarCore/Providers/Qoder/QoderUsageFetcher.swift:10-17,47-129,140-207,210-315`.
//! The primary window mapping is from `QoderUsageSnapshot.swift:30-49`; cookie
//! header construction and exact domains are from `QoderCookieImporter.swift:23-25,37-44,83-101`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    browser_cookies::{self, CookieError, CookieJar},
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "qoder";
const DOMAIN: &str = "qoder.com";
const COOKIE_DOMAINS: &[&str] = &["qoder.com", "www.qoder.com"];
const USAGE_URL: &str = "https://qoder.com/api/v2/me/usages/big_model_credits";
const ORIGIN: &str = "https://qoder.com";
const REFERER: &str = "https://qoder.com/account/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Debug, Deserialize)]
struct QoderUsageResponse {
    #[serde(rename = "totalQuota", alias = "total_quota")]
    total_quota: Option<QoderQuotaContainer>,
    #[serde(rename = "sharedQuota", alias = "shared_quota")]
    shared_quota: Option<QoderQuotaContainer>,
    #[serde(rename = "nextResetAt", alias = "next_reset_at")]
    next_reset_at: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct QoderQuotaContainer {
    #[serde(rename = "quotaSummary", alias = "quota_summary")]
    quota_summary: Option<QoderQuotaSummary>,
}

#[derive(Debug, Deserialize)]
struct QoderQuotaSummary {
    #[serde(rename = "usedValue", alias = "used_value")]
    used_value: f64,
    #[serde(rename = "limitValue", alias = "limit_value")]
    limit_value: f64,
    #[serde(rename = "remainingValue", alias = "remaining_value")]
    remaining_value: Option<f64>,
    #[serde(rename = "usagePercentage", alias = "usage_percentage")]
    usage_percentage: Option<f64>,
}

fn remaining_credits(summary: &QoderQuotaSummary) -> Result<f64, FetchError> {
    if summary.used_value < 0.0
        || summary.limit_value < 0.0
        || summary.remaining_value.is_some_and(|value| value < 0.0)
    {
        return Err(FetchError::Decode(
            "qoder: quota values must be nonnegative".to_string(),
        ));
    }

    Ok(summary
        .remaining_value
        .unwrap_or_else(|| (summary.limit_value - summary.used_value).max(0.0)))
}

fn usage_percentage(
    used: f64,
    total: f64,
    remaining: f64,
    provided: Option<f64>,
) -> Result<f64, FetchError> {
    if used < 0.0 || total < 0.0 || remaining < 0.0 {
        return Err(FetchError::Decode(
            "qoder: quota values must be nonnegative".to_string(),
        ));
    }
    if total == 0.0 {
        if used != 0.0 || remaining != 0.0 {
            return Err(FetchError::Decode(
                "qoder: zero total quota must have zero usage and remaining".to_string(),
            ));
        }
        return Ok(provided.unwrap_or(100.0));
    }

    Ok(provided.unwrap_or((used / total) * 100.0))
}

fn merged_usage_percentage(
    base: &QoderQuotaSummary,
    shared: Option<&QoderQuotaSummary>,
) -> Result<f64, FetchError> {
    let base_remaining = remaining_credits(base)?;
    let Some(shared) = shared else {
        return usage_percentage(
            base.used_value,
            base.limit_value,
            base_remaining,
            base.usage_percentage,
        );
    };

    let shared_remaining = remaining_credits(shared)?;
    usage_percentage(
        base.used_value + shared.used_value,
        base.limit_value + shared.limit_value,
        base_remaining + shared_remaining,
        None,
    )
}

fn parse_reset_at(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        let date = chrono::DateTime::parse_from_rfc3339(value.trim()).ok()?;
        return Some(
            date.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }

    let value = value.as_f64()?;
    let seconds = if value > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    };
    env::epoch_to_iso8601(seconds as i64)
}

/// Normalize Qoder's member quota response into one primary usage window.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: QoderUsageResponse = serde_json::from_slice(body).map_err(|error| {
        FetchError::Decode(format!("qoder usage response not decodable: {error}"))
    })?;
    let base = response
        .total_quota
        .as_ref()
        .and_then(|quota| quota.quota_summary.as_ref())
        .ok_or_else(|| FetchError::Decode("qoder: missing totalQuota.quotaSummary".to_string()))?;
    let shared = response
        .shared_quota
        .as_ref()
        .and_then(|quota| quota.quota_summary.as_ref());
    let used_percent = merged_usage_percentage(base, shared)?.clamp(0.0, 100.0);
    let resets_at = response.next_reset_at.as_ref().and_then(parse_reset_at);

    Ok(Usage {
        primary: Some(RateWindow {
            used_percent,
            resets_at,
            window_minutes: None,
        }),
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

fn request_cookie_header(jar: &CookieJar) -> Option<String> {
    let parts: Vec<String> = jar
        .cookies
        .iter()
        .filter(|cookie| {
            let host = cookie
                .host_key
                .strip_prefix('.')
                .unwrap_or(&cookie.host_key);
            COOKIE_DOMAINS
                .iter()
                .any(|domain| host.eq_ignore_ascii_case(domain))
        })
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect();

    (!parts.is_empty()).then(|| parts.join("; "))
}

fn map_cookie_error(error: CookieError) -> FetchError {
    match error {
        CookieError::NoStore | CookieError::NoCookie | CookieError::Unsupported => {
            FetchError::NoSession(error.to_string())
        }
        CookieError::NoKeychainKey(_) | CookieError::Extract(_) => {
            FetchError::Upstream(error.to_string())
        }
    }
}

/// The Qoder browser-cookie usage provider.
pub struct QoderProvider {
    http: reqwest::Client,
}

impl QoderProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for QoderProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for QoderProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let jar = browser_cookies::chrome_cookies_for(DOMAIN).map_err(map_cookie_error)?;
        // Qoder's importer does not designate one session-cookie name, so send
        // every cookie from its two exact international hosts.
        let cookie = request_cookie_header(&jar).ok_or_else(|| {
            FetchError::NoSession("no cookies for an exact Qoder browser host".to_string())
        })?;

        let body = JsonRequest::get(USAGE_URL)
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", cookie))
            .header(Header::new("Accept", "application/json, text/plain, */*"))
            .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
            .header(Header::new("User-Agent", USER_AGENT))
            .header(Header::new("Origin", ORIGIN))
            .header(Header::new("Referer", REFERER))
            .header(Header::new("X-Requested-With", "XMLHttpRequest"))
            .header(Header::new("Bx-V", "2.5.35"))
            .send(&self.http)
            .await?;
        let usage = normalize_usage(&body)?;

        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_snake_case_percentage_and_provider_reset_to_primary() {
        let payload = br#"{
            "next_reset_at": "2024-09-01T00:00:00Z",
            "total_quota": {
                "quota_summary": {
                    "used_value": 125,
                    "limit_value": 500,
                    "remaining_value": 375,
                    "usage_percentage": 37.5,
                    "unit": "credit"
                }
            }
        }"#;

        let usage = normalize_usage(payload).unwrap();
        assert_eq!(
            usage,
            Usage {
                primary: Some(RateWindow {
                    used_percent: 37.5,
                    resets_at: Some("2024-09-01T00:00:00Z".to_string()),
                    window_minutes: None,
                }),
                secondary: None,
                tertiary: None,
                extra_rate_windows: None,
            }
        );
    }

    #[test]
    fn merges_base_and_shared_quota_before_computing_percentage() {
        let payload = br#"{
            "totalQuota": {
                "quotaSummary": {
                    "usedValue": 1500,
                    "limitValue": 1500,
                    "remainingValue": 0,
                    "usagePercentage": 100
                }
            },
            "sharedQuota": {
                "quotaSummary": {
                    "usedValue": 200,
                    "limitValue": 1000,
                    "remainingValue": 800,
                    "usagePercentage": 20
                }
            }
        }"#;

        let usage = normalize_usage(payload).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 68.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.window_minutes, None);
        assert_eq!(usage.secondary, None);
        assert_eq!(usage.tertiary, None);
        assert_eq!(usage.extra_rate_windows, None);
    }

    #[test]
    fn emits_percentage_window_when_reset_is_absent() {
        let payload = br#"{
            "total_quota": {
                "quota_summary": {
                    "used_value": 75,
                    "limit_value": 500,
                    "remaining_value": 425,
                    "usage_percentage": 15
                }
            }
        }"#;

        let usage = normalize_usage(payload).unwrap();
        assert_eq!(
            usage,
            Usage {
                primary: Some(RateWindow {
                    used_percent: 15.0,
                    resets_at: None,
                    window_minutes: None,
                }),
                secondary: None,
                tertiary: None,
                extra_rate_windows: None,
            }
        );
    }
}
