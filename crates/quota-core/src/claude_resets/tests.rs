use super::mock::{self, MockAnthropic, MockSource, ACCOUNT, BEARER, ORGANIZATION};
use super::*;
use crate::codex_resets::{RedemptionJournal, SavedResetResult};
use crate::provider::{CredentialHandle, UsageProvider};
use serde_json::json;
use std::sync::Arc;

fn at(raw: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(raw)
        .unwrap()
        .with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    at("2026-10-06T12:00:00Z")
}

#[test]
fn kinds_follow_claude_codes_rule_including_fable_and_the_clears_fallback() {
    for (grant, kind) in [
        // Giving back the weekly model limit wins over everything else.
        (
            json!({"gives_back_weekly": true, "type": "session", "clears": ["five_hour"]}),
            "model_weekly",
        ),
        (
            json!({"gives_back_weekly": false, "type": "weekly"}),
            "weekly",
        ),
        (
            json!({"type": "session", "clears": ["seven_day"]}),
            "session",
        ),
        (json!({"type": "full", "clears": ["five_hour"]}), "full"),
        // Without a known type, the windows cleared decide.
        (json!({"clears": ["five_hour"]}), "session"),
        (
            json!({"type": "future_kind", "clears": ["five_hour"]}),
            "session",
        ),
        (json!({"clears": ["seven_day", "seven_day_opus"]}), "weekly"),
        (json!({"clears": ["five_hour", "seven_day"]}), "full"),
        (json!({"clears": []}), "full"),
        (json!({}), "full"),
        // Window keys Claude Code does not know are dropped before deciding.
        (
            json!({"clears": ["five_hour", "made_up_window"]}),
            "session",
        ),
        (json!({"clears": ["made_up_window"]}), "full"),
    ] {
        assert_eq!(grant_kind(&grant), kind, "{grant}");
    }
}

fn held_grant(id: &str, kind: &'static str, ends_at: Option<&str>) -> HeldGrant {
    HeldGrant {
        id: id.to_string(),
        kind,
        resets_left: 1,
        starts_at: Some(at("2020-01-01T00:00:00Z")),
        ends_at: ends_at.map(at),
        paused: false,
        usable_now: true,
        use_requires_limit: true,
        unavailable_reason: None,
    }
}

#[test]
fn the_grant_chosen_is_the_soonest_expiring_available_one_of_its_kind() {
    let mut paused = held_grant("paused", "session", Some("2026-10-07T00:00:00Z"));
    paused.paused = true;
    let mut unusable = held_grant("unusable", "session", Some("2026-10-07T00:00:00Z"));
    unusable.usable_now = false;
    let mut blocked = held_grant("blocked", "session", Some("2026-10-07T00:00:00Z"));
    blocked.unavailable_reason = Some("blocked_five_hour".into());
    let mut used = held_grant("used", "session", Some("2026-10-07T00:00:00Z"));
    used.resets_left = 0;
    let mut future = held_grant("future", "session", Some("2026-10-07T00:00:00Z"));
    future.starts_at = Some(at("2026-10-06T13:00:00Z"));
    let held = HeldResets {
        grants: vec![
            paused,
            unusable,
            blocked,
            used,
            future,
            held_grant("expired", "session", Some("2026-10-06T11:00:00Z")),
            held_grant("other-kind", "weekly", Some("2026-10-07T00:00:00Z")),
            held_grant("no-expiry", "session", None),
            held_grant("later", "session", Some("2026-10-20T00:00:00Z")),
            held_grant("sooner", "session", Some("2026-10-10T00:00:00Z")),
        ],
        next_grant_id: None,
    };
    assert_eq!(choose_grant(&held, "session", now()).unwrap().id, "sooner");
    assert_eq!(
        choose_grant(&held, "weekly", now()).unwrap().id,
        "other-kind"
    );
    assert!(choose_grant(&held, "full", now()).is_none());

    // Claude refuses anything but its next grant, so that one wins when it
    // is available, and is passed over when it is not.
    let mut preferred = held.clone();
    preferred.next_grant_id = Some("later".into());
    assert_eq!(
        choose_grant(&preferred, "session", now()).unwrap().id,
        "later"
    );
    preferred.next_grant_id = Some("paused".into());
    assert_eq!(
        choose_grant(&preferred, "session", now()).unwrap().id,
        "sooner"
    );

    // Only unusable grants of the kind: nothing to spend.
    let only_unusable = HeldResets {
        grants: held.grants[..6].to_vec(),
        next_grant_id: None,
    };
    assert!(choose_grant(&only_unusable, "session", now()).is_none());
}

#[test]
fn held_grants_need_a_claude_shaped_id_and_never_print_it() {
    let block = mock::eligible(
        vec![
            mock::grant("good_id-1", "2099-01-01T00:00:00Z", json!({})),
            mock::grant("Bad Id", "2099-01-01T00:00:00Z", json!({})),
            mock::grant(&"x".repeat(41), "2099-01-01T00:00:00Z", json!({})),
            json!({"resets_left": 1, "paused": false}),
        ],
        Some("not a valid id"),
    );
    let held = held_resets(Some(&block)).unwrap();
    assert_eq!(held.grants.len(), 1);
    assert_eq!(held.grants[0].id, "good_id-1");
    assert_eq!(held.next_grant_id, None);
    assert!(!format!("{held:?}").contains("good_id-1"));
    assert!(held_resets(Some(&json!({"eligible": false, "grants": []}))).is_none());
}

#[test]
fn each_redemption_result_maps_to_its_published_outcome() {
    use crate::codex_resets::SavedResetError as E;
    let reply = |body: Value| map_reply(body.to_string().as_bytes());
    assert_eq!(reply(json!({"result": "reset"})), Ok(ConsumeOutcome::Reset));
    assert_eq!(
        reply(json!({"result": "already_used", "reason": "already_used"})),
        Ok(ConsumeOutcome::AlreadyRedeemed)
    );
    assert_eq!(
        reply(json!({"result": "not_limited", "reason": "not_limited"})),
        Ok(ConsumeOutcome::NothingToReset)
    );
    assert_eq!(
        reply(json!({"result": "cooldown", "cooldown_until": "2026-10-06T13:00:00Z"})),
        Err(E::ProviderCooldown(Some(at("2026-10-06T13:00:00Z"))))
    );
    assert_eq!(
        reply(json!({"result": "cooldown", "cooldown_until": null})),
        Err(E::ProviderCooldown(None))
    );
    assert_eq!(
        reply(json!({"result": "ineligible", "reason": "tier"})),
        Err(E::ProviderDeclined {
            result: "ineligible".into(),
            reason: Some("tier".into())
        })
    );
    assert_eq!(
        reply(json!({"result": "unavailable", "reason": "Not A Code <x>"})),
        Err(E::ProviderDeclined {
            result: "unavailable".into(),
            reason: Some("unrecognized".into())
        })
    );
    assert!(matches!(
        reply(json!({"result": "something_new"})),
        Err(E::ProviderRefused(_))
    ));
    assert!(matches!(map_reply(b"not json"), Err(E::ProviderRefused(_))));

    for retryable in [
        FetchError::ProviderStatus(429, "slow down".into()),
        FetchError::ProviderStatus(503, "down".into()),
        FetchError::Upstream("connection reset".into()),
    ] {
        assert!(matches!(map_failure(retryable), E::ProviderUnavailable(_)));
    }
    for refused in [401, 403, 400] {
        assert!(matches!(
            map_failure(FetchError::ProviderStatus(refused, "no".into())),
            E::ProviderRefused(_)
        ));
    }
}

fn handle() -> CredentialHandle {
    CredentialHandle::scoped(mock::CREDENTIAL_ID, "oauth")
}

struct Fixture {
    server: MockAnthropic,
    source: Arc<MockSource>,
    provider: crate::anthropic::AnthropicProvider,
    journal: std::path::PathBuf,
    _temp: crate::tests::ResetTempDir,
}

impl Fixture {
    async fn new(block: Value) -> Self {
        let temp = crate::tests::ResetTempDir::with_prefix("ck-claude-reset", "journal");
        let journal = temp.path().join("redemptions.json");
        let server = MockAnthropic::start(mock::usage_body(block)).await;
        let source = Arc::new(MockSource::default());
        let provider = Self::provider(&server, &source, &journal).await;
        Self {
            server,
            source,
            provider,
            journal,
            _temp: temp,
        }
    }

    /// A provider over the same journal, as after a restart, that has polled.
    async fn provider(
        server: &MockAnthropic,
        source: &Arc<MockSource>,
        journal: &std::path::Path,
    ) -> crate::anthropic::AnthropicProvider {
        let provider = crate::anthropic::AnthropicProvider::redemption_fixture(
            source.clone(),
            &server.base,
            journal.to_path_buf(),
        );
        let attempt = provider.fetch_handle(&handle()).await;
        assert!(attempt.usage.is_ok(), "the fixture poll must succeed");
        provider
    }

    async fn spend(&self, request_id: &str, kind: &str) -> SavedResetResult {
        let result = self
            .provider
            .use_saved_reset(&handle(), ACCOUNT, request_id, Some(kind))
            .await;
        // Let a dispatched vault auth-failure report reach the mock.
        tokio::task::yield_now().await;
        result
    }
}

fn two_session_grants() -> Value {
    mock::eligible(
        vec![
            mock::grant(
                "grant-later",
                "2099-02-01T00:00:00Z",
                json!({"type": "session"}),
            ),
            mock::grant(
                "grant-sooner",
                "2099-01-01T00:00:00Z",
                json!({"type": "session"}),
            ),
            mock::grant(
                "grant-weekly",
                "2098-01-01T00:00:00Z",
                json!({"type": "weekly"}),
            ),
        ],
        None,
    )
}

#[tokio::test]
async fn a_spend_posts_the_chosen_grant_under_the_profiles_organization() {
    let fixture = Fixture::new(two_session_grants()).await;
    let result = fixture.spend("phone-request-0001", "session").await;
    assert_eq!(result.result, Ok(ConsumeOutcome::Reset));
    assert!(result.consume_attempted);
    let redeems = fixture.server.redeems();
    assert_eq!(redeems.len(), 1);
    let request = redeems[0].to_ascii_lowercase();
    assert!(request.starts_with(&format!(
        "post /api/organizations/{ORGANIZATION}/reset_rate_limits http/1.1\r\n"
    )));
    assert!(request.contains(&format!("authorization: bearer {BEARER}\r\n")));
    assert!(request.contains("anthropic-beta: oauth-2025-04-20\r\n"));
    assert!(request.contains("user-agent: claude-cli/2.1.282 (external, cli)\r\n"));
    let body: Value = serde_json::from_str(redeems[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"program": "cedar_ember", "grant_id": "grant-sooner",
               "request_id": "phone-request-0001"})
    );
    assert_eq!(fixture.server.profile_reads(), 1);
    // The journal records the spend by provider and account, never the grant.
    let journal = std::fs::read_to_string(&fixture.journal).unwrap();
    assert!(journal.contains("\"provider\": \"claude\""), "{journal}");
    assert!(!journal.contains("grant-"), "{journal}");
}

#[tokio::test]
async fn the_next_grant_is_preferred_over_the_soonest_expiring_one() {
    let mut block = two_session_grants();
    block["next_grant_id"] = json!("grant-later");
    let fixture = Fixture::new(block).await;
    fixture.spend("phone-request-0001", "session").await;
    assert!(fixture.server.redeems()[0].contains("\"grant_id\":\"grant-later\""));
}

#[tokio::test]
async fn a_retried_request_id_replays_its_outcome_without_a_second_post() {
    let fixture = Fixture::new(two_session_grants()).await;
    assert_eq!(
        fixture.spend("phone-request-0001", "session").await.result,
        Ok(ConsumeOutcome::Reset)
    );
    let retry = fixture.spend("phone-request-0001", "session").await;
    assert_eq!(retry.result, Ok(ConsumeOutcome::Reset));
    assert!(!retry.consume_attempted);
    // After a restart the journal alone still answers the retry.
    let restarted = Fixture::provider(&fixture.server, &fixture.source, &fixture.journal).await;
    let replay = restarted
        .use_saved_reset(&handle(), ACCOUNT, "phone-request-0001", Some("session"))
        .await;
    assert_eq!(replay.result, Ok(ConsumeOutcome::Reset));
    assert!(!replay.consume_attempted);
    assert_eq!(fixture.server.redeems().len(), 1);
}

#[tokio::test]
async fn the_spend_bound_blocks_a_second_claude_redeem_for_the_same_account() {
    let fixture = Fixture::new(two_session_grants()).await;
    fixture.spend("phone-request-0001", "session").await;
    let second = fixture.spend("phone-request-0002", "weekly").await;
    assert!(
        matches!(second.result, Err(SavedResetError::RecentlyReset(_))),
        "{:?}",
        second.result
    );
    assert!(!second.consume_attempted);
    assert_eq!(fixture.server.redeems().len(), 1);
}

#[tokio::test]
async fn no_available_grant_of_the_kind_is_nothing_to_reset_without_a_post() {
    let fixture = Fixture::new(two_session_grants()).await;
    let result = fixture.spend("phone-request-0001", "model_weekly").await;
    assert_eq!(result.result, Ok(ConsumeOutcome::NothingToReset));
    assert!(!result.consume_attempted);
    assert!(fixture.server.redeems().is_empty());
    assert!(
        !fixture.journal.exists()
            || !std::fs::read_to_string(&fixture.journal)
                .unwrap()
                .contains("phone-request-0001")
    );
    let unknown = fixture.spend("phone-request-0002", "everything").await;
    assert!(matches!(
        unknown.result,
        Err(SavedResetError::InvalidRequest(_))
    ));
}

#[tokio::test]
async fn a_401_reports_the_served_bearer_and_a_403_does_not() {
    let fixture = Fixture::new(two_session_grants()).await;
    fixture.server.set_reply(403, r#"{"error":"forbidden"}"#);
    let forbidden = fixture.spend("phone-request-0001", "session").await;
    assert!(matches!(
        forbidden.result,
        Err(SavedResetError::ProviderRefused(_))
    ));
    assert!(forbidden.consume_attempted);
    assert!(fixture.source.reports.lock().unwrap().is_empty());

    // The refused id is still pending, so retrying it is the same redemption
    // and is sent again rather than blocked by the spend bound.
    fixture.server.set_reply(401, r#"{"error":"expired"}"#);
    let rejected = fixture.spend("phone-request-0001", "session").await;
    assert!(matches!(
        rejected.result,
        Err(SavedResetError::ProviderRefused(_))
    ));
    assert_eq!(
        *fixture.source.reports.lock().unwrap(),
        vec![(401, mock::RECORD_VERSION)]
    );
    assert_eq!(fixture.server.redeems().len(), 2);
}

#[tokio::test]
async fn a_rate_limited_or_failing_redeem_is_retryable_with_the_same_grant() {
    let fixture = Fixture::new(two_session_grants()).await;
    fixture.server.set_reply(429, "{}");
    let limited = fixture.spend("phone-request-0001", "session").await;
    assert!(matches!(
        limited.result,
        Err(SavedResetError::ProviderUnavailable(_))
    ));
    // A poll between the attempts now offers a different grant first; the
    // retry of the same request id must still name the grant it sent.
    let mut block = two_session_grants();
    block["next_grant_id"] = json!("grant-later");
    fixture.server.set_usage(mock::usage_body(block));
    assert!(fixture.provider.fetch_handle(&handle()).await.usage.is_ok());
    fixture.server.set_reply(503, "{}");
    let failing = fixture.spend("phone-request-0001", "session").await;
    assert!(matches!(
        failing.result,
        Err(SavedResetError::ProviderUnavailable(_))
    ));
    fixture.server.set_reply(200, r#"{"result":"reset"}"#);
    assert_eq!(
        fixture.spend("phone-request-0001", "session").await.result,
        Ok(ConsumeOutcome::Reset)
    );
    let redeems = fixture.server.redeems();
    assert_eq!(redeems.len(), 3);
    assert!(redeems
        .iter()
        .all(|request| request.contains("\"grant_id\":\"grant-sooner\"")));
}

#[test]
fn journal_records_are_scoped_by_provider_and_old_journals_still_read() {
    let temp = crate::tests::ResetTempDir::with_prefix("ck-claude-reset", "scope");
    let path = temp.path().join("redemptions.json");
    // A record written before providers were recorded is Codex's.
    std::fs::write(
        &path,
        r#"[{"account_id":"shared","redeem_request_id":"old-request-1","created_at":"2026-10-06T11:50:00+00:00","status":"resolved","outcome":"reset"}]"#,
    )
    .unwrap();
    let journal = RedemptionJournal::new(path.clone());
    let records = journal.records().unwrap();
    assert_eq!(records[0].provider, "codex");
    // The Codex record's bound does not block the same account id on Claude,
    // and Claude's record does not reach the automatic Codex policy.
    assert_eq!(
        journal
            .reserve_manual_for("claude", "shared", "claude-request", now())
            .unwrap(),
        crate::codex_resets::ManualReservation::Pending
    );
    assert!(matches!(
        journal
            .reserve_manual("shared", "codex-request", now())
            .unwrap(),
        crate::codex_resets::ManualReservation::RecentlyReset(_)
    ));
    let state = journal.inspect_account("other", now()).unwrap();
    assert!(state
        .recent_elsewhere
        .iter()
        .all(|redemption| redemption.at != now()));
    assert_eq!(state.recent_elsewhere.len(), 1, "only the Codex record");
    // A Codex record keeps its old bytes: no provider key is written for it.
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written.matches("\"provider\"").count(), 1, "{written}");
}
