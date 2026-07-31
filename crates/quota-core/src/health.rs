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
    pub fresh: usize,
    /// Providers serving a prior good window after a transient failure (stale,
    /// but not wrong — the session is presumed intact).
    pub stale: usize,
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
    /// Names of providers in a degraded state (non-transient failure: no creds,
    /// expired session, bad shape) — they serve an error entry, not a window.
    pub degraded: Vec<String>,
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
    /// `fresh + stale + pending + degraded.len() + without_handles.len() ==
    /// providers_total`, and it holds only once `last_tick_age` is `Some`.
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
    pub cookie_logins_stale: Vec<String>,
    /// Age of the refresher's last heartbeat; `None` if it has never ticked.
    pub last_tick_age: Option<Duration>,
    /// The refresher loop is wedged/dead: its heartbeat is older than the stall
    /// horizon (or it never ticked well past startup). Maps to `degraded`.
    pub refresher_stalled: bool,
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
            degraded: Vec::new(),
            without_handles: Vec::new(),
            cookie_cohort_total,
            cookie_logins_stale: Vec::new(),
            last_tick_age: None,
            refresher_stalled: false,
            cache_poisoned: true,
        }
    }

    /// The serving path is faulted (poisoned store). Maps to `failing`.
    pub fn is_failing(&self) -> bool {
        self.cache_poisoned
    }

    /// The refresher loop is wedged/dead. Maps to `degraded`. (Not failing: the
    /// last-known windows are still served; only their freshness decays.)
    pub fn is_degraded(&self) -> bool {
        self.refresher_stalled && !self.cache_poisoned
    }

    /// Providers currently serving usable data (fresh or transiently-stale).
    pub fn serving(&self) -> usize {
        self.fresh + self.stale
    }
}
