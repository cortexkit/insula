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

use quota_core::{config::QuotaConfig, Registry};

#[tokio::main]
async fn main() {
    let filter = std::env::args().nth(1);
    let registry = Registry::with_defaults(QuotaConfig::default(), None);
    let usage = registry.get_usage(filter.as_deref()).await;
    // Pretty JSON so a human can eyeball it and `jq` can slice it for the diff.
    println!(
        "{}",
        serde_json::to_string_pretty(&usage).expect("serialize usage array")
    );
}
