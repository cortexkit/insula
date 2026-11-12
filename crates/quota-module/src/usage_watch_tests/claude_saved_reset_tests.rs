//! `usage.use_saved_reset` for Claude, through the real module handler and the
//! real Claude provider, against a local stand-in for Anthropic.

use super::saved_reset_tests::{bind, bind_unscoped, call, callosum};
use super::*;
use quota_core::anthropic::AnthropicProvider;
use quota_core::claude_resets::mock::{self, MockAnthropic, MockSource, ACCOUNT};
use quota_core::codex_resets::SavedResetResult;
use quota_core::provider::{CredentialHandle, FetchAttempt, HandlesError, UsageProvider};
use std::sync::atomic::AtomicUsize;

/// The Claude provider serving exactly one vault lane, as a host with one
/// Claude deposit does.
struct ClaudeLane(AnthropicProvider);

#[async_trait::async_trait]
impl UsageProvider for ClaudeLane {
    fn name(&self) -> &str {
        "claude"
    }
    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![CredentialHandle::scoped(mock::CREDENTIAL_ID, "oauth")])
    }
    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        self.0.fetch_handle(handle).await
    }
    async fn use_saved_reset(
        &self,
        handle: &CredentialHandle,
        account_id: &str,
        request_id: &str,
        kind: Option<&str>,
    ) -> SavedResetResult {
        self.0
            .use_saved_reset(handle, account_id, request_id, kind)
            .await
    }
}

/// A scratch directory for one fixture's redemption journal, removed on drop.
struct JournalDir(std::path::PathBuf);

impl JournalDir {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ck-insula-claude-reset-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for JournalDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) struct ClaudeFixture {
    pub server: MockAnthropic,
    pub registry: Arc<Registry>,
    _journal: JournalDir,
}

pub(super) fn grants() -> Value {
    mock::eligible(
        vec![
            mock::grant(
                "grant-session",
                "2099-01-01T00:00:00Z",
                json!({"type": "session"}),
            ),
            mock::grant(
                "grant-weekly",
                "2099-02-01T00:00:00Z",
                json!({"clears": ["seven_day"]}),
            ),
            mock::grant(
                "grant-fable",
                "2099-03-01T00:00:00Z",
                json!({"gives_back_weekly": true, "resets_left": 2, "resets_total": 2}),
            ),
        ],
        Some("grant-session"),
    )
}

pub(super) async fn claude_fixture() -> ClaudeFixture {
    let journal = JournalDir::new();
    let server = MockAnthropic::start(mock::usage_body(grants())).await;
    let provider = AnthropicProvider::redemption_fixture(
        Arc::new(MockSource::default()),
        &server.base,
        journal.0.join("redemptions.json"),
    );
    let registry = Arc::new(Registry::new(vec![Box::new(ClaudeLane(provider))]));
    registry.refresh_tick(&CancellationToken::new()).await;
    ClaudeFixture {
        server,
        registry,
        _journal: journal,
    }
}

pub(super) const CLAUDE_REQUEST: &str = "phone-claude-0001";

pub(super) fn claude_params() -> Value {
    json!({"provider": "claude", "account": ACCOUNT, "request_id": CLAUDE_REQUEST,
           "kind": "session"})
}

fn body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test]
async fn a_phone_spends_a_claude_saved_reset_once_per_request_id() {
    let fixture = claude_fixture().await;
    let mut wire = Wire::with_registry(false, 65536, fixture.registry.clone()).await;
    bind(&mut wire, 1, callosum(), Some("fed-phone:synthetic-fp")).await;
    let frame = call(&mut wire, 1, claude_params()).await;
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(
        body(&frame),
        json!({"result": {"outcome": "reset", "request_id": CLAUDE_REQUEST}})
    );
    let redeems = fixture.server.redeems();
    assert_eq!(redeems.len(), 1);
    assert!(redeems[0].contains("\"grant_id\":\"grant-session\""));
    assert!(redeems[0].contains(&format!("\"request_id\":\"{CLAUDE_REQUEST}\"")));

    // The same request id replays; a new one meets the 30-minute bound.
    assert_eq!(
        body(&call(&mut wire, 1, claude_params()).await)["result"]["outcome"],
        "reset"
    );
    let mut second = claude_params();
    second["request_id"] = json!("phone-claude-0002");
    second["kind"] = json!("weekly");
    assert_eq!(
        body(&call(&mut wire, 1, second).await)["code"],
        "recently_reset"
    );
    assert_eq!(fixture.server.redeems().len(), 1);
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn a_claude_spend_needs_a_known_kind_and_a_claude_sized_request_id() {
    let fixture = claude_fixture().await;
    let mut wire = Wire::with_registry(false, 65536, fixture.registry.clone()).await;
    bind(&mut wire, 1, callosum(), Some("fed-phone:synthetic-fp")).await;
    for (field, value) in [
        ("kind", Value::Null),
        ("kind", json!("everything")),
        ("kind", json!(7)),
        ("request_id", json!("a".repeat(65))),
    ] {
        let mut params = claude_params();
        params[field] = value;
        let frame = call(&mut wire, 1, params).await;
        assert_eq!(frame.header.ty, FrameType::Error);
        assert_eq!(body(&frame)["code"], "invalid_request");
    }
    assert!(fixture.server.redeems().is_empty());
    wire.stop().await;
    wire.finish().await;
}

/// A desktop route the daemon attests as an operator app may spend a Claude
/// reset, and a plain direct route may not, exactly as for Codex.
#[tokio::test]
async fn only_an_attested_caller_may_spend_a_claude_saved_reset() {
    let fixture = claude_fixture().await;
    let mut wire = Wire::with_registry(false, 65536, fixture.registry.clone()).await;
    bind_unscoped(
        &mut wire,
        1,
        json!({"kind": "direct"}),
        "alfonso-desktop",
        "desktop",
        None,
    )
    .await;
    assert_eq!(
        body(&call(&mut wire, 1, claude_params()).await)["code"],
        "forbidden"
    );
    assert!(fixture.server.redeems().is_empty());
    bind_unscoped(
        &mut wire,
        2,
        json!({"kind": "direct"}),
        "alfonso-desktop",
        "desktop-1",
        Some(json!({"app": "alfonso-desktop", "pid": 4242})),
    )
    .await;
    assert_eq!(
        body(&call(&mut wire, 2, claude_params()).await)["result"]["outcome"],
        "reset"
    );
    assert_eq!(fixture.server.redeems().len(), 1);
    wire.stop().await;
    wire.finish().await;
}

/// Each answer Claude can give reaches the caller under its published code.
#[tokio::test]
async fn claude_redemption_answers_map_to_published_outcomes() {
    for (status, reply, code, detail) in [
        (
            200,
            r#"{"result":"already_used","reason":"already_used"}"#,
            "already_redeemed",
            Value::Null,
        ),
        (
            200,
            r#"{"result":"not_limited","reason":"not_limited"}"#,
            "nothing_to_reset",
            Value::Null,
        ),
        (
            200,
            r#"{"result":"cooldown","cooldown_until":"2099-01-01T05:00:00Z"}"#,
            "recently_reset",
            json!({"cooldown_until": "2099-01-01T05:00:00.000000000+00:00"}),
        ),
        (
            200,
            r#"{"result":"ineligible","reason":"tier"}"#,
            "provider_refused",
            json!({"result": "ineligible", "reason": "tier"}),
        ),
        (
            200,
            r#"{"result":"unavailable","reason":"paused"}"#,
            "provider_refused",
            json!({"result": "unavailable", "reason": "paused"}),
        ),
        (429, "{}", "provider_unavailable", Value::Null),
        (503, "{}", "provider_unavailable", Value::Null),
    ] {
        let fixture = claude_fixture().await;
        fixture.server.set_reply(status, reply);
        let mut wire = Wire::with_registry(false, 65536, fixture.registry.clone()).await;
        bind(&mut wire, 1, callosum(), Some("fed-phone:synthetic-fp")).await;
        let reply_body = body(&call(&mut wire, 1, claude_params()).await);
        let got = reply_body["result"]["outcome"]
            .as_str()
            .or(reply_body["code"].as_str())
            .unwrap()
            .to_string();
        assert_eq!(got, code, "{status} {reply}: {reply_body}");
        if !detail.is_null() {
            assert_eq!(reply_body["detail"], detail, "{reply}");
        }
        assert_eq!(fixture.server.redeems().len(), 1);
        wire.stop().await;
        wire.finish().await;
    }
}

/// A sent attempt makes the account due, so the next poll shows the spend.
#[tokio::test]
async fn a_claude_spend_marks_the_account_due() {
    let fixture = claude_fixture().await;
    let key = quota_core::store::SlotKey::new(
        "claude",
        CredentialHandle::scoped(mock::CREDENTIAL_ID, "oauth"),
    );
    let due = || {
        fixture
            .registry
            .slot_store()
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .next_due_at
    };
    assert!(due() > Instant::now());
    let mut wire = Wire::with_registry(false, 65536, fixture.registry.clone()).await;
    bind(&mut wire, 1, callosum(), Some("fed-phone:synthetic-fp")).await;
    assert_eq!(
        body(&call(&mut wire, 1, claude_params()).await)["result"]["outcome"],
        "reset"
    );
    assert!(due() <= Instant::now());
    wire.stop().await;
    wire.finish().await;
}
