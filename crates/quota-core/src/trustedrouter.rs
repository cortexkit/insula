//! TrustedRouter's per-key budget, from CodexBar v0.85.0's bundled plugin.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::credential_source::{
    get_vault_credential, report_vault_auth_failure, take_utf8_payload, CredentialSource,
    VaultGetError, VAULT_READ_MIN_TTL_MS,
};
use crate::http::{provider_client_without_redirects, JsonRequest};
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::provider::{
    AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
};
use crate::vault_handles::VaultHandleLoader;

const PROVIDER_NAME: &str = "trustedrouter";
const KEY_URL: &str = "https://api.trustedrouter.com/v1/key";
// The upstream plugin accepts nonnegative safe integers for microdollars.
const MAX_MICRODOLLARS: i64 = 9_007_199_254_740_991;

fn decode_error() -> FetchError {
    FetchError::Decode("TrustedRouter returned invalid microdollar budget or usage fields".into())
}

fn microdollars(value: &Value) -> Result<i64, FetchError> {
    value
        .as_i64()
        .filter(|amount| (0..=MAX_MICRODOLLARS).contains(amount))
        .ok_or_else(decode_error)
}

fn normalize_pool(body: &[u8]) -> Result<Pool, FetchError> {
    let response: Value = serde_json::from_slice(body).map_err(|_| decode_error())?;
    let data = response.get("data").ok_or_else(decode_error)?;
    // The key response requires these usage counters. They are historical spend,
    // not budget capacity, so validate them without publishing them as quota.
    for field in [
        "usage_microdollars",
        "usage_daily_microdollars",
        "usage_weekly_microdollars",
        "usage_monthly_microdollars",
    ] {
        microdollars(data.get(field).ok_or_else(decode_error)?)?;
    }
    let limit = data.get("limit_microdollars").ok_or_else(decode_error)?;
    let remaining = data
        .get("limit_remaining_microdollars")
        .ok_or_else(decode_error)?;
    let amount = |minor| Amount {
        minor,
        exponent: 6,
        unit: "USD".into(),
    };
    let (total, remaining, basis, unlimited) = match (limit, remaining) {
        (Value::Null, Value::Null) => (None, None, PoolBasis::Unstated, Some(true)),
        (limit, remaining) => {
            let total = microdollars(limit)?;
            // The provider already netted out in-flight reservations.
            let remaining = microdollars(remaining)?;
            if remaining > total {
                return Err(FetchError::Decode(
                    "TrustedRouter returned remaining budget above the stated limit".into(),
                ));
            }
            (
                Some(amount(total)),
                Some(amount(remaining)),
                PoolBasis::Reported,
                None,
            )
        }
    };
    Ok(Pool {
        id: "key_budget".into(),
        label: "Key budget".into(),
        funding: PoolFunding::Unknown,
        remaining,
        total,
        basis,
        spendable: None,
        resets_at: None,
        expires_at: None,
        unlimited,
    })
}

pub struct TrustedRouterProvider {
    url: String,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
}

impl TrustedRouterProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            url: KEY_URL.into(),
            credential_source,
            handle_loader,
        }
    }

    async fn read(&self, key: &str) -> Result<Pool, FetchError> {
        let http = provider_client_without_redirects()?;
        let response = JsonRequest::get(&self.url)
            .bearer(key)
            .send_provider_status_first(&http, PROVIDER_NAME)
            .await
            .map_err(|error| match error {
                // A 403 is a permission refusal, not a rejected key. Do not
                // expose refusal excerpts, which can contain secrets.
                FetchError::ProviderStatus(401, _) => {
                    FetchError::ProviderStatus(401, "TrustedRouter rejected the API key".into())
                }
                FetchError::ProviderStatus(status, _) => {
                    FetchError::Upstream(format!("TrustedRouter returned HTTP {status}"))
                }
                other => other,
            })?;
        if response.status != 200 {
            return Err(FetchError::Upstream(format!(
                "TrustedRouter returned HTTP {}",
                response.status
            )));
        }
        normalize_pool(&response.body)
    }
}

#[async_trait]
impl UsageProvider for TrustedRouterProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let handles = self.handle_loader.trustedrouter_handles()?;
            if !handles.is_empty() {
                return Ok(handles);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if !handle.is_vault() {
            return FetchAttempt::failure(
                None,
                None,
                FetchError::NoSession("no apikey:trustedrouter vault deposit".into()),
            );
        }
        let Some(source) = &self.credential_source else {
            return FetchAttempt::unverified_vault_failure(VaultGetError::Permanent);
        };
        let mut credential = match get_vault_credential(source, handle, VAULT_READ_MIN_TTL_MS).await
        {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        // A key's budget does not identify an account. Track the served version
        // so a replacement key cannot inherit a stale budget.
        let observed = Some(AccountObservation::new(
            None,
            Some(credential.record_version),
        ));
        let key = match take_utf8_payload(&mut credential.payload) {
            Ok(key) => key,
            Err(error) => return FetchAttempt::failure(observed, Some("vault".into()), error),
        };
        let key = key.trim();
        if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return FetchAttempt::failure(
                observed,
                Some("vault".into()),
                FetchError::CredentialUnusable(
                    "apikey:trustedrouter deposit is not an API key".into(),
                ),
            );
        }
        match self.read(key).await {
            Ok(pool) => {
                let mut attempt = FetchAttempt::success(observed, "vault", Usage::default());
                attempt.pools = Some(vec![pool]);
                attempt
            }
            Err(error) => {
                report_vault_auth_failure(Some(source), handle, credential.record_version, &error);
                FetchAttempt::failure(observed, Some("vault".into()), error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vercel::testkit::{credential, routes, MockSource};
    use crate::Registry;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    // A capped key whose remainder already excludes reservations, from
    // CodexBar v0.85.0, TestsPlugin/TrustedRouterPluginTests.swift:9–17.
    const LIMITED_KEY: &str = r#"{"data":{"hash":"<key-hash>","name":"example","label":"sk-tr-v1-a...wxyz","disabled":false,
    "limit":10,"limit_microdollars":10000000,"limit_remaining":6.6,"limit_remaining_microdollars":6600000,
    "limit_reset":"monthly","include_byok_in_limit":true,"usage":3.25,"usage_microdollars":3250000,
    "usage_daily":0.4,"usage_daily_microdollars":400000,"usage_weekly":1.1,"usage_weekly_microdollars":1100000,
    "usage_monthly":3.25,"usage_monthly_microdollars":3250000,"byok_usage":0.0,"byok_usage_microdollars":0,
    "reserved_microdollars":150000,"expires_at":null,"management":false}}"#;
    const ID: &str = "apikey:trustedrouter:fixture";

    // The minimal key response used to vary monetary fields in CodexBar
    // v0.85.0, TestsPlugin/TrustedRouterPluginTests.swift:19–24.
    fn key(limit: &str, remaining: &str, usage: &str) -> String {
        format!(
            r#"{{"data":{{"limit_microdollars":{limit},"limit_remaining_microdollars":{remaining},
            "usage_microdollars":{usage},"usage_daily_microdollars":400000,
            "usage_weekly_microdollars":1100000,"usage_monthly_microdollars":3250000}}}}"#
        )
    }

    fn handle() -> CredentialHandle {
        CredentialHandle::scoped(ID, "apikey")
    }

    fn provider(source: Option<Arc<dyn CredentialSource>>, base: &str) -> TrustedRouterProvider {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[(ID, "apikey")]);
        let mut provider = TrustedRouterProvider::new_with_handle_loader(source, loader);
        provider.url = format!("{base}/v1/key");
        provider
    }

    async fn wire_entry(provider: TrustedRouterProvider) -> Value {
        let registry = Registry::new(vec![Box::new(provider)]);
        registry.refresh_tick(&CancellationToken::new()).await;
        let entries = registry.get_usage(Some(PROVIDER_NAME)).await;
        assert_eq!(entries.len(), 1, "{entries:?}");
        serde_json::to_value(&entries[0]).unwrap()
    }

    #[tokio::test]
    async fn the_key_budget_publishes_the_exact_reported_remainder_without_windows() {
        let (base, served) = routes(|_, _| (200, LIMITED_KEY.as_bytes().to_vec())).await;
        let (source, reports) = MockSource::serving(&[(ID, credential("tr_fixture", 7))]);
        let wire = wire_entry(provider(Some(source), &base)).await;
        assert_eq!(wire["provider"], "trustedrouter");
        assert_eq!(wire["apiProvider"], "trustedrouter");
        assert_eq!(wire["source"], "vault");
        assert_eq!(wire["usage"], json!({}));
        assert!(wire.get("account").is_none());
        assert!(wire.get("errorClass").is_none());
        assert_eq!(
            wire["spend"],
            json!([{
                "id": "key_budget", "label": "Key budget", "funding": "unknown", "basis": "reported",
                "remaining": {"minor": 6600000, "exponent": 6, "unit": "USD"},
                "total": {"minor": 10000000, "exponent": 6, "unit": "USD"}
            }])
        );
        let requests = served.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v1/key");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer tr_fixture")
        );
        assert!(reports.lock().unwrap().is_empty());

        let body = key("9007199254740991", "1234567", "0");
        let pool = normalize_pool(body.as_bytes()).unwrap();
        assert_eq!(pool.remaining.unwrap().minor, 1_234_567);
        assert_eq!(pool.total.unwrap().minor, 9_007_199_254_740_991);
    }

    #[test]
    fn unlimited_exhausted_and_zero_cap_are_distinct_pools() {
        // Null, exhausted and zero-cap fixtures from CodexBar v0.85.0,
        // TestsPlugin/TrustedRouterPluginTests.swift:47–61.
        for (limit, remaining, want_total, want_remaining, basis, unlimited) in [
            ("null", "null", None, None, PoolBasis::Unstated, Some(true)),
            (
                "10000000",
                "0",
                Some(10_000_000),
                Some(0),
                PoolBasis::Reported,
                None,
            ),
            ("0", "0", Some(0), Some(0), PoolBasis::Reported, None),
        ] {
            let body = key(limit, remaining, "3250000");
            let pool = normalize_pool(body.as_bytes()).unwrap();
            assert_eq!(pool.total.map(|amount| amount.minor), want_total);
            assert_eq!(pool.remaining.map(|amount| amount.minor), want_remaining);
            assert_eq!(pool.basis, basis);
            assert_eq!(pool.unlimited, unlimited);
        }
    }

    #[test]
    fn malformed_or_missing_amounts_fail_without_disclosure() {
        // Invalid shapes and non-numeric amounts from CodexBar v0.85.0,
        // TestsPlugin/TrustedRouterPluginTests.swift:63–87.
        for body in [
            "not-json".into(),
            "[]".into(),
            "{}".into(),
            r#"{"data":[]}"#.into(),
            r#"{"data":{"usage":3.25,"usage_daily":0.4,"usage_weekly":1.1,"usage_monthly":3.25}}"#
                .into(),
            key("10000000", "null", "0"),
            key("null", "6600000", "0"),
            key("null", "null", "0").replace(r#""limit_microdollars":null,"#, ""),
            key("null", "null", "0").replace(r#""limit_remaining_microdollars":null,"#, ""),
            key(r#""10000000""#, "6600000", "0"),
            key("10000000", "-1", "0"),
            key("10000000", "6600000.5", "0"),
            key("9007199254740992", "6600000", "0"),
            key("10000000", "6600000", r#""private-response-marker""#),
            key("10000000", "6600000", "true"),
            key("10000000", "6600000", "1e309"),
            key("10000000", "6600000", "0").replace(r#""usage_daily_microdollars":400000,"#, ""),
        ] {
            let error = normalize_pool(body.as_bytes()).expect_err(&body);
            assert_eq!(error.error_class(), "decode_failed", "{body}");
            assert!(!error.to_string().contains("private-response-marker"));
        }
    }

    #[test]
    fn remaining_above_the_stated_limit_is_not_published_or_clamped() {
        let body = key("10000000", "10000001", "0");
        let error = normalize_pool(body.as_bytes()).unwrap_err();
        assert_eq!(error.error_class(), "decode_failed");
        assert!(error
            .to_string()
            .contains("remaining budget above the stated limit"));
    }

    #[tokio::test]
    async fn forbidden_is_upstream_failed_on_the_wire_and_only_401_reports_the_key() {
        // Refusal statuses and a body that must not leak, from CodexBar v0.85.0,
        // TestsPlugin/TrustedRouterPluginTests.swift:91–109, 124–151.
        for (status, class) in [
            (401, "credential_rejected"),
            (403, "upstream_failed"),
            (429, "upstream_failed"),
            (500, "upstream_failed"),
            (404, "upstream_failed"),
            (201, "upstream_failed"),
        ] {
            let (base, served) =
                routes(move |_, _| (status, b"private-response-marker tr_fixture".to_vec())).await;
            let (source, reports) = MockSource::serving(&[(ID, credential("tr_fixture", 7))]);
            let wire = wire_entry(provider(Some(source), &base)).await;
            assert_eq!(wire["errorClass"], class, "HTTP {status}");
            let error = wire["error"].as_str().unwrap();
            assert!(error.contains(&format!("HTTP {status}")), "{error}");
            assert!(!error.contains("private-response-marker"));
            assert!(!error.contains("tr_fixture"));
            assert!(wire.get("spend").is_none());
            tokio::task::yield_now().await;
            let expected = if status == 401 {
                vec![(ID.to_string(), 401, 7)]
            } else {
                vec![]
            };
            assert_eq!(*reports.lock().unwrap(), expected, "HTTP {status}");
            assert_eq!(served.lock().unwrap().len(), 1, "HTTP {status}: no retries");
        }
    }

    #[tokio::test]
    async fn missing_unreadable_and_unusable_vault_keys_send_nothing() {
        let (base, served) = routes(|_, _| (200, LIMITED_KEY.as_bytes().to_vec())).await;
        let absent = provider(None, &base);
        assert_eq!(
            absent.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
        let error = absent
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "credential_absent");

        let mut invalid_utf8 = credential("", 5);
        invalid_utf8.payload = vec![0xff];
        for (bad, class) in [
            (credential("", 3), "credential_unusable"),
            (credential("key\r\nheader", 4), "credential_unusable"),
            (invalid_utf8, "decode_failed"),
        ] {
            let (source, reports) = MockSource::serving(&[(ID, bad)]);
            let attempt = provider(Some(source), &base).fetch_handle(&handle()).await;
            assert_eq!(attempt.usage.unwrap_err().error_class(), class);
            assert!(attempt.observed.unwrap().record_version.is_some());
            assert!(reports.lock().unwrap().is_empty());
        }

        let (source, _) = MockSource::serving(&[]);
        let loader = Arc::new(VaultHandleLoader::default());
        loader.await_first_answer();
        let awaiting = TrustedRouterProvider::new_with_handle_loader(Some(source), loader);
        assert!(awaiting.handles().is_err(), "could not look is not absence");
        assert!(served.lock().unwrap().is_empty());
    }

    #[test]
    fn the_default_registry_routes_one_bare_or_suffixed_key_only() {
        let registry = Registry::with_defaults(crate::config::QuotaConfig::default(), None);
        assert!(registry.provider_names().contains(&"trustedrouter"));
        let (source, _) = MockSource::serving(&[]);
        let loader = Arc::new(VaultHandleLoader::default());
        let provider =
            TrustedRouterProvider::new_with_handle_loader(Some(source), Arc::clone(&loader));
        assert_eq!(provider.url, "https://api.trustedrouter.com/v1/key");
        for id in ["apikey:trustedrouter", ID] {
            loader
                .install_rows_for_test(&[(id, "apikey"), ("apikey:trustedrouter-other", "apikey")]);
            assert_eq!(
                provider.handles().unwrap(),
                vec![CredentialHandle::scoped(id, "apikey")]
            );
        }
        loader.install_rows_for_test(&[("apikey:trustedrouter", "apikey"), (ID, "apikey")]);
        assert!(loader.trustedrouter_handles().unwrap().is_empty());
    }
}
