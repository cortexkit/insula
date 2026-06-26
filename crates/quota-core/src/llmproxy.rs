//! LLMProxy usage fetcher — credential from an environment variable.
//!
//! LLMProxy fronts multiple upstream providers and reports a `quota_groups` list
//! per upstream; the account-wide signal is the WORST remaining group (the first
//! pool to exhaust gates the account). We mirror CodexBar's aggregate: take the
//! minimum `remaining_percent` across all groups → `usedPercent = 100 -
//! min_remaining`, and the matching group's `reset_time` → `resetsAt`.
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
/// Faithful to CodexBar's aggregate (`LLMProxyUsageFetcher.swift:265-267`):
/// `usedPercent = 100 - min(remaining_percent)` and `resetsAt = min(reset_time)`
/// are computed INDEPENDENTLY across every group of every upstream provider — the
/// account is gated by the most-depleted pool and the soonest reset, which need
/// not be the same group.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: QuotaStatsResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("llmproxy quota-stats not decodable: {e}")))?;

    let mut min_remaining: Option<f64> = None;
    let mut min_reset: Option<String> = None;
    for stats in response.providers.values() {
        let Some(groups) = &stats.quota_groups else {
            continue;
        };
        for group in groups {
            if let Some(remaining) = group.remaining_percent {
                min_remaining = Some(min_remaining.map_or(remaining, |m| m.min(remaining)));
            }
            if let Some(reset) = &group.reset_time {
                // Lexicographic min works for like-formatted RFC 3339 timestamps,
                // which is what the proxy emits for every group.
                min_reset = Some(match min_reset {
                    Some(current) if current <= *reset => current,
                    _ => reset.clone(),
                });
            }
        }
    }

    let primary = match (min_remaining, min_reset) {
        (Some(remaining), Some(resets_at)) => Some(RateWindow {
            used_percent: (100.0 - remaining).clamp(0.0, 100.0),
            resets_at: Some(resets_at),
            window_minutes: None,
        }),
        _ => None,
    };

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
            http: reqwest::Client::new(),
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

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let api_key = env::first_env(API_KEY_ENV)
            .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
        let base = env::first_env(BASE_URL_ENV)
            .ok_or_else(|| FetchError::NoSession("LLM_PROXY_BASE_URL is not set".to_string()))?;
        let url = quota_stats_url(&base)
            .ok_or_else(|| FetchError::NoSession("LLM_PROXY_BASE_URL is empty".to_string()))?;

        let body = JsonRequest::get(url)
            .bearer(&api_key)
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
        // Independent aggregation: min remaining = 30% → used 70%; min reset =
        // 17:00 (openai's group), which need not be the most-depleted group's.
        assert_eq!(primary.used_percent, 70.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T17:00:00Z"));
        assert_eq!(primary.window_minutes, None);
    }

    #[test]
    fn no_groups_yields_no_window() {
        let body = br#"{ "providers": { "openai": { "quota_groups": null } } }"#;
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn group_without_reset_is_dropped() {
        let body =
            br#"{ "providers": { "x": { "quota_groups": [ { "remaining_percent": 10.0 } ] } } }"#;
        // Worst group has no reset_time → no well-formed window.
        assert!(normalize_usage(body).unwrap().primary.is_none());
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
