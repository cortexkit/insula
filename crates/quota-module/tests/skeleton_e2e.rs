#![forbid(unsafe_code)]

//! Walking-skeleton exit gate: prove the provider path end-to-end over the REAL
//! subc wire, against an IN-PROCESS subc daemon.
//!
//! Topology (a consumer → subc → this module → a real provider fetch → back):
//!   - stand up an in-process subc daemon (loopback TCP + HMAC auth, the real
//!     `serve_listener` + `Router` + `ControlHandler`);
//!   - spawn the REAL `quota-module` binary as a subc client; it HELLO-registers
//!     a ManagementSurface and connects back;
//!   - drive the consumer via the shared [`common`] driver: authenticate,
//!     `catalog.list`, `route.open` the management surface, then a `usage.get`
//!     REQUEST on the route channel;
//!   - assert the RESPONSE carries a `ProviderUsage[]` for `codex`.
//!
//! The consumer-side wire driver is shared with `real_daemon_e2e` via
//! `tests/common`; this file owns only the IN-PROCESS daemon setup. The
//! real-binary supervision proof (a standalone `subc-core` spawning the module from
//! `subc.jsonc`) lives in `real_daemon_e2e.rs`.
//!
//! `skeleton_returns_real_codex_window` is the load-bearing proof and is
//! `#[ignore]` so it only runs when a real codex session is present
//! (`cargo test -- --ignored`); it asserts a HEALTHY window from the real
//! provider, never a stub. `skeleton_round_trips_usage_get_over_the_wire` runs in
//! CI and proves the full wire path regardless of whether a session exists.

mod common;

use std::{net::Ipv4Addr, path::Path, path::PathBuf, process, time::Duration};

use serde_json::Value;
use subc_core::{serve_listener, ControlHandler, Registry, Router, ServerAuth};
use subc_protocol::FrameType;
use subc_transport::{
    generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    time::{sleep, Instant},
};

use common::{
    catalog_list, connect_consumer, raw_route_frame, route_open, unique_temp_dir, usage_get,
    MODULE_ID, SETUP_TIMEOUT,
};

// ---- in-process daemon -----------------------------------------------------

struct TestDaemon {
    registry: std::sync::Arc<Registry>,
    connection_file_path: PathBuf,
    temp_dir: PathBuf,
    task: tokio::task::JoinHandle<Result<(), subc_core::ServerError>>,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

async fn start_daemon() -> TestDaemon {
    let temp_dir = unique_temp_dir("quota-e2e-daemon");
    std::fs::create_dir_all(&temp_dir).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connection_file_path = temp_dir.join("subc-conn.json");
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: process::id(),
        daemon_ver: "test-quota-e2e".to_owned(),
    };
    write_atomic(&connection_file_path, &conn).unwrap();

    let registry = std::sync::Arc::new(Registry::default());
    // No process-liveness wiring: route.open's liveness gate only blocks on an
    // explicit Some(false); a HELLO-registered module's live forwarding
    // connection is what satisfies the routability check.
    let control = ControlHandler::new(std::sync::Arc::clone(&registry));
    let router = std::sync::Arc::new(Router::with_control_handler(std::sync::Arc::new(control)));
    let auth = ServerAuth::new(conn.key, conn.daemon_id, conn.daemon_ver);
    let task = tokio::spawn(serve_listener(listener, router, auth));

    TestDaemon {
        registry,
        connection_file_path,
        temp_dir,
        task,
    }
}

// ---- the real module process ----------------------------------------------

struct ModuleProcess {
    child: Child,
}

impl Drop for ModuleProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn spawn_quota_module(subc_connection_file: &Path) -> ModuleProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_quota-module"));
    command
        .arg("--subc")
        .arg(subc_connection_file)
        .env("SUBC_MODULE_ID", MODULE_ID)
        .stderr(process::Stdio::inherit())
        .kill_on_drop(true);
    let child = command.spawn().expect("spawn quota-module");
    ModuleProcess { child }
}

async fn wait_for_registration(registry: &Registry, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if registry.get_module(module_id).unwrap().is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} did not register within {wait:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

// ---- per-test orchestration (in-process daemon + real module + consumer) ----

/// Stand up daemon + real module, confirm it is in the catalog, and open a route
/// to its management surface. Returns the live pieces plus an authenticated
/// consumer already bound to `route_channel`.
async fn open_quota_route() -> (TestDaemon, ModuleProcess, tokio::net::TcpStream, u16) {
    let daemon = start_daemon().await;
    let module = spawn_quota_module(&daemon.connection_file_path);
    wait_for_registration(&daemon.registry, MODULE_ID, SETUP_TIMEOUT).await;

    let project_root = unique_temp_dir("quota-e2e-project");
    std::fs::create_dir_all(&project_root).unwrap();

    let mut consumer = connect_consumer(&daemon.connection_file_path).await;

    // catalog.list — confirm the module is discoverable as a management surface.
    let modules = catalog_list(&mut consumer, 1).await;
    assert!(
        modules.iter().any(|m| m["module_id"] == MODULE_ID),
        "quota module should be in the catalog: {modules:?}"
    );

    let route_channel = route_open(&mut consumer, &project_root, 2).await;
    let _ = std::fs::remove_dir_all(&project_root);
    (daemon, module, consumer, route_channel)
}

/// Drive the full path and return the `result` array.
async fn drive_usage_get() -> (TestDaemon, ModuleProcess, Vec<Value>) {
    let (daemon, module, mut consumer, route_channel) = open_quota_route().await;
    let response = usage_get(&mut consumer, route_channel, 3).await;
    let result = response["result"].as_array().cloned().unwrap_or_default();
    (daemon, module, result)
}

// ---- tests -----------------------------------------------------------------

/// CI gate: the full wire path round-trips and returns a codex entry. Works with
/// or without a real session (silent-degrade is acceptable here).
#[tokio::test]
async fn skeleton_round_trips_usage_get_over_the_wire() {
    let (_daemon, _module, result) = drive_usage_get().await;
    let codex = result
        .iter()
        .find(|e| e["provider"] == "codex")
        .expect("response should include a codex entry");
    // Either a healthy entry (usage present) or a silent-degraded one (error
    // present) — but the wire path itself must have produced a well-formed entry.
    let healthy = codex.get("usage").is_some();
    let degraded = codex.get("error").is_some();
    assert!(
        healthy ^ degraded,
        "codex entry must be exactly one of healthy|degraded: {codex}"
    );
}

/// Locks the module-data-plane ERROR contract (the precedent for every future
/// module): a bad request on the route channel comes back as a `FrameType::Error`
/// frame whose body is subc's canonical `ErrorBody { code, message }` — NOT an
/// error embedded in a success `result` wrapper. This lets a client share ONE
/// error codec across channel-0 control and the data plane. Per-provider
/// degradation stays embedded in `result[]` (covered by the round-trip test);
/// wholesale Error frames are reserved for bad-request/unknown-method.
#[tokio::test]
async fn unknown_method_returns_error_frame_with_canonical_error_body() {
    let (_daemon, _module, mut consumer, route_channel) = open_quota_route().await;

    // An unknown method on a well-formed body.
    let frame = raw_route_frame(
        &mut consumer,
        route_channel,
        7,
        serde_json::json!({ "method": "cost.get", "params": {} }),
    )
    .await;
    assert_eq!(
        frame.header.ty,
        FrameType::Error,
        "unknown method must be a wholesale Error frame, not a result wrapper"
    );
    let error: subc_protocol::ErrorBody = serde_json::from_slice(&frame.body)
        .expect("Error frame body must be subc's canonical ErrorBody {code,message}");
    assert_eq!(error.code, "unknown_method");
    assert!(
        error.message.contains("cost.get"),
        "message should name the rejected method: {}",
        error.message
    );

    // A malformed body (not decodable as a usage request) is also an Error frame.
    let frame = raw_route_frame(
        &mut consumer,
        route_channel,
        8,
        serde_json::json!({ "not_a_method": true }),
    )
    .await;
    assert_eq!(frame.header.ty, FrameType::Error);
    let error: subc_protocol::ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(error.code, "invalid_request");
}

/// The load-bearing proof: a REAL codex window from the real on-disk session.
/// Ignored by default; run with `cargo test -p quota-module -- --ignored`.
#[tokio::test]
#[ignore = "requires a real ~/.codex/auth.json session"]
async fn skeleton_returns_real_codex_window() {
    let (_daemon, _module, result) = drive_usage_get().await;
    let codex = result
        .iter()
        .find(|e| e["provider"] == "codex")
        .expect("response should include a codex entry");
    assert!(
        codex.get("error").is_none(),
        "expected a HEALTHY codex entry from the real session, got: {codex}"
    );
    let primary = &codex["usage"]["primary"];
    assert!(
        primary["usedPercent"].is_number(),
        "primary.usedPercent must be a real number: {codex}"
    );
    assert!(
        primary["resetsAt"].is_string(),
        "primary.resetsAt must be an ISO timestamp: {codex}"
    );
    eprintln!(
        "[skeleton] REAL codex window: usedPercent={} resetsAt={} windowMinutes={}",
        primary["usedPercent"], primary["resetsAt"], primary["windowMinutes"]
    );
}

/// 2nd-archetype proof: a REAL anthropic/claude window from opencode's auth
/// store, through the full wire. Validates that the provider abstraction holds
/// across a DISTINCT archetype (already-percent utilization, already-ISO8601
/// reset, named windows) — not just a second copy of the codex shape.
/// Ignored by default; run with `cargo test -p quota-module -- --ignored`.
#[tokio::test]
#[ignore = "requires a real anthropic OAuth session in opencode auth.json"]
async fn skeleton_returns_real_anthropic_window() {
    let (_daemon, _module, result) = drive_usage_get().await;
    let claude = result
        .iter()
        .find(|e| e["provider"] == "claude")
        .expect("response should include a claude entry");
    assert!(
        claude.get("error").is_none(),
        "expected a HEALTHY claude entry from the real session, got: {claude}"
    );
    let primary = &claude["usage"]["primary"];
    assert!(
        primary["usedPercent"].is_number(),
        "primary.usedPercent must be a real number: {claude}"
    );
    // CodexBar-faithful: the five-hour primary may be an idle 0%-used window with
    // no reset (Anthropic reports resets_at: null when nothing is pending), in
    // which case resetsAt is omitted — never fabricated. So accept present-string
    // OR absent here, and prove a REAL ISO reset flows through on the active
    // weekly window (secondary) so the live-window proof stays meaningful.
    assert!(
        primary["resetsAt"].is_string() || primary["resetsAt"].is_null(),
        "primary.resetsAt must be an ISO timestamp or omitted (idle window): {claude}"
    );
    let secondary = &claude["usage"]["secondary"];
    assert!(
        secondary["usedPercent"].is_number() && secondary["resetsAt"].is_string(),
        "the active weekly window must carry a real percent + ISO reset: {claude}"
    );
    eprintln!(
        "[skeleton] REAL claude windows: primary usedPercent={} resetsAt={} | secondary usedPercent={} resetsAt={}",
        primary["usedPercent"], primary["resetsAt"], secondary["usedPercent"], secondary["resetsAt"]
    );
}
