//! OpenAI Platform prepaid API credits, read with a vault API key.
//!
//! This billing read is separate from ChatGPT subscription usage. Some keys
//! cannot access it; a permission denial does not mean the API key is dead.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::{
    credential_source::{
        get_vault_credential, report_vault_auth_failure, take_utf8_payload, CredentialSource,
        VAULT_READ_MIN_TTL_MS,
    },
    http::JsonRequest,
    model::{Amount, Pool, PoolBasis, PoolFunding, Usage},
    provider::{
        AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
    },
    vault_handles::VaultHandleLoader,
};

const PROVIDER_NAME: &str = "openai-platform";
const CREDIT_GRANTS_URL: &str = "https://api.openai.com/v1/dashboard/billing/credit_grants";

#[derive(Deserialize)]
struct Credits {
    total_granted: Box<RawValue>,
    total_used: Box<RawValue>,
    total_available: Box<RawValue>,
    grants: Option<Grants>,
}

#[derive(Deserialize)]
struct Grants {
    data: Vec<Grant>,
}

#[derive(Deserialize)]
struct Grant {
    expires_at: Option<i64>,
}

fn decode_error() -> FetchError {
    FetchError::Decode("openai-platform: invalid credit balance".to_string())
}

fn amount(raw: &RawValue) -> Result<Amount, FetchError> {
    // RawValue keeps decimal JSON numbers intact, including values beyond f64's
    // exact range. Never round a balance through a binary float.
    crate::money::parse_amount(raw.get(), "USD").ok_or_else(decode_error)
}

fn normalize_pool(body: &[u8], now: DateTime<Utc>) -> Result<Pool, FetchError> {
    let credits: Credits = serde_json::from_slice(body).map_err(|_| decode_error())?;
    let total = amount(&credits.total_granted)?;
    amount(&credits.total_used)?;
    let remaining = amount(&credits.total_available)?;
    let expires_at = credits
        .grants
        .into_iter()
        .flat_map(|grants| grants.data)
        .filter_map(|grant| {
            grant
                .expires_at
                .and_then(|at| DateTime::from_timestamp(at, 0))
        })
        .filter(|at| *at > now)
        .min();
    Ok(Pool {
        id: "credits".to_string(),
        label: "API credits".to_string(),
        // The aggregate does not distinguish purchased credit from promotions.
        funding: PoolFunding::Unknown,
        remaining: Some(remaining),
        total: (total.minor > 0).then_some(total),
        basis: PoolBasis::Reported,
        spendable: None,
        resets_at: None,
        expires_at: expires_at.map(|at| at.to_rfc3339()),
        unlimited: None,
    })
}

#[derive(Default, Debug, Clone, PartialEq, Eq, Deserialize)]
struct ErrorFields {
    message: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    code: Option<String>,
}

fn safe_field(value: &str, key: &str) -> String {
    let mut value = value.replace(key, "[redacted]");
    // OpenAI's invalid-key message can echo a masked key instead of the exact
    // bearer. Remove those fragments too, before either logging or publishing.
    while let Some(start) = value.find("sk-") {
        let end = value[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '\"' | '\'' | ','))
            .map_or(value.len(), |end| start + end);
        value.replace_range(start..end, "[redacted]");
    }
    value.chars().take(512).collect()
}

fn error_fields(body: &[u8], key: &str) -> ErrorFields {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
    let field = |name| match &parsed["error"][name] {
        serde_json::Value::String(value) => Some(safe_field(value, key)),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    };
    ErrorFields {
        // A refusal outside OpenAI's `{"error":{...}}` shape (an edge-proxy page,
        // or an empty body) would otherwise log and publish nothing, leaving
        // "why was this key refused" unanswerable. Fall back to the start of the
        // body itself, as other providers publish refusal bodies.
        message: field("message").or_else(|| body_excerpt(body, key)),
        kind: field("type"),
        code: field("code"),
    }
}

/// The first 200 printable characters of a refusal body, with key material
/// removed, or `"empty body"` when there is nothing to show.
fn body_excerpt(body: &[u8], key: &str) -> Option<String> {
    let text: String = String::from_utf8_lossy(body)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Some("empty body".to_string());
    }
    Some(safe_field(&text, key).chars().take(200).collect())
}

fn status_error(status: u16, fields: &ErrorFields) -> FetchError {
    let detail = format!(
        "openai-platform HTTP {status}: {}",
        fields
            .message
            .as_deref()
            .unwrap_or("credit balance request refused")
    );
    match status {
        401 => FetchError::ProviderStatus(401, detail),
        403 => FetchError::CredentialUnusable(detail),
        _ => FetchError::Upstream(detail),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    status: Option<u16>,
    class: &'static str,
    fields: ErrorFields,
}

#[derive(Default)]
struct Outcomes(Mutex<HashMap<String, Outcome>>);

impl Outcomes {
    fn changed(&self, id: &str, outcome: Outcome) -> bool {
        let mut outcomes = self.0.lock().unwrap();
        if outcomes.get(id) == Some(&outcome) {
            return false;
        }
        outcomes.insert(id.to_string(), outcome);
        true
    }
}

pub struct OpenAIPlatformProvider {
    source: Option<Arc<dyn CredentialSource>>,
    loader: Arc<VaultHandleLoader>,
    http: reqwest::Client,
    credits_url: String,
    outcomes: Outcomes,
}

impl OpenAIPlatformProvider {
    pub(crate) fn new_with_handle_loader(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            source,
            loader,
            http: crate::http::provider_client(),
            credits_url: CREDIT_GRANTS_URL.to_string(),
            outcomes: Outcomes::default(),
        }
    }

    fn log_outcome(
        &self,
        handle: &CredentialHandle,
        status: Option<u16>,
        fields: ErrorFields,
        class: &'static str,
    ) {
        let outcome = Outcome {
            status,
            class,
            fields,
        };
        if self.outcomes.changed(handle.stable_id(), outcome.clone()) {
            eprintln!("{} openai-platform credential_id={:?} status={:?} outcome={} message={:?} type={:?} code={:?}", crate::LOG_TAG, handle.stable_id(), outcome.status, outcome.class, outcome.fields.message, outcome.fields.kind, outcome.fields.code);
        }
    }

    fn vault_failure(
        &self,
        handle: &CredentialHandle,
        error: crate::credential_source::VaultGetError,
    ) -> FetchAttempt {
        let attempt = FetchAttempt::unverified_vault_failure(error);
        self.log_outcome(
            handle,
            None,
            ErrorFields::default(),
            attempt.usage.as_ref().unwrap_err().error_class(),
        );
        attempt
    }
}

#[async_trait]
impl UsageProvider for OpenAIPlatformProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        let handles = self.loader.openai_platform_handles()?;
        self.outcomes
            .0
            .lock()
            .unwrap()
            .retain(|id, _| handles.iter().any(|h| h.stable_id() == id));
        Ok(handles)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let Some(source) = self.source.as_ref() else {
            return self.vault_failure(handle, crate::credential_source::VaultGetError::Permanent);
        };
        let mut credential = match get_vault_credential(source, handle, VAULT_READ_MIN_TTL_MS).await
        {
            Ok(credential) => credential,
            Err(error) => return self.vault_failure(handle, error),
        };
        let observed = Some(AccountObservation::new(
            credential.account_id.clone(),
            Some(credential.record_version),
        ));
        let mut status = None;
        let mut fields = ErrorFields::default();
        let result = async {
            let key = take_utf8_payload(&mut credential.payload)?;
            let key = key.trim();
            if key.is_empty() {
                return Err(FetchError::CredentialUnusable(
                    "openai-platform vault deposit holds no API key".to_string(),
                ));
            }
            let response = JsonRequest::get(&self.credits_url)
                .bearer(key)
                .timeout(Duration::from_secs(15))
                .send_raw(&self.http)
                .await?;
            status = Some(response.status);
            if !(200..300).contains(&response.status) {
                fields = error_fields(&response.body, key);
                let error = status_error(response.status, &fields);
                report_vault_auth_failure(Some(source), handle, credential.record_version, &error);
                return Err(error);
            }
            normalize_pool(&response.body, Utc::now())
        }
        .await;
        self.log_outcome(
            handle,
            status,
            fields,
            result.as_ref().err().map_or("ok", FetchError::error_class),
        );
        let mut attempt = match result {
            Ok(pool) => {
                let mut attempt = FetchAttempt::success(observed, "vault", Usage::default());
                attempt.pools = Some(vec![pool]);
                attempt
            }
            Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error),
        };
        attempt.account_info = credential.account_info();
        attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_source::{VaultCredential, VaultGetError};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    // CodexBar v0.43.0, Tests/CodexBarTests/OpenAIAPICreditBalanceTests.swift
    // (MIT), `parses credit grants balance`.
    const CREDITS: &str = r#"{
        "object":"credit_summary", "total_granted":25.5,
        "total_used":7.25, "total_available":18.25,
        "grants":{"object":"list","data":[{
            "grant_amount":10.0,"used_amount":1.0,
            "effective_at":1690000000,"expires_at":1800000000
        }]}
    }"#;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn decodes_codexbar_balance_without_inventing_windows() {
        let pool = normalize_pool(CREDITS.as_bytes(), now()).unwrap();
        assert_eq!(pool.id, "credits");
        assert_eq!(pool.label, "API credits");
        assert_eq!(
            pool.remaining,
            Some(Amount {
                minor: 1825,
                exponent: 2,
                unit: "USD".into()
            })
        );
        assert_eq!(
            pool.total,
            Some(Amount {
                minor: 255,
                exponent: 1,
                unit: "USD".into()
            })
        );
        assert_eq!(
            pool.expires_at.as_deref(),
            Some("2027-01-15T08:00:00+00:00")
        );
        assert_eq!(pool.funding, PoolFunding::Unknown);
        assert_eq!(pool.basis, PoolBasis::Reported);
        assert!(pool.resets_at.is_none());
        assert!(pool.spendable.is_none());
    }

    #[test]
    fn decimal_json_amounts_remain_exact_and_zero_total_is_absent() {
        let body = br#"{"total_granted":0,"total_used":0,"total_available":900719925474099.13}"#;
        let pool = normalize_pool(body, now()).unwrap();
        assert_eq!(pool.remaining.unwrap().minor, 90_071_992_547_409_913);
        assert!(pool.total.is_none());
        assert!(pool.expires_at.is_none());
        let negative = normalize_pool(
            br#"{"total_granted":-1,"total_used":0,"total_available":-0.01}"#,
            now(),
        )
        .unwrap();
        assert!(negative.total.is_none());
        assert_eq!(negative.remaining.unwrap().minor, -1);
    }

    #[test]
    fn expiry_is_the_earliest_strictly_future_grant_not_a_reset() {
        let body = br#"{"total_granted":10,"total_used":0,"total_available":10,
            "grants":{"data":[{"expires_at":1600000000},{"expires_at":1700000000},
            {"expires_at":1900000000},{"expires_at":1800000000},{"expires_at":null},{}]}}"#;
        let pool = normalize_pool(body, now()).unwrap();
        assert_eq!(
            pool.expires_at.as_deref(),
            Some("2027-01-15T08:00:00+00:00")
        );
        assert!(pool.resets_at.is_none());
        assert!(
            normalize_pool(body, DateTime::from_timestamp(2_000_000_000, 0).unwrap())
                .unwrap()
                .expires_at
                .is_none()
        );
    }

    #[test]
    fn malformed_success_is_decode_failed_not_zero_credit() {
        for body in [
            "",
            "{}",
            "<html>sign in</html>",
            r#"{"total_granted":1,"total_used":0,"total_available":"invalid"}"#,
        ] {
            assert_eq!(
                normalize_pool(body.as_bytes(), now())
                    .unwrap_err()
                    .error_class(),
                "decode_failed"
            );
        }
    }

    #[test]
    fn diagnostic_changes_are_isolated_per_credential_and_redact_keys() {
        let fields = error_fields(br#"{"error":{"message":"Incorrect key sk-masked*** or served-secret","type":"invalid_request_error","code":"invalid_api_key"},"total_available":123}"#, "served-secret");
        assert_eq!(
            fields.message.as_deref(),
            Some("Incorrect key [redacted] or [redacted]")
        );
        assert_eq!(fields.kind.as_deref(), Some("invalid_request_error"));
        assert_eq!(fields.code.as_deref(), Some("invalid_api_key"));
        let outcomes = Outcomes::default();
        let failed = Outcome {
            status: Some(401),
            class: "credential_rejected",
            fields,
        };
        assert!(outcomes.changed("apikey:openai", failed.clone()));
        assert!(!outcomes.changed("apikey:openai", failed.clone()));
        assert!(outcomes.changed("apikey:openai:fixture", failed.clone()));
        let ok = Outcome {
            status: Some(200),
            class: "ok",
            fields: ErrorFields::default(),
        };
        assert!(outcomes.changed("apikey:openai", ok.clone()));
        assert!(!outcomes.changed("apikey:openai", ok));
        assert!(outcomes.changed("apikey:openai", failed));
    }

    struct Source {
        account: Option<String>,
        reports: Arc<Mutex<Vec<(String, u16, u64)>>>,
    }

    #[async_trait]
    impl CredentialSource for Source {
        async fn get_scoped(&self, id: &str, ttl: u64) -> Result<VaultCredential, VaultGetError> {
            assert_eq!(id, "apikey:openai:fixture");
            assert_eq!(ttl, VAULT_READ_MIN_TTL_MS);
            Ok(VaultCredential {
                payload: b"synthetic-key".to_vec(),
                expires_at_ms: None,
                record_version: 7,
                account_id: self.account.clone(),
                project_id: None,
                email: None,
                org_name: None,
            })
        }
        async fn report_auth_failure_scoped(&self, id: &str, status: u16, version: u64) {
            self.reports
                .lock()
                .unwrap()
                .push((id.into(), status, version));
        }
    }

    async fn fetch(
        status: u16,
        body: &str,
        account: Option<&str>,
    ) -> (FetchAttempt, Vec<(String, u16, u64)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/dashboard/billing/credit_grants",
            listener.local_addr().unwrap()
        );
        let body = body.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                bytes.push(byte[0]);
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.starts_with("GET /v1/dashboard/billing/credit_grants HTTP/1.1\r\n"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer synthetic-key\r\n"));
            socket.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let reports = Arc::new(Mutex::new(Vec::new()));
        let source: Arc<dyn CredentialSource> = Arc::new(Source {
            account: account.map(str::to_string),
            reports: reports.clone(),
        });
        let mut provider = OpenAIPlatformProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
        );
        provider.credits_url = url;
        let attempt = provider
            .fetch_handle(&CredentialHandle::scoped("apikey:openai:fixture", "apikey"))
            .await;
        server.await.unwrap();
        // Await a task turn for the shared fire-and-forget vault report.
        tokio::task::yield_now().await;
        let reported = reports.lock().unwrap().clone();
        (attempt, reported)
    }

    #[tokio::test]
    async fn session_key_403_is_unusable_and_never_reports_a_dead_key() {
        let message = "Your request to GET /v1/dashboard/billing/credit_grants must be made with a session key (that is, it can only be made from the browser).";
        let body = serde_json::json!({"error":{"message":message,"type":"invalid_request_error","code":null}}).to_string();
        let (attempt, reports) = fetch(403, &body, None).await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable");
        assert!(error.to_string().contains(message));
        assert!(reports.is_empty());
    }

    #[test]
    fn a_refusal_outside_the_error_shape_still_says_what_came_back() {
        let page = b"<html>\n<title>403 Forbidden</title> served-secret sk-abc\n</html>";
        assert_eq!(
            error_fields(page, "served-secret").message.as_deref(),
            Some("<html> <title>403 Forbidden</title> [redacted] [redacted] </html>")
        );
        assert_eq!(
            error_fields(b"", "served-secret").message.as_deref(),
            Some("empty body")
        );
        let long = "x".repeat(500);
        assert_eq!(
            error_fields(long.as_bytes(), "k").message.unwrap().len(),
            200
        );
    }

    #[tokio::test]
    async fn only_served_bearer_401_reports_auth_failure_and_other_statuses_are_transient() {
        for (status, class) in [
            (401, "credential_rejected"),
            (429, "upstream_failed"),
            (503, "upstream_failed"),
        ] {
            let (attempt, reports) = fetch(
                status,
                r#"{"error":{"message":"refused","type":"request_error","code":"denied"}}"#,
                None,
            )
            .await;
            let error = attempt.usage.unwrap_err();
            assert_eq!(error.error_class(), class, "HTTP {status}");
            assert_eq!(
                crate::refresh::classify(&error) == crate::refresh::FetchClass::Transient,
                status != 401
            );
            assert_eq!(
                reports,
                if status == 401 {
                    vec![("apikey:openai:fixture".into(), 401, 7)]
                } else {
                    vec![]
                }
            );
        }
    }

    #[tokio::test]
    async fn success_uses_only_vault_identity_and_publishes_one_pool() {
        for account in [None, Some("synthetic-org")] {
            let (attempt, reports) = fetch(200, CREDITS, account).await;
            assert_eq!(attempt.observed.unwrap().account_id.as_deref(), account);
            assert_eq!(attempt.source.as_deref(), Some("vault"));
            assert_eq!(
                serde_json::to_value(attempt.usage.unwrap()).unwrap(),
                serde_json::json!({})
            );
            assert_eq!(attempt.pools.unwrap().len(), 1);
            assert!(reports.is_empty());
        }
    }
}
