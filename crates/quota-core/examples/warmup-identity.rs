//! Check the health conservation identity across a real warm-up.
//!
//! The identity is `fresh + stale + pending + degraded + unconfigured + withoutHandles ==
//! providersTotal`, and consumers are told to assert it and alert on an
//! imbalance. Its most stressed moment is the first seconds after a start: the
//! refresher admits a bounded number of fetch units per turn, so with the full
//! provider set most of them sit in `pending` while the first few complete, and
//! every provider is changing bucket at once.
//!
//! Unit tests cover the classification with a handful of mock providers. This
//! runs the registry that actually ships, because the interesting condition is
//! having far more providers than the concurrency cap admits — a bucket chain
//! can be correct at nine providers and wrong at thirty-five.
//!
//! Sampling is faster than a fetch can complete, so a bucket state cannot pass
//! between two reads unobserved.
//!
//! Run it against this machine's real credentials:
//!
//! ```text
//! cargo run -p quota-core --example warmup-identity
//! ```
//!
//! Exits non-zero on an imbalance, or if the warm-up never produced a `pending`
//! provider — which would mean the run proved nothing, rather than proving the
//! identity holds.
use std::time::{Duration, Instant};

use quota_core::{config::QuotaConfig, Registry};
use tokio_util::sync::CancellationToken;

/// How long to watch. Warm-up clears in well under this on a healthy host; the
/// margin is for a machine where every provider is timing out.
const WATCH: Duration = Duration::from_secs(90);

#[tokio::main]
async fn main() {
    let registry = std::sync::Arc::new(Registry::with_defaults(QuotaConfig::default(), None));
    let cancel = CancellationToken::new();

    let refresher = {
        let registry = std::sync::Arc::clone(&registry);
        let cancel = cancel.clone();
        tokio::spawn(async move { registry.refresh_loop(cancel).await })
    };

    let started = Instant::now();
    let mut samples = 0u32;
    let mut peak_pending = 0usize;
    let mut imbalances = Vec::new();

    loop {
        let health = registry.health();
        samples += 1;
        peak_pending = peak_pending.max(health.pending);

        let sum = health.fresh
            + health.stale
            + health.pending
            + health.degraded.len()
            + health.unconfigured.len()
            + health.without_handles.len();

        // Gated on the refresher having ticked, exactly as consumers are told to
        // gate it: before the first tick no provider is in any bucket, so the
        // identity is legitimately false and asserting it would fire at every
        // start.
        if health.last_tick_age.is_some() && sum != health.providers_total {
            imbalances.push(format!(
                "{:.2}s: fresh {} + stale {} + pending {} + degraded {} + unconfigured {} \
                 + withoutHandles {} = {sum}, but providersTotal is {}",
                started.elapsed().as_secs_f64(),
                health.fresh,
                health.stale,
                health.pending,
                health.degraded.len(),
                health.unconfigured.len(),
                health.without_handles.len(),
                health.providers_total,
            ));
        }

        // Done once the warm-up is over: every provider has been tried at least
        // once, which is the transition this is here to watch.
        if peak_pending > 0 && health.pending == 0 {
            break;
        }
        if started.elapsed() >= WATCH {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let elapsed = started.elapsed().as_secs_f64();
    let health = registry.health();
    println!(
        "samples: {samples} over {elapsed:.1}s   peak pending: {peak_pending}/{}   \
         settled: fresh {} stale {} degraded {} withoutHandles {}",
        health.providers_total,
        health.fresh,
        health.stale,
        health.degraded.len(),
        health.without_handles.len(),
    );

    cancel.cancel();
    let _ = refresher.await;

    if !imbalances.is_empty() {
        println!("imbalances: {}", imbalances.len());
        for line in imbalances.iter().take(10) {
            println!("  {line}");
        }
        std::process::exit(1);
    }

    // A run where nothing was ever pending never reached the state under test.
    // Reporting that as success would be the same failure as a checker that
    // examined nothing: no findings, because nothing was looked at.
    if peak_pending == 0 {
        println!("no provider was ever pending: the warm-up state was not observed");
        std::process::exit(2);
    }

    println!("identity held at every sample");
}
