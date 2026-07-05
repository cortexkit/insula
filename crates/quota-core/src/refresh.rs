//! Background-refresher state: per-provider slots, failure classification, and
//! backoff scheduling. Pure logic — no I/O, no locks — so the scheduling and
//! backoff math is unit-tested in isolation from the async loop that drives it.
//!
//! The refresher (see [`Registry::refresh_tick`](crate::Registry::refresh_tick))
//! owns all fetching; the serving read path only ever clones slots. See
//! `docs/refresher-spike-design.md` for the full design and its Oracle review.

use std::time::{Duration, Instant};

use crate::model::ProviderUsage;

/// Nominal refresh cadence for a healthy provider.
pub const BASE_INTERVAL: Duration = Duration::from_secs(60);
/// A served window is `fresh` while its last success is within this horizon.
pub const FRESH_HORIZON: Duration = Duration::from_secs(120);
/// The refresher is considered stalled if its heartbeat is older than this.
pub const STALL_HORIZON: Duration = Duration::from_secs(300);
/// Fixed re-probe delay for a non-transient failure (no creds / bad shape):
/// slow enough not to hammer, fast enough to pick up a fresh login within ~5m.
pub const NON_TRANSIENT_BACKOFF: Duration = Duration::from_secs(300);
/// Ceiling on the exponential transient backoff.
pub const MAX_TRANSIENT_BACKOFF: Duration = Duration::from_secs(900);
/// Max providers fetched concurrently within one tick.
pub const CONCURRENCY_CAP: usize = 8;
/// Hard deadline around a WHOLE `provider.fetch()` (several providers make more
/// than one awaited call, so the per-request HTTP timeout is not a per-fetch
/// bound). Just over the 30s HTTP timeout.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(35);
/// Upper bound on how long the loop sleeps between wakeups, so a newly-due
/// provider is not starved behind a far-future `next_due_at`.
pub const MAX_TICK_SLEEP: Duration = Duration::from_secs(5);

/// How a fetch failure is treated for backoff and stale-serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchClass {
    /// A blip (network, timeout, 5xx, 429): the session is presumed still valid,
    /// so the last good window keeps being served while we retry with backoff.
    Transient,
    /// The session is gone or the response is unusable (no creds, 401/403, bad
    /// shape): a stale healthy window would mislead, so it is dropped for a
    /// degraded entry and re-probed on a fixed slow cadence.
    NonTransient,
}

/// The serving state of a slot, distinct from whether its data is fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    /// Never attempted yet (cold start). Distinct from `Degraded` so an
    /// un-fetched provider is not mistaken for a failed one.
    Pending,
    /// Last fetch succeeded.
    Fresh,
    /// Last fetch was a transient failure; a previous good entry is still served.
    StaleTransient,
    /// Last fetch was a non-transient failure; the entry is degraded.
    Degraded,
}

/// One provider's refresher state. `entry` is the value the read path serves;
/// `None` means never successfully resolved (honest cold/absent state).
#[derive(Debug, Clone)]
pub struct ProviderSlot {
    /// The value the read path serves. `None` until first resolve.
    pub entry: Option<ProviderUsage>,
    /// Instant of the last OK fetch — drives read-time freshness.
    pub last_success_at: Option<Instant>,
    /// Instant the last attempt started — liveness, not data freshness.
    pub last_attempt_at: Option<Instant>,
    pub status: SlotStatus,
    /// When the refresher should next attempt this provider.
    pub next_due_at: Instant,
    /// Consecutive failures, for the exponential transient backoff.
    pub retry_count: u32,
}

impl ProviderSlot {
    /// A brand-new slot: due immediately (cold start fetches everything at t0),
    /// `Pending` until the first attempt resolves it.
    pub fn due_now(now: Instant) -> Self {
        Self {
            entry: None,
            last_success_at: None,
            last_attempt_at: None,
            status: SlotStatus::Pending,
            next_due_at: now,
            retry_count: 0,
        }
    }

    /// Whether the served window is fresh: last success within [`FRESH_HORIZON`].
    /// Computed at READ time from `last_success_at`, never a stored flag, so a
    /// wedged refresher cannot report `fresh` forever.
    pub fn is_fresh(&self, now: Instant) -> bool {
        self.last_success_at
            .map(|t| now.saturating_duration_since(t) <= FRESH_HORIZON)
            .unwrap_or(false)
    }
}

/// Classify a fetch failure. Timeouts and upstream/transport errors are
/// transient; missing/expired sessions and undecodable responses are not.
pub fn classify(err: &crate::provider::FetchError) -> FetchClass {
    use crate::provider::FetchError::*;
    match err {
        Upstream(_) => FetchClass::Transient,
        NoSession(_) | Unauthorized(_) | Decode(_) => FetchClass::NonTransient,
    }
}

/// The delay until the next attempt after a failure, measured from the attempt's
/// COMPLETION (not the tick start), so a fast provider in a slow sweep is not
/// instantly re-due. `retry_count` is the post-increment count (>= 1).
pub fn backoff(class: FetchClass, retry_count: u32) -> Duration {
    match class {
        FetchClass::NonTransient => NON_TRANSIENT_BACKOFF,
        FetchClass::Transient => {
            // 60s * 2^min(retry_count-1, 6), capped. retry_count>=1 => exp>=0.
            let exp = retry_count.saturating_sub(1).min(6);
            let secs = 60u64.saturating_mul(1u64 << exp);
            Duration::from_secs(secs).min(MAX_TRANSIENT_BACKOFF)
        }
    }
}

/// Compute the next slot after a SUCCESSFUL fetch. `attempt_start` is when the
/// fetch began (liveness); `completed` is when it finished (schedule anchor).
pub fn next_slot_on_success(
    usage: ProviderUsage,
    attempt_start: Instant,
    completed: Instant,
) -> ProviderSlot {
    ProviderSlot {
        entry: Some(usage),
        last_success_at: Some(completed),
        last_attempt_at: Some(attempt_start),
        status: SlotStatus::Fresh,
        next_due_at: completed + BASE_INTERVAL,
        retry_count: 0,
    }
}

/// Compute the next slot after a FAILED fetch, applying the class policy:
/// a transient failure WITH a prior good entry keeps serving it stale; every
/// other case (non-transient, or transient with no prior good entry) drops to a
/// visible degraded entry so a known-bad provider is never confused with an
/// unattempted one. `last_success_at` is preserved (freshness decays honestly).
pub fn next_slot_on_failure(
    prev: &ProviderSlot,
    provider_name: &str,
    err: &crate::provider::FetchError,
    attempt_start: Instant,
    completed: Instant,
) -> ProviderSlot {
    let class = classify(err);
    let retry_count = prev.retry_count.saturating_add(1);
    let delay = backoff(class, retry_count);
    // Keep serving the previous window on a transient blip ONLY when it is a
    // genuinely HEALTHY window. A previous DEGRADED entry (from an earlier
    // non-transient failure) must not be relabelled stale-transient — it would
    // then be counted as "serving" and mask a real degradation.
    let prev_is_healthy = prev
        .entry
        .as_ref()
        .is_some_and(|e| e.error.is_none() && e.usage.is_some());
    let (entry, status) = match class {
        FetchClass::Transient if prev_is_healthy => {
            (prev.entry.clone(), SlotStatus::StaleTransient)
        }
        _ => (
            Some(ProviderUsage::degraded(provider_name, err)),
            SlotStatus::Degraded,
        ),
    };
    ProviderSlot {
        entry,
        last_success_at: prev.last_success_at,
        last_attempt_at: Some(attempt_start),
        status,
        next_due_at: completed + delay,
        retry_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FetchError;

    #[test]
    fn classify_splits_transient_from_non_transient() {
        assert_eq!(
            classify(&FetchError::Upstream("503".into())),
            FetchClass::Transient
        );
        assert_eq!(
            classify(&FetchError::NoSession("no file".into())),
            FetchClass::NonTransient
        );
        assert_eq!(
            classify(&FetchError::Unauthorized("401".into())),
            FetchClass::NonTransient
        );
        assert_eq!(
            classify(&FetchError::Decode("bad json".into())),
            FetchClass::NonTransient
        );
    }

    #[test]
    fn transient_backoff_is_exponential_and_capped() {
        // retry_count is post-increment: 1 -> 60s, 2 -> 120s, doubling.
        assert_eq!(backoff(FetchClass::Transient, 1), Duration::from_secs(60));
        assert_eq!(backoff(FetchClass::Transient, 2), Duration::from_secs(120));
        assert_eq!(backoff(FetchClass::Transient, 3), Duration::from_secs(240));
        assert_eq!(backoff(FetchClass::Transient, 4), Duration::from_secs(480));
        // exponent caps at 6 => 60 * 64 = 3840s, clamped to the 900s ceiling.
        assert_eq!(backoff(FetchClass::Transient, 7), MAX_TRANSIENT_BACKOFF);
        assert_eq!(backoff(FetchClass::Transient, 50), MAX_TRANSIENT_BACKOFF);
    }

    #[test]
    fn non_transient_backoff_is_fixed() {
        assert_eq!(backoff(FetchClass::NonTransient, 1), NON_TRANSIENT_BACKOFF);
        assert_eq!(backoff(FetchClass::NonTransient, 9), NON_TRANSIENT_BACKOFF);
    }

    #[test]
    fn freshness_is_computed_from_last_success() {
        let now = Instant::now();
        let mut slot = ProviderSlot::due_now(now);
        assert!(!slot.is_fresh(now)); // never succeeded

        slot.last_success_at = Some(now);
        assert!(slot.is_fresh(now));
        assert!(slot.is_fresh(now + Duration::from_secs(119)));
        assert!(!slot.is_fresh(now + Duration::from_secs(121))); // past horizon
    }

    fn healthy_entry(name: &str) -> ProviderUsage {
        ProviderUsage::healthy(name, None, "test", crate::model::Usage::default())
    }

    #[test]
    fn success_yields_fresh_slot_scheduled_from_completion() {
        let start = Instant::now();
        let done = start + Duration::from_secs(2);
        let slot = next_slot_on_success(healthy_entry("codex"), start, done);
        assert_eq!(slot.status, SlotStatus::Fresh);
        assert_eq!(slot.retry_count, 0);
        assert_eq!(slot.last_success_at, Some(done));
        // Next due is BASE_INTERVAL after COMPLETION, not the attempt start.
        assert_eq!(slot.next_due_at, done + BASE_INTERVAL);
        assert!(slot.entry.is_some());
    }

    #[test]
    fn transient_failure_keeps_the_prior_good_entry_stale() {
        let t0 = Instant::now();
        let good = next_slot_on_success(healthy_entry("codex"), t0, t0);
        let t1 = t0 + Duration::from_secs(60);
        let next =
            next_slot_on_failure(&good, "codex", &FetchError::Upstream("503".into()), t1, t1);
        // Blip: last good window is still served, marked stale-transient.
        assert_eq!(next.status, SlotStatus::StaleTransient);
        assert_eq!(next.entry, good.entry);
        assert!(next.entry.as_ref().unwrap().error.is_none()); // still the healthy window
        assert_eq!(next.retry_count, 1);
        assert_eq!(next.last_success_at, good.last_success_at); // preserved, decays
        assert_eq!(next.next_due_at, t1 + Duration::from_secs(60));
    }

    #[test]
    fn non_transient_failure_replaces_with_degraded_even_if_prior_good() {
        let t0 = Instant::now();
        let good = next_slot_on_success(healthy_entry("codex"), t0, t0);
        let t1 = t0 + Duration::from_secs(60);
        let next = next_slot_on_failure(
            &good,
            "codex",
            &FetchError::Unauthorized("401".into()),
            t1,
            t1,
        );
        // A dead session must NOT keep serving the stale healthy window.
        assert_eq!(next.status, SlotStatus::Degraded);
        let entry = next.entry.as_ref().unwrap();
        assert!(entry.error.is_some());
        assert!(entry.usage.is_none());
        assert_eq!(next.next_due_at, t1 + NON_TRANSIENT_BACKOFF);
    }

    #[test]
    fn transient_failure_with_no_prior_entry_degrades() {
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0);
        let next = next_slot_on_failure(
            &cold,
            "codex",
            &FetchError::Upstream("timeout".into()),
            t0,
            t0,
        );
        // No good entry to keep → a visible degraded entry, not empty.
        assert_eq!(next.status, SlotStatus::Degraded);
        assert!(next.entry.as_ref().unwrap().error.is_some());
    }

    #[test]
    fn transient_failure_after_a_degraded_entry_stays_degraded() {
        // A prior DEGRADED entry must NOT be relabelled stale-transient on a
        // later transient blip — that would count it as "serving" and hide the
        // degradation from health.
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0);
        let degraded = next_slot_on_failure(
            &cold,
            "codex",
            &FetchError::Unauthorized("401".into()),
            t0,
            t0,
        );
        assert_eq!(degraded.status, SlotStatus::Degraded);

        let t1 = t0 + Duration::from_secs(300);
        let next = next_slot_on_failure(
            &degraded,
            "codex",
            &FetchError::Upstream("503".into()),
            t1,
            t1,
        );
        // Still degraded, not stale-transient.
        assert_eq!(next.status, SlotStatus::Degraded);
        assert!(next.entry.as_ref().unwrap().error.is_some());
    }
}
