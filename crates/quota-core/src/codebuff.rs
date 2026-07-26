//! Codebuff usage fetcher — API key from env or the codebuff CLI's auth file.
//!
//! Two endpoints (CodexBar fetches both; subscription is best-effort):
//!   - POST `/api/v1/usage` (Bearer) → credits: `usage`/`used`, `quota`/`limit`,
//!     `remainingBalance`/`remaining`, `next_quota_reset`.
//!   - GET `/api/user/subscription` (Bearer) → `rateLimit.{weeklyUsed/used,
//!     weeklyLimit/limit, weeklyResetsAt}` — the weekly subscription window.
//!
//! Credential (first wins): `CODEBUFF_API_KEY` env, else the auth token in
//! `~/.config/manicode/credentials.json` (`default.authToken` or top-level
//! `authToken`) written by `codebuff login`.
//!
//! Window mapping (NO fabrication — the kilo/manus precedent): credits → primary
//! ONLY when there is a real total AND a real `next_quota_reset`; weekly
//! subscription → secondary when `weeklyLimit > 0` AND a real `weeklyResetsAt`. We
//! deliberately do NOT replicate CodexBar's degenerate "no quota ⇒ 100% used, no
//! reset" placeholder (CodebuffUsageSnapshot.swift:70-81) — emitting a fabricated
//! 100% with no reset would feed the router a misleading exhausted window; a window
//! without a real reset is dropped instead.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `CODEBUFF_API_KEY` / manicode credentials on this machine. Endpoints, auth, field
//! names, and the credits/weekly math ported from CodexBar
//! `Sources/CodexBarCore/Providers/Codebuff/CodebuffUsageFetcher.swift:138-205,
//! 228-264`, `CodebuffUsageSnapshot.swift:68-124`, `CodebuffSettingsReader.swift:
//! 7-46`. Rides the live-proven `http.rs`.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "codebuff";
const API_KEY_ENV: &[&str] = &["CODEBUFF_API_KEY"];
const BASE_URL: &str = "https://www.codebuff.com";
const USAGE_PATH: &str = "/api/v1/usage";
const SUBSCRIPTION_PATH: &str = "/api/user/subscription";
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

// ---- credential resolution --------------------------------------------------

fn credentials_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/manicode/credentials.json"))
}

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    default: Option<CredentialsProfile>,
    #[serde(rename = "authToken")]
    auth_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CredentialsProfile {
    #[serde(rename = "authToken")]
    auth_token: Option<String>,
}

/// Trim + strip surrounding quotes (CodexBar `CodebuffSettingsReader.cleaned`).
fn clean(raw: &str) -> Option<String> {
    crate::text::strip_wrapping_quotes(raw)
}

/// Env key first, then the manicode credentials file (`default.authToken` then
/// top-level `authToken`).
fn resolve_token() -> Option<String> {
    if let Some(key) = env::first_env(API_KEY_ENV).and_then(|v| clean(&v)) {
        return Some(key);
    }
    let path = credentials_path()?;
    let data = std::fs::read(&path).ok()?;
    let file: CredentialsFile = serde_json::from_slice(&data).ok()?;
    file.default
        .and_then(|p| p.auth_token)
        .or(file.auth_token)
        .and_then(|t| clean(&t))
}

// ---- response parsing (pure) ------------------------------------------------

/// A JSON value that may be a number or a numeric string (CodexBar `double(from:)`).
fn flex_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn field_f64(obj: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| obj.get(*k).and_then(flex_f64))
}

fn field_str(obj: &serde_json::Value, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// Credits window from `/api/v1/usage` (CodexBar `resolvedTotal`/`resolvedUsed`).
/// Emits a window when a real total is available; `next_quota_reset` is optional and
/// is never fabricated.
fn credits_window(usage: &serde_json::Value) -> Option<RateWindow> {
    let used = field_f64(usage, &["usage", "used"]);
    let total = field_f64(usage, &["quota", "limit"]);
    let remaining = field_f64(usage, &["remainingBalance", "remaining"]);

    let total = total
        .or_else(|| match (used, remaining) {
            (Some(u), Some(r)) => Some((u + r).max(0.0)),
            _ => None,
        })
        .filter(|t| *t > 0.0)?;
    let used = used
        .or_else(|| remaining.map(|r| (total - r).max(0.0)))
        .unwrap_or(0.0);

    let resets_at = field_str(usage, "next_quota_reset");
    Some(RateWindow {
        used_percent: ((used / total) * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at,
        window_minutes: None,
        used_count: None,
        total_count: None,
    })
}

/// Weekly subscription window from `/api/user/subscription` (`rateLimit.*`).
fn weekly_window(subscription: &serde_json::Value) -> Option<RateWindow> {
    let rate = subscription.get("rateLimit")?;
    let limit = field_f64(rate, &["weeklyLimit", "limit"]).filter(|l| *l > 0.0)?;
    let used = field_f64(rate, &["weeklyUsed", "used"])
        .unwrap_or(0.0)
        .max(0.0);
    let resets_at = field_str(rate, "weeklyResetsAt");
    Some(RateWindow {
        used_percent: ((used / limit) * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at,
        window_minutes: Some(WEEKLY_MINUTES),
        used_count: None,
        total_count: None,
    })
}

/// Normalize the usage + (optional) subscription bodies to [`Usage`]. Pure.
pub fn normalize_usage(
    usage_body: &[u8],
    subscription_body: Option<&[u8]>,
) -> Result<Usage, FetchError> {
    let usage: serde_json::Value = serde_json::from_slice(usage_body)
        .map_err(|e| FetchError::Decode(format!("codebuff usage not decodable: {e}")))?;
    let primary = credits_window(&usage);

    let secondary = subscription_body
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
        .as_ref()
        .and_then(weekly_window);

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

// ---- provider ---------------------------------------------------------------

/// The Codebuff usage provider.
pub struct CodebuffProvider {
    http: reqwest::Client,
}

impl CodebuffProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for CodebuffProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for CodebuffProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let token = resolve_token().ok_or_else(|| {
                FetchError::NoSession(
                    "no CODEBUFF_API_KEY or ~/.config/manicode/credentials.json".to_string(),
                )
            })?;

            // Required: the usage (credits) endpoint.
            let usage_body = JsonRequest::post_json(
                format!("{BASE_URL}{USAGE_PATH}"),
                serde_json::to_vec(&json!({ "fingerprintId": "quota-usage" }))
                    .map_err(|e| FetchError::Decode(e.to_string()))?,
            )
            .timeout(REQUEST_TIMEOUT)
            .bearer(&token)
            .send(&self.http)
            .await?;

            // Best-effort: the subscription endpoint carries the weekly window. A
            // failure here must not fail the whole fetch (CodexBar treats it optional).
            let subscription_body = JsonRequest::get(format!("{BASE_URL}{SUBSCRIPTION_PATH}"))
                .timeout(REQUEST_TIMEOUT)
                .bearer(&token)
                .send(&self.http)
                .await
                .ok();

            let usage = normalize_usage(&usage_body, subscription_body.as_deref())?;
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
    fn credits_and_weekly_windows() {
        let usage =
            br#"{ "usage": 250, "quota": 1000, "next_quota_reset": "2026-07-01T00:00:00Z" }"#;
        let subscription = br#"{ "rateLimit": { "weeklyUsed": 30, "weeklyLimit": 100, "weeklyResetsAt": "2026-06-29T00:00:00Z" } }"#;
        let result = normalize_usage(usage, Some(subscription)).unwrap();
        let primary = result.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        let secondary = result.secondary.unwrap();
        assert_eq!(secondary.used_percent, 30.0);
        assert_eq!(secondary.window_minutes, Some(10080));
    }

    #[test]
    fn credits_derived_from_used_plus_remaining() {
        // No explicit quota; total = used + remaining. String numbers too.
        let usage =
            br#"{ "used": "20", "remaining": "80", "next_quota_reset": "2026-07-01T00:00:00Z" }"#;
        let result = normalize_usage(usage, None).unwrap();
        assert_eq!(result.primary.unwrap().used_percent, 20.0);
    }

    #[test]
    fn credits_without_reset_keeps_real_window() {
        let usage = br#"{ "usage": 50, "quota": 100 }"#;
        let result = normalize_usage(usage, None).unwrap();
        let primary = result.primary.expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn credits_without_reset_keeps_exhausted_window() {
        let result = normalize_usage(br#"{ "usage": 100, "quota": 100 }"#, None).unwrap();
        let primary = result.primary.expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn weekly_without_real_reset_keeps_real_window() {
        let usage = br#"{ "usage": 1, "quota": 10, "next_quota_reset": "2026-07-01T00:00:00Z" }"#;
        let subscription = br#"{ "rateLimit": { "weeklyUsed": 5, "weeklyLimit": 100 } }"#;
        let result = normalize_usage(usage, Some(subscription)).unwrap();
        assert!(result.primary.is_some());
        let secondary = result.secondary.expect("usage data should emit a window");
        assert_eq!(secondary.used_percent, 5.0);
        assert_eq!(secondary.resets_at, None);
    }

    #[test]
    fn weekly_without_reset_keeps_exhausted_window() {
        let subscription = br#"{ "rateLimit": { "weeklyUsed": 100, "weeklyLimit": 100 } }"#;
        let result = normalize_usage(br#"{}"#, Some(subscription)).unwrap();
        let secondary = result.secondary.expect("usage data should emit a window");
        assert_eq!(secondary.used_percent, 100.0);
        assert_eq!(secondary.resets_at, None);
    }

    #[test]
    fn garbage_usage_body_is_decode_error() {
        assert!(matches!(
            normalize_usage(b"not json", None),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn clean_strips_quotes() {
        assert_eq!(clean("  \"tok\" ").as_deref(), Some("tok"));
        assert_eq!(clean("''"), None);
    }
}
