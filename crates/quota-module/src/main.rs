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

use std::{
    error::Error,
    ffi::OsString,
    fmt,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use quota_core::{config::QuotaConfig, credential_source::CredentialSource, Registry, LOG_TAG};
use serde::Deserialize;
use serde_json::json;
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, IdentityBinding, ManagementOperation, ManagementOperationKind,
        ModuleManifest, ProviderRole, SelfSignalDeclaration, SelfSignalEffect, SelfSignalKind,
        SignalAnchor, SignalCadence, StorageBinding, StorageKind, StorageScope, TrustTier,
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

/// Recently observed quota drops, as a cursor-paged ring.
///
/// A SECOND OPERATION rather than a field on `usage.get`, because a drop is an
/// EVENT and that response is a statement of current state. Folding events into
/// it would make one poll answer two questions with different retention rules --
/// and a consumer polling for state would silently consume events it had no
/// cursor for.
const USAGE_DROPS_OP: &str = "usage.drops";
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

/// Answer `--version` / `--help` before anything else, and exit.
///
/// This binary is normally SPAWNED BY THE DAEMON with a connection file, so
/// every other path here assumes supervision. But an installer has to identify
/// what it just placed on disk before any daemon exists to spawn it, and a human
/// who runs it directly deserves better than an error about a flag they have
/// never heard of.
///
/// Handled ahead of `ModuleConfig::from_env` deliberately: that call requires
/// `--subc` and fails without it, so asking this binary its version used to
/// print `--subc <connection-file> is required` and exit 1 -- a true statement
/// about a question nobody asked, and indistinguishable from a broken install to
/// the script that asked.
fn answer_informational_flag(args: &[OsString]) -> Option<String> {
    for arg in args {
        match arg.to_str() {
            Some("--version" | "-V") => {
                return Some(format!("{} {}", BINARY_NAME, env!("CARGO_PKG_VERSION")))
            }
            Some("--help" | "-h") => {
                return Some(format!(
                    "{name} {version}\n\n\
                     Quota and balance readings for AI provider accounts, served to the\n\
                     subc daemon as module `{module}`.\n\n\
                     USAGE:\n    \
                       {name} --subc <connection-file>\n\n\
                     This binary is normally started BY the daemon, which supplies the\n\
                     connection file and the module id. Running it by hand is useful only\n\
                     for --version.\n\n\
                     FLAGS:\n    \
                       -V, --version    print the version and exit\n    \
                       -h, --help       print this message and exit\n\n\
                     ENVIRONMENT:\n    \
                       SUBC_MODULE_ID   module id to register under (default: {module})\n\
                       CK_QUOTA_STATE_DIR  redemption journal directory",
                    name = BINARY_NAME,
                    version = env!("CARGO_PKG_VERSION"),
                    module = DEFAULT_MODULE_ID,
                ))
            }
            _ => {}
        }
    }
    None
}

/// The installed file name, which is NOT the crate name.
///
/// The package is `quota-module` and the binary is `ck-insula`; an installer and
/// a human both know the second. Taken from the target rather than restated, so
/// a rename cannot leave this reporting the old name.
const BINARY_NAME: &str = env!("CARGO_BIN_NAME");

#[tokio::main]
async fn main() -> Result<(), ModuleError> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if let Some(message) = answer_informational_flag(&args) {
        println!("{message}");
        return Ok(());
    }
    let config = ModuleConfig::from_env()?;
    let quota_config = load_quota_config();
    eprintln!(
        "{LOG_TAG} codex banked resets armed={} auto_use_resets={}s (startup-only; arm one host per account)",
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
    quota_config_path_from(|key| std::env::var_os(key))
}

/// Resolve the config path over an arbitrary environment.
///
/// The order matches the subc daemon's own `default_config_path`, deliberately:
/// this is a file WE own, and an operator configuring the fleet should find
/// every module's config where the daemon's already is. Third-party credential
/// files are the opposite case and follow the tool that writes them.
///
/// **`HOME` alone is not enough.** It is normally unset for a native Windows
/// process, so reading it directly returns no path there and the config is never
/// loaded. That failure is silent in the worst way available here: an absent
/// config is a legitimate state meaning "no overrides", so banked-reset
/// auto-consume simply stays off while the module reports healthy and the wire
/// carries no field saying the feature was configured and not read.
///
/// The lookup is injected so the Windows arms are exercisable on any host. A
/// branch testable only where it runs is one nobody checks until someone reports
/// that a setting has no effect -- and the report would be about the feature,
/// not about the path.
fn quota_config_path_from(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(config_home) = lookup("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(config_home).join(QUOTA_CONFIG_RELATIVE_PATH));
    }
    if let Some(app_data) = lookup("APPDATA").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(app_data).join(QUOTA_CONFIG_RELATIVE_PATH));
    }
    if let Some(profile) = lookup("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(
            PathBuf::from(profile)
                .join("AppData")
                .join("Roaming")
                .join(QUOTA_CONFIG_RELATIVE_PATH),
        );
    }
    lookup("HOME")
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
                "{LOG_TAG} warning: {} unavailable or malformed ({error}); codex banked resets default OFF",
                path.display()
            );
            QuotaConfig::default()
        }
    }
}

fn load_quota_config() -> QuotaConfig {
    let Some(path) = quota_config_path() else {
        eprintln!(
            "{LOG_TAG} warning: cannot resolve the quota config path; codex banked resets default OFF"
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

    // Held concretely as well as behind the trait: the health path reads this
    // client's own frame-drop counters, which the CredentialSource trait does
    // not expose because they are a property of THIS transport rather than of
    // credential lookup.
    let vault = Arc::new(VaultClient::new(config.connection_file_path.clone()));
    // Process-lifetime consumption counters. Reset on restart like every other
    // process-lifetime metric here, which is why the AGES matter more than the
    // totals for an operator: a fresh process legitimately shows zero served.
    let serve = Arc::new(ServeCounters::default());
    let credential_source: Arc<dyn CredentialSource> = vault.clone();
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

    let loop_result = module_loop(
        &mut read_half,
        tx.clone(),
        &config,
        Arc::clone(&registry),
        Arc::clone(&vault),
        Arc::clone(&serve),
    )
    .await;
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
    vault: Arc<VaultClient>,
    serve: Arc<ServeCounters>,
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
        if !handle_frame(frame, &writer, &registry, &vault, &serve).await? {
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
/// Counts of route requests this process answered and refused.
///
/// EVERY OTHER HEALTH METRIC THIS MODULE PUBLISHES IS PRODUCER-SIDE -- am I
/// fetching, am I fresh, is the refresher ticking. None of them answers whether
/// anything is READING. If every consumer on a host stopped dialling an hour ago
/// (route died, client wedged, their delivery plane starved), health would read
/// exactly as it does when all is well: ok, serving, blackout false. Perfectly
/// healthy and talking to nobody.
///
/// The queue-backed answer does not transfer here. A push surface can count rows
/// owed and undelivered; `usage.get` is PULL, so there is no owed row and no
/// undelivered anything -- the only evidence a consumer exists is that it asked.
///
/// SPLIT SERVED FROM REFUSED because `last_served_age` alone cannot separate
/// nobody-asked from asked-and-failed: a consumer whose every request is refused
/// (bad body, unknown method, unframeable reply) never advances a served
/// watermark, and looks identical to silence. With both, the three states an
/// operator triages -- quiet, consuming, failing-to-consume -- are each one read.
///
/// DELIBERATELY NOT AN ALARM. A host with nobody polling is a legitimate
/// configuration, so a "no consumers" degraded state would fire on every quiet
/// host and train the surface into noise. These are numbers for a reader who has
/// context this module cannot have; the producer reports, the reader judges.
/// The four header fields every route reply must echo, carried as one value.
///
/// They are not four independent arguments -- they are the identity of the
/// request being answered, and the daemon drops a reply that gets any of them
/// wrong as belonging to a stale binding. Bundling them means a new reply site
/// cannot pass three of four, and keeps the error helper under the argument
/// count where a reader stops tracking positional meaning.
#[derive(Debug, Clone, Copy)]
struct RouteReply {
    ver: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
}

#[derive(Debug, Default)]
struct ServeCounters {
    served: AtomicU64,
    refused: AtomicU64,
    /// Monotonic instant of the last answered request, for an age at read time.
    /// Ages are computed live rather than stored, so a stalled reporter cannot
    /// publish a frozen "seconds ago" that keeps looking recent.
    last_served: Mutex<Option<Instant>>,
    last_refused: Mutex<Option<Instant>>,
}

impl ServeCounters {
    fn record_served(&self) {
        self.served.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = self.last_served.lock() {
            *slot = Some(Instant::now());
        }
    }

    fn record_refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut slot) = self.last_refused.lock() {
            *slot = Some(Instant::now());
        }
    }

    fn age_secs(slot: &Mutex<Option<Instant>>) -> Option<u64> {
        slot.lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|at| at.elapsed().as_secs())
    }
}

async fn handle_frame(
    frame: Frame,
    writer: &mpsc::Sender<Frame>,
    registry: &Arc<Registry>,
    vault: &Arc<VaultClient>,
    serve: &Arc<ServeCounters>,
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
            handle_control_request(frame, writer, registry, vault, serve).await?;
            Ok(true)
        }
        FrameType::Request => {
            // Data-plane request on a route channel. Spawn so a slow fetch never
            // head-of-line-blocks another route (ManagementSurface concurrency).
            let writer = writer.clone();
            let registry = Arc::clone(registry);
            let serve = Arc::clone(serve);
            tokio::spawn(async move {
                let _ = handle_usage_request(frame, &writer, &registry, &serve).await;
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
    vault: &VaultClient,
    serve: &ServeCounters,
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
            health_report(&registry.health(), vault, serve)
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
    let body = serde_json::to_vec(&ErrorBody::new(code, message)).map_err(ModuleError::Json)?;
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
fn health_report(
    snapshot: &quota_core::health::HealthSnapshot,
    vault: &VaultClient,
    serve: &ServeCounters,
) -> ModuleControlResponse {
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
        // CONSUMPTION, not production. Every other field here answers "am I
        // producing correctly"; these four answer "is anyone reading", which
        // nothing else on this surface could. A host whose consumers all stopped
        // dialling an hour ago reports identically to a healthy one on every
        // other metric -- serving, fresh, blackout false, refresher ticking.
        //
        // NOT ALARMED, and that is deliberate. A host with nobody polling is a
        // legitimate configuration, so degrading on it would fire everywhere and
        // train the surface into noise. These are for a reader who has context
        // the producer cannot: the module reports, the operator judges.
        //
        // Served and refused are SEPARATE because an age alone cannot tell
        // nobody-asked from asked-and-failed -- a consumer whose every request is
        // refused never advances the served watermark and looks like silence.
        // With both, quiet / consuming / failing-to-consume are each one read.
        //
        // Ages are computed at read time from a stored instant rather than
        // stored as numbers, so a stalled reporter cannot publish a frozen
        // "seconds ago" that keeps looking recent.
        "usageRequestsServed": serve.served.load(Ordering::Relaxed),
        "usageRequestsRefused": serve.refused.load(Ordering::Relaxed),
        "lastServedAgeSecs": ServeCounters::age_secs(&serve.last_served),
        "lastRefusedAgeSecs": ServeCounters::age_secs(&serve.last_refused),
        "providersTotal": snapshot.providers_total,
        "fresh": snapshot.fresh,
        "stale": snapshot.stale,
        // Monotonic count of stale-serving EPISODES since process start, beside
        // the instantaneous `stale` gauge above. A slot entering stale-serving
        // from any other status is one episode; a slot that stays stale across
        // many refresh turns is still one. This is the trace a transient failure
        // that resolves between two polls would otherwise leave nowhere: `stale`
        // returns to zero while this stays at one. NOT part of the conservation
        // identity -- it counts events over time, not members of a population.
        "staleEpisodes": snapshot.stale_episodes,
        // How those episodes were distributed. The total above says how many;
        // this says how they were spread, and only the pair separates one
        // marginal upstream from an environmental wobble -- seventeen episodes
        // over ten lanes and seventeen on one lane want opposite responses.
        //
        // Replaced a set of NAMES, which saturated: every serving lane flaps
        // eventually, so after enough uptime it listed all of them and stopped
        // discriminating. Counts keep working at any uptime.
        //
        // Membership is since boot, so a provider here is usually healthy now.
        // Not part of the conservation identity.
        "staleEpisodesByProvider": snapshot.stale_episodes_by_provider,
        // How many times an account's used percent went DOWN, per provider.
        //
        // NAMED FOR THE OBSERVATION, NOT THE INFERENCE. A window rollover, a
        // redeemed reset credit, a goodwill grant, a plan change and an upstream
        // correction all look identical from here, so calling these "resets"
        // would state a cause nothing measured. The one cause this module could
        // attribute is its own redemptions, and that belongs with a consumable
        // record rather than a counter.
        //
        // Exists to answer a design question before the record asked for on
        // insula#5 gets built: whether a 60-second poll sees these at all.
        "quotaDropsByProvider": snapshot.quota_drops_by_provider,
        // How many of those were seen across a CONTINUOUS poll interval; the
        // rest were inferred across a gap and understate what happened -- a drop
        // plus later consumption reads smaller than it was, and a drop followed
        // by a re-fill reads as nothing. The RATIO is the finding: if most are
        // inferred, a consumable record has to carry that on every row.
        "quotaDropsObservedContinuously": snapshot.quota_drops_observed_continuously,
        // The denominator the drop counts were missing: comparisons that RAN and
        // found nothing. Without it a low drop count reads identically whether
        // the host was quiet or nothing was comparable.
        "quotaComparisonsNoDrop": snapshot.quota_comparisons_no_drop,
        // Readings that could not be compared, by reason. A long run of
        // `prior_reading_was_an_error` is a credential problem wearing the shape
        // of a quiet host -- an 84-minute latch produces hundreds of these and is
        // silence in every other measure.
        "quotaNotComparable": snapshot.quota_not_comparable,
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
        // Frames this module's own vault client read off the wire and discarded.
        //
        // Both classes are correct to drop -- a reply whose caller has gone, and
        // one arriving for a caller from a previous connection -- but a silent
        // drop and a peer that never answered produce the same observable, a
        // call that waits and times out. Those two have opposite
        // investigations, so the counts are published rather than logged: a
        // non-zero value says the frames reached this process.
        //
        // Ordinary in small numbers around a daemon restart. A rising
        // `vaultUnmatchedDrops` while nothing is restarting is the one worth
        // asking about, because it means replies are arriving that no caller is
        // waiting for.
        "vaultUnmatchedDrops": vault.unmatched_terminal_drops(),
        "vaultStaleGenerationDrops": vault.stale_generation_drops(),
        // Vault connections this process has established. 1 means the first is
        // still in use; each increment is a reconnect after a transport failure.
        // There is no idle timeout or maximum lifetime, so a healthy connection
        // is held for the life of the process.
        //
        // Diagnostic rather than a health signal. It exists because an incident
        // was not narrowable without it: a re-sealed vault record kept being
        // published with its pre-re-seal verdict until a restart, and a restart
        // is the one event in such a timeline that establishes a NEW connection.
        // "Did the connection change?" previously required reading the source.
        "vaultConnectionsEstablished": vault.connections_established(),
        "cookieCohortTotal": snapshot.cookie_cohort_total,
        "cookieLoginsStale": snapshot.cookie_logins_stale,
        // Providers holding a credential that reaches no account while their
        // other accounts serve. Published because nothing else on this surface
        // can express it: the provider is genuinely healthy, so it counts as
        // fresh and every other number here reads normal. It is the state a
        // handle enters when its credential is deleted and the handle is left
        // configured, and it does not clear on its own.
        "handlesWithoutAccount": snapshot.handles_without_account,
        "lastTickAgeSecs": snapshot.last_tick_age.map(|d| d.as_secs()),
        "refresherStalled": snapshot.refresher_stalled,
        // How long since ANY fetch last succeeded, and whether that has gone on
        // long enough to call the module degraded.
        //
        // `refresherStalled` watches the loop; these watch whether the loop
        // accomplishes anything, which is a different claim and was the one
        // nobody checked. On 2026-08-19 this module served 10-hour-old windows
        // with `lastTickAgeSecs` at 1 and every number here reading normal.
        //
        // Null when nothing has ever succeeded, which is the ordinary state of a
        // host holding credentials for none of these services.
        "lastFetchSuccessAgeSecs": snapshot.last_fetch_success_age.map(|d| d.as_secs()),
        "fetchBlackout": snapshot.fetch_blackout,
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
    /// A sequence from a previous drop page. Absent means "everything retained".
    #[serde(default)]
    since: Option<u64>,
}

async fn handle_usage_request(
    frame: Frame,
    writer: &mpsc::Sender<Frame>,
    registry: &Arc<Registry>,
    serve: &Arc<ServeCounters>,
) -> Result<(), ModuleError> {
    let channel = frame.header.channel;
    // The route's binding epoch must be echoed on every reply, or the daemon
    // drops the frame as belonging to a stale binding of this channel slot.
    let epoch = frame.header.epoch;
    let corr = frame.header.corr;
    let ver = frame.header.ver;
    let reply = RouteReply {
        ver,
        channel,
        epoch,
        corr,
    };

    let request: UsageRequest = match serde_json::from_slice(&frame.body) {
        Ok(r) => r,
        Err(e) => {
            return send_route_error(
                writer,
                serve,
                reply,
                "invalid_request",
                &format!("usage request body not decodable: {e}"),
            )
            .await;
        }
    };

    if request.method == USAGE_DROPS_OP {
        let page = registry.drop_page(request.params.since).await;
        let body = match serde_json::to_vec(&page) {
            Ok(body) => body,
            Err(error) => {
                return send_route_error(
                    writer,
                    serve,
                    reply,
                    "internal_error",
                    &format!("drop page not serializable: {error}"),
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
                    serve,
                    reply,
                    "internal_error",
                    &format!("drop page could not be framed: {error}"),
                )
                .await;
            }
        };
        return send(writer, response).await;
    }

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
            serve,
            reply,
            "unknown_method",
            &format!("unknown method '{echoed}', expected '{USAGE_GET_OP}' or '{USAGE_DROPS_OP}'"),
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
                serve,
                reply,
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
                serve,
                reply,
                "internal_error",
                &format!("usage response could not be framed: {error}"),
            )
            .await;
        }
    };
    serve.record_served();
    send(writer, response).await
}

/// Answer a route request with an error frame, and count it as a refusal.
///
/// The counting lives HERE rather than at the six call sites because a return
/// site is easy to add without remembering the counter, and a refusal that is
/// not counted reads on the health surface as a request nobody made -- exactly
/// the confusion the counters exist to remove. Verified at the time of writing
/// that every caller is inside `handle_usage_request`, so this counts route
/// refusals only; a control-plane caller added later would need its own counter
/// rather than borrowing this one.
async fn send_route_error(
    writer: &mpsc::Sender<Frame>,
    serve: &Arc<ServeCounters>,
    reply: RouteReply,
    code: &str,
    message: &str,
) -> Result<(), ModuleError> {
    serve.record_refused();
    let body = serde_json::to_vec(&ErrorBody::new(code, message)).map_err(ModuleError::Json)?;
    let frame = Frame::build_with_version(
        reply.ver,
        FrameType::Error,
        Flags::new(false, Priority::Interactive, false),
        reply.channel,
        reply.epoch,
        reply.corr,
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
/// The commit this binary was built from, or None when that cannot be claimed.
///
/// SEPARATE FROM `health.metrics.buildCommit`, and the separation is the whole
/// point. That stamp is HEAD and is documented as answering "is this build
/// missing commits" -- a build from a dirty tree carries the clean sha verbatim,
/// which is fine for a skew check and wrong here. `ManifestProvenance` is read as
/// an identity claim, so a sha that resolves to bytes which are not the running
/// bytes is a precise-looking wrong answer, and the contract's own rule is that
/// absence beats that at being believed.
///
/// Empty means the build declined to claim one: dirty tree, no git binary, or no
/// resolvable HEAD. All three map to None, because a sentinel must render as
/// absence and never as a value.
fn build_provenance_sha() -> Option<String> {
    let raw = env!("CK_QUOTA_PROVENANCE_SHA");
    (!raw.is_empty()).then(|| raw.to_string())
}

/// A digest of the resolved lockfile, hashed at build time.
///
/// NOT gated on tree cleanliness, unlike the sha. This is a change-detector
/// rather than an identity claim: it answers "are these two builds the same
/// dependency set", which stays true on a dirty tree where a commit does not.
/// That is what lets a development build still be told apart from another one
/// without claiming to be a commit it is not.
fn build_lock_digest() -> Option<String> {
    let raw = env!("CK_QUOTA_LOCK_DIGEST");
    (!raw.is_empty()).then(|| raw.to_string())
}

fn manifest(module_id: &str) -> ModuleManifest {
    // Built through the builder rather than as a literal: `ModuleManifest` is
    // `#[non_exhaustive]` upstream, so a field added there becomes a compile
    // error here instead of a silent default. That is the point of the shape,
    // and the reason not to route around it.
    //
    // EVERY OPTIONAL IS PASSED EXPLICITLY, including the ones that are None.
    // The builder permits omitting them, and omitting `capabilities` would read
    // as "not reached yet" when it is a decision with a paragraph behind it. A
    // deliberate None and an unwritten one are identical on the wire and
    // opposite facts about the author.
    ModuleManifest::builder(
        module_id.to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
        TrustTier::FirstParty,
        Bindings {
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
    )
    .protocol_ver(PROTOCOL_VERSION)
    .provides(
        vec![ProviderRole::ManagementSurface {
            operations: vec![
                ManagementOperation {
                name: USAGE_GET_OP.to_string(),
                kind: ManagementOperationKind::Query,
                // One sentence, because this is discovery metadata a human reads
                // in a catalog listing rather than something a caller branches
                // on. It names the two properties that decide whether a consumer
                // can use this op at all: the read never blocks on a network
                // sweep, and the array it returns may be partial.
                description: Some(
                    "Cache-only read of per-account provider quota; never blocks on a fetch, so the array may be partial."
                        .to_string(),
                ),
                },
                ManagementOperation {
                    name: USAGE_DROPS_OP.to_string(),
                    kind: ManagementOperationKind::Query,
                    description: Some(
                        "Recently observed decreases in an account's used percent, as a cursor-paged in-memory ring."
                            .to_string(),
                    ),
                },
            ],
            config_schema: json!({ "type": "object" }),
            observability: Vec::new(),
            identity_scope: Vec::new(),
            // Every data-plane request is spawned on arrival (see handle_frame),
            // so responses may complete out of order even within one channel, and
            // nothing inbound mutates state -- `usage.get` is a cache-only read of
            // an in-memory snapshot. Declared from that behaviour rather than from
            // preference: claiming stricter ordering than the loop provides would
            // be a promise this module does not keep.
            concurrency: Concurrency::StatelessParallel,
        }]
    )
    .consumes(Vec::new())
    .self_signals(Some(vec![
    // What this module does to the surfaces it reports on, so an analyst
    // measuring provider quota can subtract our own contribution.
    //
    // THE SECOND ENTRY IS WHY THIS FIELD HAS THE SHAPE IT DOES. The kinds
    // first proposed were all observers, and an observer's contribution can
    // be subtracted after the fact once its cadence is known. A MUTATOR has
    // already rewritten the surface's history for every later reader,
    // including ones who never heard of this module: a used-percent falling
    // to zero because we SPENT a banked credit is indistinguishable on the
    // wire from the provider resetting the window, and no consumer can undo
    // a credit we consumed.
        SelfSignalDeclaration {
            name: "provider_refresh".to_string(),
            kind: SelfSignalKind::Poller,
            effect: SelfSignalEffect::Observe,
            // Fixed interval, so a periodicity check finds it unaided.
            // Declared anyway, because "findable in principle" and "found by
            // the person who needed it" are different things.
            anchored_to: SignalAnchor::FixedInterval,
            cadence: Some(SignalCadence::Literal {
                interval_ms: quota_core::refresh::BASE_INTERVAL.as_millis() as u64,
            }),
            domain: Some("provider-usage".to_string()),
            note: Some(
                "Reads every configured provider's usage endpoint. Read-only: it \
                 consumes no quota and changes nothing upstream."
                    .to_string(),
            ),
        },
        SelfSignalDeclaration {
            name: "codex_banked_reset_consume".to_string(),
            kind: SelfSignalKind::Other("mutation".to_string()),
            effect: SelfSignalEffect::Mutate,
            // EVENT-ANCHORED, which is what makes it inseparable by
            // measurement: it fires at the window's own boundary -- near a
            // credit's expiry, or at the rate-limit wall -- so it reproduces
            // the surface's grid exactly. A periodicity check cannot find
            // it, because it has no period of its own.
            anchored_to: SignalAnchor::Event {
                event: "codex quota window at the rate-limit wall, or a banked credit \
                        near expiry"
                    .to_string(),
            },
            // Config-resolved rather than constant: the arming threshold is
            // read from ck-quota.jsonc at STARTUP, so the effective value is
            // whatever that file said when this process began. Naming the
            // source rather than a number keeps the declaration from
            // claiming a cadence it cannot know.
            cadence: Some(SignalCadence::Derived {
                source: "codex.auto_use_resets in ck-quota.jsonc, read at startup"
                    .to_string(),
            }),
            domain: Some("provider-usage".to_string()),
            note: Some(
                "Spends a banked OpenAI reset credit, which ZEROES the account's used \
                 percent upstream. Disabled unless auto_use_resets is set. When \
                 engaged, the entry publishes usedPercent 0 beside the real figure in \
                 rawUsedPercent -- that pair is the per-observation discriminator, and \
                 it is on the wire whether or not this declaration is read."
                    .to_string(),
            ),
        },
    ]))
    // Built through `build_provenance` rather than as a struct literal, and that
    // is the difference between declaring these facts and declaring them in a
    // form anyone can join on. The literal still compiles, so nothing forced
    // this -- it would simply keep publishing a 12-hex sha beside other modules'
    // 40-hex ones, which can never compare equal and read as permanent drift
    // rather than as a format difference. Reported against this module as
    // iceteaSA #87.
    //
    // ON A FORM ERROR THE WHOLE BLOCK IS OMITTED, not defaulted and not fatal: a
    // build that cannot state a conforming fact should say nothing, exactly as a
    // build that could not establish the fact at all does.
    //
    // `build_git_sha` is NOT `health.metrics.buildCommit`, and an earlier version
    // reused it. That stamp is HEAD, so a dirty build carries a clean sha --
    // correct for the skew check it serves and wrong for an identity claim.
    // Reported on insula#12.
    //
    // `store_schema_version` is None as an ABSENT FACT rather than an unfilled
    // blank: this module owns no persistent store. `wire_crate_version` is filled
    // by the constructor from the linked crate, so it is no longer ours to pass.
    .provenance(
        subc_protocol::manifest::build_provenance(
            build_provenance_sha().as_deref(),
            build_lock_digest().as_deref(),
            None,
        )
        .ok(),
    )
    .capabilities(
        // No versioned capability grammar declared. `None` and an empty block are
        // NOT the same statement: the protocol makes this optional precisely so a
        // module can say nothing, and the daemon VALIDATES a present block before
        // accepting a HELLO -- so an inaccurate declaration is a module that does
        // not come up, which is worse than one that stays silent.
        //
        // Saying nothing is currently the accurate answer. The tempting entry is a
        // `requires` on the credential vault, since vault lanes go dark without
        // claustrum -- but that would be wrong twice over: this module degrades
        // rather than fails without it (local lanes keep fetching, and the vault
        // client retries a refused route indefinitely by design), and I have not
        // established what the daemon's validation does with an unsatisfied
        // requirement. Declaring a dependency I have not verified the semantics of
        // trades a working module for a tidier manifest.
        None,
    )
    .build()
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

    #[test]
    fn log_tag_matches_the_daemon_module_id() {
        let expected = format!("[{DEFAULT_MODULE_ID}]");
        assert_eq!(
            LOG_TAG, expected,
            "stderr must carry the exact id operators use to select this module's logs"
        );
    }

    /// The manifest states provenance, and states ONLY what it can source.
    ///
    /// Both halves are load-bearing and the second is the one that rots: a later
    /// edit that "fills in" a field with a plausible constant would pass a test
    /// that only checked presence, and would put a manufactured fact on the wire
    /// under a field whose entire purpose is that its contents were measured.
    ///
    /// THE SHA ASSERTION IS CONDITIONAL BY NECESSITY, not by laziness. Whether a
    /// commit may be claimed is a fact about the tree this test is compiled in,
    /// so a test demanding a sha would fail every development build and a test
    /// demanding absence would fail every release build. What is invariant is the
    /// RULE, so that is what is pinned.
    /// The manifest carries both self-signals, and the mutator is declared as
    /// one.
    ///
    /// WRITTEN BECAUSE NOTHING ELSE WOULD NOTICE A SILENT DROP. This field is
    /// discarded by the deployed daemon (its ModuleManifest predates the
    /// field, so serde drops it at HELLO and registration still succeeds), so
    /// a regression here is invisible from the wire, invisible from health,
    /// and invisible from `catalog.list`. That already made a migration risky:
    /// moving these fields onto the upstream builder is exactly the kind of
    /// mechanical edit where an optional can be dropped without a compiler
    /// complaint, since every setter is optional by construction.
    ///
    /// The MUTATOR entry is the one worth pinning by name. An observer's
    /// contribution can be subtracted after the fact; a mutator has already
    /// rewritten the surface's history for every later reader. Losing that
    /// declaration would leave this module spending banked credits while
    /// telling the fleet it only observes.
    #[test]
    fn manifest_declares_both_self_signals_including_the_mutator() {
        let m = manifest("insula");
        let signals = m
            .self_signals
            .expect("manifest must declare self-signals, not stay silent");

        let names: Vec<&str> = signals.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["provider_refresh", "codex_banked_reset_consume"],
            "both signals must survive: an observer and the credit-spending mutator"
        );

        let mutators: Vec<&str> = signals
            .iter()
            .filter(|s| matches!(s.effect, SelfSignalEffect::Mutate))
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            mutators,
            vec!["codex_banked_reset_consume"],
            "the reset consumer must be declared as a MUTATOR; downgrading it to \
                 an observer tells the fleet its effect can be subtracted after the fact"
        );
    }

    #[test]
    fn manifest_provenance_states_what_it_can_source_and_nothing_else() {
        let m = manifest("insula");
        let p = m.provenance.expect("manifest must state provenance");

        match p.build_git_sha.as_deref() {
            Some(sha) => {
                assert!(
                    !env!("CK_QUOTA_PROVENANCE_SHA").is_empty(),
                    "a sha was declared from an empty stamp -- a sentinel escaped as a value"
                );
                assert!(
                    sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()),
                    "build_git_sha must be a hex commit, got {sha:?}"
                );
            }
            None => assert!(
                env!("CK_QUOTA_PROVENANCE_SHA").is_empty(),
                "the build emitted a provenance sha and the manifest dropped it"
            ),
        }

        // The defect this test exists to fence: build_git_sha was once
        // health.metrics.buildCommit, which is HEAD and therefore carries a clean
        // sha out of a dirty tree. The two answer different questions and must be
        // free to disagree; reusing one for both read as obviously correct.
        assert!(
            p.build_git_sha.is_none() || !env!("CK_QUOTA_PROVENANCE_SHA").is_empty(),
            "provenance must come from its own stamp, not the health one"
        );

        // Unconditional, because a lockfile digest is a change-detector rather
        // than an identity claim and stays true on a dirty tree.
        let digest = p
            .build_lock_digest
            .expect("build_lock_digest is hashed at build time from Cargo.lock");
        // 64 lowercase hex, the form subc-protocol validates. Asserted here as
        // well as upstream because the constructor OMITS a non-conforming value
        // rather than failing: a build that regressed to a shorter digest would
        // publish no digest at all, and "absent" is a legitimate state that no
        // consumer can distinguish from "never stamped".
        assert!(
            digest.len() == 64
                && digest
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "build_lock_digest must be 64 lowercase hex, got {digest:?}"
        );

        // THE REFERENT IS THE FLEET'S WIRE CRATE. Asserted by identity against the
        // linked constant, not by shape: a semver-shaped check passes just as
        // happily on our own provider-usage version, which is what this field
        // previously carried and what a census gate would have misread.
        //
        // Still asserted although `build_provenance` now fills this itself. The
        // value moved upstream; the CLAIM this module publishes did not, and a
        // test that stops checking a published field because someone else
        // computes it is trusting a dependency to keep meaning what it meant.
        let wire = p
            .wire_crate_version
            .expect("wire_crate_version is the linked subc-protocol version");
        assert_eq!(
            wire,
            subc_protocol::SUBC_PROTOCOL_CRATE_VERSION,
            "must declare the SUBC-PROTOCOL version this binary links, not our own \
             envelope crate -- the two are in different numbering spaces"
        );

        // An absent fact, not an unfilled blank -- see the comment at the call site.
        assert!(
            p.store_schema_version.is_none(),
            "this module owns no persistent store, so it has no schema version to state"
        );
    }

    /// Every health metric this module publishes is explained in the contract.
    ///
    /// The metrics are the operational surface three teams alert on, and
    /// `docs/consumer-contract.md` is where they read what each key means. A key
    /// published with nothing to read is a number an operator has to guess at,
    /// and guessing produces either a false alarm or a missed one.
    ///
    /// Nothing forces the doc to keep up. Two keys were added since it was
    /// written -- `staleEpisodes` and `handlesWithoutAccount` -- and both got
    /// documented because someone remembered, which is not a mechanism. This is.
    ///
    /// ONE DIRECTION ONLY, deliberately. The reverse check -- a documented key
    /// nothing publishes -- cannot be written the way the errorClass fence is,
    /// because these keys are explained in prose across several sections rather
    /// than in one table, so there is no anchor that separates a metric name from
    /// any other backticked field. Checked by hand instead: the three
    /// metric-shaped tokens that are not published (`availableCount`,
    /// `expiresAt`, `soonestExpiresAt`) are all real fields on `SavedResets`, so
    /// the reverse drift is not currently a live risk. Revisit if the doc grows a
    /// metrics table worth scanning.
    #[test]
    fn every_published_health_metric_is_documented() {
        let ModuleControlResponse::HealthCheck { metrics, .. } = health_report(
            &healthy_snapshot(),
            &test_vault(),
            &ServeCounters::default(),
        ) else {
            panic!("health_report must answer a HealthCheck");
        };
        let metrics = metrics.expect("metrics are always published");
        let keys: Vec<String> = metrics
            .as_object()
            .expect("the metrics payload is an object")
            .keys()
            .cloned()
            .collect();

        let contract = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../docs/consumer-contract.md"),
        )
        .expect("consumer-contract.md must be readable from the crate directory");

        for key in &keys {
            assert!(
                contract.contains(&format!("`{key}`")),
                "health metric `{key}` is published with no explanation in \
                 docs/consumer-contract.md: an operator reads the number and guesses"
            );
        }

        // Not vacuous: a renamed heading or an emptied payload would satisfy the
        // loop above by iterating nothing.
        assert!(
            keys.len() >= 17,
            "expected the full metrics payload, found {} key(s): {keys:?}",
            keys.len()
        );
    }

    /// The config path follows the daemon's own resolution order.
    ///
    /// Each arm is asserted separately because they are tried in sequence, and a
    /// test that only ever supplies one variable cannot tell a correct order
    /// from an accidental one.
    #[test]
    fn the_config_path_resolves_like_the_daemon() {
        let os = std::ffi::OsString::from;

        // XDG wins wherever it is set, on every platform.
        let xdg = quota_config_path_from(|key| match key {
            "XDG_CONFIG_HOME" => Some(os("/tmp/xdg")),
            "APPDATA" => Some(os(r"C:\roaming")),
            "HOME" => Some(os("/home/qta")),
            _ => None,
        });
        assert_eq!(
            xdg,
            Some(PathBuf::from("/tmp/xdg/cortexkit/ck-quota.jsonc"))
        );

        // Windows without XDG: the roaming directory, not HOME.
        let roaming = quota_config_path_from(|key| match key {
            "APPDATA" => Some(os(r"C:\roaming")),
            _ => None,
        })
        .expect("APPDATA must resolve a path");
        assert!(
            roaming
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("roaming/cortexkit/ck-quota.jsonc"),
            "{roaming:?}"
        );

        // Stripped Windows environment: reconstruct what APPDATA would hold.
        let profile = quota_config_path_from(|key| match key {
            "USERPROFILE" => Some(os(r"C:\Users\qta")),
            _ => None,
        })
        .expect("USERPROFILE must resolve a path");
        assert!(
            profile
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("Users/qta/AppData/Roaming/cortexkit/ck-quota.jsonc"),
            "{profile:?}"
        );

        // Unix stays exactly as it was.
        let unix = quota_config_path_from(|key| match key {
            "HOME" => Some(os("/home/qta")),
            _ => None,
        });
        assert_eq!(
            unix,
            Some(PathBuf::from("/home/qta/.config/cortexkit/ck-quota.jsonc"))
        );

        // Nothing set at all: no path rather than a relative one, which would
        // resolve against the working directory of whoever spawned the module.
        assert_eq!(quota_config_path_from(|_| None), None);
    }

    /// A Windows environment must not fall through to the Unix arm.
    ///
    /// The regression this pins is the original defect: reading `HOME` alone,
    /// which is normally unset on Windows, so the config is never found and
    /// banked-reset auto-consume stays off with nothing reporting why.
    #[test]
    fn a_windows_environment_never_returns_a_unix_path() {
        let path = quota_config_path_from(|key| match key {
            "APPDATA" => Some(std::ffi::OsString::from(r"C:\roaming")),
            _ => None,
        })
        .expect("a windows environment must resolve a path");

        assert!(
            !path.to_string_lossy().contains("/.config/"),
            "resolved a Unix path on a Windows environment: {path:?}"
        );
    }

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    /// A vault client that never connects.
    ///
    /// The health path reads only this client's frame-drop counters, which start
    /// at zero and need no connection to be read -- so a client pointed at a
    /// path that does not exist is the right fixture rather than a mock.
    fn test_vault() -> VaultClient {
        VaultClient::new(std::path::PathBuf::from("/nonexistent/connection.json"))
    }

    /// A snapshot of a module serving normally, to be perturbed one field at a
    /// time by the tests that care about a single fault.
    fn healthy_snapshot() -> quota_core::health::HealthSnapshot {
        quota_core::health::HealthSnapshot {
            providers_total: 3,
            fresh: 3,
            stale: 0,
            stale_episodes: 0,
            stale_episodes_by_provider: std::collections::BTreeMap::new(),
            quota_drops_by_provider: std::collections::BTreeMap::new(),
            quota_drops_observed_continuously: 0,
            quota_comparisons_no_drop: 0,
            quota_not_comparable: std::collections::BTreeMap::new(),
            pending: 0,
            degraded: Vec::new(),
            unconfigured: Vec::new(),
            without_handles: Vec::new(),
            cookie_cohort_total: 0,
            cookie_logins_stale: Vec::new(),
            handles_without_account: Vec::new(),
            last_tick_age: Some(std::time::Duration::from_secs(5)),
            refresher_stalled: false,
            last_fetch_success_age: Some(std::time::Duration::from_secs(5)),
            fetch_blackout: false,
            cache_poisoned: false,
        }
    }

    fn status_of(snapshot: &quota_core::health::HealthSnapshot) -> HealthStatus {
        let ModuleControlResponse::HealthCheck { status, .. } =
            health_report(snapshot, &test_vault(), &ServeCounters::default())
        else {
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

        let ModuleControlResponse::HealthCheck { detail, .. } =
            health_report(&snapshot, &test_vault(), &ServeCounters::default())
        else {
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
            let serve = Arc::new(ServeCounters::default());
            let _ = handle_usage_request(frame, &tx, &registry, &serve).await;

            // Every case in this table is a REFUSAL, so the refused counter must
            // advance and the served one must not. Asserted here rather than in a
            // dedicated test because a counter that is merely PUBLISHED, and never
            // driven, passes a key-set fence perfectly while counting nothing --
            // and the whole point of the pair is that an operator can tell
            // failing-to-consume from nobody-asked.
            assert_eq!(
                serve.refused.load(Ordering::Relaxed),
                1,
                "{label}: a refusal must be counted"
            );
            assert_eq!(
                serve.served.load(Ordering::Relaxed),
                0,
                "{label}: a refusal must not count as served"
            );
            assert!(
                ServeCounters::age_secs(&serve.last_refused).is_some(),
                "{label}: a refusal must stamp last_refused"
            );
            assert!(
                ServeCounters::age_secs(&serve.last_served).is_none(),
                "{label}: a refusal must leave last_served unset"
            );

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
    /// A well-formed `usage.get` advances the SERVED counter, not the refused one.
    ///
    /// The refusal arm is covered by the table above, and covering only that arm
    /// would leave the pair half-proved in the direction that matters least: a
    /// module counting every request as refused still tells an operator that
    /// something is happening. A module that counts NOTHING as served, while
    /// consumers are being answered normally, reports permanent silence on a
    /// healthy host -- the exact wrong belief these counters exist to prevent.
    ///
    /// Driven with an empty registry, because the counter is about the REQUEST
    /// being answered rather than about what the answer contains: an empty
    /// provider set is still a served request.
    #[tokio::test]
    async fn a_well_formed_usage_request_counts_as_served() {
        let (tx, mut rx) = mpsc::channel::<Frame>(4);
        let body = serde_json::to_vec(&serde_json::json!({
            "method": USAGE_GET_OP,
            "params": {}
        }))
        .unwrap();
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
        let serve = Arc::new(ServeCounters::default());
        handle_usage_request(frame, &tx, &registry, &serve)
            .await
            .expect("a well-formed request is answered");

        let reply = rx.try_recv().expect("the consumer received a reply");
        assert_eq!(reply.header.ty, FrameType::Response);
        assert_eq!(
            serve.served.load(Ordering::Relaxed),
            1,
            "an answered request must be counted as served"
        );
        assert_eq!(
            serve.refused.load(Ordering::Relaxed),
            0,
            "an answered request must not be counted as refused"
        );
        assert!(
            ServeCounters::age_secs(&serve.last_served).is_some(),
            "an answered request must stamp last_served"
        );
        assert!(
            ServeCounters::age_secs(&serve.last_refused).is_none(),
            "an answered request must leave last_refused unset"
        );
    }

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
        handle_usage_request(frame, &tx, &registry, &Arc::new(ServeCounters::default()))
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
            let vault = VaultClient::new(std::path::PathBuf::from("/nonexistent"));
            let result =
                handle_control_request(frame, &tx, &registry, &vault, &ServeCounters::default())
                    .await;

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

        handle_control_request(
            frame,
            &tx,
            &registry,
            &test_vault(),
            &ServeCounters::default(),
        )
        .await
        .unwrap();

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
            "staleEpisodesByProvider",
            "quotaDropsByProvider",
            "quotaDropsObservedContinuously",
            "quotaComparisonsNoDrop",
            "quotaNotComparable",
            "staleEpisodes",
            "pending",
            "degraded",
            "unconfigured",
            "withoutHandles",
            "cookieCohortTotal",
            "cookieLoginsStale",
            "handlesWithoutAccount",
            "lastTickAgeSecs",
            "fetchBlackout",
            "lastFetchSuccessAgeSecs",
            "refresherStalled",
            "vaultUnmatchedDrops",
            "vaultStaleGenerationDrops",
            "vaultConnectionsEstablished",
            "usageRequestsServed",
            "usageRequestsRefused",
            "lastServedAgeSecs",
            "lastRefusedAgeSecs",
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

        handle_control_request(
            frame,
            &tx,
            &registry,
            &test_vault(),
            &ServeCounters::default(),
        )
        .await
        .unwrap();

        let response = rx.try_recv().expect("a response frame was sent");
        assert_eq!(response.header.ty, FrameType::Response);
        let body: ModuleControlResponse = serde_json::from_slice(&response.body).unwrap();
        assert!(matches!(body, ModuleControlResponse::RouteBindAck {}));
    }
}
