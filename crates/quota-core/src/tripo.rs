//! Tripo's available credits and frozen reservations, with no inferred quota period.
//!
//! Ported from CodexBar v0.84.0:
//! `Sources/CodexBarCore/Resources/Plugins/tripo.ts` and
//! `Sources/CodexBarCore/Providers/Tripo/TripoProviderDescriptor.swift`.
//! Fixtures come from that tag's `TestsPlugin/TripoPluginTests.swift`;
//! no live Tripo key was available for verification.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{value::RawValue, Number};

use crate::credential_source::CredentialSource;
use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;

pub const PROVIDER_NAME: &str = "tripo";
const API_BASE: &str = "https://openapi.tripo3d.ai";
const BALANCE_PATH: &str = "/v3/account/balance";
const CREDIT_UNIT: &str = "tripo-credit";

fn decode() -> FetchError {
    FetchError::Decode("tripo: unrecognized balance response".to_string())
}

// Expand scientific notation in decimal text, never through binary floats.
// The bound keeps allocation small; larger shifts exceed the wire amount's range.
fn plain_decimal(text: &str) -> Option<String> {
    let Some((mantissa, exponent)) = text.split_once(['e', 'E']) else {
        return Some(text.to_string());
    };
    let shift: i32 = exponent.parse().ok()?;
    if !(-40..=40).contains(&shift) {
        return None;
    }
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
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

fn credits(raw: &RawValue) -> Result<Amount, FetchError> {
    // Validate a finite JSON number, but retain the original decimal lexeme so
    // high-precision balances cannot be rounded by serde_json's float storage.
    serde_json::from_str::<Number>(raw.get()).map_err(|_| decode())?;
    let plain = plain_decimal(raw.get()).ok_or_else(decode)?;
    let amount = parse_amount(&plain, CREDIT_UNIT).ok_or_else(decode)?;
    if amount.minor < 0 {
        return Err(decode());
    }
    Ok(amount)
}

#[derive(Deserialize)]
struct Envelope<'a> {
    code: Number,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Balances<'a> {
    #[serde(borrow)]
    balance: &'a RawValue,
    #[serde(borrow)]
    frozen: &'a RawValue,
}

fn normalize(body: &[u8]) -> Result<Vec<Pool>, FetchError> {
    let envelope: Envelope<'_> = serde_json::from_slice(body).map_err(|_| decode())?;
    let code = envelope
        .code
        .as_f64()
        .filter(|code| code.is_finite() && code.fract() == 0.0)
        .ok_or_else(decode)?;
    if code != 0.0 {
        return Err(FetchError::Upstream(
            "tripo: balance API failed".to_string(),
        ));
    }
    let balances: Balances<'_> =
        serde_json::from_str(envelope.data.ok_or_else(decode)?.get()).map_err(|_| decode())?;
    let balance = credits(balances.balance)?;
    let frozen = credits(balances.frozen)?;

    // Balance already excludes frozen reservations. Never subtract them again,
    // or add the reserved credits to the account's available capacity.
    Ok([
        ("balance", "Available credits", balance, None),
        ("frozen", "Reserved credits", frozen, Some(false)),
    ]
    .into_iter()
    .map(|(id, label, amount, spendable)| Pool {
        id: id.to_string(),
        label: label.to_string(),
        funding: PoolFunding::Unknown,
        basis: PoolBasis::Reported,
        remaining: Some(amount),
        total: None,
        spendable,
        resets_at: None,
        expires_at: None,
        unlimited: None,
    })
    .collect())
}

pub struct TripoProvider {
    base: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
}

impl TripoProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            base: API_BASE.to_string(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
        }
    }

    async fn read(&self, key: &str) -> Result<Vec<Pool>, FetchError> {
        let response = JsonRequest::get(format!("{}{BALANCE_PATH}", self.base))
            .bearer(key)
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
            .map_err(|error| match error {
                // A 403 is a refused permission, not evidence of a dead key.
                // Discard refusal excerpts entirely: upstream can echo secrets.
                FetchError::ProviderStatus(401, _) => {
                    FetchError::ProviderStatus(401, "tripo: API key rejected".to_string())
                }
                FetchError::ProviderStatus(status, _) => {
                    FetchError::Upstream(format!("tripo: HTTP {status}"))
                }
                other => other,
            })?;
        if response.status != 200 {
            return Err(FetchError::Upstream(format!(
                "tripo: HTTP {}",
                response.status
            )));
        }
        normalize(&response.body)
    }
}

#[async_trait]
impl UsageProvider for TripoProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let handles = self.handle_loader.tripo_handles()?;
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
                FetchError::NoSession("no apikey:tripo vault deposit".to_string()),
            );
        }
        let Some(source) = &self.credential_source else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match crate::credential_source::get_vault_credential(
            source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let key = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(key) => key.trim().to_string(),
            Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
        };
        if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return FetchAttempt::failure(
                None,
                Some("vault".to_string()),
                FetchError::CredentialUnusable("tripo: deposit is not an API key".to_string()),
            );
        }
        match self.read(&key).await {
            Ok(pools) => {
                let mut attempt = FetchAttempt::success(None, "vault", Usage::default());
                attempt.pools = Some(pools);
                attempt
            }
            Err(error) => {
                crate::credential_source::report_vault_auth_failure(
                    Some(source),
                    handle,
                    credential.record_version,
                    &error,
                );
                FetchAttempt::failure(None, Some("vault".to_string()), error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vercel::testkit::{credential, routes, MockSource};

    // CodexBar v0.84.0, TestsPlugin/TripoPluginTests.swift, default fetch body.
    const BALANCE: &str = r#"{"code":0,"data":{"balance":125.75,"frozen":20.25}}"#;
    const ID: &str = "apikey:tripo:fixture";

    fn handle() -> CredentialHandle {
        CredentialHandle::scoped(ID, "apikey")
    }

    fn amount(minor: i64, exponent: u8) -> Amount {
        Amount {
            minor,
            exponent,
            unit: CREDIT_UNIT.to_string(),
        }
    }

    fn provider(source: Option<Arc<dyn CredentialSource>>, base: &str) -> TripoProvider {
        let mut provider =
            TripoProvider::new_with_handle_loader(source, Arc::new(VaultHandleLoader::default()));
        provider.base = base.to_string();
        provider
    }

    #[tokio::test]
    async fn available_credits_and_frozen_reservations_are_separate_pools_without_windows() {
        let (base, served) = routes(|_, _| (200, BALANCE.as_bytes().to_vec())).await;
        let (source, reports) = MockSource::serving(&[(ID, credential("tripo_fixture", 7))]);
        let attempt = provider(Some(source), &base).fetch_handle(&handle()).await;
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert!(attempt.observed.is_none());
        let usage = attempt.usage.unwrap();
        assert!(usage.primary.is_none() && usage.secondary.is_none() && usage.tertiary.is_none());
        assert!(usage.extra_rate_windows.is_none());
        let pools = attempt.pools.unwrap();
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0].id, "balance");
        assert_eq!(pools[0].remaining, Some(amount(12575, 2)));
        assert_eq!(pools[0].spendable, None);
        assert_eq!(pools[1].id, "frozen");
        assert_eq!(pools[1].remaining, Some(amount(2025, 2)));
        assert_eq!(pools[1].spendable, Some(false));
        for pool in pools {
            assert_eq!(pool.basis, PoolBasis::Reported);
            assert_eq!(pool.funding, PoolFunding::Unknown);
            assert_eq!(pool.total, None);
            assert_eq!(pool.resets_at, None);
            assert_eq!(pool.expires_at, None);
            assert_eq!(pool.unlimited, None);
        }
        let requests = served.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, BALANCE_PATH);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer tripo_fixture")
        );
        assert!(reports.lock().unwrap().is_empty());
    }

    #[test]
    fn decimal_credits_keep_exact_minor_units_without_float_rounding() {
        for (balance, expected) in [
            ("1234567890.123456789", amount(1_234_567_890_123_456_789, 9)),
            ("9007199254740993", amount(9_007_199_254_740_993, 0)),
            ("5e-7", amount(5, 7)),
            ("1.25e1", amount(125, 1)),
        ] {
            let body = format!(r#"{{"code":0,"data":{{"balance":{balance},"frozen":0}}}}"#);
            assert_eq!(
                normalize(body.as_bytes()).unwrap()[0].remaining,
                Some(expected)
            );
        }
    }

    #[test]
    fn stated_zero_credits_remain_present_and_healthy() {
        let pools = normalize(br#"{"code":0,"data":{"balance":0,"frozen":0}}"#).unwrap();
        assert_eq!(pools.len(), 2);
        for pool in pools {
            assert_eq!(pool.remaining, Some(amount(0, 0)));
        }
    }

    #[test]
    fn malformed_balances_are_decode_failed_without_echoing_the_payload() {
        // Invalid bodies from CodexBar v0.84.0, TestsPlugin/TripoPluginTests.swift,
        // plus exact-representation limits and a non-integer API code.
        for body in [
            "not-json",
            "[]",
            "{}",
            r#"{"code":0,"data":[]}"#,
            r#"{"code":false,"data":{"balance":100,"frozen":0}}"#,
            r#"{"code":0.5,"data":{"balance":100,"frozen":0}}"#,
            r#"{"code":0,"data":{"frozen":0}}"#,
            r#"{"code":0,"data":{"balance":null,"frozen":0}}"#,
            r#"{"code":0,"data":{"balance":100,"frozen":"private-response-marker"}}"#,
            r#"{"code":0,"data":{"balance":100}}"#,
            r#"{"code":0,"data":{"balance":"100","frozen":0}}"#,
            r#"{"code":0,"data":{"balance":true,"frozen":0}}"#,
            r#"{"code":0,"data":{"balance":-1,"frozen":0}}"#,
            r#"{"code":0,"data":{"balance":100,"frozen":-1}}"#,
            r#"{"code":0,"data":{"balance":1e309,"frozen":0}}"#,
            r#"{"code":0,"data":{"balance":0.1234567891,"frozen":0}}"#,
            r#"{"code":0,"data":{"balance":9223372036854775808,"frozen":0}}"#,
        ] {
            let error = normalize(body.as_bytes()).unwrap_err();
            assert_eq!(error.error_class(), "decode_failed", "{body}");
            assert!(!error.to_string().contains("private-response-marker"));
        }
    }

    #[test]
    fn nonzero_api_codes_are_upstream_failed_not_missing_quota_or_dead_keys() {
        let error = normalize(br#"{"code":1,"message":"private-response-marker"}"#).unwrap_err();
        assert_eq!(error.error_class(), "upstream_failed");
        assert!(!error.to_string().contains("private-response-marker"));
        assert!(normalize(br#"{"code":0.0,"data":{"balance":1,"frozen":0}}"#).is_ok());
    }

    #[tokio::test]
    async fn only_a_401_rejects_and_reports_the_served_vault_key() {
        for (status, class) in [
            (401, "credential_rejected"),
            (403, "upstream_failed"),
            (429, "upstream_failed"),
            (500, "upstream_failed"),
            (404, "upstream_failed"),
            (201, "upstream_failed"),
        ] {
            let (base, served) = routes(move |_, _| {
                (
                    status,
                    b"private-response-marker tripo_fixture".repeat(1000),
                )
            })
            .await;
            let (source, reports) = MockSource::serving(&[(ID, credential("tripo_fixture", 7))]);
            let attempt = provider(Some(source), &base).fetch_handle(&handle()).await;
            assert!(attempt.pools.is_none());
            let error = attempt.usage.unwrap_err();
            assert_eq!(error.error_class(), class, "HTTP {status}");
            assert_eq!(
                crate::refresh::classify(&error) == crate::refresh::FetchClass::Transient,
                status != 401
            );
            assert!(!error.to_string().contains("private-response-marker"));
            assert!(!error.to_string().contains("tripo_fixture"));
            assert!(error.to_string().len() < 100);
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            let expected = if status == 401 {
                vec![(ID.to_string(), 401, 7)]
            } else {
                Vec::new()
            };
            assert_eq!(*reports.lock().unwrap(), expected, "HTTP {status}");
            assert_eq!(served.lock().unwrap().len(), 1, "no retries");
        }
    }

    #[tokio::test]
    async fn missing_or_unusable_keys_send_nothing_and_vault_absence_is_not_inferred() {
        let (base, served) = routes(|_, _| (200, BALANCE.as_bytes().to_vec())).await;
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
        for key in ["  ", "key\r\nheader", "key with spaces"] {
            let (source, reports) = MockSource::serving(&[(ID, credential(key, 1))]);
            let error = provider(Some(source), &base)
                .fetch_handle(&handle())
                .await
                .usage
                .unwrap_err();
            assert_eq!(error.error_class(), "credential_unusable");
            assert!(reports.lock().unwrap().is_empty());
        }
        let (source, _) = MockSource::serving(&[]);
        let loader = Arc::new(VaultHandleLoader::default());
        loader.await_first_answer();
        let awaiting = TripoProvider::new_with_handle_loader(Some(source), loader);
        assert!(
            awaiting.handles().is_err(),
            "an unreadable vault is not an absent credential"
        );
        assert!(served.lock().unwrap().is_empty());
    }

    #[test]
    fn tripo_is_registered_and_bare_and_suffixed_vault_keys_route_only_to_it() {
        let registry = crate::Registry::with_defaults(crate::config::QuotaConfig::default(), None);
        assert!(registry.provider_names().contains(&PROVIDER_NAME));
        let loader = Arc::new(VaultHandleLoader::default());
        let (source, _) = MockSource::serving(&[]);
        let provider = TripoProvider::new_with_handle_loader(Some(source), Arc::clone(&loader));
        for id in ["apikey:tripo", ID] {
            loader.install_rows_for_test(&[(id, "apikey"), ("apikey:tripod", "apikey")]);
            assert_eq!(
                provider.handles().unwrap(),
                vec![CredentialHandle::scoped(id, "apikey")]
            );
        }
        assert_eq!(API_BASE, "https://openapi.tripo3d.ai");
    }
}
