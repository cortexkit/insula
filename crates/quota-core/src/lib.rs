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
pub mod codex_resets;
pub mod config;
pub mod copilot;
pub mod credential_source;
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
pub mod kimi_for_coding;
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
pub mod sub2api;
pub mod synthetic;
pub mod vault_handles;
pub mod warp;
pub mod zai;

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{stream, StreamExt};
use tokio_util::sync::CancellationToken;

use credential_source::CredentialSource;
use health::HealthSnapshot;
use model::ProviderUsage;
use provider::{CredentialHandle, FetchAttempt, UsageProvider};
use refresh::{
    next_slot_after_attempt, next_slot_after_unverified_failure, AttemptSequence, Incarnation,
    ProviderSlot, SlotStatus, STALL_HORIZON,
};
use store::{SlotKey, SlotStore};

enum FetchOutcome {
    Attempt(Box<FetchAttempt>),
    TimedOut,
    Panicked,
    Cancelled,
}

struct RegisteredProvider {
    name: String,
    cookie_based: bool,
    fetcher: Arc<dyn UsageProvider>,
}

struct DueCandidate {
    provider_index: usize,
    key: SlotKey,
    incarnation: Incarnation,
}

struct DueUnit {
    provider_index: usize,
    key: SlotKey,
    prev: ProviderSlot,
    attempt_sequence: AttemptSequence,
}

fn select_due_round_robin(
    providers: &[RegisteredProvider],
    snapshot: Vec<(SlotKey, ProviderSlot)>,
    now: Instant,
    last_admitted_provider: Option<usize>,
) -> (Vec<DueCandidate>, Option<usize>) {
    let provider_index: HashMap<&str, usize> = providers
        .iter()
        .enumerate()
        .map(|(index, provider)| (provider.name.as_str(), index))
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
            .sort_by(|(left, _), (right, _)| left.handle.sort_cmp(&right.handle));
    }

    let provider_count = providers.len();
    let first_provider = last_admitted_provider
        .map(|index| (index + 1) % provider_count.max(1))
        .unwrap_or(0);
    let mut admitted = Vec::with_capacity(refresh::CONCURRENCY_CAP);
    let mut last_admitted = last_admitted_provider;
    while admitted.len() < refresh::CONCURRENCY_CAP && provider_count > 0 {
        let mut progressed = false;
        for offset in 0..provider_count {
            let provider_index = (first_provider + offset) % provider_count;
            let Some((key, slot)) = queues[provider_index].pop_front() else {
                continue;
            };
            admitted.push(DueCandidate {
                provider_index,
                key,
                incarnation: slot.incarnation,
            });
            last_admitted = Some(provider_index);
            progressed = true;
            if admitted.len() == refresh::CONCURRENCY_CAP {
                break;
            }
        }
        if !progressed {
            break;
        }
    }
    (admitted, last_admitted)
}

fn service_rank(status: SlotStatus) -> u8 {
    match status {
        SlotStatus::Fresh => 0,
        SlotStatus::StaleTransient => 1,
        SlotStatus::Degraded => 2,
        SlotStatus::Pending => 3,
    }
}

fn relax_usage_for_read(entry: &mut ProviderUsage) {
    let Some(usage) = entry.usage.as_mut() else {
        return;
    };
    // The effective percent consumers pace on becomes 0 (a banked reset
    // guarantees the window resets before the wall), while the
    // provider-reported truth moves to `raw_used_percent` so human-facing
    // UIs can show real usage alongside the effective number. Windows
    // already at 0 stay un-annotated: there is no divergence to surface.
    for window in [
        usage.primary.as_mut(),
        usage.secondary.as_mut(),
        usage.tertiary.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        if window.used_percent != 0.0 {
            window.raw_used_percent = Some(window.used_percent);
            window.used_percent = 0.0;
        }
    }
    if let Some(extra) = usage.extra_rate_windows.as_mut() {
        for window in extra
            .iter_mut()
            .filter_map(|extra_window| extra_window.window.as_mut())
        {
            if window.used_percent != 0.0 {
                window.raw_used_percent = Some(window.used_percent);
                window.used_percent = 0.0;
            }
        }
    }
}

/// Provider registry with a cache-only read path and one background refresher.
pub struct Registry {
    providers: Vec<RegisteredProvider>,
    store: Mutex<SlotStore>,
    last_admitted_provider: Mutex<Option<usize>>,
    credential_source: Option<Arc<dyn CredentialSource>>,
}

impl Registry {
    /// Build a registry and cache provider metadata before the scheduler starts.
    /// Hot paths never call provider-owned naming or classification methods.
    pub fn new(providers: Vec<Box<dyn UsageProvider>>) -> Self {
        let providers = providers
            .into_iter()
            .enumerate()
            .map(|(index, provider)| {
                let name =
                    std::panic::catch_unwind(AssertUnwindSafe(|| provider.name().to_owned()))
                        .unwrap_or_else(|_| format!("invalid-provider-{index}"));
                let cookie_based =
                    std::panic::catch_unwind(AssertUnwindSafe(|| provider.is_cookie_based()))
                        .unwrap_or(false);
                RegisteredProvider {
                    name,
                    cookie_based,
                    fetcher: Arc::from(provider),
                }
            })
            .collect();
        Self {
            providers,
            store: Mutex::new(SlotStore::new(Instant::now())),
            last_admitted_provider: Mutex::new(None),
            credential_source: None,
        }
    }
    /// The default registry: every provider we support.
    pub fn with_defaults(
        config: config::QuotaConfig,
        credential_source: Option<Arc<dyn CredentialSource>>,
    ) -> Self {
        let vault_handle_loader = Arc::new(vault_handles::VaultHandleLoader::from_env());
        let mut registry = Self::new(vec![
            Box::new(codex::CodexProvider::new_with_handle_loader(
                config.codex,
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(anthropic::AnthropicProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(antigravity::AntigravityProvider::new()),
            Box::new(codebuff::CodebuffProvider::new()),
            Box::new(copilot::CopilotProvider::new()),
            Box::new(cursor::CursorProvider::new()),
            Box::new(doubao::DoubaoProvider::new()),
            Box::new(elevenlabs::ElevenLabsProvider::new()),
            Box::new(factory::FactoryProvider::new()),
            Box::new(gemini::GeminiProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(grok::GrokProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(jetbrains::JetBrainsProvider::new()),
            Box::new(kimi::KimiProvider::new()),
            Box::new(
                kimi_for_coding::KimiForCodingProvider::new_with_handle_loader(
                    credential_source.clone(),
                    Arc::clone(&vault_handle_loader),
                ),
            ),
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
            Box::new(sub2api::Sub2ApiProvider::new()),
            Box::new(warp::WarpProvider::new()),
            Box::new(synthetic::SyntheticProvider::new()),
            Box::new(zai::ZaiProvider::new()),
            Box::new(kilo::KiloProvider::new()),
            Box::new(alibaba::AlibabaProvider::new()),
            Box::new(amp::AmpProvider::new()),
        ]);
        registry.credential_source = credential_source;
        registry
    }

    pub fn credential_source_wired(&self) -> bool {
        self.credential_source.is_some()
    }

    /// Provider names registered, for discovery and observability.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect()
    }

    /// Serve usage exclusively from active slot snapshots.
    ///
    /// If any active handle lacks a resolved account label, one unlabeled
    /// representative is selected by freshness and health, preferring local on a
    /// tie. Once every handle resolves, entries are labeled and duplicate accounts
    /// are collapsed. A label transition without successful usage remains
    /// unavailable.
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
            .map(|(index, provider)| (provider.name.as_str(), index))
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
                .then_with(|| left.handle.sort_cmp(&right.handle))
        });

        let read_now = Instant::now();
        let mut out = Vec::new();
        for provider in &self.providers {
            let name = provider.name.as_str();
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
                let primary = slots
                    .iter()
                    .filter(|(_, slot)| !slot.label_in_flux)
                    .min_by(|(left_key, left_slot), (right_key, right_slot)| {
                        let rank = |slot: &ProviderSlot| {
                            let healthy = slot.entry.as_ref().is_some_and(|entry| {
                                entry.error.is_none() && entry.usage.is_some()
                            });
                            match (healthy, slot.is_fresh(read_now), slot.entry.is_some()) {
                                (true, true, _) => 0u8,
                                (true, false, _) => 1,
                                (false, _, true) => 2,
                                (false, _, false) => 3,
                            }
                        };
                        rank(left_slot)
                            .cmp(&rank(right_slot))
                            .then_with(|| {
                                right_key.handle.is_local().cmp(&left_key.handle.is_local())
                            })
                            .then_with(|| {
                                left_key
                                    .handle
                                    .stable_id()
                                    .cmp(right_key.handle.stable_id())
                            })
                    })
                    .map(|(_, slot)| slot);
                if let Some(primary) = primary {
                    if let Some(mut entry) = primary.entry.clone() {
                        entry.account = None;
                        if primary.relax_eligible && primary.is_fresh(read_now) {
                            relax_usage_for_read(&mut entry);
                        }
                        out.push(entry);
                    }
                }
                continue;
            }

            let mut candidates: HashMap<String, (&SlotKey, &ProviderSlot)> = HashMap::new();
            for (key, slot) in slots {
                if slot.label_in_flux || slot.entry.is_none() {
                    continue;
                }
                let Some(account_id) = slot.account_id() else {
                    continue;
                };
                let should_replace = match candidates.get(account_id) {
                    Some((_, current)) => service_rank(slot.status) < service_rank(current.status),
                    None => true,
                };
                if should_replace {
                    candidates.insert(account_id.to_string(), (key, slot));
                }
            }
            let mut selected: Vec<_> = candidates.into_iter().collect();
            selected.sort_by(|(_, (left, _)), (_, (right, _))| left.handle.sort_cmp(&right.handle));
            for (account_id, (_, slot)) in selected {
                if let Some(mut entry) = slot.entry.clone() {
                    entry.account = Some(account_id);
                    if slot.relax_eligible && slot.is_fresh(read_now) {
                        relax_usage_for_read(&mut entry);
                    }
                    out.push(entry);
                }
            }
        }
        out
    }

    /// Run one bounded scheduler turn using the production fetch deadline.
    pub async fn refresh_tick(&self, cancel: &CancellationToken) {
        self.refresh_tick_with_deadline(cancel, refresh::FETCH_DEADLINE)
            .await;
    }

    async fn refresh_tick_with_deadline(
        &self,
        cancel: &CancellationToken,
        fetch_deadline: Duration,
    ) {
        let turn_start = Instant::now();
        let enumerated: Vec<Option<Vec<CredentialHandle>>> = self
            .providers
            .iter()
            .map(|provider| {
                match std::panic::catch_unwind(AssertUnwindSafe(|| provider.fetcher.handles())) {
                    Ok(Ok(mut handles)) => {
                        handles.sort_by(CredentialHandle::sort_cmp);
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
                    store.reconcile(&provider.name, handles, turn_start);
                }
            }
            store.mark_tick(turn_start);
            store.snapshot()
        };
        let candidates = {
            let mut cursor = self
                .last_admitted_provider
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let (candidates, last_admitted) =
                select_due_round_robin(&self.providers, snapshot, turn_start, *cursor);
            *cursor = last_admitted;
            candidates
        };
        let due: Vec<_> = {
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            candidates
                .into_iter()
                .filter_map(|candidate| {
                    store.admit(&candidate.key, candidate.incarnation).map(
                        |(prev, attempt_sequence)| DueUnit {
                            provider_index: candidate.provider_index,
                            key: candidate.key,
                            prev,
                            attempt_sequence,
                        },
                    )
                })
                .collect()
        };
        if due.is_empty() {
            return;
        }

        let mut fetches = stream::iter(due.into_iter().map(|unit| {
            let provider = Arc::clone(&self.providers[unit.provider_index].fetcher);
            async move {
                let attempt_start = Instant::now();
                let handle = unit.key.handle.clone();
                // Broader blocking-I/O isolation is deferred until providers use the future credential store.
                let mut task = tokio::spawn(async move { provider.fetch_handle(&handle).await });
                let outcome = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        task.abort();
                        let _ = task.await;
                        FetchOutcome::Cancelled
                    },
                    result = tokio::time::timeout(fetch_deadline, &mut task) => match result {
                        Ok(Ok(attempt)) => FetchOutcome::Attempt(Box::new(attempt)),
                        Ok(Err(_join_error)) => FetchOutcome::Panicked,
                        Err(_elapsed) => {
                            task.abort();
                            let _ = task.await;
                            FetchOutcome::TimedOut
                        }
                    },
                };
                (unit, attempt_start, Instant::now(), outcome)
            }
        }))
        .buffer_unordered(refresh::CONCURRENCY_CAP);

        while let Some((unit, attempt_start, completed_at, outcome)) = fetches.next().await {
            let next = match outcome {
                FetchOutcome::Attempt(attempt) => next_slot_after_attempt(
                    &unit.prev,
                    &unit.key.provider,
                    *attempt,
                    attempt_start,
                    completed_at,
                ),
                FetchOutcome::TimedOut => next_slot_after_unverified_failure(
                    &unit.prev,
                    &unit.key.provider,
                    provider::FetchError::Upstream(format!(
                        "fetch exceeded {}ms deadline",
                        fetch_deadline.as_millis()
                    )),
                    attempt_start,
                    completed_at,
                ),
                FetchOutcome::Panicked => next_slot_after_unverified_failure(
                    &unit.prev,
                    &unit.key.provider,
                    provider::FetchError::Decode("provider fetch panicked".to_string()),
                    attempt_start,
                    completed_at,
                ),
                FetchOutcome::Cancelled => continue,
            };
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.publish_if_current(
                &unit.key,
                unit.prev.incarnation,
                unit.attempt_sequence,
                next,
            );
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
        let providers_total = self.providers.len();
        let cookie_cohort_total = self
            .providers
            .iter()
            .filter(|provider| provider.cookie_based)
            .count();

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
            let name = provider.name.as_str();
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
                if provider.cookie_based {
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
