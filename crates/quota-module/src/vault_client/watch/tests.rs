use super::*;
use crate::vault_client::tests::{
    accept_authenticated, loopback_listener, reply_route, LoopbackDir,
};
use quota_core::vault_handles::VaultHandleLoader;

#[derive(Clone, Copy)]
enum Opening {
    Notice,
    Unknown,
    End,
    Refused,
}

struct Stub {
    client: VaultClient,
    commands: mpsc::Sender<(FrameType, Value)>,
    opens: Arc<AtomicU64>,
    reads: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
    _dir: LoopbackDir,
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn reply(request: &Frame, ty: FrameType, value: Value) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        if ty == FrameType::StreamEnd {
            Vec::new()
        } else {
            serde_json::to_vec(&value).unwrap()
        },
    )
    .unwrap()
}

fn notice(seq: u64, view: &str, epoch: &str, keys: &str) -> Value {
    serde_json::json!({"kind":"changed", "epoch":epoch, "seq":seq, "view":view, "keys":keys})
}

impl Stub {
    async fn new(opening: Opening) -> Self {
        let (listener, path, key, daemon_id, dir) = loopback_listener("inventory-watch").await;
        let (commands, mut receive) = mpsc::channel::<(FrameType, Value)>(16);
        let opens = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));
        let (opened, listed) = (Arc::clone(&opens), Arc::clone(&reads));
        let task = tokio::spawn(async move {
            let mut socket = accept_authenticated(&listener, &key, &daemon_id).await;
            reply_route(&mut socket, 4, 5).await;
            let mut held = None;
            let mut view = "v1".to_string();
            let mut denied = false;
            loop {
                tokio::select! {
                    frame = read_frame(&mut socket) => {
                        let Some(frame) = frame.unwrap() else { return; };
                        if frame.header.ty == FrameType::Cancel { continue; }
                        let request: Value = serde_json::from_slice(&frame.body).unwrap();
                        let response = match request["method"].as_str().or_else(|| request["op"].as_str()).unwrap() {
                            "route.open" => reply(&frame, FrameType::Response, serde_json::json!({"route_channel":4,"route_epoch":5})),
                            "auth.watch_scoped" => {
                                assert!(frame.header.flags.is_subscription());
                                assert_eq!(request["params"], serde_json::json!({}));
                                opened.fetch_add(1, Ordering::Relaxed);
                                let result = match if denied { Opening::Refused } else { opening } {
                                    Opening::Notice => reply(&frame, FrameType::StreamData, notice(1, &view, "epoch", "keys")),
                                    Opening::Unknown => reply(&frame, FrameType::Error, serde_json::json!({"code":"unknown_method"})),
                                    Opening::End => reply(&frame, FrameType::StreamEnd, Value::Null),
                                    Opening::Refused => reply(&frame, FrameType::Response, serde_json::json!({"result":{"error":{"class":"permanent","code":"not_found"}}})),
                                };
                                held = Some(frame);
                                result
                            }
                            "credential.list_scoped" => {
                                listed.fetch_add(1, Ordering::Relaxed);
                                if denied {
                                    write_frame(&mut socket, &reply(&frame, FrameType::Response,
                                        serde_json::json!({"result":{"error":{"class":"permanent","code":"not_found"}}}))).await.unwrap();
                                    continue;
                                }
                                reply(&frame, FrameType::Response, serde_json::json!({"result":{
                                    "view":view,"grants":1,"grant_tuples":[],"credentials":[{
                                        "id":format!("apikey:openrouter:{view}"), "type":"apikey:openrouter",
                                        "record_version":1,"state":"active","account_id":view
                                    }]
                                }}))
                            }
                            method => panic!("unexpected method {method}"),
                        };
                        write_frame(&mut socket, &response).await.unwrap();
                    }
                    command = receive.recv() => {
                        let Some((ty, body)) = command else { return; };
                        if ty == FrameType::Response { denied = true; }
                        if ty == FrameType::Goodbye {
                            socket.shutdown().await.unwrap();
                            socket = accept_authenticated(&listener, &key, &daemon_id).await;
                            reply_route(&mut socket, 4, 5).await;
                            held = None;
                            continue;
                        }
                        if let Some(next) = body["view"].as_str() { view = next.to_string(); }
                        write_frame(&mut socket, &reply(held.as_ref().unwrap(), ty, body)).await.unwrap();
                    }
                }
            }
        });
        Self {
            client: VaultClient::new(path),
            commands,
            opens,
            reads,
            task,
            _dir: dir,
        }
    }

    async fn send(&self, ty: FrameType, value: Value) {
        // Discard the first notice's wake permit before waiting for this new frame.
        while tokio::time::timeout(Duration::from_millis(1), self.client.scoped_changed())
            .await
            .is_ok()
        {}
        self.commands.send((ty, value)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), self.client.scoped_changed())
            .await
            .unwrap();
    }

    async fn turn(&self) -> Option<Result<ScopedSnapshot, VaultGetError>> {
        tokio::time::timeout(Duration::from_secs(2), self.client.refresh_scoped())
            .await
            .unwrap()
    }

    async fn minute(&self) {
        let mut watch = self.client.watch.lock().await;
        watch.last_read = watch.last_read.map(|at| at - POLL_INTERVAL);
        if let Some(at) = &mut watch.next_attempt {
            *at -= POLL_INTERVAL;
        }
    }
}

#[tokio::test]
async fn watch_first_notice_reads_once_and_installs_changed_accounts() {
    let stub = Stub::new(Opening::Notice).await;
    let loader = VaultHandleLoader::default();
    loader.install_snapshot(
        stub.turn().await.unwrap().unwrap(),
        std::time::Instant::now(),
    );
    assert_eq!(
        stub.reads.load(Ordering::Relaxed),
        1,
        "connect and seq 1 share one read"
    );
    assert!(stub.client.scoped_watch_open());
    stub.send(FrameType::StreamData, notice(2, "v2", "epoch", "keys"))
        .await;
    loader.install_snapshot(
        stub.turn().await.unwrap().unwrap(),
        std::time::Instant::now(),
    );
    assert_eq!(stub.reads.load(Ordering::Relaxed), 2);
    assert_eq!(
        loader.snapshot().unwrap().rows[0].account_id.as_deref(),
        Some("v2")
    );
    assert_eq!(stub.client.vault_list_reads(), 2);
}

#[tokio::test]
async fn watch_unchanged_view_and_key_only_notices_do_not_read_inventory() {
    let stub = Stub::new(Opening::Notice).await;
    stub.turn().await.unwrap().unwrap();
    stub.send(FrameType::StreamData, notice(2, "v1", "epoch", "keys"))
        .await;
    assert!(stub.turn().await.is_none());
    stub.send(
        FrameType::StreamData,
        notice(3, "v1", "epoch", "other-keys"),
    )
    .await;
    assert!(stub.turn().await.is_none());
    assert_eq!(stub.reads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn watch_holds_inventory_without_sixty_second_reads() {
    let stub = Stub::new(Opening::Notice).await;
    stub.turn().await.unwrap().unwrap();
    for _ in 0..5 {
        stub.minute().await;
        assert!(stub.turn().await.is_none());
    }
    assert_eq!(stub.reads.load(Ordering::Relaxed), 1);
    assert_eq!(stub.opens.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn watch_discontinuous_sequence_or_epoch_rereads_even_the_same_view() {
    let stub = Stub::new(Opening::Notice).await;
    stub.turn().await.unwrap().unwrap();
    for (seq, epoch) in [(3, "epoch"), (4, "new-epoch")] {
        stub.send(FrameType::StreamData, notice(seq, "v1", epoch, "keys"))
            .await;
        stub.turn().await.unwrap().unwrap();
    }
    assert_eq!(stub.reads.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn watch_unknown_method_polls_each_minute_without_reopening() {
    let stub = Stub::new(Opening::Unknown).await;
    stub.turn().await.unwrap().unwrap();
    assert_eq!(stub.client.vault_list_watch(), "unsupported");
    assert!(stub.turn().await.is_none());
    for _ in 0..3 {
        stub.minute().await;
        stub.turn().await.unwrap().unwrap();
    }
    assert_eq!(stub.reads.load(Ordering::Relaxed), 4);
    assert_eq!(stub.opens.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn watch_refused_before_first_notice_reads_once_without_resubscribe_loop() {
    for opening in [Opening::End, Opening::Refused] {
        let stub = Stub::new(opening).await;
        stub.turn().await.unwrap().unwrap();
        assert_eq!(stub.client.vault_list_watch(), "refused");
        for _ in 0..4 {
            assert!(stub.turn().await.is_none());
        }
        assert_eq!(stub.opens.load(Ordering::Relaxed), 1);
        assert_eq!(stub.reads.load(Ordering::Relaxed), 1);
        stub.minute().await;
        stub.turn().await.unwrap().unwrap();
        assert_eq!(stub.opens.load(Ordering::Relaxed), 2);
        stub.minute().await;
        stub.turn().await.unwrap().unwrap();
        assert_eq!(
            stub.opens.load(Ordering::Relaxed),
            2,
            "repeated refusals back off five minutes"
        );
    }
}

#[tokio::test]
async fn watch_ended_after_notice_resubscribes_once_on_the_next_turn() {
    let stub = Stub::new(Opening::Notice).await;
    stub.turn().await.unwrap().unwrap();
    stub.send(FrameType::StreamEnd, Value::Null).await;
    stub.turn().await.unwrap().unwrap();
    assert!(!stub.client.scoped_watch_open());
    stub.minute().await;
    stub.turn().await.unwrap().unwrap();
    assert!(stub.client.scoped_watch_open());
    for _ in 0..3 {
        assert!(stub.turn().await.is_none());
    }
    assert_eq!(stub.opens.load(Ordering::Relaxed), 2);
    assert_eq!(stub.reads.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn watch_reconnect_coalesces_the_new_first_notice_and_connection_read() {
    let stub = Stub::new(Opening::Notice).await;
    stub.turn().await.unwrap().unwrap();
    stub.send(FrameType::Goodbye, Value::Null).await;
    assert!(!stub.client.scoped_watch_open());
    stub.turn().await.unwrap().unwrap();
    assert!(stub.client.scoped_watch_open());
    assert_eq!(stub.opens.load(Ordering::Relaxed), 2);
    assert_eq!(stub.reads.load(Ordering::Relaxed), 2);
    assert_eq!(stub.client.connections_established(), 2);
}

#[tokio::test]
async fn watch_route_closed_reopens_the_route_before_fallback_and_resubscribe() {
    let stub = Stub::new(Opening::Notice).await;
    stub.turn().await.unwrap().unwrap();
    stub.send(FrameType::Error, serde_json::json!({"code":"route_closed"}))
        .await;
    stub.turn().await.unwrap().unwrap();
    assert!(!stub.client.scoped_watch_open());
    stub.minute().await;
    stub.turn().await.unwrap().unwrap();
    assert!(stub.client.scoped_watch_open());
    assert_eq!(stub.opens.load(Ordering::Relaxed), 2);
    assert_eq!(stub.client.connections_established(), 1);
}

#[tokio::test]
async fn watch_revoked_enrollment_confirms_not_found_without_a_retry_loop() {
    let stub = Stub::new(Opening::Notice).await;
    stub.turn().await.unwrap().unwrap();
    stub.send(
        FrameType::Response,
        serde_json::json!({"result":{"error":{"class":"permanent","code":"not_found"}}}),
    )
    .await;
    assert_eq!(stub.turn().await.unwrap(), Err(VaultGetError::NotFound));
    assert_eq!(stub.client.vault_list_watch(), "refused");
    for _ in 0..3 {
        assert!(stub.turn().await.is_none());
    }
    assert_eq!(stub.opens.load(Ordering::Relaxed), 1);
    stub.minute().await;
    assert_eq!(stub.turn().await.unwrap(), Err(VaultGetError::NotFound));
    assert_eq!(stub.client.vault_list_watch(), "refused");
    assert_eq!(stub.opens.load(Ordering::Relaxed), 2);
}
