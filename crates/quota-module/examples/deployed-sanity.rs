//! Run the wire-sanity checks against what the **deployed module** publishes.
//!
//! This is the companion to `quota-core`'s `wire-sanity` example, and the
//! difference is the point. That one builds a registry in-process, which has no
//! credential-vault client — so it cannot see the lane that serves most of the
//! labelled accounts in production, and a clean result there says nothing about
//! them. This connects to the running daemon as an ordinary consumer and checks
//! the array the module actually serves, vault lane included.
//!
//! The checks are `quota_core::wire_sanity`, shared with the other example rather
//! than reimplemented, so the two cannot drift into disagreeing about what a
//! coherent window is.
//!
//! Run: `cargo run -p quota-module --example deployed-sanity`
//! Filter to one provider: `cargo run -p quota-module --example deployed-sanity -- codex`
//!
//! Exits non-zero when something disagrees, or when it could not examine
//! anything, so it can gate a deploy.

use std::{path::PathBuf, time::Duration};

// Reached through `quota_core` rather than added as direct dependencies: this
// example must decode with the exact same type definition the module serves, and
// a separate dependency line here could drift to a different version of the
// shared crate while still compiling.
use quota_core::{model::ProviderUsage, wire_sanity};

#[path = "../tests/common/mod.rs"]
mod common;

/// Where the daemon writes its connection file. Overridable so this can be
/// pointed at a non-default daemon.
fn connection_file() -> PathBuf {
    if let Ok(path) = std::env::var("SUBC_CONNECTION_FILE") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".local/share/cortexkit/run/subc-connection.json")
}

#[tokio::main]
async fn main() {
    let filter = std::env::args().nth(1);
    let path = connection_file();
    if !path.exists() {
        eprintln!("no daemon connection file at {}", path.display());
        eprintln!("the daemon must be running: this checks the deployed module, not a local build");
        std::process::exit(2);
    }

    let mut stream = common::connect_consumer(&path).await;
    common::wait_for_catalog(&mut stream, common::MODULE_ID, Duration::from_secs(10)).await;
    let route = common::route_open(&mut stream, &std::env::temp_dir(), 1).await;
    let body = common::usage_get(&mut stream, route, 2).await;

    let entries: Vec<ProviderUsage> = serde_json::from_value(body["result"].clone())
        .expect("usage.get result must decode as ProviderUsage[]");

    // Decoding through the wire type rather than reading JSON keys by hand is
    // deliberate: a field renamed in the shared crate has to fail here, instead
    // of silently reading as absent and shrinking what gets checked.
    let entries: Vec<ProviderUsage> = match filter.as_deref() {
        Some(name) => entries
            .into_iter()
            .filter(|entry| entry.provider == name)
            .collect(),
        None => entries,
    };

    if entries.is_empty() {
        match filter.as_deref() {
            Some(name) => println!("no entries for {name}: nothing was checked"),
            None => println!("the module published an empty array: nothing was checked"),
        }
        std::process::exit(2);
    }

    let report = wire_sanity::check_entries(&entries, wire_sanity::now());
    let labelled = entries
        .iter()
        .filter(|entry| entry.account.is_some())
        .count();
    let vault = entries
        .iter()
        .filter(|entry| entry.source.as_deref() == Some("vault"))
        .count();

    println!(
        "entries: {} ({} degraded, {labelled} labelled, {vault} vault-served)   windows checked: {}   providers compared: {}",
        report.entries, report.degraded, report.windows_checked, report.providers_compared
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
    if report.examined_nothing() {
        println!("no windows to check: every entry is degraded");
        std::process::exit(2);
    }
    println!("findings: none");
}
