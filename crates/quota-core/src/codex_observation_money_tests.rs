use super::*;
use crate::call_observations::ObservationWriter;
use crate::codex_resets::RedemptionJournal;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

struct Source(Arc<AtomicUsize>);
#[async_trait]
impl CredentialSource for Source {
    async fn get_scoped(
        &self,
        _: &str,
        _: u64,
    ) -> Result<VaultCredential, crate::credential_source::VaultGetError> {
        Ok(VaultCredential {
            payload: br#"{"access_token":"synthetic-token","account_id":"synthetic-account"}"#
                .to_vec(),
            expires_at_ms: None,
            record_version: 1,
            account_id: Some("synthetic-account".into()),
            project_id: None,
            email: None,
            org_name: None,
        })
    }
    async fn report_auth_failure_scoped(&self, _: &str, _: u16, _: u64) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Transport {
    gets: AtomicUsize,
    posts: AtomicUsize,
}
#[async_trait]
impl ResetTransport for Transport {
    async fn fetch_credits(&self, _: &ResetRequest) -> Result<CreditsHttpResponse, FetchError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        let expiry = (Utc::now() + chrono::Duration::days(20)).to_rfc3339();
        Ok(CreditsHttpResponse { body: serde_json::to_vec(&serde_json::json!({"credits":[{"id":"synthetic-credit","status":"available","expires_at":expiry}],"available_count":1})).unwrap(), date_header: None })
    }
    async fn consume(&self, _: &ResetRequest, _: &str) -> Result<Vec<u8>, FetchError> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        Err(FetchError::Upstream("unexpected consume POST".into()))
    }
}

#[tokio::test]
async fn reported_codex_wall_never_reaches_armed_coordinator_or_consume() {
    let scratch = crate::tests::ResetTempDir::with_prefix("usage-observe", "money");
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&polls);
    let server = tokio::spawn(async move {
        for percent in [41, 42] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 8192, "request headers exceed fixture bound");
            }
            let body = format!(
                r#"{{"rate_limit":{{"limit_reached":false,"primary_window":{{"used_percent":{percent},"limit_window_seconds":604800,"reset_at":1793774164}}}}}}"#
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            seen.fetch_add(1, Ordering::SeqCst);
        }
    });
    std::fs::write(
        scratch.path().join("config.toml"),
        format!("chatgpt_base_url = \"http://{address}/backend-api\"\n"),
    )
    .unwrap();
    let coordinator = Arc::new(
        ResetCoordinator::new(RedemptionJournal::new(
            scratch.path().join("redemptions.json"),
        ))
        .unwrap(),
    );
    let transport = Arc::new(Transport {
        gets: AtomicUsize::new(0),
        posts: AtomicUsize::new(0),
    });
    let reports = Arc::new(AtomicUsize::new(0));
    let loader = VaultHandleLoader::default();
    let credential = "chatgpt:openai:synthetic";
    loader.install_rows_for_test(&[(credential, "oauth")]);
    let provider = CodexProvider::new_for_test(
        CodexConfig { auto_use_resets: 1 },
        Some(Arc::new(Source(Arc::clone(&reports)))),
        transport.clone(),
        Arc::clone(&coordinator),
        loader,
        scratch.path().to_path_buf(),
    );
    let mut registry = crate::Registry::new(vec![Box::new(provider)]);
    registry.live_config = Some(crate::config::LiveQuotaConfig::from(CodexConfig {
        auto_use_resets: 1,
    }));
    registry.refresh_tick(&CancellationToken::new()).await;
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert_eq!(
        coordinator
            .sibling_with_headroom("another-account", Instant::now())
            .as_deref(),
        Some("synthetic-account")
    );
    let before = registry.usage_snapshot(None).await.to_envelope();
    let mut half = super::call_window_tests::captured_frame();
    half["rate_limits"]["primary"]["used_percent"] = serde_json::json!(50);
    let result = registry.observe_calls(ObservationWriter::Broca, vec![serde_json::json!({
        "provider":"codex","credentialId":credential,"observedAtMs":Utc::now().timestamp_millis()+1000,
        "frame":half})]);
    assert_eq!(result.accepted, 1);
    let halfway = registry.usage_snapshot(None).await.to_envelope();
    assert_eq!(halfway["result"][0]["usage"]["primary"]["usedPercent"], 0.0);
    assert_eq!(
        halfway["result"][0]["usage"]["primary"]["rawUsedPercent"],
        50.0
    );
    let result = registry.observe_calls(ObservationWriter::Broca, vec![serde_json::json!({
        "provider":"codex","credentialId":credential,"observedAtMs":Utc::now().timestamp_millis()+2000,
        "frame":super::call_window_tests::captured_frame()})]);
    assert_eq!(result.accepted, 1, "{result:?}");
    let after = registry.usage_snapshot(None).await.to_envelope();
    assert_eq!(after["result"][0]["usage"]["primary"]["usedPercent"], 100.0);
    for field in [
        "errorClass",
        "account",
        "accountInfo",
        "savedResets",
        "source",
    ] {
        assert_eq!(before["result"][0][field], after["result"][0][field]);
    }
    assert_eq!(before["completeProviders"], after["completeProviders"]);
    assert_eq!(
        coordinator
            .sibling_with_headroom("another-account", Instant::now())
            .as_deref(),
        Some("synthetic-account")
    );
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
    {
        let mut store = registry.store.lock().unwrap();
        let key =
            crate::store::SlotKey::new("codex", CredentialHandle::scoped(credential, "oauth"));
        let mut slot = store.get(&key).unwrap().clone();
        slot.next_due_at = crate::scheduler_now();
        store.publish_if_current(&key, slot.incarnation, slot.attempt_sequence, slot);
    }
    registry.refresh_tick(&CancellationToken::new()).await;
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    server.await.unwrap();
    assert_eq!(transport.gets.load(Ordering::SeqCst), 2);
    assert_eq!(
        transport.posts.load(Ordering::SeqCst),
        0,
        "a report must never spend a saved reset"
    );
    assert_eq!(
        coordinator
            .sibling_with_headroom("another-account", Instant::now())
            .as_deref(),
        Some("synthetic-account")
    );
    assert_eq!(reports.load(Ordering::SeqCst), 0);
}
