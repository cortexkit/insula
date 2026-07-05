#![forbid(unsafe_code)]

//! The ai-provider-quota subc module.
//!
//! Lifecycle (mirrors the canonical module handshake):
//!   1. read `--subc <connection-file>`, HMAC-authenticate as a client to subc;
//!   2. send `HELLO { manifest }` registering a ManagementSurface that exposes
//!      the `usage.get` query, await `HELLO_ACK`;
//!   3. serve channel-0 control (`route.bind` → ack, `PING` → `PONG`) and
//!      route-channel data requests (`usage.get` → `ProviderUsage[]`).
//!
//! The module is machine-global: it reads each provider's own on-disk session
//! (`~/.codex/auth.json`, ...) regardless of the relayed bind identity, so it
//! declares an empty identity scope and ignores `project_root`.

use std::{error::Error, ffi::OsString, fmt, path::PathBuf, sync::Arc};

use quota_core::Registry;
use serde::Deserialize;
use serde_json::json;
use subc_protocol::{
    manifest::{
        Bindings, IdentityBinding, ManagementOperation, ManagementOperationKind, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, TrustTier,
    },
    session::{
        HealthStatus, ModuleControlRequest, ModuleControlResponse, MODULE_CONTROL_OP_HEALTH_CHECK,
    },
    ErrorBody, Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority,
    PROTOCOL_VERSION, SUBC_MODULE_ID_ENV,
};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpStream,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

const DEFAULT_MODULE_ID: &str = "ai-provider-quota";
const USAGE_GET_OP: &str = "usage.get";
const HELLO_CORR: u64 = 1;
const EGRESS_BUFFER: usize = 64;

#[tokio::main]
async fn main() -> Result<(), ModuleError> {
    let config = ModuleConfig::from_env()?;
    run(config).await
}

struct ModuleConfig {
    connection_file_path: PathBuf,
    module_id: String,
}

impl ModuleConfig {
    fn from_env() -> Result<Self, ModuleError> {
        let connection_file_path = parse_subc_arg(std::env::args_os().skip(1))?;
        let module_id = std::env::var(SUBC_MODULE_ID_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODULE_ID.to_string());
        Ok(Self {
            connection_file_path,
            module_id,
        })
    }
}

async fn run(config: ModuleConfig) -> Result<(), ModuleError> {
    let stream = connect_to_subc(&config.connection_file_path).await?;
    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<Frame>(EGRESS_BUFFER);
    let writer = tokio::spawn(drain_writer(write_half, rx));

    let registry = Arc::new(Registry::with_defaults());

    // The background refresher owns all provider fetching: the serving path
    // (usage.get) only ever reads the slot store, so route reads never block on
    // the network. Cancelled when the frame loop ends so the task never leaks.
    let refresher_cancel = CancellationToken::new();
    let refresher = {
        let registry = Arc::clone(&registry);
        let cancel = refresher_cancel.clone();
        tokio::spawn(async move { registry.refresh_loop(cancel).await })
    };

    let loop_result = module_loop(&mut read_half, tx.clone(), &config, Arc::clone(&registry)).await;
    drop(tx);

    // Stop the refresher and reap it before returning.
    refresher_cancel.cancel();
    let _ = refresher.await;

    let writer_result = writer
        .await
        .map_err(|e| ModuleError::Message(e.to_string()));
    match (loop_result, writer_result) {
        (Err(loop_err), _) => Err(loop_err),
        (Ok(()), Ok(Ok(()))) => Ok(()),
        (Ok(()), Ok(Err(writer_err))) => Err(ModuleError::Message(writer_err.to_string())),
        (Ok(()), Err(join_err)) => Err(join_err),
    }
}

async fn connect_to_subc(connection_file_path: &PathBuf) -> Result<TcpStream, ModuleError> {
    let conn = connection_file::read(connection_file_path)
        .map_err(|e| ModuleError::Message(format!("reading connection file: {e}")))?;
    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| ModuleError::Message("connection file has no endpoints".into()))?;
    let addr = format!("{}:{}", endpoint.host, endpoint.port);
    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| ModuleError::Message(format!("connect {addr}: {e}")))?;
    authenticate_client(&mut stream, &conn, std::time::Duration::from_secs(2))
        .await
        .map_err(|e| ModuleError::Message(format!("authenticate: {e}")))?;
    Ok(stream)
}

async fn module_loop<R>(
    read_half: &mut R,
    writer: mpsc::Sender<Frame>,
    config: &ModuleConfig,
    registry: Arc<Registry>,
) -> Result<(), ModuleError>
where
    R: AsyncRead + Unpin,
{
    send_hello(&writer, config).await?;
    expect_hello_ack(read_half).await?;

    loop {
        let Some(frame) = read_frame(read_half)
            .await
            .map_err(|e| ModuleError::Message(e.to_string()))?
        else {
            return Ok(()); // clean EOF: subc closed the connection.
        };
        if !handle_frame(frame, &writer, &registry).await? {
            return Ok(());
        }
    }
}

async fn drain_writer<W>(write_half: W, mut rx: mpsc::Receiver<Frame>) -> Result<(), ModuleError>
where
    W: AsyncWrite + Unpin,
{
    let mut writer = BufWriter::new(write_half);
    while let Some(frame) = rx.recv().await {
        write_frame(&mut writer, &frame)
            .await
            .map_err(|e| ModuleError::Message(e.to_string()))?;
        while let Ok(frame) = rx.try_recv() {
            write_frame(&mut writer, &frame)
                .await
                .map_err(|e| ModuleError::Message(e.to_string()))?;
        }
        writer
            .flush()
            .await
            .map_err(|e| ModuleError::Message(e.to_string()))?;
    }
    writer
        .flush()
        .await
        .map_err(|e| ModuleError::Message(e.to_string()))?;
    Ok(())
}

async fn send_hello(
    writer: &mpsc::Sender<Frame>,
    config: &ModuleConfig,
) -> Result<(), ModuleError> {
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest: manifest(&config.module_id),
        protocol_ver: PROTOCOL_VERSION,
        // Advertise health.check so the daemon actively probes us (capability-
        // gated: unadvertised = health "unknown", never probed). We answer L2
        // through the same frame path and report L3 domain health from the sweep.
        control_ops: Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
        // Echo the one-time launch nonce subc injects via SUBC_LAUNCH_NONCE for a
        // reserved module; absent (None) for a normally-supervised module like this one.
        launch_nonce: std::env::var(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
            .ok()
            .filter(|value| !value.is_empty()),
    })
    .map_err(ModuleError::Json)?;
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, HELLO_CORR, body)
        .map_err(|e| ModuleError::Message(e.to_string()))?;
    send(writer, frame).await
}

async fn expect_hello_ack<R>(reader: &mut R) -> Result<ModuleHelloAckBody, ModuleError>
where
    R: AsyncRead + Unpin,
{
    let frame = read_frame(reader)
        .await
        .map_err(|e| ModuleError::Message(e.to_string()))?
        .ok_or_else(|| ModuleError::Message("connection closed before HELLO_ACK".into()))?;
    match frame.header.ty {
        FrameType::HelloAck => serde_json::from_slice(&frame.body).map_err(ModuleError::Json),
        FrameType::Error => {
            let body =
                serde_json::from_slice::<ErrorBody>(&frame.body).map_err(ModuleError::Json)?;
            Err(ModuleError::Message(format!(
                "subc rejected HELLO: {} — {}",
                body.code, body.message
            )))
        }
        ty => Err(ModuleError::Message(format!(
            "unexpected frame {ty:?} awaiting HELLO_ACK"
        ))),
    }
}

/// Returns `Ok(false)` to stop the loop (graceful goodbye / EOF).
async fn handle_frame(
    frame: Frame,
    writer: &mpsc::Sender<Frame>,
    registry: &Arc<Registry>,
) -> Result<bool, ModuleError> {
    match frame.header.ty {
        FrameType::Ping if frame.header.channel == 0 => {
            let pong = Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
                0,
                frame.header.corr,
                Vec::new(),
            )
            .map_err(|e| ModuleError::Message(e.to_string()))?;
            send(writer, pong).await?;
            Ok(true)
        }
        FrameType::Goodbye if frame.header.channel == 0 => Ok(false),
        FrameType::Goodbye => Ok(true), // route goodbye: nothing to tear down (no per-route state).
        FrameType::Request if frame.header.channel == 0 => {
            handle_control_request(frame, writer, registry).await?;
            Ok(true)
        }
        FrameType::Request => {
            // Data-plane request on a route channel. Spawn so a slow fetch never
            // head-of-line-blocks another route (ManagementSurface concurrency).
            let writer = writer.clone();
            let registry = Arc::clone(registry);
            tokio::spawn(async move {
                let _ = handle_usage_request(frame, &writer, &registry).await;
            });
            Ok(true)
        }
        _ => Ok(true),
    }
}

async fn handle_control_request(
    frame: Frame,
    writer: &mpsc::Sender<Frame>,
    registry: &Arc<Registry>,
) -> Result<(), ModuleError> {
    let request =
        serde_json::from_slice::<ModuleControlRequest>(&frame.body).map_err(ModuleError::Json)?;
    let response_body = match request {
        ModuleControlRequest::RouteBind { .. } => {
            // Machine-global surface: accept every bind. We key on no identity,
            // so there is nothing to validate or store — just ack.
            ModuleControlResponse::RouteBindAck {}
        }
        ModuleControlRequest::HealthCheck {} => {
            // L3 domain health from the last usage sweep — cheap, no fetch. Only
            // a poisoned serving cache (a panicked serving task) is `failing`;
            // otherwise `ok`, because a provider degrading on a box without its
            // creds is this prober's normal state, carried as detail not status.
            health_report(&registry.health())
        }
    };
    let body = serde_json::to_vec(&response_body).map_err(ModuleError::Json)?;
    let response = Frame::build_with_version(
        frame.header.ver,
        FrameType::Response,
        control_flags(),
        0,
        frame.header.corr,
        body,
    )
    .map_err(|e| ModuleError::Message(e.to_string()))?;
    send(writer, response).await
}

/// Map the core health snapshot onto the subc health-report wire shape. Status
/// ladder: `failing` when the slot store is poisoned (a serving/refresher task
/// panicked); `degraded` when the refresher loop is stalled (wedged/dead — its
/// last-known windows still serve, only freshness decays); otherwise `ok` with
/// per-provider staleness carried as opaque `detail`/`metrics`, because a
/// provider lacking local creds is this prober's normal resting state.
fn health_report(snapshot: &quota_core::health::HealthSnapshot) -> ModuleControlResponse {
    let status = if snapshot.is_failing() {
        HealthStatus::Failing
    } else if snapshot.is_degraded() {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    };
    let detail = if snapshot.cache_poisoned {
        Some("slot store mutex poisoned: a serving/refresher task panicked".to_string())
    } else if snapshot.refresher_stalled {
        Some(match snapshot.last_tick_age {
            Some(age) => format!("refresher stalled: last tick {}s ago", age.as_secs()),
            None => "refresher never ticked since startup".to_string(),
        })
    } else if !snapshot.degraded.is_empty() {
        Some(format!(
            "{}/{} providers degraded ({} of {} cookie-cohort); {} serving",
            snapshot.degraded.len(),
            snapshot.providers_total,
            snapshot.cookie_cohort_degraded.len(),
            snapshot.cookie_cohort_total,
            snapshot.serving(),
        ))
    } else {
        None
    };
    let metrics = json!({
        "providersTotal": snapshot.providers_total,
        "fresh": snapshot.fresh,
        "stale": snapshot.stale,
        "degraded": snapshot.degraded,
        "cookieCohortTotal": snapshot.cookie_cohort_total,
        "cookieCohortDegraded": snapshot.cookie_cohort_degraded,
        "lastTickAgeSecs": snapshot.last_tick_age.map(|d| d.as_secs()),
        "refresherStalled": snapshot.refresher_stalled,
    });
    ModuleControlResponse::HealthCheck {
        status,
        detail,
        metrics: Some(metrics),
    }
}

/// A `usage.get` request body: `{ "method": "usage.get", "params": { provider? } }`.
#[derive(Debug, Deserialize)]
struct UsageRequest {
    method: String,
    #[serde(default)]
    params: UsageParams,
}

#[derive(Debug, Deserialize, Default)]
struct UsageParams {
    #[serde(default)]
    provider: Option<String>,
}

async fn handle_usage_request(
    frame: Frame,
    writer: &mpsc::Sender<Frame>,
    registry: &Arc<Registry>,
) -> Result<(), ModuleError> {
    let channel = frame.header.channel;
    let corr = frame.header.corr;
    let ver = frame.header.ver;

    let request: UsageRequest = match serde_json::from_slice(&frame.body) {
        Ok(r) => r,
        Err(e) => {
            return send_route_error(
                writer,
                ver,
                channel,
                corr,
                "invalid_request",
                &format!("usage request body not decodable: {e}"),
            )
            .await;
        }
    };

    if request.method != USAGE_GET_OP {
        return send_route_error(
            writer,
            ver,
            channel,
            corr,
            "unknown_method",
            &format!(
                "unknown method '{}', expected '{USAGE_GET_OP}'",
                request.method
            ),
        )
        .await;
    }

    let usage = registry.get_usage(request.params.provider.as_deref()).await;
    let body = serde_json::to_vec(&json!({ "result": usage })).map_err(ModuleError::Json)?;
    let response = Frame::build_with_version(
        ver,
        FrameType::Response,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        body,
    )
    .map_err(|e| ModuleError::Message(e.to_string()))?;
    send(writer, response).await
}

async fn send_route_error(
    writer: &mpsc::Sender<Frame>,
    ver: u8,
    channel: u16,
    corr: u64,
    code: &str,
    message: &str,
) -> Result<(), ModuleError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message: message.to_string(),
    })
    .map_err(ModuleError::Json)?;
    let frame = Frame::build_with_version(
        ver,
        FrameType::Error,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        body,
    )
    .map_err(|e| ModuleError::Message(e.to_string()))?;
    send(writer, frame).await
}

async fn send(writer: &mpsc::Sender<Frame>, frame: Frame) -> Result<(), ModuleError> {
    writer
        .send(frame)
        .await
        .map_err(|_| ModuleError::Message("egress channel closed".into()))
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

/// The module's capability manifest: a ManagementSurface exposing `usage.get`.
///
/// Machine-global, so `identity_scope` is empty and the identity binding
/// requires nothing. Storage is declared `owns_schema: false` — the module keeps
/// only an in-memory TTL cache and owns no persistent schema (the manifest enum
/// has no none/ephemeral storage kind yet, so this is the honest expression).
fn manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: USAGE_GET_OP.to_string(),
                kind: ManagementOperationKind::Query,
            }],
            config_schema: json!({ "type": "object" }),
            observability: Vec::new(),
            identity_scope: Vec::new(),
        }],
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: Vec::new(),
                optional: Vec::new(),
            },
        },
    }
}

fn parse_subc_arg(args: impl IntoIterator<Item = OsString>) -> Result<PathBuf, ModuleError> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            let value = args
                .next()
                .ok_or_else(|| ModuleError::Message("--subc requires a value".into()))?;
            return Ok(PathBuf::from(value));
        }
        if let Some(raw) = arg.to_str().and_then(|a| a.strip_prefix("--subc=")) {
            if raw.is_empty() {
                return Err(ModuleError::Message("--subc= requires a value".into()));
            }
            return Ok(PathBuf::from(raw));
        }
        // Ignore unknown args (e.g. --connection-file) for forward-compat with
        // supervised launch conventions.
    }
    Err(ModuleError::Message(
        "--subc <connection-file> is required".into(),
    ))
}

#[derive(Debug)]
enum ModuleError {
    Message(String),
    Json(serde_json::Error),
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(m) => write!(f, "{m}"),
            Self::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl Error for ModuleError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the REAL channel-0 control handler with a `health.check` Request and
    /// assert it answers with a well-formed `HealthCheck` Response carrying the
    /// domain metrics. Exercises the actual arm + registry + mapper, not a mock.
    #[tokio::test]
    async fn health_check_control_request_returns_domain_report() {
        let registry = Arc::new(Registry::with_defaults());
        let (tx, mut rx) = mpsc::channel::<Frame>(4);

        let request = ModuleControlRequest::HealthCheck {};
        let frame = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Request,
            control_flags(),
            0,
            42,
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();

        handle_control_request(frame, &tx, &registry).await.unwrap();

        let response = rx.try_recv().expect("a response frame was sent");
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, 0);
        assert_eq!(response.header.corr, 42);

        let body: ModuleControlResponse = serde_json::from_slice(&response.body).unwrap();
        let ModuleControlResponse::HealthCheck {
            status, metrics, ..
        } = body
        else {
            panic!("expected a HealthCheck response");
        };
        // No refresher tick has run in this fresh registry and it was just
        // created (within the stall horizon), so it is healthy/Ok, not stalled.
        assert_eq!(status, HealthStatus::Ok);
        let metrics = metrics.expect("health report carries metrics");
        let obj = metrics.as_object().expect("metrics is a JSON object");
        for key in [
            "providersTotal",
            "fresh",
            "stale",
            "degraded",
            "cookieCohortTotal",
            "cookieCohortDegraded",
            "lastTickAgeSecs",
            "refresherStalled",
        ] {
            assert!(obj.contains_key(key), "metrics include {key}");
        }
        // The default registry has the full provider set and a non-empty cookie
        // cohort — a real count, not a placeholder.
        assert!(obj["providersTotal"].as_u64().unwrap() >= 27);
        assert!(obj["cookieCohortTotal"].as_u64().unwrap() >= 7);
        // Fresh registry: never ticked, nothing fetched yet.
        assert_eq!(obj["fresh"].as_u64().unwrap(), 0);
        assert_eq!(obj["refresherStalled"], serde_json::json!(false));
    }

    /// The `route.bind` arm still acks unchanged after threading the registry in.
    #[tokio::test]
    async fn route_bind_control_request_still_acks() {
        let registry = Arc::new(Registry::with_defaults());
        let (tx, mut rx) = mpsc::channel::<Frame>(4);

        let bind = serde_json::json!({
            "op": "route.bind",
            "route_channel": 7,
            "target": { "kind": "management_surface", "module_id": MODULE_ID_FOR_TEST },
            "identity": { "project_root": "/tmp/x", "harness": "test", "session": "s1" }
        });
        let frame = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Request,
            control_flags(),
            0,
            7,
            serde_json::to_vec(&bind).unwrap(),
        )
        .unwrap();

        handle_control_request(frame, &tx, &registry).await.unwrap();

        let response = rx.try_recv().expect("a response frame was sent");
        assert_eq!(response.header.ty, FrameType::Response);
        let body: ModuleControlResponse = serde_json::from_slice(&response.body).unwrap();
        assert!(matches!(body, ModuleControlResponse::RouteBindAck {}));
    }

    const MODULE_ID_FOR_TEST: &str = "ai-provider-quota";
}
