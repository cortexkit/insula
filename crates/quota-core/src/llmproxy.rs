//! LLMProxy usage fetcher — credential from an environment variable.
//!
//! LLMProxy fronts multiple upstream providers and reports a `quota_groups` list
//! per upstream; the account-wide signal is the WORST remaining group, since the
//! first pool to exhaust gates the account. Both emitted numbers come from that
//! same group: `usedPercent = 100 - remaining_percent` and its own `reset_time`.
//!
//! Endpoint: `GET {base}/v1/quota-stats` (base from `LLM_PROXY_BASE_URL`, already
//! `/v1`-aware). Credential: `LLM_PROXY_API_KEY` as an `Authorization: Bearer`.
//! No fixed window length is reported, so `windowMinutes` is omitted.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `LLM_PROXY_API_KEY` available. Endpoint, bearer auth, and the
//! `providers[].quota_groups[].{remaining_percent, reset_time}` response shape +
//! worst-group aggregation are ported from CodexBar
//! (`Providers/LLMProxy/LLMProxyUsageFetcher.swift:133-159, 215-218` and
//! `LLMProxyUsageSnapshot.swift:62-66`). Rides the live-proven `http.rs`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "llmproxy";
const API_KEY_ENV: &[&str] = &["LLM_PROXY_API_KEY"];
const BASE_URL_ENV: &[&str] = &["LLM_PROXY_BASE_URL"];

#[derive(Debug, Deserialize)]
struct QuotaStatsResponse {
    providers: BTreeMap<String, ProviderStats>,
}

#[derive(Debug, Deserialize)]
struct ProviderStats {
    quota_groups: Option<Vec<QuotaGroup>>,
}

#[derive(Debug, Deserialize)]
struct QuotaGroup {
    remaining_percent: Option<f64>,
    reset_time: Option<String>,
}

/// Build the quota-stats URL, honoring a `/v1`-suffixed base.
fn quota_stats_url(base: &str) -> Option<String> {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.ends_with("/v1") {
        Some(format!("{trimmed}/quota-stats"))
    } else {
        Some(format!("{trimmed}/v1/quota-stats"))
    }
}

/// Normalize the quota-stats body to [`Usage`].
///
/// The account is gated by its most-depleted pool, so the worst group across every
/// upstream provider is selected and BOTH numbers are taken from that one group.
///
/// The reference implementation reduces the minimum remaining percent and the
/// minimum reset time independently, but it keeps them as two separate scalars and
/// never claims they describe the same pool. A [`RateWindow`] does make that claim:
/// it means "this much of THIS window is used, and THIS window resets then". Pairing
/// independent minima inside one would assert something the source data does not
/// support, and the error has a direction — the minimum reset is always at or before
/// the binding group's, so the window would promise relief that has not arrived and
/// a consumer pacing on it would resume into a still-exhausted pool.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: QuotaStatsResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("llmproxy quota-stats not decodable: {e}")))?;

    let mut binding: Option<(f64, Option<&str>)> = None;
    for stats in response.providers.values() {
        let Some(groups) = &stats.quota_groups else {
            continue;
        };
        for group in groups {
            let Some(remaining) = group.remaining_percent else {
                continue;
            };
            if binding.is_none_or(|(worst, _)| remaining < worst) {
                binding = Some((remaining, group.reset_time.as_deref()));
            }
        }
    }

    // The percent is load-bearing and the reset is optional: a pool reporting how
    // depleted it is without saying when it refills is still real pressure, and
    // dropping it would make an exhausted account read as no signal at all.
    let primary = binding.map(|(remaining, resets_at)| RateWindow {
        used_percent: (100.0 - remaining).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: resets_at.map(str::to_string),
        window_minutes: None,
        used_count: None,
        total_count: None,
    });

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The LLMProxy usage provider.
pub struct LlmProxyProvider {
    http: reqwest::Client,
}

impl LlmProxyProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
        }
    }
}

impl Default for LlmProxyProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for LlmProxyProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            let base = env::first_env(BASE_URL_ENV).ok_or_else(|| {
                FetchError::NoSession("LLM_PROXY_BASE_URL is not set".to_string())
            })?;
            // The key and the base URL are both present; the URL just does not
            // resolve to a usable endpoint, which is a misconfiguration.
            let url = quota_stats_url(&base).ok_or_else(|| {
                FetchError::CredentialUnusable("LLM_PROXY_BASE_URL is empty".to_string())
            })?;

            let body = JsonRequest::get(url)
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
    fn picks_worst_group_across_providers() {
        // Shaped like the real /v1/quota-stats response.
        let body = br#"{
            "providers": {
                "openai": { "quota_groups": [ { "remaining_percent": 80.0, "reset_time": "2026-06-22T17:00:00Z" } ] },
                "anthropic": { "quota_groups": [
                    { "remaining_percent": 30.0, "reset_time": "2026-06-22T18:00:00Z" },
                    { "remaining_percent": 55.0, "reset_time": "2026-06-22T19:00:00Z" }
                ] }
            },
            "summary": { "total_requests": 10 }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        // The worst group is anthropic's 30%-remaining pool, so the window reports
        // 70% used and ITS reset. Taking the earliest reset across all groups would
        // pair this pressure with openai's 17:00 — an hour before the pool that
        // actually gates the account recovers, so a consumer would resume into a
        // still-exhausted upstream.
        assert_eq!(primary.used_percent, 70.0);
        assert_eq!(
            primary.resets_at.as_deref(),
            Some("2026-06-22T18:00:00Z"),
            "the reset must belong to the group whose depletion is being reported"
        );
        assert_eq!(primary.window_minutes, None);
    }

    #[test]
    fn no_groups_yields_no_window() {
        let body = br#"{ "providers": { "openai": { "quota_groups": null } } }"#;
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn worst_group_without_a_reset_still_reports_its_pressure() {
        // The percent is load-bearing and the reset is optional. Dropping the window
        // here would make a 90%-depleted account read as no signal rather than as
        // nearly exhausted, which is the more expensive of the two errors.
        let body =
            br#"{ "providers": { "x": { "quota_groups": [ { "remaining_percent": 10.0 } ] } } }"#;
        let primary = normalize_usage(body)
            .unwrap()
            .primary
            .expect("real pressure");
        assert_eq!(primary.used_percent, 90.0);
        assert_eq!(
            primary.resets_at, None,
            "an absent reset is carried as absent, never borrowed from another group"
        );
    }

    #[test]
    fn a_reset_is_never_borrowed_from_a_healthier_group() {
        // The dangerous pairing: the depleted group reports no reset while a
        // healthy one does. Borrowing it would promise relief for pressure that
        // reset does not relieve.
        let body = br#"{
            "providers": {
                "a": { "quota_groups": [ { "remaining_percent": 5.0 } ] },
                "b": { "quota_groups": [ { "remaining_percent": 90.0, "reset_time": "2026-06-22T17:00:00Z" } ] }
            }
        }"#;
        let primary = normalize_usage(body).unwrap().primary.unwrap();
        assert_eq!(primary.used_percent, 95.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn url_v1_aware() {
        assert_eq!(
            quota_stats_url("https://proxy.example.com").as_deref(),
            Some("https://proxy.example.com/v1/quota-stats")
        );
        assert_eq!(
            quota_stats_url("https://proxy.example.com/v1").as_deref(),
            Some("https://proxy.example.com/v1/quota-stats")
        );
        assert_eq!(quota_stats_url("").as_deref(), None);
    }
}
