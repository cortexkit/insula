//! Emit reference `usage.get` envelopes for the completeness cases, so a
//! consumer can pin its reconciliation against real output.
//!
//! A consumer deciding whether it may delete a stored account reads
//! `completeProviders` beside `result`. Its own fixtures have to come from this
//! module rather than from a hand-written approximation: a fixture assembled
//! beside the producer is free to drift from it, and the drift is invisible
//! because both sides look correct read alone. One has already been shipped in
//! this fleet asserting a pairing the producer could not emit, which passed
//! forever while teaching a shape that never existed.
//!
//! The envelopes here are produced by driving a real [`Registry`] through the
//! same scheduler and read path the module serves, then rendering with
//! [`UsageSnapshot::to_envelope`] -- the function the wire itself calls.
//!
//! The providers are stubs, because the four cases turn on CREDENTIAL states
//! that a real upstream cannot be asked to produce on demand: an account whose
//! identity is unconfirmed, a credential withdrawn between two turns, a
//! registry that has not yet run. Only the states are synthetic; the assembly,
//! the emission gate and the envelope shape are the shipping ones.
//!
//! Run: `cargo run -p quota-core --example completeness-envelopes`

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use quota_core::model::Usage;
use quota_core::provider::{
    AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
};
use quota_core::store::SlotKey;
use quota_core::Registry;
use tokio_util::sync::CancellationToken;

/// A provider whose handle set, per-handle account, and per-handle health are
/// all steerable, so each completeness case can be reached deliberately.
struct Stub {
    handles: Arc<Mutex<Vec<&'static str>>>,
    accounts: Arc<Mutex<HashMap<&'static str, Option<&'static str>>>>,
    failing: Arc<Mutex<Vec<&'static str>>>,
}

impl Stub {
    fn new(handles: &[&'static str], accounts: &[(&'static str, Option<&'static str>)]) -> Self {
        Self {
            handles: Arc::new(Mutex::new(handles.to_vec())),
            accounts: Arc::new(Mutex::new(accounts.iter().copied().collect())),
            failing: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl UsageProvider for Stub {
    fn name(&self) -> &str {
        "codex"
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(self
            .handles
            .lock()
            .unwrap()
            .iter()
            .map(|id| CredentialHandle::new(*id))
            .collect())
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let id = handle.stable_id().to_string();
        let account = self
            .accounts
            .lock()
            .unwrap()
            .get(id.as_str())
            .copied()
            .flatten();
        let observed = Some(AccountObservation::new(
            account.map(ToString::to_string),
            None,
        ));
        if self.failing.lock().unwrap().iter().any(|f| *f == id) {
            return FetchAttempt::failure(
                observed,
                Some("vault".to_string()),
                FetchError::Unauthorized("HTTP 401".to_string()),
            );
        }
        FetchAttempt::success(observed, "vault", Usage::default())
    }
}

async fn tick(registry: &Registry) {
    registry.refresh_tick(&CancellationToken::new()).await;
}

async fn emit(label: &str, note: &str, registry: &Registry) -> serde_json::Value {
    let snapshot = registry.usage_snapshot(None).await;
    let envelope = snapshot.to_envelope();
    println!("\n=== {label} ===");
    println!("{note}");
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    envelope
}

#[tokio::main]
async fn main() {
    // COLD: nothing has enumerated, so no provider can claim completeness. A
    // consumer must not read the empty array as "every account is gone".
    let cold = Registry::new(vec![Box::new(Stub::new(
        &["h1"],
        &[("h1", Some("acct-a"))],
    ))]);
    emit(
        "cold-boot",
        "No turn has run. Entries empty AND completeProviders empty: unknown, not empty.",
        &cold,
    )
    .await;

    // THE PAIR, produced from ONE registry so that the surviving account's
    // entry is literally the same bytes in both.
    //
    // Two registries would each carry their own wall-clock anchor, so `fetchedAt`
    // would differ by microseconds and the arrays would be merely equivalent.
    // That is a weaker fact and the wrong one to hand a consumer: it leaves open
    // that something OTHER than the claim also differs. Here acct-a is fetched
    // once and never becomes due again during the run, so its entry is fixed
    // while the account set changes around it.
    let pair_stub = Stub::new(
        &["h1", "h2"],
        &[("h1", Some("acct-a")), ("h2", Some("acct-b"))],
    );
    let pair_handles = Arc::clone(&pair_stub.handles);
    let pair = Registry::new(vec![Box::new(pair_stub)]);
    tick(&pair).await;
    emit(
        "before-removal",
        "Both accounts published and the provider is complete.",
        &pair,
    )
    .await;

    // WITHHELD first: acct-b's identity stops being confirmable, so its entry is
    // suppressed while its identity is kept -- which is what holds the provider
    // fully resolved and leaves acct-a labelled.
    {
        let mut store = pair.slot_store().lock().unwrap();
        let key = SlotKey::new("codex", CredentialHandle::new("h2"));
        let mut slot = store.get(&key).expect("h2 has a slot").clone();
        slot.label_in_flux = true;
        slot.entry = None;
        let incarnation = slot.incarnation;
        let attempt_sequence = slot.attempt_sequence;
        assert!(store.publish_if_current(&key, incarnation, attempt_sequence, slot));
    }
    let withheld_envelope = emit(
        "withheld-account",
        "acct-b is withheld. Entries look exactly like the removal case; the claim is what differs.",
        &pair,
    )
    .await;

    // Then the removal, on the same registry: acct-b's credential is withdrawn
    // and the enumeration SUCCEEDS without it, so its slot is reaped. acct-a is
    // not due again, so it is not refetched and its entry is untouched.
    *pair_handles.lock().unwrap() = vec!["h1"];
    tick(&pair).await;
    let removal_envelope = emit(
        "genuine-removal",
        "acct-b's credential is withdrawn. Complete AND absent: delete acct-b.",
        &pair,
    )
    .await;

    // The claim about this pair, enforced rather than asserted in prose. A
    // consumer pins these two against each other to prove its reconciliation
    // reads the claim; if the arrays ever differ for another reason, the pair
    // stops isolating the variable under test and the consumer's fixture starts
    // passing for a reason nobody chose.
    let removal_result = serde_json::to_vec(&removal_envelope["result"]).unwrap();
    let withheld_result = serde_json::to_vec(&withheld_envelope["result"]).unwrap();
    assert_eq!(
        removal_result,
        withheld_result,
        "\nthe paired envelopes must differ ONLY in completeProviders, but their \
         result arrays differ:\n  removal:  {}\n  withheld: {}",
        String::from_utf8_lossy(&removal_result),
        String::from_utf8_lossy(&withheld_result),
    );
    assert_ne!(
        removal_envelope["completeProviders"], withheld_envelope["completeProviders"],
        "the pair is only interesting because the claim differs"
    );
    println!(
        "\n=== pair check ===\nresult arrays are byte-identical ({} bytes); \
         completeProviders differs: {} vs {}",
        removal_result.len(),
        removal_envelope["completeProviders"],
        withheld_envelope["completeProviders"],
    );

    // HEALTHY + DEGRADED: the account set is complete and one member is
    // unusable. The degraded account is still NAMED, so a survivor set built
    // from usable entries alone would delete a live account.
    let degraded_stub = Stub::new(
        &["h1", "h2"],
        &[("h1", Some("acct-a")), ("h2", Some("acct-b"))],
    );
    degraded_stub.failing.lock().unwrap().push("h2");
    let degraded = Registry::new(vec![Box::new(degraded_stub)]);
    tick(&degraded).await;
    emit(
        "healthy-and-degraded",
        "Complete, and acct-b carries an error. It exists: keep the row, drop only its windows.",
        &degraded,
    )
    .await;
}
