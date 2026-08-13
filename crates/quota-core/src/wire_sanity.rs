//! Internal-consistency checks over a published `usage.get` array.
//!
//! Some defects cannot be caught by a parser test: they need real upstream data,
//! and they are only visible by comparing fields of one window against each
//! other. A 300-minute window claiming it resets in 36 hours is well-formed,
//! parses cleanly, and is wrong — the reset belonged to a different window. That
//! shipped once and was found by a person reading the output.
//!
//! Every check compares fields that must agree, so none of them needs to know
//! what any provider's correct number is. That is what lets them run against
//! live data whose values change hourly.
//!
//! These live in the crate rather than in a diagnostic binary so that both the
//! local-lane and the deployed-module checkers run the *same* checks, and so the
//! checks themselves can be unit-tested. A checker whose logic exists only
//! inside an example is the one piece of the pipeline nothing verifies.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use cortexkit_provider_usage::{ProviderUsage, RateWindow};

/// How far ahead of the reader's clock a `fetchedAt` may sit before it is
/// treated as wrong rather than as clock skew.
///
/// The producer stamps this from its own clock and a consumer reads it from
/// another, so a small lead is ordinary. A large one is not skew: it means the
/// timestamp was derived rather than observed, and an entry stamped in the
/// future ages backwards.
const FUTURE_TOLERANCE_SECS: i64 = 120;

/// How far past a window's own length its reset may sit before the reset is
/// treated as belonging to a different window.
///
/// A window resets at most one window-length from now, so anything beyond that
/// is either a misattributed timestamp or a mis-stated length. A 300-minute
/// window claiming a reset 36 hours out is the shape: both fields are
/// well-formed and only their pairing is impossible, which is why no check of
/// either field alone would notice.
///
/// The allowance is proportional plus fixed: the ratio absorbs rounding on long
/// cadences, and the fixed minutes keep a short window from being judged by a
/// margin too small to cover an upstream that rounds up to the next hour.
const RESET_SLACK_RATIO: f64 = 1.05;
const RESET_SLACK_MINUTES: f64 = 60.0;

/// How far in the past a reset may sit before it is stale rather than
/// just-crossed. A window whose reset has passed is normal for a few minutes:
/// the upstream has rolled and the cached copy has not been refetched yet.
const PAST_RESET_GRACE_MINUTES: f64 = 60.0;

/// Longest window length treated as a real quota cadence, in minutes.
///
/// Deliberately far above any cadence a provider publishes -- the longest here
/// is monthly -- because the point is not to police plausible values but to
/// catch a length that is not a duration at all. A year of slack costs nothing
/// and keeps this from firing on some future quarterly plan.
pub const MAX_WINDOW_MINUTES: i64 = 366 * 24 * 60;

/// How far the count pair may drift from the reported percent. The two are often
/// computed from different precisions upstream, so this is loose enough to
/// ignore rounding and tight enough to catch a mismatched pairing.
const COUNT_PERCENT_TOLERANCE: f64 = 2.0;

/// The current instant, so callers need not depend on `chrono` themselves just
/// to supply one.
///
/// The checks take an explicit `now` because a caller that passed the wall clock
/// implicitly could not be unit-tested against fixed timestamps.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

/// What a sweep examined, alongside what it found.
///
/// The counts are part of the result rather than an afterthought: a sweep that
/// finds nothing because it examined nothing reports exactly like a clean one,
/// and that has already happened here. A caller must be able to tell "everything
/// agrees" from "there was nothing to compare".
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SanityReport {
    pub entries: usize,
    pub degraded: usize,
    pub windows_checked: usize,
    /// Spend pools examined by the pool rules.
    ///
    /// Counted separately from windows because the two populations are
    /// independent: most providers publish windows and no pools, so a sweep can
    /// check dozens of windows while every pool rule sits idle. Without its own
    /// denominator a run that examined no pool reports the same `findings: none`
    /// as one that examined hundreds, and the six pool rules are indistinguishable
    /// from rules that never fire.
    pub pools_checked: usize,
    /// How many providers had their sibling entries compared against each other.
    ///
    /// Separate from `windows_checked` because the two can diverge sharply: an
    /// array of entirely degraded entries examines no window while still
    /// admitting every cross-entry check, and a single-account host admits the
    /// within-provider checks while never exercising a multi-account one.
    pub providers_compared: usize,
    pub findings: Vec<String>,
}

impl SanityReport {
    /// True when the sweep examined no window at all, whatever the reason.
    ///
    /// Distinct from having no findings: this is the answer being unavailable
    /// rather than favourable.
    ///
    /// Only about windows. The cross-entry checks run over degraded entries too,
    /// so a caller must report `findings` before acting on this — otherwise the
    /// condition that says "nothing was examined" also discards what was found.
    pub fn examined_nothing(&self) -> bool {
        self.windows_checked == 0
    }
}

/// Check every window of every healthy entry, optionally filtered to one
/// provider.
pub fn check_entries(entries: &[ProviderUsage], now: DateTime<Utc>) -> SanityReport {
    // Accumulated outside the report because the entry walk borrows
    // `report.findings` mutably for the duration.
    let mut pools_checked = 0usize;
    let mut report = SanityReport {
        entries: entries.len(),
        ..SanityReport::default()
    };

    for entry in entries {
        check_entry_shape(entry, now, &mut report.findings, &mut pools_checked);
        if entry.error.is_some() {
            report.degraded += 1;
            continue;
        }
        for (label, window) in windows_of(entry) {
            report.windows_checked += 1;
            let where_ = format!(
                "{}/{}/{label}",
                entry.provider,
                entry.account.as_deref().unwrap_or("unlabeled")
            );
            check_window(&where_, window, now, &mut report.findings);
        }
    }

    check_across_entries(entries, &mut report);

    report.pools_checked = pools_checked;
    report
}

/// Check one entry against the promise that it says something.
/// Check the prepaid pools on one entry.
///
/// Every window field on this wire has a range rule; money had none until these,
/// which is backwards. A percent that is wrong by a factor of a hundred is a
/// misrouted request, and a BALANCE wrong by a factor of a hundred is a spend
/// decision made against a figure nobody can reconcile.
///
/// These check internal consistency only, exactly like the window rules: nothing
/// here knows what any provider's credit is worth, and no rule fires on a value
/// merely because it is large.
fn check_pools(
    entry: &ProviderUsage,
    where_: &str,
    findings: &mut Vec<String>,
    pools_checked: &mut usize,
) {
    let Some(pools) = entry.spend.as_ref() else {
        return;
    };
    *pools_checked += pools.len();

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for pool in pools {
        // A consumer selecting "only granted credit" keys on the id. Two pools
        // sharing one leaves that selection ambiguous, and the ambiguity is
        // silent: whichever the consumer happens to reach first decides how much
        // money it believes is available.
        if !seen.insert(pool.id.as_str()) {
            findings.push(format!(
                "{where_}: publishes two pools with id '{}', so a policy naming it selects arbitrarily",
                pool.id
            ));
        }

        // An empty id cannot be keyed on at all, which makes the pool
        // unselectable while still contributing to any total a consumer sums.
        if pool.id.trim().is_empty() {
            findings.push(format!("{where_}: publishes a pool with an empty id"));
        }

        for (field, amount) in [("remaining", &pool.remaining), ("total", &pool.total)] {
            let Some(amount) = amount else { continue };

            // An amount with no denomination states a quantity of nothing, and a
            // consumer rendering it will invent a unit for it.
            if amount.unit.trim().is_empty() {
                findings.push(format!(
                    "{where_}: pool '{}' states a {field} with no unit",
                    pool.id
                ));
            }

            // The exponent scales the figure by a power of ten, so an implausible
            // one is not a cosmetic error: at exponent 9 a balance of 24.02 would
            // have been published as 24020000000 minor units, and a consumer
            // dividing correctly still gets the right answer while one that
            // ignores the exponent is off by a billion. Nine decimal places is
            // past any real money or credit precision.
            if amount.exponent > 9 {
                findings.push(format!(
                    "{where_}: pool '{}' states a {field} with exponent {}, which no currency or credit uses",
                    pool.id, amount.exponent
                ));
            }
        }

        // A pool whose total is smaller than what remains in it describes an
        // account holding more than its own ceiling. One of the two figures is
        // wrong and this cannot say which, which is why it reports rather than
        // picks.
        if let (Some(remaining), Some(total)) = (&pool.remaining, &pool.total) {
            if remaining.unit == total.unit && remaining.exponent == total.exponent {
                if remaining.minor > total.minor {
                    findings.push(format!(
                        "{where_}: pool '{}' has more remaining ({}) than its total ({})",
                        pool.id, remaining.minor, total.minor
                    ));
                }
            } else {
                // Comparing across units or scales would require converting one
                // to the other, and inventing a rate is worse than declining.
                findings.push(format!(
                    "{where_}: pool '{}' states remaining and total in different terms ({} e-{} vs {} e-{}), so neither bounds the other",
                    pool.id, remaining.unit, remaining.exponent, total.unit, total.exponent
                ));
            }
        }
    }
}

fn check_entry_shape(
    entry: &ProviderUsage,
    now: DateTime<Utc>,
    findings: &mut Vec<String>,
    pools_checked: &mut usize,
) {
    let where_ = format!(
        "{}/{}",
        entry.provider,
        entry.account.as_deref().unwrap_or("unlabeled")
    );

    // An entry is a capacity reading or a stated failure. Carrying neither says
    // nothing at all while still occupying a row, so a consumer counting
    // published providers counts it and a consumer reading capacity finds none:
    // the two disagree about the same entry.
    // Pools count as something stated. A provider can sell credit and publish no
    // rate limits at all, and such an entry has no usage and no error while
    // being entirely healthy -- its whole answer is the balance.
    check_pools(entry, &where_, findings, pools_checked);

    if entry.usage.is_none() && entry.error.is_none() && !states_a_pool(entry) {
        findings.push(format!("{where_}: entry carries neither usage nor error"));
    }

    // A usage object with no window in it is the same defect one level in: it
    // reads as a successful capacity reading and carries no capacity. It is what
    // an entry becomes when every window it had was dropped for carrying a
    // percent that could not be published, and the result is worse than an
    // absent entry, because a consumer reducing an account to its most
    // constrained window silently gets a smaller answer from the accounts that
    // survived -- or, if this was the only account, another account's standing
    // reported as this one's.
    //
    // Unless the entry also publishes a non-empty pool list, which is a capacity
    // statement of a different kind: a provider selling credit with no rate
    // limits has nothing to put in a window and its whole answer is the balance.
    //
    // Checked here rather than trusted to the emission path: the drop and the
    // shape rule are far apart, and the wire is where the two have to agree.
    if entry.usage.is_some() && windows_of(entry).is_empty() && !states_a_pool(entry) {
        findings.push(format!(
            "{where_}: carries a usage object with no window, which reads as a reading and states nothing"
        ));
    }

    // The same defect one level further in, and it survives the check above
    // whenever a sibling window remains. An extra window is a NAMED pool, and
    // dropping only its percent leaves the name published with nothing behind
    // it -- a consumer keying on that id finds an entry and no capacity, which
    // reads as a pool that exists and is unmeasured rather than one whose figure
    // could not be published.
    //
    // It matters most where the extras ARE the account's real limits: some
    // providers publish pooled windows there and leave the slots for a headline,
    // so an emptied extra is a whole pool silently absent from anything summing
    // them.
    for extra in entry
        .usage
        .as_ref()
        .and_then(|usage| usage.extra_rate_windows.as_ref())
        .into_iter()
        .flatten()
    {
        if extra.window.is_none() {
            let named = extra
                .id
                .as_deref()
                .or(extra.title.as_deref())
                .unwrap_or("unnamed");
            findings.push(format!(
                "{where_}: extra window {named:?} is published with no window behind it"
            ));
        }
    }

    // Consumers are told to age every entry on `fetchedAt` and never on their own
    // poll time, so usage without it leaves them no honest way to decide how old
    // the reading is. Both available answers are wrong: treat it as current and a
    // window from hours ago prices as live, or discard it and a healthy provider
    // vanishes.
    //
    // Unreachable by construction -- a slot serving usage has always succeeded at
    // least once, and the one path that clears that timestamp also suppresses the
    // entry -- which is exactly why it is worth asserting against the published
    // array rather than against the code that builds it.
    if entry.usage.is_some() && entry.fetched_at.is_none() {
        findings.push(format!(
            "{where_}: carries usage but no fetchedAt, leaving consumers no way to age it"
        ));
    }

    let Some(text) = entry.fetched_at.as_deref() else {
        return;
    };
    let Ok(fetched_at) = DateTime::parse_from_rfc3339(text) else {
        findings.push(format!("{where_}: fetchedAt is unparseable: {text}"));
        return;
    };

    // A timestamp ahead of now makes an entry age backwards: it reads as fresher
    // the longer it sits, so a stale window never crosses any threshold a
    // consumer sets. The tolerance covers ordinary clock movement between the
    // module stamping the value and this reading it; anything beyond that means
    // the timestamp was computed rather than observed.
    let ahead = fetched_at.with_timezone(&Utc).signed_duration_since(now);
    if ahead > chrono::Duration::seconds(FUTURE_TOLERANCE_SECS) {
        findings.push(format!(
            "{where_}: fetchedAt is {}s in the future",
            ahead.num_seconds()
        ));
    }
}

/// Check the invariants that hold between sibling entries of one provider.
///
/// These are promises the emission path makes and consumers rely on, and they
/// are invisible to any check that reads one entry at a time. They are asserted
/// against the published array rather than against the code that builds it,
/// because that is the artifact consumers actually receive.
fn check_across_entries(entries: &[ProviderUsage], report: &mut SanityReport) {
    let mut by_provider: BTreeMap<&str, Vec<&ProviderUsage>> = BTreeMap::new();
    for entry in entries {
        by_provider
            .entry(entry.provider.as_str())
            .or_default()
            .push(entry);
    }

    for (provider, siblings) in &by_provider {
        report.providers_compared += 1;

        let unlabelled = siblings.iter().filter(|e| e.account.is_none()).count();
        let labelled = siblings.len() - unlabelled;

        // One provider may publish several labelled accounts, or exactly one
        // unlabelled entry when identity could not be resolved for all of its
        // credentials. Two unlabelled entries are indistinguishable from each
        // other, so a consumer keying on (provider, account) cannot tell which
        // is which and has no basis for preferring either.
        if unlabelled > 1 {
            report.findings.push(format!(
                "{provider}: {unlabelled} unlabelled entries, which are indistinguishable to a consumer"
            ));
        }

        // Mixing the two is worse than either alone: the unlabelled row may be
        // the same account as one of the labelled ones, so a consumer summing
        // per-account capacity can count one account twice without any duplicate
        // key to notice.
        if unlabelled > 0 && labelled > 0 {
            report.findings.push(format!(
                "{provider}: {labelled} labelled and {unlabelled} unlabelled entries in the same array"
            ));
        }

        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in siblings {
            if let Some(account) = entry.account.as_deref() {
                *seen.entry(account).or_default() += 1;
            }
        }
        for (account, count) in seen {
            if count > 1 {
                report.findings.push(format!(
                    "{provider}/{account}: {count} entries for one account"
                ));
            }
        }

        // `apiProvider` is a property of the provider, not of the account, so
        // every sibling must agree. A consumer that reads it from whichever
        // entry it happens to hold would otherwise route two accounts of one
        // provider to different pricing tables.
        let distinct: BTreeSet<Option<&str>> =
            siblings.iter().map(|e| e.api_provider.as_deref()).collect();
        if distinct.len() > 1 {
            let mut rendered: Vec<&str> = distinct
                .iter()
                .map(|value| value.unwrap_or("(absent)"))
                .collect();
            rendered.sort_unstable();
            report.findings.push(format!(
                "{provider}: sibling entries disagree on apiProvider: {}",
                rendered.join(", ")
            ));
        }
    }
}

/// Whether an entry publishes at least one prepaid balance or credit pool.
///
/// Used by the shape rules above, which exist to catch an entry that occupies a
/// row and states nothing. A non-empty `spend` list is a statement about the
/// account's prepaid pools, and that is the whole answer for a provider selling
/// credit with no rate limits to report.
///
/// Presence is all this asks. Whether a pool's `remaining` is known, or whether
/// it may currently be drawn on, are separate questions with their own fields --
/// a pool whose balance the provider will not state still tells a consumer the
/// pool exists, which is more than an empty entry says.
///
/// An EMPTY pool list is deliberately not a statement. `spend: []` says the
/// producer looked and found no pools, which leaves the same hole as no usage at
/// all, so an entry carrying only an empty list still fails those rules.
fn states_a_pool(entry: &ProviderUsage) -> bool {
    entry.spend.as_ref().is_some_and(|pools| !pools.is_empty())
}

/// Every window an entry publishes.
///
/// Built by destructuring `Usage` rather than by naming slots, so a slot added to
/// the wire type fails to compile here instead of being silently skipped. A
/// checker that quietly examines fewer windows than it appears to reports a clean
/// result for the wrong reason.
fn windows_of(entry: &ProviderUsage) -> Vec<(String, &RateWindow)> {
    let Some(usage) = entry.usage.as_ref() else {
        return Vec::new();
    };
    let cortexkit_provider_usage::Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows,
    } = usage;

    let mut out = Vec::new();
    for (name, slot) in [
        ("primary", primary),
        ("secondary", secondary),
        ("tertiary", tertiary),
    ] {
        if let Some(window) = slot {
            out.push((name.to_string(), window));
        }
    }
    for extra in extra_rate_windows.iter().flatten() {
        if let Some(window) = extra.window.as_ref() {
            let name = extra
                .title
                .as_deref()
                .or(extra.id.as_deref())
                .unwrap_or("extra");
            out.push((name.to_string(), window));
        }
    }
    out
}

/// Whether a window length is a duration at all, rather than a value that has
/// lost its meaning on the way here.
///
/// Some providers derive this from an upstream number, and the conversion to an
/// integer saturates rather than failing: a nonsense float arrives as a
/// nonsense length instead of an error.
///
/// Shared with the normalizers that derive a length, so the value a provider is
/// willing to emit and the value this checker accepts cannot drift apart --
/// which would leave one of them reporting on a shape the other never produces.
pub fn plausible_window_length(minutes: i64) -> bool {
    minutes > 0 && minutes <= MAX_WINDOW_MINUTES
}

fn check_window(where_: &str, window: &RateWindow, now: DateTime<Utc>, findings: &mut Vec<String>) {
    let percent = window.used_percent;

    // Checked in its own right, not only where it is used as a bound. A window
    // length is a claim about cadence that consumers read directly -- and it is
    // also the ceiling for the reset check below, so a nonsense length silently
    // disables that check for this window rather than making it fail.
    if let Some(length) = window.window_minutes {
        if !plausible_window_length(length) {
            findings.push(format!(
                "{where_}: windowMinutes is not a plausible duration: {length}"
            ));
        }
    }
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        findings.push(format!("{where_}: usedPercent out of range: {percent}"));
    }
    if let Some(raw) = window.raw_used_percent {
        if !raw.is_finite() || !(0.0..=100.0).contains(&raw) {
            findings.push(format!("{where_}: rawUsedPercent out of range: {raw}"));
        }
    }

    if let Some(text) = window.resets_at.as_deref() {
        match DateTime::parse_from_rfc3339(text) {
            Err(_) => findings.push(format!("{where_}: resetsAt is unparseable: {text}")),
            Ok(reset) => {
                let minutes_ahead = (reset.with_timezone(&Utc) - now).num_seconds() as f64 / 60.0;
                if minutes_ahead < -PAST_RESET_GRACE_MINUTES {
                    findings.push(format!(
                        "{where_}: resetsAt is {:.0}m in the past",
                        -minutes_ahead
                    ));
                }
                // The check the misattributed-reset defect would have failed: a
                // reset further out than the window is long belongs to a
                // different window.
                //
                // Skipped for an implausible length, which is reported on its own
                // below. Deriving the ceiling from a nonsense length would put it
                // beyond any reachable reset time, so the check would pass in
                // silence for the one window whose data is known to be wrong.
                if let Some(length) = window
                    .window_minutes
                    .filter(|length| plausible_window_length(*length))
                {
                    let ceiling = length as f64 * RESET_SLACK_RATIO + RESET_SLACK_MINUTES;
                    if minutes_ahead > ceiling {
                        findings.push(format!(
                            "{where_}: resets in {minutes_ahead:.0}m but the window is only {length}m long"
                        ));
                    }
                }
            }
        }
    }

    // A count is a quantity of things -- requests, messages, tokens -- so a
    // fractional one is not a rounding blemish, it is evidence the number was
    // COMPUTED rather than measured. The wire type is f64 for range, not to
    // permit fractions, and a consumer storing counts as integers rejects the
    // whole response over one such value.
    //
    // This rule exists because the note that used to stand here observed that
    // the agreement check below cannot fire on a derived count, and stopped
    // there. That was the wrong conclusion: an unfalsifiable check is a reason
    // to find a check that DOES bite, not a limitation to document. A live
    // fractional count then reached a consumer and cost it every provider's
    // capacity on the response.
    for (field, value) in [
        ("usedCount", window.used_count),
        ("totalCount", window.total_count),
    ] {
        if let Some(value) = value {
            if value.is_finite() && value.fract() != 0.0 {
                findings.push(format!(
                    "{where_}: {field} {value} is not a whole number, so it was                      derived rather than reported"
                ));
            }
        }
    }

    // Where a provider recovers `used_count` from the percentage and the cap,
    // this comparison returns the reported percentage by construction and
    // cannot fire. It holds for any provider that publishes an absolute count
    // the upstream measured, and catches a producer that computed one of the
    // three from the other two incorrectly.
    if let (Some(used), Some(total)) = (window.used_count, window.total_count) {
        if used > total {
            findings.push(format!(
                "{where_}: usedCount {used} exceeds totalCount {total}"
            ));
        }
        // Only meaningful against the raw percent: a relaxed window reports an
        // effective zero that the counts are not expected to match.
        let reported = window.raw_used_percent.unwrap_or(percent);
        if total > 0.0 {
            let implied = used / total * 100.0;
            if (implied - reported).abs() > COUNT_PERCENT_TOLERANCE {
                findings.push(format!(
                    "{where_}: counts imply {implied:.1}% but the window reports {reported:.1}%"
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_provider_usage::{Amount, ExtraWindow, Pool, PoolBasis, PoolFunding, Usage};

    fn window(percent: f64) -> RateWindow {
        RateWindow {
            used_percent: percent,
            raw_used_percent: None,
            window_minutes: Some(300),
            resets_at: None,
            used_count: None,
            total_count: None,
        }
    }

    fn pool(id: &str, remaining: Option<(i64, u8, &str)>, total: Option<(i64, u8, &str)>) -> Pool {
        Pool {
            id: id.to_string(),
            label: id.to_string(),
            funding: PoolFunding::Unknown,
            remaining: remaining.map(|(minor, exponent, unit)| Amount {
                minor,
                exponent,
                unit: unit.to_string(),
            }),
            total: total.map(|(minor, exponent, unit)| Amount {
                minor,
                exponent,
                unit: unit.to_string(),
            }),
            basis: PoolBasis::Reported,
            spendable: None,
        }
    }

    fn entry_with_pools(pools: Vec<Pool>) -> ProviderUsage {
        let mut entry =
            ProviderUsage::healthy("deepseek", Some("acct-A".into()), "api", Usage::default());
        entry.fetched_at = Some(stamped_at());
        entry.spend = Some(pools);
        entry
    }

    /// Two pools sharing an id make a spend policy that names it select
    /// arbitrarily, and the arbitrariness is silent.
    #[test]
    fn duplicate_pool_ids_are_a_finding() {
        let entry = entry_with_pools(vec![
            pool("granted", Some((100, 2, "USD")), None),
            pool("granted", Some((250, 2, "USD")), None),
        ]);
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("two pools with id 'granted'")),
            "{:?}",
            report.findings
        );
    }

    /// A pool with no id cannot be selected, only summed.
    ///
    /// The worse half of the pair: a duplicate id makes a spend policy pick
    /// arbitrarily between two real pools, while an empty one leaves a pool that
    /// no policy can name at all -- excluded from every selection while still
    /// contributing to any total a consumer adds up. An account then appears to
    /// hold money that "only granted credit" and "only purchased credit" both
    /// decline to touch.
    #[test]
    fn an_empty_pool_id_is_a_finding() {
        let entry = entry_with_pools(vec![pool("   ", Some((100, 2, "USD")), None)]);
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("pool with an empty id")),
            "{:?}",
            report.findings
        );
    }

    /// A pool holding more than its own ceiling has one wrong figure, and the
    /// checker reports rather than picking which.
    #[test]
    fn remaining_above_total_is_a_finding() {
        let entry = entry_with_pools(vec![pool(
            "credits",
            Some((5_000, 2, "USD")),
            Some((1_000, 2, "USD")),
        )]);
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report.findings.iter().any(|f| f.contains("more remaining")),
            "{:?}",
            report.findings
        );
    }

    /// Remaining and total in different units do not bound each other, and
    /// converting between them would mean inventing a rate.
    #[test]
    fn a_total_in_other_terms_is_a_finding() {
        let entry = entry_with_pools(vec![pool(
            "credits",
            Some((100, 2, "USD")),
            Some((100, 2, "CNY")),
        )]);
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("different terms")),
            "{:?}",
            report.findings
        );
    }

    /// An exponent past any real precision scales the figure by a power of ten,
    /// so a consumer ignoring it is wrong by that factor.
    #[test]
    fn an_implausible_exponent_is_a_finding() {
        let entry = entry_with_pools(vec![pool("credits", Some((1, 12, "USD")), None)]);
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report.findings.iter().any(|f| f.contains("exponent 12")),
            "{:?}",
            report.findings
        );
    }

    /// An amount with no denomination states a quantity of nothing.
    #[test]
    fn an_amount_without_a_unit_is_a_finding() {
        let entry = entry_with_pools(vec![pool("credits", Some((100, 2, "  ")), None)]);
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report.findings.iter().any(|f| f.contains("no unit")),
            "{:?}",
            report.findings
        );
    }

    /// The control: a well-formed pool set must fire NONE of the rules above.
    ///
    /// Holds a healthy instance of every shape they inspect -- two distinct ids,
    /// a remaining below its total in matching terms, an ordinary exponent, and
    /// a stated unit -- so an over-wide rule cannot pass by examining nothing.
    #[test]
    fn a_well_formed_pool_set_is_silent() {
        let entry = entry_with_pools(vec![
            pool(
                "granted_balance",
                Some((0, 2, "CNY")),
                Some((1_000, 2, "CNY")),
            ),
            pool("topped_up_balance", Some((2_402, 2, "CNY")), None),
        ]);
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report.findings.is_empty(),
            "a well-formed pool set must be silent: {:?}",
            report.findings
        );
    }

    /// A provider selling only credit publishes no window, and that is healthy.
    ///
    /// The shape rules above exist to catch an entry that occupies a row and
    /// states nothing. A pool is a statement -- it says what the account can
    /// still spend -- so an entry whose whole answer is a balance must pass,
    /// even though it has no usage and no error.
    ///
    /// Without this the first balance-only provider would be reported as
    /// malformed by our own checker on every run, which is how a real signal
    /// gets trained away.
    #[test]
    fn an_entry_whose_only_answer_is_a_pool_is_not_a_finding() {
        let mut entry = ProviderUsage::degraded("deepseek", "placeholder");
        entry.error = None;
        entry.error_class = None;
        entry.spend = Some(vec![Pool {
            id: "granted_balance".to_string(),
            label: "Granted".to_string(),
            funding: PoolFunding::Granted,
            remaining: Some(Amount {
                minor: 1000,
                exponent: 2,
                unit: "CNY".to_string(),
            }),
            total: None,
            basis: PoolBasis::Reported,
            spendable: None,
        }]);

        entry.fetched_at = Some(stamped_at());
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report.findings.is_empty(),
            "a balance-only entry must not be a finding: {:?}",
            report.findings
        );
    }

    /// An EMPTY pool list states nothing, and must still be a finding.
    ///
    /// The distinction is the same one this wire draws everywhere else: absent
    /// means the producer has nothing to say, empty means it looked and found
    /// none. An entry carrying neither usage, nor error, nor any actual pool
    /// leaves exactly the hole the rule exists to catch -- so tolerating pools
    /// must not become tolerating the field.
    #[test]
    fn an_empty_pool_list_does_not_rescue_an_empty_entry() {
        let mut entry = ProviderUsage::degraded("deepseek", "placeholder");
        entry.error = None;
        entry.error_class = None;
        entry.spend = Some(Vec::new());

        entry.fetched_at = Some(stamped_at());
        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("neither usage nor error")),
            "an empty pool list must not count as a statement: {:?}",
            report.findings
        );
    }

    /// The checker's walk must reach every window the decision paths reach.
    ///
    /// `windows_of` here and `model::windows` are separate implementations of
    /// one rule: the first names windows for a report, the second feeds the
    /// at-wall test that spends a banked credit and the transform that rewrites
    /// what consumers pace on. Nothing but this keeps them in step.
    ///
    /// A disagreement is invisible in the direction that matters. If this walk
    /// reaches fewer windows, the checker examines a smaller denominator than
    /// exists and reports a clean sweep over the part it happened to look at --
    /// the windows it skipped are exactly the ones no check ever runs against.
    #[test]
    fn the_checker_walks_the_same_windows_as_the_decision_paths() {
        // Every shape at once: a hole between slots, an extra window, and an
        // extra entry naming a limit whose figure could not be read.
        let usage = Usage {
            primary: Some(window(10.0)),
            secondary: None,
            tertiary: Some(window(99.0)),
            extra_rate_windows: Some(vec![
                ExtraWindow {
                    title: Some("named".into()),
                    id: None,
                    window: Some(window(50.0)),
                },
                ExtraWindow {
                    title: Some("unreadable".into()),
                    id: None,
                    window: None,
                },
            ]),
        };
        let mut published =
            ProviderUsage::healthy("codex", Some("acct".into()), "oauth", usage.clone());
        published.fetched_at = Some(stamped_at());

        let checker: Vec<f64> = windows_of(&published)
            .into_iter()
            .map(|(_, window)| window.used_percent)
            .collect();
        let decisions: Vec<f64> = crate::model::windows(&usage)
            .map(|window| window.used_percent)
            .collect();

        assert_eq!(checker, decisions);
        // Not vacuous: both must actually span the hole and the unreadable
        // extra, rather than agreeing on a truncated list.
        assert_eq!(checker, vec![10.0, 99.0, 50.0]);
    }

    /// A published entry, stamped the way the module stamps one.
    ///
    /// `fetched_at` is set because every entry carrying usage has it in
    /// production: a slot serving a window has succeeded at least once. A
    /// fixture without it would model a state the module cannot emit, and would
    /// have quietly disabled the timestamp checks for every test built on it.
    fn entry(window: RateWindow) -> ProviderUsage {
        let mut entry = ProviderUsage::healthy(
            "codex",
            Some("acct".into()),
            "oauth",
            Usage {
                primary: Some(window),
                ..Usage::default()
            },
        );
        entry.fetched_at = Some(stamped_at());
        entry
    }

    /// A plausible last-success time, anchored to the clock the tests drive.
    ///
    /// The checks take an explicit `now` so they can be tested against fixed
    /// timestamps; a fixture reading the real clock instead would sit days from
    /// that instant and fail for a reason that has nothing to do with the test.
    fn stamped_at() -> String {
        (at(FIXTURE_NOW) - chrono::Duration::minutes(2)).to_rfc3339()
    }

    /// The instant the fixtures are built around. Tests that care about a
    /// different clock pass their own to `check_entries`.
    const FIXTURE_NOW: &str = "2026-07-28T10:00:00Z";

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// The checks must pass a plausible window, or every other assertion here
    /// could be satisfied by a checker that rejects everything.
    #[test]
    fn a_coherent_window_produces_no_findings() {
        let mut w = window(40.0);
        w.resets_at = Some("2026-07-28T13:00:00Z".into());
        w.used_count = Some(400.0);
        w.total_count = Some(1000.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings, Vec::<String>::new());
        // The denominator matters as much as the verdict: this must be a clean
        // result *from having checked*, not from having skipped.
        assert_eq!(report.windows_checked, 1);
        assert!(!report.examined_nothing());
    }

    /// The defect this whole checker exists for: a reset that belongs to a
    /// different, longer window. It is well-formed and parses; only its
    /// relationship to the window's own length gives it away.
    /// A length that is not a duration is reported in its own right.
    ///
    /// Some providers derive this from an upstream number through a conversion
    /// that saturates rather than failing, so a nonsense value arrives looking
    /// like an ordinary length.
    #[test]
    fn a_window_length_that_is_not_a_duration_is_reported() {
        for length in [0, -300, i64::MAX, 400 * 24 * 60] {
            let mut w = window(50.0);
            w.window_minutes = Some(length);

            let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.contains("not a plausible duration")),
                "length {length} produced {:?}",
                report.findings
            );
        }
    }

    /// Real cadences are not reported, including the longest one served.
    ///
    /// Without this, a check that rejected everything would pass the test above
    /// while making every window a finding.
    #[test]
    fn ordinary_window_lengths_are_not_reported() {
        for length in [1, 300, 10080, 43200, MAX_WINDOW_MINUTES] {
            let mut w = window(50.0);
            w.window_minutes = Some(length);

            let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

            assert_eq!(
                report.findings,
                Vec::<String>::new(),
                "length {length} was reported"
            );
        }
    }

    /// An implausible length is reported once, not twice.
    ///
    /// The reset ceiling is derived from the window's own length, so a negative
    /// length yields a negative ceiling and every reset looks too far out. That
    /// second finding is arithmetic on a value already known to be broken, and
    /// it invites reading the reset as the problem when the length is.
    ///
    /// This is the whole of what skipping the ceiling buys. A huge length hides
    /// the reset check instead of tripping it -- the ceiling lands beyond any
    /// reachable time -- and no filter can recover a check whose bound is
    /// nonsense. Reporting the length itself is what covers that case.
    #[test]
    fn an_implausible_length_is_reported_once_without_a_derived_finding() {
        let mut w = window(50.0);
        w.window_minutes = Some(-300);
        w.resets_at = Some("2026-07-30T10:00:00Z".into());

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        assert!(
            report.findings[0].contains("not a plausible duration"),
            "{}",
            report.findings[0]
        );
    }

    #[test]
    fn a_reset_further_out_than_the_window_is_long_is_reported() {
        let mut w = window(50.0);
        w.window_minutes = Some(300);
        w.resets_at = Some("2026-07-29T22:00:00Z".into());

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        assert!(
            report.findings[0].contains("but the window is only 300m long"),
            "{}",
            report.findings[0]
        );
    }

    /// A reset that has already passed means the producer stopped refreshing.
    ///
    /// Every window this module publishes is rebuilt from a fetch, so a reset
    /// timestamp in the past cannot survive a successful one. Seeing it means
    /// the entry is older than it looks and a consumer pacing against that
    /// window believes it is about to be replenished when nothing is coming.
    ///
    /// A grace period covers ordinary clock skew between this host and the
    /// provider, so only a reset well past its due time is reported.
    #[test]
    fn a_reset_already_in_the_past_is_reported() {
        let mut w = window(50.0);
        w.window_minutes = Some(300);
        w.resets_at = Some("2026-07-28T08:00:00Z".into());

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        assert!(
            report.findings[0].contains("in the past"),
            "{}",
            report.findings[0]
        );
    }

    /// Skew smaller than the grace period is not a finding.
    ///
    /// Without this the test above would pass against a rule that reports every
    /// reset at or before the current instant, which would fire constantly on a
    /// host whose clock runs slightly ahead of a provider's.
    #[test]
    fn a_reset_barely_in_the_past_is_tolerated_as_clock_skew() {
        let mut w = window(50.0);
        w.window_minutes = Some(300);
        w.resets_at = Some("2026-07-28T09:59:00Z".into());

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    /// Spending more of an allowance than it holds is not a coherent reading.
    ///
    /// The two counts come from the same upstream payload, so one exceeding the
    /// other means they were read from fields that do not describe the same
    /// allowance -- a normalizer pairing a lifetime total with a windowed count,
    /// say. The percent beside them can look entirely ordinary, which is why
    /// this is checked separately rather than inferred from the percent.
    #[test]
    fn a_used_count_above_the_total_is_reported() {
        // The counts exceed the total by a hair, and the percent agrees with
        // them. A larger overrun would also trip the counts-versus-percent rule
        // beside this one, and the test would then pass whether or not this
        // rule exists -- so the fixture is chosen to leave this rule as the only
        // explanation for a finding.
        let mut w = window(100.0);
        w.used_count = Some(10_001.0);
        w.total_count = Some(10_000.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        assert!(
            report.findings[0].contains("usedCount 10001 exceeds totalCount 10000"),
            "{}",
            report.findings[0]
        );
    }

    /// A fractional count is reported, because it proves the number was derived.
    ///
    /// The fixture is the live value that caused this rule to exist: qwen-cloud
    /// published 45.052361473854994% against a 40000 cap and multiplied the two,
    /// and a consumer storing counts as integers rejected the entire response --
    /// every provider's capacity, over one provider's arithmetic.
    /// The pool rules report their own denominator.
    ///
    /// Windows and pools are independent populations: most providers publish
    /// windows and no pools at all, so a sweep can check dozens of windows while
    /// every pool rule sits idle. Reported separately because a run that
    /// examined no pool otherwise looks identical to one that examined
    /// hundreds — both say `findings: none`, and the six pool rules are then
    /// indistinguishable from rules that never fire.
    #[test]
    fn the_pool_rules_report_how_many_pools_they_examined() {
        let mut with_pools = entry(window(10.0));
        with_pools.spend = Some(vec![
            pool("granted", Some((500, 2, "USD")), None),
            pool("purchased", Some((250, 2, "USD")), None),
        ]);

        let report = check_entries(&[with_pools, entry(window(10.0))], at(FIXTURE_NOW));
        assert_eq!(
            report.pools_checked, 2,
            "both pools of the one entry that has them must be counted"
        );

        // Zero is reported rather than hidden, which is the whole point: an
        // entry set with no pools must be visibly different from one with them.
        let none = check_entries(&[entry(window(10.0))], at(FIXTURE_NOW));
        assert_eq!(none.pools_checked, 0);
        assert!(
            none.findings.is_empty(),
            "and a run examining no pool is still a clean run, not a finding"
        );
    }

    #[test]
    fn a_count_that_is_not_a_whole_number_is_reported() {
        let mut w = window(45.052_361_473_854_994);
        w.used_count = Some(18_020.944_589_542);
        w.total_count = Some(40_000.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        assert!(
            report.findings[0].contains("usedCount")
                && report.findings[0].contains("not a whole number"),
            "{}",
            report.findings[0]
        );
    }

    /// A fractional TOTAL is reported too, not only the used half.
    ///
    /// The paired direction: a cap is as much a count as the consumption is, and
    /// a rule written for one field is exactly how the other goes unchecked.
    #[test]
    fn a_fractional_total_is_reported() {
        let mut w = window(10.0);
        w.used_count = Some(100.0);
        w.total_count = Some(1_000.5);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("totalCount") && f.contains("not a whole number")),
            "{:?}",
            report.findings
        );
    }

    /// Whole-number counts are silent.
    ///
    /// The must-not-fire control, carrying a healthy instance of the exact shape
    /// the rule inspects: both count fields present and integral, with a
    /// percentage that agrees with them.
    #[test]
    fn whole_number_counts_are_not_reported() {
        let mut w = window(45.0);
        w.used_count = Some(18_000.0);
        w.total_count = Some(40_000.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.contains("not a whole number")),
            "{:?}",
            report.findings
        );
    }

    /// Counts that fill the allowance exactly are not a finding.
    ///
    /// An exhausted window reports its used count equal to its total, and
    /// reporting that would fire on every provider that runs out.
    #[test]
    fn a_used_count_equal_to_the_total_is_not_reported() {
        let mut w = window(100.0);
        w.used_count = Some(10_000.0);
        w.total_count = Some(10_000.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }

    #[test]
    fn an_out_of_range_percent_is_reported_at_both_ends() {
        for bad in [-1.0, 101.0, f64::NAN] {
            let report = check_entries(&[entry(window(bad))], at("2026-07-28T10:00:00Z"));
            assert!(
                report.findings.iter().any(|f| f.contains("usedPercent")),
                "{bad} accepted"
            );
        }
    }

    /// `rawUsedPercent` needs its own bound, and its own test.
    ///
    /// It is a separate field with a separate source: where `usedPercent` can be
    /// zeroed by the read-time relaxation, this carries the figure the provider
    /// actually reported, so a consumer showing a user their real standing reads
    /// this one. A rule guarding the neighbouring field says nothing about it.
    ///
    /// The bad value is paired with an in-range `usedPercent` so the finding has
    /// exactly one cause: a fixture with both out of range would pass while this
    /// rule did nothing.
    #[test]
    fn an_out_of_range_raw_percent_is_reported_at_both_ends() {
        for bad in [-1.0, 101.0, f64::NAN] {
            let mut w = window(40.0);
            w.raw_used_percent = Some(bad);

            let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

            assert_eq!(
                report.findings,
                vec![format!(
                    "codex/acct/primary: rawUsedPercent out of range: {bad}"
                )],
                "{bad} accepted"
            );
        }
    }

    /// A reset timestamp that will not parse is a finding.
    ///
    /// Around a dozen providers carry the upstream's own reset string through
    /// rather than formatting one from an epoch, so this is the field most
    /// exposed to an upstream changing shape. A consumer parses it to decide
    /// when capacity returns; one that cannot be parsed is either dropped or
    /// treated as absent, and both readings are wrong in the same direction as
    /// a missing window.
    ///
    /// This rule sits in a `match` arm rather than behind an `if`, which is why
    /// it wants an explicit test: a mechanical sweep that mutates conditions
    /// skips it, and a skipped rule reads exactly like a covered one in the
    /// sweep's output.
    #[test]
    fn an_unparseable_reset_is_reported() {
        let mut w = window(40.0);
        w.resets_at = Some("not-a-timestamp".to_string());

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(
            report.findings,
            vec!["codex/acct/primary: resetsAt is unparseable: not-a-timestamp".to_string()]
        );
    }

    /// The paired silent case: a well-formed reset must stay quiet.
    ///
    /// Without it, a version reporting every reset as unparseable passes, since
    /// the neighbouring rules on this field only run once parsing has succeeded.
    #[test]
    fn a_parseable_reset_is_not_reported() {
        let mut w = window(40.0);
        w.resets_at = Some("2026-07-28T12:00:00Z".to_string());

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings, Vec::<String>::new());
        // Not vacuous: the window was examined, so the silence is the rule
        // declining to fire rather than the sweep skipping it.
        assert_eq!(report.windows_checked, 1);
    }

    /// The paired silent case, carrying a HEALTHY `rawUsedPercent`.
    ///
    /// Without a healthy instance of the field the rule inspects, an over-wide
    /// version -- one firing whenever the field is present at all -- passes
    /// every other test in this file, since no other fixture sets it.
    ///
    /// The value is deliberately not equal to `usedPercent`: this field exists
    /// precisely because the two differ when a relaxation is in effect, and a
    /// fixture where they agree would also pass a rule that compared them.
    #[test]
    fn a_raw_percent_inside_the_range_is_not_reported() {
        let mut w = window(0.0);
        w.raw_used_percent = Some(58.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings, Vec::<String>::new());
        // Not vacuous: the window really was examined, so the silence is the
        // rule declining to fire rather than the sweep skipping it.
        assert_eq!(report.windows_checked, 1);
    }

    #[test]
    fn counts_disagreeing_with_the_percent_are_reported() {
        let mut w = window(40.0);
        w.used_count = Some(900.0);
        w.total_count = Some(1000.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert!(
            report.findings.iter().any(|f| f.contains("counts imply")),
            "{:?}",
            report.findings
        );
    }

    /// A relaxed window reports an effective zero beside the provider's real
    /// percent, and the counts describe the real one. Comparing them against the
    /// effective zero would report every banked-reset account as inconsistent.
    #[test]
    fn a_relaxed_window_is_checked_against_its_raw_percent() {
        let mut w = window(0.0);
        w.raw_used_percent = Some(70.0);
        w.used_count = Some(700.0);
        w.total_count = Some(1000.0);

        let report = check_entries(&[entry(w)], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings, Vec::<String>::new());
    }

    /// A degraded entry has no windows, so a sweep over nothing but degraded
    /// entries must report that it examined nothing rather than that it agreed.
    #[test]
    fn an_all_degraded_array_reports_that_nothing_was_examined() {
        let report = check_entries(
            &[ProviderUsage::degraded(
                "codex",
                "no session: not configured",
            )],
            at("2026-07-28T10:00:00Z"),
        );

        assert!(report.examined_nothing());
        assert_eq!(report.degraded, 1);
        assert_eq!(report.findings, Vec::<String>::new());
        // The cross-entry checks do not need a window, so they still ran.
        assert_eq!(report.providers_compared, 1);
    }

    /// An entry can lose every window it had and still look like a reading.
    ///
    /// The percent is the load-bearing field, so a window carrying one that
    /// cannot be published is dropped. Drop them all and what remains is a
    /// `usage` object with nothing in it: not a failure, not an absence, and
    /// indistinguishable from a successful fetch of an account with no limits.
    ///
    /// It has to be caught here because it is worse than an absent entry. A
    /// consumer reducing an account to its most constrained window gets a
    /// smaller answer from whichever accounts survived -- and if this was the
    /// only account of its provider, another account's standing is reported as
    /// this one's.
    #[test]
    fn a_usage_object_with_no_window_is_a_finding() {
        let mut entry =
            ProviderUsage::healthy("claude", Some("acct-A".into()), "vault", Usage::default());
        entry.fetched_at = Some(stamped_at());

        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));

        assert_eq!(
            report.findings,
            vec![
                "claude/acct-A: carries a usage object with no window, which reads as a reading and states nothing"
                    .to_string()
            ]
        );
    }

    /// An extra window whose percent was dropped leaves its NAME published with
    /// nothing behind it.
    ///
    /// This survives the empty-usage rule whenever a sibling window remains, so
    /// it needs its own check. The consequence is worse than a missing entry: a
    /// consumer keying on that id finds a pool that exists and is unmeasured,
    /// rather than one whose figure could not be published — and for the
    /// providers whose real limits live in extras rather than slots, it is a
    /// whole pool silently absent from anything summing them.
    #[test]
    fn an_extra_window_with_nothing_behind_it_is_a_finding() {
        let mut entry = ProviderUsage::healthy(
            "antigravity",
            Some("acct-A".into()),
            "vault",
            Usage {
                primary: Some(window(10.0)),
                extra_rate_windows: Some(vec![ExtraWindow {
                    id: Some("claude-and-gpt-weekly".into()),
                    title: Some("Claude and GPT models".into()),
                    window: None,
                }]),
                ..Usage::default()
            },
        );
        entry.fetched_at = Some(stamped_at());

        let report = check_entries(&[entry], at("2026-07-28T10:00:00Z"));

        assert_eq!(
            report.findings,
            vec![
                "antigravity/acct-A: extra window \"claude-and-gpt-weekly\" is published with no window behind it"
                    .to_string()
            ]
        );
        // Not vacuous: the sibling slot really is intact, so the entry passed the
        // empty-usage rule and this finding came from the extras check alone.
        assert_eq!(report.windows_checked, 1);
    }

    /// The paired must-not-fire case: the two shapes that legitimately carry no
    /// window must stay silent, or the rule above becomes noise and is ignored.
    ///
    /// A degraded entry has no `usage` at all -- it states a failure instead --
    /// and an entry with a real window is the ordinary case.
    #[test]
    fn an_entry_that_states_a_failure_or_carries_a_window_is_not_a_finding() {
        let degraded = ProviderUsage::degraded("codex", "no session: not configured");

        let mut healthy = ProviderUsage::healthy(
            "claude",
            Some("acct-B".into()),
            "vault",
            Usage {
                primary: Some(window(10.0)),
                // An intact extra window, because the extras rule is the one
                // most easily written too wide: a version firing on every extra
                // rather than only on an empty one passes every test that has no
                // healthy extra in it, and is caught by nothing until it floods
                // a live run.
                extra_rate_windows: Some(vec![ExtraWindow {
                    id: Some("gemini-weekly".into()),
                    title: Some("Gemini Models".into()),
                    window: Some(window(22.0)),
                }]),
                ..Usage::default()
            },
        );
        healthy.fetched_at = Some(stamped_at());

        let report = check_entries(&[degraded, healthy], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings, Vec::<String>::new());
        // Not vacuous: both the slot and the extra were really examined, so the
        // silence is the rules declining to fire rather than the sweep skipping
        // the array.
        assert_eq!(report.windows_checked, 2);
    }

    fn labelled(provider: &str, account: &str) -> ProviderUsage {
        let mut entry = ProviderUsage::healthy(
            provider,
            Some(account.into()),
            "vault",
            Usage {
                primary: Some(window(10.0)),
                ..Usage::default()
            },
        );
        entry.fetched_at = Some(stamped_at());
        entry
    }

    fn unlabelled(provider: &str) -> ProviderUsage {
        let mut entry = ProviderUsage::healthy(
            provider,
            None,
            "oauth",
            Usage {
                primary: Some(window(10.0)),
                ..Usage::default()
            },
        );
        entry.fetched_at = Some(stamped_at());
        entry
    }

    /// Several labelled accounts under one provider is the normal multi-account
    /// shape and must not be reported — otherwise the checks below could pass by
    /// flagging every provider that has more than one entry.
    #[test]
    fn sibling_entries_with_distinct_accounts_produce_no_findings() {
        let report = check_entries(
            &[labelled("claude", "acct-a"), labelled("claude", "acct-b")],
            at("2026-07-28T10:00:00Z"),
        );

        assert_eq!(report.findings, Vec::<String>::new());
        assert_eq!(report.providers_compared, 1);
        assert_eq!(report.windows_checked, 2);
    }

    /// Two unlabelled entries cannot be told apart by a consumer keying on
    /// (provider, account), so it has no basis for preferring either.
    #[test]
    fn two_unlabelled_entries_for_one_provider_are_reported() {
        let report = check_entries(
            &[unlabelled("grok"), unlabelled("grok")],
            at("2026-07-28T10:00:00Z"),
        );

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("2 unlabelled entries")),
            "{:?}",
            report.findings
        );
    }

    /// A single unlabelled entry is the legitimate shape when identity could not
    /// be resolved, so the check above must not fire on it.
    #[test]
    fn one_unlabelled_entry_alone_is_not_reported() {
        let report = check_entries(&[unlabelled("grok")], at("2026-07-28T10:00:00Z"));

        assert_eq!(report.findings, Vec::<String>::new());
    }

    /// Mixing labelled and unlabelled entries lets a consumer count one account
    /// twice: the unlabelled row may be the same account as a labelled one, and
    /// no duplicate key exists to reveal it.
    #[test]
    fn a_labelled_and_an_unlabelled_entry_together_are_reported() {
        let report = check_entries(
            &[labelled("codex", "acct-a"), unlabelled("codex")],
            at("2026-07-28T10:00:00Z"),
        );

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("labelled and 1 unlabelled")),
            "{:?}",
            report.findings
        );
    }

    #[test]
    fn two_entries_for_the_same_account_are_reported() {
        let report = check_entries(
            &[labelled("claude", "acct-a"), labelled("claude", "acct-a")],
            at("2026-07-28T10:00:00Z"),
        );

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("2 entries for one account")),
            "{:?}",
            report.findings
        );
    }

    /// `apiProvider` is derived from the provider name, so siblings disagreeing
    /// about it means a consumer reading it from an arbitrary entry would route
    /// two accounts of one provider to different pricing tables.
    #[test]
    fn sibling_entries_disagreeing_on_api_provider_are_reported() {
        let mut first = labelled("codex", "acct-a");
        first.api_provider = Some("openai".into());
        let second = labelled("codex", "acct-b");

        let report = check_entries(&[first, second], at("2026-07-28T10:00:00Z"));

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("disagree on apiProvider")),
            "{:?}",
            report.findings
        );
    }

    /// An entry with neither usage nor error occupies a row while saying nothing,
    /// so a consumer counting published providers and one reading capacity
    /// disagree about the same entry.
    #[test]
    fn an_entry_with_neither_usage_nor_error_is_reported() {
        let mut empty = labelled("kimi", "acct-a");
        empty.usage = None;

        let report = check_entries(&[empty], at("2026-07-28T10:00:00Z"));

        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("neither usage nor error")),
            "{:?}",
            report.findings
        );
    }

    /// The cross-entry checks do not read windows, so they survive an array in
    /// which every entry is degraded — the state that suppresses every other
    /// check. A caller must therefore report findings before acting on
    /// `examined_nothing`, or this finding is discarded by the condition that
    /// says nothing was examined.
    #[test]
    fn a_finding_survives_an_array_that_examined_no_window() {
        let report = check_entries(
            &[
                ProviderUsage::degraded("grok", "upstream error: flapped"),
                ProviderUsage::degraded("grok", "upstream error: flapped"),
            ],
            at("2026-07-28T10:00:00Z"),
        );

        assert!(report.examined_nothing());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.contains("2 unlabelled entries")),
            "{:?}",
            report.findings
        );
    }
    /// Consumers age every entry on `fetchedAt` and are told never to substitute
    /// their own poll time, so usage without it leaves them no honest option:
    /// treating it as current prices an old window as live, and discarding it
    /// makes a healthy provider disappear.
    #[test]
    fn usage_without_a_fetched_at_is_reported() {
        let mut e = entry(window(40.0));
        e.fetched_at = None;

        let report = check_entries(&[e], at(FIXTURE_NOW));

        assert_eq!(
            report.findings,
            vec!["codex/acct: carries usage but no fetchedAt, leaving consumers no way to age it"]
        );
        // Not vacuous: the window was still examined, so this is a finding from
        // checking rather than a side effect of skipping the entry.
        assert_eq!(report.windows_checked, 1);
    }

    /// A degraded entry legitimately has no `fetchedAt` when its slot has never
    /// succeeded, which is the ordinary state on a host without that credential.
    /// Reporting it would fire on most providers here and bury the real findings.
    #[test]
    fn a_degraded_entry_without_a_fetched_at_is_not_reported() {
        let report = check_entries(
            &[ProviderUsage::degraded(
                "codex",
                "no session: not configured",
            )],
            at(FIXTURE_NOW),
        );

        assert_eq!(report.findings, Vec::<String>::new());
        assert_eq!(report.degraded, 1);
    }

    /// An entry stamped ahead of the reader's clock ages *backwards*: it looks
    /// fresher the longer it sits, so a stale window never crosses a staleness
    /// threshold and a consumer keeps pacing against data that stopped updating.
    #[test]
    fn a_fetched_at_in_the_future_is_reported() {
        let mut e = entry(window(40.0));
        e.fetched_at = Some((at(FIXTURE_NOW) + chrono::Duration::hours(3)).to_rfc3339());

        let report = check_entries(&[e], at(FIXTURE_NOW));

        assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
        assert!(
            report.findings[0].contains("fetchedAt is 10800s in the future"),
            "{:?}",
            report.findings
        );
    }

    /// Clock skew between the module and whoever reads it is ordinary and must
    /// not be reported, or the check fires on healthy output and gets ignored.
    #[test]
    fn a_fetched_at_slightly_ahead_is_tolerated_as_clock_skew() {
        let mut e = entry(window(40.0));
        e.fetched_at = Some((at(FIXTURE_NOW) + chrono::Duration::seconds(30)).to_rfc3339());

        let report = check_entries(&[e], at(FIXTURE_NOW));

        assert_eq!(report.findings, Vec::<String>::new());
        assert_eq!(report.windows_checked, 1);
    }

    /// There is no upper bound on how stale a served entry may be: while
    /// failures stay transient this module keeps serving the last healthy
    /// window, so an old timestamp is honest reporting rather than a defect.
    /// Flagging it would turn a correct behaviour into a permanent finding.
    #[test]
    fn a_very_old_fetched_at_is_not_a_finding() {
        let mut e = entry(window(40.0));
        e.fetched_at = Some((at(FIXTURE_NOW) - chrono::Duration::days(7)).to_rfc3339());

        let report = check_entries(&[e], at(FIXTURE_NOW));

        assert_eq!(report.findings, Vec::<String>::new());
        assert_eq!(report.windows_checked, 1);
    }

    #[test]
    fn an_unparseable_fetched_at_is_reported() {
        let mut e = entry(window(40.0));
        e.fetched_at = Some("last tuesday".into());

        let report = check_entries(&[e], at(FIXTURE_NOW));

        assert_eq!(
            report.findings,
            vec!["codex/acct: fetchedAt is unparseable: last tuesday"]
        );
    }
}
