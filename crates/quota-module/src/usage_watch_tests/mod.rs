use super::*;
use quota_core::model::ProviderUsage;
use quota_module::usage_watch::{MAX_WATCHERS, NOTICE_CAPACITY};
use serde_json::Value;
use std::time::Duration;
use subc_protocol::{AdmissionClass, FLAG_SUBSCRIPTION};
use tokio::{io::DuplexStream, sync::oneshot, task::JoinHandle};

mod quota_wire_vectors;
mod saved_reset_tests;

const DEADLINE: Duration = Duration::from_secs(2);

struct Wire {
    client: DuplexStream,
    serve: Arc<ServeCounters>,
    epoch: String,
    start_writer: Option<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<Result<(), ModuleError>>>,
}

impl Drop for Wire {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Wire {
    async fn new(paused_writer: bool) -> Self {
        Self::with_capacity(paused_writer, 64 * 1024).await
    }

    async fn with_capacity(paused_writer: bool, capacity: usize) -> Self {
        Self::with_registry(paused_writer, capacity, Arc::new(Registry::new(vec![]))).await
    }

    async fn with_registry(paused_writer: bool, capacity: usize, registry: Arc<Registry>) -> Self {
        let (client, server) = tokio::io::duplex(capacity);
        let (mut read, write) = tokio::io::split(server);
        let epoch = registry.drop_page(None).await.epoch;
        let serve = Arc::new(ServeCounters {
            samples: Arc::new(bus::QuotaSamples::new(epoch.clone())),
            ..Default::default()
        });
        let (tx, rx) = mpsc::channel(EGRESS_BUFFER);
        let (start_tx, start_rx) = oneshot::channel();
        let writer_serve = Arc::clone(&serve);
        let writer = tokio::spawn(async move {
            start_rx.await.unwrap();
            drain_writer(write, rx, &writer_serve.samples.watches).await
        });
        let loop_serve = Arc::clone(&serve);
        let frame_loop = tokio::spawn(async move {
            module_loop(
                &mut read,
                tx,
                &ModuleConfig {
                    connection_file_path: PathBuf::from("/nonexistent/usage-watch.json"),
                    module_id: "insula".into(),
                    launch_nonce: None,
                },
                registry,
                Arc::new(VaultClient::new(PathBuf::from(
                    "/nonexistent/usage-watch.json",
                ))),
                loop_serve,
            )
            .await
        });
        let mut wire = Self {
            client,
            serve,
            epoch,
            start_writer: Some(start_tx),
            tasks: vec![frame_loop, writer],
        };
        if !paused_writer {
            wire.start();
            assert_eq!(wire.recv().await.header.ty, FrameType::Hello);
        }
        wire.send(
            Frame::build(
                FrameType::HelloAck,
                control_flags(),
                0,
                0,
                HELLO_CORR,
                serde_json::to_vec(&json!({"negotiated_ver":PROTOCOL_VERSION,
                "subc_ops":[],"subc_capabilities":[]}))
                .unwrap(),
            )
            .unwrap(),
        )
        .await;
        wire
    }

    fn start(&mut self) {
        self.start_writer.take().unwrap().send(()).unwrap();
    }

    async fn send(&mut self, frame: Frame) {
        tokio::time::timeout(DEADLINE, write_frame(&mut self.client, &frame))
            .await
            .expect("request writes must stay prompt")
            .unwrap();
    }

    async fn recv(&mut self) -> Frame {
        tokio::time::timeout(DEADLINE, read_frame(&mut self.client))
            .await
            .expect("expected watch frame before deadline")
            .unwrap()
            .expect("unexpected EOF")
    }

    async fn subscribe(&mut self, channel: u16, epoch: u32, corr: u64) {
        self.send(request(channel, epoch, corr, USAGE_WATCH_OP, true))
            .await;
    }

    async fn quiet(&mut self, reason: &str) {
        assert!(
            tokio::time::timeout(Duration::from_millis(80), read_frame(&mut self.client))
                .await
                .is_err(),
            "{reason}"
        );
    }

    async fn barrier(&mut self) {
        self.send(Frame::build(FrameType::Ping, control_flags(), 0, 0, 999, vec![]).unwrap())
            .await;
        assert_eq!(self.recv().await.header.ty, FrameType::Pong);
    }

    async fn health_metrics(&mut self) -> Value {
        let body = serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).unwrap();
        self.send(Frame::build(FrameType::Request, control_flags(), 0, 0, 998, body).unwrap())
            .await;
        let frame = self.recv().await;
        assert_eq!(
            (frame.header.ty, frame.header.corr),
            (FrameType::Response, 998)
        );
        let ModuleControlResponse::HealthCheck { metrics, .. } =
            serde_json::from_slice(&frame.body).unwrap()
        else {
            panic!("expected health report");
        };
        metrics.unwrap()
    }

    async fn stop(&mut self) {
        self.send(Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, vec![]).unwrap())
            .await;
    }

    async fn finish(mut self) {
        for task in self.tasks.drain(..) {
            tokio::time::timeout(DEADLINE, task)
                .await
                .expect("module shutdown must finish")
                .unwrap()
                .unwrap();
        }
    }
}

fn request(channel: u16, epoch: u32, corr: u64, method: &str, subscription: bool) -> Frame {
    let mut flags = Flags::new(false, Priority::Interactive, false);
    if subscription {
        flags.0 |= FLAG_SUBSCRIPTION;
    }
    Frame::build(
        FrameType::Request,
        flags,
        channel,
        epoch,
        corr,
        serde_json::to_vec(&json!({"method":method})).unwrap(),
    )
    .unwrap()
}

fn entry(account: Option<&str>, used: u64) -> ProviderUsage {
    let mut value = json!({"provider":"deepseek", "source":"api", "fetchedAt":"2026-01-01T00:00:00Z",
        "usage":{"primary":{"usedPercent":used}}, "accountInfo":{"email":"private@example.com"}});
    if let Some(account) = account {
        value["account"] = json!(account);
    }
    serde_json::from_value(value).unwrap()
}

fn notice(frame: Frame, channel: u16, epoch: u32, corr: u64) -> Value {
    assert_eq!(frame.header.ty, FrameType::StreamData);
    assert_eq!(
        (frame.header.channel, frame.header.epoch, frame.header.corr),
        (channel, epoch, corr)
    );
    assert_eq!(frame.header.ver, PROTOCOL_VERSION);
    assert_eq!(
        frame.header.flags.admission_class(),
        Some(AdmissionClass::Sheddable)
    );
    assert!(!frame.header.flags.is_last());
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test]
async fn usage_watch_immediate_notice_has_no_ids() {
    let mut wire = Wire::new(false).await;
    wire.subscribe(5, 7, 91).await;
    let frame = tokio::time::timeout(DEADLINE, read_frame(&mut wire.client))
        .await
        .expect("usage.watch must send immediate seq-1 notice")
        .unwrap()
        .unwrap();
    let value = notice(frame, 5, 7, 91);
    assert_eq!(
        value,
        json!({"kind":"changed", "epoch":wire.epoch, "seq":1})
    );
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_changed_entry_names_its_identity() {
    let mut wire = Wire::new(false).await;
    wire.serve.samples.observe(vec![entry(Some("a"), 20)]).await;
    wire.subscribe(5, 7, 91).await;
    wire.recv().await;
    wire.serve.samples.observe(vec![entry(Some("a"), 21)]).await;
    let value = notice(wire.recv().await, 5, 7, 91);
    assert_eq!(value["seq"], 2);
    assert_eq!(value["provider"], "deepseek");
    assert_eq!(value["account"], "a");
    assert_eq!(
        value.as_object().unwrap().len(),
        5,
        "notices must contain identities, not quota or metadata"
    );
    wire.serve.samples.observe(vec![entry(None, 22)]).await;
    let value = notice(wire.recv().await, 5, 7, 91);
    assert_eq!(value["seq"], 3);
    assert_eq!(value["provider"], "deepseek");
    assert!(value.get("account").is_none());
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_unchanged_turn_is_quiet() {
    let mut wire = Wire::new(false).await;
    wire.serve.samples.observe(vec![entry(Some("a"), 20)]).await;
    wire.subscribe(5, 7, 91).await;
    wire.recv().await;
    wire.serve.samples.observe(vec![entry(Some("a"), 20)]).await;
    wire.quiet("unchanged entries must not notify a watch")
        .await;
    let mut metadata_only = serde_json::to_value(entry(Some("a"), 20)).unwrap();
    metadata_only["accountInfo"] = json!({"email":"renamed@example.com"});
    wire.serve
        .samples
        .observe(vec![serde_json::from_value(metadata_only).unwrap()])
        .await;
    wire.quiet("account metadata alone must not notify a watch")
        .await;
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_cancel_stops_the_correlation() {
    let mut wire = Wire::new(false).await;
    wire.subscribe(5, 7, 91).await;
    wire.recv().await;
    // A stale route epoch must not cancel a watch on a reused channel slot.
    wire.send(Frame::build(FrameType::Cancel, control_flags(), 5, 6, 91, vec![]).unwrap())
        .await;
    wire.barrier().await;
    wire.serve.samples.observe(vec![entry(Some("a"), 20)]).await;
    assert_eq!(notice(wire.recv().await, 5, 7, 91)["seq"], 2);
    wire.send(Frame::build(FrameType::Cancel, control_flags(), 5, 7, 91, vec![]).unwrap())
        .await;
    wire.barrier().await;
    wire.serve.samples.observe(vec![entry(Some("a"), 21)]).await;
    wire.quiet("CANCEL must stop further frames for its correlation")
        .await;
    assert_eq!(wire.serve.samples.metrics()["usageWatchers"], 0);
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_shutdown_sends_a_reliable_terminal() {
    let mut wire = Wire::new(false).await;
    for corr in [91, 92] {
        wire.subscribe(5, 7, corr).await;
        assert_eq!(notice(wire.recv().await, 5, 7, corr)["seq"], 1);
    }
    wire.stop().await;
    for corr in [91, 92] {
        let end = wire.recv().await;
        assert_eq!(end.header.ty, FrameType::StreamEnd);
        assert_eq!(
            (end.header.channel, end.header.epoch, end.header.corr),
            (5, 7, corr)
        );
        assert!(end.header.flags.is_last());
        assert_eq!(
            end.header.flags.admission_class(),
            Some(AdmissionClass::Normal)
        );
    }
    assert_eq!(wire.serve.samples.metrics()["usageWatchers"], 0);
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_overflow_keeps_newest_and_prioritizes_usage_get() {
    // Leave the connection unread and its writer unscheduled while changes
    // accumulate. This deterministically exercises saturation without relying
    // on a platform's socket-buffer size or on task scheduling luck.
    let mut wire = Wire::new(true).await;
    wire.subscribe(5, 7, 91).await;
    tokio::time::timeout(DEADLINE, async {
        while wire.serve.samples.metrics()["usageWatchers"] != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("frame loop must admit a watch without writer progress");
    tokio::time::timeout(Duration::from_millis(100), async {
        for used in 0..40 {
            wire.serve
                .samples
                .observe(vec![entry(Some("a"), used)])
                .await;
        }
    })
    .await
    .expect("a full watch queue must not block the refresher");
    assert_eq!(wire.serve.samples.metrics()["usageWatchNoticesDropped"], 25);
    wire.send(request(5, 7, 92, USAGE_GET_OP, false)).await;
    tokio::time::timeout(DEADLINE, async {
        while wire.serve.served.load(Ordering::Relaxed) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("usage.get on the same route must not await watcher capacity");
    wire.start();
    assert_eq!(wire.recv().await.header.ty, FrameType::Hello);
    let reply = wire.recv().await;
    assert_eq!(
        (reply.header.ty, reply.header.corr),
        (FrameType::Response, 92),
        "ordinary replies must precede queued watch notices"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&reply.body).unwrap()["result"],
        json!([])
    );
    // The immediate notice was sequence 1. A reader expecting it sees the gap
    // and reloads the snapshot, rather than assuming a lossless history.
    for seq in 26..=41 {
        assert_eq!(notice(wire.recv().await, 5, 7, 91)["seq"], seq);
    }
    let metrics = wire.health_metrics().await;
    assert_eq!(metrics["usageWatchers"], 1);
    assert_eq!(metrics["usageWatchNoticesDropped"], 25);
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_limit_and_route_goodbye_release_state() {
    let mut wire = Wire::new(false).await;
    // A request without the SUBSCRIPTION flag is a watch too: the TypeScript
    // subc client never sets bit 7, and the auth plugins subscribe through it.
    wire.send(request(5, 7, 90, USAGE_WATCH_OP, false)).await;
    assert_eq!(notice(wire.recv().await, 5, 7, 90)["seq"], 1);
    wire.send(Frame::build(FrameType::Cancel, control_flags(), 5, 7, 90, vec![]).unwrap())
        .await;
    for corr in 1..=MAX_WATCHERS as u64 {
        wire.subscribe(5, 7, corr).await;
        assert_eq!(notice(wire.recv().await, 5, 7, corr)["seq"], 1);
    }
    wire.subscribe(5, 7, 65).await;
    let error = wire.recv().await;
    assert_eq!(error.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&error.body).unwrap()["code"],
        "usage_watch_limit"
    );
    assert_eq!(wire.serve.samples.metrics()["usageWatchers"], 64);
    wire.send(Frame::build(FrameType::Goodbye, control_flags(), 5, 7, 0, vec![]).unwrap())
        .await;
    wire.barrier().await;
    assert_eq!(wire.serve.samples.metrics()["usageWatchers"], 0);
    wire.subscribe(6, 8, 1).await;
    assert_eq!(notice(wire.recv().await, 6, 8, 1)["seq"], 1);
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_cancel_discards_queued_notices_in_receive_order() {
    let mut wire = Wire::new(true).await;
    wire.subscribe(5, 7, 91).await;
    wire.send(Frame::build(FrameType::Cancel, control_flags(), 5, 7, 91, vec![]).unwrap())
        .await;
    // Keep a second correlation alive, then wait for the subsequent cache read
    // to be served. The receive loop must have registered both subscriptions
    // and processed CANCEL before it dispatches that read.
    wire.subscribe(5, 7, 92).await;
    wire.send(request(5, 7, 93, USAGE_GET_OP, false)).await;
    tokio::time::timeout(DEADLINE, async {
        while wire.serve.served.load(Ordering::Relaxed) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    wire.start();
    assert_eq!(wire.recv().await.header.ty, FrameType::Hello);
    assert_eq!(wire.recv().await.header.corr, 93);
    assert_eq!(notice(wire.recv().await, 5, 7, 92)["seq"], 1);
    wire.quiet("cancelled correlations must discard notices already queued")
        .await;
    wire.stop().await;
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_eof_ends_watches_and_reaps_writer() {
    let mut wire = Wire::new(false).await;
    wire.subscribe(5, 7, 91).await;
    wire.recv().await;
    wire.client.shutdown().await.unwrap();
    let end = wire.recv().await;
    assert_eq!((end.header.ty, end.header.corr), (FrameType::StreamEnd, 91));
    wire.finish().await;
}

#[tokio::test]
async fn usage_watch_stalled_transport_does_not_stall_serving() {
    let mut wire = Wire::with_capacity(false, 512).await;
    wire.subscribe(5, 7, 91).await;
    assert_eq!(notice(wire.recv().await, 5, 7, 91)["seq"], 1);
    // A real byte-level stall, in addition to the deterministic queue test:
    // don't read again until the writer's small transport buffer is full.
    for used in 0..200 {
        wire.serve
            .samples
            .observe(vec![entry(Some("a"), used)])
            .await;
        tokio::task::yield_now().await;
    }
    assert!(
        wire.serve.samples.metrics()["usageWatchNoticesDropped"]
            .as_u64()
            .unwrap()
            > 0
    );
    wire.send(request(5, 7, 92, USAGE_GET_OP, false)).await;
    tokio::time::timeout(DEADLINE, async {
        while wire.serve.served.load(Ordering::Relaxed) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("serving usage.get must progress even when the transport cannot write notices");
    let mut previous = 1;
    let mut saw_gap = false;
    let mut before_reply = 0;
    loop {
        let frame = wire.recv().await;
        if frame.header.ty == FrameType::Response {
            assert_eq!(frame.header.corr, 92);
            break;
        }
        let seq = notice(frame, 5, 7, 91)["seq"].as_u64().unwrap();
        saw_gap |= seq != previous + 1;
        previous = seq;
        before_reply += 1;
        assert!(
            before_reply < NOTICE_CAPACITY,
            "buffered notices must not starve the ordinary reply"
        );
    }
    while previous < 201 {
        let seq = notice(wire.recv().await, 5, 7, 91)["seq"].as_u64().unwrap();
        saw_gap |= seq != previous + 1;
        previous = seq;
    }
    assert!(
        saw_gap,
        "the next delivered notices must expose the dropped sequence numbers"
    );
    wire.stop().await;
    wire.finish().await;
}

struct ClaudePoll;
#[async_trait::async_trait]
impl quota_core::provider::UsageProvider for ClaudePoll {
    fn name(&self) -> &str {
        "claude"
    }
    async fn fetch_handle(
        &self,
        _: &quota_core::provider::CredentialHandle,
    ) -> quota_core::provider::FetchAttempt {
        let entry = serde_json::from_value(
            json!({"provider":"claude","account":"work","source":"oauth",
            "usage":{"primary":{"usedPercent":90.0,"windowMinutes":300,"windowKind":"five_hour"},
            "secondary":{"usedPercent":99.0,"windowMinutes":10080,"windowKind":"weekly"},
            "extraRateWindows":[{"id":"fable","window":{"usedPercent":13.0}}]}}),
        )
        .unwrap();
        quota_core::provider::FetchAttempt::from_provider_usage(Ok(entry))
    }
}

async fn bind_writer(wire: &mut Wire, principal: Value, epoch: u32) {
    // Scope ownership is deliberately routing even for untrusted callers. It
    // attests the scope owner, not the module sending requests on this route.
    let body = json!({"op":"route.bind","route_channel":5,"epoch":epoch,
        "target":{"kind":"management_surface","module_id":"insula"},
        "identity":{"project_root":"/tmp/x","harness":"prefrontal-routing","session":"s"},
        "principal":principal,
        "scope":{"owner":{"kind":"reserved","module_id":"prefrontal-routing"},
            "ref":"head","scope_epoch":1,"kind":"worker","owner_authorized":true}});
    wire.send(
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            400,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await;
    assert_eq!(wire.recv().await.header.ty, FrameType::Response);
}

async fn observe_wire(wire: &mut Wire, epoch: u32, corr: u64, at: i64) -> Frame {
    let body = json!({"method":"usage.observe", "params":{"observations":[
        {"provider":"claude","account":"missing","observedAtMs":at,"headers":{}},
        {"provider":"claude","account":"work","observedAtMs":at,
            "headers":{"anthropic-ratelimit-unified-5h-utilization":"0.21", "anthropic-ratelimit-unified-5h-reset":"1790394000",
                "anthropic-ratelimit-unified-7d-utilization":"1.0","anthropic-ratelimit-unified-7d-reset":"1790740800"}}
    ]}});
    wire.send(
        Frame::build(
            FrameType::Request,
            control_flags(),
            5,
            epoch,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await;
    wire.recv().await
}

#[tokio::test]
async fn usage_observe_requires_attested_routing_and_changes_get_samples_and_watch() {
    let registry = Arc::new(Registry::new(vec![Box::new(ClaudePoll)]));
    registry.refresh_tick(&CancellationToken::new()).await;
    let before = serde_json::to_value(registry.get_usage(None).await).unwrap();
    let at = 1890394000123;
    let mut wire = Wire::with_registry(false, 64 * 1024, registry.clone()).await;
    wire.serve
        .samples
        .observe(registry.get_usage(None).await)
        .await;
    for (epoch, principal) in [
        (1, Value::Null),
        (2, json!({"kind":"direct"})),
        (3, json!({"kind":"unverified"})),
        (4, json!({"kind":"reserved","module_id":"prefrontal-core"})),
    ] {
        bind_writer(&mut wire, principal, epoch).await;
        let response = observe_wire(&mut wire, epoch, 401, at).await;
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            body["code"], "forbidden",
            "only attested routing may observe"
        );
        assert_eq!(
            serde_json::to_value(registry.get_usage(None).await).unwrap(),
            before,
            "a forbidden writer must change nothing"
        );
        assert_eq!(registry.call_observation_metrics().applied, 0);
        assert!(registry.call_observation_metrics().ignored.is_empty());
    }
    bind_writer(
        &mut wire,
        json!({"kind":"reserved","module_id":"prefrontal-routing"}),
        5,
    )
    .await;
    wire.subscribe(5, 5, 91).await;
    wire.recv().await;
    let response = observe_wire(&mut wire, 5, 402, at).await;
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["result"]["accepted"], 1);
    assert_eq!(body["result"]["ignored"][0]["reason"], "unknown_account");
    let changed = notice(wire.recv().await, 5, 5, 91);
    assert_eq!(changed["provider"], "claude");
    assert_eq!(changed["account"], "work");
    wire.send(request(5, 5, 403, USAGE_GET_OP, false)).await;
    let served: Value = serde_json::from_slice(&wire.recv().await.body).unwrap();
    assert_eq!(served["result"][0]["usage"]["primary"]["usedPercent"], 21.0);
    assert_eq!(
        served["result"][0]["usage"]["extraRateWindows"][0]["window"]["usedPercent"],
        13.0
    );
    assert_eq!(registry.drop_page(None).await.drops.len(), 1);
    let metrics = wire.health_metrics().await;
    assert_eq!(metrics["callObservationsApplied"], 1);
    assert_eq!(metrics["callObservationsIgnored"]["unknown_account"], 1);

    bind_writer(&mut wire, json!({"kind":"direct"}), 6).await;
    let stale_epoch = observe_wire(&mut wire, 5, 404, at + 1).await;
    let body: Value = serde_json::from_slice(&stale_epoch.body).unwrap();
    assert_eq!(body["code"], "forbidden");
    wire.stop().await;
    wire.finish().await;
}
