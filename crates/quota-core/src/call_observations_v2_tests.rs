use super::*;
use crate::model::ProviderUsage;
use crate::provider::{CredentialHandle, FetchAttempt, UsageProvider};
use crate::store::SlotKey;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const AT: i64 = 1_790_394_000_000;
const CREDENTIAL: &str = "chatgpt:openai:synthetic";

struct Poll(&'static str);
#[async_trait::async_trait]
impl UsageProvider for Poll {
    fn name(&self) -> &str {
        self.0
    }
    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        Ok(vec![CredentialHandle::scoped(CREDENTIAL, "oauth")])
    }
    async fn fetch_handle(&self, _: &CredentialHandle) -> FetchAttempt {
        let entry: ProviderUsage = serde_json::from_value(json!({
            "provider":self.0,"account":"synthetic-account","source":"oauth",
            "accountInfo":{"email":"synthetic@example.invalid"},
            "usage":{"primary":{"usedPercent":40,"windowMinutes":300,"resetsAt":"2026-09-26T06:00:00Z"},
                "secondary":{"usedPercent":70,"windowMinutes":10080,"resetsAt":"2026-09-30T04:00:00Z"}}
        })).unwrap();
        FetchAttempt::from_provider_usage(Ok(entry))
            .with_value_observed_at(DateTime::from_timestamp_millis(AT - 1000).unwrap())
    }
}

async fn registry(provider: &'static str) -> Registry {
    let registry = Registry::new(vec![Box::new(Poll(provider))]);
    registry.refresh_tick(&CancellationToken::new()).await;
    registry
}

fn row(provider: &str) -> Value {
    json!({"provider":provider,"credentialId":CREDENTIAL,"observedAtMs":AT,
        "headers":{"anthropic-ratelimit-unified-5h-utilization":"0.6",
            "x-codex-primary-used-percent":"80","x-codex-primary-window-minutes":"10080",
            "x-codex-primary-reset-at":"1790740800"}})
}

fn ignored(registry: &Registry, writer: ObservationWriter, row: Value, reason: &str) {
    let result = registry.observe_calls(writer, vec![row]);
    assert_eq!(result.accepted, 0);
    assert_eq!(result.ignored.len(), 1);
    assert_eq!(result.ignored[0].reason, reason);
}

#[test]
fn observation_sources_cannot_call_reset_policy() {
    for source in [
        include_str!("call_observations.rs"),
        include_str!("codex_call_windows.rs"),
    ] {
        let body = crate::tests::production_body(source);
        for forbidden in [
            "ResetCoordinator",
            "UsageFacts",
            "observe_headroom",
            "observe_handle",
            "process_tick",
        ] {
            assert!(
                !body.contains(forbidden),
                "observation source refers to {forbidden}"
            );
        }
    }
}

#[tokio::test]
async fn claustrum_requires_matching_vouch() {
    let registry = registry("claude").await;
    ignored(
        &registry,
        ObservationWriter::Claustrum,
        row("claude"),
        "vouch_mismatch",
    );
    let mut bad = row("claude");
    bad["vouched"] = json!({"principal":"enrolled:synthetic","credentialId":"wrong"});
    ignored(
        &registry,
        ObservationWriter::Claustrum,
        bad,
        "vouch_mismatch",
    );
    let mut good = row("claude");
    good["vouched"] = json!({"principal":"enrolled:synthetic","credentialId":CREDENTIAL});
    assert_eq!(
        registry
            .observe_calls(ObservationWriter::Claustrum, vec![good])
            .accepted,
        1
    );
    assert_eq!(
        registry.call_observation_metrics().vouched_principals["enrolled:synthetic"],
        1
    );
}

#[tokio::test]
async fn only_claustrum_may_vouch() {
    let registry = registry("claude").await;
    for writer in [ObservationWriter::Broca, ObservationWriter::Routing] {
        let mut bad = row("claude");
        bad["vouched"] = json!({"principal":"enrolled:synthetic","credentialId":CREDENTIAL});
        ignored(&registry, writer, bad.clone(), "vouch_mismatch");
        bad["vouched"] = Value::Null;
        ignored(&registry, writer, bad, "vouch_mismatch");
    }
}

#[tokio::test]
async fn unknown_credential_cannot_fall_back_to_account() {
    let registry = registry("claude").await;
    let mut bad = row("claude");
    bad["credentialId"] = json!("missing");
    bad["account"] = json!("synthetic-account");
    ignored(
        &registry,
        ObservationWriter::Broca,
        bad,
        "unknown_credential",
    );
    assert_eq!(
        registry
            .observe_calls(ObservationWriter::Broca, vec![row("claude")])
            .accepted,
        1
    );
}

#[tokio::test]
async fn account_mismatch_refuses_without_mutation() {
    let registry = registry("claude").await;
    let before = registry.usage_snapshot(None).await.to_envelope();
    let mut bad = row("claude");
    bad["account"] = json!("another-account");
    ignored(&registry, ObservationWriter::Broca, bad, "account_mismatch");
    assert_eq!(before, registry.usage_snapshot(None).await.to_envelope());
}

#[tokio::test]
async fn future_reading_is_refused() {
    let registry = registry("claude").await;
    let mut bad = row("claude");
    bad["observedAtMs"] = json!(Utc::now().timestamp_millis() + 61_000);
    ignored(&registry, ObservationWriter::Broca, bad, "future_reading");
}

#[tokio::test]
async fn early_new_period_refuses_whole_row_for_both_providers() {
    for provider in ["claude", "codex"] {
        let registry = registry(provider).await;
        let before = registry.usage_snapshot(None).await.to_envelope();
        let mut bad = row(provider);
        bad["headers"] = json!({
            "anthropic-ratelimit-unified-5h-utilization":"0.01",
            "anthropic-ratelimit-unified-5h-reset":"1791345600",
            "x-codex-primary-used-percent":"1","x-codex-primary-window-minutes":"10080",
            "x-codex-primary-reset-at":"1791345600"});
        ignored(
            &registry,
            ObservationWriter::Broca,
            bad.clone(),
            "early_new_period",
        );
        assert_eq!(before, registry.usage_snapshot(None).await.to_envelope());
        bad["observedAtMs"] = json!(1790740800000_i64);
        assert_eq!(
            registry
                .observe_calls(ObservationWriter::Broca, vec![bad])
                .accepted,
            1
        );
    }
}

#[tokio::test]
async fn codex_matches_length_not_primary_slot_and_never_adds_periods() {
    let registry = registry("codex").await;
    assert_eq!(
        registry
            .observe_calls(ObservationWriter::Broca, vec![row("codex")])
            .accepted,
        1
    );
    let entries = registry.get_usage(None).await;
    let usage = entries[0].usage.as_ref().unwrap();
    assert_eq!(usage.primary.as_ref().unwrap().used_percent, 40.0);
    assert_eq!(usage.secondary.as_ref().unwrap().used_percent, 80.0);
    let mut unknown = row("codex");
    unknown["observedAtMs"] = json!(AT + 1000);
    unknown["headers"]["x-codex-primary-window-minutes"] = json!("60");
    ignored(
        &registry,
        ObservationWriter::Broca,
        unknown,
        "no_usable_window",
    );
}

#[tokio::test]
async fn malformed_and_oversize_rows_do_not_refuse_siblings() {
    let registry = registry("claude").await;
    let mut oversized = row("claude");
    oversized["frame"] = json!({"padding":"x".repeat(16 * 1024)});
    let mut wrong_frame = row("claude");
    wrong_frame["frame"] = json!([]);
    let result = registry.observe_calls(
        ObservationWriter::Broca,
        vec![oversized, wrong_frame, row("claude")],
    );
    assert_eq!(result.accepted, 1);
    assert_eq!(
        result.ignored.iter().map(|i| i.reason).collect::<Vec<_>>(),
        vec!["invalid_observation"; 2]
    );
    assert_eq!(
        registry.call_observation_metrics().by_writer["reserved:broca"],
        3
    );
}

#[tokio::test(start_paused = true)]
async fn codex_cadence_exception_tracks_live_config() {
    let mut registry = registry("codex").await;
    let config = crate::config::LiveQuotaConfig::from(crate::config::QuotaConfig::default());
    registry.live_config = Some(config.clone());
    assert_eq!(
        registry
            .observe_calls(ObservationWriter::Broca, vec![row("codex")])
            .accepted,
        1
    );
    let key = SlotKey::new("codex", CredentialHandle::scoped(CREDENTIAL, "oauth"));
    let slot = registry.store.lock().unwrap().get(&key).unwrap().clone();
    let last = slot.last_poll_completed_at.unwrap();
    let now = last + std::time::Duration::from_secs(60);
    let due = || {
        crate::select_due_round_robin(
            &registry.providers,
            vec![(key.clone(), slot.clone())],
            now,
            None,
            registry.codex_resets_armed(),
        )
        .0
        .len()
    };
    assert_eq!(due(), 0);
    let armed = crate::config::QuotaConfig {
        codex: crate::config::CodexConfig { auto_use_resets: 1 },
        ..Default::default()
    };
    let temp = crate::tests::ResetTempDir::with_prefix("usage-observe", "live-config");
    let path = temp.path().join("quota.json");
    std::fs::write(&path, "{}").unwrap();
    let mut reloader = crate::config::ConfigReloader::new(
        path.clone(),
        Box::new(|s| serde_json::from_str(s).map_err(|e| e.to_string())),
        config.clone(),
    );
    reloader.check();
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"codex":{"auto_use_resets":armed.codex.auto_use_resets}}))
            .unwrap(),
    )
    .unwrap();
    reloader.check();
    assert_eq!(
        due(),
        1,
        "arming resets must restore the normal poll immediately"
    );
    std::fs::write(&path, "{}").unwrap();
    reloader.check();
    assert_eq!(due(), 0);
    assert_eq!(
        slot.poll_due_at("codex", now, false),
        last + std::time::Duration::from_secs(900)
    );
}
