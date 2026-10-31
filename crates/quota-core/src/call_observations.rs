//! Forwarded model-call readings update only windows whose headers are present.
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{anthropic, refresh, scheduler_now, Registry};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallObservation {
    pub provider: String,
    pub account: String,
    pub observed_at_ms: i64,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Serialize)]
pub struct ObserveResult {
    pub accepted: u64,
    pub ignored: Vec<IgnoredObservation>,
}

#[derive(Debug, Serialize)]
pub struct IgnoredObservation {
    pub account: String,
    pub reason: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct CallObservationMetrics {
    pub applied: u64,
    pub ignored: BTreeMap<String, u64>,
}

impl Registry {
    pub fn call_observation_metrics(&self) -> CallObservationMetrics {
        self.call_metrics
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The transport must authenticate the writer before calling this method.
    /// Decode rows independently so a malformed row cannot refuse its siblings.
    pub fn observe_calls(&self, observations: Vec<serde_json::Value>) -> ObserveResult {
        let mut result = ObserveResult::default();
        for value in observations {
            let account = value
                .get("account")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_owned();
            let applied = serde_json::from_value::<CallObservation>(value)
                .map_err(|_| "invalid_observation")
                .and_then(|observation| self.apply_call_observation(observation));
            let mut metrics = self.call_metrics.lock().unwrap_or_else(|p| p.into_inner());
            match applied {
                Ok(()) => {
                    result.accepted += 1;
                    metrics.applied += 1;
                }
                Err(reason) => {
                    *metrics.ignored.entry(reason.to_owned()).or_default() += 1;
                    result.ignored.push(IgnoredObservation { account, reason });
                }
            }
        }
        result
    }

    fn apply_call_observation(&self, observation: CallObservation) -> Result<(), &'static str> {
        if observation.provider != "claude" {
            return Err("unsupported_provider");
        }
        let now = scheduler_now();
        let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let slots: Vec<_> = store
            .snapshot()
            .into_iter()
            .filter(|(key, slot)| {
                key.provider == "claude"
                    && !slot.label_in_flux
                    && slot.account_id() == Some(observation.account.as_str())
            })
            .collect();
        if slots.is_empty() {
            return Err("unknown_account");
        }
        let eligible: Vec<_> = slots
            .into_iter()
            .filter(|(_, slot)| {
                slot.last_poll_completed_at.is_some()
                    && slot
                        .entry
                        .as_ref()
                        .is_some_and(|e| e.error.is_none() && e.usage.is_some())
            })
            .collect();
        if eligible.is_empty() {
            return Err("no_poll_reading_yet");
        }
        let incoming = DateTime::<Utc>::from_timestamp_millis(observation.observed_at_ms)
            .ok_or("invalid_observation")?;
        let held = eligible
            .iter()
            .filter_map(|(_, slot)| slot.last_success_wall)
            .max();
        if !refresh::value_time_supersedes(Some(incoming), held, true) {
            return Err("older_than_held");
        }
        let windows = anthropic::call_windows(&observation.headers);
        if windows.primary.is_none() && windows.secondary.is_none() {
            return Err("no_usable_window");
        }
        if eligible.iter().any(|(_, slot)| regresses(slot, &windows)) {
            return Err("window_regressed");
        }
        for (key, prev) in eligible {
            let mut next = prev.clone();
            let usage = next.entry.as_mut().unwrap().usage.as_mut().unwrap();
            // Preserve breakdowns and all poll-only windows and metadata. A
            // partial call response cannot describe the complete account
            // inventory, so it only updates windows the poll already holds and
            // never adds one. Anthropic removed the 5-hour limit from verified
            // Ultra accounts (October 2026): their usage endpoint answers
            // `five_hour: null`, and a 5-hour header on a model call must not
            // bring back a window that limit no longer has.
            for (held, update) in [
                (&mut usage.primary, &windows.primary),
                (&mut usage.secondary, &windows.secondary),
            ] {
                if let (Some(held), Some(update)) = (held.as_mut(), update) {
                    held.used_percent = update.used_percent;
                    held.resets_at = update.resets_at.clone();
                }
            }
            next.last_success_wall = Some(incoming);
            next.last_success_at = Some(now);
            next.last_call_observation_at = Some(now);
            next.status = refresh::SlotStatus::Fresh;
            next.failing_since = None;
            next.failing_since_wall = None;
            // Keep poll failures and their retry deadline: a model success must
            // not erase the endpoint's rate-limit backoff or vault repair state.
            self.publish_reading(&mut store, &key, &prev, next, now);
        }
        Ok(())
    }
}

/// Whether a forwarded reading reports less use than the held reading for a
/// window whose reset hasn't moved.
///
/// Use within one window period only grows, so a fall beyond rounding with the
/// same reset time can't be real. Anthropic does send such readings: on
/// 2026-10-22 one account's responses alternated, within seconds and within one
/// run, between its real weekly figure (61%) and a reply with the weekly and
/// 5-hour counters at 0 and the weekly reset unchanged, apparently from a
/// replica that had lost the count. Applying those made the account flap
/// between exhausted and empty. The whole observation is refused, because its
/// other windows came from the same bad reply.
///
/// The 1-point allowance covers rounding: forwarded headers carry two decimals
/// and the usage endpoint whole percents. Resets compare to the minute, because
/// the endpoint sends sub-second resets and the headers whole seconds. A genuine
/// early reset that keeps its reset time would be refused here too, but only
/// until insula's own poll of the usage endpoint, which this check never blocks.
fn regresses(slot: &refresh::ProviderSlot, update: &crate::model::Usage) -> bool {
    let Some(held) = slot.entry.as_ref().and_then(|e| e.usage.as_ref()) else {
        return false;
    };
    let same_reset = |a: &Option<String>, b: &Option<String>| {
        let parse = |s: &Option<String>| {
            s.as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.timestamp())
        };
        matches!((parse(a), parse(b)), (Some(a), Some(b)) if (a - b).abs() < 60)
    };
    [
        (&held.primary, &update.primary),
        (&held.secondary, &update.secondary),
    ]
    .into_iter()
    .any(|(held, update)| match (held, update) {
        (Some(held), Some(update)) => {
            same_reset(&held.resets_at, &update.resets_at)
                && update.used_percent < held.used_percent - 1.0
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProviderUsage, Usage};
    use crate::provider::{
        AccountObservation, CredentialHandle, FetchAttempt, FetchError, UsageProvider,
    };
    use crate::store::SlotKey;
    use serde_json::{json, Value};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    const STAMP: i64 = 1790394000123;
    fn headers() -> BTreeMap<String, String> {
        [
            ("status", "rejected"),
            ("representative-claim", "seven_day"),
            ("reset", "1790740800"),
            ("5h-status", "allowed"),
            ("5h-utilization", "0.21"),
            ("5h-reset", "1790394000"),
            ("7d-status", "rejected"),
            ("7d-utilization", "1.0"),
            ("7d-reset", "1790740800"),
            ("7d-surpassed-threshold", "1.0"),
            ("overage-status", "rejected"),
            ("overage-disabled-reason", "org_level_disabled"),
        ]
        .into_iter()
        .map(|(key, value)| {
            (
                format!("anthropic-ratelimit-unified-{key}"),
                value.to_owned(),
            )
        })
        .collect()
    }
    fn observation(at: i64) -> Value {
        json!({"provider":"claude", "account":"a", "observedAtMs":at, "headers":headers()})
    }
    fn poll_entry() -> ProviderUsage {
        serde_json::from_value(json!({"provider":"claude", "account":"a", "source":"oauth",
            "accountInfo":{"email":"a@example.com"},
            "savedResets":{"availableCount":2},
            "usage":{"primary":{"usedPercent":90,"windowMinutes":300,"windowKind":"five_hour"},
                "secondary":{"usedPercent":100,"windowMinutes":10080,"windowKind":"weekly"},
                "tertiary":{"usedPercent":17},
                "extraRateWindows":[{"id":"fable", "label":"Fable", "window":{"usedPercent":8}}]},
            "spend":[{"id":"extra_usage","label":"Extra usage","funding":"unknown","basis":"derived","spendable":true,
                "total":{"minor":10000,"exponent":2,"unit":"USD"},"remaining":{"minor":8723,"exponent":2,"unit":"USD"}}]
        })).unwrap()
    }
    struct Poll;
    #[async_trait::async_trait]
    impl UsageProvider for Poll {
        fn name(&self) -> &str {
            "claude"
        }
        async fn fetch_handle(&self, _: &CredentialHandle) -> FetchAttempt {
            FetchAttempt::from_provider_usage(Ok(poll_entry()))
                .with_value_observed_at(DateTime::from_timestamp_millis(STAMP - 1000).unwrap())
        }
    }
    async fn registry() -> Registry {
        let registry = Registry::new(vec![Box::new(Poll)]);
        registry.refresh_tick(&CancellationToken::new()).await;
        registry
    }

    #[test]
    fn captured_call_headers_parse_and_missing_values_do_not_invent_windows() {
        let parsed = anthropic::call_windows(&headers());
        let poll = anthropic::normalize_usage(br#"{"five_hour":{"utilization":21,"resets_at":"2026-09-26T03:40:00Z"},"seven_day":{"utilization":100,"resets_at":"2026-09-30T04:00:00Z"}}"#).unwrap();
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            serde_json::to_value(poll).unwrap()
        );
        let mut bad = headers();
        bad.remove("anthropic-ratelimit-unified-5h-utilization");
        bad.insert(
            "anthropic-ratelimit-unified-7d-utilization".into(),
            "garbage".into(),
        );
        let parsed = anthropic::call_windows(&bad);
        assert!(parsed.primary.is_none() && parsed.secondary.is_none());
        // 0.14 * 100 is 14.000000000000002 in floating point, live on 2026-10-21.
        // The header said 0.14, so the window must say exactly 14.
        for (value, expected) in [
            ("-0.3", 0.0),
            ("1.5", 100.0),
            ("0.14", 14.0),
            ("0.29", 29.0),
        ] {
            bad.insert(
                "anthropic-ratelimit-unified-5h-utilization".into(),
                value.into(),
            );
            assert_eq!(
                anthropic::call_windows(&bad).primary.unwrap().used_percent,
                expected
            );
        }
        bad.insert(
            "anthropic-ratelimit-unified-5h-utilization".into(),
            "NaN".into(),
        );
        assert!(anthropic::call_windows(&bad).primary.is_none());
    }

    #[tokio::test]
    async fn call_observation_preserves_poll_inventory_and_publishes_drop_and_value_time() {
        let registry = registry().await;
        let before = registry.get_usage(None).await;
        let key = SlotKey::new("claude", CredentialHandle::implicit());
        {
            let mut store = registry.store.lock().unwrap();
            let prev = store.get(&key).unwrap().clone();
            let failed = refresh::next_slot_after_attempt(
                &prev,
                "claude",
                FetchAttempt::failure(
                    Some(AccountObservation::new(Some("a".into()), None)),
                    Some("oauth".into()),
                    FetchError::Upstream("429".into()),
                ),
                scheduler_now(),
                scheduler_now(),
            );
            store.publish_if_current(&key, prev.incarnation, prev.attempt_sequence, failed);
        }
        assert!(registry.get_usage(None).await[0].stale.is_some());
        let result = registry.observe_calls(vec![observation(STAMP)]);
        assert_eq!(result.accepted, 1);
        let after = registry.get_usage(None).await;
        assert!(after[0].stale.is_none());
        assert_eq!(
            after[0].fetched_at,
            Some(crate::rfc3339_canonical(
                DateTime::from_timestamp_millis(STAMP).unwrap()
            ))
        );
        let before = serde_json::to_value(&before[0]).unwrap();
        let after = serde_json::to_value(&after[0]).unwrap();
        for field in ["spend", "savedResets", "accountInfo", "source"] {
            assert_eq!(before[field], after[field], "preserve {field}");
        }
        for field in ["tertiary", "extraRateWindows"] {
            assert_eq!(
                before["usage"][field], after["usage"][field],
                "preserve {field}"
            );
        }
        assert_eq!(after["usage"]["primary"]["usedPercent"], 21.0);
        assert_eq!(registry.drop_page(None).await.drops.len(), 1);
        assert_eq!(registry.call_observation_metrics().applied, 1);
    }

    /// An account whose poll reports no 5-hour window (Anthropic removed that
    /// limit for verified Ultra accounts) keeps none when a model call's headers
    /// carry 5-hour figures; the call still updates the weekly window.
    #[tokio::test]
    async fn a_call_reading_never_adds_a_window_the_poll_did_not_report() {
        let registry = registry().await;
        let key = SlotKey::new("claude", CredentialHandle::implicit());
        {
            let mut store = registry.store.lock().unwrap();
            let prev = store.get(&key).unwrap().clone();
            let mut no_five_hour = prev.clone();
            no_five_hour
                .entry
                .as_mut()
                .unwrap()
                .usage
                .as_mut()
                .unwrap()
                .primary = None;
            store.publish_if_current(&key, prev.incarnation, prev.attempt_sequence, no_five_hour);
        }
        assert!(registry.get_usage(None).await[0]
            .usage
            .as_ref()
            .unwrap()
            .primary
            .is_none());
        let result = registry.observe_calls(vec![observation(STAMP)]);
        assert_eq!(result.accepted, 1);
        let after = registry.get_usage(None).await;
        let usage = after[0].usage.as_ref().unwrap();
        assert!(
            usage.primary.is_none(),
            "a call reading added a 5-hour window the poll did not report"
        );
        assert_eq!(usage.secondary.as_ref().unwrap().used_percent, 100.0);
        assert_eq!(
            after[0].fetched_at,
            Some(crate::rfc3339_canonical(
                DateTime::from_timestamp_millis(STAMP).unwrap()
            ))
        );
    }

    /// The reply Anthropic sent on 2026-10-22 alongside the real one: weekly and
    /// 5-hour at 0, the weekly reset unchanged, the 5-hour reset later.
    fn zeroed_replica(at: i64, weekly_reset: &str) -> Value {
        let mut headers = headers();
        for (key, value) in [
            ("7d-utilization", "0.0"),
            ("7d-reset", weekly_reset),
            ("5h-utilization", "0.0"),
            ("5h-reset", "1790396400"),
        ] {
            headers.insert(format!("anthropic-ratelimit-unified-{key}"), value.into());
        }
        json!({"provider":"claude", "account":"a", "observedAtMs":at, "headers":headers})
    }

    #[tokio::test]
    async fn a_weekly_drop_with_an_unchanged_reset_is_refused_and_a_new_period_is_not() {
        let registry = registry().await;
        assert_eq!(registry.observe_calls(vec![observation(STAMP)]).accepted, 1);
        let held = serde_json::to_value(registry.get_usage(None).await).unwrap();
        let result = registry.observe_calls(vec![zeroed_replica(STAMP + 1000, "1790740800")]);
        assert_eq!(result.accepted, 0);
        assert_eq!(result.ignored[0].reason, "window_regressed");
        assert_eq!(
            serde_json::to_value(registry.get_usage(None).await).unwrap(),
            held,
            "a refused reading must change nothing"
        );
        // A new weekly period (the reset moved) may start at 0.
        let result = registry.observe_calls(vec![zeroed_replica(STAMP + 2000, "1791345600")]);
        assert_eq!(result.accepted, 1);
        let after = registry.get_usage(None).await;
        let weekly = after[0].usage.as_ref().unwrap().secondary.as_ref().unwrap();
        assert_eq!(weekly.used_percent, 0.0);
    }

    #[tokio::test]
    async fn older_call_observations_are_ignored_without_changing_the_held_reading() {
        let registry = registry().await;
        assert_eq!(registry.observe_calls(vec![observation(STAMP)]).accepted, 1);
        let before = serde_json::to_value(registry.get_usage(None).await).unwrap();
        for at in [STAMP - 1, STAMP] {
            let result = registry.observe_calls(vec![observation(at)]);
            assert_eq!(
                result.accepted, 0,
                "an older or equal observation must not be accepted"
            );
            assert_eq!(result.ignored[0].reason, "older_than_held");
            assert_eq!(
                serde_json::to_value(registry.get_usage(None).await).unwrap(),
                before
            );
        }
    }

    #[tokio::test]
    async fn ignored_calls_name_unknown_cold_unsupported_and_unusable_rows_independently() {
        let registry = registry().await;
        let mut cold = observation(STAMP);
        cold["account"] = json!("cold");
        {
            let mut store = registry.store.lock().unwrap();
            let handle = CredentialHandle::new("cold");
            store.insert_known("claude", std::slice::from_ref(&handle), scheduler_now());
            let key = SlotKey::new("claude", handle);
            let mut slot = store.get(&key).unwrap().clone();
            slot.observation = Some(AccountObservation::new(Some("cold".into()), None));
            store.publish_if_current(&key, slot.incarnation, slot.attempt_sequence, slot);
        }
        let mut unknown = observation(STAMP);
        unknown["account"] = json!("unknown");
        let mut unsupported = observation(STAMP);
        unsupported["provider"] = json!("codex");
        let mut unusable = observation(STAMP);
        unusable["headers"] = json!({});
        let result = registry.observe_calls(vec![
            cold,
            unknown,
            unsupported,
            unusable,
            json!({"account":"bad"}),
            observation(STAMP),
        ]);
        assert_eq!(result.accepted, 1);
        assert_eq!(
            result.ignored.iter().map(|i| i.reason).collect::<Vec<_>>(),
            [
                "no_poll_reading_yet",
                "unknown_account",
                "unsupported_provider",
                "no_usable_window",
                "invalid_observation"
            ]
        );
        assert_eq!(
            registry
                .call_observation_metrics()
                .ignored
                .values()
                .sum::<u64>(),
            5
        );
    }

    #[tokio::test(start_paused = true)]
    async fn claude_call_cadence_defers_only_recent_observations_and_keeps_backoff() {
        let registry = registry().await;
        let key = SlotKey::new("claude", CredentialHandle::implicit());
        let mut slot = registry.store.lock().unwrap().get(&key).unwrap().clone();
        let last = slot.last_poll_completed_at.unwrap();
        assert_eq!(slot.next_due_at, last + Duration::from_secs(180));
        assert!(
            slot.poll_due_at("claude", last + Duration::from_secs(179))
                > last + Duration::from_secs(179)
        );
        assert_eq!(
            slot.poll_due_at("claude", last + Duration::from_secs(180)),
            last + Duration::from_secs(180)
        );
        tokio::time::advance(Duration::from_secs(179)).await;
        assert_eq!(registry.observe_calls(vec![observation(STAMP)]).accepted, 1);
        slot = registry.store.lock().unwrap().get(&key).unwrap().clone();
        let (due, _) = crate::select_due_round_robin(
            &registry.providers,
            vec![(key.clone(), slot.clone())],
            last + Duration::from_secs(180),
            None,
        );
        assert!(
            due.is_empty(),
            "recent calls must defer real scheduler admission"
        );
        assert_eq!(
            slot.poll_due_at("claude", last + Duration::from_secs(180)),
            last + Duration::from_secs(900),
            "recent call readings must defer the poll to its fifteen-minute backstop"
        );
        assert_eq!(
            slot.poll_due_at("claude", last + Duration::from_secs(900)),
            last + Duration::from_secs(180)
        );
        assert_eq!(
            slot.poll_due_at("claude", last + Duration::from_secs(359)),
            last + Duration::from_secs(180)
        );
        let (due, _) = crate::select_due_round_robin(
            &registry.providers,
            vec![(key.clone(), slot.clone())],
            last + Duration::from_secs(359),
            None,
        );
        assert_eq!(
            due.len(),
            1,
            "call silence must resume real scheduler admission"
        );
        assert_eq!(
            slot.poll_due_at("codex", last + Duration::from_secs(180)),
            slot.next_due_at
        );
        let other = refresh::next_slot_after_attempt(
            &refresh::ProviderSlot::due_now(last, slot.incarnation),
            "codex",
            FetchAttempt::success(None, "api", Usage::default()),
            last,
            last,
        );
        assert_eq!(other.next_due_at, last + refresh::BASE_INTERVAL);
        slot.last_failure_class = Some(refresh::FetchClass::Transient);
        slot.next_due_at = last + Duration::from_secs(600);
        assert_eq!(
            slot.poll_due_at("claude", last + Duration::from_secs(180)),
            slot.next_due_at
        );
        slot.next_due_at = last;
        assert_eq!(
            slot.poll_due_at("claude", last + Duration::from_secs(180)),
            last
        );
        let cold = refresh::ProviderSlot::due_now(Instant::now(), slot.incarnation);
        assert_eq!(cold.poll_due_at("claude", last), cold.next_due_at);
    }
}
