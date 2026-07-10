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

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures_util::{stream, FutureExt, StreamExt};
use tokio_util::sync::CancellationToken;

use health::HealthSnapshot;
use model::ProviderUsage;
use provider::{CredentialHandle, FetchAttempt, UsageProvider};
use refresh::{next_slot_after_attempt, ProviderSlot, SlotStatus, STALL_HORIZON};
use store::{SlotKey, SlotStore};

enum FetchOutcome {
    Attempt(Box<FetchAttempt>),
    TimedOut,
    Panicked,
    Cancelled,
}

struct DueUnit {
    provider_index: usize,
    key: SlotKey,
    prev: ProviderSlot,
}

fn select_due_round_robin(
    providers: &[Box<dyn UsageProvider>],
    snapshot: Vec<(SlotKey, ProviderSlot)>,
    now: Instant,
) -> Vec<DueUnit> {
    let provider_index: HashMap<&str, usize> = providers
        .iter()
        .enumerate()
        .map(|(index, provider)| (provider.name(), index))
        .collect();
    let mut queues: Vec<VecDeque<(SlotKey, ProviderSlot)>> =
        (0..providers.len()).map(|_| VecDeque::new()).collect();

    for (key, slot) in snapshot {
        if slot.next_due_at > now {
            continue;
        }
        if let Some(&index) = provider_index.get(key.provider.as_str()) {
            queues[index].push_back((key, slot));
        }
    }
    for queue in &mut queues {
        queue
            .make_contiguous()
            .sort_by(|(left, _), (right, _)| left.handle.cmp(&right.handle));
    }

    let mut admitted = Vec::with_capacity(refresh::CONCURRENCY_CAP);
    while admitted.len() < refresh::CONCURRENCY_CAP {
        let mut progressed = false;
        for (provider_index, queue) in queues.iter_mut().enumerate() {
            let Some((key, prev)) = queue.pop_front() else {
                continue;
            };
            admitted.push(DueUnit {
                provider_index,
                key,
                prev,
            });
            progressed = true;
            if admitted.len() == refresh::CONCURRENCY_CAP {
                break;
            }
        }
        if !progressed {
            break;
        }
    }
    admitted
}

/// Provider registry with a cache-only read path and one background refresher.
pub struct Registry {
    providers: Vec<Box<dyn UsageProvider>>,
    store: Mutex<SlotStore>,
}

impl Registry {
    /// Build a registry. Handle enumeration is deferred to the first scheduler
    /// turn so construction never performs provider-specific configuration reads.
    pub fn new(providers: Vec<Box<dyn UsageProvider>>) -> Self {
        Self {
            providers,
            store: Mutex::new(SlotStore::new(Instant::now())),
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

    /// Provider names registered, for discovery and observability.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers
            .iter()
            .map(|provider| provider.name())
            .collect()
    }

    /// Serve usage exclusively from active slot snapshots.
    ///
    /// If any active handle lacks a resolved account label, only the stable first
    /// handle is eligible and its entry is unlabeled. Once every handle resolves,
    /// entries are labeled and duplicate accounts are collapsed. A label
    /// transition without successful usage remains unavailable.
    pub async fn get_usage(&self, provider_filter: Option<&str>) -> Vec<ProviderUsage> {
        let mut snapshot = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.snapshot()
        };
        let provider_index: HashMap<&str, usize> = self
            .providers
            .iter()
            .enumerate()
            .map(|(index, provider)| (provider.name(), index))
            .collect();
        snapshot.sort_by(|(left, _), (right, _)| {
            let left_index = provider_index
                .get(left.provider.as_str())
                .copied()
                .unwrap_or(usize::MAX);
            let right_index = provider_index
                .get(right.provider.as_str())
                .copied()
                .unwrap_or(usize::MAX);
            left_index
                .cmp(&right_index)
                .then_with(|| left.handle.cmp(&right.handle))
        });

        let mut out = Vec::new();
        for provider in &self.providers {
            let name = provider.name();
            if provider_filter.is_some_and(|filter| filter != name) {
                continue;
            }
            let slots: Vec<_> = snapshot
                .iter()
                .filter(|(key, _)| key.provider == name)
                .collect();
            if slots.is_empty() {
                continue;
            }

            let all_resolved = slots.iter().all(|(_, slot)| slot.account_id().is_some());
            if !all_resolved {
                let (_, primary) = slots[0];
                if primary.label_in_flux {
                    continue;
                }
                if let Some(mut entry) = primary.entry.clone() {
                    entry.account = None;
                    out.push(entry);
                }
                continue;
            }

            let mut emitted_accounts = HashSet::new();
            for (_, slot) in slots {
                if slot.label_in_flux {
                    continue;
                }
                let Some(account_id) = slot.account_id() else {
                    continue;
                };
                if !emitted_accounts.insert(account_id.to_string()) {
                    continue;
                }
                if let Some(mut entry) = slot.entry.clone() {
                    entry.account = Some(account_id.to_string());
                    out.push(entry);
                }
            }
        }
        out
    }

    /// Run one bounded scheduler turn.
    ///
    /// Every provider is enumerated before reconciliation. Failed enumeration
    /// retains that provider's last-known-good active set. Due handles are
    /// admitted round-robin across providers up to the concurrency cap.
    pub async fn refresh_tick(&self, cancel: &CancellationToken) {
        let turn_start = Instant::now();
        let enumerated: Vec<Option<Vec<CredentialHandle>>> = self
            .providers
            .iter()
            .map(|provider| {
                match std::panic::catch_unwind(AssertUnwindSafe(|| provider.handles())) {
                    Ok(Ok(mut handles)) => {
                        handles.sort();
                        handles.dedup();
                        Some(handles)
                    }
                    Ok(Err(_)) | Err(_) => None,
                }
            })
            .collect();

        let snapshot = {
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (provider, handles) in self.providers.iter().zip(enumerated.iter()) {
                if let Some(handles) = handles {
                    store.reconcile(provider.name(), handles, turn_start);
                }
            }
            store.mark_tick(turn_start);
            store.snapshot()
        };
        let due = select_due_round_robin(&self.providers, snapshot, turn_start);
        if due.is_empty() {
            return;
        }

        let mut fetches = stream::iter(due.into_iter().map(|unit| {
            let provider = &self.providers[unit.provider_index];
            async move {
                let attempt_start = Instant::now();
                let fetch =
                    AssertUnwindSafe(provider.fetch_handle(&unit.key.handle)).catch_unwind();
                let outcome = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => FetchOutcome::Cancelled,
                    result = tokio::time::timeout(refresh::FETCH_DEADLINE, fetch) => match result {
                        Ok(Ok(attempt)) => FetchOutcome::Attempt(Box::new(attempt)),
                        Ok(Err(_)) => FetchOutcome::Panicked,
                        Err(_) => FetchOutcome::TimedOut,
                    },
                };
                (unit, attempt_start, Instant::now(), outcome)
            }
        }))
        .buffer_unordered(refresh::CONCURRENCY_CAP);

        while let Some((unit, attempt_start, completed_at, outcome)) = fetches.next().await {
            let attempt = match outcome {
                FetchOutcome::Attempt(attempt) => *attempt,
                FetchOutcome::TimedOut => FetchAttempt::failure(
                    None,
                    None,
                    provider::FetchError::Upstream(format!(
                        "fetch exceeded {}s deadline",
                        refresh::FETCH_DEADLINE.as_secs()
                    )),
                ),
                FetchOutcome::Panicked => FetchAttempt::failure(
                    None,
                    None,
                    provider::FetchError::Decode("provider fetch panicked".to_string()),
                ),
                FetchOutcome::Cancelled => continue,
            };
            let next = next_slot_after_attempt(
                &unit.prev,
                &unit.key.provider,
                attempt,
                attempt_start,
                completed_at,
            );
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.publish_if_current(&unit.key, unit.prev.incarnation, next);
        }
    }

    /// Drive bounded scheduler turns until cancellation.
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

    /// Sleep until the earliest active handle is due, bounded so discovery keeps
    /// running even when every known handle is backed off.
    fn sleep_until_next_due(&self, now: Instant) -> Duration {
        let snapshot = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.snapshot()
        };
        snapshot
            .iter()
            .map(|(_, slot)| slot.next_due_at)
            .min()
            .map(|due| {
                due.saturating_duration_since(now)
                    .min(refresh::MAX_TICK_SLEEP)
            })
            .unwrap_or(refresh::MAX_TICK_SLEEP)
    }

    /// Aggregate handle state at provider level.
    ///
    /// A provider is degraded only when every active handle is degraded; one
    /// usable account keeps the provider in a serving bucket.
    pub fn health(&self) -> HealthSnapshot {
        let cookie_names: HashSet<&str> = self
            .providers
            .iter()
            .filter(|provider| provider.is_cookie_based())
            .map(|provider| provider.name())
            .collect();
        let providers_total = self.providers.len();
        let cookie_cohort_total = cookie_names.len();

        let (snapshot, last_tick_at, created_at) = match self.store.lock() {
            Ok(store) => (store.snapshot(), store.last_tick_at(), store.created_at()),
            Err(_) => return HealthSnapshot::poisoned(providers_total, cookie_cohort_total),
        };
        let now = Instant::now();
        let mut fresh = 0;
        let mut stale = 0;
        let mut degraded = Vec::new();
        let mut cookie_cohort_degraded = Vec::new();

        for provider in &self.providers {
            let name = provider.name();
            let slots: Vec<_> = snapshot
                .iter()
                .filter(|(key, _)| key.provider == name)
                .map(|(_, slot)| slot)
                .collect();
            if slots.is_empty() {
                continue;
            }
            let has_fresh = slots
                .iter()
                .any(|slot| slot.status == SlotStatus::Fresh && slot.is_fresh(now));
            let has_stale = slots.iter().any(|slot| {
                slot.status == SlotStatus::StaleTransient
                    || (slot.status == SlotStatus::Fresh && !slot.is_fresh(now))
            });
            let all_degraded = slots.iter().all(|slot| slot.status == SlotStatus::Degraded);

            if has_fresh {
                fresh += 1;
            } else if has_stale {
                stale += 1;
            } else if all_degraded {
                degraded.push(name.to_string());
                if cookie_names.contains(name) {
                    cookie_cohort_degraded.push(name.to_string());
                }
            }
        }

        let last_tick_age = last_tick_at.map(|tick| now.saturating_duration_since(tick));
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
mod tests;
