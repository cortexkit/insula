//! The per-provider slot store: the single background refresher writes resolved
//! [`ProviderSlot`]s here, and the serving read path only ever clones out of it.
//!
//! This replaces the old single-slot TTL cache. Freshness is no longer a TTL on
//! the read; the refresher owns it (see `refresh.rs`), and the read path serves
//! whatever the last sweep produced. The store also holds the refresher's
//! heartbeat (`last_tick_at`) and its birth time, which `health()` reads to tell
//! a wedged refresher from a merely idle one.

use std::collections::HashMap;
use std::time::Instant;

use crate::refresh::ProviderSlot;

/// The refresher-owned state behind the registry's mutex. Every op here is a
/// cheap in-memory map/timestamp touch, so the lock is never held long and never
/// across a `.await`.
pub struct SlotStore {
    slots: HashMap<String, ProviderSlot>,
    /// When the store was created — lets `health()` detect a refresher that
    /// never started (vs one that started and then stalled).
    created_at: Instant,
    /// Heartbeat: stamped at the top of every refresher tick, unconditionally.
    last_tick_at: Option<Instant>,
}

impl SlotStore {
    /// Seed one due-now slot per provider, so the first refresher tick fetches
    /// the whole set (cold start).
    pub fn new<I, S>(provider_names: I, now: Instant) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let slots = provider_names
            .into_iter()
            .map(|n| (n.into(), ProviderSlot::due_now(now)))
            .collect();
        Self {
            slots,
            created_at: now,
            last_tick_at: None,
        }
    }

    pub fn get(&self, name: &str) -> Option<&ProviderSlot> {
        self.slots.get(name)
    }

    /// Whole-slot atomic replacement (the refresher computes the next slot
    /// entirely outside the lock, then inserts it here).
    pub fn insert(&mut self, name: String, slot: ProviderSlot) {
        self.slots.insert(name, slot);
    }

    /// Stamp the refresher heartbeat.
    pub fn mark_tick(&mut self, now: Instant) {
        self.last_tick_at = Some(now);
    }

    pub fn last_tick_at(&self) -> Option<Instant> {
        self.last_tick_at
    }

    pub fn created_at(&self) -> Instant {
        self.created_at
    }
}
