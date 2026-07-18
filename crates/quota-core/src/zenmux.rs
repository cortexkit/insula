//! ZenMux usage fetcher.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `ZENMUX_MANAGEMENT_API_KEY` was available.
//!
//! Ported from CodexBar v0.44.0:
//! - `Sources/CodexBarCore/Providers/ZenMux/ZenMuxUsageFetcher.swift:121-205`
//! - `Sources/CodexBarCore/Providers/ZenMux/ZenMuxSettingsReader.swift:4-23`
//!
//! Note: The PAYG balance USD call (`payg/balance`) is BALANCE-AXIS and is skipped
//! entirely (not fetched), as instructed.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{AccountInfo, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "zenmux";
const API_KEY_ENV: &[&str] = &["ZENMUX_MANAGEMENT_API_KEY"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SUBSCRIPTION_DETAIL_URL: &str = "https://zenmux.ai/api/v1/management/subscription/detail";

#[derive(Debug, Deserialize)]
struct SubscriptionEnvelope {
    success: bool,
    data: DataPayload,
}

#[derive(Debug, Deserialize)]
struct DataPayload {
    plan: Plan,
    #[serde(rename = "account_status")]
    account_status: String,
    #[serde(rename = "quota_5_hour")]
    quota_5_hour: Quota,
    #[serde(rename = "quota_7_day")]
    quota_7_day: Quota,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Plan {
    tier: String,
    #[serde(rename = "expires_at")]
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Quota {
    #[serde(rename = "usage_percentage")]
    usage_percentage: f64,
    #[serde(rename = "resets_at")]
    resets_at: Option<String>,
    #[serde(rename = "max_flows")]
    max_flows: f64,
    #[serde(rename = "used_flows")]
    used_flows: f64,
    #[serde(rename = "remaining_flows")]
    remaining_flows: f64,
}

fn clean_setting(value: Option<String>) -> Option<String> {
    let mut value = value?.trim().to_string();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value = value[1..value.len() - 1].trim().to_string();
    }
    (!value.is_empty()).then_some(value)
}

fn settings_from_env() -> Result<String, FetchError> {
    clean_setting(env::first_env(API_KEY_ENV))
        .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))
}

fn reported_reset_at(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    let reset_at = raw.trim();
    (!reset_at.is_empty() && chrono::DateTime::parse_from_rfc3339(reset_at).is_ok())
        .then(|| reset_at.to_string())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

fn plan_label(tier: &str) -> Option<String> {
    let tier = tier.trim();
    if tier.is_empty() {
        None
    } else {
        Some(format!("{} plan", capitalize(tier)))
    }
}

fn login_method(plan_tier: &str, account_status: &str) -> Option<String> {
    let plan = plan_tier.trim();
    let status = account_status.trim();
    let plan_lbl = plan_label(plan);

    if status.eq_ignore_ascii_case("healthy") || status.is_empty() {
        plan_lbl
    } else {
        let status_cap = capitalize(status);
        match plan_lbl {
            Some(lbl) => Some(format!("{} · {}", lbl, status_cap)),
            None => Some(status_cap),
        }
    }
}

pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let (usage, _) = normalize_usage_envelope(body)?;
    Ok(usage)
}

pub fn normalize_usage_envelope(body: &[u8]) -> Result<(Usage, Option<AccountInfo>), FetchError> {
    let response: SubscriptionEnvelope = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("zenmux usage not decodable: {error}")))?;

    if !response.success {
        return Err(FetchError::Decode(
            "zenmux subscription response reported failure".to_string(),
        ));
    }

    let quota_5h = response.data.quota_5_hour;
    let quota_7d = response.data.quota_7_day;

    if !quota_5h.usage_percentage.is_finite() || !quota_7d.usage_percentage.is_finite() {
        return Err(FetchError::Decode(
            "zenmux response has non-finite usage percentage".to_string(),
        ));
    }

    let primary = RateWindow {
        used_percent: (quota_5h.usage_percentage * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: reported_reset_at(quota_5h.resets_at),
        window_minutes: Some(5 * 60),
    };

    let secondary = RateWindow {
        used_percent: (quota_7d.usage_percentage * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: reported_reset_at(quota_7d.resets_at),
        window_minutes: Some(7 * 24 * 60),
    };

    let usage = Usage {
        primary: Some(primary),
        secondary: Some(secondary),
        tertiary: None,
        extra_rate_windows: None,
    };

    let plan_type = login_method(&response.data.plan.tier, &response.data.account_status);
    let account_info = plan_type.map(|pt| AccountInfo {
        email: None,
        org_name: None,
        plan_type: Some(pt),
    });

    Ok((usage, account_info))
}

pub struct ZenMuxProvider {
    http: reqwest::Client,
}

impl ZenMuxProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for ZenMuxProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for ZenMuxProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = settings_from_env()?;
            let body = JsonRequest::get(SUBSCRIPTION_DETAIL_URL)
                .bearer(&api_key)
                .timeout(REQUEST_TIMEOUT)
                .send(&self.http)
                .await?;
            let (usage, account_info) = normalize_usage_envelope(&body)?;
            let mut entry = ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage);
            entry.account_info = account_info;
            Ok(entry)
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_codexbar_shaped_subscription_to_rate_windows() {
        let body = br#"{
            "success": true,
            "data": {
                "plan": {
                    "tier": "pro",
                    "expires_at": "2026-07-18T12:30:00Z"
                },
                "account_status": "healthy",
                "quota_5_hour": {
                    "usage_percentage": 0.25,
                    "resets_at": "2026-07-11T12:30:00Z",
                    "max_flows": 100.0,
                    "used_flows": 25.0,
                    "remaining_flows": 75.0
                },
                "quota_7_day": {
                    "usage_percentage": 0.40,
                    "resets_at": "2026-07-18T12:30:00Z",
                    "max_flows": 1000.0,
                    "used_flows": 400.0,
                    "remaining_flows": 600.0
                }
            }
        }"#;

        let (usage, account_info) = normalize_usage_envelope(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-11T12:30:00Z"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 40.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-18T12:30:00Z"));
        assert_eq!(secondary.window_minutes, Some(10080));

        let info = account_info.unwrap();
        assert_eq!(info.plan_type.as_deref(), Some("Pro plan"));
    }

    #[test]
    fn clamps_usage_percentage_to_percent() {
        let body = br#"{
            "success": true,
            "data": {
                "plan": {
                    "tier": "pro",
                    "expires_at": null
                },
                "account_status": "healthy",
                "quota_5_hour": {
                    "usage_percentage": 1.5,
                    "resets_at": null,
                    "max_flows": 100.0,
                    "used_flows": 150.0,
                    "remaining_flows": -50.0
                },
                "quota_7_day": {
                    "usage_percentage": -0.5,
                    "resets_at": null,
                    "max_flows": 1000.0,
                    "used_flows": -500.0,
                    "remaining_flows": 1500.0
                }
            }
        }"#;

        let (usage, _) = normalize_usage_envelope(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 100.0);

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 0.0);
    }

    #[test]
    fn keeps_window_when_reset_is_absent() {
        let body = br#"{
            "success": true,
            "data": {
                "plan": {
                    "tier": "pro",
                    "expires_at": null
                },
                "account_status": "healthy",
                "quota_5_hour": {
                    "usage_percentage": 0.25,
                    "resets_at": null,
                    "max_flows": 100.0,
                    "used_flows": 25.0,
                    "remaining_flows": 75.0
                },
                "quota_7_day": {
                    "usage_percentage": 0.40,
                    "resets_at": null,
                    "max_flows": 1000.0,
                    "used_flows": 400.0,
                    "remaining_flows": 600.0
                }
            }
        }"#;

        let (usage, _) = normalize_usage_envelope(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.resets_at, None);

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.resets_at, None);
    }

    #[test]
    fn missing_environment_settings_return_no_session() {
        std::env::remove_var("ZENMUX_MANAGEMENT_API_KEY");
        assert!(matches!(settings_from_env(), Err(FetchError::NoSession(_))));
    }
}
