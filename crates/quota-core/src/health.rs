//! Domain health (subc L3) computed from the last usage sweep — cheap and
//! in-memory.
//!
//! [`Registry::health`](crate::Registry::health) reads the most recent
//! full-provider sweep the serving path cached and summarizes it: how many
//! providers are currently degraded, and how many of the browser-cookie cohort
//! are degraded (the stale-login signal). It NEVER triggers a fetch — the subc
//! prober requires a cheap reply — and it reports the serving cache mutex being
//! poisoned as a real data-path fault, so an inline health answer reflects the
//! actual serving state rather than mere process liveness.
//!
//! This snapshot is wire-agnostic: quota-core knows nothing about subc. The
//! `quota-module` crate maps it onto the protocol's health report.

use std::time::Duration;

/// A cheap snapshot of the registry's serving health, derived from the last
/// cached full sweep.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthSnapshot {
    /// Providers registered in the registry.
    pub providers_total: usize,
    /// Providers that produced a usable (non-degraded) entry in the last sweep.
    pub providers_ok: usize,
    /// Names of providers degraded in the last sweep (they carried an error).
    pub degraded: Vec<String>,
    /// Browser-cookie providers registered (the desktop-coupled cohort).
    pub cookie_cohort_total: usize,
    /// Names of cookie-cohort providers degraded in the last sweep — the signal
    /// that the local browser login went stale and needs a re-login.
    pub cookie_cohort_degraded: Vec<String>,
    /// Age of the sweep this snapshot reflects; `None` when no full sweep has
    /// been cached yet (the module has not been asked for usage).
    pub last_sweep_age: Option<Duration>,
    /// The serving cache mutex was poisoned by a panicked task — the data path
    /// is faulted. This is the one condition that maps to `failing`.
    pub cache_poisoned: bool,
}

impl HealthSnapshot {
    /// A snapshot for a poisoned serving cache: the data path is faulted, so we
    /// report it fail-closed rather than answering a blind "ok".
    pub(crate) fn poisoned(providers_total: usize, cookie_cohort_total: usize) -> Self {
        Self {
            providers_total,
            providers_ok: 0,
            degraded: Vec::new(),
            cookie_cohort_total,
            cookie_cohort_degraded: Vec::new(),
            last_sweep_age: None,
            cache_poisoned: true,
        }
    }

    /// Whether the serving path is faulted. Maps to the protocol's `failing`.
    pub fn is_failing(&self) -> bool {
        self.cache_poisoned
    }
}
