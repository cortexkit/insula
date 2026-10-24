//! Trae CN credit usage from a `cookie:trae.cn[:<account>]` vault deposit.
//!
//! VERIFICATION: FIXTURE-VERIFIED, not live-verified; no Trae CN account exists
//! on this host. Wire contract and fixtures come from CodexBar v0.81.0
//! (`ee890b5f392e70d6e20162d3e95cf9f1be265b60`):
//! - `Sources/CodexBarCore/Resources/Plugins/traecn.js`
//! - `Sources/CodexBarCore/Providers/TraeCN/TraeCNProviderDescriptor.swift`
//! - `Tests/CodexBarTests/TraeCNPluginTests.swift`
//!
//! Upstream reads a browser: automatically from the parent domain `trae.cn`,
//! whose session cookies the dashboard and the API share, or from a Cookie
//! header pasted by hand from a request to `api.trae.cn`. Insula never reads a
//! browser. Either kind of header arrives as the one deposit for this family,
//! and its parsed cookies are sent to `api.trae.cn` and nowhere else. That is the one
//! origin upstream sends it to: before v0.81.0 upstream built each URL from the
//! cookie's domain, and once that domain became the parent `trae.cn` it pinned
//! both calls to `api.trae.cn` instead, so a session imported from the parent
//! domain is never sent to the parent site itself.
//!
//! Two calls per refresh, both POSTs to `api.trae.cn`: the session is exchanged
//! for a short-lived token, then the entitlement summary is read with that
//! token. The token lives only for that refresh and is never stored.
//!
//! The summary aggregates every credit pack on the account, so it is published
//! as one derived credit pool. Pack expiry and billing timestamps describe
//! single packs, so no reset, renewal or cadence is inferred from them. The
//! response carries no stable user id, so no account identity is published.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    http::{Header, JsonRequest},
    model::{AccountInfo, Amount, Pool, PoolBasis, PoolFunding, ProviderUsage, Usage},
    provider::{CredentialHandle, FetchAttempt, FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "traecn";
const COOKIE_FAMILY: &str = "cookie:trae.cn";
/// Exchanges the session cookie for a short-lived `Cloud-IDE-JWT` token.
const TOKEN_URL: &str = "https://api.trae.cn/cloudide/api/v3/common/GetUserToken";
/// Reads the signed-in account's entitlement summary with that token.
const ENTITLEMENT_URL: &str = "https://api.trae.cn/trae/api/v2/pay/user_current_entitlement_list";
/// The dashboard origin both calls claim to come from, as the browser would
/// send it. It is a header value only: no request is ever sent there.
const ORIGIN_URL: &str = "https://www.trae.cn";
const REFERER_URL: &str = "https://www.trae.cn/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const TOKEN_BODY: &str = "{}";
const ENTITLEMENT_BODY: &str = r#"{"require_usage":true,"full_data":true}"#;
/// The entitlement response's in-band "not signed in" code, which upstream
/// treats as a rejected session even on HTTP 200.
const SIGNED_OUT_CODE: f64 = 1001.0;
/// The response states credits without a currency. A plain credit unit, as
/// other credit pools publish, keeps a consumer from reading it as money.
const CREDIT_UNIT: &str = "credit";
/// Credit figures arrive as JSON numbers, so a fraction can carry float noise
/// past any stated precision. The shared amount parser refuses more than nine
/// places, and nine keeps every digit a credit figure could mean.
const MAX_FRACTION_DIGITS: usize = 9;

/// One request this provider makes: where it goes, what it carries.
///
/// Built as data before it is sent, so a test can read exactly which origin
/// each header reaches without a live account.
#[derive(Debug, Clone)]
struct Call {
    url: &'static str,
    headers: Vec<(&'static str, String)>,
    body: &'static str,
}

/// The headers both calls carry. The session cookie goes on both, because
/// both are on the API origin that the session authorizes.
fn session_headers(cookie: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Cookie", cookie.to_string()),
        ("Origin", ORIGIN_URL.to_string()),
        ("Referer", REFERER_URL.to_string()),
    ]
}

fn token_call(cookie: &str) -> Call {
    Call {
        url: TOKEN_URL,
        headers: session_headers(cookie),
        body: TOKEN_BODY,
    }
}

fn entitlement_call(cookie: &str, token: &str) -> Call {
    let mut headers = session_headers(cookie);
    headers.push(("Authorization", format!("Cloud-IDE-JWT {token}")));
    Call {
        url: ENTITLEMENT_URL,
        headers,
        body: ENTITLEMENT_BODY,
    }
}

/// What came back for one [`Call`].
struct Reply {
    status: u16,
    body: Vec<u8>,
}

/// Sends a [`Call`]. A trait so tests can answer with upstream's fixtures and
/// record what each call carried.
#[async_trait]
trait Transport: Send + Sync {
    async fn post(&self, call: &Call) -> Result<Reply, FetchError>;
}

struct HttpTransport {
    client: reqwest::Client,
}

#[async_trait]
impl Transport for HttpTransport {
    async fn post(&self, call: &Call) -> Result<Reply, FetchError> {
        let mut request = JsonRequest::post_json(call.url, call.body.as_bytes().to_vec())
            .timeout(REQUEST_TIMEOUT);
        for &(name, ref value) in &call.headers {
            request = request.header(Header::new(name, value.clone()));
        }
        let response = request.send_raw(&self.client).await?;
        Ok(Reply {
            status: response.status,
            body: response.body,
        })
    }
}

/// The shared pool settings, with redirects disabled.
///
/// Upstream reads a redirect as a signed-out session. Following one would
/// instead land on a sign-in page and misreport it as a format change, and
/// would carry the session to wherever the redirect pointed on the same host.
/// If this client cannot be built, the shared `provider_client()` is used
/// instead; it follows redirects, but drops the Cookie and Authorization
/// headers whenever a redirect leaves the host.
fn no_redirect_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(crate::http::POOL_IDLE_TIMEOUT)
        .tcp_keepalive(crate::http::POOL_IDLE_TIMEOUT)
        .build()
        .unwrap_or_else(|_| crate::http::provider_client())
}

/// Classify one reply the way upstream does, returning the body to parse.
///
/// A signed-out session (401, 403 or any redirect) is kept apart from every
/// other failure: only the first is fixed by capturing the login again, while
/// a rate limit, an outage or an unexpected status passes on its own. Upstream
/// separates them the same way. Neither message carries the response body,
/// which upstream also never echoes.
fn accept(reply: &Reply) -> Result<&[u8], FetchError> {
    let status = reply.status;
    if matches!(status, 401 | 403 | 300..=399) {
        return Err(FetchError::Unauthorized(format!(
            "traecn session rejected (HTTP {status})"
        )));
    }
    if status != 200 {
        return Err(FetchError::ProviderStatus(
            status,
            "traecn request failed".to_string(),
        ));
    }
    if reply.body.is_empty() {
        // An empty success is a flap, not a format change, so it keeps the
        // last good reading instead of replacing it with a decode failure.
        return Err(FetchError::Upstream(format!(
            "traecn HTTP 200: {}",
            crate::http::EMPTY_BODY_MARKER
        )));
    }
    Ok(&reply.body)
}

fn decode_error(what: &str) -> FetchError {
    FetchError::Decode(format!("traecn: {what}"))
}

/// The exchanged token, from `Result.Token`.
fn read_token(body: &[u8]) -> Result<String, FetchError> {
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| decode_error("token response is not JSON"))?;
    payload
        .get("Result")
        .and_then(|result| result.get("Token"))
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| decode_error("token response has no Result.Token"))
}

/// One credit figure. Upstream accepts only a finite, non-negative JSON
/// number; a string, a negative or a missing figure is a format change, never
/// zero usage.
fn credits(summary: &Value, field: &str) -> Result<Amount, FetchError> {
    let invalid = || decode_error(&format!("invalid usage_summary.{field}"));
    let number = match summary.get(field) {
        Some(Value::Number(number)) => number,
        _ => return Err(invalid()),
    };
    if !number.as_f64().is_some_and(|value| value >= 0.0) {
        return Err(invalid());
    }
    let text = number.to_string();
    let text = match text.split_once('.') {
        Some((whole, fraction)) if fraction.len() > MAX_FRACTION_DIGITS => {
            // Toward zero: a figure is never overstated by the cut.
            format!("{whole}.{}", &fraction[..MAX_FRACTION_DIGITS])
        }
        _ => text,
    };
    crate::money::parse_amount(&text, CREDIT_UNIT).ok_or_else(invalid)
}

/// Restate two amounts at the larger exponent so they can be subtracted and
/// compared. Scaling up by a power of ten is exact; an overflow is refused.
fn same_exponent(a: Amount, b: Amount) -> Option<(Amount, Amount)> {
    let exponent = a.exponent.max(b.exponent);
    let rescale = |amount: Amount| -> Option<Amount> {
        let factor = 10i64.checked_pow(u32::from(exponent - amount.exponent))?;
        Some(Amount {
            minor: amount.minor.checked_mul(factor)?,
            exponent,
            unit: amount.unit,
        })
    };
    Some((rescale(a)?, rescale(b)?))
}

/// The membership pack's plan label: the first pack with `group_type` 0 whose
/// `status` is absent or 1, as upstream selects it.
fn plan_label(payload: &Value) -> Option<String> {
    payload
        .get("user_entitlement_pack_list")
        .and_then(Value::as_array)?
        .iter()
        .find(|pack| {
            pack.get("group_type").and_then(Value::as_f64) == Some(0.0)
                && match pack.get("status") {
                    None => true,
                    Some(status) => status.as_f64() == Some(1.0),
                }
        })?
        .get("display_desc")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .map(str::to_string)
}

/// What one entitlement response publishes.
#[derive(Debug)]
struct Reading {
    pool: Pool,
    plan: Option<String>,
}

/// Decode the entitlement response into the account's credit pool.
fn normalize(body: &[u8]) -> Result<Reading, FetchError> {
    let payload: Value = serde_json::from_slice(body)
        .map_err(|_| decode_error("entitlement response is not JSON"))?;
    if payload.get("code").and_then(Value::as_f64) == Some(SIGNED_OUT_CODE) {
        return Err(FetchError::Unauthorized(
            "traecn session signed out (code 1001)".to_string(),
        ));
    }
    // Dollar billing reports a different product in the same fields; reading
    // it as credits would publish money as a credit count.
    if payload.get("is_credits_billing") != Some(&Value::Bool(true)) {
        return Err(decode_error("account is not on credit billing"));
    }
    let summary = payload
        .get("usage_summary")
        .ok_or_else(|| decode_error("missing usage_summary"))?;
    let total = credits(summary, "total_amount")?;
    let used = credits(summary, "consumed_amount")?;
    let (total, used) =
        same_exponent(total, used).ok_or_else(|| decode_error("credit figures overflow"))?;
    // Clamped as upstream clamps it: consumption past the allowance leaves
    // nothing, not a negative balance.
    let remaining = Amount {
        minor: total.minor.saturating_sub(used.minor).max(0),
        exponent: total.exponent,
        unit: total.unit.clone(),
    };
    Ok(Reading {
        pool: Pool {
            // The response's own name for the figure.
            id: "usage_summary".to_string(),
            label: "Credits".to_string(),
            // The summary spans subscription and add-on packs alike, so no
            // single funding kind is true of it.
            funding: PoolFunding::Unknown,
            remaining: Some(remaining),
            total: Some(total),
            // Computed from a total and a consumption figure covering every
            // pack, not stated as a remainder.
            basis: PoolBasis::Derived,
            spendable: None,
            // Packs expire independently; none of their dates is a reset or an
            // expiry of the summed pool.
            resets_at: None,
            expires_at: None,
            unlimited: None,
        },
        plan: plan_label(&payload),
    })
}

/// The two-call exchange for one deposited session.
async fn exchange(cookie: &str, transport: &dyn Transport) -> Result<Reading, FetchError> {
    let reply = transport.post(&token_call(cookie)).await?;
    let token = read_token(accept(&reply)?)?;
    let reply = transport.post(&entitlement_call(cookie, &token)).await?;
    normalize(accept(&reply)?)
}

pub struct TraeCnProvider {
    vault: crate::cookie_vault::CookieVault,
    transport: Arc<dyn Transport>,
}

impl TraeCnProvider {
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
            transport: Arc::new(HttpTransport {
                client: no_redirect_client(),
            }),
        }
    }
}

#[async_trait]
impl UsageProvider for TraeCnProvider {
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
            // Upstream names no required session cookie, so any captured
            // cookie is tried; only a deposit with none at all is absent.
            if jar.cookies.is_empty() {
                return Err(FetchError::NoSession(format!(
                    "no trae.cn session {} ({})",
                    crate::cookie_vault::DEPOSIT_PHRASE,
                    jar.session_absence_detail()
                )));
            }
            let reading = exchange(&jar.header(), self.transport.as_ref()).await?;
            let mut entry = ProviderUsage::healthy(PROVIDER_NAME, None, source, Usage::default());
            entry.spend = Some(vec![reading.pool]);
            entry.account_info = reading.plan.map(|plan| AccountInfo {
                plan_type: Some(plan),
                ..AccountInfo::default()
            });
            Ok(entry)
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_source::{
        CredentialSource, VaultCapability, VaultCredential, VaultGetError,
    };
    use crate::vault_handles::VaultHandleLoader;
    use std::collections::HashMap;
    use std::sync::Mutex;

    // Fixtures from CodexBar v0.81.0 (ee890b5f392e70d6e20162d3e95cf9f1be265b60),
    // Tests/CodexBarTests/TraeCNPluginTests.swift:12-19 and 237: the session
    // cookie, the entitlement body and the token response its fake transport
    // answers with.
    const COOKIE: &str = "sessionid=fixture-session";
    const FIXTURE: &str = r#"{"is_credits_billing":true,"usage_summary":{"total_amount":10000,"consumed_amount":2000},
    "user_entitlement_pack_list":[
      {"group_type":0,"status":1,"display_desc":"Fixture Pro","expire_time":2000000000,"next_billing_time":1900000000},
      {"group_type":4,"status":1,"expire_time":1850000000}]}"#;
    const TOKEN_REPLY: &str = r#"{"Result":{"Token":"fixture-jwt"}}"#;
    /// What upstream's fake transport answers on any non-200, so a test can
    /// prove no error message echoes the body.
    const PRIVATE_BODY: &str = "private-response";

    /// Upstream's fake transport (TraeCNPluginTests.swift:206-240): a cookie
    /// starting `rejected=` gets 401 on any call, otherwise the token call and
    /// the entitlement call answer with the configured statuses. Every call is
    /// recorded so a test can read where each header went.
    struct FakeTransport {
        token_status: u16,
        usage_status: u16,
        usage_body: String,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeTransport {
        fn new(usage_body: &str) -> Self {
            Self::with_statuses(usage_body, 200, 200)
        }

        fn with_statuses(usage_body: &str, token_status: u16, usage_status: u16) -> Self {
            Self {
                token_status,
                usage_status,
                usage_body: usage_body.to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    fn header<'a>(call: &'a Call, name: &str) -> Option<&'a str> {
        call.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn post(&self, call: &Call) -> Result<Reply, FetchError> {
            self.calls.lock().unwrap().push(call.clone());
            let is_token = call.url == TOKEN_URL;
            let status = if header(call, "Cookie").is_some_and(|c| c.starts_with("rejected=")) {
                401
            } else if is_token {
                self.token_status
            } else {
                self.usage_status
            };
            let body = match (status, is_token) {
                (200, true) => TOKEN_REPLY.to_string(),
                (200, false) => self.usage_body.clone(),
                _ => PRIVATE_BODY.to_string(),
            };
            Ok(Reply {
                status,
                body: body.into_bytes(),
            })
        }
    }

    async fn fetch(transport: &FakeTransport) -> Result<Reading, FetchError> {
        exchange(COOKIE, transport).await
    }

    fn credit_amount(minor: i64) -> Amount {
        Amount {
            minor,
            exponent: 0,
            unit: "credit".to_string(),
        }
    }

    #[tokio::test]
    async fn credit_summary_publishes_used_total_and_remaining_without_dates() {
        // TraeCNPluginTests `credit summary does not invent reset or renewal
        // dates`: 2,000 used of 10,000 leaves 8,000, and the pack expiry and
        // billing timestamps become no reset, renewal or expiry.
        let transport = FakeTransport::new(FIXTURE);
        let reading = fetch(&transport).await.unwrap();
        let pool = &reading.pool;
        assert_eq!(pool.total, Some(credit_amount(10_000)));
        assert_eq!(pool.remaining, Some(credit_amount(8_000)));
        assert_eq!(pool.basis, PoolBasis::Derived);
        assert_eq!(pool.funding, PoolFunding::Unknown);
        assert_eq!(
            (&pool.resets_at, &pool.expires_at, pool.unlimited),
            (&None, &None, None)
        );
        assert_eq!(reading.plan.as_deref(), Some("Fixture Pro"));
        assert_eq!(transport.calls().len(), 2);
    }

    #[tokio::test]
    async fn zero_credits_stay_visible_without_a_fabricated_quota() {
        // TraeCNPluginTests `zero credits remain visible without a fabricated
        // quota`: an empty allowance is a real 0 of 0, not a missing pool.
        let body =
            r#"{"is_credits_billing":true,"usage_summary":{"total_amount":0,"consumed_amount":0}}"#;
        let reading = fetch(&FakeTransport::new(body)).await.unwrap();
        assert_eq!(reading.pool.total, Some(credit_amount(0)));
        assert_eq!(reading.pool.remaining, Some(credit_amount(0)));
        assert_eq!(reading.pool.resets_at, None);
        assert_eq!(reading.plan, None);
    }

    #[tokio::test]
    async fn overspent_and_fractional_credits_keep_one_exponent_and_never_go_negative() {
        let body = r#"{"is_credits_billing":true,"usage_summary":{"total_amount":100,"consumed_amount":120.5}}"#;
        let reading = fetch(&FakeTransport::new(body)).await.unwrap();
        let total = reading.pool.total.unwrap();
        let remaining = reading.pool.remaining.unwrap();
        assert_eq!((total.minor, total.exponent), (1_000, 1));
        assert_eq!((remaining.minor, remaining.exponent), (0, 1));
    }

    #[tokio::test]
    async fn invalid_credits_dollar_billing_and_bad_tokens_report_decode_failed() {
        // The first five bodies are TraeCNPluginTests `invalid credits and
        // dollar billing do not become zero usage`; the rest cover the other
        // shapes upstream's plugin refuses as a format change.
        for body in [
            "null",
            r#"{"is_credits_billing":true,"usage_summary":{"total_amount":100}}"#,
            r#"{"is_credits_billing":true,"usage_summary":{"total_amount":100,"consumed_amount":-1}}"#,
            r#"{"is_credits_billing":true,"usage_summary":{"total_amount":"100","consumed_amount":1}}"#,
            r#"{"is_credits_billing":false,"usage_summary":{"total_amount":100,"consumed_amount":1}}"#,
            "not JSON",
            r#"{"usage_summary":{"total_amount":100,"consumed_amount":1}}"#,
            r#"{"is_credits_billing":true}"#,
            r#"{"is_credits_billing":true,"usage_summary":null}"#,
        ] {
            let error = fetch(&FakeTransport::new(body)).await.unwrap_err();
            assert_eq!(error.error_class(), "decode_failed", "{body}");
        }
        for token_reply in [
            "not JSON",
            r#"{"Result":{}}"#,
            r#"{"Result":{"Token":"   "}}"#,
            r#"{"Result":{"Token":7}}"#,
        ] {
            let error = normalize_token_only(token_reply).unwrap_err();
            assert_eq!(error.error_class(), "decode_failed", "{token_reply}");
        }
    }

    fn normalize_token_only(body: &str) -> Result<String, FetchError> {
        read_token(accept(&Reply {
            status: 200,
            body: body.as_bytes().to_vec(),
        })?)
    }

    #[tokio::test]
    async fn a_signed_out_session_is_credential_rejected_on_either_call() {
        // A rejected token exchange stops before the entitlement call, so the
        // dead session is never sent a second time.
        let transport = FakeTransport::new(FIXTURE);
        let error = exchange("rejected=fixture", &transport).await.unwrap_err();
        assert_eq!(error.error_class(), "credential_rejected");
        assert_eq!(transport.calls().len(), 1);
        // 401, 403 and any redirect on the entitlement call, as upstream's
        // `rejected` predicate reads them.
        for status in [401, 403, 301, 302, 303, 307] {
            let transport = FakeTransport::with_statuses(FIXTURE, 200, status);
            let error = fetch(&transport).await.unwrap_err();
            assert_eq!(error.error_class(), "credential_rejected", "HTTP {status}");
            assert!(matches!(
                crate::refresh::classify(&error),
                crate::refresh::FetchClass::NonTransient
            ));
        }
        // The in-band signed-out code on an HTTP 200.
        let error = fetch(&FakeTransport::new(r#"{"code":1001}"#))
            .await
            .unwrap_err();
        assert_eq!(error.error_class(), "credential_rejected");
    }

    #[tokio::test]
    async fn api_failures_stay_transient_and_distinct_from_session_rejection() {
        // TraeCNPluginTests `API failures retain classification and never echo
        // bodies or tokens`: a rate limit, an outage or any other status is
        // not a signed-out session, on either call, and no message carries
        // the response body or the exchanged token.
        for (token_status, usage_status) in [
            (200, 429),
            (200, 503),
            (200, 500),
            (200, 408),
            (200, 404),
            (200, 400),
            (200, 201),
            (429, 200),
            (503, 200),
        ] {
            let transport = FakeTransport::with_statuses(FIXTURE, token_status, usage_status);
            let error = fetch(&transport).await.unwrap_err();
            let case = format!("token HTTP {token_status}, entitlement HTTP {usage_status}");
            assert_eq!(error.error_class(), "upstream_failed", "{case}");
            assert!(
                matches!(
                    crate::refresh::classify(&error),
                    crate::refresh::FetchClass::Transient
                ),
                "{case}"
            );
            let message = error.to_string();
            assert!(!message.contains(PRIVATE_BODY), "{case}: {message}");
            assert!(!message.contains("fixture-jwt"), "{case}: {message}");
        }
    }

    #[test]
    fn an_empty_success_body_is_transient_not_a_format_change() {
        let error = accept(&Reply {
            status: 200,
            body: Vec::new(),
        })
        .unwrap_err();
        assert_eq!(error.error_class(), "upstream_failed");
    }

    #[tokio::test]
    async fn the_session_and_token_reach_only_the_api_origin() {
        // Upstream's transport expects host `api.trae.cn` on every request
        // (TraeCNPluginTests.swift:210), the cookie on both calls and the token
        // only on the second. The session is captured from the parent domain
        // `trae.cn`, and `www.trae.cn` is named in Origin and Referer, but
        // neither ever receives a request.
        let transport = FakeTransport::new(FIXTURE);
        fetch(&transport).await.unwrap();
        let calls = transport.calls();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            let url = reqwest::Url::parse(call.url).unwrap();
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some("api.trae.cn"), "{}", call.url);
            assert_eq!(header(call, "Cookie"), Some(COOKIE));
            assert_eq!(header(call, "Origin"), Some("https://www.trae.cn"));
            assert_eq!(header(call, "Referer"), Some("https://www.trae.cn/"));
        }
        let (token, usage) = (&calls[0], &calls[1]);
        assert_eq!(
            reqwest::Url::parse(token.url).unwrap().path(),
            "/cloudide/api/v3/common/GetUserToken"
        );
        assert_eq!(header(token, "Authorization"), None);
        assert_eq!(token.body, "{}");
        assert_eq!(
            reqwest::Url::parse(usage.url).unwrap().path(),
            "/trae/api/v2/pay/user_current_entitlement_list"
        );
        assert_eq!(
            header(usage, "Authorization"),
            Some("Cloud-IDE-JWT fixture-jwt")
        );
        assert_eq!(
            serde_json::from_str::<Value>(usage.body).unwrap(),
            serde_json::json!({"require_usage": true, "full_data": true})
        );
    }

    /// Serves each deposit's payload by credential id, so a test deposits the
    /// header a capture would have produced.
    struct DepositSource {
        payloads: HashMap<String, String>,
    }

    #[async_trait]
    impl CredentialSource for DepositSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            Err(VaultGetError::FailClosed)
        }

        async fn get_scoped(
            &self,
            credential_id: &str,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            let payload = self
                .payloads
                .get(credential_id)
                .ok_or(VaultGetError::NotFound)?;
            Ok(VaultCredential {
                payload: payload.clone().into_bytes(),
                expires_at_ms: None,
                record_version: 1,
                account_id: None,
                project_id: None,
                email: None,
                org_name: None,
            })
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            _provider_status: u16,
            _record_version: u64,
        ) {
        }
    }

    fn provider_with(deposits: &[(&str, &str)], transport: Arc<FakeTransport>) -> TraeCnProvider {
        let loader = Arc::new(VaultHandleLoader::default());
        let rows: Vec<(&str, &str)> = deposits.iter().map(|(id, _)| (*id, "cookie")).collect();
        loader.install_rows_for_test(&rows);
        let source = DepositSource {
            payloads: deposits
                .iter()
                .map(|(id, payload)| (id.to_string(), payload.to_string()))
                .collect(),
        };
        TraeCnProvider {
            vault: crate::cookie_vault::CookieVault::new(
                Some(Arc::new(source)),
                loader,
                COOKIE_FAMILY,
            ),
            transport,
        }
    }

    #[tokio::test]
    async fn parent_domain_and_hand_supplied_deposits_both_fetch_the_credit_pool() {
        // Upstream's two broker cases (TraeCNPluginTests.swift:21-67): a
        // `.trae.cn` parent-domain `sessionid` imported automatically, and the
        // same header pasted by hand from an `api.trae.cn` request. Both reach
        // insula as a deposited request header; the first as the bare
        // deposit, the second under an account suffix.
        for deposit in ["cookie:trae.cn", "cookie:trae.cn:manual"] {
            let transport = Arc::new(FakeTransport::new(FIXTURE));
            let provider = provider_with(&[(deposit, COOKIE)], Arc::clone(&transport));
            let handles = provider.handles().unwrap();
            assert_eq!(handles.len(), 1);
            assert_eq!(handles[0].vault_credential_id(), Some(deposit));
            let attempt = provider.fetch_handle(&handles[0]).await;
            assert!(attempt.usage.is_ok(), "{deposit}: {:?}", attempt.usage);
            assert_eq!(attempt.source.as_deref(), Some("vault"));
            let pools = attempt.pools.expect("the credit pool is published");
            assert_eq!(pools.len(), 1);
            assert_eq!(pools[0].remaining, Some(credit_amount(8_000)));
            assert_eq!(
                attempt
                    .account_info
                    .and_then(|info| info.plan_type)
                    .as_deref(),
                Some("Fixture Pro")
            );
            let calls = transport.calls();
            assert_eq!(calls.len(), 2);
            assert!(calls
                .iter()
                .all(|call| header(call, "Cookie") == Some(COOKIE)));
        }
    }

    #[tokio::test]
    async fn no_deposit_is_cleanly_absent_and_an_empty_header_is_no_session() {
        let transport = Arc::new(FakeTransport::new(FIXTURE));
        let provider = provider_with(&[("cookie:ampcode.com", COOKIE)], Arc::clone(&transport));
        assert!(provider.handles().unwrap().is_empty());

        let provider = provider_with(
            &[("cookie:trae.cn", "not a cookie")],
            Arc::clone(&transport),
        );
        let handle = provider.handles().unwrap().remove(0);
        let attempt = provider.fetch_handle(&handle).await;
        assert_eq!(
            attempt.usage.unwrap_err().error_class(),
            "credential_absent"
        );
        assert!(transport.calls().is_empty(), "no request without a cookie");
    }
}
