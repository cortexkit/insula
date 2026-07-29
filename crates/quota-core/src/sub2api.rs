//! Sub2API usage fetcher — group API key and self-hosted endpoint from env.
//!
//! Every accepted `rate_limits[]` entry becomes a named extra window. USD quota,
//! subscription, and balance fields are deliberately omitted: they measure money
//! rather than a rate-limit period, and the balance output seam is not wired yet.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `SUB2API_API_KEY` / `SUB2API_BASE_URL` pair was available. Endpoint selection,
//! GET bearer authentication, request query parameters, and decoder field names are
//! ported from CodexBar
//! (`Sources/CodexBarCore/Providers/Sub2API/Sub2APIUsageFetcher.swift:241-255,
//! 294-320, 326-390`); environment keys and the HTTPS-or-loopback-HTTP endpoint
//! restrictions are from
//! `Sources/CodexBarCore/Providers/Sub2API/Sub2APISettingsReader.swift:11-31`.
//! Named rate-limit mapping, clamped ratios, and the known `5h` / `1d` / `7d`
//! durations and labels follow
//! `Sources/CodexBarCore/Providers/Sub2API/Sub2APIUsageFetcher.swift:141-150,
//! 201-204, 214-230`. The descriptor reserves primary/secondary/tertiary labels for
//! subscription periods (CodexBar descriptor at
//! `Sources/CodexBarCore/Providers/Sub2API/Sub2APIProviderDescriptor.swift:3-18`);
//! those USD values stay off the rate axis.

use std::{net::IpAddr, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{ExtraWindow, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "sub2api";
const API_KEY_ENV: &[&str] = &["SUB2API_API_KEY"];
const BASE_URL_ENV: &[&str] = &["SUB2API_BASE_URL"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The rate-limit fields decoded from the Sub2API `/v1/usage` response.
#[derive(Debug, Deserialize)]
struct Sub2ApiUsageResponse {
    #[serde(rename = "isValid")]
    is_valid: Option<bool>,
    #[serde(rename = "rate_limits")]
    rate_limits: Option<Vec<RateLimitResponse>>,
}

#[derive(Debug, Deserialize)]
struct RateLimitResponse {
    window: String,
    limit: f64,
    used: f64,
    remaining: f64,
    #[serde(rename = "reset_at")]
    reset_at: Option<String>,
}

fn clean_setting(value: Option<String>) -> Option<String> {
    crate::text::strip_wrapping_quotes(&value?)
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || match host.parse::<IpAddr>() {
            Ok(address) => address.is_loopback(),
            Err(_) => false,
        }
}

fn has_embedded_credentials(raw: &str) -> bool {
    let Some((_, authority_and_path)) = raw.split_once("://") else {
        return false;
    };
    let authority = authority_and_path
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    authority.contains('@')
}

/// Validate the self-hosted endpoint before a bearer token can be sent to it.
fn validate_base_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    let allowed_scheme =
        url.scheme() == "https" || (url.scheme() == "http" && is_loopback_host(host));
    if !allowed_scheme
        || has_embedded_credentials(raw)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(url)
}

fn settings_from_values(
    api_key: Option<String>,
    base_url: Option<String>,
) -> Result<(String, Url), FetchError> {
    let api_key = clean_setting(api_key)
        .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
    let raw_base_url = clean_setting(base_url)
        .ok_or_else(|| FetchError::NoSession(format!("none of {BASE_URL_ENV:?} is set")))?;
    // The variable is set and non-empty; the value is just not a usable base
    // URL. That is a configuration someone can fix, not an absent credential.
    let base_url = validate_base_url(&raw_base_url).ok_or_else(|| {
        FetchError::CredentialUnusable(
            "SUB2API_BASE_URL must be HTTPS, or loopback HTTP, without credentials, query, or fragment"
                .to_string(),
        )
    })?;
    Ok((api_key, base_url))
}

fn settings_from_env() -> Result<(String, Url), FetchError> {
    settings_from_values(env::first_env(API_KEY_ENV), env::first_env(BASE_URL_ENV))
}

/// Add `v1/usage` without duplicating a versioned or complete endpoint path.
fn usage_url(mut base_url: Url) -> Url {
    let (is_complete_usage_url, ends_at_v1) = {
        let components: Vec<&str> = base_url
            .path()
            .split('/')
            .filter(|component| !component.is_empty())
            .collect();
        (
            components.ends_with(&["v1", "usage"]),
            components.last() == Some(&"v1"),
        )
    };
    if is_complete_usage_url {
        return base_url;
    }

    let Ok(mut segments) = base_url.path_segments_mut() else {
        return base_url;
    };
    segments.pop_if_empty();
    if ends_at_v1 {
        segments.push("usage");
    } else {
        segments.push("v1");
        segments.push("usage");
    }
    drop(segments);
    base_url
}

/// Use the process time-zone setting when present; UTC is a safe fallback.
fn timezone_identifier() -> String {
    std::env::var("TZ")
        .ok()
        .map(|value| value.trim().trim_start_matches(':').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "UTC".to_string())
}

fn usage_request_url(base_url: Url) -> String {
    let mut url = usage_url(base_url);
    url.query_pairs_mut()
        .append_pair("days", "30")
        .append_pair("timezone", &timezone_identifier());
    url.into()
}

fn window_minutes(window: &str) -> Option<i64> {
    match window.to_ascii_lowercase().as_str() {
        "5h" => Some(5 * 60),
        "1d" => Some(24 * 60),
        "7d" => Some(7 * 24 * 60),
        _ => None,
    }
}

fn rate_limit_title(window: &str) -> String {
    match window.to_ascii_lowercase().as_str() {
        "5h" => "5 hour limit".to_string(),
        "1d" => "Daily limit".to_string(),
        "7d" => "7 day limit".to_string(),
        _ => format!("{window} limit"),
    }
}

/// Keep only reported RFC 3339 resets; an absent or malformed reset stays absent.
fn reported_reset_at(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let reset_at = raw.trim();
    (!reset_at.is_empty() && chrono::DateTime::parse_from_rfc3339(reset_at).is_ok())
        .then(|| reset_at.to_string())
}

fn rate_limit_to_extra(rate_limit: RateLimitResponse) -> Option<ExtraWindow> {
    if !rate_limit.limit.is_finite()
        || !rate_limit.used.is_finite()
        || !rate_limit.remaining.is_finite()
        || rate_limit.limit <= 0.0
    {
        return None;
    }
    let used_percent = rate_limit.used / rate_limit.limit * 100.0;
    if !used_percent.is_finite() {
        return None;
    }

    Some(ExtraWindow {
        title: Some(rate_limit_title(&rate_limit.window)),
        id: Some(rate_limit.window.clone()),
        window: Some(RateWindow {
            used_percent: used_percent.clamp(0.0, 100.0),
            raw_used_percent: None,
            resets_at: reported_reset_at(rate_limit.reset_at),
            window_minutes: window_minutes(&rate_limit.window),
            used_count: None,
            total_count: None,
        }),
    })
}

/// Normalize a Sub2API `/v1/usage` body to named rate-limit windows.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: Sub2ApiUsageResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("sub2api usage not decodable: {error}")))?;

    if response.is_valid == Some(false) {
        return Err(FetchError::Unauthorized(
            "sub2api rejected the API key".to_string(),
        ));
    }

    let extra_rate_windows: Vec<ExtraWindow> = response
        .rate_limits
        .unwrap_or_default()
        .into_iter()
        .filter_map(rate_limit_to_extra)
        .collect();
    if extra_rate_windows.is_empty() {
        return Err(FetchError::Decode(
            "sub2api response has no usable rate limit windows".to_string(),
        ));
    }

    Ok(Usage {
        primary: None,
        secondary: None,
        tertiary: None,
        extra_rate_windows: Some(extra_rate_windows),
    })
}

/// The Sub2API usage provider.
pub struct Sub2ApiProvider {
    http: reqwest::Client,
}

impl Sub2ApiProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for Sub2ApiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for Sub2ApiProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let (api_key, base_url) = settings_from_env()?;
            let body = JsonRequest::get(usage_request_url(base_url))
                .bearer(&api_key)
                .timeout(REQUEST_TIMEOUT)
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

    #[test]
    fn maps_codexbar_shaped_rate_limits_to_named_extra_windows() {
        let body = br#"{
            "mode": "quota_limited",
            "isValid": true,
            "balance": 42.5,
            "unit": "USD",
            "quota": {
                "limit": 100,
                "used": 25,
                "remaining": 75,
                "unit": "USD"
            },
            "subscription": {
                "daily_usage_usd": 2,
                "weekly_usage_usd": 10,
                "monthly_usage_usd": 30,
                "daily_limit_usd": 10,
                "weekly_limit_usd": 40,
                "monthly_limit_usd": 100
            },
            "rate_limits": [
                {
                    "window": "5h",
                    "limit": 20,
                    "used": 5,
                    "remaining": 15,
                    "reset_at": "2026-07-11T12:30:00Z"
                },
                {
                    "window": "1d",
                    "limit": 80,
                    "used": 20,
                    "remaining": 60,
                    "reset_at": "2026-07-12T12:30:00Z"
                },
                {
                    "window": "7d",
                    "limit": 200,
                    "used": 40,
                    "remaining": 160,
                    "reset_at": "2026-07-18T12:30:00Z"
                }
            ]
        }"#;

        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());

        let windows = usage.extra_rate_windows.as_ref().unwrap();
        assert_eq!(windows.len(), 3);

        assert_eq!(windows[0].id.as_deref(), Some("5h"));
        assert_eq!(windows[0].title.as_deref(), Some("5 hour limit"));
        let five_hour = windows[0].window.as_ref().unwrap();
        assert_eq!(five_hour.used_percent, 25.0);
        assert_eq!(five_hour.resets_at.as_deref(), Some("2026-07-11T12:30:00Z"));
        assert_eq!(five_hour.window_minutes, Some(300));

        assert_eq!(windows[1].id.as_deref(), Some("1d"));
        assert_eq!(windows[1].title.as_deref(), Some("Daily limit"));
        let daily = windows[1].window.as_ref().unwrap();
        assert_eq!(daily.used_percent, 25.0);
        assert_eq!(daily.resets_at.as_deref(), Some("2026-07-12T12:30:00Z"));
        assert_eq!(daily.window_minutes, Some(1440));

        assert_eq!(windows[2].id.as_deref(), Some("7d"));
        assert_eq!(windows[2].title.as_deref(), Some("7 day limit"));
        let weekly = windows[2].window.as_ref().unwrap();
        assert_eq!(weekly.used_percent, 20.0);
        assert_eq!(weekly.resets_at.as_deref(), Some("2026-07-18T12:30:00Z"));
        assert_eq!(weekly.window_minutes, Some(10080));
    }

    #[test]
    fn keeps_a_window_when_the_provider_omits_its_reset() {
        let body = br#"{
            "isValid": true,
            "rate_limits": [
                { "window": "7d", "limit": 200, "used": 40, "remaining": 160 }
            ]
        }"#;

        let usage = normalize_usage(body).unwrap();
        let weekly = usage
            .extra_rate_windows
            .unwrap()
            .pop()
            .unwrap()
            .window
            .unwrap();
        assert_eq!(weekly.used_percent, 20.0);
        assert_eq!(weekly.resets_at, None);
        assert_eq!(weekly.window_minutes, Some(10080));
    }

    #[test]
    fn zero_limit_yields_a_decode_error_instead_of_healthy_empty_usage() {
        let body = br#"{
            "isValid": true,
            "rate_limits": [
                { "window": "5h", "limit": 0, "used": 0, "remaining": 0 }
            ]
        }"#;

        assert!(matches!(normalize_usage(body), Err(FetchError::Decode(_))));
    }

    #[test]
    fn balance_fields_do_not_create_rate_windows() {
        let body = br#"{
            "isValid": true,
            "balance": 42.5,
            "unit": "USD",
            "quota": { "limit": 100, "used": 25, "remaining": 75, "unit": "USD" },
            "subscription": {
                "daily_usage_usd": 2,
                "weekly_usage_usd": 10,
                "monthly_usage_usd": 30,
                "daily_limit_usd": 10,
                "weekly_limit_usd": 40,
                "monthly_limit_usd": 100
            }
        }"#;

        assert!(matches!(normalize_usage(body), Err(FetchError::Decode(_))));
    }

    #[test]
    fn missing_environment_settings_return_no_session() {
        assert!(matches!(
            settings_from_values(None, Some("https://proxy.example.com".to_string())),
            Err(FetchError::NoSession(_))
        ));
        assert!(matches!(
            settings_from_values(Some("sk-group".to_string()), None),
            Err(FetchError::NoSession(_))
        ));
    }

    #[test]
    fn validates_safe_base_urls_and_builds_the_usage_path() {
        let root = validate_base_url("https://api.example.com").unwrap();
        assert_eq!(usage_url(root).as_str(), "https://api.example.com/v1/usage");

        let versioned = validate_base_url("https://api.example.com/proxy/v1").unwrap();
        assert_eq!(
            usage_url(versioned).as_str(),
            "https://api.example.com/proxy/v1/usage"
        );

        let complete = validate_base_url("https://api.example.com/v1/usage").unwrap();
        assert_eq!(
            usage_url(complete).as_str(),
            "https://api.example.com/v1/usage"
        );

        assert!(validate_base_url("http://127.0.0.1:8080").is_some());
        assert!(validate_base_url("http://[::1]:8080").is_some());
        assert!(validate_base_url("http://api.example.com").is_none());
        assert!(validate_base_url("https://user:pass@api.example.com").is_none());
        assert!(validate_base_url("https://@api.example.com").is_none());
        assert!(validate_base_url("https://api.example.com?token=secret").is_none());
        assert!(validate_base_url("https://api.example.com#fragment").is_none());
    }

    #[test]
    fn request_url_includes_the_codexbar_accounting_query() {
        let url = Url::parse(&usage_request_url(
            validate_base_url("https://api.example.com").unwrap(),
        ))
        .unwrap();
        let query: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert!(query.contains(&("days".to_string(), "30".to_string())));
        assert!(query
            .iter()
            .any(|(key, value)| key == "timezone" && !value.is_empty()));
    }

    #[test]
    fn rejects_non_finite_rate_limit_values() {
        let rate_limit = RateLimitResponse {
            window: "5h".to_string(),
            limit: f64::NAN,
            used: 5.0,
            remaining: 15.0,
            reset_at: Some("2026-07-11T12:30:00Z".to_string()),
        };
        assert!(rate_limit_to_extra(rate_limit).is_none());
    }

    #[test]
    fn invalid_credential_response_is_unauthorized() {
        let body = br#"{
            "isValid": false,
            "rate_limits": [
                { "window": "5h", "limit": 20, "used": 5, "remaining": 15 }
            ]
        }"#;
        assert!(matches!(
            normalize_usage(body),
            Err(FetchError::Unauthorized(_))
        ));
    }
}
