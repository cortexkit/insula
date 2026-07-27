//! Handle-scoped refresher state, failure classification, and backoff logic.
//!
//! This module is pure scheduling state: no I/O and no locks. The registry
//! performs credential enumeration and fetches outside its store mutex, then
//! publishes one whole slot through an incarnation fence.

use std::time::{Duration, Instant};

use crate::model::{AccountInfo, ProviderUsage, SavedResets};
use crate::provider::{AccountObservation, CredentialResolution, FetchAttempt, FetchError};

/// Nominal refresh cadence for a healthy fetch unit.
pub const BASE_INTERVAL: Duration = Duration::from_secs(60);
/// A served window is `fresh` while its last success is within this horizon.
pub const FRESH_HORIZON: Duration = Duration::from_secs(120);
/// The refresher is considered stalled if its heartbeat is older than this.
pub const STALL_HORIZON: Duration = Duration::from_secs(300);
/// Fixed re-probe delay for a non-transient failure.
pub const NON_TRANSIENT_BACKOFF: Duration = Duration::from_secs(300);
/// Ceiling on the exponential transient backoff.
pub const MAX_TRANSIENT_BACKOFF: Duration = Duration::from_secs(900);
/// Maximum fetch units admitted in one bounded scheduler turn.
pub const CONCURRENCY_CAP: usize = 8;
/// Hard deadline around a whole handle fetch.
pub const FETCH_DEADLINE: Duration = Duration::from_secs(35);
/// Maximum idle sleep, which also bounds discovery of newly added handles.
pub const MAX_TICK_SLEEP: Duration = Duration::from_secs(5);

/// A monotonically assigned lifetime for an active `(provider, handle)` key.
/// A removed and later re-added key receives a different value, preventing an
/// old in-flight fetch from publishing into the new lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Incarnation(u128);

impl Incarnation {
    pub(crate) fn from_counter(value: u128) -> Self {
        Self(value)
    }
}

/// Orders overlapping attempts within one active incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttemptSequence(u128);

impl AttemptSequence {
    pub(crate) fn from_counter(value: u128) -> Self {
        Self(value)
    }
}

/// How a fetch failure is treated for backoff and stale-serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchClass {
    /// A network or upstream blip. A previous healthy window may remain stale.
    Transient,
    /// Missing/rejected credentials or an unusable response. Stale data is unsafe.
    NonTransient,
}

/// The serving state of a slot, distinct from whether its data is fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    Pending,
    Fresh,
    StaleTransient,
    Degraded,
}

/// State for one active `(provider, handle)` fetch unit.
#[derive(Debug, Clone)]
pub struct ProviderSlot {
    /// Lifetime fence assigned when the key entered the active set.
    pub incarnation: Incarnation,
    /// Latest attempt admitted for this incarnation. Older completions are dropped.
    pub attempt_sequence: AttemptSequence,
    /// Value eligible for serving. `None` is an honest unavailable state.
    pub entry: Option<ProviderUsage>,
    /// Most recent independently observed credential identity.
    pub observation: Option<AccountObservation>,
    /// The credential changed but no successful usage belongs to the new label.
    /// Readers suppress the slot until a success closes the transition.
    pub label_in_flux: bool,
    /// Whether fresh reads may zero the raw percentages stored in `entry`.
    pub relax_eligible: bool,
    pub last_success_at: Option<Instant>,
    pub last_attempt_at: Option<Instant>,
    pub status: SlotStatus,
    pub next_due_at: Instant,
    pub retry_count: u32,
}

impl ProviderSlot {
    /// A brand-new active fetch unit, due immediately.
    pub fn due_now(now: Instant, incarnation: Incarnation) -> Self {
        Self {
            incarnation,
            attempt_sequence: AttemptSequence::from_counter(0),
            entry: None,
            observation: None,
            label_in_flux: false,
            relax_eligible: false,
            last_success_at: None,
            last_attempt_at: None,
            status: SlotStatus::Pending,
            next_due_at: now,
            retry_count: 0,
        }
    }

    /// Whether the served window is fresh at read time.
    pub fn is_fresh(&self, now: Instant) -> bool {
        self.last_success_at
            .map(|t| now.saturating_duration_since(t) <= FRESH_HORIZON)
            .unwrap_or(false)
    }

    /// Resolved account label, if the credential source exposes one.
    pub fn account_id(&self) -> Option<&str> {
        self.observation
            .as_ref()
            .and_then(|observation| observation.account_id.as_deref())
    }
}

/// Classify a fetch failure for stale-serving and scheduling.
pub fn classify(err: &FetchError) -> FetchClass {
    match err {
        FetchError::Upstream(_) => FetchClass::Transient,
        FetchError::ProviderStatus(401 | 403)
        | FetchError::NoSession(_)
        | FetchError::Unauthorized(_)
        | FetchError::Decode(_) => FetchClass::NonTransient,
        FetchError::ProviderStatus(_) => FetchClass::Transient,
    }
}

/// Delay after a failure, measured from attempt completion.
pub fn backoff(class: FetchClass, retry_count: u32) -> Duration {
    match class {
        FetchClass::NonTransient => NON_TRANSIENT_BACKOFF,
        FetchClass::Transient => {
            let exp = retry_count.saturating_sub(1).min(6);
            let secs = 60u64.saturating_mul(1u64 << exp);
            Duration::from_secs(secs).min(MAX_TRANSIENT_BACKOFF)
        }
    }
}

fn healthy_entry(
    provider_name: &str,
    observation: Option<&AccountObservation>,
    source: Option<String>,
    usage: crate::model::Usage,
    account_info: Option<AccountInfo>,
    saved_resets: Option<SavedResets>,
) -> ProviderUsage {
    ProviderUsage {
        provider: provider_name.to_string(),
        api_provider: None,
        account: observation.and_then(|value| value.account_id.clone()),
        source,
        account_info: account_info.filter(|info| !info.is_empty()),
        fetched_at: None,
        saved_resets,
        usage: Some(usage),
        error: None,
    }
}

/// Compute the whole next slot from one completed attempt.
///
/// A newly observed account invalidates all data belonging to the previous
/// account. If that same attempt fails, the slot remains unavailable and its
/// retry sequence restarts; transient stale-serving must never cross identities.
pub fn next_slot_after_attempt(
    prev: &ProviderSlot,
    provider_name: &str,
    attempt: FetchAttempt,
    attempt_start: Instant,
    completed: Instant,
) -> ProviderSlot {
    let identity_unverified = attempt.credential_resolution == CredentialResolution::Unverified;
    next_slot_after_attempt_inner(
        prev,
        provider_name,
        attempt,
        attempt_start,
        completed,
        identity_unverified,
    )
}

/// Fail closed when the scheduler terminates an attempt before the provider can
/// return its credential observation. A previously labeled slot cannot safely
/// stale-serve because the credential may have changed during the lost attempt.
pub fn next_slot_after_unverified_failure(
    prev: &ProviderSlot,
    provider_name: &str,
    error: FetchError,
    attempt_start: Instant,
    completed: Instant,
) -> ProviderSlot {
    next_slot_after_attempt_inner(
        prev,
        provider_name,
        FetchAttempt::failure(None, None, error),
        attempt_start,
        completed,
        true,
    )
}

fn next_slot_after_attempt_inner(
    prev: &ProviderSlot,
    provider_name: &str,
    attempt: FetchAttempt,
    attempt_start: Instant,
    completed: Instant,
    identity_unverified: bool,
) -> ProviderSlot {
    // A successful usage response cannot validate a credential transition unless
    // the provider explicitly reports the identity used for that response.
    if prev.label_in_flux && attempt.observed.is_none() && attempt.usage.is_ok() {
        return ProviderSlot {
            incarnation: prev.incarnation,
            attempt_sequence: prev.attempt_sequence,
            entry: None,
            observation: prev.observation.clone(),
            label_in_flux: true,
            relax_eligible: false,
            last_success_at: None,
            last_attempt_at: Some(attempt_start),
            status: prev.status,
            next_due_at: completed + BASE_INTERVAL,
            retry_count: prev.retry_count,
        };
    }

    let observed_account_changed = match (&prev.observation, &attempt.observed) {
        (Some(old), Some(new)) => {
            if old.account_id.is_some() || new.account_id.is_some() {
                old.account_id != new.account_id
            } else {
                // Neither observation carries an account id, so a change of
                // account cannot be seen directly. Some credential records are
                // unlabeled by contract, yet the same durable handle can be
                // repointed at a different account, and the record version is the
                // only evidence of that: it identifies the served record and
                // always advances when the record is replaced.
                //
                // Treating a version change as a possible identity change can
                // discard a still-valid entry when a record is re-versioned
                // without changing account. That costs one refresh cycle. The
                // alternative is serving one account's usage under another
                // account's credential for as long as fetches keep failing, which
                // a consumer cannot detect and would act on.
                //
                // Local sources leave the version absent and re-resolve every
                // fetch, so absent-vs-absent compares equal and they are
                // unaffected.
                old.record_version != new.record_version
            }
        }
        _ => false,
    };
    let identity_may_have_changed = identity_unverified && prev.observation.is_some();
    let account_changed = observed_account_changed || identity_may_have_changed;
    let observation = attempt
        .observed
        .clone()
        .or_else(|| prev.observation.clone());

    match attempt.usage {
        Ok(usage) => ProviderSlot {
            incarnation: prev.incarnation,
            attempt_sequence: prev.attempt_sequence,
            entry: Some(healthy_entry(
                provider_name,
                observation.as_ref(),
                attempt.source,
                usage,
                attempt.account_info,
                attempt.saved_resets,
            )),
            observation,
            label_in_flux: false,
            relax_eligible: attempt.relax_eligible,
            last_success_at: Some(completed),
            last_attempt_at: Some(attempt_start),
            status: SlotStatus::Fresh,
            next_due_at: completed + BASE_INTERVAL,
            retry_count: 0,
        },
        Err(error) => {
            let retry_base = if account_changed {
                ProviderSlot::due_now(completed, prev.incarnation)
            } else {
                prev.clone()
            };
            let class = classify(&error);
            let retry_count = retry_base.retry_count.saturating_add(1);
            let prev_is_healthy = retry_base
                .entry
                .as_ref()
                .is_some_and(|entry| entry.error.is_none() && entry.usage.is_some());
            let (entry, status) = match class {
                FetchClass::Transient if prev_is_healthy => {
                    (retry_base.entry.clone(), SlotStatus::StaleTransient)
                }
                _ => (
                    Some(ProviderUsage::degraded(provider_name, &error)),
                    SlotStatus::Degraded,
                ),
            };
            let label_in_flux = account_changed || retry_base.label_in_flux;
            ProviderSlot {
                incarnation: prev.incarnation,
                attempt_sequence: prev.attempt_sequence,
                entry: if label_in_flux { None } else { entry },
                observation,
                label_in_flux,
                relax_eligible: false,
                last_success_at: if account_changed {
                    None
                } else {
                    retry_base.last_success_at
                },
                last_attempt_at: Some(attempt_start),
                status,
                next_due_at: completed + backoff(class, retry_count),
                retry_count,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_source::VaultGetError;
    use crate::model::Usage;

    fn incarnation() -> Incarnation {
        Incarnation::from_counter(1)
    }

    fn attempt(account: Option<&str>, usage: Result<Usage, FetchError>) -> FetchAttempt {
        FetchAttempt {
            observed: Some(AccountObservation::new(
                account.map(ToString::to_string),
                None,
            )),
            source: Some("test".to_string()),
            usage,
            account_info: None,
            saved_resets: None,
            relax_eligible: false,
            credential_resolution: CredentialResolution::Verified,
        }
    }

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
        assert_eq!(
            classify(&FetchError::ProviderStatus(401)),
            FetchClass::NonTransient
        );
        assert_eq!(
            classify(&FetchError::ProviderStatus(503)),
            FetchClass::Transient
        );
    }

    #[test]
    fn transient_backoff_is_exponential_and_capped() {
        assert_eq!(backoff(FetchClass::Transient, 1), Duration::from_secs(60));
        assert_eq!(backoff(FetchClass::Transient, 2), Duration::from_secs(120));
        assert_eq!(backoff(FetchClass::Transient, 3), Duration::from_secs(240));
        assert_eq!(backoff(FetchClass::Transient, 4), Duration::from_secs(480));
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
        let mut slot = ProviderSlot::due_now(now, incarnation());
        assert!(!slot.is_fresh(now));
        slot.last_success_at = Some(now);
        assert!(slot.is_fresh(now + Duration::from_secs(119)));
        assert!(!slot.is_fresh(now + Duration::from_secs(121)));
    }

    #[test]
    fn success_is_fresh_and_scheduled_from_completion() {
        let start = Instant::now();
        let done = start + Duration::from_secs(2);
        let cold = ProviderSlot::due_now(start, incarnation());
        let slot = next_slot_after_attempt(
            &cold,
            "codex",
            attempt(Some("A"), Ok(Usage::default())),
            start,
            done,
        );
        assert_eq!(slot.status, SlotStatus::Fresh);
        assert_eq!(slot.retry_count, 0);
        assert_eq!(slot.last_success_at, Some(done));
        assert_eq!(slot.next_due_at, done + BASE_INTERVAL);
        assert_eq!(slot.account_id(), Some("A"));
    }

    #[test]
    fn transient_failure_keeps_a_prior_healthy_entry() {
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0, incarnation());
        let good = next_slot_after_attempt(
            &cold,
            "codex",
            attempt(Some("A"), Ok(Usage::default())),
            t0,
            t0,
        );
        let t1 = t0 + BASE_INTERVAL;
        let next = next_slot_after_attempt(
            &good,
            "codex",
            attempt(Some("A"), Err(FetchError::Upstream("503".into()))),
            t1,
            t1,
        );
        assert_eq!(next.status, SlotStatus::StaleTransient);
        assert_eq!(next.entry, good.entry);
        assert_eq!(next.retry_count, 1);
        assert_eq!(next.last_success_at, good.last_success_at);
    }

    /// A transient failure stale-serves only when there is something to serve.
    /// On a slot that has never succeeded there is no prior window, so the
    /// transient class degrades like any other -- which is what a consumer sees
    /// first from a provider whose opening fetch times out. The resulting entry
    /// carries a transient cause and no success timestamp, so it is a verdict
    /// about the attempt rather than about the credential.
    #[test]
    fn a_transient_failure_with_no_prior_success_degrades() {
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0, incarnation());
        assert!(cold.entry.is_none(), "precondition: nothing to stale-serve");

        let next = next_slot_after_attempt(
            &cold,
            "codex",
            attempt(
                Some("A"),
                Err(FetchError::Upstream("connect timeout".into())),
            ),
            t0,
            t0,
        );

        // Pin the reason for the degradation below. The error is Transient --
        // the class that stale-serves -- so the slot degrades for lack of a
        // prior window, not because the failure was classified non-transient.
        assert_eq!(
            classify(&FetchError::Upstream("connect timeout".into())),
            FetchClass::Transient,
        );
        assert_eq!(next.status, SlotStatus::Degraded);
        let entry = next.entry.as_ref().expect("a degraded entry is emitted");
        assert!(entry.error.is_some(), "it carries the transient cause");
        assert!(entry.usage.is_none());
        // No success has ever happened, so the entry dates nothing: a consumer
        // has nothing to retain or hard-age here.
        assert!(next.last_success_at.is_none());
    }

    #[test]
    fn non_transient_failure_replaces_a_prior_healthy_entry() {
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0, incarnation());
        let good = next_slot_after_attempt(
            &cold,
            "codex",
            attempt(Some("A"), Ok(Usage::default())),
            t0,
            t0,
        );
        let next = next_slot_after_attempt(
            &good,
            "codex",
            attempt(Some("A"), Err(FetchError::Unauthorized("401".into()))),
            t0,
            t0,
        );
        assert_eq!(next.status, SlotStatus::Degraded);
        assert!(next
            .entry
            .as_ref()
            .is_some_and(|entry| entry.error.is_some()));
    }

    #[test]
    fn observed_account_change_on_failure_clears_data_and_restarts_backoff() {
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0, incarnation());
        let mut good = next_slot_after_attempt(
            &cold,
            "codex",
            attempt(Some("A"), Ok(Usage::default())),
            t0,
            t0,
        );
        good.retry_count = 5;
        let t1 = t0 + BASE_INTERVAL;
        let next = next_slot_after_attempt(
            &good,
            "codex",
            attempt(Some("B"), Err(FetchError::Upstream("timeout".into()))),
            t1,
            t1,
        );
        assert!(next.entry.is_none());
        assert!(next.last_success_at.is_none());
        assert!(next.label_in_flux);
        assert_eq!(next.account_id(), Some("B"));
        assert_eq!(next.retry_count, 1);
        assert_eq!(next.next_due_at, t1 + BASE_INTERVAL);
    }

    #[test]
    fn vault_get_transient_failure_fails_closed_instead_of_stale_serving() {
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0, incarnation());
        let good = next_slot_after_attempt(
            &cold,
            "codex",
            attempt(Some("old-account"), Ok(Usage::default())),
            t0,
            t0,
        );
        let next = next_slot_after_attempt(
            &good,
            "codex",
            FetchAttempt::unverified_vault_failure(VaultGetError::Transient),
            t0 + BASE_INTERVAL,
            t0 + BASE_INTERVAL,
        );

        assert!(next.entry.is_none(), "old account window was stale-served");
        assert!(next.label_in_flux);
        assert!(next.last_success_at.is_none());
        assert_eq!(next.retry_count, 1);
    }

    #[test]
    fn transient_failure_after_degradation_stays_degraded() {
        let t0 = Instant::now();
        let cold = ProviderSlot::due_now(t0, incarnation());
        let degraded = next_slot_after_attempt(
            &cold,
            "codex",
            attempt(None, Err(FetchError::Unauthorized("no credentials".into()))),
            t0,
            t0,
        );
        let next = next_slot_after_attempt(
            &degraded,
            "codex",
            attempt(None, Err(FetchError::Upstream("503".into()))),
            t0,
            t0,
        );
        assert_eq!(next.status, SlotStatus::Degraded);
        assert!(next
            .entry
            .as_ref()
            .is_some_and(|entry| entry.error.is_some()));
    }

    #[test]
    fn an_unlabeled_record_repointed_to_another_account_fails_closed() {
        // Some vault records carry no account id by contract, yet the same durable
        // handle can be repointed to a different account. In that case a changed
        // record version is the only available signal that the handle now refers to
        // a different account. If a usage fetch then fails transiently, keeping the
        // cached entry would serve the previous account's usage under the new
        // account's credential, and repeated failures could extend that
        // indefinitely.
        let t0 = Instant::now();
        let healthy = next_slot_after_attempt(
            &ProviderSlot::due_now(t0, incarnation()),
            "gemini",
            FetchAttempt::success(
                Some(AccountObservation::new(None, Some(116))),
                "vault",
                Usage::default(),
            ),
            t0,
            t0,
        );
        assert!(
            healthy.entry.is_some(),
            "precondition: a healthy window exists"
        );

        let after = next_slot_after_attempt(
            &healthy,
            "gemini",
            FetchAttempt::failure(
                Some(AccountObservation::new(None, Some(117))),
                None,
                FetchError::Upstream("429".to_string()),
            ),
            t0,
            t0,
        );

        assert!(
            after.entry.is_none(),
            "the previous account's window must not survive a record replacement"
        );
        assert!(
            after.label_in_flux,
            "identity is unconfirmed until a fetch succeeds"
        );
        assert_eq!(
            after.last_success_at, None,
            "backoff restarts from the change"
        );
    }

    #[test]
    fn an_unchanged_unlabeled_record_still_stale_serves_through_a_flap() {
        // The boundary that keeps the rule above from over-reacting: same record,
        // same version, ordinary transient failure. This is the common case, and
        // discarding the window here would throw away good data on every flap.
        let t0 = Instant::now();
        let healthy = next_slot_after_attempt(
            &ProviderSlot::due_now(t0, incarnation()),
            "gemini",
            FetchAttempt::success(
                Some(AccountObservation::new(None, Some(116))),
                "vault",
                Usage::default(),
            ),
            t0,
            t0,
        );
        let after = next_slot_after_attempt(
            &healthy,
            "gemini",
            FetchAttempt::failure(
                Some(AccountObservation::new(None, Some(116))),
                None,
                FetchError::Upstream("429".to_string()),
            ),
            t0,
            t0,
        );

        assert!(
            after.entry.is_some(),
            "an unchanged record keeps serving stale"
        );
        assert_eq!(after.status, SlotStatus::StaleTransient);
        assert!(!after.label_in_flux);
    }

    #[test]
    fn a_local_source_without_versions_is_unaffected() {
        // Local sources leave the record version absent and re-resolve identity on
        // every fetch, so absent-vs-absent must compare equal rather than reading
        // as a change on every single attempt.
        let t0 = Instant::now();
        let healthy = next_slot_after_attempt(
            &ProviderSlot::due_now(t0, incarnation()),
            "local",
            FetchAttempt::success(
                Some(AccountObservation::new(None, None)),
                "local",
                Usage::default(),
            ),
            t0,
            t0,
        );
        let after = next_slot_after_attempt(
            &healthy,
            "local",
            FetchAttempt::failure(
                Some(AccountObservation::new(None, None)),
                None,
                FetchError::Upstream("timeout".to_string()),
            ),
            t0,
            t0,
        );

        assert!(
            after.entry.is_some(),
            "an unversioned local slot keeps serving stale"
        );
        assert!(!after.label_in_flux);
    }
}
