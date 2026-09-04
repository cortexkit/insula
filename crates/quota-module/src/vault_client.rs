//! Multiplexed subc consumer for `cortexkit-credentials`.
//!
//! Connection and route readiness are separate single-flight state machines. A
//! route failure reopens only the route; transport death replaces the connection.
//! Generation checks prevent late failures from evicting newer state, and every
//! waiter owns a drop guard that removes its pending correlation on cancellation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use quota_core::credential_source::{
    CredentialSource, CredentialStatus, VaultCapability, VaultCredential, VaultGetError,
};
use quota_core::LOG_TAG;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, Notify};
use tokio::time::Instant;

use crate::ids::CREDENTIALS_MODULE_ID;
/// Bounds both the TCP connect and the HMAC handshake that follows it.
///
/// One constant covers two concerns because they share a budget: a peer that
/// accepts the socket and then stalls mid-handshake is as unreachable as one
/// that never accepts, and neither should hold a refresher tick. Named here
/// because reusing a connect timeout for an authentication deadline is the kind
/// of choice that looks like an oversight when it is read back.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FAILED_CONNECT_COOLDOWN: Duration = Duration::from_secs(2);
const WRITER_BUFFER: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Route {
    channel: u16,
    epoch: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientFailure {
    Transport,
    RouteGone,
    Classified(VaultGetError),
    /// A route-layer error frame carrying a code this client has not been taught.
    ///
    /// SEPARATE FROM `Protocol` BECAUSE THE TWO ARE DIFFERENT CLAIMS. `Protocol`
    /// means this client could not read the reply at all -- a body that does not
    /// decode, a field that is missing. This means the reply was well-formed and
    /// the daemon told us something about ROUTING that we do not recognise, which
    /// is a statement about the route rather than about the credential.
    UnknownRouteCode,
    Protocol,
}

impl ClientFailure {
    fn vault_error(self) -> VaultGetError {
        match self {
            Self::Transport | Self::RouteGone => VaultGetError::Transient,
            Self::Classified(error) => error,
            // TRANSIENT, THOUGH UNRECOGNISED. The vault states its verdict in
            // `class`; a frame without one is the ROUTE speaking, and no route
            // condition is evidence that a credential is bad. Failing closed here
            // spends 300 seconds and replaces a healthy cached window with
            // `decode_failed` on the strength of a code we simply have not read
            // yet -- which is precisely how `module_warming` cost five minutes a
            // restart (insula#14), and the exhaustiveness test cannot stop the
            // next one: Rust has no reflection over the daemon's constants, so a
            // new code falls through here silently.
            //
            // The vault's own TypeScript client reached this default independently
            // from the other side of the wire: unrecognised or absent class is
            // transient, and the discarded value is logged.
            //
            // BORROWED, NOT VERIFIED HERE: that reading is the vault seat's report
            // of their own code (packages/client/src/errors.ts:57-66, reported
            // 2026-09-02), not something this repository can check. It is
            // corroboration for a decision the paragraph above already justifies
            // on its own, so if it has since drifted the argument does not move.
            // The date is here because a cited source with no date reads as
            // current forever.
            //
            // Note their premise does NOT transfer automatically: a
            // request/response client surfaces a repeated failure to its caller,
            // while this background refresher does not. It holds here only
            // because a stale-served entry publishes `stale: { since, class }`.
            //
            // Retrying costs one tick; failing closed costs an outage with a
            // misattributed cause.
            //
            // The accepted risk is the same one the `unknown_module` arm takes: a
            // permanently unrecognised code stale-serves a healthy-looking window
            // indefinitely. That is why this path logs the code -- the wire looks
            // fine, so stderr has to be where it does not.
            Self::UnknownRouteCode => VaultGetError::Transient,
            Self::Protocol => VaultGetError::FailClosed,
        }
    }
}

struct SharedAttempt<T: Clone> {
    result: Mutex<Option<Result<T, ClientFailure>>>,
    ready: Notify,
}

impl<T: Clone> SharedAttempt<T> {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Notify::new(),
        }
    }

    fn complete(&self, result: Result<T, ClientFailure>) {
        *self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
        self.ready.notify_waiters();
    }

    async fn wait(&self) -> Result<T, ClientFailure> {
        loop {
            let notified = self.ready.notified();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

struct LiveConnection {
    generation: u64,
    writer: mpsc::Sender<Frame>,
}

impl std::fmt::Debug for LiveConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveConnection")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

enum ConnectionState {
    Empty {
        retry_after: Option<Instant>,
    },
    Connecting {
        generation: u64,
        attempt: Arc<SharedAttempt<Arc<LiveConnection>>>,
    },
    Ready(Arc<LiveConnection>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RouteLease {
    route: Route,
    connection_generation: u64,
    route_generation: u64,
}

enum RouteState {
    Empty,
    Opening {
        connection_generation: u64,
        route_generation: u64,
        attempt: Arc<SharedAttempt<RouteLease>>,
    },
    Ready(RouteLease),
}

struct Pending {
    connection_generation: u64,
    sender: oneshot::Sender<Result<Frame, ClientFailure>>,
}

struct ClientState {
    connection_file_path: PathBuf,
    connection: AsyncMutex<ConnectionState>,
    route: AsyncMutex<RouteState>,
    pending: Mutex<HashMap<(u32, u64), Pending>>,
    next_connection_generation: AtomicU64,
    next_route_generation: AtomicU64,
    /// Frames the reader loop discarded, by class. See [`Self::dispatch`].
    unmatched_terminal_drops: AtomicU64,
    stale_generation_drops: AtomicU64,
    next_corr: AtomicU64,
    request_timeout: Duration,
}

impl ClientState {
    fn new(connection_file_path: PathBuf) -> Self {
        Self::with_timeout(connection_file_path, REQUEST_TIMEOUT)
    }

    fn with_timeout(connection_file_path: PathBuf, request_timeout: Duration) -> Self {
        Self {
            connection_file_path,
            connection: AsyncMutex::new(ConnectionState::Empty { retry_after: None }),
            route: AsyncMutex::new(RouteState::Empty),
            pending: Mutex::new(HashMap::new()),
            next_connection_generation: AtomicU64::new(1),
            next_route_generation: AtomicU64::new(1),
            unmatched_terminal_drops: AtomicU64::new(0),
            stale_generation_drops: AtomicU64::new(0),
            next_corr: AtomicU64::new(1),
            request_timeout,
        }
    }

    fn next_corr(&self) -> u64 {
        self.next_corr.fetch_add(1, Ordering::Relaxed)
    }

    async fn connection(self: &Arc<Self>) -> Result<Arc<LiveConnection>, ClientFailure> {
        let (attempt, opener, generation) = {
            let mut state = self.connection.lock().await;
            match &*state {
                ConnectionState::Ready(connection) => return Ok(Arc::clone(connection)),
                ConnectionState::Connecting {
                    generation,
                    attempt,
                } => (Arc::clone(attempt), false, *generation),
                ConnectionState::Empty { retry_after }
                    if retry_after.is_some_and(|deadline| deadline > Instant::now()) =>
                {
                    return Err(ClientFailure::Transport);
                }
                ConnectionState::Empty { .. } => {
                    let generation = self
                        .next_connection_generation
                        .fetch_add(1, Ordering::Relaxed);
                    let attempt = Arc::new(SharedAttempt::new());
                    *state = ConnectionState::Connecting {
                        generation,
                        attempt: Arc::clone(&attempt),
                    };
                    (attempt, true, generation)
                }
            }
        };

        if opener {
            let state_owner = Arc::clone(self);
            let shared_attempt = Arc::clone(&attempt);
            tokio::spawn(async move {
                let result = state_owner.open_connection(generation).await;
                {
                    let mut state = state_owner.connection.lock().await;
                    if matches!(
                        &*state,
                        ConnectionState::Connecting {
                            generation: current_generation,
                            attempt: current_attempt,
                        } if *current_generation == generation
                            && Arc::ptr_eq(current_attempt, &shared_attempt)
                    ) {
                        *state = match &result {
                            Ok(connection) => ConnectionState::Ready(Arc::clone(connection)),
                            Err(_) => ConnectionState::Empty {
                                retry_after: Some(Instant::now() + FAILED_CONNECT_COOLDOWN),
                            },
                        };
                    }
                }
                shared_attempt.complete(result);
            });
        }
        attempt.wait().await
    }

    async fn open_connection(
        self: &Arc<Self>,
        generation: u64,
    ) -> Result<Arc<LiveConnection>, ClientFailure> {
        let metadata = connection_file::read(&self.connection_file_path)
            .map_err(|_| ClientFailure::Transport)?;
        let endpoint = metadata.endpoints.first().ok_or(ClientFailure::Transport)?;
        let address = format!("{}:{}", endpoint.host, endpoint.port);
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address))
            .await
            .map_err(|_| ClientFailure::Transport)?
            .map_err(|_| ClientFailure::Transport)?;
        authenticate_client(&mut stream, &metadata, CONNECT_TIMEOUT)
            .await
            .map_err(|_| ClientFailure::Transport)?;

        let (reader, writer) = tokio::io::split(stream);
        let (writer_tx, writer_rx) = mpsc::channel(WRITER_BUFFER);
        let connection = Arc::new(LiveConnection {
            generation,
            writer: writer_tx,
        });

        let writer_state = Arc::clone(self);
        tokio::spawn(async move {
            let mut writer = writer;
            let mut outgoing = writer_rx;
            while let Some(frame) = outgoing.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() || writer.flush().await.is_err()
                {
                    writer_state.invalidate_connection(generation).await;
                    return;
                }
            }
        });

        let reader_state = Arc::clone(self);
        let pong_writer = connection.writer.clone();
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(frame)) if frame.header.ty == FrameType::Ping => {
                        if let Ok(pong) = Frame::build_with_version(
                            frame.header.ver,
                            FrameType::Pong,
                            frame.header.flags,
                            0,
                            0,
                            frame.header.corr,
                            Vec::new(),
                        ) {
                            if pong_writer.send(pong).await.is_err() {
                                reader_state.invalidate_connection(generation).await;
                                return;
                            }
                        }
                    }
                    Ok(Some(frame))
                        if matches!(frame.header.ty, FrameType::Response | FrameType::Error) =>
                    {
                        reader_state.dispatch(generation, frame);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        reader_state.invalidate_connection(generation).await;
                        return;
                    }
                }
            }
        });
        Ok(connection)
    }

    /// Deliver one frame to its waiting caller, or count the drop.
    ///
    /// Two classes of frame are correctly discarded here: one whose
    /// (epoch, corr) matches no pending entry, and one that matches an entry
    /// belonging to an older connection. Both are right -- a reply to a request
    /// whose caller has gone, and a reply arriving after a reconnect that
    /// already failed the call.
    ///
    /// The DROPPING is correct; the invisibility is the defect. A silently
    /// discarded reply and a peer that sent nothing produce the same observable
    /// -- a call that waits and then times out -- and those two states have
    /// opposite investigations. Counting separates them without changing any
    /// behaviour: a non-zero count says the frames arrived and this client threw
    /// them away.
    ///
    /// The counters are cumulative for the process and deliberately not reset by
    /// a reconnect, since a reconnect is exactly when a burst is expected and
    /// clearing them would erase the evidence of the event that caused it.
    fn dispatch(&self, generation: u64, frame: Frame) {
        let key = (frame.header.epoch, frame.header.corr);
        let (pending, stale_generation) = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match pending.get(&key) {
                Some(entry) if entry.connection_generation == generation => {
                    (pending.remove(&key), false)
                }
                // Matched a caller, but one from a previous connection.
                Some(_) => (None, true),
                None => (None, false),
            }
        };
        if let Some(pending) = pending {
            let _ = pending.sender.send(Ok(frame));
            return;
        }
        if stale_generation {
            self.stale_generation_drops.fetch_add(1, Ordering::Relaxed);
        } else {
            self.unmatched_terminal_drops
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Frames discarded because no caller was waiting for them.
    /// How many vault connections this process has established.
    ///
    /// 1 means the first connection is still in use; every increment is a
    /// reconnect after a transport failure. There is no idle timeout and no
    /// maximum lifetime here, and the client answers pings, so a healthy
    /// connection is held indefinitely -- this number can sit at 1 for the life
    /// of the process.
    ///
    /// PUBLISHED BECAUSE AN INCIDENT WAS NOT NARROWABLE WITHOUT IT. A vault
    /// record was re-sealed and this module kept publishing the pre-re-seal
    /// verdict for an hour, recovering only on restart (insula#8). Every
    /// in-process explanation was falsifiable by test; what remained was that a
    /// restart is the one event in such a timeline that establishes a NEW
    /// connection. Answering "did the connection change?" required reading this
    /// source, which is not available to whoever is looking at a stuck lane.
    ///
    /// Diagnostic, not a health signal: reconnects are ordinary around a daemon
    /// restart. It answers a question about a moment, not about whether anything
    /// is wrong.
    pub fn connections_established(&self) -> u64 {
        // The counter names the NEXT generation to hand out, so the number of
        // connections made so far is one less.
        self.next_connection_generation
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(1)
    }

    pub fn unmatched_terminal_drops(&self) -> u64 {
        self.unmatched_terminal_drops.load(Ordering::Relaxed)
    }

    /// Frames discarded because their caller belonged to an older connection.
    pub fn stale_generation_drops(&self) -> u64 {
        self.stale_generation_drops.load(Ordering::Relaxed)
    }

    async fn invalidate_connection(&self, failed_generation: u64) {
        {
            let mut state = self.connection.lock().await;
            let belongs_to_failed_generation = match &*state {
                ConnectionState::Ready(connection) => connection.generation == failed_generation,
                ConnectionState::Connecting { generation, .. } => *generation == failed_generation,
                ConnectionState::Empty { .. } => false,
            };
            if belongs_to_failed_generation {
                *state = ConnectionState::Empty { retry_after: None };
            }
        }
        {
            let mut route = self.route.lock().await;
            let belongs_to_failed_connection = match &*route {
                RouteState::Ready(lease) => lease.connection_generation == failed_generation,
                RouteState::Opening {
                    connection_generation,
                    ..
                } => *connection_generation == failed_generation,
                RouteState::Empty => false,
            };
            if belongs_to_failed_connection {
                *route = RouteState::Empty;
            }
        }
        let failed = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let keys: Vec<_> = pending
                .iter()
                .filter_map(|(key, value)| {
                    (value.connection_generation == failed_generation).then_some(*key)
                })
                .collect();
            keys.into_iter()
                .filter_map(|key| pending.remove(&key))
                .collect::<Vec<_>>()
        };
        for pending in failed {
            let _ = pending.sender.send(Err(ClientFailure::Transport));
        }
    }

    async fn route(
        self: &Arc<Self>,
        connection: &Arc<LiveConnection>,
    ) -> Result<RouteLease, ClientFailure> {
        let (attempt, opener, route_generation) = {
            let mut state = self.route.lock().await;
            match &*state {
                RouteState::Ready(lease)
                    if lease.connection_generation == connection.generation =>
                {
                    return Ok(*lease);
                }
                RouteState::Opening {
                    connection_generation,
                    route_generation,
                    attempt,
                } if *connection_generation == connection.generation => {
                    (Arc::clone(attempt), false, *route_generation)
                }
                _ => {
                    let route_generation =
                        self.next_route_generation.fetch_add(1, Ordering::Relaxed);
                    let attempt = Arc::new(SharedAttempt::new());
                    *state = RouteState::Opening {
                        connection_generation: connection.generation,
                        route_generation,
                        attempt: Arc::clone(&attempt),
                    };
                    (attempt, true, route_generation)
                }
            }
        };
        if opener {
            let state_owner = Arc::clone(self);
            let connection = Arc::clone(connection);
            let shared_attempt = Arc::clone(&attempt);
            tokio::spawn(async move {
                let result = state_owner.open_route(&connection, route_generation).await;
                {
                    let mut state = state_owner.route.lock().await;
                    if matches!(
                        &*state,
                        RouteState::Opening {
                            connection_generation,
                            route_generation: current_generation,
                            attempt: current_attempt,
                        } if *connection_generation == connection.generation
                            && *current_generation == route_generation
                            && Arc::ptr_eq(current_attempt, &shared_attempt)
                    ) {
                        *state = match result {
                            Ok(lease) => RouteState::Ready(lease),
                            Err(_) => RouteState::Empty,
                        };
                    }
                }
                shared_attempt.complete(result);
            });
        }
        attempt.wait().await
    }

    async fn open_route(
        self: &Arc<Self>,
        connection: &Arc<LiveConnection>,
        route_generation: u64,
    ) -> Result<RouteLease, ClientFailure> {
        let corr = self.next_corr();
        let body = serde_json::to_vec(&serde_json::json!({
            "op": "route.open",
            "target": {
                "kind": "management_surface",
                "module_id": CREDENTIALS_MODULE_ID,
            },
            "identity": {
                "project_root": "/",
                "harness": "ck-quota",
                "session": "vault-consumer",
            },
        }))
        .map_err(|_| ClientFailure::Protocol)?;
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            corr,
            body,
        )
        .map_err(|_| ClientFailure::Protocol)?;
        let response = tokio::time::timeout(self.request_timeout, self.request(connection, frame))
            .await
            .map_err(|_| ClientFailure::Transport)??;
        if response.header.ty == FrameType::Error {
            return Err(classify_error_frame(&response.body));
        }
        let value: Value =
            serde_json::from_slice(&response.body).map_err(|_| ClientFailure::Protocol)?;
        let channel = value
            .get("route_channel")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or(ClientFailure::Protocol)?;
        let epoch = value
            .get("route_epoch")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(ClientFailure::Protocol)?;
        Ok(RouteLease {
            route: Route { channel, epoch },
            connection_generation: connection.generation,
            route_generation,
        })
    }

    async fn invalidate_route(&self, failed: RouteLease) {
        let mut state = self.route.lock().await;
        if matches!(
            &*state,
            RouteState::Ready(current)
                if current.connection_generation == failed.connection_generation
                    && current.route_generation == failed.route_generation
        ) {
            *state = RouteState::Empty;
        }
    }

    async fn request(
        self: &Arc<Self>,
        connection: &Arc<LiveConnection>,
        frame: Frame,
    ) -> Result<Frame, ClientFailure> {
        let key = (frame.header.epoch, frame.header.corr);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                key,
                Pending {
                    connection_generation: connection.generation,
                    sender,
                },
            );
        let mut guard = PendingGuard {
            state: Arc::clone(self),
            key,
            connection_generation: connection.generation,
            active: true,
        };
        if connection.writer.send(frame).await.is_err() {
            self.invalidate_connection(connection.generation).await;
            return Err(ClientFailure::Transport);
        }
        let result = receiver.await.unwrap_or(Err(ClientFailure::Transport));
        guard.active = false;
        result
    }

    async fn call_once(self: &Arc<Self>, body: Vec<u8>) -> Result<Frame, ClientFailure> {
        let connection = self.connection().await?;
        let route = self.route(&connection).await?;
        let corr = self.next_corr();
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            route.route.channel,
            route.route.epoch,
            corr,
            body,
        )
        .map_err(|_| ClientFailure::Protocol)?;
        let response = self.request(&connection, frame).await?;
        if response.header.ty == FrameType::Error {
            let error = classify_error_frame(&response.body);
            if error == ClientFailure::RouteGone {
                self.invalidate_route(route).await;
            }
            return Err(error);
        }
        Ok(response)
    }

    async fn call(self: &Arc<Self>, body: Vec<u8>) -> Result<Frame, ClientFailure> {
        let outcome = tokio::time::timeout(self.request_timeout, async {
            let mut last = ClientFailure::Transport;
            for attempt in 0..2 {
                match self.call_once(body.clone()).await {
                    Ok(frame) => return Ok(frame),
                    Err(error @ (ClientFailure::Transport | ClientFailure::RouteGone))
                        if attempt == 0 =>
                    {
                        last = error;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last)
        })
        .await;

        match outcome {
            Ok(result) => result,
            Err(_elapsed) => {
                // THE CONNECTION DID NOT ANSWER, SO IT MUST NOT BE KEPT.
                //
                // Every other way this client drops a connection needs the
                // socket to SAY something: the reader sees EOF or an error, or a
                // write fails. A socket killed while the host slept says
                // nothing -- writes are accepted into the void, no FIN arrives,
                // and the reader stays blocked forever. Silence is the only
                // symptom, and a timeout is the only place it surfaces.
                //
                // Leaving it installed made every later call reuse it, spend the
                // full budget, and fail, so all vault-served lanes stayed dark
                // until the process restarted. That is the shape of insula#8 and
                // of a fleet-wide wake, which kills every connection at once
                // without closing any.
                //
                // Discarding a connection that was merely SLOW costs one
                // handshake, and `call` already retries a transport failure
                // once. That asymmetry is the whole argument: a needless
                // reconnect is a few milliseconds, a retained dead connection is
                // an outage that ends only with a restart.
                self.invalidate_current_connection().await;
                Err(ClientFailure::Transport)
            }
        }
    }

    /// Drop whichever connection is installed right now.
    ///
    /// Separate from `invalidate_connection`, which fences on a generation so a
    /// late failure cannot evict the connection that replaced it. Here there is
    /// no generation to fence on: the timeout fires OUTSIDE the attempt, so the
    /// caller cannot know which connection was in play, and the danger is
    /// keeping a dead one rather than dropping a live one.
    async fn invalidate_current_connection(self: &Arc<Self>) {
        let generation = {
            let state = self.connection.lock().await;
            match &*state {
                ConnectionState::Ready(connection) => Some(connection.generation),
                ConnectionState::Connecting { generation, .. } => Some(*generation),
                ConnectionState::Empty { .. } => None,
            }
        };
        if let Some(generation) = generation {
            self.invalidate_connection(generation).await;
        }
    }
}

struct PendingGuard {
    state: Arc<ClientState>,
    key: (u32, u64),
    connection_generation: u64,
    active: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut pending = self
            .state
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending
            .get(&self.key)
            .is_some_and(|pending| pending.connection_generation == self.connection_generation)
        {
            pending.remove(&self.key);
        }
    }
}

/// Process-wide credential source. It connects lazily so a missing vault module
/// cannot stop ck-quota from registering and serving health.
pub struct VaultClient {
    state: Arc<ClientState>,
}

impl VaultClient {
    /// Frames the reader loop discarded because no caller was waiting.
    ///
    /// See [`ClientState::dispatch`] for why both drop classes are counted
    /// rather than merely being correct.
    pub fn unmatched_terminal_drops(&self) -> u64 {
        self.state.unmatched_terminal_drops()
    }

    /// Frames discarded because their caller belonged to an older connection.
    /// See [`ClientState::connections_established`].
    pub fn connections_established(&self) -> u64 {
        self.state.connections_established()
    }

    pub fn stale_generation_drops(&self) -> u64 {
        self.state.stale_generation_drops()
    }

    pub fn new(connection_file_path: impl Into<PathBuf>) -> Self {
        Self {
            state: Arc::new(ClientState::new(connection_file_path.into())),
        }
    }

    #[cfg(test)]
    fn with_timeout(connection_file_path: impl Into<PathBuf>, request_timeout: Duration) -> Self {
        Self {
            state: Arc::new(ClientState::with_timeout(
                connection_file_path.into(),
                request_timeout,
            )),
        }
    }
}

impl std::fmt::Debug for VaultClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VaultClient")
            .field("target", &CREDENTIALS_MODULE_ID)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct GetRequest<'a> {
    method: &'static str,
    params: GetParams<'a>,
}

#[derive(Serialize)]
struct GetParams<'a> {
    handle: &'a str,
    min_ttl_ms: u64,
}

#[derive(Serialize)]
struct ReportRequest<'a> {
    method: &'static str,
    params: ReportParams<'a>,
}

#[derive(Serialize)]
struct ReportParams<'a> {
    handle: &'a str,
    provider_status: u16,
    record_version: u64,
}

#[derive(Serialize)]
struct StatusRequest<'a> {
    method: &'static str,
    params: StatusParams<'a>,
}

#[derive(Serialize)]
struct StatusParams<'a> {
    handle: &'a str,
}

#[derive(Deserialize)]
struct VaultSuccessResult {
    payload: Vec<u8>,
    #[serde(default)]
    expires_at_ms: Option<i64>,
    record_version: u64,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
}

#[derive(Deserialize)]
struct VaultStatusResult {
    ready: bool,
    #[serde(default)]
    record_version: Option<u64>,
    #[serde(default)]
    stale_pending: Option<bool>,
}

#[derive(Deserialize)]
struct VaultErrorResult {
    error: VaultReadError,
}

#[derive(Deserialize)]
struct VaultReadError {
    class: String,
    /// The vault's finer-grained reason, where it sends one.
    ///
    /// The class decides retry behaviour and is the only field to branch on for
    /// that. The code exists so a failure can be *described* accurately when two
    /// situations share a class -- which they do here: a record that was never
    /// created and one the vault has quarantined as corrupt are both permanent,
    /// and an operator's next step differs completely between them.
    #[serde(default)]
    code: Option<String>,
}

/// Map a vault failure onto the outcome this module reports.
///
/// Retry behaviour follows the **class** alone, per the agreed contract. The
/// code refines only how the outcome is described, and an unrecognised one
/// falls back to the class -- so a vault that adds a code cannot change how this
/// module retries, and a code that stops being sent degrades to the plain class
/// rather than to a wrong one.
fn read_error_to_outcome(class: &str, code: Option<&str>) -> VaultGetError {
    // A quarantined record is permanent like an absent one, and needs the
    // opposite response: absent means nobody has logged in, corrupt means a
    // record exists and the vault refuses to serve it. Reported distinctly so
    // the first sighting -- which will be the day something went wrong -- is not
    // rendered as "never configured".
    if class == "permanent" && code == Some("corrupt") {
        return VaultGetError::Corrupt;
    }

    // No credential exists for this handle. Distinguished from the general
    // permanent answer because the remedies are opposite: a corrupt or
    // unavailable record is something an operator repairs, while this one means
    // the handle names a credential that is gone, and the only fix is to stop
    // configuring it.
    //
    // BRANCHING ON THE CLASS IS LOAD-BEARING, not defensive. The vault produces
    // this code only on a clean zero-row lookup -- a store it cannot read maps
    // to a transient class, and a vault that is down returns no body at all --
    // so the pair cannot occur during an outage. Matching the CODE alone would
    // drop that guarantee the moment any producer arm hands out the same code
    // in a transient context, and callers may act on this answer rather than
    // merely report it.
    if class == "permanent" && code == Some("not_found") {
        return VaultGetError::NotFound;
    }

    match class {
        "transient" => VaultGetError::Transient,
        "auth_required" => VaultGetError::AuthRequired,
        "permanent" => VaultGetError::Permanent,
        "context_overflow" => VaultGetError::FailClosed,
        _ => VaultGetError::FailClosed,
    }
}

/// Map a control-plane error frame onto a retry policy.
///
/// THE CODES ARE READ FROM `subc_protocol::error_codes` RATHER THAN SPELLED AS
/// literals. A literal agrees only with itself: when the daemon added
/// `module_warming` it became a code this match had never heard of, and nothing
/// here or upstream could notice, because a string that no arm matches is
/// indistinguishable from a string that does not exist.
///
/// THE FALLTHROUGH IS THE DANGEROUS ARM, not the matched ones. An unrecognised
/// code lands on `Protocol` -> `FailClosed` -> `FetchError::Decode`, which is
/// non-transient: a flat 300s before the lane is asked again, and an
/// `errorClass` of `decode_failed` on the wire, which tells a reader the vault
/// sent an unparseable payload and sends them to this repo hunting a parse bug.
/// That is how a router's "try again in a moment" became a five-minute outage
/// with a misleading cause, reported as insula#14.
fn classify_error_frame(body: &[u8]) -> ClientFailure {
    use subc_protocol::error_codes as codes;

    let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    match value.get("code").and_then(Value::as_str) {
        // The target is not there. Retrying is right for a module that is coming
        // back and futile for a name that will never resolve, and this arm cannot
        // tell them apart -- so it retries and NAMES THE ID, because the futile
        // case is otherwise silent forever.
        //
        // The per-call retry is bounded; the refresher's is not. A transient
        // failure with a prior healthy window serves that window and tries again
        // next tick, indefinitely -- so a wrong or retired id publishes a
        // healthy-looking wire forever. This client hand-rolls its frame loop
        // rather than using an SDK, and the SDKs are where the
        // retry-budget-exhausted error that names the target lives, so without
        // this warning the id is printed nowhere in this process.
        Some(code @ ("unknown_channel" | codes::UNKNOWN_MODULE | codes::MODULE_REMOVED)) => {
            eprintln!(
                "{LOG_TAG} warning: daemon answered {code:?} for module id \
                 {CREDENTIALS_MODULE_ID:?} -- restarting, renamed, or removed from the config?"
            );
            ClientFailure::RouteGone
        }
        // The target exists and is not ready yet. `module_warming` is emphatically
        // NOT a failure: the daemon is telling us the module is mid-handshake and
        // to come back. On a daemon restart the lanes race the vault's own
        // registration, and whichever fires first can lose it by a millisecond --
        // so treating this as anything but a next-tick retry converts routine
        // startup ordering into a per-lane outage.
        Some(codes::MODULE_RELOADING | codes::MODULE_WARMING) => ClientFailure::RouteGone,
        Some(codes::TARGET_UNAVAILABLE | codes::MODULE_TIMEOUT | "backend_error") => {
            ClientFailure::Transport
        }
        _ => {
            // The code sits beside the class wherever the VAULT sends one, so it
            // is read from the same object rather than defaulted -- otherwise a
            // quarantined record reaching this path would render as an absent one.
            let code = value
                .get("code")
                .or_else(|| value.pointer("/error/code"))
                .and_then(Value::as_str);
            match value
                .get("class")
                .or_else(|| value.pointer("/error/class"))
                .and_then(Value::as_str)
            {
                Some(class) => ClientFailure::Classified(read_error_to_outcome(class, code)),
                // NEITHER a class NOR a code: this is not an error frame this
                // client can read at all, which is a different claim from one it
                // reads and does not recognise. Fail closed -- refusing to trust
                // an unreadable reply is right, and there is no route condition
                // here to be lenient about.
                None if code.is_none() => ClientFailure::Protocol,
                None => {
                    // NAME THE CODE WE ARE ABOUT TO DISCARD. The frame carried a
                    // distinction and this arm destroys it; without this line the
                    // only trace is the word `FailClosed`, and reconstructing
                    // insula#14 took the daemon's journal plus the vault module's
                    // audit log as a negative control. One line here would have
                    // made it a one-line read.
                    //
                    // A code with no vault `class` is a ROUTE-LAYER reply this
                    // match has not been taught, which in practice means the
                    // daemon grew a code since this was written.
                    eprintln!(
                        "{LOG_TAG} warning: unclassified control error code {:?} from the \
                         daemon -- retrying next tick and serving the cached window. If \
                         this condition is permanent it will repeat forever while the wire \
                         reads healthy; give it an arm in classify_error_frame.",
                        code.unwrap_or("<absent>")
                    );
                    ClientFailure::UnknownRouteCode
                }
            }
        }
    }
}

/// Decode a `credential.get` reply.
///
/// **The field names here are not proven by any test in this crate.** The
/// loopback fixtures construct the reply themselves, so they speak whatever
/// spelling this function reads and would pass with any of them: a rename on
/// either side leaves the suite green and breaks production. What actually
/// proves them is the `vault-lanes` example, which dials the live vault and
/// reports how many handles each provider is serving — a lane that decodes
/// nothing reports zero and the example exits non-zero.
///
/// The general shape, worth recognising elsewhere: a test that builds both ends
/// of a contract certifies the contract against itself. It is a real test of
/// this function's LOGIC — which branches it takes, what it does with an empty
/// payload — and no evidence at all about the NAMES, because the names are the
/// half the fixture supplies rather than checks.
///
/// **`vault-lanes` proves these names only for the DEPLOYED binary.** It dials
/// the running daemon through its connection file, so it exercises whatever was
/// last installed, not the working tree. Renaming a wire field here and running
/// it reports `findings: none` — verified by doing exactly that — because the
/// edit is not in the process being asked. That does not weaken the check, it
/// names its subject: the names are proven for the deployment, and a
/// working-tree change is covered only once deployed, which makes
/// deploy-then-check the order that carries evidence.
fn decode_get_response(body: &[u8]) -> Result<VaultCredential, VaultGetError> {
    let response: Value = serde_json::from_slice(body).map_err(|_| VaultGetError::FailClosed)?;
    let result = response
        .get("result")
        .and_then(Value::as_object)
        .ok_or(VaultGetError::FailClosed)?;
    let has_error = result.contains_key("error");
    let has_success = [
        "payload",
        "expires_at_ms",
        "record_version",
        "account_id",
        "project_id",
        "email",
        "org_name",
    ]
    .iter()
    .any(|field| result.contains_key(*field));
    match (has_success, has_error) {
        (true, false) => {
            let success: VaultSuccessResult = serde_json::from_value(Value::Object(result.clone()))
                .map_err(|_| VaultGetError::FailClosed)?;
            // A success reply carrying no credential bytes is not a credential.
            // Every lane converts the payload straight into a bearer, and an
            // empty one converts cleanly into an empty bearer -- so without this
            // the request goes out unauthenticated and the upstream answers 401,
            // which reads as a dead session and sends whoever investigates to
            // re-authenticate an account whose credential was never sent.
            //
            // Reported as its own class rather than as a rejected reply: this one
            // parsed correctly, so the fault is in the record, and the remedy
            // differs from both an absent credential and a malformed response.
            if success.payload.is_empty() {
                return Err(VaultGetError::EmptyPayload);
            }
            Ok(VaultCredential {
                payload: success.payload,
                expires_at_ms: success.expires_at_ms,
                record_version: success.record_version,
                account_id: success
                    .account_id
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                project_id: success
                    .project_id
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                email: success
                    .email
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                org_name: success
                    .org_name
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
            })
        }
        (false, true) => {
            let failure: VaultErrorResult = serde_json::from_value(Value::Object(result.clone()))
                .map_err(|_| VaultGetError::FailClosed)?;
            Err(read_error_to_outcome(
                &failure.error.class,
                failure.error.code.as_deref(),
            ))
        }
        (true, true) | (false, false) => Err(VaultGetError::FailClosed),
    }
}

/// Decode a `credential.status` reply.
///
/// Same two-level shape as [`decode_get_response`]: a `result` object is either
/// a success body or an `error` body, never both, never neither. Extra wire
/// fields (`last_error_code`, `lease_held`) are ignored -- this poll only acts
/// on `ready` / `record_version` / `stale_pending`.
///
/// A decode failure here is returned to the caller. It must not be fed into
/// slot transition: status is an accelerator, and an accelerator that can
/// condemn a credential is a defect.
fn decode_status_response(body: &[u8]) -> Result<CredentialStatus, VaultGetError> {
    let response: Value = serde_json::from_slice(body).map_err(|_| VaultGetError::FailClosed)?;
    let result = response
        .get("result")
        .and_then(Value::as_object)
        .ok_or(VaultGetError::FailClosed)?;
    let has_error = result.contains_key("error");
    let has_success = ["ready", "record_version", "stale_pending"]
        .iter()
        .any(|field| result.contains_key(*field));
    match (has_success, has_error) {
        (true, false) => {
            let success: VaultStatusResult = serde_json::from_value(Value::Object(result.clone()))
                .map_err(|_| VaultGetError::FailClosed)?;
            Ok(CredentialStatus {
                ready: success.ready,
                record_version: success.record_version,
                stale_pending: success.stale_pending,
            })
        }
        (false, true) => {
            let failure: VaultErrorResult = serde_json::from_value(Value::Object(result.clone()))
                .map_err(|_| VaultGetError::FailClosed)?;
            Err(read_error_to_outcome(
                &failure.error.class,
                failure.error.code.as_deref(),
            ))
        }
        (true, true) | (false, false) => Err(VaultGetError::FailClosed),
    }
}

#[async_trait]
impl CredentialSource for VaultClient {
    async fn get(
        &self,
        capability: &VaultCapability,
        min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError> {
        let body = serde_json::to_vec(&GetRequest {
            method: "credential.get",
            params: GetParams {
                handle: capability.expose_secret(),
                min_ttl_ms,
            },
        })
        .map_err(|_| VaultGetError::FailClosed)?;
        let frame = self
            .state
            .call(body)
            .await
            .map_err(ClientFailure::vault_error)?;
        decode_get_response(&frame.body)
    }

    /// Tell the vault a served credential was refused upstream.
    ///
    /// **The least-proven shape this module emits.** Every other emitted shape
    /// has a receiver that accepts or refuses it continuously: the `usage.get`
    /// reply is decoded by consumers on every poll, the `credential.get` request
    /// is honoured by the vault whenever a lane serves, and the health report is
    /// rendered by the daemon. Those are proven by use.
    ///
    /// This one is sent only when an upstream returns 401 or 403, which on a
    /// healthy host is never. So a rename or a shape change here is not caught
    /// by anything running today — it surfaces on the first real auth failure,
    /// which is the worst moment to discover the report does not arrive, since
    /// that report is what invalidates a static API key that has no refresh
    /// adapter to notice for itself.
    ///
    /// It is not testable by sending one: a synthetic report would mutate the
    /// vault's record state. The bounded gap is stated here rather than closed,
    /// and the acceptance evidence is the absence of the warning below during a
    /// genuine failure.
    async fn report_auth_failure(
        &self,
        capability: &VaultCapability,
        provider_status: u16,
        record_version: u64,
    ) {
        let result: Result<(), VaultGetError> = async {
            let body = serde_json::to_vec(&ReportRequest {
                method: "credential.report_auth_failure",
                params: ReportParams {
                    handle: capability.expose_secret(),
                    provider_status,
                    record_version,
                },
            })
            .map_err(|_| VaultGetError::FailClosed)?;
            let frame = self
                .state
                .call(body)
                .await
                .map_err(ClientFailure::vault_error)?;
            let response: Value =
                serde_json::from_slice(&frame.body).map_err(|_| VaultGetError::FailClosed)?;
            if let Some(class) = response
                .pointer("/result/error/class")
                .and_then(Value::as_str)
            {
                let code = response
                    .pointer("/result/error/code")
                    .and_then(Value::as_str);
                return Err(read_error_to_outcome(class, code));
            }
            response
                .get("result")
                .filter(|result| result.is_object())
                .map(|_| ())
                .ok_or(VaultGetError::FailClosed)
        }
        .await;
        if let Err(error) = result {
            eprintln!("{LOG_TAG} warning: vault auth-failure report failed class={error:?}");
        }
    }

    async fn status(
        &self,
        capability: &VaultCapability,
    ) -> Result<CredentialStatus, VaultGetError> {
        let body = serde_json::to_vec(&StatusRequest {
            method: "credential.status",
            params: StatusParams {
                handle: capability.expose_secret(),
            },
        })
        .map_err(|_| VaultGetError::FailClosed)?;
        let frame = self
            .state
            .call(body)
            .await
            .map_err(ClientFailure::vault_error)?;
        decode_status_response(&frame.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn classify(code: &str) -> ClientFailure {
        classify_error_frame(format!(r#"{{"code":"{code}"}}"#).as_bytes())
    }

    /// A daemon restart must not cost a vault lane a non-transient backoff.
    ///
    /// On restart the lanes race the vault module's own registration, and the
    /// first to fire can lose by a millisecond. The daemon answers
    /// `module_warming` -- "it exists, it is mid-handshake, come back" -- which
    /// this classifier did not recognise, so it fell through to fail-closed and
    /// became `FetchError::Decode`: 300 seconds before the lane was asked again,
    /// published as `decode_failed`, which accuses the vault of sending an
    /// unparseable payload. Measured at 10 consecutive samples over 5 minutes on
    /// the `claude` lane, insula#14.
    ///
    /// Asserted through `vault_error()` rather than on the ClientFailure variant,
    /// because the variant is not what costs the 300s -- the mapping onto
    /// `Transient` is, and a refactor that renamed the variant while changing
    /// that mapping would keep a variant-level assertion green.
    #[test]
    fn a_warming_module_is_retried_rather_than_failed_closed() {
        assert_eq!(
            classify(subc_protocol::error_codes::MODULE_WARMING).vault_error(),
            VaultGetError::Transient,
            "a warming target is the textbook retry-next-tick case; failing closed \
             converts routine startup ordering into a 300s per-lane outage"
        );
    }

    /// An unrecognised route-layer reply is retried, not treated as a bad credential.
    ///
    /// The vault states its verdict in `class`. A frame carrying a code and no
    /// class is the ROUTE speaking, and no route condition is evidence that a
    /// credential is bad -- so failing closed on one spends 300 seconds and
    /// replaces a healthy cached window with `decode_failed`, accusing the vault
    /// of sending an unparseable payload, on the strength of a code this client
    /// has merely not read yet.
    ///
    /// That is `module_warming` exactly (insula#14), and naming that one code did
    /// not fix the class: the exhaustiveness test above cannot see a seventh code
    /// added upstream, so the next one lands here. This is the vault's own
    /// TypeScript client's default, reached independently from the other side of
    /// the wire.
    ///
    /// A frame this client cannot READ at all is still fail-closed -- that is a
    /// different claim, and `ClientFailure::Protocol` keeps it.
    #[test]
    fn an_unrecognised_route_code_is_retried_rather_than_blamed_on_the_credential() {
        assert_eq!(
            classify("a_code_the_daemon_grew_last_week").vault_error(),
            VaultGetError::Transient,
            "a route-layer code we have not been taught must cost one tick, not an outage"
        );

        // The other half, and the reason this is a new variant rather than a
        // widened `Protocol`: an unreadable body is not a route condition, and
        // must still refuse to trust the reply.
        assert_eq!(
            classify_error_frame(b"not json at all").vault_error(),
            VaultGetError::FailClosed,
            "a reply this client cannot read is a different claim from one it can \
             read and does not recognise"
        );
    }

    /// Every control error code the protocol crate exports is classified here.
    ///
    /// THE FALLTHROUGH IS SILENT AND EXPENSIVE: an unrecognised code lands on
    /// fail-closed, which is non-transient, so a code this match has not been
    /// taught costs a lane 300 seconds and publishes `decode_failed` -- a cause
    /// that sends a reader to this repository looking for a parse bug. That is
    /// exactly how `module_warming` cost five minutes per daemon restart while
    /// every surface here read as a vault problem.
    ///
    /// WHAT THIS TEST CANNOT DO, stated because the gap is why the fallthrough's
    /// own behaviour had to change: Rust has no reflection over a module's
    /// constants, so this list is written by hand and pins the six that exist
    /// today. A SEVENTH CODE ADDED UPSTREAM STILL REACHES THE FALLTHROUGH and
    /// this test still passes. The mitigations are both there rather than here --
    /// the fallthrough retries instead of failing closed, so an unknown code
    /// costs one tick rather than an outage, and it logs the code, so the next
    /// one is a single line rather than a reconstruction from the daemon journal.
    #[test]
    fn every_exported_control_error_code_has_an_arm() {
        use subc_protocol::error_codes as codes;
        let exported = [
            codes::UNKNOWN_MODULE,
            codes::MODULE_REMOVED,
            codes::MODULE_RELOADING,
            codes::MODULE_WARMING,
            codes::TARGET_UNAVAILABLE,
            codes::MODULE_TIMEOUT,
        ];

        let unclassified: Vec<&str> = exported
            .iter()
            .copied()
            .filter(|code| classify(code) == ClientFailure::UnknownRouteCode)
            .collect();

        assert!(
            unclassified.is_empty(),
            "these daemon error codes reach the fallthrough rather than a named arm, \
             so this client is guessing at their retry policy: {unclassified:?}"
        );

        // THE CONTROL, and it is what keeps the filter above from being vacuous.
        // The filter looks for codes landing on `UnknownRouteCode`; if the
        // fallthrough ever stopped producing that variant, every code would pass
        // trivially and this test would certify a classifier that classifies
        // nothing. This pins the fallthrough it measures against.
        assert_eq!(
            classify("a_code_no_daemon_sends"),
            ClientFailure::UnknownRouteCode,
            "an unrecognised code must reach the fallthrough for the filter above to mean anything"
        );
    }

    async fn loopback_listener(label: &str) -> (TcpListener, PathBuf, Vec<u8>, [u8; 16]) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let key = vec![0x5a; 32];
        let daemon_id = [0x31; 16];
        let path = std::env::temp_dir().join(format!(
            "ck-quota-vault-client-{label}-{}-{}.json",
            std::process::id(),
            NEXT_LOOPBACK_ID.fetch_add(1, Ordering::Relaxed)
        ));
        connection_file::write_atomic(
            &path,
            &connection_file::ConnectionInfo {
                schema: connection_file::SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![connection_file::Endpoint {
                    host: std::net::Ipv4Addr::LOCALHOST.to_string(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "vault-loopback-test".to_string(),
            },
        )
        .unwrap();
        (listener, path, key, daemon_id)
    }

    static NEXT_LOOPBACK_ID: AtomicU64 = AtomicU64::new(0);

    async fn accept_authenticated(
        listener: &TcpListener,
        key: &[u8],
        daemon_id: &[u8; 16],
    ) -> TcpStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        subc_transport::authenticate_server(
            &mut stream,
            key,
            daemon_id,
            "vault-loopback-test",
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        stream
    }

    async fn reply_route(stream: &mut TcpStream, channel: u16, epoch: u32) {
        let request = read_frame(stream).await.unwrap().unwrap();
        assert_eq!(request.header.ty, FrameType::Request);
        assert_eq!(request.header.channel, 0);
        let response = Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            request.header.corr,
            serde_json::to_vec(&serde_json::json!({
                "route_channel": channel,
                "route_epoch": epoch
            }))
            .unwrap(),
        )
        .unwrap();
        write_frame(stream, &response).await.unwrap();
    }

    fn get_success(request: &Frame, payload: &[u8]) -> Frame {
        Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Interactive, false),
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            serde_json::to_vec(&serde_json::json!({
                "result": {
                    "payload": payload,
                    "record_version": 1,
                    "account_id": "loopback-account"
                }
            }))
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn class_mapping_is_exhaustive_and_unknown_fails_closed() {
        let map = |class| read_error_to_outcome(class, None);
        assert_eq!(map("transient"), VaultGetError::Transient);
        assert_eq!(map("auth_required"), VaultGetError::AuthRequired);
        assert_eq!(map("permanent"), VaultGetError::Permanent);
        assert_eq!(map("context_overflow"), VaultGetError::FailClosed);
        assert_eq!(map("future_class"), VaultGetError::FailClosed);
    }

    /// A quarantined record is told apart from one that never existed.
    ///
    /// Both are permanent, so retry behaviour is identical and the class alone
    /// cannot separate them -- but the operator's next step is opposite: an
    /// absent credential is created by logging in, while a corrupt one already
    /// exists and something damaged it. The vault sends a finer-grained code
    /// beside the class, and reading only the class discards exactly that.
    /// A transient class keeps its class whatever code rides with it.
    ///
    /// `not_found` is the one code a caller may ACT on rather than merely
    /// report -- a handle naming a removed credential is a handle to stop
    /// configuring. That is safe only because the vault emits it on a clean
    /// zero-row lookup, while a store it cannot read maps to a transient class
    /// instead. Matching the code alone would discard that guarantee and let a
    /// future producer arm turn an outage into a reap.
    #[test]
    fn a_transient_class_is_never_read_as_a_missing_credential() {
        assert_eq!(
            read_error_to_outcome("transient", Some("not_found")),
            VaultGetError::Transient,
            "a transient answer must not be read as a permanently missing credential"
        );
        assert_eq!(
            read_error_to_outcome("auth_required", Some("not_found")),
            VaultGetError::AuthRequired
        );
    }

    #[test]
    fn a_quarantined_record_is_not_reported_as_an_absent_one() {
        assert_eq!(
            read_error_to_outcome("permanent", Some("corrupt")),
            VaultGetError::Corrupt
        );
        assert_eq!(
            read_error_to_outcome("permanent", Some("not_found")),
            VaultGetError::NotFound
        );

        // Retry behaviour is unchanged: the code refines the description only,
        // and both remain non-transient.
        for outcome in [VaultGetError::Corrupt, VaultGetError::NotFound] {
            assert_eq!(
                quota_core::refresh::classify(
                    &quota_core::provider::FetchAttempt::unverified_vault_failure(outcome)
                        .usage
                        .unwrap_err()
                ),
                quota_core::refresh::FetchClass::NonTransient,
            );
        }

        // An unrecognised code falls back to the class rather than to a wrong
        // outcome, so a vault adding one cannot change how this module behaves.
        assert_eq!(
            read_error_to_outcome("permanent", Some("a_code_from_a_newer_vault")),
            VaultGetError::Permanent
        );
        // And the code is only consulted for the class it belongs to.
        assert_eq!(
            read_error_to_outcome("transient", Some("corrupt")),
            VaultGetError::Transient
        );
    }

    /// The code is read from the wire, not just from the mapping function.
    ///
    /// The decode and the mapping are separate steps, so a mapping that handles
    /// the code correctly still reports the wrong outcome if the decoder drops
    /// the field before it gets there.
    #[test]
    fn a_quarantined_record_survives_the_decode() {
        let body = serde_json::json!({
            "result": { "error": { "class": "permanent", "code": "corrupt" } }
        });
        assert!(matches!(
            decode_get_response(&serde_json::to_vec(&body).unwrap()),
            Err(VaultGetError::Corrupt)
        ));

        // The pair a caller may ACT on, asserted through the real body rather
        // than by calling the classifier with two strings. This is the shape a
        // dead handle produces -- one naming a credential the vault no longer
        // holds -- and the class half is what makes it safe to act on, since a
        // vault that cannot read its store answers transient instead. Testing
        // the classifier alone leaves the extraction untested, so a decoder that
        // stopped reading either field would keep every classifier test green.
        let body = serde_json::json!({
            "result": { "error": { "class": "permanent", "code": "not_found" } }
        });
        assert_eq!(
            decode_get_response(&serde_json::to_vec(&body).unwrap()),
            Err(VaultGetError::NotFound)
        );

        // Not vacuous: the same shape without the code still decodes, as the
        // plain permanent outcome.
        let body = serde_json::json!({
            "result": { "error": { "class": "permanent" } }
        });
        assert!(matches!(
            decode_get_response(&serde_json::to_vec(&body).unwrap()),
            Err(VaultGetError::Permanent)
        ));
    }

    #[test]
    fn i1_mixed_success_and_error_result_fails_closed() {
        let body = serde_json::json!({
            "result": {
                "payload": b"must-not-be-served",
                "record_version": 12,
                "error": {"code": "needs_reauth", "class": "auth_required"}
            }
        });
        assert_eq!(
            decode_get_response(&serde_json::to_vec(&body).unwrap()).unwrap_err(),
            VaultGetError::FailClosed
        );
    }

    #[test]
    fn status_decode_reads_ready_version_and_stale_pending() {
        let body = serde_json::json!({
            "result": {
                "ready": true,
                "record_version": 12,
                "stale_pending": false,
                "last_error_code": null,
                "lease_held": true
            }
        });
        assert_eq!(
            decode_status_response(&serde_json::to_vec(&body).unwrap()).unwrap(),
            CredentialStatus {
                ready: true,
                record_version: Some(12),
                stale_pending: Some(false),
            }
        );
    }

    #[test]
    fn status_decode_omitted_version_is_absent_not_zero() {
        let body = serde_json::json!({
            "result": {
                "ready": false,
                "last_error_code": null,
                "lease_held": false
            }
        });
        let status = decode_status_response(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(!status.ready);
        assert_eq!(status.record_version, None);
        assert_eq!(status.stale_pending, None);
    }

    #[test]
    fn status_decode_classifies_errors_the_same_way_as_get() {
        let body = serde_json::json!({
            "result": { "error": { "class": "permanent", "code": "not_found" } }
        });
        assert_eq!(
            decode_status_response(&serde_json::to_vec(&body).unwrap()),
            Err(VaultGetError::NotFound)
        );
    }

    #[test]
    fn status_decode_mixed_success_and_error_fails_closed() {
        let body = serde_json::json!({
            "result": {
                "ready": true,
                "record_version": 4,
                "error": { "class": "permanent", "code": "not_found" }
            }
        });
        assert_eq!(
            decode_status_response(&serde_json::to_vec(&body).unwrap()).unwrap_err(),
            VaultGetError::FailClosed
        );
    }

    #[test]
    fn i3_module_reloading_is_transient_route_availability() {
        for code in [
            "unknown_module",
            "target_unavailable",
            "module_reloading",
            "module_timeout",
            "backend_error",
        ] {
            let body = serde_json::to_vec(&serde_json::json!({
                "code": code,
                "message": "module is temporarily unavailable"
            }))
            .unwrap();
            assert_eq!(
                classify_error_frame(&body).vault_error(),
                VaultGetError::Transient,
                "availability code {code} became fail-closed"
            );
        }
        // THIS EXPECTATION WAS DELIBERATELY REVERSED, and the reason is not
        // that the old one was thoughtless. It pinned a real judgement: a code
        // that is not a known availability code might be an AUTHORIZATION
        // condition, and retrying one of those forever is futile.
        //
        // What changed is the visibility of forever. When this was written, a
        // transient failure with a prior healthy window stale-served it with
        // nothing on the wire to say so, and "retry indefinitely" really did
        // mean a healthy-looking wire hiding a permanent condition. Since then
        // this module publishes `stale: { since, class }` on exactly those
        // entries, plus `staleEpisodesByProvider` -- so a code retried forever
        // now shows as a `since` that keeps receding and an episode count that
        // climbs. The wire is no longer silent, and the argument for failing
        // closed rested on it being silent.
        //
        // Against that, failing closed costs a real outage on the retryable
        // case with a cause that names the wrong subsystem, which is what
        // `module_warming` did for five minutes a restart (insula#14). The
        // reporter's vault TypeScript client reaches the same default from the
        // other side of the wire -- though the transfer is not automatic, since
        // a request/response client surfaces a repeated failure to its caller
        // while a background refresher does not, and only the staleness
        // disclosure closes that gap here.
        let unknown = serde_json::to_vec(&serde_json::json!({
            "code": "future_authorization_error",
            "message": "not an availability class"
        }))
        .unwrap();
        assert_eq!(
            classify_error_frame(&unknown).vault_error(),
            VaultGetError::Transient,
            "a route-layer code with no vault class is the ROUTE speaking, and no \
                 route condition is evidence a credential is bad"
        );
    }

    /// A call that times out must not leave the dead connection in place.
    ///
    /// THE HAZARD, which is why this is worth a loopback stub. This client holds
    /// ONE long-lived connection with no idle timeout and no maximum lifetime.
    /// It is dropped when the reader sees EOF or an error, and when a write
    /// fails -- all of which need the socket to SAY something. A socket killed
    /// while the host slept says nothing: writes are accepted into the void, no
    /// FIN arrives, and the reader stays blocked. The only symptom is that
    /// replies stop.
    ///
    /// If a timeout leaves the connection installed, every later call reuses it,
    /// costs the full request budget, and fails -- so every vault-served lane
    /// stays dark until the process restarts. That is the shape filed as
    /// insula#8 (`credential_unusable` sticky across a re-seal, cured only by a
    /// restart) and the shape a fleet-wide mass route-invalidation event
    /// produces: a host wake kills every connection at once without closing any.
    ///
    /// Observable is `connections_established`, which increments per connection:
    /// if the timed-out one was discarded the second call builds a new one, and
    /// if it was retained the second call reuses it and the count stays at 1.
    #[tokio::test]
    async fn a_timed_out_call_discards_the_connection_it_could_not_reach() {
        let (listener, path, key, daemon_id) = loopback_listener("wedged").await;
        // A daemon that accepts, completes the handshake and the route, and then
        // never answers -- the observable shape of a connection that died
        // without closing. It accepts twice, so a client that DOES reconnect is
        // not blocked by the stub.
        let server = tokio::spawn(async move {
            let mut accepted = 0usize;
            for _ in 0..2 {
                let mut stream = accept_authenticated(&listener, &key, &daemon_id).await;
                reply_route(&mut stream, 7, 3).await;
                let _ = read_frame(&mut stream).await;
                accepted += 1;
                // Hold the socket open: closing it would invalidate the
                // connection through the EOF path and prove nothing about the
                // timeout path this test exists for.
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            accepted
        });
        let client = VaultClient::with_timeout(&path, Duration::from_millis(600));

        let first = client
            .get(&VaultCapability::new("ckh_wedged"), 120_000)
            .await;
        assert_eq!(first.unwrap_err(), VaultGetError::Transient);
        assert_eq!(
            client.state.connections_established(),
            1,
            "precondition: exactly one connection was built for the first call"
        );

        let second = client
            .get(&VaultCapability::new("ckh_wedged"), 120_000)
            .await;
        assert_eq!(second.unwrap_err(), VaultGetError::Transient);
        assert_eq!(
            client.state.connections_established(),
            2,
            "the second call must build a NEW connection: reusing one that did \
             not answer wedges every vault lane until the process restarts"
        );

        server.abort();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn i9_public_client_timeout_deregisters_and_discards_late_response() {
        let (listener, path, key, daemon_id) = loopback_listener("timeout").await;
        // The client timeout must fire strictly BETWEEN the request being sent
        // and the response arriving, on any runner speed: the 1s budget is
        // generous for loopback connect + auth + route.open (so the request is
        // always sent), while the response is delayed well past the timeout.
        // A tighter budget hangs slow runners: if the timeout expires before
        // the request is even written, the server waits on a request that
        // never comes and `server.await` never returns.
        let server = tokio::spawn(async move {
            let mut stream = accept_authenticated(&listener, &key, &daemon_id).await;
            reply_route(&mut stream, 7, 3).await;
            let request = read_frame(&mut stream).await.unwrap().unwrap();
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            let _ = write_frame(&mut stream, &get_success(&request, b"late-token")).await;
        });
        let client = VaultClient::with_timeout(&path, Duration::from_secs(1));

        let result = client
            .get(&VaultCapability::new("ckh_timeout"), 120_000)
            .await;
        assert_eq!(result.unwrap_err(), VaultGetError::Transient);
        // The caller's own pending entry is removed by its drop guard at
        // cancellation, but the detached single-flight route opener may still
        // hold a channel-0 entry on a slow runner until its response lands, so
        // deregistration is asserted as an eventually-empty map, not instant.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if client.state.pending.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pending entries were not deregistered after the request timeout"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Bounded: a stub stuck waiting for a request that was never sent must
        // fail the test, not wedge the whole test binary.
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("stub server did not finish: the get request never reached it")
            .unwrap();
        // The late credential.get response arrived after the timeout: it must
        // have been discarded without reviving or leaking a pending entry.
        assert!(client.state.pending.lock().unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn i9_public_client_connection_death_completes_waiters() {
        let (listener, path, key, daemon_id) = loopback_listener("connection-death").await;
        let server = tokio::spawn(async move {
            let mut stream = accept_authenticated(&listener, &key, &daemon_id).await;
            reply_route(&mut stream, 7, 3).await;
            let _ = read_frame(&mut stream).await;
            drop(stream);
            drop(listener);
        });
        let client = VaultClient::with_timeout(&path, Duration::from_millis(200));
        let first_capability = VaultCapability::new("ckh_dead_one");
        let second_capability = VaultCapability::new("ckh_dead_two");
        let first = client.get(&first_capability, 120_000);
        let second = client.get(&second_capability, 120_000);
        let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(first, second)
        })
        .await
        .expect("connection-death waiters hung");
        assert_eq!(first.unwrap_err(), VaultGetError::Transient);
        assert_eq!(second.unwrap_err(), VaultGetError::Transient);
        assert!(client.state.pending.lock().unwrap().is_empty());
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn i9_public_client_unknown_channel_reopens_route_on_same_connection() {
        let (listener, path, key, daemon_id) = loopback_listener("route-reopen").await;
        let accepts = Arc::new(AtomicU64::new(0));
        let server_accepts = Arc::clone(&accepts);
        let server = tokio::spawn(async move {
            let mut stream = accept_authenticated(&listener, &key, &daemon_id).await;
            server_accepts.fetch_add(1, Ordering::Relaxed);
            reply_route(&mut stream, 7, 3).await;
            let first = read_frame(&mut stream).await.unwrap().unwrap();
            let route_error = Frame::build(
                FrameType::Error,
                Flags::new(false, Priority::Interactive, false),
                first.header.channel,
                first.header.epoch,
                first.header.corr,
                serde_json::to_vec(&subc_protocol::ErrorBody::new(
                    "unknown_channel",
                    "route expired",
                ))
                .unwrap(),
            )
            .unwrap();
            write_frame(&mut stream, &route_error).await.unwrap();
            reply_route(&mut stream, 8, 4).await;
            let second = read_frame(&mut stream).await.unwrap().unwrap();
            write_frame(&mut stream, &get_success(&second, b"recovered-token"))
                .await
                .unwrap();
        });
        let client = VaultClient::with_timeout(&path, Duration::from_millis(500));

        let credential = client
            .get(&VaultCapability::new("ckh_route"), 120_000)
            .await
            .unwrap();
        assert_eq!(credential.payload, b"recovered-token");
        server.await.unwrap();
        assert_eq!(accepts.load(Ordering::Relaxed), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn two_level_reply_decode_normalizes_metadata_and_redacts_payload_debug() {
        let secret = b"vault-token-secret";
        let body = serde_json::json!({
            "result": {
                "payload": secret,
                "expires_at_ms": null,
                "record_version": 9,
                "account_id": "  acct-9  "
            }
        });
        let credential = decode_get_response(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(credential.payload, secret);
        assert_eq!(credential.account_id.as_deref(), Some("acct-9"));
        assert_eq!(credential.email, None);
        assert_eq!(credential.org_name, None);
        assert!(!format!("{credential:?}").contains("vault-token-secret"));
    }

    /// The credential store's reply field names are a cross-repository join:
    /// this struct names them, the vault daemon writes them, and nothing but a
    /// matching string connects the two.
    ///
    /// `account_id` is the load-bearing one. Every field here is optional, so a
    /// renamed or misspelled key parses cleanly and yields `None` -- and an
    /// identity-less handle collapses every account of that provider into a
    /// single unlabeled entry, because labeled entries are emitted only when
    /// all handles resolve an account. Multi-account visibility disappears with
    /// nothing failing anywhere.
    ///
    /// Kept separate from the normalization test below even though both parse a
    /// reply. That one is named for trimming and redaction, so narrowing it to
    /// its stated subject would take this assertion with it, and the person
    /// narrowing would have no reason to look for it.
    #[test]
    fn the_served_account_identity_survives_the_reply_field_names() {
        // Exactly the keys the credential store writes.
        let body = serde_json::json!({
            "result": {
                "payload": b"token",
                "record_version": 4,
                "account_id": "acct-live",
                "email": "person@example.test",
                "org_name": "Example Org"
            }
        });

        let credential = decode_get_response(&serde_json::to_vec(&body).unwrap()).unwrap();

        assert_eq!(
            credential.account_id.as_deref(),
            Some("acct-live"),
            "the identity field did not join: entries would collapse to one unlabeled row"
        );
        // Not vacuous: a decode that dropped every optional field would still
        // satisfy an assertion written only about absence.
        assert_eq!(credential.email.as_deref(), Some("person@example.test"));
        assert_eq!(credential.org_name.as_deref(), Some("Example Org"));
        assert_eq!(credential.record_version, 4);
    }

    #[test]
    fn get_result_decodes_optional_email_and_org_name() {
        let body = serde_json::json!({
            "result": {
                "payload": b"vault-token",
                "record_version": 10,
                "email": "  user@example.com ",
                "org_name": "  Example Org  "
            }
        });
        let credential = decode_get_response(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(credential.email.as_deref(), Some("user@example.com"));
        assert_eq!(credential.org_name.as_deref(), Some("Example Org"));
    }

    /// An empty credential is refused, and says so in its own words.
    ///
    /// Refusing it at all is the load-bearing part: an empty payload converts
    /// cleanly into an empty bearer, the request goes out unauthenticated, and
    /// the 401 that comes back reads as a dead session.
    ///
    /// The class it is refused under matters separately, because it is what
    /// someone investigating acts on. An absent credential is closed by logging
    /// in; an empty one means something wrote a value that should never have
    /// been writable, and the record is the evidence. Reporting it as a
    /// malformed reply would also mislead -- this reply parsed correctly.
    #[test]
    fn a_success_reply_with_an_empty_payload_is_refused_as_an_empty_credential() {
        // Well-formed in every other respect: the reply parses, carries a record
        // version, and resolves an account. Only the credential bytes are absent.
        let body = serde_json::json!({
            "result": {
                "payload": Vec::<u8>::new(),
                "record_version": 11,
                "account_id": "acct-11"
            }
        });
        let decoded = decode_get_response(&serde_json::to_vec(&body).unwrap());
        assert!(
            matches!(decoded, Err(VaultGetError::EmptyPayload)),
            "an empty payload must be refused as an empty credential, not served \
             as a bearer nor reported as a malformed reply"
        );

        // Non-vacuity: the identical reply with one byte of payload decodes, so
        // the rejection above is the emptiness and not the rest of the shape.
        let body = serde_json::json!({
            "result": {
                "payload": b"t",
                "record_version": 11,
                "account_id": "acct-11"
            }
        });
        assert!(decode_get_response(&serde_json::to_vec(&body).unwrap()).is_ok());
    }

    #[tokio::test]
    async fn eight_out_of_order_responses_keep_their_correlations() {
        let state = Arc::new(ClientState::new(PathBuf::from("unused")));
        let mut receivers = Vec::new();
        let mut guards = Vec::new();
        for corr in 1..=8 {
            let key = (19, corr);
            let (sender, receiver) = oneshot::channel();
            state.pending.lock().unwrap().insert(
                key,
                Pending {
                    connection_generation: 5,
                    sender,
                },
            );
            receivers.push((corr, receiver));
            guards.push(PendingGuard {
                state: Arc::clone(&state),
                key,
                connection_generation: 5,
                active: true,
            });
        }

        for corr in (1..=8).rev() {
            let frame = Frame::build(
                FrameType::Response,
                Flags::new(false, Priority::Interactive, false),
                7,
                19,
                corr,
                corr.to_string().into_bytes(),
            )
            .unwrap();
            state.dispatch(5, frame);
        }

        for (corr, receiver) in receivers {
            let frame = receiver.await.unwrap().unwrap();
            assert_eq!(frame.header.corr, corr);
            assert_eq!(frame.body, corr.to_string().as_bytes());
        }
        assert!(state.pending.lock().unwrap().is_empty());
        drop(guards);
    }

    #[test]
    fn cancelled_waiter_drop_guard_deregisters_pending_entry() {
        let state = Arc::new(ClientState::new(PathBuf::from("unused")));
        let (sender, _receiver) = oneshot::channel();
        state.pending.lock().unwrap().insert(
            (7, 11),
            Pending {
                connection_generation: 3,
                sender,
            },
        );
        {
            let _guard = PendingGuard {
                state: Arc::clone(&state),
                key: (7, 11),
                connection_generation: 3,
                active: true,
            };
        }
        assert!(state.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn route_gone_reopens_on_the_live_connection() {
        let state = Arc::new(ClientState::new(PathBuf::from("unused")));
        let (writer, mut outgoing) = mpsc::channel(4);
        let connection = Arc::new(LiveConnection {
            generation: 3,
            writer,
        });
        *state.connection.lock().await = ConnectionState::Ready(Arc::clone(&connection));
        let old_lease = RouteLease {
            route: Route {
                channel: 7,
                epoch: 20,
            },
            connection_generation: 3,
            route_generation: 5,
        };
        *state.route.lock().await = RouteState::Ready(old_lease);
        state.invalidate_route(old_lease).await;

        let responder_state = Arc::clone(&state);
        tokio::spawn(async move {
            let request = outgoing.recv().await.unwrap();
            let response = Frame::build(
                FrameType::Response,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&serde_json::json!({
                    "route_channel": 8,
                    "route_epoch": 21
                }))
                .unwrap(),
            )
            .unwrap();
            responder_state.dispatch(3, response);
        });

        let replacement = state.route(&connection).await.unwrap();
        assert_eq!(replacement.route.channel, 8);
        assert_eq!(replacement.route.epoch, 21);
        assert!(matches!(
            &*state.connection.lock().await,
            ConnectionState::Ready(current) if current.generation == 3
        ));
    }

    #[tokio::test]
    async fn connect_storm_is_single_flight_and_enters_cooldown() {
        let state = Arc::new(ClientState::new(PathBuf::from("missing-connection-file")));
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let state = Arc::clone(&state);
            calls.spawn(async move { state.connection().await });
        }
        while let Some(result) = calls.join_next().await {
            assert!(result.unwrap().is_err());
        }
        assert_eq!(state.next_connection_generation.load(Ordering::Relaxed), 2);
        assert!(matches!(
            &*state.connection.lock().await,
            ConnectionState::Empty {
                retry_after: Some(_)
            }
        ));
    }

    #[tokio::test]
    async fn late_connection_invalidation_does_not_evict_replacement() {
        let state = Arc::new(ClientState::new(PathBuf::from("unused")));
        let (writer, _reader) = mpsc::channel(1);
        *state.connection.lock().await = ConnectionState::Ready(Arc::new(LiveConnection {
            generation: 2,
            writer,
        }));
        state.invalidate_connection(1).await;
        assert!(matches!(
            &*state.connection.lock().await,
            ConnectionState::Ready(connection) if connection.generation == 2
        ));
    }

    #[tokio::test]
    async fn late_route_invalidation_does_not_evict_replacement() {
        let state = Arc::new(ClientState::new(PathBuf::from("unused")));
        let replacement = RouteLease {
            route: Route {
                channel: 8,
                epoch: 21,
            },
            connection_generation: 4,
            route_generation: 9,
        };
        *state.route.lock().await = RouteState::Ready(replacement);
        state
            .invalidate_route(RouteLease {
                route: Route {
                    channel: 7,
                    epoch: 20,
                },
                connection_generation: 4,
                route_generation: 8,
            })
            .await;
        assert!(matches!(
            *state.route.lock().await,
            RouteState::Ready(lease) if lease == replacement
        ));
    }

    #[test]
    fn protocol_error_text_and_capability_never_enter_fixed_error() {
        let secret = "ckh_must_not_leak";
        let body = serde_json::to_vec(&serde_json::json!({
            "code": "bad_request",
            "message": format!("bad capability {secret}"),
            "class": "future_unknown"
        }))
        .unwrap();
        let error = classify_error_frame(&body);
        assert!(!format!("{error:?}").contains(secret));
    }

    #[test]
    fn canonical_error_body_unknown_channel_is_route_only_failure() {
        let body = serde_json::to_vec(&subc_protocol::ErrorBody::new(
            "unknown_channel",
            "unknown channel 7",
        ))
        .unwrap();
        assert_eq!(classify_error_frame(&body), ClientFailure::RouteGone);
    }

    #[test]
    fn connection_path_is_not_formatted_with_capability_data() {
        let client = VaultClient::new(std::path::Path::new("/tmp/subc-connection.json"));
        assert!(!format!("{client:?}").contains("subc-connection.json"));
    }
}

#[cfg(test)]
mod drop_counter_tests {
    use super::*;

    fn state() -> ClientState {
        ClientState::new(PathBuf::from("/nonexistent/connection.json"))
    }

    fn frame(epoch: u32, corr: u64) -> Frame {
        Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Passive, false),
            0,
            epoch,
            corr,
            Vec::new(),
        )
        .expect("frame")
    }

    /// A reply nobody is waiting for is counted, not silently discarded.
    ///
    /// The drop itself is correct. What it must not be is invisible: a
    /// discarded reply and a peer that sent nothing both present as a call that
    /// waits and then times out, and those two states send an investigation to
    /// opposite components. A non-zero count says the frame did arrive here.
    #[test]
    fn an_unmatched_terminal_frame_is_counted() {
        let state = state();
        assert_eq!(state.unmatched_terminal_drops(), 0);

        state.dispatch(1, frame(7, 42));

        assert_eq!(
            state.unmatched_terminal_drops(),
            1,
            "a frame with no waiting caller must be counted"
        );
        assert_eq!(
            state.stale_generation_drops(),
            0,
            "and must not be attributed to the other class"
        );
    }

    /// A reply for a caller from a previous connection counts separately.
    ///
    /// Kept apart from the unmatched class because they mean different things:
    /// this one says a reconnect raced a reply in flight, which is expected
    /// during a daemon restart, while an unmatched terminal in a quiet period is
    /// not expected at all. One combined number would let a burst of the
    /// ordinary kind hide the other.
    #[test]
    fn a_frame_for_an_older_connection_is_counted_as_stale() {
        let state = state();
        let (sender, _receiver) = oneshot::channel();
        state.pending.lock().unwrap().insert(
            (7, 42),
            Pending {
                connection_generation: 1,
                sender,
            },
        );

        // Same key, but the reader loop is now on a newer connection.
        state.dispatch(2, frame(7, 42));

        assert_eq!(state.stale_generation_drops(), 1);
        assert_eq!(state.unmatched_terminal_drops(), 0);
        assert!(
            state.pending.lock().unwrap().contains_key(&(7, 42)),
            "a stale frame must not consume the current caller's entry"
        );
    }

    /// A matched frame is delivered and counts nothing.
    ///
    /// The control: without it, a dispatch that counted every frame as a drop
    /// would satisfy both tests above.
    #[test]
    fn a_matched_frame_is_delivered_and_not_counted() {
        let state = state();
        let (sender, receiver) = oneshot::channel();
        state.pending.lock().unwrap().insert(
            (7, 42),
            Pending {
                connection_generation: 1,
                sender,
            },
        );

        state.dispatch(1, frame(7, 42));

        assert!(receiver.blocking_recv().is_ok(), "caller must receive it");
        assert_eq!(state.unmatched_terminal_drops(), 0);
        assert_eq!(state.stale_generation_drops(), 0);
    }
}
