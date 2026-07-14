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

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::codex_resets::{
    normalize_credits, response_now, CreditsHttpResponse, CreditsSnapshot, ReqwestResetTransport,
    ResetCoordinator, ResetRequest, ResetTickInput, ResetTransport, UsageFacts,
};
use crate::config::CodexConfig;
use crate::provider::AccountObservation;
use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    http::{Header, JsonRequest},
    model::{RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "codex";
const DEFAULT_BASE: &str = "https://chatgpt.com/backend-api";
const USAGE_PATH: &str = "/wham/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ARMED_USAGE_TIMEOUT: Duration = Duration::from_secs(12);

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
                account_id: tokens
                    .account_id
                    .clone()
                    .filter(|account_id| !account_id.is_empty()),
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
    #[serde(default)]
    limit_reached: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct WindowSnapshot {
    used_percent: Option<f64>,
    reset_at: Option<i64>,
    limit_window_seconds: Option<i64>,
}

/// Normalize one upstream window snapshot to a [`RateWindow`].
///
/// Requires only `used_percent`; `reset_at` is carried through when present and
/// omitted otherwise (an idle window can report a percent with no pending reset,
/// which CodexBar shows rather than dropping). The reset is never fabricated.
fn normalize_window(snapshot: &WindowSnapshot) -> Option<RateWindow> {
    let used_percent = snapshot.used_percent?;
    let resets_at = snapshot.reset_at.and_then(crate::env::epoch_to_iso8601);
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

/// Raw usage plus the server's explicit wall indicator.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexUsageSnapshot {
    pub usage: Usage,
    pub limit_reached: Option<bool>,
}

/// Normalize a full `/wham/usage` JSON body without relaxing any percentages.
pub fn normalize_usage_snapshot(body: &[u8]) -> Result<CodexUsageSnapshot, FetchError> {
    let response: UsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("usage response not decodable: {e}")))?;
    let rate_limit = response
        .rate_limit
        .ok_or_else(|| FetchError::Decode("usage response missing rate_limit".to_string()))?;
    Ok(CodexUsageSnapshot {
        usage: Usage {
            primary: rate_limit
                .primary_window
                .as_ref()
                .and_then(normalize_window),
            secondary: rate_limit
                .secondary_window
                .as_ref()
                .and_then(normalize_window),
            tertiary: None,
            extra_rate_windows: None,
        },
        limit_reached: rate_limit.limit_reached,
    })
}

/// Compatibility normalizer used by tests and read-only consumers.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    normalize_usage_snapshot(body).map(|snapshot| snapshot.usage)
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

/// Resolve the API base, honoring a `chatgpt_base_url` override in config.toml.
fn resolve_base_url(config_toml: Option<&str>) -> String {
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
    normalized
}

fn resolve_usage_url(config_toml: Option<&str>) -> String {
    format!("{}{USAGE_PATH}", resolve_base_url(config_toml))
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

fn usage_request(url: String, credentials: &CodexCredentials, timeout: Duration) -> JsonRequest {
    let mut request = JsonRequest::get(url)
        .timeout(timeout)
        .bearer(&credentials.bearer)
        .header(Header::new("User-Agent", "ai-provider-quota"));
    if let Some(account_id) = &credentials.account_id {
        request = request.header(Header::new("ChatGPT-Account-Id", account_id.clone()));
    }
    request
}

pub(crate) fn normalize_credits_tick(
    response: Result<CreditsHttpResponse, FetchError>,
    local_now: DateTime<Utc>,
) -> Result<(CreditsSnapshot, DateTime<Utc>), FetchError> {
    response.and_then(|response| {
        let now = response_now(response.date_header.as_deref(), local_now);
        normalize_credits(&response.body).map(|credits| (credits, now))
    })
}

pub(crate) fn unarmed_usage_attempt(
    observed: Option<AccountObservation>,
    source: &str,
    snapshot: CodexUsageSnapshot,
) -> FetchAttempt {
    FetchAttempt::success(observed, source, snapshot.usage)
}

pub(crate) fn reset_trigger_expiry(
    credits: &CreditsSnapshot,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    credits.earliest_usable_expiry(now)
}

fn reset_credentials_eligible(config: &CodexConfig, credentials: &CodexCredentials) -> bool {
    config.is_enabled()
        && credentials.is_oauth
        && credentials
            .account_id
            .as_deref()
            .is_some_and(|account_id| !account_id.trim().is_empty())
}

fn log_reset_tick(
    facts: Option<&UsageFacts>,
    credits: Option<&CreditsSnapshot>,
    earliest_expiry: Option<chrono::DateTime<Utc>>,
    armed: bool,
    relax_eligible: bool,
) {
    let raw_percents = facts
        .map(|facts| format!("{:?}", facts.raw_percents))
        .unwrap_or_else(|| "unavailable".to_string());
    let credit_count = credits
        .map(|credits| credits.available.len().to_string())
        .unwrap_or_else(|| "unavailable".to_string());
    let earliest_expiry = earliest_expiry
        .map(|expiry| expiry.to_rfc3339())
        .unwrap_or_else(|| "none".to_string());
    eprintln!(
        "[ck-quota] codex reset tick raw_percents={raw_percents} credit_count={credit_count} earliest_expiry={earliest_expiry} armed={armed} relax_eligible={relax_eligible}"
    );
}

/// The codex usage provider.
pub struct CodexProvider {
    http: reqwest::Client,
    reset_config: CodexConfig,
    reset_transport: Arc<dyn ResetTransport>,
    reset_coordinator: Result<Arc<ResetCoordinator>, String>,
}

impl CodexProvider {
    pub fn new(reset_config: CodexConfig) -> Self {
        let http = reqwest::Client::new();
        let reset_coordinator = if reset_config.is_enabled() {
            ResetCoordinator::from_env()
                .map(Arc::new)
                .map_err(|error| error.to_string())
        } else {
            Err("feature disabled".to_string())
        };
        Self {
            reset_transport: Arc::new(ReqwestResetTransport::new(http.clone())),
            http,
            reset_config,
            reset_coordinator,
        }
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new(CodexConfig::default())
    }
}

#[async_trait]
impl UsageProvider for CodexProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let attempt_started = Instant::now();
        let resolved = tokio::task::spawn_blocking(|| {
            let home = codex_home().ok_or_else(|| {
                FetchError::NoSession("cannot resolve CODEX_HOME or $HOME/.codex".to_string())
            })?;
            let auth_path = home.join("auth.json");
            let data = std::fs::read(&auth_path).map_err(|e| {
                FetchError::NoSession(format!("reading {}: {e}", auth_path.display()))
            })?;
            let creds = parse_credentials(&data)?;
            let config_toml = std::fs::read_to_string(home.join("config.toml")).ok();
            Ok::<_, FetchError>((creds, config_toml))
        })
        .await;

        let (creds, config_toml) = match resolved {
            Ok(Ok(resolved)) => resolved,
            Ok(Err(error)) => {
                if self.reset_config.is_enabled() {
                    log_reset_tick(None, None, None, false, false);
                }
                return FetchAttempt::failure(None, None, error);
            }
            Err(_) => {
                if self.reset_config.is_enabled() {
                    log_reset_tick(None, None, None, false, false);
                }
                return FetchAttempt::failure(
                    None,
                    None,
                    FetchError::Decode("codex credential resolution task panicked".to_string()),
                );
            }
        };
        let observed = Some(AccountObservation::new(creds.account_id.clone(), None));
        let source = if creds.is_oauth { "oauth" } else { "api" };
        let usage_url = resolve_usage_url(config_toml.as_deref());

        // Feature-off and non-account-scoped credentials retain the exact legacy
        // single GET and 30-second timeout. Mutation cannot be keyed without an
        // OAuth account id, so API-key fallback never fetches reset endpoints.
        let Some(account_id) = creds
            .account_id
            .as_deref()
            .filter(|_| reset_credentials_eligible(&self.reset_config, &creds))
        else {
            let usage = usage_request(usage_url, &creds, REQUEST_TIMEOUT)
                .send(&self.http)
                .await
                .and_then(|body| normalize_usage(&body));
            if self.reset_config.is_enabled() {
                let facts = usage
                    .as_ref()
                    .ok()
                    .map(|usage| UsageFacts::from_usage(usage, None));
                log_reset_tick(facts.as_ref(), None, None, false, false);
            }
            return match usage {
                Ok(usage) => FetchAttempt::success(observed, source, usage),
                Err(error) => FetchAttempt::failure(observed, Some(source.to_string()), error),
            };
        };

        let base_url = resolve_base_url(config_toml.as_deref());
        let reset_request = ResetRequest {
            base_url,
            bearer: creds.bearer.clone(),
            account_id: account_id.to_string(),
        };
        let usage_future = usage_request(usage_url, &creds, ARMED_USAGE_TIMEOUT).send(&self.http);
        let credits_future = self.reset_transport.fetch_credits(&reset_request);
        let (usage_http, credits_http) = tokio::join!(usage_future, credits_future);

        let usage_snapshot = usage_http.and_then(|body| normalize_usage_snapshot(&body));
        let local_now = Utc::now();
        let credits_snapshot = normalize_credits_tick(credits_http, local_now);

        let usage_snapshot = match usage_snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let credits = credits_snapshot.as_ref().ok().map(|(credits, _)| credits);
                let earliest_expiry = credits.and_then(CreditsSnapshot::earliest_available_expiry);
                log_reset_tick(None, credits, earliest_expiry, false, false);
                return FetchAttempt::failure(observed, Some(source.to_string()), error);
            }
        };
        let facts = UsageFacts::from_usage(&usage_snapshot.usage, usage_snapshot.limit_reached);
        let (credits, now) = match credits_snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!(
                    "[ck-quota] warning: codex credits GET failed account_id={account_id}: {error}; tick unarmed"
                );
                log_reset_tick(Some(&facts), None, None, false, false);
                return unarmed_usage_attempt(observed, source, usage_snapshot);
            }
        };
        let Some(earliest_expiry) = reset_trigger_expiry(&credits, now) else {
            log_reset_tick(Some(&facts), Some(&credits), None, false, false);
            return FetchAttempt::success(observed, source, usage_snapshot.usage);
        };

        let coordinator = match &self.reset_coordinator {
            Ok(coordinator) => coordinator,
            Err(error) => {
                eprintln!(
                    "[ck-quota] warning: codex reset journal path unavailable: {error}; tick disarmed"
                );
                log_reset_tick(
                    Some(&facts),
                    Some(&credits),
                    Some(earliest_expiry),
                    false,
                    false,
                );
                return FetchAttempt::success(observed, source, usage_snapshot.usage);
            }
        };
        let result = coordinator
            .process_tick(
                account_id,
                ResetTickInput {
                    armed: true,
                    now,
                    earliest_expiry: Some(earliest_expiry),
                    auto_use_resets_secs: self.reset_config.auto_use_resets,
                    facts: facts.clone(),
                    elapsed_since_attempt_start: attempt_started.elapsed(),
                },
                self.reset_transport.as_ref(),
                &reset_request,
            )
            .await;
        log_reset_tick(
            Some(&facts),
            Some(&credits),
            Some(earliest_expiry),
            result.armed,
            result.relax_eligible,
        );
        FetchAttempt::success(observed, source, usage_snapshot.usage)
            .with_relax_eligible(result.relax_eligible)
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
    fn banked_resets_require_enabled_oauth_with_account_id() {
        let enabled = CodexConfig {
            auto_use_resets: 60,
        };
        let oauth = CodexCredentials {
            bearer: "oauth".to_string(),
            account_id: Some("acct".to_string()),
            is_oauth: true,
        };
        assert!(reset_credentials_eligible(&enabled, &oauth));

        let mut api_key = oauth.clone();
        api_key.is_oauth = false;
        assert!(!reset_credentials_eligible(&enabled, &api_key));

        let mut no_account = oauth.clone();
        no_account.account_id = None;
        assert!(!reset_credentials_eligible(&enabled, &no_account));
        assert!(!reset_credentials_eligible(&CodexConfig::default(), &oauth));
    }

    #[test]
    fn f7_legacy_whitespace_account_id_is_preserved_but_cannot_arm_resets() {
        let raw = br#"{
            "auth_mode": "chatgpt",
            "tokens": { "access_token": "oauth", "account_id": "   " }
        }"#;
        let credentials = parse_credentials(raw).unwrap();
        assert_eq!(credentials.account_id.as_deref(), Some("   "));
        assert!(!reset_credentials_eligible(
            &CodexConfig {
                auto_use_resets: 60
            },
            &credentials
        ));
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
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T13:44:39Z"));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.window_minutes, Some(10080)); // weekly
        assert!(usage.tertiary.is_none());
    }

    #[test]
    fn usage_snapshot_preserves_explicit_limit_reached() {
        let snapshot = normalize_usage_snapshot(
            br#"{
                "rate_limit": {
                    "limit_reached": true,
                    "primary_window": { "used_percent": 12 }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(snapshot.limit_reached, Some(true));
        assert_eq!(snapshot.usage.primary.unwrap().used_percent, 12.0);
    }

    #[test]
    fn window_with_percent_but_no_reset_is_kept_resetless() {
        // CodexBar-faithful: a window reporting a percent with no reset (e.g. an
        // idle window) is emitted with resetsAt omitted, not dropped. The reset is
        // never fabricated.
        let body = br#"{ "rate_limit": { "primary_window": { "used_percent": 50 } } }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.expect("window kept with percent, reset-less");
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn window_without_used_percent_is_dropped() {
        // The percent is the load-bearing field; without it there is no window.
        let body = br#"{ "rate_limit": { "primary_window": { "reset_at": 1782135879 } } }"#;
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
            resolve_usage_url(Some(
                "chatgpt_base_url = \"https://proxy.local/backend-api\"\n"
            )),
            "https://proxy.local/backend-api/wham/usage"
        );
    }
}
