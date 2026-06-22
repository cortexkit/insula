#![forbid(unsafe_code)]

//! Walking-skeleton exit gate: prove ONE provider end-to-end over the REAL subc
//! wire.
//!
//! Topology (a consumer → subc → this module → a real provider fetch → back):
//!   - stand up an in-process subc daemon (loopback TCP + HMAC auth, the real
//!     `serve_listener` + `Router` + `ControlHandler`);
//!   - spawn the REAL `quota-module` binary as a subc client; it HELLO-registers
//!     a ManagementSurface and connects back;
//!   - drive a consumer the way `subc-probe` does: authenticate, `catalog.list`,
//!     `route.open` the management surface, then a `usage.get` REQUEST on the
//!     route channel;
//!   - assert the RESPONSE carries a `ProviderUsage[]` for `codex`.
//!
//! `skeleton_returns_real_codex_window` is the load-bearing proof and is
//! `#[ignore]` so it only runs when a real codex session is present
//! (`cargo test -- --ignored`); it asserts a HEALTHY window from the real
//! provider, never a stub. `skeleton_round_trips_usage_get_over_the_wire` runs in
//! CI and proves the full wire path regardless of whether a session exists
//! (degraded entry is an acceptable silent-degrade outcome there).

use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::Value;
use subc_core::{
    read_frame, serve_listener, write_frame, ControlHandler, Frame, Registry, Router, ServerAuth,
};
use subc_protocol::{
    session::ConfigTier, BindIdentity, Flags, FrameType, Priority, RouteTarget,
};
use subc_transport::{
    authenticate_client, connection_file, generate_daemon_id, generate_key, write_atomic,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(10);

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
        .env("SUBC_MODULE_ID", "ai-provider-quota")
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

// ---- consumer driver (mirrors subc-probe) ----------------------------------

async fn connect_consumer(connection_file_path: &Path) -> TcpStream {
    let conn = connection_file::read(connection_file_path).unwrap();
    let endpoint = conn.endpoints.first().unwrap();
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    authenticate_client(&mut stream, &conn, Duration::from_secs(2))
        .await
        .unwrap();
    stream
}

async fn control_rpc(stream: &mut TcpStream, corr: u64, body: Value) -> Frame {
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    read_until_channel0(stream, corr).await
}

async fn read_until_channel0(stream: &mut TcpStream, corr: u64) -> Frame {
    loop {
        let frame = read_frame_timeout(stream).await;
        if frame.header.channel == 0
            && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
            && frame.header.corr == corr
        {
            return frame;
        }
    }
}

async fn read_frame_timeout(stream: &mut TcpStream) -> Frame {
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for a frame")
}

async fn route_open(stream: &mut TcpStream, project_root: &Path, corr: u64) -> u16 {
    let request = json_route_open(project_root);
    let frame = control_rpc(stream, corr, request).await;
    assert_eq!(
        frame.header.ty,
        FrameType::Response,
        "route.open should succeed: {}",
        String::from_utf8_lossy(&frame.body)
    );
    let value: Value = serde_json::from_slice(&frame.body).unwrap();
    value["route_channel"].as_u64().unwrap() as u16
}

fn json_route_open(project_root: &Path) -> Value {
    let target = RouteTarget::ManagementSurface {
        module_id: "ai-provider-quota".to_string(),
    };
    let identity = BindIdentity {
        project_root: project_root.to_path_buf(),
        harness: "quota-e2e".to_string(),
        session: "session-1".to_string(),
    };
    let config: Vec<ConfigTier> = Vec::new();
    serde_json::json!({
        "op": "route.open",
        "target": target,
        "identity": identity,
        "config": config,
    })
}

async fn usage_get(stream: &mut TcpStream, route_channel: u16, corr: u64) -> Value {
    let body = serde_json::json!({ "method": "usage.get", "params": {} });
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        route_channel,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();

    loop {
        let frame = read_frame_timeout(stream).await;
        match frame.header.ty {
            FrameType::Response if frame.header.corr == corr => {
                return serde_json::from_slice(&frame.body).unwrap();
            }
            FrameType::Error if frame.header.corr == corr => {
                panic!(
                    "usage.get returned error: {}",
                    String::from_utf8_lossy(&frame.body)
                );
            }
            _ => continue,
        }
    }
}

/// Drive the full path and return the `result` array.
async fn drive_usage_get() -> (TestDaemon, ModuleProcess, Vec<Value>) {
    let daemon = start_daemon().await;
    let module = spawn_quota_module(&daemon.connection_file_path);
    wait_for_registration(&daemon.registry, "ai-provider-quota", SETUP_TIMEOUT).await;

    let project_root = unique_temp_dir("quota-e2e-project");
    std::fs::create_dir_all(&project_root).unwrap();

    let mut consumer = connect_consumer(&daemon.connection_file_path).await;

    // catalog.list — confirm the module is discoverable as a management surface.
    let catalog = control_rpc(&mut consumer, 1, serde_json::json!({ "op": "catalog.list" })).await;
    assert_eq!(catalog.header.ty, FrameType::Response);
    let catalog_value: Value = serde_json::from_slice(&catalog.body).unwrap();
    let found = catalog_value["modules"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["module_id"] == "ai-provider-quota");
    assert!(found, "quota module should be in the catalog: {catalog_value}");

    let route_channel = route_open(&mut consumer, &project_root, 2).await;
    let response = usage_get(&mut consumer, route_channel, 3).await;

    let _ = std::fs::remove_dir_all(&project_root);
    let result = response["result"].as_array().cloned().unwrap_or_default();
    (daemon, module, result)
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{label}-{}-{n}", process::id()))
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
