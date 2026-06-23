#![allow(dead_code)]
//! Shared consumer-side wire driver for the quota-module e2e tests.
//!
//! Both harnesses — `skeleton_e2e` (in-process subc daemon) and `real_daemon_e2e`
//! (the standalone `subc-core` binary supervising the module) — drive the SAME
//! consumer path against the module: authenticate as a client, `catalog.list`,
//! `route.open` the ManagementSurface, then `usage.get` on the route channel. That
//! consumer driver lives here so the two harnesses share one implementation and
//! only differ in how the daemon is stood up.

use std::{
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::Value;
use subc_core::{read_frame, write_frame, Frame};
use subc_protocol::{BindIdentity, Flags, FrameType, Priority, RouteTarget};
use subc_transport::{authenticate_client, connection_file};
use tokio::{
    net::TcpStream,
    time::{timeout, Instant},
};

pub const MODULE_ID: &str = "ai-provider-quota";
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
pub const READ_TIMEOUT: Duration = Duration::from_secs(10);

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A process-unique temp path under the OS temp dir (no collision across tests).
pub fn unique_temp_dir(label: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{label}-{}-{n}", process::id()))
}

/// Connect to a daemon from its connection file and complete the client HMAC
/// handshake. Works identically for the in-process and real-binary daemons.
pub async fn connect_consumer(connection_file_path: &Path) -> TcpStream {
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

/// Send a channel-0 control request and read until its channel-0 reply for `corr`.
pub async fn control_rpc(stream: &mut TcpStream, corr: u64, body: Value) -> Frame {
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

pub async fn read_frame_timeout(stream: &mut TcpStream) -> Frame {
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for a frame")
}

/// `catalog.list` → the `modules` array.
pub async fn catalog_list(stream: &mut TcpStream, corr: u64) -> Vec<Value> {
    let frame = control_rpc(stream, corr, serde_json::json!({ "op": "catalog.list" })).await;
    assert_eq!(frame.header.ty, FrameType::Response);
    let value: Value = serde_json::from_slice(&frame.body).unwrap();
    value["modules"].as_array().cloned().unwrap_or_default()
}

/// Poll `catalog.list` until the quota module appears (real daemon spawns the
/// module asynchronously, so registration is not immediate after connect).
pub async fn wait_for_catalog(stream: &mut TcpStream, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 1000;
    loop {
        let modules = catalog_list(stream, corr).await;
        if modules.iter().any(|m| m["module_id"] == module_id) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} did not appear in catalog within {wait:?}"
        );
        corr += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn json_route_open(project_root: &Path) -> Value {
    let target = RouteTarget::ManagementSurface {
        module_id: MODULE_ID.to_string(),
    };
    let identity = BindIdentity {
        project_root: project_root.to_path_buf(),
        harness: "quota-e2e".to_string(),
        session: "session-1".to_string(),
    };
    serde_json::json!({
        "op": "route.open",
        "target": target,
        "identity": identity,
    })
}

/// `route.open` the management surface; returns the route channel.
pub async fn route_open(stream: &mut TcpStream, project_root: &Path, corr: u64) -> u16 {
    let frame = control_rpc(stream, corr, json_route_open(project_root)).await;
    assert_eq!(
        frame.header.ty,
        FrameType::Response,
        "route.open should succeed: {}",
        String::from_utf8_lossy(&frame.body)
    );
    let value: Value = serde_json::from_slice(&frame.body).unwrap();
    value["route_channel"].as_u64().unwrap() as u16
}

/// Send a `usage.get` request on the route channel and return the decoded body.
pub async fn usage_get(stream: &mut TcpStream, route_channel: u16, corr: u64) -> Value {
    raw_route_request(
        stream,
        route_channel,
        corr,
        serde_json::json!({ "method": "usage.get", "params": {} }),
    )
    .await
}

/// Send a raw data-plane request on the route channel; returns the decoded body of
/// the terminal Response (panics on Error).
pub async fn raw_route_request(
    stream: &mut TcpStream,
    route_channel: u16,
    corr: u64,
    body: Value,
) -> Value {
    let frame = raw_route_frame(stream, route_channel, corr, body).await;
    match frame.header.ty {
        FrameType::Response => serde_json::from_slice(&frame.body).unwrap(),
        FrameType::Error => panic!(
            "route request returned error: {}",
            String::from_utf8_lossy(&frame.body)
        ),
        ty => panic!("unexpected route frame {ty:?}"),
    }
}

/// Like [`raw_route_request`] but returns the raw terminal frame (Response OR
/// Error) for callers asserting the error contract.
pub async fn raw_route_frame(
    stream: &mut TcpStream,
    route_channel: u16,
    corr: u64,
    body: Value,
) -> Frame {
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
        if frame.header.corr == corr
            && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
        {
            return frame;
        }
    }
}
