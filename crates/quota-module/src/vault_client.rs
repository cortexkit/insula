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
    CredentialSource, VaultCapability, VaultCredential, VaultGetError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, Notify};
use tokio::time::Instant;

const CREDENTIALS_MODULE_ID: &str = "cortexkit-credentials";
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
    Protocol,
}

impl ClientFailure {
    fn vault_error(self) -> VaultGetError {
        match self {
            Self::Transport | Self::RouteGone => VaultGetError::Transient,
            Self::Classified(error) => error,
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

    fn dispatch(&self, generation: u64, frame: Frame) {
        let key = (frame.header.epoch, frame.header.corr);
        let pending = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if pending
                .get(&key)
                .is_some_and(|entry| entry.connection_generation == generation)
            {
                pending.remove(&key)
            } else {
                None
            }
        };
        if let Some(pending) = pending {
            let _ = pending.sender.send(Ok(frame));
        }
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
        tokio::time::timeout(self.request_timeout, async {
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
        .await
        .map_err(|_| ClientFailure::Transport)?
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
struct VaultErrorResult {
    error: VaultReadError,
}

#[derive(Deserialize)]
struct VaultReadError {
    class: String,
}

fn class_to_error(class: &str) -> VaultGetError {
    match class {
        "transient" => VaultGetError::Transient,
        "auth_required" => VaultGetError::AuthRequired,
        "permanent" => VaultGetError::Permanent,
        "context_overflow" => VaultGetError::FailClosed,
        _ => VaultGetError::FailClosed,
    }
}

fn classify_error_frame(body: &[u8]) -> ClientFailure {
    let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    match value.get("code").and_then(Value::as_str) {
        Some("unknown_channel" | "unknown_module" | "module_reloading") => {
            return ClientFailure::RouteGone;
        }
        Some("target_unavailable" | "module_timeout" | "backend_error") => {
            return ClientFailure::Transport;
        }
        _ => {}
    }
    value
        .get("class")
        .or_else(|| value.pointer("/error/class"))
        .and_then(Value::as_str)
        .map(class_to_error)
        .map(ClientFailure::Classified)
        .unwrap_or(ClientFailure::Protocol)
}

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
            // A success reply carrying no credential bytes is malformed, not a
            // credential. Every lane converts the payload straight into a bearer,
            // and an empty one converts cleanly into an empty bearer -- which the
            // upstream answers with 401, a NON-TRANSIENT class that clears the
            // cached window and reports the account as auth-dead. Rejecting it
            // here fails closed instead, so the lane retries a vault that is
            // briefly answering wrongly rather than condemning the account.
            if success.payload.is_empty() {
                return Err(VaultGetError::FailClosed);
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
            Err(class_to_error(&failure.error.class))
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
                return Err(class_to_error(class));
            }
            response
                .get("result")
                .filter(|result| result.is_object())
                .map(|_| ())
                .ok_or(VaultGetError::FailClosed)
        }
        .await;
        if let Err(error) = result {
            eprintln!("[ck-quota] warning: vault auth-failure report failed class={error:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

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
        assert_eq!(class_to_error("transient"), VaultGetError::Transient);
        assert_eq!(class_to_error("auth_required"), VaultGetError::AuthRequired);
        assert_eq!(class_to_error("permanent"), VaultGetError::Permanent);
        assert_eq!(
            class_to_error("context_overflow"),
            VaultGetError::FailClosed
        );
        assert_eq!(class_to_error("future_class"), VaultGetError::FailClosed);
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
        let unknown = serde_json::to_vec(&serde_json::json!({
            "code": "future_authorization_error",
            "message": "not an availability class"
        }))
        .unwrap();
        assert_eq!(
            classify_error_frame(&unknown).vault_error(),
            VaultGetError::FailClosed
        );
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
                serde_json::to_vec(&subc_protocol::ErrorBody {
                    code: "unknown_channel".to_string(),
                    message: "route expired".to_string(),
                })
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

    #[test]
    fn a_success_reply_with_an_empty_payload_fails_closed() {
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
            matches!(decoded, Err(VaultGetError::FailClosed)),
            "an empty payload must fail closed rather than serve an empty bearer"
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
        let body = serde_json::to_vec(&subc_protocol::ErrorBody {
            code: "unknown_channel".to_string(),
            message: "unknown channel 7".to_string(),
        })
        .unwrap();
        assert_eq!(classify_error_frame(&body), ClientFailure::RouteGone);
    }

    #[test]
    fn connection_path_is_not_formatted_with_capability_data() {
        let client = VaultClient::new(std::path::Path::new("/tmp/subc-connection.json"));
        assert!(!format!("{client:?}").contains("subc-connection.json"));
    }
}
