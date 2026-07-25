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
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::codex_resets::{
    normalize_credits, response_now, CreditsHttpResponse, CreditsSnapshot, ReqwestResetTransport,
    ResetCoordinator, ResetRequest, ResetTickInput, ResetTransport, UsageFacts,
};
use crate::config::CodexConfig;
use crate::credential_source::{CredentialSource, VaultCapability, VaultCredential};
use crate::provider::AccountObservation;
use crate::provider::{CredentialHandle, FetchAttempt};
use crate::vault_handles::VaultHandleLoader;
use crate::{
    http::{Header, JsonRequest},
    model::{AccountInfo, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "codex";
const DEFAULT_BASE: &str = "https://chatgpt.com/backend-api";
const USAGE_PATH: &str = "/wham/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ARMED_USAGE_TIMEOUT: Duration = Duration::from_secs(12);

/// Resolved credentials for the usage call.
#[derive(Clone, PartialEq)]
pub struct CodexCredentials {
    pub bearer: String,
    pub account_id: Option<String>,
    /// Email from the local OAuth id_token profile claim, when present.
    pub email: Option<String>,
    /// True when `bearer` is the OAuth access token (account-scoped), false when
    /// it is a fallback API key.
    pub is_oauth: bool,
}

/// The subset of `auth.json` we read.
#[derive(Deserialize)]
struct AuthFile {
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
    id_token: Option<String>,
}

impl std::fmt::Debug for CodexCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexCredentials")
            .field("bearer", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("email", &self.email)
            .field("is_oauth", &self.is_oauth)
            .finish()
    }
}

impl std::fmt::Debug for AuthFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthFile")
            .field(
                "openai_api_key",
                &self.openai_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("tokens", &self.tokens)
            .finish()
    }
}

impl std::fmt::Debug for AuthTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthTokens")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("account_id", &self.account_id)
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// One immutable credential result used by every read and mutation in a tick.
struct ServedCodexContext {
    bearer: String,
    canonical_account_id: Option<String>,
    record_version: Option<u64>,
    email: Option<String>,
    org_name: Option<String>,
    capability: Option<VaultCapability>,
    is_oauth: bool,
    source: &'static str,
}

impl std::fmt::Debug for ServedCodexContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServedCodexContext")
            .field("bearer", &"<redacted>")
            .field("canonical_account_id", &self.canonical_account_id)
            .field("record_version", &self.record_version)
            .field("email", &self.email)
            .field("org_name", &self.org_name)
            .field("capability", &self.capability)
            .field("is_oauth", &self.is_oauth)
            .field("source", &self.source)
            .finish()
    }
}

fn canonical_account_id(account_id: Option<String>) -> Option<String> {
    account_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn chatgpt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    canonical_account_id(
        claims
            .get("chatgpt_account_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                claims
                    .get("https://api.openai.com/auth")
                    .and_then(|auth| auth.get("chatgpt_account_id"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(ToString::to_string),
    )
}

fn id_token_email(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    canonical_label(
        claims
            .get("https://api.openai.com/profile")
            .and_then(|profile| profile.get("email"))
            .and_then(serde_json::Value::as_str)
            .map(|email| email.trim().to_ascii_lowercase()),
    )
}

impl ServedCodexContext {
    fn local(credentials: CodexCredentials) -> Self {
        Self {
            canonical_account_id: credentials.account_id,
            bearer: credentials.bearer,
            record_version: None,
            email: canonical_label(credentials.email),
            org_name: None,
            capability: None,
            is_oauth: credentials.is_oauth,
            source: if credentials.is_oauth { "oauth" } else { "api" },
        }
    }

    fn vault(
        capability: VaultCapability,
        mut credential: VaultCredential,
    ) -> Result<Self, FetchError> {
        let bearer = match String::from_utf8(std::mem::take(&mut credential.payload)) {
            Ok(bearer) => bearer,
            Err(error) => {
                let mut payload = error.into_bytes();
                payload.fill(0);
                return Err(FetchError::Decode(
                    "vault credential payload is not valid UTF-8".to_string(),
                ));
            }
        };
        let canonical_account_id = canonical_account_id(credential.account_id.clone())
            .or_else(|| chatgpt_account_id(&bearer));
        let email = canonical_label(credential.email.clone());
        let org_name = canonical_label(credential.org_name.clone());
        Ok(Self {
            bearer,
            canonical_account_id,
            record_version: Some(credential.record_version),
            email,
            org_name,
            capability: Some(capability),
            is_oauth: true,
            source: "vault",
        })
    }

    fn credentials(&self) -> CodexCredentials {
        CodexCredentials {
            bearer: self.bearer.clone(),
            account_id: self.canonical_account_id.clone(),
            email: self.email.clone(),
            is_oauth: self.is_oauth,
        }
    }

    fn observation(&self) -> AccountObservation {
        AccountObservation::new(self.canonical_account_id.clone(), self.record_version)
    }

    fn account_info(&self, plan_type: Option<String>) -> Option<AccountInfo> {
        let info = AccountInfo {
            email: self.email.clone(),
            org_name: self.org_name.clone(),
            plan_type: plan_type.and_then(|value| canonical_label(Some(value))),
        };
        (!info.is_empty()).then_some(info)
    }
}

fn canonical_label(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
                // The explicit field duplicates a claim the access token already
                // carries, so fall back to the token when it is absent. Losing the
                // identity is not cosmetic: it disarms account-scoped banked resets,
                // and because the read path emits a single unlabeled entry unless
                // EVERY handle resolves an account, one identity-less lane collapses
                // all of this provider's accounts into one row. The vault path
                // already resolves identity this way.
                account_id: tokens
                    .account_id
                    .clone()
                    .filter(|account_id| !account_id.is_empty())
                    .or_else(|| chatgpt_account_id(access)),
                email: tokens.id_token.as_deref().and_then(id_token_email),
                is_oauth: true,
            });
        }
    }

    if let Some(key) = auth.openai_api_key.as_deref().filter(|k| !k.is_empty()) {
        return Ok(CodexCredentials {
            bearer: key.to_string(),
            account_id: None,
            email: None,
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
    #[serde(default)]
    plan_type: Option<String>,
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
        raw_used_percent: None,
        resets_at,
        window_minutes,
        used_count: None,
        total_count: None,
    })
}

/// Raw usage plus the server's explicit wall indicator.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexUsageSnapshot {
    pub usage: Usage,
    pub limit_reached: Option<bool>,
    pub plan_type: Option<String>,
}

/// Normalize a full `/wham/usage` JSON body without relaxing any percentages.
pub fn normalize_usage_snapshot(body: &[u8]) -> Result<CodexUsageSnapshot, FetchError> {
    let response: UsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("usage response not decodable: {e}")))?;
    let rate_limit = response
        .rate_limit
        .ok_or_else(|| FetchError::Decode("usage response missing rate_limit".to_string()))?;
    Ok(CodexUsageSnapshot {
        plan_type: canonical_label(response.plan_type),
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

async fn send_codex_request(
    request: JsonRequest,
    client: &reqwest::Client,
    preserve_provider_status: bool,
) -> Result<Vec<u8>, FetchError> {
    if preserve_provider_status {
        request
            .send_provider_status_first(client, PROVIDER_NAME)
            .await
            .map(|response| response.body)
    } else {
        request.send(client).await
    }
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
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    codex_home_override: Option<PathBuf>,
}

impl CodexProvider {
    pub fn new(
        reset_config: CodexConfig,
        credential_source: Option<Arc<dyn CredentialSource>>,
    ) -> Self {
        Self::new_with_handle_loader(
            reset_config,
            credential_source,
            Arc::new(VaultHandleLoader::from_env()),
        )
    }

    pub(crate) fn new_with_handle_loader(
        reset_config: CodexConfig,
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
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
            credential_source,
            handle_loader,
            codex_home_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        reset_config: CodexConfig,
        credential_source: Option<Arc<dyn CredentialSource>>,
        reset_transport: Arc<dyn ResetTransport>,
        reset_coordinator: Arc<ResetCoordinator>,
        handle_loader: VaultHandleLoader,
        codex_home_override: PathBuf,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            reset_config,
            reset_transport,
            reset_coordinator: Ok(reset_coordinator),
            credential_source,
            handle_loader: Arc::new(handle_loader),
            codex_home_override: Some(codex_home_override),
        }
    }

    fn resolved_codex_home(&self) -> Option<PathBuf> {
        self.codex_home_override.clone().or_else(codex_home)
    }

    fn report_auth_failure(&self, context: &ServedCodexContext, error: &FetchError) {
        let FetchError::ProviderStatus(status @ (401 | 403)) = error else {
            return;
        };
        let (Some(source), Some(capability), Some(record_version)) = (
            self.credential_source.as_ref(),
            context.capability.as_ref(),
            context.record_version,
        ) else {
            return;
        };
        let source = Arc::clone(source);
        let capability = capability.clone();
        let status = *status;
        tokio::spawn(async move {
            source
                .report_auth_failure(&capability, status, record_version)
                .await;
        });
    }

    async fn fetch_context(
        &self,
        context: ServedCodexContext,
        config_toml: Option<String>,
        attempt_started: Instant,
    ) -> FetchAttempt {
        let observed = Some(context.observation());
        let source = context.source;
        let credentials = context.credentials();
        let preserve_provider_status = context.capability.is_some();
        let usage_url = resolve_usage_url(config_toml.as_deref());
        let account_id = context
            .canonical_account_id
            .as_deref()
            .filter(|account_id| context.is_oauth && !account_id.trim().is_empty());
        let reset_eligible = account_id.is_some_and(|account_id| {
            reset_credentials_eligible(&self.reset_config, &credentials)
                && !account_id.trim().is_empty()
        });

        let Some(account_id) = account_id else {
            let usage = send_codex_request(
                usage_request(usage_url, &credentials, REQUEST_TIMEOUT),
                &self.http,
                preserve_provider_status,
            )
            .await
            .and_then(|body| normalize_usage_snapshot(&body));
            if let Err(error) = &usage {
                self.report_auth_failure(&context, error);
            }
            if self.reset_config.is_enabled() {
                let facts = usage.as_ref().ok().map(|snapshot| {
                    UsageFacts::from_usage(&snapshot.usage, snapshot.limit_reached)
                });
                log_reset_tick(facts.as_ref(), None, None, false, false);
            }
            return match usage {
                Ok(snapshot) => {
                    let plan_type = snapshot.plan_type.clone();
                    unarmed_usage_attempt(observed, source, snapshot)
                        .with_account_info(context.account_info(plan_type))
                }
                Err(error) => FetchAttempt::failure(observed, Some(source.to_string()), error),
            };
        };

        let auth_failure = match (
            self.credential_source.as_ref(),
            context.capability.as_ref(),
            context.record_version,
        ) {
            (Some(credential_source), Some(capability), Some(record_version)) => {
                Some(crate::codex_resets::AuthFailureContext {
                    source: Arc::clone(credential_source),
                    capability: capability.clone(),
                    record_version,
                })
            }
            _ => None,
        };
        let reset_request = ResetRequest {
            base_url: resolve_base_url(config_toml.as_deref()),
            bearer: context.bearer.clone(),
            account_id: account_id.to_string(),
            auth_failure,
        };
        let usage_timeout = if reset_eligible {
            ARMED_USAGE_TIMEOUT
        } else {
            REQUEST_TIMEOUT
        };
        let usage_future = send_codex_request(
            usage_request(usage_url, &credentials, usage_timeout),
            &self.http,
            preserve_provider_status,
        );
        // Metadata reads are always allowed for account-scoped OAuth contexts;
        // only the coordinator below can authorize a consume POST.
        let credits_future = self.reset_transport.fetch_credits(&reset_request);
        let (usage_http, credits_http) = tokio::join!(usage_future, credits_future);
        if let Err(error) = &credits_http {
            reset_request.report_auth_failure(error);
        }

        let usage_snapshot = usage_http.and_then(|body| normalize_usage_snapshot(&body));
        let local_now = Utc::now();
        let credits_snapshot = normalize_credits_tick(credits_http, local_now);

        let usage_snapshot = match usage_snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.report_auth_failure(&context, &error);
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
                    "[ck-quota] warning: codex credits GET failed account_id={account_id}: {error}; usage metadata unavailable"
                );
                log_reset_tick(Some(&facts), None, None, false, false);
                return FetchAttempt::success(observed, source, usage_snapshot.usage)
                    .with_account_info(context.account_info(usage_snapshot.plan_type));
            }
        };
        let saved_resets = Some(credits.saved_resets());
        let account_info = context.account_info(usage_snapshot.plan_type);
        let Some(earliest_expiry) = reset_trigger_expiry(&credits, now) else {
            log_reset_tick(Some(&facts), Some(&credits), None, false, false);
            return FetchAttempt::success(observed, source, usage_snapshot.usage)
                .with_account_info(account_info)
                .with_saved_resets(saved_resets);
        };

        if !reset_eligible {
            log_reset_tick(
                Some(&facts),
                Some(&credits),
                Some(earliest_expiry),
                false,
                false,
            );
            return FetchAttempt::success(observed, source, usage_snapshot.usage)
                .with_account_info(account_info)
                .with_saved_resets(saved_resets);
        }

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
                return FetchAttempt::success(observed, source, usage_snapshot.usage)
                    .with_account_info(account_info)
                    .with_saved_resets(saved_resets);
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
            .with_account_info(account_info)
            .with_saved_resets(saved_resets)
            .with_relax_eligible(result.relax_eligible)
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new(CodexConfig::default(), None)
    }
}

#[async_trait]
impl UsageProvider for CodexProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        let mut handles = vec![CredentialHandle::implicit()];
        if self.credential_source.is_some() {
            handles.extend(self.handle_loader.codex_handles()?);
        }
        Ok(handles)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let attempt_started = Instant::now();
        let codex_home = self.resolved_codex_home();
        match handle.vault_capability() {
            Some(capability) => {
                let Some(credential_source) = self.credential_source.as_ref() else {
                    return FetchAttempt::unverified_vault_failure(
                        crate::credential_source::VaultGetError::Permanent,
                    );
                };
                let credential = match credential_source.get(capability, 120_000).await {
                    Ok(credential) => credential,
                    Err(error) => return FetchAttempt::unverified_vault_failure(error),
                };
                let fallback_observation = Some(AccountObservation::new(
                    canonical_account_id(credential.account_id.clone()),
                    Some(credential.record_version),
                ));
                let context = match ServedCodexContext::vault(capability.clone(), credential) {
                    Ok(context) => context,
                    Err(error) => {
                        return FetchAttempt::failure(fallback_observation, None, error);
                    }
                };
                let config_toml = tokio::task::spawn_blocking(move || {
                    codex_home
                        .and_then(|home| std::fs::read_to_string(home.join("config.toml")).ok())
                })
                .await
                .unwrap_or(None);
                self.fetch_context(context, config_toml, attempt_started)
                    .await
            }
            None => {
                let resolved = tokio::task::spawn_blocking(move || {
                    let home = codex_home.ok_or_else(|| {
                        FetchError::NoSession(
                            "cannot resolve CODEX_HOME or $HOME/.codex".to_string(),
                        )
                    })?;
                    let auth_path = home.join("auth.json");
                    let data = std::fs::read(&auth_path).map_err(|error| {
                        FetchError::NoSession(format!("reading {}: {error}", auth_path.display()))
                    })?;
                    let credentials = parse_credentials(&data)?;
                    let config_toml = std::fs::read_to_string(home.join("config.toml")).ok();
                    Ok::<_, FetchError>((ServedCodexContext::local(credentials), config_toml))
                })
                .await;
                let (context, config_toml) = match resolved {
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
                            FetchError::Decode(
                                "codex credential resolution task panicked".to_string(),
                            ),
                        );
                    }
                };
                self.fetch_context(context, config_toml, attempt_started)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_resets::RedemptionJournal;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct ReportingSource {
        reports: Arc<Mutex<Vec<(u16, u64)>>>,
    }

    #[async_trait]
    impl CredentialSource for ReportingSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, crate::credential_source::VaultGetError> {
            unreachable!("reporting test does not fetch")
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            provider_status: u16,
            record_version: u64,
        ) {
            self.reports
                .lock()
                .unwrap()
                .push((provider_status, record_version));
        }
    }

    async fn serve_one_raw_response(response: Vec<u8>) -> String {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let _ = stream.read(&mut request).await;
            stream.write_all(&response).await.unwrap();
        });
        format!("http://{address}/backend-api")
    }

    async fn serve_codex_metadata_routes(
        usage_body: Vec<u8>,
        credits_body: Vec<u8>,
    ) -> (
        String,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let usage_gets = Arc::new(AtomicUsize::new(0));
        let credits_gets = Arc::new(AtomicUsize::new(0));
        let consume_posts = Arc::new(AtomicUsize::new(0));
        let usage_gets_server = Arc::clone(&usage_gets);
        let credits_gets_server = Arc::clone(&credits_gets);
        let consume_posts_server = Arc::clone(&consume_posts);
        let task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("");
                let (body, method) = if path.ends_with("/wham/usage") {
                    usage_gets_server.fetch_add(1, Ordering::SeqCst);
                    (&usage_body, "GET")
                } else if path.ends_with("/wham/rate-limit-reset-credits") {
                    credits_gets_server.fetch_add(1, Ordering::SeqCst);
                    (&credits_body, "GET")
                } else if path.ends_with("/wham/rate-limit-reset-credits/consume") {
                    consume_posts_server.fetch_add(1, Ordering::SeqCst);
                    (&credits_body, "POST")
                } else {
                    (&credits_body, "GET")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
                let _ = method;
            }
        });
        (
            format!("http://{address}/backend-api"),
            usage_gets,
            credits_gets,
            consume_posts,
            task,
        )
    }

    fn context_config(base_url: &str) -> Option<String> {
        Some(format!("chatgpt_base_url = {base_url:?}\n"))
    }

    struct CountingResetTransport {
        gets: AtomicUsize,
        posts: AtomicUsize,
        fail_get: bool,
    }

    #[async_trait]
    impl ResetTransport for CountingResetTransport {
        async fn fetch_credits(
            &self,
            _request: &ResetRequest,
        ) -> Result<CreditsHttpResponse, FetchError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            if self.fail_get {
                return Err(FetchError::Upstream("credits unavailable".to_string()));
            }
            Ok(CreditsHttpResponse {
                body: br#"{
                    "credits": [{
                        "id": "credit-unarmed",
                        "status": "available",
                        "expires_at": "2026-07-15T12:00:00Z"
                    }],
                    "available_count": 1
                }"#
                .to_vec(),
                date_header: None,
            })
        }

        async fn consume(
            &self,
            _request: &ResetRequest,
            _redeem_request_id: &str,
        ) -> Result<Vec<u8>, FetchError> {
            self.posts.fetch_add(1, Ordering::SeqCst);
            Err(FetchError::Upstream(
                "consume should not be called".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn unarmed_registry_fetches_credits_without_consume_post() {
        let usage_body = br#"{
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {"used_percent": 41.0}
            }
        }"#;
        let usage_response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            usage_body.len(),
            String::from_utf8_lossy(usage_body)
        );
        let usage_base = serve_one_raw_response(usage_response.into_bytes()).await;
        let home =
            std::env::temp_dir().join(format!("ck-quota-codex-unarmed-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            br#"{"tokens":{"access_token":"oauth-token","account_id":"acct-unarmed"}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("config.toml"),
            context_config(&usage_base).unwrap(),
        )
        .unwrap();

        let transport = Arc::new(CountingResetTransport {
            gets: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
            fail_get: false,
        });
        let journal_dir = home.join("state");
        let coordinator = Arc::new(
            ResetCoordinator::new(RedemptionJournal::new(journal_dir.join("redemptions.json")))
                .unwrap(),
        );
        let provider = CodexProvider::new_for_test(
            CodexConfig::default(),
            None,
            Arc::clone(&transport) as Arc<dyn ResetTransport>,
            coordinator,
            VaultHandleLoader::new(None),
            home.clone(),
        );
        let registry = crate::Registry::new(vec![Box::new(provider)]);
        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let usage = registry.get_usage(None).await;

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].account.as_deref(), Some("acct-unarmed"));
        assert_eq!(
            usage[0].account_info.as_ref().unwrap().plan_type.as_deref(),
            Some("pro")
        );
        assert_eq!(usage[0].saved_resets.as_ref().unwrap().available_count, 1);
        assert_eq!(transport.gets.load(Ordering::SeqCst), 1);
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn unarmed_registry_http_gets_credits_without_consume_post() {
        let usage_body = br#"{
            "plan_type": "pro",
            "rate_limit": {"primary_window": {"used_percent": 41.0}}
        }"#
        .to_vec();
        let credits_body = br#"{
            "credits": [{
                "id": "credit-http",
                "status": "available",
                "expires_at": "2026-07-15T12:00:00Z"
            }],
            "available_count": 1
        }"#
        .to_vec();
        let (base_url, usage_gets, credits_gets, consume_posts, server) =
            serve_codex_metadata_routes(usage_body, credits_body).await;
        let home = std::env::temp_dir().join(format!(
            "ck-quota-codex-http-unarmed-{}-{}",
            std::process::id(),
            usage_gets.as_ref() as *const _ as usize
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("auth.json"),
            br#"{"tokens":{"access_token":"oauth-token","account_id":"acct-http-unarmed"}}"#,
        )
        .unwrap();
        std::fs::write(home.join("config.toml"), context_config(&base_url).unwrap()).unwrap();
        let coordinator = Arc::new(
            ResetCoordinator::new(RedemptionJournal::new(home.join("redemptions.json"))).unwrap(),
        );
        let provider = CodexProvider::new_for_test(
            CodexConfig::default(),
            None,
            Arc::new(ReqwestResetTransport::new(reqwest::Client::new())),
            coordinator,
            VaultHandleLoader::new(None),
            home.clone(),
        );
        let registry = crate::Registry::new(vec![Box::new(provider)]);
        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let usage = registry.get_usage(None).await;
        server.await.unwrap();

        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].saved_resets.as_ref().unwrap().available_count, 1);
        assert_eq!(usage_gets.load(Ordering::SeqCst), 1);
        assert_eq!(credits_gets.load(Ordering::SeqCst), 1);
        assert_eq!(consume_posts.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn credits_get_failure_keeps_codex_usage_healthy() {
        let usage_body = br#"{
            "plan_type": "pro",
            "rate_limit": {
                "primary_window": {"used_percent": 41.0}
            }
        }"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            usage_body.len(),
            String::from_utf8_lossy(usage_body)
        );
        let base_url = serve_one_raw_response(response.into_bytes()).await;
        let transport = Arc::new(CountingResetTransport {
            gets: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
            fail_get: true,
        });
        let state_dir = std::env::temp_dir().join(format!(
            "ck-quota-codex-credits-failure-{}",
            std::process::id()
        ));
        let coordinator = Arc::new(
            ResetCoordinator::new(RedemptionJournal::new(state_dir.join("redemptions.json")))
                .unwrap(),
        );
        let provider = CodexProvider::new_for_test(
            CodexConfig::default(),
            None,
            Arc::clone(&transport) as Arc<dyn ResetTransport>,
            coordinator,
            VaultHandleLoader::new(None),
            state_dir.clone(),
        );
        let attempt = provider
            .fetch_context(
                ServedCodexContext {
                    bearer: "oauth-token".to_string(),
                    canonical_account_id: Some("acct-credits-failure".to_string()),
                    record_version: None,
                    email: None,
                    org_name: None,
                    capability: None,
                    is_oauth: true,
                    source: "oauth",
                },
                context_config(&base_url),
                Instant::now(),
            )
            .await;

        assert!(attempt.usage.is_ok());
        assert!(attempt.saved_resets.is_none());
        assert_eq!(transport.gets.load(Ordering::SeqCst), 1);
        assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn i2_local_401_keeps_legacy_error_while_vault_preserves_status() {
        let response =
            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        let local_url = serve_one_raw_response(response.to_vec()).await;
        let provider = CodexProvider::default();
        let local = provider
            .fetch_context(
                ServedCodexContext::local(CodexCredentials {
                    bearer: "local-secret".to_string(),
                    account_id: None,
                    email: None,
                    is_oauth: true,
                }),
                context_config(&local_url),
                Instant::now(),
            )
            .await;
        assert!(matches!(
            local.usage,
            Err(FetchError::Unauthorized(message)) if message.starts_with("HTTP 401")
        ));

        let vault_url = serve_one_raw_response(response.to_vec()).await;
        let vault = provider
            .fetch_context(
                ServedCodexContext {
                    bearer: "vault-secret".to_string(),
                    canonical_account_id: None,
                    email: None,
                    org_name: None,
                    record_version: Some(4),
                    capability: Some(VaultCapability::new("ckh_test")),
                    is_oauth: true,
                    source: "vault",
                },
                context_config(&vault_url),
                Instant::now(),
            )
            .await;
        assert!(matches!(vault.usage, Err(FetchError::ProviderStatus(401))));
    }

    #[tokio::test]
    async fn i4_truncated_401_preserves_status_and_reports_auth_failure() {
        let response =
            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 100\r\nconnection: close\r\n\r\nshort";
        let base_url = serve_one_raw_response(response.to_vec()).await;
        let reports = Arc::new(Mutex::new(Vec::new()));
        let source: Arc<dyn CredentialSource> = Arc::new(ReportingSource {
            reports: Arc::clone(&reports),
        });
        let provider = CodexProvider::new(CodexConfig::default(), Some(source));
        let attempt = provider
            .fetch_context(
                ServedCodexContext {
                    bearer: "vault-secret".to_string(),
                    canonical_account_id: None,
                    email: None,
                    org_name: None,
                    record_version: Some(44),
                    capability: Some(VaultCapability::new("ckh_truncated")),
                    is_oauth: true,
                    source: "vault",
                },
                context_config(&base_url),
                Instant::now(),
            )
            .await;
        assert!(matches!(
            attempt.usage,
            Err(FetchError::ProviderStatus(401))
        ));
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 44)]);
    }

    #[test]
    fn secret_bearing_codex_debug_is_redacted() {
        let secret = "codex-debug-secret";
        let credentials = CodexCredentials {
            bearer: secret.to_string(),
            account_id: Some("acct".to_string()),
            email: None,
            is_oauth: true,
        };
        let auth: AuthFile = serde_json::from_value(serde_json::json!({
            "OPENAI_API_KEY": secret,
            "tokens": { "access_token": secret, "account_id": "acct" }
        }))
        .unwrap();
        for debug in [format!("{credentials:?}"), format!("{auth:?}")] {
            assert!(!debug.contains(secret));
            assert!(debug.contains("redacted"));
        }
    }

    #[test]
    fn vault_context_resolves_trimmed_account_once_with_jwt_fallback() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "chatgpt_account_id": "  jwt-account  "
            }))
            .unwrap(),
        );
        let token = format!("header.{payload}.signature");
        let context = ServedCodexContext::vault(
            VaultCapability::new("ckh_context_secret"),
            VaultCredential {
                payload: token.into_bytes(),
                expires_at_ms: None,
                record_version: 17,
                account_id: None,
                email: Some("vault@example.com".to_string()),
                org_name: None,
                project_id: None,
            },
        )
        .unwrap();
        assert_eq!(context.canonical_account_id.as_deref(), Some("jwt-account"));
        assert_eq!(context.observation().record_version, Some(17));
        assert_eq!(
            context.account_info(None).unwrap().email.as_deref(),
            Some("vault@example.com")
        );
        assert!(!format!("{context:?}").contains("ckh_context_secret"));
    }

    #[test]
    fn i5_organization_claim_is_not_an_account_identity() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"organizations":[{"id":"shared-org"}]}"#);
        let token = format!("header.{payload}.signature");
        assert_eq!(chatgpt_account_id(&token), None);
    }

    #[tokio::test]
    async fn usage_auth_failure_report_uses_served_record_version() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let source: Arc<dyn CredentialSource> = Arc::new(ReportingSource {
            reports: Arc::clone(&reports),
        });
        let provider = CodexProvider::new(CodexConfig::default(), Some(source));
        let context = ServedCodexContext {
            bearer: "secret".to_string(),
            canonical_account_id: Some("acct".to_string()),
            email: None,
            org_name: None,
            record_version: Some(23),
            capability: Some(VaultCapability::new("ckh_report_secret")),
            is_oauth: true,
            source: "vault",
        };
        provider.report_auth_failure(&context, &FetchError::ProviderStatus(401));
        tokio::task::yield_now().await;
        assert_eq!(*reports.lock().unwrap(), vec![(401, 23)]);
    }

    fn jwt_with_payload(payload: serde_json::Value) -> String {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("header.{encoded}.signature")
    }

    #[test]
    fn local_id_token_profile_email_is_trimmed_and_lowercased() {
        let id_token = jwt_with_payload(serde_json::json!({
            "https://api.openai.com/profile": {"email": "  User@Example.COM "}
        }));
        let raw = serde_json::to_vec(&serde_json::json!({
            "tokens": {
                "access_token": "oauth",
                "account_id": "acct",
                "id_token": id_token
            }
        }))
        .unwrap();
        let credentials = parse_credentials(&raw).unwrap();
        assert_eq!(credentials.email.as_deref(), Some("user@example.com"));
        assert_eq!(
            ServedCodexContext::local(credentials)
                .account_info(None)
                .unwrap()
                .email
                .as_deref(),
            Some("user@example.com")
        );
    }

    #[test]
    fn malformed_or_missing_id_token_email_is_not_a_credential_error() {
        for id_token in [None, Some("not-a-jwt")] {
            let mut tokens = serde_json::json!({"access_token": "oauth"});
            if let Some(id_token) = id_token {
                tokens["id_token"] = serde_json::Value::String(id_token.to_string());
            }
            let raw = serde_json::to_vec(&serde_json::json!({"tokens": tokens})).unwrap();
            let credentials = parse_credentials(&raw).unwrap();
            assert_eq!(credentials.email, None);
        }
    }

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
    fn local_account_id_falls_back_to_the_access_token_claim() {
        // auth.json's account_id duplicates a claim the access token already
        // carries, and it is not guaranteed to be written. Losing the identity
        // disarms account-scoped banked resets, and because the read path emits a
        // single unlabeled entry unless EVERY handle resolves an account, one
        // identity-less lane collapses all Codex accounts into one row.
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-from-jwt"}}"#);
        let raw = format!(r#"{{ "tokens": {{ "access_token": "hdr.{claims}.sig" }} }}"#);

        let creds = parse_credentials(raw.as_bytes()).unwrap();
        assert_eq!(creds.account_id.as_deref(), Some("acct-from-jwt"));
        assert!(creds.is_oauth);
    }

    #[test]
    fn an_explicit_local_account_id_wins_over_the_token_claim() {
        // The fallback must not override an explicit value: the file is the
        // authority when it states one, and a token can outlive a re-login.
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-from-jwt"}}"#);
        let raw = format!(
            r#"{{ "tokens": {{ "access_token": "hdr.{claims}.sig", "account_id": "acct-explicit" }} }}"#
        );

        let creds = parse_credentials(raw.as_bytes()).unwrap();
        assert_eq!(creds.account_id.as_deref(), Some("acct-explicit"));
    }

    #[test]
    fn an_opaque_local_token_still_resolves_no_account() {
        // A token carrying no claim must stay None rather than acquiring a
        // fabricated identity — the fallback recovers identity, never invents it.
        let raw = br#"{ "tokens": { "access_token": "opaque-not-a-jwt" } }"#;
        let creds = parse_credentials(raw).unwrap();
        assert_eq!(creds.account_id, None);
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
            email: None,
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
        let snapshot = normalize_usage_snapshot(body).unwrap();
        assert_eq!(snapshot.plan_type.as_deref(), Some("pro"));
        let usage = snapshot.usage;
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
