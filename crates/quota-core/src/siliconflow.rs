//! SiliconFlow regional balances: API keys in both regions, and a deposited
//! China console session when no China key is configured.
//!
//! The models.dev provider name fixes the origin and currency. A key is never
//! probed against the other region. Only the aggregate balance is published:
//! purchased and bonus balances overlap it and are not additional money.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::sync::Arc;

use crate::cookie_vault::{CookieVault, SOURCE};
use crate::credential_source::CredentialSource;
use crate::http::{Header, HttpResponse, JsonRequest};
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{
    AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
};
use crate::vault_handles::{handle_id_names_family, VaultHandleLoader, SILICONFLOW_CN_COOKIE};

const INTERNATIONAL_URL: &str = "https://api.siliconflow.com/v1/user/info";
const CHINA_URL: &str = "https://api.siliconflow.cn/v1/user/info";
const BILLING_PAGE_URL: &str = "https://cloud.siliconflow.cn/me/expensebill";
const WALLET_URL: &str = "https://cloud.siliconflow.cn/walletd-server/api/v1/subject/profile/peek";
// Recognize this Location without ever requesting it.
const LOGIN_REDIRECT: &str = "https://account.siliconflow.cn/login";
const SUBJECT_HEADER: &str = "x-subject-id";

#[derive(Clone, Copy)]
pub(crate) enum Region {
    International,
    China,
}

impl Region {
    fn name(self) -> &'static str {
        match self {
            Self::International => "siliconflow",
            Self::China => "siliconflow-cn",
        }
    }

    fn url(self) -> &'static str {
        match self {
            Self::International => INTERNATIONAL_URL,
            Self::China => CHINA_URL,
        }
    }

    fn currency(self) -> &'static str {
        match self {
            Self::International => "USD",
            Self::China => "CNY",
        }
    }
}

#[derive(Deserialize)]
struct Envelope {
    code: i64,
    status: Option<Box<RawValue>>,
    data: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Balance {
    total_balance: Box<RawValue>,
    charge_balance: Option<Box<RawValue>>,
    balance: Option<Box<RawValue>>,
    status: Option<String>,
    id: Option<String>,
}

fn decode() -> FetchError {
    // Never quote a payload or serde error: either may echo a credential.
    FetchError::Decode("siliconflow: unrecognized balance response".to_string())
}

fn retired() -> FetchError {
    // Unlike CodexBar's generic unavailable classification, keep the reason
    // explicit: a retired query proves neither a dead key nor an empty account.
    FetchError::Upstream("siliconflow: balance query unavailable; check the console".to_string())
}

fn check_status(status: u16) -> Result<(), FetchError> {
    match status {
        200 => Ok(()),
        401 => Err(FetchError::ProviderStatus(
            401,
            "siliconflow: API key rejected".to_string(),
        )),
        // CodexBar treats 403 as expired authentication. Permission to read a
        // balance can be refused without killing the key, so never invalidate it.
        403 => Err(FetchError::Upstream(
            "siliconflow: balance query forbidden".to_string(),
        )),
        // CodexBar calls 410/20092 unavailable, not an empty balance. Preserve
        // that distinction instead of publishing no_quota_reported or zero.
        410 => Err(retired()),
        _ => Err(FetchError::Upstream(format!(
            "siliconflow: balance query returned HTTP {status}"
        ))),
    }
}

fn amount(raw: &RawValue, currency: &str) -> Result<Amount, FetchError> {
    let text = raw.get();
    let quoted = text.starts_with('"');
    let text = if quoted {
        serde_json::from_str::<String>(text).map_err(|_| decode())?
    } else {
        text.to_string()
    };
    // Strings follow upstream's strict decimal grammar; JSON numbers may also
    // use exponent notation. RawValue avoids rounding either through f64.
    let (mantissa, shift) = if !quoted {
        match text.split_once(['e', 'E']) {
            Some((mantissa, shift)) => (mantissa, shift.parse::<i32>().map_err(|_| decode())?),
            None => (text.as_str(), 0),
        }
    } else {
        (text.as_str(), 0)
    };
    if !(-40..=40).contains(&shift) {
        return Err(decode());
    }
    let (sign, digits) = mantissa
        .strip_prefix('-')
        .map_or(("", mantissa), |s| ("-", s));
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
        || (digits.contains('.') && fraction.is_empty())
    {
        return Err(decode());
    }
    let combined = format!("{whole}{fraction}");
    let point = i64::try_from(whole.len()).map_err(|_| decode())? + i64::from(shift);
    let length = i64::try_from(combined.len()).map_err(|_| decode())?;
    let decimal = if point <= 0 {
        format!("0.{}{combined}", "0".repeat((-point) as usize))
    } else if point >= length {
        format!("{combined}{}", "0".repeat((point - length) as usize))
    } else {
        let (head, tail) = combined.split_at(point as usize);
        format!("{head}.{tail}")
    };
    parse_amount(&format!("{sign}{decimal}"), currency).ok_or_else(decode)
}

fn normalize(body: &[u8], region: Region) -> Result<(Option<String>, Pool), FetchError> {
    let envelope: Envelope = serde_json::from_slice(body).map_err(|_| decode())?;
    match envelope.code {
        // As with HTTP 410, this is a query refusal, not zero credit.
        20092 => return Err(retired()),
        30014 => {
            return Err(FetchError::Unauthorized(
                "siliconflow: API key rejected".to_string(),
            ))
        }
        20000
            if envelope
                .status
                .as_ref()
                .is_some_and(|status| status.get() == "true") => {}
        _ => return Err(decode()),
    }
    let data: Balance =
        serde_json::from_str(envelope.data.ok_or_else(decode)?.get()).map_err(|_| decode())?;
    let remaining = amount(&data.total_balance, region.currency())?;
    // Validate the optional components, but never emit them beside their sum.
    for component in [data.charge_balance, data.balance].into_iter().flatten() {
        amount(&component, region.currency())?;
    }
    if data.status.is_some_and(|status| {
        status.is_empty()
            || status.len() > 32
            || !status
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    }) {
        return Err(decode());
    }
    let account = data.id.filter(|id| !id.trim().is_empty());
    Ok((account, balance_pool(remaining)))
}

fn balance_pool(remaining: Amount) -> Pool {
    Pool {
        id: "balance".to_string(),
        label: "Account balance".to_string(),
        funding: PoolFunding::Unknown,
        remaining: Some(remaining),
        total: None,
        basis: PoolBasis::Reported,
        spendable: None,
        resets_at: None,
        expires_at: None,
        unlimited: None,
    }
}

fn normalize_wallet(body: &[u8]) -> Result<Pool, FetchError> {
    let wallet: serde_json::Value = serde_json::from_slice(body).map_err(|_| decode())?;
    if wallet["code"].as_i64() != Some(20000) {
        return Err(decode());
    }
    let value = wallet["data"]["financialInfo"]["balance"]
        .as_str()
        .ok_or_else(decode)?;
    let digits = value.strip_prefix('-').unwrap_or(value);
    if !(1..=30).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(decode());
    }
    let picoyuan = value.parse::<i128>().map_err(|_| decode())?;
    // The wire supports at most nine places. Floor the excess digits so we
    // never overstate credit or understate debt. Exact multiples are unchanged.
    let mut minor = picoyuan.div_euclid(1000);
    let mut exponent = 9;
    while exponent > 0 && minor % 10 == 0 {
        minor /= 10;
        exponent -= 1;
    }
    Ok(balance_pool(Amount {
        minor: i64::try_from(minor).map_err(|_| decode())?,
        exponent,
        unit: Region::China.currency().to_string(),
    }))
}

fn is_login_redirect(location: &str) -> bool {
    location
        .get(..LOGIN_REDIRECT.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(LOGIN_REDIRECT))
        && matches!(
            location.as_bytes().get(LOGIN_REDIRECT.len()),
            None | Some(b'/' | b'?' | b'#')
        )
}

fn check_console_status(response: &HttpResponse) -> Result<(), FetchError> {
    let status = response.status;
    if status == 401
        || ((300..400).contains(&status)
            && response.header("location").is_some_and(is_login_redirect))
    {
        return Err(FetchError::Unauthorized(
            "siliconflow: sign in to the China console and re-capture the cookie".into(),
        ));
    }
    match status {
        200 => Ok(()),
        403 => Err(FetchError::Upstream(
            "siliconflow: console balance forbidden; check the account's billing access".into(),
        )),
        _ => Err(FetchError::Upstream(format!(
            "siliconflow: console balance query returned HTTP {status}"
        ))),
    }
}

fn console_subject(page: &HttpResponse) -> Result<&str, FetchError> {
    page.header(SUBJECT_HEADER)
        .filter(|subject| {
            (1..=128).contains(&subject.len())
                && subject
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
        .ok_or_else(decode)
}

pub struct SiliconFlowProvider {
    region: Region,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    console: CookieVault,
    #[cfg(test)]
    url: Option<String>,
    #[cfg(test)]
    console_urls: Option<(String, String)>,
}

impl SiliconFlowProvider {
    pub(crate) fn new_with_handle_loader(
        region: Region,
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            region,
            http: crate::http::provider_client(),
            console: CookieVault::new(
                credential_source.clone(),
                handle_loader.clone(),
                SILICONFLOW_CN_COOKIE,
            ),
            credential_source,
            handle_loader,
            #[cfg(test)]
            url: None,
            #[cfg(test)]
            console_urls: None,
        }
    }

    async fn read(&self, key: &str) -> Result<(Option<String>, Pool), FetchError> {
        let url = self.region.url();
        #[cfg(test)]
        let url = self.url.as_deref().unwrap_or(url);
        let response = JsonRequest::get(url)
            .bearer(key)
            .send_raw(&self.http)
            .await?;
        check_status(response.status)?;
        normalize(response.body_for_parsing()?, self.region)
    }

    async fn read_console(&self, handle: &CredentialHandle) -> Result<Pool, FetchError> {
        let (jar, _) = self.console.jar_for(handle).await?;
        let cookie = jar.header();
        if cookie.is_empty() {
            return Err(FetchError::CredentialUnusable(
                "siliconflow: console deposit contains no usable cookies".into(),
            ));
        }
        let (page_url, wallet_url) = (BILLING_PAGE_URL, WALLET_URL);
        #[cfg(test)]
        let (page_url, wallet_url) = self
            .console_urls
            .as_ref()
            .map_or((page_url, wallet_url), |(page, wallet)| {
                (page.as_str(), wallet.as_str())
            });

        // A captured Cookie header lacks path metadata. Never resend it to a
        // redirect destination, even another route on the console host.
        let http = crate::http::provider_client_without_redirects()?;
        let page = JsonRequest::get_accepting(page_url, "text/html")
            .header(Header::new("Cookie", &cookie))
            .send_raw_selecting_body(&http, |_| false)
            .await?;
        check_console_status(&page)?;
        let subject = console_subject(&page)?;
        let wallet = JsonRequest::get(wallet_url)
            .header(Header::new("Referer", page_url))
            .header(Header::new("Cookie", &cookie))
            .header(Header::new(SUBJECT_HEADER, subject))
            .send_raw_selecting_body(&http, |status| status == 200)
            .await?;
        check_console_status(&wallet)?;
        normalize_wallet(wallet.body_for_parsing()?)
    }
}

#[async_trait]
impl UsageProvider for SiliconFlowProvider {
    fn name(&self) -> &str {
        self.region.name()
    }

    fn is_cookie_based(&self) -> bool {
        matches!(self.region, Region::China)
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            return match self.region {
                Region::International => self.handle_loader.siliconflow_handles(),
                Region::China => self.handle_loader.siliconflow_cn_handles(),
            };
        }
        Ok(Vec::new())
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if self.is_cookie_based()
            && handle
                .vault_credential_id()
                .is_some_and(|id| handle_id_names_family(id, SILICONFLOW_CN_COOKIE))
        {
            return match self.read_console(handle).await {
                Ok(pool) => {
                    // The subject scopes the wallet request; it is not a stated account identity.
                    let mut attempt = FetchAttempt::success(
                        Some(AccountObservation::new(None, None)),
                        SOURCE,
                        Usage::default(),
                    );
                    attempt.pools = Some(vec![pool]);
                    attempt
                }
                Err(error) => FetchAttempt::failure(None, Some(SOURCE.to_string()), error),
            };
        }
        if !handle
            .vault_credential_id()
            .is_some_and(|id| handle_id_names_family(id, &format!("apikey:{}", self.name())))
        {
            return FetchAttempt::failure(
                None,
                Some("vault".to_string()),
                FetchError::CredentialUnusable(
                    "siliconflow: credential belongs to a different region or family".to_string(),
                ),
            );
        }
        let Some(source) = self.credential_source.as_ref() else {
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
        let result = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(key) if !key.trim().is_empty() => self.read(key.trim()).await,
            Ok(_) => Err(FetchError::CredentialUnusable(
                "siliconflow: empty API key deposit".to_string(),
            )),
            Err(error) => Err(error),
        };
        match result {
            Ok((account, pool)) => {
                let mut attempt = FetchAttempt::success(
                    Some(AccountObservation::new(
                        account,
                        Some(credential.record_version),
                    )),
                    "vault",
                    Usage::default(),
                );
                attempt.pools = Some(vec![pool]);
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
    use crate::vercel::testkit::{credential, routes, Log, MockSource};

    // CodexBar v0.84.0, TestsPlugin/SiliconFlowPluginTests.swift, contract and
    // international-overdue fixtures. Identities here are upstream's test data.
    const CONTRACT: &str = r#"{"code":20000,"message":"OK","status":true,"data":{"id":"user","name":"hidden","email":"hidden@example.com","balance":"0.88","status":"normal","chargeBalance":"88.00","totalBalance":"88.88"}}"#;
    const OVERDUE: &str = r#"{"code":20000,"status":true,"data":{"balance":"0.00","chargeBalance":"0.00","totalBalance":"-1.50","status":"overdue"}}"#;
    const KEY: &str = "sk-fixture";

    // CodexBar v0.85.0, TestsPlugin/SiliconFlowWebPluginTests.swift:12–47,
    // 109–165, and 310–345: wallet shape, session headers, and refusal fixtures.
    const CONSOLE_ID: &str = "cookie:cloud.siliconflow.cn:fixture";
    const CONSOLE_COOKIE: &str = "__SF_auth.session-token=sf-session-1";
    const PAGE_PATH: &str = "/me/expensebill";
    const WALLET_PATH: &str = "/walletd-server/api/v1/subject/profile/peek";
    const SUBJECT: &str = "X-Subject-Id: subject-fixture\r\n";
    const CHUNKED: &str = "Transfer-Encoding: chunked\r\n";

    fn wallet(balance: &str) -> String {
        format!(
            r#"{{"code":20000,"data":{{"financialInfo":{{"balance":{balance},"available":"1","recharged":"400000000000000","used":"387500000000000"}}}}}}"#
        )
    }

    async fn console_fixture(
        answer: impl Fn(&str) -> (u16, String, String) + Send + 'static,
        key: Option<&str>,
        cookie: &str,
    ) -> (
        SiliconFlowProvider,
        Arc<std::sync::Mutex<Vec<String>>>,
        crate::vercel::testkit::Reports,
    ) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let requests = log.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let request = crate::loopback::read_request(&mut stream).await;
                let path = request.split_whitespace().nth(1).unwrap();
                let (status, headers, body) = answer(path);
                requests.lock().unwrap().push(request);
                let length = if headers.contains(CHUNKED) {
                    String::new()
                } else {
                    format!("Content-Length: {}\r\n", body.len())
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\n{headers}{length}Connection: close\r\n\r\n{body}"
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let mut rows = vec![(CONSOLE_ID, "cookie")];
        let mut payloads = vec![(CONSOLE_ID, credential(cookie, 17))];
        if let Some(key) = key {
            rows.push(("apikey:siliconflow-cn:fixture", "apikey"));
            payloads.push(("apikey:siliconflow-cn:fixture", credential(key, 18)));
        }
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&rows);
        let (source, reports) = MockSource::serving(&payloads);
        let mut provider =
            SiliconFlowProvider::new_with_handle_loader(Region::China, Some(source), loader);
        provider.url = Some(format!("{base}/v1/user/info"));
        provider.console_urls =
            Some((format!("{base}{PAGE_PATH}"), format!("{base}{WALLET_PATH}")));
        (provider, log, reports)
    }

    #[tokio::test]
    async fn console_binds_wallet_to_the_billing_page_subject_and_same_cookie() {
        let (provider, requests, reports) = console_fixture(
            |path| match path {
                // The billing response body is irrelevant even if it ends mid-stream.
                PAGE_PATH => (200, format!("{SUBJECT}{CHUNKED}"), String::new()),
                WALLET_PATH => (200, String::new(), wallet(r#""12500000000000""#)),
                _ => panic!("unexpected route {path}"),
            },
            None,
            CONSOLE_COOKIE,
        )
        .await;
        let handles = provider.handles().unwrap();
        assert_eq!(handles, [CredentialHandle::scoped(CONSOLE_ID, "cookie")]);
        let attempt = provider.fetch_handle(&handles[0]).await;
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(attempt.observed.unwrap().account_id, None);
        assert_eq!(attempt.usage.unwrap(), Usage::default());
        let pools = attempt.pools.unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id, "balance");
        assert_eq!(pools[0].remaining, Some(expected(125, 1, "CNY")));
        assert_eq!(pools[0].funding, PoolFunding::Unknown);
        assert_eq!(pools[0].basis, PoolBasis::Reported);
        assert!(
            pools[0].total.is_none()
                && pools[0].spendable.is_none()
                && pools[0].resets_at.is_none()
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for (raw, path) in requests.iter().zip([PAGE_PATH, WALLET_PATH]) {
            assert!(raw.starts_with(&format!("GET {path} ")));
            let raw = raw.to_ascii_lowercase();
            assert!(raw.contains(&format!(
                "\r\ncookie: {}\r\n",
                CONSOLE_COOKIE.to_ascii_lowercase()
            )));
            assert!(!raw.contains("\r\nauthorization:"));
            assert_eq!(raw.matches("\r\naccept:").count(), 1);
        }
        let page = requests[0].to_ascii_lowercase();
        assert!(!page.contains("x-subject-id:"));
        assert!(page.contains("\r\naccept: text/html\r\n"));
        let wallet = requests[1].to_ascii_lowercase();
        assert!(wallet.contains("\r\nx-subject-id: subject-fixture\r\n"));
        assert!(wallet.contains("\r\naccept: application/json\r\n"));
        assert!(wallet.contains(&format!(
            "\r\nreferer: {}\r\n",
            provider.console_urls.unwrap().0
        )));
        assert!(reports.lock().unwrap().is_empty());
    }

    #[test]
    fn console_picoyuan_preserves_exact_values_and_floors_credit_and_debt() {
        for (value, minor, exponent) in [
            ("0", 0, 0),
            ("-1500000000000", -15, 1),
            ("98765432100000000000000", 98_765_432_100, 0),
            ("1234567891987", 1_234_567_891, 9),
            ("-1234567891987", -1_234_567_892, 9),
            ("-1", -1, 9),
            ("9007199254740991000", 9_007_199_254_740_991, 9),
        ] {
            assert_eq!(
                normalize_wallet(wallet(&format!("\"{value}\"")).as_bytes())
                    .unwrap()
                    .remaining,
                Some(expected(minor, exponent, "CNY")),
                "{value}",
            );
        }
    }

    #[test]
    fn console_invalid_or_missing_wallet_amounts_do_not_publish_a_balance() {
        for body in [
            "private-response-sentinel".into(),
            r#"{"code":20000,"data":{"financialInfo":{}}}"#.into(),
            r#"{"code":1,"data":{"financialInfo":{"balance":"1"}}}"#.into(),
            r#"[20000,[["1"]]]"#.into(),
            wallet(r#""""#),
            wallet(r#""1.5""#),
            wallet("12"),
            wallet("null"),
            wallet(r#""+1""#),
            wallet(r#""9223372036854775808000000000000""#),
            wallet(r#""922337203685477580812345678901""#),
        ] {
            let error = normalize_wallet(body.as_bytes()).unwrap_err();
            assert_eq!(error.error_class(), "decode_failed", "{body}");
            assert!(!error.to_string().contains("private-response-sentinel"));
        }
    }

    #[tokio::test]
    async fn console_missing_or_malformed_subject_stops_before_wallet() {
        for subject in [None, Some(""), Some("bad subject"), Some("bad/subject")] {
            let (provider, requests, _) = console_fixture(
                move |_| {
                    (
                        200,
                        subject.map_or_else(String::new, |s| format!("X-Subject-Id: {s}\r\n")),
                        String::new(),
                    )
                },
                None,
                CONSOLE_COOKIE,
            )
            .await;
            let attempt = provider.fetch_handle(&provider.handles().unwrap()[0]).await;
            assert_eq!(attempt.usage.unwrap_err().error_class(), "decode_failed");
            assert!(attempt.pools.is_none());
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn console_login_and_permission_refusals_stay_distinct_and_redacted() {
        for (status, location, class) in [
            (
                307,
                "https://account.siliconflow.cn/login?state=state-sentinel",
                "credential_rejected",
            ),
            (401, "", "credential_rejected"),
            (
                403,
                "https://account.siliconflow.cn/login",
                "upstream_failed",
            ),
            (429, "", "upstream_failed"),
            (503, "", "upstream_failed"),
            (
                307,
                "https://account.siliconflow.cn.evil/login",
                "upstream_failed",
            ),
            (
                307,
                "https://account.siliconflow.cn/login-elsewhere",
                "upstream_failed",
            ),
        ] {
            for refuse_path in [PAGE_PATH, WALLET_PATH] {
                let (provider, requests, reports) = console_fixture(
                    move |path| {
                        if path == refuse_path {
                            (
                                status,
                                format!("Location: {location}\r\n{CHUNKED}"),
                                // An incomplete refusal body must not override its status.
                                format!("800\r\nprivate-response-sentinel: {CONSOLE_COOKIE}"),
                            )
                        } else {
                            (200, SUBJECT.into(), String::new())
                        }
                    },
                    None,
                    CONSOLE_COOKIE,
                )
                .await;
                let error = provider
                    .fetch_handle(&provider.handles().unwrap()[0])
                    .await
                    .usage
                    .unwrap_err();
                assert_eq!(error.error_class(), class, "HTTP {status} {location}");
                for secret in [
                    "state-sentinel",
                    "private-response-sentinel",
                    CONSOLE_COOKIE,
                ] {
                    assert!(!error.to_string().contains(secret));
                }
                assert_eq!(
                    requests.lock().unwrap().len(),
                    if refuse_path == PAGE_PATH { 1 } else { 2 }
                );
                tokio::task::yield_now().await;
                assert!(reports.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn console_never_follows_redirects_even_on_the_same_host() {
        let (provider, requests, _) = console_fixture(
            |path| match path {
                PAGE_PATH => (302, "Location: /other\r\n".into(), String::new()),
                _ => (200, SUBJECT.into(), wallet(r#""1""#)),
            },
            None,
            CONSOLE_COOKIE,
        )
        .await;
        let error = provider
            .fetch_handle(&provider.handles().unwrap()[0])
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "upstream_failed");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn console_never_takes_over_for_a_retired_china_key() {
        let (provider, requests, reports) = console_fixture(
            |_| (410, String::new(), String::new()),
            Some(KEY),
            CONSOLE_COOKIE,
        )
        .await;
        let handles = provider.handles().unwrap();
        assert_eq!(
            handles,
            [CredentialHandle::scoped(
                "apikey:siliconflow-cn:fixture",
                "apikey"
            )]
        );
        let error = provider.fetch_handle(&handles[0]).await.usage.unwrap_err();
        assert_eq!(error.error_class(), "upstream_failed");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /v1/user/info "));
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("\r\nauthorization: bearer sk-fixture\r\n"));
        assert!(!requests[0].to_ascii_lowercase().contains("\r\ncookie:"));
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn console_unusable_deposit_and_international_cookie_handle_send_nothing() {
        let (provider, requests, _) =
            console_fixture(|_| (200, SUBJECT.into(), String::new()), None, " ").await;
        let handle = provider.handles().unwrap().remove(0);
        let error = provider.fetch_handle(&handle).await.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable");
        assert!(requests.lock().unwrap().is_empty());

        let (mut provider, requests, _) = console_fixture(
            |_| (200, SUBJECT.into(), wallet(r#""1""#)),
            None,
            CONSOLE_COOKIE,
        )
        .await;
        provider.region = Region::International;
        assert!(!provider.is_cookie_based());
        assert!(provider.handles().unwrap().is_empty());
        let error = provider.fetch_handle(&handle).await.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable");
        assert!(requests.lock().unwrap().is_empty());
    }

    fn expected(minor: i64, exponent: u8, currency: &str) -> Amount {
        Amount {
            minor,
            exponent,
            unit: currency.to_string(),
        }
    }

    fn body(value: &str) -> String {
        format!(r#"{{"code":20000,"status":true,"data":{{"totalBalance":{value}}}}}"#)
    }

    async fn provider(
        region: Region,
        status: u16,
        body: &str,
        key: &str,
    ) -> (
        SiliconFlowProvider,
        CredentialHandle,
        Log,
        crate::vercel::testkit::Reports,
    ) {
        let payload = body.as_bytes().to_vec();
        let (base, log) = routes(move |path, _| {
            assert_eq!(path, "/v1/user/info");
            (status, payload.clone())
        })
        .await;
        let id = format!("apikey:{}:fixture", region.name());
        let mut deposited = credential(key, 17);
        deposited.account_id = Some("vault-owner".to_string());
        let (source, reports) = MockSource::serving(&[(&id, deposited)]);
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[(&id, "apikey")]);
        let mut provider =
            SiliconFlowProvider::new_with_handle_loader(region, Some(source), loader);
        provider.url = Some(format!("{base}/v1/user/info"));
        (
            provider,
            CredentialHandle::scoped(id, "apikey"),
            log,
            reports,
        )
    }

    #[tokio::test]
    async fn regional_vault_reads_publish_only_the_reported_total_and_verified_identity() {
        for (region, currency) in [(Region::International, "USD"), (Region::China, "CNY")] {
            let (provider, handle, log, reports) = provider(region, 200, CONTRACT, KEY).await;
            assert_eq!(provider.handles().unwrap(), vec![handle.clone()]);
            let attempt = provider.fetch_handle(&handle).await;
            assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
            assert_eq!(
                attempt.observed.as_ref().unwrap().account_id.as_deref(),
                Some("user")
            );
            assert_eq!(attempt.source.as_deref(), Some("vault"));
            let usage = serde_json::to_value(attempt.usage.unwrap()).unwrap();
            assert_eq!(usage, serde_json::json!({}));
            let pools = attempt.pools.unwrap();
            assert_eq!(
                pools.len(),
                1,
                "the components must not double-count the total"
            );
            let pool = &pools[0];
            assert_eq!(pool.id, "balance");
            assert_eq!(pool.remaining, Some(expected(8888, 2, currency)));
            assert_eq!(pool.funding, PoolFunding::Unknown);
            assert_eq!(pool.basis, PoolBasis::Reported);
            assert!(
                pool.total.is_none()
                    && pool.spendable.is_none()
                    && pool.resets_at.is_none()
                    && pool.expires_at.is_none()
                    && pool.unlimited.is_none()
            );
            let requests = log.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].authorization.as_deref(),
                Some("Bearer sk-fixture")
            );
            assert!(requests[0].query.is_empty());
            assert!(reports.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn negative_overdue_balances_and_unstated_identity_are_preserved() {
        let (account, pool) = normalize(OVERDUE.as_bytes(), Region::International).unwrap();
        assert_eq!(account, None);
        assert_eq!(pool.remaining, Some(expected(-150, 2, "USD")));
        assert_eq!(
            pool.spendable, None,
            "account status is not a per-pool enable flag"
        );
    }

    #[tokio::test]
    async fn credential_metadata_never_supplies_an_unstated_account() {
        let (provider, handle, _, _) = provider(Region::International, 200, OVERDUE, KEY).await;
        let attempt = provider.fetch_handle(&handle).await;
        assert!(attempt.usage.is_ok());
        assert_eq!(attempt.observed.unwrap().account_id, None);
    }

    #[test]
    fn decimal_strings_and_json_numbers_keep_exact_precision_without_floats() {
        for (raw, minor, exponent) in [
            (r#""0""#, 0, 0),
            ("0", 0, 0),
            (r#""90071992547409.12345""#, 9_007_199_254_740_912_345, 5),
            ("90071992547409.12345", 9_007_199_254_740_912_345, 5),
            ("0.000000001", 1, 9),
            ("1e-9", 1, 9),
            ("1.25e2", 125, 0),
        ] {
            let (_, pool) = normalize(body(raw).as_bytes(), Region::China).unwrap();
            assert_eq!(
                pool.remaining,
                Some(expected(minor, exponent, "CNY")),
                "{raw}"
            );
        }
    }

    #[test]
    fn malformed_or_missing_balances_never_turn_into_zero_or_absence() {
        // Malformed fixtures extend CodexBar v0.84.0's plugin tests with the
        // precision and optional-field limits of the integer money wire type.
        for raw in [
            "null",
            "true",
            "{}",
            "[]",
            r#""1e2""#,
            r#""88.88.1""#,
            r#"" 1 ""#,
            r#""+1""#,
            r#"".5""#,
            r#""1.""#,
            r#""NaN""#,
            "0.0000000001",
            "1e100",
            r#""9223372036854775808""#,
        ] {
            assert_eq!(
                normalize(body(raw).as_bytes(), Region::China)
                    .unwrap_err()
                    .error_class(),
                "decode_failed",
                "{raw}"
            );
        }
        for raw in [
            "not-json",
            "[]",
            "{}",
            r#"{"code":20000,"status":true,"data":{}}"#,
            r#"{"code":20000,"status":false,"data":{"totalBalance":"1"}}"#,
            r#"{"code":1,"status":true,"data":{"totalBalance":"1"}}"#,
            r#"{"code":20000,"status":true,"data":{"totalBalance":"1","balance":true}}"#,
            r#"{"code":20000,"status":true,"data":{"totalBalance":"1","status":"bad status"}}"#,
        ] {
            assert_eq!(
                normalize(raw.as_bytes(), Region::China)
                    .unwrap_err()
                    .error_class(),
                "decode_failed",
                "{raw}"
            );
        }
        let (_, pool) = normalize(br#"{"code":20000,"status":true,"data":{"totalBalance":"1","balance":null,"chargeBalance":null}}"#, Region::China).unwrap();
        assert_eq!(pool.remaining, Some(expected(1, 0, "CNY")));
    }

    #[test]
    fn region_names_fix_official_endpoints_and_currency() {
        let loader = Arc::new(VaultHandleLoader::default());
        for (region, name, url, currency) in [
            (
                Region::International,
                "siliconflow",
                "https://api.siliconflow.com/v1/user/info",
                "USD",
            ),
            (
                Region::China,
                "siliconflow-cn",
                "https://api.siliconflow.cn/v1/user/info",
                "CNY",
            ),
        ] {
            let provider =
                SiliconFlowProvider::new_with_handle_loader(region, None, loader.clone());
            assert_eq!(provider.name(), name);
            assert_eq!(region.url(), url);
            assert_eq!(region.currency(), currency);
            assert!(
                provider.handles().unwrap().is_empty(),
                "vault-only provider has no local fallback"
            );
        }
    }

    #[tokio::test]
    async fn refusals_keep_their_classes_and_only_http_401_reports_a_dead_key() {
        for (status, payload, class, reported) in [
            (401, "private-response-marker", "credential_rejected", true),
            (403, "private-response-marker", "upstream_failed", false),
            (410, "private-response-marker", "upstream_failed", false),
            (429, "private-response-marker", "upstream_failed", false),
            (500, "private-response-marker", "upstream_failed", false),
            (404, "private-response-marker", "upstream_failed", false),
            (
                200,
                r#"{"code":20092,"status":false,"message":"private-response-marker"}"#,
                "upstream_failed",
                false,
            ),
            (
                200,
                r#"{"code":30014,"status":false,"message":"private-response-marker"}"#,
                "credential_rejected",
                false,
            ),
            (
                200,
                r#"{"code":30014,"status":"error","message":"private-response-marker"}"#,
                "credential_rejected",
                false,
            ),
            (200, "private-response-marker", "decode_failed", false),
            (200, "", "upstream_failed", false),
        ] {
            let secret_body = payload.replace(
                "private-response-marker",
                &format!("{}{}", KEY, "x".repeat(20_000)),
            );
            let (provider, handle, log, reports) =
                provider(Region::International, status, &secret_body, KEY).await;
            let error = provider.fetch_handle(&handle).await.usage.unwrap_err();
            assert_eq!(error.error_class(), class, "HTTP {status}, {payload}");
            assert!(!error.to_string().contains(KEY));
            assert!(error.to_string().len() < 200);
            assert_eq!(
                log.lock().unwrap().len(),
                1,
                "no retry or cross-region fallback"
            );
            tokio::task::yield_now().await;
            let reports = reports.lock().unwrap();
            assert_eq!(reports.len(), usize::from(reported), "HTTP {status}");
            if reported {
                assert_eq!(
                    reports[0],
                    ("apikey:siliconflow:fixture".to_string(), 401, 17)
                );
            }
        }
    }

    #[tokio::test]
    async fn unusable_or_cross_region_deposits_send_nothing() {
        for key in ["", "  "] {
            let (provider, handle, log, _) =
                provider(Region::International, 200, CONTRACT, key).await;
            let error = provider.fetch_handle(&handle).await.usage.unwrap_err();
            assert_eq!(error.error_class(), "credential_unusable");
            assert!(log.lock().unwrap().is_empty());
        }
        for region in [Region::International, Region::China] {
            let (mut provider, _, log, _) = provider(region, 200, CONTRACT, KEY).await;
            let other = match region {
                Region::International => "siliconflow-cn",
                Region::China => "siliconflow",
            };
            let id = format!("apikey:{other}:fixture");
            provider.credential_source = Some(MockSource::serving(&[(&id, credential(KEY, 17))]).0);
            let error = provider
                .fetch_handle(&CredentialHandle::scoped(id, "apikey"))
                .await
                .usage
                .unwrap_err();
            assert_eq!(error.error_class(), "credential_unusable");
            assert!(log.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn an_unavailable_vault_is_not_an_absent_credential() {
        struct Unavailable;
        #[async_trait]
        impl CredentialSource for Unavailable {
            async fn get_scoped(
                &self,
                _: &str,
                _: u64,
            ) -> Result<
                crate::credential_source::VaultCredential,
                crate::credential_source::VaultGetError,
            > {
                Err(crate::credential_source::VaultGetError::Transient)
            }
        }
        let (mut provider, _, log, _) = provider(Region::China, 200, CONTRACT, KEY).await;
        provider.handle_loader = Arc::new(VaultHandleLoader::default());
        provider.credential_source = Some(Arc::new(Unavailable));
        provider.handle_loader.await_first_answer();
        assert!(provider.handles().is_err());
        let missing = CredentialHandle::scoped("apikey:siliconflow-cn:missing", "apikey");
        let attempt = provider.fetch_handle(&missing).await;
        assert_eq!(attempt.usage.unwrap_err().error_class(), "upstream_failed");
        assert!(log.lock().unwrap().is_empty());
    }
}
