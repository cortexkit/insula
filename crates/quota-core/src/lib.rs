//! quota-core — provider usage fetchers + RateWindow normalization + TTL cache.
//!
//! Wire-agnostic: this crate knows nothing about subc. It exposes a [`Registry`]
//! that fetches per-provider usage (reusing each provider's own session) and
//! assembles the silent-degrade `/usage` array Alfonso consumes. The subc module
//! (`quota-module`) wraps this behind the `usage.get` route op.

pub mod alibaba;
pub mod amp;
pub mod anthropic;
pub mod antigravity;
pub mod browser_cookies;
pub mod codebuff;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod doubao;
pub mod elevenlabs;
pub mod env;
pub mod factory;
pub mod gemini;
pub mod grok;
pub mod health;
pub mod http;
pub mod jetbrains;
pub mod kilo;
pub mod kimi;
pub mod llmproxy;
pub mod manus;
pub mod mimo;
pub mod minimax;
pub mod model;
pub mod ollama;
pub mod opencode;
pub mod opencode_auth;
pub mod opencodego;
pub mod provider;
pub mod qoder;
pub mod refresh;
pub mod sakana;
pub mod stepfun;
pub mod store;
pub mod synthetic;
pub mod warp;
pub mod zai;

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use std::panic::AssertUnwindSafe;

use futures_util::{stream, FutureExt, StreamExt};
use tokio_util::sync::CancellationToken;

use health::HealthSnapshot;
use model::ProviderUsage;
use provider::UsageProvider;
use refresh::{next_slot_on_failure, next_slot_on_success, SlotStatus, STALL_HORIZON};
use store::SlotStore;

/// The outcome of one provider fetch attempt within a refresher sweep. The
/// success payload is boxed so the enum stays small (the healthy `ProviderUsage`
/// dwarfs the other variants).
enum FetchOutcome {
    /// Fetch resolved successfully.
    Ok(Box<ProviderUsage>),
    /// Fetch returned a normal error (classified transient/non-transient).
    Err(provider::FetchError),
    /// The whole fetch exceeded [`refresh::FETCH_DEADLINE`] — transient.
    TimedOut,
    /// The fetch future panicked — contained here as that provider's own
    /// non-transient failure, never a crash of the refresher loop.
    Panicked,
    /// Shutdown was requested before/while fetching — skip, leave the slot.
    Cancelled,
}

/// The provider registry: holds the fetchers and the refresher-owned slot store,
/// and serves the `usage.get` query from the store WITHOUT ever fetching inline
/// (the background refresher owns all fetching — see `refresh_loop`).
pub struct Registry {
    providers: Vec<Box<dyn UsageProvider>>,
    store: Mutex<SlotStore>,
}

impl Registry {
    /// Build a registry from a provider set. Every provider starts due-now so
    /// the refresher's first tick warms the whole set.
    pub fn new(providers: Vec<Box<dyn UsageProvider>>) -> Self {
        let names: Vec<String> = providers.iter().map(|p| p.name().to_string()).collect();
        Self {
            providers,
            store: Mutex::new(SlotStore::new(names, Instant::now())),
        }
    }

    /// The default registry: every provider we support.
    pub fn with_defaults() -> Self {
        Self::new(vec![
            Box::new(codex::CodexProvider::new()),
            Box::new(anthropic::AnthropicProvider::new()),
            Box::new(antigravity::AntigravityProvider::new()),
            Box::new(codebuff::CodebuffProvider::new()),
            Box::new(copilot::CopilotProvider::new()),
            Box::new(cursor::CursorProvider::new()),
            Box::new(doubao::DoubaoProvider::new()),
            Box::new(elevenlabs::ElevenLabsProvider::new()),
            Box::new(factory::FactoryProvider::new()),
            Box::new(gemini::GeminiProvider::new()),
            Box::new(grok::GrokProvider::new()),
            Box::new(jetbrains::JetBrainsProvider::new()),
            Box::new(kimi::KimiProvider::new()),
            Box::new(llmproxy::LlmProxyProvider::new()),
            Box::new(manus::ManusProvider::new()),
            Box::new(mimo::MimoProvider::new()),
            Box::new(minimax::MinimaxProvider::new()),
            Box::new(ollama::OllamaProvider::new()),
            Box::new(opencode::OpenCodeProvider::new()),
            Box::new(opencodego::OpenCodeGoProvider::new()),
            Box::new(qoder::QoderProvider::new()),
            Box::new(sakana::SakanaProvider::new()),
            Box::new(stepfun::StepFunProvider::new()),
            Box::new(warp::WarpProvider::new()),
            Box::new(synthetic::SyntheticProvider::new()),
            Box::new(zai::ZaiProvider::new()),
            Box::new(kilo::KiloProvider::new()),
            Box::new(alibaba::AlibabaProvider::new()),
            Box::new(amp::AmpProvider::new()),
        ])
    }

    /// Provider names registered, for discovery/observability.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Serve `usage.get`: return the usage array from the slot store, optionally
    /// filtered to one provider. CACHE-ONLY — this NEVER fetches, so it never
    /// blocks on the network (the non-blocking read guarantee); the background
    /// refresher owns all fetching. Providers not yet resolved are simply absent
    /// from the array (the honest cold state). Entries are assembled in registry
    /// order for a stable response, not `HashMap` order.
    ///
    /// `async` is kept for signature stability (the serving path is synchronous
    /// but callers await it); it does no `.await` internally.
    pub async fn get_usage(&self, provider_filter: Option<&str>) -> Vec<ProviderUsage> {
        // Poison-tolerant: the store is just a cache — recover a poisoned guard
        // and keep serving rather than panicking the whole module. A torn write
        // is at worst a stale/missing entry (writes are whole-slot inserts).
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = Vec::new();
        for provider in &self.providers {
            let name = provider.name();
            if let Some(filter) = provider_filter {
                if name != filter {
                    continue;
                }
            }
            if let Some(slot) = store.get(name) {
                if let Some(entry) = &slot.entry {
                    out.push(entry.clone());
                }
            }
        }
        out
    }

    /// One refresher sweep: fetch every provider whose `next_due_at <= now`,
    /// bounded by [`refresh::CONCURRENCY_CAP`] and a per-fetch deadline, then
    /// write each result back as a whole-slot replacement. The store mutex is
    /// only ever held across cheap map ops — NEVER across a `fetch().await`.
    ///
    /// `cancel` lets a shutdown abort in-flight fetches promptly.
    pub async fn refresh_tick(&self, cancel: &CancellationToken) {
        let tick_start = Instant::now();
        // Heartbeat + due snapshot under the lock (cheap), then release it.
        let due: Vec<usize> = {
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.mark_tick(tick_start);
            self.providers
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    store
                        .get(p.name())
                        .map(|s| s.next_due_at <= tick_start)
                        .unwrap_or(true)
                })
                .map(|(i, _)| i)
                .collect()
        };
        if due.is_empty() {
            return;
        }

        // Fetch the due providers concurrently (cap-bounded), each under a hard
        // whole-fetch deadline, publishing EACH result the moment it completes
        // (a fast provider is not held behind a slow one). Nothing holds the
        // store lock across a fetch: for each result we clone `prev` under the
        // lock, drop it, compute the next slot OUTSIDE the lock, then re-lock
        // only to insert.
        let mut stream = stream::iter(due.into_iter().map(|idx| {
            let provider = &self.providers[idx];
            async move {
                let name = provider.name().to_string();
                let attempt_start = Instant::now();
                // Contain a panicking provider: its unwind becomes a
                // non-transient failure of THAT provider, so the refresher loop
                // keeps running (a crash here would freeze all served data, since
                // this loop is the only thing that fetches).
                let fetch = AssertUnwindSafe(provider.fetch()).catch_unwind();
                let outcome = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => FetchOutcome::Cancelled,
                    r = tokio::time::timeout(refresh::FETCH_DEADLINE, fetch) => match r {
                        Ok(Ok(Ok(usage))) => FetchOutcome::Ok(Box::new(usage)),
                        Ok(Ok(Err(err))) => FetchOutcome::Err(err),
                        Ok(Err(_panic)) => FetchOutcome::Panicked,
                        Err(_elapsed) => FetchOutcome::TimedOut,
                    },
                };
                let completed_at = Instant::now();
                (name, attempt_start, completed_at, outcome)
            }
        }))
        .buffer_unordered(refresh::CONCURRENCY_CAP);

        while let Some((name, attempt_start, completed_at, outcome)) = stream.next().await {
            // Cancelled: leave the slot as-is for the next run — nothing to write.
            if matches!(outcome, FetchOutcome::Cancelled) {
                continue;
            }
            // Clone prev under the lock, then release BEFORE computing next.
            let prev = {
                let store = self
                    .store
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match store.get(&name) {
                    Some(slot) => slot.clone(),
                    None => continue, // provider vanished (cannot happen today)
                }
            };
            let next = match outcome {
                FetchOutcome::Cancelled => unreachable!("handled above"),
                FetchOutcome::Ok(usage) => {
                    next_slot_on_success(*usage, attempt_start, completed_at)
                }
                FetchOutcome::Err(err) => {
                    next_slot_on_failure(&prev, &name, &err, attempt_start, completed_at)
                }
                // A whole-fetch timeout is a transient failure (the session is
                // presumed intact); a panic is that provider's own bug, mapped to
                // a non-transient Decode so it re-probes slowly, never crashing us.
                FetchOutcome::TimedOut => next_slot_on_failure(
                    &prev,
                    &name,
                    &provider::FetchError::Upstream(format!(
                        "fetch exceeded {}s deadline",
                        refresh::FETCH_DEADLINE.as_secs()
                    )),
                    attempt_start,
                    completed_at,
                ),
                FetchOutcome::Panicked => next_slot_on_failure(
                    &prev,
                    &name,
                    &provider::FetchError::Decode("provider fetch panicked".to_string()),
                    attempt_start,
                    completed_at,
                ),
            };
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.insert(name, next);
        }
    }

    /// Drive [`refresh_tick`](Self::refresh_tick) forever: an immediate first
    /// sweep (cold-start warm), then wake on the soonest `next_due_at` (bounded
    /// by [`refresh::MAX_TICK_SLEEP`] so a newly-due provider is not starved).
    /// Returns when `cancel` fires.
    pub async fn refresh_loop(&self, cancel: CancellationToken) {
        loop {
            self.refresh_tick(&cancel).await;
            if cancel.is_cancelled() {
                return;
            }
            let sleep_for = self.sleep_until_next_due(Instant::now());
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(sleep_for) => {}
            }
        }
    }

    /// How long to sleep before the next due provider, clamped to
    /// [`refresh::MAX_TICK_SLEEP`]. Pure given the store; extracted for testing.
    fn sleep_until_next_due(&self, now: Instant) -> Duration {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let soonest = self
            .providers
            .iter()
            .filter_map(|p| store.get(p.name()))
            .map(|s| s.next_due_at)
            .min();
        match soonest {
            Some(t) => t
                .saturating_duration_since(now)
                .min(refresh::MAX_TICK_SLEEP),
            None => refresh::MAX_TICK_SLEEP,
        }
    }

    /// A cheap domain-health snapshot (subc L3), read from the slot store — no
    /// fetch. Reports the refresher's liveness (heartbeat age) plus per-provider
    /// fresh/stale/degraded counts and the cookie-cohort staleness. The module
    /// maps it to the protocol status: a poisoned store → `failing`; a stalled
    /// refresher → `degraded`; otherwise `ok` with the rest as detail, because a
    /// provider lacking local creds is this prober's normal resting state.
    pub fn health(&self) -> HealthSnapshot {
        let cookie_names: std::collections::HashSet<&str> = self
            .providers
            .iter()
            .filter(|p| p.is_cookie_based())
            .map(|p| p.name())
            .collect();
        let providers_total = self.providers.len();
        let cookie_cohort_total = cookie_names.len();

        // Poison-tolerant + fail-closed: a poisoned store mutex is a real fault,
        // so we report `poisoned` (→ Failing) WITHOUT reading the per-provider
        // slots. This deliberately diverges from "recover the guard and compute
        // metrics anyway": under a torn lock the per-provider counts are exactly
        // what is untrustworthy, so bare Failing is the honest signal. (Poison is
        // near-unreachable regardless — writes are whole-slot inserts and a
        // panicking fetch runs outside the lock.)
        let store = match self.store.lock() {
            Ok(store) => store,
            Err(_) => return HealthSnapshot::poisoned(providers_total, cookie_cohort_total),
        };
        let now = Instant::now();
        let last_tick_at = store.last_tick_at();
        let created_at = store.created_at();

        let mut fresh = 0usize;
        let mut stale = 0usize;
        let mut degraded = Vec::new();
        let mut cookie_cohort_degraded = Vec::new();
        for provider in &self.providers {
            let name = provider.name();
            let Some(slot) = store.get(name) else {
                continue;
            };
            match slot.status {
                // Not yet attempted — counted in none of the buckets.
                SlotStatus::Pending => {}
                SlotStatus::Fresh => {
                    if slot.is_fresh(now) {
                        fresh += 1;
                    } else {
                        // Was fresh but the refresher has fallen behind on it.
                        stale += 1;
                    }
                }
                SlotStatus::StaleTransient => stale += 1,
                SlotStatus::Degraded => {
                    degraded.push(name.to_string());
                    if cookie_names.contains(name) {
                        cookie_cohort_degraded.push(name.to_string());
                    }
                }
            }
        }
        drop(store);

        // Liveness: the refresher is stalled if it has not ticked within the
        // stall horizon, OR it never ticked well past startup (dead-on-arrival).
        let last_tick_age = last_tick_at.map(|t| now.saturating_duration_since(t));
        let refresher_stalled = match last_tick_age {
            Some(age) => age > STALL_HORIZON,
            None => now.saturating_duration_since(created_at) > STALL_HORIZON,
        };

        HealthSnapshot {
            providers_total,
            fresh,
            stale,
            degraded,
            cookie_cohort_total,
            cookie_cohort_degraded,
            last_tick_age,
            refresher_stalled,
            cache_poisoned: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;
    use crate::model::Usage;
    use crate::provider::FetchError;

    /// A stub provider with a controllable outcome, so health() can be tested
    /// against a real sweep rather than a mocked snapshot.
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
        async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
            if self.ok {
                Ok(ProviderUsage::healthy(
                    self.name,
                    None,
                    "test",
                    Usage::default(),
                ))
            } else {
                Err(FetchError::NoSession("stub degraded".into()))
            }
        }
    }

    /// A stub that blocks on a barrier until released — lets a test prove the
    /// read path does not wait on an in-flight fetch.
    struct BlockingProvider {
        name: &'static str,
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl UsageProvider for BlockingProvider {
        fn name(&self) -> &str {
            self.name
        }
        async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
            self.gate.notified().await; // hang until the test releases us
            Ok(ProviderUsage::healthy(
                self.name,
                None,
                "test",
                Usage::default(),
            ))
        }
    }

    use std::sync::Arc;

    fn registry(specs: &[(&'static str, bool, bool)]) -> Registry {
        let providers: Vec<Box<dyn UsageProvider>> = specs
            .iter()
            .map(|&(name, cookie, ok)| {
                Box::new(StubProvider { name, cookie, ok }) as Box<dyn UsageProvider>
            })
            .collect();
        Registry::new(providers)
    }

    async fn tick(reg: &Registry) {
        reg.refresh_tick(&CancellationToken::new()).await;
    }

    #[tokio::test]
    async fn health_reflects_the_refreshers_slot_outcomes() {
        let reg = registry(&[
            ("codex", false, true),       // healthy api/oauth provider
            ("cursor", true, false),      // degraded cookie provider (stale login)
            ("amp", true, true),          // healthy cookie provider
            ("elevenlabs", false, false), // degraded api provider (no key)
        ]);

        // Before any tick: everything is Pending, NOT degraded — an un-fetched
        // provider must not be mistaken for a failed one.
        let pre = reg.health();
        assert_eq!(pre.providers_total, 4);
        assert_eq!(pre.cookie_cohort_total, 2);
        assert_eq!(pre.fresh, 0);
        assert!(pre.degraded.is_empty());
        assert!(!pre.cache_poisoned);
        assert!(!pre.refresher_stalled); // just born, within horizon

        // Run one refresher sweep.
        tick(&reg).await;

        // Non-vacuity: health() reflects the ACTUAL sweep outcome (fails on a
        // blind ok). codex+amp fresh; cursor+elevenlabs degraded (NoSession).
        let post = reg.health();
        assert_eq!(post.fresh, 2);
        assert_eq!(post.degraded.len(), 2);
        assert!(post.degraded.contains(&"cursor".to_string()));
        assert!(post.degraded.contains(&"elevenlabs".to_string()));
        assert_eq!(post.cookie_cohort_degraded, vec!["cursor".to_string()]);
        assert!(post.last_tick_age.is_some());
        assert!(!post.is_failing());
        assert!(!post.is_degraded());
    }

    #[tokio::test]
    async fn get_usage_serves_only_resolved_providers_in_registry_order() {
        let reg = registry(&[
            ("codex", false, true),
            ("cursor", true, false), // degraded → still emitted, carries error
            ("amp", true, true),
        ]);
        // Cold: nothing resolved yet → empty array (honest cold state).
        assert!(reg.get_usage(None).await.is_empty());

        tick(&reg).await;
        let out = reg.get_usage(None).await;
        // All three resolved (healthy or degraded), in registry order.
        let names: Vec<&str> = out.iter().map(|e| e.provider.as_str()).collect();
        assert_eq!(names, vec!["codex", "cursor", "amp"]);
        // codex healthy, cursor degraded (carries error), amp healthy.
        assert!(out[0].error.is_none());
        assert!(out[1].error.is_some());
        assert!(out[2].error.is_none());
    }

    /// The non-blocking read guarantee: a read NEVER blocks on an in-flight fetch.
    #[tokio::test]
    async fn read_never_blocks_on_an_inflight_fetch() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let reg = Arc::new(Registry::new(vec![Box::new(BlockingProvider {
            name: "codex",
            gate: gate.clone(),
        })]));

        // Kick a refresher tick that will hang inside fetch().
        let reg2 = Arc::clone(&reg);
        let cancel = CancellationToken::new();
        let sweeping = tokio::spawn(async move { reg2.refresh_tick(&cancel).await });
        // Give the sweep a moment to enter the (blocked) fetch.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // The read must return immediately even though a fetch is mid-flight.
        let read = tokio::time::timeout(std::time::Duration::from_millis(200), reg.get_usage(None));
        let out = read
            .await
            .expect("read did not block on the in-flight fetch");
        assert!(out.is_empty()); // provider not resolved yet, but we did not hang

        gate.notify_one(); // release the fetch so the sweep completes
        let _ = sweeping.await;
    }

    #[tokio::test]
    async fn transient_failure_keeps_serving_last_good_window() {
        // Provider succeeds once, then fails transiently; the good window stays.
        struct FlipProvider {
            calls: std::sync::Mutex<u32>,
        }
        #[async_trait]
        impl UsageProvider for FlipProvider {
            fn name(&self) -> &str {
                "codex"
            }
            async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                if *n == 1 {
                    Ok(ProviderUsage::healthy(
                        "codex",
                        None,
                        "test",
                        Usage::default(),
                    ))
                } else {
                    Err(FetchError::Upstream("503".into())) // transient
                }
            }
        }
        let reg = Registry::new(vec![Box::new(FlipProvider {
            calls: std::sync::Mutex::new(0),
        })]);

        tick(&reg).await; // success
        let first = reg.get_usage(None).await;
        assert_eq!(first.len(), 1);
        assert!(first[0].error.is_none());

        // Force it due again and tick: transient failure keeps the good window.
        {
            let mut store = reg.store.lock().unwrap();
            let mut slot = store.get("codex").unwrap().clone();
            slot.next_due_at = Instant::now();
            store.insert("codex".into(), slot);
        }
        tick(&reg).await; // transient failure
        let second = reg.get_usage(None).await;
        assert_eq!(second.len(), 1);
        assert!(second[0].error.is_none()); // STILL the healthy window, not blanked
        assert_eq!(reg.health().stale, 1);
    }

    #[test]
    fn poisoned_store_reports_failing() {
        let reg = registry(&[("codex", false, true), ("cursor", true, true)]);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = reg.store.lock().unwrap();
            panic!("poison the slot store mutex");
        }));
        let h = reg.health();
        assert!(h.cache_poisoned);
        assert!(h.is_failing());
        assert_eq!(h.providers_total, 2);
        assert_eq!(h.cookie_cohort_total, 1);
    }

    #[test]
    fn stalled_refresher_reports_degraded() {
        // A store whose heartbeat is well past the stall horizon reads Degraded.
        let reg = registry(&[("codex", false, true)]);
        {
            let mut store = reg.store.lock().unwrap();
            store.mark_tick(Instant::now() - (STALL_HORIZON + Duration::from_secs(30)));
        }
        let h = reg.health();
        assert!(h.refresher_stalled);
        assert!(h.is_degraded());
        assert!(!h.is_failing());
    }

    /// A panicking provider must NOT crash the refresher — if it did, the
    /// background loop that owns all fetching would die and the served data would
    /// freeze forever. A panic must degrade only its own slot, leaving the other
    /// providers in the same sweep to resolve normally.
    #[tokio::test]
    async fn panicking_provider_is_contained_and_others_still_resolve() {
        struct PanicProvider;
        #[async_trait]
        impl UsageProvider for PanicProvider {
            fn name(&self) -> &str {
                "boom"
            }
            async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
                panic!("provider blew up mid-fetch");
            }
        }
        let reg = Registry::new(vec![
            Box::new(PanicProvider),
            Box::new(StubProvider {
                name: "codex",
                cookie: false,
                ok: true,
            }),
        ]);

        // The sweep must COMPLETE despite the panicking provider.
        tick(&reg).await;

        let out = reg.get_usage(None).await;
        let boom = out.iter().find(|e| e.provider == "boom").unwrap();
        assert!(
            boom.error.is_some(),
            "panicked provider degrades its own slot"
        );
        let codex = out.iter().find(|e| e.provider == "codex").unwrap();
        assert!(codex.error.is_none(), "healthy provider still resolved");
        // The panic is a non-transient failure → the slot is degraded, not stale.
        let h = reg.health();
        assert!(h.degraded.contains(&"boom".to_string()));
        assert_eq!(h.fresh, 1); // codex
    }

    #[tokio::test]
    async fn cancelled_tick_leaves_slots_pending_without_hanging() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let reg = Registry::new(vec![Box::new(BlockingProvider {
            name: "codex",
            gate: gate.clone(),
        })]);
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancelled: the fetch select short-circuits
                         // Must return promptly despite the provider being a blocking one.
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            reg.refresh_tick(&cancel),
        )
        .await
        .expect("cancelled tick returned promptly");
        // Slot stays Pending (never resolved).
        assert!(reg.get_usage(None).await.is_empty());
    }
}
