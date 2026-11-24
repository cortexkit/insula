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
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `ELEVENLABS_API_KEY` was available to fetch a real window. The endpoint, the
//! `xi-api-key` header, and the `character_count/character_limit/
//! next_character_count_reset_unix` response shape are ported from CodexBar's
//! working parser (`Providers/ElevenLabs/ElevenLabsUsageFetcher.swift:24-33,
//! 192-238`); the test payload mirrors that shape. The shared HTTP transport it
//! rides (`http.rs`) is itself live-proven via codex + anthropic.
//!
//! That Swift file exists up to CodexBar v0.63.0 and NO LONGER EXISTS from
//! v0.64.0: upstream moved this provider into a bundled plugin,
//! `Sources/CodexBarCore/Resources/Plugins/elevenlabs.ts`. Read the citation
//! above at v0.63.0 or earlier; the plugin is where to look for later changes.
//! (Recorded for `scripts/parity-citations.py`, which reports a cited file that
//! is gone at the current anchor until its module says what replaced it.)

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
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
    let base = if trimmed.is_empty() {
        DEFAULT_BASE
    } else {
        trimmed
    };
    if base.ends_with("/v1") {
        format!("{base}/user/subscription")
    } else {
        format!("{base}/v1/user/subscription")
    }
}

/// Normalize the subscription body to [`Usage`]. Pure — unit-testable.
///
/// `character_count / character_limit * 100` → the primary window's
/// utilization; `next_character_count_reset_unix` → `resetsAt` when present.
/// Emits nothing when the limit is missing or zero (no meaningful utilization);
/// a missing reset is carried as absent rather than discarding the window.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: SubscriptionResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("elevenlabs subscription not decodable: {e}")))?;

    let primary = match (response.character_count, response.character_limit) {
        (Some(count), Some(limit)) if limit > 0.0 => {
            let used_percent = (count / limit * 100.0).clamp(0.0, 100.0);
            // The percent is load-bearing and the reset is optional: a real
            // character limit is a real window even when the upstream omits the
            // next reset. Dropping it would make an exhausted allowance report as
            // no signal at all, which reads downstream as unused capacity.
            let resets_at = response
                .next_character_count_reset_unix
                .and_then(env::epoch_to_iso8601);
            Some(RateWindow {
                window_kind: None,
                used_percent,
                raw_used_percent: None,
                resets_at,
                window_minutes: None,
                used_count: None,
                total_count: None,
                regeneration: None,
                breakdown: None,
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
            http: crate::http::provider_client(),
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

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            let base = env::first_env(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE.to_string());
            let url = subscription_url(&base);

            let body = JsonRequest::get(url)
                .header(Header::new("xi-api-key", api_key))
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
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T13:44:39Z"));
        assert_eq!(primary.window_minutes, None);
        assert_eq!(primary.window_kind, None);
        assert_eq!(primary.raw_used_percent, None);
        assert_eq!((primary.used_count, primary.total_count), (None, None));
        assert!(primary.regeneration.is_none() && primary.breakdown.is_none());
        assert!(usage.secondary.is_none() && usage.tertiary.is_none());
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn missing_counts_and_nonpositive_limits_yield_no_window() {
        for body in [
            r#"{"character_count":0,"character_limit":0}"#,
            r#"{"character_count":3,"character_limit":-1}"#,
            r#"{"character_count":3}"#,
            r#"{"character_limit":10}"#,
        ] {
            assert!(
                normalize_usage(body.as_bytes()).unwrap().primary.is_none(),
                "{body}"
            );
        }
    }

    #[test]
    fn clamps_usage_and_drops_unrepresentable_reset_dates() {
        for (count, percent) in [(-1, 0.0), (0, 0.0), (11, 100.0)] {
            let body = format!(
                r#"{{"character_count":{count},"character_limit":10,"next_character_count_reset_unix":9223372036854775807}}"#
            );
            let primary = normalize_usage(body.as_bytes()).unwrap().primary.unwrap();
            assert_eq!(primary.used_percent, percent, "count {count}");
            assert_eq!(primary.resets_at, None);
        }
    }

    #[test]
    fn invalid_subscription_fields_are_decode_errors() {
        assert_eq!(
            normalize_usage(br#"{"character_count":"10","character_limit":20}"#)
                .unwrap_err()
                .error_class(),
            "decode_failed"
        );
    }

    #[test]
    fn exhausted_allowance_without_reset_still_reports_its_percent() {
        // The percent is load-bearing and the reset is optional. Pinned at 100%
        // because a vanishing exhausted window is the dangerous direction: a
        // consumer cannot distinguish "no window" from "plenty of room".
        let body = br#"{ "character_count": 100, "character_limit": 100 }"#;
        let primary = normalize_usage(body)
            .unwrap()
            .primary
            .expect("a real limit is a real window even with no reset reported");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(
            primary.resets_at, None,
            "an absent reset is carried as absent, never fabricated"
        );
    }

    #[test]
    fn url_does_not_double_v1() {
        assert_eq!(
            subscription_url("  ///  "),
            "https://api.elevenlabs.io/v1/user/subscription"
        );
        assert_eq!(
            subscription_url("  https://proxy.local/v1///  "),
            "https://proxy.local/v1/user/subscription"
        );
    }

    #[tokio::test]
    async fn subscription_fetch_uses_env_precedence_and_the_xi_header() {
        const CASE: &str = "INSULA_TEST_ELEVENLABS_CASE";
        if let Ok(case) = std::env::var(CASE) {
            let provider = ElevenLabsProvider::default();
            assert_eq!(provider.name(), "elevenlabs");
            let attempt = provider
                .fetch_handle(&CredentialHandle::ImplicitLocal)
                .await;
            if case == "missing" {
                assert_eq!(
                    attempt.usage.unwrap_err().error_class(),
                    "credential_absent"
                );
            } else if case == "default-base" {
                assert_eq!(attempt.usage.unwrap_err().error_class(), "upstream_failed");
            } else {
                assert_eq!(attempt.source.as_deref(), Some("api"));
                assert_eq!(
                    attempt.observed,
                    Some(crate::provider::AccountObservation::new(None, None))
                );
                assert_eq!(attempt.usage.unwrap().primary.unwrap().used_percent, 25.0);
            }
            return;
        }

        // An isolated test process selects credentials without racing other tests' env reads.
        for (case, main, alias, expected) in [
            ("missing", "", "  ", None),
            ("primary", " main-key ", "alias-key", Some("main-key")),
            ("alias", "   ", "alias-key", Some("alias-key")),
            ("default-base", "main-key", "", None),
        ] {
            let (base, request) = crate::loopback::serve_once(
                200,
                br#"{"character_count":1,"character_limit":4}"#.to_vec(),
            )
            .await;
            let (url, proxy) = if case == "default-base" {
                // End the HTTPS tunnel locally, so the default host is observed without dialing it.
                ("", base.as_str())
            } else {
                (base.as_str(), "http://127.0.0.1:0")
            };
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "elevenlabs::tests::subscription_fetch_uses_env_precedence_and_the_xi_header",
                ])
                .env(CASE, case)
                .env("ELEVENLABS_API_KEY", main)
                .env("XI_API_KEY", alias)
                .env("ELEVENLABS_API_URL", url)
                .env("NO_PROXY", "127.0.0.1")
                .env("HTTPS_PROXY", proxy)
                .env("HTTP_PROXY", proxy)
                .output()
                .await
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed;"),
                "{case}: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if let Some(key) = expected {
                let request = request.await.unwrap().to_ascii_lowercase();
                assert!(
                    request.starts_with("get /v1/user/subscription http/1.1\r\n"),
                    "{request}"
                );
                assert!(
                    request.contains(&format!("\r\nxi-api-key: {key}\r\n")),
                    "{request}"
                );
                assert!(!request.contains("\r\nauthorization:"), "{request}");
            } else if case == "default-base" {
                let request = tokio::time::timeout(std::time::Duration::from_secs(5), request)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    request.starts_with("CONNECT api.elevenlabs.io:443 HTTP/1.1\r\n"),
                    "{request}"
                );
            } else {
                request.abort();
            }
        }
    }
}
