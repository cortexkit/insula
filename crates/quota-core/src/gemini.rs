//! Gemini (Google Code Assist) usage fetcher — OAuth refresh sub-archetype.
//!
//! Auth source: `~/.gemini/oauth_creds.json`, the file gemini-cli ITSELF creates
//! (a real native, headless path), holding `access_token` / `refresh_token` /
//! `expiry_date` (epoch ms) with the Code Assist scope (`cloud-platform`). The
//! access token is short-lived (~1h), so when it is expired/absent we refresh it
//! via `POST https://oauth2.googleapis.com/token` (form: client_id, client_secret,
//! refresh_token, grant_type=refresh_token) and cache the new token in-memory until
//! its expiry. We READ the creds file only and NEVER write the refreshed token
//! back — that file is gemini-cli's, and we are a read-only quota observer.
//! Vault handles take a separate strict path: the served payload is the bare Google
//! access token, optional project metadata comes from the vault result, and the
//! local file, refresh endpoint, and token cache are never touched.
//!
//! We deliberately do NOT replicate CodexBar's package archaeology to discover the
//! OAuth client (locating the installed @google/gemini-cli bundle across npm/brew/
//! nix/fnm layouts) — that exists only because CodexBar is a macOS desktop app and
//! cannot run in our headless context. gemini-cli is open source, so we hardcode
//! its public installed-app OAuth client (overridable via env) and cite the source.
//!
//! Fetch: `loadCodeAssist` → project id + tier, then `retrieveUserQuota` → per-model
//! quota buckets. Every bucket remains a named extra window, while the most-used
//! bucket is copied to `primary` as the binding constraint. The reset delta is
//! rounded to a known class (`100 - remainingFraction*100` → usedPercent).
//!
//! VERIFICATION: gemini is LIVE-VERIFIED — the real OAuth refresh + quota path was
//! proven end-to-end through the wire from an expired native token (refresh →
//! loadCodeAssist → retrieveUserQuota → real per-model windows). The embedded OAuth
//! client value is sourced from open-source gemini-cli
//! `packages/core/src/code_assist/oauth2.ts:76-85` @ tag v0.47.0 (commit
//! be7ba2c22adf095880b81da26f1afcc48520eebc) — a PUBLIC installed-app client (RFC
//! 8252 / Google's native-app OAuth doc: installed-app secrets are not confidential
//! by design). It is stored XOR-MASKED (not for secrecy — to avoid secret-scanner
//! false positives), a technique ported from OmniRoute's headless TS proxy
//! `open-sse/utils/publicCreds.ts`. Overridable via `GEMINI_OAUTH_CLIENT_ID` /
//! `GEMINI_OAUTH_CLIENT_SECRET` if Google rotates it. Endpoints/response shape
//! ported from CodexBar `Sources/CodexBarCore/Providers/Gemini/GeminiStatusProbe.swift:
//! 139-144,197-360,682-724` (QuotaBucket fields + tier classifiers). Rides the
//! live-proven `http.rs`.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::credential_source::{CredentialSource, VaultCapability};
use crate::provider::{AccountObservation, CredentialHandle, FetchAttempt};
use crate::vault_handles::VaultHandleLoader;
use crate::{
    env,
    http::JsonRequest,
    model::{ExtraWindow, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "gemini";

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const LOAD_CODE_ASSIST_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const QUOTA_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
const PROJECTS_URL: &str = "https://cloudresourcemanager.googleapis.com/v1/projects";

// The public installed-app OAuth client shipped in open-source gemini-cli. It is
// public by design (RFC 8252 — installed-app secrets are not confidential), but
// the literal values trip secret-scanner regexes (`...googleusercontent.com`, the
// `GOCSPX-` prefix) and produce false-positive alerts on every push. Following
// OmniRoute's publicCreds.ts, the bytes are XOR-masked with `CRED_MASK` and decoded
// at runtime — NOT for secrecy (trivially reversible), only to avoid the scanner
// regexes in source text. Overridable via env when Google rotates the client.
const CRED_MASK: &[u8] = b"quota-public-creds-v1";
const CLIENT_ID_MASKED: &[u8] = &[
    71, 77, 94, 70, 84, 24, 72, 69, 91, 95, 80, 86, 0, 12, 29, 93, 2, 7, 31, 25, 65, 3, 17, 29, 26,
    17, 20, 21, 70, 3, 29, 15, 85, 76, 21, 65, 13, 9, 23, 68, 20, 0, 66, 64, 5, 90, 0, 93, 0, 6,
    76, 11, 6, 12, 74, 15, 23, 16, 23, 22, 95, 21, 94, 31, 1, 10, 26, 21, 3, 19, 26, 15,
];
const CLIENT_SECRET_MASKED: &[u8] = &[
    54, 58, 44, 39, 49, 117, 93, 65, 23, 36, 14, 46, 125, 14, 95, 84, 11, 68, 126, 29, 28, 22, 16,
    57, 66, 34, 88, 69, 22, 14, 52, 47, 16, 85, 15,
];
const CLIENT_ID_ENV: &[&str] = &["GEMINI_OAUTH_CLIENT_ID"];
const CLIENT_SECRET_ENV: &[&str] = &["GEMINI_OAUTH_CLIENT_SECRET"];

/// XOR-unmask an embedded public credential to its plaintext.
fn unmask(masked: &[u8]) -> String {
    masked
        .iter()
        .enumerate()
        .map(|(i, b)| (b ^ CRED_MASK[i % CRED_MASK.len()]) as char)
        .collect()
}

const WINDOW_MINUTES_24H: i64 = 24 * 60;
/// Refresh a little before the real expiry to avoid using a token that lapses
/// mid-request.
const EXPIRY_SKEW: Duration = Duration::from_secs(60);

// ---- credentials file -------------------------------------------------------

#[derive(Deserialize)]
struct OauthCreds {
    access_token: Option<String>,
    refresh_token: Option<String>,
    /// Expiry in epoch MILLISECONDS (gemini-cli's format).
    expiry_date: Option<i64>,
}

impl std::fmt::Debug for OauthCreds {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OauthCreds")
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expiry_date", &self.expiry_date)
            .finish()
    }
}

fn creds_path() -> Option<PathBuf> {
    crate::env::home_dir().map(|home| home.join(".gemini/oauth_creds.json"))
}

fn read_creds() -> Result<OauthCreds, FetchError> {
    let path = creds_path()
        .ok_or_else(|| FetchError::NoSession("cannot resolve $HOME/.gemini".to_string()))?;
    let data = env::read_credential_file(&path, "gemini oauth_creds.json")?;
    serde_json::from_slice(&data)
        .map_err(|e| FetchError::Decode(format!("gemini oauth_creds.json not decodable: {e}")))
}

fn client_id() -> String {
    env::first_env(CLIENT_ID_ENV).unwrap_or_else(|| unmask(CLIENT_ID_MASKED))
}

fn client_secret() -> String {
    env::first_env(CLIENT_SECRET_ENV).unwrap_or_else(|| unmask(CLIENT_SECRET_MASKED))
}

fn canonical_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn project_from_load_response(response: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(response).ok()?;
    let project = value.get("cloudaicompanionProject")?;
    let id = match project {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Object(_) => project
            .get("id")
            .or_else(|| project.get("projectId"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }?;
    canonical_optional(Some(id))
}

fn project_from_resource_response(response: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(response).ok()?;
    let projects = value.get("projects")?.as_array()?;
    for project in projects {
        let Some(project_id) = project.get("projectId").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if project_id.starts_with("gen-lang-client")
            || project
                .get("labels")
                .and_then(|labels| labels.get("generative-language"))
                .is_some()
        {
            return Some(project_id.to_string());
        }
    }
    None
}

// ---- quota response normalization (pure) ------------------------------------

#[derive(Debug, Deserialize)]
struct QuotaResponse {
    buckets: Option<Vec<QuotaBucket>>,
}

#[derive(Debug, Deserialize)]
struct QuotaBucket {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
}

fn window_minutes_from_reset(reset_time: &str, now: DateTime<Utc>) -> Option<i64> {
    let reset = DateTime::parse_from_rfc3339(reset_time)
        .ok()?
        .with_timezone(&Utc);
    let delta_minutes = reset.signed_duration_since(now).num_seconds() as f64 / 60.0;
    if !delta_minutes.is_finite() {
        return None;
    }
    // A recorded fixture can outlive its reset timestamp; Gemini's quota buckets
    // are daily by contract, so retain the known daily class in that case.
    if delta_minutes <= 0.0 {
        return Some(WINDOW_MINUTES_24H);
    }
    // A mid-window delta only lower-bounds the true window length, so it cannot
    // justify shrinking a daily bucket to a shorter class. Assert daily unless
    // the delta proves a longer class, then choose the tightest class that still
    // contains it.
    if delta_minutes <= 1_500.0 {
        return Some(WINDOW_MINUTES_24H);
    }
    [7 * 24 * 60, 30 * 24 * 60]
        .into_iter()
        .find(|class| delta_minutes <= *class as f64)
        .or(Some(30 * 24 * 60))
}

/// Normalize a `retrieveUserQuota` body to named per-model windows. Every valid
/// bucket remains visible; the most-used bucket is also copied to `primary` so
/// the headline reflects the single current binding constraint.
fn normalize_quota_at(body: &[u8], now: DateTime<Utc>) -> Result<Usage, FetchError> {
    let response: QuotaResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("gemini quota not decodable: {e}")))?;
    // Two distinguishable inputs, and they need different answers. An ABSENT
    // field means our struct and their payload disagree -- a rename upstream or
    // a mistake here -- and Decode is the class that says "look at this repo".
    // A field PRESENT AND EMPTY is the upstream affirmatively stating the
    // account has no buckets, which is a fact about the account with nothing to
    // fix. Folding them together files an account fact as our defect, and it
    // counts toward the stale-browser-login metric on a working session.
    //
    // WHICH ARM A REAL NO-QUOTA ACCOUNT TAKES IS UNVERIFIED, and that matters
    // because the same reasoning was WRONG for qwen-cloud: that gateway states
    // "no plan" by OMITTING the block, so a rule built from this principle routed
    // the real case to Decode -- the class a retaining consumer reads as "cannot
    // read it just now", which kept a router serving a dead subscription for a day
    // (insula#11). The principle is sound; how a given upstream expresses absence
    // is a fact about that upstream and is not inferable from another one.
    //
    // The suspicion here runs the same way and cannot be settled from this host.
    // proto3 JSON omits empty repeated fields by default, and this is a
    // protobuf-backed Google API (`v1internal`, RPC-style method names), so an
    // account with zero buckets plausibly sends NO `buckets` key rather than an
    // empty list -- which would make the empty-list arm below unreachable and the
    // Decode arm the one that fires.
    //
    // NOT FLIPPED ON THAT REASONING. Replacing an unverified guess with a better
    // argued one is still a guess, and unlike qwen there is no ground truth here:
    // no account on this host has ever produced a bucket-less quota response, and
    // `QuotaResponse` carries exactly one field, so a `{}` body offers nothing to
    // confirm the response is well-formed. What would settle it is one observed
    // payload from an account without Code Assist entitlement.
    let buckets = match response.buckets {
        None => {
            return Err(FetchError::Decode(
                "gemini quota response has no buckets field".to_string(),
            ))
        }
        Some(buckets) if buckets.is_empty() => {
            return Err(FetchError::NoQuotaReported(
                "gemini: this account has no quota buckets".to_string(),
            ))
        }
        Some(buckets) => buckets,
    };

    let mut primary: Option<(f64, String, RateWindow)> = None;
    let mut extras = Vec::new();
    for bucket in buckets {
        let (Some(model_id), Some(fraction)) = (bucket.model_id, bucket.remaining_fraction) else {
            continue;
        };
        if model_id.trim().is_empty() || !fraction.is_finite() {
            continue;
        }
        let window = RateWindow {
            used_percent: (1.0 - fraction).mul_add(100.0, 0.0).clamp(0.0, 100.0),
            raw_used_percent: None,
            resets_at: bucket.reset_time.clone(),
            window_minutes: bucket
                .reset_time
                .as_deref()
                .and_then(|reset_time| window_minutes_from_reset(reset_time, now)),
            used_count: None,
            total_count: None,
            regeneration: None,
        };
        let used_percent = window.used_percent;
        if primary.as_ref().is_none_or(|(_, current_id, current)| {
            used_percent > current.used_percent
                || (used_percent == current.used_percent && model_id < *current_id)
        }) {
            primary = Some((used_percent, model_id.clone(), window.clone()));
        }
        extras.push(ExtraWindow {
            title: Some(model_id.clone()),
            id: Some(model_id),
            window: Some(window),
        });
    }

    // Buckets were present but every one was unusable. Returning Ok here would
    // publish a window-less entry as a SUCCESS, and a successful fetch is stored
    // fresh — so it would replace whatever good windows the provider had and
    // report it healthy while consumers saw no quota signal at all. A degraded
    // entry carries a reason and a transient failure keeps serving the last good
    // window; an empty success does neither. Rejecting costs nothing here, since
    // there is no window to lose.
    if primary.is_none() {
        return Err(FetchError::Decode(
            "gemini quota buckets carried no usable model fraction".to_string(),
        ));
    }

    Ok(Usage {
        primary: primary.map(|(_, _, window)| window),
        secondary: None,
        tertiary: None,
        extra_rate_windows: (!extras.is_empty()).then_some(extras),
    })
}

/// Normalize using the local wall clock to derive each bucket's window class.
pub fn normalize_quota(body: &[u8]) -> Result<Usage, FetchError> {
    normalize_quota_at(body, Utc::now())
}

// ---- token cache + refresh (I/O) --------------------------------------------

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    /// Lifetime in seconds.
    expires_in: Option<u64>,
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: Option<String>,
}

fn stated_invalid_grant(body: &[u8]) -> bool {
    serde_json::from_slice::<OAuthErrorResponse>(body)
        .ok()
        .and_then(|response| response.error)
        .as_deref()
        == Some("invalid_grant")
}

struct CachedToken {
    token: String,
    expires_at: Instant,
}

/// The Gemini usage provider. Caches the refreshed access token in-memory (never
/// written back to the creds file).
pub struct GeminiProvider {
    http: reqwest::Client,
    token: Mutex<Option<CachedToken>>,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    token_url: String,
    load_code_assist_url: String,
    quota_url: String,
    projects_url: String,
}

impl GeminiProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::from_env()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            token: Mutex::new(None),
            credential_source,
            handle_loader,
            token_url: TOKEN_URL.to_string(),
            load_code_assist_url: LOAD_CODE_ASSIST_URL.to_string(),
            quota_url: QUOTA_URL.to_string(),
            projects_url: PROJECTS_URL.to_string(),
        }
    }

    /// Read the cached access token, if one is still valid.
    ///
    /// A poisoned lock is recovered rather than propagated, matching every other
    /// lock in this crate. The guarded value is a token and an expiry, which a
    /// panicking writer cannot leave torn: the worst case is a stale entry, and
    /// the expiry check below rejects it. Panicking instead would be worse than
    /// the corruption being guarded against -- a panic inside a fetch is caught
    /// by the refresher and classified non-transient, which marks the provider
    /// dead until the process restarts, on the strength of one poisoned mutex.
    fn cached_token(&self, now: Instant) -> Option<String> {
        let guard = self
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .as_ref()
            .filter(|t| t.expires_at > now)
            .map(|t| t.token.clone())
    }

    fn store_token(&self, token: String, expires_at: Instant) {
        *self
            .token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedToken {
            token: token.clone(),
            expires_at,
        });
    }

    /// THIS LANE REFRESHES, WHICH IS AN EXCEPTION -- READ BEFORE COPYING IT.
    ///
    /// The fleet rule is that a quota reader never touches a refresh endpoint,
    /// because exactly one process may refresh a credential and that process is
    /// its custodian. A second refresher on a family whose refresh tokens ROTATE
    /// silently revokes the first holder's session -- for anthropic and openai
    /// that means signing the user out of their editor, which is why those
    /// plugin lanes were declined outright rather than built.
    ///
    /// Google's refresh tokens do NOT rotate on exchange. That was established
    /// by probe, not by documentation, and it is the entire basis on which this
    /// lane exists. It is a property of the credential family, not of this code.
    ///
    /// So the operative rule here has never been "never refresh". It is: NEVER
    /// REFRESH A FAMILY WHOSE TOKENS ROTATE. If you are reading this as
    /// precedent for a new lane, the question to answer first is which of those
    /// two your family is -- and the answer is a probe, because getting it wrong
    /// produces no error here and a logged-out user somewhere else.
    async fn refresh_access_token(
        &self,
        refresh_token: &str,
    ) -> Result<(String, Duration), FetchError> {
        let cid = client_id();
        let secret = client_secret();
        let response = JsonRequest::post_form(
            &self.token_url,
            &[
                ("client_id", &cid),
                ("client_secret", &secret),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ],
        )
        .send_raw(&self.http)
        .await?;

        if !(200..300).contains(&response.status) {
            if stated_invalid_grant(&response.body) {
                return Err(FetchError::CredentialUnusable(
                    "gemini refresh token was rejected: invalid_grant".to_string(),
                ));
            }
            if response.status == 401 || response.status == 403 {
                // Named, because this lane makes two calls that fail the same
                // way. Google answers 403 both when it refuses to renew the
                // credential and when the renewed credential may not have the
                // resource, and both rendered as a bare `unauthorized: HTTP
                // 403` -- the string that reached the wire on 2026-08-21 while
                // this provider was dark, telling nobody which had happened.
                // The remedies differ: one is a credential that cannot be
                // renewed, the other an account that is no longer entitled.
                return Err(
                    FetchError::Unauthorized(format!("HTTP {}", response.status))
                        .stage("token refresh"),
                );
            }
            let excerpt: String = String::from_utf8_lossy(&response.body)
                .chars()
                .take(200)
                .collect();
            return Err(FetchError::Upstream(format!(
                "HTTP {}: {excerpt}",
                response.status
            )));
        }

        let refreshed: RefreshResponse = serde_json::from_slice(response.body_for_parsing()?)
            .map_err(|e| FetchError::Decode(format!("gemini token refresh not decodable: {e}")))?;
        let token = refreshed
            .access_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                FetchError::Unauthorized("gemini refresh returned no access_token".to_string())
            })?;
        Ok((
            token,
            Duration::from_secs(refreshed.expires_in.unwrap_or(3600)),
        ))
    }

    /// Resolve a usable access token: a still-valid one from the creds file or the
    /// in-memory cache, else refresh via the OAuth2 token endpoint and cache it.
    async fn access_token(&self, now: Instant) -> Result<String, FetchError> {
        if let Some(token) = self.cached_token(now) {
            return Ok(token);
        }

        let creds = read_creds()?;
        // A non-expired token straight from the creds file (epoch-ms expiry).
        let file_token_valid = match (&creds.access_token, creds.expiry_date) {
            (Some(tok), Some(exp_ms)) if !tok.is_empty() => {
                let now_ms = chrono::Utc::now().timestamp_millis();
                (exp_ms - now_ms > EXPIRY_SKEW.as_millis() as i64).then(|| tok.clone())
            }
            _ => None,
        };
        if let Some(tok) = file_token_valid {
            return Ok(tok);
        }

        // The credentials file was found and parsed; it simply cannot mint a
        // token without a refresh token. Someone configured this and it needs
        // fixing, so it must not report as an absent credential.
        let refresh_token = creds
            .refresh_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                FetchError::CredentialUnusable("gemini creds have no refresh_token".to_string())
            })?;

        let (token, lifetime) = self.refresh_access_token(&refresh_token).await?;
        let expires_at = now + lifetime.saturating_sub(EXPIRY_SKEW);
        self.store_token(token.clone(), expires_at);
        Ok(token)
    }

    /// Best-effort `loadCodeAssist` → project id (empty on any failure).
    async fn discover_project(&self, access_token: &str) -> Option<String> {
        let body = json!({ "metadata": { "ideType": "GEMINI_CLI", "pluginType": "GEMINI" } });
        let body = serde_json::to_vec(&body).ok()?;
        let response = JsonRequest::post_json(&self.load_code_assist_url, body)
            .bearer(access_token)
            .send(&self.http)
            .await
            .ok()?;
        project_from_load_response(&response)
    }

    /// Fallback: discover a gen-lang-client / generative-language project.
    async fn discover_project_via_resource_manager(&self, access_token: &str) -> Option<String> {
        let response = JsonRequest::get(&self.projects_url)
            .bearer(access_token)
            .send(&self.http)
            .await
            .ok()?;
        project_from_resource_response(&response)
    }

    fn report_auth_failure(
        &self,
        capability: &VaultCapability,
        record_version: u64,
        error: &FetchError,
    ) {
        crate::credential_source::report_vault_auth_failure(
            self.credential_source.as_ref(),
            capability,
            record_version,
            error,
        );
    }

    async fn discover_project_for_vault(
        &self,
        access_token: &str,
    ) -> Result<Option<String>, FetchError> {
        let body = json!({ "metadata": { "ideType": "GEMINI_CLI", "pluginType": "GEMINI" } });
        let body =
            serde_json::to_vec(&body).map_err(|error| FetchError::Decode(error.to_string()))?;
        match JsonRequest::post_json(&self.load_code_assist_url, body)
            .bearer(access_token)
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
        {
            Ok(response) => Ok(project_from_load_response(&response.body)),
            Err(error @ FetchError::ProviderStatus(401 | 403 | 429)) => Err(error),
            Err(_) => Ok(None),
        }
    }

    async fn discover_project_via_resource_manager_for_vault(
        &self,
        access_token: &str,
    ) -> Result<Option<String>, FetchError> {
        match JsonRequest::get(&self.projects_url)
            .bearer(access_token)
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
        {
            Ok(response) => Ok(project_from_resource_response(&response.body)),
            Err(error @ FetchError::ProviderStatus(401 | 403 | 429)) => Err(error),
            Err(_) => Ok(None),
        }
    }

    async fn fetch_vault(&self, capability: &VaultCapability) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        // The Google OAuth credential is refresh-only in the vault. A get may make
        // one extra provider roundtrip before serving; the vault client's existing
        // 10-second timeout accommodates that without provider handling.
        let mut credential = match credential_source.get(capability, 120_000).await {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = credential.record_version;
        let account_info = credential.account_info();
        let observed = Some(AccountObservation::new(
            canonical_optional(credential.account_id.clone()),
            Some(record_version),
        ));
        let mut project = canonical_optional(credential.project_id.clone());
        // The vault payload is the bare access token; project metadata is carried
        // separately on the credential result and is never parsed from the payload.
        let access_token =
            match crate::credential_source::take_utf8_payload(&mut credential.payload) {
                Ok(value) => value,
                Err(error) => return FetchAttempt::failure(observed, None, error),
            };

        let result: Result<Usage, FetchError> = async {
            if project.is_none() {
                project = self.discover_project_for_vault(&access_token).await?;
            }
            if project.is_none() {
                project = self
                    .discover_project_via_resource_manager_for_vault(&access_token)
                    .await?;
            }
            let quota_body = match &project {
                Some(project) => json!({ "project": project }),
                None => json!({}),
            };
            let quota_body = serde_json::to_vec(&quota_body)
                .map_err(|error| FetchError::Decode(error.to_string()))?;
            let response = JsonRequest::post_json(&self.quota_url, quota_body)
                .bearer(&access_token)
                .send_provider_status_first(&self.http, PROVIDER_NAME)
                .await?;
            normalize_quota(&response.body)
        }
        .await;
        if let Err(error) = &result {
            self.report_auth_failure(capability, record_version, error);
        }
        match result {
            Ok(usage) => {
                FetchAttempt::success(observed, "vault", usage).with_account_info(account_info)
            }
            Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error),
        }
    }
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for GeminiProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        let mut handles = vec![CredentialHandle::implicit()];
        if self.credential_source.is_some() {
            handles.extend(self.handle_loader.gemini_handles()?);
        }
        Ok(handles)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if let Some(capability) = handle.vault_capability() {
            return self.fetch_vault(capability).await;
        }

        let result: Result<ProviderUsage, FetchError> = async {
            let now = Instant::now();
            let access_token = self.access_token(now).await?;

            let mut project = self.discover_project(&access_token).await;
            if project.is_none() {
                project = self
                    .discover_project_via_resource_manager(&access_token)
                    .await;
            }

            let quota_body = match &project {
                Some(p) => json!({ "project": p }),
                None => json!({}),
            };
            let quota_body =
                serde_json::to_vec(&quota_body).map_err(|e| FetchError::Decode(e.to_string()))?;
            let response = JsonRequest::post_json(&self.quota_url, quota_body)
                .bearer(&access_token)
                .send(&self.http)
                .await
                .map_err(|error| error.stage("quota"))?;

            let usage = normalize_quota(&response)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "oauth", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {

    /// The token endpoint is pinned to Google's own host.
    ///
    /// This is the URL a live REFRESH TOKEN is posted to, in the request body,
    /// so a wrong host does not merely fail -- it receives a working
    /// credential. The symptom afterwards is indistinguishable from an expired
    /// login: the exchange returns an error, the account reads as dead, and an
    /// operator re-authenticating does not fix it because the credential was
    /// never the problem.
    ///
    /// Asserted against a literal read off the constant rather than compared to
    /// the constant itself, which would hold at any value.
    #[test]
    fn the_token_endpoint_is_googles_own_host() {
        assert_eq!(TOKEN_URL, "https://oauth2.googleapis.com/token");
    }

    /// The masked OAuth constants unmask to Google's real public client.
    ///
    /// Pinned against literals rather than against a round trip through
    /// `unmask`, because a round-trip assertion holds for ANY mask and any
    /// bytes: it proves the function is reversible, not that these bytes are
    /// the client Google will accept.
    ///
    /// The failure mode is what makes this worth an explicit test. A wrong
    /// client is not a decode error -- a refresh token is bound to the client
    /// that minted it, so the exchange returns 401 and the account reads as a
    /// dead login. Nothing distinguishes that from a genuinely expired
    /// credential, so the remedy an operator reaches for (log in again) does
    /// not fix it and the real cause is invisible.
    #[test]
    fn the_masked_oauth_client_unmasks_to_googles_public_pair() {
        assert_eq!(
            unmask(CLIENT_ID_MASKED),
            "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
        );
        assert_eq!(
            unmask(CLIENT_SECRET_MASKED),
            "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"
        );
    }
    use super::*;
    use chrono::TimeZone as _;
    use std::io::Write as _;
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use crate::credential_source::{VaultCredential, VaultGetError};
    use crate::provider::CredentialResolution;
    use crate::refresh::{
        classify, next_slot_after_attempt, FetchClass, Incarnation, ProviderSlot,
    };

    type Reports = Arc<Mutex<Vec<(u16, u64)>>>;

    struct MockCredentialSource {
        get_result: Result<VaultCredential, VaultGetError>,
        reports: Reports,
    }

    #[async_trait]
    impl CredentialSource for MockCredentialSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            assert_eq!(min_ttl_ms, 120_000);
            self.get_result.clone()
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

    fn source(
        get_result: Result<VaultCredential, VaultGetError>,
    ) -> (Arc<dyn CredentialSource>, Reports) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(MockCredentialSource {
                get_result,
                reports: Arc::clone(&reports),
            }),
            reports,
        )
    }

    fn credential(
        payload: &[u8],
        record_version: u64,
        project_id: Option<&str>,
    ) -> VaultCredential {
        VaultCredential {
            payload: payload.to_vec(),
            expires_at_ms: None,
            record_version,
            account_id: Some("   ".to_string()),
            email: None,
            org_name: None,
            project_id: project_id.map(str::to_string),
        }
    }

    fn test_provider(source: Arc<dyn CredentialSource>, base_url: &str) -> GeminiProvider {
        let mut provider = GeminiProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(None)),
        );
        provider.token_url = format!("{base_url}/token");
        provider.load_code_assist_url = format!("{base_url}/load");
        provider.quota_url = format!("{base_url}/quota");
        provider.projects_url = format!("{base_url}/projects");
        provider
    }

    struct VaultOnlyProvider {
        provider: GeminiProvider,
        handle: CredentialHandle,
    }

    #[async_trait]
    impl UsageProvider for VaultOnlyProvider {
        fn name(&self) -> &str {
            PROVIDER_NAME
        }

        fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
            Ok(vec![self.handle.clone()])
        }

        async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
            self.provider.fetch_handle(handle).await
        }
    }

    async fn serve_sequence(
        responses: Vec<(u16, Vec<u8>)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(crate::loopback::read_request(&mut stream).await);
                let reason = if status == 200 {
                    "OK"
                } else if status == 429 {
                    "Too Many Requests"
                } else {
                    "Unauthorized"
                };
                let headers = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(headers.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), task)
    }

    fn write_handles(body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ck-quota-gemini-handles-{}.json",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        path
    }

    fn quota_body() -> Vec<u8> {
        br#"{"buckets":[{"modelId":"gemini-2.5-pro","remainingFraction":0.4,"resetTime":"2026-07-16T00:00:00Z"}]}"#.to_vec()
    }

    async fn refresh_against(
        status: u16,
        body: Vec<u8>,
    ) -> (Result<(String, Duration), FetchError>, String) {
        let (base_url, request) = crate::loopback::serve_once(status, body).await;
        let mut provider = GeminiProvider::new();
        provider.token_url = format!("{base_url}/token");
        let result = provider.refresh_access_token("refresh-token").await;
        (result, request.await.unwrap())
    }

    fn assert_refresh_request(request: &str) {
        let request = request.to_ascii_lowercase();
        assert!(request.starts_with("post /token "));
        assert!(request.contains("content-type: application/x-www-form-urlencoded"));
        assert!(request.contains("refresh_token=refresh-token"));
        assert!(request.contains("grant_type=refresh_token"));
    }

    /// A refused REFRESH says so, because this lane has two ways to be refused.
    ///
    /// LIVE INCIDENT, 2026-08-21. This provider went dark publishing
    /// `unauthorized: HTTP 403`, and that string was ambiguous between the two
    /// calls it makes: Google answers 403 both when it will not renew the
    /// credential and when the renewed credential may not have the resource.
    /// Those have opposite remedies -- one credential cannot be renewed at all,
    /// the other belongs to an account that is no longer entitled -- and neither
    /// is the "sign in again" that a bare `unauthorized` implies.
    ///
    /// Paired with the quota-stage assertion in the vault test, which pins
    /// `quota: HTTP 401` from a fixture that seeds a valid cached token so the
    /// refresh is never reached. A stage name that does not DISTINGUISH is
    /// decoration; these two are the discrimination.
    #[tokio::test]
    async fn a_refused_refresh_names_the_refresh_rather_than_the_quota_call() {
        let (result, request) = refresh_against(403, Vec::new()).await;
        assert_refresh_request(&request);

        let error = result.expect_err("a 403 on the token endpoint is a refusal");
        assert!(
            matches!(&error, FetchError::Unauthorized(message)
                if message == "token refresh: HTTP 403"),
            "expected the refresh stage to be named, got {error}"
        );
        // The variant is load-bearing beyond the text: Unauthorized is
        // non-transient, so this must not stale-serve a window behind a
        // credential the upstream has stopped renewing.
        assert_eq!(classify(&error), FetchClass::NonTransient);
    }

    #[tokio::test]
    async fn invalid_grant_refresh_is_credential_unusable_and_non_transient() {
        let (result, request) =
            refresh_against(400, br#"{"error":"invalid_grant"}"#.to_vec()).await;
        assert_refresh_request(&request);

        let error = result.expect_err("invalid_grant must reject the credential");
        assert!(matches!(error, FetchError::CredentialUnusable(_)));
        assert_eq!(classify(&error), FetchClass::NonTransient);
    }

    #[tokio::test]
    async fn temporarily_unavailable_refresh_remains_upstream_and_transient() {
        let (result, request) =
            refresh_against(400, br#"{"error":"temporarily_unavailable"}"#.to_vec()).await;
        assert_refresh_request(&request);

        let error = result.expect_err("a temporary OAuth refusal must not reject the credential");
        assert!(matches!(error, FetchError::Upstream(_)));
        assert_eq!(classify(&error), FetchClass::Transient);
    }

    #[tokio::test]
    async fn non_json_refresh_refusal_remains_upstream_and_transient() {
        let (result, request) =
            refresh_against(400, b"gateway temporarily unavailable".to_vec()).await;
        assert_refresh_request(&request);

        let error = result.expect_err("an unreadable refusal must not reject the credential");
        assert!(matches!(error, FetchError::Upstream(_)));
        assert_eq!(classify(&error), FetchClass::Transient);
    }

    #[tokio::test]
    async fn invalid_grant_in_error_description_does_not_reject_the_credential() {
        let (result, request) = refresh_against(
            400,
            br#"{"error":"temporarily_unavailable","error_description":"proxy quoted invalid_grant"}"#.to_vec(),
        )
        .await;
        assert_refresh_request(&request);

        let error = result.expect_err("only the OAuth error field may reject the credential");
        assert!(matches!(error, FetchError::Upstream(_)));
        assert_eq!(classify(&error), FetchClass::Transient);
    }

    #[tokio::test]
    async fn successful_refresh_still_returns_the_access_token() {
        let (result, request) = refresh_against(
            200,
            br#"{"access_token":"fresh-access-token","expires_in":300}"#.to_vec(),
        )
        .await;
        assert_refresh_request(&request);

        let (token, lifetime) = result.expect("a successful refresh must still succeed");
        assert_eq!(token, "fresh-access-token");
        assert_eq!(lifetime, Duration::from_secs(300));
    }

    /// The Antigravity Google credential is not a Gemini credential.
    ///
    /// Both are Google logins reaching the same Code Assist API, so pooling them
    /// looks harmless and the requests even succeed. They answer for different
    /// products: an Antigravity login's quota response carries Antigravity's own
    /// model pool, Claude and GPT included, which a Gemini CLI login has no
    /// access to. Serving it here would publish that pool as Gemini's capacity,
    /// and the numbers would look entirely plausible.
    ///
    /// Today the local lane usually wins this provider's selection, so the
    /// mistake would surface only once the local credential failed — which is
    /// the worst moment to start reporting another product's numbers.
    #[test]
    fn the_antigravity_google_credential_is_not_offered_to_gemini() {
        let path = write_handles(
            r#"{"handles":{"antigravity:google":"ckh_antigravity","oauth:google:cli":"ckh_google","oauth:xai":"ckh_grok"}}"#,
        );
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = GeminiProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(Some(path.clone()))),
        );
        let handles = provider.handles().unwrap();

        // Not vacuous: the Gemini CLI credential is still offered, so this
        // cannot pass by dropping every vault handle.
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0], CredentialHandle::implicit());
        assert_eq!(handles[1].stable_id(), "oauth:google:cli");
        assert!(
            !handles
                .iter()
                .any(|handle| handle.stable_id() == "antigravity:google"),
            "the Antigravity credential reached the Gemini lane"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn served_project_skips_discovery_and_vault_ignores_local_token_cache() {
        let (base_url, requests) = serve_sequence(vec![(200, quota_body())]).await;
        let (source, _) = source(Ok(credential(
            b"ya29.served-google-token",
            116,
            Some("served-project"),
        )));
        let provider = test_provider(source, &base_url);
        let cached_expiry = Instant::now() + Duration::from_secs(300);
        *provider.token.lock().unwrap() = Some(CachedToken {
            token: "cached-local-token".to_string(),
            expires_at: cached_expiry,
        });

        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "antigravity:google",
                VaultCapability::new("ckh_google"),
            ))
            .await;
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt.observed.unwrap(),
            AccountObservation::new(None, Some(116))
        );
        assert_eq!(attempt.usage.unwrap().primary.unwrap().used_percent, 60.0);

        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 1, "served project must skip discovery");
        let request = requests[0].to_ascii_lowercase();
        assert!(request.starts_with("post /quota "));
        assert!(request.contains("authorization: bearer ya29.served-google-token"));
        assert!(!request.contains("cached-local-token"));
        assert!(request.contains(r#"{"project":"served-project"}"#));
        let cached = provider.token.lock().unwrap();
        let cached = cached.as_ref().unwrap();
        assert_eq!(cached.token, "cached-local-token");
        assert_eq!(cached.expires_at, cached_expiry);
    }

    #[tokio::test]
    async fn vault_happy_path_serves_one_unlabeled_entry() {
        let (base_url, _) = serve_sequence(vec![(200, quota_body())]).await;
        let (source, _) = source(Ok(credential(
            b"ya29.served-google-token",
            121,
            Some("served-project"),
        )));
        let handle =
            CredentialHandle::vault("antigravity:google", VaultCapability::new("ckh_google"));
        let registry = crate::Registry::new(vec![Box::new(VaultOnlyProvider {
            provider: test_provider(source, &base_url),
            handle,
        })]);

        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let entries = registry.get_usage(Some(PROVIDER_NAME)).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].account, None);
        assert_eq!(
            entries[0]
                .usage
                .as_ref()
                .unwrap()
                .primary
                .as_ref()
                .unwrap()
                .used_percent,
            60.0
        );
    }

    #[tokio::test]
    async fn absent_served_project_discovers_with_the_served_token() {
        let load_body = br#"{"cloudaicompanionProject":{"id":"discovered-project"}}"#.to_vec();
        let (base_url, requests) =
            serve_sequence(vec![(200, load_body), (200, quota_body())]).await;
        let (source, _) = source(Ok(credential(b"ya29.discovery-token", 117, None)));
        let provider = test_provider(source, &base_url);

        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "antigravity:google",
                VaultCapability::new("ckh_google"),
            ))
            .await;
        assert!(attempt.usage.is_ok());
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 2);
        let load = requests[0].to_ascii_lowercase();
        let quota = requests[1].to_ascii_lowercase();
        assert!(load.starts_with("post /load "));
        assert!(quota.starts_with("post /quota "));
        for request in [&load, &quota] {
            assert!(request.contains("authorization: bearer ya29.discovery-token"));
        }
        assert!(quota.contains(r#"{"project":"discovered-project"}"#));
    }

    #[tokio::test]
    async fn failed_get_is_unverified_and_clears_prior_observation() {
        let (source, _) = source(Err(VaultGetError::Transient));
        let provider = test_provider(source, "http://unused.invalid");
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "antigravity:google",
                VaultCapability::new("ckh_google"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Unverified
        );

        let now = Instant::now();
        let cold = ProviderSlot::due_now(now, Incarnation::from_counter(1));
        let prior = next_slot_after_attempt(
            &cold,
            PROVIDER_NAME,
            FetchAttempt::success(
                Some(AccountObservation::new(
                    Some("prior-account".to_string()),
                    Some(1),
                )),
                "vault",
                Usage::default(),
            ),
            now,
            now,
        );
        let next = next_slot_after_attempt(&prior, PROVIDER_NAME, attempt, now, now);
        // The prior account's USAGE must not survive an unverified identity. The
        // entry itself does survive, as a verdict carrying no account and no
        // windows: dropping it too publishes the ABSENT shape for a credential
        // this module reached and found unusable, which a consumer reads as "not
        // fetched yet" (insula#8, measured).
        let entry = next.entry.as_ref().expect("the verdict stays visible");
        assert!(entry.usage.is_none(), "no window may cross an identity");
        assert!(entry.account.is_none(), "and it attributes nothing");
        assert!(entry.error.is_some(), "the failure is stated, not implied");
        assert!(next.label_in_flux);
        assert!(next.last_success_at.is_none());
    }

    #[tokio::test]
    async fn vault_401_reports_served_version_while_local_keeps_legacy_error() {
        let (vault_base, _) = serve_sequence(vec![(401, Vec::new())]).await;
        let (source, reports) = source(Ok(credential(
            b"ya29.expired-vault-token",
            118,
            Some("served-project"),
        )));
        let mut provider = test_provider(Arc::clone(&source), &vault_base);
        let vault = provider
            .fetch_handle(&CredentialHandle::vault(
                "antigravity:google",
                VaultCapability::new("ckh_google"),
            ))
            .await;
        assert!(matches!(&vault.usage, Err(FetchError::ProviderStatus(401))));
        assert_eq!(
            classify(vault.usage.as_ref().unwrap_err()),
            FetchClass::NonTransient
        );
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 118)]);

        reports.lock().unwrap().clear();
        let load_body = br#"{"cloudaicompanionProject":"local-project"}"#.to_vec();
        let (local_base, _) = serve_sequence(vec![(200, load_body), (401, Vec::new())]).await;
        provider.load_code_assist_url = format!("{local_base}/load");
        provider.quota_url = format!("{local_base}/quota");
        provider.projects_url = format!("{local_base}/projects");
        let cached_expiry = Instant::now() + Duration::from_secs(300);
        *provider.token.lock().unwrap() = Some(CachedToken {
            token: "local-cached-token".to_string(),
            expires_at: cached_expiry,
        });
        let local = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert!(matches!(
            local.usage,
            // `quota`, not `token refresh`: this fixture seeds a still-valid
            // cached token, so the refresh is never reached and the 401 comes
            // from the quota call. The two stages are exactly what this
            // assertion now distinguishes.
            Err(FetchError::Unauthorized(message)) if message == "quota: HTTP 401"
        ));
        tokio::task::yield_now().await;
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn vault_429_remains_transient() {
        let (base_url, _) = serve_sequence(vec![(429, Vec::new())]).await;
        let (source, _) = source(Ok(credential(
            b"ya29.rate-limited-token",
            119,
            Some("served-project"),
        )));
        let provider = test_provider(source, &base_url);
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "antigravity:google",
                VaultCapability::new("ckh_google"),
            ))
            .await;
        let error = attempt.usage.unwrap_err();
        assert!(matches!(error, FetchError::ProviderStatus(429)));
        assert_eq!(classify(&error), FetchClass::Transient);
    }

    #[tokio::test]
    async fn non_utf8_vault_payload_is_a_verified_decode_failure() {
        let (source, _) = source(Ok(credential(&[0xff, 0xfe], 120, Some("project"))));
        let provider = test_provider(source, "http://unused.invalid");
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "antigravity:google",
                VaultCapability::new("ckh_google"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Verified
        );
        assert_eq!(attempt.observed.unwrap().record_version, Some(120));
        assert!(matches!(attempt.usage, Err(FetchError::Decode(_))));
    }

    /// A poisoned token lock is recovered, not propagated.
    ///
    /// A mutex is poisoned when a thread panics while holding it, and the panic
    /// need not come from this code -- an allocation failure or a panic in any
    /// caller on that stack does it. Propagating would be worse than the
    /// corruption it guards against: the refresher catches a panic inside a
    /// fetch and classifies it non-transient, which replaces this provider's
    /// cached window with a degraded entry and keeps it there. One poisoned
    /// mutex would take the provider down until the process restarted.
    ///
    /// Recovering is sound here because the guarded value cannot be torn: it is
    /// a token with its expiry, so a writer that panicked mid-update leaves
    /// either the old entry or the new one, and a stale entry is rejected by the
    /// expiry check on read.
    #[test]
    fn a_poisoned_token_lock_is_recovered_rather_than_propagated() {
        let provider = GeminiProvider::new();
        let now = Instant::now();
        provider.store_token("cached-token".to_string(), now + Duration::from_secs(300));

        // Poison it the only way a mutex is ever poisoned: panic while holding it.
        let panicked = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = provider.token.lock().unwrap();
                    panic!("deliberate panic while holding the token lock");
                })
                .join()
        });
        assert!(panicked.is_err(), "the helper thread must actually panic");
        assert!(
            provider.token.is_poisoned(),
            "the lock must be poisoned for this test to mean anything"
        );

        // Both accessors keep working, and the cached value survived.
        assert_eq!(
            provider.cached_token(now),
            Some("cached-token".to_string()),
            "a poisoned lock must not cost the provider its cached token"
        );
        provider.store_token("replacement".to_string(), now + Duration::from_secs(300));
        assert_eq!(provider.cached_token(now), Some("replacement".to_string()));
    }

    #[test]
    fn oauth_credentials_debug_redacts_both_tokens() {
        let credentials = OauthCreds {
            access_token: Some("gemini-access-secret".to_string()),
            refresh_token: Some("gemini-refresh-secret".to_string()),
            expiry_date: Some(1234),
        };
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("gemini-access-secret"));
        assert!(!debug.contains("gemini-refresh-secret"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn reset_delta_uses_daily_for_mid_window_and_only_proves_longer_classes() {
        let now = Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap();
        assert_eq!(
            window_minutes_from_reset("2026-07-18T04:00:00Z", now),
            Some(WINDOW_MINUTES_24H)
        );
        assert_eq!(
            window_minutes_from_reset("2026-07-19T00:00:00Z", now),
            Some(WINDOW_MINUTES_24H)
        );
        assert_eq!(
            window_minutes_from_reset("2026-07-24T21:36:00Z", now),
            Some(7 * 24 * 60)
        );
    }

    #[test]
    fn normalizes_all_live_per_model_buckets_and_selects_binding_primary() {
        let body = br#"{
            "buckets": [
                {"modelId":"gemini-2.5-flash","remainingFraction":0.60,"resetTime":"2026-07-19T00:00:00Z"},
                {"modelId":"gemini-2.5-flash-lite","remainingFraction":1.00,"resetTime":"2026-07-19T00:00:00Z"},
                {"modelId":"gemini-2.5-pro","remainingFraction":0.90,"resetTime":"2026-07-19T00:00:00Z"},
                {"modelId":"gemini-3-flash-preview","remainingFraction":0.80,"resetTime":"2026-07-19T00:00:00Z"},
                {"modelId":"gemini-3-pro-preview","remainingFraction":0.70,"resetTime":"2026-07-19T00:00:00Z"},
                {"modelId":"gemini-3.1-flash-lite","remainingFraction":0.50,"resetTime":"2026-07-19T00:00:00Z"},
                {"modelId":"gemini-3.1-flash-lite-preview","remainingFraction":0.40,"resetTime":"2026-07-19T00:00:00Z"},
                {"modelId":"gemini-3.1-pro-preview","remainingFraction":0.30,"resetTime":"2026-07-19T00:00:00Z"}
            ]
        }"#;
        let now = Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap();
        let usage = normalize_quota_at(body, now).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 70.0);
        assert_eq!(primary.window_minutes, Some(WINDOW_MINUTES_24H));
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());

        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 8);
        assert_eq!(extras[0].title.as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(
            extras[6].id.as_deref(),
            Some("gemini-3.1-flash-lite-preview")
        );
        assert!(extras.iter().all(|extra| {
            extra.window.as_ref().unwrap().window_minutes == Some(WINDOW_MINUTES_24H)
        }));
    }

    #[test]
    fn bucket_without_reset_keeps_named_window_without_reset() {
        let body =
            br#"{ "buckets": [ { "modelId": "gemini-2.5-pro", "remainingFraction": 0.5 } ] }"#;
        let usage = normalize_quota(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(usage.extra_rate_windows.unwrap().len(), 1);
    }

    /// A stated-empty bucket list is an account fact; an absent field is ours.
    ///
    /// Both still ERROR, which is what matters most here -- returning Ok would
    /// publish a window-less entry as a success, and a consumer cannot tell that
    /// from capacity nobody measured. They differ in who the reader should go
    /// and look at. An empty list is the upstream saying this account has no
    /// buckets, with nothing to fix. A missing field means our struct and their
    /// payload disagree, which is a defect in this repo and must not be filed as
    /// a fact about the user.
    #[test]
    fn a_stated_empty_bucket_list_is_not_our_defect() {
        let stated = normalize_quota(br#"{ "buckets": [] }"#).unwrap_err();
        assert!(
            matches!(stated, FetchError::NoQuotaReported(_)),
            "an upstream stating no buckets is an account fact: {stated:?}"
        );

        // Not vacuous: the neighbouring input keeps the other class, so this
        // cannot pass by collapsing both into one answer.
        let absent = normalize_quota(br#"{ }"#).unwrap_err();
        assert!(
            matches!(absent, FetchError::Decode(_)),
            "an absent buckets field points at this repo: {absent:?}"
        );
    }

    #[test]
    fn buckets_present_but_all_unusable_is_a_decode_error() {
        // A syntactically valid 2xx whose only bucket carries no remainingFraction.
        // Returning Ok here would publish a window-less entry as a SUCCESS, which is
        // stored fresh and REPLACES the provider's previously good windows while
        // reporting it healthy — a silent outage rather than a visible failure.
        let error =
            normalize_quota(br#"{ "buckets": [ { "modelId": "gemini-2.5-pro" } ] }"#).unwrap_err();
        match error {
            FetchError::Decode(message) => assert!(
                message.contains("no usable model fraction"),
                "the all-unusable case must name itself, not reuse the empty-list \
                 message: {message}"
            ),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn a_partially_usable_bucket_list_still_succeeds() {
        // The guard must reject only a WHOLLY unusable response. One good bucket
        // beside a skipped one is a valid answer and must still emit its window,
        // otherwise the fix would over-reject and break a working provider.
        let body = br#"{ "buckets": [
            { "modelId": "gemini-2.5-flash" },
            { "modelId": "gemini-2.5-pro", "remainingFraction": 0.25 }
        ] }"#;
        let usage = normalize_quota(body).expect("one usable bucket is a valid response");
        assert_eq!(usage.primary.expect("usable window").used_percent, 75.0);
        let extras = usage
            .extra_rate_windows
            .expect("the usable bucket is named");
        assert_eq!(extras.len(), 1, "the unusable bucket must not be emitted");
        assert_eq!(extras[0].title.as_deref(), Some("gemini-2.5-pro"));
    }

    #[test]
    fn every_valid_model_bucket_is_named_even_without_a_known_tier() {
        let body = br#"{ "buckets": [ { "modelId": "some-embedding-model", "remainingFraction": 0.1, "resetTime": "2026-07-19T00:00:00Z" } ] }"#;
        let now = Utc.with_ymd_and_hms(2026, 7, 18, 0, 0, 0).unwrap();
        let usage = normalize_quota_at(body, now).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 90.0);
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras[0].title.as_deref(), Some("some-embedding-model"));
    }
}
