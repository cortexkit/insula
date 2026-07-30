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

use chrono::{DateTime, Utc};
use cortexkit_provider_usage::{ProviderUsage, RateWindow};

/// How far past a window's own length its reset may sit before it is
/// misattributed. A window resets at most one window-length from now, plus a
/// margin for clock skew and for upstreams that round to the next hour.
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
    pub findings: Vec<String>,
}

impl SanityReport {
    /// True when the sweep examined no window at all, whatever the reason.
    ///
    /// Distinct from having no findings: this is the answer being unavailable
    /// rather than favourable.
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

    report
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
    use cortexkit_provider_usage::Usage;

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

    fn entry(window: RateWindow) -> ProviderUsage {
        ProviderUsage::healthy(
            "codex",
            Some("acct".into()),
            "oauth",
            Usage {
                primary: Some(window),
                ..Usage::default()
            },
        )
    }

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
    }
}
