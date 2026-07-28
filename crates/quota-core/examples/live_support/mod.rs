//! Drive the background refresher far enough that a live example has data.
//!
//! `Registry::get_usage` is cache-only by design: it never fetches, so that a
//! read can never block on the network. A freshly constructed registry
//! therefore serves an empty array until the background refresher has published
//! something, and an example that calls `get_usage` directly reports nothing at
//! all while looking like it ran.
//!
//! Both examples in this directory need the same warm-up, so it lives here
//! rather than being written twice and drifting.

use std::time::{Duration, Instant};

use cortexkit_provider_usage::ProviderUsage;
use quota_core::{config::QuotaConfig, Registry};
use tokio_util::sync::CancellationToken;

/// Give the fetchers long enough to finish. Handles are admitted round-robin up
/// to a concurrency cap, so several ticks may be needed before every provider
/// has been tried once.
const WARM_UP_LIMIT: Duration = Duration::from_secs(150);

/// Wait this long after the entry count stops growing before deciding the sweep
/// has settled, so a slow provider is not mistaken for the end of the list.
const SETTLE_QUIET: Duration = Duration::from_secs(12);

/// Fetch live usage, warming the cache first.
///
/// Returns the entries and how long the warm-up took. An empty result here is a
/// real answer (no provider on this machine resolved a credential), not the
/// artefact of reading a cold cache.
pub async fn collect_live_usage(filter: Option<&str>) -> (Vec<ProviderUsage>, Duration) {
    let registry = std::sync::Arc::new(Registry::with_defaults(QuotaConfig::default(), None));
    let cancel = CancellationToken::new();

    let refresher = {
        let registry = std::sync::Arc::clone(&registry);
        let cancel = cancel.clone();
        tokio::spawn(async move { registry.refresh_loop(cancel).await })
    };

    let started = Instant::now();
    let mut best = 0usize;
    let mut last_growth = Instant::now();

    while started.elapsed() < WARM_UP_LIMIT {
        tokio::time::sleep(Duration::from_millis(500)).await;

        // A tick must have completed before the counts mean anything: until
        // then every provider legitimately has no slot yet.
        if registry.health().last_tick_age.is_none() {
            continue;
        }

        let count = registry.get_usage(filter).await.len();
        if count > best {
            best = count;
            last_growth = Instant::now();
        } else if best > 0 && last_growth.elapsed() >= SETTLE_QUIET {
            break;
        }
    }

    let usage = registry.get_usage(filter).await;
    cancel.cancel();
    let _ = refresher.await;
    (usage, started.elapsed())
}
