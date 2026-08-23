//! Domain health (subc L3) computed from the refresher's slot store — cheap and
//! in-memory.
//!
//! [`Registry::health`](crate::Registry::health) summarizes the current slots:
//! how many providers are serving fresh vs stale vs degraded data, how much of
//! the browser-cookie cohort is degraded (the stale-login signal), and — the
//! load-bearing liveness signal — how long since the refresher last ticked. It
//! NEVER fetches.
//!
//! Status ladder (only the module maps this onto the protocol report):
//! - cache mutex poisoned → `failing` (a serving/refresher task panicked);
//! - refresher heartbeat older than the stall horizon → `degraded` (the loop is
//!   wedged/dead — this makes the non-blocking read guarantee observable, since
//!   a stalled refresher is the one thing that silently erodes served freshness);
//! - otherwise → `ok`, with per-provider staleness carried as detail, because a
//!   provider legitimately lacking local creds is this prober's normal resting
//!   state, not a module fault.
//!
//! This snapshot is wire-agnostic: quota-core knows nothing about subc.

use std::time::Duration;

/// A cheap snapshot of the registry's serving health, derived from the slots.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthSnapshot {
    /// Providers registered in the registry.
    pub providers_total: usize,
    /// Providers whose last fetch succeeded and is within the freshness horizon.
    ///
    /// **Provider-scoped, and ANY healthy handle is enough.** A provider with
    /// three working accounts and one broken credential counts here, because it
    /// is genuinely serving and putting it in `degraded` would fire an operator
    /// alarm while the thing works. The consequence is that a broken credential
    /// beside working ones is invisible in every bucket on this snapshot.
    ///
    /// That fact is not unreported, it is reported on the OTHER wire: such a
    /// provider is omitted from `completeProviders` on `usage.get`, and its
    /// failing credential appears there as its own entry carrying an
    /// `errorClass`. Health answers "is this provider serving"; the usage array
    /// answers "did every credential resolve". Reading `fresh` as the second
    /// question is the misuse to avoid, and the buckets cannot answer it without
    /// either breaking the conservation identity or alarming on healthy
    /// providers.
    pub fresh: usize,
    /// Providers serving a prior good window after a transient failure (stale,
    /// but not wrong — the session is presumed intact).
    pub stale: usize,
    /// Monotonic count of stale-serving EPISODES since process start.
    ///
    /// An episode is a slot entering `StaleTransient` from any other status; a
    /// slot that stays stale across many refresh turns is one episode, not one
    /// per turn. This is the difference between counting failure incidence and
    /// counting refresh cadence: `stale` above is a gauge of how many providers
    /// are stale-serving at this instant, while this is a counter of how many
    /// times stale-serving has ever begun since boot. A transient failure that
    /// resolves between two polls leaves `stale` back at zero but this at one,
    /// which is the trace a continuous watcher would otherwise miss.
    ///
    /// In-memory and reset by a restart on purpose: it answers "has this fired
    /// since boot", and a durable file would be a second state store with its
    /// own crash semantics for a diagnostic.
    ///
    /// NOT part of the conservation identity. It counts events over time, not
    /// members of a population, so folding it into `fresh + stale + pending +
    /// degraded + unconfigured + withoutHandles == providersTotal` would break
    /// an instrument consumers rely on.
    pub stale_episodes: u64,
    /// How those episodes were distributed across providers.
    ///
    /// The total says HOW MANY; this says HOW THEY WERE SPREAD, and only the
    /// pair separates one marginal upstream from an environmental wobble. The
    /// per-provider figures sum to the total.
    ///
    /// Not part of the conservation identity: these are episodes since boot, not
    /// a partition of the current population. A provider named here may be
    /// perfectly healthy right now, and usually is.
    pub stale_episodes_by_provider: std::collections::BTreeMap<String, u64>,
    /// Providers serving nothing yet because at least one handle has not
    /// completed its first fetch.
    ///
    /// Ordinary rather than exceptional: the refresher admits a bounded number
    /// of fetch units per turn, so with more units than that cap some providers
    /// wait several turns after a start. Counted rather than omitted because
    /// every provider must land in exactly one bucket — see the conservation
    /// identity on `without_handles`.
    ///
    /// Not a fault, and distinct from `without_handles`: this provider resolved
    /// its credentials and is queued, rather than failing to enumerate any.
    pub pending: usize,
    /// Names of providers failing in a way somebody can act on: a credential
    /// exists and does not work, the upstream refused it, or the response could
    /// not be read. They serve an error entry, not a window.
    ///
    /// Deliberately excludes providers with no credential on this host, which
    /// are reported in `unconfigured`. Folding the two together made this a
    /// permanent alarm — on a machine using four services, thirty-one adapters
    /// have nothing configured, so a real failure moved the number from 31 to 32
    /// and nobody could see it.
    pub degraded: Vec<String>,
    /// Names of providers with no credential source on this host.
    ///
    /// The expected state for most adapters on most machines, and not a fault:
    /// the module ships thirty-five providers and no one uses all of them. Kept
    /// as a separate bucket rather than dropped so the conservation identity
    /// still accounts for every provider.
    pub unconfigured: Vec<String>,
    /// Registered providers that resolved no credential handle at all, so they
    /// hold no slots and appear in none of the counts above.
    ///
    /// Without this they would be counted in `providers_total` and then be
    /// invisible everywhere else, so the buckets could silently under-sum and a
    /// provider that never came up would read as an absence rather than as a
    /// problem. Only populated once the refresher has completed a tick: before
    /// that every provider legitimately has no slots yet.
    ///
    /// The conservation identity this exists to preserve is
    /// `fresh + stale + pending + degraded.len() + unconfigured.len() +
    /// without_handles.len() == providers_total`, and it holds only once
    /// `last_tick_age` is `Some`.
    /// Consumers are told to assert it, so every branch that classifies a
    /// provider must increment exactly one bucket.
    pub without_handles: Vec<String>,
    /// Browser-cookie providers registered (the desktop-coupled cohort).
    pub cookie_cohort_total: usize,
    /// Cookie-cohort providers whose browser login went stale: a cookie was
    /// found and the upstream rejected it, or the page no longer parses.
    ///
    /// **Not** the cookie providers that are degraded, which is a larger set and
    /// what the name previously suggested. A provider with no cookie at all is
    /// deliberately excluded: not being logged into a service is the correct
    /// state on any host that does not use it, so counting those would pin this
    /// at the cohort size on every machine and leave a real stale login to show
    /// up as one more in a number nobody reads.
    ///
    /// Read alongside `cookie_cohort_total` as "N of C logins stale", never as a
    /// share of `degraded` — the two answer different questions and a host can
    /// easily have eight degraded cookie providers and two stale logins.
    ///
    /// **The proxy is "a cookie was found", which is weaker than "a login
    /// exists", and the gap is visible on this host.** Some adapters here check
    /// for a recognised session-cookie name before fetching (`is_session_cookie`
    /// in `cursor`, `ollama`, `amp`, `factory`, `opencode`), so a rejection from
    /// them really does mean a known login stopped working. The rest send every
    /// cookie the domain has, and a domain can hold cookies without anyone ever
    /// having signed in: `qoder`'s jar contains exactly one, `tfstk`, which is
    /// Alibaba's tracking cookie. That is sent, refused with a 401, and counted
    /// here as a stale login on a host where nobody ever logged in.
    ///
    /// The classification itself is not wrong and matches the upstream this was
    /// ported from, which also maps 401 to invalid credentials and reserves
    /// "missing" for an empty cookie header. What is missing is a session-cookie
    /// predicate for those adapters, and it is deliberately NOT guessed: naming
    /// the wrong cookie makes a working provider report as never configured,
    /// which is far more expensive than one over-counted entry in a metric.
    /// Closing it needs one observation this host cannot supply — a jar from a
    /// browser actually signed in to that service, showing which cookie carries
    /// the session.
    pub cookie_logins_stale: Vec<String>,
    /// Providers holding a credential that reaches no account while their other
    /// accounts serve normally.
    ///
    /// The state a handle enters when the credential behind it is deleted and
    /// the handle is left configured: it can never resolve again, and nothing
    /// about it changes on its own. The provider is genuinely serving, so it
    /// lands in `fresh` and every other count here reads normal — which is why
    /// this needs its own line rather than a bucket. Without it the only
    /// evidence is the provider's absence from `completeProviders` on
    /// `usage.get`, which is a signal you have to know to look for.
    ///
    /// Requires a serving sibling on purpose. A provider whose accounts are ALL
    /// unresolved is already named by `unconfigured` or `degraded`, and
    /// repeating it here would bury the case nothing else reports.
    ///
    /// Does not participate in the conservation identity: a provider named here
    /// is still counted in exactly one bucket.
    pub handles_without_account: Vec<String>,
    /// Age of the refresher's last heartbeat; `None` if it has never ticked.
    pub last_tick_age: Option<Duration>,
    /// The refresher loop is wedged/dead: its heartbeat is older than the stall
    /// horizon (or it never ticked well past startup). Maps to `degraded`.
    pub refresher_stalled: bool,
    /// Age of the most recent SUCCESSFUL fetch across every slot; `None` when no
    /// slot has ever succeeded, which is the ordinary state of a host holding
    /// credentials for nothing.
    pub last_fetch_success_age: Option<Duration>,
    /// Every lane that once worked has now gone a long time without a success,
    /// while the loop keeps ticking. Maps to `degraded`.
    ///
    /// This is the fault `refresher_stalled` cannot see. That flag watches the
    /// LOOP; this one watches whether the loop accomplishes anything. A process
    /// whose transport has died keeps ticking, keeps stale-serving, and reads
    /// `ok` indefinitely -- observed for ten hours on 2026-08-19.
    pub fetch_blackout: bool,
    /// The serving mutex was poisoned by a panicked task — the data path is
    /// faulted. Maps to `failing`.
    pub cache_poisoned: bool,
}

impl HealthSnapshot {
    /// A snapshot for a poisoned serving store: the data path is faulted, so we
    /// report it fail-closed rather than answering a blind "ok".
    pub(crate) fn poisoned(providers_total: usize, cookie_cohort_total: usize) -> Self {
        Self {
            providers_total,
            fresh: 0,
            stale: 0,
            // Every bucket is empty here and the identity does not balance, which
            // is correct: the store could not be read, so no provider was
            // classified at all. Consumers gate the identity on `last_tick_age`
            // being set, and this snapshot leaves it `None`.
            pending: 0,
            stale_episodes: 0,
            stale_episodes_by_provider: std::collections::BTreeMap::new(),
            degraded: Vec::new(),
            unconfigured: Vec::new(),
            without_handles: Vec::new(),
            cookie_cohort_total,
            cookie_logins_stale: Vec::new(),
            handles_without_account: Vec::new(),
            last_tick_age: None,
            refresher_stalled: false,
            last_fetch_success_age: None,
            fetch_blackout: false,
            cache_poisoned: true,
        }
    }

    /// The serving path is faulted (poisoned store). Maps to `failing`.
    pub fn is_failing(&self) -> bool {
        self.cache_poisoned
    }

    /// The refresher is not producing usable work. Maps to `degraded`. (Not
    /// failing: the last-known windows are still served; only their freshness
    /// decays -- and `failing` asks the supervisor to restart us, which is an
    /// action taken on a theory rather than a measurement.)
    ///
    /// Two independent faults, because the loop being alive and the loop being
    /// useful are different claims and only the first was ever checked.
    pub fn is_degraded(&self) -> bool {
        (self.refresher_stalled || self.fetch_blackout) && !self.cache_poisoned
    }

    /// Providers currently serving usable data (fresh or transiently-stale).
    pub fn serving(&self) -> usize {
        self.fresh + self.stale
    }
}
