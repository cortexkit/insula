//! Codex (openai) usage fetcher — the "oauth-bearer-from-local-file" archetype.
//!
//! Auth: read `~/.codex/auth.json`. CodexBar's credential parser prefers a
//! top-level `OPENAI_API_KEY` over the OAuth token, but for USAGE we deliberately
//! prefer the OAuth token: the usage endpoint is account-scoped via the
//! `ChatGPT-Account-Id` header, which an API key alone does not carry, and the
//! OAuth `tokens.access_token` is the credential CodexBar's own usage path uses.
//! When only an API key is present we fall back to it (no account header).
//!
//! Fetch: `GET {base}/wham/usage` with `Authorization: Bearer <token>`,
//! `ChatGPT-Account-Id: <account_id>`. `base` defaults to
//! `https://chatgpt.com/backend-api` and is overridable via `chatgpt_base_url`
//! in `~/.codex/config.toml` (CODEX_HOME-relative), matching CodexBar.
//!
//! Normalize: `rate_limit.{primary,secondary}_window` →
//! `usage.{primary,secondary}` RateWindows. Each upstream window is
//! `{ used_percent, reset_at (epoch s), limit_window_seconds }`; we map
//! `reset_at` → ISO 8601 `resetsAt` and `limit_window_seconds / 60` →
//! `windowMinutes`.

use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "codex";
const DEFAULT_BASE: &str = "https://chatgpt.com/backend-api";
const USAGE_PATH: &str = "/wham/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolved credentials for the usage call.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexCredentials {
    pub bearer: String,
    pub account_id: Option<String>,
    /// True when `bearer` is the OAuth access token (account-scoped), false when
    /// it is a fallback API key.
    pub is_oauth: bool,
}

/// The subset of `auth.json` we read.
#[derive(Debug, Deserialize)]
struct AuthFile {
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    tokens: Option<AuthTokens>,
}

#[derive(Debug, Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

/// Parse credentials from raw `auth.json` bytes, preferring the OAuth token.
pub fn parse_credentials(data: &[u8]) -> Result<CodexCredentials, FetchError> {
    let auth: AuthFile = serde_json::from_slice(data)
        .map_err(|e| FetchError::Decode(format!("auth.json is not valid JSON: {e}")))?;

    // Prefer the OAuth access token (account-scoped usage). Only when it is
    // absent do we fall back to a bare API key.
    if let Some(tokens) = &auth.tokens {
        if let Some(access) = tokens.access_token.as_deref().filter(|t| !t.is_empty()) {
            return Ok(CodexCredentials {
                bearer: access.to_string(),
                account_id: tokens.account_id.clone().filter(|a| !a.is_empty()),
                is_oauth: true,
            });
        }
    }

    if let Some(key) = auth.openai_api_key.as_deref().filter(|k| !k.is_empty()) {
        return Ok(CodexCredentials {
            bearer: key.to_string(),
            account_id: None,
            is_oauth: false,
        });
    }

    Err(FetchError::NoSession(
        "auth.json has neither tokens.access_token nor OPENAI_API_KEY".to_string(),
    ))
}

/// Upstream `/wham/usage` response (the subset we normalize).
#[derive(Debug, Deserialize)]
struct UsageResponse {
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    primary_window: Option<WindowSnapshot>,
    secondary_window: Option<WindowSnapshot>,
}

#[derive(Debug, Deserialize)]
struct WindowSnapshot {
    used_percent: Option<f64>,
    reset_at: Option<i64>,
    limit_window_seconds: Option<i64>,
}

/// Normalize one upstream window snapshot to a [`RateWindow`].
///
/// Returns `None` when the window lacks the load-bearing fields (`used_percent`
/// + `reset_at`), so the consumer never sees a half-formed window.
fn normalize_window(snapshot: &WindowSnapshot) -> Option<RateWindow> {
    let used_percent = snapshot.used_percent?;
    let reset_at = snapshot.reset_at?;
    let resets_at = crate::env::epoch_to_iso8601(reset_at)?;
    let window_minutes = snapshot
        .limit_window_seconds
        .filter(|s| *s > 0)
        .map(|s| s / 60);
    Some(RateWindow {
        used_percent,
        resets_at,
        window_minutes,
    })
}

/// Normalize a full `/wham/usage` JSON body to [`Usage`]. Pure — no I/O — so it
/// is unit-testable against recorded real payloads.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: UsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("usage response not decodable: {e}")))?;
    let rate_limit = response
        .rate_limit
        .ok_or_else(|| FetchError::Decode("usage response missing rate_limit".to_string()))?;
    Ok(Usage {
        primary: rate_limit.primary_window.as_ref().and_then(normalize_window),
        secondary: rate_limit
            .secondary_window
            .as_ref()
            .and_then(normalize_window),
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// Resolve the codex home directory (`CODEX_HOME` or `~/.codex`).
fn codex_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(home));
    }
    dirs_home().map(|h| h.join(".codex"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Resolve the usage URL, honoring a `chatgpt_base_url` override in config.toml.
fn resolve_usage_url(config_toml: Option<&str>) -> String {
    let base = config_toml
        .and_then(parse_chatgpt_base_url)
        .unwrap_or_else(|| DEFAULT_BASE.to_string());
    let mut normalized = base.trim().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        normalized = DEFAULT_BASE.to_string();
    }
    // chatgpt.com / chat.openai.com hosts carry the /backend-api prefix.
    if (normalized.starts_with("https://chatgpt.com")
        || normalized.starts_with("https://chat.openai.com"))
        && !normalized.contains("/backend-api")
    {
        normalized.push_str("/backend-api");
    }
    format!("{normalized}{USAGE_PATH}")
}

/// Extract `chatgpt_base_url = "..."` from a config.toml body (comment-aware).
fn parse_chatgpt_base_url(contents: &str) -> Option<String> {
    for raw_line in contents.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "chatgpt_base_url" {
            continue;
        }
        let value = value.trim().trim_matches(['"', '\'']).trim();
        if value.is_empty() {
            return None;
        }
        return Some(value.to_string());
    }
    None
}

/// The codex usage provider.
pub struct CodexProvider {
    http: reqwest::Client,
}

impl CodexProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for CodexProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let home = codex_home().ok_or_else(|| {
            FetchError::NoSession("cannot resolve CODEX_HOME or $HOME/.codex".to_string())
        })?;
        let auth_path = home.join("auth.json");
        let data = std::fs::read(&auth_path).map_err(|e| {
            FetchError::NoSession(format!("reading {}: {e}", auth_path.display()))
        })?;
        let creds = parse_credentials(&data)?;

        let config_toml = std::fs::read_to_string(home.join("config.toml")).ok();
        let url = resolve_usage_url(config_toml.as_deref());

        let mut request = JsonRequest::get(url)
            .timeout(REQUEST_TIMEOUT)
            .bearer(&creds.bearer)
            .header(Header::new("User-Agent", "ai-provider-quota"));
        if let Some(account_id) = &creds.account_id {
            request = request.header(Header::new("ChatGPT-Account-Id", account_id.clone()));
        }

        let body = request.send(&self.http).await?;
        let usage = normalize_usage(&body)?;
        let source = if creds.is_oauth { "oauth" } else { "api" };
        Ok(ProviderUsage::healthy(
            PROVIDER_NAME,
            creds.account_id,
            source,
            usage,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_oauth_token_over_api_key() {
        let raw = br#"{
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": "sk-should-not-win",
            "tokens": { "access_token": "oauth-wins", "account_id": "acct-1" }
        }"#;
        let creds = parse_credentials(raw).unwrap();
        assert_eq!(creds.bearer, "oauth-wins");
        assert_eq!(creds.account_id.as_deref(), Some("acct-1"));
        assert!(creds.is_oauth);
    }

    #[test]
    fn falls_back_to_api_key_when_no_oauth_token() {
        let raw = br#"{ "OPENAI_API_KEY": "sk-fallback" }"#;
        let creds = parse_credentials(raw).unwrap();
        assert_eq!(creds.bearer, "sk-fallback");
        assert_eq!(creds.account_id, None);
        assert!(!creds.is_oauth);
    }

    #[test]
    fn no_session_when_empty() {
        let raw = br#"{ "auth_mode": "chatgpt", "tokens": {} }"#;
        assert!(matches!(
            parse_credentials(raw),
            Err(FetchError::NoSession(_))
        ));
    }

    #[test]
    fn normalizes_real_shaped_payload() {
        // Shaped exactly like the live HTTP 200 we captured.
        let body = br#"{
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": { "used_percent": 41, "limit_window_seconds": 18000, "reset_at": 1782135879 },
                "secondary_window": { "used_percent": 28, "limit_window_seconds": 604800, "reset_at": 1782667719 }
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 41.0);
        assert_eq!(primary.window_minutes, Some(300)); // 18000s / 60 = 300m (5h)
        assert_eq!(primary.resets_at, "2026-06-22T13:44:39Z");
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.window_minutes, Some(10080)); // weekly
        assert!(usage.tertiary.is_none());
    }

    #[test]
    fn window_without_required_fields_is_dropped() {
        let body = br#"{ "rate_limit": { "primary_window": { "used_percent": 50 } } }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
    }

    #[test]
    fn resolve_url_default_and_override() {
        assert_eq!(
            resolve_usage_url(None),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            resolve_usage_url(Some("chatgpt_base_url = \"https://proxy.local/backend-api\"\n")),
            "https://proxy.local/backend-api/wham/usage"
        );
    }
}
