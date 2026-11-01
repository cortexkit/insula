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
    access_type_sku: Option<serde_json::Value>,
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
    #[serde(default)]
    unlimited: bool,
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
        unlimited: false,
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

fn window(
    snapshot: &QuotaSnapshot,
    reset: Option<&str>,
    named_monthly: bool,
) -> Option<RateWindow> {
    if snapshot.unlimited || !snapshot.usable() {
        return None;
    }
    let used_percent = snapshot.used_percent()?;
    // A usable quota with no reset date drops the window (never fabricated).
    let resets_at = normalize_reset(reset?);
    Some(RateWindow {
        window_kind: named_monthly
            .then(|| cortexkit_provider_usage::window_kind::MONTHLY.to_string()),
        used_percent,
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: Some(MONTHLY_WINDOW_MINUTES),
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    })
}

/// Normalize the `/copilot_internal/user` body to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: CopilotUsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("copilot usage not decodable: {e}")))?;

    // Ended subscriptions can still carry old quota figures; do not publish them.
    if response
        .access_type_sku
        .as_ref()
        .and_then(serde_json::Value::as_str)
        == Some("subscription_ended")
    {
        return Err(FetchError::NoQuotaReported(
            "Copilot subscription ended".to_string(),
        ));
    }

    // Missing quota fields describe an unknown response shape, not an account
    // known to have no quota. Keep that distinct from an ended subscription.
    if response.quota_snapshots.is_none()
        && response.monthly_quotas.is_none()
        && response.limited_user_quotas.is_none()
    {
        return Err(FetchError::Decode(
            "copilot usage has no quota fields".to_string(),
        ));
    }

    // Prefer the direct quota_snapshots; fall back to monthly/limited counts.
    let named_monthly = response.quota_snapshots.is_none() && response.monthly_quotas.is_some();
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
    let premium_window = premium
        .as_ref()
        .and_then(|s| window(s, reset, named_monthly));
    let chat_window = chat.as_ref().and_then(|s| window(s, reset, named_monthly));

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
            http: crate::http::provider_client(),
        }
    }

    async fn fetch_usage(&self, url: &str, token: &str) -> Result<Usage, FetchError> {
        // GitHub's `token <oauth>` scheme (not Bearer) + the editor headers the
        // copilot_internal endpoint expects, reproduced from CodexBar.
        let body = JsonRequest::get(url)
            .header(Header::new("Authorization", format!("token {token}")))
            .header(Header::new("Editor-Version", "vscode/1.96.2"))
            .header(Header::new("Editor-Plugin-Version", "copilot-chat/0.26.7"))
            .header(Header::new("User-Agent", "GitHubCopilotChat/0.26.7"))
            .header(Header::new("X-Github-Api-Version", "2025-04-01"))
            .send(&self.http)
            .await?;
        normalize_usage(&body)
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

            let usage = self.fetch_usage(USAGE_URL, &token).await?;
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
    fn ended_subscription_explains_why_quotas_are_unavailable() {
        // CodexBar v0.83.0, Tests/CodexBarTests/CopilotUsageFetcherTests.swift:
        // "ended subscription explains why quotas are unavailable".
        let body = br#"{"login":"fixture-user","access_type_sku":"subscription_ended","copilot_plan":"individual",
             "chat_enabled":false,"cli_enabled":false,"can_signup_for_limited":true}"#;
        let error = normalize_usage(body).unwrap_err();
        assert!(matches!(error, FetchError::NoQuotaReported(_)));
        assert_eq!(error.error_class(), "no_quota_reported");
        assert_eq!(
            error.to_string(),
            "no quota reported: Copilot subscription ended"
        );
    }

    #[test]
    fn unknown_quota_less_response_keeps_its_decoding_error() {
        // CodexBar v0.83.0, Tests/CodexBarTests/CopilotUsageFetcherTests.swift:
        // "unknown quota-less response keeps its decoding error".
        let body = br#"{"copilot_plan":"individual","access_type_sku":"future-entitlement"}"#;
        let error = normalize_usage(body).unwrap_err();
        assert!(matches!(error, FetchError::Decode(_)));
        assert_eq!(error.error_class(), "decode_failed");
        assert_eq!(
            error.to_string(),
            "decode error: copilot usage has no quota fields"
        );
    }

    #[tokio::test]
    async fn transport_or_decode_failure_is_not_mistaken_for_ended_subscription() {
        use tokio::io::AsyncWriteExt;

        // CodexBar v0.83.0, Tests/CodexBarTests/CopilotUsageFetcherTests.swift:
        // "transport decoding failure is not mistaken for an ended subscription".
        // Exercise both a failed body transfer and a complete but undecodable body.
        let body = r#"{"access_type_sku":"subscription_ended","quota_snapshots":false}"#;
        for truncated in [true, false] {
            let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                crate::loopback::read_request(&mut socket).await;
                let content_length = body.len() + usize::from(truncated);
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n{body}"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let provider = CopilotProvider::new();
            let error = provider
                .fetch_usage(&url, "fixture-token")
                .await
                .unwrap_err();
            if truncated {
                assert!(matches!(error, FetchError::Upstream(_)), "{error:?}");
                assert_eq!(error.error_class(), "upstream_failed");
            } else {
                assert!(matches!(error, FetchError::Decode(_)), "{error:?}");
                assert_eq!(error.error_class(), "decode_failed");
            }
            assert_ne!(
                error.to_string(),
                "no quota reported: Copilot subscription ended"
            );
            server.await.unwrap();
        }
    }

    #[test]
    fn quota_fields_without_windows_keep_existing_behavior() {
        for field in ["quota_snapshots", "monthly_quotas", "limited_user_quotas"] {
            let mut body = serde_json::json!({"access_type_sku": "future-entitlement"});
            body[field] = serde_json::json!({});
            let usage = normalize_usage(&serde_json::to_vec(&body).unwrap()).unwrap();
            assert!(usage.primary.is_none(), "field: {field}");
            assert!(usage.secondary.is_none(), "field: {field}");
        }
    }

    #[test]
    fn ended_subscription_does_not_publish_remaining_quota_figures() {
        let body = br#"{
            "access_type_sku": "subscription_ended",
            "quota_reset_date": "2026-07-01",
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 300, "remaining": 240,
                    "percent_remaining": 80.0, "quota_id": "premium_interactions"
                }
            }
        }"#;
        let error = normalize_usage(body).unwrap_err();
        assert!(matches!(error, FetchError::NoQuotaReported(_)));
        assert_eq!(
            error.to_string(),
            "no quota reported: Copilot subscription ended"
        );
    }

    #[test]
    fn other_access_type_skus_keep_quota_mapping() {
        for sku in [
            None,
            Some(serde_json::json!("future-entitlement")),
            Some(serde_json::json!("Subscription_ended")),
            Some(serde_json::json!("subscription_ended ")),
            Some(serde_json::json!("")),
            Some(serde_json::json!(null)),
            Some(serde_json::json!(42)),
            Some(serde_json::json!({})),
        ] {
            let mut body = serde_json::json!({
                "quota_reset_date": "2026-07-01",
                "quota_snapshots": {
                    "premium_interactions": {
                        "entitlement": 300, "remaining": 240,
                        "percent_remaining": 80.0, "quota_id": "premium_interactions"
                    }
                }
            });
            if let Some(sku) = &sku {
                body["access_type_sku"] = sku.clone();
            }
            let usage = normalize_usage(&serde_json::to_vec(&body).unwrap()).unwrap();
            assert_eq!(usage.primary.unwrap().used_percent, 20.0, "sku: {sku:?}");
            assert!(usage.secondary.is_none(), "sku: {sku:?}");
        }
    }

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
        assert_eq!(primary.window_kind, None);
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
        assert_eq!(
            usage.primary.as_ref().unwrap().window_kind.as_deref(),
            Some("monthly")
        );
        assert_eq!(
            usage.secondary.as_ref().unwrap().window_kind.as_deref(),
            Some("monthly")
        );
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

    #[test]
    fn unlimited_snapshots_do_not_become_metered_windows() {
        let body = br#"{
            "quota_reset_date": "2026-07-01",
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 300, "remaining": 300, "percent_remaining": 100.0,
                    "quota_id": "premium_interactions", "unlimited": true
                },
                "chat": {
                    "entitlement": 1000, "remaining": 1000, "percent_remaining": 100.0,
                    "quota_id": "chat", "unlimited": true
                }
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn unlimited_premium_keeps_metered_chat_window() {
        let body = br#"{
            "quota_reset_date": "2026-07-01",
            "quota_snapshots": {
                "premium_interactions": {
                    "entitlement": 300, "remaining": 300, "percent_remaining": 100.0,
                    "quota_id": "premium_interactions", "unlimited": true
                },
                "chat": {
                    "entitlement": 1000, "remaining": 500, "percent_remaining": 50.0,
                    "quota_id": "chat"
                }
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
        assert_eq!(usage.secondary.unwrap().used_percent, 50.0);
    }
}
