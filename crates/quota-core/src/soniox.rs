//! Soniox — an organization's prepaid balance and monthly budget, read with an
//! organization API key.
//!
//! - Endpoints, on `https://api.soniox.com` only, bearer organization API key:
//!   `GET /v1/organization/balance` (`available_balance_usd`,
//!   `invoice_billing_enabled`), then `GET /v1/organization/usage/monthly`
//!   (`usage_usd`, `monthly_budget_usd`, `month` as `YYYY-MM`). Amounts arrive
//!   as decimal strings.
//! - Reference: CodexBar v0.80.0
//!   `Sources/CodexBarCore/Resources/Plugins/soniox.ts` and `docs/soniox.md`,
//!   which cite Soniox's organization API documentation. Tested against
//!   fixtures shaped from upstream's `SonioxPluginTests.swift` only; no Soniox
//!   account was available when this was written.
//!
//! WHAT IS PUBLISHED, and only what upstream reads:
//!
//! - A `primary` window for the monthly budget, when the budget is positive:
//!   `usedPercent` is `usage_usd / monthly_budget_usd × 100`, clamped to 0-100
//!   as upstream clamps it. It resets at 00:00 UTC on the 1st of the month
//!   after the reported `month`, so it carries `windowKind: monthly` and no
//!   `windowMinutes` (a month has no fixed length). A zero budget publishes no
//!   window: there is no denominator, and upstream invents none either.
//! - A `balance` pool for the prepaid balance, exactly as stated (zero and
//!   negative included), but only when invoice billing is off. An invoiced
//!   organization reports a balance that is not money it holds, and upstream
//!   drops it so a zero is never read as exhausted credit.
//! - Nothing else. The budget is a spending cap, not money held, so it is a
//!   window rather than a pool. When neither exists (an invoiced organization
//!   with no budget) the reading is `no_quota_reported`, never an empty success.
//!
//! Any field upstream would refuse fails the whole reading as `decode_failed`,
//! rather than publishing a zero. The two requests are sequential, as upstream
//! sends them: an unreadable balance never reaches the usage endpoint.
//!
//! Credential: a vault deposit under `apikey:soniox` when one exists, otherwise
//! `SONIOX_ORG_API_KEY`, the only variable upstream reads. A project key
//! (`SONIOX_API_KEY`) cannot read organization billing and is not read.

use async_trait::async_trait;
use chrono::NaiveDate;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::credential_source::CredentialSource;
use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, RateWindow, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;
use crate::vercel::{default_env, EnvLookup};

pub const PROVIDER_NAME: &str = "soniox";
/// The only origin the organization key is ever sent to.
const API_BASE: &str = "https://api.soniox.com";
const BALANCE_PATH: &str = "/v1/organization/balance";
const MONTHLY_PATH: &str = "/v1/organization/usage/monthly";
const KEY_ENV: &str = "SONIOX_ORG_API_KEY";
/// Upstream bounds every Soniox request to 15 seconds.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const USD: &str = "USD";

/// Decode failures never quote the body: an unexpected answer is not
/// something to publish, and it may echo the request.
fn decode(what: &str) -> FetchError {
    FetchError::Decode(format!("soniox: {what}"))
}

/// A decimal string held to upstream's exact shape, `^-?\d+(?:\.\d+)?$` after
/// trimming, returned trimmed.
///
/// Upstream also refuses a value whose number is not finite (a digit string
/// too long for a double), so that is refused here too. `signed` allows a
/// negative value, which only the prepaid balance may be.
fn decimal_text<'a>(
    value: Option<&'a Value>,
    field: &str,
    signed: bool,
) -> Result<(&'a str, f64), FetchError> {
    let refused = || decode(&format!("{field} is not a decimal string upstream accepts"));
    let text = value.and_then(Value::as_str).ok_or_else(refused)?.trim();
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let all_digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(whole) || fraction.is_some_and(|fraction| !all_digits(fraction)) {
        return Err(refused());
    }
    let number: f64 = text.parse().map_err(|_| refused())?;
    // `-0` passes upstream's `amount < 0` test, and passes this one.
    if !number.is_finite() || (!signed && number < 0.0) {
        return Err(refused());
    }
    Ok((text, number))
}

fn object(body: &[u8], what: &str) -> Result<serde_json::Map<String, Value>, FetchError> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(decode(&format!("the {what} answer is not a JSON object"))),
        Err(_) => Err(decode(&format!("the {what} answer is not JSON"))),
    }
}

/// The `/balance` answer.
#[derive(Debug, Clone, PartialEq)]
struct Balance {
    available: Amount,
    invoice_billing: bool,
}

fn parse_balance(body: &[u8]) -> Result<Balance, FetchError> {
    let map = object(body, "balance")?;
    let (text, _) = decimal_text(
        map.get("available_balance_usd"),
        "available_balance_usd",
        true,
    )?;
    // The shared parser keeps the stated precision as minor units. It refuses
    // more than nine decimal places, which upstream would accept; such a
    // balance fails the reading rather than being rounded.
    let available = parse_amount(text, USD)
        .ok_or_else(|| decode("available_balance_usd has more precision than can be published"))?;
    let invoice_billing = map
        .get("invoice_billing_enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| decode("invoice_billing_enabled is not a Boolean"))?;
    Ok(Balance {
        available,
        invoice_billing,
    })
}

/// The `/usage/monthly` answer.
#[derive(Debug, Clone, PartialEq)]
struct Monthly {
    used: f64,
    budget: f64,
    /// 00:00 UTC on the 1st of the month after the reported one.
    resets_at: String,
}

/// The start of the month after a `YYYY-MM` month, upstream's reset rule.
///
/// The month must be exactly four digits, a dash and `01`-`12`, as upstream's
/// pattern requires. The reset follows the month Soniox reports, not the
/// host's clock.
fn next_month_start(month: &str) -> Option<String> {
    let bytes = month.as_bytes();
    if bytes.len() != 7 || bytes[4] != b'-' {
        return None;
    }
    let (year, month) = (&month[..4], &month[5..]);
    if !year
        .bytes()
        .chain(month.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let year: i32 = year.parse().ok()?;
    let month: u32 = month.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let (year, month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let start = NaiveDate::from_ymd_opt(year, month, 1)?
        .and_hms_opt(0, 0, 0)?
        .and_utc();
    crate::env::epoch_to_iso8601(start.timestamp())
}

fn parse_monthly(body: &[u8]) -> Result<Monthly, FetchError> {
    let map = object(body, "monthly usage")?;
    let (_, used) = decimal_text(map.get("usage_usd"), "usage_usd", false)?;
    let (_, budget) = decimal_text(map.get("monthly_budget_usd"), "monthly_budget_usd", false)?;
    let resets_at = map
        .get("month")
        .and_then(Value::as_str)
        .and_then(next_month_start)
        .ok_or_else(|| decode("month is not YYYY-MM"))?;
    Ok(Monthly {
        used,
        budget,
        resets_at,
    })
}

/// The window and pools one balance and one month publish.
fn reading(balance: &Balance, monthly: &Monthly) -> Result<(Usage, Vec<Pool>), FetchError> {
    let primary = (monthly.budget > 0.0).then(|| RateWindow {
        used_percent: (monthly.used / monthly.budget * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: Some(monthly.resets_at.clone()),
        window_minutes: None,
        window_kind: Some(cortexkit_provider_usage::window_kind::MONTHLY.to_string()),
        // Dollars, not a count of things, so neither count field applies.
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    });
    let mut pools = Vec::new();
    if !balance.invoice_billing {
        pools.push(Pool {
            id: "balance".to_string(),
            label: "Prepaid balance".to_string(),
            // One figure for whatever credit the organization holds; Soniox does
            // not say whether it was bought or given.
            funding: PoolFunding::Unknown,
            remaining: Some(balance.available.clone()),
            // The monthly budget caps spending; it is not the size of this pool.
            total: None,
            basis: PoolBasis::Reported,
            // No enable flag is stated, and a positive balance is not one.
            spendable: None,
            resets_at: None,
            expires_at: None,
            unlimited: None,
        });
    }
    if primary.is_none() && pools.is_empty() {
        return Err(FetchError::NoQuotaReported(
            "soniox: invoice billing with no monthly budget states no balance and no limit"
                .to_string(),
        ));
    }
    let usage = Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    };
    Ok((usage, pools))
}

pub struct SonioxProvider {
    base: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    env: EnvLookup,
}

impl SonioxProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            base: API_BASE.to_string(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
            env: default_env(),
        }
    }

    async fn get(&self, path: &str, key: &str, vault: bool) -> Result<Vec<u8>, FetchError> {
        let request = JsonRequest::get(format!("{}{path}", self.base))
            .bearer(key)
            .timeout(REQUEST_TIMEOUT);
        // Only the vault lane keeps the numeric status, which the shared gate
        // needs to report a 401 to the vault; the environment has nobody to tell.
        if vault {
            request
                .send_provider_status_first(&self.http, PROVIDER_NAME)
                .await
                .map(|response| response.body)
        } else {
            request.send(&self.http).await
        }
    }

    async fn read(&self, key: &str, vault: bool) -> Result<(Usage, Vec<Pool>), FetchError> {
        let balance = self
            .get(BALANCE_PATH, key, vault)
            .await
            .and_then(|body| parse_balance(&body))
            .map_err(|error| error.stage("balance"))?;
        let monthly = self
            .get(MONTHLY_PATH, key, vault)
            .await
            .and_then(|body| parse_monthly(&body))
            .map_err(|error| error.stage("usage/monthly"))?;
        reading(&balance, &monthly)
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
                    "the apikey:soniox deposit holds no organization API key".to_string(),
                ),
            );
        }
        let result = self.read(&key, true).await;
        if let Err(error) = &result {
            // The shared gate reports only a 401 on this served key: a 403 is
            // Soniox refusing a permission the key lacks, not a dead key.
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
                FetchError::NoSession(format!("no {KEY_ENV} and no apikey:soniox vault deposit")),
            );
        };
        attempt_from(self.read(&key, false).await, "api")
    }
}

fn attempt_from(result: Result<(Usage, Vec<Pool>), FetchError>, source: &str) -> FetchAttempt {
    match result {
        Ok((usage, pools)) => {
            let mut attempt = FetchAttempt::success(None, source, usage);
            // Always present, even empty: Soniox was asked, and for an invoiced
            // organization it states there is no prepaid balance. An empty list
            // says exactly that, where an absent one would say nothing.
            attempt.pools = Some(pools);
            attempt
        }
        Err(error) => FetchAttempt::failure(None, Some(source.to_string()), error),
    }
}

#[async_trait]
impl UsageProvider for SonioxProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.soniox_handles()?;
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
    use crate::vercel::testkit::{credential, routes, Log, MockSource};

    // Upstream's own fixtures, from `SonioxPluginTests.swift`.
    const BALANCE: &str = r#"{"available_balance_usd":"18.50","invoice_billing_enabled":false}"#;
    const MONTHLY: &str = r#"{"month":"2026-10","usage_usd":"6.00","monthly_budget_usd":"100.00"}"#;
    const KEY: &str = "snx_org_fixture";

    fn provider(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
        base: &str,
        env: &[(&str, &str)],
    ) -> SonioxProvider {
        let mut provider = SonioxProvider::new_with_handle_loader(source, loader);
        provider.base = base.to_string();
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
        CredentialHandle::scoped("apikey:soniox", "apikey")
    }

    /// A loopback Soniox answering each path with its own body, or with
    /// `status` and a private marker on `failing`.
    async fn soniox(
        balance: &str,
        monthly: &str,
        failing: Option<(&'static str, u16)>,
    ) -> (String, Log) {
        let (balance, monthly) = (balance.to_string(), monthly.to_string());
        routes(move |path, _| {
            if let Some((failing_path, status)) = failing {
                if path == failing_path {
                    return (status, b"private-response-marker".to_vec());
                }
            }
            match path {
                BALANCE_PATH => (200, balance.as_bytes().to_vec()),
                MONTHLY_PATH => (200, monthly.as_bytes().to_vec()),
                _ => (404, Vec::new()),
            }
        })
        .await
    }

    /// Read through the environment lane against a loopback Soniox.
    async fn fetch_env(balance: &str, monthly: &str) -> (FetchAttempt, Log) {
        let (base, served) = soniox(balance, monthly, None).await;
        let provider = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, KEY)],
        );
        (
            provider.fetch_handle(&CredentialHandle::implicit()).await,
            served,
        )
    }

    fn read(balance: &str, monthly: &str) -> Result<(Usage, Vec<Pool>), FetchError> {
        reading(
            &parse_balance(balance.as_bytes())?,
            &parse_monthly(monthly.as_bytes())?,
        )
    }

    fn usd(minor: i64, exponent: u8) -> Amount {
        Amount {
            minor,
            exponent,
            unit: "USD".to_string(),
        }
    }

    #[tokio::test]
    async fn a_prepaid_organization_publishes_its_budget_window_and_balance_pool() {
        let (attempt, served) = fetch_env(BALANCE, MONTHLY).await;
        let usage = attempt.usage.as_ref().expect("the fixture reads");
        assert_eq!(attempt.source.as_deref(), Some("api"));

        let window = usage
            .primary
            .as_ref()
            .expect("a positive budget is a window");
        assert_eq!(window.used_percent, 6.0);
        assert_eq!(window.resets_at.as_deref(), Some("2026-11-01T00:00:00Z"));
        assert_eq!(window.window_kind.as_deref(), Some("monthly"));
        assert_eq!(window.window_minutes, None, "a month has no fixed length");
        assert_eq!((window.used_count, window.total_count), (None, None));
        assert!(usage.secondary.is_none() && usage.tertiary.is_none());

        let pools = attempt.pools.as_ref().unwrap();
        assert_eq!(pools.len(), 1, "{pools:?}");
        let pool = &pools[0];
        assert_eq!(pool.id, "balance");
        assert_eq!(pool.remaining, Some(usd(1850, 2)));
        assert_eq!(pool.basis, PoolBasis::Reported);
        assert_eq!(pool.funding, PoolFunding::Unknown);
        // The budget caps spending; it is not the balance pool's size.
        assert_eq!(pool.total, None);
        assert_eq!(pool.spendable, None);
        assert_eq!(pool.resets_at, None);

        let requests = served.lock().unwrap().clone();
        let paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, vec![BALANCE_PATH, MONTHLY_PATH]);
        for request in &requests {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer snx_org_fixture")
            );
        }
    }

    /// An invoiced organization's balance is not money it holds, so no pool is
    /// published for it and a zero never reads as exhausted credit.
    #[test]
    fn invoice_billing_publishes_the_window_and_no_balance_pool() {
        let (usage, pools) = read(
            r#"{"available_balance_usd":"0","invoice_billing_enabled":true}"#,
            MONTHLY,
        )
        .unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 6.0);
        assert!(pools.is_empty(), "{pools:?}");
    }

    #[test]
    fn prepaid_balances_keep_zero_subcent_and_negative_amounts() {
        for (amount, expected) in [
            ("0", usd(0, 0)),
            ("0.001", usd(1, 3)),
            ("-0.50", usd(-50, 2)),
        ] {
            let balance = format!(
                r#"{{"available_balance_usd":"{amount}","invoice_billing_enabled":false}}"#
            );
            let (usage, pools) = read(&balance, MONTHLY).unwrap();
            assert_eq!(pools[0].remaining, Some(expected), "{amount}");
            assert_eq!(usage.primary.unwrap().used_percent, 6.0, "{amount}");
        }
    }

    /// A zero budget has no denominator, so no window; an exceeded one reads
    /// full, never past it.
    #[test]
    fn a_zero_budget_has_no_window_and_an_exceeded_one_reads_full() {
        let (usage, pools) = read(
            BALANCE,
            r#"{"month":"2026-10","usage_usd":"0","monthly_budget_usd":"0"}"#,
        )
        .unwrap();
        assert!(usage.primary.is_none(), "{usage:?}");
        assert_eq!(pools.len(), 1, "the balance still publishes");

        let (usage, _) = read(
            BALANCE,
            r#"{"month":"2026-10","usage_usd":"100.25","monthly_budget_usd":"100"}"#,
        )
        .unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 100.0);
    }

    /// Invoice billing with no budget states neither a balance nor a limit:
    /// that is no quota reported, not an empty healthy reading.
    #[tokio::test]
    async fn invoice_billing_with_no_budget_is_no_quota_reported() {
        let (attempt, _) = fetch_env(
            r#"{"available_balance_usd":"0","invoice_billing_enabled":true}"#,
            r#"{"month":"2026-10","usage_usd":"3","monthly_budget_usd":"0"}"#,
        )
        .await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "no_quota_reported", "{error:?}");
    }

    #[test]
    fn the_reset_follows_the_reported_month_across_year_and_leap_boundaries() {
        for (month, expected) in [
            ("2026-12", "2027-01-01T00:00:00Z"),
            ("2028-02", "2028-03-01T00:00:00Z"),
            ("2026-01", "2026-02-01T00:00:00Z"),
        ] {
            let monthly =
                format!(r#"{{"month":"{month}","usage_usd":"0","monthly_budget_usd":"100"}}"#);
            let (usage, _) = read(BALANCE, &monthly).unwrap();
            assert_eq!(
                usage.primary.unwrap().resets_at.as_deref(),
                Some(expected),
                "{month}"
            );
        }
    }

    /// Every body upstream's tests refuse is a decode failure here, through
    /// the provider, and none of them is echoed into the error.
    #[tokio::test]
    async fn malformed_billing_is_decode_failed_rather_than_zero_usage() {
        let cases: Vec<(&str, String)> = vec![
            ("balance", "private-response-marker".to_string()),
            ("balance", "[]".to_string()),
            (
                "balance",
                r#"{"available_balance_usd":18.5,"invoice_billing_enabled":false}"#.to_string(),
            ),
            (
                "balance",
                r#"{"available_balance_usd":"","invoice_billing_enabled":false}"#.to_string(),
            ),
            (
                "balance",
                r#"{"available_balance_usd":"NaN","invoice_billing_enabled":false}"#.to_string(),
            ),
            (
                "balance",
                r#"{"available_balance_usd":"18.5","invoice_billing_enabled":"false"}"#.to_string(),
            ),
            ("monthly", "{}".to_string()),
            (
                "monthly",
                r#"{"month":"2026-10","usage_usd":"-1","monthly_budget_usd":"100"}"#.to_string(),
            ),
            (
                "monthly",
                r#"{"month":"2026-10","usage_usd":"6","monthly_budget_usd":null}"#.to_string(),
            ),
            (
                "monthly",
                r#"{"month":"2026-13","usage_usd":"6","monthly_budget_usd":"100"}"#.to_string(),
            ),
            // Shapes the shared money parser alone would accept.
            (
                "balance",
                r#"{"available_balance_usd":"+1.00","invoice_billing_enabled":false}"#.to_string(),
            ),
            (
                "monthly",
                r#"{"month":"2026-10","usage_usd":".5","monthly_budget_usd":"100"}"#.to_string(),
            ),
        ];
        for (which, body) in cases {
            let (balance, monthly) = if which == "balance" {
                (body.as_str(), MONTHLY)
            } else {
                (BALANCE, body.as_str())
            };
            let (attempt, _) = fetch_env(balance, monthly).await;
            match attempt.usage {
                Err(error @ FetchError::Decode(_)) => {
                    assert_eq!(error.error_class(), "decode_failed");
                    assert!(!error.to_string().contains("private-response"), "{error}");
                }
                other => panic!("{which} {body} must be decode_failed, got {other:?}"),
            }
        }
    }

    /// Each refusal lands in its class on either request, and only a 401 on a
    /// served vault key is reported to the vault.
    #[tokio::test]
    async fn each_status_maps_to_its_class_and_only_a_vault_401_is_reported() {
        for path in [BALANCE_PATH, MONTHLY_PATH] {
            for (status, class, reported) in [
                (401, "credential_rejected", true),
                (403, "credential_rejected", false),
                (429, "upstream_failed", false),
                (500, "upstream_failed", false),
                (503, "upstream_failed", false),
                (404, "upstream_failed", false),
            ] {
                let (base, _) = soniox(BALANCE, MONTHLY, Some((path, status))).await;
                let (source, reports) =
                    MockSource::serving(&[("apikey:soniox", credential(KEY, 11))]);
                let provider = provider(
                    Some(source),
                    Arc::new(VaultHandleLoader::default()),
                    &base,
                    &[],
                );
                let attempt = provider.fetch_handle(&vault_handle()).await;
                assert_eq!(attempt.source.as_deref(), Some("vault"));
                let error = attempt.usage.unwrap_err();
                assert_eq!(
                    error.error_class(),
                    class,
                    "{path} HTTP {status}: {error:?}"
                );
                // 429 and 5xx keep the last reading served while they last.
                // Other refusals (404 here) take whatever the shared
                // `upstream_failed` class gives them, so they are not pinned.
                if status == 429 || status >= 500 {
                    assert_eq!(
                        crate::refresh::classify(&error),
                        crate::refresh::FetchClass::Transient,
                        "{path} HTTP {status}"
                    );
                } else if status == 401 || status == 403 {
                    assert_ne!(
                        crate::refresh::classify(&error),
                        crate::refresh::FetchClass::Transient,
                        "a rejected key must not be served stale as an outage: {path} HTTP {status}"
                    );
                }
                for _ in 0..20 {
                    if !reports.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                let expected = if reported {
                    vec![("apikey:soniox".to_string(), 401, 11)]
                } else {
                    Vec::new()
                };
                assert_eq!(*reports.lock().unwrap(), expected, "{path} HTTP {status}");
            }
        }
    }

    /// A 401 on the environment key is rejected too, and nothing is reported:
    /// the vault did not serve that key, so it has nothing to retire.
    #[tokio::test]
    async fn an_environment_401_is_rejected_and_never_reported_to_the_vault() {
        let (base, _) = soniox(BALANCE, MONTHLY, Some((BALANCE_PATH, 401))).await;
        let (source, reports) = MockSource::serving(&[("apikey:soniox", credential(KEY, 5))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, "env-org-key")],
        );
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_rejected", "{error:?}");
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(reports.lock().unwrap().is_empty());
    }

    #[test]
    fn a_vault_deposit_routes_to_soniox_and_replaces_the_environment_lane() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:soniox", "apikey")]);
        let (source, _) = MockSource::serving(&[]);
        let with_vault = provider(
            Some(source),
            Arc::clone(&loader),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-org-key")],
        );
        assert_eq!(
            with_vault.handles().unwrap(),
            vec![vault_handle()],
            "a soniox deposit must replace the environment lane"
        );

        // The control: the same environment with no deposit enumerates the
        // environment lane, so the line above is the vault replacing it.
        let (source, _) = MockSource::serving(&[]);
        let without_vault = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-org-key")],
        );
        assert_eq!(
            without_vault.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    /// With both present, the vault key is the one sent, and the environment
    /// key never leaves the host.
    #[tokio::test]
    async fn the_vault_key_is_sent_and_the_environment_key_is_not() {
        let (base, served) = soniox(BALANCE, MONTHLY, None).await;
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:soniox", "apikey")]);
        let (source, reports) =
            MockSource::serving(&[("apikey:soniox", credential("vault-org-key", 3))]);
        let provider = provider(Some(source), loader, &base, &[(KEY_ENV, "env-org-key")]);
        let handles = provider.handles().unwrap();
        assert_eq!(
            handles,
            vec![vault_handle()],
            "a soniox deposit must replace the environment lane"
        );
        for handle in &handles {
            let attempt = provider.fetch_handle(handle).await;
            assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
            assert_eq!(attempt.source.as_deref(), Some("vault"));
        }
        let requests = served.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer vault-org-key")
            );
        }
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_key_anywhere_is_credential_absent_and_sends_nothing() {
        let (base, served) = soniox(BALANCE, MONTHLY, None).await;
        // A project key is not an organization key, and is not read.
        let provider = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[("SONIOX_API_KEY", "project-fixture")],
        );
        let error = provider
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "credential_absent", "{error:?}");
        assert!(served.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_vault_deposit_is_unusable_and_sends_nothing() {
        let (base, served) = soniox(BALANCE, MONTHLY, None).await;
        let (source, _) = MockSource::serving(&[("apikey:soniox", credential("  ", 1))]);
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
        assert_eq!(error.error_class(), "credential_unusable", "{error:?}");
        assert!(served.lock().unwrap().is_empty());
    }

    /// A failed balance read stops before the usage request, as upstream does.
    #[tokio::test]
    async fn an_unreadable_balance_never_reaches_the_usage_endpoint() {
        let (attempt, served) = fetch_env("[]", MONTHLY).await;
        assert_eq!(attempt.usage.unwrap_err().error_class(), "decode_failed");
        let paths: Vec<String> = served
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect();
        assert_eq!(paths, vec![BALANCE_PATH.to_string()]);
    }
}
