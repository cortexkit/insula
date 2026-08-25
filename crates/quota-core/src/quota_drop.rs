//! Detecting that an account's used percent went DOWN between two readings.
//!
//! # Why this exists
//!
//! The refresher holds the previous reading and the new one at every publish and
//! throws the comparison away. By the time any consumer sees a percentage the
//! history is gone, so nobody downstream can reconstruct it — which makes this
//! the one place in the system where "quota came back" is observable at all.
//! Asked for as `reset_occurred` on insula#5.
//!
//! # This slice counts DROPS, and deliberately does not call them resets
//!
//! A drop is what can be observed. The CAUSE of a drop is not: a window
//! rollover, a banked-reset redemption, a goodwill grant, a plan change and an
//! upstream correction are identical from here. Naming the count after the
//! observation rather than the inference is the same discipline that keeps
//! `usedPercent` a capacity reading rather than an enforcement state.
//!
//! The one cause this module CAN attribute is its own: a resolved redemption in
//! the codex journal means we caused that drop. That attribution is not wired
//! here yet — it belongs with the consumable record, not with a counter.
//!
//! # The question this slice answers
//!
//! Whether drops are observable from a 60-second poll at all, and how often the
//! observation is trustworthy. Nothing in this module has ever recorded one, so
//! the consumable wire record asked for on insula#5 would otherwise be designed
//! against an assumption about its own frequency. Measure, then design.

use std::time::{Duration, Instant};

use crate::model::{RateWindow, Usage};
use crate::refresh::ProviderSlot;

/// A used-percent decrease on at least one window of an account.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaDrop {
    /// Largest decrease seen across the account's windows, in percentage points.
    pub magnitude: f64,
    /// Whether the two readings were taken across a CONTINUOUS poll interval.
    ///
    /// False means the interval had a gap — host sleep, a fetch blackout, a
    /// backoff — and a gap hides things: a drop plus subsequent consumption reads
    /// as a SMALLER drop than happened, and a drop followed by a full re-fill
    /// reads as no drop at all. A record that cannot say which kind it is would
    /// be worse than none, because it looks like evidence.
    ///
    /// FALSE IS RARER THAN "THERE WAS A GAP", and the narrowness is worth knowing
    /// before reading a run of `true` as proof the flag works. Four conditions
    /// must hold together (traced from source by a consumer on insula#5):
    ///
    ///   1. `detect` is reached at all -- so the prior entry exists, carries
    ///      usage, and carries no error;
    ///   2. the prior entry SURVIVED the failure -- only the
    ///      `Transient if prev_is_healthy` arm clones it forward, and a
    ///      credential-source failure cannot take that arm, because an
    ///      unverified identity rebases the retry on a FRESH slot (see
    ///      `next_slot_after_unverified_failure`), so `prev_is_healthy` is false;
    ///   3. the gap exceeds two base intervals -- `last_success_at` only advances
    ///      on success, so it grows across a stale-serving run;
    ///   4. the later reading is actually lower.
    ///
    /// (3) and (4) together are the tight part: the gap grows only while stale
    /// serving, and a decrease appears only if a window boundary fell INSIDE that
    /// same outage. So the false arm needs a provider-side transient outage longer
    /// than the horizon that also spans a quota reset -- a conjunction of two
    /// independent rare events, which explains an unfired arm better than sample
    /// size does.
    ///
    /// The case it is really for is a HOST SUSPEND spanning a window boundary,
    /// which the wall clock sees and a monotonic clock does not. That is also the
    /// case no co-located consumer can witness, because their poller slept too --
    /// which is precisely why the record has to carry its own confidence rather
    /// than leaving it to be checked afterwards.
    pub observed_continuously: bool,
}

/// Below this many percentage points, a decrease is not treated as a drop.
///
/// PROVISIONAL, and the number is doing less work than it appears to. Upstreams
/// recompute percentages against denominators that move (qwen-cloud's console
/// divides by a different total than it reports), and rounding wobble at the
/// third decimal is routine, so some floor is required or the count measures
/// arithmetic noise.
///
/// One point is chosen as clearly above rounding and clearly below any
/// meaningful return of capacity. It is NOT derived, because the data that would
/// derive it does not exist yet — which is the whole reason this slice ships as
/// a counter. `magnitude` is recorded so the distribution can correct this.
const NOISE_FLOOR_PERCENT: f64 = 1.0;

/// How far past the base interval a gap must reach before the pair is untrusted.
///
/// The refresher targets one reading per `BASE_INTERVAL`, and a tick that runs
/// late by a few seconds is ordinary scheduling rather than a gap. Two intervals
/// is comfortably outside normal jitter and comfortably inside the shortest
/// backoff, so it separates "polled as intended" from "something interrupted the
/// series" without either side being a close call.
const CONTINUOUS_MULTIPLIER: u32 = 2;

/// The mechanic string that makes a decrease meaningless as a drop.
///
/// A continuously replenishing pool goes down and up as a matter of course, so a
/// lower reading says "Tuesday" rather than "quota returned". This is the
/// dependency that blocked the whole feature: without a stated mechanic, a
/// detector keyed on a decrease fires every tick for a drip provider.
const DRIP: &str = "drip";

/// What one paired reading concluded.
///
/// Three outcomes rather than two, because "no drop" and "could not compare" are
/// different facts that look identical from outside a counter.
#[derive(Debug, Clone, PartialEq)]
pub enum DropObservation {
    /// The comparison ran and found a decrease past the noise floor.
    Drop(QuotaDrop),
    /// The comparison ran and the account had not gone down.
    NoDrop,
    /// No comparison was possible.
    NotComparable(NotComparable),
}

/// Why a pair of readings could not be compared.
///
/// Named individually so an operator can tell a quiet host from a broken one.
/// Each maps to its own counter, incremented in the same statement that returns
/// it, so the count and the decision cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotComparable {
    /// The credential now serves a different account, so the two readings do not
    /// describe one series.
    AccountChanged,
    /// Nothing to compare against: a cold slot, or an entry carrying no usage.
    NoPriorReading,
    /// The previous reading was a degraded entry. The one a consumer most needs
    /// named -- a latched credential produces a long run of these, and they are
    /// silence in every other measure.
    PriorReadingWasAnError,
}

impl NotComparable {
    /// The stable key this reason is counted under.
    pub fn as_key(self) -> &'static str {
        match self {
            Self::AccountChanged => "account_changed",
            Self::NoPriorReading => "no_prior_reading",
            Self::PriorReadingWasAnError => "prior_reading_was_an_error",
        }
    }
}

/// Pair two readings of one account and report the largest decrease.
///
/// Returns `None` when nothing decreased, when the comparison would be invalid,
/// or when every decrease sits under the noise floor.
///
/// # What makes a comparison invalid
///
/// AN ACCOUNT CHANGE, above all. Comparing a reading of one account with a
/// reading of another fabricates a transition that never happened — the exact
/// error that produced a false published claim about anthropic's reset instants,
/// where two accounts were read as one series. The caller's `account_changed`
/// already computes this for its own purposes and is passed in rather than
/// recomputed.
///
/// A previous slot with no usable entry is simply nothing to compare against.
pub fn detect(
    prev: &ProviderSlot,
    next: &Usage,
    account_changed: bool,
    completed: Instant,
    base_interval: Duration,
) -> DropObservation {
    // WHY THE REJECTIONS ARE NAMED RATHER THAN COLLAPSED TO ONE ABSENCE. A low
    // drop count has two completely different readings -- a quiet host where
    // quota simply did not come back, and a host where nothing was COMPARABLE
    // because its credentials were failing -- and from outside the counters those
    // are identical. A consumer measured an 84-minute credential latch that
    // produced no drop records at all and was invisible in the numbers
    // (insula#5).
    if account_changed {
        return DropObservation::NotComparable(NotComparable::AccountChanged);
    }

    let Some(prev_entry) = prev.entry.as_ref() else {
        return DropObservation::NotComparable(NotComparable::NoPriorReading);
    };
    if prev_entry.error.is_some() {
        return DropObservation::NotComparable(NotComparable::PriorReadingWasAnError);
    }
    let Some(prev_usage) = prev_entry.usage.as_ref() else {
        return DropObservation::NotComparable(NotComparable::NoPriorReading);
    };

    // Past here the comparison HAPPENED, so what follows is an answer about the
    // account rather than a statement about our ability to look.
    let Some(magnitude) = largest_decrease(prev_usage, next) else {
        return DropObservation::NoDrop;
    };
    if magnitude < NOISE_FLOOR_PERCENT {
        return DropObservation::NoDrop;
    }

    // Measured from the previous SUCCESS rather than the previous attempt: a run
    // of failures between two successes is exactly the gap this flag is about,
    // and `last_attempt_at` advances on every one of them, so it would report a
    // continuous series across an outage.
    //
    // BOTH CLOCKS, LARGER GAP WINS, because each is blind to something the other
    // sees and the two blindnesses point opposite ways:
    //
    //   `Instant` on macOS is CLOCK_UPTIME_RAW, which std documents as not
    //   incrementing while the system is asleep -- so a ten-hour suspend reads as
    //   no gap at all, and the flag would claim continuity across the single most
    //   likely cause of a real one.
    //
    //   The wall clock counts the suspend, but can also be moved by NTP. A
    //   BACKWARD step would shrink the gap and manufacture continuity.
    //
    // The monotonic gap is a true lower bound on elapsed awake time; the wall gap
    // includes suspended time. Taking the larger means a suspend and a forward
    // step both read as a gap ("inferred", which understates confidence and is
    // safe), while a backward step falls back to the monotonic reading (correct).
    // Claiming continuity you do not have is the only unsafe direction here,
    // because it is the claim a consumer acts on.
    let horizon = base_interval * CONTINUOUS_MULTIPLIER;
    let monotonic_gap = prev
        .last_success_at
        .map(|previous| completed.duration_since(previous));
    let wall_gap = prev
        .last_success_wall
        .and_then(|previous| (chrono::Utc::now() - previous).to_std().ok());
    let observed_continuously = match (monotonic_gap, wall_gap) {
        (Some(monotonic), Some(wall)) => monotonic.max(wall) <= horizon,
        (Some(gap), None) | (None, Some(gap)) => gap <= horizon,
        // Nothing to measure the interval with. Not continuous by default: an
        // unmeasurable gap is exactly the case the flag exists to disclose.
        (None, None) => false,
    };

    DropObservation::Drop(QuotaDrop {
        magnitude,
        observed_continuously,
    })
}

/// The largest decrease across paired windows, ignoring drip pools.
///
/// PAIRED BY IDENTITY, NOT POSITION. Slots pair slot-for-slot and extras pair by
/// id, because a window appearing or disappearing shifts everything after it —
/// and comparing a 5-hour window against a weekly one manufactures a drop out of
/// two unrelated numbers. Windows with no counterpart are skipped: a window that
/// did not exist last time has nothing to have decreased from.
fn largest_decrease(prev: &Usage, next: &Usage) -> Option<f64> {
    let mut largest: Option<f64> = None;

    let slots = [
        (prev.primary.as_ref(), next.primary.as_ref()),
        (prev.secondary.as_ref(), next.secondary.as_ref()),
        (prev.tertiary.as_ref(), next.tertiary.as_ref()),
    ];
    for (before, after) in slots {
        consider(before, after, &mut largest);
    }

    if let (Some(before_extras), Some(after_extras)) = (
        prev.extra_rate_windows.as_ref(),
        next.extra_rate_windows.as_ref(),
    ) {
        for after in after_extras {
            let Some(before) = before_extras
                .iter()
                .find(|candidate| candidate.id == after.id)
            else {
                continue;
            };
            consider(before.window.as_ref(), after.window.as_ref(), &mut largest);
        }
    }

    largest
}

/// Fold one paired window into the running maximum decrease.
fn consider(before: Option<&RateWindow>, after: Option<&RateWindow>, largest: &mut Option<f64>) {
    let (Some(before), Some(after)) = (before, after) else {
        return;
    };

    // A stated drip mechanic makes a lower reading ordinary rather than
    // informative. Checked on the NEW window: it describes the account as it is
    // now, and a provider that started stating a mechanic between the two
    // readings should be believed immediately rather than after one more tick.
    if after
        .regeneration
        .as_ref()
        .is_some_and(|regeneration| regeneration.mechanic == DRIP)
    {
        return;
    }

    // The RAW figures where present. Banked-reset relaxation publishes a zeroed
    // `used_percent` while the account is armed, so comparing the effective
    // number would report a drop the moment relaxation engages and another when
    // it disarms -- neither of which is quota returning.
    let before_percent = before.raw_used_percent.unwrap_or(before.used_percent);
    let after_percent = after.raw_used_percent.unwrap_or(after.used_percent);
    if !before_percent.is_finite() || !after_percent.is_finite() {
        return;
    }

    let decrease = before_percent - after_percent;
    if decrease > 0.0 && largest.is_none_or(|current| decrease > current) {
        *largest = Some(decrease);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ExtraWindow, ProviderUsage, Regeneration};
    use crate::refresh::{Incarnation, ProviderSlot};

    const BASE: Duration = Duration::from_secs(60);

    /// Unwrap a `Drop`, naming what arrived instead.
    ///
    /// The outcome is printed rather than asserted away: a test that expected a
    /// drop and got `NotComparable(PriorReadingWasAnError)` has a FIXTURE
    /// problem, and a bare unwrap hides which of the two went wrong.
    fn expect_drop(observation: DropObservation) -> QuotaDrop {
        match observation {
            DropObservation::Drop(drop) => drop,
            other => panic!("expected a drop, got {other:?}"),
        }
    }

    fn window(used_percent: f64) -> RateWindow {
        RateWindow {
            used_percent,
            raw_used_percent: None,
            resets_at: None,
            window_minutes: None,
            used_count: None,
            total_count: None,
            regeneration: None,
        }
    }

    fn usage(primary: Option<RateWindow>) -> Usage {
        Usage {
            primary,
            secondary: None,
            tertiary: None,
            extra_rate_windows: None,
        }
    }

    /// A slot serving `usage`, last successful `age` ago.
    fn slot_at(now: Instant, usage_value: Usage, age: Duration) -> ProviderSlot {
        let mut slot = ProviderSlot::due_now(now, Incarnation::from_counter(1));
        slot.entry = Some(ProviderUsage::healthy("codex", None, "oauth", usage_value));
        slot.last_success_at = Some(now - age);
        // Both clocks, as production sets them: they are written in one
        // expression precisely so a fixture cannot drift into a state the
        // refresher never produces.
        slot.last_success_wall =
            Some(chrono::Utc::now() - chrono::Duration::from_std(age).unwrap());
        slot
    }

    /// A decrease past the floor, seen one interval apart, is a drop.
    #[test]
    fn a_decrease_across_one_interval_is_an_observed_drop() {
        let now = Instant::now();
        let prev = slot_at(now, usage(Some(window(92.0))), BASE);
        let found = expect_drop(detect(&prev, &usage(Some(window(0.0))), false, now, BASE));

        assert!((found.magnitude - 92.0).abs() < 0.001);
        assert!(
            found.observed_continuously,
            "one base interval apart is the ordinary polling cadence"
        );
    }

    /// The same decrease seen across a long gap is NOT trusted.
    ///
    /// The twin of the test above, and a separate case because the pair is the
    /// point: identical percentages, opposite confidence, decided only by the
    /// interval between the readings. A gap hides things -- a drop plus later
    /// consumption reads smaller than it was, and a drop followed by a re-fill
    /// reads as no drop at all.
    #[test]
    fn the_same_decrease_across_a_gap_is_not_observed_continuously() {
        let now = Instant::now();
        let prev = slot_at(
            now,
            usage(Some(window(92.0))),
            Duration::from_secs(9 * 3600),
        );
        let found = expect_drop(detect(&prev, &usage(Some(window(0.0))), false, now, BASE));

        assert!((found.magnitude - 92.0).abs() < 0.001);
        assert!(
            !found.observed_continuously,
            "nine hours is not a continuous series"
        );
    }

    /// A suspend is a GAP even though the monotonic clock did not notice.
    ///
    /// THE CASE THE FLAG EXISTS FOR, and the one it could not previously report.
    /// `Instant` on macOS is CLOCK_UPTIME_RAW, documented in std as not
    /// incrementing while the system is asleep, so a slot whose monotonic age is
    /// one ordinary interval can have a wall age of ten hours -- exactly what a
    /// laptop lid-close leaves behind. Reading only the monotonic clock reports
    /// continuity across the single most likely cause of a real gap.
    ///
    /// Built by making the two clocks disagree, because no test can suspend a
    /// machine and the awake case makes them agree exactly.
    #[test]
    fn a_suspend_shaped_clock_disagreement_is_not_observed_continuously() {
        let now = Instant::now();
        let mut prev = slot_at(now, usage(Some(window(92.0))), BASE);
        // Monotonic says one interval; the wall says ten hours.
        prev.last_success_wall = Some(chrono::Utc::now() - chrono::Duration::hours(10));

        let found = expect_drop(detect(&prev, &usage(Some(window(0.0))), false, now, BASE));
        assert!(
            !found.observed_continuously,
            "ten hours of wall time is a gap, whatever the monotonic clock counted"
        );
    }

    /// A backward wall step does not manufacture continuity.
    ///
    /// The control for taking the larger gap. NTP can move the wall clock
    /// backwards, which would SHRINK the measured interval and claim a continuous
    /// series across a real outage -- the one unsafe direction, because continuity
    /// is the claim a consumer acts on. The monotonic reading is a true lower
    /// bound and holds the line.
    #[test]
    fn a_backward_wall_step_does_not_manufacture_continuity() {
        let now = Instant::now();
        let mut prev = slot_at(
            now,
            usage(Some(window(92.0))),
            Duration::from_secs(9 * 3600),
        );
        // The wall clock has been stepped back to just now, while the monotonic
        // clock still records nine hours.
        prev.last_success_wall = Some(chrono::Utc::now());

        let found = expect_drop(detect(&prev, &usage(Some(window(0.0))), false, now, BASE));
        assert!(
            !found.observed_continuously,
            "a clock step must not turn a nine-hour outage into a continuous series"
        );
    }

    /// An account change refuses the comparison entirely.
    ///
    /// THE LOAD-BEARING CASE. Comparing a reading of one account against a
    /// reading of another fabricates a transition nobody made -- and this is not
    /// hypothetical: a published claim about anthropic's reset instants came from
    /// exactly that mistake, two accounts read as one series. A credential swap
    /// makes the numbers look like a reset every time.
    #[test]
    fn a_changed_account_is_never_a_drop() {
        let now = Instant::now();
        let prev = slot_at(now, usage(Some(window(92.0))), BASE);
        assert_eq!(
            detect(&prev, &usage(Some(window(0.0))), true, now, BASE),
            DropObservation::NotComparable(NotComparable::AccountChanged),
            "two accounts are not one series"
        );
    }

    /// A drip pool going down is Tuesday, not a drop.
    ///
    /// The dependency that blocked this whole feature: without a stated mechanic
    /// a detector keyed on a decrease fires every tick for a continuously
    /// replenishing pool.
    #[test]
    fn a_stated_drip_mechanic_suppresses_the_decrease() {
        let now = Instant::now();
        let mut after = window(10.0);
        after.regeneration = Some(Regeneration {
            mechanic: "drip".to_string(),
            rate: None,
        });
        let prev = slot_at(now, usage(Some(window(90.0))), BASE);

        assert_eq!(
            detect(&prev, &usage(Some(after)), false, now, BASE),
            DropObservation::NoDrop,
            "a pool that refills continuously reads lower as a matter of course"
        );
    }

    /// A stated CLIFF mechanic does not suppress it.
    ///
    /// The control for the test above. Without this, suppressing every window
    /// that states any mechanic would pass -- and jetbrains, the only emitter
    /// today, states `cliff`.
    #[test]
    fn a_stated_cliff_mechanic_leaves_the_decrease_visible() {
        let now = Instant::now();
        let mut after = window(10.0);
        after.regeneration = Some(Regeneration {
            mechanic: "cliff".to_string(),
            rate: None,
        });
        let prev = slot_at(now, usage(Some(window(90.0))), BASE);

        assert!(
            matches!(
                detect(&prev, &usage(Some(after)), false, now, BASE),
                DropObservation::Drop(_)
            ),
            "a cliff window dropping IS the event this counts"
        );
    }

    /// Rounding wobble under the floor is not a drop.
    #[test]
    fn a_decrease_below_the_noise_floor_is_not_counted() {
        let now = Instant::now();
        let prev = slot_at(now, usage(Some(window(50.5))), BASE);
        assert_eq!(
            detect(&prev, &usage(Some(window(50.0))), false, now, BASE),
            DropObservation::NoDrop,
            "half a point is upstream arithmetic, not capacity returning"
        );
    }

    /// Banked-reset relaxation must not read as quota returning.
    ///
    /// While armed, the effective percent is published as zero and the truth
    /// moves to `raw_used_percent`. Comparing the effective number would report a
    /// drop the moment relaxation engages and another when it disarms, neither of
    /// which is a change in the account.
    #[test]
    fn relaxation_engaging_is_not_a_drop() {
        let now = Instant::now();
        let mut relaxed = window(0.0);
        relaxed.raw_used_percent = Some(92.0);
        let prev = slot_at(now, usage(Some(window(92.0))), BASE);

        assert_eq!(
            detect(&prev, &usage(Some(relaxed)), false, now, BASE),
            DropObservation::NoDrop,
            "the account did not change; only which number is effective did"
        );
    }

    /// Extras pair by id, so a new pool cannot manufacture a drop.
    ///
    /// Position pairing would compare whichever windows happened to line up. Here
    /// the only shared id did not move, and a second pool appears at a lower
    /// percentage -- which position pairing would report as a large decrease.
    #[test]
    fn extra_windows_pair_by_id_rather_than_position() {
        let now = Instant::now();
        let extra = |id: &str, used: f64| ExtraWindow {
            id: Some(id.to_string()),
            title: None,
            window: Some(window(used)),
        };
        let before = Usage {
            primary: None,
            secondary: None,
            tertiary: None,
            extra_rate_windows: Some(vec![extra("gemini", 40.0)]),
        };
        let after = Usage {
            primary: None,
            secondary: None,
            tertiary: None,
            extra_rate_windows: Some(vec![extra("claude-gpt", 5.0), extra("gemini", 40.0)]),
        };
        let prev = slot_at(now, before, BASE);

        assert_eq!(
            detect(&prev, &after, false, now, BASE),
            DropObservation::NoDrop,
            "a pool that did not exist before has nothing to have decreased from"
        );
    }

    /// A slot that never succeeded is NOT COMPARABLE, which is not the same as
    /// having found no drop.
    ///
    /// The distinction the three-outcome type exists for: a cold slot and a quiet
    /// account both used to report the same absence, and an operator reading the
    /// counters could not tell a host that saw nothing from a host that could not
    /// look.
    #[test]
    fn a_slot_with_no_previous_entry_is_not_comparable() {
        let now = Instant::now();
        let prev = ProviderSlot::due_now(now, Incarnation::from_counter(1));
        assert_eq!(
            detect(&prev, &usage(Some(window(0.0))), false, now, BASE),
            DropObservation::NotComparable(NotComparable::NoPriorReading)
        );
    }

    /// A degraded previous reading is named as its own reason.
    ///
    /// The one that made an 84-minute credential latch invisible: every tick in
    /// that window had a previous entry carrying an error, so nothing was
    /// comparable and the drop counters stayed silent in a way indistinguishable
    /// from a quiet host.
    #[test]
    fn a_degraded_previous_reading_is_named_as_its_own_reason() {
        let now = Instant::now();
        let mut prev = ProviderSlot::due_now(now, Incarnation::from_counter(1));
        prev.entry = Some(ProviderUsage::degraded(
            "codex",
            "provider returned HTTP 401",
        ));
        prev.last_success_at = Some(now - BASE);

        assert_eq!(
            detect(&prev, &usage(Some(window(0.0))), false, now, BASE),
            DropObservation::NotComparable(NotComparable::PriorReadingWasAnError),
            "a latched credential must not read as a quiet account"
        );
    }

    /// Going UP is not a drop.
    #[test]
    fn ordinary_consumption_is_not_a_drop() {
        let now = Instant::now();
        let prev = slot_at(now, usage(Some(window(10.0))), BASE);
        assert_eq!(
            detect(&prev, &usage(Some(window(40.0))), false, now, BASE),
            DropObservation::NoDrop
        );
    }
}
