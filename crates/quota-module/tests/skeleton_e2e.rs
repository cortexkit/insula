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

use std::{
    collections::HashMap, net::Ipv4Addr, path::Path, path::PathBuf, process, time::Duration,
};

use serde_json::Value;
use subc_core::{
    read_frame, serve_listener, write_frame, ControlHandler, Registry, Router, ServerAuth,
};
use subc_protocol::{
    manifest::{
        Bindings, IdentityBinding, ManagementOperation, ManagementOperationKind, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, TrustTier,
    },
    session::{ModuleControlRequest, ModuleControlResponse},
    Flags, Frame, FrameType, ModuleHelloBody, Priority, PROTOCOL_VERSION,
};
use subc_transport::{
    generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::{Child, Command},
    time::{sleep, Instant},
};

use common::{
    catalog_list, connect_consumer, raw_route_frame, route_open, unique_temp_dir, usage_get, Route,
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
        // Declare the wire-v2 envelope so the module's version validation
        // exercises the same path as a real daemon's connection file.
        wire_version: Some(subc_protocol::PROTOCOL_VERSION),
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

// ---- credential and HTTP stubs --------------------------------------------

use common::VAULT_MODULE_ID;

struct VaultStub {
    task: tokio::task::JoinHandle<()>,
}

impl VaultStub {
    fn stop(&mut self) {
        self.task.abort();
    }
}

impl Drop for VaultStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn vault_manifest() -> ModuleManifest {
    ModuleManifest {
        module_id: VAULT_MODULE_ID.to_string(),
        module_version: "test-stub".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ManagementSurface {
            operations: vec![
                ManagementOperation {
                    name: "credential.get".to_string(),
                    kind: ManagementOperationKind::Query,
                },
                ManagementOperation {
                    name: "credential.report_auth_failure".to_string(),
                    kind: ManagementOperationKind::Query,
                },
            ],
            config_schema: serde_json::json!({"type":"object"}),
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

async fn start_vault_stub(connection_file_path: &Path) -> VaultStub {
    let mut stream = connect_consumer(connection_file_path).await;
    let hello = Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&ModuleHelloBody {
            manifest: vault_manifest(),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        })
        .unwrap(),
    )
    .unwrap();
    write_frame(&mut stream, &hello).await.unwrap();
    let ack = read_frame(&mut stream).await.unwrap().unwrap();
    assert_eq!(ack.header.ty, FrameType::HelloAck);

    let credentials: HashMap<&'static str, (&'static [u8], &'static str, u64)> = HashMap::from([
        (
            "ckh_openai_primary",
            (b"vault-token-primary".as_slice(), "account-primary", 7),
        ),
        (
            "ckh_openai_second",
            (b"vault-token-second".as_slice(), "account-second", 11),
        ),
    ]);
    let task = tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut stream).await {
            let response = match frame.header.ty {
                FrameType::Ping => Frame::build_with_version(
                    frame.header.ver,
                    FrameType::Pong,
                    frame.header.flags,
                    0,
                    0,
                    frame.header.corr,
                    Vec::new(),
                )
                .unwrap(),
                FrameType::Request if frame.header.channel == 0 => {
                    let request: ModuleControlRequest =
                        serde_json::from_slice(&frame.body).unwrap();
                    let body = match request {
                        ModuleControlRequest::RouteBind { .. } => {
                            serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap()
                        }
                        ModuleControlRequest::HealthCheck {} => unreachable!(),
                    };
                    Frame::build_with_version(
                        frame.header.ver,
                        FrameType::Response,
                        Flags::new(false, Priority::Passive, false),
                        0,
                        0,
                        frame.header.corr,
                        body,
                    )
                    .unwrap()
                }
                FrameType::Request => {
                    let request: Value = serde_json::from_slice(&frame.body).unwrap();
                    let result = match request["method"].as_str() {
                        Some("credential.get") => {
                            assert_eq!(request["params"]["min_ttl_ms"], 120_000);
                            let handle = request["params"]["handle"].as_str().unwrap_or_default();
                            match credentials.get(handle) {
                                Some((payload, account_id, record_version)) => serde_json::json!({
                                    "result": {
                                        "payload": payload,
                                        "expires_at_ms": null,
                                        "record_version": record_version,
                                        "account_id": account_id,
                                    }
                                }),
                                None => serde_json::json!({
                                    "result": {"error": {"code": "not_found", "class": "permanent"}}
                                }),
                            }
                        }
                        Some("credential.report_auth_failure") => {
                            serde_json::json!({"result": {}})
                        }
                        _ => serde_json::json!({
                            "result": {"error": {"code": "unknown_method", "class": "permanent"}}
                        }),
                    };
                    Frame::build_with_version(
                        frame.header.ver,
                        FrameType::Response,
                        Flags::new(false, Priority::Interactive, false),
                        frame.header.channel,
                        frame.header.epoch,
                        frame.header.corr,
                        serde_json::to_vec(&result).unwrap(),
                    )
                    .unwrap()
                }
                _ => continue,
            };
            if write_frame(&mut stream, &response).await.is_err() {
                return;
            }
        }
    });
    VaultStub { task }
}

struct UsageHttpStub {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for UsageHttpStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_usage_http_stub() -> UsageHttpStub {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = vec![0; 16 * 1024];
                let Ok(size) = stream.read(&mut request).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&request[..size]);
                let header = |wanted: &str| {
                    request.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case(wanted).then(|| value.trim())
                    })
                };
                let account = header("chatgpt-account-id");
                let authorization = header("authorization");
                assert!(
                    matches!(
                        (account, authorization),
                        (
                            Some("account-primary"),
                            Some("Bearer local-token" | "Bearer vault-token-primary")
                        ) | (Some("account-second"), Some("Bearer vault-token-second"))
                    ),
                    "bearer/account headers must come from one served context"
                );
                let used_percent = match account {
                    Some("account-second") => 62,
                    _ => 21,
                };
                let body = serde_json::json!({
                    "rate_limit": {
                        "limit_reached": false,
                        "primary_window": {
                            "used_percent": used_percent,
                            "reset_at": 1900000000,
                            "limit_window_seconds": 604800
                        }
                    }
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    UsageHttpStub {
        base_url: format!("http://{address}/backend-api"),
        task,
    }
}

fn write_owner_only(path: &Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).unwrap();
    std::io::Write::write_all(&mut file, body).unwrap();
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

fn quota_module_command(subc_connection_file: &Path, test_temp_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-insula"));
    command
        .arg("--subc")
        .arg(subc_connection_file)
        .env("SUBC_MODULE_ID", MODULE_ID)
        .env("XDG_CONFIG_HOME", test_temp_dir.join("quota-config"))
        .env("CK_QUOTA_STATE_DIR", test_temp_dir.join("quota-state"))
        .env(
            "CK_QUOTA_VAULT_HANDLES_PATH",
            test_temp_dir.join("absent-vault-handles.json"),
        )
        .stderr(process::Stdio::inherit())
        .kill_on_drop(true);
    command
}

fn spawn_quota_module(subc_connection_file: &Path, test_temp_dir: &Path) -> ModuleProcess {
    let child = quota_module_command(subc_connection_file, test_temp_dir)
        .spawn()
        .expect("spawn quota-module");
    ModuleProcess { child }
}

#[test]
fn f1_module_process_cannot_inherit_real_reset_config_or_state() {
    let temp_dir = Path::new("/isolated-test-rig");
    let command = quota_module_command(Path::new("/isolated/connection.json"), temp_dir);
    let env: std::collections::HashMap<_, _> = command
        .as_std()
        .get_envs()
        .filter_map(|(key, value)| Some((key.to_str()?, value?.to_str()?)))
        .collect();
    assert_eq!(
        env.get("XDG_CONFIG_HOME"),
        Some(&"/isolated-test-rig/quota-config")
    );
    assert_eq!(
        env.get("CK_QUOTA_STATE_DIR"),
        Some(&"/isolated-test-rig/quota-state")
    );
    assert_eq!(
        env.get("CK_QUOTA_VAULT_HANDLES_PATH"),
        Some(&"/isolated-test-rig/absent-vault-handles.json")
    );
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
/// consumer already bound to the route `(channel, epoch)`.
async fn open_quota_route() -> (TestDaemon, ModuleProcess, tokio::net::TcpStream, Route) {
    let daemon = start_daemon().await;
    let module = spawn_quota_module(&daemon.connection_file_path, &daemon.temp_dir);
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

    let route = route_open(&mut consumer, &project_root, 2).await;
    let _ = std::fs::remove_dir_all(&project_root);
    (daemon, module, consumer, route)
}

/// Drive the full path and return the `result` array once `want_provider` has
/// been resolved. Serving is cache-only and the refresher publishes each
/// provider's result AS IT COMPLETES, so a non-empty array may still be missing
/// a specific provider mid-sweep; poll until the asserted provider appears (or a
/// deadline), exactly as a real consumer reading async-refreshed data would.
async fn drive_usage_get_for(want_provider: &str) -> (TestDaemon, ModuleProcess, Vec<Value>) {
    let (daemon, module, mut consumer, route) = open_quota_route().await;
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut corr = 3;
    let result = loop {
        let response = usage_get(&mut consumer, route, corr).await;
        let result = response["result"].as_array().cloned().unwrap_or_default();
        let has_target = result.iter().any(|e| e["provider"] == want_provider);
        if has_target || Instant::now() >= deadline {
            break result;
        }
        corr += 1;
        sleep(Duration::from_millis(200)).await;
    };
    (daemon, module, result)
}

// ---- tests -----------------------------------------------------------------

/// CI gate: the full wire path round-trips and returns a codex entry. Works with
/// or without a real session (silent-degrade is acceptable here).
#[tokio::test]
async fn skeleton_round_trips_usage_get_over_the_wire() {
    let (_daemon, _module, result) = drive_usage_get_for("codex").await;
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

#[tokio::test]
async fn i8_vault_stub_two_accounts_fail_closed_without_handle_reap() {
    let daemon = start_daemon().await;
    let usage_stub = start_usage_http_stub().await;
    let mut vault_stub = start_vault_stub(&daemon.connection_file_path).await;
    wait_for_registration(&daemon.registry, VAULT_MODULE_ID, SETUP_TIMEOUT).await;

    let codex_home = daemon.temp_dir.join("codex-home");
    write_owner_only(
        &codex_home.join("auth.json"),
        br#"{"tokens":{"access_token":"local-token","account_id":"account-primary"}}"#,
    );
    write_owner_only(
        &codex_home.join("config.toml"),
        format!("chatgpt_base_url = {:?}\n", usage_stub.base_url).as_bytes(),
    );
    let handles_path = daemon.temp_dir.join("vault-handles.json");
    write_owner_only(
        &handles_path,
        br#"{"handles":{"chatgpt:openai":"ckh_openai_primary","chatgpt:openai:gmail":"ckh_openai_second"}}"#,
    );

    let child = quota_module_command(&daemon.connection_file_path, &daemon.temp_dir)
        .env("CODEX_HOME", &codex_home)
        .env("CK_QUOTA_VAULT_HANDLES_PATH", &handles_path)
        .spawn()
        .expect("spawn vault-wired quota-module");
    let _module = ModuleProcess { child };
    wait_for_registration(&daemon.registry, MODULE_ID, SETUP_TIMEOUT).await;

    let project_root = daemon.temp_dir.join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let mut consumer = connect_consumer(&daemon.connection_file_path).await;
    let route = route_open(&mut consumer, &project_root, 10).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut corr = 11;
    let initial = loop {
        let response = usage_get(&mut consumer, route, corr).await;
        let result = response["result"].as_array().cloned().unwrap_or_default();
        let mut codex_accounts = result
            .iter()
            .filter(|entry| entry["provider"] == "codex")
            .filter_map(|entry| entry["account"].as_str())
            .collect::<Vec<_>>();
        codex_accounts.sort_unstable();
        if codex_accounts == ["account-primary", "account-second"] {
            let used_percent = |account: &str| {
                result
                    .iter()
                    .find(|entry| entry["provider"] == "codex" && entry["account"] == account)
                    .and_then(|entry| entry["usage"]["primary"]["usedPercent"].as_f64())
            };
            assert_eq!(used_percent("account-primary"), Some(21.0));
            assert_eq!(used_percent("account-second"), Some(62.0));
            break result;
        }
        assert!(
            Instant::now() < deadline,
            "two vault-backed codex accounts did not arrive: {result:?}"
        );
        corr += 1;
        sleep(Duration::from_millis(100)).await;
    };
    let unaffected_provider = initial
        .iter()
        .find(|entry| entry["provider"] != "codex")
        .and_then(|entry| entry["provider"].as_str())
        .map(ToString::to_string)
        .expect("at least one non-codex provider should complete in the same sweep");

    vault_stub.stop();

    let deadline = Instant::now() + Duration::from_secs(80);
    loop {
        corr += 1;
        let response = usage_get(&mut consumer, route, corr).await;
        let result = response["result"].as_array().cloned().unwrap_or_default();
        let codex = result
            .iter()
            .filter(|entry| entry["provider"] == "codex")
            .collect::<Vec<_>>();
        let failed_closed = codex.len() == 1
            && codex[0]["account"] == "account-primary"
            && codex[0]["error"].is_null()
            && codex[0]["usage"]["primary"]["usedPercent"] == 21.0
            && !result
                .iter()
                .any(|entry| entry["provider"] == "codex" && entry["account"] == "account-second");
        if failed_closed {
            assert!(
                result
                    .iter()
                    .any(|entry| entry["provider"] == unaffected_provider),
                "non-codex provider disappeared after the vault was killed: {result:?}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "vault labels were stale-served after the stub died: {result:?}"
        );
        sleep(Duration::from_millis(250)).await;
    }
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
    let (_daemon, _module, mut consumer, route) = open_quota_route().await;

    // An unknown method on a well-formed body.
    let frame = raw_route_frame(
        &mut consumer,
        route,
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
        route,
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
    let (_daemon, _module, result) = drive_usage_get_for("codex").await;
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
    let (_daemon, _module, result) = drive_usage_get_for("claude").await;
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
