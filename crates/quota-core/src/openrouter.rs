//! OpenRouter credit balance.
//!
//! OpenRouter's credits endpoint reports a purchased total and cumulative usage,
//! not a remaining balance or rate window. The one truthful signal is therefore a
//! derived USD pool; the endpoint says nothing about whether the credits were
//! purchased or granted, nor whether the pool is presently spendable.
//!
//! - Credential: the `openrouter` API entry in opencode's auth store.
//! - Endpoint: `GET https://openrouter.ai/api/v1/credits`, bearer API key.
//! - Account: `GET https://openrouter.ai/api/v1/key` with the same key returns
//!   `data.organization_id` and `data.creator_user_id`. The account is the
//!   organization when one is set, because an org key's credits bill to the org,
//!   and otherwise the user who created the key. See the "openrouter" section of
//!   `docs/audits/account-identity-survey.md`, which verified the fields live.
//!
//!   A key's owner does not change, so the lookup is made once per credential and
//!   cached in memory: per `record_version` for a vault record, per key value for
//!   the local key. The cached key is never logged. The lookup is enrichment only:
//!   if it fails, the balance still publishes, with no account, and the lookup is
//!   tried again on the next fetch.

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::credential_source::CredentialSource;
#[cfg(test)]
use crate::credential_source::VaultCapability;
use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{
    AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
};
use crate::vault_handles::VaultHandleLoader;
use crate::LOG_TAG;

const PROVIDER_NAME: &str = "openrouter";
const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";
const KEY_URL: &str = "https://openrouter.ai/api/v1/key";
const USD: &str = "USD";
const USD_EXPONENT: u8 = 2;
// OpenRouter sends JSON numbers rather than decimal strings. Preserve up to the
// shared parser's maximum stated precision before rounding the final balance to
// USD cents once, rather than scaling a binary float.
const INPUT_EXPONENT: u8 = 9;
const INPUT_UNITS_PER_CENT: i128 = 10_000_000;

#[derive(Debug, Deserialize)]
struct CreditsResponse {
    data: CreditsData,
}

#[derive(Debug, Deserialize)]
struct CreditsData {
    total_credits: serde_json::Number,
    total_usage: serde_json::Number,
}

#[derive(Debug, Deserialize)]
struct KeyResponse {
    data: KeyData,
}

#[derive(Debug, Deserialize)]
struct KeyData {
    #[serde(default)]
    creator_user_id: Option<String>,
    #[serde(default)]
    organization_id: Option<String>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The account a key belongs to, from the `/api/v1/key` payload.
///
/// The organization wins when set: credits on an org key bill to the org, so
/// two keys created by different members of one org spend the same balance.
/// A payload naming neither is a valid answer of "no account", not an error.
fn account_from_key_response(body: &[u8]) -> Result<Option<String>, FetchError> {
    let response: KeyResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("openrouter key info: {error}")))?;
    Ok(non_empty(response.data.organization_id).or(non_empty(response.data.creator_user_id)))
}

/// Which credential a cached account was resolved for.
///
/// Deliberately not `Debug`: the local variant holds the API key itself, so
/// that a changed key is looked up again, and it must never be formatted.
#[derive(PartialEq, Eq)]
enum CredentialVersion {
    Vault(u64),
    Local(String),
}

/// The account resolved for one handle, and the credential it was resolved for.
struct CachedAccount {
    version: CredentialVersion,
    account: Option<String>,
}

/// Parse a JSON number through the shared money parser without scaling an `f64`.
fn amount_from_number(
    number: &serde_json::Number,
    field: &'static str,
) -> Result<Amount, FetchError> {
    let decimal = number.to_string();
    parse_amount(&decimal, USD).ok_or_else(|| {
        FetchError::Decode(format!("openrouter: {field} is not a readable USD amount"))
    })
}

/// Convert an amount accepted by `parse_amount` to the common nine-decimal scale.
fn input_units(amount: &Amount) -> Option<i128> {
    let shift = INPUT_EXPONENT.checked_sub(amount.exponent)?;
    (amount.minor as i128).checked_mul(10_i128.pow(u32::from(shift)))
}

/// Floor a value in input units to a whole-cent USD [`Amount`].
///
/// FLOOR rather than round-half-up, and the direction is the whole point: these
/// are money figures a consumer may decide to spend against, so overstating one
/// -- even by half a cent -- reports money that is not there. Understating by a
/// fraction of a cent costs nothing anyone can observe.
///
/// Shared by the remainder and the grant so the two cannot round differently. A
/// total that rounded up beside a remainder that floored could publish a
/// remaining greater than the total on the right input, which is a state no
/// consumer should ever have to reason about.
fn floor_units_to_usd(units: i128, field: &str) -> Result<Amount, FetchError> {
    let cents = i64::try_from(units / INPUT_UNITS_PER_CENT)
        .map_err(|_| FetchError::Decode(format!("openrouter: {field} exceeds USD amount range")))?;

    // Format the floored cent value as a USD decimal and pass it through the
    // shared parser instead of constructing minor units directly.
    let cents_per_dollar = 10_i64.pow(u32::from(USD_EXPONENT));
    parse_amount(
        &format!(
            "{}.{:0width$}",
            cents / cents_per_dollar,
            cents % cents_per_dollar,
            width = usize::from(USD_EXPONENT),
        ),
        USD,
    )
    .ok_or_else(|| FetchError::Decode(format!("openrouter: {field} is not a readable USD amount")))
}

/// Convert one reported figure into whole-cent USD.
fn floor_to_cents(amount: Amount, field: &str) -> Result<Amount, FetchError> {
    let units = input_units(&amount).ok_or_else(|| {
        FetchError::Decode(format!("openrouter: {field} exceeds supported precision"))
    })?;
    floor_units_to_usd(units, field)
}

/// Derive a whole-cent USD balance from OpenRouter's total and usage counters.
fn remaining_amount(total: Amount, usage: Amount) -> Result<Amount, FetchError> {
    let total = input_units(&total).ok_or_else(|| {
        FetchError::Decode("openrouter: total_credits exceeds supported precision".to_string())
    })?;
    let usage = input_units(&usage).ok_or_else(|| {
        FetchError::Decode("openrouter: total_usage exceeds supported precision".to_string())
    })?;

    // Clamp before cent rounding. A negative credit balance means the account is
    // overdrawn, not that a router should receive a negative amount to compare.
    let remaining = total.saturating_sub(usage).max(0);
    floor_units_to_usd(remaining, "remaining balance")
}

/// Normalize the OpenRouter credits payload into its one derived credit pool.
pub fn normalize_pools(body: &[u8]) -> Result<Vec<Pool>, FetchError> {
    let response: CreditsResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("openrouter: {error}")))?;
    let total = amount_from_number(&response.data.total_credits, "total_credits")?;
    let usage = amount_from_number(&response.data.total_usage, "total_usage")?;
    // The grant is published beside the remainder because the upstream states
    // it, and without it a consumer sees an amount with no denominator: 5.49 USD
    // left OF WHAT is a different fact from 5.49 USD left. Floored on the same
    // reasoning as the remainder -- this is money, and the direction that
    // overstates is the one that misleads.
    let total_published = floor_to_cents(total.clone(), "total_credits")?;
    let remaining = remaining_amount(total, usage)?;

    Ok(vec![Pool {
        id: "credits".to_string(),
        label: "OpenRouter credits".to_string(),
        // The endpoint calls these credits but never says whether they were
        // bought or comped. A funding guess could make a router spend money.
        funding: PoolFunding::Unknown,
        remaining: Some(remaining),
        total: Some(total_published),
        // OpenRouter states a grant and its consumption, not a remainder.
        basis: PoolBasis::Derived,
        // No per-pool availability signal is present in this payload.
        spendable: None,
        resets_at: None,
    }])
}

/// Resolve the API key while preserving the auth reader's absence/error classes.
fn api_key_from_auth(
    auth: Result<Option<crate::opencode_auth::OpencodeAuth>, FetchError>,
) -> Result<String, FetchError> {
    match auth? {
        Some(crate::opencode_auth::OpencodeAuth::Api { key }) if key.trim().is_empty() => {
            Err(FetchError::CredentialUnusable(
                "openrouter API key in the opencode auth store is empty".to_string(),
            ))
        }
        Some(crate::opencode_auth::OpencodeAuth::Api { key }) => Ok(key),
        Some(crate::opencode_auth::OpencodeAuth::Oauth { .. }) => {
            Err(FetchError::CredentialUnusable(
                "openrouter entry in the opencode auth store is not an API key".to_string(),
            ))
        }
        None => Err(FetchError::NoSession(
            "no openrouter entry in the opencode auth store".to_string(),
        )),
    }
}

pub struct OpenRouterProvider {
    url: String,
    key_url: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    /// Resolved accounts keyed by handle id, one entry per handle, replaced
    /// when that handle's credential changes. Memory only.
    accounts: Mutex<HashMap<String, CachedAccount>>,
}

impl OpenRouterProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            url: CREDITS_URL.to_string(),
            key_url: KEY_URL.to_string(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
            accounts: Mutex::new(HashMap::new()),
        }
    }

    /// The account behind `key`, looked up once per credential.
    ///
    /// Returns none, and caches nothing, when the lookup fails: a missing
    /// identity must not cost the balance reading it would have labelled, and the
    /// next fetch tries again. A successful answer is cached even when it names
    /// no account, because a key's owner does not change.
    async fn resolve_account(
        &self,
        handle_id: &str,
        version: CredentialVersion,
        key: &str,
    ) -> Option<String> {
        if let Some(cached) = self
            .accounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(handle_id)
            .filter(|cached| cached.version == version)
        {
            return cached.account.clone();
        }

        let lookup = JsonRequest::get(&self.key_url)
            .bearer(key)
            .send(&self.http)
            .await
            .and_then(|body| account_from_key_response(&body));
        match lookup {
            Ok(account) => {
                self.accounts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        handle_id.to_string(),
                        CachedAccount {
                            version,
                            account: account.clone(),
                        },
                    );
                account
            }
            Err(error) => {
                // The class only: error text can quote a response body, and
                // nothing about the key belongs in a log line.
                eprintln!(
                    "{LOG_TAG} warning: openrouter account lookup failed ({handle_id}): {}",
                    error.error_class()
                );
                None
            }
        }
    }

    fn report_auth_failure(
        &self,
        handle: &CredentialHandle,
        record_version: u64,
        error: &FetchError,
    ) {
        crate::credential_source::report_vault_auth_failure(
            self.credential_source.as_ref(),
            handle,
            record_version,
            error,
        );
    }

    fn api_key() -> Result<String, FetchError> {
        api_key_from_auth(crate::opencode_auth::read_provider(PROVIDER_NAME))
    }

    async fn fetch_with_key(&self, key: &str) -> Result<Vec<Pool>, FetchError> {
        let body = JsonRequest::get(&self.url)
            .bearer(key)
            .send(&self.http)
            .await?;
        normalize_pools(&body)
    }

    /// Fetch the balance for the local key and label it with the key's account.
    async fn fetch_local_key(&self, handle: &CredentialHandle, key: String) -> FetchAttempt {
        let pools = match self.fetch_with_key(&key).await {
            Ok(pools) => pools,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        let account = self
            .resolve_account(
                handle.stable_id(),
                CredentialVersion::Local(key.clone()),
                &key,
            )
            .await;

        // This endpoint has no rate windows. An otherwise empty Usage beside a
        // non-empty spend list is the balance-only shape accepted by wire_sanity.
        // No account resolved means no observation, as before the lookup existed.
        let observed = account.map(|account| AccountObservation::new(Some(account), None));
        let mut attempt = FetchAttempt::success(observed, "api", Usage::default());
        attempt.pools = Some(pools);
        attempt
    }

    async fn fetch_vault(&self, handle: &CredentialHandle) -> FetchAttempt {
        let handle_id = handle.stable_id();
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
            Err(error) => {
                eprintln!(
                    "{LOG_TAG} warning: openrouter vault credential.get failed ({handle_id}): {error:?}"
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
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
            .map(|response| response.body)
            .and_then(|body| normalize_pools(&body));
        if let Err(error) = &result {
            self.report_auth_failure(handle, record_version, error);
        }
        match result {
            Ok(pools) => {
                let account = self
                    .resolve_account(handle_id, CredentialVersion::Vault(record_version), &key)
                    .await;
                // No account resolved means no observation, which is what this
                // lane published before it looked the account up.
                let observed = account
                    .map(|account| AccountObservation::new(Some(account), Some(record_version)));
                let mut attempt = FetchAttempt::success(observed, "vault", Usage::default());
                attempt.pools = Some(pools);
                attempt
            }
            Err(error) => FetchAttempt::failure(None, Some("vault".to_string()), error),
        }
    }

    #[cfg(test)]
    fn with_url(url: String) -> Self {
        Self {
            url,
            key_url: KEY_URL.to_string(),
            http: crate::http::provider_client(),
            credential_source: None,
            handle_loader: Arc::new(VaultHandleLoader::new(None)),
            accounts: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl UsageProvider for OpenRouterProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.openrouter_handles()?;
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
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }
        let key = match Self::api_key() {
            Ok(key) => key,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        self.fetch_local_key(handle, key).await
    }
}

#[cfg(test)]
mod tests {

    /// The grant is published beside the remainder, not discarded.
    ///
    /// LIVE CAPTURE, 2026-08-16. Without the total a consumer sees an amount
    /// with no denominator: "5.49 USD left" and "5.49 USD left of 25" are
    /// different facts, and only the second supports a proportion. The upstream
    /// states the grant, so dropping it would be this module discarding
    /// something it was told.
    ///
    /// Also the only fixture that gives `wire_sanity`'s remaining-within-total
    /// rule anything to compare -- that rule reported zero comparisons on live
    /// data until this pool carried both figures.
    #[test]
    fn the_grant_is_published_beside_the_remainder() {
        let body = br#"{"data":{"total_credits":25,"total_usage":19.506207297}}"#;
        let pools = normalize_pools(body).expect("a credits payload must publish a pool");
        let pool = &pools[0];

        let total = pool.total.as_ref().expect("the upstream states a grant");
        let remaining = pool.remaining.as_ref().expect("a derived remainder");
        assert_eq!((total.minor, total.exponent), (2500, 2));
        assert_eq!((remaining.minor, remaining.exponent), (549, 2));

        // Not vacuous as a pair: the remainder must be inside the grant, which is
        // the invariant a consumer would otherwise have to take on trust.
        assert!(
            remaining.minor <= total.minor,
            "a remainder outside its grant is a state nobody should have to reason about"
        );
    }

    /// A sub-cent remainder rounds DOWN, never up.
    ///
    /// The distinction is invisible in every other fixture, because whole-cent
    /// figures round identically either way. It matters because this is a
    /// balance a consumer may spend against: rounding up reports money the
    /// account does not have, while rounding down understates by a fraction of a
    /// cent that nobody can observe.
    ///
    /// 0.005 USD is exactly half a cent -- the value where half-up and floor
    /// disagree by construction, so this fixture cannot pass under both.
    #[test]
    fn a_sub_cent_remainder_rounds_down_rather_than_inventing_money() {
        let body = br#"{"data":{"total_credits":1.005,"total_usage":1.0}}"#;
        let pools = normalize_pools(body).expect("a positive balance must publish a pool");
        assert_eq!(pools.len(), 1);
        let amount = pools[0]
            .remaining
            .as_ref()
            .expect("a derived balance states a remainder");
        assert_eq!(
            amount.minor, 0,
            "half a cent must floor to zero: rounding up publishes money the \
             account does not have"
        );
    }
    use super::*;

    /// LIVE CAPTURE, 2026-08-16.
    const LIVE_CAPTURE: &[u8] = br#"{
        "data": {
            "total_credits": 25,
            "total_usage": 19.506207297
        }
    }"#;

    #[test]
    fn live_credits_payload_publishes_one_derived_usd_pool() {
        let pools = normalize_pools(LIVE_CAPTURE).expect("live capture must parse");

        assert_eq!(
            pools.len(),
            1,
            "the payload has one combined credit balance"
        );
        assert_eq!(pools[0].id, "credits");
        assert_eq!(pools[0].funding, PoolFunding::Unknown);
        assert_eq!(pools[0].basis, PoolBasis::Derived);
        assert_eq!(pools[0].spendable, None);
        assert_eq!(
            pools[0].remaining,
            Some(Amount {
                minor: 549,
                exponent: USD_EXPONENT,
                unit: USD.to_string(),
            })
        );
    }

    #[test]
    fn an_overdrawn_account_clamps_the_derived_balance_to_zero() {
        let pools = normalize_pools(br#"{ "data": { "total_credits": 5, "total_usage": 6 } }"#)
            .expect("an overdrawn account is still a valid response");

        assert_eq!(
            pools[0].remaining,
            Some(Amount {
                minor: 0,
                exponent: USD_EXPONENT,
                unit: USD.to_string(),
            }),
            "a router must not receive a negative balance"
        );
    }

    #[test]
    fn missing_total_credits_is_a_decode_failure_not_an_empty_pool_list() {
        let error = normalize_pools(br#"{ "data": { "total_usage": 1 } }"#)
            .expect_err("a total is required to derive a balance");

        assert!(matches!(error, FetchError::Decode(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn a_401_uses_the_shared_credential_rejection_mapping() {
        let (base, request) = crate::loopback::serve_once(401, b"denied".to_vec()).await;
        let provider = OpenRouterProvider::with_url(format!("{base}/api/v1/credits"));

        let error = provider
            .fetch_with_key("test-openrouter-key")
            .await
            .expect_err("401 must reject the credential");

        assert_eq!(error.error_class(), "credential_rejected");
        let request = request.await.expect("loopback request completes");
        assert!(request.starts_with("GET /api/v1/credits "), "{request:?}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-openrouter-key"),
            "{request:?}"
        );
    }

    #[test]
    fn absent_and_unreadable_auth_store_results_keep_their_distinct_classes() {
        let absent = api_key_from_auth(Ok(None)).expect_err("missing entry is no session");
        assert!(matches!(absent, FetchError::NoSession(_)), "got {absent:?}");

        let unreadable = api_key_from_auth(Err(FetchError::CredentialUnusable(
            "auth store cannot be read".to_string(),
        )))
        .expect_err("the reader's unusable classification must propagate");
        assert!(
            matches!(unreadable, FetchError::CredentialUnusable(_)),
            "got {unreadable:?}"
        );
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

    #[test]
    fn an_apikey_handle_routes_to_the_openrouter_provider() {
        let loader = crate::vault_handles::VaultHandleLoader::default();
        loader.install_rows_for_test(&[("apikey:openrouter", "apikey")]);
        let handles = loader.openrouter_handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].stable_id(), "apikey:openrouter");
        assert_eq!(handles[0].vault_credential_type(), Some("apikey"));
    }

    #[test]
    fn vault_handles_replace_the_implicit_local_lane() {
        let loader = Arc::new(crate::vault_handles::VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:openrouter", "apikey")]);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = OpenRouterProvider::new_with_handle_loader(Some(source), loader);
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].is_vault());
        assert_eq!(handles[0].stable_id(), "apikey:openrouter");
    }

    #[test]
    fn implicit_local_lane_survives_when_no_vault_handles_are_mapped() {
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = OpenRouterProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::default()),
        );
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    #[test]
    fn two_handles_for_one_apikey_family_are_refused() {
        let loader = crate::vault_handles::VaultHandleLoader::default();
        loader.install_rows_for_test(&[
            ("apikey:openrouter", "apikey"),
            ("apikey:openrouter:second", "apikey"),
        ]);
        assert!(loader.openrouter_handles().unwrap().is_empty());
    }

    /// The vault lane serves the key and reports a 401 to the store.
    #[tokio::test]
    async fn vault_lane_serves_the_key_and_reports_a_401() {
        let body = br#"{"data":{"total_credits":25,"total_usage":19.506207297}}"#.to_vec();
        let (base, request) = crate::loopback::serve_once(200, body).await;
        let (source, reports) = source(Ok(credential(b"openrouter-vault-key", 9)));
        let mut provider = OpenRouterProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        provider.url = format!("{base}/api/v1/credits");
        // The one-shot server is gone by the account lookup, so it fails fast
        // on loopback instead of reaching the real host.
        provider.key_url = format!("{base}/api/v1/key");
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "apikey:openrouter",
                VaultCapability::new("ckh_openrouter"),
            ))
            .await;
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        let pools = attempt.pools.as_ref().unwrap();
        assert_eq!(pools.len(), 1);
        assert!(request
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer openrouter-vault-key"));
        assert!(reports.lock().unwrap().is_empty());
    }

    /// A loopback OpenRouter that answers `/api/v1/credits` with a balance and
    /// `/api/v1/key` with `key_status` and `key_body`, counting `/key` requests.
    ///
    /// Every response closes its connection, so each request is its own accept
    /// and the count is exact.
    async fn serve_openrouter(
        key_status: u16,
        key_body: &'static str,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let key_requests = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&key_requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request = crate::loopback::read_request(&mut stream).await;
                let (status, body) = if request.starts_with("GET /api/v1/key ") {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (key_status, key_body)
                } else if request.starts_with("GET /api/v1/credits ") {
                    (
                        200,
                        r#"{"data":{"total_credits":25,"total_usage":19.506207297}}"#,
                    )
                } else {
                    (404, "")
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
            }
        });
        (format!("http://{address}"), key_requests)
    }

    fn provider_at(base: &str, source: Option<Arc<dyn CredentialSource>>) -> OpenRouterProvider {
        let mut provider = OpenRouterProvider::new_with_handle_loader(
            source,
            Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        provider.url = format!("{base}/api/v1/credits");
        provider.key_url = format!("{base}/api/v1/key");
        provider
    }

    const PERSONAL_KEY: &str =
        r#"{"data":{"creator_user_id":"user_creator","workspace_id":"w","organization_id":null}}"#;
    const ORG_KEY: &str =
        r#"{"data":{"creator_user_id":"user_creator","organization_id":"org_billing"}}"#;

    fn observed_account(attempt: &FetchAttempt) -> Option<&str> {
        attempt
            .observed
            .as_ref()
            .and_then(|observed| observed.account_id.as_deref())
    }

    /// A personal key's account is the user who created it.
    #[tokio::test]
    async fn a_key_with_a_creator_labels_the_entry_with_that_user() {
        let (base, _) = serve_openrouter(200, PERSONAL_KEY).await;
        let provider = provider_at(&base, None);

        let attempt = provider
            .fetch_local_key(&CredentialHandle::implicit(), "local-key".to_string())
            .await;

        assert_eq!(observed_account(&attempt), Some("user_creator"));
        assert_eq!(attempt.observed.as_ref().unwrap().record_version, None);
        assert_eq!(attempt.pools.as_ref().unwrap().len(), 1);
    }

    /// An org key bills its credits to the org, so the org is the account even
    /// though a user created the key.
    #[tokio::test]
    async fn the_organization_wins_over_the_creator_when_set() {
        let (base, _) = serve_openrouter(200, ORG_KEY).await;
        let provider = provider_at(&base, None);

        let attempt = provider
            .fetch_local_key(&CredentialHandle::implicit(), "local-key".to_string())
            .await;

        assert_eq!(observed_account(&attempt), Some("org_billing"));
    }

    /// A key payload naming neither id publishes the balance with no
    /// observation, exactly as the lane did before the lookup existed.
    #[tokio::test]
    async fn a_key_naming_no_owner_publishes_the_balance_unlabelled() {
        let (base, _) = serve_openrouter(200, r#"{"data":{"workspace_id":"w"}}"#).await;
        let provider = provider_at(&base, None);

        let attempt = provider
            .fetch_local_key(&CredentialHandle::implicit(), "local-key".to_string())
            .await;

        assert!(attempt.observed.is_none());
        assert!(attempt.usage.is_ok());
        assert_eq!(attempt.pools.as_ref().unwrap().len(), 1);
    }

    /// A failing lookup costs the label, never the reading.
    #[tokio::test]
    async fn a_failing_key_lookup_still_publishes_the_balance() {
        let (base, key_requests) = serve_openrouter(500, "upstream down").await;
        let provider = provider_at(&base, None);

        let attempt = provider
            .fetch_local_key(&CredentialHandle::implicit(), "local-key".to_string())
            .await;

        assert_eq!(
            key_requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the lookup must actually have been made and failed"
        );
        assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
        assert!(attempt.observed.is_none());
        let pools = attempt.pools.as_ref().expect("the balance still publishes");
        assert_eq!(pools[0].remaining.as_ref().unwrap().minor, 549);
    }

    /// The owner of a key does not change, so two fetches with one credential
    /// make one lookup -- on the local lane and on the vault lane -- and a new
    /// credential is looked up afresh.
    #[tokio::test]
    async fn the_key_lookup_is_made_once_per_credential() {
        use std::sync::atomic::Ordering;

        let (base, key_requests) = serve_openrouter(200, PERSONAL_KEY).await;
        let provider = provider_at(&base, None);
        let local = CredentialHandle::implicit();
        for _ in 0..2 {
            let attempt = provider
                .fetch_local_key(&local, "local-key".to_string())
                .await;
            assert_eq!(observed_account(&attempt), Some("user_creator"));
        }
        assert_eq!(key_requests.load(Ordering::SeqCst), 1);

        provider
            .fetch_local_key(&local, "a-replaced-key".to_string())
            .await;
        assert_eq!(
            key_requests.load(Ordering::SeqCst),
            2,
            "a different key is a different credential"
        );

        let (vault_base, vault_requests) = serve_openrouter(200, ORG_KEY).await;
        let (source, _) = source(Ok(credential(b"openrouter-vault-key", 7)));
        let provider = provider_at(&vault_base, Some(source));
        let handle = CredentialHandle::vault("apikey:openrouter", VaultCapability::new("ckh_or"));
        for _ in 0..2 {
            let attempt = provider.fetch_handle(&handle).await;
            assert_eq!(
                attempt.observed,
                Some(AccountObservation::new(
                    Some("org_billing".to_string()),
                    Some(7)
                ))
            );
        }
        assert_eq!(vault_requests.load(Ordering::SeqCst), 1);
    }

    /// A 401 on the vault lane is reported to the credential store.
    #[tokio::test]
    async fn vault_401_reports_the_served_version() {
        let (base, _) = crate::loopback::serve_once(401, Vec::new()).await;
        let (source, reports) = source(Ok(credential(b"openrouter-vault-key", 44)));
        let mut provider = OpenRouterProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        provider.url = format!("{base}/api/v1/credits");
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "apikey:openrouter",
                VaultCapability::new("ckh_openrouter"),
            ))
            .await;
        assert!(matches!(
            attempt.usage,
            Err(FetchError::ProviderStatus(401, _))
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
