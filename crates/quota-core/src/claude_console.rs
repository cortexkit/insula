//! Claude Console prepaid API credit, read only through a deposited web session.
//!
//! API keys cannot read this balance. The organizations response supplies the
//! identity for each cookie lane, so separate deposits can safely serve separate
//! organizations without trusting the operator's credential labels as identity.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    cookie_vault::CookieVault,
    credential_source::CredentialSource,
    http::{Header, JsonRequest},
    model::{AccountInfo, Amount, Pool, PoolBasis, PoolFunding, Usage},
    provider::{
        AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
    },
    vault_handles::VaultHandleLoader,
};

const PROVIDER_NAME: &str = "claude-console";
const COOKIE_FAMILY: &str = "cookie:platform.claude.com";
const ORGANIZATIONS_URL: &str = "https://platform.claude.com/api/organizations";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const SUBSCRIPTION_PROGRAM: &str = "subscription_api_credit";

#[derive(Deserialize)]
struct Credits {
    amount: i64,
    currency: Option<String>,
    balance: Option<Balance>,
    promo_tranches: Option<Vec<Grant>>,
    tranches: Option<Vec<Grant>>,
}

#[derive(Deserialize)]
struct Balance {
    credits: Option<CreditScale>,
}

#[derive(Deserialize)]
struct CreditScale {
    exponent: Option<u8>,
}

#[derive(Deserialize)]
struct Grant {
    granted_amount_minor_units: i64,
    remaining_amount_minor_units: i64,
    expires_at: Option<Value>,
    name: Option<String>,
    program_id: Option<String>,
    id: Option<String>,
}

fn text(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn decode_error(field: &str) -> FetchError {
    FetchError::Decode(format!("claude-console: could not parse {field}"))
}

/// Preserve reported minor units instead of converting money through floats.
fn normalize_pools(body: &[u8], now: DateTime<Utc>) -> Result<Vec<Pool>, FetchError> {
    let credits: Credits =
        serde_json::from_slice(body).map_err(|_| decode_error("credit balance"))?;
    let currency = text(credits.currency.as_deref()).unwrap_or("USD");
    if currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Err(decode_error("currency"));
    }
    let exponent = credits
        .balance
        .and_then(|balance| balance.credits)
        .and_then(|scale| scale.exponent)
        .unwrap_or(2);
    if exponent > 4 {
        return Err(decode_error("currency exponent"));
    }
    let amount = |minor| Amount {
        minor,
        exponent,
        unit: currency.to_string(),
    };
    let promos = credits.promo_tranches.unwrap_or_default();
    let purchased = credits.tranches.unwrap_or_default();
    let has_grants = !promos.is_empty() || !purchased.is_empty();
    let mut ranked = Vec::new();
    for (field, grants, bought) in [
        ("promo_tranches", promos, false),
        ("tranches", purchased, true),
    ] {
        for (index, grant) in grants.into_iter().enumerate() {
            // Missing or malformed expiry means unknown, not expired. Expiry is
            // never a reset: losing unused credit does not replenish the pool,
            // so it is published as `expires_at` and `resets_at` stays unset.
            let expiry = grant
                .expires_at
                .as_ref()
                .and_then(Value::as_str)
                .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok());
            if grant.granted_amount_minor_units <= 0 || expiry.is_some_and(|at| at <= now) {
                continue;
            }
            let subscription =
                !bought && text(grant.program_id.as_deref()) == Some(SUBSCRIPTION_PROGRAM);
            let funding = if bought {
                PoolFunding::Purchased
            } else if subscription {
                PoolFunding::Subscription
            } else {
                PoolFunding::Granted
            };
            let pool = Pool {
                id: text(grant.id.as_deref())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{field}:{index}")),
                label: text(grant.name.as_deref())
                    .unwrap_or(if bought {
                        "Purchased credit"
                    } else {
                        "API credit"
                    })
                    .to_string(),
                funding,
                remaining: Some(amount(grant.remaining_amount_minor_units.max(0))),
                total: Some(amount(grant.granted_amount_minor_units)),
                basis: PoolBasis::Reported,
                spendable: None,
                resets_at: None,
                // Whole seconds in UTC with a Z, the form pool `resets_at` uses.
                expires_at: expiry.map(|at| {
                    at.with_timezone(&Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                }),
            };
            ranked.push((
                if subscription {
                    0
                } else if bought {
                    1
                } else {
                    2
                },
                pool,
            ));
        }
    }
    // The aggregate covers the same money as the grants. Even if all listed
    // grants were expired or invalidated, publishing it would resurrect credit.
    if !has_grants {
        ranked.push((
            3,
            Pool {
                id: "credits".to_string(),
                label: "API credit balance".to_string(),
                funding: PoolFunding::Unknown,
                remaining: Some(amount(credits.amount.max(0))),
                total: None,
                basis: PoolBasis::Reported,
                spendable: None,
                resets_at: None,
                expires_at: None,
            },
        ));
    }
    ranked.sort_by_key(|(rank, _)| *rank);
    Ok(ranked.into_iter().map(|(_, pool)| pool).collect())
}

struct Organization {
    id: String,
    name: Option<String>,
}

fn api_organizations(body: &[u8]) -> Result<Vec<Organization>, FetchError> {
    let values: Vec<Value> =
        serde_json::from_slice(body).map_err(|_| decode_error("organizations"))?;
    let mut organizations = Vec::new();
    for value in values {
        let object = value
            .as_object()
            .ok_or_else(|| decode_error("organization"))?;
        if !object
            .get("capabilities")
            .and_then(Value::as_array)
            .is_some_and(|items| items.iter().any(|item| item.as_str() == Some("api")))
        {
            continue;
        }
        let id = text(object.get("uuid").and_then(Value::as_str))
            .filter(|id| {
                id.len() <= 64
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
            .ok_or_else(|| decode_error("organization ID"))?;
        organizations.push(Organization {
            id: id.to_string(),
            name: text(object.get("name").and_then(Value::as_str)).map(str::to_string),
        });
    }
    if organizations.is_empty() {
        return Err(FetchError::NoQuotaReported(
            "this login has no Claude Console API organization".to_string(),
        ));
    }
    Ok(organizations)
}

fn billing_denied() -> FetchError {
    FetchError::CredentialUnusable(
        "Claude Console login lacks billing access; ask the organization owner for billing access"
            .to_string(),
    )
}

pub struct ClaudeConsoleProvider {
    vault: CookieVault,
    http: reqwest::Client,
    organizations_url: String,
}

impl ClaudeConsoleProvider {
    pub(crate) fn new_with_handle_loader(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            vault: CookieVault::new(source, loader, COOKIE_FAMILY),
            http: crate::http::provider_client(),
            organizations_url: ORGANIZATIONS_URL.to_string(),
        }
    }

    async fn request(&self, url: &str, cookie: &str, billing: bool) -> Result<Vec<u8>, FetchError> {
        let result = JsonRequest::get(url)
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", cookie))
            .header(Header::new("Accept", "application/json"))
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await;
        match result {
            Ok(response) => Ok(response.body),
            // Billing permission is not a session check. Re-capturing the same
            // login cannot grant it access to an organization's billing data.
            Err(FetchError::ProviderStatus(403, _)) if billing => Err(billing_denied()),
            Err(FetchError::ProviderStatus(401 | 403, _)) => Err(FetchError::Unauthorized(
                "Claude Console session expired; sign in again at https://platform.claude.com/login".to_string())),
            Err(FetchError::ProviderStatus(status, _)) => Err(FetchError::Upstream(format!("Claude Console returned HTTP {status}"))),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl UsageProvider for ClaudeConsoleProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }
    fn is_cookie_based(&self) -> bool {
        true
    }
    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.vault.handles()
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let resolved = async {
            let (jar, _) = self.vault.jar_for(handle).await?;
            if !jar.has_cookie_named(|name| name == "sessionKey") {
                return Err(FetchError::CredentialUnusable(format!(
                    "no sessionKey {}: sign in before capturing",
                    crate::cookie_vault::DEPOSIT_PHRASE
                )));
            }
            let body = self
                .request(&self.organizations_url, &jar.header(), false)
                .await?;
            Ok((jar, api_organizations(&body)?))
        }
        .await;
        let (jar, organizations) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                return FetchAttempt::failure(
                    None,
                    Some(crate::cookie_vault::SOURCE.to_string()),
                    error,
                )
            }
        };
        for org in organizations {
            let url = format!("{}/{}/prepaid/credits", self.organizations_url, org.id);
            let result = self.request(&url, &jar.header(), true).await;
            if matches!(result, Err(FetchError::CredentialUnusable(_))) {
                continue;
            }
            let observed = Some(AccountObservation::new(Some(org.id), None));
            let pools = result.and_then(|body| normalize_pools(&body, Utc::now()));
            let mut attempt = match pools {
                Ok(pools) => {
                    let mut attempt = FetchAttempt::success(
                        observed,
                        crate::cookie_vault::SOURCE,
                        Usage::default(),
                    );
                    attempt.pools = Some(pools);
                    attempt
                }
                Err(error) => FetchAttempt::failure(
                    observed,
                    Some(crate::cookie_vault::SOURCE.to_string()),
                    error,
                ),
            };
            attempt.account_info = Some(AccountInfo {
                org_name: org.name,
                ..Default::default()
            });
            // The wire has no monthly-spend field. Do not fetch current_spend or
            // disguise cumulative consumption as a remaining balance or window.
            return attempt;
        }
        FetchAttempt::failure(
            None,
            Some(crate::cookie_vault::SOURCE.to_string()),
            billing_denied(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_source::{VaultCapability, VaultCredential, VaultGetError};
    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle};

    // Adapted from CodexBar v0.77.0 TestsPlugin/ClaudePlatformPluginTests.swift
    // (MIT). These values exercise a subscription grant beside purchased credit.
    const ORGANIZATIONS: &str = r#"[
      {"uuid":"11111111-1111-4111-8111-111111111111","name":"Synthetic Chat Organization","capabilities":["chat","claude_max"]},
      {"uuid":"22222222-2222-4222-8222-222222222222","name":"Synthetic API Org","capabilities":["api","api_individual","customer_terms:standard"]}]
    "#;
    const API_ORG: &str = "22222222-2222-4222-8222-222222222222";
    const CREDITS: &str = r#"{"amount":18725,"currency":"USD","balance":{"money":null,"credits":{"amount_minor":18725,"exponent":2}},
      "balance_credits":187,"amount_without_scoped_credits":6250,"auto_reload_settings":null,"last_paid_purchase_cents":10000,
      "tranches":[{"remaining_amount_minor_units":6250,"currency":"USD","expires_at":"2027-05-01T00:00:00Z",
        "granted_amount_minor_units":10000,"granted_at":"2026-05-01T00:00:00Z","program_id":"api_credits",
        "name":null,"id":"fixture-purchase","scope":{"kind":"unscoped","applies_to":[]}}],
      "promo_tranches":[{"remaining_amount_minor_units":12475,"currency":"USD","expires_at":"2026-10-28T00:00:00Z",
        "granted_amount_minor_units":20000,"granted_at":"2026-10-01T00:00:00Z","program_id":"subscription_api_credit",
        "name":"API credit (Max 20x)","id":"fixture-promo","scope":{"kind":"scoped","applies_to":[{"product":"api"},{"product":"agent_sdk"}]}}],
      "next_expires_at":"2026-10-28T00:00:00Z"}"#;

    fn now() -> DateTime<Utc> {
        "2026-10-13T00:00:00Z".parse().unwrap()
    }

    fn grant(granted: i64, remaining: i64, program: &str, expiry: Option<&str>) -> Value {
        json!({"granted_amount_minor_units": granted, "remaining_amount_minor_units": remaining,
            "program_id": program, "expires_at": expiry})
    }

    fn pools(value: Value) -> Vec<Pool> {
        normalize_pools(&serde_json::to_vec(&value).unwrap(), now()).unwrap()
    }

    #[test]
    fn grants_publish_no_aggregate_pool() {
        let pools = normalize_pools(CREDITS.as_bytes(), now()).unwrap();
        assert_eq!(pools.len(), 2, "aggregate must not accompany grants");
        assert_eq!(pools[0].id, "fixture-promo");
        assert_eq!(pools[0].label, "API credit (Max 20x)");
        assert_eq!(pools[0].funding, PoolFunding::Subscription);
        assert_eq!(
            pools[0].remaining,
            Some(Amount {
                minor: 12475,
                exponent: 2,
                unit: "USD".to_string()
            })
        );
        assert_eq!(pools[0].total.as_ref().unwrap().minor, 20000);
        assert_eq!(pools[1].id, "fixture-purchase");
        assert_eq!(pools[1].remaining.as_ref().unwrap().minor, 6250);
        assert_eq!(pools[1].total.as_ref().unwrap().minor, 10000);
        assert_eq!(pools[1].funding, PoolFunding::Purchased);
        assert!(pools.iter().all(|pool| pool.basis == PoolBasis::Reported
            && pool.resets_at.is_none()
            && pool.spendable.is_none()));
    }

    /// Each grant publishes its own stated expiry as `expires_at`, never as a
    /// reset: lapsed credit isn't replaced by the same pool refilling.
    #[test]
    fn each_grant_publishes_its_expiry_and_no_reset() {
        let listed = normalize_pools(CREDITS.as_bytes(), now()).unwrap();
        assert_eq!(
            listed[0].expires_at.as_deref(),
            Some("2026-10-28T00:00:00Z")
        );
        assert_eq!(
            listed[1].expires_at.as_deref(),
            Some("2027-05-01T00:00:00Z")
        );
        assert!(listed.iter().all(|pool| pool.resets_at.is_none()));

        // An offset expiry is published in UTC with whole seconds.
        let offset = pools(json!({"amount": 100, "promo_tranches": [grant(
            100, 100, SUBSCRIPTION_PROGRAM, Some("2026-11-06T02:00:00.123+02:00"))]}));
        assert_eq!(
            offset[0].expires_at.as_deref(),
            Some("2026-11-06T00:00:00Z")
        );

        // Missing or malformed expiry is unknown: the grant still publishes.
        for expiry in [None, Some("next month")] {
            let unknown = pools(json!({"amount": 100, "promo_tranches": [grant(
                100, 100, SUBSCRIPTION_PROGRAM, expiry)]}));
            assert_eq!(unknown.len(), 1, "{expiry:?}");
            assert_eq!(unknown[0].expires_at, None, "{expiry:?}");
        }

        // The aggregate balance states no expiry, so it publishes none.
        let aggregate = pools(json!({"amount": 18725}));
        assert_eq!(aggregate[0].expires_at, None);
    }

    #[test]
    fn no_grants_publishes_only_aggregate() {
        for extra in [
            json!({}),
            json!({"promo_tranches": [], "tranches": []}),
            json!({"promo_tranches": null, "tranches": null}),
        ] {
            let mut value = json!({"amount": 18725});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let pools = pools(value);
            assert_eq!(pools.len(), 1);
            assert_eq!(pools[0].id, "credits");
            assert_eq!(
                pools[0].remaining,
                Some(Amount {
                    minor: 18725,
                    exponent: 2,
                    unit: "USD".to_string()
                })
            );
            assert_eq!(pools[0].funding, PoolFunding::Unknown);
            assert!(pools[0].total.is_none());
        }
    }

    #[test]
    fn exponent_zero_two_four_preserves_minor_units_and_currency() {
        for (exponent, major) in [(0, 18725.0), (2, 187.25), (4, 1.8725)] {
            let pools = pools(
                json!({"amount": 18725,"currency":"EUR","balance":{"credits":{"exponent":exponent}},
                "promo_tranches":[grant(20000,12475,SUBSCRIPTION_PROGRAM,None)]}),
            );
            let remaining = pools[0].remaining.as_ref().unwrap();
            assert_eq!(remaining.minor, 12475);
            assert_eq!(remaining.exponent, exponent);
            assert_eq!(remaining.unit, "EUR");
            let aggregate = pools_for_aggregate(exponent);
            assert_eq!(
                aggregate.minor as f64 / 10_f64.powi(i32::from(aggregate.exponent)),
                major
            );
            assert_eq!(pools[0].total.as_ref().unwrap().exponent, exponent);
        }
    }

    fn pools_for_aggregate(exponent: u8) -> Amount {
        pools(json!({"amount":18725,"balance":{"credits":{"exponent":exponent}}}))[0]
            .remaining
            .clone()
            .unwrap()
    }

    #[test]
    fn subscription_then_purchased_then_other_promotions() {
        let pools = pools(json!({"amount": 1000,
            "promo_tranches": [grant(100,50,"promo",None),grant(200,150,SUBSCRIPTION_PROGRAM,None)],
            "tranches": [grant(300,250,"api_credits",None)]}));
        assert_eq!(
            pools.iter().map(|pool| pool.funding).collect::<Vec<_>>(),
            [
                PoolFunding::Subscription,
                PoolFunding::Purchased,
                PoolFunding::Granted
            ]
        );
        assert_eq!(
            pools
                .iter()
                .map(|pool| pool.remaining.as_ref().unwrap().minor)
                .collect::<Vec<_>>(),
            [150, 250, 50]
        );
        assert_ne!(pools[0].id, pools[2].id);
    }

    #[test]
    fn expired_and_nonpositive_grants_are_skipped_without_resurrecting_aggregate() {
        let pools = pools(json!({"amount":18725,"promo_tranches":[
            grant(100,50,SUBSCRIPTION_PROGRAM,Some("2026-10-13T00:00:00Z")),
            grant(0,100,"promo",None), grant(-1,100,"promo",None)],
            "tranches":[grant(100,50,"api_credits",Some("2026-10-12T00:00:00Z"))]}));
        assert!(pools.is_empty());
    }

    #[test]
    fn negative_remaining_is_clamped_and_unknown_expiry_is_allowed() {
        let pools = pools(json!({"amount": -1,"promo_tranches":[
            grant(100,-1,SUBSCRIPTION_PROGRAM,None), grant(100,50,"promo",Some("2026-10-31Tnot-a-date")),
            {"granted_amount_minor_units":100,"remaining_amount_minor_units":20}]}));
        assert_eq!(pools.len(), 3);
        assert_eq!(pools[0].remaining.as_ref().unwrap().minor, 0);
        assert_eq!(pools[1].remaining.as_ref().unwrap().minor, 50);
        assert_eq!(pools[2].remaining.as_ref().unwrap().minor, 20);
        assert!(pools.iter().all(|pool| pool.resets_at.is_none()));
        assert_eq!(pools_for_aggregate(2).exponent, 2);
    }

    #[test]
    fn malformed_credit_payloads_fail_closed() {
        for raw in [
            r#"{"amount":null,"currency":"USD"}"#,
            r#"{"amount":"18725","currency":"USD"}"#,
            r#"{"amount":187.25,"currency":"USD"}"#,
            r#"{"amount":1e99,"currency":"USD"}"#,
            r#"{"amount":18725,"currency":"USD","tranches":{}}"#,
            r#"{"amount":1,"balance":{"credits":{"exponent":5}}}"#,
            r#"{"amount":1,"balance":{"credits":{"exponent":-1}}}"#,
            r#"{"amount":1,"balance":{"credits":{"exponent":2.5}}}"#,
            r#"{"amount":1,"currency":"usd"}"#,
            r#"{"amount":1,"promo_tranches":[{}]}"#,
        ] {
            assert_eq!(
                normalize_pools(raw.as_bytes(), now())
                    .unwrap_err()
                    .error_class(),
                "decode_failed",
                "{raw}"
            );
        }
    }

    struct CookieSource;

    #[async_trait]
    impl CredentialSource for CookieSource {
        async fn get(&self, _: &VaultCapability, _: u64) -> Result<VaultCredential, VaultGetError> {
            Err(VaultGetError::FailClosed)
        }

        async fn get_scoped(&self, id: &str, ttl: u64) -> Result<VaultCredential, VaultGetError> {
            assert_eq!(ttl, 120_000);
            Ok(VaultCredential {
                payload: format!("sessionKey=synthetic-{}", id.rsplit(':').next().unwrap())
                    .into_bytes(),
                expires_at_ms: None,
                record_version: 1,
                account_id: None,
                email: None,
                org_name: None,
                project_id: None,
            })
        }

        async fn report_auth_failure(&self, _: &VaultCapability, _: u16, _: u64) {
            panic!("cookie sessions cannot be refreshed by the vault");
        }

        async fn report_auth_failure_scoped(&self, _: &str, _: u16, _: u64) {
            panic!("cookie sessions cannot be refreshed by the vault");
        }
    }

    // Each step pins the path as well as the status so a chat organization's
    // billing URL cannot accidentally satisfy a test meant for an API owner.
    async fn server(steps: Vec<(String, u16, String)>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (path, status, body) in steps {
                let Ok(accepted) =
                    tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
                else {
                    break;
                };
                let (mut stream, _) = accepted.unwrap();
                let request = crate::loopback::read_request(&mut stream).await;
                assert!(
                    request.starts_with(&format!("GET {path} HTTP/1.1\r\n")),
                    "{request}"
                );
                let lower = request.to_ascii_lowercase();
                assert!(lower.contains("accept: application/json\r\n"));
                assert!(lower.contains("cookie: sessionkey=synthetic-"));
                assert!(!lower.contains("authorization:"));
                assert!(request.ends_with("\r\n\r\n"), "GET must be bodyless");
                stream.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                requests.push(request);
            }
            requests
        });
        (format!("http://{address}/api/organizations"), task)
    }

    fn step(path: &str, status: u16, body: &str) -> (String, u16, String) {
        (path.to_string(), status, body.to_string())
    }

    fn credits_path(org: &str) -> String {
        format!("/api/organizations/{org}/prepaid/credits")
    }

    fn provider(url: String, loader: Arc<VaultHandleLoader>) -> ClaudeConsoleProvider {
        let mut provider =
            ClaudeConsoleProvider::new_with_handle_loader(Some(Arc::new(CookieSource)), loader);
        provider.organizations_url = url;
        provider
    }

    async fn fetch(steps: Vec<(String, u16, String)>) -> FetchAttempt {
        let (url, requests) = server(steps).await;
        let provider = provider(url, Arc::new(VaultHandleLoader::default()));
        let result = provider
            .fetch_handle(&CredentialHandle::scoped(
                "cookie:platform.claude.com:first",
                "browser-session",
            ))
            .await;
        requests.await.unwrap();
        result
    }

    #[tokio::test]
    async fn first_readable_api_org_wins_after_billing_forbidden() {
        let organizations = json!([
            {"uuid":"chat","capabilities":["chat"]},
            {"uuid":"member","name":"Member Org","capabilities":["api"]},
            {"uuid":API_ORG,"name":"Synthetic Owner Org","capabilities":["api"]},
            {"uuid":"unused","capabilities":["api"]}])
        .to_string();
        let attempt = fetch(vec![
            step("/api/organizations", 200, &organizations),
            step(&credits_path("member"), 403, "not-json"),
            step(&credits_path(API_ORG), 200, r#"{"amount":18725}"#),
        ])
        .await;
        assert!(attempt.usage.is_ok());
        assert!(attempt.usage.unwrap().primary.is_none());
        assert_eq!(
            attempt.observed.unwrap().account_id.as_deref(),
            Some(API_ORG)
        );
        assert_eq!(
            attempt.account_info.unwrap().org_name.as_deref(),
            Some("Synthetic Owner Org")
        );
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt.pools.unwrap()[0].remaining.as_ref().unwrap().minor,
            18725
        );
    }

    #[tokio::test]
    async fn org_list_401_and_403_are_rejected_sessions() {
        for status in [401, 403] {
            let attempt = fetch(vec![step("/api/organizations", status, "not-json")]).await;
            let error = attempt.usage.unwrap_err();
            assert_eq!(error.error_class(), "credential_rejected");
            assert!(error.to_string().contains("sign in again"));
        }
    }

    #[tokio::test]
    async fn all_billing_403_is_unusable_not_expired() {
        let organizations = json!([
            {"uuid":"first","capabilities":["api"]},{"uuid":"second","capabilities":["api"]}])
        .to_string();
        let attempt = fetch(vec![
            step("/api/organizations", 200, &organizations),
            step(&credits_path("first"), 403, "{}"),
            step(&credits_path("second"), 403, "{}"),
        ])
        .await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(
            error.error_class(),
            "credential_unusable",
            "billing denial is not session expiry"
        );
        assert!(error.to_string().contains("lacks billing access"));
    }

    #[tokio::test]
    async fn no_api_org_reports_no_quota() {
        let attempt = fetch(vec![step(
            "/api/organizations",
            200,
            r#"[{"uuid":"chat","capabilities":["chat"]}]"#,
        )])
        .await;
        assert_eq!(
            attempt.usage.unwrap_err().error_class(),
            "no_quota_reported"
        );
    }

    #[tokio::test]
    async fn rate_limits_server_errors_and_transport_are_transient() {
        for status in [429, 500, 503] {
            for billing in [false, true] {
                let steps = if billing {
                    vec![
                        step("/api/organizations", 200, ORGANIZATIONS),
                        step(&credits_path(API_ORG), status, "{}"),
                    ]
                } else {
                    vec![step("/api/organizations", status, "{}")]
                };
                assert!(matches!(
                    fetch(steps).await.usage,
                    Err(FetchError::Upstream(_))
                ));
            }
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!(
            "http://{}/api/organizations",
            listener.local_addr().unwrap()
        );
        drop(listener);
        let provider = provider(url, Arc::new(VaultHandleLoader::default()));
        assert!(matches!(
            provider
                .fetch_handle(&CredentialHandle::scoped(COOKIE_FAMILY, "browser-session"))
                .await
                .usage,
            Err(FetchError::Upstream(_))
        ));
    }

    #[tokio::test]
    async fn malformed_responses_are_decode_failed() {
        for body in [
            "not-json",
            "{}",
            "[null]",
            r#"[{"uuid":"../escape","capabilities":["api"]}]"#,
        ] {
            assert_eq!(
                fetch(vec![step("/api/organizations", 200, body)])
                    .await
                    .usage
                    .unwrap_err()
                    .error_class(),
                "decode_failed"
            );
        }
        for body in ["not-json", r#"{"amount":null}"#] {
            let attempt = fetch(vec![
                step("/api/organizations", 200, ORGANIZATIONS),
                step(&credits_path(API_ORG), 200, body),
            ])
            .await;
            assert_eq!(attempt.usage.unwrap_err().error_class(), "decode_failed");
            assert_eq!(
                attempt.observed.unwrap().account_id.as_deref(),
                Some(API_ORG)
            );
        }
    }

    #[tokio::test]
    async fn unavailable_spend_cannot_discard_balance() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!(
            "http://{}/api/organizations",
            listener.local_addr().unwrap()
        );
        let (finished, mut stop) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut spend_calls = 0;
            loop {
                let accepted = tokio::select! {
                    _ = &mut stop => break,
                    accepted = listener.accept() => accepted.unwrap(),
                };
                let (mut stream, _) = accepted;
                let request = crate::loopback::read_request(&mut stream).await;
                let (status, body) = if request.starts_with("GET /api/organizations ") {
                    (200, ORGANIZATIONS)
                } else if request.starts_with(&format!("GET {} ", credits_path(API_ORG))) {
                    (200, r#"{"amount":18725}"#)
                } else {
                    assert!(request
                        .starts_with(&format!("GET /api/organizations/{API_ORG}/current_spend ")));
                    spend_calls += 1;
                    (500, "spend unavailable")
                };
                stream.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
            spend_calls
        });
        let attempt = provider(url, Arc::new(VaultHandleLoader::default()))
            .fetch_handle(&CredentialHandle::scoped(COOKIE_FAMILY, "browser-session"))
            .await;
        finished.send(()).unwrap();
        assert!(attempt.usage.is_ok());
        assert_eq!(
            attempt.pools.unwrap()[0].remaining.as_ref().unwrap().minor,
            18725
        );
        assert_eq!(
            task.await.unwrap(),
            0,
            "there is no honest wire field for monthly spend, so do not request it"
        );
    }

    #[tokio::test]
    async fn two_deposits_resolve_distinct_orgs_and_same_org_deduplicates() {
        for same_org in [false, true] {
            let second_org = if same_org { "org-first" } else { "org-second" };
            let first = r#"[{"uuid":"org-first","name":"First Org","capabilities":["api"]}]"#;
            let second =
                json!([{"uuid":second_org,"name":"Second Org","capabilities":["api"]}]).to_string();
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let url = format!(
                "http://{}/api/organizations",
                listener.local_addr().unwrap()
            );
            let requests = tokio::spawn(async move {
                let mut requests = Vec::new();
                for _ in 0..4 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = crate::loopback::read_request(&mut stream).await;
                    let is_first = request.contains("sessionKey=synthetic-first\r\n");
                    let org = if is_first { "org-first" } else { second_org };
                    let body = if request.starts_with("GET /api/organizations ") {
                        if is_first {
                            first
                        } else {
                            &second
                        }
                    } else {
                        assert!(request.starts_with(&format!("GET {} ", credits_path(org))));
                        if is_first {
                            r#"{"amount":100}"#
                        } else {
                            r#"{"amount":200}"#
                        }
                    };
                    stream.write_all(format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    requests.push(request);
                }
                requests
            });
            let loader = Arc::new(VaultHandleLoader::default());
            loader.install_rows_for_test(&[
                ("cookie:platform.claude.com:first", "browser-session"),
                ("cookie:platform.claude.com:second", "browser-session"),
            ]);
            let provider = provider(url, loader);
            assert_eq!(provider.handles().unwrap().len(), 2);
            let registry = crate::Registry::new(vec![Box::new(provider)]);
            // Both real cookie fetches must settle before identity-aware emission
            // can compare them, regardless of the scheduler's per-turn budget.
            for _ in 0..2 {
                registry
                    .refresh_tick(&tokio_util::sync::CancellationToken::new())
                    .await;
            }
            let entries = registry.get_usage(Some(PROVIDER_NAME)).await;
            let requests = requests.await.unwrap();
            assert_eq!(requests.len(), 4);
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.contains("synthetic-first"))
                    .count(),
                2
            );
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.contains("synthetic-second"))
                    .count(),
                2
            );
            assert_eq!(entries.len(), if same_org { 1 } else { 2 });
            assert!(entries.iter().all(|entry| entry.api_provider.is_none()
                && entry.usage.as_ref().unwrap().primary.is_none()));
            assert_eq!(entries[0].account.as_deref(), Some("org-first"));
            if !same_org {
                assert_eq!(entries[1].account.as_deref(), Some("org-second"));
            }
        }
    }

    #[test]
    fn no_deposit_has_no_lane_and_other_cookie_families_still_refuse_two() {
        let loader = Arc::new(VaultHandleLoader::default());
        let provider = provider(ORGANIZATIONS_URL.to_string(), Arc::clone(&loader));
        assert!(provider.handles().unwrap().is_empty());
        loader.install_rows_for_test(&[
            ("cookie:platform.claude.com:first", "browser-session"),
            ("cookie:platform.claude.com:second", "browser-session"),
            ("cookie:ampcode.com:first", "browser-session"),
            ("cookie:ampcode.com:second", "browser-session"),
        ]);
        assert_eq!(provider.handles().unwrap().len(), 2);
        assert!(
            loader.amp_handles().unwrap().is_empty(),
            "identity-less cookie families must still refuse two deposits"
        );
    }
}
