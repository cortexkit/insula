//! A single-slot TTL cache for the assembled usage array.
//!
//! CodexBar caches its `/usage` response for 60s; we match that. The cache is
//! keyed only by the optional provider filter, since the full-array fetch is the
//! common path and providers read independent machine-global sessions.

use std::time::{Duration, Instant};

use crate::model::ProviderUsage;

/// 60s TTL — CodexBar parity.
pub const DEFAULT_TTL: Duration = Duration::from_secs(60);

struct Entry {
    stored_at: Instant,
    value: Vec<ProviderUsage>,
}

/// A TTL cache over `(provider filter) -> ProviderUsage[]`.
pub struct UsageCache {
    ttl: Duration,
    entries: std::collections::HashMap<String, Entry>,
}

impl UsageCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: std::collections::HashMap::new(),
        }
    }

    fn key(provider_filter: Option<&str>) -> String {
        provider_filter.unwrap_or("*").to_string()
    }

    /// Return the cached array if it is still within TTL.
    pub fn get(&self, provider_filter: Option<&str>, now: Instant) -> Option<Vec<ProviderUsage>> {
        let entry = self.entries.get(&Self::key(provider_filter))?;
        if now.duration_since(entry.stored_at) < self.ttl {
            Some(entry.value.clone())
        } else {
            None
        }
    }

    /// Return the most recent FULL sweep (unfiltered) and its age, ignoring TTL.
    ///
    /// Health summarization uses this: it wants the last serving pass regardless
    /// of freshness, because staleness is itself a reported signal (the age is
    /// returned) rather than a reason to hide the sweep. `None` when no full
    /// sweep has been cached yet.
    pub fn latest_full_sweep(&self, now: Instant) -> Option<(Vec<ProviderUsage>, Duration)> {
        let entry = self.entries.get(&Self::key(None))?;
        Some((entry.value.clone(), now.duration_since(entry.stored_at)))
    }

    /// Store an array under the given filter.
    pub fn put(&mut self, provider_filter: Option<&str>, value: Vec<ProviderUsage>, now: Instant) {
        self.entries.insert(
            Self::key(provider_filter),
            Entry {
                stored_at: now,
                value,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<ProviderUsage> {
        vec![ProviderUsage::degraded("codex", "no session")]
    }

    #[test]
    fn returns_within_ttl_and_expires_after() {
        let mut cache = UsageCache::new(Duration::from_secs(60));
        let t0 = Instant::now();
        cache.put(None, sample(), t0);
        assert!(cache.get(None, t0 + Duration::from_secs(59)).is_some());
        assert!(cache.get(None, t0 + Duration::from_secs(61)).is_none());
    }

    #[test]
    fn keys_by_provider_filter() {
        let mut cache = UsageCache::new(Duration::from_secs(60));
        let t0 = Instant::now();
        cache.put(Some("codex"), sample(), t0);
        assert!(cache.get(Some("codex"), t0).is_some());
        assert!(cache.get(None, t0).is_none());
    }
}
