//! Measure how long a full refresh sweep takes, and how much headroom the
//! per-turn admission cap leaves.
//!
//! A scheduler turn drains every unit it admitted before the next one starts, so
//! a turn costs its slowest unit and a full sweep of `n` units takes
//! `ceil(n / CONCURRENCY_CAP)` turns. The nominal refresh interval is therefore a
//! floor: once a sweep takes longer than the interval, the achieved period is
//! the sweep time, and beyond the freshness horizon healthy providers begin
//! reporting as stale.
//!
//! Whether that is close depends on fetch latency, which is not a constant, so
//! it cannot be asserted in a test — the documentation on `CONCURRENCY_CAP`
//! says to re-measure, and this is the measurement. Run it when the provider
//! set grows or when the cap is being reconsidered.
//!
//! It runs the real fetchers against this machine's credentials, so the timing
//! reflects this host: providers with no credentials here fail fast and make the
//! sweep look quicker than a fleet where every unit reaches the network. The
//! projections below are the useful output, not the raw sweep time.
//!
//! Exits non-zero if the sweep did not finish inside the probe window, so a
//! wedged run cannot be mistaken for a fast one.

use std::time::{Duration, Instant};

use quota_core::{config::QuotaConfig, refresh, Registry};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let registry = std::sync::Arc::new(Registry::with_defaults(QuotaConfig::default(), None));
    let providers = registry.health().providers_total;
    let cancel = CancellationToken::new();

    let handle = {
        let registry = std::sync::Arc::clone(&registry);
        let cancel = cancel.clone();
        tokio::spawn(async move { registry.refresh_loop(cancel).await })
    };

    // A cold start is the widest realistic sweep: every unit is due at once, so
    // the loop runs back-to-back turns until all of them have been attempted.
    // `pending` reaching zero is exactly the point where every unit has had its
    // first attempt.
    let started = Instant::now();
    let mut swept_at = None;
    while started.elapsed() < Duration::from_secs(90) {
        let health = registry.health();
        if health.pending == 0 && health.last_tick_age.is_some() {
            swept_at = Some(started.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cancel.cancel();
    let _ = handle.await;

    let Some(sweep) = swept_at else {
        println!("the sweep did not complete within the probe window");
        std::process::exit(2);
    };

    let cap = refresh::CONCURRENCY_CAP;
    let turns = providers.div_ceil(cap);
    let per_turn = sweep.as_secs_f64() / turns as f64;
    let interval = refresh::BASE_INTERVAL.as_secs_f64();
    let horizon = refresh::FRESH_HORIZON.as_secs_f64();

    println!("  providers: {providers}   cap: {cap}/turn   turns per sweep: ~{turns}");
    println!(
        "  cold sweep: {:.2}s   => ~{:.2}s per turn",
        sweep.as_secs_f64(),
        per_turn
    );
    println!(
        "  at this latency, units before a sweep exceeds the {interval:.0}s interval:  ~{:.0}",
        (interval / per_turn) * cap as f64
    );
    println!(
        "  at this latency, units before it exceeds the {horizon:.0}s freshness horizon: ~{:.0}",
        (horizon / per_turn) * cap as f64
    );
    println!(
        "  at this fleet size, seconds per turn before a sweep exceeds the horizon:  ~{:.0}s",
        horizon / turns as f64
    );
}
