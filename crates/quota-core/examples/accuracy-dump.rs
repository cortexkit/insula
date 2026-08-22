//! Dump the live `usage.get` array as JSON, as this crate's registry serves it.
//!
//! **Not identical to what the deployed module serves.** The registry built here
//! has no credential-vault client -- that lives in the module crate -- so
//! vault-served providers fall back to local credentials, report no account, and
//! appear here as a single unlabelled row (or as degraded, when no local
//! credential exists) even while the deployed module publishes several healthy
//! labelled accounts for them. Compare account labels against the running module,
//! not against this dump.
//!
//! This is the module side of the codexbar-vs-module accuracy diff: compare the
//! per-provider windows (usedPercent, resetsAt, windowMinutes) against what
//! CodexBar shows for the same machine credentials. It exercises the real
//! provider fetchers (no mocks), and it is the ONLY check of CORRESPONDENCE this
//! repo has -- `wire-sanity` and `deployed-sanity` check that our numbers are
//! internally consistent, not that they are true.
//!
//! HOW TO GET THE OTHER SIDE, corrected 2026-08-22. This comment used to say
//! "run codexbar's `/usage`". That is not runnable here: CodexBar was running
//! (pid checked, `lsof -nP -iTCP -sTCP:LISTEN -a -p <pid>`) and exposed NO TCP
//! listener and no HTTP preference. Whether it once did, or does under some
//! build, I cannot tell from this host -- so the instruction stood as a
//! procedure that fails at the moment someone follows it.
//!
//! What works today is the menu-bar UI: read CodexBar's own per-provider figures
//! and compare by eye. Slower, and a human step rather than a diff, but it is
//! the real remaining path and knowing that beats discovering it mid-check.
//!
//! Run: `cargo run -p quota-core --example accuracy-dump`
//! Filter to one provider: `cargo run -p quota-core --example accuracy-dump -- codex`

#[path = "live_support/mod.rs"]
mod live_support;

#[tokio::main]
async fn main() {
    let filter = std::env::args().nth(1);
    // Reads are cache-only, so the refresher has to publish before there is
    // anything to dump. Without this the example prints an empty array and
    // looks like a provider outage rather than a cold cache.
    let (usage, warm_up) = live_support::collect_live_usage(filter.as_deref()).await;
    if usage.is_empty() {
        eprintln!(
            "no entries after {:.0}s of warm-up: nothing was dumped",
            warm_up.as_secs_f64()
        );
        std::process::exit(2);
    }
    // Pretty JSON so a human can eyeball it and `jq` can slice it for the diff.
    println!(
        "{}",
        serde_json::to_string_pretty(&usage).expect("serialize usage array")
    );
}
