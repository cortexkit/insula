//! Muse Code (Meta) — a subscription with a 5-hour and a weekly window.
//!
//! - Endpoint: `POST https://api.meta.ai/muse-code/key`, body `{}`, with the
//!   Muse CLI's device-code access token as the bearer. Only that token works:
//!   it starts with `dca:`, and Meta dashboard `LLM_` keys and minted `LLM|`
//!   inference keys are refused with 401.
//! - The answer states the plan (`subs_tier_name`), the login's email
//!   (`user_email`), whether a subscription is active, whether billing is
//!   incomplete, and `subs_usage` with the two windows.
//! - Reference: CodexBar v0.79.0, `Sources/CodexBarCore/Resources/Plugins/muse.ts`
//!   and `docs/muse.md` in https://github.com/steipete/CodexBar. Insula's lane
//!   contract is documented in `docs/consumer-contract.md` in this repository.
//!
//! The same response also carries the account's inference key (`api_key`) and
//! its `payment_method`. Neither is declared in [`KeyResponse`], so serde drops
//! them unread; nothing here may log or publish the raw body.
//!
//! CodexBar's docs call this a "mint" endpoint, which raised the question of
//! whether polling it creates a new key each time. It doesn't: two calls two
//! seconds apart on a live account (2026-10-13) returned the same `api_key` and
//! identical usage fields; only the signed `user_avatar_url` changed. So the
//! ordinary poll interval applies. If that ever changes, every poll would add a
//! key to the account, and the interval for this provider would need raising.
//!
//! Credential: a vault deposit (`apikey:muse`) when one exists, otherwise
//! `MUSE_DEVICE_TOKEN`, otherwise an inline `providers.meta.access_token` in the
//! Muse CLI's metadata file (`MUSE_AUTH_PATH`, default `~/.config/muse/auth.json`).
//! When the CLI keeps the token in the macOS Keychain the file holds only
//! metadata, and this module does not read the Keychain: reading another
//! program's Keychain item would prompt the user from a background daemon.
//! An optional `cookie:dev.meta.ai` vault deposit and explicit `muse.team_id`
//! can fill an idle response after a restart. They never create a usage lane;
//! the mint response still identifies the account and plan.

use async_trait::async_trait;
use serde::Deserialize;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::credential_source::CredentialSource;
use crate::http::{Header, JsonRequest};
use crate::model::{AccountInfo, RateWindow, Usage};
use crate::provider::{
    AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
};
use crate::vault_handles::VaultHandleLoader;

mod web;

pub const PROVIDER_NAME: &str = "muse";
const KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const WEB_URL: &str = "https://dev.meta.ai";
const TOKEN_ENV: &str = "MUSE_DEVICE_TOKEN";
const AUTH_PATH_ENV: &str = "MUSE_AUTH_PATH";
const DEVICE_TOKEN_PREFIX: &str = "dca:";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;

type EnvLookup = Arc<dyn Fn(&str) -> Option<OsString> + Send + Sync>;

#[cfg(not(test))]
fn default_env() -> EnvLookup {
    Arc::new(|key| std::env::var_os(key).filter(|value| !value.is_empty()))
}

/// Registry tests build production providers too. They get an empty
/// environment and a home that doesn't exist, so no test reads the developer's
/// real Muse login.
#[cfg(test)]
fn default_env() -> EnvLookup {
    let home = std::env::temp_dir().join(format!("insula-absent-muse-{}", std::process::id()));
    Arc::new(move |key| match key {
        "HOME" | "USERPROFILE" => Some(home.clone().into_os_string()),
        _ => None,
    })
}

#[derive(Debug, Deserialize)]
struct KeyResponse {
    #[serde(default)]
    require_payment: Option<bool>,
    #[serde(default)]
    is_subs_active: Option<bool>,
    #[serde(default)]
    subs_tier_name: Option<String>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    subs_usage: Option<SubsUsage>,
}

#[derive(Debug, Deserialize)]
struct SubsUsage {
    window: UsageWindow,
    weekly: UsageWindow,
}

#[derive(Debug, Deserialize)]
struct UsageWindow {
    used_percent: f64,
    #[serde(default)]
    window_duration_mins: Option<f64>,
    #[serde(default)]
    resets_at: Option<f64>,
}

/// What one `muse-code/key` answer says about the account.
#[derive(Debug, PartialEq)]
struct Reading {
    /// `None` when Meta left `subs_usage` out: the 5-hour window is idle.
    usage: Option<Usage>,
    email: Option<String>,
    plan: Option<String>,
}

fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// Unix seconds to RFC 3339. A reset outside a plausible range is dropped and
/// the window kept: its percentage is still true.
fn reset_at(seconds: Option<f64>) -> Option<String> {
    let seconds = seconds?;
    if !seconds.is_finite() || seconds <= 0.0 || seconds > 64_092_211_200.0 {
        return None;
    }
    crate::env::epoch_to_iso8601(seconds as i64)
}

fn window(
    used_percent: f64,
    minutes: i64,
    kind: Option<&str>,
    resets_at: Option<f64>,
) -> RateWindow {
    RateWindow {
        window_kind: kind.map(str::to_string),
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: reset_at(resets_at),
        window_minutes: Some(minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    }
}

/// Normalize a 200 answer. Pure, so every branch is testable without a server.
fn parse(body: &[u8]) -> Result<Reading, FetchError> {
    let response: KeyResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("muse-code/key: {error}")))?;
    if response.require_payment == Some(true) {
        return Err(FetchError::NoQuotaReported(
            "Muse Code billing is incomplete: add a payment method at https://dev.meta.ai"
                .to_string(),
        ));
    }
    if response.is_subs_active != Some(true) {
        return Err(FetchError::NoQuotaReported(
            "no Muse Code subscription is active on this login".to_string(),
        ));
    }
    let email = trimmed(response.user_email);
    let plan = trimmed(response.subs_tier_name);
    let Some(subs) = response.subs_usage else {
        // Meta leaves `subs_usage` out (or null) while the 5-hour window is idle,
        // even when the weekly window has usage. The caller decides what that
        // means, because only it remembers the account's last weekly figure.
        return Ok(Reading {
            usage: None,
            email,
            plan,
        });
    };
    for (field, value) in [
        ("window.used_percent", subs.window.used_percent),
        ("weekly.used_percent", subs.weekly.used_percent),
    ] {
        if !value.is_finite() {
            return Err(FetchError::Decode(format!(
                "muse-code/key: {field} is not finite"
            )));
        }
    }
    let minutes = subs
        .window
        .window_duration_mins
        .filter(|minutes| minutes.is_finite() && *minutes >= 1.0)
        .map(|minutes| minutes.round() as i64)
        .ok_or_else(|| FetchError::Decode("muse-code/key: window_duration_mins".to_string()))?;
    // `five_hour` names the cadence, so it's stated only when the duration says so.
    let short_kind = (minutes == 300).then_some(cortexkit_provider_usage::window_kind::FIVE_HOUR);
    let usage = Usage {
        primary: Some(window(
            subs.window.used_percent,
            minutes,
            short_kind,
            subs.window.resets_at,
        )),
        secondary: Some(window(
            subs.weekly.used_percent,
            WEEKLY_MINUTES,
            Some(cortexkit_provider_usage::window_kind::WEEKLY),
            subs.weekly.resets_at,
        )),
        tertiary: None,
        extra_rate_windows: None,
    };
    Ok(Reading {
        usage: Some(usage),
        email,
        plan,
    })
}

/// The reading to publish when Meta omitted usage because the 5-hour window is
/// idle, built from the account's last full reading.
///
/// Both windows are known without being sent. The 5-hour window is idle, so it
/// is 0% with no reset pending. The weekly figure can't have moved since the
/// last full reading: any request would have opened a new 5-hour window, and an
/// open window makes Meta send `subs_usage` again. So the last weekly figure is
/// still the current one, until its own reset passes, after which it's 0% with
/// the next reset unknown. With no earlier full reading (just after a restart)
/// there's nothing to build from, so the caller may try the deposited web session.
fn idle_usage(last: &Usage, now: chrono::DateTime<chrono::Utc>) -> Option<Usage> {
    let short = last.primary.as_ref()?;
    let weekly = last.secondary.as_ref()?;
    let weekly_reset_passed = weekly
        .resets_at
        .as_deref()
        .and_then(|reset| chrono::DateTime::parse_from_rfc3339(reset).ok())
        .is_some_and(|reset| reset <= now);
    Some(Usage {
        primary: Some(RateWindow {
            used_percent: 0.0,
            resets_at: None,
            ..short.clone()
        }),
        secondary: Some(if weekly_reset_passed {
            RateWindow {
                used_percent: 0.0,
                resets_at: None,
                ..weekly.clone()
            }
        } else {
            weekly.clone()
        }),
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The device token inside a vault deposit: the bare `dca:` string, or a JSON
/// object carrying it as `access_token` (the shape the CLI keeps in its
/// Keychain item).
fn token_from_payload(payload: &str) -> Option<String> {
    let payload = payload.trim();
    if payload.starts_with('{') {
        let value: serde_json::Value = serde_json::from_str(payload).ok()?;
        return find_access_token(&value);
    }
    Some(payload.to_string())
}

fn find_access_token(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(token)) = map.get("access_token") {
                return Some(token.trim().to_string());
            }
            map.values().find_map(find_access_token)
        }
        _ => None,
    }
}

/// The Muse CLI metadata file, `providers.meta`.
#[derive(Debug, Deserialize)]
struct AuthFile {
    #[serde(default)]
    providers: Option<AuthProviders>,
}

#[derive(Debug, Deserialize)]
struct AuthProviders {
    #[serde(default)]
    meta: Option<MetaLogin>,
}

#[derive(Debug, Deserialize)]
struct MetaLogin {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    storage: Option<String>,
}

pub struct MuseProvider {
    url: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    env: EnvLookup,
    /// Each account's last full reading, keyed by Meta's `user_email`, so an
    /// idle answer can be filled in (see [`idle_usage`]). Process memory only.
    last_full: std::sync::Mutex<std::collections::HashMap<String, Usage>>,
    web: web::TeamQuota,
}

impl MuseProvider {
    pub(crate) fn new_with_handle_loader(
        config: crate::config::LiveQuotaConfig,
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            url: KEY_URL.to_string(),
            http: crate::http::provider_client(),
            web: web::TeamQuota::new(config, credential_source.clone(), handle_loader.clone()),
            credential_source,
            handle_loader,
            env: default_env(),
            last_full: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn remember(&self, email: &str, usage: &Usage) {
        if let Ok(mut last) = self.last_full.lock() {
            last.insert(email.to_string(), usage.clone());
        }
    }

    fn auth_path(&self) -> Result<PathBuf, FetchError> {
        if let Some(path) = (self.env)(AUTH_PATH_ENV) {
            return Ok(PathBuf::from(path));
        }
        let env = Arc::clone(&self.env);
        let home = crate::env::home_dir_from(move |key| env(key)).ok_or_else(|| {
            FetchError::LocalSourceUnavailable(
                "home directory unresolved, so the Muse CLI login can't be located".to_string(),
            )
        })?;
        Ok(home.join(".config/muse/auth.json"))
    }

    /// The device token from the environment or the Muse CLI's metadata file.
    fn local_token(&self) -> Result<String, FetchError> {
        if let Some(token) = (self.env)(TOKEN_ENV) {
            return Ok(token.to_string_lossy().trim().to_string());
        }
        let path = self.auth_path()?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(FetchError::NoSession(
                    "no MUSE_DEVICE_TOKEN and no Muse CLI login (run `muse login`)".to_string(),
                ));
            }
            // Couldn't look is not "nobody signed in": an absent class would let a
            // consumer drop the account.
            Err(error) => {
                return Err(FetchError::LocalSourceUnavailable(format!(
                    "Muse CLI login file unreadable: {error}"
                )));
            }
        };
        let file: AuthFile = serde_json::from_str(&text).map_err(|error| {
            FetchError::CredentialUnusable(format!("Muse CLI login file did not parse: {error}"))
        })?;
        let Some(login) = file.providers.and_then(|providers| providers.meta) else {
            return Err(FetchError::NoSession(
                "the Muse CLI login file holds no Meta login (run `muse login`)".to_string(),
            ));
        };
        if let Some(token) = trimmed(login.access_token) {
            return Ok(token);
        }
        let storage = login
            .storage
            .unwrap_or_else(|| "an external store".to_string());
        Err(FetchError::CredentialUnusable(format!(
            "the Muse CLI keeps its token in {storage}, which insula does not read; \
             deposit the token in the vault as apikey:muse"
        )))
    }

    async fn request(&self, token: &str, vault: bool) -> Result<Vec<u8>, FetchError> {
        let request = JsonRequest::post_json(&self.url, b"{}".to_vec())
            .bearer(token)
            .header(Header::new("x-api-version", "1.0.0"))
            .header(Header::new("Content-Type", "application/json"))
            .timeout(REQUEST_TIMEOUT);
        // Only the vault lane keeps the numeric status, which the shared gate
        // needs to report a 401 to the vault; the local lane has nobody to tell.
        if vault {
            request
                .send_provider_status_first(&self.http, PROVIDER_NAME)
                .await
                .map(|response| response.body)
        } else {
            request.send(&self.http).await
        }
    }

    async fn fetch_with_token(
        &self,
        token: &str,
        source: &str,
        record_version: Option<u64>,
        vault: bool,
    ) -> (FetchAttempt, Option<FetchError>) {
        if !token.starts_with(DEVICE_TOKEN_PREFIX) {
            // A dashboard `LLM_` key or a minted `LLM|` inference key: Meta refuses
            // both on this endpoint, so asking would only record a rejection.
            let error = FetchError::CredentialUnusable(
                "the Muse credential is not a device-code login token (dca:…); \
                 run `muse login` and use its token"
                    .to_string(),
            );
            return (
                FetchAttempt::failure(None, Some(source.to_string()), error),
                None,
            );
        }
        match self
            .request(token, vault)
            .await
            .and_then(|body| parse(&body))
        {
            Ok(reading) => {
                let observed = reading
                    .email
                    .clone()
                    .map(|email| AccountObservation::new(Some(email), record_version));
                let info = reading.email.clone().map(|email| AccountInfo {
                    email: Some(email),
                    org_name: None,
                    plan_type: reading.plan.clone(),
                    subscription_renews_at: None,
                    subscription_ends_at: None,
                });
                let usage = match (reading.usage, reading.email.as_deref()) {
                    (Some(usage), email) => {
                        if let Some(email) = email {
                            self.remember(email, &usage);
                        }
                        usage
                    }
                    (None, email) => {
                        let idle = email.and_then(|email| {
                            let last = self.last_full.lock().ok()?.get(email).cloned()?;
                            idle_usage(&last, chrono::Utc::now())
                        });
                        let idle = match idle {
                            Some(idle) => Some(idle),
                            None => {
                                let web = self.web.fetch(email, reading.plan.as_deref()).await;
                                // A verified team reading is a full reading: remember
                                // it, so the next idle answer fills from memory
                                // instead of calling dev.meta.ai again every minute.
                                if let (Some(email), Some(usage)) = (email, web.as_ref()) {
                                    self.remember(email, usage);
                                }
                                web
                            }
                        };
                        let Some(idle) = idle else {
                            // Nothing to build from, so keep the last served
                            // reading (if any) with a transient class, rather
                            // than publish a weekly figure nobody sent.
                            let error = FetchError::Upstream(
                                "Muse Code omitted usage, which Meta does while the 5-hour \
                                 window is idle, and neither a remembered reading nor a verified \
                                 dev.meta.ai team quota is available"
                                    .to_string(),
                            );
                            return (
                                FetchAttempt::failure(observed, Some(source.to_string()), error)
                                    .with_account_info(info),
                                None,
                            );
                        };
                        idle
                    }
                };
                (
                    FetchAttempt::success(observed, source, usage).with_account_info(info),
                    None,
                )
            }
            Err(error) => {
                let reportable = matches!(error, FetchError::ProviderStatus(..))
                    .then(|| FetchError::ProviderStatus(status_of(&error), String::new()));
                (
                    FetchAttempt::failure(None, Some(source.to_string()), error),
                    reportable,
                )
            }
        }
    }

    async fn fetch_vault(&self, handle: &CredentialHandle) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match crate::credential_source::get_vault_credential(
            credential_source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = credential.record_version;
        let payload = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(payload) => payload,
            Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
        };
        let Some(token) = token_from_payload(&payload) else {
            return FetchAttempt::failure(
                None,
                Some("vault".to_string()),
                FetchError::CredentialUnusable(
                    "the apikey:muse deposit is JSON without an access_token".to_string(),
                ),
            );
        };
        let (attempt, reportable) = self
            .fetch_with_token(&token, "vault", Some(record_version), true)
            .await;
        if let Some(error) = reportable {
            crate::credential_source::report_vault_auth_failure(
                Some(credential_source),
                handle,
                record_version,
                &error,
            );
        }
        attempt
    }
}

fn status_of(error: &FetchError) -> u16 {
    match error {
        FetchError::ProviderStatus(code, _) => *code,
        _ => 0,
    }
}

#[async_trait]
impl UsageProvider for MuseProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.muse_handles()?;
            if !vault.is_empty() {
                // A vault deposit replaces the local lanes. Keeping both would put
                // two slots on one account, and the local one can't be read on a
                // host whose CLI keeps its token in the Keychain.
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }
        let token = match self.local_token() {
            Ok(token) => token,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        self.fetch_with_token(&token, "api", None, false).await.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_source::{VaultCapability, VaultCredential, VaultGetError};
    use std::sync::Mutex;

    const FULL: &str = r#"{
        "require_payment": false,
        "is_subs_active": true,
        "subs_tier_name": "Muse Code Power Usage",
        "user_email": " person@example.com ",
        "key": "LLM|secret-inference-key",
        "payment_method": {"brand": "visa", "last4": "4242"},
        "subs_usage": {
            "window": {"used_percent": 37.5, "window_duration_mins": 300, "resets_at": 1791000000},
            "weekly": {"used_percent": 12.25, "resets_at": 1791500000}
        }
    }"#;

    fn env_with(vars: &[(&str, &str)]) -> EnvLookup {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(key, value)| (key.to_string(), OsString::from(value)))
            .collect();
        Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
    }

    fn provider(env: EnvLookup) -> MuseProvider {
        let mut provider = MuseProvider::new_with_handle_loader(
            crate::config::QuotaConfig::default().into(),
            None,
            Arc::new(VaultHandleLoader::default()),
        );
        provider.env = env;
        provider
    }

    /// A per-test scratch directory, removed when the returned guard drops.
    fn scratch(name: &str) -> crate::tests::ResetTempDir {
        crate::tests::ResetTempDir::with_prefix("insula-muse", name)
    }

    #[test]
    fn a_full_answer_publishes_both_windows_the_plan_and_the_email() {
        let reading = parse(FULL.as_bytes()).unwrap();
        let usage = reading.usage.as_ref().unwrap();
        let primary = usage.primary.as_ref().unwrap();
        assert_eq!(primary.used_percent, 37.5);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.window_kind.as_deref(), Some("five_hour"));
        assert_eq!(primary.resets_at.as_deref(), Some("2026-10-03T04:00:00Z"));
        let weekly = usage.secondary.as_ref().unwrap();
        assert_eq!(weekly.used_percent, 12.25);
        assert_eq!(weekly.window_minutes, Some(WEEKLY_MINUTES));
        assert_eq!(weekly.window_kind.as_deref(), Some("weekly"));
        assert_eq!(reading.email.as_deref(), Some("person@example.com"));
        assert_eq!(reading.plan.as_deref(), Some("Muse Code Power Usage"));
    }

    /// The answer carries a live inference key and payment details. Nothing in
    /// the published reading may contain either.
    #[test]
    fn the_inference_key_and_payment_method_never_reach_the_reading() {
        let reading = parse(FULL.as_bytes()).unwrap();
        let rendered = format!("{reading:?}");
        assert!(!rendered.contains("secret-inference-key"), "{rendered}");
        assert!(!rendered.contains("4242"), "{rendered}");
    }

    #[test]
    fn a_non_five_hour_short_window_states_no_kind() {
        let body = FULL.replace(
            "\"window_duration_mins\": 300",
            "\"window_duration_mins\": 240",
        );
        let reading = parse(body.as_bytes()).unwrap();
        let primary = reading.usage.unwrap().primary.unwrap();
        assert_eq!(primary.window_minutes, Some(240));
        assert_eq!(primary.window_kind, None);
    }

    /// Meta omits `subs_usage` (absent or null) while the 5-hour window is idle.
    /// `parse` reports that as "no usage" with the account still identified, so
    /// the provider can fill it from the last full reading.
    #[test]
    fn omitted_usage_parses_as_idle_whether_absent_or_null() {
        let absent = r#"{"is_subs_active": true, "subs_tier_name": "Pro", "user_email": "a@b.c"}"#;
        let null = r#"{"is_subs_active": true, "user_email": "a@b.c", "subs_usage": null}"#;
        for body in [absent, null] {
            let reading = parse(body.as_bytes()).unwrap();
            assert_eq!(reading.usage, None, "{body}");
            assert_eq!(reading.email.as_deref(), Some("a@b.c"), "{body}");
        }
    }

    const IDLE: &str = r#"{"is_subs_active": true, "subs_tier_name": "Muse Code Power Usage", "user_email": "person@example.com"}"#;

    /// With no earlier full reading (just after a restart) an idle answer can't
    /// be filled in, so it's a transient failure and no weekly figure is invented.
    #[tokio::test]
    async fn an_idle_answer_with_nothing_remembered_is_transient() {
        let (base, _) = crate::loopback::serve_once(200, IDLE.as_bytes().to_vec()).await;
        let mut provider = provider(env_with(&[(TOKEN_ENV, "dca:local")]));
        provider.url = format!("{base}/muse-code/key");
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert_eq!(attempt.usage.unwrap_err().error_class(), "upstream_failed");
    }

    fn weekly_in(full: &str, resets_at: i64) -> String {
        full.replace("1791500000", &resets_at.to_string())
    }

    /// After a full reading, an idle answer is a fresh reading: the 5-hour
    /// window at 0% with no reset, and the weekly figure unchanged, because no
    /// request can have been made without reopening the 5-hour window.
    #[tokio::test]
    async fn an_idle_answer_after_a_full_reading_serves_both_windows() {
        let future = chrono::Utc::now().timestamp() + 3 * 86_400;
        let mut provider = provider(env_with(&[(TOKEN_ENV, "dca:local")]));
        let (base, _) =
            crate::loopback::serve_once(200, weekly_in(FULL, future).into_bytes()).await;
        provider.url = format!("{base}/muse-code/key");
        assert!(provider
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .is_ok());

        let (base, _) = crate::loopback::serve_once(200, IDLE.as_bytes().to_vec()).await;
        provider.url = format!("{base}/muse-code/key");
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        let usage = attempt
            .usage
            .as_ref()
            .expect("an idle answer after a full one serves");
        let short = usage.primary.as_ref().unwrap();
        assert_eq!(short.used_percent, 0.0);
        assert_eq!(short.resets_at, None);
        assert_eq!(short.window_minutes, Some(300));
        let weekly = usage.secondary.as_ref().unwrap();
        assert_eq!(weekly.used_percent, 12.25);
        assert!(weekly.resets_at.is_some());
        assert_eq!(
            attempt.observed.as_ref().unwrap().account_id.as_deref(),
            Some("person@example.com")
        );
    }

    /// Once the remembered weekly reset has passed, that window restarted at 0%.
    #[tokio::test]
    async fn an_idle_answer_after_the_weekly_reset_reads_zero() {
        let past = chrono::Utc::now().timestamp() - 60;
        let mut provider = provider(env_with(&[(TOKEN_ENV, "dca:local")]));
        let (base, _) = crate::loopback::serve_once(200, weekly_in(FULL, past).into_bytes()).await;
        provider.url = format!("{base}/muse-code/key");
        assert!(provider
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .is_ok());

        let (base, _) = crate::loopback::serve_once(200, IDLE.as_bytes().to_vec()).await;
        provider.url = format!("{base}/muse-code/key");
        let usage = provider
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .unwrap();
        let weekly = usage.secondary.unwrap();
        assert_eq!(weekly.used_percent, 0.0);
        assert_eq!(weekly.resets_at, None);
    }

    /// A remembered reading belongs to one account: an idle answer for another
    /// email gets nothing filled in.
    #[tokio::test]
    async fn an_idle_answer_for_another_account_is_not_filled_from_this_one() {
        let future = chrono::Utc::now().timestamp() + 3 * 86_400;
        let mut provider = provider(env_with(&[(TOKEN_ENV, "dca:local")]));
        let (base, _) =
            crate::loopback::serve_once(200, weekly_in(FULL, future).into_bytes()).await;
        provider.url = format!("{base}/muse-code/key");
        assert!(provider
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .is_ok());

        let other = IDLE.replace("person@example.com", "other@example.com");
        let (base, _) = crate::loopback::serve_once(200, other.into_bytes()).await;
        provider.url = format!("{base}/muse-code/key");
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert_eq!(attempt.usage.unwrap_err().error_class(), "upstream_failed");
    }

    #[test]
    fn incomplete_billing_and_an_inactive_subscription_report_no_quota() {
        let billing = r#"{"require_payment": true, "is_subs_active": true}"#;
        let inactive = r#"{"require_payment": false, "is_subs_active": false}"#;
        let missing = r#"{}"#;
        for body in [billing, inactive, missing] {
            let error = parse(body.as_bytes()).unwrap_err();
            assert_eq!(error.error_class(), "no_quota_reported", "{body}");
        }
    }

    #[test]
    fn a_malformed_window_is_a_decode_failure() {
        let body = FULL.replace("\"used_percent\": 37.5", "\"used_percent\": \"lots\"");
        assert_eq!(
            parse(body.as_bytes()).unwrap_err().error_class(),
            "decode_failed"
        );
        let body = FULL.replace("\"window_duration_mins\": 300, ", "");
        assert_eq!(
            parse(body.as_bytes()).unwrap_err().error_class(),
            "decode_failed"
        );
    }

    #[test]
    fn an_out_of_range_reset_drops_the_reset_but_keeps_the_window() {
        let body = FULL.replace("1791000000", "99999999999999");
        let primary = parse(body.as_bytes())
            .unwrap()
            .usage
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.used_percent, 37.5);
    }

    #[test]
    fn vault_payload_may_be_the_bare_token_or_the_cli_json() {
        assert_eq!(
            token_from_payload(" dca:abc \n").as_deref(),
            Some("dca:abc")
        );
        assert_eq!(
            token_from_payload(r#"{"meta": {"access_token": "dca:xyz", "refresh_token": "r"}}"#)
                .as_deref(),
            Some("dca:xyz")
        );
        assert_eq!(token_from_payload(r#"{"meta": {}}"#), None);
    }

    #[test]
    fn the_environment_token_wins_over_the_cli_file() {
        let provider = provider(env_with(&[(TOKEN_ENV, "dca:from-env")]));
        assert_eq!(provider.local_token().unwrap(), "dca:from-env");
    }

    #[test]
    fn an_inline_cli_token_is_read() {
        let dir = scratch("inline");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"providers": {"meta": {"mechanism": "oauth", "access_token": "dca:inline"}}}"#,
        )
        .unwrap();
        let provider = provider(env_with(&[(AUTH_PATH_ENV, path.to_str().unwrap())]));
        assert_eq!(provider.local_token().unwrap(), "dca:inline");
    }

    /// The CLI default on macOS: the file names the login, the token lives in
    /// the Keychain. That's a configured credential we can't use, not an absent one.
    #[test]
    fn a_keychain_only_login_is_unusable_not_absent() {
        let dir = scratch("keychain");
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{"providers": {"meta": {"mechanism": "oauth", "storage": "keychain"}}}"#,
        )
        .unwrap();
        let provider = provider(env_with(&[(AUTH_PATH_ENV, path.to_str().unwrap())]));
        let error = provider.local_token().unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable");
        assert!(format!("{error:?}").contains("apikey:muse"), "{error:?}");
    }

    #[test]
    fn no_login_file_is_absent() {
        let dir = scratch("absent");
        let provider = provider(env_with(&[(
            AUTH_PATH_ENV,
            dir.path().join("missing.json").to_str().unwrap(),
        )]));
        assert_eq!(
            provider.local_token().unwrap_err().error_class(),
            "credential_absent"
        );
    }

    /// A login file we can't read says nothing about whether someone signed in.
    /// Reading it as absent would let a consumer prune the account.
    #[test]
    fn an_unreadable_login_file_is_unavailable_not_absent() {
        // A directory where the file should be: present, but not readable as a file.
        let dir = scratch("unreadable");
        let provider = provider(env_with(&[(AUTH_PATH_ENV, dir.path().to_str().unwrap())]));
        assert_eq!(
            provider.local_token().unwrap_err().error_class(),
            "local_source_unavailable"
        );
    }

    #[test]
    fn the_default_path_is_under_the_home_config() {
        let home = std::env::temp_dir().join("muse-home");
        let home = home.to_str().unwrap();
        let provider = provider(env_with(&[("HOME", home), ("USERPROFILE", home)]));
        assert_eq!(
            provider.auth_path().unwrap(),
            PathBuf::from(home).join(".config/muse/auth.json")
        );
    }

    #[tokio::test]
    async fn a_non_device_token_is_refused_without_a_request() {
        let provider = provider(env_with(&[(TOKEN_ENV, "LLM_dashboard_key")]));
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable");
    }

    #[tokio::test]
    async fn the_local_lane_sends_the_device_token_and_publishes_the_account() {
        let (base, request) = crate::loopback::serve_once(200, FULL.as_bytes().to_vec()).await;
        let mut provider = provider(env_with(&[(TOKEN_ENV, "dca:local")]));
        provider.url = format!("{base}/muse-code/key");
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert_eq!(attempt.source.as_deref(), Some("api"));
        let usage = attempt.usage.as_ref().unwrap();
        assert_eq!(usage.primary.as_ref().unwrap().used_percent, 37.5);
        assert_eq!(
            attempt.observed.as_ref().unwrap().account_id.as_deref(),
            Some("person@example.com")
        );
        let info = attempt.account_info.as_ref().unwrap();
        assert_eq!(info.plan_type.as_deref(), Some("Muse Code Power Usage"));
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post "), "{request}");
        assert!(
            request.contains("authorization: bearer dca:local"),
            "{request}"
        );
        assert!(request.contains("x-api-version: 1.0.0"), "{request}");
    }

    #[tokio::test]
    async fn a_local_401_is_rejected_and_a_429_is_transient() {
        for (status, class) in [
            (401, "credential_rejected"),
            (429, "upstream_failed"),
            (503, "upstream_failed"),
        ] {
            let (base, _) = crate::loopback::serve_once(status, Vec::new()).await;
            let mut provider = provider(env_with(&[(TOKEN_ENV, "dca:local")]));
            provider.url = format!("{base}/muse-code/key");
            let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
            assert_eq!(attempt.usage.unwrap_err().error_class(), class, "{status}");
        }
    }

    type Reports = Arc<Mutex<Vec<(u16, u64)>>>;

    struct MockCredentialSource {
        payload: Vec<u8>,
        reports: Reports,
    }

    #[async_trait]
    impl CredentialSource for MockCredentialSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            Ok(VaultCredential {
                payload: self.payload.clone(),
                expires_at_ms: None,
                record_version: 7,
                account_id: None,
                email: None,
                org_name: None,
                project_id: None,
            })
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

    fn vault_provider(payload: &[u8], loader: Arc<VaultHandleLoader>) -> (MuseProvider, Reports) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let source: Arc<dyn CredentialSource> = Arc::new(MockCredentialSource {
            payload: payload.to_vec(),
            reports: Arc::clone(&reports),
        });
        let mut provider = MuseProvider::new_with_handle_loader(
            crate::config::QuotaConfig::default().into(),
            Some(source),
            loader,
        );
        provider.env = env_with(&[]);
        (provider, reports)
    }

    #[test]
    fn a_vault_deposit_replaces_the_local_lane() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:muse", "apikey")]);
        let (provider, _) = vault_provider(b"dca:v", loader);
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].stable_id(), "apikey:muse");
    }

    #[test]
    fn without_a_deposit_the_local_lane_stands() {
        let (provider, _) = vault_provider(b"dca:v", Arc::new(VaultHandleLoader::default()));
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    #[tokio::test]
    async fn the_vault_lane_serves_and_reports_a_401_with_its_version() {
        let (base, _) = crate::loopback::serve_once(200, FULL.as_bytes().to_vec()).await;
        let (mut provider, reports) = vault_provider(
            br#"{"access_token": "dca:vault"}"#,
            Arc::new(VaultHandleLoader::new(None)),
        );
        provider.url = format!("{base}/muse-code/key");
        let handle = CredentialHandle::vault("apikey:muse", VaultCapability::new("ckh_muse"));
        let attempt = provider.fetch_handle(&handle).await;
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert!(attempt.usage.is_ok());
        assert_eq!(attempt.observed.as_ref().unwrap().record_version, Some(7));
        assert!(reports.lock().unwrap().is_empty());

        let (base, _) = crate::loopback::serve_once(401, Vec::new()).await;
        provider.url = format!("{base}/muse-code/key");
        let attempt = provider.fetch_handle(&handle).await;
        assert_eq!(
            attempt.usage.unwrap_err().error_class(),
            "credential_rejected"
        );
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 7)]);
    }
}
