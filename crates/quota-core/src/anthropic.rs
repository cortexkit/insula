//! Anthropic (claude) usage fetcher — the OAuth-bearer archetype, 2nd instance.
//!
//! This is the deliberate 2nd spike: it shares codex's "OAuth bearer → one GET →
//! decode JSON" skeleton but differs on every detail that could break the
//! abstraction, which is exactly why it validates it:
//!   - Session source: opencode's unified auth.json (`anthropic` OAuth entry),
//!     NOT a provider-native file. CodexBar reads the macOS Keychain; we prefer
//!     opencode's cross-platform store, which already holds the same token.
//!   - Endpoint: `GET https://api.anthropic.com/api/oauth/usage` with the beta
//!     header `anthropic-beta: oauth-2025-04-20` and a `claude-code/<ver>` UA.
//!   - Response: NAMED windows (`five_hour`, `seven_day`, `seven_day_sonnet`, ...)
//!     where `utilization` is ALREADY a 0-100 percent and `resets_at` is ALREADY
//!     ISO 8601 — unlike codex's int-percent + epoch. So normalization is a
//!     near-passthrough here, mapping window names to known window lengths.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    opencode_auth::{self, OpencodeAuth},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "claude";
const OPENCODE_PROVIDER: &str = "anthropic";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA_HEADER: &str = "oauth-2025-04-20";
const CLAUDE_CODE_UA: &str = "claude-code/2.1.0";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const FIVE_HOUR_MINUTES: i64 = 5 * 60;
const SEVEN_DAY_MINUTES: i64 = 7 * 24 * 60;

/// One named window in the response. `utilization` is already a 0-100 percent.
#[derive(Debug, Deserialize)]
struct OAuthWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// The `/api/oauth/usage` response (the windows we normalize).
#[derive(Debug, Deserialize)]
struct OAuthUsageResponse {
    five_hour: Option<OAuthWindow>,
    seven_day: Option<OAuthWindow>,
    seven_day_opus: Option<OAuthWindow>,
    seven_day_sonnet: Option<OAuthWindow>,
}

fn to_window(window: Option<&OAuthWindow>, window_minutes: i64) -> Option<RateWindow> {
    let window = window?;
    // CodexBar's makeWindow (ClaudeUsageFetcher.swift:945-956) builds a window
    // from `utilization` alone and leaves resetsAt nil when absent — an idle
    // session window reports `utilization: 0.0, resets_at: null` (nothing pending
    // to reset), and CodexBar shows it. So require only the percent; carry the
    // reset through when present, omit it otherwise. Never fabricate a reset.
    let used_percent = window.utilization?;
    Some(RateWindow {
        used_percent,
        resets_at: window.resets_at.clone(),
        window_minutes: Some(window_minutes),
    })
}

/// Normalize the `/api/oauth/usage` body to [`Usage`]. Pure — unit-testable.
///
/// Mapping: `five_hour` → primary (the session window), `seven_day` → secondary
/// (the weekly all-models window), and the model-scoped weekly (`seven_day_opus`
/// preferred, else `seven_day_sonnet`) → tertiary. Account-wide windows only;
/// per-model routing is a later concern (the consumer's extractor handles that).
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: OAuthUsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("anthropic usage not decodable: {e}")))?;
    Ok(Usage {
        primary: to_window(response.five_hour.as_ref(), FIVE_HOUR_MINUTES),
        secondary: to_window(response.seven_day.as_ref(), SEVEN_DAY_MINUTES),
        tertiary: to_window(
            response
                .seven_day_opus
                .as_ref()
                .or(response.seven_day_sonnet.as_ref()),
            SEVEN_DAY_MINUTES,
        ),
        extra_rate_windows: None,
    })
}

/// The anthropic usage provider.
pub struct AnthropicProvider {
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for AnthropicProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let auth = opencode_auth::read_provider(OPENCODE_PROVIDER)
            .map_err(FetchError::NoSession)?
            .ok_or_else(|| {
                FetchError::NoSession("no anthropic entry in opencode auth.json".to_string())
            })?;
        let access = match auth {
            OpencodeAuth::Oauth { access, .. } => access,
            OpencodeAuth::Api { key } => key,
        };

        let body = JsonRequest::get(USAGE_URL)
            .timeout(REQUEST_TIMEOUT)
            .bearer(&access)
            .header(Header::new("anthropic-beta", BETA_HEADER))
            .header(Header::new("User-Agent", CLAUDE_CODE_UA))
            .send(&self.http)
            .await?;

        let usage = normalize_usage(&body)?;
        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "oauth", usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_real_shaped_payload() {
        // Shaped exactly like the live HTTP 200 we captured: utilization is
        // already a percent, resets_at already ISO8601, named windows.
        let body = br#"{
            "five_hour": { "utilization": 16.0, "resets_at": "2026-06-22T17:00:00.175593+00:00" },
            "seven_day": { "utilization": 48.0, "resets_at": "2026-06-24T14:00:00.175619+00:00" },
            "seven_day_oauth_apps": null,
            "seven_day_opus": null,
            "seven_day_sonnet": { "utilization": 4.0, "resets_at": "2026-06-24T14:00:00.175629+00:00" }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 16.0); // already a percent, NOT /100
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(
            primary.resets_at.as_deref(),
            Some("2026-06-22T17:00:00.175593+00:00")
        );
        assert_eq!(usage.secondary.unwrap().used_percent, 48.0);
        // opus is null, so tertiary falls back to sonnet.
        assert_eq!(usage.tertiary.unwrap().used_percent, 4.0);
    }

    #[test]
    fn window_without_utilization_is_dropped() {
        let body = br#"{ "five_hour": { "resets_at": "2026-06-22T17:00:00Z" } }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
    }

    #[test]
    fn idle_zero_percent_window_with_null_reset_is_kept() {
        // The exact live shape Anthropic returns for an idle session: five_hour
        // utilization 0.0 with resets_at: null (nothing pending to reset). CodexBar
        // shows this 0% window; we keep it reset-less rather than dropping it, so
        // the headline session window does not vanish when simply empty.
        let body = br#"{
            "five_hour": { "utilization": 0.0, "resets_at": null },
            "seven_day": { "utilization": 91.0, "resets_at": "2026-06-24T14:00:00Z" }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.expect("idle 0% window kept");
        assert_eq!(primary.used_percent, 0.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.window_minutes, Some(300));
        // The active weekly window is unaffected.
        assert_eq!(usage.secondary.unwrap().used_percent, 91.0);
    }

    #[test]
    fn missing_windows_yield_empty_usage() {
        let usage = normalize_usage(br#"{}"#).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());
    }
}
