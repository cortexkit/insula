//! Refresher-owned active fetch units behind the registry mutex.
//!
//! The store only performs in-memory reconciliation, snapshots, heartbeat
//! updates, and incarnation-fenced whole-slot publication. Enumeration, sorting,
//! transition computation, and all asynchronous work happen outside its lock.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::provider::CredentialHandle;
use crate::refresh::{AttemptSequence, Incarnation, ProviderSlot};

/// Active fetch-unit identity. The account label is not part of the key because
/// a credential can change accounts without changing its handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlotKey {
    pub provider: String,
    pub handle: CredentialHandle,
}

impl SlotKey {
    pub fn new(provider: impl Into<String>, handle: CredentialHandle) -> Self {
        Self {
            provider: provider.into(),
            handle,
        }
    }
}

/// Refresher state protected by [`Registry`](crate::Registry)'s mutex.
pub struct SlotStore {
    slots: HashMap<SlotKey, ProviderSlot>,
    created_at: Instant,
    last_tick_at: Option<Instant>,
    next_incarnation: u128,
    next_attempt_sequence: u128,
}

impl SlotStore {
    pub fn new(now: Instant) -> Self {
        Self {
            slots: HashMap::new(),
            created_at: now,
            last_tick_at: None,
            next_incarnation: 1,
            next_attempt_sequence: 1,
        }
    }

    /// Apply one authoritative, canonical handle snapshot for a provider.
    /// Missing handles are reaped and new handles become immediately due.
    pub fn reconcile(&mut self, provider: &str, handles: &[CredentialHandle], now: Instant) {
        let active: HashSet<&CredentialHandle> = handles.iter().collect();
        self.slots
            .retain(|key, _| key.provider != provider || active.contains(&key.handle));

        for handle in handles {
            let key = SlotKey::new(provider, handle.clone());
            if self.slots.contains_key(&key) {
                continue;
            }
            let incarnation = Incarnation::from_counter(self.next_incarnation);
            // Exhaustion is practically unreachable, but wrapping is safer than
            // panicking while the scheduler owns the cache mutex.
            self.next_incarnation = self.next_incarnation.wrapping_add(1);
            self.slots
                .insert(key, ProviderSlot::due_now(now, incarnation));
        }
    }

    pub fn get(&self, key: &SlotKey) -> Option<&ProviderSlot> {
        self.slots.get(key)
    }

    /// Clone active slots for computation after releasing the mutex.
    pub fn snapshot(&self) -> Vec<(SlotKey, ProviderSlot)> {
        self.slots
            .iter()
            .map(|(key, slot)| (key.clone(), slot.clone()))
            .collect()
    }

    /// Reserve an ordered attempt for an active incarnation. Concurrent attempts
    /// may overlap, but only the newest reservation is allowed to publish.
    pub fn admit(
        &mut self,
        key: &SlotKey,
        incarnation: Incarnation,
    ) -> Option<(ProviderSlot, AttemptSequence)> {
        if self.slots.get(key)?.incarnation != incarnation {
            return None;
        }
        let sequence = AttemptSequence::from_counter(self.next_attempt_sequence);
        self.next_attempt_sequence = self.next_attempt_sequence.wrapping_add(1);
        let current = self.slots.get_mut(key)?;
        current.attempt_sequence = sequence;
        Some((current.clone(), sequence))
    }

    /// Publish only into the exact active lifetime and latest admitted attempt.
    pub fn publish_if_current(
        &mut self,
        key: &SlotKey,
        incarnation: Incarnation,
        attempt_sequence: AttemptSequence,
        next: ProviderSlot,
    ) -> bool {
        let Some(current) = self.slots.get(key) else {
            return false;
        };
        if current.incarnation != incarnation || current.attempt_sequence != attempt_sequence {
            return false;
        }
        self.slots.insert(key.clone(), next);
        true
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_and_readd_assigns_a_new_incarnation() {
        let now = Instant::now();
        let handle = CredentialHandle::new("H");
        let key = SlotKey::new("mock", handle.clone());
        let mut store = SlotStore::new(now);

        store.reconcile("mock", std::slice::from_ref(&handle), now);
        let first = store.get(&key).unwrap().incarnation;
        store.reconcile("mock", &[], now);
        store.reconcile("mock", std::slice::from_ref(&handle), now);
        let second = store.get(&key).unwrap().incarnation;

        assert_ne!(first, second);
    }

    #[test]
    fn stale_publication_cannot_resurrect_or_overwrite_a_readded_key() {
        let now = Instant::now();
        let handle = CredentialHandle::new("H");
        let key = SlotKey::new("mock", handle.clone());
        let mut store = SlotStore::new(now);

        store.reconcile("mock", std::slice::from_ref(&handle), now);
        let old_incarnation = store.get(&key).unwrap().incarnation;
        let (old, old_sequence) = store.admit(&key, old_incarnation).unwrap();
        store.reconcile("mock", &[], now);
        assert!(!store.publish_if_current(&key, old_incarnation, old_sequence, old.clone()));
        assert!(store.get(&key).is_none());

        store.reconcile("mock", std::slice::from_ref(&handle), now);
        assert!(!store.publish_if_current(&key, old_incarnation, old_sequence, old));
        assert_ne!(store.get(&key).unwrap().incarnation, old_incarnation);
    }

    #[test]
    fn older_attempt_cannot_publish_after_a_newer_admission() {
        let now = Instant::now();
        let handle = CredentialHandle::new("H");
        let key = SlotKey::new("mock", handle.clone());
        let mut store = SlotStore::new(now);
        store.reconcile("mock", std::slice::from_ref(&handle), now);
        let incarnation = store.get(&key).unwrap().incarnation;

        let (old, old_sequence) = store.admit(&key, incarnation).unwrap();
        let (new, new_sequence) = store.admit(&key, incarnation).unwrap();
        assert!(!store.publish_if_current(&key, incarnation, old_sequence, old));
        assert!(store.publish_if_current(&key, incarnation, new_sequence, new));
    }

    #[test]
    fn incarnation_counter_wraps_without_panicking() {
        let now = Instant::now();
        let mut store = SlotStore::new(now);
        store.next_incarnation = u128::MAX;

        store.reconcile("mock", &[CredentialHandle::new("H1")], now);
        let first = store
            .get(&SlotKey::new("mock", CredentialHandle::new("H1")))
            .unwrap()
            .incarnation;
        store.reconcile("mock", &[], now);
        store.reconcile("mock", &[CredentialHandle::new("H2")], now);
        let second = store
            .get(&SlotKey::new("mock", CredentialHandle::new("H2")))
            .unwrap()
            .incarnation;

        assert_ne!(first, second);
    }
}
