#![forbid(unsafe_code)]

//! Real-daemon supervision proof: the STANDALONE `subc-core` binary reads a
//! `subc.jsonc`, spawns + supervises `quota-module` as a child process it owns, and
//! routes `usage.get` end-to-end.
//!
//! This is the "is it actually a subc module" gate that `skeleton_e2e` cannot give:
//! skeleton_e2e runs the daemon IN-PROCESS and spawns the module itself. Here a real
//! `subc-core` process — launched exactly as a user would run it — does the
//! spawning from config, injects `SUBC_MODULE_ID`, and supervises the child. We only
//! drive the consumer; the daemon owns the module lifecycle.
//!
//! Requires both binaries built (the harness builds them): `quota-module` (this
//! workspace) and `subc-core` (the sibling `../subconscious` workspace). The test is
//! `#[ignore]` by default because it shells out to `cargo build` in a sibling repo
//! and binds real loopback ports — run it explicitly with
//! `cargo test -p quota-module --test real_daemon_e2e -- --ignored --nocapture`.

mod common;

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::process::{Child, Command};

use common::{
    connect_consumer, raw_route_frame, route_open, unique_temp_dir, usage_get, wait_for_catalog,
    MODULE_ID, SETUP_TIMEOUT,
};

const SUBCONSCIOUS_REL: &str = "../../../subconscious";

/// A real `subc-core` daemon process plus its isolated rig dir; killed on drop.
struct RealDaemon {
    child: Child,
    rig: PathBuf,
    connection_file: PathBuf,
}

impl Drop for RealDaemon {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.rig);
    }
}

/// `CARGO_MANIFEST_DIR` is `crates/quota-module`; the sibling subconscious repo is
/// resolved relative to it so the test is location-independent.
fn subconscious_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(SUBCONSCIOUS_REL)
        .canonicalize()
        .expect("sibling ../subconscious repo must exist for the real-daemon test")
}

/// Build a binary in the subconscious workspace and return its path.
fn build_subc_core() -> PathBuf {
    let root = subconscious_root();
    let status = std::process::Command::new(env!("CARGO"))
        .current_dir(&root)
        .args(["build", "--bin", "subc-core"])
        .status()
        .expect("run cargo build for subc-core");
    assert!(status.success(), "building subc-core failed");
    let bin = root.join("target/debug/subc-core");
    assert!(
        bin.exists(),
        "subc-core binary missing at {}",
        bin.display()
    );
    bin
}

/// Launch a real subc-core daemon with an isolated rig whose subc.jsonc supervises
/// our freshly-built quota-module binary. Waits for the connection file.
async fn start_real_daemon() -> RealDaemon {
    let subc_core = build_subc_core();
    let quota_module = PathBuf::from(env!("CARGO_BIN_EXE_ck-quota"));
    assert!(quota_module.exists());

    let rig = unique_temp_dir("quota-real-daemon");
    let config_dir = rig.join("config/cortexkit");
    let runtime_dir = rig.join("runtime");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&runtime_dir).unwrap();

    // The subc.jsonc the daemon reads: one module entry keyed by module_id, pointing
    // `program` at our binary. The daemon appends `--subc <connfile>` and injects
    // SUBC_MODULE_ID itself — we set neither here.
    let subc_jsonc = serde_json::json!({
        "version": 1,
        "modules": {
            MODULE_ID: { "program": quota_module, "args": [], "env": {} }
        }
    });
    std::fs::write(
        config_dir.join("subc.jsonc"),
        serde_json::to_vec_pretty(&subc_jsonc).unwrap(),
    )
    .unwrap();

    let child = Command::new(&subc_core)
        .env("XDG_CONFIG_HOME", rig.join("config"))
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("SUBC_PORT", "0") // ephemeral port
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn real subc-core daemon");

    let connection_file = runtime_dir.join("subc-connection.json");
    let deadline = tokio::time::Instant::now() + SETUP_TIMEOUT;
    while !connection_file.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon did not publish a connection file within {SETUP_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    RealDaemon {
        child,
        rig,
        connection_file,
    }
}

/// The full supervision proof: a real subc-core binary spawns the module from
/// subc.jsonc, it registers, and usage.get routes end-to-end returning a
/// ProviderUsage[] array (silent-degrade entries are fine — the point is the
/// real spawn + route, not live windows).
#[tokio::test]
#[ignore = "builds subc-core in ../subconscious and binds loopback ports"]
async fn real_subc_core_supervises_quota_module_and_routes_usage_get() {
    let daemon = start_real_daemon().await;
    let mut consumer = connect_consumer(&daemon.connection_file).await;

    // The daemon spawns the module asynchronously; poll the catalog until it lands.
    wait_for_catalog(&mut consumer, MODULE_ID, SETUP_TIMEOUT).await;

    let project_root = unique_temp_dir("quota-real-daemon-project");
    std::fs::create_dir_all(&project_root).unwrap();
    let route_channel = route_open(&mut consumer, &project_root, 1).await;

    // Serving is cache-only: the module returns whatever its background
    // refresher has resolved so far, so poll until the first sweep warms the
    // array (or a deadline) — the real-consumer view of async-refreshed data.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
    let mut corr = 2;
    let result = loop {
        let response = usage_get(&mut consumer, route_channel, corr).await;
        let result = response["result"]
            .as_array()
            .cloned()
            .expect("usage.get response must carry a result array");
        if !result.is_empty() || std::time::Instant::now() >= deadline {
            break result;
        }
        corr += 1;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    // Entries may be healthy or degraded depending on which provider sessions
    // exist on this machine. The proof is the SUPERVISION + ROUTE path through a
    // real daemon, so a non-panicking ProviderUsage[] suffices.
    eprintln!(
        "[real-daemon] usage.get returned {} provider entries",
        result.len()
    );
    assert!(
        !result.is_empty(),
        "expected the provider registry to return entries (healthy or degraded)"
    );

    let _ = std::fs::remove_dir_all(&project_root);
}

/// The module-level error contract holds through a REAL daemon too: an unknown
/// method on the route channel returns a canonical Error frame, not a Response.
#[tokio::test]
#[ignore = "builds subc-core in ../subconscious and binds loopback ports"]
async fn real_daemon_unknown_method_returns_error_frame() {
    let daemon = start_real_daemon().await;
    let mut consumer = connect_consumer(&daemon.connection_file).await;
    wait_for_catalog(&mut consumer, MODULE_ID, SETUP_TIMEOUT).await;

    let project_root = unique_temp_dir("quota-real-daemon-err");
    std::fs::create_dir_all(&project_root).unwrap();
    let route_channel = route_open(&mut consumer, &project_root, 1).await;

    let frame = raw_route_frame(
        &mut consumer,
        route_channel,
        2,
        serde_json::json!({ "method": "does.not.exist", "params": {} }),
    )
    .await;
    assert_eq!(
        frame.header.ty,
        subc_protocol::FrameType::Error,
        "unknown method must return an Error frame"
    );
    let body: subc_protocol::ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body.code, "unknown_method");

    let _ = std::fs::remove_dir_all(&project_root);
}
