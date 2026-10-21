//! Vercel AI Gateway — the team's credit balance, read with an AI Gateway key.
//!
//! - Endpoint: `GET https://ai-gateway.vercel.sh/v1/credits`, bearer AI Gateway
//!   API key. The answer carries `balance` (the team's remaining USD credit) and
//!   `total_used` (lifetime USD spend), both as decimal strings.
//! - Reference: CodexBar v0.79.0 `Sources/CodexBarCore/Resources/Plugins/vercel.js`
//!   and `docs/vercel.md`, which cite Vercel's REST credit API documentation.
//!   Tested against fixtures shaped from those sources only; no Vercel account
//!   was available when this was written.
//!
//! The balance is published as one pool with `basis: reported`, because the
//! endpoint states the remainder directly. Its funding is `unknown`: one figure
//! covers whatever credit the team holds, and the endpoint does not say whether
//! it was bought or given. A zero or negative balance is published as stated.
//! `total_used` is validated, as upstream does, but not published: a lifetime
//! spend figure is neither a remainder nor a pool size.
//!
//! WHY TWO PROVIDERS. Vercel offers a second reading, a user's personal budget
//! inside one team, which needs a different credential (an access token plus a
//! team id) sent to a different host. That lane is the `vercel-budget` provider
//! in `vercel_budget.rs`. Both cannot be handles of one provider: the credits
//! answer carries no account identity, and one identity-less handle collapses a
//! provider's entries into a single unlabelled one, so one lane's reading would
//! hide the other's.
//!
//! Credential: a vault deposit under `apikey:vercel` when one exists, otherwise
//! `AI_GATEWAY_API_KEY`, the variable upstream and models.dev both document.

use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

use crate::credential_source::CredentialSource;
use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;

pub const PROVIDER_NAME: &str = "vercel";
/// The AI Gateway origin. Only the gateway key is ever sent here.
pub(crate) const GATEWAY_BASE: &str = "https://ai-gateway.vercel.sh";
/// The Vercel REST API origin. Only the budget access token is ever sent here.
pub(crate) const API_BASE: &str = "https://api.vercel.com";
const CREDITS_PATH: &str = "/v1/credits";
const KEY_ENV: &str = "AI_GATEWAY_API_KEY";
/// Upstream bounds every Vercel request to 15 seconds.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const USD: &str = "USD";

/// Reads one environment variable, trimmed, absent when unset or blank.
///
/// Injected rather than read from the process so tests never see a developer's
/// real Vercel credentials.
pub(crate) type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[cfg(not(test))]
pub(crate) fn default_env() -> EnvLookup {
    Arc::new(|key| {
        std::env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// Registry tests build production providers too; they get an empty
/// environment.
#[cfg(test)]
pub(crate) fn default_env() -> EnvLookup {
    Arc::new(|_| None)
}

/// The two Vercel origins, held together by both Vercel providers.
///
/// Each provider sends its credential to exactly one of them. They travel
/// together so a test can point each origin at its own loopback server and
/// check, from what each server received, that neither credential ever
/// reaches the other's host.
#[derive(Clone, Debug)]
pub(crate) struct VercelHosts {
    pub(crate) gateway: String,
    pub(crate) api: String,
}

impl VercelHosts {
    pub(crate) fn production() -> Self {
        Self {
            gateway: GATEWAY_BASE.to_string(),
            api: API_BASE.to_string(),
        }
    }
}

/// A decimal-string amount, held to upstream's exact shape.
///
/// Upstream accepts only `^-?\d+(?:\.\d+)?$`. The shared money parser is a
/// little wider (it takes a leading `+`, `.5` and `5.`), so the stricter shape
/// is checked first: a value upstream refuses is refused here too.
fn decimal_amount(value: Option<&serde_json::Value>, field: &str) -> Result<Amount, FetchError> {
    let refused = || FetchError::Decode(format!("vercel credits: {field} is not a decimal string"));
    let text = value
        .and_then(serde_json::Value::as_str)
        .ok_or_else(refused)?;
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(whole) || fraction.is_some_and(|fraction| !all_digits(fraction)) {
        return Err(refused());
    }
    parse_amount(text, USD).ok_or_else(refused)
}

/// Turn a `/v1/credits` answer into the pool to publish.
///
/// A missing or malformed amount fails the reading rather than becoming zero:
/// a zero balance is a reason to stop routing here, a parse failure is not.
pub(crate) fn normalize_credits(body: &[u8]) -> Result<Vec<Pool>, FetchError> {
    // The parse error is replaced rather than quoted, so nothing from an
    // unexpected body reaches the published error.
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| FetchError::Decode("vercel credits: the answer is not JSON".to_string()))?;
    let balance = decimal_amount(value.get("balance"), "balance")?;
    let spent = decimal_amount(value.get("total_used"), "total_used")?;
    if spent.minor < 0 {
        return Err(FetchError::Decode(
            "vercel credits: total_used is negative".to_string(),
        ));
    }
    Ok(vec![Pool {
        id: "balance".to_string(),
        label: "Team credit balance".to_string(),
        // One figure for whatever credit the team holds; the endpoint does not
        // say whether it was bought or given.
        funding: PoolFunding::Unknown,
        remaining: Some(balance),
        // `total_used` is lifetime spend, not the pool's size.
        total: None,
        basis: PoolBasis::Reported,
        // The endpoint states no enable flag, and a positive balance is not one.
        spendable: None,
        resets_at: None,
        expires_at: None,
    }])
}

pub struct VercelProvider {
    hosts: VercelHosts,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    env: EnvLookup,
}

impl VercelProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            hosts: VercelHosts::production(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
            env: default_env(),
        }
    }

    fn credits_url(&self) -> String {
        format!("{}{CREDITS_PATH}", self.hosts.gateway)
    }

    /// Point both origins at test servers, for the budget provider's tests that
    /// run the two Vercel providers side by side.
    #[cfg(test)]
    pub(crate) fn set_hosts_for_test(&mut self, hosts: VercelHosts) {
        self.hosts = hosts;
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
            Ok(key) => key.trim().to_string(),
            Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
        };
        if key.is_empty() {
            return FetchAttempt::failure(
                None,
                Some("vault".to_string()),
                FetchError::CredentialUnusable(
                    "the apikey:vercel deposit holds no AI Gateway key".to_string(),
                ),
            );
        }
        // The status-first send keeps the numeric status, which the shared gate
        // needs: only a 401 on this served key is reported to the vault.
        let result = JsonRequest::get(self.credits_url())
            .bearer(&key)
            .timeout(REQUEST_TIMEOUT)
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
            .and_then(|response| normalize_credits(&response.body));
        if let Err(error) = &result {
            crate::credential_source::report_vault_auth_failure(
                Some(credential_source),
                handle,
                record_version,
                error,
            );
        }
        attempt_from(result, "vault")
    }

    async fn fetch_env(&self) -> FetchAttempt {
        let Some(key) = (self.env)(KEY_ENV) else {
            return FetchAttempt::failure(
                None,
                None,
                FetchError::NoSession(format!("no {KEY_ENV} and no apikey:vercel vault deposit")),
            );
        };
        let result = JsonRequest::get(self.credits_url())
            .bearer(&key)
            .timeout(REQUEST_TIMEOUT)
            .send(&self.http)
            .await
            .and_then(|body| normalize_credits(&body));
        attempt_from(result, "api")
    }
}

fn attempt_from(result: Result<Vec<Pool>, FetchError>, source: &str) -> FetchAttempt {
    match result {
        Ok(pools) => {
            // No window: the endpoint reports none, and an empty `Usage` beside
            // a pool is the shape the wire contract gives a balance-only entry.
            let mut attempt = FetchAttempt::success(None, source, Usage::default());
            attempt.pools = Some(pools);
            attempt
        }
        Err(error) => FetchAttempt::failure(None, Some(source.to_string()), error),
    }
}

#[async_trait]
impl UsageProvider for VercelProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.vercel_handles()?;
            if !vault.is_empty() {
                // Vault-only custody once a deposit exists. The key carries no
                // account identity, so an environment lane beside it would be a
                // second identity-less row that the read path collapses into one
                // with an arbitrary winner.
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }
        self.fetch_env().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vercel::testkit::{credential, routes, MockSource, Served};

    /// Shaped from upstream's own fixture in `APIBalancePluginTests.swift`.
    const CREDITS: &str = r#"{"balance":"95.50","total_used":"4.50"}"#;

    fn credits_body(balance: &str) -> String {
        format!(r#"{{"balance":"{balance}","total_used":"4.50"}}"#)
    }

    fn provider(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
        gateway: &str,
        env: &[(&str, &str)],
    ) -> VercelProvider {
        let mut provider = VercelProvider::new_with_handle_loader(source, loader);
        provider.hosts = VercelHosts {
            gateway: gateway.to_string(),
            api: "http://127.0.0.1:9".to_string(),
        };
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        provider.env = Arc::new(move |key| {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        });
        provider
    }

    fn vault_handle() -> CredentialHandle {
        CredentialHandle::scoped("apikey:vercel", "apikey")
    }

    #[test]
    fn the_documented_balance_is_one_reported_usd_pool_of_unknown_funding() {
        let pools = normalize_credits(CREDITS.as_bytes()).unwrap();
        assert_eq!(pools.len(), 1, "{pools:?}");
        let pool = &pools[0];
        assert_eq!(pool.id, "balance");
        assert_eq!(pool.basis, PoolBasis::Reported);
        assert_eq!(pool.funding, PoolFunding::Unknown);
        assert_eq!(
            pool.remaining,
            Some(Amount {
                minor: 9550,
                exponent: 2,
                unit: "USD".to_string()
            })
        );
        // Lifetime spend is not the pool's size, and nothing says it is open.
        assert_eq!(pool.total, None);
        assert_eq!(pool.spendable, None);
        assert_eq!(pool.resets_at, None);
    }

    /// Upstream preserves zero and negative balances; so does this.
    #[test]
    fn zero_and_negative_balances_are_published_as_stated() {
        let zero = normalize_credits(credits_body("0.00").as_bytes()).unwrap();
        assert_eq!(zero[0].remaining.as_ref().unwrap().minor, 0);
        let negative = normalize_credits(credits_body("-1.250000").as_bytes()).unwrap();
        let amount = negative[0].remaining.as_ref().unwrap();
        assert_eq!((amount.minor, amount.exponent), (-1_250_000, 6));
    }

    /// Every body upstream's tests refuse is a decode failure here, and none of
    /// them is echoed into the error.
    #[test]
    fn malformed_balances_are_decode_failures_without_echoing_the_body() {
        let mut bodies: Vec<String> = ["", " ", "NaN", "1e999", "0x10", "private-response"]
            .iter()
            .map(|value| credits_body(value))
            .collect();
        bodies.extend(
            [
                "private-response",
                "{}",
                "null",
                r#"{"balance":12,"total_used":"0"}"#,
                r#"{"balance":"1.00","total_used":"-1.00"}"#,
                r#"{"balance":"1.00"}"#,
                // Shapes the shared money parser alone would accept.
                r#"{"balance":"+1.00","total_used":"0"}"#,
                r#"{"balance":".5","total_used":"0"}"#,
                r#"{"balance":"5.","total_used":"0"}"#,
            ]
            .iter()
            .map(|body| body.to_string()),
        );
        for body in bodies {
            match normalize_credits(body.as_bytes()) {
                Err(FetchError::Decode(message)) => {
                    assert!(!message.contains("private-response"), "{message}")
                }
                other => panic!("{body} must be a decode failure, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_vault_deposit_routes_to_vercel_and_replaces_the_environment_lane() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:vercel", "apikey")]);
        let (source, _) = MockSource::serving(&[]);
        let with_vault = provider(
            Some(source),
            Arc::clone(&loader),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-gateway-key")],
        );
        assert_eq!(with_vault.handles().unwrap(), vec![vault_handle()]);

        // The control: the same environment with no deposit enumerates the
        // environment lane, so the line above is the vault replacing it.
        let (source, _) = MockSource::serving(&[]);
        let without_vault = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-gateway-key")],
        );
        assert_eq!(
            without_vault.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    /// The budget family is a different family: a budget deposit is never
    /// enumerated as a gateway-key lane.
    #[test]
    fn a_budget_deposit_is_not_a_gateway_key_lane() {
        let loader = VaultHandleLoader::default();
        loader.install_rows_for_test(&[("apikey:vercel-budget", "apikey")]);
        assert!(loader.vercel_handles().unwrap().is_empty());
        assert_eq!(loader.vercel_budget_handles().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_vault_key_reads_the_balance() {
        let (base, served) = routes(|_, _| (200, CREDITS.as_bytes().to_vec())).await;
        let (source, reports) = MockSource::serving(&[("apikey:vercel", credential("gw-key", 3))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[],
        );
        let attempt = provider.fetch_handle(&vault_handle()).await;
        assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(attempt.pools.as_ref().unwrap().len(), 1);
        let requests = served.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/v1/credits");
        assert_eq!(requests[0].authorization.as_deref(), Some("Bearer gw-key"));
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_environment_key_reads_the_balance_without_a_vault() {
        let (base, served) = routes(|_, _| (200, CREDITS.as_bytes().to_vec())).await;
        let provider = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, "env-gateway-key")],
        );
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
        assert_eq!(attempt.source.as_deref(), Some("api"));
        let requests: Vec<Served> = served.lock().unwrap().clone();
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer env-gateway-key")
        );
    }

    #[tokio::test]
    async fn no_key_anywhere_is_credential_absent_and_sends_nothing() {
        let (base, served) = routes(|_, _| (200, CREDITS.as_bytes().to_vec())).await;
        let provider = provider(None, Arc::new(VaultHandleLoader::default()), &base, &[]);
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_absent", "{error:?}");
        assert!(served.lock().unwrap().is_empty());
    }

    /// Each refusal lands in its class, and only the 401 on a served vault key
    /// is reported to the vault.
    #[tokio::test]
    async fn each_status_maps_to_its_class_and_only_a_401_is_reported() {
        for (status, class, reported) in [
            (401, "credential_rejected", true),
            (403, "credential_rejected", false),
            (429, "upstream_failed", false),
            (503, "upstream_failed", false),
        ] {
            let (base, _) = routes(move |_, _| (status, b"private-response".to_vec())).await;
            let (source, reports) =
                MockSource::serving(&[("apikey:vercel", credential("gw-key", 11))]);
            let provider = provider(
                Some(source),
                Arc::new(VaultHandleLoader::default()),
                &base,
                &[],
            );
            let attempt = provider.fetch_handle(&vault_handle()).await;
            let error = attempt.usage.unwrap_err();
            assert_eq!(error.error_class(), class, "HTTP {status}: {error:?}");
            // 429 and 5xx keep the last reading served while they last.
            assert_eq!(
                crate::refresh::classify(&error) == crate::refresh::FetchClass::Transient,
                status == 429 || status >= 500,
                "HTTP {status}"
            );
            for _ in 0..20 {
                if !reports.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let expected = if reported {
                vec![("apikey:vercel".to_string(), 401, 11)]
            } else {
                Vec::new()
            };
            assert_eq!(*reports.lock().unwrap(), expected, "HTTP {status}");
        }
    }

    #[tokio::test]
    async fn a_bad_payload_is_decode_failed() {
        let (base, _) = routes(|_, _| (200, br#"{"balance":12}"#.to_vec())).await;
        let (source, reports) = MockSource::serving(&[("apikey:vercel", credential("gw-key", 1))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[],
        );
        let error = provider
            .fetch_handle(&vault_handle())
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "decode_failed", "{error:?}");
        assert!(reports.lock().unwrap().is_empty());
    }
}

/// Loopback servers and a scripted vault, shared by both Vercel providers'
/// tests. Placed after the test module so the source scanners that stop at a
/// file's `mod tests` never read it as production code.
#[cfg(test)]
pub(crate) mod testkit {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    use crate::credential_source::{
        CredentialSource, VaultCapability, VaultCredential, VaultGetError,
    };

    /// One request a loopback server answered.
    #[derive(Clone, Debug)]
    pub(crate) struct Served {
        pub(crate) path: String,
        /// The decoded query parameters, in the order sent.
        pub(crate) query: Vec<(String, String)>,
        /// The `Authorization` header exactly as received, if any.
        pub(crate) authorization: Option<String>,
    }

    pub(crate) type Log = Arc<Mutex<Vec<Served>>>;

    /// Serve every request on a loopback port from `answer(path, query)`, and
    /// record what each request carried.
    pub(crate) async fn routes<F>(answer: F) -> (String, Log)
    where
        F: Fn(&str, &[(String, String)]) -> (u16, Vec<u8>) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let served = Arc::clone(&log);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request = crate::loopback::read_request(&mut stream).await;
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.split(' ').nth(1))
                    .unwrap_or("/")
                    .to_string();
                let parsed = url::Url::parse(&format!("http://loopback{target}")).unwrap();
                let query: Vec<(String, String)> = parsed
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
                let authorization = request.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_string())
                });
                let (status, body) = answer(parsed.path(), &query);
                served.lock().unwrap().push(Served {
                    path: parsed.path().to_string(),
                    query,
                    authorization,
                });
                let head = format!(
                    "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });
        (format!("http://{address}"), log)
    }

    pub(crate) type Reports = Arc<Mutex<Vec<(String, u16, u64)>>>;

    /// A vault that serves fixed credentials by scoped id and records every
    /// auth-failure report.
    pub(crate) struct MockSource {
        credentials: HashMap<String, VaultCredential>,
        reports: Reports,
    }

    impl MockSource {
        pub(crate) fn serving(
            credentials: &[(&str, VaultCredential)],
        ) -> (Arc<dyn CredentialSource>, Reports) {
            let reports: Reports = Arc::new(Mutex::new(Vec::new()));
            let source = Self {
                credentials: credentials
                    .iter()
                    .map(|(id, credential)| (id.to_string(), credential.clone()))
                    .collect(),
                reports: Arc::clone(&reports),
            };
            (Arc::new(source), reports)
        }
    }

    pub(crate) fn credential(payload: &str, record_version: u64) -> VaultCredential {
        VaultCredential {
            payload: payload.as_bytes().to_vec(),
            expires_at_ms: None,
            record_version,
            account_id: None,
            email: None,
            org_name: None,
            project_id: None,
        }
    }

    #[async_trait]
    impl CredentialSource for MockSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            Err(VaultGetError::FailClosed)
        }

        async fn get_scoped(
            &self,
            credential_id: &str,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            self.credentials
                .get(credential_id)
                .cloned()
                .ok_or(VaultGetError::NotFound)
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            _provider_status: u16,
            _record_version: u64,
        ) {
        }

        async fn report_auth_failure_scoped(
            &self,
            credential_id: &str,
            provider_status: u16,
            record_version: u64,
        ) {
            self.reports.lock().unwrap().push((
                credential_id.to_string(),
                provider_status,
                record_version,
            ));
        }
    }
}
