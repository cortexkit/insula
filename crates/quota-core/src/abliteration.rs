//! abliteration.ai — an account's prepaid USD credits, read with an API key.
//!
//! - Endpoint: `GET https://api.abliteration.ai/v1/credits`, bearer API key,
//!   one request per refresh. The answer is
//!   `{"data":{"total_credits":50.0,"total_usage":0.010637}}`: two cumulative
//!   USD counters, sent as JSON numbers, with no reset and no period.
//! - Reference: CodexBar v0.81.0
//!   `Sources/CodexBarCore/Resources/Plugins/abliteration.ts` and
//!   `docs/abliteration.md`. Tested against fixtures from upstream's
//!   `AbliterationPluginTests.swift` only; no abliteration.ai account was
//!   available when this was written.
//!
//! WHAT IS PUBLISHED: one `credits` pool in USD, and no rate window.
//!
//! - `remaining` is `max(0, total_credits - total_usage)`, floored at zero as
//!   upstream floors its balance: an overspent account has nothing left to
//!   spend, and a negative amount is not something a router should compare.
//! - `total` is `total_credits`, stated beside the remainder so a consumer can
//!   see what it is a remainder of. Both are restated at one exponent, so they
//!   compare directly.
//! - `basis: derived`, because the provider states a grant and a consumption
//!   rather than the remainder itself; `funding: unknown`, because it never
//!   says whether the credits were bought or given.
//! - A zero `total_credits` is still an answer: an authenticated account with
//!   nothing to spend, published as 0 of 0. Nothing divides by the total.
//! - The counters are cumulative and never reset, so no window, reset time or
//!   cadence is published; any of those would be invented.
//!
//! Amounts are exact, in integer minor units at the precision stated: each
//! number is read through its decimal text, never by scaling a binary float. A
//! counter upstream refuses (absent, not a number, negative, or too large to be
//! finite) fails the reading as `decode_failed`, and so does one stated to more
//! than nine decimal places, which the shared money parser cannot hold exactly;
//! publishing a rounded figure there would misstate somebody's balance.
//!
//! Credential: a vault deposit under `apikey:abliteration` when one exists,
//! otherwise `ABLITERATION_API_KEY`, the variable upstream reads.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::credential_source::CredentialSource;
use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;
use crate::vercel::{default_env, EnvLookup};

pub const PROVIDER_NAME: &str = "abliteration";
/// The only origin the API key is ever sent to.
const API_BASE: &str = "https://api.abliteration.ai";
const CREDITS_PATH: &str = "/v1/credits";
const KEY_ENV: &str = "ABLITERATION_API_KEY";
const USD: &str = "USD";
/// The largest power of ten a JSON number's exponent may shift by. Anything
/// further out cannot fit the shared parser's 64-bit minor units or its
/// nine-decimal limit anyway; the bound only keeps the expansion small.
const MAX_DECIMAL_SHIFT: i32 = 40;

/// Decode failures never quote the body: an unexpected answer is not
/// something to publish, and it may echo the request.
fn decode(what: &str) -> FetchError {
    FetchError::Decode(format!("abliteration: {what}"))
}

/// A JSON number's text as a plain decimal, with exponent notation expanded.
///
/// serde_json renders a float such as `0.0000005` as `5e-7`, which the shared
/// money parser refuses by design. Upstream accepts any finite number, so the
/// exponent is applied here by moving the decimal point in the text, which is
/// exact, instead of by arithmetic on a float, which is not.
fn plain_decimal(text: &str) -> Option<String> {
    let Some((mantissa, exponent)) = text.split_once(['e', 'E']) else {
        return Some(text.to_string());
    };
    let shift: i32 = exponent.parse().ok()?;
    if shift.abs() > MAX_DECIMAL_SHIFT {
        return None;
    }
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Where the decimal point lands, counted in digits from the left.
    let point = i32::try_from(whole.len()).ok()? + shift;
    let length = i32::try_from(digits.len()).ok()?;
    let plain = if point <= 0 {
        format!("0.{}{digits}", "0".repeat(usize::try_from(-point).ok()?))
    } else if point >= length {
        format!(
            "{digits}{}",
            "0".repeat(usize::try_from(point - length).ok()?)
        )
    } else {
        let (head, tail) = digits.split_at(usize::try_from(point).ok()?);
        format!("{head}.{tail}")
    };
    Some(format!("{sign}{plain}"))
}

/// One USD counter, held to upstream's rule: a finite JSON number, not below
/// zero.
///
/// A string, a Boolean or `null` is refused rather than coerced, as upstream
/// refuses it. A number too large to be finite never reaches here: serde_json
/// refuses the whole body.
fn counter(value: Option<&Value>, field: &str) -> Result<Amount, FetchError> {
    let refused = || decode(&format!("{field} is not a non-negative USD number"));
    let Some(Value::Number(number)) = value else {
        return Err(refused());
    };
    let text = plain_decimal(&number.to_string()).ok_or_else(refused)?;
    let amount = parse_amount(&text, USD).ok_or_else(|| {
        decode(&format!(
            "{field} has more precision or magnitude than can be published exactly"
        ))
    })?;
    // `-0.0` parses to zero minor units and passes, as it passes upstream's
    // `value >= 0`.
    if amount.minor < 0 {
        return Err(refused());
    }
    Ok(amount)
}

/// Restate an amount at a larger exponent. Scaling up by a power of ten is
/// exact; an overflow is refused rather than published wrong.
fn at_exponent(amount: &Amount, exponent: u8, field: &str) -> Result<Amount, FetchError> {
    let minor = 10_i64
        .checked_pow(u32::from(exponent - amount.exponent))
        .and_then(|factor| amount.minor.checked_mul(factor))
        .ok_or_else(|| decode(&format!("{field} is too large to publish exactly")))?;
    Ok(Amount {
        minor,
        exponent,
        unit: amount.unit.clone(),
    })
}

/// Turn a `/v1/credits` answer into the one credits pool to publish.
fn normalize_credits(body: &[u8]) -> Result<Vec<Pool>, FetchError> {
    // The parse error is replaced rather than quoted, so nothing from an
    // unexpected body reaches the published error.
    let decoded: Value =
        serde_json::from_slice(body).map_err(|_| decode("the credits answer is not JSON"))?;
    let data = decoded
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| decode("the credits answer has no data object"))?;
    let total = counter(data.get("total_credits"), "total_credits")?;
    let used = counter(data.get("total_usage"), "total_usage")?;

    // The remainder and the total are published at one exponent: the wire
    // rules only compare amounts stated in the same terms.
    let exponent = total.exponent.max(used.exponent);
    let total = at_exponent(&total, exponent, "total_credits")?;
    let used = at_exponent(&used, exponent, "total_usage")?;
    // Both counters are non-negative, so the subtraction cannot overflow. An
    // overspent account reads as nothing left, never a negative balance.
    let remaining = Amount {
        minor: (total.minor - used.minor).max(0),
        exponent,
        unit: USD.to_string(),
    };

    Ok(vec![Pool {
        id: "credits".to_string(),
        label: "abliteration.ai credits".to_string(),
        // The endpoint calls these credits but never says whether they were
        // bought or given; a funding guess could make a router spend money.
        funding: PoolFunding::Unknown,
        remaining: Some(remaining),
        total: Some(total),
        // A grant and its consumption are stated, not the remainder.
        basis: PoolBasis::Derived,
        // The endpoint states no enable flag, and a positive balance is not one.
        spendable: None,
        // Cumulative counters: nothing resets and nothing is said to expire.
        resets_at: None,
        expires_at: None,
        unlimited: None,
    }])
}

pub struct AbliterationProvider {
    base: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    env: EnvLookup,
}

impl AbliterationProvider {
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

    /// One credits request. 401 and 403 become `credential_rejected`; 429, 5xx
    /// and any other refusal become `upstream_failed`, the shared mapping of
    /// upstream's authentication, rate-limit, unavailable and API failures.
    async fn read(&self, key: &str, vault: bool) -> Result<Vec<Pool>, FetchError> {
        let request = JsonRequest::get(format!("{}{CREDITS_PATH}", self.base)).bearer(key);
        // Only the vault lane keeps the numeric status, which the shared gate
        // needs to report a 401 to the vault; the environment has nobody to tell.
        let body = if vault {
            request
                .send_provider_status_first(&self.http, PROVIDER_NAME)
                .await?
                .body
        } else {
            request.send(&self.http).await?
        };
        normalize_credits(&body)
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
                    "the apikey:abliteration deposit holds no API key".to_string(),
                ),
            );
        }
        let result = self.read(&key, true).await;
        if let Err(error) = &result {
            // The shared gate reports only a 401 on this served key: a 403 may
            // be a refused permission rather than a dead key.
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
                FetchError::NoSession(format!(
                    "no {KEY_ENV} and no apikey:abliteration vault deposit"
                )),
            );
        };
        attempt_from(self.read(&key, false).await, "api")
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
impl UsageProvider for AbliterationProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.abliteration_handles()?;
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

    // Upstream's own fixtures, from `AbliterationPluginTests.swift`.
    const CREDITS: &str = r#"{"data":{"total_credits":50.0,"total_usage":0.010637}}"#;
    const KEY: &str = "abl_fixture";

    fn provider(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
        base: &str,
        env: &[(&str, &str)],
    ) -> AbliterationProvider {
        let mut provider = AbliterationProvider::new_with_handle_loader(source, loader);
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
        CredentialHandle::scoped("apikey:abliteration", "apikey")
    }

    /// A loopback abliteration.ai answering `/v1/credits` with `status` and
    /// `body`, and anything else with 404.
    async fn abliteration(status: u16, body: &str) -> (String, Log) {
        let body = body.to_string();
        routes(move |path, _| match path {
            CREDITS_PATH => (status, body.as_bytes().to_vec()),
            _ => (404, Vec::new()),
        })
        .await
    }

    /// Read through the environment lane against a loopback abliteration.ai.
    async fn fetch_env(body: &str) -> (FetchAttempt, Log) {
        let (base, served) = abliteration(200, body).await;
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

    /// The one pool a body publishes, through the provider.
    async fn pool(body: &str) -> Pool {
        let (attempt, _) = fetch_env(body).await;
        assert!(attempt.usage.is_ok(), "{body}: {:?}", attempt.usage);
        let pools = attempt.pools.expect("a reading always carries its pool");
        assert_eq!(pools.len(), 1, "{pools:?}");
        pools.into_iter().next().unwrap()
    }

    fn usd(minor: i64, exponent: u8) -> Amount {
        Amount {
            minor,
            exponent,
            unit: "USD".to_string(),
        }
    }

    /// Upstream shows `$49.99` left of `$50.00`; the exact figures behind that
    /// display are 49.989363 of 50.000000.
    #[tokio::test]
    async fn the_contract_response_publishes_an_exact_usd_pool_and_no_window() {
        let (attempt, served) = fetch_env(CREDITS).await;
        assert_eq!(attempt.source.as_deref(), Some("api"));
        let usage = attempt.usage.as_ref().expect("the fixture reads");
        assert!(
            usage.primary.is_none() && usage.secondary.is_none() && usage.tertiary.is_none(),
            "cumulative counters are not a window: {usage:?}"
        );
        assert!(usage.extra_rate_windows.is_none());

        let pools = attempt.pools.as_ref().unwrap();
        assert_eq!(pools.len(), 1, "{pools:?}");
        let pool = &pools[0];
        assert_eq!(pool.id, "credits");
        assert_eq!(pool.remaining, Some(usd(49_989_363, 6)));
        assert_eq!(pool.total, Some(usd(50_000_000, 6)));
        assert_eq!(pool.basis, PoolBasis::Derived);
        assert_eq!(pool.funding, PoolFunding::Unknown);
        assert_eq!(pool.spendable, None);
        assert_eq!(pool.resets_at, None, "nothing resets");
        assert_eq!(pool.expires_at, None);

        let requests = served.lock().unwrap().clone();
        assert_eq!(requests.len(), 1, "one request per refresh");
        assert_eq!(requests[0].path, CREDITS_PATH);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer abl_fixture")
        );
    }

    /// Upstream's honest-amounts fixtures: no usage leaves the whole grant, an
    /// overspent account has nothing left rather than a negative balance, and
    /// zero credits is an authenticated balance of nothing, not a failure.
    #[tokio::test]
    async fn zero_usage_overspending_and_zero_credits_keep_honest_amounts() {
        let unused = pool(r#"{"data":{"total_credits":50,"total_usage":0}}"#).await;
        assert_eq!(unused.remaining, Some(usd(50, 0)));
        assert_eq!(unused.total, Some(usd(50, 0)));

        let overspent = pool(r#"{"data":{"total_credits":50,"total_usage":60}}"#).await;
        assert_eq!(
            overspent.remaining,
            Some(usd(0, 0)),
            "an overspent account must read as nothing left"
        );
        assert_eq!(overspent.total, Some(usd(50, 0)));

        for used in [0, 1] {
            let body = format!(r#"{{"data":{{"total_credits":0,"total_usage":{used}}}}}"#);
            let zero = pool(&body).await;
            assert_eq!(zero.remaining, Some(usd(0, 0)), "{body}");
            assert_eq!(zero.total, Some(usd(0, 0)), "{body}");
        }
    }

    /// A number serde_json renders in exponent notation is still read exactly,
    /// and a counter stated past nine decimal places is refused rather than
    /// rounded.
    #[tokio::test]
    async fn exponent_notation_is_exact_and_excess_precision_is_refused() {
        let tiny = pool(r#"{"data":{"total_credits":1,"total_usage":5e-7}}"#).await;
        assert_eq!(tiny.remaining, Some(usd(9_999_995, 7)));
        assert_eq!(tiny.total, Some(usd(10_000_000, 7)));

        let large = pool(r#"{"data":{"total_credits":1.5e17,"total_usage":0}}"#).await;
        assert_eq!(large.total, Some(usd(150_000_000_000_000_000, 0)));

        let (attempt, _) =
            fetch_env(r#"{"data":{"total_credits":50,"total_usage":0.0106371234567}}"#).await;
        assert_eq!(
            attempt.usage.unwrap_err().error_class(),
            "decode_failed",
            "a figure that cannot be held exactly must not be published rounded"
        );
    }

    #[test]
    fn plain_decimal_moves_the_point_without_arithmetic() {
        for (text, expected) in [
            ("50.0", "50.0"),
            ("0.010637", "0.010637"),
            ("5e-7", "0.0000005"),
            ("1.5e-7", "0.00000015"),
            ("1.5e21", "1500000000000000000000"),
            ("1.25e1", "12.5"),
            ("-1e-7", "-0.0000001"),
        ] {
            assert_eq!(plain_decimal(text).as_deref(), Some(expected), "{text}");
        }
        assert_eq!(plain_decimal("1e400"), None);
    }

    /// Every body upstream's tests refuse is a decode failure here, through
    /// the provider, and none of them is echoed into the error.
    #[tokio::test]
    async fn malformed_counters_are_decode_failed_and_never_echoed() {
        for body in [
            "private-response-marker",
            "not-json",
            "[]",
            "{}",
            r#"{"data":[]}"#,
            r#"{"data":{"total_credits":50}}"#,
            r#"{"data":{"total_credits":"50","total_usage":0}}"#,
            r#"{"data":{"total_credits":null,"total_usage":0}}"#,
            r#"{"data":{"total_credits":-1,"total_usage":0}}"#,
            r#"{"data":{"total_credits":50,"total_usage":-1}}"#,
            r#"{"data":{"total_credits":50,"total_usage":true}}"#,
            r#"{"data":{"total_credits":1e309,"total_usage":0}}"#,
        ] {
            let (attempt, _) = fetch_env(body).await;
            match attempt.usage {
                Err(error @ FetchError::Decode(_)) => {
                    assert_eq!(error.error_class(), "decode_failed");
                    assert!(!error.to_string().contains("private-response"), "{error}");
                }
                other => panic!("{body} must be decode_failed, got {other:?}"),
            }
            assert!(attempt.pools.is_none(), "{body} must publish no pool");
        }
    }

    /// Each refusal lands in its class with one request and no retry, and only
    /// a 401 on a served vault key is reported to the vault.
    #[tokio::test]
    async fn each_status_maps_to_its_class_and_only_a_vault_401_is_reported() {
        for (status, class, reported) in [
            (401, "credential_rejected", true),
            (403, "credential_rejected", false),
            (429, "upstream_failed", false),
            (500, "upstream_failed", false),
            (503, "upstream_failed", false),
            (404, "upstream_failed", false),
        ] {
            let (base, served) = abliteration(status, "private-response-marker").await;
            let (source, reports) =
                MockSource::serving(&[("apikey:abliteration", credential(KEY, 11))]);
            let provider = provider(
                Some(source),
                Arc::new(VaultHandleLoader::default()),
                &base,
                &[],
            );
            let attempt = provider.fetch_handle(&vault_handle()).await;
            assert_eq!(attempt.source.as_deref(), Some("vault"));
            let error = attempt.usage.unwrap_err();
            assert_eq!(error.error_class(), class, "HTTP {status}: {error:?}");
            // 429 and 5xx keep the last reading served while they last.
            // Other refusals (404 here) take whatever the shared
            // `upstream_failed` class gives them, so they are not pinned.
            if status == 429 || status >= 500 {
                assert_eq!(
                    crate::refresh::classify(&error),
                    crate::refresh::FetchClass::Transient,
                    "HTTP {status}"
                );
            } else if status == 401 || status == 403 {
                assert_ne!(
                    crate::refresh::classify(&error),
                    crate::refresh::FetchClass::Transient,
                    "a rejected key must not be served stale as an outage: HTTP {status}"
                );
            }
            assert_eq!(served.lock().unwrap().len(), 1, "HTTP {status}: no retry");
            for _ in 0..20 {
                if !reports.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let expected = if reported {
                vec![("apikey:abliteration".to_string(), 401, 11)]
            } else {
                Vec::new()
            };
            assert_eq!(*reports.lock().unwrap(), expected, "HTTP {status}");
        }
    }

    /// A 401 on the environment key is rejected too, and nothing is reported:
    /// the vault did not serve that key, so it has nothing to retire.
    #[tokio::test]
    async fn an_environment_401_is_rejected_and_never_reported_to_the_vault() {
        let (base, _) = abliteration(401, "").await;
        let (source, reports) = MockSource::serving(&[("apikey:abliteration", credential(KEY, 5))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, "env-key")],
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
    fn a_vault_deposit_routes_to_abliteration_and_replaces_the_environment_lane() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:abliteration", "apikey")]);
        let (source, _) = MockSource::serving(&[]);
        let with_vault = provider(
            Some(source),
            Arc::clone(&loader),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-key")],
        );
        assert_eq!(
            with_vault.handles().unwrap(),
            vec![vault_handle()],
            "an abliteration deposit must replace the environment lane"
        );

        // The control: the same environment with no deposit enumerates the
        // environment lane, so the line above is the vault replacing it.
        let (source, _) = MockSource::serving(&[]);
        let without_vault = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-key")],
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
        let (base, served) = abliteration(200, CREDITS).await;
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:abliteration", "apikey")]);
        let (source, reports) =
            MockSource::serving(&[("apikey:abliteration", credential("vault-key", 3))]);
        let provider = provider(Some(source), loader, &base, &[(KEY_ENV, "env-key")]);
        let handles = provider.handles().unwrap();
        assert_eq!(
            handles,
            vec![vault_handle()],
            "an abliteration deposit must replace the environment lane"
        );
        for handle in &handles {
            let attempt = provider.fetch_handle(handle).await;
            assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
            assert_eq!(attempt.source.as_deref(), Some("vault"));
        }
        let requests = served.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer vault-key")
        );
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_key_anywhere_is_credential_absent_and_sends_nothing() {
        let (base, served) = abliteration(200, CREDITS).await;
        let provider = provider(None, Arc::new(VaultHandleLoader::default()), &base, &[]);
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
        let (base, served) = abliteration(200, CREDITS).await;
        let (source, _) = MockSource::serving(&[("apikey:abliteration", credential("  ", 1))]);
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
}
