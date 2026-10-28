//! ClinePass usage fetcher — credential from vault `apikey:cline-pass` or
//! `CLINE_API_KEY` / `CLINEPASS_API_KEY` environment variables.
//!
//! Endpoint: `GET https://api.cline.bot/api/v1/users/me/plan/usage-limits`
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `CLINE_API_KEY` or `CLINEPASS_API_KEY` was available to fetch a real window.
//! The endpoint, the `Authorization: Bearer` header, and the response shape
//! are ported from CodexBar's working parser at tag v0.44.0:
//! - `Sources/CodexBarCore/Providers/ClinePass/ClinePassUsageFetcher.swift:116-117, 133-135, 163-210`
//! - `Sources/CodexBarCore/Providers/ClinePass/ClinePassSettingsReader.swift:4-5, 22-33`
//!
//! The test payload mirrors that shape. The shared HTTP transport it rides
//! (`http.rs`) is itself live-proven.
//!
//! These upstream paths were verified at CodexBar v0.45.0 and NO LONGER EXIST
//! upstream: the QuickJS plugin-engine migration replaced this provider's
//! Swift fetcher with plugin JS. The citation is kept with its tag rather
//! than deleted -- it is the provenance for a fixture-verified port, and a
//! tagged path is still openable while a bare one sends a reader hunting.
//! Flagged by scripts/parity-citations.py.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::credential_source::CredentialSource;
use crate::provider::{CredentialHandle, FetchAttempt, HandlesError};
use crate::vault_handles::VaultHandleLoader;
use crate::{
    env,
    http::JsonRequest,
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "clinepass";
const API_KEY_ENV: &[&str] = &["CLINE_API_KEY", "CLINEPASS_API_KEY"];

#[derive(Debug, Deserialize)]
struct ClinePassLimitsResponse {
    data: ClinePassLimitsData,
    success: bool,
}

#[derive(Debug, Deserialize)]
struct ClinePassLimitsData {
    limits: Vec<ClinePassLimit>,
}

#[derive(Debug, Deserialize)]
struct ClinePassLimit {
    #[serde(rename = "type")]
    limit_type: String,
    #[serde(rename = "percentUsed")]
    percent_used: f64,
    #[serde(rename = "resetsAt")]
    resets_at: Option<String>,
}

/// Cline answers a key whose account has no Cline Pass plan with HTTP 404 and
/// `{"data":null,"error":"no plan history found for user","success":false}`
/// (measured on a live key, 2026-10-28). The key works and there's simply no
/// plan to report, so that is `no_quota_reported`, not a transient upstream
/// failure retried forever. Any other 404, such as a moved endpoint, keeps its
/// class, so it stays visible as a failure.
fn no_plan_or(error: FetchError) -> FetchError {
    const NO_PLAN: &str = "no plan history found";
    let no_plan = match &error {
        FetchError::ProviderStatus(404, body) => body.contains(NO_PLAN),
        FetchError::Upstream(message) => message.contains("404") && message.contains(NO_PLAN),
        _ => false,
    };
    if no_plan {
        FetchError::NoQuotaReported(
            "Cline reports no Cline Pass plan on this account (\"no plan history found for user\")"
                .to_string(),
        )
    } else {
        error
    }
}

fn clean_key(key: String) -> Option<String> {
    crate::text::strip_wrapping_quotes(&key)
}

/// Normalize a ClinePass `/api/v1/users/me/plan/usage-limits` body to rate-limit windows.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: ClinePassLimitsResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("clinepass limits not decodable: {e}")))?;

    if !response.success {
        return Err(FetchError::Decode(
            "ClinePass response success was false".to_string(),
        ));
    }

    let mut primary = None;
    let mut secondary = None;
    let mut tertiary = None;

    for limit in response.data.limits {
        // A limit type this module does not know is DISCARDED, and the cost is
        // that an upstream can add a window and we publish an account as less
        // constrained than it is -- silently, since the entry still looks like a
        // complete reading. The alternative would be to publish it as an extra
        // window with an unknown cadence, which states a limit nobody can act on
        // and is the wrong direction under the cost-asymmetry fence.
        //
        // What keeps the discard honest is the guard after this loop: if the
        // known types match NOTHING, the fetch fails rather than reporting an
        // empty success. So the invisible case is a response mixing known and
        // unknown types, which is worth re-checking whenever this provider's
        // upstream changes shape.
        let window_minutes = match limit.limit_type.as_str() {
            "five_hour" => 5 * 60,
            "weekly" => 7 * 24 * 60,
            "monthly" => 30 * 24 * 60,
            _ => continue,
        };

        let resets_at = match limit.resets_at {
            Some(raw) => {
                let trimmed = raw.trim();
                let dt = chrono::DateTime::parse_from_rfc3339(trimmed)
                    .map_err(|e| FetchError::Decode(format!("invalid resetsAt timestamp: {e}")))?;
                Some(
                    dt.with_timezone(&chrono::Utc)
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string(),
                )
            }
            None => None,
        };

        let window = RateWindow {
            window_kind: match limit.limit_type.as_str() {
                "five_hour" => Some(cortexkit_provider_usage::window_kind::FIVE_HOUR.to_string()),
                "weekly" => Some(cortexkit_provider_usage::window_kind::WEEKLY.to_string()),
                "monthly" => Some(cortexkit_provider_usage::window_kind::MONTHLY.to_string()),
                _ => None,
            },
            used_percent: limit.percent_used.clamp(0.0, 100.0),
            raw_used_percent: None,
            resets_at,
            window_minutes: Some(window_minutes),
            used_count: None,
            total_count: None,
            regeneration: None,
            breakdown: None,
        };

        match limit.limit_type.as_str() {
            "five_hour" => primary = Some(window),
            "weekly" => secondary = Some(window),
            "monthly" => tertiary = Some(window),
            _ => {}
        }
    }

    // The loop skips any limit whose type is not one of the three known windows,
    // so a response can be well-formed and still yield nothing. Returning Ok in
    // that case would publish a window-less entry as a SUCCESS, and a successful
    // fetch is stored fresh — it would replace whatever good windows the provider
    // had, reset its retry state, and report it healthy while consumers saw no
    // quota signal. A degraded entry carries a reason and a transient failure
    // keeps serving the last good window; an empty success does neither.
    //
    // Only the zero-output case is rejected: a response carrying one recognized
    // window beside several unrecognized ones is still a usable answer, and the
    // upstream type vocabulary is deliberately not validated.
    if primary.is_none() && secondary.is_none() && tertiary.is_none() {
        return Err(FetchError::Decode(
            "clinepass limits carried no recognized window type".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

/// The ClinePass usage provider.
pub struct ClinePassProvider {
    url: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
}

impl ClinePassProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::default()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            url: "https://api.cline.bot/api/v1/users/me/plan/usage-limits".to_string(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
        }
    }

    async fn read(&self, api_key: &str, vault: bool) -> Result<Usage, FetchError> {
        let request = JsonRequest::get(&self.url).bearer(api_key);
        // Keep the status on the vault lane so only a 401 retires the served key.
        // The environment lane retains the shared transport's existing mapping.
        let sent = if vault {
            request
                .send_provider_status_first(&self.http, PROVIDER_NAME)
                .await
                .map(|response| response.body)
        } else {
            request.send(&self.http).await
        };
        let body = sent.map_err(no_plan_or)?;
        normalize_usage(&body)
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
        let key = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(key) => clean_key(key),
            Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
        };
        let Some(key) = key else {
            return FetchAttempt::failure(
                None,
                Some("vault".to_string()),
                FetchError::CredentialUnusable(
                    "the apikey:cline-pass deposit holds no API key".to_string(),
                ),
            );
        };
        match self.read(&key, true).await {
            Ok(usage) => FetchAttempt::success(None, "vault", usage),
            Err(error) => {
                crate::credential_source::report_vault_auth_failure(
                    Some(credential_source),
                    handle,
                    record_version,
                    &error,
                );
                FetchAttempt::failure(None, Some("vault".to_string()), error)
            }
        }
    }

    async fn fetch_handle_from(
        &self,
        handle: &CredentialHandle,
        lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }
        let result: Result<ProviderUsage, FetchError> = async {
            let raw_key = env::first_env_from(API_KEY_ENV, lookup)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            // Set but empty: configured wrong rather than not configured.
            let api_key = clean_key(raw_key).ok_or_else(|| {
                FetchError::CredentialUnusable("ClinePass API key is empty".to_string())
            })?;

            let usage = self.read(&api_key, false).await?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

impl Default for ClinePassProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for ClinePassProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.clinepass_handles()?;
            if !vault.is_empty() {
                // The key carries no account identity, so a parallel environment
                // lane could overwrite the vault reading as the same account.
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        self.fetch_handle_from(handle, |key| std::env::var_os(key))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vercel::testkit::{credential, routes, Log, MockSource};

    const LIMITS: &str =
        r#"{"success":true,"data":{"limits":[{"type":"weekly","percentUsed":42.0}]}}"#;

    fn provider(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
        base: &str,
    ) -> ClinePassProvider {
        let mut provider = ClinePassProvider::new_with_handle_loader(source, loader);
        provider.url = format!("{base}/api/v1/users/me/plan/usage-limits");
        provider
    }

    fn vault_handle() -> CredentialHandle {
        CredentialHandle::scoped("apikey:cline-pass", "apikey")
    }

    async fn clinepass(status: u16, body: &str) -> (String, Log) {
        let body = body.to_string();
        routes(move |path, _| match path {
            "/api/v1/users/me/plan/usage-limits" => (status, body.as_bytes().to_vec()),
            _ => (404, Vec::new()),
        })
        .await
    }

    #[test]
    fn a_vault_deposit_routes_to_clinepass_and_replaces_the_environment_lane() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:cline-pass", "apikey")]);
        let (source, _) = MockSource::serving(&[]);
        let provider = provider(Some(source), loader, "http://127.0.0.1:9");
        assert_eq!(provider.handles().unwrap(), vec![vault_handle()]);
    }

    #[tokio::test]
    async fn the_vault_key_is_sent_and_the_environment_key_is_not() {
        let (base, served) = clinepass(200, LIMITS).await;
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:cline-pass", "apikey")]);
        let (source, reports) =
            MockSource::serving(&[("apikey:cline-pass", credential("vault-key", 3))]);
        let provider = provider(Some(source), loader, &base);
        let handles = provider.handles().unwrap();
        assert_eq!(handles, vec![vault_handle()]);
        let attempt = provider
            .fetch_handle_from(&handles[0], |_| Some("env-key".into()))
            .await;
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(attempt.usage.unwrap().secondary.unwrap().used_percent, 42.0);
        let requests = served.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer vault-key")
        );
        assert!(reports.lock().unwrap().is_empty());
    }

    /// A key on an account with no Cline Pass plan gets Cline's 404 "no plan
    /// history" body: no quota on both lanes, never reported to the vault. Any
    /// other 404 stays an upstream failure.
    #[tokio::test]
    async fn no_plan_history_is_no_quota_and_other_404s_are_not() {
        const NO_PLAN: &str =
            r#"{"data":null,"error":"no plan history found for user","success":false}"#;
        for (body, class) in [
            (NO_PLAN, "no_quota_reported"),
            ("Not Found", "upstream_failed"),
        ] {
            for vault in [true, false] {
                let (base, _) = clinepass(404, body).await;
                let (source, reports) =
                    MockSource::serving(&[("apikey:cline-pass", credential("vault-key", 2))]);
                let provider =
                    provider(Some(source), Arc::new(VaultHandleLoader::default()), &base);
                let handle = if vault {
                    vault_handle()
                } else {
                    CredentialHandle::implicit()
                };
                let attempt = provider
                    .fetch_handle_from(&handle, |_| Some("env-key".into()))
                    .await;
                assert_eq!(
                    attempt.usage.unwrap_err().error_class(),
                    class,
                    "body {body:?}, vault={vault}"
                );
                assert!(reports.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn only_a_vault_401_is_reported_once_with_the_record_version() {
        for (status, vault) in [(401, true), (403, true), (401, false)] {
            let (base, served) = clinepass(status, "").await;
            let (source, reports) =
                MockSource::serving(&[("apikey:cline-pass", credential("vault-key", 11))]);
            let provider = provider(Some(source), Arc::new(VaultHandleLoader::default()), &base);
            let handle = if vault {
                vault_handle()
            } else {
                CredentialHandle::implicit()
            };
            let attempt = provider
                .fetch_handle_from(&handle, |_| Some("env-key".into()))
                .await;
            assert_eq!(
                attempt.usage.unwrap_err().error_class(),
                "credential_rejected"
            );
            assert_eq!(served.lock().unwrap().len(), 1, "no retry on HTTP {status}");
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            let expected = if vault && status == 401 {
                vec![("apikey:cline-pass".to_string(), 401, 11)]
            } else {
                Vec::new()
            };
            assert_eq!(
                *reports.lock().unwrap(),
                expected,
                "HTTP {status}, vault={vault}"
            );
        }
    }

    #[tokio::test]
    async fn no_vault_handle_keeps_both_environment_key_spellings_working() {
        for name in API_KEY_ENV {
            let (base, served) = clinepass(200, LIMITS).await;
            let (source, reports) = MockSource::serving(&[]);
            let provider = provider(Some(source), Arc::new(VaultHandleLoader::default()), &base);
            let handles = provider.handles().unwrap();
            assert_eq!(handles, vec![CredentialHandle::implicit()]);
            let attempt = provider
                .fetch_handle_from(&handles[0], |key| (key == *name).then(|| "env-key".into()))
                .await;
            assert_eq!(attempt.source.as_deref(), Some("api"));
            assert_eq!(attempt.usage.unwrap().secondary.unwrap().used_percent, 42.0);
            let requests = served.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].authorization.as_deref(), Some("Bearer env-key"));
            assert!(reports.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn normalizes_happy_path_with_all_windows() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 25.5,
                        "resetsAt": "2026-07-11T12:30:00Z"
                    },
                    {
                        "type": "weekly",
                        "percentUsed": 50.0,
                        "resetsAt": "2026-07-18T12:30:00.123Z"
                    },
                    {
                        "type": "monthly",
                        "percentUsed": 75.0,
                        "resetsAt": "2026-08-11T12:30:00Z"
                    },
                    {
                        "type": "unknown_type",
                        "percentUsed": 10.0,
                        "resetsAt": "2026-07-11T12:30:00Z"
                    }
                ]
            }
        }"#;

        let usage = normalize_usage(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.5);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-11T12:30:00Z"));
        assert_eq!(primary.window_kind.as_deref(), Some("five_hour"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 50.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-18T12:30:00Z"));
        assert_eq!(secondary.window_kind.as_deref(), Some("weekly"));
        assert_eq!(secondary.window_minutes, Some(10080));

        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 75.0);
        assert_eq!(tertiary.resets_at.as_deref(), Some("2026-08-11T12:30:00Z"));
        assert_eq!(tertiary.window_kind.as_deref(), Some("monthly"));
        assert_eq!(tertiary.window_minutes, Some(43200));
    }

    #[test]
    fn percent_clamping() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 120.0,
                        "resetsAt": null
                    },
                    {
                        "type": "weekly",
                        "percentUsed": -5.0,
                        "resetsAt": null
                    }
                ]
            }
        }"#;

        let usage = normalize_usage(body).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 100.0);

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 0.0);
    }

    #[test]
    fn missing_resets_at_keeps_window_with_none() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 25.0,
                        "resetsAt": null
                    }
                ]
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn garbage_timestamp_returns_decode_error() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "five_hour",
                        "percentUsed": 25.0,
                        "resetsAt": "garbage"
                    }
                ]
            }
        }"#;

        let result = normalize_usage(body);
        assert!(matches!(result, Err(FetchError::Decode(_))));
    }

    /// A response mixing a known limit with an unknown one is a SUCCESS whose
    /// unknown window is silently gone.
    ///
    /// This is the case the all-skipped guard cannot catch, and it is the shape
    /// an upstream produces the day it adds a window type: the entry looks like
    /// a complete reading, and an account is published as less constrained than
    /// it is. Pinned so the behaviour is a recorded decision rather than an
    /// accident — if this provider should ever fail, or mark, on an unknown
    /// type, this test is where that argument gets made.
    #[test]
    fn a_known_limit_beside_an_unknown_one_publishes_only_the_known_window() {
        let body = br#"{
            "success": true,
            "data": {
                "limits": [
                    { "type": "weekly", "percentUsed": 40.0 },
                    { "type": "quarterly", "percentUsed": 95.0 }
                ]
            }
        }"#;

        let usage = normalize_usage(body).expect("a known limit makes this a usable response");

        assert_eq!(usage.secondary.expect("weekly window").used_percent, 40.0);
        // The unknown limit reached no slot and no extra window: nothing on the
        // wire says a 95%-used limit was seen and dropped.
        assert!(usage.primary.is_none());
        assert!(usage.tertiary.is_none());
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn only_unrecognized_limit_types_is_a_decode_error() {
        // A well-formed response whose every limit is skipped, or which lists
        // none. Returning Ok here would publish an empty entry as a success,
        // which is stored fresh and replaces the provider's previously good
        // windows while reporting it healthy — a silent outage rather than a
        // visible failure. The garbage timestamp is incidental: it is never
        // parsed, because the unrecognized type is skipped first.
        let unrecognized = br#"{
            "success": true,
            "data": {
                "limits": [
                    {
                        "type": "unknown_type",
                        "percentUsed": 25.0,
                        "resetsAt": "garbage"
                    }
                ]
            }
        }"#
        .as_slice();
        let empty = br#"{ "success": true, "data": { "limits": [] } }"#.as_slice();

        for body in [unrecognized, empty] {
            match normalize_usage(body).unwrap_err() {
                FetchError::Decode(message) => assert!(
                    message.contains("no recognized window type"),
                    "the all-skipped case must name itself: {message}"
                ),
                other => panic!("expected Decode, got {other:?}"),
            }
        }
    }

    #[test]
    fn success_false_returns_decode_error() {
        let body = br#"{
            "success": false,
            "data": {
                "limits": []
            }
        }"#;

        let result = normalize_usage(body);
        assert!(matches!(result, Err(FetchError::Decode(_))));
    }

    #[tokio::test]
    async fn fetch_handle_missing_key_returns_no_session() {
        let provider = ClinePassProvider::new();
        let attempt = provider
            .fetch_handle_from(&CredentialHandle::implicit(), |_| None)
            .await;

        assert!(matches!(attempt.usage, Err(FetchError::NoSession(_))));
    }
}
