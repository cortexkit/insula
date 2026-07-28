//! Dump the live `usage.get` array as JSON, exactly as the subc module serves it.
//!
//! This is the module side of the codexbar-vs-module accuracy diff: run codexbar's
//! `/usage` and this dump against the same machine credentials and compare the
//! per-provider windows (usedPercent, resetsAt, windowMinutes). It exercises the
//! real provider fetchers (no mocks), so it is the live-verification step before
//! relying on the module as the daily quota source.
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
