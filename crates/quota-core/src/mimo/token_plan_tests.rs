use super::*;
use crate::loopback::serve_once;
use crate::vercel::testkit::{credential, MockSource};
use crate::Registry;
use tokio_util::sync::CancellationToken;

// A Lite plan with non-round period boundaries and billions of credits, from
// CodexBar v0.85.0, TestsPlugin/MiMoTokenPlanPluginTests.swift:9–15.
const CONTRACT: &str = r#"{"object":"token_plan","plan_code":"lite",
 "period":{"start_at":"2026-11-11 21:05:27","end_at":"2026-12-11 23:59:59"},
 "quota":{"total_credits":4100000000,"used_credits":1400000000,"remaining_credits":2700000000},
 "auto_renew":true,"claw_enabled":false}"#;
const CHINA: &str = "apikey:xiaomi-token-plan-cn";
const SINGAPORE: &str = "apikey:xiaomi-token-plan-sgp:fixture";
const COOKIE: &str = "cookie:xiaomimimo.com:fixture";

fn adapter(rows: &[(&str, &str)], source: Arc<dyn CredentialSource>) -> MimoProvider {
    let loader = Arc::new(VaultHandleLoader::default());
    loader.install_rows_for_test(rows);
    MimoProvider::new_with_handle_loader(Some(source), loader)
}

async fn published(provider: MimoProvider) -> ProviderUsage {
    let registry = Registry::new(vec![Box::new(provider)]);
    registry.refresh_tick(&CancellationToken::new()).await;
    let mut entries = registry.usage_snapshot(Some("mimo")).await.entries;
    assert_eq!(entries.len(), 1);
    let entry = entries.pop().unwrap();
    assert_eq!(entry.provider, "mimo");
    assert_eq!(entry.account, None);
    entry
}

#[tokio::test]
async fn selected_regional_key_owns_the_row_and_uses_only_its_endpoint() {
    // The quota GET uses a bearer and never carries a cookie in either region
    // (CodexBar v0.85.0, TestsPlugin/MiMoTokenPlanPluginTests.swift:70–88).
    for (id, slug, endpoint) in [
        (
            CHINA,
            "xiaomi-token-plan-cn",
            "https://token-plan-cn.xiaomimimo.com/v1/platform/token_plan",
        ),
        (
            SINGAPORE,
            "xiaomi-token-plan-sgp",
            "https://token-plan-sgp.xiaomimimo.com/v1/platform/token_plan",
        ),
    ] {
        let mut key = credential("tp-fixture", 17);
        key.account_id = Some("vault-owner".to_string());
        let (source, reports) = MockSource::serving(&[(id, key)]);
        let mut provider = adapter(&[(COOKIE, "cookie"), (id, "apikey")], source);
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::scoped(id, "apikey")]
        );
        let (base, request) = serve_once(200, CONTRACT.as_bytes().to_vec()).await;
        provider
            .token_plan_test_urls
            .push((endpoint, format!("{base}/v1/platform/token_plan")));

        let entry = published(provider).await;
        assert_eq!(entry.api_provider.as_deref(), Some(slug));
        assert_eq!(entry.source.as_deref(), Some("vault"));
        assert!(entry.account_info.is_none());
        let window = entry
            .usage
            .unwrap_or_else(|| panic!("{:?}", entry.error))
            .primary
            .unwrap();
        assert_eq!(window.used_count, Some(1_400_000_000.0));
        assert_eq!(window.total_count, Some(4_100_000_000.0));
        assert!((window.used_percent - 140.0 / 4.1).abs() < 0.000001);
        assert_eq!(window.resets_at.as_deref(), Some("2026-12-11T23:59:59Z"));
        assert_eq!(window.window_minutes, None);
        assert_eq!(window.window_kind, None);
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/platform/token_plan http/1.1\r\n"));
        assert!(request
            .lines()
            .any(|line| line == "authorization: bearer tp-fixture"));
        assert!(!request.lines().any(|line| line.starts_with("cookie:")));
        assert!(reports.lock().unwrap().is_empty());
    }
}

struct UnreadableVault;

#[async_trait]
impl CredentialSource for UnreadableVault {
    async fn get_scoped(
        &self,
        id: &str,
        _min_ttl_ms: u64,
    ) -> Result<crate::credential_source::VaultCredential, VaultGetError> {
        assert_eq!(id, CHINA, "only the selected key may be read");
        Err(VaultGetError::Transient)
    }
}

#[tokio::test]
async fn vault_read_failure_does_not_fall_back_to_cookie() {
    let provider = adapter(
        &[(CHINA, "apikey"), (COOKIE, "cookie")],
        Arc::new(UnreadableVault),
    );
    let entry = published(provider).await;
    assert_eq!(entry.api_provider.as_deref(), Some("xiaomi-token-plan-cn"));
    assert_eq!(entry.error_class.as_deref(), Some("upstream_failed"));
    assert!(entry.usage.is_none());
}

#[tokio::test]
async fn competing_keys_use_no_deposit_and_cookie_alone_keeps_its_lane() {
    for competitor in [SINGAPORE, "apikey:xiaomi-token-plan-cn:other"] {
        let source: Arc<dyn CredentialSource> = Arc::new(UnreadableVault);
        let provider = adapter(
            &[
                (CHINA, "apikey"),
                (competitor, "apikey"),
                (COOKIE, "cookie"),
            ],
            source,
        );
        let entry = published(provider).await;
        assert_eq!(entry.api_provider, None);
        assert_eq!(entry.error_class.as_deref(), Some("credential_unusable"));
        assert!(entry
            .error
            .as_deref()
            .unwrap()
            .contains("conflicting Token Plan keys"));
    }

    let (source, reports) = MockSource::serving(&[(COOKIE, credential("tracking=fixture", 17))]);
    let provider = adapter(
        &[
            (COOKIE, "cookie"),
            ("apikey:xiaomi-token-plan-cn-other", "apikey"),
        ],
        source,
    );
    assert_eq!(
        provider.handles().unwrap(),
        vec![CredentialHandle::scoped(COOKIE, "cookie")]
    );
    let entry = published(provider).await;
    assert_eq!(entry.api_provider, None);
    assert!(entry
        .error
        .as_deref()
        .unwrap()
        .contains("missing required mimo session cookies"));
    assert!(reports.lock().unwrap().is_empty());
}

#[tokio::test]
async fn refusals_fail_the_key_lane_and_only_401_reports_the_served_record() {
    // CodexBar v0.85.0, TestsPlugin/MiMoTokenPlanPluginTests.swift:55–67,
    // exercises authentication, permission, throttling, and server failures.
    for (status, class) in [
        (401, "credential_rejected"),
        (403, "upstream_failed"),
        (429, "upstream_failed"),
        (500, "upstream_failed"),
        (302, "upstream_failed"),
    ] {
        let (base, request) = serve_once(status, b"tp-fixture".to_vec()).await;
        let (source, reports) = MockSource::serving(&[(SINGAPORE, credential("tp-fixture", 17))]);
        let mut provider = adapter(&[(SINGAPORE, "apikey"), (COOKIE, "cookie")], source);
        provider
            .token_plan_test_urls
            .push((SINGAPORE_TOKEN_PLAN_URL, base));
        let entry = published(provider).await;
        request.await.unwrap();
        assert!(entry.usage.is_none());
        assert_eq!(entry.api_provider.as_deref(), Some("xiaomi-token-plan-sgp"));
        assert_eq!(entry.error_class.as_deref(), Some(class));
        assert!(!entry.error.as_deref().unwrap().contains("tp-fixture"));
        tokio::task::yield_now().await;
        let expected = if status == 401 {
            vec![(SINGAPORE.to_string(), 401, 17)]
        } else {
            vec![]
        };
        assert_eq!(*reports.lock().unwrap(), expected);
    }
}

#[tokio::test]
async fn a_token_plan_key_does_not_follow_redirects() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let (other_origin, request) = serve_once(200, CONTRACT.as_bytes().to_vec()).await;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let redirect = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let incoming = crate::loopback::read_request(&mut stream).await;
        assert!(incoming
            .to_ascii_lowercase()
            .contains("authorization: bearer tp-fixture"));
        stream.write_all(format!(
            "HTTP/1.1 302 Found\r\nLocation: {other_origin}/v1/platform/token_plan\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ).as_bytes()).await.unwrap();
    });
    let (source, _) = MockSource::serving(&[(CHINA, credential("tp-fixture", 17))]);
    let mut provider = adapter(&[(CHINA, "apikey")], source);
    provider
        .token_plan_test_urls
        .push((CHINA_TOKEN_PLAN_URL, base));
    let entry = published(provider).await;
    redirect.await.unwrap();
    assert_eq!(entry.error_class.as_deref(), Some("upstream_failed"));
    assert!(entry.error.as_deref().unwrap().contains("302"));
    assert!(
        !request.is_finished(),
        "the redirect target must receive no request"
    );
    request.abort();
}

#[test]
fn exhausted_and_over_cap_credit_counts_are_preserved() {
    // CodexBar v0.85.0, TestsPlugin/MiMoTokenPlanPluginTests.swift:47–54,
    // sets used equal to the limit and reports no remaining credits.
    for used in ["4100000000", "9007199254740991"] {
        let body = CONTRACT
            .replace("1400000000", used)
            .replace("2700000000", "0");
        let window = normalize_token_plan(body.as_bytes())
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(window.used_percent, 100.0);
        assert_eq!(window.raw_used_percent, None);
        assert_eq!(window.used_count, Some(used.parse::<f64>().unwrap()));
        assert_eq!(window.total_count, Some(4_100_000_000.0));
    }
}

#[test]
fn only_exact_reported_utc_boundaries_define_the_period() {
    for (fields, expected_reset, expected_minutes) in [
        (
            r#""start_at":"2026-12-11 23:58:59","end_at":"2026-12-11 23:59:59""#,
            Some("2026-12-11T23:59:59Z"),
            Some(1),
        ),
        (
            r#""start_at":"2026-12-11 23:58:58","end_at":"2026-12-11 23:59:59""#,
            Some("2026-12-11T23:59:59Z"),
            None,
        ),
        (
            r#""end_at":"2026-12-11 23:59:59""#,
            Some("2026-12-11T23:59:59Z"),
            None,
        ),
        (
            r#""start_at":"2026-11-11 21:05:27","end_at":null"#,
            None,
            None,
        ),
        ("", None, None),
    ] {
        let body = CONTRACT.replace(
            r#""start_at":"2026-11-11 21:05:27","end_at":"2026-12-11 23:59:59""#,
            fields,
        );
        let window = normalize_token_plan(body.as_bytes())
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(window.resets_at.as_deref(), expected_reset);
        assert_eq!(window.window_minutes, expected_minutes);
        assert_eq!(window.window_kind, None);
    }
}

#[test]
fn missing_or_invalid_counts_and_dates_do_not_become_usage() {
    // CodexBar v0.85.0, TestsPlugin/MiMoTokenPlanPluginTests.swift:34–44,
    // rejects bad JSON, inconsistent counters, and impossible dates.
    for body in ["{}", "[]", "not-json"] {
        assert!(matches!(
            normalize_token_plan(body.as_bytes()),
            Err(FetchError::Decode(_))
        ));
    }
    for (old, new) in [
        (r#""total_credits":4100000000,"#, ""),
        ("4100000000", "-1"),
        ("1400000000", "true"),
        ("1400000000", "1400000000.25"),
        ("2700000000", "0"),
        ("2026-12-11", "2026-02-30"),
        ("2026-12-11", "2026-11-10"),
        ("23:59:59", "23:59:60"),
        ("23:59:59", "23:59:59Z"),
        ("token_plan", "other"),
    ] {
        let body = CONTRACT.replace(old, new);
        assert!(
            matches!(
                normalize_token_plan(body.as_bytes()),
                Err(FetchError::Decode(_))
            ),
            "{body}"
        );
    }
    let too_large = CONTRACT
        .replace("1400000000", "9007199254740992")
        .replace("2700000000", "0");
    assert!(matches!(
        normalize_token_plan(too_large.as_bytes()),
        Err(FetchError::Decode(_))
    ));
}

#[test]
fn zero_allowance_reports_no_quota_without_inventing_a_window() {
    let body = CONTRACT
        .replace("4100000000", "0")
        .replace("2700000000", "0");
    assert!(matches!(
        normalize_token_plan(body.as_bytes()),
        Err(FetchError::NoQuotaReported(_))
    ));
}
