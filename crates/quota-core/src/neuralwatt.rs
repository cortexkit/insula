//! NeuralWatt usage provider.
//!
//! The prepaid balance (`credits_remaining_usd`) is parsed and not published.
//! Balances have a home now -- `Pool`s on `ProviderUsage::spend` -- so what is
//! missing is a credential to verify against, not somewhere to put it.
//!
//! VERIFICATION:
//! This port is fixture-verified against CodexBar source.
//! Ported from CodexBar:
//! - `Sources/CodexBarCore/Providers/NeuralWatt/NeuralWattUsageFetcher.swift`
//! - `Sources/CodexBarCore/Providers/NeuralWatt/NeuralWattSettingsReader.swift`

use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{ExtraWindow, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "neuralwatt";
const DEFAULT_BASE: &str = "https://api.neuralwatt.com";
const API_KEY_ENV: &[&str] = &["NEURALWATT_API_KEY"];
const BASE_URL_ENV: &[&str] = &["NEURALWATT_API_URL"];

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattBalance {
    credits_remaining_usd: Option<f64>,
    total_credits_usd: Option<f64>,
    credits_used_usd: Option<f64>,
    accounting_method: Option<String>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattUsagePeriod {
    cost_usd: Option<f64>,
    requests: Option<i64>,
    tokens: Option<i64>,
    energy_kwh: Option<f64>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattUsage {
    lifetime: Option<NeuralWattUsagePeriod>,
    current_month: Option<NeuralWattUsagePeriod>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattLimits {
    overage_limit_usd: Option<f64>,
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattSubscription {
    plan: Option<String>,
    status: Option<String>,
    billing_interval: Option<String>,
    current_period_start: Option<String>,
    current_period_end: Option<String>,
    auto_renew: Option<bool>,
    kwh_included: Option<f64>,
    kwh_used: Option<f64>,
    kwh_remaining: Option<f64>,
    in_overage: Option<bool>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattKeyAllowance {
    limit_usd: Option<f64>,
    period: Option<String>,
    spent_usd: Option<f64>,
    remaining_usd: Option<f64>,
    blocked: Option<bool>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattKey {
    name: Option<String>,
    allowance: Option<NeuralWattKeyAllowance>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
struct NeuralWattQuotaResponse {
    snapshot_at: Option<String>,
    balance: Option<NeuralWattBalance>,
    usage: Option<NeuralWattUsage>,
    limits: Option<NeuralWattLimits>,
    subscription: Option<NeuralWattSubscription>,
    key: Option<NeuralWattKey>,
}

fn valid_non_negative(value: Option<f64>) -> Option<f64> {
    let v = value?;
    if v.is_finite() && v >= 0.0 {
        Some(v)
    } else {
        None
    }
}

fn valid_positive(value: Option<f64>) -> Option<f64> {
    let v = value?;
    if v.is_finite() && v > 0.0 {
        Some(v)
    } else {
        None
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
    }
}

fn parse_iso8601(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok()
}

fn clean_env_value(raw: &str) -> Option<String> {
    crate::text::strip_wrapping_quotes(raw)
}

fn validate_and_normalize_url(raw: &str) -> Option<url::Url> {
    let has_scheme = raw.contains("://");
    let url_str = if has_scheme {
        raw.to_string()
    } else {
        format!("https://{}", raw)
    };
    let url = url::Url::parse(&url_str).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?;
    if host.is_empty()
        || host.contains('%')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    if host
        .chars()
        .any(|c| ['/', '\\', '?', '#', '@'].contains(&c))
    {
        return None;
    }
    Some(url)
}

fn quota_url(mut base_url: url::Url) -> url::Url {
    let ends_with_v1 = base_url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        == Some("v1");

    if let Ok(mut segments) = base_url.path_segments_mut() {
        segments.pop_if_empty();
        if ends_with_v1 {
            segments.push("quota");
        } else {
            segments.push("v1");
            segments.push("quota");
        }
    }
    base_url
}

/// Normalize the NeuralWatt quota response to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let decoded: NeuralWattQuotaResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("failed to parse Neuralwatt response: {e}")))?;

    let balance = decoded
        .balance
        .as_ref()
        .ok_or_else(|| FetchError::Decode("Missing Neuralwatt balance object".to_string()))?;

    if valid_non_negative(balance.credits_remaining_usd).is_none()
        && valid_non_negative(balance.credits_used_usd).is_none()
        && valid_positive(balance.total_credits_usd).is_none()
    {
        return Err(FetchError::Decode(
            "Missing Neuralwatt credit balance fields".to_string(),
        ));
    }

    let subscription = decoded.subscription.as_ref();

    // effectiveSubscriptionTotalKWh
    let effective_subscription_total_kwh = || -> Option<f64> {
        let sub = subscription?;
        if let Some(included) = valid_positive(sub.kwh_included) {
            return Some(included);
        }
        let used = valid_non_negative(sub.kwh_used)?;
        let remaining = valid_non_negative(sub.kwh_remaining)?;
        let total = used + remaining;
        if total > 0.0 {
            Some(total)
        } else {
            None
        }
    }();

    // effectiveSubscriptionUsedKWh
    let effective_subscription_used_kwh = || -> Option<f64> {
        let sub = subscription?;
        if let Some(used) = valid_non_negative(sub.kwh_used) {
            return Some(used);
        }
        let total = effective_subscription_total_kwh?;
        let remaining = valid_non_negative(sub.kwh_remaining)?;
        Some((total - remaining).max(0.0))
    }();

    // subscriptionRateWindow
    let primary = || -> Option<RateWindow> {
        let total = effective_subscription_total_kwh?;
        let used = effective_subscription_used_kwh?;
        let sub = subscription?;

        let window_minutes = if let (Some(start_str), Some(end_str)) =
            (&sub.current_period_start, &sub.current_period_end)
        {
            if let (Some(start), Some(end)) = (parse_iso8601(start_str), parse_iso8601(end_str)) {
                let delta = end.signed_duration_since(start).num_seconds();
                if delta > 0 {
                    Some((delta / 60).max(1))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let used_percent = (used / total * 100.0).clamp(0.0, 100.0);
        let resets_at = sub
            .current_period_end
            .clone()
            .and_then(|s| parse_iso8601(&s).map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()));

        Some(RateWindow {
            used_percent,
            raw_used_percent: None,
            resets_at,
            window_minutes,
            used_count: None,
            total_count: None,
        })
    }();

    let key_allowance_used_percent = || -> Option<f64> {
        let key = decoded.key.as_ref()?;
        let allowance = key.allowance.as_ref()?;
        if allowance.blocked == Some(true) {
            return Some(100.0);
        }
        let spent = allowance.spent_usd?;
        let limit = allowance.limit_usd?;
        if limit > 0.0 {
            Some((spent / limit * 100.0).clamp(0.0, 100.0))
        } else {
            None
        }
    }();

    let extra_rate_windows = || -> Option<Vec<ExtraWindow>> {
        let percent = key_allowance_used_percent?;
        let key = decoded.key.as_ref()?;
        let allowance = key.allowance.as_ref()?;
        let period = allowance.period.as_deref().unwrap_or("allowance");
        let period_title = capitalize(period);

        Some(vec![ExtraWindow {
            id: Some("key-allowance".to_string()),
            title: Some(format!("Key {}", period_title)),
            window: Some(RateWindow {
                used_percent: percent,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: None,
                used_count: None,
                total_count: None,
            }),
        }])
    }();

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows,
    })
}

/// The NeuralWatt usage provider.
pub struct NeuralWattProvider {
    http: reqwest::Client,
}

impl NeuralWattProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for NeuralWattProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for NeuralWattProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key_raw = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            // The variable is set but holds nothing usable, which is a
            // configuration mistake rather than an absent credential.
            let api_key = clean_env_value(&api_key_raw).ok_or_else(|| {
                FetchError::CredentialUnusable("NEURALWATT_API_KEY is empty".to_string())
            })?;

            let base_url_raw = env::first_env(BASE_URL_ENV);
            let base_url = if let Some(raw) = base_url_raw {
                let cleaned = clean_env_value(&raw).ok_or_else(|| {
                    FetchError::CredentialUnusable("NEURALWATT_API_URL is empty".to_string())
                })?;
                validate_and_normalize_url(&cleaned).ok_or_else(|| {
                    FetchError::CredentialUnusable(
                        "Neuralwatt endpoint override NEURALWATT_API_URL must use HTTPS or a bare host."
                            .to_string(),
                    )
                })?
            } else {
                url::Url::parse(DEFAULT_BASE).unwrap()
            };

            let url = quota_url(base_url).to_string();

            let body = JsonRequest::get(url)
                .bearer(&api_key)
                .timeout(Duration::from_secs(15))
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
    fn test_capitalize() {
        assert_eq!(capitalize("month"), "Month");
        assert_eq!(capitalize("allowance"), "Allowance");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn test_clean_env_value() {
        assert_eq!(clean_env_value("  foo  "), Some("foo".to_string()));
        assert_eq!(clean_env_value("\"foo\""), Some("foo".to_string()));
        assert_eq!(clean_env_value("'foo'"), Some("foo".to_string()));
        assert_eq!(clean_env_value("  \"  foo  \"  "), Some("foo".to_string()));
        assert_eq!(clean_env_value(""), None);
    }

    #[test]
    fn test_validate_and_normalize_url() {
        assert_eq!(
            validate_and_normalize_url("api.neuralwatt.com")
                .unwrap()
                .as_str(),
            "https://api.neuralwatt.com/"
        );
        assert_eq!(
            validate_and_normalize_url("https://api.neuralwatt.com")
                .unwrap()
                .as_str(),
            "https://api.neuralwatt.com/"
        );
        assert!(validate_and_normalize_url("http://api.neuralwatt.com").is_none());
        assert!(validate_and_normalize_url("https://user:pass@api.neuralwatt.com").is_none());
    }

    #[test]
    fn test_quota_url() {
        let base = url::Url::parse("https://api.neuralwatt.com").unwrap();
        assert_eq!(
            quota_url(base).as_str(),
            "https://api.neuralwatt.com/v1/quota"
        );

        let base_v1 = url::Url::parse("https://api.neuralwatt.com/v1").unwrap();
        assert_eq!(
            quota_url(base_v1).as_str(),
            "https://api.neuralwatt.com/v1/quota"
        );
    }

    #[test]
    fn test_normalize_codexbar_fixture() {
        let body = br#"{
            "snapshot_at": "2026-07-24T12:00:00Z",
            "balance": {
                "credits_remaining_usd": 10.0,
                "total_credits_usd": 20.0,
                "credits_used_usd": 10.0,
                "accounting_method": "prepaid"
            },
            "subscription": {
                "plan": "pro",
                "status": "active",
                "billing_interval": "month",
                "current_period_start": "2026-07-24T12:00:00Z",
                "current_period_end": "2026-08-24T12:00:00Z",
                "auto_renew": true,
                "kwh_included": 100.0,
                "kwh_used": 50.0,
                "kwh_remaining": 50.0,
                "in_overage": false
            },
            "key": {
                "name": "my-key",
                "allowance": {
                    "limit_usd": 50.0,
                    "period": "month",
                    "spent_usd": 25.0,
                    "remaining_usd": 25.0,
                    "blocked": false
                }
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-08-24T12:00:00Z"));
        assert_eq!(primary.window_minutes, Some(44640));

        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        let extra = &extras[0];
        assert_eq!(extra.id.as_deref(), Some("key-allowance"));
        assert_eq!(extra.title.as_deref(), Some("Key Month"));
        let extra_win = extra.window.as_ref().unwrap();
        assert_eq!(extra_win.used_percent, 50.0);
        assert_eq!(extra_win.resets_at, None);
        assert_eq!(extra_win.window_minutes, None);
    }

    #[test]
    fn test_total_fallback_path() {
        let body = br#"{
            "balance": {
                "credits_remaining_usd": 10.0
            },
            "subscription": {
                "kwh_used": 30.0,
                "kwh_remaining": 70.0,
                "current_period_start": "2026-07-24T12:00:00Z",
                "current_period_end": "2026-08-24T12:00:00Z"
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 30.0);
        assert_eq!(primary.window_minutes, Some(44640));
    }

    #[test]
    fn test_missing_subscription_retains_extras() {
        let body = br#"{
            "balance": {
                "credits_remaining_usd": 10.0
            },
            "key": {
                "allowance": {
                    "limit_usd": 50.0,
                    "period": "month",
                    "spent_usd": 25.0,
                    "remaining_usd": 25.0,
                    "blocked": false
                }
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].title.as_deref(), Some("Key Month"));
    }

    #[test]
    fn test_percent_clamp() {
        let body = br#"{
            "balance": {
                "credits_remaining_usd": 10.0
            },
            "subscription": {
                "kwh_included": 100.0,
                "kwh_used": 150.0,
                "kwh_remaining": -50.0
            },
            "key": {
                "allowance": {
                    "limit_usd": 50.0,
                    "spent_usd": 60.0,
                    "blocked": true
                }
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 100.0);

        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras[0].window.as_ref().unwrap().used_percent, 100.0);
    }

    #[tokio::test]
    async fn test_missing_key_no_session() {
        let old_val = std::env::var("NEURALWATT_API_KEY").ok();
        std::env::remove_var("NEURALWATT_API_KEY");

        let provider = NeuralWattProvider::new();
        let handle = CredentialHandle::implicit();
        let attempt = provider.fetch_handle(&handle).await;

        assert!(matches!(attempt.usage, Err(FetchError::NoSession(_))));

        if let Some(val) = old_val {
            std::env::set_var("NEURALWATT_API_KEY", val);
        }
    }
}
