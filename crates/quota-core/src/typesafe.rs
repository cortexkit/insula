//! TypeSafe billing from a `cookie:console.typesafe.ai[:<account>]` vault deposit.
//!
//! Fixture-verified only. Translated from CodexBar v0.84.0:
//! `Sources/CodexBarCore/Resources/Plugins/typesafe.ts` and
//! `Sources/CodexBarCore/Providers/TypeSafe/TypeSafeWebFetchStrategy.swift`.
//! No browser extraction or inference API key can supply this billing session.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{value::RawValue, Value};

use crate::{
    http::{Header, JsonRequest},
    model::{AccountInfo, Amount, Pool, PoolBasis, PoolFunding, ProviderUsage, Usage},
    provider::{CredentialHandle, FetchAttempt, FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "typesafe";
const COOKIE_FAMILY: &str = "cookie:console.typesafe.ai";
const OVERVIEW_URL: &str = "https://console.typesafe.ai/api/billing/overview";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Deserialize)]
struct Overview {
    billing: Billing,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Billing {
    spent: f64,
    balance: Box<RawValue>,
    plan: Option<Value>,
    credits: Option<Value>,
}

fn malformed() -> FetchError {
    FetchError::Decode("TypeSafe billing overview could not be read".into())
}

fn expired() -> FetchError {
    FetchError::Unauthorized("TypeSafe session expired; capture a fresh billing session".into())
}

fn amount(raw: &RawValue) -> Result<Amount, FetchError> {
    // Keep JSON decimals intact instead of rounding a balance through f64.
    crate::money::parse_amount(raw.get(), "USD").ok_or_else(malformed)
}

fn single_grant_expiry(credits: Option<&Value>) -> Option<String> {
    let mut positive = None;
    for credit in credits?.as_array()? {
        let remaining = credit.get("remaining")?.as_f64()?;
        if remaining > 0.0 {
            if positive.is_some() {
                return None;
            }
            positive = Some(credit);
        }
    }
    let credit = positive?;
    credit.get("amount")?.as_f64()?;
    let expiry = credit.get("expiresAt")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(expiry).ok()?;
    Some(expiry.to_string())
}

fn normalize(body: &[u8], source: &str) -> Result<ProviderUsage, FetchError> {
    let text = std::str::from_utf8(body).map_err(|_| malformed())?.trim();
    if text.starts_with('<') {
        let lower = text.to_ascii_lowercase();
        if ["login", "sign in", "sign-in", "signin", "(auth)"]
            .iter()
            .any(|marker| lower.contains(marker))
        {
            return Err(expired());
        }
        return Err(malformed());
    }
    let overview: Overview = serde_json::from_slice(body).map_err(|_| malformed())?;
    let billing = overview.billing;
    if !billing.spent.is_finite() {
        return Err(malformed());
    }
    let balance = amount(&billing.balance)?;
    let mut entry = ProviderUsage::healthy(PROVIDER_NAME, None, source, Usage::default());
    entry.spend = Some(vec![Pool {
        id: "balance".into(),
        label: "Credit balance".into(),
        funding: PoolFunding::Unknown,
        remaining: Some(balance),
        total: None,
        basis: PoolBasis::Reported,
        spendable: None,
        resets_at: None,
        // Grant balances overlap the aggregate. A date for one of several
        // grants does not mean the whole balance expires on that date.
        expires_at: single_grant_expiry(billing.credits.as_ref()),
        unlimited: None,
    }]);
    if let Some(plan) = billing
        .plan
        .as_ref()
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
    {
        entry.account_info = Some(AccountInfo {
            plan_type: Some(plan.into()),
            ..Default::default()
        });
    }
    Ok(entry)
}

fn is_login_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.host_str().is_some_and(|host| {
            ["login.", "signin.", "auth.", "sso."]
                .iter()
                .any(|prefix| host.starts_with(prefix))
        }) || url.path_segments().is_some_and(|mut parts| {
            parts.any(|part| {
                ["login", "signin", "sign-in", "auth", "oauth", "sso"]
                    .iter()
                    .any(|marker| part.eq_ignore_ascii_case(marker))
            })
        })
    })
}

fn check_response(status: u16, final_url: &str, location: Option<&str>) -> Result<(), FetchError> {
    if status == 401 {
        return Err(expired());
    }
    // CodexBar treats 403 as expired. Insula does not: a permission denial or
    // network challenge is not proof that the session is dead.
    if status == 403 {
        return Err(FetchError::Upstream("TypeSafe HTTP 403".into()));
    }
    if is_login_url(final_url) {
        return Err(expired());
    }
    if (300..400).contains(&status) {
        let target = reqwest::Url::parse(final_url)
            .ok()
            .and_then(|base| base.join(location?).ok());
        if target
            .as_ref()
            .is_some_and(|url| is_login_url(url.as_str()))
        {
            return Err(expired());
        }
        return Err(malformed());
    }
    if !(200..300).contains(&status) {
        return Err(FetchError::Upstream(format!("TypeSafe HTTP {status}")));
    }
    Ok(())
}

pub struct TypeSafeProvider {
    vault: crate::cookie_vault::CookieVault,
    overview_url: String,
}

impl TypeSafeProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn crate::credential_source::CredentialSource>>,
        handle_loader: Arc<crate::vault_handles::VaultHandleLoader>,
    ) -> Self {
        Self {
            vault: crate::cookie_vault::CookieVault::new(
                credential_source,
                handle_loader,
                COOKIE_FAMILY,
            ),
            overview_url: OVERVIEW_URL.into(),
        }
    }
}

#[async_trait]
impl UsageProvider for TypeSafeProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        self.vault.handles()
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let result = async {
            let (jar, source) = self.vault.jar_for(handle).await?;
            if jar.header().is_empty() {
                return Err(FetchError::CredentialUnusable(
                    "TypeSafe cookie deposit contains no usable cookies".into(),
                ));
            }
            // A flattened Cookie header has no path provenance. Never resend it
            // to a redirect destination, even another route on the same host.
            let http = crate::http::provider_client_without_redirects()?;
            let response = JsonRequest::get(&self.overview_url)
                .timeout(Duration::from_secs(10))
                .header(Header::new("Cookie", jar.header()))
                .header(Header::new("Accept", "application/json"))
                .header(Header::new("User-Agent", USER_AGENT))
                .send_raw(&http)
                .await?;
            check_response(
                response.status,
                &response.final_url,
                response.header("location"),
            )?;
            normalize(response.body_for_parsing()?, source)
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        credential_source::{CredentialSource, VaultCredential, VaultGetError},
        vault_handles::VaultHandleLoader,
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    // CodexBar v0.84.0, TestsPlugin/TypeSafePluginTests.swift, `body` fixture.
    const BILLING: &str = r#"{"billing":{"plan":"free_plan","spent":0.01,"freeCreditsRemaining":4.98,"balance":4.98,"purchased":0,"resetsInDays":12,"cycleLabel":"September 2026","credits":[{"id":"00000000-0000-4000-8000-000000000001","amount":5,"remaining":4.98,"expiresAt":"2026-10-19T00:00:00Z"},{"id":"00000000-0000-4000-8000-000000000002","amount":2,"remaining":0,"expiresAt":"2026-10-19T00:00:00Z"}]},"payments":[],"credits":[],"hasMore":false}"#;
    const COOKIE: &str = "session=fixture";

    fn pool(body: &str) -> Pool {
        normalize(body.as_bytes(), "vault")
            .unwrap()
            .spend
            .unwrap()
            .remove(0)
    }

    #[test]
    fn billing_is_one_exact_reported_balance_not_spent_or_summed_grants() {
        let entry = normalize(BILLING.as_bytes(), "vault").unwrap();
        assert_eq!(entry.account, None);
        assert_eq!(entry.source.as_deref(), Some("vault"));
        assert_eq!(
            entry.account_info.unwrap().plan_type.as_deref(),
            Some("free_plan")
        );
        assert_eq!(entry.usage, Some(Usage::default()));
        let pools = entry.spend.unwrap();
        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        assert_eq!(pool.id, "balance");
        assert_eq!(pool.basis, PoolBasis::Reported);
        assert_eq!(pool.funding, PoolFunding::Unknown);
        assert_eq!(
            pool.remaining,
            Some(Amount {
                minor: 498,
                exponent: 2,
                unit: "USD".into()
            })
        );
        assert_eq!(pool.total, None);
        assert_eq!(pool.spendable, None);
        assert_eq!(pool.resets_at, None);
        assert_eq!(pool.unlimited, None);
        assert_eq!(pool.expires_at.as_deref(), Some("2026-10-19T00:00:00Z"));
    }

    #[test]
    fn zero_negative_and_high_precision_balances_remain_exact() {
        for (raw, minor, exponent) in [
            ("0", 0, 0),
            ("-1.250000", -1_250_000, 6),
            ("90071992547409.91", 9_007_199_254_740_991, 2),
            ("0.000000001", 1, 9),
        ] {
            let amount = pool(&BILLING.replace("\"balance\":4.98", &format!("\"balance\":{raw}")))
                .remaining
                .unwrap();
            assert_eq!((amount.minor, amount.exponent), (minor, exponent), "{raw}");
        }
    }

    #[test]
    fn only_one_positive_grant_with_readable_expiry_dates_the_aggregate() {
        let body = |credits: &str| {
            format!(r#"{{"billing":{{"spent":1,"balance":9,"credits":{credits}}}}}"#)
        };
        for credits in [
            "null",
            "{}",
            "[]",
            r#"[{"amount":5,"remaining":0,"expiresAt":"2027-01-01T00:00:00Z"}]"#,
            r#"[{"amount":5,"remaining":4,"expiresAt":"invalid"}]"#,
            r#"[{"amount":5,"remaining":4}]"#,
            r#"[{"amount":5,"remaining":4,"expiresAt":"2027-01-01T00:00:00Z"},{"amount":5,"remaining":4,"expiresAt":"2028-01-01T00:00:00Z"}]"#,
            r#"[{"amount":5,"remaining":4,"expiresAt":"2027-01-01T00:00:00Z"},{"amount":5,"remaining":4,"expiresAt":"invalid"}]"#,
            r#"[{"amount":5,"remaining":4,"expiresAt":"2027-01-01T00:00:00Z"},{}]"#,
        ] {
            assert_eq!(pool(&body(credits)).expires_at, None, "{credits}");
        }
        assert_eq!(
            pool(r#"{"billing":{"spent":0,"balance":0}}"#).expires_at,
            None
        );
    }

    #[test]
    fn missing_or_unreadable_payloads_are_decode_failures_not_absence_or_zero() {
        let mut bodies = vec![
            "not json".to_string(),
            "{}".into(),
            "null".into(),
            "[]".into(),
        ];
        for field in ["spent", "balance"] {
            for value in ["null", "true", "\"private-response\"", "{}", "1e999"] {
                let old = if field == "spent" {
                    "\"spent\":0.01"
                } else {
                    "\"balance\":4.98"
                };
                bodies.push(BILLING.replace(old, &format!("\"{field}\":{value}")));
            }
        }
        bodies.push(r#"{"billing":{"spent":0}}"#.into());
        bodies.push(r#"{"billing":{"balance":0}}"#.into());
        for body in bodies {
            let error = normalize(body.as_bytes(), "vault").unwrap_err();
            assert_eq!(error.error_class(), "decode_failed", "{body}");
            assert!(!error.to_string().contains("private-response"));
        }
    }

    #[test]
    fn login_evidence_is_rejected_but_permission_and_integration_failures_are_not() {
        for url in [
            "https://console.typesafe.ai/login",
            "https://auth.typesafe.ai/",
        ] {
            assert_eq!(
                check_response(200, url, None).unwrap_err().error_class(),
                "credential_rejected"
            );
        }
        for location in [
            "/login?next=%2Fbilling",
            "/sign-in",
            "https://sso.example.com/",
        ] {
            assert_eq!(
                check_response(307, OVERVIEW_URL, Some(location))
                    .unwrap_err()
                    .error_class(),
                "credential_rejected"
            );
        }
        for status in [403, 408, 429, 500, 503] {
            let error = check_response(status, OVERVIEW_URL, None).unwrap_err();
            assert_eq!(error.error_class(), "upstream_failed");
            assert!(matches!(
                crate::refresh::classify(&error),
                crate::refresh::FetchClass::Transient
            ));
        }
        assert_eq!(
            check_response(401, OVERVIEW_URL, None)
                .unwrap_err()
                .error_class(),
            "credential_rejected"
        );
        assert_eq!(
            check_response(307, OVERVIEW_URL, Some("/billing/"))
                .unwrap_err()
                .error_class(),
            "decode_failed"
        );
        // Login and moved-page bodies are from TypeSafePluginTests.swift at v0.84.0.
        assert_eq!(
            normalize(
                b"<html><title>TypeSafe</title><body>Sign in to continue</body></html>",
                "vault"
            )
            .unwrap_err()
            .error_class(),
            "credential_rejected"
        );
        assert_eq!(
            normalize(b"<html>Billing moved</html>", "vault")
                .unwrap_err()
                .error_class(),
            "decode_failed"
        );
    }

    struct CookieSource;

    #[async_trait]
    impl CredentialSource for CookieSource {
        async fn get_scoped(&self, id: &str, ttl: u64) -> Result<VaultCredential, VaultGetError> {
            assert_eq!(id, format!("{COOKIE_FAMILY}:fixture"));
            assert_eq!(ttl, 120_000);
            Ok(VaultCredential {
                payload: COOKIE.as_bytes().to_vec(),
                expires_at_ms: None,
                record_version: 1,
                account_id: None,
                email: None,
                org_name: None,
                project_id: None,
            })
        }

        async fn report_auth_failure_scoped(&self, _: &str, _: u16, _: u64) {
            panic!("cookie providers must not report authentication failures to the vault");
        }
    }

    async fn fetch(status: u16, body: &str, location: Option<&str>) -> FetchAttempt {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_string();
        let redirect = location
            .map(|url| format!("Location: {url}\r\n"))
            .unwrap_or_default();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = crate::loopback::read_request(&mut stream).await;
            assert!(
                request.starts_with("GET /api/billing/overview HTTP/1.1\r\n"),
                "{request}"
            );
            assert!(request
                .to_ascii_lowercase()
                .contains("accept: application/json\r\n"));
            assert!(
                request.contains(&format!("user-agent: {USER_AGENT}\r\n")),
                "{request}"
            );
            assert!(
                request.contains(&format!("cookie: {COOKIE}\r\n")),
                "{request}"
            );
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            stream.write_all(format!("HTTP/1.1 {status} Test\r\n{redirect}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let mut provider = TypeSafeProvider::new_with_handle_loader(
            Some(Arc::new(CookieSource)),
            Arc::new(VaultHandleLoader::default()),
        );
        provider.overview_url = format!("http://{address}/api/billing/overview");
        let result = provider
            .fetch_handle(&CredentialHandle::scoped(
                format!("{COOKIE_FAMILY}:fixture"),
                "cookie",
            ))
            .await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn billing_get_uses_the_deposit_and_browser_ua_without_inventing_identity() {
        let attempt = fetch(200, BILLING, None).await;
        assert!(attempt.usage.is_ok());
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert!(attempt.observed.unwrap().account_id.is_none());
        assert_eq!(
            attempt.pools.unwrap()[0].remaining.as_ref().unwrap().minor,
            498
        );
    }

    #[tokio::test]
    async fn cookie_header_is_never_forwarded_to_any_redirect_destination() {
        let sink = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let target = format!("http://{}/billing", sink.local_addr().unwrap());
        for location in ["/billing", target.as_str()] {
            let error = fetch(307, "", Some(location)).await.usage.unwrap_err();
            assert_eq!(error.error_class(), "decode_failed");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), sink.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn refused_bodies_never_echo_secrets_or_report_cookie_auth_to_the_vault() {
        let body = format!("{COOKIE} {}", "private-response".repeat(1000));
        for status in [401, 403, 404, 429, 503] {
            let attempt = fetch(status, &body, None).await;
            let error = attempt.usage.unwrap_err();
            assert!(!error.to_string().contains(COOKIE));
            assert!(!error.to_string().contains("private-response"));
            assert!(error.to_string().len() < 200);
        }
    }
}
