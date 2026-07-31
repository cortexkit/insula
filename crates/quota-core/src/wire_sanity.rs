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

/// How far past a window's own length its reset may sit before it is
/// misattributed. A window resets at most one window-length from now, plus a
/// margin for clock skew and for upstreams that round to the next hour.
/// How far ahead of the reader's clock a `fetchedAt` may sit before it is
/// treated as wrong rather than as clock skew.
///
/// The producer stamps this from its own clock and a consumer reads it from
/// another, so a small lead is ordinary. A large one is not skew: it means the
/// timestamp was derived rather than observed, and an entry stamped in the
/// future ages backwards.
const FUTURE_TOLERANCE_SECS: i64 = 120;

const RESET_SLACK_RATIO: f64 = 1.05;
const RESET_SLACK_MINUTES: f64 = 60.0;

/// How far in the past a reset may sit before it is stale rather than
/// just-crossed. A window whose reset has passed is normal for a few minutes:
/// the upstream has rolled and the cached copy has not been refetched yet.
const PAST_RESET_GRACE_MINUTES: f64 = 60.0;

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
    let mut report = SanityReport {
        entries: entries.len(),
        ..SanityReport::default()
    };

    for entry in entries {
        check_entry_shape(entry, now, &mut report.findings);
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

    report
}

/// Check one entry against the promise that it says something.
fn check_entry_shape(entry: &ProviderUsage, now: DateTime<Utc>, findings: &mut Vec<String>) {
    let where_ = format!(
        "{}/{}",
        entry.provider,
        entry.account.as_deref().unwrap_or("unlabeled")
    );

    // An entry is a capacity reading or a stated failure. Carrying neither says
    // nothing at all while still occupying a row, so a consumer counting
    // published providers counts it and a consumer reading capacity finds none:
    // the two disagree about the same entry.
    if entry.usage.is_none() && entry.error.is_none() {
        findings.push(format!("{where_}: entry carries neither usage nor error"));
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

fn check_window(where_: &str, window: &RateWindow, now: DateTime<Utc>, findings: &mut Vec<String>) {
    let percent = window.used_percent;
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
                if let Some(length) = window.window_minutes {
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
    use cortexkit_provider_usage::{ExtraWindow, Usage};

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
