//! Check the live `usage.get` array for internally inconsistent windows.
//!
//! Some defects cannot be caught by a unit test: they need real upstream data,
//! and they are only visible by comparing fields of one window against each
//! other. A 300-minute window that claims it resets in 36 hours is well-formed,
//! parses cleanly, and is wrong — the reset belonged to a different window.
//! That shipped once and was found by a person reading the output.
//!
//! Every check here compares fields that must agree, so none of them needs to
//! know what any provider's correct number is.
//!
//! Run: `cargo run -p quota-core --example wire-sanity`
//! Filter to one provider: `cargo run -p quota-core --example wire-sanity -- codex`
//!
//! Exits non-zero when something disagrees, so it can gate a deploy.

use chrono::{DateTime, Utc};
use cortexkit_provider_usage::{ProviderUsage, RateWindow};

#[path = "live_support/mod.rs"]
mod live_support;

/// How far past a window's own length its reset may sit before we call it
/// misattributed. A window resets at most one window-length from now, plus a
/// margin for clock skew and for upstreams that round to the next hour.
const RESET_SLACK_RATIO: f64 = 1.05;
const RESET_SLACK_MINUTES: f64 = 60.0;

/// How far in the past a reset may sit before it is stale rather than
/// just-crossed. A window whose reset has passed is normal for a few minutes:
/// the upstream has rolled and our cached copy has not been refetched yet.
const PAST_RESET_GRACE_MINUTES: f64 = 60.0;

/// How far the count pair may drift from the reported percent. The two are
/// often computed from different precisions upstream, so this is loose enough
/// to ignore rounding and tight enough to catch a mismatched pairing.
const COUNT_PERCENT_TOLERANCE: f64 = 2.0;

fn main() {
    let filter = std::env::args().nth(1);
    let rt = tokio::runtime::Runtime::new().expect("build runtime");
    let (entries, warm_up) =
        rt.block_on(async { live_support::collect_live_usage(filter.as_deref()).await });

    // An empty result would otherwise print as a clean pass. It is reported as
    // a failure to check rather than as a check that found nothing.
    if entries.is_empty() {
        println!(
            "no entries after {:.0}s of warm-up: nothing was checked",
            warm_up.as_secs_f64()
        );
        std::process::exit(2);
    }

    let now = Utc::now();
    let mut findings = Vec::new();
    let mut checked = 0usize;
    let mut degraded = 0usize;

    for entry in &entries {
        if entry.error.is_some() {
            degraded += 1;
            continue;
        }
        for (label, window) in windows_of(entry) {
            checked += 1;
            let where_ = format!(
                "{}/{}/{label}",
                entry.provider,
                entry.account.as_deref().unwrap_or("unlabeled")
            );
            check_window(&where_, window, now, &mut findings);
        }
    }

    println!(
        "entries: {} ({degraded} degraded)   windows checked: {checked}   warm-up {:.0}s",
        entries.len(),
        warm_up.as_secs_f64()
    );
    // Every entry could be degraded, which is a legitimate state but means no
    // window was examined. Saying "no findings" there would claim a check that
    // did not happen.
    if checked == 0 {
        println!("no windows to check: every entry is degraded");
        std::process::exit(2);
    }
    if findings.is_empty() {
        println!("findings: none");
        return;
    }
    println!("findings: {}", findings.len());
    for finding in &findings {
        println!("  {finding}");
    }
    std::process::exit(1);
}

/// Every window an entry publishes.
///
/// Built by matching on the `Usage` struct rather than by naming slots, so a
/// slot added to the wire type fails to compile here instead of being silently
/// skipped. A checker that quietly examines fewer windows than it appears to
/// reports a clean result for the wrong reason.
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
                // The check the misattributed-reset defect would have failed:
                // a reset further out than the window is long belongs to a
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
