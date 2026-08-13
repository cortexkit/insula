//! Check that every configured vault credential is actually serving usage.
//!
//! ## Why this exists
//!
//! The module's other checks all measure internal agreement — health buckets
//! that sum, an envelope whose fields do not contradict each other, a build
//! stamp matching a commit. None of them answers *did something stop being
//! served*, because **a set that shrinks stays consistent**: providers that
//! vanish move between buckets and every total still balances.
//!
//! That gap has been reached in production. The credential vault's daemon module
//! id changed, this module kept dialling the old one, and every vault-served
//! account went dark for hours while the health status read `ok`, the
//! conservation identity held exactly, and the wire-sanity checker found nothing
//! to report. A wrong module id answers `unknown_module`, which is classified
//! transient because a restarting module answers identically, so the refresher
//! retried forever and never reached a verdict anyone could see.
//!
//! ## What it checks
//!
//! The credential handle file is the declared intent: each key is a credential
//! this host is configured to use. This walks those keys, maps each to the
//! provider that consumes it, and asserts the deployed module is serving usage
//! on that provider's vault lane.
//!
//! It is deliberately **discriminating** rather than a health reading — it fails
//! when a lane is dark, and it cannot pass for the wrong reason, because the
//! evidence it requires (a `source` of `vault` on an entry carrying usage) is
//! producible only by a live credential fetch.
//!
//! ## Scope, stated so a clean run is not over-read
//!
//! A provider is counted as serving when *any* of its vault handles resolved.
//! Per-handle attribution is not always possible on the wire: several providers
//! resolve no account identity, so their handles are indistinguishable once
//! emitted. This therefore catches a lane that is entirely dark — which is the
//! failure that has actually happened — and not the loss of one handle among
//! several for the same provider.
//!
//! **A provider with a second, non-vault lane is not covered.** Where a provider
//! can reach its upstream another way, that lane keeps the entry present and
//! sourced to itself, so a dark vault lane is invisible here. Such providers are
//! listed in [`DUAL_LANE`] with the reason, and they are reported rather than
//! silently skipped — a checker that omits a member without saying so is
//! indistinguishable from one that examined it.
//!
//! Run against the deployed module through the daemon:
//! `cargo run -p quota-module --example vault-lanes`

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

// Reached through `quota_core` rather than as a direct dependency, so this
// decodes with the exact type the module serves. A separate dependency line
// could drift to a different version of the shared crate and still compile.
use quota_core::model::ProviderUsage;

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

/// Providers that can serve from something other than a stored credential.
///
/// Requiring a `vault` source from these produces a false alarm whenever the
/// other lane is healthy, and a checker that cries wolf on a working provider
/// stops being read — which costs more than the coverage it buys, because the
/// failure this exists to catch takes down *every* stored lane at once and the
/// remaining providers still prove it.
const DUAL_LANE: &[(&str, &str)] = &[
    (
        "antigravity",
        "a local editor process is probed first and wins when both are healthy",
    ),
    (
        "grok",
        "a local opencode oauth token reaches the same account, and grok resolves \
no account identity, so both lanes dedup into one entry whose source names \
whichever won",
    ),
];

/// Maps a credential handle key to the provider that consumes it.
///
/// Keys are matched by prefix because the vault mints additional accounts for a
/// family as `<base>:<label>` — `oauth:anthropic:ufuk2` alongside
/// `oauth:anthropic`. Exact matching here would silently ignore every secondary
/// account, which is the same defect this checker exists to catch.
///
/// The family list is the library's own, not a copy. A restated one would drift,
/// and the drift is silent in the direction that matters: a family this checker
/// lacked would be reported as "handles no provider here consumes", which reads
/// like a stray credential rather than a gap in the checker, so the lane it
/// should have examined goes unchecked and the run still ends in `findings:
/// none`.
///
/// Sharing it does not weaken the check. What is compared is what this host is
/// CONFIGURED for against what the wire is SERVING, and those two remain
/// independent of each other — a family mapped to the wrong provider still
/// leaves the right provider with no credential, so the lane goes dark and this
/// fires anyway.
fn provider_for_handle(key: &str) -> Option<&'static str> {
    quota_core::vault_handles::CREDENTIAL_FAMILIES
        .iter()
        .find(|(prefix, _)| key == *prefix || key.starts_with(&format!("{prefix}:")))
        .map(|(_, provider)| *provider)
}

#[tokio::main]
async fn main() {
    let handles_path = match std::env::var_os("CK_QUOTA_VAULT_HANDLES_PATH") {
        Some(path) => std::path::PathBuf::from(path),
        None => match std::env::var_os("HOME") {
            Some(home) => {
                std::path::PathBuf::from(home).join(".config/cortexkit/ck-quota/vault-handles.json")
            }
            None => {
                eprintln!("cannot resolve HOME to find the credential handle file");
                std::process::exit(2);
            }
        },
    };

    let raw = match std::fs::read_to_string(&handles_path) {
        Ok(raw) => raw,
        Err(error) => {
            // No handle file means no vault credentials are configured, so there
            // is nothing this check can assert. Exit 2 rather than 0: a clean
            // pass would claim every configured lane is serving, which is
            // vacuously true and indistinguishable from a real one.
            eprintln!("no credential handle file to check ({error})");
            std::process::exit(2);
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("credential handle file is not readable JSON: {error}");
            std::process::exit(2);
        }
    };

    let mut expected: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let mut unmapped: Vec<String> = Vec::new();
    if let Some(handles) = parsed.get("handles").and_then(|h| h.as_object()) {
        for key in handles.keys() {
            match provider_for_handle(key) {
                Some(provider) => expected.entry(provider).or_default().push(key.clone()),
                // An unmapped key is reported rather than ignored: it means this
                // host is configured for a credential no provider here consumes,
                // which is a real configuration finding and would otherwise be
                // invisible.
                None => unmapped.push(key.clone()),
            }
        }
    }

    if expected.is_empty() {
        eprintln!("credential handle file names no handles this module consumes");
        std::process::exit(2);
    }

    let path = connection_file();
    if !path.exists() {
        eprintln!("no daemon connection file at {}", path.display());
        eprintln!("the daemon must be running: this checks the deployed module, not a local build");
        std::process::exit(2);
    }

    let mut stream = common::connect_consumer(&path).await;
    common::wait_for_catalog(&mut stream, common::MODULE_ID, Duration::from_secs(10)).await;
    let route = common::route_open(&mut stream, &std::env::temp_dir(), 1).await;
    let body = common::usage_get(&mut stream, route, 2).await;

    let entries: Vec<ProviderUsage> = serde_json::from_value(body["result"].clone())
        .expect("usage.get result must decode as ProviderUsage[]");

    // Providers whose account set the producer fully enumerated this tick. A
    // provider ABSENT from this list published fewer accounts than it holds,
    // which is the failure a per-provider "did it serve at all" reading cannot
    // see: one handle that resolves no identity collapses every sibling into a
    // single unlabeled entry, so a provider with four configured accounts serves
    // one row and still looks alive.
    //
    // The case that motivated reading it: a handle left pointing at a credential
    // the vault no longer holds. It can never resolve, so it suppresses the
    // labels of every healthy account beside it, permanently and silently.
    let complete: BTreeSet<String> = body
        .get("completeProviders")
        .and_then(|value| serde_json::from_value::<Vec<String>>(value.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut serving_vault: BTreeSet<String> = BTreeSet::new();
    for entry in &entries {
        if entry.source.as_deref() == Some("vault") && entry.usage.is_some() {
            serving_vault.insert(entry.provider.clone());
        }
    }

    let mut dark: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut uncovered: Vec<(&'static str, &'static str)> = Vec::new();
    println!("  configured vault lanes: {}", expected.len());
    for (provider, keys) in &expected {
        if let Some((_, reason)) = DUAL_LANE.iter().find(|(name, _)| name == provider) {
            println!(
                "    {:16} {:9} {} handle(s): {}",
                provider,
                "uncovered",
                keys.len(),
                keys.join(", ")
            );
            uncovered.push((provider, reason));
            continue;
        }
        let ok = serving_vault.contains(*provider);
        println!(
            "    {:16} {:9} {} handle(s): {}",
            provider,
            if ok { "serving" } else { "DARK" },
            keys.len(),
            keys.join(", ")
        );
        if !ok {
            dark.push((provider, keys.clone()));
        }
    }

    for (provider, reason) in &uncovered {
        println!("  not checked - {provider}: {reason}");
    }

    let checked = expected.len() - uncovered.len();
    println!("  checked {checked} of {} configured lanes", expected.len());
    if checked == 0 {
        eprintln!("no lane was actually checked; a clean result here would be vacuous");
        std::process::exit(2);
    }

    // An unmapped handle is a finding, not a note. This host holds a credential
    // that no provider consumes, so it is being maintained and refreshed while
    // reaching no upstream -- indistinguishable, from the wire, from a lane that
    // was never configured. Printing it beside `findings: none` and exiting 0 is
    // the exact shape this checker exists to refuse: the fact was on screen and
    // the exit code said everything was fine.
    if !unmapped.is_empty() {
        println!(
            "  findings: {} handle(s) no provider here consumes",
            unmapped.len()
        );
        for key in &unmapped {
            println!("    {key}: configured on this host and reaching no provider");
        }
    }

    // A provider serving from the vault but absent from `completeProviders`
    // published fewer accounts than it holds. Reported separately from a dark
    // lane because the lane is UP: it serves real usage, and only the per-account
    // breakdown is missing, so every other reading here says it is healthy.
    // Absence from `completeProviders` is necessary but not sufficient: several
    // providers resolve no account identity at all -- their upstream returns no
    // account id -- so they are permanently absent from that list while being
    // entirely healthy. Requiring FEWER PUBLISHED ENTRIES THAN CONFIGURED
    // HANDLES separates the two without naming any provider, which matters
    // because the null-identity set changes as upstreams add or drop the field.
    let mut incomplete: Vec<(&'static str, usize, usize)> = Vec::new();
    for (provider, keys) in &expected {
        let is_dual = DUAL_LANE.iter().any(|(name, _)| name == provider);
        if is_dual || !serving_vault.contains(*provider) || complete.contains(*provider) {
            continue;
        }
        let published = entries
            .iter()
            .filter(|entry| entry.provider == *provider)
            .count();
        if published < keys.len() {
            incomplete.push((provider, published, keys.len()));
        }
    }
    if !incomplete.is_empty() {
        println!(
            "  findings: {} provider(s) serving with an incomplete account set",
            incomplete.len()
        );
        for (provider, published, configured) in &incomplete {
            println!(
                "    {provider}: {published} entr(ies) published against {configured} configured \
                 handle(s); a handle that resolves no account identity collapses its healthy \
                 siblings into one unlabeled row"
            );
        }
    }

    if dark.is_empty() {
        if unmapped.is_empty() && incomplete.is_empty() {
            println!("  findings: none");
            return;
        }
        std::process::exit(1);
    }

    println!("  findings: {} lane(s) dark", dark.len());
    for (provider, keys) in &dark {
        println!(
            "    {provider}: configured with {} handle(s) and serving no vault usage \
             — check the credential vault's module id against the daemon config",
            keys.len()
        );
    }
    std::process::exit(1);
}
