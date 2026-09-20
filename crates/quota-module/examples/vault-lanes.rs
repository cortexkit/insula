//! Check that every credential in the module's installed scoped snapshot reaches
//! a serving vault lane.
//!
//! The snapshot is read from the module's health report through the daemon. This
//! process deliberately does not enumerate the vault: a standalone client has a
//! direct principal, while `credential.list_scoped` is authorised only on the
//! supervised module's `reserved:insula` route. It also runs no refresher of its
//! own, so its configured set cannot drift from the snapshot the deployed module
//! actually installed.
//!
//! Four counts describe different populations and are always printed separately:
//! `enumerated` credential rows, routed `(credential_id, provider)` pairs,
//! distinct expected providers after precedence suppression, and providers the
//! checker actually examined.
//!
//! Exit 0 means checked and clean, 1 means checked with findings, and 2 means no
//! defensible verdict was possible.
//!
//! Run against the deployed module through the daemon:
//! `cargo run -p quota-module --example vault-lanes`

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use quota_core::model::ProviderUsage;
use serde_json::Value;
use subc_daemon::Frame;
use subc_protocol::{BindIdentity, FrameType, RouteTarget};
use subc_transport::{authenticate_client, connection_file};
use tokio::net::TcpStream;

#[path = "../tests/common/mod.rs"]
mod common;

/// Providers where a non-vault lane takes precedence over a vault lane.
///
/// These remain visible in the routed-pair count but are omitted from the
/// expected-provider count because the published row can truthfully carry the
/// winning non-vault source. The checker still verifies that the premise is
/// visible on the wire; an exemption with no non-vault row is a finding.
const DUAL_LANE: &[(&str, &str)] = &[(
    "antigravity",
    "a local editor process is probed first and wins when both are healthy",
)];

/// Enumerated ids intentionally unsupported by this module.
///
/// Exact ids only. A prefix exemption would hide a new routing-table defect for
/// another credential in the same vendor family.
const ENUMERATED_UNSUPPORTED: &[(&str, &str)] = &[(
    "apikey:openai",
    "a platform API key cannot feed the ChatGPT-subscription Codex lane",
)];

fn connection_file_path() -> PathBuf {
    if let Ok(path) = std::env::var("SUBC_CONNECTION_FILE") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local/share/cortexkit/run/subc-connection.json")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InstalledSnapshot {
    credential_ids: Vec<String>,
    mapping_warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SnapshotReadback {
    Granted(InstalledSnapshot),
    NotGranted,
}

#[derive(Clone, Debug, Default)]
struct UsageReadback {
    entries: Vec<ProviderUsage>,
    complete_providers: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    enumerated: usize,
    routed: usize,
    expected: usize,
    checked: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckReport {
    exit_code: i32,
    counts: Option<Counts>,
    lines: Vec<String>,
}

impl CheckReport {
    fn could_not_check(message: impl Into<String>) -> Self {
        Self {
            exit_code: 2,
            counts: None,
            lines: vec![format!("error: {}", message.into())],
        }
    }

    fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }
}

fn providers_for_id<'a>(credential_id: &str, families: &'a [(&str, &str)]) -> Vec<&'a str> {
    families
        .iter()
        .filter(|(prefix, _)| {
            quota_core::vault_handles::handle_id_names_family(credential_id, prefix)
        })
        .map(|(_, provider)| *provider)
        .collect()
}

fn find_module_metrics<'a>(value: &'a Value, module_id: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            let is_target = map
                .get("module_id")
                .or_else(|| map.get("id"))
                .and_then(Value::as_str)
                == Some(module_id);
            if is_target {
                if let Some(metrics) = map.get("health").and_then(|health| health.get("metrics")) {
                    return Some(metrics);
                }
                if let Some(metrics) = map.get("metrics") {
                    return Some(metrics);
                }
            }
            map.values()
                .find_map(|child| find_module_metrics(child, module_id))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| find_module_metrics(child, module_id)),
        _ => None,
    }
}

fn parse_snapshot_metrics(metrics: &Value) -> Result<SnapshotReadback, String> {
    let metrics = metrics
        .as_object()
        .ok_or_else(|| "module health metrics are not a JSON object".to_string())?;

    match metrics.get("vaultEnumerationFailure") {
        Some(Value::String(failure)) if failure == "principal is not granted" => {
            return Ok(SnapshotReadback::NotGranted);
        }
        Some(Value::String(failure)) => {
            let age = metrics
                .get("retainedVaultSnapshotAgeSecs")
                .filter(|value| !value.is_null())
                .map(|value| format!("; retained snapshot age {value}s"))
                .unwrap_or_default();
            return Err(format!(
                "scoped credential enumeration failed: {failure}{age}"
            ));
        }
        Some(Value::Null) => {}
        Some(other) => {
            return Err(format!(
                "vaultEnumerationFailure has unexpected shape: {other}"
            ));
        }
        None => return Err("health metrics omit vaultEnumerationFailure".to_string()),
    }

    let ids = metrics
        .get("scopedCredentialIds")
        .and_then(Value::as_array)
        .ok_or_else(|| "health metrics omit scopedCredentialIds[]".to_string())?;
    let credential_ids = ids
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| "scopedCredentialIds contains a non-string value".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mapping_warning = match metrics.get("vaultMappingWarning") {
        Some(Value::String(warning)) => Some(warning.clone()),
        Some(Value::Null) | None => None,
        Some(other) => return Err(format!("vaultMappingWarning has unexpected shape: {other}")),
    };

    Ok(SnapshotReadback::Granted(InstalledSnapshot {
        credential_ids,
        mapping_warning,
    }))
}

fn counts_lines(counts: Counts) -> Vec<String> {
    vec![
        format!("  enumerated rows: {}", counts.enumerated),
        format!("  routed id-provider pairs: {}", counts.routed),
        format!("  expected providers: {}", counts.expected),
        format!("  checked providers: {}", counts.checked),
    ]
}

fn evaluate(
    readback: Result<SnapshotReadback, String>,
    usage: Result<UsageReadback, String>,
    families: &[(&str, &str)],
    dual_lane: &[(&str, &str)],
    unsupported: &[(&str, &str)],
) -> CheckReport {
    let installed = match readback {
        Err(error) => return CheckReport::could_not_check(error),
        Ok(SnapshotReadback::NotGranted) => {
            return CheckReport::could_not_check("principal is not granted");
        }
        Ok(SnapshotReadback::Granted(installed)) => installed,
    };

    let cursor_oauth_present = installed
        .credential_ids
        .iter()
        .any(|id| quota_core::vault_handles::handle_id_names_family(id, "oauth:cursor"));
    let mut identityless_families: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (prefix, _) in families {
        if prefix.starts_with("cookie:") || prefix.starts_with("apikey:") {
            identityless_families.entry(prefix).or_default();
        }
    }
    for credential_id in &installed.credential_ids {
        for (prefix, ids) in &mut identityless_families {
            if quota_core::vault_handles::handle_id_names_family(credential_id, prefix) {
                ids.push(credential_id);
            }
        }
    }
    identityless_families.retain(|_, ids| ids.len() > 1);
    let refused_ids: BTreeSet<&str> = identityless_families
        .values()
        .flat_map(|ids| ids.iter().copied())
        .collect();

    let mut expected: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut routed_providers = BTreeSet::new();
    let mut unmapped = Vec::new();
    let mut deliberately_unsupported = Vec::new();
    let mut routed = 0;

    for credential_id in &installed.credential_ids {
        let providers = providers_for_id(credential_id, families);
        if providers.is_empty() {
            if let Some((_, reason)) = unsupported
                .iter()
                .find(|(unsupported_id, _)| credential_id == unsupported_id)
            {
                deliberately_unsupported.push((credential_id.clone(), *reason));
            } else {
                unmapped.push(credential_id.clone());
            }
            continue;
        }

        routed += providers.len();
        for provider in providers {
            routed_providers.insert(provider);
            let precedence_suppressed = cursor_oauth_present
                && quota_core::vault_handles::handle_id_names_family(
                    credential_id,
                    "cookie:cursor.com",
                );
            if !dual_lane.iter().any(|(name, _)| *name == provider)
                && !precedence_suppressed
                && !refused_ids.contains(credential_id.as_str())
            {
                expected
                    .entry(provider)
                    .or_default()
                    .push(credential_id.clone());
            }
        }
    }

    let mut counts = Counts {
        enumerated: installed.credential_ids.len(),
        routed,
        expected: expected.len(),
        checked: 0,
    };

    if counts.enumerated == 0 {
        let mut lines = counts_lines(counts);
        lines.push("  findings: installed scoped snapshot contains zero rows".to_string());
        return CheckReport {
            exit_code: 1,
            counts: Some(counts),
            lines,
        };
    }

    let usage = match usage {
        Ok(usage) => usage,
        Err(error) => return CheckReport::could_not_check(error),
    };

    let serving_vault: BTreeSet<&str> = usage
        .entries
        .iter()
        .filter(|entry| entry.source.as_deref() == Some("vault") && entry.usage.is_some())
        .map(|entry| entry.provider.as_str())
        .collect();

    let mut dark = Vec::new();
    let mut incomplete = Vec::new();
    for (provider, credential_ids) in &expected {
        counts.checked += 1;
        if !serving_vault.contains(provider) {
            dark.push((*provider, credential_ids.clone()));
            continue;
        }
        if usage.complete_providers.contains(*provider) {
            continue;
        }
        let published = usage
            .entries
            .iter()
            .filter(|entry| entry.provider == *provider)
            .count();
        if published < credential_ids.len() {
            incomplete.push((*provider, published, credential_ids.len()));
        }
    }

    let stale_exemptions: Vec<(&str, &str)> = dual_lane
        .iter()
        .copied()
        .filter(|(provider, _)| routed_providers.contains(provider))
        .filter(|(provider, _)| {
            !usage.entries.iter().any(|entry| {
                entry.provider == *provider
                    && entry
                        .source
                        .as_deref()
                        .is_some_and(|source| source != "vault")
            })
        })
        .collect();

    let mut lines = counts_lines(counts);
    for (credential_id, reason) in &deliberately_unsupported {
        lines.push(format!(
            "  unsupported: {credential_id}: {reason}; no lane is expected"
        ));
    }
    for credential_id in &unmapped {
        lines.push(format!(
            "  finding: {credential_id}: enumerated but no credential family maps it"
        ));
    }
    for (family, credential_ids) in &identityless_families {
        lines.push(format!(
            "  finding: {family}: multiple identity-less credential rows are refused: {}",
            credential_ids.join(", ")
        ));
    }
    if let Some(warning) = &installed.mapping_warning {
        lines.push(format!("  snapshot mapping warning: {warning}"));
    }
    for (provider, credential_ids) in &dark {
        lines.push(format!(
            "  finding: {provider}: DARK with {} credential row(s): {}",
            credential_ids.len(),
            credential_ids.join(", ")
        ));
    }
    for (provider, published, configured) in &incomplete {
        lines.push(format!(
            "  finding: {provider}: published {published} entr(ies) for {configured} credential row(s)"
        ));
    }
    for (provider, reason) in &stale_exemptions {
        lines.push(format!(
            "  finding: stale DUAL_LANE exemption for {provider}: no non-vault row proves `{reason}`"
        ));
    }

    let findings = unmapped.len()
        + identityless_families.len()
        + dark.len()
        + incomplete.len()
        + stale_exemptions.len();
    if counts.checked == 0 && findings == 0 {
        lines.push(
            "error: no provider was actually checked; a clean result would be vacuous".into(),
        );
        return CheckReport {
            exit_code: 2,
            counts: Some(counts),
            lines,
        };
    }

    if findings == 0 {
        lines.push("  findings: none".to_string());
        CheckReport {
            exit_code: 0,
            counts: Some(counts),
            lines,
        }
    } else {
        lines.push(format!("  findings: {findings}"));
        CheckReport {
            exit_code: 1,
            counts: Some(counts),
            lines,
        }
    }
}

async fn connect_to_daemon(path: &Path) -> Result<TcpStream, String> {
    let info = connection_file::read(path)
        .map_err(|error| format!("could not read daemon connection file: {error}"))?;
    let endpoint = info
        .endpoints
        .first()
        .ok_or_else(|| "daemon connection file contains no endpoints".to_string())?;
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(|error| format!("could not connect to daemon: {error}"))?;
    authenticate_client(&mut stream, &info, Duration::from_secs(2))
        .await
        .map_err(|error| format!("daemon authentication failed: {error}"))?;
    Ok(stream)
}

fn frame_error(context: &str, frame: &Frame) -> String {
    let detail = String::from_utf8_lossy(&frame.body);
    format!("{context} returned a daemon route error: {detail}")
}

async fn read_installed_snapshot(stream: &mut TcpStream) -> Result<SnapshotReadback, String> {
    let frame =
        common::control_rpc(stream, 1, serde_json::json!({ "op": "supervisor.health" })).await;
    if frame.header.ty == FrameType::Error {
        return Err(frame_error("supervisor.health", &frame));
    }
    if frame.header.ty != FrameType::Response {
        return Err(format!(
            "supervisor.health returned unexpected frame {:?}",
            frame.header.ty
        ));
    }
    let body: Value = serde_json::from_slice(&frame.body)
        .map_err(|error| format!("supervisor.health reply was not JSON: {error}"))?;
    let metrics = find_module_metrics(&body, common::MODULE_ID).ok_or_else(|| {
        format!(
            "supervisor.health carried no metrics for {}",
            common::MODULE_ID
        )
    })?;
    parse_snapshot_metrics(metrics)
}

async fn open_usage_route(stream: &mut TcpStream) -> Result<common::Route, String> {
    let project_root = std::env::current_dir()
        .map_err(|error| format!("could not resolve checker working directory: {error}"))?;
    let identity = BindIdentity::new(project_root, "vault-lanes", "readback-check");
    let target = RouteTarget::ManagementSurface {
        module_id: common::MODULE_ID.to_string(),
    };
    let frame = common::control_rpc(
        stream,
        2,
        serde_json::json!({
            "op": "route.open",
            "target": target,
            "identity": identity,
        }),
    )
    .await;
    if frame.header.ty == FrameType::Error {
        return Err(frame_error("route.open", &frame));
    }
    if frame.header.ty != FrameType::Response {
        return Err(format!(
            "route.open returned unexpected frame {:?}",
            frame.header.ty
        ));
    }
    let body: Value = serde_json::from_slice(&frame.body)
        .map_err(|error| format!("route.open reply was not JSON: {error}"))?;
    let channel = body
        .get("route_channel")
        .and_then(Value::as_u64)
        .ok_or_else(|| "route.open reply omitted route_channel".to_string())?;
    let epoch = body
        .get("route_epoch")
        .and_then(Value::as_u64)
        .ok_or_else(|| "route.open reply omitted route_epoch".to_string())?;
    Ok(common::Route {
        channel: channel as u16,
        epoch: epoch as u32,
    })
}

async fn read_usage(stream: &mut TcpStream, route: common::Route) -> Result<UsageReadback, String> {
    let frame = common::raw_route_frame(
        stream,
        route,
        3,
        serde_json::json!({ "method": "usage.get", "params": {} }),
    )
    .await;
    if frame.header.ty == FrameType::Error {
        return Err(frame_error("usage.get", &frame));
    }
    if frame.header.ty != FrameType::Response {
        return Err(format!(
            "usage.get returned unexpected frame {:?}",
            frame.header.ty
        ));
    }
    let body: Value = serde_json::from_slice(&frame.body)
        .map_err(|error| format!("usage.get reply was not JSON: {error}"))?;
    let entries = serde_json::from_value::<Vec<ProviderUsage>>(
        body.get("result")
            .cloned()
            .ok_or_else(|| "usage.get reply omitted result[]".to_string())?,
    )
    .map_err(|error| format!("usage.get result did not decode: {error}"))?;
    let complete_providers = body
        .get("completeProviders")
        .cloned()
        .map(serde_json::from_value::<Vec<String>>)
        .transpose()
        .map_err(|error| format!("completeProviders did not decode: {error}"))?
        .unwrap_or_default()
        .into_iter()
        .collect();
    Ok(UsageReadback {
        entries,
        complete_providers,
    })
}

#[tokio::main]
async fn main() {
    let path = connection_file_path();
    if !path.exists() {
        eprintln!("error: no daemon connection file at {}", path.display());
        std::process::exit(2);
    }
    let mut stream = match connect_to_daemon(&path).await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    };

    let readback = read_installed_snapshot(&mut stream).await;
    let usage = match &readback {
        Ok(SnapshotReadback::Granted(snapshot)) if !snapshot.credential_ids.is_empty() => {
            match open_usage_route(&mut stream).await {
                Ok(route) => read_usage(&mut stream, route).await,
                Err(error) => Err(error),
            }
        }
        _ => Ok(UsageReadback::default()),
    };

    let report = evaluate(
        readback,
        usage,
        quota_core::vault_handles::CREDENTIAL_FAMILIES,
        DUAL_LANE,
        ENUMERATED_UNSUPPORTED,
    );
    report.print();
    if report.exit_code != 0 {
        std::process::exit(report.exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quota_core::model::Usage;

    fn healthy(provider: &str, source: &str) -> ProviderUsage {
        ProviderUsage::healthy(provider, None, source, Usage::default())
    }

    fn granted(ids: &[&str]) -> Result<SnapshotReadback, String> {
        Ok(SnapshotReadback::Granted(InstalledSnapshot {
            credential_ids: ids.iter().map(|id| (*id).to_string()).collect(),
            mapping_warning: None,
        }))
    }

    fn usage(entries: Vec<ProviderUsage>) -> Result<UsageReadback, String> {
        Ok(UsageReadback {
            entries,
            complete_providers: BTreeSet::new(),
        })
    }

    #[test]
    fn installed_health_snapshot_drives_the_checker_without_a_local_refresher() {
        let metrics = serde_json::json!({
            "scopedCredentialIds": ["oauth:anthropic", "oauth:anthropic:second"],
            "vaultEnumerationFailure": null,
            "retainedVaultSnapshotAgeSecs": null,
            "vaultMappingWarning": null,
        });
        let readback = parse_snapshot_metrics(&metrics);
        let report = evaluate(
            readback,
            usage(vec![healthy("claude", "vault"), healthy("claude", "vault")]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );

        assert_eq!(report.exit_code, 0, "{report:?}");
        assert_eq!(report.counts.unwrap().enumerated, 2);
    }

    #[test]
    fn all_four_populations_are_counted_separately() {
        let report = evaluate(
            granted(&[
                "cookie:opencode.ai",
                "oauth:anthropic",
                "oauth:anthropic:second",
                "oauth:anthropic:third",
                "antigravity:google",
            ]),
            usage(vec![
                healthy("opencode", "vault"),
                healthy("opencodego", "vault"),
                healthy("claude", "vault"),
                healthy("claude", "vault"),
                healthy("claude", "vault"),
                healthy("antigravity", "local"),
            ]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            DUAL_LANE,
            ENUMERATED_UNSUPPORTED,
        );
        let counts = report.counts.expect("a checked report has counts");

        assert_eq!(counts.enumerated, 5);
        assert_eq!(counts.routed, 6);
        assert_eq!(counts.expected, 3);
        assert_eq!(counts.checked, 3);
        assert_eq!(report.exit_code, 0, "{report:?}");
    }

    #[test]
    fn readback_and_route_failures_exit_two_and_print_the_error() {
        let readback_failure = evaluate(
            parse_snapshot_metrics(&serde_json::json!({
                "scopedCredentialIds": ["retained"],
                "vaultEnumerationFailure": "health readback failed",
                "retainedVaultSnapshotAgeSecs": 17,
            })),
            usage(Vec::new()),
            &[],
            &[],
            &[],
        );
        assert_eq!(readback_failure.exit_code, 2);
        assert!(readback_failure
            .lines
            .join("\n")
            .contains("health readback failed; retained snapshot age 17s"));

        let route_failure = evaluate(
            granted(&["oauth:anthropic"]),
            Err("route.open returned unknown_module".into()),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );
        assert_eq!(route_failure.exit_code, 2);
        assert!(route_failure
            .lines
            .join("\n")
            .contains("route.open returned unknown_module"));
    }

    #[test]
    fn zero_grants_and_zero_rows_have_distinct_exit_contracts() {
        let not_granted = evaluate(
            parse_snapshot_metrics(&serde_json::json!({
                "scopedCredentialIds": ["retained"],
                "vaultEnumerationFailure": "principal is not granted",
                "retainedVaultSnapshotAgeSecs": 17,
            })),
            usage(Vec::new()),
            &[],
            &[],
            &[],
        );
        assert_eq!(not_granted.exit_code, 2);
        assert!(not_granted
            .lines
            .join("\n")
            .contains("principal is not granted"));

        let empty = evaluate(granted(&[]), usage(Vec::new()), &[], &[], &[]);
        assert_eq!(empty.exit_code, 1);
        assert_eq!(empty.counts.unwrap().enumerated, 0);
    }

    #[test]
    fn unknown_ids_fail_but_the_exact_unsupported_id_is_reported_cleanly() {
        let unknown = evaluate(
            granted(&["oauth:anthropic", "oauth:unknown-vendor"]),
            usage(vec![healthy("claude", "vault")]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );
        assert_eq!(unknown.exit_code, 1, "{unknown:?}");

        let unsupported = evaluate(
            Ok(SnapshotReadback::Granted(InstalledSnapshot {
                credential_ids: vec!["oauth:anthropic".into(), "apikey:openai".into()],
                mapping_warning: Some(
                    "ignored ids outside supported vault mapping [apikey:openai]".into(),
                ),
            })),
            usage(vec![healthy("claude", "vault")]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );
        assert_eq!(unsupported.exit_code, 0, "{unsupported:?}");

        // THE EXEMPTION IS BY EXACT ID, AND THIS IS WHAT PROVES IT.
        //
        // A SECOND unmapped id in the SAME vendor family as the exempted one. Widen
        // the match from equality to a `apikey:` prefix and this row is silently
        // excused with the first, so a genuine routing gap exits 0 under a confident
        // coverage count. Verified against the live vault before writing it: this
        // host holds `apikey:amazon-bedrock` and `apikey:apns-alfonso` beside
        // `apikey:openai`, so the widened match would hide real rows rather than a
        // hypothetical one.
        //
        // Without this case the fixture holds exactly one `apikey:` id -- the
        // exempted one -- so exact and prefix matching are indistinguishable and the
        // constant's own doc comment ("Exact ids only") has no defender.
        let sibling_in_the_same_family = evaluate(
            Ok(SnapshotReadback::Granted(InstalledSnapshot {
                credential_ids: vec![
                    "oauth:anthropic".into(),
                    "apikey:openai".into(),
                    "apikey:amazon-bedrock".into(),
                ],
                mapping_warning: Some(
                    "ignored ids outside supported vault mapping [apikey:amazon-bedrock, apikey:openai]"
                        .into(),
                ),
            })),
            usage(vec![healthy("claude", "vault")]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );
        assert_eq!(
            sibling_in_the_same_family.exit_code, 1,
            "an unmapped id sharing the exempted id's method segment is a routing \
             finding, not an exemption: {sibling_in_the_same_family:?}"
        );
        assert!(unsupported
            .lines
            .join("\n")
            .contains("unsupported: apikey:openai"));
    }

    #[test]
    fn a_run_that_examines_no_provider_exits_two() {
        let report = evaluate(
            granted(&["antigravity:google"]),
            usage(vec![healthy("antigravity", "local")]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            DUAL_LANE,
            ENUMERATED_UNSUPPORTED,
        );

        assert_eq!(report.exit_code, 2, "{report:?}");
        assert_eq!(report.counts.unwrap().checked, 0);
    }

    #[test]
    fn a_dark_lane_and_a_stale_exemption_are_each_findings() {
        let dark = evaluate(
            granted(&["oauth:anthropic"]),
            usage(Vec::new()),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );
        assert_eq!(dark.exit_code, 1, "{dark:?}");
        assert!(dark.lines.join("\n").contains("DARK"));

        let stale = evaluate(
            granted(&["oauth:anthropic", "antigravity:google"]),
            usage(vec![
                healthy("claude", "vault"),
                healthy("antigravity", "vault"),
            ]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            DUAL_LANE,
            ENUMERATED_UNSUPPORTED,
        );
        assert_eq!(stale.exit_code, 1, "{stale:?}");
        assert!(stale.lines.join("\n").contains("stale DUAL_LANE exemption"));
    }

    #[test]
    fn precedence_and_identityless_refusal_follow_the_installed_rows() {
        let cursor = evaluate(
            granted(&["oauth:cursor", "cookie:cursor.com"]),
            usage(vec![healthy("cursor", "vault")]),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );
        let cursor_counts = cursor.counts.expect("cursor run has counts");
        assert_eq!(cursor_counts.enumerated, 2);
        assert_eq!(cursor_counts.routed, 2);
        assert_eq!(cursor_counts.expected, 1);
        assert_eq!(cursor.exit_code, 0, "{cursor:?}");

        let duplicate_cookie = evaluate(
            granted(&["cookie:opencode.ai", "cookie:opencode.ai:second"]),
            usage(Vec::new()),
            quota_core::vault_handles::CREDENTIAL_FAMILIES,
            &[],
            ENUMERATED_UNSUPPORTED,
        );
        assert_eq!(duplicate_cookie.exit_code, 1, "{duplicate_cookie:?}");
        assert!(duplicate_cookie
            .lines
            .join("\n")
            .contains("multiple identity-less credential rows are refused"));
    }

    #[test]
    fn removing_a_required_family_mapping_is_not_excused_as_unsupported() {
        let families_without_anthropic: Vec<_> = quota_core::vault_handles::CREDENTIAL_FAMILIES
            .iter()
            .copied()
            .filter(|(prefix, _)| *prefix != "oauth:anthropic")
            .collect();
        let report = evaluate(
            granted(&["oauth:anthropic"]),
            usage(vec![healthy("claude", "vault")]),
            &families_without_anthropic,
            &[],
            ENUMERATED_UNSUPPORTED,
        );

        assert_eq!(report.exit_code, 1, "{report:?}");
        assert!(report
            .lines
            .join("\n")
            .contains("no credential family maps it"));
    }
}
