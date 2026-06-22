//! ElevenLabs usage fetcher — credential from an environment variable.
//!
//! The simplest HTTP archetype: the credential comes from an env-var list
//! (`ELEVENLABS_API_KEY` / `XI_API_KEY`), attached as a provider-specific
//! `xi-api-key` header (NOT an `Authorization: Bearer` header) — which is why
//! `http::JsonRequest::header` accepts an arbitrary header, not just bearer.
//!
//! Endpoint: `GET {base}/v1/user/subscription` (base overridable via
//! `ELEVENLABS_API_URL`). Window: `character_count / character_limit` → a single
//! account-wide usage window resetting at `next_character_count_reset_unix`
//! (epoch seconds). ElevenLabs does not report a fixed window length, so
//! `windowMinutes` is omitted and the consumer paces on utilization alone.

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "elevenlabs";
const API_KEY_ENV: &[&str] = &["ELEVENLABS_API_KEY", "XI_API_KEY"];
const BASE_URL_ENV: &[&str] = &["ELEVENLABS_API_URL"];
const DEFAULT_BASE: &str = "https://api.elevenlabs.io";

/// The `/v1/user/subscription` response (the fields we normalize).
#[derive(Debug, Deserialize)]
struct SubscriptionResponse {
    character_count: Option<f64>,
    character_limit: Option<f64>,
    next_character_count_reset_unix: Option<i64>,
}

/// Build the subscription URL, honoring a `/v1`-suffixed base (don't double it).
fn subscription_url(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    let base = if trimmed.is_empty() { DEFAULT_BASE } else { trimmed };
    if base.ends_with("/v1") {
        format!("{base}/user/subscription")
    } else {
        format!("{base}/v1/user/subscription")
    }
}

/// Normalize the subscription body to [`Usage`]. Pure — unit-testable.
///
/// `character_count / character_limit * 100` → the primary window's
/// utilization; `next_character_count_reset_unix` → `resetsAt`. Emits nothing
/// when the limit is missing or zero (no meaningful utilization).
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: SubscriptionResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("elevenlabs subscription not decodable: {e}")))?;

    let primary = match (response.character_count, response.character_limit) {
        (Some(count), Some(limit)) if limit > 0.0 => {
            let used_percent = (count / limit * 100.0).clamp(0.0, 100.0);
            let resets_at = response
                .next_character_count_reset_unix
                .and_then(env::epoch_to_iso8601);
            resets_at.map(|resets_at| RateWindow {
                used_percent,
                resets_at,
                window_minutes: None,
            })
        }
        _ => None,
    };

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The ElevenLabs usage provider.
pub struct ElevenLabsProvider {
    http: reqwest::Client,
}

impl ElevenLabsProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for ElevenLabsProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for ElevenLabsProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let api_key = env::first_env(API_KEY_ENV).ok_or_else(|| {
            FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set"))
        })?;
        let base = env::first_env(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE.to_string());
        let url = subscription_url(&base);

        let body = JsonRequest::get(url)
            .header(Header::new("xi-api-key", api_key))
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
    fn normalizes_subscription_payload() {
        // Shaped like the real /v1/user/subscription response.
        let body = br#"{
            "tier": "creator",
            "character_count": 25000,
            "character_limit": 100000,
            "next_character_count_reset_unix": 1782135879
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at, "2026-06-22T13:44:39Z");
        assert_eq!(primary.window_minutes, None);
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn zero_limit_yields_no_window() {
        let body = br#"{ "character_count": 0, "character_limit": 0 }"#;
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn missing_reset_drops_window() {
        let body = br#"{ "character_count": 10, "character_limit": 100 }"#;
        // No reset timestamp → no well-formed window.
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn url_does_not_double_v1() {
        assert_eq!(
            subscription_url("https://api.elevenlabs.io"),
            "https://api.elevenlabs.io/v1/user/subscription"
        );
        assert_eq!(
            subscription_url("https://proxy.local/v1"),
            "https://proxy.local/v1/user/subscription"
        );
    }
}
