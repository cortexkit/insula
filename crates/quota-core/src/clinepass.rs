//! ClinePass usage fetcher — credential from environment variables.
//!
//! Endpoint: `GET https://api.cline.bot/api/v1/users/me/plan/usage-limits`
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `CLINE_API_KEY` or `CLINEPASS_API_KEY` was available to fetch a real window.
//! The endpoint, the `Authorization: Bearer` header, and the response shape
//! are ported from CodexBar's working parser at tag v0.44.0:
//! - `Sources/CodexBarCore/Providers/ClinePass/ClinePassUsageFetcher.swift:116-117, 133-135, 163-210`
//! - `Sources/CodexBarCore/Providers/ClinePass/ClinePassSettingsReader.swift:4-5, 22-33`
//!
//! The test payload mirrors that shape. The shared HTTP transport it rides
//! (`http.rs`) is itself live-proven.

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "clinepass";
const API_KEY_ENV: &[&str] = &["CLINE_API_KEY", "CLINEPASS_API_KEY"];

#[derive(Debug, Deserialize)]
struct ClinePassLimitsResponse {
    data: ClinePassLimitsData,
    success: bool,
}

#[derive(Debug, Deserialize)]
struct ClinePassLimitsData {
    limits: Vec<ClinePassLimit>,
}

#[derive(Debug, Deserialize)]
struct ClinePassLimit {
    #[serde(rename = "type")]
    limit_type: String,
    #[serde(rename = "percentUsed")]
    percent_used: f64,
    #[serde(rename = "resetsAt")]
    resets_at: Option<String>,
}

fn clean_key(key: String) -> Option<String> {
    let mut value = key.trim().to_string();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value = value[1..value.len() - 1].trim().to_string();
    }
    (!value.is_empty()).then_some(value)
}

/// Normalize a ClinePass `/api/v1/users/me/plan/usage-limits` body to rate-limit windows.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: ClinePassLimitsResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("clinepass limits not decodable: {e}")))?;

    if !response.success {
        return Err(FetchError::Decode(
            "ClinePass response success was false".to_string(),
        ));
    }

    let mut primary = None;
    let mut secondary = None;
    let mut tertiary = None;

    for limit in response.data.limits {
        let window_minutes = match limit.limit_type.as_str() {
            "five_hour" => 5 * 60,
            "weekly" => 7 * 24 * 60,
            "monthly" => 30 * 24 * 60,
            _ => continue,
        };

        let resets_at = match limit.resets_at {
            Some(raw) => {
                let trimmed = raw.trim();
                let dt = chrono::DateTime::parse_from_rfc3339(trimmed)
                    .map_err(|e| FetchError::Decode(format!("invalid resetsAt timestamp: {e}")))?;
                Some(
                    dt.with_timezone(&chrono::Utc)
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string(),
                )
            }
            None => None,
        };

        let window = RateWindow {
            used_percent: limit.percent_used.clamp(0.0, 100.0),
            raw_used_percent: None,
            resets_at,
            window_minutes: Some(window_minutes),
        };

        match limit.limit_type.as_str() {
            "five_hour" => primary = Some(window),
            "weekly" => secondary = Some(window),
            "monthly" => tertiary = Some(window),
            _ => {}
        }
    }

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

/// The ClinePass usage provider.
pub struct ClinePassProvider {
    http: reqwest::Client,
}

impl ClinePassProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for ClinePassProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for ClinePassProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let raw_key = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            let api_key = clean_key(raw_key)
                .ok_or_else(|| FetchError::NoSession("ClinePass API key is empty".to_string()))?;

            let body = JsonRequest::get("https://api.cline.bot/api/v1/users/me/plan/usage-limits")
                .bearer(&api_key)
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
    fn normalizes_happy_path_with_all_windows() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 25.5,
                        "resetsAt": "2026-07-11T12:30:00Z"
                    },
                    {
                        "type": "weekly",
                        "percentUsed": 50.0,
                        "resetsAt": "2026-07-18T12:30:00.123Z"
                    },
                    {
                        "type": "monthly",
                        "percentUsed": 75.0,
                        "resetsAt": "2026-08-11T12:30:00Z"
                    },
                    {
                        "type": "unknown_type",
                        "percentUsed": 10.0,
                        "resetsAt": "2026-07-11T12:30:00Z"
                    }
                ]
            }
        }"#;

        let usage = normalize_usage(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.5);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-11T12:30:00Z"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 50.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-18T12:30:00Z"));
        assert_eq!(secondary.window_minutes, Some(10080));

        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 75.0);
        assert_eq!(tertiary.resets_at.as_deref(), Some("2026-08-11T12:30:00Z"));
        assert_eq!(tertiary.window_minutes, Some(43200));
    }

    #[test]
    fn percent_clamping() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 120.0,
                        "resetsAt": null
                    },
                    {
                        "type": "weekly",
                        "percentUsed": -5.0,
                        "resetsAt": null
                    }
                ]
            }
        }"#;

        let usage = normalize_usage(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 100.0);

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 0.0);
    }

    #[test]
    fn missing_resets_at_keeps_window_with_none() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 25.0,
                        "resetsAt": null
                    }
                ]
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn garbage_timestamp_returns_decode_error() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 25.0,
                        "resetsAt": "garbage"
                    }
                ]
            }
        }"#;

        let result = normalize_usage(body);
        assert!(matches!(result, Err(FetchError::Decode(_))));
    }

    #[test]
    fn garbage_timestamp_on_unknown_type_is_ignored() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "unknown_type",
                        "percentUsed": 25.0,
                        "resetsAt": "garbage"
                    }
                ]
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());
    }

    #[test]
    fn success_false_returns_decode_error() {
        let body = br#"{
            "success": false,
            "data": {
                "limits": []
            }
        }"#;

        let result = normalize_usage(body);
        assert!(matches!(result, Err(FetchError::Decode(_))));
    }

    #[tokio::test]
    async fn fetch_handle_missing_key_returns_no_session() {
        let prev_cline = std::env::var("CLINE_API_KEY").ok();
        let prev_clinepass = std::env::var("CLINEPASS_API_KEY").ok();
        std::env::remove_var("CLINE_API_KEY");
        std::env::remove_var("CLINEPASS_API_KEY");

        let provider = ClinePassProvider::new();
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert!(matches!(attempt.usage, Err(FetchError::NoSession(_))));

        if let Some(val) = prev_cline {
            std::env::set_var("CLINE_API_KEY", val);
        }
        if let Some(val) = prev_clinepass {
            std::env::set_var("CLINEPASS_API_KEY", val);
        }
    }

    #[test]
    fn clean_key_strips_quotes_and_whitespace() {
        assert_eq!(clean_key("  key  ".to_string()), Some("key".to_string()));
        assert_eq!(clean_key("\"key\"".to_string()), Some("key".to_string()));
        assert_eq!(clean_key("'key'".to_string()), Some("key".to_string()));
        assert_eq!(
            clean_key("  \"key\"  ".to_string()),
            Some("key".to_string())
        );
        assert_eq!(clean_key("".to_string()), None);
        assert_eq!(clean_key("\"\"".to_string()), None);
    }
}
