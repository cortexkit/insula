use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::codex_resets::{
    evaluate_trigger, normalize_credits, reporting_eligible, response_now, AuthFailureContext,
    ConsumeOutcome, CreditsHttpResponse, RedemptionJournal, ReqwestResetTransport, Reservation,
    ResetCoordinator, ResetRequest, ResetTickInput, ResetTransport, TriggerInput, UsageFacts,
};
use crate::credential_source::{CredentialSource, VaultCapability, VaultCredential, VaultGetError};
use crate::model::{ExtraWindow, RateWindow, Usage};
use crate::provider::{AccountObservation, FetchError, HandlesError};
use crate::refresh::{BASE_INTERVAL, FRESH_HORIZON};
use crate::vault_handles::VaultHandleLoader;

fn handle(id: &str) -> CredentialHandle {
    CredentialHandle::new(id)
}

fn observed(account: Option<&str>) -> Option<AccountObservation> {
    Some(AccountObservation::new(
        account.map(ToString::to_string),
        None,
    ))
}

async fn tick(registry: &Registry) {
    registry.refresh_tick(&CancellationToken::new()).await;
}

fn force_due(registry: &Registry, provider: &str) {
    let mut store = registry.store.lock().unwrap();
    for (key, mut slot) in store.snapshot() {
        if key.provider != provider {
            continue;
        }
        slot.next_due_at = Instant::now();
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }
}

fn slot(registry: &Registry, provider: &str, handle_id: &str) -> ProviderSlot {
    let key = SlotKey::new(provider, handle(handle_id));
    registry.store.lock().unwrap().get(&key).unwrap().clone()
}

struct StubProvider {
    name: &'static str,
    cookie: bool,
    ok: bool,
}

#[async_trait]
impl UsageProvider for StubProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn is_cookie_based(&self) -> bool {
        self.cookie
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.ok {
            FetchAttempt::success(None, "test", Usage::default())
        } else {
            FetchAttempt::failure(None, None, FetchError::NoSession("stub degraded".into()))
        }
    }
}

fn registry(specs: &[(&'static str, bool, bool)]) -> Registry {
    Registry::new(
        specs
            .iter()
            .map(|&(name, cookie, ok)| {
                Box::new(StubProvider { name, cookie, ok }) as Box<dyn UsageProvider>
            })
            .collect(),
    )
}

struct BlockingProvider {
    name: &'static str,
    started: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for BlockingProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.started.notify_one();
        self.gate.notified().await;
        FetchAttempt::success(None, "test", Usage::default())
    }
}

struct RelaxThenBlockProvider {
    calls: AtomicUsize,
    started: Arc<Notify>,
    gate: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for RelaxThenBlockProvider {
    fn name(&self) -> &str {
        "codex"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return FetchAttempt::success(None, "test", full_usage(47.0)).with_relax_eligible(true);
        }
        self.started.notify_one();
        self.gate.notified().await;
        FetchAttempt::success(None, "test", full_usage(99.0))
    }
}

#[tokio::test]
async fn f4_in_flight_refresh_revokes_previous_relaxation() {
    let started = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(RelaxThenBlockProvider {
        calls: AtomicUsize::new(0),
        started: Arc::clone(&started),
        gate: Arc::clone(&gate),
    })]));
    tick(&registry).await;
    assert_eq!(primary_percent(&registry.get_usage(None).await[0]), 0.0);

    force_due(&registry, "codex");
    let refresh = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    started.notified().await;
    assert_eq!(
        primary_percent(&registry.get_usage(None).await[0]),
        47.0,
        "the raw stored value must be visible while a newer fetch is in flight"
    );
    gate.notify_one();
    refresh.await.unwrap();
}

/// A cookie provider that is not logged in and one whose live cookie was
/// rejected are different facts, and only the second is worth acting on.
///
/// The cohort count exists to say "a browser login went stale". If an absent
/// cookie counted too, the number would sit at the cohort size forever on any
/// host that does not use every one of these services -- which is every host --
/// and a real stale login would move it from seven to eight.
#[tokio::test]
async fn only_a_failed_cookie_counts_as_a_stale_login() {
    struct CookieProvider {
        name: &'static str,
        error: fn() -> FetchError,
    }

    #[async_trait]
    impl UsageProvider for CookieProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn is_cookie_based(&self) -> bool {
            true
        }
        async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
            FetchAttempt::failure(None, None, (self.error)())
        }
    }

    let registry = Registry::new(vec![
        Box::new(CookieProvider {
            name: "not-logged-in",
            error: || FetchError::NoSession("no session cookie in browser".into()),
        }),
        Box::new(CookieProvider {
            name: "login-expired",
            error: || FetchError::Unauthorized("HTTP 401".into()),
        }),
        // The site is down. A visitor with a perfectly good session sees the
        // same failure, so re-authenticating would change nothing.
        Box::new(CookieProvider {
            name: "site-erroring",
            error: || FetchError::Upstream("HTTP 500".into()),
        }),
        // The credential works and the account has no plan to report on.
        Box::new(CookieProvider {
            name: "no-plan",
            error: || FetchError::NoQuotaReported("no active quota".into()),
        }),
        // Our own defect. Reporting it as a credential problem would send the
        // reader to re-authenticate an account that is fine.
        Box::new(CookieProvider {
            name: "our-bug",
            error: || FetchError::Internal("provider fetch panicked".into()),
        }),
        // A scraped page that answered 200 without usage data. Several cookie
        // providers have no explicit signed-out detection, so this is what their
        // expired session actually looks like.
        Box::new(CookieProvider {
            name: "page-unparseable",
            error: || FetchError::Decode("no usage windows in response".into()),
        }),
    ]);
    tick(&registry).await;

    let health = registry.health();
    // Every one is degraded: none is serving a window.
    assert_eq!(
        health.degraded,
        vec![
            "not-logged-in",
            "login-expired",
            "site-erroring",
            "no-plan",
            "our-bug",
            "page-unparseable"
        ]
    );
    // Only the two that mean a stored login stopped working. Asserted as the
    // whole list rather than by membership, so a class wrongly joining the count
    // fails here instead of passing unnoticed.
    assert_eq!(
        health.cookie_cohort_degraded,
        vec!["login-expired", "page-unparseable"]
    );
    assert_eq!(health.cookie_cohort_total, 6);
}

#[tokio::test]
async fn health_reflects_provider_outcomes() {
    let registry = registry(&[
        ("codex", false, true),
        ("cursor", true, false),
        ("amp", true, true),
        ("elevenlabs", false, false),
    ]);
    assert!(registry.health().degraded.is_empty());

    tick(&registry).await;

    let health = registry.health();
    assert_eq!(health.fresh, 2);
    assert_eq!(health.degraded, vec!["cursor", "elevenlabs"]);
    // Both failures here are absent credentials, so neither is a stale-login
    // signal: a cookie provider nobody has logged into is behaving correctly.
    // The cohort count is exercised with a real failure in
    // `only_a_failed_cookie_counts_as_a_stale_login`.
    assert!(health.cookie_cohort_degraded.is_empty());
    assert!(health.last_tick_age.is_some());
}

#[tokio::test]
async fn get_usage_preserves_registry_order() {
    let registry = registry(&[
        ("codex", false, true),
        ("cursor", true, false),
        ("amp", true, true),
    ]);
    assert!(registry.get_usage(None).await.is_empty());

    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    let names: Vec<_> = usage.iter().map(|entry| entry.provider.as_str()).collect();
    assert_eq!(names, vec!["codex", "cursor", "amp"]);
    assert!(usage[0].error.is_none());
    assert!(usage[1].error.is_some());
    assert!(usage[2].error.is_none());
}

#[tokio::test]
async fn read_never_blocks_on_an_inflight_fetch() {
    let started = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(BlockingProvider {
        name: "codex",
        started: Arc::clone(&started),
        gate: Arc::clone(&gate),
    })]));
    let running = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    started.notified().await;

    let usage = tokio::time::timeout(Duration::from_millis(200), registry.get_usage(None))
        .await
        .expect("cache-only read waited for an in-flight fetch");
    assert!(usage.is_empty());

    gate.notify_one();
    running.await.unwrap();
}

#[test]
fn every_api_provider_key_names_a_registered_provider() {
    // `apiProvider` is a claim about identity, not a label: consumers join
    // pricing and spend data on it. The mapping is keyed by this module's own
    // provider name, so renaming a provider silently strips the canonical slug
    // from its wire entries -- the stale key simply stops matching and the
    // lookup returns None, which is also what "no counterpart exists" looks
    // like. Nothing else would fail.
    //
    // Checking every key resolves to a registered provider makes that drift
    // visible at the moment of the rename.
    let registry = Registry::with_defaults(crate::config::QuotaConfig::default(), None);
    let registered: std::collections::HashSet<&str> =
        registry.provider_names().into_iter().collect();

    let source = include_str!("lib.rs");
    let start = source
        .find("fn api_provider_name")
        .expect("api_provider_name must exist");
    let body_end = source[start..]
        .find("\n}")
        .expect("api_provider_name must have a body");
    let body = &source[start..start + body_end];

    let mut keys = Vec::new();
    for line in body.lines() {
        let Some((left, right)) = line.split_once("=> Some(") else {
            continue;
        };
        let Some(key) = left.split('"').nth(1) else {
            continue;
        };
        let value = right.split('"').nth(1).unwrap_or_default().to_string();
        keys.push((key.to_string(), value));
    }

    assert!(
        keys.len() >= 15,
        "parsed too few mapping keys ({}), the extraction is broken rather than the map",
        keys.len()
    );

    let unknown: Vec<&str> = keys
        .iter()
        .map(|(key, _)| key.as_str())
        .filter(|key| !registered.contains(key))
        .collect();
    assert!(
        unknown.is_empty(),
        "api_provider_name maps provider names that are not registered: {unknown:?} -- \
         a provider was renamed and its canonical slug silently stopped reaching the wire"
    );

    for (key, value) in &keys {
        assert!(
            !value.is_empty(),
            "provider {key} maps to an empty canonical name"
        );
    }
}

struct LabelProvider {
    labels: Arc<Mutex<HashMap<String, Option<String>>>>,
}

#[async_trait]
impl UsageProvider for LabelProvider {
    fn name(&self) -> &str {
        "multi"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![handle("H1"), handle("H2")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let account = self.labels.lock().unwrap()[handle.stable_id()].clone();
        FetchAttempt::success(
            Some(AccountObservation::new(account, None)),
            "test",
            Usage::default(),
        )
    }
}

#[tokio::test]
async fn unresolved_multi_handle_provider_emits_one_unlabeled_entry_then_deduplicates() {
    let labels = Arc::new(Mutex::new(HashMap::from([
        ("H1".to_string(), None),
        ("H2".to_string(), None),
    ])));
    let registry = Registry::new(vec![Box::new(LabelProvider {
        labels: Arc::clone(&labels),
    })]);

    tick(&registry).await;
    let unresolved = registry.get_usage(None).await;
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].account, None);

    *labels.lock().unwrap() = HashMap::from([
        ("H1".to_string(), Some("A".to_string())),
        ("H2".to_string(), Some("B".to_string())),
    ]);
    force_due(&registry, "multi");
    tick(&registry).await;
    let labeled = registry.get_usage(None).await;
    assert_eq!(
        labeled
            .iter()
            .map(|entry| entry.account.as_deref())
            .collect::<Vec<_>>(),
        vec![Some("A"), Some("B")]
    );

    labels.lock().unwrap().insert("H2".into(), Some("A".into()));
    force_due(&registry, "multi");
    tick(&registry).await;
    let deduplicated = registry.get_usage(None).await;
    assert_eq!(deduplicated.len(), 1);
    assert_eq!(deduplicated[0].account.as_deref(), Some("A"));
}

#[tokio::test]
async fn one_identity_less_handle_suppresses_the_labels_of_every_other_handle() {
    // The mixed case: one handle resolves an account and the other cannot. The
    // read path emits labeled entries only when EVERY handle resolves one, so a
    // single identity-less lane collapses the whole provider to one unlabeled
    // entry and the account that WAS resolved becomes invisible.
    //
    // The shape that reaches this state: a provider whose vault lanes carry
    // identity beside a local lane that can never resolve one. It is invisible
    // while both lanes are identity-less, and appears the moment the credential
    // store starts capturing identity for that provider — without any change
    // here.
    let labels = Arc::new(Mutex::new(HashMap::from([
        ("H1".to_string(), Some("A".to_string())),
        ("H2".to_string(), None),
    ])));
    let registry = Registry::new(vec![Box::new(LabelProvider {
        labels: Arc::clone(&labels),
    })]);

    tick(&registry).await;
    let mixed = registry.get_usage(None).await;
    assert_eq!(mixed.len(), 1, "mixed resolution must not emit two entries");
    assert_eq!(
        mixed[0].account, None,
        "one identity-less handle forces the whole provider unlabeled"
    );

    // Remove the identity-less lane and the resolved account becomes visible,
    // which is what proves the suppression above was caused by that lane rather
    // than by the label never being resolved at all.
    labels.lock().unwrap().insert("H2".into(), Some("A".into()));
    force_due(&registry, "multi");
    tick(&registry).await;
    let labeled = registry.get_usage(None).await;
    assert_eq!(labeled.len(), 1);
    assert_eq!(labeled[0].account.as_deref(), Some("A"));
}

struct UnresolvedSelectionProvider;

#[async_trait]
impl UsageProvider for UnresolvedSelectionProvider {
    fn name(&self) -> &str {
        "selection"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![
            CredentialHandle::implicit(),
            CredentialHandle::vault(
                "chatgpt:openai",
                crate::credential_source::VaultCapability::new("ckh_selection_secret"),
            ),
        ])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_local() {
            FetchAttempt::success(observed(None), "local-primary", Usage::default())
        } else {
            FetchAttempt::failure(
                None,
                None,
                FetchError::NoSession("vault credential not found".to_string()),
            )
        }
    }
}

#[tokio::test]
async fn unresolved_selection_prefers_fresh_local_over_cold_vault() {
    assert!(
        CredentialHandle::vault(
            "chatgpt:openai",
            crate::credential_source::VaultCapability::new("ckh_sort_proof"),
        )
        .sort_cmp(&CredentialHandle::implicit())
            == std::cmp::Ordering::Less,
        "the regression must put the degraded vault slot first in stable order"
    );
    let registry = Registry::new(vec![Box::new(UnresolvedSelectionProvider)]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].source.as_deref(), Some("local-primary"));
    assert!(usage[0].error.is_none());
}

#[tokio::test]
async fn i6_pending_local_does_not_hide_degraded_vault_entry() {
    let registry = Registry::new(vec![Box::new(UnresolvedSelectionProvider)]);
    tick(&registry).await;

    let local_key = SlotKey::new("selection", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut local = store.get(&local_key).unwrap().clone();
        local.entry = None;
        local.status = refresh::SlotStatus::Pending;
        assert!(store.publish_if_current(
            &local_key,
            local.incarnation,
            local.attempt_sequence,
            local,
        ));
    }

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert!(usage[0].error.is_some(), "degraded vault entry was hidden");
}

struct SwapFailureProvider {
    calls: Mutex<usize>,
}

#[async_trait]
impl UsageProvider for SwapFailureProvider {
    fn name(&self) -> &str {
        "swap"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            FetchAttempt::failure(
                observed(Some("B")),
                Some("test".into()),
                FetchError::Upstream("timeout".into()),
            )
        }
    }
}

#[tokio::test]
async fn account_change_observed_on_failure_clears_old_window_and_restarts_backoff() {
    let registry = Registry::new(vec![Box::new(SwapFailureProvider {
        calls: Mutex::new(0),
    })]);
    tick(&registry).await;
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("A")
    );

    force_due(&registry, "swap");
    tick(&registry).await;

    assert!(registry.get_usage(None).await.is_empty());
    let slot = slot(&registry, "swap", "implicit-local");
    assert!(slot.entry.is_none());
    assert!(slot.last_success_at.is_none());
    assert!(slot.label_in_flux);
    assert_eq!(slot.account_id(), Some("B"));
    assert_eq!(slot.retry_count, 1);
    assert!(slot.next_due_at >= slot.last_attempt_at.unwrap() + BASE_INTERVAL);
}

#[tokio::test]
async fn label_in_flux_is_unavailable_even_if_a_stale_entry_exists() {
    let registry = Registry::new(vec![Box::new(StubProvider {
        name: "flux",
        cookie: false,
        ok: true,
    })]);
    tick(&registry).await;
    assert_eq!(registry.get_usage(None).await.len(), 1);

    let key = SlotKey::new("flux", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut slot = store.get(&key).unwrap().clone();
        assert!(slot.entry.is_some());
        slot.label_in_flux = true;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }

    assert!(registry.get_usage(None).await.is_empty());
}

struct FencedProvider {
    handles: Arc<Mutex<Vec<CredentialHandle>>>,
    calls: AtomicUsize,
    started: [Arc<Notify>; 2],
    gates: [Arc<Notify>; 2],
}

#[async_trait]
impl UsageProvider for FencedProvider {
    fn name(&self) -> &str {
        "fenced"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(self.handles.lock().unwrap().clone())
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.started[call].notify_one();
        self.gates[call].notified().await;
        let account = if call == 0 { "OLD" } else { "NEW" };
        FetchAttempt::success(observed(Some(account)), "test", Usage::default())
    }
}

#[tokio::test]
async fn late_fetch_cannot_resurrect_a_reaped_or_readded_handle() {
    let handles = Arc::new(Mutex::new(vec![handle("H")]));
    let old_started = Arc::new(Notify::new());
    let new_started = Arc::new(Notify::new());
    let old_gate = Arc::new(Notify::new());
    let new_gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(FencedProvider {
        handles: Arc::clone(&handles),
        calls: AtomicUsize::new(0),
        started: [Arc::clone(&old_started), Arc::clone(&new_started)],
        gates: [Arc::clone(&old_gate), Arc::clone(&new_gate)],
    })]));

    let old_tick = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    old_started.notified().await;
    let old_incarnation = slot(&registry, "fenced", "H").incarnation;

    handles.lock().unwrap().clear();
    tick(&registry).await;
    assert!(registry.store.lock().unwrap().snapshot().is_empty());

    handles.lock().unwrap().push(handle("H"));
    let new_tick = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    new_started.notified().await;
    let new_incarnation = slot(&registry, "fenced", "H").incarnation;
    assert_ne!(old_incarnation, new_incarnation);

    old_gate.notify_one();
    old_tick.await.unwrap();
    assert!(registry.get_usage(None).await.is_empty());

    new_gate.notify_one();
    new_tick.await.unwrap();
    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].account.as_deref(), Some("NEW"));
}

struct EnumerationProvider {
    handles: Arc<Mutex<Vec<CredentialHandle>>>,
    fail_enumeration: Arc<Mutex<bool>>,
    calls: Arc<Mutex<HashMap<String, usize>>>,
}

#[async_trait]
impl UsageProvider for EnumerationProvider {
    fn name(&self) -> &str {
        "enumerated"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if *self.fail_enumeration.lock().unwrap() {
            Err(HandlesError::new("temporary parse failure"))
        } else {
            Ok(self.handles.lock().unwrap().clone())
        }
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        *self
            .calls
            .lock()
            .unwrap()
            .entry(handle.stable_id().to_string())
            .or_default() += 1;
        FetchAttempt::success(observed(Some(handle.stable_id())), "test", Usage::default())
    }
}

#[tokio::test]
async fn enumeration_error_retains_active_set_and_new_handle_is_immediately_due() {
    let handles = Arc::new(Mutex::new(vec![handle("H1")]));
    let fail_enumeration = Arc::new(Mutex::new(false));
    let calls = Arc::new(Mutex::new(HashMap::new()));
    let registry = Registry::new(vec![Box::new(EnumerationProvider {
        handles: Arc::clone(&handles),
        fail_enumeration: Arc::clone(&fail_enumeration),
        calls: Arc::clone(&calls),
    })]);
    tick(&registry).await;

    *fail_enumeration.lock().unwrap() = true;
    handles.lock().unwrap().clear();
    tick(&registry).await;
    assert_eq!(registry.get_usage(None).await.len(), 1);
    assert_eq!(calls.lock().unwrap()["H1"], 1);

    *fail_enumeration.lock().unwrap() = false;
    *handles.lock().unwrap() = vec![handle("H1"), handle("H2")];
    tick(&registry).await;
    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 2);
    assert_eq!(calls.lock().unwrap()["H1"], 1);
    assert_eq!(calls.lock().unwrap()["H2"], 1);
}

struct SlowManyProvider {
    gate: Arc<Semaphore>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UsageProvider for SlowManyProvider {
    fn name(&self) -> &str {
        "slow"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok((0..12)
            .map(|index| handle(&format!("H{index:02}")))
            .collect())
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let permit = self.gate.acquire().await.unwrap();
        permit.forget();
        FetchAttempt::success(None, "test", Usage::default())
    }
}

struct QuickProvider {
    called: Arc<Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UsageProvider for QuickProvider {
    fn name(&self) -> &str {
        "quick"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.called.notify_one();
        FetchAttempt::success(None, "test", Usage::default())
    }
}

#[tokio::test]
async fn round_robin_admission_and_turn_heartbeat_prevent_large_provider_starvation() {
    let gate = Arc::new(Semaphore::new(0));
    let slow_calls = Arc::new(AtomicUsize::new(0));
    let quick_calls = Arc::new(AtomicUsize::new(0));
    let quick_called = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![
        Box::new(SlowManyProvider {
            gate: Arc::clone(&gate),
            calls: Arc::clone(&slow_calls),
        }),
        Box::new(QuickProvider {
            called: Arc::clone(&quick_called),
            calls: Arc::clone(&quick_calls),
        }),
    ]));
    registry
        .store
        .lock()
        .unwrap()
        .mark_tick(Instant::now() - STALL_HORIZON - Duration::from_secs(1));

    let running = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    tokio::time::timeout(Duration::from_millis(200), quick_called.notified())
        .await
        .expect("the later provider was starved behind slow handles");
    assert_eq!(quick_calls.load(Ordering::SeqCst), 1);
    assert!(slow_calls.load(Ordering::SeqCst) < refresh::CONCURRENCY_CAP);
    assert!(!registry.health().refresher_stalled);

    gate.add_permits(refresh::CONCURRENCY_CAP);
    running.await.unwrap();
}

struct MixedHealthProvider;

#[async_trait]
impl UsageProvider for MixedHealthProvider {
    fn name(&self) -> &str {
        "mixed"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![handle("healthy"), handle("degraded")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.stable_id() == "healthy" {
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            FetchAttempt::failure(
                observed(Some("B")),
                None,
                FetchError::NoSession("logged out".into()),
            )
        }
    }
}

#[tokio::test]
async fn one_healthy_handle_prevents_provider_wide_degradation() {
    let registry = Registry::new(vec![Box::new(MixedHealthProvider)]);
    tick(&registry).await;

    let health = registry.health();
    assert_eq!(health.fresh, 1);
    assert!(health.degraded.is_empty());
}

struct FlipProvider {
    calls: Mutex<usize>,
}

#[async_trait]
impl UsageProvider for FlipProvider {
    fn name(&self) -> &str {
        "codex"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FetchAttempt::success(None, "test", Usage::default())
        } else {
            FetchAttempt::failure(None, None, FetchError::Upstream("503".into()))
        }
    }
}

/// A provider whose first fetch succeeds and whose later fetches return whatever
/// the real normalizer produces for an EMPTY response body. This drives the
/// classification through the actual refresher state machine rather than
/// asserting `classify()`'s return value, which would pass trivially.
struct EmptyBodyProvider {
    name: &'static str,
    calls: Mutex<usize>,
    empty_body_error: fn() -> FetchError,
}

#[async_trait]
impl UsageProvider for EmptyBodyProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            let usage = Usage {
                primary: Some(RateWindow {
                    used_percent: 42.0,
                    raw_used_percent: None,
                    resets_at: Some("2026-08-01T00:00:00Z".to_string()),
                    window_minutes: Some(300),
                    used_count: None,
                    total_count: None,
                }),
                ..Usage::default()
            };
            FetchAttempt::success(None, "test", usage)
        } else {
            FetchAttempt::failure(None, None, (self.empty_body_error)())
        }
    }
}

/// An empty response body must not discard a known-good window.
///
/// This asserts the OBSERVABLE stale-serving behavior through a real refresh
/// cycle: after a healthy fetch, a fetch whose body is empty must keep serving
/// the last-healthy 42% window (status stale) instead of replacing it with a
/// degraded entry. Reverting either normalizer's empty-body branch to
/// `FetchError::Decode` makes this fail, because `classify` routes Decode to
/// NonTransient, which replaces the entry and drops the window.
#[tokio::test]
async fn empty_response_body_keeps_serving_the_last_healthy_window() {
    for (name, empty_body_error) in [
        (
            "alibaba",
            (|| crate::alibaba::normalize_usage(b"").expect_err("empty body must error"))
                as fn() -> FetchError,
        ),
        ("qwen-cloud", || {
            crate::qwen_cloud::normalize_usage(br#"{"successResponse":true}"#)
                .expect_err("empty envelope must error")
        }),
        ("grok", || {
            crate::grok::normalize_usage(b"").expect_err("empty frames must error")
        }),
    ] {
        let registry = Registry::new(vec![Box::new(EmptyBodyProvider {
            name,
            calls: Mutex::new(0),
            empty_body_error,
        })]);
        tick(&registry).await;

        let healthy = registry.get_usage(None).await;
        assert_eq!(healthy.len(), 1, "{name}: expected one entry after success");
        assert_eq!(
            healthy[0]
                .usage
                .as_ref()
                .and_then(|usage| usage.primary.as_ref())
                .map(|window| window.used_percent),
            Some(42.0),
            "{name}: first fetch should serve the real window"
        );

        force_due(&registry, name);
        tick(&registry).await;

        let after_empty = registry.get_usage(None).await;
        assert_eq!(after_empty.len(), 1, "{name}: expected one entry");
        assert!(
            after_empty[0].error.is_none(),
            "{name}: an empty body must not produce a degraded entry, got {:?}",
            after_empty[0].error
        );
        assert_eq!(
            after_empty[0]
                .usage
                .as_ref()
                .and_then(|usage| usage.primary.as_ref())
                .map(|window| window.used_percent),
            Some(42.0),
            "{name}: the last-healthy window must still be served"
        );
        assert_eq!(
            registry.health().stale,
            1,
            "{name}: the slot should be stale-serving, not degraded"
        );
    }
}

/// The boundary that makes the rule above safe: a parse failure on a NON-EMPTY
/// body stays non-transient and DOES degrade, because real bytes that will not
/// parse may be a permanent contract break.
#[tokio::test]
async fn malformed_non_empty_body_still_degrades() {
    let registry = Registry::new(vec![Box::new(EmptyBodyProvider {
        name: "alibaba",
        calls: Mutex::new(0),
        empty_body_error: || {
            crate::alibaba::normalize_usage(b"{not json").expect_err("garbage must error")
        },
    })]);
    tick(&registry).await;
    force_due(&registry, "alibaba");
    tick(&registry).await;

    let after_garbage = registry.get_usage(None).await;
    assert_eq!(after_garbage.len(), 1);
    assert!(
        after_garbage[0].error.is_some(),
        "a malformed non-empty body must degrade, not stale-serve"
    );
    assert_eq!(registry.health().stale, 0);
}

#[tokio::test]
async fn transient_failure_keeps_serving_last_good_window() {
    let registry = Registry::new(vec![Box::new(FlipProvider {
        calls: Mutex::new(0),
    })]);
    tick(&registry).await;
    force_due(&registry, "codex");
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert!(usage[0].error.is_none());
    assert_eq!(registry.health().stale, 1);
}

#[test]
fn poisoned_store_reports_failing() {
    let registry = registry(&[("codex", false, true), ("cursor", true, true)]);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = registry.store.lock().unwrap();
        panic!("poison the slot store mutex");
    }));
    assert!(registry.health().is_failing());
}

#[test]
fn stalled_refresher_reports_degraded() {
    let registry = registry(&[("codex", false, true)]);
    registry
        .store
        .lock()
        .unwrap()
        .mark_tick(Instant::now() - STALL_HORIZON - Duration::from_secs(1));
    assert!(registry.health().is_degraded());
}

struct PanicProvider;

#[async_trait]
impl UsageProvider for PanicProvider {
    fn name(&self) -> &str {
        "boom"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        panic!("provider blew up mid-fetch");
    }
}

#[tokio::test]
async fn panicking_provider_is_contained_and_other_provider_resolves() {
    let registry = Registry::new(vec![
        Box::new(PanicProvider),
        Box::new(StubProvider {
            name: "codex",
            cookie: false,
            ok: true,
        }),
    ]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert!(usage
        .iter()
        .find(|entry| entry.provider == "boom")
        .expect("the panicking provider still produces an entry")
        .error
        .is_some());
    assert!(usage
        .iter()
        .find(|entry| entry.provider == "codex")
        .unwrap()
        .error
        .is_none());
}

/// A panic is this module's defect, and the published entry must say so.
///
/// Reporting it as a decode failure would state that the upstream sent a
/// payload we could not parse, sending anyone who acts on it to the provider's
/// API rather than to this codebase.
///
/// Separate from the containment test above even though the setup is
/// identical. That test is named for containment, so a later pass narrowing it
/// to its stated subject would delete this assertion without anything looking
/// wrong -- and an audit listing test names would report attribution as
/// untested while a mutation run reported it as covered.
#[tokio::test]
async fn a_contained_panic_is_attributed_to_this_module_not_the_upstream() {
    let registry = Registry::new(vec![Box::new(PanicProvider)]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    let text = usage[0]
        .error
        .as_deref()
        .expect("a contained panic still publishes an entry");
    assert!(
        text.starts_with("internal error:"),
        "panic attributed as {text:?}"
    );
    // Not vacuous: it is the panic being described, not some other failure that
    // happens to carry the same prefix.
    assert!(text.contains("panicked"), "unexpected: {text:?}");
}

#[tokio::test]
async fn cancelled_tick_leaves_slots_pending_without_hanging() {
    let registry = Registry::new(vec![Box::new(BlockingProvider {
        name: "codex",
        started: Arc::new(Notify::new()),
        gate: Arc::new(Notify::new()),
    })]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    tokio::time::timeout(Duration::from_millis(200), registry.refresh_tick(&cancel))
        .await
        .expect("cancelled turn did not return promptly");
    assert!(registry.get_usage(None).await.is_empty());
    assert_eq!(
        slot(&registry, "codex", "implicit-local").status,
        SlotStatus::Pending
    );
}

struct OuterTimeoutSwapProvider {
    calls: AtomicUsize,
    current_account: Arc<Mutex<&'static str>>,
}

#[async_trait]
impl UsageProvider for OuterTimeoutSwapProvider {
    fn name(&self) -> &str {
        "outer-timeout"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            *self.current_account.lock().unwrap() = "B";
            std::future::pending::<()>().await;
            unreachable!("the scheduler deadline must terminate this attempt")
        }
    }
}

/// A fetch that outruns the scheduler deadline is reported as an upstream
/// failure, and that attribution is load-bearing rather than cosmetic: it is
/// the only transient class among the three synthetic failures, so a slow
/// provider keeps serving its last healthy window instead of losing it.
///
/// Calling this our own defect -- as a panic correctly is -- would make it
/// non-transient, and a provider that merely answered slowly would have its
/// cached window replaced by a degraded entry.
#[tokio::test]
async fn a_deadline_overrun_is_attributed_to_the_upstream_and_stays_transient() {
    let registry = Registry::new(vec![Box::new(BlockingProvider {
        name: "codex",
        started: Arc::new(Notify::new()),
        gate: Arc::new(Notify::new()),
    })]);

    registry
        .refresh_tick_with_deadline(&CancellationToken::new(), Duration::from_millis(20))
        .await;

    let entry = &registry.get_usage(None).await[0];
    let text = entry
        .error
        .as_deref()
        .expect("the overrun degrades the slot");
    assert!(
        text.starts_with("upstream error:"),
        "deadline overrun attributed as {text:?}"
    );
    // Not vacuous: it is the overrun being described, not some other failure.
    assert!(text.contains("deadline"), "unexpected: {text:?}");

    // The consequence the attribution buys, asserted rather than implied.
    assert_eq!(
        crate::refresh::classify(&crate::provider::FetchError::Upstream(String::new())),
        crate::refresh::FetchClass::Transient,
    );
}

#[tokio::test]
async fn outer_timeout_after_credential_swap_fails_closed() {
    let current_account = Arc::new(Mutex::new("A"));
    let provider = OuterTimeoutSwapProvider {
        calls: AtomicUsize::new(0),
        current_account: Arc::clone(&current_account),
    };
    let registry = Registry::new(vec![Box::new(provider)]);
    tick(&registry).await;
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("A")
    );

    force_due(&registry, "outer-timeout");
    registry
        .refresh_tick_with_deadline(&CancellationToken::new(), Duration::from_millis(20))
        .await;

    assert_eq!(*current_account.lock().unwrap(), "B");
    assert!(registry.get_usage(None).await.is_empty());
    let slot = slot(&registry, "outer-timeout", "implicit-local");
    assert!(slot.label_in_flux);
    assert!(slot.entry.is_none());
    assert!(slot.last_success_at.is_none());
    assert_eq!(slot.retry_count, 1);
}

struct PanickingNameProvider;

#[async_trait]
impl UsageProvider for PanickingNameProvider {
    fn name(&self) -> &str {
        panic!("provider name lookup panicked")
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(None, "test", Usage::default())
    }
}

#[tokio::test]
async fn panicking_provider_name_is_contained_at_registration() {
    let registry = Registry::new(vec![Box::new(PanickingNameProvider)]);
    assert_eq!(registry.provider_names(), vec!["invalid-provider-0"]);

    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].provider, "invalid-provider-0");
}

struct DropBomb;

impl Drop for DropBomb {
    fn drop(&mut self) {
        panic!("provider future panicked while being dropped")
    }
}

struct DropPanicProvider {
    started: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for DropPanicProvider {
    fn name(&self) -> &str {
        "drop-panic"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let _bomb = DropBomb;
        self.started.notify_one();
        std::future::pending::<()>().await;
        unreachable!("cancellation must terminate this attempt")
    }
}

#[tokio::test]
async fn cancellation_contains_a_provider_future_drop_panic() {
    let started = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(DropPanicProvider {
        started: Arc::clone(&started),
    })]));
    let cancel = CancellationToken::new();
    let running = {
        let registry = Arc::clone(&registry);
        let cancel = cancel.clone();
        tokio::spawn(async move { registry.refresh_tick(&cancel).await })
    };
    started.notified().await;
    cancel.cancel();

    running
        .await
        .expect("provider teardown panic escaped the supervised task");
    assert_eq!(
        slot(&registry, "drop-panic", "implicit-local").status,
        SlotStatus::Pending
    );
}

struct CursorProvider {
    name: String,
    handles: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl UsageProvider for CursorProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok((0..self.handles)
            .map(|index| handle(&format!("H{index}")))
            .collect())
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        self.calls.fetch_add(1, Ordering::SeqCst);
        FetchAttempt::success(None, "test", Usage::default())
    }
}

/// Every registered provider lands in exactly one health bucket, including one
/// whose first fetch has not run yet.
///
/// Consumers are told to assert `fresh + stale + pending + degraded +
/// withoutHandles == providersTotal` and alert on an imbalance, so a state that
/// breaks it without anything being wrong is worse than no invariant at all: it
/// fires after every restart and trains the reader to ignore the alert.
///
/// The unbucketed state is ordinary rather than exotic. The refresher admits a
/// bounded number of fetch units per turn, so any registry with more units than
/// that cap has providers still queued after the first tick -- which is when the
/// documented precondition (`lastTickAgeSecs` is set) says the identity holds.
#[tokio::test]
async fn every_provider_lands_in_exactly_one_health_bucket() {
    let provider_count = refresh::CONCURRENCY_CAP + 1;
    let calls: Vec<_> = (0..provider_count)
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();
    // The providers before the cap take two handles each, so the cap is spent
    // before the last provider is reached and its slots stay Pending.
    let providers: Vec<Box<dyn UsageProvider>> = (0..provider_count)
        .map(|index| {
            Box::new(CursorProvider {
                name: format!("provider-{index:02}"),
                handles: if index < refresh::CONCURRENCY_CAP {
                    2
                } else {
                    1
                },
                calls: Arc::clone(&calls[index]),
            }) as Box<dyn UsageProvider>
        })
        .collect();
    let registry = Registry::new(providers);

    tick(&registry).await;

    let health = registry.health();
    // The precondition consumers gate on: the identity is only claimed once the
    // refresher has ticked.
    assert!(health.last_tick_age.is_some());

    // Not vacuous: this is the state that breaks the identity when Pending has
    // no bucket. Without it the assertion below would pass on any all-fresh run.
    assert_eq!(
        calls[provider_count - 1].load(Ordering::SeqCst),
        0,
        "the last provider must not have been fetched yet"
    );
    assert_eq!(health.pending, 1, "the unfetched provider is pending");

    assert_eq!(
        health.fresh
            + health.stale
            + health.pending
            + health.degraded.len()
            + health.without_handles.len(),
        health.providers_total,
        "buckets: fresh {} stale {} pending {} degraded {:?} withoutHandles {:?} total {}",
        health.fresh,
        health.stale,
        health.pending,
        health.degraded,
        health.without_handles,
        health.providers_total
    );

    // And it still balances once everything has been fetched, so the fix is not
    // simply moving the imbalance to a later turn.
    tick(&registry).await;
    let health = registry.health();
    assert_eq!(health.pending, 0);
    assert_eq!(
        health.fresh
            + health.stale
            + health.pending
            + health.degraded.len()
            + health.without_handles.len(),
        health.providers_total
    );
}

#[tokio::test]
async fn persisted_round_robin_cursor_admits_provider_after_the_first_cap() {
    let provider_count = refresh::CONCURRENCY_CAP + 1;
    let calls: Vec<_> = (0..provider_count)
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();
    let providers: Vec<Box<dyn UsageProvider>> = (0..provider_count)
        .map(|index| {
            Box::new(CursorProvider {
                name: format!("provider-{index:02}"),
                handles: if index < refresh::CONCURRENCY_CAP {
                    2
                } else {
                    1
                },
                calls: Arc::clone(&calls[index]),
            }) as Box<dyn UsageProvider>
        })
        .collect();
    let registry = Registry::new(providers);

    tick(&registry).await;
    assert_eq!(calls[provider_count - 1].load(Ordering::SeqCst), 0);
    tick(&registry).await;

    assert_eq!(calls[provider_count - 1].load(Ordering::SeqCst), 1);
}

struct OverlappingAttemptProvider {
    calls: AtomicUsize,
    old_started: Arc<Notify>,
    old_gate: Arc<Notify>,
}

#[async_trait]
impl UsageProvider for OverlappingAttemptProvider {
    fn name(&self) -> &str {
        "overlap"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            self.old_started.notify_one();
            self.old_gate.notified().await;
            FetchAttempt::success(observed(Some("A")), "test", Usage::default())
        } else {
            FetchAttempt::success(observed(Some("B")), "test", Usage::default())
        }
    }
}

#[tokio::test]
async fn older_overlapping_attempt_cannot_overwrite_newer_result() {
    let old_started = Arc::new(Notify::new());
    let old_gate = Arc::new(Notify::new());
    let registry = Arc::new(Registry::new(vec![Box::new(OverlappingAttemptProvider {
        calls: AtomicUsize::new(0),
        old_started: Arc::clone(&old_started),
        old_gate: Arc::clone(&old_gate),
    })]));

    let old_tick = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.refresh_tick(&CancellationToken::new()).await })
    };
    old_started.notified().await;
    let incarnation = slot(&registry, "overlap", "implicit-local").incarnation;

    tick(&registry).await;
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("B")
    );
    assert_eq!(
        slot(&registry, "overlap", "implicit-local").incarnation,
        incarnation
    );

    old_gate.notify_one();
    old_tick.await.unwrap();
    assert_eq!(
        registry.get_usage(None).await[0].account.as_deref(),
        Some("B")
    );
}

#[test]
fn unobserved_success_cannot_close_label_flux() {
    let now = Instant::now();
    let cold = ProviderSlot::due_now(now, Incarnation::from_counter(1));
    let account_a = next_slot_after_attempt(
        &cold,
        "flux",
        FetchAttempt::success(observed(Some("A")), "test", Usage::default()),
        now,
        now,
    );
    let account_b_failed = next_slot_after_attempt(
        &account_a,
        "flux",
        FetchAttempt::failure(
            observed(Some("B")),
            Some("test".into()),
            FetchError::Upstream("timeout".into()),
        ),
        now,
        now,
    );
    assert!(account_b_failed.label_in_flux);

    let unobserved_success = next_slot_after_attempt(
        &account_b_failed,
        "flux",
        FetchAttempt::success(None, "test", Usage::default()),
        now,
        now,
    );
    assert!(unobserved_success.label_in_flux);
    assert!(unobserved_success.entry.is_none());

    let observed_success = next_slot_after_attempt(
        &unobserved_success,
        "flux",
        FetchAttempt::success(observed(Some("B")), "test", Usage::default()),
        now,
        now,
    );
    assert!(!observed_success.label_in_flux);
    assert_eq!(observed_success.account_id(), Some("B"));
}

struct DuplicateAccountProvider;

#[async_trait]
impl UsageProvider for DuplicateAccountProvider {
    fn name(&self) -> &str {
        "duplicate"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![handle("A-degraded"), handle("B-fresh")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.stable_id() == "A-degraded" {
            FetchAttempt::failure(
                observed(Some("shared-account")),
                None,
                FetchError::NoSession("logged out".into()),
            )
        } else {
            FetchAttempt::success(observed(Some("shared-account")), "test", Usage::default())
        }
    }
}

#[tokio::test]
async fn duplicate_account_prefers_fresh_handle_over_earlier_degraded_handle() {
    let registry = Registry::new(vec![Box::new(DuplicateAccountProvider)]);
    tick(&registry).await;

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].account.as_deref(), Some("shared-account"));
    assert!(usage[0].error.is_none());
    assert!(usage[0].usage.is_some());
}

#[test]
fn adapter_distinguishes_absent_label_from_unavailable_observation() {
    let attempt = FetchAttempt::from_provider_usage(Ok(ProviderUsage::healthy(
        "unlabeled",
        None,
        "test",
        Usage::default(),
    )));
    assert!(attempt.observed.is_some());
    assert_eq!(attempt.observed.unwrap().account_id, None);
}

static RESET_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

struct ResetTempDir {
    dir: std::path::PathBuf,
}

impl ResetTempDir {
    fn new(label: &str) -> Self {
        let id = RESET_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ck-quota-reset-{label}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn journal(&self) -> RedemptionJournal {
        RedemptionJournal::new(self.dir.join("redemptions.json"))
    }
}

impl Drop for ResetTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn reset_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0)
        .single()
        .unwrap()
}

fn reset_request(bearer: &str) -> ResetRequest {
    ResetRequest {
        base_url: "https://example.invalid/backend-api".to_string(),
        bearer: bearer.to_string(),
        account_id: "acct-reset".to_string(),
        auth_failure: None,
    }
}

fn reset_facts(percent: f64, at_wall: bool) -> UsageFacts {
    UsageFacts {
        raw_percents: vec![percent],
        any_used_floor: percent >= 1.0,
        at_wall,
        wall_clear: !at_wall,
    }
}

fn reset_tick_input(now: chrono::DateTime<Utc>, facts: UsageFacts) -> ResetTickInput {
    ResetTickInput {
        armed: true,
        now,
        earliest_expiry: Some(now + chrono::Duration::minutes(5)),
        auto_use_resets_secs: 10 * 60,
        facts,
        elapsed_since_attempt_start: Duration::from_secs(1),
    }
}

#[derive(Clone, Copy)]
enum MockConsumeBehavior {
    Outcome(ConsumeOutcome),
    Error,
    Status(u16),
    Hang,
    Block(ConsumeOutcome),
}

struct MockResetTransport {
    behavior: MockConsumeBehavior,
    gets: AtomicUsize,
    posts: AtomicUsize,
    consume_accounts: Mutex<Vec<String>>,
    reservation_visible: AtomicBool,
    journal_path: Option<std::path::PathBuf>,
    credits_error: bool,
    started: Semaphore,
    release: Semaphore,
}

impl MockResetTransport {
    fn new(behavior: MockConsumeBehavior) -> Self {
        Self {
            behavior,
            gets: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
            consume_accounts: Mutex::new(Vec::new()),
            reservation_visible: AtomicBool::new(false),
            journal_path: None,
            credits_error: false,
            started: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }

    fn with_journal(mut self, journal: &RedemptionJournal) -> Self {
        self.journal_path = Some(journal.path().to_path_buf());
        self
    }

    fn credits_failure(mut self) -> Self {
        self.credits_error = true;
        self
    }

    fn body(outcome: ConsumeOutcome) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "code": outcome.as_code(),
            "windows_reset": ["primary"]
        }))
        .unwrap()
    }
}

#[async_trait]
impl ResetTransport for MockResetTransport {
    async fn fetch_credits(
        &self,
        _request: &ResetRequest,
    ) -> Result<CreditsHttpResponse, FetchError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if self.credits_error {
            return Err(FetchError::Upstream("mock credits failure".into()));
        }
        Ok(CreditsHttpResponse {
            body: br#"{
                "credits": [{
                    "id": "credit-1",
                    "status": "available",
                    "expires_at": "2026-07-15T12:00:00Z"
                }],
                "available_count": 1
            }"#
            .to_vec(),
            date_header: Some("Tue, 14 Jul 2026 12:00:00 +0000".to_string()),
        })
    }

    async fn consume(
        &self,
        request: &ResetRequest,
        redeem_request_id: &str,
    ) -> Result<Vec<u8>, FetchError> {
        if let Some(path) = &self.journal_path {
            let records: Vec<crate::codex_resets::RedemptionRecord> =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            self.reservation_visible.store(
                records.iter().any(|record| {
                    record.redeem_request_id == redeem_request_id
                        && record.status == crate::codex_resets::JournalStatus::Pending
                        && record.last_attempt_at.is_some()
                        && record.attempt_count == 1
                }),
                Ordering::SeqCst,
            );
        }
        self.posts.fetch_add(1, Ordering::SeqCst);
        self.consume_accounts
            .lock()
            .unwrap()
            .push(request.account_id.clone());
        self.started.add_permits(1);
        match self.behavior {
            MockConsumeBehavior::Outcome(outcome) => Ok(Self::body(outcome)),
            MockConsumeBehavior::Error => Err(FetchError::Upstream("mock HTTP 503".into())),
            MockConsumeBehavior::Status(status) => Err(FetchError::ProviderStatus(status)),
            MockConsumeBehavior::Hang => {
                std::future::pending::<Result<Vec<u8>, FetchError>>().await
            }
            MockConsumeBehavior::Block(outcome) => {
                let permit = self.release.acquire().await.unwrap();
                permit.forget();
                Ok(Self::body(outcome))
            }
        }
    }
}

#[derive(Default)]
struct MockReportingCredentialSource {
    reports: Mutex<Vec<(u16, u64)>>,
}

#[async_trait]
impl CredentialSource for MockReportingCredentialSource {
    async fn get(
        &self,
        _capability: &VaultCapability,
        _min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError> {
        Err(VaultGetError::Permanent)
    }

    async fn report_auth_failure(
        &self,
        _capability: &VaultCapability,
        provider_status: u16,
        record_version: u64,
    ) {
        self.reports
            .lock()
            .unwrap()
            .push((provider_status, record_version));
    }
}

fn reporting_reset_request(
    source: Arc<MockReportingCredentialSource>,
    secret: &str,
) -> ResetRequest {
    let source: Arc<dyn CredentialSource> = source;
    ResetRequest {
        base_url: "https://example.invalid/backend-api".to_string(),
        bearer: secret.to_string(),
        account_id: "acct-reset".to_string(),
        auth_failure: Some(AuthFailureContext {
            source,
            capability: VaultCapability::new("ckh_reporting_secret"),
            record_version: 31,
        }),
    }
}

/// A rejected vault credential must reach the credential store as a reportable
/// failure, and that depends on which error the HTTP layer produces.
///
/// The reporting gate matches only `ProviderStatus`, so a 401 mapped to
/// `Unauthorized` is dropped silently: the store is never told the credential
/// is dead, no re-login is prompted, and the account stays dark until someone
/// investigates by hand. Which mapping happens is decided by a dispatch inside
/// the transport, and every other test in this area hands the gate a
/// pre-built error, so none of them exercise that decision.
///
/// This drives a real request at a loopback server answering 401.
#[tokio::test]
async fn a_rejected_vault_credential_surfaces_as_a_reportable_status() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 8 * 1024];
        let _ = stream.read(&mut buffer).await.unwrap();
        let body = r#"{"error":"invalid_token"}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });

    let source = Arc::new(MockReportingCredentialSource::default());
    let request = ResetRequest {
        base_url: format!("http://{address}/backend-api"),
        ..reporting_reset_request(Arc::clone(&source), "reset-bearer-secret")
    };

    let transport = ReqwestResetTransport::new(reqwest::Client::new());
    let error = transport
        .fetch_credits(&request)
        .await
        .expect_err("a 401 must fail");

    // The gate's own condition, asserted against what the transport actually
    // produced rather than against a value constructed here.
    assert!(
        matches!(error, FetchError::ProviderStatus(401)),
        "a rejected credential produced {error:?}, which the reporting gate drops"
    );

    // And the consequence: the store is told, with the version that was served.
    request.report_auth_failure(&error);
    tokio::task::yield_now().await;
    assert_eq!(*source.reports.lock().unwrap(), vec![(401, 31)]);

    server.await.unwrap();
}

#[tokio::test]
async fn credits_and_consume_auth_failures_report_served_version() {
    let source = Arc::new(MockReportingCredentialSource::default());
    let request = reporting_reset_request(Arc::clone(&source), "reset-bearer-secret");

    request.report_auth_failure(&FetchError::ProviderStatus(401));
    tokio::task::yield_now().await;

    let temp = ResetTempDir::new("consume-auth-report");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Status(401));
    coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(100.0, true)),
            &transport,
            &request,
        )
        .await;
    tokio::task::yield_now().await;

    let mut reports = source.reports.lock().unwrap().clone();
    reports.sort_unstable();
    assert_eq!(reports, vec![(401, 31), (401, 31)]);
}

#[test]
fn reset_request_debug_redacts_bearer_and_capability() {
    let source = Arc::new(MockReportingCredentialSource::default());
    let request = reporting_reset_request(source, "reset-debug-secret");
    let debug = format!("{request:?}");
    assert!(!debug.contains("reset-debug-secret"));
    assert!(!debug.contains("ckh_reporting_secret"));
    assert!(debug.contains("redacted"));
}

struct SameAccountVaultSource {
    gets: AtomicUsize,
}

#[async_trait]
impl CredentialSource for SameAccountVaultSource {
    async fn get(
        &self,
        _capability: &VaultCapability,
        min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError> {
        assert_eq!(min_ttl_ms, 120_000);
        self.gets.fetch_add(1, Ordering::SeqCst);
        Ok(VaultCredential {
            payload: b"real-provider-vault-token".to_vec(),
            expires_at_ms: None,
            record_version: 8,
            account_id: Some("real-provider-account".to_string()),
            email: None,
            org_name: None,
            project_id: None,
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

fn write_owner_only_test_file(path: &std::path::Path, body: &[u8]) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).unwrap();
    std::io::Write::write_all(&mut file, body).unwrap();
}

#[tokio::test]
async fn i9_real_codex_provider_two_units_same_account_send_one_consume_post() {
    let temp = ResetTempDir::new("real-provider-two-unit-one-post");
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let http_server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let size = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.lines().any(|line| {
                line.eq_ignore_ascii_case("chatgpt-account-id: real-provider-account")
            }));
            assert!(request.lines().any(|line| {
                line.eq_ignore_ascii_case("authorization: Bearer real-provider-local-token")
                    || line.eq_ignore_ascii_case("authorization: Bearer real-provider-vault-token")
            }));
            let body = serde_json::json!({
                "rate_limit": {
                    "limit_reached": true,
                    "primary_window": {
                        "used_percent": 100.0,
                        "reset_at": 1_900_000_000_i64,
                        "limit_window_seconds": 604_800
                    }
                }
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let codex_home = temp.dir.join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    write_owner_only_test_file(
        &codex_home.join("auth.json"),
        br#"{"tokens":{"access_token":"real-provider-local-token","account_id":"real-provider-account"}}"#,
    );
    write_owner_only_test_file(
        &codex_home.join("config.toml"),
        format!(
            "chatgpt_base_url = {:?}\n",
            format!("http://{address}/backend-api")
        )
        .as_bytes(),
    );
    let handles_path = temp.dir.join("vault-handles.json");
    write_owner_only_test_file(
        &handles_path,
        br#"{"handles":{"chatgpt:openai":"ckh_real_provider"}}"#,
    );

    let source = Arc::new(SameAccountVaultSource {
        gets: AtomicUsize::new(0),
    });
    let credential_source: Arc<dyn CredentialSource> = source.clone();
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Outcome(
        ConsumeOutcome::Reset,
    )));
    let reset_transport: Arc<dyn ResetTransport> = transport.clone();
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let provider = crate::codex::CodexProvider::new_for_test(
        crate::config::CodexConfig {
            auto_use_resets: 172_800,
        },
        Some(credential_source),
        reset_transport,
        coordinator,
        VaultHandleLoader::new(Some(handles_path)),
        codex_home,
    );
    let registry = Registry::new(vec![Box::new(provider)]);

    tick(&registry).await;
    http_server.await.unwrap();

    assert_eq!(source.gets.load(Ordering::SeqCst), 1);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    assert_eq!(
        *transport.consume_accounts.lock().unwrap(),
        vec!["real-provider-account".to_string()]
    );
    let usage = registry.get_usage(Some("codex")).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].account.as_deref(), Some("real-provider-account"));
}

struct TwoUnitResetProvider {
    coordinator: Arc<ResetCoordinator>,
    transport: Arc<MockResetTransport>,
}

#[async_trait]
impl UsageProvider for TwoUnitResetProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![
            CredentialHandle::implicit(),
            CredentialHandle::vault(
                "chatgpt:openai:gmail",
                crate::credential_source::VaultCapability::new("ckh_same_account"),
            ),
        ])
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let facts = reset_facts(100.0, true);
        self.coordinator
            .process_tick(
                "same-codex-account",
                reset_tick_input(reset_now(), facts),
                self.transport.as_ref(),
                &ResetRequest {
                    base_url: "https://example.invalid/backend-api".to_string(),
                    bearer: "same-account-token".to_string(),
                    account_id: "same-codex-account".to_string(),
                    auth_failure: None,
                },
            )
            .await;
        FetchAttempt::success(
            observed(Some("same-codex-account")),
            "test",
            full_usage(100.0),
        )
    }
}

#[tokio::test]
async fn registry_scheduler_two_codex_units_same_account_send_one_consume_post() {
    let temp = ResetTempDir::new("registry-two-unit-one-post");
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Outcome(
        ConsumeOutcome::Reset,
    )));
    let registry = Registry::new(vec![Box::new(TwoUnitResetProvider {
        coordinator,
        transport: Arc::clone(&transport),
    })]);

    tick(&registry).await;

    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    let usage = registry.get_usage(Some("codex")).await;
    assert_eq!(usage.len(), 1, "same-account slots must deduplicate");
    assert_eq!(usage[0].account.as_deref(), Some("same-codex-account"));
}

#[test]
fn codex_reset_trigger_truth_table_is_fully_fenced() {
    let now = reset_now();
    let base = TriggerInput {
        armed: true,
        now,
        earliest_expiry: Some(now + chrono::Duration::minutes(5)),
        auto_use_resets_secs: 10 * 60,
        any_used_floor: true,
        at_wall: false,
        pending: false,
        spend_bound_allows: true,
        before_post_cutoff: true,
    };
    let mut cases = Vec::new();
    cases.push(("expiry", base.clone(), true));

    let mut unarmed = base.clone();
    unarmed.armed = false;
    cases.push(("unarmed", unarmed, false));

    let mut outside_expiry = base.clone();
    outside_expiry.earliest_expiry = Some(now + chrono::Duration::hours(1));
    cases.push(("outside-expiry", outside_expiry, false));

    let mut no_floor = base.clone();
    no_floor.any_used_floor = false;
    cases.push(("no-used-floor", no_floor, false));

    let mut exhaustion = base.clone();
    exhaustion.any_used_floor = false;
    exhaustion.earliest_expiry = None;
    exhaustion.at_wall = true;
    cases.push(("exhaustion", exhaustion, true));

    let mut pending = base.clone();
    pending.pending = true;
    cases.push(("pending", pending, false));

    let mut spend_bound = base.clone();
    spend_bound.spend_bound_allows = false;
    cases.push(("spend-bound", spend_bound, false));

    let mut cutoff = base;
    cutoff.before_post_cutoff = false;
    cases.push(("pre-post-cutoff", cutoff, false));

    for (name, input, expected) in cases {
        assert_eq!(evaluate_trigger(&input).fire, expected, "case {name}");
    }
}

#[test]
fn redemption_journal_reuses_pending_id_after_restart() {
    let temp = ResetTempDir::new("restart");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal.reserve("acct", now).unwrap() {
        Reservation::New(id) => id,
        other => panic!("expected a new reservation, got {other:?}"),
    };

    let parsed_id = uuid::Uuid::parse_str(&id).expect("journal id is a UUID");
    assert_eq!(parsed_id.get_version_num(), 4);

    let restarted = RedemptionJournal::new(journal.path().to_path_buf());
    assert_eq!(
        restarted.reserve("acct", now + chrono::Duration::minutes(1)),
        Ok(Reservation::ExistingPending(id.clone()))
    );
    let records = restarted.records().unwrap();
    assert_eq!(records.len(), 1, "restart must not mint a second id");
    assert_eq!(records[0].redeem_request_id, id);
}

#[tokio::test]
async fn restarted_coordinator_reposts_the_same_pending_id_without_a_new_trigger() {
    let temp = ResetTempDir::new("restart-repost");
    let now = reset_now();
    let journal = temp.journal();
    let pending_id = match journal.reserve("acct-reset", now).unwrap() {
        Reservation::New(id) => id,
        other => panic!("expected reservation, got {other:?}"),
    };
    let restarted =
        ResetCoordinator::new(RedemptionJournal::new(journal.path().to_path_buf())).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(
        ConsumeOutcome::AlreadyRedeemed,
    ));
    let mut input = reset_tick_input(now + chrono::Duration::minutes(1), reset_facts(0.0, false));
    input.earliest_expiry = Some(now + chrono::Duration::days(1));

    let result = restarted
        .process_tick("acct-reset", input, &transport, &reset_request("token"))
        .await;

    assert!(result.consume_attempted, "pending id was not retried");
    assert_eq!(result.outcome, Some(ConsumeOutcome::AlreadyRedeemed));
    let records = journal.records().unwrap();
    assert_eq!(records.len(), 1, "retry minted a second logical redemption");
    assert_eq!(records[0].redeem_request_id, pending_id);
    assert_eq!(records[0].outcome, Some(ConsumeOutcome::AlreadyRedeemed));
}

#[test]
fn f2_old_pending_redemption_is_reused_forever() {
    let temp = ResetTempDir::new("old-pending");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal
        .reserve("acct", now - chrono::Duration::hours(25))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected a new pending id, got {other:?}"),
    };

    assert_eq!(
        journal.reserve("acct", now),
        Ok(Reservation::ExistingPending(id.clone()))
    );
    let records = journal.records().unwrap();
    assert_eq!(records.len(), 1, "old pending id was replaced");
    assert_eq!(records[0].redeem_request_id, id);
    assert_eq!(
        records[0].status,
        crate::codex_resets::JournalStatus::Pending
    );
    assert_eq!(records[0].outcome, None);
}

#[test]
fn f2_late_pending_attempt_starts_durable_spend_bound() {
    let temp = ResetTempDir::new("late-pending-bound");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal
        .reserve("acct", now - chrono::Duration::hours(1))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected a new pending id, got {other:?}"),
    };
    journal.record_attempt("acct", &id, now).unwrap();
    journal.resolve("acct", &id, ConsumeOutcome::Reset).unwrap();

    let restarted = temp.journal();
    let bounded = restarted
        .inspect_account("acct", now + chrono::Duration::minutes(1))
        .unwrap();
    assert!(!bounded.spend_bound_allows);
    let records = restarted.records().unwrap();
    assert_eq!(records[0].last_attempt_at, Some(now.to_rfc3339()));
    assert_eq!(records[0].attempt_count, 1);
    assert_eq!(
        restarted
            .reserve("acct", now + chrono::Duration::minutes(1))
            .unwrap(),
        Reservation::SpendBound
    );
}

#[test]
fn f2_legacy_journal_records_default_attempt_metadata() {
    let temp = ResetTempDir::new("legacy-attempt-fields");
    let journal = temp.journal();
    std::fs::write(
        journal.path(),
        format!(
            r#"[{{"account_id":"acct","redeem_request_id":"legacy-id","created_at":"{}","status":"pending","outcome":null}}]"#,
            reset_now().to_rfc3339()
        ),
    )
    .unwrap();

    let records = journal.records().unwrap();
    assert_eq!(records[0].last_attempt_at, None);
    assert_eq!(records[0].attempt_count, 0);
}

#[test]
fn redemption_journal_prunes_old_resolved_records_only() {
    let temp = ResetTempDir::new("prune");
    let now = reset_now();
    let journal = temp.journal();
    let old_id = match journal
        .reserve("old", now - chrono::Duration::days(8))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected old reservation, got {other:?}"),
    };
    journal
        .resolve("old", &old_id, ConsumeOutcome::Reset)
        .unwrap();
    let recent_id = match journal
        .reserve("recent", now - chrono::Duration::days(1))
        .unwrap()
    {
        Reservation::New(id) => id,
        other => panic!("expected recent reservation, got {other:?}"),
    };
    journal
        .resolve("recent", &recent_id, ConsumeOutcome::NoCredit)
        .unwrap();

    assert_eq!(journal.prune(now).unwrap(), 1);
    let records = journal.records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].account_id, "recent");
}

#[test]
fn corrupt_redemption_journal_fails_closed() {
    let temp = ResetTempDir::new("corrupt");
    let journal = temp.journal();
    std::fs::write(journal.path(), b"not json").unwrap();
    assert!(journal.records().is_err());
    assert!(journal.inspect_account("acct", reset_now()).is_err());
}

#[test]
fn spend_bound_survives_a_journal_restart() {
    let temp = ResetTempDir::new("spend-bound");
    let now = reset_now();
    let journal = temp.journal();
    let id = match journal.reserve("acct", now).unwrap() {
        Reservation::New(id) => id,
        other => panic!("expected reservation, got {other:?}"),
    };
    journal
        .resolve("acct", &id, ConsumeOutcome::NothingToReset)
        .unwrap();

    let restarted = RedemptionJournal::new(journal.path().to_path_buf());
    assert_eq!(
        restarted.reserve("acct", now + chrono::Duration::minutes(29)),
        Ok(Reservation::SpendBound)
    );
    assert!(matches!(
        restarted
            .reserve("acct", now + chrono::Duration::minutes(31))
            .unwrap(),
        Reservation::New(_)
    ));
}

#[tokio::test]
async fn pre_post_cutoff_prevents_reservation_and_mutation() {
    let temp = ResetTempDir::new("pre-post-cutoff");
    let journal = temp.journal();
    let coordinator = ResetCoordinator::new(journal.clone()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let mut input = reset_tick_input(reset_now(), reset_facts(100.0, true));
    input.elapsed_since_attempt_start = crate::codex_resets::PRE_POST_CUTOFF;

    let result = coordinator
        .process_tick("acct-reset", input, &transport, &reset_request("token"))
        .await;

    assert!(!result.consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
    assert!(journal.records().unwrap().is_empty());
}

#[tokio::test]
async fn f2_redemption_and_attempt_metadata_are_durable_before_post() {
    let temp = ResetTempDir::new("reserve-before-post");
    let journal = temp.journal();
    let coordinator = ResetCoordinator::new(journal.clone()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset))
        .with_journal(&journal);
    let now = reset_now();

    let result = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("handle-a"),
        )
        .await;

    assert!(result.consume_attempted);
    assert!(transport.reservation_visible.load(Ordering::SeqCst));
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

async fn wait_for_mock_post(transport: &MockResetTransport) {
    let permit = transport.started.acquire().await.unwrap();
    permit.forget();
}

#[tokio::test]
async fn overlapping_fetches_for_one_account_send_exactly_one_post() {
    let temp = ResetTempDir::new("overlap-one-account");
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Block(
        ConsumeOutcome::Reset,
    )));
    let now = reset_now();

    let first = {
        let coordinator = Arc::clone(&coordinator);
        let transport = Arc::clone(&transport);
        tokio::spawn(async move {
            coordinator
                .process_tick(
                    "shared-account",
                    reset_tick_input(now, reset_facts(100.0, true)),
                    transport.as_ref(),
                    &reset_request("same-handle"),
                )
                .await
        })
    };
    wait_for_mock_post(&transport).await;
    let second = coordinator
        .process_tick(
            "shared-account",
            reset_tick_input(now, reset_facts(100.0, true)),
            transport.as_ref(),
            &reset_request("same-handle"),
        )
        .await;

    assert!(!second.consume_attempted);
    assert!(second.pending);
    assert!(!second.relax_eligible);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    transport.release.add_permits(1);
    assert!(first.await.unwrap().consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unresolved_post_is_not_retried_again_in_the_same_tick() {
    let temp = ResetTempDir::new("same-tick-pending");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Error);
    let now = reset_now();

    let first = coordinator
        .process_tick(
            "shared-account",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("handle-a"),
        )
        .await;
    let second = coordinator
        .process_tick(
            "shared-account",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("handle-b"),
        )
        .await;

    assert!(first.consume_attempted);
    assert!(!second.consume_attempted);
    assert!(second.pending);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn two_handles_resolving_same_account_send_exactly_one_post() {
    let temp = ResetTempDir::new("two-handles-one-account");
    let coordinator = Arc::new(ResetCoordinator::new(temp.journal()).unwrap());
    let transport = Arc::new(MockResetTransport::new(MockConsumeBehavior::Block(
        ConsumeOutcome::AlreadyRedeemed,
    )));
    let now = reset_now();

    let handle_a = {
        let coordinator = Arc::clone(&coordinator);
        let transport = Arc::clone(&transport);
        tokio::spawn(async move {
            coordinator
                .process_tick(
                    "same-account-id",
                    reset_tick_input(now, reset_facts(99.0, true)),
                    transport.as_ref(),
                    &reset_request("handle-a-token"),
                )
                .await
        })
    };
    wait_for_mock_post(&transport).await;
    let handle_b = coordinator
        .process_tick(
            "same-account-id",
            reset_tick_input(now, reset_facts(99.0, true)),
            transport.as_ref(),
            &reset_request("handle-b-token"),
        )
        .await;

    assert!(!handle_b.consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
    transport.release.add_permits(1);
    handle_a.await.unwrap();
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

fn primary_percent(entry: &ProviderUsage) -> f64 {
    entry
        .usage
        .as_ref()
        .and_then(|usage| usage.primary.as_ref())
        .expect("test usage has a primary window")
        .used_percent
}

fn full_usage(percent: f64) -> Usage {
    let window = |used_percent: f64, label: &str, minutes| RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at: Some(format!("2026-07-15T{label}:00Z")),
        window_minutes: Some(minutes),
        used_count: None,
        total_count: None,
    };
    Usage {
        primary: Some(window(percent, "01:00", 300)),
        secondary: Some(window(percent + 1.0, "02:00", 10_080)),
        tertiary: Some(window(percent + 2.0, "03:00", 43_200)),
        extra_rate_windows: Some(vec![ExtraWindow {
            title: Some("model pool".to_string()),
            id: Some("model-1".to_string()),
            window: Some(window(percent + 3.0, "04:00", 60)),
        }]),
    }
}

struct RelaxingProvider {
    name: &'static str,
    eligible: bool,
    usage: Usage,
}

#[async_trait]
impl UsageProvider for RelaxingProvider {
    fn name(&self) -> &str {
        self.name
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(None, "test", self.usage.clone()).with_relax_eligible(self.eligible)
    }
}

struct LabeledRelaxingProvider;

#[async_trait]
impl UsageProvider for LabeledRelaxingProvider {
    fn name(&self) -> &str {
        "codex-labeled"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(observed(Some("codex-account")), "test", full_usage(38.0))
            .with_relax_eligible(true)
    }
}

#[tokio::test]
async fn f8_labeled_emission_applies_fresh_relaxation_transform() {
    let registry = Registry::new(vec![Box::new(LabeledRelaxingProvider)]);
    tick(&registry).await;

    let served = registry.get_usage(Some("codex-labeled")).await;
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].account.as_deref(), Some("codex-account"));
    assert_eq!(primary_percent(&served[0]), 0.0);
    assert_eq!(
        slot(&registry, "codex-labeled", "implicit-local")
            .entry
            .as_ref()
            .and_then(|entry| entry.usage.as_ref())
            .and_then(|usage| usage.primary.as_ref())
            .unwrap()
            .used_percent,
        38.0
    );
}

#[tokio::test]
async fn relaxation_is_read_time_only_and_expires_at_the_freshness_horizon() {
    let registry = Registry::new(vec![Box::new(RelaxingProvider {
        name: "codex",
        eligible: true,
        usage: full_usage(41.0),
    })]);
    tick(&registry).await;

    let stored = slot(&registry, "codex", "implicit-local");
    let stored_usage = stored.entry.as_ref().unwrap().usage.as_ref().unwrap();
    assert_eq!(stored_usage.primary.as_ref().unwrap().used_percent, 41.0);
    assert_eq!(stored_usage.secondary.as_ref().unwrap().used_percent, 42.0);
    assert_eq!(
        stored_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .used_percent,
        44.0,
        "the slot must retain raw truth"
    );

    let fresh = registry.get_usage(None).await;
    let fresh_usage = fresh[0].usage.as_ref().unwrap();
    assert_eq!(fresh_usage.primary.as_ref().unwrap().used_percent, 0.0);
    assert_eq!(fresh_usage.secondary.as_ref().unwrap().used_percent, 0.0);
    assert_eq!(fresh_usage.tertiary.as_ref().unwrap().used_percent, 0.0);
    assert_eq!(
        fresh_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .used_percent,
        0.0
    );
    // The provider-reported truth rides beside the effective zero so human
    // UIs can display real usage.
    assert_eq!(
        fresh_usage.primary.as_ref().unwrap().raw_used_percent,
        Some(41.0)
    );
    assert_eq!(
        fresh_usage.secondary.as_ref().unwrap().raw_used_percent,
        Some(42.0)
    );
    assert_eq!(
        fresh_usage.tertiary.as_ref().unwrap().raw_used_percent,
        Some(43.0)
    );
    assert_eq!(
        fresh_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .raw_used_percent,
        Some(44.0)
    );
    assert_eq!(
        fresh_usage.primary.as_ref().unwrap().resets_at,
        stored_usage.primary.as_ref().unwrap().resets_at
    );
    assert_eq!(
        fresh_usage.primary.as_ref().unwrap().window_minutes,
        Some(300)
    );

    let key = SlotKey::new("codex", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut stale = store.get(&key).unwrap().clone();
        stale.last_success_at = Some(Instant::now() - FRESH_HORIZON - Duration::from_secs(1));
        let incarnation = stale.incarnation;
        let sequence = stale.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, sequence, stale));
    }
    let stale = registry.get_usage(None).await;
    let stale_usage = stale[0].usage.as_ref().unwrap();
    assert_eq!(stale_usage.primary.as_ref().unwrap().used_percent, 41.0);
    assert_eq!(
        stale_usage.primary.as_ref().unwrap().raw_used_percent,
        None,
        "an unrelaxed window must not carry a raw annotation"
    );
    assert_eq!(stale_usage.secondary.as_ref().unwrap().used_percent, 42.0);
    assert_eq!(
        stale_usage.extra_rate_windows.as_ref().unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .used_percent,
        44.0,
        "a relaxed percentage survived beyond freshness"
    );
}

struct DegradingRelaxProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl UsageProvider for DegradingRelaxProvider {
    fn name(&self) -> &str {
        "codex-degraded"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            FetchAttempt::success(None, "test", full_usage(55.0)).with_relax_eligible(true)
        } else {
            FetchAttempt::failure(None, None, FetchError::NoSession("signed out".to_string()))
        }
    }
}

struct TransientlyFailingRelaxProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl UsageProvider for TransientlyFailingRelaxProvider {
    fn name(&self) -> &str {
        "codex-stale-transient"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            FetchAttempt::success(None, "test", full_usage(62.0)).with_relax_eligible(true)
        } else {
            FetchAttempt::failure(
                None,
                Some("test".to_string()),
                FetchError::Upstream("temporary credits outage".to_string()),
            )
        }
    }
}

#[tokio::test]
async fn timestamp_formatting_runs_after_store_unlock() {
    let registry = Arc::new(registry(&[("format-lock", false, true)]));
    tick(&registry).await;
    let hook_ran = Arc::new(AtomicBool::new(false));

    BEFORE_FETCHED_AT_FORMAT.with(|hook| {
        let registry = Arc::clone(&registry);
        let hook_ran = Arc::clone(&hook_ran);
        *hook.borrow_mut() = Some(Box::new(move || {
            assert!(
                registry.store.try_lock().is_ok(),
                "store lock held during fetched_at formatting"
            );
            hook_ran.store(true, Ordering::SeqCst);
        }));
    });

    let usage = registry.get_usage(Some("format-lock")).await;
    assert_eq!(usage.len(), 1);
    assert!(hook_ran.load(Ordering::SeqCst));
    BEFORE_FETCHED_AT_FORMAT.with(|hook| *hook.borrow_mut() = None);
}

#[tokio::test]
async fn fetched_at_is_stable_for_fresh_and_stale_slot_entries() {
    let registry = Registry::new(vec![Box::new(TransientlyFailingRelaxProvider {
        calls: AtomicUsize::new(0),
    })]);
    tick(&registry).await;

    let fresh = registry.get_usage(Some("codex-stale-transient")).await;
    assert_eq!(fresh.len(), 1);
    let fetched_at = fresh[0].fetched_at.clone().expect("fresh slot timestamp");
    let parsed = chrono::DateTime::parse_from_rfc3339(&fetched_at).unwrap();
    assert!(
        (Utc::now() - parsed.with_timezone(&Utc))
            .num_seconds()
            .abs()
            < 5
    );

    force_due(&registry, "codex-stale-transient");
    tick(&registry).await;
    let stale = registry.get_usage(Some("codex-stale-transient")).await;
    assert_eq!(stale[0].fetched_at.as_deref(), Some(fetched_at.as_str()));
}

#[tokio::test]
async fn entries_for_a_never_successful_slot_have_no_fetched_at() {
    // Only covers a slot that has NEVER succeeded. A degraded entry whose slot
    // succeeded earlier does carry a timestamp — see the test below, which is
    // the case a credential-poor host cannot produce.
    let registry = registry(&[("pending-or-degraded", false, false)]);
    assert!(registry.get_usage(None).await.is_empty());
    tick(&registry).await;
    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1);
    assert!(usage[0].fetched_at.is_none());
}

/// A provider whose first fetch succeeds and whose later fetches fail
/// non-transiently, producing a degraded entry on a slot that has a recorded
/// success.
struct SucceedsThenAuthFailsProvider {
    calls: Mutex<usize>,
}

#[async_trait]
impl UsageProvider for SucceedsThenAuthFailsProvider {
    fn name(&self) -> &str {
        "codex"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        if *calls == 1 {
            FetchAttempt::success(None, "test", Usage::default())
        } else {
            FetchAttempt::failure(None, None, FetchError::Unauthorized("401".into()))
        }
    }
}

#[tokio::test]
async fn a_degraded_entry_keeps_the_timestamp_of_its_last_success() {
    // The timestamp survives the failure that degrades the entry, so it reports
    // when the provider last SUCCEEDED — not when the error was observed, and not
    // when anything now in the entry was true. Consumers are told this, so it is
    // pinned here: presence of a timestamp says nothing about whether an entry is
    // usable, and the value must not be read as a failure time.
    //
    // This shape cannot be captured from a host lacking the credential, because
    // there every degraded provider has never succeeded and so carries no
    // timestamp at all — which is consistent with the opposite claim and is not
    // evidence for it. It has to be constructed.
    let registry = Registry::new(vec![Box::new(SucceedsThenAuthFailsProvider {
        calls: Mutex::new(0),
    })]);

    tick(&registry).await;
    let healthy = registry.get_usage(Some("codex")).await;
    let first_seen = healthy[0]
        .fetched_at
        .clone()
        .expect("a successful fetch stamps its entry");
    assert!(healthy[0].usage.is_some());

    force_due(&registry, "codex");
    tick(&registry).await;

    let degraded = registry.get_usage(Some("codex")).await;
    assert_eq!(degraded.len(), 1);
    assert!(
        degraded[0].error.is_some() && degraded[0].usage.is_none(),
        "the auth failure must degrade the entry"
    );
    assert_eq!(
        degraded[0].fetched_at.as_deref(),
        Some(first_seen.as_str()),
        "a degraded entry keeps its last success time, so the timestamp dates \
         content that is no longer present and must not be read as usability \
         or as when the failure happened"
    );
}

#[tokio::test]
async fn f8_stale_transient_success_serves_raw_usage_after_failure() {
    let registry = Registry::new(vec![Box::new(TransientlyFailingRelaxProvider {
        calls: AtomicUsize::new(0),
    })]);
    tick(&registry).await;
    assert_eq!(primary_percent(&registry.get_usage(None).await[0]), 0.0);

    force_due(&registry, "codex-stale-transient");
    tick(&registry).await;
    let served = registry.get_usage(Some("codex-stale-transient")).await;
    assert_eq!(served.len(), 1);
    assert_eq!(primary_percent(&served[0]), 62.0);
    let stale = slot(&registry, "codex-stale-transient", "implicit-local");
    assert_eq!(stale.status, SlotStatus::StaleTransient);
    assert!(!stale.relax_eligible);
}

#[tokio::test]
async fn degraded_slots_and_other_providers_never_relax() {
    let registry = Registry::new(vec![
        Box::new(DegradingRelaxProvider {
            calls: AtomicUsize::new(0),
        }),
        Box::new(RelaxingProvider {
            name: "other-provider",
            eligible: false,
            usage: full_usage(31.0),
        }),
    ]);
    tick(&registry).await;
    let initial = registry.get_usage(None).await;
    assert_eq!(
        initial[1]
            .usage
            .as_ref()
            .unwrap()
            .primary
            .as_ref()
            .unwrap()
            .used_percent,
        31.0,
        "a provider that never opted in was transformed"
    );

    force_due(&registry, "codex-degraded");
    tick(&registry).await;
    let degraded = registry.get_usage(Some("codex-degraded")).await;
    assert_eq!(degraded.len(), 1);
    assert!(degraded[0].error.is_some());
    assert!(degraded[0].usage.is_none());
    assert!(!slot(&registry, "codex-degraded", "implicit-local").relax_eligible);
}

#[tokio::test]
async fn reporting_gate_keeps_wall_and_mutation_ticks_raw() {
    let temp = ResetTempDir::new("reporting-wall");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let now = reset_now();
    let facts = reset_facts(100.0, true);
    let result = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, facts.clone()),
            &transport,
            &reset_request("token"),
        )
        .await;

    assert!(result.consume_attempted);
    assert!(!result.relax_eligible);
    assert!(!reporting_eligible(true, &facts, true, false, true));
}

#[tokio::test]
async fn below_wall_expiry_mutation_tick_is_raw() {
    let temp = ResetTempDir::new("reporting-mutation");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let result = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(20.0, false)),
            &transport,
            &reset_request("token"),
        )
        .await;

    assert!(result.trigger.expiry_trigger);
    assert!(result.consume_attempted);
    assert!(
        !result.relax_eligible,
        "mutation tick reported relaxed data"
    );
}

#[tokio::test]
async fn nothing_to_reset_storm_stays_raw_and_respects_spend_bound() {
    let temp = ResetTempDir::new("nothing-storm");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport =
        MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::NothingToReset));
    let now = reset_now();
    let first = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("token"),
        )
        .await;
    let second = coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now + chrono::Duration::minutes(1), reset_facts(100.0, true)),
            &transport,
            &reset_request("token"),
        )
        .await;

    assert_eq!(first.outcome, Some(ConsumeOutcome::NothingToReset));
    assert!(!first.relax_eligible);
    assert!(!second.consume_attempted);
    assert!(!second.relax_eligible);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn spend_bound_delays_mutation_but_not_below_wall_truth_relaxation() {
    let temp = ResetTempDir::new("bound-not-reporting");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let now = reset_now();
    coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(now, reset_facts(100.0, true)),
            &transport,
            &reset_request("token"),
        )
        .await;

    let mut below_wall =
        reset_tick_input(now + chrono::Duration::minutes(1), reset_facts(20.0, false));
    below_wall.earliest_expiry = Some(now + chrono::Duration::days(1));
    let result = coordinator
        .process_tick(
            "acct-reset",
            below_wall,
            &transport,
            &reset_request("token"),
        )
        .await;
    assert!(!result.consume_attempted);
    assert!(result.relax_eligible);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 1);
}

#[test]
fn corrupt_journal_prevents_reset_coordinator_construction() {
    let temp = ResetTempDir::new("corrupt-process");
    let journal = temp.journal();
    std::fs::write(journal.path(), b"{").unwrap();

    assert!(ResetCoordinator::new(journal).is_err());
}

#[cfg(unix)]
#[test]
fn f5_read_only_journal_parent_disarms_before_reporting_can_relax() {
    use std::os::unix::fs::PermissionsExt;

    let temp = ResetTempDir::new("read-only-journal");
    let parent = temp.journal().path().parent().unwrap().to_path_buf();
    let original = std::fs::metadata(&parent).unwrap().permissions();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = ResetCoordinator::new(temp.journal());
    std::fs::set_permissions(&parent, original).unwrap();

    assert!(result.is_err());
}

struct CreditsFailurePathProvider {
    transport: MockResetTransport,
}

#[async_trait]
impl UsageProvider for CreditsFailurePathProvider {
    fn name(&self) -> &str {
        "codex-credits-failure"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let usage_snapshot = crate::codex::normalize_usage_snapshot(
            br#"{"rate_limit":{"primary_window":{"used_percent":73.0},"limit_reached":false}}"#,
        )
        .unwrap();
        let credits = self.transport.fetch_credits(&reset_request("token")).await;
        match crate::codex::normalize_credits_tick(credits, Utc::now()) {
            Ok(_) => panic!("the mock credits request unexpectedly armed the provider"),
            Err(_) => crate::codex::unarmed_usage_attempt(
                observed(Some("acct-reset")),
                "oauth",
                usage_snapshot,
            ),
        }
    }
}

#[tokio::test]
async fn f8_credits_get_failure_follows_provider_path_and_keeps_raw_percentages() {
    let registry = Registry::new(vec![Box::new(CreditsFailurePathProvider {
        transport: MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset))
            .credits_failure(),
    })]);
    tick(&registry).await;
    let served = registry.get_usage(None).await;
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].account.as_deref(), Some("acct-reset"));
    assert_eq!(primary_percent(&served[0]), 73.0);
    assert!(!slot(&registry, "codex-credits-failure", "implicit-local").relax_eligible);
}

#[tokio::test]
async fn f6_missing_limit_reached_never_relaxes_even_at_low_usage() {
    let snapshot = crate::codex::normalize_usage_snapshot(
        br#"{"rate_limit":{"primary_window":{"used_percent":10.0,"reset_at":1900000000,"limit_window_seconds":3600}}}"#,
    )
    .unwrap();
    assert_eq!(snapshot.limit_reached, None);
    let facts = UsageFacts::from_usage(&snapshot.usage, snapshot.limit_reached);
    assert!(!facts.at_wall);
    assert!(!facts.below_wall());

    let temp = ResetTempDir::new("missing-wall-evidence");
    let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
    let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(ConsumeOutcome::Reset));
    let mut input = reset_tick_input(reset_now(), facts);
    input.earliest_expiry = Some(input.now + chrono::Duration::days(20));
    let result = coordinator
        .process_tick("acct-reset", input, &transport, &reset_request("token"))
        .await;
    assert!(!result.relax_eligible);
    assert!(!result.consume_attempted);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 0);
}

#[test]
fn f3_trigger_uses_earliest_credit_outside_the_safety_margin() {
    let now = reset_now();
    let credit = |id: &str, expiry: chrono::DateTime<Utc>| {
        format!(
            r#"{{"id":"{id}","reset_type":"codex_rate_limits","status":"available","expires_at":"{}"}}"#,
            expiry.to_rfc3339()
        )
    };
    let mixed = normalize_credits(
        format!(
            r#"{{"credits":[{},{}],"available_count":2}}"#,
            credit("inside-margin", now + chrono::Duration::seconds(30)),
            credit("healthy", now + chrono::Duration::days(20))
        )
        .as_bytes(),
    )
    .unwrap();
    let mixed_expiry = crate::codex::reset_trigger_expiry(&mixed, now);
    let mixed_trigger = evaluate_trigger(&TriggerInput {
        armed: mixed_expiry.is_some(),
        now,
        earliest_expiry: mixed_expiry,
        auto_use_resets_secs: 24 * 60 * 60,
        any_used_floor: true,
        at_wall: false,
        pending: false,
        spend_bound_allows: true,
        before_post_cutoff: true,
    });
    assert!(!mixed_trigger.expiry_trigger);
    assert!(!mixed_trigger.fire);

    let near = normalize_credits(
        format!(
            r#"{{"credits":[{}],"available_count":1}}"#,
            credit("healthy-near", now + chrono::Duration::hours(12))
        )
        .as_bytes(),
    )
    .unwrap();
    let near_expiry = crate::codex::reset_trigger_expiry(&near, now);
    assert!(
        evaluate_trigger(&TriggerInput {
            armed: near_expiry.is_some(),
            now,
            earliest_expiry: near_expiry,
            auto_use_resets_secs: 24 * 60 * 60,
            any_used_floor: true,
            at_wall: false,
            pending: false,
            spend_bound_allows: true,
            before_post_cutoff: true,
        })
        .fire
    );
}

#[test]
fn zero_or_expiring_now_credits_cannot_arm() {
    let now = reset_now();
    let zero = normalize_credits(br#"{"credits":[],"available_count":0}"#).unwrap();
    assert_eq!(zero.earliest_usable_expiry(now), None);

    let expiring = normalize_credits(
        br#"{
            "credits": [{
                "id": "credit-soon",
                "status": "available",
                "expires_at": "2026-07-14T12:00:30Z"
            }],
            "available_count": 1
        }"#,
    )
    .unwrap();
    assert_eq!(expiring.earliest_usable_expiry(now), None);
    assert!(!reporting_eligible(
        false,
        &reset_facts(20.0, false),
        false,
        false,
        true
    ));
}

#[tokio::test]
async fn consume_response_codes_http_error_and_timeout_are_fail_closed() {
    for outcome in [
        ConsumeOutcome::Reset,
        ConsumeOutcome::NothingToReset,
        ConsumeOutcome::NoCredit,
        ConsumeOutcome::AlreadyRedeemed,
    ] {
        let temp = ResetTempDir::new(outcome.as_code());
        let coordinator = ResetCoordinator::new(temp.journal()).unwrap();
        let transport = MockResetTransport::new(MockConsumeBehavior::Outcome(outcome));
        let result = coordinator
            .process_tick(
                "acct-reset",
                reset_tick_input(reset_now(), reset_facts(100.0, true)),
                &transport,
                &reset_request("token"),
            )
            .await;
        assert_eq!(result.outcome, Some(outcome));
        assert!(!result.pending);
        assert!(!result.relax_eligible);
    }

    let error_temp = ResetTempDir::new("http-error");
    let error_coordinator = ResetCoordinator::new(error_temp.journal()).unwrap();
    let error_transport = MockResetTransport::new(MockConsumeBehavior::Error);
    let error_result = error_coordinator
        .process_tick(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(100.0, true)),
            &error_transport,
            &reset_request("token"),
        )
        .await;
    assert!(error_result.consume_attempted);
    assert!(error_result.pending);
    assert_eq!(error_result.outcome, None);
    assert!(!error_result.relax_eligible);

    let timeout_temp = ResetTempDir::new("timeout");
    let timeout_coordinator = ResetCoordinator::new(timeout_temp.journal()).unwrap();
    let timeout_transport = MockResetTransport::new(MockConsumeBehavior::Hang);
    let timeout_result = timeout_coordinator
        .process_tick_with_timeout(
            "acct-reset",
            reset_tick_input(reset_now(), reset_facts(100.0, true)),
            &timeout_transport,
            &reset_request("token"),
            Duration::from_millis(10),
        )
        .await;
    assert!(timeout_result.consume_attempted);
    assert!(timeout_result.pending);
    assert_eq!(timeout_result.outcome, None);
    assert!(!timeout_result.relax_eligible);
}

#[test]
fn credits_date_header_is_the_arming_clock_when_valid() {
    let local = Utc.with_ymd_and_hms(2026, 7, 10, 1, 2, 3).single().unwrap();
    let server = response_now(Some("Tue, 14 Jul 2026 12:00:00 GMT"), local);
    assert_eq!(server, reset_now());
    assert_eq!(response_now(Some("not-a-date"), local), local);
}

#[test]
fn credits_normalizer_matches_live_captured_contract() {
    let credits = normalize_credits(
        br#"{
            "credits": [
                {
                    "id": "rc_01",
                    "reset_type": "codex_rate_limits",
                    "status": "available",
                    "granted_at": "2026-06-14T12:00:00Z",
                    "expires_at": "2026-07-14T13:00:01Z",
                    "redeem_started_at": null,
                    "redeemed_at": null,
                    "title": "Full reset",
                    "description": "Reset Codex rate limits"
                },
                {
                    "id": "rc_02",
                    "reset_type": "codex_rate_limits",
                    "status": "redeemed",
                    "granted_at": "2026-06-01T12:00:00Z",
                    "expires_at": "2026-07-01T12:00:00Z",
                    "redeem_started_at": "2026-06-20T12:00:00Z",
                    "redeemed_at": "2026-06-20T12:00:01Z",
                    "title": "Full reset",
                    "description": "Reset Codex rate limits"
                }
            ],
            "available_count": 1
        }"#,
    )
    .unwrap();

    assert_eq!(credits.reported_available_count, 1);
    assert_eq!(credits.available.len(), 1);
    assert_eq!(credits.available[0].id, "rc_01");
    assert_eq!(
        credits.available[0].expires_at,
        Utc.with_ymd_and_hms(2026, 7, 14, 13, 0, 1)
            .single()
            .unwrap()
    );
    assert_eq!(credits.saved_resets().available_count, 1);
    assert_eq!(
        credits.saved_resets().soonest_expires_at.as_deref(),
        Some("2026-07-14T13:00:01+00:00")
    );
    assert_eq!(credits.saved_resets().credits.len(), 1);
}

#[tokio::test]
async fn a_saturated_provider_still_advances_its_later_handles() {
    // Enough due providers to fill the concurrency cap on the first pass, so each
    // provider gets roughly one admission per turn while holding two due handles.
    //
    // This is the pressure case: if admission order were fixed, the same handle
    // would be taken every turn and the second would never be reached, leaving it
    // permanently unattempted. An unattempted handle has no resolved account, and
    // the read path collapses a provider to a single unlabeled entry unless every
    // handle resolves one — so the visible cost is that the provider's accounts
    // stop being reported separately.
    let provider_count = refresh::CONCURRENCY_CAP + 1;
    let calls: Vec<_> = (0..provider_count)
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();
    let providers: Vec<Box<dyn UsageProvider>> = (0..provider_count)
        .map(|index| {
            Box::new(CursorProvider {
                name: format!("provider-{index:02}"),
                handles: 2,
                calls: Arc::clone(&calls[index]),
            }) as Box<dyn UsageProvider>
        })
        .collect();
    let registry = Registry::new(providers);

    // Sustained pressure: a handle that was just refreshed falls due again before
    // the next turn, which is what a refresh interval shorter than a full turn
    // looks like. Slots that have NOT been attempted keep their original due time,
    // because nothing has refreshed them — overwriting it would erase how long
    // they have been waiting, which is the signal fair admission depends on.
    for _ in 0..6 {
        tick(&registry).await;
        let mut store = registry.store.lock().unwrap();
        for (key, mut slot) in store.snapshot() {
            if slot.last_success_at.is_none() {
                continue;
            }
            slot.next_due_at = Instant::now();
            let incarnation = slot.incarnation;
            let attempt_sequence = slot.attempt_sequence;
            assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
        }
    }

    let store = registry.store.lock().unwrap();
    let never_attempted: Vec<_> = store
        .snapshot()
        .into_iter()
        .filter(|(_, slot)| slot.status == SlotStatus::Pending)
        .map(|(key, _)| format!("{}/{}", key.provider, key.handle.stable_id()))
        .collect();

    assert!(
        never_attempted.is_empty(),
        "every handle must eventually be admitted under sustained pressure; \
         these were never attempted: {never_attempted:?}"
    );
}

/// Two handles resolving the same account, both serving stale data after their
/// own transient failures, with different observation ages.
struct StaleDuplicateProvider;

#[async_trait]
impl UsageProvider for StaleDuplicateProvider {
    fn name(&self) -> &str {
        "stale-dup"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        // Deliberately ordered so the handle carrying the OLDER snapshot sorts
        // first: if selection fell back to handle order, it would win.
        Ok(vec![handle("A-early"), handle("B-late")])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let used_percent = if handle.stable_id() == "A-early" {
            10.0
        } else {
            90.0
        };
        let usage = Usage {
            primary: Some(RateWindow {
                used_percent,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: Some(300),
                used_count: None,
                total_count: None,
            }),
            ..Usage::default()
        };
        FetchAttempt::success(observed(Some("shared")), "test", usage)
    }
}

#[tokio::test]
async fn duplicate_account_with_equal_status_serves_the_newer_observation() {
    let registry = Registry::new(vec![Box::new(StaleDuplicateProvider)]);
    tick(&registry).await;

    // Both slots end up stale-serving, but one observation is ten minutes older
    // than the other. Usage only grows within a window, so the older snapshot
    // necessarily understates pressure — serving it can report a near-exhausted
    // account as comfortable, which is the direction that costs a consumer most.
    {
        let mut store = registry.store.lock().unwrap();
        for (key, mut slot) in store.snapshot() {
            slot.status = SlotStatus::StaleTransient;
            let age_secs = if key.handle.stable_id() == "A-early" {
                600
            } else {
                5
            };
            slot.last_success_at = Some(Instant::now() - Duration::from_secs(age_secs));
            let incarnation = slot.incarnation;
            let attempt_sequence = slot.attempt_sequence;
            assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
        }
    }

    let usage = registry.get_usage(None).await;
    assert_eq!(usage.len(), 1, "one entry per account");
    assert_eq!(
        usage[0]
            .usage
            .as_ref()
            .and_then(|usage| usage.primary.as_ref())
            .map(|window| window.used_percent),
        Some(90.0),
        "the newer observation must win; serving the older one understates usage"
    );
}

#[tokio::test]
async fn a_provider_that_resolves_no_handle_is_named_rather_than_skipped() {
    // A provider whose credential enumeration keeps failing holds no slots, so it
    // lands in none of the fresh/stale/degraded buckets while still counting
    // toward providers_total. Skipping it silently would let the buckets
    // under-sum, and "configured but never came up" would read as an absence
    // rather than as a problem worth acting on.
    let provider = EnumerationProvider {
        handles: Arc::new(Mutex::new(vec![handle("H1")])),
        fail_enumeration: Arc::new(Mutex::new(true)),
        calls: Arc::new(Mutex::new(HashMap::new())),
    };
    let registry = Registry::new(vec![Box::new(provider)]);

    // Before the first tick every provider legitimately has no slots yet, so
    // reporting then would name all of them for the first seconds after start.
    assert!(
        registry.health().without_handles.is_empty(),
        "a provider must not be reported as handle-less before the refresher runs"
    );

    tick(&registry).await;

    let health = registry.health();
    assert_eq!(
        health.without_handles,
        vec!["enumerated"],
        "a provider that resolved no handle must be visible by name"
    );
    assert_eq!(health.fresh + health.stale + health.degraded.len(), 0);
    assert_eq!(
        health.providers_total, 1,
        "it still counts in the total, which is why its absence must be reported"
    );
}

#[tokio::test]
async fn an_ineligible_slot_is_served_raw_even_when_fresh() {
    // The relaxation transform GRANTS a capacity claim: it reports 0% used while
    // the provider actually reported far more, so that a router treats the account
    // as having room. Two conditions must BOTH hold before that claim is made —
    // the slot opted in, and it is fresh.
    //
    // This pins the opt-in half directly. The freshness half has its own test, but
    // dropping the eligibility check reddened only unrelated happy-path tests,
    // which means the condition was defended by accident rather than on purpose.
    // A grant that is only incidentally guarded is one refactor away from being
    // handed out unearned, and the failure is silent: the wire would assert spare
    // capacity that does not exist.
    let registry = Registry::new(vec![Box::new(RelaxingProvider {
        name: "never-opted-in",
        eligible: false,
        usage: full_usage(88.0),
    })]);
    tick(&registry).await;

    let entries = registry.get_usage(Some("never-opted-in")).await;
    let primary = entries[0]
        .usage
        .as_ref()
        .expect("a healthy window")
        .primary
        .as_ref()
        .expect("primary window");

    assert_eq!(
        primary.used_percent, 88.0,
        "a slot that never opted in must be served exactly as the provider reported"
    );
    assert_eq!(
        primary.raw_used_percent, None,
        "the raw annotation marks a relaxed window, so an untransformed one must \
         not carry it"
    );
}

struct LabeledIneligibleProvider;

#[async_trait]
impl UsageProvider for LabeledIneligibleProvider {
    fn name(&self) -> &str {
        "codex-labeled-ineligible"
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        FetchAttempt::success(observed(Some("codex-account")), "test", full_usage(73.0))
    }
}

#[tokio::test]
async fn an_ineligible_labeled_slot_is_served_raw() {
    // The labeled path is where banked resets actually operate, since arming
    // requires a resolved account. It has a test proving it DOES relax when the
    // slot opted in; this pins the other side, that it does not otherwise.
    //
    // Both emission paths grant the same claim and each needs its own guard: a
    // test on one says nothing about the other, and the labeled branch is the one
    // that matters most, because a router reads a labeled entry as a specific
    // account's spare capacity.
    let registry = Registry::new(vec![Box::new(LabeledIneligibleProvider)]);
    tick(&registry).await;

    let served = registry.get_usage(Some("codex-labeled-ineligible")).await;
    assert_eq!(served.len(), 1);
    assert_eq!(served[0].account.as_deref(), Some("codex-account"));
    assert_eq!(
        primary_percent(&served[0]),
        73.0,
        "a labeled slot that never opted in must report the provider's own number"
    );
}

#[tokio::test]
async fn a_labeled_entry_is_withheld_while_its_identity_is_in_flux() {
    // Attaching an account to an entry GRANTS a claim: it says this usage
    // belongs to that account, and a consumer routes and meters on it. The claim
    // must be withheld while identity is unconfirmed, because the alternative is
    // attributing one account's usage to another — the most expensive thing this
    // module can get wrong, and undetectable downstream.
    //
    // The unlabeled path has its own test for this. The labeled path is the one
    // that carries an account on the wire, so it needs its own: the guards are
    // separate expressions and a test on one proves nothing about the other.
    let registry = Registry::new(vec![Box::new(LabeledRelaxingProvider)]);
    tick(&registry).await;
    assert_eq!(
        registry.get_usage(Some("codex-labeled")).await.len(),
        1,
        "precondition: the account resolved and is served"
    );

    let key = SlotKey::new("codex-labeled", CredentialHandle::implicit());
    {
        let mut store = registry.store.lock().unwrap();
        let mut slot = store.get(&key).unwrap().clone();
        assert!(
            slot.entry.is_some(),
            "a cached entry is what makes this a risk"
        );
        assert!(
            slot.account_id().is_some(),
            "precondition: an account label exists to be withheld"
        );
        slot.label_in_flux = true;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }

    assert!(
        registry.get_usage(Some("codex-labeled")).await.is_empty(),
        "a cached entry whose identity is unconfirmed must not be served under \
         its previous account label"
    );
}
