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
//!   wedged/dead — this is the Q4 non-blocking guarantee made observable);
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
    /// Names of providers in a degraded state (non-transient failure: no creds,
    /// expired session, bad shape) — they serve an error entry, not a window.
    pub degraded: Vec<String>,
    /// Browser-cookie providers registered (the desktop-coupled cohort).
    pub cookie_cohort_total: usize,
    /// Cookie-cohort providers that are degraded — a stale browser-login signal
    /// distinct from an ordinary missing credential.
    pub cookie_cohort_degraded: Vec<String>,
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
            degraded: Vec::new(),
            cookie_cohort_total,
            cookie_cohort_degraded: Vec::new(),
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
