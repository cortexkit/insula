//! OpenRouter credit balance.
//!
//! OpenRouter's credits endpoint reports a purchased total and cumulative usage,
//! not a remaining balance or rate window. The one truthful signal is therefore a
//! derived USD pool; the endpoint says nothing about whether the credits were
//! purchased or granted, nor whether the pool is presently spendable.
//!
//! - Credential: the `openrouter` API entry in opencode's auth store.
//! - Endpoint: `GET https://openrouter.ai/api/v1/credits`, bearer API key.

use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;

use crate::credential_source::{CredentialSource, VaultCapability};
use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;

const PROVIDER_NAME: &str = "openrouter";
const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";
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
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
}

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::from_env()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            url: CREDITS_URL.to_string(),
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
                    "[ck-quota] warning: openrouter vault credential.get failed ({handle_id}): {error:?}"
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

    #[cfg(test)]
    fn with_url(url: String) -> Self {
        Self {
            url,
            http: crate::http::provider_client(),
            credential_source: None,
            handle_loader: Arc::new(VaultHandleLoader::new(None)),
        }
    }
}

impl Default for OpenRouterProvider {
    fn default() -> Self {
        Self::new()
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
        if let Some(capability) = handle.vault_capability() {
            return self.fetch_vault(handle.stable_id(), capability).await;
        }
        let key = match Self::api_key() {
            Ok(key) => key,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        let pools = match self.fetch_with_key(&key).await {
            Ok(pools) => pools,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };

        // This endpoint has no rate windows. An otherwise empty Usage beside a
        // non-empty spend list is the balance-only shape accepted by wire_sanity.
        let mut attempt = FetchAttempt::success(None, "api", Usage::default());
        attempt.pools = Some(pools);
        attempt
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

    fn write_handles(body: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ck-quota-openrouter-handles-{}-{}.json",
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

    /// An `apikey:openrouter` handle routes to the openrouter provider.
    #[test]
    fn an_apikey_handle_routes_to_the_openrouter_provider() {
        let path = write_handles(r#"{"handles":{"apikey:openrouter":"ckh_openrouter"}}"#);
        let loader = crate::vault_handles::VaultHandleLoader::new(Some(path.clone()));
        let handles = loader.openrouter_handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].stable_id(), "apikey:openrouter");
        assert!(handles[0].vault_capability().is_some());
        let _ = std::fs::remove_file(path);
    }

    /// Vault configured for the family: the local lane is absent.
    #[test]
    fn vault_handles_replace_the_implicit_local_lane() {
        let path = write_handles(r#"{"handles":{"apikey:openrouter":"ckh_openrouter"}}"#);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = OpenRouterProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(Some(path.clone()))),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].vault_capability().is_some());
        assert_eq!(handles[0].stable_id(), "apikey:openrouter");
        let _ = std::fs::remove_file(path);
    }

    /// No vault handle configured: the local lane is present.
    #[test]
    fn implicit_local_lane_survives_when_no_vault_handles_are_mapped() {
        let path = write_handles(r#"{"handles":{"oauth:xai":"ckh_grok"}}"#);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = OpenRouterProvider::new_with_handle_loader(
            Some(source),
            Arc::new(crate::vault_handles::VaultHandleLoader::new(Some(path.clone()))),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles, vec![CredentialHandle::implicit()]);
        let _ = std::fs::remove_file(path);
    }

    /// Two handles for one api-key family are refused at load time.
    #[test]
    fn two_handles_for_one_apikey_family_are_refused() {
        let path = write_handles(
            r#"{"handles":{"apikey:openrouter":"ckh_a","apikey:openrouter:second":"ckh_b"}}"#,
        );
        let loader = crate::vault_handles::VaultHandleLoader::new(Some(path.clone()));
        assert!(loader.openrouter_handles().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
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
        assert!(matches!(attempt.usage, Err(FetchError::ProviderStatus(401))));
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 44)]);
    }
}
