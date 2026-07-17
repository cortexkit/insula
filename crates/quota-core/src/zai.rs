//! Zai usage fetcher — credential from an environment variable.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! Z_AI_API_KEY available. Endpoint, headers, optional success message, team scope,
//! and response shape ported from CodexBar
//! (Sources/CodexBarCore/Providers/Zai/ZaiUsageStats.swift:7-19,21-45,56-124,138-315,318-460,
//! Sources/CodexBarCore/Providers/Zai/ZaiSettingsReader.swift:6-10,12-31,
//! Sources/CodexBarCore/Providers/Zai/ZaiAPIRegion.swift:3-35, and fixture
//! Tests/CodexBarTests/ZaiProviderTests.swift:406-454,457-527). Rides the live-proven http.rs.

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "zai";
const API_KEY_ENV: &[&str] = &["Z_AI_API_KEY"];
const BIGMODEL_ORGANIZATION_ENV: &str = "Z_AI_BIGMODEL_ORGANIZATION";
const BIGMODEL_PROJECT_ENV: &str = "Z_AI_BIGMODEL_PROJECT";
const DEFAULT_BASE: &str = "https://api.z.ai";

#[derive(Debug, Deserialize)]
struct ZaiQuotaLimitResponse {
    code: i64,
    msg: Option<String>,
    data: Option<ZaiQuotaLimitData>,
    success: bool,
}

#[derive(Debug, Deserialize)]
struct ZaiQuotaLimitData {
    limits: Vec<ZaiLimitRaw>,
}

#[derive(Debug, Deserialize)]
struct ZaiLimitRaw {
    #[serde(rename = "type")]
    limit_type: String,
    unit: i64,
    number: i64,
    usage: Option<i64>,
    #[serde(rename = "currentValue")]
    current_value: Option<i64>,
    remaining: Option<i64>,
    percentage: i64,
    #[serde(rename = "nextResetTime")]
    next_reset_time: Option<i64>,
}

#[derive(Clone)]
struct ValidLimit {
    used_percent: f64,
    window_minutes: Option<i64>,
    resets_at: String,
}

struct ZaiTeamContext {
    organization: String,
    project: String,
}

struct ZaiRequestSpec {
    url: String,
    headers: Vec<Header>,
}

impl ZaiRequestSpec {
    fn into_json_request(self) -> JsonRequest {
        self.headers
            .into_iter()
            .fold(JsonRequest::get(self.url), |request, header| {
                request.header(header)
            })
    }

    #[cfg(test)]
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }
}

fn get_used_percent(
    limit: Option<i64>,
    remaining: Option<i64>,
    current_value: Option<i64>,
    percentage: i64,
) -> f64 {
    if let Some(limit_val) = limit {
        if limit_val > 0 {
            let mut used_raw = None;
            if let Some(rem) = remaining {
                let used_from_remaining = limit_val - rem;
                if let Some(curr) = current_value {
                    used_raw = Some(used_from_remaining.max(curr));
                } else {
                    used_raw = Some(used_from_remaining);
                }
            } else if let Some(curr) = current_value {
                used_raw = Some(curr);
            }
            if let Some(used) = used_raw {
                let used_clamped = used.clamp(0, limit_val);
                let percent = (used_clamped as f64 / limit_val as f64) * 100.0;
                return percent.clamp(0.0, 100.0);
            }
        }
    }
    percentage as f64
}

fn get_window_minutes(unit: i64, number: i64) -> Option<i64> {
    if number <= 0 {
        return None;
    }
    match unit {
        5 => Some(number),
        3 => Some(number * 60),
        1 => Some(number * 24 * 60),
        6 => Some(number * 7 * 24 * 60),
        _ => None,
    }
}

/// Normalize the quota limit response body to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: ZaiQuotaLimitResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("zai quota limit not decodable: {e}")))?;

    if !response.success || response.code != 200 {
        let message = response
            .msg
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("zai quota API returned code {}", response.code));
        return Err(FetchError::Upstream(format!(
            "zai API error: code={}, msg={message}",
            response.code
        )));
    }

    let data = response
        .data
        .ok_or_else(|| FetchError::Decode("zai response missing data".to_string()))?;

    let mut token_limits = Vec::new();
    let mut time_limit = None;

    for limit in data.limits {
        if limit.limit_type != "TOKENS_LIMIT" && limit.limit_type != "TIME_LIMIT" {
            continue;
        }
        let resets_at = match limit
            .next_reset_time
            .and_then(|ms| env::epoch_to_iso8601(ms / 1000))
        {
            Some(r) => r,
            None => continue, // Drop the window if no nextResetTime
        };
        let used_percent = get_used_percent(
            limit.usage,
            limit.remaining,
            limit.current_value,
            limit.percentage,
        );
        let window_minutes = if limit.limit_type == "TOKENS_LIMIT" {
            get_window_minutes(limit.unit, limit.number)
        } else {
            None
        };
        let valid = ValidLimit {
            used_percent,
            window_minutes,
            resets_at,
        };
        if limit.limit_type == "TOKENS_LIMIT" {
            token_limits.push(valid);
        } else {
            time_limit = Some(valid);
        }
    }

    let mut token_limit = None;
    let mut session_token_limit = None;

    if token_limits.len() >= 2 {
        token_limits.sort_by_key(|limit| limit.window_minutes.unwrap_or(i64::MAX));
        session_token_limit = Some(token_limits.remove(0));
        token_limit = token_limits.pop();
    } else if !token_limits.is_empty() {
        token_limit = Some(token_limits.remove(0));
    }

    let primary = token_limit
        .clone()
        .or(time_limit.clone())
        .map(|limit| RateWindow {
            used_percent: limit.used_percent,
            raw_used_percent: None,
            resets_at: Some(limit.resets_at),
            window_minutes: limit.window_minutes,
        });

    let secondary = if token_limit.is_some() && time_limit.is_some() {
        time_limit.map(|limit| RateWindow {
            used_percent: limit.used_percent,
            raw_used_percent: None,
            resets_at: Some(limit.resets_at),
            window_minutes: limit.window_minutes,
        })
    } else {
        None
    };

    let tertiary = session_token_limit.map(|limit| RateWindow {
        used_percent: limit.used_percent,
        raw_used_percent: None,
        resets_at: Some(limit.resets_at),
        window_minutes: limit.window_minutes,
    });

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

fn resolve_quota_url() -> String {
    if let Some(quota_url) = env::first_env(&["Z_AI_QUOTA_URL"]) {
        return quota_url;
    }
    if let Some(api_host) = env::first_env(&["Z_AI_API_HOST"]) {
        let trimmed = api_host.trim().trim_end_matches('/');
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            if let Ok(url) = reqwest::Url::parse(trimmed) {
                if url.path() == "" || url.path() == "/" {
                    return format!("{}/api/monitor/usage/quota/limit", trimmed);
                }
                return trimmed.to_string();
            }
            return trimmed.to_string();
        } else {
            return format!("https://{}/api/monitor/usage/quota/limit", trimmed);
        }
    }
    format!("{}/api/monitor/usage/quota/limit", DEFAULT_BASE)
}

fn team_context_from_env() -> Option<ZaiTeamContext> {
    Some(ZaiTeamContext {
        organization: env::first_env(&[BIGMODEL_ORGANIZATION_ENV])?,
        project: env::first_env(&[BIGMODEL_PROJECT_ENV])?,
    })
}

fn team_scope_url(raw_url: &str) -> Result<String, FetchError> {
    let mut url = reqwest::Url::parse(raw_url)
        .map_err(|error| FetchError::Upstream(format!("invalid zai quota URL: {error}")))?;
    let existing_query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| name != "type")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(existing_query)
        .append_pair("type", "2");
    Ok(url.to_string())
}

fn build_quota_request(
    api_key: &str,
    url: String,
    team_context: Option<ZaiTeamContext>,
) -> Result<ZaiRequestSpec, FetchError> {
    let mut headers = vec![Header::new("Authorization", format!("Bearer {api_key}"))];
    let url = if let Some(team_context) = team_context {
        headers.push(Header::new(
            "Bigmodel-Organization",
            team_context.organization,
        ));
        headers.push(Header::new("Bigmodel-Project", team_context.project));
        team_scope_url(&url)?
    } else {
        url
    };

    Ok(ZaiRequestSpec { url, headers })
}

/// The Zai usage provider.
pub struct ZaiProvider {
    http: reqwest::Client,
}

impl ZaiProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for ZaiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for ZaiProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            let request =
                build_quota_request(&api_key, resolve_quota_url(), team_context_from_env())?;

            let body = request.into_json_request().send(&self.http).await?;

            let usage = normalize_usage(&body)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::Mutex};

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn normalizes_quota_limit_payload() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "success": true,
            "data": {
                "planName": "Pro",
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 100000,
                        "currentValue": 20000,
                        "remaining": 80000,
                        "percentage": 20,
                        "nextResetTime": 1782135879000
                    },
                    {
                        "type": "TIME_LIMIT",
                        "unit": 1,
                        "number": 30,
                        "usage": 100,
                        "currentValue": 10,
                        "remaining": 90,
                        "percentage": 10,
                        "nextResetTime": 1782135879000
                    }
                ]
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0);
        assert_eq!(primary.window_minutes, Some(300)); // 5 hours = 300 minutes
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T13:44:39Z"));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 10.0);
        assert_eq!(secondary.window_minutes, None); // TIME_LIMIT has no window_minutes
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-06-22T13:44:39Z"));

        assert!(usage.tertiary.is_none());
    }

    #[test]
    fn normalizes_bigmodel_cn_payload_without_msg() {
        let body = br#"{
          "code": 200,
          "data": {
            "limits": [
              {
                "type": "TIME_LIMIT",
                "unit": 5,
                "number": 1,
                "usage": 1000,
                "currentValue": 147,
                "remaining": 853,
                "percentage": 14,
                "nextResetTime": 1784706344993,
                "usageDetails": [
                  { "modelCode": "search-prime", "usage": 84 },
                  { "modelCode": "web-reader", "usage": 41 },
                  { "modelCode": "zread", "usage": 8 }
                ]
              },
              {
                "type": "TOKENS_LIMIT",
                "unit": 3,
                "number": 5,
                "percentage": 8,
                "nextResetTime": 1783049703178
              },
              {
                "type": "TOKENS_LIMIT",
                "unit": 6,
                "number": 1,
                "percentage": 7,
                "nextResetTime": 1783496744998
              }
            ],
            "level": "pro"
          },
          "success": true
        }"#;

        let usage = normalize_usage(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 7.0);
        assert_eq!(primary.window_minutes, Some(10_080));
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-08T07:45:44Z"));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 14.7);
        assert_eq!(secondary.window_minutes, None);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-22T07:45:44Z"));

        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 8.0);
        assert_eq!(tertiary.window_minutes, Some(300));
        assert_eq!(tertiary.resets_at.as_deref(), Some("2026-07-03T03:35:03Z"));
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn team_env_builds_scoped_request_with_bigmodel_headers() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _organization = EnvGuard::set(BIGMODEL_ORGANIZATION_ENV, "org-test");
        let _project = EnvGuard::remove(BIGMODEL_PROJECT_ENV);
        let base_url = format!("{DEFAULT_BASE}/api/monitor/usage/quota/limit");

        let personal =
            build_quota_request("zai-test-token", base_url.clone(), team_context_from_env())
                .unwrap();
        assert_eq!(personal.url, base_url);
        assert!(personal.header("Bigmodel-Organization").is_none());
        assert!(personal.header("Bigmodel-Project").is_none());

        std::env::set_var(BIGMODEL_PROJECT_ENV, "proj-test");
        let team =
            build_quota_request("zai-test-token", base_url, team_context_from_env()).unwrap();

        assert_eq!(
            team.url,
            "https://api.z.ai/api/monitor/usage/quota/limit?type=2"
        );
        assert_eq!(team.header("Authorization"), Some("Bearer zai-test-token"));
        assert_eq!(team.header("Bigmodel-Organization"), Some("org-test"));
        assert_eq!(team.header("Bigmodel-Project"), Some("proj-test"));
    }

    #[test]
    fn normalizes_multiple_tokens_limits() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 24,
                        "usage": 100000,
                        "currentValue": 20000,
                        "remaining": 80000,
                        "percentage": 20,
                        "nextResetTime": 1782135879000
                    },
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 50000,
                        "currentValue": 15000,
                        "remaining": 35000,
                        "percentage": 30,
                        "nextResetTime": 1782135879000
                    }
                ]
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        // Shortest window (5h) -> sessionTokenLimit (tertiary)
        // Longest window (24h) -> tokenLimit (primary)
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0);
        assert_eq!(primary.window_minutes, Some(1440)); // 24 hours = 1440 minutes

        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 30.0);
        assert_eq!(tertiary.window_minutes, Some(300)); // 5 hours = 300 minutes

        assert!(usage.secondary.is_none());
    }

    #[test]
    fn missing_reset_drops_window() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": 100000,
                        "currentValue": 20000,
                        "remaining": 80000,
                        "percentage": 20,
                        "nextResetTime": null
                    }
                ]
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
    }

    #[test]
    fn fallback_to_percentage_when_quota_fields_absent() {
        let body = br#"{
            "code": 200,
            "msg": "success",
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "TOKENS_LIMIT",
                        "unit": 3,
                        "number": 5,
                        "usage": null,
                        "currentValue": null,
                        "remaining": null,
                        "percentage": 45,
                        "nextResetTime": 1782135879000
                    }
                ]
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 45.0);
    }
}
