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
/// Providers that can serve from something other than a stored credential.
///
/// Requiring a `vault` source from these produces a false alarm whenever the
/// other lane is healthy, and a checker that cries wolf on a working provider
/// stops being read -- which costs more than the coverage it buys, because the
/// failure this exists to catch takes down *every* stored lane at once and the
/// remaining providers still prove it.
///
/// EVERY ENTRY IS CHECKED AGAINST THE PROVIDER IT DESCRIBES, by
/// [`stale_exemptions`]. One entry here (grok) outlived its reason within hours:
/// it said "a local opencode oauth token reaches the same account", which stopped
/// being true when grok moved to vault-only custody the same day. The checker
/// went on exempting a provider it could by then verify and printed
/// `checked 4 of 6` -- a sentence that reads as a fact about the host rather than
/// a fact about a table nobody re-read.
///
/// It failed SAFE, which is why it was invisible: under-reporting coverage raises
/// no alarm, so nothing pressures anyone to re-read it. An operator caught it by
/// reading this file against the commit that invalidated it.
const DUAL_LANE: &[(&str, &str)] = &[(
    "antigravity",
    "a local editor process is probed first and wins when both are healthy",
)];

/// Entries in [`DUAL_LANE`] whose premise no longer holds.
///
/// Every reason there is a claim about one thing: the provider enumerates a lane
/// BESIDE its vault handles, so the read-time dedup can publish the other lane's
/// source. Ask the provider. If it enumerates only vault handles, the exemption
/// is suppressing a check that would now pass, and that is a finding rather than
/// a note -- the whole point is that a silent exemption is a lane which has
/// quietly stopped being verified.
///
/// NOT A REPLACEMENT FOR THE TABLE, and the difference is load-bearing. Deriving
/// the exemption outright ("two lanes exist, so skip") also catches codex, whose
/// second lane is a DIFFERENT ACCOUNT rather than a competing lane for the same
/// one -- measured on this host: codex publishes two rows with two identities,
/// one `vault` and one `oauth`, so requiring a vault-sourced row is meaningful
/// there and skipping it would lose a real check. Coexistence is necessary for
/// the exemption and not sufficient; what antigravity's entry actually records is
/// PRECEDENCE, which cannot be observed without re-running the dedup.
fn stale_exemptions(registry: &quota_core::Registry) -> Vec<&'static str> {
    let dual = registry.providers_with_a_lane_beside_vault();
    DUAL_LANE
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !dual.contains(name))
        .collect()
}

/// Maps a credential handle key to every provider that consumes it.
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
fn providers_for_handle(key: &str) -> Vec<&'static str> {
    // A cookie may deliberately feed both OpenCode plans. The list is therefore
    // collected rather than first-matched; ordinary families still yield one.
    quota_core::vault_handles::CREDENTIAL_FAMILIES
        .iter()
        .filter(|(prefix, _)| quota_core::vault_handles::handle_id_names_family(key, prefix))
        .map(|(_, provider)| *provider)
        .collect()
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
        // Both outcomes are exit 2 -- neither can support a verdict -- but they
        // are opposite conditions for whoever runs this. An absent file means no
        // vault credentials are configured, which is the ordinary state on a host
        // that uses none. An unreadable one means credentials may well be
        // configured and this check cannot see them, which is a fault to fix
        // before the result means anything. Reporting both as "no handle file"
        // sends the second case away as normal.
        //
        // Exit 2 rather than 0 in either case: a clean pass would claim every
        // configured lane is serving, which is vacuously true and
        // indistinguishable from a real one.
        //
        // *** THIS ARM GOES BLIND AT THE SCOPED-GRANT CUTOVER. ***
        //
        // The plan deletes this file: the module will enumerate credentials from
        // `credential.list_scoped` instead, so `ck auth login` alone makes an
        // account appear. On that day the file is absent while ten credentials
        // are configured, and the NotFound arm below reports the ordinary state
        // of a host that uses no vault credentials -- exit 2, "nothing to check",
        // which is the quietest failure available. Not an alarm, not a finding:
        // a checker that has silently stopped verifying the lanes it exists for.
        //
        // Same shape as the stale DUAL_LANE exemption fixed earlier today, and
        // the same shape CKCRED hit in their own operator probe, which demanded a
        // capability handle before it would exercise the handle-FREE path. A tool
        // whose input is the thing being removed encodes the assumption it exists
        // to remove.
        //
        // So the cutover has a step beyond deleting the file: repoint this reader
        // at `list_scoped` in the SAME change. Landing the deletion first leaves a
        // window where nothing verifies the vault lanes and nothing says so.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("no credential handle file: no vault credentials configured here");
            std::process::exit(2);
        }
        Err(error) => {
            eprintln!(
                "credential handle file exists but could not be read ({error}); \
                 configured lanes cannot be checked until that is fixed"
            );
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

    // The module refuses this file outright for reasons a plain read cannot see:
    // a symbolic link, group- or world-accessible permissions, anything that is
    // not a regular file. On refusal it serves ZERO vault handles and reaps every
    // vault lane -- so a checker that parsed the same bytes successfully would
    // report those lanes as configured and healthy while the module served none
    // of them, which is a clean pass asserting the opposite of the truth.
    //
    // Asked through the module's own loader rather than by re-implementing its
    // refusals here, because a second copy of that logic would drift from the
    // first and the drift would be invisible in exactly this direction.
    let loader = quota_core::vault_handles::VaultHandleLoader::new(Some(handles_path.clone()));
    let module_sees_any = [
        loader.codex_handles(),
        loader.anthropic_handles(),
        loader.grok_handles(),
        loader.gemini_handles(),
        loader.antigravity_handles(),
        loader.kimi_for_coding_handles(),
        loader.amp_handles(),
        loader.cursor_handles(),
        loader.qwen_cloud_handles(),
        loader.qoder_handles(),
        loader.factory_handles(),
        loader.mimo_handles(),
        loader.ollama_handles(),
        loader.opencode_handles(),
        loader.opencodego_handles(),
        loader.deepseek_handles(),
        loader.synthetic_handles(),
        loader.openrouter_handles(),
    ]
    .iter()
    .any(|result| result.as_ref().is_ok_and(|handles| !handles.is_empty()));

    let mut expected: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let mut unmapped: Vec<String> = Vec::new();
    if let Some(handles) = parsed.get("handles").and_then(|h| h.as_object()) {
        if !handles.is_empty() && !module_sees_any {
            eprintln!(
                "the handle file names {} credential(s) and the module accepts none of \
                 them: it is refusing the file itself, most likely a symbolic link or \
                 group/world-accessible permissions. Every vault lane is reaped, so no \
                 lane can be checked.",
                handles.len()
            );
            std::process::exit(2);
        }
        for key in handles.keys() {
            let providers = providers_for_handle(key);
            if providers.is_empty() {
                // An unmapped key is reported rather than ignored: it means this
                // host is configured for a credential no provider here consumes,
                // which is a real configuration finding and would otherwise be
                // invisible.
                unmapped.push(key.clone());
            } else {
                for provider in providers {
                    expected.entry(provider).or_default().push(key.clone());
                }
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

    // The same client the module runs, so "which lanes does this provider
    // enumerate" is answered by the code under test rather than by a copy of its
    // conclusions. Enumeration never calls the source -- it only asks whether one
    // is wired -- so this costs no vault round-trip.
    let vault: std::sync::Arc<dyn quota_core::credential_source::CredentialSource> =
        std::sync::Arc::new(quota_module::vault_client::VaultClient::new(path.clone()));
    let registry = quota_core::Registry::with_defaults(
        quota_core::config::QuotaConfig::default(),
        Some(vault),
    );
    let stale_exempt = stale_exemptions(&registry);

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

    // A STALE EXEMPTION IS A FINDING, NOT A NOTE. It suppresses a check that
    // would now pass, so the lane it covers has quietly stopped being verified --
    // and because under-reporting coverage raises no alarm, nothing else will ever
    // surface it. Printed with the remedy so the next reader is not left deciding
    // which of the exemptions is still load-bearing.
    if !stale_exempt.is_empty() {
        println!(
            "  findings: {} stale exemption(s) in DUAL_LANE",
            stale_exempt.len()
        );
        for provider in &stale_exempt {
            println!(
                "    {provider}: enumerates only vault handles now, so its exemption suppresses \
                 a check that would pass; delete its DUAL_LANE entry"
            );
        }
    }

    if dark.is_empty() {
        if unmapped.is_empty() && incomplete.is_empty() && stale_exempt.is_empty() {
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
