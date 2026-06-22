//! Zai usage fetcher — credential from an environment variable.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! Z_AI_API_KEY available. Endpoint, headers, and response shape ported from
//! CodexBar (Sources/CodexBarCore/Providers/Zai/ZaiUsageStats.swift:7-19,56-124,138-284,313-344,
//! Sources/CodexBarCore/Providers/Zai/ZaiSettingsReader.swift:6-8,10-31, and
//! Sources/CodexBarCore/Providers/Zai/ZaiAPIRegion.swift:3-35). Rides the live-proven http.rs.

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "zai";
const API_KEY_ENV: &[&str] = &["Z_AI_API_KEY"];
const DEFAULT_BASE: &str = "https://api.z.ai";

#[derive(Debug, Deserialize)]
struct ZaiQuotaLimitResponse {
    code: i64,
    msg: String,
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
        return Err(FetchError::Upstream(format!(
            "zai API error: code={}, msg={}",
            response.code, response.msg
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

    let primary = token_limit.clone().or(time_limit.clone()).map(|limit| RateWindow {
        used_percent: limit.used_percent,
        resets_at: limit.resets_at,
        window_minutes: limit.window_minutes,
    });

    let secondary = if token_limit.is_some() && time_limit.is_some() {
        time_limit.map(|limit| RateWindow {
            used_percent: limit.used_percent,
            resets_at: limit.resets_at,
            window_minutes: limit.window_minutes,
        })
    } else {
        None
    };

    let tertiary = session_token_limit.map(|limit| RateWindow {
        used_percent: limit.used_percent,
        resets_at: limit.resets_at,
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

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let api_key = env::first_env(API_KEY_ENV)
            .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
        let url = resolve_quota_url();

        let body = JsonRequest::get(url)
            .header(Header::new("authorization", format!("Bearer {api_key}")))
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
        assert_eq!(primary.resets_at, "2026-06-22T13:44:39Z");

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 10.0);
        assert_eq!(secondary.window_minutes, None); // TIME_LIMIT has no window_minutes
        assert_eq!(secondary.resets_at, "2026-06-22T13:44:39Z");

        assert!(usage.tertiary.is_none());
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
