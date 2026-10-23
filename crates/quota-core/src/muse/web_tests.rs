use super::*;
use crate::config::{ConfigReloader, MuseConfig, QuotaConfig};
use crate::credential_source::{VaultCapability, VaultCredential, VaultGetError};
use crate::muse::MuseProvider;
use crate::provider::UsageProvider;
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

const TEAM: &str = "424242424242";
const ME: &str = r#"{"userId":"1","email":"Ada@Example.com","accountType":"META_ACCOUNT"}"#;
const TEAMS: &str = r#"{"teams":[{"team_id":424242424242,"team_name":"My Team"}]}"#;
const IDLE: &str = r#"{"is_subs_active":true,"user_email":"ada@example.com","subs_tier_name":"Muse Code Power Usage"}"#;
const DEVICE: &str = "dca:fixture-device";
const COOKIE: &str = "llama_dev_sess=fixture-session; tracking=not-needed";

// Shaped from CodexBar v0.79.0's MusePluginTests, not serialized Rust models.
fn quota() -> Value {
    serde_json::json!({"subscription_quota": {
        "tier_id": "1", "tier": "Muse Code Power Usage", "as_of": 1790341873,
        "window_weighted_limit": "20000000000", "window_duration_secs": 18000,
        "weekly_weighted_limit": "60000000000", "weekly_resets_at": 4102444800_i64,
        "window_weighted_used": "4000000000", "window_resets_at": 4102441200_i64,
        "weekly_weighted_used": "9043782620"
    }})
}

struct Source;
#[async_trait]
impl CredentialSource for Source {
    async fn get(&self, _: &VaultCapability, _: u64) -> Result<VaultCredential, VaultGetError> {
        panic!("all fixture deposits use scoped reads")
    }
    async fn get_scoped(&self, id: &str, _: u64) -> Result<VaultCredential, VaultGetError> {
        let payload = if id.starts_with("cookie:dev.meta.ai") {
            COOKIE
        } else {
            DEVICE
        };
        Ok(VaultCredential {
            payload: payload.as_bytes().to_vec(),
            record_version: 7,
            expires_at_ms: None,
            account_id: None,
            email: None,
            org_name: None,
            project_id: None,
        })
    }
    async fn report_auth_failure(&self, _: &VaultCapability, _: u16, _: u64) {
        panic!("cookie failures must not reach the vault")
    }
    async fn report_auth_failure_scoped(&self, _: &str, _: u16, _: u64) {
        panic!("cookie failures must not reach the vault")
    }
}

struct Server {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(reply: impl Fn(&str) -> (u16, String, String) + Send + 'static) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let request = crate::loopback::read_request(&mut stream).await;
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let (status, body, headers) = reply(path);
                seen.lock().unwrap().push(request);
                let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            base,
            requests,
            task,
        }
    }
    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.split_whitespace().nth(1).unwrap().to_string())
            .collect()
    }
}

struct Fixture {
    provider: MuseProvider,
    mint: Server,
    web: Server,
    loader: Arc<VaultHandleLoader>,
}
impl Fixture {
    async fn new(team: Option<&str>, overrides: Vec<(&str, u16, String)>) -> Self {
        let mint = Server::new(|_| (200, IDLE.into(), String::new())).await;
        let overrides: HashMap<String, (u16, String)> = overrides
            .into_iter()
            .map(|(path, status, body)| (path.to_string(), (status, body)))
            .collect();
        let web = Server::new(move |path| {
            if let Some((status, body)) = overrides.get(path) { return (*status, body.clone(), String::new()); }
            let body = match path {
                "/api/auth/me" => ME.into(),
                "/api/portal/teams" => TEAMS.into(),
                p if p.ends_with("/members") => r#"{"members":[{"member_id":"9","user_id":"1001","name":"Ada","email":" ADA@Example.com ","role":"owner"}]}"#.into(),
                p if p.ends_with("/subscription-quota") => quota().to_string(),
                _ => return (404, "{}".into(), String::new()),
            };
            (200, body, String::new())
        }).await;
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[
            ("apikey:muse:ada", "apikey"),
            ("cookie:dev.meta.ai:ada", "cookie"),
        ]);
        let config = QuotaConfig {
            muse: MuseConfig {
                team_id: team.map(str::to_string),
            },
            ..QuotaConfig::default()
        };
        let mut provider = MuseProvider::new_with_handle_loader(
            config.into(),
            Some(Arc::new(Source)),
            loader.clone(),
        );
        provider.url = format!("{}/muse-code/key", mint.base);
        provider.web.base = web.base.clone();
        Self {
            provider,
            mint,
            web,
            loader,
        }
    }
    async fn fetch(&self) -> crate::provider::FetchAttempt {
        let handles = self.provider.handles().unwrap();
        assert_eq!(
            handles.len(),
            1,
            "enrichment must not create another Muse row"
        );
        assert_eq!(handles[0].stable_id(), "apikey:muse:ada");
        self.provider.fetch_handle(&handles[0]).await
    }
    async fn withheld(&self) {
        let attempt = self.fetch().await;
        assert_eq!(attempt.usage.unwrap_err().error_class(), "upstream_failed");
        assert_eq!(
            attempt.observed.unwrap().account_id.as_deref(),
            Some("ada@example.com")
        );
    }
}

#[tokio::test]
async fn web_email_team_and_tier_match_publish_windows_with_mint_identity() {
    let f = Fixture::new(Some(TEAM), vec![]).await;
    let attempt = f.fetch().await;
    let usage = attempt.usage.unwrap();
    let primary = usage.primary.unwrap();
    assert_eq!(primary.used_percent, 20.0);
    assert_eq!(primary.window_minutes, Some(300));
    assert_eq!(primary.resets_at.as_deref(), Some("2099-12-31T23:00:00Z"));
    let weekly = usage.secondary.unwrap();
    assert!((weekly.used_percent - 15.07297103).abs() < 0.00001);
    assert_eq!(weekly.window_minutes, Some(10080));
    assert_eq!(weekly.resets_at.as_deref(), Some("2100-01-01T00:00:00Z"));
    assert_eq!(
        attempt.observed.unwrap().account_id.as_deref(),
        Some("ada@example.com")
    );
    assert_eq!(
        attempt.account_info.unwrap().plan_type.as_deref(),
        Some("Muse Code Power Usage")
    );
    assert_eq!(
        f.web.paths(),
        [
            "/api/auth/me",
            "/api/portal/teams",
            &format!("/api/portal/teams/{TEAM}/subscription-quota")
        ]
    );
}

#[tokio::test]
async fn blank_email_is_bound_by_the_session_users_membership() {
    for user in ["1001", "\"1001\""] {
        let f = Fixture::new(
            Some(TEAM),
            vec![(
                "/api/auth/me",
                200,
                format!(r#"{{"userId":{user},"email":" "}}"#),
            )],
        )
        .await;
        assert!(f.fetch().await.usage.is_ok());
        assert_eq!(
            f.web.paths(),
            [
                "/api/auth/me",
                "/api/portal/teams",
                &format!("/api/portal/teams/{TEAM}/members"),
                &format!("/api/portal/teams/{TEAM}/subscription-quota")
            ]
        );
    }
}

#[tokio::test]
async fn email_mismatch_withholds_web_reading() {
    let f = Fixture::new(
        Some(TEAM),
        vec![(
            "/api/auth/me",
            200,
            r#"{"userId":"1","email":"bob@example.com"}"#.into(),
        )],
    )
    .await;
    f.withheld().await;
    assert_eq!(f.web.paths(), ["/api/auth/me"]);
}

#[tokio::test]
async fn no_configured_team_lists_visible_teams_once_without_reading_quota() {
    let f = Fixture::new(None, vec![]).await;
    f.withheld().await;
    f.withheld().await;
    assert_eq!(
        f.web.paths(),
        [
            "/api/auth/me",
            "/api/portal/teams",
            "/api/auth/me",
            "/api/portal/teams"
        ]
    );
    let state = f.provider.web.state.lock().unwrap();
    assert_eq!(
        state.teams.get("ada@example.com"),
        Some(&vec![Team {
            id: TEAM.into(),
            name: "My Team".into()
        }])
    );
}

#[tokio::test]
async fn invisible_team_is_not_queried() {
    let f = Fixture::new(Some("123"), vec![]).await;
    f.withheld().await;
    assert_eq!(f.web.paths(), ["/api/auth/me", "/api/portal/teams"]);
}

#[tokio::test]
async fn tier_mismatch_withholds_web_reading() {
    let mut body = quota();
    body["subscription_quota"]["tier"] = "Muse Code Everyday Usage".into();
    let f = Fixture::new(
        Some(TEAM),
        vec![(
            &format!("/api/portal/teams/{TEAM}/subscription-quota"),
            200,
            body.to_string(),
        )],
    )
    .await;
    f.withheld().await;
}

#[tokio::test]
async fn members_403_ignores_session_without_expiring_or_reporting_cookie() {
    let f = Fixture::new(
        Some(TEAM),
        vec![
            (
                "/api/auth/me",
                200,
                r#"{"userId":"1001","email":""}"#.into(),
            ),
            (
                &format!("/api/portal/teams/{TEAM}/members"),
                403,
                r#"{"error":"Forbidden"}"#.into(),
            ),
        ],
    )
    .await;
    f.withheld().await;
    tokio::task::yield_now().await;
    assert!(f.provider.web.state.lock().unwrap().expired.is_empty());
    assert_eq!(f.web.paths().len(), 3);
}

#[tokio::test]
async fn a_401_expires_only_the_cookie_and_never_reports_to_vault() {
    let f = Fixture::new(Some(TEAM), vec![("/api/auth/me", 401, "{}".into())]).await;
    f.withheld().await;
    tokio::task::yield_now().await;
    assert!(f
        .provider
        .web
        .state
        .lock()
        .unwrap()
        .expired
        .contains(&("cookie:dev.meta.ai:ada".to_string(), 7)));
    assert_eq!(f.web.paths(), ["/api/auth/me"]);
}

#[tokio::test]
async fn login_redirect_expires_cookie_without_forwarding_it() {
    let mut f = Fixture::new(Some(TEAM), vec![]).await;
    let login = Server::new(|_| (200, "login".into(), String::new())).await;
    let location = format!("Location: {}/login\r\n", login.base);
    let redirect = Server::new(move |_| (302, String::new(), location.clone())).await;
    f.provider.web.base = redirect.base.clone();
    f.withheld().await;
    assert!(login.paths().is_empty());
    assert!(f
        .provider
        .web
        .state
        .lock()
        .unwrap()
        .expired
        .contains(&("cookie:dev.meta.ai:ada".to_string(), 7)));
}

#[tokio::test]
async fn web_never_receives_device_token() {
    let f = Fixture::new(Some(TEAM), vec![]).await;
    assert!(f.fetch().await.usage.is_ok());
    let requests = f.web.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    for request in requests.iter() {
        assert!(
            !request.to_lowercase().contains("authorization:"),
            "device bearer reached dev.meta.ai: {request}"
        );
        assert!(!request.contains(DEVICE));
        assert!(request.contains("llama_dev_sess=fixture-session"));
        assert!(!request.contains("tracking="));
    }
}

#[tokio::test]
async fn mint_never_receives_cookie() {
    let f = Fixture::new(Some(TEAM), vec![]).await;
    assert!(f.fetch().await.usage.is_ok());
    let requests = f.mint.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("Bearer dca:fixture-device"));
    assert!(!requests[0].to_lowercase().contains("cookie:"));
    assert!(!requests[0].contains("fixture-session"));
}

#[tokio::test]
async fn remembered_reading_wins_without_any_web_requests() {
    let mut f = Fixture::new(Some(TEAM), vec![]).await;
    let full = r#"{"is_subs_active":true,"user_email":"ada@example.com","subs_tier_name":"Muse Code Power Usage","subs_usage":{"window":{"used_percent":96,"window_duration_mins":300,"resets_at":4102441200},"weekly":{"used_percent":40,"resets_at":4102444800}}}"#;
    let mint = Server::new(move |_| (200, full.into(), String::new())).await;
    f.provider.url = format!("{}/muse-code/key", mint.base);
    assert!(f.fetch().await.usage.is_ok());
    f.provider.url = format!("{}/muse-code/key", f.mint.base);
    let usage = f.fetch().await.usage.unwrap();
    assert_eq!(
        f.web.paths().len(),
        0,
        "remembered quota must cost no dev.meta.ai requests"
    );
    assert_eq!(usage.primary.unwrap().used_percent, 0.0);
    assert_eq!(usage.secondary.unwrap().used_percent, 40.0);
}

/// A verified team reading is remembered, so the next idle answer is filled
/// from memory instead of calling dev.meta.ai again on every poll.
#[tokio::test]
async fn a_web_reading_is_remembered_so_the_next_idle_answer_costs_no_web_request() {
    let f = Fixture::new(Some(TEAM), vec![]).await;
    assert!(f.fetch().await.usage.is_ok());
    let after_first = f.web.paths().len();
    assert_eq!(after_first, 3, "the first idle answer reads the team quota");
    let usage = f.fetch().await.usage.unwrap();
    assert_eq!(
        f.web.paths().len(),
        after_first,
        "the second idle answer must come from memory"
    );
    assert!((usage.secondary.unwrap().used_percent - 15.07297103).abs() < 0.00001);
}

#[tokio::test]
async fn idle_or_expired_five_hour_window_has_no_usage_or_reset() {
    for expired in [false, true] {
        let mut body = quota();
        if expired {
            body["subscription_quota"]["window_resets_at"] = 1.into();
        } else {
            body["subscription_quota"]["window_weighted_used"] = "0".into();
            body["subscription_quota"]
                .as_object_mut()
                .unwrap()
                .remove("window_resets_at");
        }
        let f = Fixture::new(
            Some(TEAM),
            vec![(
                &format!("/api/portal/teams/{TEAM}/subscription-quota"),
                200,
                body.to_string(),
            )],
        )
        .await;
        let usage = f.fetch().await.usage.unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 0.0);
        assert_eq!(primary.resets_at, None);
        assert!(usage.secondary.is_some());
    }
}

#[tokio::test]
async fn malformed_reset_withholds_the_entire_reading() {
    for field in ["window_resets_at", "weekly_resets_at"] {
        for reset in [
            serde_json::json!("invalid"),
            serde_json::json!(1e30),
            serde_json::json!(-1),
            serde_json::json!(true),
            Value::Null,
        ] {
            let mut body = quota();
            body["subscription_quota"][field] = reset;
            let f = Fixture::new(
                Some(TEAM),
                vec![(
                    &format!("/api/portal/teams/{TEAM}/subscription-quota"),
                    200,
                    body.to_string(),
                )],
            )
            .await;
            f.withheld().await;
        }
    }
}

#[tokio::test]
async fn weekly_reset_in_the_past_withholds_the_entire_reading() {
    let mut body = quota();
    body["subscription_quota"]["weekly_resets_at"] = 1.into();
    let f = Fixture::new(
        Some(TEAM),
        vec![(
            &format!("/api/portal/teams/{TEAM}/subscription-quota"),
            200,
            body.to_string(),
        )],
    )
    .await;
    f.withheld().await;
}

#[tokio::test]
async fn zero_or_missing_limit_withholds_the_entire_reading() {
    for field in ["window_weighted_limit", "weekly_weighted_limit"] {
        for value in [Value::from("0"), Value::Null] {
            let mut body = quota();
            body["subscription_quota"][field] = value;
            let f = Fixture::new(
                Some(TEAM),
                vec![(
                    &format!("/api/portal/teams/{TEAM}/subscription-quota"),
                    200,
                    body.to_string(),
                )],
            )
            .await;
            f.withheld().await;
        }
    }
}

#[tokio::test]
async fn blank_email_requires_own_membership_not_another_members_email() {
    let f = Fixture::new(Some(TEAM), vec![
        ("/api/auth/me", 200, r#"{"userId":"1001","email":""}"#.into()),
        (&format!("/api/portal/teams/{TEAM}/members"), 200, r#"{"members":[{"user_id":"2002","email":"ada@example.com"},{"user_id":"1001","email":"bob@example.com"}]}"#.into()),
    ]).await;
    f.withheld().await;
    assert_eq!(f.web.paths().len(), 3);
}

#[tokio::test]
async fn no_cookie_deposit_makes_no_web_request() {
    let f = Fixture::new(Some(TEAM), vec![]).await;
    f.loader
        .install_rows_for_test(&[("apikey:muse:ada", "apikey")]);
    f.withheld().await;
    assert!(f.web.paths().is_empty());
}

#[tokio::test]
async fn team_selection_reloads_on_the_existing_provider() {
    let mut f = Fixture::new(None, vec![]).await;
    let live = LiveQuotaConfig::new(QuotaConfig::default());
    f.provider.web.config = live.clone();
    f.withheld().await;
    let dir = crate::tests::ResetTempDir::with_prefix("insula-muse", "team-reload");
    let path = dir.path().join("quota.json");
    std::fs::write(&path, format!(r#"{{"muse":{{"team_id":"{TEAM}"}}}}"#)).unwrap();
    let mut reloader = ConfigReloader::new(
        path,
        Box::new(|text| serde_json::from_str(text).map_err(|error| error.to_string())),
        live,
    );
    reloader.check();
    assert!(f.fetch().await.usage.is_ok());
    assert_eq!(
        f.web
            .paths()
            .iter()
            .filter(|path| path.ends_with("/subscription-quota"))
            .count(),
        1
    );
}

#[tokio::test]
async fn served_cookie_lifetime_records_first_expiry_only() {
    let mut f = Fixture::new(Some(TEAM), vec![]).await;
    assert!(f.fetch().await.usage.is_ok());
    assert_eq!(
        f.provider
            .web
            .state
            .lock()
            .unwrap()
            .lifetimes
            .rejection_reported("cookie:dev.meta.ai:ada"),
        Some(false)
    );
    let expired = Server::new(|_| (401, "{}".into(), String::new())).await;
    f.provider.web.base = expired.base.clone();
    // The first reading is remembered and would serve the next idle answers
    // without touching dev.meta.ai; forget it so each fetch reaches the session.
    f.provider.last_full.lock().unwrap().clear();
    f.withheld().await;
    f.withheld().await;
    assert_eq!(
        f.provider
            .web
            .state
            .lock()
            .unwrap()
            .lifetimes
            .rejection_reported("cookie:dev.meta.ai:ada"),
        Some(true)
    );
}

#[test]
fn reference_zero_usage_with_a_future_reset_keeps_that_reset() {
    let mut body = quota();
    body["subscription_quota"]["window_weighted_used"] = "0".into();
    let usage = parse_quota(&body["subscription_quota"], 1790341873.0).unwrap();
    assert_eq!(usage.primary.as_ref().unwrap().used_percent, 0.0);
    assert!(usage.primary.unwrap().resets_at.is_some());
}

#[tokio::test]
async fn team_log_names_ids_and_changes_only_when_the_visible_list_changes() {
    let f = Fixture::new(None, vec![]).await;
    let teams = vec![Team {
        id: TEAM.into(),
        name: "My Team".into(),
    }];
    let line = f
        .provider
        .web
        .team_listing("ada@example.com", &teams)
        .unwrap();
    assert!(line.contains("424242424242 (My Team)"));
    assert!(line.contains("muse.team_id"));
    assert!(f
        .provider
        .web
        .team_listing("ada@example.com", &teams)
        .is_none());
    assert!(f
        .provider
        .web
        .team_listing("ada@example.com", &[])
        .is_some());
}

#[tokio::test]
async fn multiple_enrichment_deposits_share_the_five_request_budget() {
    let f = Fixture::new(
        Some(TEAM),
        vec![(
            "/api/auth/me",
            200,
            r#"{"userId":"1001","email":""}"#.into(),
        )],
    )
    .await;
    f.loader.install_rows_for_test(&[
        ("apikey:muse:ada", "apikey"),
        ("cookie:dev.meta.ai:ada", "cookie"),
        ("cookie:dev.meta.ai:other", "cookie"),
    ]);
    assert!(f.loader.warning().is_none());
    assert!(f.fetch().await.usage.is_ok());
    assert_eq!(
        f.web.paths(),
        [
            "/api/auth/me",
            "/api/auth/me",
            "/api/portal/teams",
            &format!("/api/portal/teams/{TEAM}/members"),
            &format!("/api/portal/teams/{TEAM}/subscription-quota")
        ]
    );
}
