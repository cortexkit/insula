use super::*;
use quota_core::{
    codex_resets::{ConsumeOutcome, SavedResetError, SavedResetResult},
    provider::{AccountObservation, CredentialHandle, FetchAttempt, UsageProvider},
};
use std::sync::atomic::AtomicUsize;

pub(super) const CLIENT_ID: &str = "phone-request-0001";
pub(super) const ACCOUNT: &str = "synthetic-codex-id";
pub(super) const AT: &str = "2026-07-14T11:48:00Z";

pub(super) struct PhoneProvider {
    pub posts: Arc<AtomicUsize>,
    pub result: Arc<Mutex<Result<ConsumeOutcome, SavedResetError>>>,
}

#[async_trait::async_trait]
impl UsageProvider for PhoneProvider {
    fn name(&self) -> &str {
        "codex"
    }
    async fn fetch_handle(&self, _: &CredentialHandle) -> FetchAttempt {
        let mut entry =
            ProviderUsage::healthy("codex", Some(ACCOUNT.into()), "test", Default::default());
        entry.saved_resets = Some(quota_core::model::SavedResets {
            available_count: if self.posts.load(Ordering::SeqCst) == 0 {
                2
            } else {
                1
            },
            soonest_expires_at: Some("2026-08-01T00:00:00Z".into()),
            credits: vec![],
        });
        let mut attempt = FetchAttempt::from_provider_usage(Ok(entry));
        attempt.observed = Some(AccountObservation::new(
            Some("synthetic-codex-id".into()),
            None,
        ));
        attempt.account_info = Some(quota_core::model::AccountInfo {
            email: Some("phone@example.com".into()),
            ..Default::default()
        });
        attempt.value_observed_at = Some("2026-07-14T12:00:00Z".parse().unwrap());
        attempt
    }
    async fn use_saved_reset(
        &self,
        handle: &CredentialHandle,
        account_id: &str,
        request_id: &str,
    ) -> SavedResetResult {
        assert_eq!(handle, &CredentialHandle::implicit());
        assert_eq!(account_id, "synthetic-codex-id");
        assert_eq!(request_id, CLIENT_ID);
        self.posts.fetch_add(1, Ordering::SeqCst);
        SavedResetResult {
            result: self.result.lock().unwrap().clone(),
            consume_attempted: true,
        }
    }
}

pub(super) async fn fixture() -> (
    Arc<Registry>,
    Arc<AtomicUsize>,
    Arc<Mutex<Result<ConsumeOutcome, SavedResetError>>>,
) {
    let posts = Arc::new(AtomicUsize::new(0));
    let result = Arc::new(Mutex::new(Ok(ConsumeOutcome::Reset)));
    let registry = Arc::new(Registry::new(vec![Box::new(PhoneProvider {
        posts: posts.clone(),
        result: result.clone(),
    })]));
    registry.refresh_tick(&CancellationToken::new()).await;
    (registry, posts, result)
}

pub(super) fn reset_params() -> Value {
    json!({"provider":"codex", "account":ACCOUNT, "request_id":CLIENT_ID})
}

pub(super) async fn call(wire: &mut Wire, epoch: u32, params: Value) -> Frame {
    wire.send(
        Frame::build(
            FrameType::Request,
            control_flags(),
            5,
            epoch,
            701,
            serde_json::to_vec(&json!({"method":USE_SAVED_RESET_OP,"params":params})).unwrap(),
        )
        .unwrap(),
    )
    .await;
    wire.recv().await
}

pub(super) async fn bind(wire: &mut Wire, epoch: u32, principal: Value, harness: Option<&str>) {
    let mut identity = json!({"session":"synthetic-session"});
    if let Some(harness) = harness {
        identity["harness"] = json!(harness);
    }
    let body = json!({"op":"route.bind", "route_channel":5, "epoch":epoch,
        "target":{"kind":"management_surface","module_id":"insula"}, "identity":identity,
        "principal":principal,
        "scope":{"owner":{"kind":"reserved","module_id":"callosum"},"ref":"head","scope_epoch":1,"kind":"worker","owner_authorized":true}});
    wire.send(
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            700,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await;
    let response = wire.recv().await;
    if harness.is_some() {
        assert_eq!(response.header.ty, FrameType::Response);
    }
}

/// A bind with no scope stamp, the shape the desktop app's plain local route
/// has. `bind` always sends a stamp, so it can't build this one.
async fn bind_unscoped(
    wire: &mut Wire,
    epoch: u32,
    principal: Value,
    harness: &str,
    session: &str,
) {
    let identity = json!({"session":session, "harness":harness});
    let body = json!({"op":"route.bind", "route_channel":5, "epoch":epoch,
        "target":{"kind":"management_surface","module_id":"insula"}, "identity":identity,
        "principal":principal});
    wire.send(
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            700,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await;
    assert_eq!(wire.recv().await.header.ty, FrameType::Response);
}

/// The desktop app may use a saved reset during development: an unscoped
/// direct route that isn't federated and isn't the OpenCode plugin's channel.
/// Each of those three conditions, and the principal, refuses on its own.
#[tokio::test]
async fn the_desktop_app_may_use_a_saved_reset_on_an_unscoped_direct_route() {
    let (registry, posts, _) = fixture().await;
    let mut wire = Wire::with_registry(false, 65536, registry).await;
    let direct = || json!({"kind":"direct"});
    for (i, (principal, harness, session)) in [
        (direct(), "fed-mac:fp", "s"),
        (direct(), "alfonso", "prefrontal-core"),
        (json!({"kind":"unverified"}), "alfonso-desktop", "s"),
        (
            json!({"kind":"reserved","module_id":"other"}),
            "alfonso-desktop",
            "s",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let epoch = i as u32 + 1;
        bind_unscoped(&mut wire, epoch, principal, harness, session).await;
        assert_eq!(
            body(&call(&mut wire, epoch, Value::Null).await)["code"],
            "forbidden",
            "{harness}/{session} must not spend"
        );
    }
    assert_eq!(posts.load(Ordering::SeqCst), 0);
    // A direct route WITH a scope stamp is refused too: `bind` always sends one,
    // and `saved_reset_caller_requires_callosum_principal` covers it.
    bind_unscoped(&mut wire, 20, direct(), "alfonso-desktop", "desktop-1").await;
    assert_eq!(
        body(&call(&mut wire, 20, reset_params()).await)["outcome"],
        "reset",
        "the desktop app's own route may spend"
    );
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    wire.stop().await;
    wire.finish().await;
}

pub(super) fn callosum() -> Value {
    json!({"kind":"reserved","module_id":"callosum"})
}
fn body(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test]
async fn saved_reset_caller_requires_callosum_principal() {
    let (registry, posts, _) = fixture().await;
    let mut wire = Wire::with_registry(false, 65536, registry).await;
    for (i, principal) in [
        Value::Null,
        json!({"kind":"direct"}),
        json!({"kind":"unverified"}),
        json!({"kind":"reserved","module_id":"prefrontal-routing"}),
        json!({"kind":"reserved","module_id":"other"}),
    ]
    .into_iter()
    .enumerate()
    {
        let epoch = i as u32 + 1;
        bind(&mut wire, epoch, principal, Some("fed-phone:synthetic-fp")).await;
        let response = call(&mut wire, epoch, Value::Null).await;
        assert_eq!(response.header.ty, FrameType::Error);
        assert_eq!(
            body(&response)["code"],
            "forbidden",
            "a phone harness is trusted only under callosum's attested principal"
        );
    }
    assert_eq!(posts.load(Ordering::SeqCst), 0);
    bind(&mut wire, 20, callosum(), Some("fed-phone:synthetic-fp")).await;
    assert_eq!(
        body(&call(&mut wire, 20, reset_params()).await)["outcome"],
        "reset"
    );
    assert_eq!(posts.load(Ordering::SeqCst), 1);
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn saved_reset_caller_requires_phone_harness() {
    let (registry, posts, _) = fixture().await;
    let mut wire = Wire::with_registry(false, 65536, registry).await;
    for (i, harness) in [
        Some("fed-sim:fp"),
        Some("fed-mac:fp"),
        Some("fed-unknown:fp"),
        Some("fed-phone"),
        Some(""),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let epoch = i as u32 + 1;
        bind(&mut wire, epoch, callosum(), harness).await;
        assert_eq!(
            body(&call(&mut wire, epoch, Value::Null).await)["code"],
            "forbidden",
            "callosum may spend only for a fed-phone: route"
        );
    }
    assert_eq!(posts.load(Ordering::SeqCst), 0);
    bind(&mut wire, 20, callosum(), Some("fed-phone:synthetic-fp")).await;
    assert_eq!(
        body(&call(&mut wire, 20, reset_params()).await)["outcome"],
        "reset"
    );
    bind(&mut wire, 21, callosum(), Some("fed-mac:fp")).await;
    assert_eq!(
        body(&call(&mut wire, 20, reset_params()).await)["code"],
        "forbidden",
        "rebinding revokes the previous epoch"
    );
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn saved_reset_request_id_validation_and_all_wire_mappings() {
    let (registry, posts, result) = fixture().await;
    let mut wire = Wire::with_registry(false, 65536, registry).await;
    bind(&mut wire, 1, callosum(), Some("fed-phone:synthetic-fp")).await;
    for id in [
        json!("short"),
        json!("a".repeat(129)),
        json!("invalid_id"),
        json!("éééééééé"),
        Value::Null,
        json!(12),
    ] {
        let mut params = reset_params();
        params["request_id"] = id;
        assert_eq!(
            body(&call(&mut wire, 1, params).await)["code"],
            "invalid_request"
        );
    }
    for id in ["abcdefgh".to_owned(), "A9-".repeat(42), "x".repeat(128)] {
        assert!(valid_reset_request_id(&id));
    }
    for (field, value, code) in [
        ("provider", "claude", "unsupported_provider"),
        ("account", "missing", "unknown_account"),
    ] {
        let mut params = reset_params();
        params[field] = json!(value);
        assert_eq!(body(&call(&mut wire, 1, params).await)["code"], code);
    }
    assert_eq!(posts.load(Ordering::SeqCst), 0);
    // The published codes are literals here, not `as_code()`: the phone reads
    // these strings, so a renamed code must fail this test.
    for (outcome, code) in [
        (ConsumeOutcome::Reset, "reset"),
        (ConsumeOutcome::NothingToReset, "nothing_to_reset"),
        (ConsumeOutcome::NoCredit, "no_credit"),
        (ConsumeOutcome::AlreadyRedeemed, "already_redeemed"),
    ] {
        *result.lock().unwrap() = Ok(outcome);
        let frame = call(&mut wire, 1, reset_params()).await;
        assert_eq!(frame.header.ty, FrameType::Response);
        assert_eq!(body(&frame), json!({"outcome":code,"request_id":CLIENT_ID}));
    }
    for (error, code) in [
        (
            SavedResetError::RecentlyReset(AT.parse().unwrap()),
            "recently_reset",
        ),
        (
            SavedResetError::ProviderRefused("synthetic refusal".into()),
            "provider_refused",
        ),
        (
            SavedResetError::ProviderUnavailable("timeout".into()),
            "provider_unavailable",
        ),
    ] {
        *result.lock().unwrap() = Err(error);
        let frame = call(&mut wire, 1, reset_params()).await;
        assert_eq!(frame.header.ty, FrameType::Error);
        let body = body(&frame);
        assert_eq!(body["code"], code);
        if code == "recently_reset" {
            assert_eq!(
                body["detail"]["last_reset_at"],
                "2026-07-14T11:48:00.000000000+00:00"
            );
        }
        if code == "provider_refused" {
            assert_eq!(body["detail"]["excerpt"], "synthetic refusal");
            // The detail must survive a decode through subc's own error type,
            // which is what a relay such as callosum uses.
            let decoded: subc_protocol::ErrorBody =
                serde_json::from_value(body.clone()).expect("decodes as subc ErrorBody");
            assert!(
                decoded.detail.is_some(),
                "detail lost through ErrorBody: {body}"
            );
        }
    }
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn saved_reset_marks_slot_due_and_refresh_emits_watch_change() {
    let (registry, _, _) = fixture().await;
    let key = quota_core::store::SlotKey::new("codex", CredentialHandle::implicit());
    let mut wire = Wire::with_registry(false, 65536, registry.clone()).await;
    bind(&mut wire, 1, callosum(), Some("fed-phone:synthetic-fp")).await;
    wire.serve
        .samples
        .observe(registry.get_usage(None).await)
        .await;
    wire.subscribe(5, 1, 703).await;
    wire.recv().await;
    let before = registry
        .slot_store()
        .lock()
        .unwrap()
        .get(&key)
        .unwrap()
        .next_due_at;
    assert!(before > Instant::now());
    assert_eq!(
        body(&call(&mut wire, 1, reset_params()).await)["outcome"],
        "reset"
    );
    assert!(
        registry
            .slot_store()
            .lock()
            .unwrap()
            .get(&key)
            .unwrap()
            .next_due_at
            <= Instant::now(),
        "a completed consume must make the account due now"
    );
    registry.refresh_tick(&CancellationToken::new()).await;
    wire.serve
        .samples
        .observe(registry.get_usage(None).await)
        .await;
    let changed = notice(wire.recv().await, 5, 1, 703);
    assert_eq!(changed["provider"], "codex");
    assert_eq!(changed["account"], ACCOUNT);
    wire.stop().await;
    wire.finish().await;
}
