//! Anthropic (claude) usage fetcher — the OAuth-bearer archetype, 2nd instance.
//!
//! This is the deliberate 2nd spike: it shares codex's "OAuth bearer → one GET →
//! decode JSON" skeleton but differs on every detail that could break the
//! abstraction, which is exactly why it validates it:
//!   - Implicit-local session source: opencode's unified auth.json (`anthropic`
//!     OAuth entry), NOT a provider-native file. Vault handles use the bare bearer
//!     bytes served by the injected credential source. CodexBar reads the macOS
//!     Keychain; we prefer
//!     opencode's cross-platform store, which already holds the same token.
//!   - Endpoint: `GET https://api.anthropic.com/api/oauth/usage` with the beta
//!     header `anthropic-beta: oauth-2025-04-20` and a `claude-code/<ver>` UA.
//!   - Response: NAMED windows (`five_hour`, `seven_day`, `seven_day_sonnet`, ...)
//!     where `utilization` is ALREADY a 0-100 percent and `resets_at` is ALREADY
//!     ISO 8601 — unlike codex's int-percent + epoch. So normalization is a
//!     near-passthrough here, mapping window names to known window lengths.

use std::{collections::HashSet, sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;

use crate::credential_source::{CredentialSource, VaultCapability};
use crate::provider::{AccountObservation, CredentialHandle, FetchAttempt};
use crate::vault_handles::VaultHandleLoader;
use crate::{
    http::{Header, JsonRequest},
    model::{RateWindow, Usage},
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
    limits: Option<Vec<ApiLimitEntry>>,
}

#[derive(Debug, Deserialize)]
struct ApiLimitEntry {
    kind: Option<String>,
    group: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    is_active: Option<bool>,
    scope: Option<ApiLimitScope>,
}

#[derive(Debug, Deserialize)]
struct ApiLimitScope {
    model: Option<ApiLimitModel>,
}

#[derive(Debug, Deserialize)]
struct ApiLimitModel {
    id: Option<String>,
    display_name: Option<String>,
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut last_was_dash = false;
    for character in value.trim().to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            last_was_dash = false;
        } else if !last_was_dash {
            result.push('-');
            last_was_dash = true;
        }
    }
    result.trim_matches('-').to_string()
}

fn is_all_models_scope(model_id: Option<&str>, model_name: &str) -> bool {
    if slug(model_name) == "all-models" {
        return true;
    }
    let Some(model_id) = model_id else {
        return false;
    };
    let id_slug = slug(model_id);
    id_slug == "all-models" || id_slug.ends_with("-all-models")
}

fn scoped_weekly_extras(
    limits: Option<&[ApiLimitEntry]>,
) -> Option<Vec<crate::model::ExtraWindow>> {
    let mut seen = HashSet::new();
    let extras: Vec<_> = limits
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.kind.as_deref() == Some("weekly_scoped")
                && entry.group.as_deref().unwrap_or("weekly") == "weekly"
                && entry.is_active != Some(false)
        })
        .filter_map(|entry| {
            let model = entry.scope.as_ref()?.model.as_ref()?;
            let display_name = model
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())?;
            let model_id = model
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty());
            if is_all_models_scope(model_id, display_name) {
                return None;
            }
            let identity = model_id.unwrap_or(display_name).to_string();
            let percent = entry.percent?;
            if !percent.is_finite() || !seen.insert(identity.clone()) {
                return None;
            }
            Some(crate::model::ExtraWindow {
                title: Some(format!("7 Day ({display_name})")),
                id: Some(identity),
                window: Some(RateWindow {
                    used_percent: percent.clamp(0.0, 100.0),
                    raw_used_percent: None,
                    resets_at: entry.resets_at.clone(),
                    window_minutes: Some(SEVEN_DAY_MINUTES),
                }),
            })
        })
        .collect();
    (!extras.is_empty()).then_some(extras)
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
        raw_used_percent: None,
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
        extra_rate_windows: scoped_weekly_extras(response.limits.as_deref()),
    })
}

fn canonical_account_id(account_id: Option<String>) -> Option<String> {
    account_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn usage_request(url: &str, bearer: &str) -> JsonRequest {
    JsonRequest::get(url)
        .timeout(REQUEST_TIMEOUT)
        .bearer(bearer)
        .header(Header::new("anthropic-beta", BETA_HEADER))
        .header(Header::new("User-Agent", CLAUDE_CODE_UA))
}

/// The anthropic usage provider.
pub struct AnthropicProvider {
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    usage_url: String,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::from_env()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            credential_source,
            handle_loader,
            usage_url: USAGE_URL.to_string(),
        }
    }

    fn report_auth_failure(
        &self,
        capability: &VaultCapability,
        record_version: u64,
        error: &FetchError,
    ) {
        let FetchError::ProviderStatus(status @ (401 | 403)) = error else {
            return;
        };
        let Some(source) = self.credential_source.as_ref() else {
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

    async fn fetch_local_bearer(&self, bearer: &str) -> FetchAttempt {
        let result = usage_request(&self.usage_url, bearer)
            .send(&self.http)
            .await
            .and_then(|body| normalize_usage(&body));
        match result {
            Ok(usage) => {
                FetchAttempt::success(Some(AccountObservation::new(None, None)), "oauth", usage)
            }
            Err(error) => FetchAttempt::failure(None, None, error),
        }
    }

    async fn fetch_vault(&self, capability: &VaultCapability) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match credential_source.get(capability, 120_000).await {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = credential.record_version;
        let account_info = credential.account_info();
        let observed = Some(AccountObservation::new(
            canonical_account_id(credential.account_id.clone()),
            Some(record_version),
        ));
        let bearer = match String::from_utf8(std::mem::take(&mut credential.payload)) {
            Ok(bearer) => bearer,
            Err(error) => {
                let mut payload = error.into_bytes();
                payload.fill(0);
                return FetchAttempt::failure(
                    observed,
                    None,
                    FetchError::Decode("vault credential payload is not valid UTF-8".to_string()),
                );
            }
        };

        let result = usage_request(&self.usage_url, &bearer)
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
            .map(|response| response.body)
            .and_then(|body| normalize_usage(&body));
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

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        let mut handles = vec![CredentialHandle::implicit()];
        if self.credential_source.is_some() {
            handles.extend(self.handle_loader.anthropic_handles()?);
        }
        Ok(handles)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if let Some(capability) = handle.vault_capability() {
            return self.fetch_vault(capability).await;
        }

        let access = match opencode_auth::read_provider(OPENCODE_PROVIDER)
            .map_err(FetchError::NoSession)
        {
            Ok(Some(OpencodeAuth::Oauth { access, .. })) => access,
            Ok(Some(OpencodeAuth::Api { key })) => key,
            Ok(None) => {
                return FetchAttempt::failure(
                    None,
                    None,
                    FetchError::NoSession("no anthropic entry in opencode auth.json".to_string()),
                );
            }
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        self.fetch_local_bearer(&access).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::credential_source::{VaultCredential, VaultGetError};
    use crate::provider::CredentialResolution;
    use crate::refresh::{next_slot_after_attempt, Incarnation, ProviderSlot};

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

    fn credential(payload: &[u8], record_version: u64) -> VaultCredential {
        VaultCredential {
            payload: payload.to_vec(),
            expires_at_ms: None,
            record_version,
            account_id: Some("   ".to_string()),
            email: None,
            org_name: None,
            project_id: None,
        }
    }

    fn test_provider(source: Arc<dyn CredentialSource>, usage_url: String) -> AnthropicProvider {
        let mut provider = AnthropicProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(None)),
        );
        provider.usage_url = usage_url;
        provider
    }

    struct VaultOnlyProvider {
        provider: AnthropicProvider,
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

    async fn serve_once(status: u16, body: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_string();
            let reason = if status == 200 { "OK" } else { "Unauthorized" };
            let headers = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            request
        });
        (format!("http://{address}/usage"), task)
    }

    fn write_handles(body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ck-quota-anthropic-handles-{}.json",
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

    #[test]
    fn handles_include_mapped_vault_entries_when_source_is_wired() {
        let path = write_handles(
            r#"{"handles":{"oauth:anthropic":"ckh_anthropic","oauth:xai":"ckh_grok"}}"#,
        );
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = AnthropicProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(Some(path.clone()))),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 2);
        assert_eq!(handles[0], CredentialHandle::implicit());
        assert_eq!(handles[1].stable_id(), "oauth:anthropic");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn vault_happy_path_uses_served_bearer_and_record_version() {
        let body = br#"{"five_hour":{"utilization":12.0,"resets_at":null}}"#.to_vec();
        let (url, request) = serve_once(200, body).await;
        let mut vault_credential = credential(b"anthropic-vault-token", 27);
        vault_credential.email = Some("user@example.com".to_string());
        vault_credential.org_name = Some("Example Org".to_string());
        let (source, _) = source(Ok(vault_credential));
        let provider = test_provider(source, url);
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;

        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt.observed.unwrap(),
            AccountObservation::new(None, Some(27))
        );
        let account_info = attempt.account_info.as_ref().unwrap();
        assert_eq!(account_info.email.as_deref(), Some("user@example.com"));
        assert_eq!(account_info.org_name.as_deref(), Some("Example Org"));
        assert_eq!(account_info.plan_type, None);
        assert_eq!(attempt.usage.unwrap().primary.unwrap().used_percent, 12.0);
        assert!(request
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer anthropic-vault-token"));
    }

    #[tokio::test]
    async fn vault_happy_path_serves_one_unlabeled_entry() {
        let body = br#"{"five_hour":{"utilization":7.0,"resets_at":null}}"#.to_vec();
        let (url, _) = serve_once(200, body).await;
        let (source, _) = source(Ok(credential(b"anthropic-vault-token", 28)));
        let handle =
            CredentialHandle::vault("oauth:anthropic", VaultCapability::new("ckh_anthropic"));
        let registry = crate::Registry::new(vec![Box::new(VaultOnlyProvider {
            provider: test_provider(source, url),
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
            7.0
        );
    }

    #[tokio::test]
    async fn failed_get_is_unverified_and_clears_prior_observation() {
        let (source, _) = source(Err(VaultGetError::Transient));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Unverified
        );

        let now = std::time::Instant::now();
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
        assert!(next.entry.is_none());
        assert!(next.label_in_flux);
        assert!(next.last_success_at.is_none());
    }

    #[tokio::test]
    async fn vault_401_reports_served_version_while_local_keeps_legacy_error() {
        let (vault_url, _) = serve_once(401, Vec::new()).await;
        let (source, reports) = source(Ok(credential(b"anthropic-vault-token", 44)));
        let mut provider = test_provider(Arc::clone(&source), vault_url);
        let vault = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;
        assert!(matches!(vault.usage, Err(FetchError::ProviderStatus(401))));
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 44)]);

        reports.lock().unwrap().clear();
        let (local_url, _) = serve_once(401, Vec::new()).await;
        provider.usage_url = local_url;
        let local = provider.fetch_local_bearer("anthropic-local-token").await;
        assert!(matches!(
            local.usage,
            Err(FetchError::Unauthorized(message)) if message == "HTTP 401"
        ));
        tokio::task::yield_now().await;
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_utf8_vault_payload_is_a_verified_decode_failure() {
        let (source, _) = source(Ok(credential(&[0xff, 0xfe], 8)));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Verified
        );
        assert_eq!(attempt.observed.unwrap().record_version, Some(8));
        assert!(matches!(attempt.usage, Err(FetchError::Decode(_))));
    }

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
        assert!(usage.extra_rate_windows.is_none());
        // opus is null, so tertiary falls back to sonnet.
        assert_eq!(usage.tertiary.unwrap().used_percent, 4.0);
    }

    #[test]
    fn scoped_weekly_limits_become_named_extra_windows() {
        let usage = normalize_usage(
            br#"{
                "five_hour": {"utilization": 12.0, "resets_at": "2026-07-18T12:00:00Z"},
                "seven_day": {"utilization": 60.0, "resets_at": "2026-07-20T12:00:00Z"},
                "limits": [
                    {
                        "kind": "weekly_scoped",
                        "percent": 100.0,
                        "resets_at": "2026-07-18T12:00:00Z",
                        "scope": {"model": {"display_name": "Fable"}},
                        "is_active": true
                    },
                    {
                        "kind": "weekly_scoped",
                        "percent": 20.0,
                        "resets_at": "2026-07-18T12:00:00Z",
                        "scope": {"model": {"display_name": "Fable"}},
                        "is_active": true
                    },
                    {
                        "kind": "weekly_scoped",
                        "percent": 80.0,
                        "scope": {"model": {"display_name": "Inactive"}},
                        "is_active": false
                    },
                    {
                        "kind": "weekly_scoped",
                        "percent": 80.0,
                        "scope": {"model": {}},
                        "is_active": true
                    }
                ]
            }"#,
        )
        .unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].title.as_deref(), Some("7 Day (Fable)"));
        assert_eq!(extras[0].id.as_deref(), Some("Fable"));
        let window = extras[0].window.as_ref().unwrap();
        assert_eq!(window.used_percent, 100.0);
        assert_eq!(window.window_minutes, Some(SEVEN_DAY_MINUTES));
    }

    #[test]
    fn all_models_scopes_are_skipped() {
        let usage = normalize_usage(
            br#"{
                "seven_day": {"utilization": 10.0, "resets_at": "2026-07-20T12:00:00Z"},
                "limits": [{
                    "kind": "weekly_scoped",
                    "group": "weekly",
                    "percent": 50.0,
                    "scope": {"model": {"id": "claude-all-models", "display_name": "All Models"}}
                }]
            }"#,
        )
        .unwrap();
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn non_weekly_scopes_are_skipped() {
        let usage = normalize_usage(
            br#"{
                "limits": [{
                    "kind": "weekly_scoped",
                    "group": "session",
                    "percent": 50.0,
                    "scope": {"model": {"id": "fable", "display_name": "Fable"}}
                }]
            }"#,
        )
        .unwrap();
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn absent_group_keeps_weekly_scoped_window() {
        let usage = normalize_usage(
            br#"{
                "limits": [{
                    "kind": "weekly_scoped",
                    "percent": 25.0,
                    "scope": {"model": {"id": "fable-v1", "display_name": "Fable"}}
                }]
            }"#,
        )
        .unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].title.as_deref(), Some("7 Day (Fable)"));
    }

    #[test]
    fn scoped_windows_deduplicate_by_model_id() {
        let usage = normalize_usage(
            br#"{
                "limits": [
                    {
                        "kind": "weekly_scoped", "group": "weekly", "percent": 10.0,
                        "scope": {"model": {"id": "fable-v1", "display_name": "Fable"}}
                    },
                    {
                        "kind": "weekly_scoped", "group": "weekly", "percent": 20.0,
                        "scope": {"model": {"id": "fable-v1", "display_name": "Fable"}}
                    },
                    {
                        "kind": "weekly_scoped", "group": "weekly", "percent": 30.0,
                        "scope": {"model": {"id": "fable-v2", "display_name": "Fable"}}
                    }
                ]
            }"#,
        )
        .unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].id.as_deref(), Some("fable-v1"));
        assert_eq!(extras[1].id.as_deref(), Some("fable-v2"));
    }

    #[test]
    fn missing_anthropic_limits_do_not_add_extra_windows() {
        let usage =
            normalize_usage(br#"{"five_hour":{"utilization":1.0,"resets_at":null}}"#).unwrap();
        assert!(usage.extra_rate_windows.is_none());
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
