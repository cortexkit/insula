//! Refresher-owned active fetch units behind the registry mutex.
//!
//! The store only performs in-memory reconciliation, snapshots, heartbeat
//! updates, and incarnation-fenced whole-slot publication. Enumeration, sorting,
//! transition computation, and all asynchronous work happen outside its lock.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, Utc};

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

/// Successful handle enumerations indexed before acquiring the store mutex.
pub(crate) struct AuthoritativeHandles<'a> {
    ordered: Vec<(&'a str, &'a [CredentialHandle])>,
    by_provider: HashMap<&'a str, HashSet<&'a CredentialHandle>>,
}

impl<'a> AuthoritativeHandles<'a> {
    pub(crate) fn new(
        providers: impl IntoIterator<Item = (&'a str, &'a [CredentialHandle])>,
    ) -> Self {
        let ordered: Vec<_> = providers.into_iter().collect();
        let by_provider = ordered
            .iter()
            .map(|(provider, handles)| (*provider, handles.iter().collect()))
            .collect();
        Self {
            ordered,
            by_provider,
        }
    }
}

/// Refresher state protected by [`Registry`](crate::Registry)'s mutex.
pub struct SlotStore {
    slots: HashMap<SlotKey, ProviderSlot>,
    /// Providers whose most recent handle enumeration succeeded.
    ///
    /// Replaced wholesale each turn, so a provider that succeeded once and then
    /// failed is removed rather than keeping a stale claim. Empty until the
    /// first turn, which is what keeps a completeness claim unrepresentable
    /// before the refresher has ever run.
    ///
    /// A failed enumeration is not the same as one returning nothing: the first
    /// retains the provider's existing slots and can say nothing about which
    /// accounts it has, while the second is an authoritative statement that it
    /// has none. Both leave the provider with no fresh handle list, so without
    /// recording the outcome the two are indistinguishable afterwards.
    enumerated_ok: HashSet<String>,
    created_at: Instant,
    created_at_wall: DateTime<Utc>,
    last_tick_at: Option<Instant>,
    next_incarnation: u128,
    next_attempt_sequence: u128,
}

impl SlotStore {
    pub fn new(now: Instant) -> Self {
        Self {
            slots: HashMap::new(),
            enumerated_ok: HashSet::new(),
            created_at: now,
            created_at_wall: Utc::now(),
            last_tick_at: None,
            next_incarnation: 1,
            next_attempt_sequence: 1,
        }
    }

    /// Whether this provider's most recent handle enumeration succeeded.
    pub fn enumeration_succeeded(&self, provider: &str) -> bool {
        self.enumerated_ok.contains(provider)
    }

    /// Apply one authoritative, canonical handle snapshot for a provider.
    /// Missing handles are reaped and new handles become immediately due.
    pub fn reconcile(&mut self, provider: &str, handles: &[CredentialHandle], now: Instant) {
        let active: HashSet<&CredentialHandle> = handles.iter().collect();
        self.slots
            .retain(|key, _| key.provider != provider || active.contains(&key.handle));

        self.insert_missing(provider, handles, now);
    }

    /// Apply every successful provider enumeration with one store-wide retain pass.
    pub(crate) fn reconcile_batch(
        &mut self,
        authoritative: &AuthoritativeHandles<'_>,
        now: Instant,
    ) {
        // Every provider is enumerated each turn and only the successes reach
        // here, so the set of providers present IS the set that succeeded.
        // Replaced rather than extended: a provider that succeeded last turn and
        // failed this one must lose the claim, not keep it.
        self.enumerated_ok = authoritative
            .ordered
            .iter()
            .map(|(provider, _)| (*provider).to_string())
            .collect();

        self.slots.retain(|key, _| {
            let Some(active) = authoritative.by_provider.get(key.provider.as_str()) else {
                // An absent provider failed enumeration, so its last-known slots remain active.
                return true;
            };
            active.contains(&key.handle)
        });

        for (provider, handles) in &authoritative.ordered {
            self.insert_missing(provider, handles, now);
        }
    }

    fn insert_missing(&mut self, provider: &str, handles: &[CredentialHandle], now: Instant) {
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
        // Admission means the previous relaxation proof is no longer current:
        // serve its raw usage while the next upstream observation is in flight.
        current.relax_eligible = false;
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

    /// Copy the clock anchors needed for timestamp conversion after unlocking.
    pub(crate) fn wall_time_anchor(&self) -> (Instant, DateTime<Utc>) {
        (self.created_at, self.created_at_wall)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_reconciliation_preserves_slots_for_failed_providers() {
        let now = Instant::now();
        let a_old = CredentialHandle::new("A-old");
        let a_new = CredentialHandle::new("A-new");
        let b_handles = [CredentialHandle::new("B1"), CredentialHandle::new("B2")];
        let mut store = SlotStore::new(now);
        store.reconcile("a", std::slice::from_ref(&a_old), now);
        store.reconcile("b", &b_handles, now);

        for handle in &b_handles {
            let key = SlotKey::new("b", handle.clone());
            // Built through the constructor rather than as a struct literal:
            // a literal here would silently keep its own defaults for any field
            // added later, so this fixture could drift from a real entry
            // without anything failing.
            store.slots.get_mut(&key).unwrap().entry = Some(crate::model::ProviderUsage::healthy(
                "b",
                Some(handle.stable_id().to_string()),
                "cached",
                crate::model::Usage::default(),
            ));
        }
        let b_before: Vec<_> = b_handles
            .iter()
            .map(|handle| {
                let slot = store.get(&SlotKey::new("b", handle.clone())).unwrap();
                (slot.incarnation, slot.entry.clone())
            })
            .collect();

        let a_handles = [a_new.clone()];
        let authoritative = AuthoritativeHandles::new([("a", a_handles.as_slice())]);
        store.reconcile_batch(&authoritative, now);

        assert!(store.get(&SlotKey::new("a", a_old)).is_none());
        assert!(store.get(&SlotKey::new("a", a_new)).is_some());
        for (handle, (incarnation, entry)) in b_handles.iter().zip(b_before) {
            let slot = store.get(&SlotKey::new("b", handle.clone())).unwrap();
            assert_eq!(slot.incarnation, incarnation);
            assert_eq!(slot.entry, entry);
        }
    }

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
    fn same_id_changed_capability_is_remove_add_with_new_incarnation() {
        use crate::credential_source::VaultCapability;

        let now = Instant::now();
        let first_handle =
            CredentialHandle::vault("chatgpt:openai", VaultCapability::new("ckh_first_secret"));
        let second_handle =
            CredentialHandle::vault("chatgpt:openai", VaultCapability::new("ckh_second_secret"));
        assert_ne!(first_handle, second_handle);
        assert!(!format!("{first_handle:?}").contains("ckh_first_secret"));

        let first_key = SlotKey::new("codex", first_handle.clone());
        let second_key = SlotKey::new("codex", second_handle.clone());
        let mut store = SlotStore::new(now);
        store.reconcile("codex", &[first_handle], now);
        let first_incarnation = store.get(&first_key).unwrap().incarnation;

        store.reconcile("codex", &[second_handle], now);
        assert!(store.get(&first_key).is_none());
        assert_ne!(
            store.get(&second_key).unwrap().incarnation,
            first_incarnation
        );
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

    /// The incarnation half of the publish fence has to be load-bearing on its
    /// own, independently of the attempt sequence beside it.
    ///
    /// A handle removed and re-added gets a fresh incarnation, and an attempt
    /// admitted before the removal may still be in flight. Its sequence can be
    /// the newest one the re-added slot has issued -- nothing resets the
    /// sequence counter across a reconcile -- so the sequence check alone lets
    /// that attempt publish into a slot it never belonged to. The published
    /// usage would then describe whatever credential the handle pointed at
    /// before it was replaced, under the identity of the one that replaced it.
    ///
    /// The neighbouring test covers a re-added key whose in-flight attempt was
    /// admitted under the OLD incarnation and never re-admitted, which the
    /// sequence check happens to reject as well. This one constructs the case
    /// where only the incarnation differs, so it fails if that conjunct is
    /// dropped.
    #[test]
    fn an_attempt_from_a_previous_incarnation_cannot_publish_into_a_readded_slot() {
        let now = Instant::now();
        let handle = CredentialHandle::new("H");
        let key = SlotKey::new("mock", handle.clone());
        let mut store = SlotStore::new(now);

        store.reconcile("mock", std::slice::from_ref(&handle), now);
        let old_incarnation = store.get(&key).unwrap().incarnation;
        let (in_flight, _) = store.admit(&key, old_incarnation).unwrap();

        // The handle goes away and comes back: a new incarnation, and the slot
        // is fresh.
        store.reconcile("mock", &[], now);
        store.reconcile("mock", std::slice::from_ref(&handle), now);
        let new_incarnation = store.get(&key).unwrap().incarnation;
        assert_ne!(new_incarnation, old_incarnation);

        // Publish with the sequence the re-added slot currently holds, so the
        // sequence check passes and the incarnation check is the only thing
        // left to reject it. Without that check, this stale attempt publishes.
        let current_sequence = store.get(&key).unwrap().attempt_sequence;
        assert!(
            !store.publish_if_current(&key, old_incarnation, current_sequence, in_flight),
            "an attempt from a previous incarnation published into the re-added slot"
        );
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
