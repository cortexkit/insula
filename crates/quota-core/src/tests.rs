use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::model::Usage;
use crate::provider::{AccountObservation, FetchError, HandlesError};
use crate::refresh::BASE_INTERVAL;

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
    assert_eq!(health.cookie_cohort_degraded, vec!["cursor"]);
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
        .unwrap()
        .error
        .is_some());
    assert!(usage
        .iter()
        .find(|entry| entry.provider == "codex")
        .unwrap()
        .error
        .is_none());
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
