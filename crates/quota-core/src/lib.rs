//! quota-core — provider fetchers, normalization, and the background refresher.
//!
//! Fetches each provider's own reporting and normalizes it into two kinds of
//! statement: `RateWindow`s, a share of a period with a reset, and prepaid
//! pools, an amount with no period at all. A provider may publish either or
//! both.
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
pub mod clinepass;
pub mod codebuff;
pub mod codex;
pub mod codex_resets;
pub mod config;
mod cookie_vault;
pub mod copilot;
pub mod credential_source;
pub mod cursor;
pub mod deepseek;
pub mod doubao;
pub mod elevenlabs;
pub mod env;
pub mod factory;
pub mod gemini;
pub mod grok;
pub mod health;
pub mod http;
pub mod jetbrains;
mod json_scan;
pub mod kilo;
pub mod kimi;
pub mod kimi_for_coding;
pub mod llmproxy;
#[cfg(test)]
pub mod loopback;
pub mod manus;
pub mod mimo;
pub mod minimax;
pub mod model;
pub mod money;
pub mod neuralwatt;
pub mod ollama;
pub mod opencode;
pub mod opencode_auth;
pub mod opencodego;
pub mod openrouter;
pub mod provider;
pub mod qoder;
pub mod quota_drop;
pub mod qwen_cloud;
pub mod refresh;
pub mod sakana;
pub mod stepfun;
pub mod store;
pub mod sub2api;
pub mod synthetic;
pub mod text;
pub mod vault_handles;
pub mod warp;
pub mod wire_sanity;
pub mod zai;
pub mod zenmux;

/// The one prefix used for every production stderr emission.
///
/// This drifted into two spellings -- 27 sites saying `[ck-quota]` and 2 saying
/// `[insula]`, the majority naming a binary that had not existed since the
/// rename -- because nothing reads a log tag to disagree with it.
///
/// NOT LOAD-BEARING FOR TOOLING, and the note it replaces said it was. When this
/// was unified on 2026-09-04, supervised modules shared one log file and
/// `ck module logs <id>` was expected to select on this string; the same day the
/// daemon owner ruled the opposite, because two lanes disagreeing about what a
/// module's output IS is the fault rather than the inconvenience. The supervisor
/// already pipes each child's stderr, so it will own a per-module file and a line
/// is attributed by the pipe it arrived on -- which cannot disagree with itself,
/// where a tag can and did.
///
/// So the tag is now a courtesy to a human grepping the legacy shared file, and
/// that is reason enough to keep it consistent: a tag naming a dead binary sends
/// someone looking for output that is there under another name. It is NOT reason
/// to build anything on it, and the id fence in `quota-module` exists to keep it
/// honest rather than because a tool depends on it.
///
/// Recorded because the previous sentence here was true when written and false
/// within the day, and a justification that quietly stops holding is how a rule
/// outlives its reason.
pub const LOG_TAG: &str = "[insula]";

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{stream, StreamExt};
use tokio_util::sync::CancellationToken;

use credential_source::{CredentialSource, VaultCapability};
use health::HealthSnapshot;
use model::ProviderUsage;
use provider::{CredentialHandle, FetchAttempt, UsageProvider};
use refresh::{
    next_slot_after_attempt, next_slot_after_unverified_failure, AttemptSequence, Incarnation,
    ProviderSlot, SlotStatus, FETCH_BLACKOUT_HORIZON, STALL_HORIZON,
};
use store::{AuthoritativeHandles, SlotKey, SlotStore};

#[cfg(test)]
thread_local! {
    static BEFORE_FETCHED_AT_FORMAT: std::cell::RefCell<Option<Box<dyn Fn()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn before_fetched_at_format() {
    BEFORE_FETCHED_AT_FORMAT.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook();
        }
    });
}

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
    // Most-overdue first, then stable handle order as a deterministic tie-break.
    //
    // Sorting by handle alone makes admission order identical every turn, so the
    // same handle is taken each time its provider is visited. While a provider has
    // more due handles than it gets admissions per turn, a later handle can then be
    // passed over indefinitely: it never becomes "next", because the earlier handle
    // keeps returning to the front as it falls due again. Ordering by due time
    // instead makes waiting itself advance a handle's position, so a passed-over
    // handle overtakes one that was just refreshed. A never-attempted handle carries
    // the oldest due time of all, so it is admitted first rather than last.
    for queue in &mut queues {
        queue
            .make_contiguous()
            .sort_by(|(left, left_slot), (right, right_slot)| {
                left_slot
                    .next_due_at
                    .cmp(&right_slot.next_due_at)
                    .then_with(|| left.handle.sort_cmp(&right.handle))
            });
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

/// Format a timestamp so the same instant always produces the same string.
///
/// `to_rfc3339` picks its precision from the value: an instant with a whole
/// number of seconds prints no fractional part, one whose nanoseconds end in
/// three zeros prints six digits, and everything else prints nine. All three
/// are valid RFC 3339 and all three are the same formatter, so the variation
/// looks like a code path difference when it is arithmetic.
///
/// That is invisible while a timestamp is only a freshness hint and load-bearing
/// the moment anything keys on it: if one instant has several spellings then
/// string equality stops meaning instant equality, and a dedupe or cache key on
/// the raw string admits duplicates a parsed comparison would have caught.
/// Pinning the precision makes the string canonical.
fn rfc3339_canonical(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
}

/// Apply the banked-reset relaxation to an entry about to be published, if this
/// slot has earned it.
///
/// The eligibility test lives here rather than at the call sites deliberately.
/// This transform publishes a zero where the provider reported real usage, so a
/// caller that applies it without the gate tells consumers an exhausted account
/// is idle. With the check outside, that mistake is a missing `if` at a new call
/// site and compiles cleanly; with it inside, the transform cannot be reached
/// without the slot being consulted.
///
/// Both conditions matter. `relax_eligible` records that the fetch which
/// produced this entry had banked credits and had not spent one; `is_fresh`
/// bounds how long that finding is trusted, because a stale slot's eligibility
/// describes an observation that may no longer hold.
/// Disclose that an entry is a preserved reading served through a failure.
///
/// Stamped in the same walk as `fetched_at`, which is the ONE place every
/// emitted entry passes through. A per-entry field set at the three emission
/// sites instead would be set at two of them within a year: a consumer reading a
/// bare entry cannot tell which path produced it, so a missing disclosure reads
/// as a fresh entry rather than as an unvisited branch.
///
/// `since` is the start of the current failure run, not the last attempt. During
/// a long outage the latter is always seconds ago, which would report the entry
/// as freshly checked at the moment it is least trustworthy -- the conflation
/// this field exists to prevent.
fn staleness_disclosure(slot: &ProviderSlot) -> Option<cortexkit_provider_usage::Stale> {
    if slot.status != SlotStatus::StaleTransient {
        return None;
    }
    // Same reasoning as `fetchedAt`: the wall reading taken when the failure run
    // began, rather than a monotonic age that a suspend does not count.
    slot.failing_since_wall
        .map(|since| cortexkit_provider_usage::Stale {
            since: rfc3339_canonical(since),
            class: slot.error_class.map(|class| class.to_string()),
        })
}

fn relax_usage_for_read(entry: &mut ProviderUsage, slot: &ProviderSlot, read_now: Instant) {
    if !(slot.relax_eligible && slot.is_fresh(read_now)) {
        return;
    }
    let Some(usage) = entry.usage.as_mut() else {
        return;
    };
    // The effective percent consumers pace on becomes 0 (a banked reset
    // guarantees the window resets before the wall), while the
    // provider-reported truth moves to `raw_used_percent` so human-facing
    // UIs can show real usage alongside the effective number. Windows
    // already at 0 stay un-annotated: there is no divergence to surface.
    //
    // Enumerated through the shared helper so a slot added to the wire type
    // cannot be missed here: publishing a raw percent as the effective one
    // would have a consumer pace against a wall this account's credits are
    // about to remove.
    for window in model::windows_mut(usage) {
        if window.used_percent != 0.0 {
            window.raw_used_percent = Some(window.used_percent);
            window.used_percent = 0.0;
        }
    }
}

/// Map a CodexBar provider name to its canonical models.dev slug, when one
/// exists. Returns `None` for providers with no exact models.dev counterpart —
/// including `kimi`, whose www.kimi.com consumer subscription is distinct from
/// models.dev's `moonshotai`/`moonshotai-cn` developer APIs — where consumers
/// fall back to the CodexBar `provider` name. Kept total over providers that
/// HAVE a counterpart and absent otherwise, never guessed (astrocyte's pricing
/// joins key on this). Populated onto the wire as `apiProvider` so every
/// consumer keys on one canonical name instead of maintaining its own
/// CodexBar→canonical map.
fn api_provider_name(provider: &str) -> Option<&'static str> {
    match provider {
        "codex" => Some("openai"),
        "claude" => Some("anthropic"),
        "gemini" => Some("google"),
        "grok" => Some("xai"),
        "copilot" => Some("github-copilot"),
        "deepseek" => Some("deepseek"),
        "kimi-for-coding" => Some("kimi-for-coding"),
        "kilo" => Some("kilo"),
        "minimax" => Some("minimax"),
        "stepfun" => Some("stepfun"),
        "zai" => Some("zai"),
        "synthetic" => Some("synthetic"),
        "opencode" => Some("opencode"),
        "opencodego" => Some("opencode-go"),
        "openrouter" => Some("openrouter"),
        "ollama" => Some("ollama-cloud"),
        "sakana" => Some("sakana"),
        "neuralwatt" => Some("neuralwatt"),
        "zenmux" => Some("zenmux"),
        "alibaba" => Some("alibaba-coding-plan"),
        "qwen-cloud" => Some("alibaba-token-plan"),
        "mimo" => Some("xiaomi"),
        _ => None,
    }
}

/// One usage read: the entries, and which providers' account sets are complete.
///
/// A provider named in `complete_providers` has had its full account set
/// published in `entries`, so a consumer holding stored accounts for it may
/// replace that set -- including removing accounts the entries do not name.
///
/// A provider NOT named is **unknown, never empty**. Its entries are as usable
/// as any other; the module is only declining to say that they are all of them.
/// Treating absence as an empty account set is the destructive misreading this
/// exists to prevent.
///
/// The survivor set is every LABELLED entry of that provider, including degraded
/// ones. A degraded entry names an account that exists and is currently
/// unusable, so usability governs which windows to store, never which accounts
/// to keep -- deleting an account because it is unhealthy is the same data loss
/// in a different costume.
///
/// Only a consumer performing a destructive action should consult this. A
/// display-only consumer must not: gating what it shows on completeness would
/// hide live data for exactly the providers whose identity is momentarily
/// unsettled.
#[derive(Debug, Clone, Default)]
pub struct UsageSnapshot {
    pub entries: Vec<ProviderUsage>,
    /// Provider registry names, in registry order, matching `entries`.
    ///
    /// Registry names rather than the optional canonical `apiProvider` slug: a
    /// provider without a counterpart has no slug at all, so keying on it would
    /// silently exclude that provider from ever being complete -- the same
    /// absence-as-answer failure this field exists to close.
    pub complete_providers: Vec<String>,
}

impl UsageSnapshot {
    /// Render this snapshot as the `usage.get` reply body.
    ///
    /// Lives here rather than at the wire boundary so that anything producing a
    /// reference envelope -- a consumer's pinned fixture, a diagnostic dump --
    /// goes through the same construction the module serves. A fixture built
    /// beside the real path rather than from it is free to drift, and a consumer
    /// pinned to a shape this module never emits passes forever while testing
    /// nothing.
    pub fn to_envelope(&self) -> serde_json::Value {
        serde_json::json!({
            "result": self.entries,
            "completeProviders": self.complete_providers,
        })
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
            Box::new(antigravity::AntigravityProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(codebuff::CodebuffProvider::new()),
            Box::new(copilot::CopilotProvider::new()),
            Box::new(cursor::CursorProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(deepseek::DeepSeekProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(doubao::DoubaoProvider::new()),
            Box::new(elevenlabs::ElevenLabsProvider::new()),
            Box::new(factory::FactoryProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
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
            Box::new(clinepass::ClinePassProvider::new()),
            Box::new(llmproxy::LlmProxyProvider::new()),
            Box::new(manus::ManusProvider::new()),
            Box::new(mimo::MimoProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(minimax::MinimaxProvider::new()),
            Box::new(neuralwatt::NeuralWattProvider::new()),
            Box::new(ollama::OllamaProvider::new()),
            Box::new(opencode::OpenCodeProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(opencodego::OpenCodeGoProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(openrouter::OpenRouterProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(qoder::QoderProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(qwen_cloud::QwenCloudProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(sakana::SakanaProvider::new()),
            Box::new(stepfun::StepFunProvider::new()),
            Box::new(sub2api::Sub2ApiProvider::new()),
            Box::new(warp::WarpProvider::new()),
            Box::new(synthetic::SyntheticProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
            Box::new(zai::ZaiProvider::new()),
            Box::new(zenmux::ZenMuxProvider::new()),
            Box::new(kilo::KiloProvider::new()),
            Box::new(alibaba::AlibabaProvider::new()),
            Box::new(amp::AmpProvider::new_with_handle_loader(
                credential_source.clone(),
                Arc::clone(&vault_handle_loader),
            )),
        ]);
        registry.credential_source = credential_source;
        registry
    }

    pub fn credential_source_wired(&self) -> bool {
        self.credential_source.is_some()
    }

    #[cfg(test)]
    pub(crate) fn attach_credential_source(&mut self, source: Arc<dyn CredentialSource>) {
        self.credential_source = Some(source);
    }

    /// The slot store, for diagnostics that must stage a specific slot state.
    ///
    /// A few states this module must answer for are reachable only through a
    /// sequence of upstream conditions -- an account whose identity stopped
    /// being confirmable, a credential replaced between two turns. Waiting for
    /// them costs a refresh interval each, so a tool that renders reference
    /// output for consumers stages them directly.
    ///
    /// Staging a slot bypasses the transitions that normally produce it, so a
    /// state set here is only as faithful as the setter. Everything downstream
    /// of the slot -- the emission gate, the completeness claim, the envelope --
    /// is the shipping path.
    pub fn slot_store(&self) -> &Mutex<SlotStore> {
        &self.store
    }

    /// Provider names registered, for discovery and observability.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect()
    }

    /// Names of the providers that authenticate with a browser session cookie.
    ///
    /// Derived from the same predicate the stale-login health metric counts, so
    /// a caller asking "who is in the cookie cohort" cannot get a different
    /// answer from the one the metric is sized against.
    pub fn cookie_based_provider_names(&self) -> Vec<&str> {
        self.providers
            .iter()
            .filter(|provider| provider.fetcher.is_cookie_based())
            .map(|provider| provider.name.as_str())
            .collect()
    }

    /// Serve usage exclusively from active slot snapshots, discarding the
    /// completeness claim.
    ///
    /// If any active handle lacks a resolved account label, one unlabeled
    /// representative is selected by freshness and health, preferring local on a
    /// tie. Once every handle resolves, entries are labeled and duplicate accounts
    /// are collapsed. A label transition without successful usage remains
    /// unavailable.
    pub async fn get_usage(&self, provider_filter: Option<&str>) -> Vec<ProviderUsage> {
        self.usage_snapshot(provider_filter).await.entries
    }

    /// Serve usage together with the providers whose account set is complete.
    ///
    /// The completeness claim is built by the SAME walk that emits the entries,
    /// rather than computed separately from the same slots. A second predicate
    /// over the same data is free to drift from the one that actually governs
    /// emission, and a claim that disagrees with the entries beside it is worse
    /// than no claim at all: it authorises a consumer to delete accounts the
    /// array does not support.
    pub async fn usage_snapshot(&self, provider_filter: Option<&str>) -> UsageSnapshot {
        let (mut snapshot, enumerated_ok) = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let enumerated_ok: Vec<bool> = self
                .providers
                .iter()
                .map(|provider| store.enumeration_succeeded(provider.name.as_str()))
                .collect();
            (store.snapshot(), enumerated_ok)
        };
        for (_, slot) in &mut snapshot {
            // Read before the mutable borrow: the disclosure describes the SLOT,
            // and the entry it lands on is a clone of an earlier successful read.
            // The WALL reading taken at the success, not a monotonic age added to
            // a startup anchor. The anchor form fell behind real time by exactly
            // any suspended duration, permanently, because `Instant` on macOS is
            // `CLOCK_UPTIME_RAW` -- a clock std documents as not incrementing
            // while the system is asleep. A lid-close made a healthy module
            // publish ancient timestamps until it was restarted.
            let fetched_at = slot.last_success_wall.map(rfc3339_canonical);
            let stale = staleness_disclosure(slot);
            if let Some(entry) = slot.entry.as_mut() {
                #[cfg(test)]
                before_fetched_at_format();
                entry.fetched_at = fetched_at;
                entry.stale = stale;
            }
        }
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
        let mut complete_providers = Vec::new();
        for (index, provider) in self.providers.iter().enumerate() {
            let name = provider.name.as_str();
            if provider_filter.is_some_and(|filter| filter != name) {
                continue;
            }
            // A provider can only claim completeness on the strength of a
            // handle enumeration that SUCCEEDED. A failed one keeps the previous
            // slots, which may name accounts that no longer exist and cannot
            // name any that were added, so it is silent about the account set
            // rather than confirming it.
            let enumeration_succeeded = enumerated_ok[index];
            let slots: Vec<_> = snapshot
                .iter()
                .filter(|(key, _)| key.provider == name)
                .collect();
            if slots.is_empty() {
                // A successful enumeration that returned no handles is a
                // statement that this provider has no accounts, and it is the
                // only way a consumer is ever authorised to clear one. Keying
                // completeness on having at least one slot instead would make
                // that unrepresentable -- the exact case the signal exists for.
                if enumeration_succeeded {
                    complete_providers.push(name.to_string());
                }
                continue;
            }

            // An unresolved handle only has to suppress its siblings when it
            // could BE one of them. A handle serving usage without an identity
            // may be the same account as a labeled sibling -- a local lane
            // beside a vault lane commonly is -- and publishing both would count
            // one account's capacity twice, which is worse than publishing it
            // once without a label.
            //
            // A handle carrying only an error has no capacity to double-count,
            // by construction: a degraded entry has no usage. So it cannot
            // create that ambiguity, and collapsing the provider for it costs
            // every healthy sibling its identity to guard against nothing. That
            // is the state a handle reaches when the credential behind it is
            // removed and the handle is left configured -- permanent, silent,
            // and previously enough to blind an entire provider.
            let ambiguous_capacity = slots.iter().any(|(_, slot)| {
                slot.account_id().is_none()
                    && slot
                        .entry
                        .as_ref()
                        .is_some_and(|entry| entry.usage.is_some())
            });
            if ambiguous_capacity {
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
                        entry.api_provider = api_provider_name(name).map(String::from);
                        relax_usage_for_read(&mut entry, primary, read_now);
                        out.push(entry);
                    }
                }
                continue;
            }

            let mut candidates: HashMap<String, (&SlotKey, &ProviderSlot)> = HashMap::new();
            // Set by the emission walk itself. A slot that is skipped here is an
            // account this response does not mention, and a consumer cannot tell
            // that from the account having been removed -- so any skip forfeits
            // the completeness claim.
            let mut skipped_a_slot = false;
            let mut unidentified: Vec<(&SlotKey, &ProviderSlot)> = Vec::new();
            for (key, slot) in slots {
                if slot.entry.is_none() {
                    skipped_a_slot = true;
                    continue;
                }
                if slot.label_in_flux {
                    // Identity is unverified, so this slot never contributes a
                    // LABELED entry and never counts toward completeness.
                    skipped_a_slot = true;
                    // A usage-bearing entry is withheld entirely: serving one
                    // account's readings under another's credential is the whole
                    // reason the flux fence exists.
                    //
                    // A VERDICT is not withheld. A degraded entry carries no
                    // account, no account_info and no usage, so there is nothing
                    // for it to mis-attribute -- and dropping it publishes the
                    // ABSENT shape for a credential this module reached and
                    // concluded was unusable. The contract rules those apart, and
                    // absence is the calmest reading a consumer can take: measured
                    // at 285 seconds of a dead credential looking not-yet-fetched
                    // (insula#8). It joins the unlabeled representatives below,
                    // which is where an entry with a real error and no identity
                    // already belongs.
                    if slot
                        .entry
                        .as_ref()
                        .is_some_and(|entry| entry.usage.is_none())
                    {
                        unidentified.push((key, slot));
                    }
                    continue;
                }
                let Some(account_id) = slot.account_id() else {
                    // Identity-less and carrying no usage, or this branch would
                    // not have been taken. Held back for a single unlabeled
                    // representative below rather than emitted per handle: two
                    // unlabeled entries for one provider are indistinguishable
                    // from two accounts to a consumer.
                    unidentified.push((key, slot));
                    skipped_a_slot = true;
                    continue;
                };
                // Prefer the better service status, then the more recent
                // observation. Two handles can resolve the SAME account (a local
                // lane beside a vault lane, or two vault handles), and when both
                // are serving stale data the newer snapshot is the one worth
                // showing: comparing status alone leaves them tied, so the winner
                // would be decided by handle order and could be arbitrarily older.
                //
                // Recency, not the reported figure. A snapshot is preferred
                // because it describes the account more recently, and the older
                // one may describe a state the account has already left. Usage
                // usually grows within a window -- so the older snapshot usually
                // understates pressure -- but a window that resets between the
                // two fetches makes the newer figure lower, and it is still the
                // one to serve. Choosing by the figure instead would pin the
                // account at its pre-reset percent until the older handle
                // succeeded again.
                let should_replace = match candidates.get(account_id) {
                    Some((_, current)) => {
                        let rank = service_rank(slot.status).cmp(&service_rank(current.status));
                        rank.then_with(|| current.last_success_at.cmp(&slot.last_success_at))
                            .is_lt()
                    }
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
                    entry.api_provider = api_provider_name(name).map(String::from);
                    relax_usage_for_read(&mut entry, slot, read_now);
                    out.push(entry);
                }
            }
            // One entry for every handle that resolved nothing, so the failure
            // stays visible without asserting how many handles are behind it.
            // Emitting it keeps a dead handle reportable to an operator; the
            // alternative is a provider that renders perfectly while quietly
            // holding a credential that can never resolve again.
            if let Some((_, slot)) = unidentified
                .into_iter()
                .min_by(|(left, _), (right, _)| left.handle.sort_cmp(&right.handle))
            {
                if let Some(mut entry) = slot.entry.clone() {
                    entry.account = None;
                    entry.api_provider = api_provider_name(name).map(String::from);
                    out.push(entry);
                }
            }
            // Every slot contributed its account, so the emitted accounts ARE
            // the resolved inventory -- compared as a set, since two handles
            // resolving one account are collapsed on purpose and a count would
            // read that deliberate dedup as a missing account forever.
            if enumeration_succeeded && !skipped_a_slot {
                complete_providers.push(name.to_string());
            }
        }
        // Applied once, to the assembled array, rather than at each of the
        // twenty-odd sites that compute a percent: a single missed guard there
        // would publish a null that costs every consumer the whole response.
        model::drop_uncomputable_windows(&mut out);
        // Same reasoning for length: an upstream-derived string long enough to
        // push the reply past the frame limit costs every provider's usage, not
        // just its own field.
        model::bound_wire_strings(&mut out);
        UsageSnapshot {
            entries: out,
            complete_providers,
        }
    }

    /// A page of recently observed quota drops.
    ///
    /// Cache-only, like every other read here: it returns what the refresher has
    /// already recorded and never waits for a sweep.
    ///
    /// `since` is a sequence from a previous page. A consumer that has never
    /// polled passes `None` and gets everything retained. A consumer whose cursor
    /// predates `oldest_retained` has lost records, and can tell.
    pub async fn drop_page(&self, since: Option<u64>) -> store::DropPage {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        store.drop_page(since)
    }

    /// Ask the vault whether a replaced record can end a non-transient backoff.
    ///
    /// Only vault-backed slots already waiting out the 300s floor are polled, and
    /// only at [`refresh::BASE_INTERVAL`]. A successful version advance makes the
    /// slot due for the fetch that follows in this turn. Any status failure leaves
    /// serving state untouched -- the poll is an accelerator, not a verdict.
    async fn poll_replaced_credentials(&self, now: Instant) {
        let Some(source) = self.credential_source.as_ref() else {
            return;
        };
        let eligible: Vec<(SlotKey, ProviderSlot, VaultCapability)> = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store
                .snapshot()
                .into_iter()
                .filter_map(|(key, slot)| {
                    let capability = key.handle.vault_capability()?.clone();
                    refresh::should_poll_credential_status(&slot, &key.handle, now)
                        .then_some((key, slot, capability))
                })
                .collect()
        };
        for (key, slot, capability) in eligible {
            let next = match source.status(&capability).await {
                Ok(status) => refresh::apply_credential_status(&slot, &status, now),
                Err(_) => refresh::slot_after_status_error(&slot, now),
            };
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.publish_if_current(&key, slot.incarnation, slot.attempt_sequence, next);
        }
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

        let authoritative =
            AuthoritativeHandles::new(self.providers.iter().zip(enumerated.iter()).filter_map(
                |(provider, handles)| {
                    handles
                        .as_deref()
                        .map(|handles| (provider.name.as_str(), handles))
                },
            ));
        {
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            store.reconcile_batch(&authoritative, turn_start);
            store.mark_tick(turn_start);
        }
        self.poll_replaced_credentials(turn_start).await;
        let snapshot = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                // A panic in our own fetch code, reported as ours: calling it a
                // decode failure would tell consumers the upstream sent a
                // malformed payload and send anyone investigating to the
                // provider's API rather than to this module.
                FetchOutcome::Panicked => next_slot_after_unverified_failure(
                    &unit.prev,
                    &unit.key.provider,
                    provider::FetchError::Internal("provider fetch panicked".to_string()),
                    attempt_start,
                    completed_at,
                ),
                FetchOutcome::Cancelled => continue,
            };
            // Whether this attempt made the slot ENTER stale-serving. Decided
            // here, where the transition is computed, and applied under the
            // store lock below so the counter is consistent with the slot it
            // describes. A slot already stale is a continuation, not a new
            // episode.
            let enters_stale = refresh::enters_stale_transient(&unit.prev, &next);
            // Whether this attempt saw the account's used percent go DOWN.
            // Decided here for the same reason as the line above: the comparison
            // needs both readings, and this is the only place that holds them.
            //
            // The identity check is the load-bearing argument, not the
            // percentages: comparing a reading of one account against a reading
            // of another fabricates a transition nobody made.
            //
            // THIS attempt failing is itself a not-comparable reason, and is
            // counted as one: a failed fetch produces no new reading, which is
            // exactly the case an operator needs distinguished from a quiet host.
            let observation = match next
                .entry
                .as_ref()
                .filter(|entry| entry.error.is_none())
                .and_then(|entry| entry.usage.as_ref())
            {
                Some(usage) => quota_drop::detect(
                    &unit.prev,
                    usage,
                    refresh::observations_differ(
                        unit.prev.observation.as_ref(),
                        next.observation.as_ref(),
                    ),
                    completed_at,
                    refresh::BASE_INTERVAL,
                ),
                None => quota_drop::DropObservation::NotComparable(
                    quota_drop::NotComparable::PriorReadingWasAnError,
                ),
            };
            let mut store = self
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if enters_stale {
                store.record_stale_episode(&unit.key.provider);
            }
            match observation {
                quota_drop::DropObservation::Drop(drop) => {
                    store.record_quota_drop(&unit.key.provider, drop.observed_continuously);
                }
                quota_drop::DropObservation::NoDrop => store.record_comparable_no_drop(),
                quota_drop::DropObservation::NotComparable(reason) => {
                    store.record_not_comparable(reason);
                }
            }
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

        let (
            snapshot,
            last_tick_at,
            created_at,
            stale_episodes,
            stale_episodes_by_provider,
            quota_drops_by_provider,
            quota_drops_observed_continuously,
            quota_comparisons_no_drop,
            quota_not_comparable,
        ) = match self.store.lock() {
            Ok(store) => (
                store.snapshot(),
                store.last_tick_at(),
                store.created_at(),
                store.stale_episodes(),
                store.stale_episodes_by_provider(),
                store.quota_drops_by_provider(),
                store.quota_drops_observed_continuously(),
                store.quota_comparisons_no_drop(),
                store.quota_not_comparable(),
            ),
            Err(_) => return HealthSnapshot::poisoned(providers_total, cookie_cohort_total),
        };
        let now = Instant::now();
        let mut fresh = 0;
        let mut stale = 0;
        let mut pending = 0;
        let mut degraded = Vec::new();
        let mut unconfigured = Vec::new();
        let mut without_handles = Vec::new();
        let mut cookie_logins_stale = Vec::new();
        let mut handles_without_account = Vec::new();

        for provider in &self.providers {
            let name = provider.name.as_str();
            let keyed: Vec<_> = snapshot
                .iter()
                .filter(|(key, _)| key.provider == name)
                .collect();
            let slots: Vec<&ProviderSlot> = keyed.iter().map(|(_, slot)| slot).collect();
            if slots.is_empty() {
                // Registered but holding no slots, so it contributes to no count
                // below. Report it by name rather than skipping it: otherwise the
                // buckets silently under-sum against providers_total, and a
                // provider whose credential enumeration keeps failing reads as an
                // absence instead of a problem.
                //
                // Before the refresher's first tick every provider legitimately
                // has no slots, so reporting then would name all of them for the
                // first few seconds after start.
                if last_tick_at.is_some() {
                    without_handles.push(name.to_string());
                }
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

            // A credential that reaches no account while its siblings serve. The
            // provider is genuinely healthy, so it lands in `fresh` or `stale`
            // and every count on this snapshot reads normal -- which is the
            // whole problem: this is the state a handle enters when the
            // credential behind it is deleted and the handle is left
            // configured, and it never clears on its own.
            //
            // Requires a serving sibling. A provider whose accounts are ALL
            // unresolved is already reported by `unconfigured` or `degraded`;
            // repeating it here would bury the case that nothing else names.
            //
            // Reported separately from the buckets rather than as one of them,
            // so the conservation identity is untouched: a provider counted here
            // is still counted in exactly one bucket.
            if (has_fresh || has_stale)
                && keyed.iter().any(|(key, slot)| {
                    // Vault handles only. One exists because somebody minted it,
                    // so a failing one is a credential that was configured and
                    // stopped working. Most providers also keep an implicit local
                    // lane -- an environment variable or a file path -- that
                    // exists whether or not anyone uses it, and on a host that
                    // does not, it fails with an absent credential and no
                    // identity while the vault lane beside it serves perfectly.
                    // Counting those named a provider whose only fault was
                    // shipping a lane nobody configured.
                    !key.handle.is_local()
                        && slot
                            .observation
                            .as_ref()
                            .and_then(|observation| observation.account_id.as_deref())
                            .is_none()
                        && slot
                            .entry
                            .as_ref()
                            .is_some_and(|entry| entry.error.is_some())
                })
            {
                handles_without_account.push(name.to_string());
            }

            if has_fresh {
                fresh += 1;
            } else if has_stale {
                stale += 1;
            } else if !all_degraded {
                // Serving nothing, and at least one handle has not finished its
                // first fetch. Reachable whenever there are more fetch units than
                // the concurrency cap admits per turn, so with this many providers
                // it is the ordinary state for the first few turns after a start.
                //
                // Counted rather than skipped: every provider must land in exactly
                // one bucket, or the buckets under-sum against providers_total and
                // a consumer asserting that identity sees an imbalance that means
                // nothing is wrong.
                pending += 1;
            } else if slots.iter().all(|slot| {
                slot.error_class
                    .is_some_and(provider::class_is_expected_absence)
            }) {
                // Every account of this provider failed for a reason that means
                // nothing is configured here. Separated from `degraded` so that
                // count stays an operator trigger: most adapters have no
                // credential on any given host, and folding them in makes a real
                // failure move the number by one against a baseline in the
                // twenties.
                //
                // Requires ALL slots to agree. One account with a broken
                // credential beside one that was never configured is a provider
                // somebody should look at.
                unconfigured.push(name.to_string());
            } else {
                degraded.push(name.to_string());
                // Only a stored login that stopped working counts. A cookie that
                // is simply absent means nobody signed in, which is permanent and
                // correct on a host that does not use the service, and an upstream
                // outage is not something re-authenticating would fix.
                //
                // Asked as "which classes count" rather than "which are excluded",
                // so a class added to the taxonomy later does not join this number
                // by default -- see `class_means_credential_stopped_working`.
                if provider.cookie_based
                    && slots.iter().any(|slot| {
                        slot.error_class
                            .is_some_and(provider::class_means_credential_stopped_working)
                    })
                {
                    cookie_logins_stale.push(name.to_string());
                }
            }
        }

        let last_tick_age = last_tick_at.map(|tick| now.saturating_duration_since(tick));
        let refresher_stalled = match last_tick_age {
            Some(age) => age > STALL_HORIZON,
            None => now.saturating_duration_since(created_at) > STALL_HORIZON,
        };

        // How long ago the most recent SUCCESSFUL fetch was, across every slot.
        //
        // Asked only of slots that have succeeded at least once. A host holding
        // credentials for nothing has no successes ever, and reading that as a
        // blackout would make the signal permanently true on the most ordinary
        // machine there is -- the same failure as any number that is never zero
        // when nothing is wrong. With no prior success there is no claim to make.
        let last_fetch_success_age = snapshot
            .iter()
            .filter_map(|(_, slot)| slot.last_success_at)
            .max()
            .map(|latest| now.saturating_duration_since(latest));
        let fetch_blackout = last_fetch_success_age.is_some_and(|age| age > FETCH_BLACKOUT_HORIZON);
        HealthSnapshot {
            providers_total,
            fresh,
            stale,
            stale_episodes,
            stale_episodes_by_provider,
            quota_drops_by_provider,
            quota_drops_observed_continuously,
            quota_comparisons_no_drop,
            quota_not_comparable,
            pending,
            degraded,
            unconfigured,
            without_handles,
            cookie_cohort_total,
            cookie_logins_stale,
            handles_without_account,
            last_tick_age,
            refresher_stalled,
            last_fetch_success_age,
            fetch_blackout,
            cache_poisoned: false,
        }
    }
}
#[cfg(test)]
mod tests;
