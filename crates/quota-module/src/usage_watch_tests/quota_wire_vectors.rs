//! Pin producer bytes so a consumer can review wire drift without live accounts.

use super::*;
use async_trait::async_trait;
use quota_core::model::{
    AccountInfo, Amount, CreditExpiry, ExtraWindow, Pool, PoolBasis, PoolFunding, RateWindow,
    SavedResets, Usage,
};
use quota_core::provider::{
    AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
};
use quota_core::store::SlotKey;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;

const FETCHED_AT: &str = "2026-01-15T12:00:00Z";
const FAILED_AT: &str = "2026-01-15T12:05:00Z";
const WATCH_EPOCH: &str = "00000000-0000-4000-8000-000000000001";
const REGENERATE: &str = "INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_vectors_match";

struct VectorProvider {
    name: &'static str,
    accounts: Vec<&'static str>,
    fail_stale: Arc<AtomicBool>,
}

fn window(used_percent: f64, minutes: i64, kind: &str, resets_at: &str) -> RateWindow {
    RateWindow {
        used_percent,
        window_minutes: Some(minutes),
        window_kind: Some(kind.into()),
        resets_at: Some(resets_at.into()),
        raw_used_percent: None,
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    }
}

#[async_trait]
impl UsageProvider for VectorProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(self
            .accounts
            .iter()
            .map(|id| CredentialHandle::new(*id))
            .collect())
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let account = handle.stable_id();
        let observed = Some(AccountObservation::new(Some(account.into()), None));
        if account == "unread@example.com" {
            return FetchAttempt::failure(
                observed,
                Some("api".into()),
                FetchError::Unauthorized("synthetic credential rejected".into()),
            );
        }
        if account == "stale@example.com" && self.fail_stale.load(Ordering::SeqCst) {
            return FetchAttempt::failure(
                observed,
                Some("api".into()),
                FetchError::ProviderStatus(503, "synthetic upstream unavailable".into()),
            );
        }
        let exhausted = account == "exhausted@example.com";
        let mut usage = Usage {
            primary: Some(window(
                if exhausted { 100.0 } else { 37.5 },
                300,
                "rolling",
                "2026-01-15T17:00:00Z",
            )),
            secondary: Some(window(62.5, 10080, "weekly", "2026-01-19T12:00:00Z")),
            ..Default::default()
        };
        if self.name == "antigravity" {
            usage.extra_rate_windows = Some(vec![ExtraWindow {
                id: Some("gemini-pro".into()),
                title: Some("Gemini Pro".into()),
                window: Some(window(12.5, 300, "rolling", "2026-01-15T17:00:00Z")),
            }]);
        }
        let mut attempt = FetchAttempt::success(observed, "api", usage);
        attempt.value_observed_at = Some(FETCHED_AT.parse().unwrap());
        attempt.account_info = Some(AccountInfo {
            email: Some(account.into()),
            ..Default::default()
        });
        if account == "fresh@example.com" {
            attempt.pools = Some(vec![
                pool(
                    "subscription",
                    PoolFunding::Subscription,
                    55598241861500,
                    9,
                    "credits",
                ),
                pool("purchase", PoolFunding::Purchased, 2500, 2, "USD"),
            ]);
            attempt.saved_resets = Some(SavedResets {
                available_count: 1,
                soonest_expires_at: Some("2026-02-01T00:00:00Z".into()),
                credits: vec![CreditExpiry {
                    expires_at: "2026-02-01T00:00:00Z".into(),
                }],
            });
        } else if self.name == "deepseek" {
            attempt.pools = Some(vec![pool(
                "overdrawn",
                PoolFunding::Purchased,
                -2,
                2,
                "USD",
            )]);
        }
        attempt
    }
}

fn pool(id: &str, funding: PoolFunding, minor: i64, exponent: u8, unit: &str) -> Pool {
    Pool {
        id: id.into(),
        label: format!("Synthetic {id} credit"),
        funding,
        remaining: Some(Amount {
            minor,
            exponent,
            unit: unit.into(),
        }),
        total: None,
        basis: PoolBasis::Reported,
        spendable: Some(minor > 0),
        resets_at: None,
        expires_at: Some("2026-02-01T00:00:00Z".into()),
    }
}

async fn vector_registry() -> Arc<Registry> {
    let fail_stale = Arc::new(AtomicBool::new(false));
    let providers = [
        (
            "codex",
            vec![
                "fresh@example.com",
                "exhausted@example.com",
                "unread@example.com",
            ],
        ),
        ("claude", vec!["stale@example.com"]),
        ("deepseek", vec!["overdrawn@example.com"]),
        ("antigravity", vec!["named-windows@example.com"]),
    ]
    .into_iter()
    .map(|(name, accounts)| {
        Box::new(VectorProvider {
            name,
            accounts,
            fail_stale: Arc::clone(&fail_stale),
        }) as Box<dyn UsageProvider>
    })
    .collect();
    let registry = Arc::new(Registry::new(providers));
    registry.refresh_tick(&CancellationToken::new()).await;
    fail_stale.store(true, Ordering::SeqCst);
    update_stale_slot(&registry, |slot| {
        slot.next_due_at = tokio::time::Instant::now().into_std();
    });
    registry.refresh_tick(&CancellationToken::new()).await;
    {
        // The failure transition reads the wall clock. Pin only its timestamp;
        // leave the retained reading and stale/degraded decisions to the registry.
        update_stale_slot(&registry, |slot| {
            slot.failing_since_wall = Some(FAILED_AT.parse().unwrap());
        });
    }
    registry
}

fn update_stale_slot(
    registry: &Registry,
    update: impl FnOnce(&mut quota_core::refresh::ProviderSlot),
) {
    let key = SlotKey::new("claude", CredentialHandle::new("stale@example.com"));
    let mut store = registry.slot_store().lock().unwrap();
    let mut slot = store.get(&key).unwrap().clone();
    update(&mut slot);
    assert!(store.publish_if_current(&key, slot.incarnation, slot.attempt_sequence, slot));
}

async fn generated_files() -> Vec<(&'static str, Vec<u8>)> {
    let registry = vector_registry().await;
    let serve = Arc::new(ServeCounters {
        samples: Arc::new(bus::QuotaSamples::new(WATCH_EPOCH.into())),
        ..Default::default()
    });
    let (tx, mut rx) = mpsc::channel(1);
    handle_usage_request(
        request(5, 7, 91, USAGE_GET_OP, false),
        &tx,
        &registry,
        &serve,
    )
    .await
    .unwrap();
    let response = rx.recv().await.unwrap();
    assert_eq!(response.header.ty, FrameType::Response);

    let mut entries = registry.get_usage(None).await;
    serve.samples.observe(entries.clone()).await;
    handle_usage_request(
        request(5, 7, 92, USAGE_WATCH_OP, true),
        &tx,
        &registry,
        &serve,
    )
    .await
    .unwrap();
    let read_now = serve.samples.watches.try_next().unwrap();
    assert_eq!(read_now.header.ty, FrameType::StreamData);
    let fresh = entries
        .iter_mut()
        .find(|row| row.account.as_deref() == Some("fresh@example.com"))
        .unwrap();
    fresh
        .usage
        .as_mut()
        .unwrap()
        .primary
        .as_mut()
        .unwrap()
        .used_percent = 40.0;
    serve.samples.observe(entries).await;
    let row_change = serve.samples.watches.try_next().unwrap();
    assert_eq!(row_change.header.ty, FrameType::StreamData);
    assert!(serve.samples.watches.try_next().is_none());

    let mut files = vec![
        ("usage-get.json", response.body),
        ("usage-watch-read-now.json", read_now.body),
        ("usage-watch-row-change.json", row_change.body),
        ("README.md", VECTOR_README.as_bytes().to_vec()),
    ];
    let mut sums = String::new();
    for (name, bytes) in &files {
        // Hashed in-process, so the test runs where `shasum` doesn't exist
        // (Windows CI). Hashing the generated bytes rather than the files on
        // disk also catches a stale SHA256SUMS. The output format matches
        // `shasum -a 256`, which is how consumers verify the directory.
        use sha2::Digest as _;
        let digest: String = sha2::Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        sums.push_str(&format!("{digest}  {name}\n"));
    }
    files.push(("SHA256SUMS", sums.into_bytes()));
    files
}

#[tokio::test(start_paused = true)]
async fn committed_quota_wire_vectors_match() {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/quota-wire-v1");
    let files = generated_files().await;
    if std::env::var_os("INSULA_WRITE_VECTORS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::create_dir_all(&directory).unwrap();
        for (name, bytes) in &files {
            std::fs::write(directory.join(name), bytes).unwrap();
        }
    }
    let mut differences = Vec::new();
    for (name, bytes) in &files {
        if std::fs::read(directory.join(name)).ok().as_ref() != Some(bytes) {
            differences.push((*name).to_string());
        }
    }
    // An added file must join the generator and checksum manifest, rather than
    // silently shipping outside the consumer's pinned set.
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if !files.iter().any(|(expected, _)| *expected == name) {
                differences.push(name);
            }
        }
    }
    differences.sort();
    assert!(
        differences.is_empty(),
        "committed quota wire vectors differ: {}. Regenerate with: {REGENERATE}",
        differences.join(", ")
    );
}

const V2_README: &str = "# Quota wire v2: explicit Codex saved reset\n\nSynthetic identities only: account `synthetic-codex-id`, email `phone@example.com`,\nprincipal `reserved:callosum`, harness `fed-phone:synthetic-fp`. No credentials.\nThe fixed clock is 2026-07-14T12:00:00Z; the blocking reservation is 12 minutes older.\n\n`use-saved-reset-request.json` is the routed request envelope; its `params` is the\noperation request body. Success files are Response frame bodies, error files are\nError frame bodies. `recently_reset` includes RFC 3339 `details.last_reset_at`.\n`provider_refused` includes the bounded (200-character) provider excerpt in details.\nThe before/after files are individual rows from `usage.get`, with availableCount\nchanging from 2 to 1 after the due slot is refreshed. The operation reply does not\nwait for that refresh. A repeated request_id replays its durable outcome; a new id\nis not a retry. Only callosum's attested fed-phone: routes may spend a credit.\n\nGenerated in process by the Rust wire tests, including SHA256SUMS. Regenerate with:\n`INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_v2_vectors_match`\n\nv1 remains unchanged. Consumers may verify SHA256SUMS with `shasum -a 256 -c SHA256SUMS`.\n";

async fn generated_v2_files() -> Vec<(String, Vec<u8>)> {
    use super::saved_reset_tests::{self as phone, *};
    use quota_core::codex_resets::{ConsumeOutcome, SavedResetError};
    let (registry, _, result) = fixture().await;
    let mut wire = Wire::with_registry(false, 65536, registry.clone()).await;
    let mut files = vec![
        ("README.md".to_owned(), V2_README.as_bytes().to_vec()),
        (
            "use-saved-reset-request.json".into(),
            serde_json::to_vec(&json!({"method":USE_SAVED_RESET_OP,"params":reset_params()}))
                .unwrap(),
        ),
    ];
    let forbidden = call(&mut wire, 1, reset_params()).await;
    assert_eq!(forbidden.header.ty, FrameType::Error);
    files.push(("error-forbidden.json".into(), forbidden.body));
    bind(&mut wire, 1, callosum(), Some("fed-phone:synthetic-fp")).await;
    wire.send(request(5, 1, 702, USAGE_GET_OP, false)).await;
    let before: Value = serde_json::from_slice(&wire.recv().await.body).unwrap();
    files.push((
        "usage-get-row-before.json".into(),
        serde_json::to_vec(&before["result"][0]).unwrap(),
    ));
    for (field, value, code) in [
        ("request_id", "short", "invalid_request"),
        ("provider", "claude", "unsupported_provider"),
        ("account", "missing", "unknown_account"),
    ] {
        let mut params = reset_params();
        params[field] = json!(value);
        let frame = call(&mut wire, 1, params).await;
        assert_eq!(frame.header.ty, FrameType::Error);
        files.push((format!("error-{code}.json"), frame.body));
    }
    for (error, code) in [
        (
            SavedResetError::RecentlyReset(phone::AT.parse().unwrap()),
            "recently_reset",
        ),
        (
            SavedResetError::ProviderRefused("synthetic provider refusal".into()),
            "provider_refused",
        ),
        (
            SavedResetError::ProviderUnavailable("synthetic timeout".into()),
            "provider_unavailable",
        ),
    ] {
        *result.lock().unwrap() = Err(error);
        let frame = call(&mut wire, 1, reset_params()).await;
        assert_eq!(frame.header.ty, FrameType::Error);
        files.push((format!("error-{code}.json"), frame.body));
    }
    for outcome in [
        ConsumeOutcome::Reset,
        ConsumeOutcome::NothingToReset,
        ConsumeOutcome::NoCredit,
        ConsumeOutcome::AlreadyRedeemed,
    ] {
        *result.lock().unwrap() = Ok(outcome);
        let frame = call(&mut wire, 1, reset_params()).await;
        assert_eq!(frame.header.ty, FrameType::Response);
        files.push((format!("success-{}.json", outcome.as_code()), frame.body));
    }
    registry.refresh_tick(&CancellationToken::new()).await;
    wire.send(request(5, 1, 702, USAGE_GET_OP, false)).await;
    let after: Value = serde_json::from_slice(&wire.recv().await.body).unwrap();
    files.push((
        "usage-get-row-after.json".into(),
        serde_json::to_vec(&after["result"][0]).unwrap(),
    ));
    wire.stop().await;
    wire.finish().await;
    let mut sums = String::new();
    for (name, bytes) in &files {
        use sha2::Digest as _;
        let digest: String = sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        sums.push_str(&format!("{digest}  {name}\n"));
    }
    files.push(("SHA256SUMS".into(), sums.into_bytes()));
    files
}

#[tokio::test(start_paused = true)]
async fn committed_quota_wire_v2_vectors_match() {
    let directory =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/quota-wire-v2");
    let files = generated_v2_files().await;
    if std::env::var_os("INSULA_WRITE_VECTORS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::create_dir_all(&directory).unwrap();
        for (name, bytes) in &files {
            std::fs::write(directory.join(name), bytes).unwrap();
        }
    }
    let mut differences = Vec::new();
    for (name, bytes) in &files {
        if std::fs::read(directory.join(name)).ok().as_ref() != Some(bytes) {
            differences.push(name.clone());
        }
    }
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if !files.iter().any(|(expected, _)| *expected == name) {
                differences.push(name);
            }
        }
    }
    assert!(differences.is_empty(), "committed quota wire v2 vectors differ: {}. Regenerate with INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_v2_vectors_match", differences.join(", "));
}

const VECTOR_README: &str = "# Quota wire v1\n\n\
`usage-get.json` is the exact compact response body from the module's `usage.get` handler, driven by stub providers through the real Registry. It includes fresh 5h and weekly windows, a retained stale reading and fetchedAt, an exhausted (100%) window, subscription-funded and bought pools with expiry, saved reset expiry, a degraded account with no reading, and named extra rate windows. Amount edges include an overdrawn DeepSeek remaining balance (-2 minor, exponent 2, USD) and a large Codex balance (55598241861500 minor, exponent 9). All identities are synthetic example.com addresses; all clocks are fixed.\n\n\
`usage-watch-read-now.json` is the exact first StreamData body produced by the real watch handler: sequence 1 tells the consumer to read now, with no row identity. The epoch is a made-up UUID fixed for reproducibility.\n\n\
`usage-watch-row-change.json` is the next body from the same watch, after the real sample observer sees fresh@example.com's primary usage change. Sequence 2 names that row; notices carry invalidations, not quota snapshots.\n\n\
`README.md` is generated alongside the bodies. String fields are open vocabularies, not closed enums; consumers must tolerate unfamiliar values. See [the consumer contract](../../docs/consumer-contract.md) for semantics and reconciliation rules.\n\n\
`SHA256SUMS` covers every other file in this directory in `shasum -a 256` format. JSON files contain the frame body bytes without a trailing newline. Verify with `shasum -a 256 -c SHA256SUMS` from this directory. Regenerate all files with `INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_vectors_match`. The named drift test checks every file and the checksums byte-for-byte.\n";
