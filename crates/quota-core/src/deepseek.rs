//! DeepSeek — a prepaid balance and nothing else.
//!
//! This endpoint reports no rate-window fields at all, so there is no window to
//! normalize and the account's whole standing is whatever credit remains.
//!
//! DeepSeek states granted and purchased credit as separate live remainders,
//! rather than as grants with consumption tracked against their sum. A consumer
//! policy naming one pool -- "spend only granted credit" -- is therefore exact
//! here rather than a ceiling, which is why these pools carry
//! `PoolBasis::Reported`.
//!
//! - Endpoint: `GET https://api.deepseek.com/user/balance`, bearer API key.
//! - Documented at <https://api-docs.deepseek.com/api/get-user-balance>.

use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

use crate::credential_source::{CredentialSource, VaultCapability};
use crate::env;
use crate::http::JsonRequest;
use crate::model::{Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;
use crate::LOG_TAG;

const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";
const ENV_KEYS: &[&str] = &["DEEPSEEK_API_KEY", "DEEPSEEK_TOKEN"];
const OPENCODE_PROVIDER: &str = "deepseek";

/// The documented response shape.
///
/// Every amount arrives as a **string**, which is deliberate on DeepSeek's part
/// and preserved here: see [`parse_amount`].
#[derive(Debug, Deserialize)]
struct BalanceResponse {
    is_available: Option<bool>,
    balance_infos: Option<Vec<BalanceInfo>>,
}

#[derive(Debug, Deserialize)]
struct BalanceInfo {
    currency: Option<String>,
    /// Present in the payload and deliberately unpublished: it is the sum of the
    /// two pools below, and emitting it as a third pool would have a consumer
    /// adding up its own money twice.
    ///
    /// Kept in the struct so the documented shape stays visible here, and so a
    /// future reader sees that its absence from the output is a decision.
    #[allow(dead_code)]
    total_balance: Option<String>,
    granted_balance: Option<String>,
    topped_up_balance: Option<String>,
}

/// Build the pools DeepSeek reports for one currency.
///
/// `granted` and `topped_up` are published as separate pools because DeepSeek
/// states each as its own live remainder. `total_balance` is deliberately not
/// published as a third pool: it is the sum of the other two, and a consumer
/// adding up pools would count the money twice.
fn pools_from(info: &BalanceInfo) -> Vec<Pool> {
    let unit = info.currency.as_deref().unwrap_or("").trim();
    if unit.is_empty() {
        // Without a denomination an amount states a quantity of nothing. Better
        // to publish no pool than a number a consumer cannot interpret.
        return Vec::new();
    }

    let mut pools = Vec::new();
    let mut push = |raw: &Option<String>, id: &str, label: &str, funding: PoolFunding| {
        let Some(text) = raw.as_deref() else {
            return;
        };
        let Some(amount) = parse_amount(text, unit) else {
            return;
        };
        pools.push(Pool {
            id: id.to_string(),
            label: label.to_string(),
            funding,
            remaining: Some(amount),
            total: None,
            // DeepSeek states each remainder directly, so a policy keyed on one
            // pool is exact here rather than a ceiling.
            basis: PoolBasis::Reported,
            // DeepSeek publishes no per-pool enable flag. Absent, not guessed:
            // `is_available` is an account-level statement and saying it applies
            // to each pool would be this module's inference, not DeepSeek's.
            spendable: None,
        });
    };

    push(
        &info.granted_balance,
        "granted_balance",
        "Granted credit",
        PoolFunding::Granted,
    );
    push(
        &info.topped_up_balance,
        "topped_up_balance",
        "Purchased credit",
        PoolFunding::Purchased,
    );
    pools
}

/// Turn a response body into the pools to publish.
pub fn normalize_pools(body: &[u8]) -> Result<Vec<Pool>, FetchError> {
    let response: BalanceResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("deepseek: {error}")))?;

    let Some(infos) = response.balance_infos else {
        return Err(FetchError::Decode(
            "deepseek: response carries no balance_infos".to_string(),
        ));
    };

    let pools: Vec<Pool> = infos.iter().flat_map(pools_from).collect();

    // Nothing publishable came out of the payload: no currency block at all, or
    // every amount in them unreadable. That is a decode failure rather than an
    // account with no money, and the two call for opposite responses --
    // publishing it as "no pools" would report a parse problem as a financial
    // fact, and a consumer would stop routing here rather than retrying.
    if pools.is_empty() {
        return Err(FetchError::Decode(
            "deepseek: no readable balance in any currency".to_string(),
        ));
    }

    Ok(pools)
}

/// Whether DeepSeek reports the balance as insufficient to serve API calls.
///
/// The upstream field is `is_available`, documented as "whether the user's
/// balance is sufficient for API calls" -- a statement about spending capacity,
/// not about whether the account exists or is enabled. The threshold is not
/// documented.
///
/// Only the negative is propagated onto pools. `false` means no call will be
/// served, which is true of every pool at once and is therefore safe to state
/// about each. `true` says the account can serve calls overall, which is not a
/// claim about any individual pool, so marking pools spendable from it would
/// publish this module's inference as DeepSeek's word.
fn balance_insufficient(body: &[u8]) -> bool {
    serde_json::from_slice::<BalanceResponse>(body)
        .ok()
        .and_then(|response| response.is_available)
        .is_some_and(|available| !available)
}

pub struct DeepSeekProvider {
    url: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
}

impl Default for DeepSeekProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DeepSeekProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::from_env()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            url: BALANCE_URL.to_string(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
        }
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

    /// The API key, from the environment or the shared auth store.
    ///
    /// Both are checked because a host may have either: the environment is the
    /// documented way to configure this provider, and the auth store is where a
    /// user who signed in through another tool already has the same key.
    fn api_key() -> Option<String> {
        if let Some(key) = env::first_env(ENV_KEYS) {
            return Some(key);
        }
        match crate::opencode_auth::read_provider(OPENCODE_PROVIDER) {
            Ok(Some(crate::opencode_auth::OpencodeAuth::Api { key })) => Some(key),
            // An OAuth entry under this name is not this provider's credential:
            // the balance endpoint takes an API key, and sending an access token
            // would produce a 401 recorded as a rejected credential.
            _ => None,
        }
    }

    async fn fetch_with_key(&self, key: &str) -> Result<Vec<Pool>, FetchError> {
        let body = JsonRequest::get(&self.url)
            .bearer(key)
            .send(&self.http)
            .await?;
        let mut pools = normalize_pools(&body)?;
        // Insufficient overall means no pool can currently fund a call, which is
        // a per-pool fact worth stating. Sufficient overall is not.
        if balance_insufficient(&body) {
            for pool in &mut pools {
                pool.spendable = Some(false);
            }
        }
        Ok(pools)
    }

    async fn fetch_vault(&self, handle_id: &str, capability: &VaultCapability) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match credential_source.get(capability, 120_000).await {
            Ok(credential) => credential,
            Err(error) => {
                eprintln!(
                    "{LOG_TAG} warning: deepseek vault credential.get failed ({handle_id}): {error:?}"
                );
                return FetchAttempt::unverified_vault_failure(error);
            }
        };
        let record_version = credential.record_version;
        let key = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(value) => value,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };

        let result = JsonRequest::get(&self.url)
            .bearer(&key)
            .send_provider_status_first(&self.http, "deepseek")
            .await
            .map(|response| response.body)
            .and_then(|body| {
                let mut pools = normalize_pools(&body)?;
                if balance_insufficient(&body) {
                    for pool in &mut pools {
                        pool.spendable = Some(false);
                    }
                }
                Ok(pools)
            });
        if let Err(error) = &result {
            self.report_auth_failure(capability, record_version, error);
        }
        match result {
            Ok(pools) => {
                let mut attempt = FetchAttempt::success(None, "vault", Usage::default());
                attempt.pools = Some(pools);
                attempt
            }
            Err(error) => FetchAttempt::failure(None, Some("vault".to_string()), error),
        }
    }
}

#[async_trait]
impl UsageProvider for DeepSeekProvider {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.deepseek_handles()?;
            if !vault.is_empty() {
                // Vault-only custody once an apikey handle exists: a static API
                // key carries no inline account identity, so keeping the local
                // lane alongside a vault lane would force the emission gate to
                // collapse both into one unlabelled row with an arbitrary
                // winner. And the migration case is precisely that the key has
                // LEFT the file and been replaced by a pointer, so there is no
                // competing local value -- only garbage that produces a 401.
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if let Some(capability) = handle.vault_capability() {
            return self.fetch_vault(handle.stable_id(), capability).await;
        }
        let Some(key) = Self::api_key() else {
            return FetchAttempt::failure(
                None,
                None,
                FetchError::NoSession(
                    "no DEEPSEEK_API_KEY and no deepseek entry in the opencode auth store"
                        .to_string(),
                ),
            );
        };

        let pools = match self.fetch_with_key(&key).await {
            Ok(pools) => pools,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };

        // No windows: this endpoint reports no rate-window fields, so an empty
        // `Usage` beside a non-empty pool list is the honest shape. The wire
        // checker accepts that combination for exactly this case.
        let mut attempt = FetchAttempt::success(None, "api", Usage::default());
        attempt.pools = Some(pools);
        attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Amount;

    /// Live-shape fixture from the published documentation, values as sent.
    ///
    /// Synthetic in the sense that it was transcribed from DeepSeek's own
    /// example rather than captured from an account, but the SHAPE is the
    /// documented one: amounts as strings, currency alongside.
    const DOC_FIXTURE: &[u8] = br#"{
        "is_available": true,
        "balance_infos": [
          { "currency": "CNY",
            "total_balance": "110.00",
            "granted_balance": "10.00",
            "topped_up_balance": "100.00" }
        ]
    }"#;

    #[test]
    fn the_documented_payload_yields_a_granted_and_a_purchased_pool() {
        let pools = normalize_pools(DOC_FIXTURE).expect("documented shape must parse");

        assert_eq!(pools.len(), 2, "one pool per stated remainder: {pools:?}");
        assert_eq!(pools[0].id, "granted_balance");
        assert_eq!(pools[0].funding, PoolFunding::Granted);
        assert_eq!(
            pools[0].remaining,
            Some(Amount {
                minor: 1000,
                exponent: 2,
                unit: "CNY".to_string()
            })
        );
        assert_eq!(pools[1].id, "topped_up_balance");
        assert_eq!(pools[1].funding, PoolFunding::Purchased);
        assert_eq!(
            pools[1].remaining,
            Some(Amount {
                minor: 10_000,
                exponent: 2,
                unit: "CNY".to_string()
            })
        );
    }

    /// The total is not published as a pool.
    ///
    /// It is the sum of the other two, so a consumer adding pools to find what
    /// an account holds would count every unit twice and route as though it had
    /// double the credit.
    #[test]
    fn the_total_is_not_published_as_a_third_pool() {
        let pools = normalize_pools(DOC_FIXTURE).expect("parses");
        assert!(
            !pools.iter().any(|pool| pool.id.contains("total")),
            "total must not be a pool: {pools:?}"
        );
        let summed: i64 = pools
            .iter()
            .filter_map(|pool| pool.remaining.as_ref().map(|amount| amount.minor))
            .sum();
        assert_eq!(summed, 11_000, "the pools must sum to the stated total");
    }

    /// A body whose amounts are all unreadable is a decode failure, not an
    /// account with no money.
    ///
    /// Publishing it as "no pools" would report a parsing problem as a financial
    /// fact, and the two call for opposite responses: one is retried and fixed,
    /// the other is a reason to stop routing here.
    #[test]
    fn a_body_with_no_readable_amount_is_a_decode_error() {
        let body = br#"{ "is_available": true,
            "balance_infos": [ { "currency": "USD", "granted_balance": "1,000.00" } ] }"#;
        assert!(matches!(normalize_pools(body), Err(FetchError::Decode(_))));
    }

    /// An amount with no currency states a quantity of nothing.
    #[test]
    fn an_amount_without_a_currency_is_not_published() {
        let body = br#"{ "is_available": true,
            "balance_infos": [ { "granted_balance": "10.00" } ] }"#;
        assert!(matches!(normalize_pools(body), Err(FetchError::Decode(_))));
    }

    /// An insufficient balance marks every pool unspendable; a sufficient one
    /// says nothing about any single pool.
    ///
    /// `is_available` is documented as whether the balance suffices for API
    /// calls. False applies to every pool at once, so stating it per pool is
    /// faithful. True is an account-wide statement that establishes nothing
    /// about an individual pool, and propagating it would publish an inference
    /// of ours as the provider's word.
    #[test]
    fn an_insufficient_balance_marks_pools_unspendable_and_a_sufficient_one_says_nothing() {
        let insufficient = br#"{ "is_available": false,
            "balance_infos": [ { "currency": "USD", "granted_balance": "1.00" } ] }"#;
        assert!(
            balance_insufficient(insufficient),
            "false must read as insufficient"
        );

        // Sufficient, and unstated, are both "no statement about this pool".
        assert!(!balance_insufficient(DOC_FIXTURE));
        let unstated =
            br#"{ "balance_infos": [ { "currency": "USD", "granted_balance": "1.00" } ] }"#;
        assert!(!balance_insufficient(unstated));

        let pools = normalize_pools(DOC_FIXTURE).expect("parses");
        assert!(
            pools.iter().all(|pool| pool.spendable.is_none()),
            "a sufficient balance must not claim per-pool spendability: {pools:?}"
        );
    }

    /// Every pool is reported rather than derived, because DeepSeek states each
    /// remainder directly. A policy naming one pool is exact here.
    #[test]
    fn deepseek_pools_are_reported_not_derived() {
        let pools = normalize_pools(DOC_FIXTURE).expect("parses");
        assert!(
            pools.iter().all(|pool| pool.basis == PoolBasis::Reported),
            "{pools:?}"
        );
    }

    /// Malformed JSON is a decode failure naming this provider.
    #[test]
    fn a_garbage_body_is_a_decode_error() {
        assert!(matches!(
            normalize_pools(b"not json"),
            Err(FetchError::Decode(_))
        ));
    }

    use std::sync::Mutex;

    use crate::credential_source::{VaultCredential, VaultGetError};

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
            account_id: None,
            email: None,
            org_name: None,
            project_id: None,
        }
    }

    fn write_handles(body: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ck-quota-deepseek-handles-{}-{}.json",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        std::io::Write::write_all(&mut file, body.as_bytes()).unwrap();
        path
    }

    /// An `apikey:deepseek` handle routes to the deepseek provider.
    #[test]
    fn an_apikey_handle_routes_to_the_deepseek_provider() {
        let path = write_handles(r#"{"handles":{"apikey:deepseek":"ckh_deepseek"}}"#);
        let loader = crate::vault_handles::VaultHandleLoader::new(Some(path.clone()));
        let handles = loader.deepseek_handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].stable_id(), "apikey:deepseek");
        assert!(handles[0].vault_capability().is_some());
        let _ = std::fs::remove_file(path);
    }

    /// Vault configured for the family: the local lane is absent.
    #[test]
    fn vault_handles_replace_the_implicit_local_lane() {
        let path = write_handles(r#"{"handles":{"apikey:deepseek":"ckh_deepseek"}}"#);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = DeepSeekProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(Some(
                path.clone(),
            ))),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].vault_capability().is_some());
        assert_eq!(handles[0].stable_id(), "apikey:deepseek");
        let _ = std::fs::remove_file(path);
    }

    /// No vault handle configured: the local lane is present.
    #[test]
    fn implicit_local_lane_survives_when_no_vault_handles_are_mapped() {
        let path = write_handles(r#"{"handles":{"oauth:xai":"ckh_grok"}}"#);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = DeepSeekProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(Some(
                path.clone(),
            ))),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles, vec![CredentialHandle::implicit()]);
        let _ = std::fs::remove_file(path);
    }

    /// Two handles for one api-key family are refused at load time.
    ///
    /// The refusal itself lives in `vault_handles`; this pins the observable
    /// effect on the provider's lane: a refused family serves no vault handle.
    #[test]
    fn two_handles_for_one_apikey_family_are_refused() {
        let path = write_handles(
            r#"{"handles":{"apikey:deepseek":"ckh_a","apikey:deepseek:second":"ckh_b"}}"#,
        );
        let loader = crate::vault_handles::VaultHandleLoader::new(Some(path.clone()));
        assert!(loader.deepseek_handles().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }

    /// The vault lane serves the key and reports a 401 to the store.
    #[tokio::test]
    async fn vault_lane_serves_the_key_and_reports_a_401() {
        let body = br#"{"is_available":true,"balance_infos":[{"currency":"USD","granted_balance":"10.00","topped_up_balance":"5.00"}]}"#
            .to_vec();
        let (base, request) = crate::loopback::serve_once(200, body).await;
        let (source, reports) = source(Ok(credential(b"deepseek-vault-key", 9)));
        let mut provider = DeepSeekProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        provider.url = format!("{base}/user/balance");
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "apikey:deepseek",
                VaultCapability::new("ckh_deepseek"),
            ))
            .await;
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        let pools = attempt.pools.as_ref().unwrap();
        assert_eq!(pools.len(), 2);
        assert!(request
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer deepseek-vault-key"));
        assert!(reports.lock().unwrap().is_empty());
    }

    /// A 401 on the vault lane is reported to the credential store.
    #[tokio::test]
    async fn vault_401_reports_the_served_version() {
        let (base, _) = crate::loopback::serve_once(401, Vec::new()).await;
        let (source, reports) = source(Ok(credential(b"deepseek-vault-key", 44)));
        let mut provider = DeepSeekProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        provider.url = format!("{base}/user/balance");
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "apikey:deepseek",
                VaultCapability::new("ckh_deepseek"),
            ))
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
}
