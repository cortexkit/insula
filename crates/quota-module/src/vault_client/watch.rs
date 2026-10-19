//! Inventory notifications share the existing vault route, never another connection.
use super::*;
use cortexkit_claustrum_client::{WatchAction, WatchNotice, WatchTracker};

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const REFUSAL_BACKOFF: Duration = Duration::from_secs(300);

pub(super) struct StreamSender {
    pub generation: u64,
    pub route: RouteLease,
    pub sender: mpsc::Sender<Result<Frame, ClientFailure>>,
}

#[cfg(test)]
mod tests;

struct Subscription {
    state: Arc<ClientState>,
    connection: Arc<LiveConnection>,
    route: RouteLease,
    corr: u64,
    receiver: mpsc::Receiver<Result<Frame, ClientFailure>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.state
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(self.route.route.epoch, self.corr));
        // A timed-out or malformed watch must release the vault's per-route slot.
        if let Ok(frame) = Frame::build(
            FrameType::Cancel,
            Flags::new(false, Priority::Interactive, false),
            self.route.route.channel,
            self.route.route.epoch,
            self.corr,
            Vec::new(),
        ) {
            let _ = self.connection.writer.try_send(frame);
        }
    }
}

#[derive(Default)]
pub(super) struct InventoryWatch {
    tracker: WatchTracker,
    subscription: Option<Subscription>,
    generation: Option<u64>,
    next_attempt: Option<Instant>,
    last_read: Option<Instant>,
    failures: u32,
    unsupported: bool,
    dirty: bool,
    confirming_refusal: bool,
}

impl VaultClient {
    pub fn vault_list_watch(&self) -> &'static str {
        match self.state.watch_health.load(Ordering::Relaxed) {
            1 => "watching",
            2 => "refused",
            3 => "unsupported",
            _ => "polling",
        }
    }

    pub fn vault_list_reads(&self) -> u64 {
        self.state.list_reads.load(Ordering::Relaxed)
    }
}

impl InventoryWatch {
    fn health(&self, client: &VaultClient, state: u8) {
        client.state.watch_health.store(state, Ordering::Relaxed);
    }

    fn retry_later(&mut self) {
        self.failures = self.failures.saturating_add(1);
        self.next_attempt = Some(
            Instant::now()
                + if self.failures > 1 {
                    REFUSAL_BACKOFF
                } else {
                    POLL_INTERVAL
                },
        );
    }

    async fn open(
        &mut self,
        client: &VaultClient,
        connection: Arc<LiveConnection>,
    ) -> Result<(), ClientFailure> {
        let route = client.state.route(&connection).await?;
        let corr = client.state.next_corr();
        let (sender, receiver) = mpsc::channel(64);
        let frame = Frame::build(
            FrameType::Request,
            Flags(
                Flags::new(false, Priority::Interactive, false).0
                    | subc_protocol::FLAG_SUBSCRIPTION,
            ),
            route.route.channel,
            route.route.epoch,
            corr,
            serde_json::to_vec(&serde_json::json!({"method":"auth.watch_scoped","params":{}}))
                .map_err(|_| ClientFailure::Protocol)?,
        )
        .map_err(|_| ClientFailure::Protocol)?;
        client
            .state
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (route.route.epoch, corr),
                StreamSender {
                    generation: connection.generation,
                    route,
                    sender,
                },
            );
        self.subscription = Some(Subscription {
            state: Arc::clone(&client.state),
            connection: Arc::clone(&connection),
            route,
            corr,
            receiver,
        });
        self.tracker.opened();
        if connection.writer.send(frame).await.is_err() {
            client
                .state
                .invalidate_connection(connection.generation)
                .await;
            return Err(ClientFailure::Transport);
        }
        Ok(())
    }

    async fn frame(&mut self, client: &VaultClient, frame: Result<Frame, ClientFailure>) {
        let action = match frame {
            Ok(frame) if frame.header.ty == FrameType::StreamData => {
                match WatchNotice::decode(&frame.body) {
                    Ok(notice) => {
                        self.health(client, 1);
                        self.failures = 0;
                        self.next_attempt = None;
                        self.tracker.notice(&notice)
                    }
                    Err(_) => {
                        self.health(client, 2);
                        self.tracker.refused()
                    }
                }
            }
            Ok(frame) if frame.header.ty == FrameType::StreamEnd => self.tracker.closed(),
            Ok(frame) if frame.header.ty == FrameType::Error => {
                let body = serde_json::from_slice::<Value>(&frame.body).unwrap_or(Value::Null);
                let code = body
                    .get("code")
                    .or_else(|| body.pointer("/error/code"))
                    .and_then(Value::as_str);
                if code == Some("unknown_method") {
                    self.unsupported = true;
                    self.health(client, 3);
                    self.tracker.unknown_method()
                } else {
                    if let Some(subscription) = &self.subscription {
                        client.state.invalidate_route(subscription.route).await;
                    }
                    self.health(client, 0);
                    self.tracker.route_lost()
                }
            }
            Ok(_) => {
                // Domain refusals arrive as RESPONSE, including enrollment revocation.
                self.health(client, 2);
                self.tracker.refused()
            }
            Err(_) => {
                self.health(client, 0);
                self.tracker.route_lost()
            }
        };
        match action {
            WatchAction::Reread { inventory, .. } => self.dirty |= inventory,
            WatchAction::ReadListOnce => {
                self.confirming_refusal = true;
                self.dirty = true;
                self.health(client, 2);
                self.subscription = None;
                self.retry_later();
            }
            WatchAction::ResubscribeAndReread | WatchAction::ReconnectAndReread => {
                self.dirty = true;
                self.health(client, 0);
                self.subscription = None;
                self.retry_later();
            }
            WatchAction::Fallback | WatchAction::Stop => {
                self.dirty = true;
                self.subscription = None;
                self.retry_later();
            }
            WatchAction::RetryLater => self.retry_later(),
        }
    }

    pub(super) async fn refresh(
        &mut self,
        client: &VaultClient,
    ) -> Option<Result<CompleteScopedListResult, VaultGetError>> {
        let now = Instant::now();
        let connection = match client.state.connection().await {
            Ok(connection) => Some(connection),
            Err(_) => {
                self.subscription = None;
                self.health(client, 0);
                None
            }
        };
        if let Some(connection) = connection.as_ref() {
            if self.generation != Some(connection.generation) {
                self.subscription = None;
                self.generation = Some(connection.generation);
                self.unsupported = false;
                self.next_attempt = None;
                self.dirty = true;
                self.health(client, 0);
            }
        }
        if let Some(subscription) = self.subscription.as_mut() {
            let mut frames = Vec::new();
            let disconnected = loop {
                match subscription.receiver.try_recv() {
                    Ok(frame) => frames.push(frame),
                    Err(mpsc::error::TryRecvError::Empty) => break false,
                    Err(mpsc::error::TryRecvError::Disconnected) => break true,
                }
            };
            for frame in frames {
                self.frame(client, frame).await;
            }
            if disconnected && self.subscription.is_some() {
                self.frame(client, Err(ClientFailure::Transport)).await;
            }
        }
        if self.subscription.is_none()
            && !self.unsupported
            && self.next_attempt.is_none_or(|deadline| now >= deadline)
        {
            if let Some(connection) = connection {
                // An accepted watch sends seq 1 immediately. Waiting for it coalesces
                // the connection read and the tracker's mandatory first-notice read.
                let first = tokio::time::timeout(client.state.request_timeout, async {
                    self.open(client, connection).await?;
                    self.subscription
                        .as_mut()
                        .unwrap()
                        .receiver
                        .recv()
                        .await
                        .unwrap_or(Err(ClientFailure::Transport))
                })
                .await
                .unwrap_or(Err(ClientFailure::Transport));
                self.frame(client, first).await;
            }
        }
        let polling_due = self.subscription.is_none()
            && self
                .last_read
                .is_none_or(|read| now.saturating_duration_since(read) >= POLL_INTERVAL);
        if !self.dirty && !polling_due {
            return None;
        }
        self.dirty = false;
        self.last_read = Some(now);
        let result = client.list_scoped().await;
        if let Ok(snapshot) = &result {
            self.tracker.inventory_read(snapshot.view.clone());
        } else {
            // A failed read must be retried even if the producer sends no new digest.
            self.subscription = None;
            self.health(
                client,
                if self.unsupported {
                    3
                } else if client.vault_list_watch() == "refused" {
                    2
                } else {
                    0
                },
            );
            if self.next_attempt.is_none() {
                self.retry_later();
            }
        }
        if self.confirming_refusal {
            self.confirming_refusal = false;
            let _ = if matches!(
                &result,
                Err(VaultGetError::Transient | VaultGetError::Unavailable)
            ) {
                self.tracker.route_lost()
            } else {
                self.tracker.list_result(result.is_ok())
            };
        }
        Some(result)
    }
}
