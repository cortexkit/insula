//! Run the wire-sanity checks against what the **deployed module** publishes.
//!
//! This is the companion to `quota-core`'s `wire-sanity` example, and the
//! difference is the point. That one builds a registry in-process, which has no
//! credential-vault client — so it cannot see the lane that serves most of the
//! labelled accounts in production, and a clean result there says nothing about
//! them. This connects to the running daemon as an ordinary consumer and checks
//! the array the module actually serves, vault lane included.
//!
//! The checks are `quota_core::wire_sanity`, shared with the other example rather
//! than reimplemented, so the two cannot drift into disagreeing about what a
//! coherent window is.
//!
//! Run: `cargo run -p quota-module --example deployed-sanity`
//! Filter to one provider: `cargo run -p quota-module --example deployed-sanity -- codex`
//!
//! Exits non-zero when something disagrees, or when it could not examine
//! anything, so it can gate a deploy.

use std::{path::PathBuf, time::Duration};

// Reached through `quota_core` rather than added as direct dependencies: this
// example must decode with the exact same type definition the module serves, and
// a separate dependency line here could drift to a different version of the
// shared crate while still compiling.
use quota_core::{model::ProviderUsage, wire_sanity};

#[path = "../tests/common/mod.rs"]
mod common;

/// Where the daemon writes its connection file. Overridable so this can be
/// pointed at a non-default daemon.
fn connection_file() -> PathBuf {
    if let Ok(path) = std::env::var("SUBC_CONNECTION_FILE") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".local/share/cortexkit/run/subc-connection.json")
}

#[tokio::main]
async fn main() {
    let filter = std::env::args().nth(1);
    let path = connection_file();
    if !path.exists() {
        eprintln!("no daemon connection file at {}", path.display());
        eprintln!("the daemon must be running: this checks the deployed module, not a local build");
        std::process::exit(2);
    }

    let mut stream = common::connect_consumer(&path).await;
    common::wait_for_catalog(&mut stream, common::MODULE_ID, Duration::from_secs(10)).await;
    let route = common::route_open(&mut stream, &std::env::temp_dir(), 1).await;
    // Called BEFORE usage.get so a broken drops op cannot hide behind a clean
    // usage report. Shipping an operation and verifying only the neighbouring one
    // is how a fix looked complete for a week in August: the tests drove a path
    // production does not take, and nothing read the wire.
    let drops = common::raw_route_request(
        &mut stream,
        route,
        2,
        serde_json::json!({ "method": "usage.drops", "params": {} }),
    )
    .await;
    check_drop_page(&drops);

    let body = common::usage_get(&mut stream, route, 3).await;

    let health_findings = check_health_identity(&mut stream).await;

    // The envelope is an OBJECT with `result` beside `completeProviders`, and this
    // checker had verified only the first half since the day the second shipped --
    // caught 2026-08-29 when a consumer reported the route hands back a bare array
    // and I could not disprove it from my own live check. `completeProviders` is
    // the ONLY thing that authorises a consumer to prune stored accounts, so a
    // checker blind to it would pass through the exact regression that silently
    // costs someone their account rows.
    //
    // Asserted as SHAPE, not as membership: which providers are complete varies
    // with what resolved identity this tick, so pinning names would fail on an
    // ordinary degraded lane. What must hold is that the key EXISTS, is an array,
    // and names only registered providers -- a rename or a dropped key reddens.
    let complete = body.get("completeProviders").unwrap_or_else(|| {
        panic!(
            "usage.get envelope must carry completeProviders beside result; got keys {:?}",
            body.as_object().map(|map| map.keys().collect::<Vec<_>>())
        )
    });
    let complete: Vec<String> = serde_json::from_value(complete.clone())
        .expect("completeProviders must decode as a string array");

    let entries: Vec<ProviderUsage> = serde_json::from_value(body["result"].clone())
        .expect("usage.get result must decode as ProviderUsage[]");

    // Cross-checked against the SERVED set rather than a hardcoded list, because a
    // hardcoded list is a second copy of the registry that drifts on the next
    // provider added. A name here that no entry carries means the completeness
    // claim references a provider this response never mentioned -- which is the
    // shape that would authorise pruning accounts under a name nobody served.
    let served: std::collections::BTreeSet<&str> = entries
        .iter()
        .map(|entry| entry.provider.as_str())
        .collect();
    for name in &complete {
        assert!(
            served.contains(name.as_str()),
            "completeProviders names {name:?}, which no entry in this same response carries"
        );
    }

    // Decoding through the wire type rather than reading JSON keys by hand is
    // deliberate: a field renamed in the shared crate has to fail here, instead
    // of silently reading as absent and shrinking what gets checked.
    let entries: Vec<ProviderUsage> = match filter.as_deref() {
        Some(name) => entries
            .into_iter()
            .filter(|entry| entry.provider == name)
            .collect(),
        None => entries,
    };

    if entries.is_empty() {
        match filter.as_deref() {
            Some(name) => println!("no entries for {name}: nothing was checked"),
            None => println!("the module published an empty array: nothing was checked"),
        }
        // Health is a separate axis and is still worth reporting: an empty array
        // is exactly when a consumer consults it, so suppressing it here would
        // hide the answer at the moment it is asked for.
        for finding in &health_findings {
            println!("  {finding}");
        }
        std::process::exit(2);
    }

    let report = wire_sanity::check_entries(&entries, wire_sanity::now());
    let labelled = entries
        .iter()
        .filter(|entry| entry.account.is_some())
        .count();
    let vault = entries
        .iter()
        .filter(|entry| entry.source.as_deref() == Some("vault"))
        .count();

    // Names the lane for the same reason the local checker does: both finish
    // with "findings: none", and only one of them saw the vault.
    println!("lane: deployed module through the daemon (vault lane included)");
    println!(
        "entries: {} ({} degraded, {labelled} labelled, {vault} vault-served)   windows checked: {}   pools checked: {} ({} amounts, {} bound comparisons)   providers compared: {}",
        report.entries,
        report.degraded,
        report.windows_checked,
        report.pools_checked,
        report.pool_amounts_checked,
        report.pool_comparisons,
        report.providers_compared
    );

    // Findings are reported before the no-windows exit. The cross-entry checks
    // examine degraded entries too, so an all-degraded array can still carry real
    // findings -- exiting on "nothing examined" first would discard precisely the
    // ones that survived the condition suppressing everything else.
    let findings: Vec<&String> = report
        .findings
        .iter()
        .chain(health_findings.iter())
        .collect();
    if !findings.is_empty() {
        println!("findings: {}", findings.len());
        for finding in &findings {
            println!("  {finding}");
        }
        std::process::exit(1);
    }
    if report.examined_nothing() {
        println!("no windows to check: every entry is degraded");
        std::process::exit(2);
    }
    println!("findings: none");
}

/// Check the health conservation identity against the deployed module.
///
/// Consumers are told to assert `fresh + stale + pending + degraded +
/// unconfigured + withoutHandles == providersTotal` and alert on an imbalance, so the producer
/// should be checking it too -- a bucket that classifies a provider into nothing
/// under-sums silently, and the first person to notice would otherwise be a
/// consumer whose alert fires for a reason they cannot diagnose.
///
/// Read from the JSON metrics rather than through a typed struct, because this
/// must see what a consumer sees: a field that stopped being published should
/// fail here, not be quietly defaulted.
///
/// Asked as `supervisor.health`, which is the consumer-facing op. `health.check`
/// is the daemon's own request to a module and is not in the consumer
/// vocabulary, so this reads the record the supervisor last collected -- the same
/// one `ck module status` shows.
/// Pull this module's health metrics out of the supervisor's reply.
///
/// Searched by module id rather than by position: the reply covers every
/// supervised module, and indexing into it would silently start reading a
/// different module's numbers the moment the fleet's composition changed.
fn find_module_metrics(body: &serde_json::Value) -> Option<serde_json::Value> {
    fn walk(value: &serde_json::Value) -> Option<serde_json::Value> {
        match value {
            serde_json::Value::Object(map) => {
                let is_this_module = map
                    .get("module_id")
                    .or_else(|| map.get("id"))
                    .and_then(|id| id.as_str())
                    == Some(common::MODULE_ID);
                if is_this_module {
                    if let Some(metrics) = map.get("health").and_then(|h| h.get("metrics")) {
                        return Some(metrics.clone());
                    }
                    if let Some(metrics) = map.get("metrics") {
                        return Some(metrics.clone());
                    }
                }
                map.values().find_map(walk)
            }
            serde_json::Value::Array(values) => values.iter().find_map(walk),
            _ => None,
        }
    }
    walk(body)
}

async fn check_health_identity(stream: &mut tokio::net::TcpStream) -> Vec<String> {
    let reply =
        common::control_rpc(stream, 3, serde_json::json!({ "op": "supervisor.health" })).await;
    let body: serde_json::Value = match serde_json::from_slice(&reply.body) {
        Ok(value) => value,
        Err(error) => return vec![format!("supervisor.health reply did not decode: {error}")],
    };
    let Some(metrics) = find_module_metrics(&body) else {
        return vec![format!(
            "supervisor.health carried no metrics for {}",
            common::MODULE_ID
        )];
    };
    let metrics = &metrics;

    let number = |key: &str| metrics[key].as_u64();
    let list_len = |key: &str| metrics[key].as_array().map(|values| values.len() as u64);

    // Every term is required. Reading a missing field as zero would let the
    // identity balance by arithmetic while the module had stopped reporting a
    // whole bucket -- the failure this check exists to catch.
    let terms = [
        ("fresh", number("fresh")),
        ("stale", number("stale")),
        ("pending", number("pending")),
        ("degraded", list_len("degraded")),
        ("unconfigured", list_len("unconfigured")),
        ("withoutHandles", list_len("withoutHandles")),
        ("providersTotal", number("providersTotal")),
    ];
    let missing: Vec<&str> = terms
        .iter()
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| *name)
        .collect();
    if !missing.is_empty() {
        return vec![format!(
            "health metrics missing the terms of the conservation identity: {}",
            missing.join(", ")
        )];
    }

    // Before the refresher's first tick no provider is in any bucket, so the
    // identity is legitimately false and `lastTickAgeSecs` is the signal that it
    // has become meaningful. Checking it earlier would report an imbalance at
    // every module start, and a check that cries wolf at boot gets ignored by
    // the time it has something real to say.
    if metrics["lastTickAgeSecs"].is_null() {
        return Vec::new();
    }

    let value = |name: &str| {
        terms
            .iter()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| *value)
            .unwrap_or_default()
    };
    let sum = value("fresh")
        + value("stale")
        + value("pending")
        + value("degraded")
        + value("unconfigured")
        + value("withoutHandles");
    let total = value("providersTotal");
    if sum != total {
        return vec![format!(
            "health buckets do not account for every provider: fresh {} + stale {} + pending {} \
             + degraded {} + unconfigured {} + withoutHandles {} = {sum}, but providersTotal is \
             {total}",
            value("fresh"),
            value("stale"),
            value("pending"),
            value("degraded"),
            value("unconfigured"),
            value("withoutHandles"),
        )];
    }
    Vec::new()
}

/// Check the shape a consumer polls for events, on the deployed module.
///
/// Deliberately not "did it return 200". The three fields a consumer needs are
/// exactly the three that make an empty page readable, and an empty ring -- the
/// ordinary state on a quiet host -- is when they are easiest to get wrong,
/// because every assertion about the records themselves passes vacuously.
fn check_drop_page(page: &serde_json::Value) {
    let epoch = page["epoch"].as_str().unwrap_or_else(|| {
        panic!("usage.drops must state an epoch, or a held cursor cannot be validated: {page}")
    });
    assert!(
        !epoch.trim().is_empty(),
        "an empty epoch identifies nothing: {page}"
    );

    let next = page["next"].as_u64().unwrap_or_else(|| {
        panic!("usage.drops must state the next sequence so a consumer can resume: {page}")
    });

    let drops = page["drops"]
        .as_array()
        .unwrap_or_else(|| panic!("usage.drops must carry a drops array, empty or not: {page}"));

    // `oldestRetained` is absent exactly when the ring is empty, and present
    // otherwise. Checking BOTH directions here rather than one, because the
    // absent case is the one this host produces and it would pass on its own.
    match page.get("oldestRetained").and_then(|v| v.as_u64()) {
        None => assert!(
            drops.is_empty(),
            "a ring holding records must say which is oldest, or a consumer \
             cannot tell a lost cursor from a quiet interval: {page}"
        ),
        Some(oldest) => {
            assert!(
                !drops.is_empty(),
                "an empty ring must not claim to retain a sequence: {page}"
            );
            assert!(
                oldest < next,
                "the oldest retained sequence must precede the next: {page}"
            );
        }
    }

    for drop in drops {
        assert!(
            drop["seq"].as_u64().is_some_and(|seq| seq < next),
            "every record needs a sequence below the next cursor: {drop}"
        );
        let at = drop["at"].as_str().unwrap_or_else(|| {
            panic!(
                "every record needs a timestamp, so a consumer can resume from its own log: {drop}"
            )
        });
        // Shape-checked rather than parsed: this crate does not depend on a date
        // library, and pulling one in for a checker would add a dependency to the
        // deployed binary's crate to satisfy an example. The property that
        // matters here is that a timestamp is present and looks like an instant;
        // the canonical-format guarantee is pinned by unit tests in quota-core.
        assert!(
            at.len() >= 20 && at.contains('T') && (at.ends_with('Z') || at.contains('+')),
            "record timestamp does not look like RFC3339: {drop}"
        );
        assert!(
            drop["observedContinuously"].is_boolean(),
            "the confidence flag must be stated, never inferred from absence: {drop}"
        );
    }

    println!(
        "  usage.drops: epoch {epoch}, next {next}, {} record(s) retained",
        drops.len()
    );
}
