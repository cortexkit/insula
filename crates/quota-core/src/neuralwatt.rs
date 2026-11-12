//! NeuralWatt usage provider.
//!
//! The reported prepaid USD balance is a resetless pool, separate from the
//! subscription's kWh window and the key's allowance window. Missing balances
//! are not derived from total credits or consumption.
//!
//! VERIFICATION:
//! This port is fixture-verified against CodexBar source.
//! Balance mapping: CodexBar v0.84.0
//! `Sources/CodexBarCore/Resources/Plugins/neuralwatt.js`.
//! Ported from CodexBar:
//! - `Sources/CodexBarCore/Providers/NeuralWatt/NeuralWattUsageFetcher.swift`
//! - `Sources/CodexBarCore/Providers/NeuralWatt/NeuralWattSettingsReader.swift`
//!
//! The fetcher exists up to CodexBar v0.64.0 and NO LONGER EXISTS from v0.64.1:
//! upstream moved this provider into a bundled plugin,
//! `Sources/CodexBarCore/Resources/Plugins/neuralwatt.js`. Read the fetcher
//! citation at v0.64.0 or earlier; the plugin is where to look for later changes.
//! (Recorded for `scripts/parity-citations.py`, which reports a cited file that
//! is gone at the current anchor until its module says what replaced it.)

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::time::Duration;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{Amount, ExtraWindow, Pool, PoolBasis, PoolFunding, ProviderUsage, RateWindow, Usage},
    money::parse_amount,
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "neuralwatt";
const DEFAULT_BASE: &str = "https://api.neuralwatt.com";
const API_KEY_ENV: &[&str] = &["NEURALWATT_API_KEY"];
const BASE_URL_ENV: &[&str] = &["NEURALWATT_API_URL"];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct NeuralWattBalance {
    credits_remaining_usd: Option<Box<RawValue>>,
    total_credits_usd: Option<f64>,
    credits_used_usd: Option<f64>,
    #[serde(rename = "accounting_method")]
    _accounting_method: Option<String>,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct NeuralWattQuotaResponse {
    #[serde(rename = "snapshot_at")]
    _snapshot_at: Option<String>,
    balance: Option<NeuralWattBalance>,
    #[serde(rename = "usage")]
    _usage: Option<NeuralWattUsage>,
    #[serde(rename = "limits")]
    _limits: Option<NeuralWattLimits>,
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
    normalize_reading(body).map(|(usage, _)| usage)
}

fn reported_balance(balance: &NeuralWattBalance) -> Result<Option<Amount>, FetchError> {
    balance
        .credits_remaining_usd
        .as_deref()
        .map(|raw| {
            // Preserve the JSON decimal text, including sub-cent precision and
            // amounts beyond f64's exact range. Quoted numbers are not numbers.
            parse_amount(raw.get(), "USD").ok_or_else(|| {
                FetchError::Decode(
                    "Neuralwatt credits_remaining_usd is not an exact USD amount".to_string(),
                )
            })
        })
        .transpose()
        .map(|amount| amount.filter(|amount| amount.minor >= 0))
}

fn normalize_reading(body: &[u8]) -> Result<(Usage, Option<Pool>), FetchError> {
    let decoded: NeuralWattQuotaResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("failed to parse Neuralwatt response: {e}")))?;

    let balance = decoded
        .balance
        .as_ref()
        .ok_or_else(|| FetchError::Decode("Missing Neuralwatt balance object".to_string()))?;

    let remaining = reported_balance(balance)?;
    if remaining.is_none()
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
            window_kind: match sub
                .billing_interval
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "month" | "monthly" => {
                    Some(cortexkit_provider_usage::window_kind::MONTHLY.to_string())
                }
                "week" | "weekly" => {
                    Some(cortexkit_provider_usage::window_kind::WEEKLY.to_string())
                }
                "day" | "daily" => Some(cortexkit_provider_usage::window_kind::DAILY.to_string()),
                "hour" | "hourly" => {
                    Some(cortexkit_provider_usage::window_kind::HOURLY.to_string())
                }
                _ => None,
            },
            used_percent,
            raw_used_percent: None,
            resets_at,
            window_minutes,
            used_count: None,
            total_count: None,
            regeneration: None,
            breakdown: None,
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
                window_kind: match period.trim().to_ascii_lowercase().as_str() {
                    "hour" | "hourly" => {
                        Some(cortexkit_provider_usage::window_kind::HOURLY.to_string())
                    }
                    "day" | "daily" => {
                        Some(cortexkit_provider_usage::window_kind::DAILY.to_string())
                    }
                    "week" | "weekly" => {
                        Some(cortexkit_provider_usage::window_kind::WEEKLY.to_string())
                    }
                    "month" | "monthly" => {
                        Some(cortexkit_provider_usage::window_kind::MONTHLY.to_string())
                    }
                    _ => None,
                },
                used_percent: percent,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: None,
                used_count: None,
                total_count: None,
                regeneration: None,
                breakdown: None,
            }),
        }])
    }();

    let usage = Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows,
    };
    let pool = remaining.map(|remaining| Pool {
        id: "prepaid_balance".to_string(),
        label: "NeuralWatt prepaid balance".to_string(),
        // Neither the prepaid label nor accounting_method states whether this
        // credit was bought or granted, or whether the key may spend it.
        funding: PoolFunding::Unknown,
        remaining: Some(remaining),
        total: None,
        basis: PoolBasis::Reported,
        spendable: None,
        resets_at: None,
        expires_at: None,
        unlimited: None,
    });
    Ok((usage, pool))
}

fn normalize_response(body: &[u8]) -> Result<ProviderUsage, FetchError> {
    let (usage, pool) = normalize_reading(body)?;
    let mut entry = ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage);
    entry.spend = pool.map(|pool| vec![pool]);
    Ok(entry)
}

/// The NeuralWatt usage provider.
pub struct NeuralWattProvider {
    http: reqwest::Client,
}

impl NeuralWattProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
        }
    }

    async fn fetch_handle_from(
        &self,
        _handle: &CredentialHandle,
        lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key_raw = env::first_env_from(API_KEY_ENV, &lookup)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            // The variable is set but holds nothing usable, which is a
            // configuration mistake rather than an absent credential.
            let api_key = clean_env_value(&api_key_raw).ok_or_else(|| {
                FetchError::CredentialUnusable("NEURALWATT_API_KEY is empty".to_string())
            })?;

            let base_url_raw = env::first_env_from(BASE_URL_ENV, lookup);
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

            normalize_response(&body)
        }
        .await;
        FetchAttempt::from_provider_usage(result)
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

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        self.fetch_handle_from(handle, |key| std::env::var_os(key))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CodexBar v0.84.0, TestsPlugin/NeuralWattPluginTests.swift, first fixture.
    const PREPAID_WITH_WINDOWS: &[u8] = br#"{
      "snapshot_at": "2026-04-16T18:30:00Z",
      "balance": {
        "credits_remaining_usd": 32.6774,
        "total_credits_usd": 52.34,
        "credits_used_usd": 19.6626,
        "accounting_method": "energy"
      },
      "usage": {
        "lifetime": {"cost_usd": 243.9145, "requests": 37801, "tokens": 1235477176, "energy_kwh": 15.6009},
        "current_month": {"cost_usd": 160.1463, "requests": 23902, "tokens": 1116658995, "energy_kwh": 9.7278}
      },
      "limits": {"overage_limit_usd": null, "rate_limit_tier": "standard"},
      "subscription": {
        "plan": "standard",
        "status": "active",
        "billing_interval": "month",
        "current_period_start": "2026-04-11T05:05:25Z",
        "current_period_end": "2026-05-11T05:05:25Z",
        "auto_renew": true,
        "kwh_included": 20.0,
        "kwh_used": 13.9023,
        "kwh_remaining": 6.0977,
        "in_overage": false
      },
      "key": {
        "name": "my-production-key",
        "allowance": {"limit_usd": 50.0, "period": "monthly", "spent_usd": 12.5, "remaining_usd": 37.5, "blocked": false}
      }
    }"#;

    #[test]
    fn reported_balance_is_separate_from_subscription_and_key_windows() {
        let attempt = FetchAttempt::from_provider_usage(normalize_response(PREPAID_WITH_WINDOWS));
        let usage = attempt.usage.unwrap();
        let primary = usage.primary.unwrap();
        assert!((primary.used_percent - 69.5115).abs() < 1e-10);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-05-11T05:05:25Z"));
        assert_eq!(primary.window_minutes, Some(43200));
        assert_eq!(primary.window_kind.as_deref(), Some("monthly"));
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].id.as_deref(), Some("key-allowance"));
        assert_eq!(extras[0].window.as_ref().unwrap().used_percent, 25.0);
        assert!(usage.secondary.is_none() && usage.tertiary.is_none());

        let pools = attempt.pools.unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id, "prepaid_balance");
        assert_eq!(pools[0].label, "NeuralWatt prepaid balance");
        assert_eq!(
            pools[0].remaining,
            Some(Amount {
                minor: 326774,
                exponent: 4,
                unit: "USD".to_string(),
            })
        );
        assert_eq!(pools[0].basis, PoolBasis::Reported);
        assert_eq!(pools[0].funding, PoolFunding::Unknown);
        assert_eq!(pools[0].total, None);
        assert_eq!(pools[0].spendable, None);
        assert_eq!(pools[0].resets_at, None);
        assert_eq!(pools[0].expires_at, None);
        assert_eq!(pools[0].unlimited, None);
    }

    #[test]
    fn reported_balances_keep_exact_decimals_and_zero() {
        for (number, minor, exponent) in [
            ("0.0", 0, 1),
            ("0.000000001", 1, 9),
            ("4.5000", 45000, 4),
            ("900719925474099.13", 90_071_992_547_409_913, 2),
        ] {
            let body = format!(r#"{{"balance":{{"credits_remaining_usd":{number}}}}}"#);
            let entry = normalize_response(body.as_bytes()).unwrap();
            let pool = &entry.spend.as_ref().unwrap()[0];
            assert_eq!(
                pool.remaining,
                Some(Amount {
                    minor,
                    exponent,
                    unit: "USD".to_string(),
                }),
                "{number}"
            );
            let wire = serde_json::to_value(&entry).unwrap();
            assert_eq!(wire["spend"][0]["remaining"]["minor"], minor);
        }
    }

    #[test]
    fn absent_remaining_is_not_derived_or_zero() {
        // The 10-minus-3 case is from CodexBar v0.84.0,
        // TestsPlugin/NeuralWattPluginTests.swift. Only a stated remainder is
        // reported here; total minus used would have different provenance.
        for balance in [
            r#"{"total_credits_usd":10,"credits_used_usd":3}"#,
            r#"{"credits_remaining_usd":null,"total_credits_usd":10,"credits_used_usd":3}"#,
            r#"{"credits_remaining_usd":-1,"total_credits_usd":10,"credits_used_usd":3}"#,
        ] {
            let body = format!(
                r#"{{"balance":{balance},"subscription":{{"kwh_included":10,"kwh_used":2.5}}}}"#
            );
            let entry = normalize_response(body.as_bytes()).unwrap();
            assert_eq!(entry.usage.unwrap().primary.unwrap().used_percent, 25.0);
            assert_eq!(entry.spend, None);
        }
    }

    #[test]
    fn balance_only_accounts_publish_pools_without_windows() {
        // Balance-only and zero-balance inputs from CodexBar v0.84.0,
        // TestsPlugin/NeuralWattPluginTests.swift, with unused fields omitted.
        for number in ["4.5", "0.0"] {
            let body = format!(
                r#"{{"balance":{{"credits_remaining_usd":{number}}},"subscription":null,"key":{{"allowance":null}}}}"#
            );
            let attempt = FetchAttempt::from_provider_usage(normalize_response(body.as_bytes()));
            let usage = attempt.usage.unwrap();
            assert!(
                usage.primary.is_none() && usage.secondary.is_none() && usage.tertiary.is_none()
            );
            assert!(usage.extra_rate_windows.is_none());
            assert_eq!(attempt.pools.unwrap().len(), 1);
        }
    }

    #[test]
    fn malformed_reported_balances_are_decode_failed_without_echoing_payloads() {
        for raw in [
            r#""secret_fixture""#,
            r#""32.6774""#,
            "true",
            "{}",
            "[]",
            "1e1",
            "0.0000000001",
            "9223372036854775808",
        ] {
            let body = format!(
                r#"{{"balance":{{"credits_remaining_usd":{raw},"total_credits_usd":10}}}}"#
            );
            let error = normalize_response(body.as_bytes()).unwrap_err();
            assert!(matches!(error, FetchError::Decode(_)), "{error:?}");
            assert_eq!(
                error.to_string(),
                "decode error: Neuralwatt credits_remaining_usd is not an exact USD amount"
            );
        }
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("month"), "Month");
        assert_eq!(capitalize("allowance"), "Allowance");
        assert_eq!(capitalize(""), "");
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
        assert_eq!(primary.window_kind.as_deref(), Some("monthly"));
        assert_eq!(primary.window_minutes, Some(44640));

        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        let extra = &extras[0];
        assert_eq!(extra.id.as_deref(), Some("key-allowance"));
        assert_eq!(extra.title.as_deref(), Some("Key Month"));
        let extra_win = extra.window.as_ref().unwrap();
        assert_eq!(extra_win.used_percent, 50.0);
        assert_eq!(extra_win.resets_at, None);
        assert_eq!(extra_win.window_kind.as_deref(), Some("monthly"));
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
        let provider = NeuralWattProvider::new();
        let handle = CredentialHandle::implicit();

        // Every variable except the key is set, so a lookup under a misspelt
        // name would find something here and fail this arm.
        let attempt = provider
            .fetch_handle_from(&handle, |key| {
                (key != "NEURALWATT_API_KEY").then(|| "set".into())
            })
            .await;
        assert!(
            matches!(&attempt.usage, Err(FetchError::NoSession(message)) if message.contains("NEURALWATT_API_KEY")),
            "the absent key is named: {:?}",
            attempt.usage
        );

        // Control: only the real names hold values, and the provider reads the
        // key rather than reporting it absent. The endpoint override is plain
        // http, which is refused before any request is made.
        let attempt = provider
            .fetch_handle_from(&handle, |key| match key {
                "NEURALWATT_API_KEY" => Some("nw-key".into()),
                "NEURALWATT_API_URL" => Some("http://insecure.example".into()),
                _ => None,
            })
            .await;
        assert!(
            matches!(&attempt.usage, Err(FetchError::CredentialUnusable(message)) if message.contains("NEURALWATT_API_URL")),
            "{:?}",
            attempt.usage
        );
    }
}
