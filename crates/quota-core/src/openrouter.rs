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

use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, UsageProvider};

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
    let cents = remaining
        .checked_add(INPUT_UNITS_PER_CENT / 2)
        .map(|value| value / INPUT_UNITS_PER_CENT)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| {
            FetchError::Decode("openrouter: remaining balance exceeds USD amount range".to_string())
        })?;

    // Format the rounded cent value as a USD decimal and pass it through the
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
    .ok_or_else(|| {
        FetchError::Decode("openrouter: remaining balance is not a readable USD amount".to_string())
    })
}

/// Normalize the OpenRouter credits payload into its one derived credit pool.
pub fn normalize_pools(body: &[u8]) -> Result<Vec<Pool>, FetchError> {
    let response: CreditsResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("openrouter: {error}")))?;
    let total = amount_from_number(&response.data.total_credits, "total_credits")?;
    let usage = amount_from_number(&response.data.total_usage, "total_usage")?;
    let remaining = remaining_amount(total, usage)?;

    Ok(vec![Pool {
        id: "credits".to_string(),
        label: "OpenRouter credits".to_string(),
        // The endpoint calls these credits but never says whether they were
        // bought or comped. A funding guess could make a router spend money.
        funding: PoolFunding::Unknown,
        remaining: Some(remaining),
        total: None,
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
}

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self {
            url: CREDITS_URL.to_string(),
            http: reqwest::Client::new(),
        }
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

    #[cfg(test)]
    fn with_url(url: String) -> Self {
        Self {
            url,
            http: reqwest::Client::new(),
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

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
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
}
