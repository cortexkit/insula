//! GitHub Copilot usage fetcher — credential from an environment variable.
//!
//! Copilot reports a monthly, refilling quota: `quota_snapshots.{premium_
//! interactions,chat}` each carry `percent_remaining`, and the response carries a
//! real top-level `quota_reset_date`. CodexBar parses that reset date but drops it
//! in its SIMPLIFIED per-quota window; recovering it here is faithful use of a
//! genuine provider field, not a fabricated period. Quota refills monthly
//! (`monthly_quotas` / `quota_reset_date`), so `windowMinutes = 43200`.
//!
//! Auth: the GitHub OAuth token (`COPILOT_API_TOKEN`, or a `GITHUB_TOKEN`/`GH_TOKEN`
//! fallback) as `Authorization: token <token>` (GitHub's scheme, NOT Bearer), with
//! the editor headers GitHub's API expects. Endpoint:
//! `GET https://api.github.com/copilot_internal/user`.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no GitHub
//! Copilot token available. Endpoint, the `token` auth scheme + editor headers, and
//! the response shape (quota_snapshots, monthly/limited fallback, quota_reset_date)
//! are ported from CodexBar (`Sources/CodexBarCore/Providers/Copilot/
//! CopilotUsageFetcher.swift:36-143` and `Sources/CodexBarCore/CopilotUsageModels.swift:
//! 19-56, 214-304`). `quota_reset_date` is a raw provider string CodexBar stores
//! but never parses, so it is normalized when it parses as a date and passed
//! through otherwise. Rides the live-proven `http.rs`.

use async_trait::async_trait;
use chrono::TimeZone;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "copilot";
const API_KEY_ENV: &[&str] = &["COPILOT_API_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"];
const USAGE_URL: &str = "https://api.github.com/copilot_internal/user";
const MONTHLY_WINDOW_MINUTES: i64 = 30 * 24 * 60; // 43200

#[derive(Debug, Deserialize)]
struct CopilotUsageResponse {
    quota_snapshots: Option<QuotaSnapshots>,
    monthly_quotas: Option<QuotaCounts>,
    limited_user_quotas: Option<QuotaCounts>,
    quota_reset_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QuotaSnapshots {
    premium_interactions: Option<QuotaSnapshot>,
    chat: Option<QuotaSnapshot>,
}

#[derive(Debug, Deserialize)]
struct QuotaSnapshot {
    #[serde(default)]
    entitlement: f64,
    #[serde(default)]
    remaining: f64,
    percent_remaining: Option<f64>,
    #[serde(default)]
    quota_id: String,
}

#[derive(Debug, Deserialize)]
struct QuotaCounts {
    completions: Option<f64>,
    chat: Option<f64>,
}

impl QuotaSnapshot {
    /// CodexBar's placeholder/usability guards: a snapshot with no real data, or
    /// one missing `percent_remaining`, is not a usable window.
    fn usable(&self) -> bool {
        let placeholder = self.entitlement == 0.0
            && self.remaining == 0.0
            && self.percent_remaining == Some(0.0)
            && self.quota_id.is_empty();
        !placeholder && self.percent_remaining.is_some()
    }

    fn used_percent(&self) -> Option<f64> {
        let remaining = self.percent_remaining?;
        Some((100.0 - remaining).max(0.0))
    }
}

/// Build one quota snapshot from the `monthly_quotas`/`limited_user_quotas`
/// fallback (`monthly` = entitlement, `limited` = remaining). Faithful to
/// CodexBar's makeQuotaSnapshot: both required, entitlement must be > 0.
fn snapshot_from_counts(
    monthly: Option<f64>,
    limited: Option<f64>,
    quota_id: &str,
) -> Option<QuotaSnapshot> {
    let entitlement = monthly?.max(0.0);
    let remaining = limited?.max(0.0);
    if entitlement <= 0.0 {
        return None;
    }
    let percent_remaining = ((remaining / entitlement) * 100.0).clamp(0.0, 100.0);
    Some(QuotaSnapshot {
        entitlement,
        remaining,
        percent_remaining: Some(percent_remaining),
        quota_id: quota_id.to_string(),
    })
}

/// Normalize `quota_reset_date` for the consumer: emit a clean ISO 8601 `...Z`
/// when it parses as an RFC 3339 timestamp or a `YYYY-MM-DD` date; otherwise pass
/// the provider's raw string through unchanged (never fabricate).
fn normalize_reset(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return dt
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        if let Some(dt) = date.and_hms_opt(0, 0, 0) {
            return chrono::Utc
                .from_utc_datetime(&dt)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
        }
    }
    trimmed.to_string()
}

fn window(snapshot: &QuotaSnapshot, reset: Option<&str>) -> Option<RateWindow> {
    if !snapshot.usable() {
        return None;
    }
    let used_percent = snapshot.used_percent()?;
    // A usable quota with no reset date drops the window (never fabricated).
    let resets_at = normalize_reset(reset?);
    Some(RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: Some(MONTHLY_WINDOW_MINUTES),
    })
}

/// Normalize the `/copilot_internal/user` body to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: CopilotUsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("copilot usage not decodable: {e}")))?;

    // Prefer the direct quota_snapshots; fall back to monthly/limited counts.
    let (premium, chat) = match response.quota_snapshots {
        Some(snapshots) => (snapshots.premium_interactions, snapshots.chat),
        None => {
            let monthly = response.monthly_quotas;
            let limited = response.limited_user_quotas;
            let premium = snapshot_from_counts(
                monthly.as_ref().and_then(|m| m.completions),
                limited.as_ref().and_then(|l| l.completions),
                "completions",
            );
            let chat = snapshot_from_counts(
                monthly.as_ref().and_then(|m| m.chat),
                limited.as_ref().and_then(|l| l.chat),
                "chat",
            );
            (premium, chat)
        }
    };

    let reset = response.quota_reset_date.as_deref();
    let premium_window = premium.as_ref().and_then(|s| window(s, reset));
    let chat_window = chat.as_ref().and_then(|s| window(s, reset));

    // Premium → primary, chat → secondary; on a chat-only plan, chat stays in
    // secondary so the labels remain accurate (CodexBar parity).
    let (primary, secondary) = match (premium_window, chat_window) {
        (Some(p), c) => (Some(p), c),
        (None, Some(c)) => (None, Some(c)),
        (None, None) => (None, None),
    };

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The Copilot usage provider.
pub struct CopilotProvider {
    http: reqwest::Client,
}

impl CopilotProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for CopilotProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for CopilotProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let token = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;

            // GitHub's `token <oauth>` scheme (not Bearer) + the editor headers the
            // copilot_internal endpoint expects, reproduced from CodexBar.
            let body = JsonRequest::get(USAGE_URL)
                .header(Header::new("Authorization", format!("token {token}")))
                .header(Header::new("Editor-Version", "vscode/1.96.2"))
                .header(Header::new("Editor-Plugin-Version", "copilot-chat/0.26.7"))
                .header(Header::new("User-Agent", "GitHubCopilotChat/0.26.7"))
                .header(Header::new("X-Github-Api-Version", "2025-04-01"))
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
    fn normalizes_quota_snapshots_with_reset() {
        // Shaped like the real /copilot_internal/user response.
        let body = br#"{
            "copilot_plan": "individual",
            "quota_reset_date": "2026-07-01",
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 300, "remaining": 240,
                    "percent_remaining": 80.0, "quota_id": "premium_interactions"
                },
                "chat": {
                    "entitlement": 1000, "remaining": 500,
                    "percent_remaining": 50.0, "quota_id": "chat"
                }
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0); // 100 - 80
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-01T00:00:00Z")); // YYYY-MM-DD normalized
        assert_eq!(primary.window_minutes, Some(43200)); // monthly
        assert_eq!(usage.secondary.unwrap().used_percent, 50.0);
    }

    #[test]
    fn falls_back_to_monthly_limited_counts() {
        let body = br#"{
            "quota_reset_date": "2026-07-01T00:00:00Z",
            "monthly_quotas": { "completions": 300, "chat": 1000 },
            "limited_user_quotas": { "completions": 150, "chat": 1000 }
        }"#;
        let usage = normalize_usage(body).unwrap();
        // premium: 150/300 remaining = 50% remaining → 50% used.
        assert_eq!(usage.primary.unwrap().used_percent, 50.0);
        // chat: 1000/1000 = 100% remaining → 0% used.
        assert_eq!(usage.secondary.unwrap().used_percent, 0.0);
    }

    #[test]
    fn usable_quota_without_reset_drops_window() {
        let body = br#"{
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 300, "remaining": 240,
                    "percent_remaining": 80.0, "quota_id": "premium_interactions"
                }
            }
        }"#;
        // Real quota but no quota_reset_date → no window (never fabricated).
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn placeholder_snapshot_is_skipped() {
        let body = br#"{
            "quota_reset_date": "2026-07-01",
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 0, "remaining": 0, "percent_remaining": 0.0, "quota_id": ""
                },
                "chat": {
                    "entitlement": 1000, "remaining": 500, "percent_remaining": 50.0, "quota_id": "chat"
                }
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        // premium is a placeholder → primary empty; chat falls into secondary.
        assert!(usage.primary.is_none());
        assert_eq!(usage.secondary.unwrap().used_percent, 50.0);
    }
}
