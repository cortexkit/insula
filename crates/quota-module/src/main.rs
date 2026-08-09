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

mod ids;
mod vault_client;

use ids::DEFAULT_MODULE_ID;

use std::{error::Error, ffi::OsString, fmt, path::PathBuf, sync::Arc};

use quota_core::{config::QuotaConfig, credential_source::CredentialSource, Registry};
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
use vault_client::VaultClient;

const USAGE_GET_OP: &str = "usage.get";
const HELLO_CORR: u64 = 1;
const EGRESS_BUFFER: usize = 64;
/// The `ck-quota` name is a literal, deliberately not derived from the binary or
/// module name — both of which have since been renamed, so beside a binary called
/// `ck-insula` this reads like a leftover. It is the live config file.
///
/// It is read ONCE, at startup. Renaming it, or any restart that cannot find it,
/// leaves the codex banked-resets feature OFF while the module comes up entirely
/// healthy: nothing on the wire, in health metrics, or in the supervisor view
/// records that a config was expected. The only way to tell the feature is armed
/// is from its effect on the wire: while it is armed, a codex entry's windows
/// carry `usedPercent` 0 with the provider's actual figure moved into
/// `rawUsedPercent`. An unarmed module publishes the real figure in
/// `usedPercent` and omits `rawUsedPercent` entirely.
const QUOTA_CONFIG_RELATIVE_PATH: &str = "cortexkit/ck-quota.jsonc";

#[tokio::main]
async fn main() -> Result<(), ModuleError> {
    let config = ModuleConfig::from_env()?;
    let quota_config = load_quota_config();
    eprintln!(
        "[ck-quota] codex banked resets armed={} auto_use_resets={}s (startup-only; arm one host per account)",
        quota_config.codex.is_enabled(),
        quota_config.codex.auto_use_resets
    );
    run(config, quota_config).await
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

fn quota_config_path() -> Option<PathBuf> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(config_home).join(QUOTA_CONFIG_RELATIVE_PATH));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".config").join(QUOTA_CONFIG_RELATIVE_PATH))
}

fn parse_quota_config(contents: &str) -> Result<QuotaConfig, String> {
    let json = subc_jsonc::jsonc_to_json(contents)?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn load_quota_config_file(path: &std::path::Path) -> QuotaConfig {
    let parsed = std::fs::read_to_string(path)
        .map_err(|error| format!("read failed: {error}"))
        .and_then(|contents| parse_quota_config(&contents));
    match parsed {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "[ck-quota] warning: {} unavailable or malformed ({error}); codex banked resets default OFF",
                path.display()
            );
            QuotaConfig::default()
        }
    }
}

fn load_quota_config() -> QuotaConfig {
    let Some(path) = quota_config_path() else {
        eprintln!(
            "[ck-quota] warning: cannot resolve the quota config path; codex banked resets default OFF"
        );
        return QuotaConfig::default();
    };
    load_quota_config_file(&path)
}

async fn run(config: ModuleConfig, quota_config: QuotaConfig) -> Result<(), ModuleError> {
    let stream = connect_to_subc(&config.connection_file_path).await?;
    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<Frame>(EGRESS_BUFFER);
    let writer = tokio::spawn(drain_writer(write_half, rx));

    let credential_source: Arc<dyn CredentialSource> =
        Arc::new(VaultClient::new(config.connection_file_path.clone()));
    let registry = Arc::new(Registry::with_defaults(
        quota_config,
        Some(credential_source),
    ));

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
    // Channel-0 control frame: epoch is always 0.
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, 0, HELLO_CORR, body)
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
    // A body this module cannot decode is answered, not escalated. Returning an
    // error here propagates to the frame loop, which treats it as fatal and ends
    // the connection -- so one unrecognised control body would take the module
    // down, losing every provider's usage until the supervisor restarts it.
    //
    // This is reachable without anything being broken: the daemon may add a
    // control op, or a field to an existing one, and a module built against the
    // older definition cannot decode it. The daemon handles an error reply
    // gracefully -- it releases the route reservation and relays the message to
    // the consumer -- so answering leaves the module serving while the mismatch
    // is visible in the reply.
    let request = match serde_json::from_slice::<ModuleControlRequest>(&frame.body) {
        Ok(request) => request,
        Err(error) => {
            return send_control_error(
                writer,
                frame.header.ver,
                frame.header.corr,
                "invalid_control_body",
                &format!("control body not decodable: {error}"),
            )
            .await;
        }
    };
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
    // Channel-0 control reply: epoch is always 0.
    let response = Frame::build_with_version(
        frame.header.ver,
        FrameType::Response,
        control_flags(),
        0,
        0,
        frame.header.corr,
        body,
    )
    .map_err(|e| ModuleError::Message(e.to_string()))?;
    send(writer, response).await
}

/// Cap on request-supplied text echoed back in an error reply.
///
/// Generous next to any real method name, so a caller that mistyped one still
/// sees it in full. The bound exists for the case where the value is not a
/// method name at all.
const MAX_ECHOED_METHOD_BYTES: usize = 256;

/// Reply to a channel-0 control request with a canonical subc error.
///
/// `code` uses the daemon's own control vocabulary so the message is legible to
/// it and to whatever consumer the failure is relayed to.
async fn send_control_error(
    writer: &mpsc::Sender<Frame>,
    ver: u8,
    corr: u64,
    code: &str,
    message: &str,
) -> Result<(), ModuleError> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message: message.to_string(),
    })
    .map_err(ModuleError::Json)?;
    // Channel-0 control reply: channel and epoch are always 0.
    let frame = Frame::build_with_version(ver, FrameType::Error, control_flags(), 0, 0, corr, body)
        .map_err(|e| ModuleError::Message(e.to_string()))?;
    send(writer, frame).await
}

/// The git commit this binary was built from, stamped by `build.rs`. Falls back
/// to "unknown" when the build could not resolve a repository.
const BUILD_COMMIT: &str = env!("CK_QUOTA_BUILD_COMMIT");

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
    } else if !snapshot.degraded.is_empty() || snapshot.serving() < snapshot.providers_total {
        // Serving first, then the number an operator should act on, then the
        // ones that are merely not set up here. Leading with degraded made the
        // line read as an alarm on every host, because most adapters have no
        // credential on any given machine.
        //
        // The cookie count is narrower still -- only browser logins that went
        // stale, excluding never having logged in -- so it keeps its own clause
        // rather than being rendered in parallel with the others.
        Some(format!(
            "{} serving, {} degraded, {} unconfigured of {}; {} of {} cookie logins stale",
            snapshot.serving(),
            snapshot.degraded.len(),
            snapshot.unconfigured.len(),
            snapshot.providers_total,
            snapshot.cookie_logins_stale.len(),
            snapshot.cookie_cohort_total,
        ))
    } else {
        None
    };
    let metrics = json!({
        // The commit this binary was built from. The module is supervised and
        // long-lived, so the repository's HEAD says nothing about what is actually
        // serving — a fix can be merged and green while an older binary still runs.
        // Reporting it here makes "is the deployed build current?" answerable from
        // the RUNNING process, which a file's modification time cannot establish.
        "buildCommit": BUILD_COMMIT,
        "providersTotal": snapshot.providers_total,
        "fresh": snapshot.fresh,
        "stale": snapshot.stale,
        // Providers whose first fetch has not completed. The refresher admits a
        // bounded number of fetch units per turn, so after a start the providers
        // beyond that cap are queued for several turns -- ordinary, not a fault.
        // Published because the conservation identity under-sums without it, and
        // an identity that is briefly false after every restart trains its reader
        // to ignore the imbalance that matters.
        "pending": snapshot.pending,
        // Providers somebody can act on: a credential exists and is failing.
        // Kept apart from `unconfigured` so a consumer can alert on this being
        // non-empty, which is not possible when the two are one number.
        "degraded": snapshot.degraded,
        // Providers with no credential source on this host -- the expected state
        // for most adapters on most machines. Published by name rather than
        // omitted so the conservation identity still accounts for them.
        "unconfigured": snapshot.unconfigured,
        // Registered providers that resolved no credential handle, so they appear
        // in none of the counts above. Reported by name so the buckets can be
        // reconciled against providersTotal rather than silently under-summing.
        "withoutHandles": snapshot.without_handles,
        "cookieCohortTotal": snapshot.cookie_cohort_total,
        "cookieLoginsStale": snapshot.cookie_logins_stale,
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
    // The route's binding epoch must be echoed on every reply, or the daemon
    // drops the frame as belonging to a stale binding of this channel slot.
    let epoch = frame.header.epoch;
    let corr = frame.header.corr;
    let ver = frame.header.ver;

    let request: UsageRequest = match serde_json::from_slice(&frame.body) {
        Ok(r) => r,
        Err(e) => {
            return send_route_error(
                writer,
                ver,
                channel,
                epoch,
                corr,
                "invalid_request",
                &format!("usage request body not decodable: {e}"),
            )
            .await;
        }
    };

    if request.method != USAGE_GET_OP {
        // The method name is echoed so the caller can see what was rejected, but
        // it comes from the request and nothing upstream bounds it. An error
        // reply large enough to exceed the frame limit cannot be built, and the
        // caller would be left with no reply at all -- turning a clear rejection
        // into a silent timeout, which is the outcome this surface promises
        // cannot happen.
        let echoed = quota_core::text::truncate_for_wire(&request.method, MAX_ECHOED_METHOD_BYTES);
        return send_route_error(
            writer,
            ver,
            channel,
            epoch,
            corr,
            "unknown_method",
            &format!("unknown method '{echoed}', expected '{USAGE_GET_OP}'"),
        )
        .await;
    }

    let snapshot = registry
        .usage_snapshot(request.params.provider.as_deref())
        .await;

    // A reply that cannot be built must still be a reply. Both steps below can
    // fail on data rather than on programmer error -- the body is assembled from
    // upstream-derived strings, and the frame layer caps a body at 64 MiB -- and
    // returning the error here would leave the consumer with no response frame
    // at all, since the caller discards it to keep the loop alive. It would then
    // wait out its own timeout with nothing saying why, which is the one
    // outcome this surface promises cannot happen: every poll either answers or
    // names its failure.
    // `result` keeps its shape and meaning; `completeProviders` is a sibling key
    // beside it. Every consumer reads this envelope as an untyped value and
    // takes `result` out of it, so a new key is invisible until one chooses to
    // read it -- which is why this is additive rather than a second operation.
    // A second operation would have left the original serving path alive and
    // correct-looking forever, and that path is exactly the one that cannot say
    // whether an account is missing or gone.
    //
    // Shaped by the snapshot itself so a consumer's reference envelope and the
    // one served here cannot differ.
    let body = match serde_json::to_vec(&snapshot.to_envelope()) {
        Ok(body) => body,
        Err(error) => {
            return send_route_error(
                writer,
                ver,
                channel,
                epoch,
                corr,
                "internal_error",
                &format!("usage response could not be serialized: {error}"),
            )
            .await;
        }
    };
    let response = match Frame::build_with_version(
        ver,
        FrameType::Response,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        body,
    ) {
        Ok(response) => response,
        Err(error) => {
            return send_route_error(
                writer,
                ver,
                channel,
                epoch,
                corr,
                "internal_error",
                &format!("usage response could not be framed: {error}"),
            )
            .await;
        }
    };
    send(writer, response).await
}

async fn send_route_error(
    writer: &mpsc::Sender<Frame>,
    ver: u8,
    channel: u16,
    epoch: u32,
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
        epoch,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    /// A snapshot of a module serving normally, to be perturbed one field at a
    /// time by the tests that care about a single fault.
    fn healthy_snapshot() -> quota_core::health::HealthSnapshot {
        quota_core::health::HealthSnapshot {
            providers_total: 3,
            fresh: 3,
            stale: 0,
            pending: 0,
            degraded: Vec::new(),
            unconfigured: Vec::new(),
            without_handles: Vec::new(),
            cookie_cohort_total: 0,
            cookie_logins_stale: Vec::new(),
            last_tick_age: Some(std::time::Duration::from_secs(5)),
            refresher_stalled: false,
            cache_poisoned: false,
        }
    }

    fn status_of(snapshot: &quota_core::health::HealthSnapshot) -> HealthStatus {
        let ModuleControlResponse::HealthCheck { status, .. } = health_report(snapshot) else {
            panic!("health_report must produce a HealthCheck response");
        };
        status
    }

    /// The health detail is read by a person deciding whether anything needs
    /// attention, so its two counts must not read as one measurement.
    ///
    /// `degraded` counts every provider that failed for any reason — on a host
    /// with credentials for a handful of providers that is most of them, and
    /// entirely normal. The cookie count is narrower: browser logins that went
    /// stale, deliberately excluding services never logged into. Rendering them
    /// as "N/M degraded (K of C cookie-cohort)" invites reading K as the cookie
    /// providers that are degraded, which is a different and larger number.
    #[test]
    fn the_health_detail_does_not_conflate_its_three_counts() {
        // Modelled on this host: most providers have no credential at all, a few
        // are genuinely failing, most cookie providers were never logged into,
        // and two live logins went stale.
        let snapshot = quota_core::health::HealthSnapshot {
            providers_total: 35,
            fresh: 7,
            degraded: (0..4).map(|index| format!("broken-{index}")).collect(),
            unconfigured: (0..24).map(|index| format!("unused-{index}")).collect(),
            cookie_cohort_total: 9,
            cookie_logins_stale: vec!["opencodego".into(), "qoder".into()],
            ..healthy_snapshot()
        };

        let ModuleControlResponse::HealthCheck { detail, .. } = health_report(&snapshot) else {
            panic!("health_report must produce a HealthCheck response");
        };
        let detail = detail.expect("a degraded provider produces a detail line");

        // Each count is attached to what it measures.
        assert!(detail.contains("7 serving"), "detail: {detail}");
        assert!(detail.contains("4 degraded"), "detail: {detail}");
        assert!(detail.contains("24 unconfigured of 35"), "detail: {detail}");
        assert!(
            detail.contains("2 of 9 cookie logins stale"),
            "detail: {detail}"
        );

        // Not vacuous in two directions. The phrasing that invited reading the
        // cookie count as a degraded count must not come back -- the two are
        // independent, and a reader who conflates them sees a quarter of the real
        // number. And the operator count must not silently absorb the
        // unconfigured providers again, which is what made this line a permanent
        // alarm: 28 of 35 on a host where four things were actually wrong.
        assert!(
            !detail.contains("cookie-cohort"),
            "the cohort count is being rendered as a degraded count: {detail}"
        );
        assert!(
            !detail.contains("28 degraded") && !detail.contains("28/35"),
            "unconfigured providers are being counted as degraded: {detail}"
        );
    }

    /// The status this module reports is not a label: the daemon branches on
    /// it, and its default action for `failing` is to restart the module. So a
    /// fault reported one step too high restarts a module that is serving, and
    /// one reported too low leaves a faulted module running untouched.
    ///
    /// Each fault is asserted to produce its own status. Asserting only that
    /// some status is produced would pass if any two of them were swapped.
    #[test]
    fn each_fault_reports_the_status_the_daemon_acts_on() {
        assert_eq!(status_of(&healthy_snapshot()), HealthStatus::Ok);

        // A poisoned store means the data path is faulted: a task panicked
        // while holding the lock and nothing will serve again without a
        // restart, which is the one condition worth restarting for.
        let poisoned = quota_core::health::HealthSnapshot {
            cache_poisoned: true,
            ..healthy_snapshot()
        };
        assert_eq!(status_of(&poisoned), HealthStatus::Failing);

        // A stalled refresher still serves its last-known windows -- only
        // freshness decays -- so it must NOT reach the restart action.
        let stalled = quota_core::health::HealthSnapshot {
            refresher_stalled: true,
            ..healthy_snapshot()
        };
        assert_eq!(status_of(&stalled), HealthStatus::Degraded);

        // Both at once takes the higher of the two rather than the later
        // branch: a poisoned store is not made less severe by also being stale.
        let both = quota_core::health::HealthSnapshot {
            cache_poisoned: true,
            refresher_stalled: true,
            ..healthy_snapshot()
        };
        assert_eq!(status_of(&both), HealthStatus::Failing);

        // Every provider degraded is the resting state of a host that has
        // credentials for none of them. It is not a module fault and must not
        // be reported as one, or this module restarts forever on a laptop that
        // is behaving exactly as expected.
        let all_degraded = quota_core::health::HealthSnapshot {
            fresh: 0,
            degraded: vec!["a".into(), "b".into(), "c".into()],
            ..healthy_snapshot()
        };
        assert_eq!(status_of(&all_degraded), HealthStatus::Ok);
    }

    fn temp_config_path(label: &str) -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "ck-quota-config-{label}-{}-{id}.jsonc",
            std::process::id()
        ))
    }

    /// Every path out of the usage handler sends the consumer a frame.
    ///
    /// The frame loop discards this handler's error to stay alive, so a path
    /// that returns without writing leaves the consumer with no response at
    /// all -- waiting out its own timeout with nothing saying why. This surface
    /// promises every poll either answers or names its failure, and silence is
    /// neither.
    ///
    /// The two request-shaped failures are driven here. The two build failures
    /// (serialization and framing) are backstops: serialization of these types
    /// cannot fail, and framing fails only on a body past the protocol's 64 MiB
    /// cap, so neither is reachable from a unit test. They send an error frame
    /// rather than returning for the same reason these do.
    #[tokio::test]
    async fn every_exit_from_the_usage_handler_answers_the_consumer() {
        for (label, body) in [
            ("undecodable body", b"not json at all".to_vec()),
            (
                "unknown method",
                serde_json::to_vec(&serde_json::json!({
                    "method": "usage.nope",
                    "params": {}
                }))
                .unwrap(),
            ),
        ] {
            let (tx, mut rx) = mpsc::channel::<Frame>(4);
            let frame = Frame::build_with_version(
                PROTOCOL_VERSION,
                FrameType::Request,
                Flags::new(false, Priority::Interactive, false),
                9,
                3,
                77,
                body,
            )
            .unwrap();

            let registry = Arc::new(quota_core::Registry::new(Vec::new()));
            let _ = handle_usage_request(frame, &tx, &registry).await;

            let reply = rx
                .try_recv()
                .unwrap_or_else(|_| panic!("{label}: the consumer received nothing"));
            assert_eq!(reply.header.ty, FrameType::Error, "{label}");
            // The route identity must be echoed or the daemon drops the frame,
            // leaving the consumer no better off than with silence.
            assert_eq!(reply.header.channel, 9, "{label}");
            assert_eq!(reply.header.epoch, 3, "{label}");
            assert_eq!(reply.header.corr, 77, "{label}");

            // Not vacuous: the failure is named, so this cannot pass on an
            // empty error body.
            let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert!(!error.code.is_empty(), "{label}");
            assert!(!error.message.is_empty(), "{label}");
        }
    }

    /// An unknown method is rejected with a reply that stays small.
    ///
    /// The method name is echoed back so the caller can see what was rejected,
    /// and it comes from the request -- nothing upstream bounds it. The reply
    /// must not grow with it: the frame layer caps a body at 64 MiB, and a
    /// request just under that cap would otherwise produce an error reply just
    /// over it. That reply cannot be built, so the caller receives nothing and
    /// waits out its own timeout -- a clear rejection turned into silence.
    ///
    /// Driven here at a size that is fast to run rather than at the cap itself;
    /// the property under test is that the reply size does not follow the
    /// request's, which a bounded echo gives at every size.
    #[tokio::test]
    async fn an_oversized_method_name_still_receives_an_error_reply() {
        let (tx, mut rx) = mpsc::channel::<Frame>(4);
        let body = serde_json::to_vec(&serde_json::json!({
            "method": "x".repeat(1024 * 1024),
        }))
        .unwrap();
        let frame = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            7,
            3,
            61,
            body,
        )
        .unwrap();

        let registry = Arc::new(quota_core::Registry::new(Vec::new()));
        handle_usage_request(frame, &tx, &registry)
            .await
            .expect("the rejection must be deliverable");

        let reply = rx.try_recv().expect("the caller received nothing");
        assert_eq!(reply.header.ty, FrameType::Error);
        assert_eq!(reply.header.channel, 7);
        assert_eq!(reply.header.epoch, 3);

        let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(error.code, "unknown_method");
        // Not vacuous: the reply still names what was rejected and says the
        // text was cut, so this cannot pass by dropping the echo entirely.
        assert!(error.message.contains("unknown method 'xxx"));
        assert!(error.message.contains("more bytes]"), "{}", error.message);
        assert!(
            reply.body.len() < 4096,
            "reply body {} bytes",
            reply.body.len()
        );
    }

    /// A control body this module cannot decode is answered, not escalated.
    ///
    /// An error returned from the control handler reaches the frame loop, which
    /// treats it as fatal and ends the connection -- so one unrecognised body
    /// would take the module down and lose every provider's usage until the
    /// supervisor restarts it.
    ///
    /// This is reachable without anything being broken: the daemon may add a
    /// control op, or a field to an existing one, and a module built against the
    /// older definition cannot decode it. The daemon handles an error reply
    /// gracefully, so answering keeps the module serving while the mismatch
    /// stays visible.
    #[tokio::test]
    async fn an_undecodable_control_body_is_answered_rather_than_fatal() {
        for (label, body) in [
            ("not json", b"{{{".to_vec()),
            (
                "unknown op from a newer daemon",
                serde_json::to_vec(&serde_json::json!({ "op": "route.rebind" })).unwrap(),
            ),
            (
                "known op missing a required field",
                serde_json::to_vec(&serde_json::json!({ "op": "route.bind" })).unwrap(),
            ),
        ] {
            let (tx, mut rx) = mpsc::channel::<Frame>(4);
            let frame = Frame::build_with_version(
                PROTOCOL_VERSION,
                FrameType::Request,
                control_flags(),
                0,
                0,
                51,
                body,
            )
            .unwrap();

            let registry = Arc::new(quota_core::Registry::new(Vec::new()));
            let result = handle_control_request(frame, &tx, &registry).await;

            // The handler must not signal failure upward: that is what the frame
            // loop treats as fatal.
            assert!(result.is_ok(), "{label}: escalated to the frame loop");

            let reply = rx
                .try_recv()
                .unwrap_or_else(|_| panic!("{label}: the daemon received nothing"));
            assert_eq!(reply.header.ty, FrameType::Error, "{label}");
            assert_eq!(reply.header.channel, 0, "{label}");
            assert_eq!(reply.header.corr, 51, "{label}");

            // Not vacuous: the reply names the failure in the daemon's own
            // vocabulary, so this cannot pass on an empty error.
            let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(error.code, "invalid_control_body", "{label}");
            assert!(!error.message.is_empty(), "{label}");
        }
    }

    #[test]
    fn quota_config_accepts_jsonc_and_unknown_fields() {
        let config = parse_quota_config(
            r#"{
                // startup-only reset policy
                "codex": { "auto_use_resets": 86400, },
                "future_provider": true,
            }"#,
        )
        .unwrap();
        assert_eq!(config.codex.auto_use_resets, 86_400);
    }

    #[test]
    fn absent_or_malformed_quota_config_defaults_off() {
        let absent = temp_config_path("absent");
        let malformed = temp_config_path("malformed");
        std::fs::write(&malformed, "{ not-jsonc").unwrap();

        assert_eq!(load_quota_config_file(&absent), QuotaConfig::default());
        assert_eq!(load_quota_config_file(&malformed), QuotaConfig::default());

        let _ = std::fs::remove_file(malformed);
    }

    /// Drive the REAL channel-0 control handler with a `health.check` Request and
    /// assert it answers with a well-formed `HealthCheck` Response carrying the
    /// domain metrics. Exercises the actual arm + registry + mapper, not a mock.
    #[tokio::test]
    async fn health_check_control_request_returns_domain_report() {
        let registry = Arc::new(Registry::with_defaults(QuotaConfig::default(), None));
        let (tx, mut rx) = mpsc::channel::<Frame>(4);

        let request = ModuleControlRequest::HealthCheck {};
        let frame = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Request,
            control_flags(),
            0,
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
        // Every published key, not a sample: consumers are told these exist, and
        // a field silently dropped from the payload is invisible to a test that
        // lists only some of them. The equality check is what makes it an
        // enumeration -- a `contains_key` loop over a subset would still pass
        // with a key missing.
        let expected = [
            "buildCommit",
            "providersTotal",
            "fresh",
            "stale",
            "pending",
            "degraded",
            "unconfigured",
            "withoutHandles",
            "cookieCohortTotal",
            "cookieLoginsStale",
            "lastTickAgeSecs",
            "refresherStalled",
        ];
        let mut published: Vec<&str> = obj.keys().map(String::as_str).collect();
        published.sort_unstable();
        let mut wanted = expected;
        wanted.sort_unstable();
        assert_eq!(
            published, wanted,
            "the published metric keys changed: update docs/consumer-contract.md too"
        );
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
        let registry = Arc::new(Registry::with_defaults(QuotaConfig::default(), None));
        let (tx, mut rx) = mpsc::channel::<Frame>(4);

        let bind = serde_json::json!({
            "op": "route.bind",
            "route_channel": 7,
            "epoch": 1,
            "target": { "kind": "management_surface", "module_id": DEFAULT_MODULE_ID },
            "identity": { "project_root": "/tmp/x", "harness": "test", "session": "s1" }
        });
        let frame = Frame::build_with_version(
            PROTOCOL_VERSION,
            FrameType::Request,
            control_flags(),
            0,
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
}
