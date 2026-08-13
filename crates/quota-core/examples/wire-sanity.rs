//! Run the wire-sanity checks against this crate's own registry.
//!
//! The checks themselves live in `quota_core::wire_sanity` so that this and the
//! deployed-module checker run the same ones, and so they are unit-tested.
//!
//! Run: `cargo run -p quota-core --example wire-sanity`
//! Filter to one provider: `cargo run -p quota-core --example wire-sanity -- codex`
//!
//! Exits non-zero when something disagrees, so it can gate a deploy.
//!
//! **This examines the local-credential lane only.** The registry it builds has
//! no credential-vault client, because that client lives in the module crate
//! rather than here, so every provider whose credentials are served by the vault
//! falls back to whatever local credentials exist and reports no account. Rows
//! here therefore read `unlabeled` for providers that serve several labelled
//! accounts in production, and a provider with no local credentials at all shows
//! as degraded even while it is healthy on the deployed module.
//!
//! To check what production actually publishes, including the vault lane, use
//! the module-side checker: `cargo run -p quota-module --example deployed-sanity`.

use chrono::Utc;
use quota_core::wire_sanity;

#[path = "live_support/mod.rs"]
mod live_support;

fn main() {
    let filter = std::env::args().nth(1);
    let rt = tokio::runtime::Runtime::new().expect("build runtime");
    let (entries, warm_up) =
        rt.block_on(async { live_support::collect_live_usage(filter.as_deref()).await });

    // An empty result would otherwise print as a clean pass. It is reported as a
    // failure to check rather than as a check that found nothing.
    if entries.is_empty() {
        println!(
            "no entries after {:.0}s of warm-up: nothing was checked",
            warm_up.as_secs_f64()
        );
        std::process::exit(2);
    }

    let report = wire_sanity::check_entries(&entries, Utc::now());
    // Name the lane in the OUTPUT, not only in the file header. Both checkers
    // finish with "findings: none", and a clean run of this one is easily
    // mistaken for a clean run of the deployed one -- which examines strictly
    // more, because the vault lane exists only there. A reader who cannot tell
    // the two apart from the transcript has a pass for a check that never ran.
    println!("lane: local credentials only (vault lane NOT examined; see deployed-sanity)");
    println!(
        "entries: {} ({} degraded)   windows checked: {}   pools checked: {} ({} amounts, {} bound comparisons)   providers compared: {}   warm-up {:.0}s",
        report.entries,
        report.degraded,
        report.windows_checked,
        report.pools_checked,
        report.pool_amounts_checked,
        report.pool_comparisons,
        report.providers_compared,
        warm_up.as_secs_f64()
    );

    // Findings are reported before the no-windows exit. The cross-entry checks
    // examine degraded entries too, so an all-degraded array can still carry real
    // findings -- exiting on "nothing examined" first would discard precisely the
    // ones that survived the condition suppressing everything else.
    if !report.findings.is_empty() {
        println!("findings: {}", report.findings.len());
        for finding in &report.findings {
            println!("  {finding}");
        }
        std::process::exit(1);
    }
    // Every entry could be degraded, which is a legitimate state but means no
    // window was examined. Saying "no findings" there would claim a check that
    // did not happen.
    if report.examined_nothing() {
        println!("no windows to check: every entry is degraded");
        std::process::exit(2);
    }
    println!("findings: none");
}
