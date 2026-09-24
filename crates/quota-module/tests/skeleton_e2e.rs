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
use subc_daemon::{
    read_frame, serve_listener, write_frame, ControlHandler, Registry, Router, ServerAuth,
};
use subc_protocol::{
    manifest::{
        Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
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
    catalog_list, connect_consumer, isolate_env, isolated_env, raw_route_frame, route_open,
    unique_temp_dir, usage_get, Route, MODULE_ID, SETUP_TIMEOUT,
};

// ---- in-process daemon -----------------------------------------------------

struct TestDaemon {
    registry: std::sync::Arc<Registry>,
    connection_file_path: PathBuf,
    temp_dir: PathBuf,
    task: tokio::task::JoinHandle<Result<(), subc_daemon::ServerError>>,
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
    // Builder, not a literal: ModuleManifest is #[non_exhaustive] upstream, so
    // a new field is a compile error here rather than a silent default.
    //
    // TRUST TIER AND BINDINGS STAY OMITTED, and this is the one place in this
    // repository where omitting them is right for a reason OTHER than truth.
    //
    // The real credential vault declares both, and truthfully -- it is first-party
    // and it does own a project-scoped SQLite schema. So a stub that mirrored it
    // would state `FirstParty` correctly. The reason not to is that this fixture
    // cannot VERIFY either claim: nothing here reads claustrum's manifest, so a
    // value copied from it today becomes a stale assertion about another module
    // the moment that module changes, and the copy would look like a check while
    // being a guess. Same reason its operations carry no descriptions below.
    //
    // Distinct from the production manifest in main.rs, where I dropped a TRUE
    // trust tier by association at 3af16ad and restored it at 1452f38. There the
    // question was whether an honest value existed; here an honest value exists
    // and is not MINE TO STATE.
    ModuleManifest::builder(VAULT_MODULE_ID.to_string(), "test-stub".to_string())
        .protocol_ver(PROTOCOL_VERSION)
        .provides(vec![ProviderRole::ManagementSurface {
            operations: vec![
                // No description on either: the stub mirrors what the real vault
                // declares, and inventing copy for another module's operations
                // would put words in its manifest that nothing here can check.
                ManagementOperation {
                    name: "credential.get".to_string(),
                    kind: ManagementOperationKind::Query,
                    description: None,
                },
                ManagementOperation {
                    name: "credential.report_auth_failure".to_string(),
                    kind: ManagementOperationKind::Query,
                    description: None,
                },
            ],
            config_schema: serde_json::json!({"type":"object"}),
            observability: Vec::new(),
            identity_scope: Vec::new(),
            // The stub mirrors the module's own declaration; a vault stub that
            // claimed different ordering would test a manifest nothing ships.
            concurrency: Concurrency::StatelessParallel,
        }])
        .consumes(Vec::new())
        .self_signals(
            // An empty list, not None. The two are wire-distinct as of subc-protocol
            // 0.14.0: None means the module has not adopted the vocabulary, an empty
            // list is an affirmative "examined, and there are none".
            //
            // This stub was examined -- it drives frames and nothing else, running no
            // refresher and spending nothing -- so the affirmative zero is the true
            // statement. Declaring None would say the question had not been asked,
            // which is false for a manifest edited in the same commit that asked it.
            Some(Vec::new()),
        )
        .provenance(
            // None, for the same reason the operations carry no description: this
            // stub stands in for ANOTHER module, and provenance is a claim about
            // which source built a binary. Stamping this test's own build stamp here
            // would attribute our provenance to the credential vault; inventing a
            // plausible one would put an unmeasured fact in a field that exists to
            // carry measured ones. A stub that cannot source it should say so.
            None,
        )
        .capabilities(
            // Mirrors the module's own `None`, for the reason on the line above about
            // ordering: a stub declaring capabilities the real vault does not would
            // exercise a handshake nothing ships.
            None,
        )
        .build()
}

#[derive(Clone, Copy)]
struct StubCredential {
    id: &'static str,
    kind: &'static str,
    serves: &'static str,
    payload: &'static [u8],
    account_id: &'static str,
    record_version: u64,
    refresh_adapter: Option<&'static str>,
}

impl StubCredential {
    const fn new(
        id: &'static str,
        kind: &'static str,
        serves: &'static str,
        payload: &'static [u8],
        account_id: &'static str,
        record_version: u64,
        refresh_adapter: Option<&'static str>,
    ) -> Self {
        Self {
            id,
            kind,
            serves,
            payload,
            account_id,
            record_version,
            refresh_adapter,
        }
    }

    fn published_row(self) -> Value {
        let mut row = serde_json::json!({
            "id": self.id,
            "categories": ["llm-provider"],
            "type": self.kind,
            "serves": [self.serves],
            "state": "active",
            "record_version": self.record_version,
            "operations": ["read"],
            "account_id": self.account_id,
            "email": format!("{}@example.test", self.account_id),
            "org_name": "Vault Stub",
        });
        if let Some(refresh_adapter) = self.refresh_adapter {
            row["refresh_adapter"] = Value::String(refresh_adapter.to_string());
        }
        row
    }
}

const DEFAULT_STUB_CREDENTIALS: [StubCredential; 2] = [
    StubCredential::new(
        "chatgpt:openai",
        "oauth",
        "openai",
        b"vault-token-primary",
        "account-primary",
        7,
        Some("openai-oauth"),
    ),
    StubCredential::new(
        "chatgpt:openai:gmail",
        "oauth",
        "openai",
        b"vault-token-second",
        "account-second",
        11,
        Some("openai-oauth"),
    ),
];

fn scoped_stub_credential(
    id: &'static str,
    kind: &'static str,
    serves: &'static str,
    record_version: u64,
) -> StubCredential {
    StubCredential::new(
        id,
        kind,
        serves,
        b"\xff",
        id,
        record_version,
        (kind == "oauth").then_some("test-oauth"),
    )
}

async fn start_vault_stub(connection_file_path: &Path) -> VaultStub {
    start_vault_stub_with_credentials(connection_file_path, &DEFAULT_STUB_CREDENTIALS).await
}

async fn start_vault_stub_with_credentials(
    connection_file_path: &Path,
    stub_credentials: &[StubCredential],
) -> VaultStub {
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

    let credential_rows = stub_credentials
        .iter()
        .copied()
        .map(StubCredential::published_row)
        .collect::<Vec<_>>();
    let credentials = stub_credentials
        .iter()
        .map(|credential| {
            (
                credential.id.to_string(),
                (
                    credential.payload.to_vec(),
                    credential.account_id.to_string(),
                    credential.record_version,
                ),
            )
        })
        .collect::<HashMap<_, _>>();
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
                        Some("credential.list_scoped") => serde_json::json!({
                            "result": {
                                "view": "sha256:test-view",
                                "grants": 1,
                                "grant_tuples": [{
                                    "selector_kind": "category",
                                    "selector": "llm-provider",
                                    "operation": "read"
                                }],
                                "credentials": credential_rows.clone()
                            }
                        }),
                        Some("credential.get_scoped") => {
                            assert_eq!(request["params"]["min_ttl_ms"], 120_000);
                            let credential_id = request["params"]["credential_id"]
                                .as_str()
                                .unwrap_or_default();
                            match credentials.get(credential_id) {
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

/// Whether a spawned module may see the developer's own provider sessions.
#[derive(Clone, Copy)]
enum HostSessions {
    /// Every credential location points inside the rig. The default, and the
    /// only choice for a test that runs without `--ignored`.
    Isolated,
    /// The live proofs, which exist to read a real session: they alone get the
    /// real home directory back, and only when someone runs them on purpose.
    /// Reset config and state stay in the rig even then, so a live proof can
    /// never act on the developer's real reset configuration.
    Real,
}

/// The host variables a live proof needs to find a real codex or anthropic
/// session: the home directory, and the two overrides those providers honour.
const REAL_SESSION_ENV: &[&str] = &["HOME", "USERPROFILE", "CODEX_HOME", "XDG_DATA_HOME"];

fn quota_module_command(subc_connection_file: &Path, test_temp_dir: &Path) -> Command {
    quota_module_command_for(subc_connection_file, test_temp_dir, HostSessions::Isolated)
}

fn quota_module_command_for(
    subc_connection_file: &Path,
    test_temp_dir: &Path,
    sessions: HostSessions,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-insula"));
    isolate_env(&mut command, test_temp_dir);
    if let HostSessions::Real = sessions {
        for name in REAL_SESSION_ENV {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    command
        .arg("--subc")
        .arg(subc_connection_file)
        .env("SUBC_MODULE_ID", MODULE_ID)
        .stderr(process::Stdio::inherit())
        .kill_on_drop(true);
    command
}

fn spawn_quota_module(
    subc_connection_file: &Path,
    test_temp_dir: &Path,
    sessions: HostSessions,
) -> ModuleProcess {
    let child = quota_module_command_for(subc_connection_file, test_temp_dir, sessions)
        .spawn()
        .expect("spawn quota-module");
    ModuleProcess { child }
}

/// Set on the probe process `f1_…` spawns; see [`env_probe_prints_its_environment_when_asked`].
const ENV_PROBE_MARKER: &str = "INSULA_E2E_ENV_PROBE";
const ENV_PROBE_TEST: &str = "env_probe_prints_its_environment_when_asked";
const ENV_PROBE_LINE: &str = "env-probe ";

/// Not a test on its own: a no-op unless [`ENV_PROBE_MARKER`] is set. `f1_…`
/// re-runs this test binary filtered to this one test, under the same
/// environment isolation the module gets, and reads back what it prints. That
/// observes the environment a spawned process actually receives, which the
/// command's own list of explicit variables cannot show: whether everything
/// else was cleared.
#[test]
fn env_probe_prints_its_environment_when_asked() {
    if std::env::var_os(ENV_PROBE_MARKER).is_none() {
        return;
    }
    for (key, value) in std::env::vars_os() {
        println!(
            "{ENV_PROBE_LINE}{}={}",
            key.to_string_lossy(),
            value.to_string_lossy()
        );
    }
}

/// The name of every credential-bearing environment variable the providers
/// read, taken from the source rather than typed out here.
///
/// Every such name appears in the provider code as a string literal written in
/// upper snake case (`"MINIMAX_API_KEY"`, `"GH_TOKEN"`,
/// `"VOLCENGINE_SECRET_ACCESS_KEY"`). Collecting them by scanning means a
/// provider added tomorrow is covered without anyone remembering to extend a
/// list, which is exactly how a hand-typed deny-list goes stale.
fn provider_secret_env_names() -> std::collections::BTreeSet<String> {
    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read a source directory") {
            let path = entry.expect("read a source directory entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    const SECRET_WORDS: &[&str] = &["KEY", "TOKEN", "SECRET", "COOKIE", "PASSWORD"];

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    rust_sources(&manifest.join("src"), &mut sources);
    rust_sources(&manifest.join("../quota-core/src"), &mut sources);

    let mut names = std::collections::BTreeSet::new();
    for source in sources {
        let text = std::fs::read_to_string(&source).expect("read a provider source file");
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'"' {
                i += 1;
                continue;
            }
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_uppercase()
                    || bytes[end].is_ascii_digit()
                    || bytes[end] == b'_')
            {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b'"' && end > start {
                let name = &text[start..end];
                if bytes[start].is_ascii_uppercase()
                    && SECRET_WORDS.iter().any(|word| name.contains(word))
                {
                    names.insert(name.to_owned());
                }
                // The closing quote may open the next literal, so look at it again.
                i = end;
            } else {
                i = start;
            }
        }
    }
    names
}

#[test]
fn f1_module_process_cannot_inherit_real_reset_config_or_state() {
    let rig = Path::new("/isolated-test-rig");
    let command = quota_module_command(Path::new("/isolated/connection.json"), rig);
    let env: HashMap<String, Option<String>> = command
        .as_std()
        .get_envs()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let set = |name: &str| env.get(name).cloned().flatten();

    // Expectations are built with the same path joining the code under test
    // uses, rather than written as literal strings. The property being checked
    // is that each variable points inside the rig -- a real credential must not
    // be reachable from a test process -- and the separator it is spelled with
    // is the platform's business. A literal picks one platform's separator and
    // fails everywhere else for a reason unrelated to isolation.
    let expect = |name: &str| {
        rig.join(name)
            .to_str()
            .expect("the rig path is valid UTF-8")
            .to_owned()
    };
    assert_eq!(set("XDG_CONFIG_HOME"), Some(expect("quota-config")));
    assert_eq!(set("CK_QUOTA_STATE_DIR"), Some(expect("quota-state")));

    // Every root a credential path is resolved from is set, and inside the rig.
    for name in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        let value = set(name).unwrap_or_else(|| {
            panic!("{name} must be set to a rig path, or the module inherits the real one")
        });
        assert!(
            Path::new(&value).starts_with(rig),
            "{name}={value} must point inside the rig {}",
            rig.display()
        );
    }
    for (name, value) in &env {
        if let (true, Some(value)) = (name.starts_with("XDG_"), value) {
            assert!(
                Path::new(value).starts_with(rig),
                "{name}={value} must point inside the rig"
            );
        }
    }

    // The scan must actually find the provider keys, or the checks below
    // would pass over an empty list. These are two the module is known to
    // read from the environment.
    let secrets = provider_secret_env_names();
    for known in ["MINIMAX_API_KEY", "KIMI_CODE_API_KEY"] {
        assert!(
            secrets.contains(known),
            "the provider-key scan missed {known}; found {secrets:?}"
        );
    }
    let allowed: std::collections::BTreeSet<&str> = isolated_env(rig)
        .iter()
        .map(|(name, _)| *name)
        .chain(common::INHERITED_ENV.iter().copied())
        .chain(["SUBC_MODULE_ID"])
        .collect();
    let leaked_by_allowlist: Vec<_> = allowed
        .iter()
        .filter(|name| secrets.contains(**name))
        .collect();
    assert!(
        leaked_by_allowlist.is_empty(),
        "credential variables must never be on the pass-through list: {leaked_by_allowlist:?}"
    );
    for (name, value) in &env {
        if value.is_some() {
            assert!(
                allowed.contains(name.as_str()),
                "{name} is set on the module command but is not part of the isolated environment"
            );
        }
    }

    // Now what a spawned process actually receives. Seed the probe with a fake
    // real home and every provider key, as a developer's shell would, then
    // isolate it exactly as the module is isolated. Any of them surviving means
    // the inherited environment was not cleared.
    let mut probe = Command::new(std::env::current_exe().expect("this test binary"));
    probe.env("HOME", "/leaked-real-home");
    for name in &secrets {
        probe.env(name, "leaked-credential");
    }
    isolate_env(&mut probe, rig);
    probe.env(ENV_PROBE_MARKER, "1").args([
        ENV_PROBE_TEST,
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]);
    let output = probe
        .as_std_mut()
        .output()
        .expect("run the environment probe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "environment probe failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let received: HashMap<&str, &str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix(ENV_PROBE_LINE))
        .filter_map(|line| line.split_once('='))
        .collect();
    assert!(
        received.contains_key(ENV_PROBE_MARKER),
        "the probe printed no environment, so nothing below would be checked: {stdout}"
    );
    assert_eq!(
        received.get("HOME").copied(),
        rig.join("home").to_str(),
        "the probe must see the rig home, not the caller's"
    );
    // macOS adds `__CF_USER_TEXT_ENCODING` (the user's id and text encoding)
    // to every new process's environment even when the parent passed none, so
    // it arrives whatever the isolation does. It names no file and no secret.
    let added_by_os = ["__CF_USER_TEXT_ENCODING"];
    for name in received.keys() {
        assert!(
            allowed.contains(name) || *name == ENV_PROBE_MARKER || added_by_os.contains(name),
            "{name} reached the spawned process although the isolation does not set it: {received:?}"
        );
    }
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
async fn open_quota_route(
    sessions: HostSessions,
) -> (TestDaemon, ModuleProcess, tokio::net::TcpStream, Route) {
    let daemon = start_daemon().await;
    let module = spawn_quota_module(&daemon.connection_file_path, &daemon.temp_dir, sessions);
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
async fn drive_usage_get_for(
    want_provider: &str,
    sessions: HostSessions,
) -> (TestDaemon, ModuleProcess, Vec<Value>) {
    let (daemon, module, mut consumer, route) = open_quota_route(sessions).await;
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
    let (_daemon, _module, result) = drive_usage_get_for("codex", HostSessions::Isolated).await;
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
async fn scoped_inventory_routes_every_vault_backed_provider_family() {
    let daemon = start_daemon().await;
    let stub_credentials = [
        scoped_stub_credential("oauth:anthropic", "oauth", "anthropic", 1),
        scoped_stub_credential("oauth:anthropic:ufuk2", "oauth", "anthropic", 2),
        scoped_stub_credential("oauth:anthropic:umutaday", "oauth", "anthropic", 3),
        scoped_stub_credential("oauth:anthropic:wwaxgmail", "oauth", "anthropic", 4),
        scoped_stub_credential("oauth:anthropic:yiyi", "oauth", "anthropic", 5),
        scoped_stub_credential("chatgpt:openai", "oauth", "openai", 6),
        scoped_stub_credential("chatgpt:openai:gmail", "oauth", "openai", 7),
        scoped_stub_credential("antigravity:google", "oauth", "google", 8),
        scoped_stub_credential("oauth:xai", "oauth", "xai", 9),
        scoped_stub_credential("oauth:cursor", "oauth", "cursor", 10),
        scoped_stub_credential("apikey:kimi-for-coding", "apikey", "kimi-for-coding", 11),
        scoped_stub_credential("apikey:deepseek", "apikey", "deepseek", 12),
        scoped_stub_credential("apikey:openrouter", "apikey", "openrouter", 13),
        scoped_stub_credential("apikey:openai", "apikey", "openai", 14),
        scoped_stub_credential("apikey:openai:astro", "apikey", "openai", 15),
        scoped_stub_credential("apikey:cerebras", "apikey", "cerebras", 16),
        scoped_stub_credential("apikey:fireworks-ai", "apikey", "fireworks-ai", 17),
    ];
    let _vault_stub =
        start_vault_stub_with_credentials(&daemon.connection_file_path, &stub_credentials).await;
    wait_for_registration(&daemon.registry, VAULT_MODULE_ID, SETUP_TIMEOUT).await;

    let codex_home = daemon.temp_dir.join("isolated-codex-home");
    write_owner_only(
        &codex_home.join("auth.json"),
        br#"{"tokens":{"access_token":"local-token","account_id":"chatgpt:openai"}}"#,
    );
    write_owner_only(
        &codex_home.join("config.toml"),
        b"chatgpt_base_url = \"http://127.0.0.1:0/backend-api\"\n",
    );
    let child = quota_module_command(&daemon.connection_file_path, &daemon.temp_dir)
        .env("CODEX_HOME", &codex_home)
        .env_remove("KIMI_CODE_API_KEY")
        .spawn()
        .expect("spawn quota-module with the seventeen-row scoped inventory");
    let _module = ModuleProcess { child };
    wait_for_registration(&daemon.registry, MODULE_ID, SETUP_TIMEOUT).await;

    let project_root = daemon.temp_dir.join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let mut consumer = connect_consumer(&daemon.connection_file_path).await;
    let route = route_open(&mut consumer, &project_root, 20).await;

    let deadline = Instant::now() + Duration::from_secs(80);
    let mut corr = 21;
    let response = loop {
        let response = usage_get(&mut consumer, route, corr).await;
        let entries = response["result"].as_array().cloned().unwrap_or_default();
        let provider_settled = |provider: &str| {
            let has_declared_handle = stub_credentials.iter().any(|credential| match provider {
                "claude" => credential.id.starts_with("oauth:anthropic"),
                "kimi-for-coding" => credential.id == "apikey:kimi-for-coding",
                "codex" => credential.id.starts_with("chatgpt:openai"),
                _ => false,
            });
            !has_declared_handle
                || response["completeProviders"]
                    .as_array()
                    .is_some_and(|providers| providers.iter().any(|name| name == provider))
                || entries.iter().any(|entry| {
                    entry["provider"] == provider && entry["errorClass"] == "credential_absent"
                })
        };
        if ["claude", "kimi-for-coding", "codex"]
            .into_iter()
            .all(provider_settled)
        {
            break response;
        }
        assert!(
            Instant::now() < deadline,
            "vault-backed providers did not settle after scoped enumeration: {response:?}"
        );
        corr += 1;
        sleep(Duration::from_millis(100)).await;
    };
    let result = response["result"].as_array().unwrap();

    let expected_claude_accounts = [
        "oauth:anthropic",
        "oauth:anthropic:ufuk2",
        "oauth:anthropic:umutaday",
        "oauth:anthropic:wwaxgmail",
        "oauth:anthropic:yiyi",
    ];
    let claude = result
        .iter()
        .filter(|entry| entry["provider"] == "claude")
        .collect::<Vec<_>>();
    assert_eq!(
        claude.len(),
        expected_claude_accounts.len(),
        "claude must publish one entry per scoped anthropic handle: {claude:?}"
    );
    let mut claude_accounts = claude
        .iter()
        .filter_map(|entry| entry["account"].as_str())
        .collect::<Vec<_>>();
    claude_accounts.sort_unstable();
    assert_eq!(claude_accounts, expected_claude_accounts);
    assert!(
        claude
            .iter()
            .all(|entry| entry["errorClass"] == "decode_failed"),
        "each fake anthropic credential must resolve its handle before failing: {claude:?}"
    );

    let kimi = result
        .iter()
        .filter(|entry| entry["provider"] == "kimi-for-coding")
        .collect::<Vec<_>>();
    assert_eq!(
        kimi.len(),
        1,
        "kimi-for-coding must publish its scoped vault lane: {kimi:?}"
    );
    assert!(
        kimi[0]["account"] == "apikey:kimi-for-coding"
            && kimi[0]["errorClass"].is_string()
            && kimi[0]["errorClass"] != "credential_absent"
            && !kimi[0]["error"]
                .as_str()
                .is_some_and(|error| error.contains("KIMI_CODE_API_KEY")),
        "kimi-for-coding must resolve its scoped handle instead of the environment lane: {:?}",
        kimi[0]
    );

    let codex = result
        .iter()
        .filter(|entry| entry["provider"] == "codex")
        .collect::<Vec<_>>();
    assert_eq!(
        codex.len(),
        2,
        "codex must publish both scoped chatgpt handles: {codex:?}"
    );
    let mut codex_accounts = codex
        .iter()
        .filter_map(|entry| entry["account"].as_str())
        .collect::<Vec<_>>();
    codex_accounts.sort_unstable();
    assert_eq!(codex_accounts, ["chatgpt:openai", "chatgpt:openai:gmail"]);
    assert!(
        codex.iter().all(|entry| {
            entry["errorClass"].is_string() && entry["errorClass"] != "credential_absent"
        }),
        "each fake codex credential must resolve its handle before failing: {codex:?}"
    );

    let unrouted_ids = [
        "apikey:openai",
        "apikey:openai:astro",
        "apikey:cerebras",
        "apikey:fireworks-ai",
    ];
    let unrouted_entries = result
        .iter()
        .filter(|entry| {
            entry["account"]
                .as_str()
                .is_some_and(|account| unrouted_ids.contains(&account))
        })
        .collect::<Vec<_>>();
    assert!(
        unrouted_entries.is_empty(),
        "unsupported credential ids must not create provider lanes: {unrouted_entries:?}"
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
    let child = quota_module_command(&daemon.connection_file_path, &daemon.temp_dir)
        .env("CODEX_HOME", &codex_home)
        .spawn()
        .expect("spawn vault-wired quota-module");
    let _module = ModuleProcess { child };
    wait_for_registration(&daemon.registry, MODULE_ID, SETUP_TIMEOUT).await;

    let project_root = daemon.temp_dir.join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let mut consumer = connect_consumer(&daemon.connection_file_path).await;
    let route = route_open(&mut consumer, &project_root, 10).await;
    // DERIVED FROM THE SCHEDULER, NOT CHOSEN. This test drives the REAL registry,
    // so the second of codex's two handles is not fetched until codex's second
    // round: the refresher selects round-robin ACROSS providers under a
    // concurrency cap, so one round costs roughly (providers / cap) slot fetches,
    // each bounded by the fetch deadline.
    //
    // With 37 registered providers and a cap of 8, two rounds is on the order of a
    // minute on a host whose providers are configured and therefore actually make
    // network calls. The old 20s bound was written when fewer lanes had
    // credentials, and it held for exactly as long as that stayed true.
    //
    // MEASURED before changing it, so this is a corrected bound rather than a
    // raised one: at 20s the poll saw 23 of 37 providers and one of two codex
    // accounts -- a sweep still in progress, not a stuck one. At 90s the same test
    // passes in 101s with both accounts and their distinct percentages.
    //
    // CI is unaffected in either direction: with no credentials every lane fails
    // fast, a full sweep takes seconds, and this bound is never approached. It
    // only binds on a populated host, which is where it was failing.
    let deadline = Instant::now() + Duration::from_secs(150);
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
        // Fail-closed is about the second account's USAGE, not about its silence.
        // The labeled row must be gone and must not be stale-served; a single
        // UNLABELED verdict in its place is correct and is now asserted, because
        // dropping that too publishes the "not fetched yet" shape for a credential
        // the module reached and could not resolve (insula#8).
        let labeled_primary = codex
            .iter()
            .filter(|entry| entry["account"] == "account-primary")
            .collect::<Vec<_>>();
        let unlabeled_verdict = codex
            .iter()
            .filter(|entry| entry["account"].is_null() && !entry["error"].is_null())
            .count();
        let failed_closed = labeled_primary.len() == 1
            && labeled_primary[0]["error"].is_null()
            && labeled_primary[0]["usage"]["primary"]["usedPercent"] == 21.0
            && unlabeled_verdict == 1
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
    let (_daemon, _module, mut consumer, route) = open_quota_route(HostSessions::Isolated).await;

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
    let (_daemon, _module, result) = drive_usage_get_for("codex", HostSessions::Real).await;
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
    let (_daemon, _module, result) = drive_usage_get_for("claude", HostSessions::Real).await;
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
