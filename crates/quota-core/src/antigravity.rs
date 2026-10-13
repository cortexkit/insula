//! Antigravity usage — LOCAL PROBE of the running Antigravity editor.
//!
//! Unlike the OAuth providers, Antigravity's authoritative paid-tier usage is not a
//! cloud call: the Antigravity editor (its `language_server`, or the `agy` CLI) runs a
//! local server on a loopback port. The opencode antigravity-auth plugin publishes
//! remaining fractions, reset times and observation times for that paid quota in
//! its non-secret sidebar v2 JSON file. We prefer the live server, then a recent
//! sidebar reading; vault credentials without a known paid tier may use the cloud.
//!
//! Flow:
//!  1. `ps -ax -o pid=,command=` → find the Antigravity `language_server` (app/IDE) or
//!     `agy` CLI process; pull its `--csrf_token` from the command line (the CLI needs
//!     none).
//!  2. `lsof -nP -iTCP -sTCP:LISTEN -a -p <pid>` → its loopback listening port(s).
//!  3. POST `https://127.0.0.1:<port>/exa.language_server_pb.LanguageServerService/
//!     RetrieveUserQuotaSummary` with body `{"forceRefresh":true}`, headers
//!     `Content-Type: application/json` + `Connect-Protocol-Version: 1` +
//!     `X-Codeium-Csrf-Token: <token>` (omitted for the CLI).
//!  4. Parse the quota summary: groups → buckets, each with `remainingFraction`
//!     (0..1) and `resetTime`. The two pools (native Gemini models vs external
//!     Claude/GPT models) are independent meters: only the Gemini pool maps to
//!     the unnamed `primary`; every bucket (both pools) is surfaced as a named
//!     per-pool extra window.
//!
//! The local server uses a SELF-SIGNED cert on loopback, so this provider builds ONE
//! dedicated reqwest client with cert validation disabled — used EXCLUSIVELY for
//! 127.0.0.1 (validating a loopback self-signed cert is meaningless; the peer is the
//! user's own machine). Every request URL is guarded to be loopback before sending.
//!
//! PAID-TIER AUTHORITY REMAINS DESKTOP-COUPLED: a live editor is authoritative, and
//! the plugin cache extends that reading for at most one hour. Beyond that, withholding
//! is safer than replacing the paid pool with the cloud client's different standard
//! pool. Free-tier accounts use that cloud lane because it is their real pool.
//!
//! VERIFICATION: LIVE-verified — the real local-probe chain (discover `agy` →
//! loopback port → POST quota summary → parse) returns real windows on a machine
//! running the Antigravity CLI (Gemini + Claude/GPT weekly + 5-hour buckets with real
//! resets; see `tests/it/antigravity_live.rs`). The live wire revealed three details a
//! fixture alone would have missed, now matched: the CLI serves HTTP (not HTTPS) on
//! loopback (so [`probe`] tries both schemes), the summary is wrapped in a
//! `{"response": {...}}` envelope, and each bucket carries an explicit `window`
//! (`"5h"`/`"weekly"`). The parser is also unit-tested against that captured shape.
//! Wire format + field mapping ported from CodexBar
//! `Sources/CodexBarCore/Providers/Antigravity/AntigravityStatusProbe.swift`
//! (ps :1013-1018, process match :1104-1156, csrf :1130-1184, lsof :1191-1232, paths
//! :771-775, request/headers :1467-1505/:1651-1660, representative :231-244, bucket
//! kinds :362-371) + `AntigravityQuotaSummaryParser.swift:96-173`.
//!
//! Cadence precedence follows upstream at v0.70.0 (`quotaCadenceCandidates` in
//! the same `AntigravityStatusProbe.swift`; fixtures in
//! `Tests/CodexBarTests/AntigravityQuotaSourceParityTests.swift`): a bucket's
//! explicit, non-empty `window` alone decides its cadence, and `bucketId` and
//! `displayName` are read only when `window` is absent or blank. An unrecognised
//! `window` gives no cadence. On the live wire `window` and the id agree, so
//! this changes no published window today.

use std::{collections::HashSet, sync::Arc, time::Duration};

use crate::credential_source::{CredentialSource, VaultGetError};
use crate::provider::AccountObservation;
use crate::subprocess::{BoundedCommand, Gate, SubprocessError};
use crate::vault_handles::VaultHandleLoader;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ExtraWindow, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "antigravity";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// The cloud Code Assist quota SUMMARY endpoint, used when no local process is
/// running.
///
/// This returns merged pools carrying explicit per-cadence windows -- the same
/// `gemini-5h` / `gemini-weekly` / `3p-5h` / `3p-weekly` bucket ids the local
/// probe returns, so [`parse_quota_summary`] handles both and the two lanes
/// cannot describe one account differently.
const REMOTE_QUOTA_SUMMARY_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
const LOAD_CODE_ASSIST_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";

/// The per-model fallback, used only when the summary endpoint refuses.
///
/// IT IS A FALLBACK, NOT AN EQUIVALENT, and this module treated it as the
/// primary for weeks. It answers with one bucket per model carrying a single
/// implicit window, so every cadence collapses into one number: on this host it
/// published the Gemini pool's 5h figure of 0.09% used while the same account's
/// weekly window sat at 16.63%. A consumer reading that sees roughly sixteen
/// points of headroom that does not exist, and nothing on the wire says a window
/// was dropped -- `windowMinutes` is simply null, which reads as "cadence
/// unknown" rather than "a second window was discarded".
///
/// Both reference implementations order these the same way and neither treats
/// them as interchangeable: CodexBar logs "Falling back to retrieveUserQuota"
/// only on permissionDenied, and pane comments its summary call "Authoritative
/// endpoint first: merged pools + weekly windows".
const REMOTE_QUOTA_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
/// The Hub user-agent version is pinned to CodexBar 2.9.1; change it only with a version verified against CodexBar.
const HUB_USER_AGENT_VERSION: &str = "2.9.1";

/// Map Rust target identifiers to the spellings used by the Hub user agent.
/// Like CodexBar, unlisted architectures map to `amd64`; `windows` is our explicit
/// platform extension because CodexBar does not build Windows targets.
fn hub_target(platform: &str, architecture: &str) -> (&'static str, &'static str) {
    let platform = match platform {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => "darwin",
    };
    let architecture = match architecture {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => "amd64",
    };
    (platform, architecture)
}

fn hub_user_agent_for(platform: &str, architecture: &str) -> String {
    let (platform, architecture) = hub_target(platform, architecture);
    format!("antigravity/hub/{HUB_USER_AGENT_VERSION} {platform}/{architecture}")
}

/// Identifies Cloud Code requests as the Antigravity Hub client.
fn remote_user_agent() -> String {
    hub_user_agent_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn remote_user_agent_header() -> Header {
    Header::new("User-Agent", remote_user_agent())
}

/// Installation evidence only: never open the plugin's credential store.
const ACCOUNTS_FILE: &str = ".config/opencode/antigravity-accounts.json";

/// What the plugin leaves beside [`ACCOUNTS_FILE`] once its 3.0 migration has
/// moved the accounts into a generation store. Its presence is installation
/// evidence even when no sidebar snapshot has been published yet.
const MIGRATED_STORE_POINTER: &str =
    ".config/opencode/antigravity-accounts.json.store.pointer.json";
const SIDEBAR_STATE_ENV: &str = "ANTIGRAVITY_AUTH_SIDEBAR_STATE_FILE";
const SIDEBAR_STATE_RELATIVE: &str = "cortexkit/antigravity-auth/sidebar-state.json";

/// A cached pool older than this is more than 20% of the five-hour window's
/// period old, so it has stopped describing the window it claims to describe.
/// The weekly window would tolerate longer, but one cache entry carries both
/// cadences and the tighter window governs.
///
/// CHANGING THIS MOVES A FACT RECORDED IN ANOTHER REPOSITORY. A consumer store
/// (astrocyte, `window_sample`) documents at its insert site that a published
/// reading on this lane is bounded at 3,600s and that past the bound the row is
/// ABSENT rather than older — which is what makes their observed lag a sample
/// from a bounded distribution rather than evidence about a tail. Raising this
/// silently makes that comment wrong, and nothing in either repository connects
/// the two; tell them in the same change.
///
/// Calibration, such as it is: one host, one paid account, one afternoon
/// (2026-09-13) measured a typical lag of ~1,546s against this 3,600s bound —
/// the first evidence that the ordinary case sits well inside a number chosen
/// from window-period reasoning alone. Not a distribution.
const CACHED_QUOTA_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// The plugin handle retains `source: oauth`: it identifies an opencode
/// Antigravity login, even though the plugin owns all token access and polling.
const PLUGIN_SOURCE: &str = "oauth";

type EnvLookup = Arc<dyn Fn(&str) -> Option<std::ffi::OsString> + Send + Sync>;

#[cfg(not(test))]
fn default_sidebar_env() -> EnvLookup {
    Arc::new(|key| std::env::var_os(key).filter(|value| !value.is_empty()))
}

#[cfg(test)]
fn default_sidebar_env() -> EnvLookup {
    // Registry tests construct production providers too. Give each one an absent
    // home and no process environment so neither quota nor evidence is read from
    // the developer's plugin installation.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let home = std::env::temp_dir().join(format!(
        "insula-absent-sidebar-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    Arc::new(move |key| match key {
        "HOME" | "USERPROFILE" => Some(home.clone().into_os_string()),
        _ => None,
    })
}
/// Email, enabled status, captured tier and quota projected by opencode's
/// antigravity-auth plugin. Its `docs/sidebar-contract.md` defines the v2 file;
/// credentials and project identifiers are deliberately excluded.
#[derive(Debug, Clone, Deserialize)]
pub struct StoredAccount {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Paid accounts spend a different pool from the cloud credential's standard
    /// quota; only positive paid-tier metadata justifies withholding cloud usage.
    #[serde(default)]
    pub tier: Option<SidebarTier>,
    #[serde(default)]
    pub quota: CachedQuota,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SidebarTier {
    #[serde(default, rename = "paidId")]
    pub paid_id: Option<String>,
}

/// The partial map of model groups, not a credential or account store.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CachedQuota {
    #[serde(default)]
    pub gemini: Option<CachedQuotaPool>,
    #[serde(default, rename = "non-gemini")]
    pub non_gemini: Option<CachedQuotaPool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CachedQuotaPool {
    /// Quota observation time, not the top-level file publication `checkedAt`.
    /// Rewriting the file must not make an old quota reading appear fresh.
    #[serde(default, rename = "checkedAt")]
    pub checked_at: Option<i64>,
    #[serde(default)]
    pub windows: Vec<CachedQuotaWindow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CachedQuotaWindow {
    #[serde(default)]
    pub window: String,
    #[serde(default, rename = "remainingFraction")]
    pub remaining_fraction: Option<f64>,
    /// Rounded presentation metadata; never substitutes for the source fraction.
    #[serde(default, rename = "remainingPercent")]
    pub remaining_percent: Option<f64>,
    #[serde(default, rename = "resetAt")]
    pub reset_at: Option<i64>,
}

/// Find the sidebar's email identity without using a positional account key.
fn stored_account_for_email<'a>(
    accounts: &'a [StoredAccount],
    email: Option<&str>,
) -> Option<&'a StoredAccount> {
    accounts
        .iter()
        .find(|account| emails_match(email, account.email.as_deref()))
}

/// Only positive paid-tier metadata withholds the cloud's standard-tier pool.
/// An absent tier is unknown, so a host with no plugin can still use its vault.
fn account_holds_paid_tier(account: Option<&StoredAccount>) -> bool {
    account
        .and_then(|account| account.tier.as_ref())
        .and_then(|tier| tier.paid_id.as_deref())
        .is_some_and(|tier| tier != "free-tier")
}

/// Withhold a paid account's cloud quota because it is a different, standard-tier
/// pool the user does not spend. An unmatched credential is also withheld when
/// any paid account is known: it could belong to that account. With no positive
/// paid-tier evidence, cloud usage remains available for free or unknown tiers.
fn cloud_lane_must_withhold(
    accounts: &[StoredAccount],
    plugin_account: Option<&StoredAccount>,
) -> bool {
    if account_holds_paid_tier(plugin_account) {
        return true;
    }
    plugin_account.is_none()
        && accounts
            .iter()
            .any(|account| account_holds_paid_tier(Some(account)))
}

/// Why a paid-tier account with neither a matching live editor nor a recent
/// sidebar reading publishes nothing.
///
/// Deliberately `LocalSourceUnavailable` rather than a degraded entry: it is
/// classified transient, so the last correct reading keeps serving while the
/// editor is closed. Publishing the cloud number instead would replace a true
/// figure with a confident wrong one -- a different pool, not a stale one -- and
/// a router cannot tell those apart.
fn paid_tier_usage_unavailable() -> FetchError {
    FetchError::LocalSourceUnavailable(
        "this account holds a paid Antigravity tier, but neither a matching local \
         editor session nor a recent plugin sidebar reading is available: the cloud credential \
         is entitled to the standard tier and would describe a pool this account \
         does not use"
            .to_string(),
    )
}

#[derive(Debug, Deserialize)]
struct SidebarFile {
    version: Value,
    #[serde(default)]
    accounts: Value,
}

fn sidebar_path(lookup: &EnvLookup) -> Result<std::path::PathBuf, String> {
    if let Some(path) = lookup(SIDEBAR_STATE_ENV) {
        return Ok(path.into());
    }
    if let Some(state_home) = lookup("XDG_STATE_HOME") {
        return Ok(std::path::PathBuf::from(state_home).join(SIDEBAR_STATE_RELATIVE));
    }
    let home = env::home_dir_from(|key| lookup(key))
        .ok_or_else(|| "home directory unresolved, so the sidebar can't be located".to_string())?;
    Ok(home.join(".local/state").join(SIDEBAR_STATE_RELATIVE))
}

/// Failure means keep the last roster, not remove its accounts. Only absence
/// without installation evidence proves there is no plugin roster to enumerate.
fn read_sidebar(lookup: &EnvLookup) -> Result<Vec<StoredAccount>, String> {
    let path = sidebar_path(lookup)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let home = env::home_dir_from(|key| lookup(key)).ok_or_else(|| {
                "home directory unresolved, so plugin evidence can't be checked".to_string()
            })?;
            for evidence in [ACCOUNTS_FILE, MIGRATED_STORE_POINTER] {
                match home.join(evidence).try_exists() {
                    Ok(true) => {
                        return Err("Antigravity plugin is installed but its sidebar state is absent; update the plugin and let it publish quota".to_string());
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return Err(format!("plugin installation evidence unreadable: {error}"))
                    }
                }
            }
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("plugin sidebar state unreadable: {error}")),
    };
    let file = serde_json::from_str::<SidebarFile>(&text)
        .map_err(|error| format!("plugin sidebar state did not parse: {error}"))?;
    if file.version.as_u64() != Some(2) {
        return Err(format!(
            "unsupported Antigravity sidebar version {}; update the plugin to sidebar v2",
            file.version
        ));
    }
    // An unknown major may change the account shape. Check its version before
    // interpreting account fields so schema errors cannot mask the update remedy.
    let accounts: Vec<StoredAccount> = serde_json::from_value(file.accounts)
        .map_err(|error| format!("plugin sidebar accounts did not parse: {error}"))?;
    Ok(accounts
        .into_iter()
        .filter(|account| account.enabled != Some(false))
        .filter(|account| account_handle_name(account).is_some())
        .collect())
}

/// Positional ids and labels can move when the roster is reordered, so only a
/// genuine non-empty email may key identity fencing and failure history.
fn account_handle_name(account: &StoredAccount) -> Option<String> {
    account
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(|email| format!("plugin:{email}"))
}

const QUOTA_SUMMARY_PATH: &str =
    "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";

/// Query the editor's signed-in account email to guard against attributing a local
/// session to the wrong account when multiple Google accounts exist.
const USER_STATUS_PATH: &str = "/exa.language_server_pb.LanguageServerService/GetUserStatus";

#[derive(Deserialize)]
struct UserStatusEnvelope {
    #[serde(default)]
    response: Option<UserStatusBody>,
    #[serde(default, rename = "userStatus")]
    user_status: Option<UserStatus>,
}

#[derive(Deserialize)]
struct UserStatusBody {
    #[serde(default, rename = "userStatus")]
    user_status: Option<UserStatus>,
}

#[derive(Deserialize)]
struct UserStatus {
    #[serde(default)]
    email: Option<String>,
}

/// Extract the signed-in account email from a GetUserStatus JSON RPC response.
///
/// Returns None if the payload is malformed, the call was refused, or the email
/// field is absent or empty. An unattributable snapshot must never be attributed
/// to a named account.
fn parse_user_status_email(body: &str) -> Option<String> {
    let envelope: UserStatusEnvelope = serde_json::from_str(body).ok()?;
    let status = envelope
        .response
        .and_then(|r| r.user_status)
        .or(envelope.user_status)?;
    status
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
}

/// Case-insensitive trimmed email comparison for the Antigravity account guard.
///
/// Returns true only when both expected and local emails are present, non-empty,
/// and match case-insensitively after trimming. An absent or blank email never matches.
fn emails_match(expected: Option<&str>, local: Option<&str>) -> bool {
    let Some(expected) = expected.map(str::trim).filter(|e| !e.is_empty()) else {
        return false;
    };
    let Some(local) = local.map(str::trim).filter(|e| !e.is_empty()) else {
        return false;
    };
    expected.eq_ignore_ascii_case(local)
}

const SESSION_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;

// ---- process + port discovery ----------------------------------------------

/// A discovered Antigravity language-server process and how to talk to it.
#[derive(Clone, Debug)]
pub struct LocalServer {
    pub pid: i32,
    /// CSRF token for the request header. Empty for the CLI (which needs none).
    pub csrf_token: String,
}

/// A parsed local quota snapshot paired with the signed-in account email if available.
#[derive(Clone, Debug)]
struct LocalSnapshot {
    usage: Usage,
    email: Option<String>,
}

/// Whether a command line is an Antigravity language server or `agy` CLI, and the
/// CSRF token to use (empty string = CLI, needs none; None = app/IDE with no token,
/// which we cannot auth so skip).
fn classify_command(command: &str) -> Option<String> {
    if command_requires_csrf(command)? {
        // app/IDE language server: requires a --csrf_token; skip if absent.
        extract_csrf_token(command)
    } else {
        // CLI language server: no token required.
        Some(String::new())
    }
}

/// The same identification is used for argv and Windows' image-path prefilter.
fn command_requires_csrf(command: &str) -> Option<bool> {
    let lower = command.to_ascii_lowercase().replace('\\', "/");
    let is_language_server = lower.contains("language_server") || lower.contains("language-server");
    let is_antigravity = lower.contains("antigravity");
    let is_cli = lower.contains("antigravity-cli")
        || lower.contains("antigravity_cli")
        || lower.contains("/agy ")
        || lower.ends_with("/agy")
        || lower.contains("/agy.exe ")
        || lower.contains("/agy.exe\"")
        || lower.ends_with("/agy.exe");

    if is_language_server && is_antigravity {
        return Some(true);
    }
    if is_cli {
        return Some(false);
    }
    None
}

/// Extract the value of `--csrf_token` (`--csrf_token=VALUE` or `--csrf_token VALUE`).
fn extract_csrf_token(command: &str) -> Option<String> {
    let flag = "--csrf_token";
    let at = command.find(flag)? + flag.len();
    let rest = command[at..].trim_start_matches(['=', ' ', '\t']);
    let token: String = rest
        .chars()
        .take_while(|c| !c.is_ascii_whitespace())
        .collect();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Parse `ps -ax -o pid=,command=` output into candidate Antigravity servers.
#[cfg(any(target_os = "macos", test))]
fn parse_process_list(output: &str) -> Vec<LocalServer> {
    let mut out = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_start();
        let Some((pid_str, command)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_str.trim().parse::<i32>() else {
            continue;
        };
        if let Some(csrf_token) = classify_command(command.trim()) {
            out.push(LocalServer { pid, csrf_token });
        }
    }
    out
}

/// Parse loopback listening ports from `lsof -nP -iTCP -sTCP:LISTEN` output. Each
/// listening line ends with `:<port> (LISTEN)`.
#[cfg(any(target_os = "macos", test))]
fn parse_listening_ports(output: &str) -> Vec<u16> {
    let mut ports = Vec::new();
    for line in output.lines() {
        let Some(idx) = line.find("(LISTEN)") else {
            continue;
        };
        let head = line[..idx].trim_end();
        let Some(colon) = head.rfind(':') else {
            continue;
        };
        let port_str: String = head[colon + 1..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(port) = port_str.parse::<u16>() {
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    select_ports(0, ports.into_iter().map(|port| (0, port)))
}

/// Select one owner's ports consistently across lsof, procfs and TCP owner tables.
fn select_ports(pid: i32, rows: impl IntoIterator<Item = (i32, u16)>) -> Vec<u16> {
    let mut ports = Vec::new();
    for (owner, port) in rows {
        if owner == pid && !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports
}

/// How long a process-table scan may run before the probe stops waiting for it.
///
/// Measured on the development host (1,336 processes): a healthy
/// `/bin/ps -ax -o pid=,command=` took 0.28-0.35 s, and the same command has been
/// seen hanging for minutes (one measurement took 256 s) while the process table
/// was stalled host-wide. Five seconds is roughly fifteen times the healthy
/// figure, so a slow-but-working scan still answers, while a stalled one costs a
/// fetch five seconds instead of its whole deadline.
const PROCESS_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one `lsof` listening-port scan may run.
///
/// Measured on the same host at 0.02-0.05 s for one process. Five seconds for
/// the same reason as [`PROCESS_SCAN_TIMEOUT`]: generous for a working scan and
/// bounded for a stuck one.
const PORT_SCAN_TIMEOUT: Duration = Duration::from_secs(5);

/// Lets only one process-table scan run at a time, including a native worker or
/// `ps` child whose caller has already timed out.
static PROCESS_SCAN_GATE: Gate = Gate::new("Antigravity process scan");

/// Lets only one port scan run at a time, including a native worker or `lsof`
/// child whose caller has already timed out.
static PORT_SCAN_GATE: Gate = Gate::new("Antigravity listening-port scan");

/// The process-table scan discovery runs: the real `/bin/ps`, bounded by
/// [`PROCESS_SCAN_TIMEOUT`]. Tests substitute their own command.
fn process_scan() -> Option<BoundedCommand> {
    #[cfg(target_os = "macos")]
    return Some(BoundedCommand::new(
        &PROCESS_SCAN_GATE,
        "/bin/ps",
        ["-ax", "-o", "pid=,command="],
        PROCESS_SCAN_TIMEOUT,
    ));
    #[cfg(not(target_os = "macos"))]
    None
}

/// Report a scan that could not run as "could not look".
///
/// Transient `LocalSourceUnavailable`, so the last healthy reading keeps
/// serving, and worded so a reader can tell it apart from "no Antigravity
/// running": a stalled process table says nothing about whether the editor is
/// open.
fn could_not_look(what: &str, error: &SubprocessError) -> FetchError {
    FetchError::LocalSourceUnavailable(format!("could not {what}: {error}"))
}

/// List the running Antigravity servers.
///
/// `Ok(empty)` means the scan ran and found none. A scan that timed out, was
/// skipped because the previous one is still stuck, or could not start is an
/// `Err`, never an empty list: an empty list is published as "no Antigravity
/// running", which is a different fact.
#[cfg(any(target_os = "macos", test))]
async fn discover_servers_with_command(
    scan: &BoundedCommand,
) -> Result<Vec<LocalServer>, FetchError> {
    let output = match scan.output().await {
        Ok(output) => output,
        Err(error) => return Err(could_not_look("list processes", &error)),
    };
    if !output.status.success() {
        return Err(FetchError::LocalSourceUnavailable(format!(
            "could not list processes: `ps` exited with {}",
            output.status
        )));
    }
    Ok(parse_process_list(&String::from_utf8_lossy(&output.stdout)))
}

async fn discover_servers(scan: &Option<BoundedCommand>) -> Result<Vec<LocalServer>, FetchError> {
    #[cfg(any(target_os = "macos", test))]
    if let Some(scan) = scan {
        return discover_servers_with_command(scan).await;
    }
    #[cfg(not(any(target_os = "macos", test)))]
    let _ = scan;
    #[cfg(any(target_os = "linux", windows))]
    {
        let processes = PROCESS_SCAN_GATE
            .run_blocking(PROCESS_SCAN_TIMEOUT, native_processes)
            .await
            .map_err(|error| could_not_look("list processes", &error))?;
        Ok(processes
            .into_iter()
            .filter_map(|(pid, command)| {
                classify_command(&command).map(|csrf_token| LocalServer { pid, csrf_token })
            })
            .collect())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    Err(FetchError::LocalSourceUnavailable(
        "could not list processes: no native enumerator".into(),
    ))
}

/// The loopback ports a server process listens on.
///
/// Same contract as [`discover_servers`]: a scan that could not run is an `Err`.
/// The exit status is not checked because `lsof` exits 1 when the process simply
/// has no listening socket, which is an ordinary empty answer.
#[cfg(target_os = "macos")]
async fn discover_ports(pid: i32) -> Result<Vec<u16>, FetchError> {
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let Some(lsof) = lsof else {
        return Err(FetchError::LocalSourceUnavailable(
            "could not list listening ports: no lsof binary on this machine".to_string(),
        ));
    };
    let scan = BoundedCommand::new(
        &PORT_SCAN_GATE,
        lsof,
        [
            "-nP".to_string(),
            "-iTCP".to_string(),
            "-sTCP:LISTEN".to_string(),
            "-a".to_string(),
            "-p".to_string(),
            pid.to_string(),
        ],
        PORT_SCAN_TIMEOUT,
    );
    match scan.output().await {
        Ok(output) => Ok(parse_listening_ports(&String::from_utf8_lossy(
            &output.stdout,
        ))),
        Err(error) => Err(could_not_look("list listening ports", &error)),
    }
}

#[cfg(any(target_os = "linux", windows))]
async fn discover_ports(pid: i32) -> Result<Vec<u16>, FetchError> {
    PORT_SCAN_GATE
        .run_blocking(PORT_SCAN_TIMEOUT, move |deadline| {
            native_ports(pid, deadline)
        })
        .await
        .map_err(|error| could_not_look("list listening ports", &error))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
async fn discover_ports(_pid: i32) -> Result<Vec<u16>, FetchError> {
    Err(FetchError::LocalSourceUnavailable(
        "could not list listening ports: no native enumerator".into(),
    ))
}

#[cfg(any(target_os = "linux", windows))]
fn scan_deadline(deadline: std::time::Instant) -> std::io::Result<()> {
    if std::time::Instant::now() >= deadline {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "native scan deadline expired",
        ))
    } else {
        Ok(())
    }
}

/// Decode procfs argv without retaining NUL separators in the shared matcher.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_cmdline(bytes: &[u8]) -> String {
    bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}

/// LISTEN rows have a hexadecimal local port and a decimal inode in field ten.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_tcp(output: &str) -> Vec<(u64, u16)> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                return None;
            }
            let (address, port) = fields[1].split_once(':')?;
            if !matches!(address.len(), 8 | 32)
                || !address.bytes().all(|b| b.is_ascii_hexdigit())
                || port.len() != 4
            {
                return None;
            }
            Some((fields[9].parse().ok()?, u16::from_str_radix(port, 16).ok()?))
        })
        .collect()
}

/// Decode collected procfs entries independently of filesystem access, so the
/// Linux discovery logic can be fixture-tested on every supported platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn enumerate_proc_cmdlines(
    entries: impl IntoIterator<Item = (i32, Vec<u8>)>,
) -> Vec<(i32, String)> {
    entries
        .into_iter()
        .map(|(pid, bytes)| (pid, parse_proc_cmdline(&bytes)))
        .collect()
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn proc_socket_inode(target: &str) -> Option<u64> {
    target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// A socket table is shared by processes; only inodes from this pid's fd table
/// may supply candidate ports.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn select_proc_ports(
    pid: i32,
    inodes: &HashSet<u64>,
    rows: impl IntoIterator<Item = (u64, u16)>,
) -> Vec<u16> {
    select_ports(
        pid,
        rows.into_iter()
            .filter_map(|(inode, port)| inodes.contains(&inode).then_some((pid, port))),
    )
}

#[cfg(target_os = "linux")]
fn read_proc_file(
    path: &std::path::Path,
    cap: u64,
    deadline: std::time::Instant,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    scan_deadline(deadline)?;
    // procfs files have no useful metadata length. Cap reads and refuse symlinks
    // or blocking opens, including when this reader is used with fixture roots.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    scan_deadline(deadline)?;
    if bytes.len() as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "procfs entry exceeds scan cap",
        ));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn linux_processes(
    root: &std::path::Path,
    deadline: std::time::Instant,
) -> std::io::Result<Vec<(i32, String)>> {
    scan_deadline(deadline)?;
    let entries = std::fs::read_dir(root)?;
    let mut processes = Vec::new();
    let mut readable = false;
    for entry in entries {
        scan_deadline(deadline)?;
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        // A disappearing or inaccessible process is normal during enumeration.
        let Ok(bytes) = read_proc_file(&entry.path().join("cmdline"), 64 * 1024, deadline) else {
            continue;
        };
        readable = true;
        processes.push((pid, bytes));
    }
    scan_deadline(deadline)?;
    if !readable {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "no readable procfs command lines",
        ));
    }
    Ok(enumerate_proc_cmdlines(processes))
}

#[cfg(target_os = "linux")]
fn linux_ports(
    root: &std::path::Path,
    pid: i32,
    deadline: std::time::Instant,
) -> std::io::Result<Vec<u16>> {
    scan_deadline(deadline)?;
    let mut inodes = HashSet::new();
    for entry in std::fs::read_dir(root.join(pid.to_string()).join("fd"))? {
        scan_deadline(deadline)?;
        let Ok(entry) = entry else { continue };
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let Some(inode) = target.to_str().and_then(proc_socket_inode) else {
            continue;
        };
        inodes.insert(inode);
    }
    let mut rows = Vec::new();
    for table in ["net/tcp", "net/tcp6"] {
        let bytes = match read_proc_file(&root.join(table), 16 * 1024 * 1024, deadline) {
            Ok(bytes) => bytes,
            // IPv6 can be disabled at kernel build time.
            Err(error)
                if table.ends_with("tcp6") && error.kind() == std::io::ErrorKind::NotFound =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        rows.extend(parse_proc_tcp(&String::from_utf8_lossy(&bytes)));
    }
    scan_deadline(deadline)?;
    Ok(select_proc_ports(pid, &inodes, rows))
}

#[cfg(target_os = "linux")]
fn native_processes(deadline: std::time::Instant) -> std::io::Result<Vec<(i32, String)>> {
    linux_processes(std::path::Path::new("/proc"), deadline)
}

#[cfg(target_os = "linux")]
fn native_ports(pid: i32, deadline: std::time::Instant) -> std::io::Result<Vec<u16>> {
    linux_ports(std::path::Path::new("/proc"), pid, deadline)
}

/// Decode documented MIB_TCP{,6}ROW_OWNER_PID layouts without casting an
/// untrusted row count into a slice. DWORD ports carry two network-order bytes.
#[cfg(any(windows, test))]
fn parse_tcp_owner_table(bytes: &[u8], ipv6: bool) -> std::io::Result<Vec<(i32, u16)>> {
    let invalid =
        || std::io::Error::new(std::io::ErrorKind::InvalidData, "malformed TCP owner table");
    let count =
        u32::from_le_bytes(bytes.get(..4).ok_or_else(invalid)?.try_into().unwrap()) as usize;
    let (row_size, port_at, state_at, pid_at) = if ipv6 {
        (56, 20, 48, 52)
    } else {
        (24, 8, 0, 20)
    };
    let rows = bytes.get(4..).ok_or_else(invalid)?;
    if count > rows.len() / row_size {
        return Err(invalid());
    }
    let mut out = Vec::new();
    for row in rows.chunks_exact(row_size).take(count) {
        let state = u32::from_le_bytes(row[state_at..state_at + 4].try_into().unwrap());
        let pid = u32::from_le_bytes(row[pid_at..pid_at + 4].try_into().unwrap());
        if state == 2 {
            if let Ok(pid) = i32::try_from(pid) {
                out.push((pid, u16::from_be_bytes([row[port_at], row[port_at + 1]])));
            }
        }
    }
    Ok(out)
}

#[cfg(windows)]
fn native_processes(deadline: std::time::Instant) -> std::io::Result<Vec<(i32, String)>> {
    windows_discovery::processes(deadline, None)
}

#[cfg(windows)]
fn native_ports(pid: i32, deadline: std::time::Instant) -> std::io::Result<Vec<u16>> {
    windows_discovery::ports(pid, deadline)
}

#[cfg(windows)]
mod windows_discovery {
    use super::{command_requires_csrf, parse_tcp_owner_table, scan_deadline, select_ports};
    use std::{ffi::c_void, io, mem, ptr, time::Instant};
    use windows_sys::{
        Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation},
        Win32::{
            Foundation::{
                CloseHandle, GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_SUPPORTED,
                ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING,
            },
            NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER},
            Networking::WinSock::{AF_INET, AF_INET6},
            System::{
                Diagnostics::{
                    Debug::ReadProcessMemory,
                    ToolHelp::{
                        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
                        TH32CS_SNAPPROCESS,
                    },
                },
                Threading::{
                    GetCurrentProcess, IsWow64Process2, OpenProcess, QueryFullProcessImageNameW,
                    PEB, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_INFORMATION,
                    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
                    RTL_USER_PROCESS_PARAMETERS,
                },
            },
        },
    };

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // Only successful OpenProcess/snapshot handles are owned here.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn open(pid: u32, rights: u32) -> io::Result<OwnedHandle> {
        // Request explicit rights only; never enable SeDebugPrivilege.
        let handle = unsafe { OpenProcess(rights, 0, pid) };
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(OwnedHandle(handle))
        }
    }

    fn image_path(handle: HANDLE) -> io::Result<String> {
        let mut path = vec![0u16; 32768];
        let mut size = path.len() as u32;
        // The allocated buffer is writable for the supplied character count.
        if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(String::from_utf16_lossy(&path[..size as usize]))
    }

    fn machine(handle: HANDLE) -> io::Result<u16> {
        let (mut process, mut native) = (0, 0);
        // Both output pointers are live; UNKNOWN (zero) means a native process.
        if unsafe { IsWow64Process2(handle, &mut process, &mut native) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(if process == 0 { native } else { process })
    }

    fn read_memory(
        handle: HANDLE,
        address: usize,
        bytes: &mut [u8],
        deadline: Instant,
    ) -> io::Result<()> {
        scan_deadline(deadline)?;
        if address == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "null remote address",
            ));
        }
        let mut read = 0;
        // The remote pointer is never dereferenced locally. Win32 validates it;
        // the local byte buffer is writable, and partial reads are refused.
        if unsafe {
            ReadProcessMemory(
                handle,
                address as *const c_void,
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut read,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if read != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "partial remote read",
            ));
        }
        scan_deadline(deadline)
    }

    fn command_line(handle: HANDLE, deadline: Instant) -> io::Result<String> {
        scan_deadline(deadline)?;
        // PEB layouts are native-width only. Refuse even same-width foreign
        // architectures rather than assuming a foreign/emulated layout.
        if machine(handle)? != machine(unsafe { GetCurrentProcess() })? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "different process architecture",
            ));
        }
        let mut basic = PROCESS_BASIC_INFORMATION::default();
        let mut returned = 0;
        // ProcessBasicInformation and these field layouts are documented at
        // Microsoft Learn's winternl/PEB/RTL_USER_PROCESS_PARAMETERS pages.
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                ProcessBasicInformation,
                (&mut basic as *mut PROCESS_BASIC_INFORMATION).cast(),
                mem::size_of_val(&basic) as u32,
                &mut returned,
            )
        };
        if status < 0 || returned as usize != mem::size_of_val(&basic) {
            return Err(io::Error::other(format!(
                "process basic information failed ({status:#x})"
            )));
        }
        let invalid =
            || io::Error::new(io::ErrorKind::InvalidData, "malformed process command line");
        let parameters_at = (basic.PebBaseAddress as usize)
            .checked_add(mem::offset_of!(PEB, ProcessParameters))
            .ok_or_else(invalid)?;
        let mut pointer = [0u8; mem::size_of::<usize>()];
        read_memory(handle, parameters_at, &mut pointer, deadline)?;
        let parameters = usize::from_ne_bytes(pointer);
        if parameters == 0 {
            return Err(invalid());
        }
        let command_at = parameters
            .checked_add(mem::offset_of!(RTL_USER_PROCESS_PARAMETERS, CommandLine))
            .ok_or_else(invalid)?;
        let mut descriptor = [0u8; mem::size_of::<UNICODE_STRING>()];
        read_memory(handle, command_at, &mut descriptor, deadline)?;
        // UNICODE_STRING contains integer lengths and a raw pointer only; every
        // bit pattern is valid, and read_unaligned avoids buffer alignment assumptions.
        let command = unsafe { ptr::read_unaligned(descriptor.as_ptr().cast::<UNICODE_STRING>()) };
        if command.Length == 0
            || command.Length > 32 * 1024
            || command.Length % 2 != 0
            || command.Length > command.MaximumLength
            || command.MaximumLength % 2 != 0
            || command.Buffer.is_null()
        {
            return Err(invalid());
        }
        let mut bytes = vec![0u8; command.Length as usize];
        read_memory(handle, command.Buffer as usize, &mut bytes, deadline)?;
        let units: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        String::from_utf16(&units).map_err(|_| invalid())
    }

    /// The optional own pid lets CI exercise exactly the same PEB reader without
    /// pretending that the quota-core test executable is an Antigravity server.
    pub(super) fn processes(
        deadline: Instant,
        own_pid: Option<u32>,
    ) -> io::Result<Vec<(i32, String)>> {
        scan_deadline(deadline)?;
        // Process-only snapshots need no module walk or target memory access.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = OwnedHandle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut processes = Vec::new();
        let mut candidates = 0;
        // dwSize is initialized and the snapshot remains owned for the walk.
        let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) };
        let mut error = if more == 0 {
            unsafe { GetLastError() }
        } else {
            ERROR_NO_MORE_FILES
        };
        while more != 0 {
            scan_deadline(deadline)?;
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            let image = String::from_utf16_lossy(&entry.szExeFile[..end]);
            let lower = image.to_ascii_lowercase();
            let pid = entry.th32ProcessID;
            let own = own_pid == Some(pid) && pid == std::process::id();
            let possible = own
                || lower.contains("language_server")
                || lower.contains("language-server")
                || command_requires_csrf(&format!("/{image}")).is_some();
            if possible {
                let read = (|| {
                    let limited = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
                    let path = image_path(limited.0)?;
                    if !own && command_requires_csrf(&path).is_none() {
                        return Ok(None);
                    }
                    // Check the path again on the memory-read handle so pid reuse
                    // between opens cannot expose an unrelated process's argv.
                    let process = open(pid, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
                    if image_path(process.0)? != path {
                        return Err(io::Error::other("process image changed"));
                    }
                    command_line(process.0, deadline).map(Some)
                })();
                match read {
                    Ok(None) => {}
                    Ok(Some(command)) => {
                        candidates += 1;
                        if let Ok(pid) = i32::try_from(pid) {
                            processes.push((pid, command));
                        }
                    }
                    Err(error) => {
                        candidates += 1;
                        // Command lines contain CSRF tokens: only image, pid and
                        // the read failure may appear in diagnostics.
                        eprintln!(
                            "{} skipped local process pid={pid} image={image:?}: {error}",
                            crate::LOG_TAG
                        );
                    }
                }
            }
            more = unsafe { Process32NextW(snapshot.0, &mut entry) };
            if more == 0 {
                error = unsafe { GetLastError() };
            }
        }
        if error != ERROR_NO_MORE_FILES {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        scan_deadline(deadline)?;
        if candidates > 0 && processes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no readable candidate process command lines",
            ));
        }
        Ok(processes)
    }

    fn tcp_table(ipv6: bool, deadline: Instant) -> io::Result<Vec<(i32, u16)>> {
        let family = if ipv6 { AF_INET6 } else { AF_INET } as u32;
        let mut size = 0;
        // The first call asks only for the required size; no buffer is supplied.
        let status = unsafe {
            GetExtendedTcpTable(
                ptr::null_mut(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if ipv6 && status == ERROR_NOT_SUPPORTED {
            return Ok(Vec::new());
        }
        if status != ERROR_INSUFFICIENT_BUFFER && status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        // A busy TCP table can grow between calls. Bound both retries and allocation.
        for _ in 0..3 {
            scan_deadline(deadline)?;
            if !(4..=16 * 1024 * 1024).contains(&size) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "TCP owner table exceeds scan cap",
                ));
            }
            let mut buffer = vec![0u32; (size as usize).div_ceil(4)];
            let capacity = buffer.len() * 4;
            let status = unsafe {
                GetExtendedTcpTable(
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if status == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            if size as usize > capacity {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "TCP owner table overrun",
                ));
            }
            // The DWORD allocation is aligned and initialized; only returned bytes
            // are exposed, and the parser validates the row count before access.
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size as usize) };
            return parse_tcp_owner_table(bytes, ipv6);
        }
        Err(io::Error::other("TCP owner table kept growing during scan"))
    }

    pub(super) fn ports(pid: i32, deadline: Instant) -> io::Result<Vec<u16>> {
        let mut rows = tcp_table(false, deadline)?;
        rows.extend(tcp_table(true, deadline)?);
        scan_deadline(deadline)?;
        Ok(select_ports(pid, rows))
    }
}

/// What one local probe found, cached briefly so concurrent handle fetches share
/// a single discovery.
#[derive(Clone, Debug)]
struct LocalProbe {
    snapshots: Vec<LocalSnapshot>,
    /// Whether any Antigravity server process was found, so a probe that served
    /// nothing can say whether nothing is running or nothing answered.
    servers_found: bool,
}

// ---- response parsing (pure, unit-tested) -----------------------------------

/// The RPC wraps the summary in a `{"response": {...}}` envelope (confirmed on the
/// live `agy` wire); older/fixture shapes may carry `groups` at the top level.
#[derive(Deserialize)]
struct QuotaSummaryEnvelope {
    #[serde(default)]
    response: Option<QuotaSummary>,
}

#[derive(Deserialize)]
struct QuotaSummary {
    #[serde(default)]
    groups: Vec<QuotaGroup>,
}

#[derive(Deserialize)]
struct QuotaGroup {
    #[serde(rename = "displayName", default)]
    display_name: String,
    #[serde(default)]
    buckets: Vec<QuotaBucket>,
}

#[derive(Deserialize)]
struct QuotaBucket {
    #[serde(rename = "bucketId", default)]
    bucket_id: String,
    #[serde(rename = "displayName", default)]
    display_name: String,
    /// Explicit window kind on the live wire (`"5h"` / `"weekly"`); preferred over
    /// inferring from the id/name.
    #[serde(default)]
    window: Option<String>,
    #[serde(default)]
    disabled: bool,
    #[serde(rename = "remainingFraction", default)]
    remaining_fraction: Option<f64>,
    /// Newer payloads nest the fraction under `remaining` (`{case, value}` or
    /// `{remainingFraction}`); checked as a fallback.
    #[serde(default)]
    remaining: Option<Value>,
    #[serde(rename = "resetTime", default)]
    reset_time: Option<Value>,
}

#[derive(Clone, Copy, PartialEq)]
enum Pool {
    Gemini,
    ClaudeGpt,
    Other,
}

/// The pool a model belongs to, for the remote lane.
///
/// The local server labels its groups ("Gemini Models", "Claude and GPT
/// models"); the cloud API does not, and returns a flat list of models. The
/// model id is the only pool evidence it carries, and the same two-pool split is
/// visible in it: the native Gemini models meter separately from the external
/// Claude and GPT ones.
fn pool_of_model_id(model_id: &str) -> Pool {
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("gemini") {
        Pool::Gemini
    } else if id.starts_with("claude") || id.contains("gpt") {
        Pool::ClaudeGpt
    } else {
        Pool::Other
    }
}

const SESSION_CADENCE_ALIASES: &[&str] = &["session", "5h", "5-hour", "five hour", "five-hour"];

/// The cadence words a bucket states, normalized.
///
/// An explicit, non-empty `window` is the ONLY source when present; the id and
/// display name are read only when it is absent or blank. Taking the union
/// instead let the id outvote the window: session aliases win over weekly, so
/// `{"bucketId":"gemini-5h","window":"weekly"}` was published as a 300-minute
/// window although the server said weekly, and consumers read `windowMinutes`
/// as the pace. The id is a legacy name; the window is the statement.
///
/// A consequence: an unrecognised `window` (say `"unknown"`) yields no cadence
/// at all, rather than borrowing one from the id. Upstream does the same
/// (`quotaCadenceCandidates` in
/// `Sources/CodexBarCore/Providers/Antigravity/AntigravityStatusProbe.swift`, and
/// `explicit cadence takes precedence over legacy bucket names` in
/// `Tests/CodexBarTests/AntigravityQuotaSourceParityTests.swift`, v0.70.0).
fn quota_cadence_candidates(bucket: &QuotaBucket) -> HashSet<String> {
    let mut candidates = HashSet::new();
    let explicit = bucket
        .window
        .as_deref()
        .map(str::trim)
        .filter(|window| !window.is_empty());
    let values: Vec<&str> = match explicit {
        Some(window) => vec![window],
        None => vec![bucket.bucket_id.as_str(), bucket.display_name.as_str()],
    };
    for raw_value in values {
        let normalized = raw_value.trim().to_ascii_lowercase().replace('_', "-");
        if normalized.is_empty() {
            continue;
        }

        let mut normalized_candidates = vec![normalized.clone()];
        if let Some(stripped) = normalized.strip_suffix(" limit") {
            normalized_candidates.push(stripped.to_string());
        }
        for candidate in normalized_candidates {
            candidates.insert(candidate.clone());
            for alias in SESSION_CADENCE_ALIASES.iter().copied().chain(["weekly"]) {
                if candidate.ends_with(&format!("-{alias}")) {
                    candidates.insert(alias.to_string());
                }
            }
        }
    }
    candidates
}

/// Window length in minutes for a bucket, by its 5-hour/weekly kind (faithful
/// derivation from the bucket's own cadence fields, not invention). None when unknown.
fn window_minutes_of(bucket: &QuotaBucket) -> Option<i64> {
    let candidates = quota_cadence_candidates(bucket);
    if SESSION_CADENCE_ALIASES
        .iter()
        .any(|alias| candidates.contains(*alias))
    {
        Some(SESSION_WINDOW_MINUTES)
    } else if candidates.contains("weekly") {
        Some(WEEKLY_WINDOW_MINUTES)
    } else {
        None
    }
}

fn remaining_fraction_of(bucket: &QuotaBucket) -> Option<f64> {
    if let Some(f) = bucket.remaining_fraction {
        return Some(f);
    }
    let remaining = bucket.remaining.as_ref()?;
    if let Some(f) = remaining.get("remainingFraction").and_then(Value::as_f64) {
        return Some(f);
    }
    // `{case: "remainingFraction", value: <f>}`
    if remaining.get("case").and_then(Value::as_str) == Some("remainingFraction") {
        return remaining.get("value").and_then(Value::as_f64);
    }
    None
}

/// Parse `resetTime` (ISO8601 string, or epoch seconds as number/string) to ISO8601.
fn parse_reset(value: &Value) -> Option<String> {
    if let Some(n) = value.as_f64() {
        if n > 0.0 {
            return env::epoch_to_iso8601(n as i64);
        }
    }
    let s = value.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    if let Ok(secs) = s.parse::<i64>() {
        return env::epoch_to_iso8601(secs);
    }
    None
}

/// A bucket that resolved to a usable window, tagged with its pool.
struct ResolvedWindow {
    window: RateWindow,
    title: String,
    id: String,
}

fn resolve_window(group: &QuotaGroup, bucket: &QuotaBucket) -> Option<ResolvedWindow> {
    if bucket.disabled {
        return None;
    }
    let remaining = remaining_fraction_of(bucket)?;
    let reset = bucket.reset_time.as_ref().and_then(parse_reset);
    // Round to 2dp: the fraction→percent arithmetic exposes float noise
    // (e.g. (1 - 0.8) * 100 = 19.999999999999996), same cleanup grok applies.
    let used_percent = (((1.0 - remaining) * 100.0).clamp(0.0, 100.0) * 100.0).round() / 100.0;
    // THE CADENCE THIS MODULE DERIVED, NOT THE WORDING THE LANE HAPPENED TO CARRY.
    // Both lanes reach this function, and they arrive with different text for the
    // same meter: the cloud lane brings upstream's `displayName` ("Weekly Limit
    // Remaining"), while the cache lane has only the raw cadence key ("weekly").
    // Concatenating whatever arrived gave one account "Gemini Models weekly" and
    // its sibling "Gemini Models Weekly Limit Remaining" for the SAME window id --
    // and made the same account change label when an editor opened.
    //
    // Upstream's own wording is not stable enough to key on even within one lane:
    // "Weekly Limit", "Weekly Limit Remaining" and "Weekly_Limit" have all been
    // observed for this field. Deriving from `window_minutes` makes the title a
    // function of the same classification the id encodes, so the two cannot
    // disagree and neither can two lanes.
    let window_minutes = window_minutes_of(bucket);
    let cadence = match window_minutes {
        Some(300) => "5h".to_string(),
        Some(10080) => "weekly".to_string(),
        // An unclassified cadence keeps the upstream text rather than inventing
        // one: a wrong cadence in a label is worse than an unfamiliar one, and
        // this is the branch a new window shape arrives through.
        _ => bucket.display_name.trim().to_string(),
    };
    let title = format!("{} {}", group.display_name.trim(), cadence)
        .trim()
        .to_string();
    Some(ResolvedWindow {
        window: RateWindow {
            window_kind: {
                let candidates = quota_cadence_candidates(bucket);
                if ["5h", "5-hour", "five hour", "five-hour"]
                    .iter()
                    .any(|alias| candidates.contains(*alias))
                {
                    Some(cortexkit_provider_usage::window_kind::FIVE_HOUR.to_string())
                } else if [
                    "weekly",
                    "weekly-limit",
                    "weekly limit remaining",
                    "weekly-limit-remaining",
                ]
                .iter()
                .any(|label| candidates.contains(*label))
                {
                    Some(cortexkit_provider_usage::window_kind::WEEKLY.to_string())
                } else {
                    None
                }
            },
            used_percent,
            raw_used_percent: None,
            resets_at: reset,
            window_minutes,
            used_count: None,
            total_count: None,
            regeneration: None,
            breakdown: None,
        },
        title,
        id: bucket.bucket_id.clone(),
    })
}

/// Normalize a RetrieveUserQuotaSummary JSON body to [`Usage`]. Pure — unit-testable.
pub fn parse_quota_summary(body: &str) -> Result<Usage, FetchError> {
    let (envelope, unread) =
        crate::unread_keys::decode_collecting_unread::<QuotaSummaryEnvelope>(body.as_bytes());
    let envelope = envelope
        .map_err(|e| FetchError::Decode(format!("antigravity quota summary not JSON: {e}")))?;
    // Prefer the `{"response": {...}}` envelope (live wire); fall back to a top-level
    // `{"groups": ...}` shape. Unread keys are reported only for the shape
    // actually used: decoding a top-level shape as the envelope skips `groups`,
    // which the fallback then reads.
    let summary = match envelope.response {
        Some(s) => {
            crate::unread_keys::report_unread(PROVIDER_NAME, unread);
            s
        }
        None => crate::unread_keys::decode_reporting_unread::<QuotaSummary>(
            PROVIDER_NAME,
            body.as_bytes(),
        )
        .map_err(|e| FetchError::Decode(format!("antigravity quota summary not JSON: {e}")))?,
    };
    normalize_quota_summary(summary)
}

fn normalize_quota_summary(summary: QuotaSummary) -> Result<Usage, FetchError> {
    let mut resolved: Vec<ResolvedWindow> = Vec::new();
    for group in &summary.groups {
        for bucket in &group.buckets {
            if let Some(w) = resolve_window(group, bucket) {
                resolved.push(w);
            }
        }
    }

    if resolved.is_empty() {
        return Err(FetchError::Decode(
            "antigravity: no quota buckets with a known fraction".to_string(),
        ));
    }

    // NO UNNAMED `primary` FOR THIS PROVIDER. Antigravity meters two
    // independent pools at two cadences, and every one of those four windows
    // is named -- so anything in `primary` is necessarily a COPY of one of
    // them. It was the native Gemini pool's representative, which made the
    // provider publish five windows for four limits and rendered a fifth,
    // unlabelled row beside the four labelled ones.
    //
    // The duplication was documented as a caveat every consumer had to handle
    // (invisible to a maximum, double-counting under a sum or a mean). Removing
    // the copy retires the caveat rather than adding another.
    //
    // A consumer reading `primary` alone gets nothing here, and that reading
    // was already wrong: two pools cannot be summarised by one window, which is
    // why the contract requires the maximum across slots AND extras. The one
    // consumer known to have read `primary` alone had a defect from it and
    // fixed it.

    // Each resolved bucket is surfaced as a per-pool named extra window.
    let extra: Vec<ExtraWindow> = resolved
        .iter()
        .map(|w| ExtraWindow {
            title: Some(w.title.clone()),
            id: Some(w.id.clone()),
            window: Some(w.window.clone()),
        })
        .collect();

    Ok(Usage {
        primary: None,
        secondary: None,
        tertiary: None,
        extra_rate_windows: if extra.is_empty() { None } else { Some(extra) },
    })
}

/// Serve only dated, recent quota groups whose `5h`/`weekly` windows have a
/// remaining fraction. Use the oldest served observation so a newer group
/// cannot make the combined reading appear fresher than all of its values.
fn parse_cached_quota(cache: &CachedQuota, now: DateTime<Utc>) -> Option<(Usage, DateTime<Utc>)> {
    let mut extra = Vec::new();
    let mut oldest: Option<DateTime<Utc>> = None;
    for (pool, display_name, id_prefix) in [
        (cache.gemini.as_ref(), "Gemini Models", "gemini"),
        (cache.non_gemini.as_ref(), "Claude and GPT models", "3p"),
    ] {
        let Some(pool) = pool else { continue };
        let Some(checked_at) = pool
            .checked_at
            .and_then(DateTime::<Utc>::from_timestamp_millis)
        else {
            continue;
        };
        let Ok(age) = now.signed_duration_since(checked_at).to_std() else {
            continue;
        };
        if age > CACHED_QUOTA_MAX_AGE {
            continue;
        }
        let windows: Vec<_> = pool
            .windows
            .iter()
            .filter_map(|window| {
                if !matches!(window.window.as_str(), "5h" | "weekly") {
                    return None;
                }
                let remaining = window.remaining_fraction?;
                if !remaining.is_finite() || !(0.0..=1.0).contains(&remaining) {
                    return None;
                }
                let group = QuotaGroup {
                    display_name: display_name.to_string(),
                    buckets: Vec::new(),
                };
                let bucket = QuotaBucket {
                    bucket_id: format!("{id_prefix}-{}", window.window),
                    display_name: window.window.clone(),
                    window: Some(window.window.clone()),
                    disabled: false,
                    remaining_fraction: Some(remaining),
                    remaining: None,
                    reset_time: None,
                };
                let mut resolved = resolve_window(&group, &bucket)?;
                // Do not round the precise remainingFraction like remainingPercent:
                // rounding would discard real headroom changes between plugin polls.
                resolved.window.used_percent = (1.0 - remaining) * 100.0;
                resolved.window.resets_at = window
                    .reset_at
                    .and_then(DateTime::<Utc>::from_timestamp_millis)
                    .map(|reset| reset.format("%Y-%m-%dT%H:%M:%SZ").to_string());
                Some(ExtraWindow {
                    title: Some(resolved.title),
                    id: Some(resolved.id),
                    window: Some(resolved.window),
                })
            })
            .collect();
        if windows.is_empty() {
            continue;
        }
        oldest = Some(oldest.map_or(checked_at, |previous| previous.min(checked_at)));
        extra.extend(windows);
    }
    Some((
        Usage {
            primary: None,
            secondary: None,
            tertiary: None,
            extra_rate_windows: Some(extra),
        },
        oldest?,
    ))
}

/// Serve a recent sidebar only for positively identified paid accounts. Free
/// and unknown tiers keep the vault credential's standard cloud quota path.
fn fresh_paid_cached_quota(
    account: Option<&StoredAccount>,
    now: DateTime<Utc>,
) -> Option<(Usage, DateTime<Utc>)> {
    let account = account.filter(|account| account_holds_paid_tier(Some(account)))?;
    parse_cached_quota(&account.quota, now)
}

// ---- remote lane (cloud, no local process) -----------------------------------

/// A `retrieveUserQuota` response: a flat list of per-model buckets.
#[derive(Debug, Deserialize)]
struct RemoteQuotaResponse {
    buckets: Option<Vec<RemoteQuotaBucket>>,
}

#[derive(Debug, Deserialize)]
struct RemoteQuotaBucket {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

/// Normalize a cloud `retrieveUserQuota` body into the same shape the local
/// probe publishes.
///
/// The two lanes describe one account and must agree, but they are handed
/// different granularities: the local server returns named pool groups, while
/// the cloud returns one bucket per model. Twenty-odd near-identical model rows
/// are not what a reader wants, and they are not independent meters either --
/// every model in a pool shares that pool's fraction and reset. So the models
/// are folded back into their pools here, and the published shape is the same
/// either way: the native Gemini pool in the unnamed `primary`, both pools as
/// named extra windows.
///
/// So `primary` is a HEADLINE POINTING AT one of the extras, not an additional
/// limit, and this provider is the only one on the wire where a slot and a named
/// extra are the same window. That is invisible to the reduction the consumer
/// contract recommends -- a maximum is duplication-insensitive -- and it double
/// counts under a sum or a mean. Stated in docs/consumer-contract.md beside the
/// place those other policies are invited; if this shape ever changes, that
/// paragraph is the one that goes stale.
///
/// A model whose bucket states no reset is skipped rather than pooled. Those are
/// the always-available internal models, and folding a permanently-idle bucket
/// into a metered pool would drag the pool's worst-case reading toward zero.
///
/// Pooling also keeps this provider's identifiers out of a namespace it shares
/// with another. Antigravity and Gemini are separate products on the same Google
/// API, and every model id the Gemini provider publishes appears in this
/// response too. Publishing per-model detail here would emit identifiers
/// byte-identical to that provider's while describing a different quota pool
/// under a different credential -- so a consumer keying on the identifier alone
/// would merge two products and see plausible numbers throughout. The wire
/// contract asks consumers to key on `(provider, id)`, but a shape that cannot
/// collide is worth more than a rule saying it must not.
///
/// No window length is published. The cloud response states none -- its buckets
/// carry only a model id, a fraction, a reset and a token type -- and it cannot
/// be inferred from the reset either: the local server meters each pool on both
/// a five-hour and a weekly window, while the cloud returns a single reset per
/// pool, so which of the two meters that reset belongs to is not knowable from
/// this response. An absent cadence is a state consumers already handle; a
/// guessed one would be acted on.
/// Whether a summary-endpoint failure means "not permitted here", the one
/// condition that justifies dropping to the per-model lane.
///
/// Narrow ON PURPOSE. The fallback publishes strictly less than the summary
/// endpoint -- one collapsed window instead of a cadence pair -- so widening
/// this predicate trades a loud, retryable failure for a quiet wrong number.
/// A 403 is the account not being entitled to the summary; a timeout, a 5xx or
/// a decode failure are all conditions where the good endpoint may answer on the
/// next tick, and stale-serving through them is what the refresher already does
/// correctly.
fn summary_unavailable(error: &FetchError) -> bool {
    matches!(
        error,
        FetchError::ProviderStatus(403 | 404, _) | FetchError::Unauthorized(_)
    )
}

fn parse_remote_quota(body: &[u8]) -> Result<Usage, FetchError> {
    let response: RemoteQuotaResponse =
        crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body)
            .map_err(|e| FetchError::Decode(format!("antigravity remote quota not JSON: {e}")))?;
    // Two distinguishable inputs needing different answers. An ABSENT field means
    // our struct and their payload disagree -- a rename upstream or a mistake
    // here -- and Decode is the class that sends a reader to this repo. A field
    // PRESENT AND EMPTY is the upstream stating that this account has no
    // buckets, which is a fact about the account with nothing to fix.
    //
    // WHICH ARM A REAL NO-QUOTA ACCOUNT TAKES IS UNVERIFIED, same as gemini's
    // twin of this block, and for the same reason: proto3 JSON omits empty
    // repeated fields, so this protobuf-backed Google API plausibly sends no
    // `buckets` key at all rather than an empty list. If so the empty-list arm is
    // unreachable and the Decode arm is what a bucket-less account gets -- the
    // exact shape that cost a router a day of serving an ended qwen-cloud plan
    // (insula#11), where the same principle predicted the wrong arm.
    //
    // Left as it stands deliberately: no account on this host has produced a
    // bucket-less response, and swapping one unverified arm for another buys
    // nothing. One observed payload from an unentitled account settles it, and
    // this comment is here so whoever sees one knows it is worth capturing.
    let buckets = match response.buckets {
        None => {
            return Err(FetchError::Decode(
                "antigravity remote quota response has no buckets field".to_string(),
            ))
        }
        Some(buckets) if buckets.is_empty() => {
            return Err(FetchError::NoQuotaReported(
                "antigravity: this account has no quota buckets".to_string(),
            ))
        }
        Some(buckets) => buckets,
    };

    // Pool -> (worst used percent seen, its reset, how many models it covers).
    let mut pools: Vec<(Pool, f64, Option<String>, usize)> = Vec::new();
    for bucket in &buckets {
        let (Some(model_id), Some(remaining)) = (
            bucket
                .model_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty()),
            bucket.remaining_fraction,
        ) else {
            continue;
        };
        if !remaining.is_finite() {
            continue;
        }
        let Some(reset) = bucket
            .reset_time
            .as_deref()
            .map(str::trim)
            .filter(|reset| !reset.is_empty())
        else {
            continue;
        };
        // Same 2dp rounding as the local lane, so one account cannot read
        // differently depending on which lane answered.
        let used_percent = (((1.0 - remaining) * 100.0).clamp(0.0, 100.0) * 100.0).round() / 100.0;
        let pool = pool_of_model_id(model_id);
        match pools.iter_mut().find(|(existing, ..)| *existing == pool) {
            Some((_, worst, worst_reset, count)) => {
                *count += 1;
                if used_percent > *worst {
                    *worst = used_percent;
                    *worst_reset = Some(reset.to_string());
                }
            }
            None => pools.push((pool, used_percent, Some(reset.to_string()), 1)),
        }
    }

    if pools.is_empty() {
        return Err(FetchError::Decode(
            "antigravity remote quota: no bucket carried a usable fraction and reset".to_string(),
        ));
    }

    let mut resolved: Vec<ResolvedWindow> = Vec::new();
    for (pool, used_percent, reset, models) in pools {
        let resets_at = reset
            .as_deref()
            .and_then(|reset| parse_reset(&Value::String(reset.to_string())));
        let title = match pool {
            Pool::Gemini => "Gemini Models",
            Pool::ClaudeGpt => "Claude and GPT models",
            Pool::Other => "Other models",
        };
        resolved.push(ResolvedWindow {
            window: RateWindow {
                window_kind: None,
                used_percent,
                raw_used_percent: None,
                window_minutes: None,
                resets_at,
                used_count: None,
                total_count: None,
                regeneration: None,
                breakdown: None,
            },
            title: format!("{title} ({models} models)"),
            id: title.to_string(),
        });
    }

    // No unnamed slot, for the same reason as the local lane: every window this
    // provider meters is named, so a slot could only hold a copy. Both lanes
    // must agree on this or the same account changes shape when an editor opens.
    let extra: Vec<ExtraWindow> = resolved
        .iter()
        .map(|w| ExtraWindow {
            title: Some(w.title.clone()),
            id: Some(w.id.clone()),
            window: Some(w.window.clone()),
        })
        .collect();

    Ok(Usage {
        primary: None,
        secondary: None,
        tertiary: None,
        extra_rate_windows: if extra.is_empty() { None } else { Some(extra) },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadCodeAssistResponse {
    current_tier: Option<Value>,
    cloudaicompanion_project: Option<CodeAssistProject>,
    // Optional inventory must not make an otherwise usable quota lane fail to decode.
    ineligible_tiers: Option<Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CodeAssistProject {
    Id(String),
    Reference {
        id: Option<String>,
        #[serde(rename = "projectId")]
        project_id: Option<String>,
    },
}

impl LoadCodeAssistResponse {
    fn rejects_client_for_consumer_tier(&self) -> bool {
        let project = self
            .cloudaicompanion_project
            .as_ref()
            .and_then(|project| match project {
                CodeAssistProject::Id(id) => Some(id.as_str()),
                CodeAssistProject::Reference { id, project_id } => {
                    id.as_deref().or(project_id.as_deref())
                }
            });
        self.current_tier.is_none()
            && project.is_none_or(|id| id.trim().is_empty())
            && self
                .ineligible_tiers
                .as_ref()
                .and_then(Value::as_array)
                .is_some_and(|tiers| {
                    tiers.iter().any(|tier| {
                        tier.get("reasonCode").and_then(Value::as_str)
                            == Some("GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT")
                    })
                })
    }
}

// ---- provider ---------------------------------------------------------------

/// The Antigravity usage provider: a local-process probe and a cloud lane.
pub struct AntigravityProvider {
    /// Loopback-only client: cert validation disabled because the editor's local
    /// server uses a self-signed cert. NEVER used for a non-loopback URL (guarded).
    http: reqwest::Client,
    /// Ordinary client for the cloud lane.
    ///
    /// Deliberately separate from `http`: that one accepts any certificate, which
    /// is only defensible against a server on this machine. Sharing it with a
    /// public endpoint would silently extend that exemption across the network.
    remote_http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    quota_url: String,
    quota_summary_url: String,
    load_code_assist_url: String,
    /// The last local probe and when it was taken. A failed probe is cached as
    /// its reason, for the same two seconds, so every handle in one tick gets the
    /// same answer.
    ///
    /// An async lock held ACROSS discovery, not just around the read and write:
    /// handles fetched concurrently must wait for one discovery and share it. The
    /// process scan admits one child at a time, so a second handle racing the
    /// first would otherwise be refused as "still running" and report a
    /// could-not-look error during an ordinary, healthy tick.
    local_cache: tokio::sync::Mutex<Option<(std::time::Instant, Result<LocalProbe, String>)>>,
    /// macOS uses bounded `ps`; other platforms enumerate natively. Tests can
    /// substitute a command to simulate a stalled process table.
    process_scan: Option<BoundedCommand>,
    /// Which lane last answered for each handle, so a SWITCH can be logged.
    ///
    /// Three lanes stamp the same `source` on the wire (the local editor probe,
    /// the editor plugin's on-disk cache, and the cloud API), which is correct --
    /// `source` is per-lane-kind and consumers are told not to key on it flipping
    /// -- but it leaves no way to ask WHICH ONE ANSWERED. That cost real time:
    /// diagnosing a frozen reading took ten turns of elimination across three
    /// lanes and ended without an answer, because every theory fitted the
    /// observations equally well.
    ///
    /// A system with several indistinguishable producers cannot be diagnosed by
    /// watching its output, however carefully. THE INSTRUMENT HAS TO NAME THE
    /// PRODUCER.
    served_lane: std::sync::Mutex<std::collections::HashMap<String, &'static str>>,
    local_endpoints: Option<Vec<(LocalServer, u16)>>,
    override_store: Option<Result<Vec<StoredAccount>, String>>,
    env_lookup: EnvLookup,
}

impl AntigravityProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::new(None)))
    }

    pub fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap_or_else(|_| crate::http::provider_client());
        Self {
            http,
            remote_http: crate::http::provider_client(),
            credential_source,
            handle_loader,
            quota_url: REMOTE_QUOTA_URL.to_string(),
            quota_summary_url: REMOTE_QUOTA_SUMMARY_URL.to_string(),
            load_code_assist_url: LOAD_CODE_ASSIST_URL.to_string(),
            local_cache: tokio::sync::Mutex::new(None),
            process_scan: process_scan(),
            served_lane: std::sync::Mutex::new(std::collections::HashMap::new()),
            local_endpoints: None,
            override_store: None,
            env_lookup: default_sidebar_env(),
        }
    }

    #[doc(hidden)]
    pub fn set_local_endpoints(&mut self, endpoints: Vec<(LocalServer, u16)>) {
        self.local_endpoints = Some(endpoints);
        *self.local_cache.get_mut() = None;
    }

    #[doc(hidden)]
    pub fn set_override_accounts(&mut self, accounts: Vec<StoredAccount>) {
        self.override_store = Some(Ok(accounts));
    }

    #[doc(hidden)]
    pub fn set_override_store_unreadable(&mut self, reason: &str) {
        self.override_store = Some(Err(reason.to_string()));
    }

    fn stored_accounts(&self) -> Result<Vec<StoredAccount>, String> {
        if let Some(accounts) = &self.override_store {
            return accounts.clone();
        }
        read_sidebar(&self.env_lookup)
    }

    /// Log source switches, not every poll. Different sources use the same
    /// output `source` name, so this diagnostic explains sudden changes in usage
    /// or observation time without logging every unchanged account.
    fn note_lane(&self, key: &str, lane: &'static str) {
        let Ok(mut seen) = self.served_lane.lock() else {
            return;
        };
        match seen.insert(key.to_string(), lane) {
            Some(previous) if previous != lane => {
                eprintln!(
                    "{tag} antigravity lane switch for {key}: {previous} -> {lane} \
                     (a reading whose value time steps backwards across this is the switch, \
                     not a fault)",
                    tag = crate::LOG_TAG
                );
            }
            _ => {}
        }
    }

    async fn fetch_plugin_account(&self, handle_name: &str) -> FetchAttempt {
        let accounts = match self.stored_accounts() {
            Ok(accounts) => accounts,
            // An unreadable sidebar is reported as LocalSourceUnavailable, which is
            // transient: the refresher keeps serving this account's last reading
            // and marks it stale. Treating it as "no longer listed" would publish
            // the account as signed out.
            Err(reason) => {
                return FetchAttempt::failure(
                    None,
                    Some(PLUGIN_SOURCE.to_string()),
                    FetchError::LocalSourceUnavailable(reason),
                );
            }
        };
        let found = accounts
            .iter()
            .find(|account| account_handle_name(account).as_deref() == Some(handle_name));
        let Some(account) = found else {
            // The account was removed or disabled between enumeration and fetch.
            // Absent rather than broken: nothing here is wrong, the user simply
            // signed it out.
            return FetchAttempt::failure(
                None,
                Some(PLUGIN_SOURCE.to_string()),
                FetchError::NoSession(
                    "the opencode antigravity plugin no longer lists this account".to_string(),
                ),
            );
        };

        let observed = account
            .email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(|email| AccountObservation::new(Some(email.to_string()), None));

        // Email is both the stable identity and the account's display metadata.
        let account_info = account
            .email
            .as_deref()
            .map(str::trim)
            .filter(|email| !email.is_empty())
            .map(|email| crate::model::AccountInfo {
                email: Some(email.to_string()),
                org_name: None,
                plan_type: None,
                subscription_renews_at: None,
                subscription_ends_at: None,
            });

        // Prefer the live editor after its GetUserStatus response proves the
        // session email matches this sidebar account. Otherwise an editor signed
        // into another account could publish its quota under the wrong handle.
        if let Some(expected_email) = account.email.as_deref() {
            // A probe that could not look falls through exactly like one that
            // found no matching session: either way the local lane has nothing to
            // serve, and the lanes below decide what this account publishes.
            let snapshots = self
                .probe_local_snapshots()
                .await
                .map(|probe| probe.snapshots)
                .unwrap_or_default();
            if let Some(matching) = snapshots
                .into_iter()
                .find(|s| emails_match(Some(expected_email), s.email.as_deref()))
            {
                self.note_lane(handle_name, "local-probe");
                return FetchAttempt::success(observed, PLUGIN_SOURCE, matching.usage)
                    .with_account_info(account_info);
            }
        }

        // The plugin owns quota polling. This lane reads only the sidebar's cached
        // quota data and never tries a token exchange or cloud fallback.
        if let Some((usage, checked_at)) = parse_cached_quota(&account.quota, Utc::now()) {
            self.note_lane(handle_name, "plugin-cache");
            return FetchAttempt::success(observed, PLUGIN_SOURCE, usage)
                .with_value_observed_at(checked_at)
                .with_account_info(account_info);
        }
        FetchAttempt::failure(
            observed,
            Some(PLUGIN_SOURCE.to_string()),
            FetchError::LocalSourceUnavailable(
                "no dated, recent Antigravity sidebar quota is available for this account"
                    .to_string(),
            ),
        )
        .with_account_info(account_info)
    }

    /// Call the cloud quota endpoint with an already-resolved access token.
    ///
    /// Only the vault supplies cloud credentials; the sidebar lane is cache-only.
    async fn fetch_remote_quota(
        &self,
        access_token: &str,
        project: Option<&str>,
    ) -> Result<Usage, FetchError> {
        if project.is_none() {
            // Google can return a successful placeholder quota for the wrong OAuth
            // client. Only an explicit consumer-tier refusal withholds it; a failed
            // preflight leaves the existing quota lane usable, even on HTTP 401.
            // This request shares the quota calls' bearer, Hub identity and timeout.
            let body = serde_json::to_vec(&serde_json::json!({
                "metadata": {
                    "ideType": "ANTIGRAVITY",
                    "platform": "PLATFORM_UNSPECIFIED",
                    "pluginType": "GEMINI"
                }
            }))
            .map_err(|e| FetchError::Decode(e.to_string()))?;
            if let Ok(response) = self
                .post_quota(&self.load_code_assist_url, body, access_token)
                .await
            {
                if let Ok(code_assist) =
                    serde_json::from_slice::<LoadCodeAssistResponse>(&response.body)
                {
                    // CodexBar AntigravityRemoteUsageFetcher.swift, 1c70e8966:
                    // no stored project, no current tier, no response project,
                    // and GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT mean sign in again.
                    if code_assist.rejects_client_for_consumer_tier() {
                        // Unauthorized publishes credential_rejected without claiming
                        // an HTTP 401, so this semantic refusal is not reported to the vault.
                        return Err(FetchError::Unauthorized(
                            "Google rejected this login's OAuth client for the account's tier; sign in again to restore quota access".to_string(),
                        ));
                    }
                }
            }
        }

        // The project scopes the query where one is known. The endpoint also
        // answers without it, so an absent project is not a failure.
        let body = match project {
            Some(project) => serde_json::json!({ "project": project }),
            None => serde_json::json!({}),
        };
        let body = serde_json::to_vec(&body).map_err(|e| FetchError::Decode(e.to_string()))?;

        // Summary first, per-model only if it refuses. See the constants above
        // for why these are not interchangeable.
        //
        // NOT DEFENDED BY A UNIT TEST, and mutation-checked to confirm that:
        // swapping this back to `quota_url` reddens nothing, because which URL is
        // dialled is observable only against a live server. The check that would
        // catch a revert is the post-deploy wire read -- antigravity carrying a
        // `windowMinutes` pair (300 and 10080) rather than a single null. If that
        // pair is ever absent again, this line is the first place to look.
        match self
            .post_quota(&self.quota_summary_url, body.clone(), access_token)
            .await
        {
            Ok(response) => return parse_quota_summary(&String::from_utf8_lossy(&response.body)),
            Err(error) if summary_unavailable(&error) => {
                // Fall through. Only a REFUSAL falls back: a timeout or a 5xx is
                // a transient condition on the good endpoint, and degrading to a
                // lane that silently drops a window would turn a retryable blip
                // into a wrong number that looks fine.
            }
            Err(error) => return Err(error),
        }

        let response = self.post_quota(&self.quota_url, body, access_token).await?;
        parse_remote_quota(&response.body)
    }

    async fn post_quota(
        &self,
        url: &str,
        body: Vec<u8>,
        access_token: &str,
    ) -> Result<crate::http::HttpResponse, FetchError> {
        JsonRequest::post_json(url, body)
            .bearer(access_token)
            // This User-Agent identifies the client making the Cloud Code quota request.
            .header(remote_user_agent_header())
            .timeout(REQUEST_TIMEOUT)
            .send_provider_status_first(&self.remote_http, PROVIDER_NAME)
            .await
    }

    /// Fetch quota from the cloud, needing no local process.
    ///
    /// The credential is Antigravity's own Google login, served by the vault.
    /// This is the same Code Assist endpoint the Gemini provider calls, and the
    /// account behind the token is what makes the answers differ: an Antigravity
    /// login's quota covers Antigravity's model pool, Claude and GPT included.
    async fn fetch_remote(&self, handle: &CredentialHandle) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(VaultGetError::Permanent);
        };
        let mut credential = match crate::credential_source::get_vault_credential(
            credential_source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = credential.record_version;
        let account_info = credential.account_info();
        let observed_email = credential
            .email
            .as_deref()
            .or(credential.account_id.as_deref())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let observed = Some(AccountObservation::new(
            credential
                .account_id
                .clone()
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty()),
            Some(record_version),
        ));

        // KEYED ON THE EMAIL, NEVER THE CAPABILITY. The capability is the bearer of
        // vault authority and must not reach a log line; the email is already
        // published as `accountInfo` on this entry, so it adds no exposure.
        let lane_key = observed_email
            .clone()
            .unwrap_or_else(|| "vault-unidentified".to_string());

        // 1. Probe the local lane first if this vault handle's identity is known.
        // A local editor outranks the cloud credential because the cloud credential is
        // entitled to a different tier. If the local session matches this vault handle's
        // identity, serve local usage.
        if let Some(expected_email) = &observed_email {
            // As in the plugin lane: could-not-look falls through to the lanes
            // below, the same as no matching session.
            let snapshots = self
                .probe_local_snapshots()
                .await
                .map(|probe| probe.snapshots)
                .unwrap_or_default();
            if let Some(matching) = snapshots
                .into_iter()
                .find(|s| emails_match(Some(expected_email), s.email.as_deref()))
            {
                self.note_lane(&lane_key, "local-probe");
                return FetchAttempt::success(observed, "vault", matching.usage)
                    .with_account_info(account_info);
            }
        }

        // Without the sidebar, this lane can't tell whether the account is paid,
        // and for a paid account the cloud reading below is a different pool.
        // So it keeps the last reading instead of guessing.
        let accounts = match self.stored_accounts() {
            Ok(accounts) => accounts,
            Err(reason) => {
                return FetchAttempt::failure(
                    observed,
                    Some("vault".to_string()),
                    FetchError::LocalSourceUnavailable(reason),
                )
                .with_account_info(account_info);
            }
        };
        let plugin_account = stored_account_for_email(&accounts, observed_email.as_deref());

        // The account-local plugin cache outranks this live network call because
        // the token below is entitled to the standard tier, while the cache was
        // read from the pool the signed-in account actually spends.
        if let Some((usage, updated_at)) = fresh_paid_cached_quota(plugin_account, Utc::now()) {
            self.note_lane(&lane_key, "plugin-cache");
            return FetchAttempt::success(observed, "vault", usage)
                .with_value_observed_at(updated_at)
                .with_account_info(account_info);
        }

        // A paid account's cloud credential reaches a different, standard-tier
        // pool. The sidebar identifies paid accounts by email; if this credential
        // has no matching identity while a paid account is known, it could be
        // that account and must also be withheld. No paid evidence means serve.
        if cloud_lane_must_withhold(&accounts, plugin_account) {
            return FetchAttempt::failure(
                observed,
                Some("vault".to_string()),
                paid_tier_usage_unavailable(),
            )
            .with_account_info(account_info);
        }

        let project = credential
            .project_id
            .clone()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        let access_token =
            match crate::credential_source::take_utf8_payload(&mut credential.payload) {
                Ok(value) => value,
                Err(error) => return FetchAttempt::failure(observed, None, error),
            };

        let result: Result<Usage, FetchError> = async {
            // The project scopes the query where the vault knows one. The endpoint
            // also answers without it, so an absent project is not a failure.
            self.fetch_remote_quota(&access_token, project.as_deref())
                .await
        }
        .await;

        if let Err(error) = &result {
            crate::credential_source::report_vault_auth_failure(
                self.credential_source.as_ref(),
                handle,
                record_version,
                error,
            );
        }

        match result {
            Ok(usage) => {
                self.note_lane(&lane_key, "cloud-api");
                FetchAttempt::success(observed, "vault", usage).with_account_info(account_info)
            }
            Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error),
        }
    }

    /// Query GetUserStatus on the loopback server for the signed-in account email.
    async fn probe_user_status(
        &self,
        server: &LocalServer,
        scheme: &str,
        port: u16,
    ) -> Option<String> {
        let url = format!("{scheme}://127.0.0.1:{port}{USER_STATUS_PATH}");
        if !is_loopback_url(&url) {
            return None;
        }
        let mut req = JsonRequest::post_json(url, b"{}".to_vec())
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Content-Type", "application/json"))
            .header(Header::new("Connect-Protocol-Version", "1"));
        if !server.csrf_token.is_empty() {
            req = req.header(Header::new(
                "X-Codeium-Csrf-Token",
                server.csrf_token.clone(),
            ));
        }
        let response = req.send(&self.http).await.ok()?;
        parse_user_status_email(&String::from_utf8_lossy(&response))
    }

    /// POST the quota-summary RPC to one discovered server/port. Returns the parsed
    /// usage and the signed-in account email (if available), or an error to try the
    /// next candidate. The local server may speak http (the `agy` CLI, confirmed on the wire)
    /// or https-with-self-signed (the app language server); try both loopback schemes.
    async fn probe(
        &self,
        server: &LocalServer,
        port: u16,
    ) -> Result<(Usage, Option<String>), FetchError> {
        let mut last_err =
            FetchError::Upstream(format!("no loopback scheme served quota on port {port}"));
        for scheme in ["http", "https"] {
            let url = format!("{scheme}://127.0.0.1:{port}{QUOTA_SUMMARY_PATH}");
            // Containment guard: this client disables cert validation, so it must
            // only ever talk to loopback. Refuse anything else.
            if !is_loopback_url(&url) {
                return Err(FetchError::Upstream(
                    "refusing non-loopback URL".to_string(),
                ));
            }

            let mut req = JsonRequest::post_json(url, b"{\"forceRefresh\":true}".to_vec())
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Content-Type", "application/json"))
                .header(Header::new("Connect-Protocol-Version", "1"));
            if !server.csrf_token.is_empty() {
                req = req.header(Header::new(
                    "X-Codeium-Csrf-Token",
                    server.csrf_token.clone(),
                ));
            }

            match req.send(&self.http).await {
                Ok(body) => {
                    let usage = parse_quota_summary(&String::from_utf8_lossy(&body))?;
                    let email = self.probe_user_status(server, scheme, port).await;
                    return Ok((usage, email));
                }
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Discover and probe running Antigravity servers on loopback, caching the
    /// snapshot for 2 seconds to share discovery across concurrent handle fetches within a tick.
    ///
    /// `Err` means discovery could not look (a scan timed out, was skipped because
    /// the previous one is still stuck, or could not start). It is never folded
    /// into an empty probe, which would read as "no Antigravity running".
    async fn probe_local_snapshots(&self) -> Result<LocalProbe, FetchError> {
        let mut cache = self.local_cache.lock().await;
        if let Some((taken_at, probe)) = cache.as_ref() {
            if taken_at.elapsed() < Duration::from_secs(2) {
                return probe.clone().map_err(FetchError::LocalSourceUnavailable);
            }
        }

        let probe = self.probe_local_uncached().await;
        // Stamped when discovery FINISHES, so handles that queued behind a slow
        // discovery reuse its answer instead of starting their own.
        *cache = Some((
            std::time::Instant::now(),
            probe.as_ref().cloned().map_err(|error| match error {
                FetchError::LocalSourceUnavailable(reason) => reason.clone(),
                other => other.to_string(),
            }),
        ));
        probe
    }

    async fn probe_local_uncached(&self) -> Result<LocalProbe, FetchError> {
        let (candidates, servers_found, port_error) = if let Some(endpoints) = &self.local_endpoints
        {
            (endpoints.clone(), !endpoints.is_empty(), None)
        } else {
            let servers = discover_servers(&self.process_scan).await?;
            let mut list = Vec::new();
            let mut port_error = None;
            for server in &servers {
                match discover_ports(server.pid).await {
                    Ok(ports) => {
                        for port in ports {
                            list.push((server.clone(), port));
                        }
                    }
                    // Kept rather than returned at once: another server may
                    // still answer, and an answer beats the error.
                    Err(error) => {
                        port_error.get_or_insert(error);
                    }
                }
            }
            (list, !servers.is_empty(), port_error)
        };

        let mut snapshots = Vec::new();
        for (server, port) in &candidates {
            if let Ok((usage, email)) = self.probe(server, *port).await {
                snapshots.push(LocalSnapshot { usage, email });
            }
        }

        if snapshots.is_empty() {
            if let Some(error) = port_error {
                return Err(error);
            }
        }
        Ok(LocalProbe {
            snapshots,
            servers_found,
        })
    }
}

/// Whether a URL really addresses this machine, for a client that has certificate
/// validation switched off.
///
/// The host is read from the parsed URL rather than from the start of the string.
/// Userinfo precedes the host in a URL, so `http://localhost:8080@example.test/`
/// begins with a loopback-looking prefix while actually addressing
/// `example.test` -- a string test says yes and the request leaves the machine.
///
/// Userinfo is refused outright rather than merely ignored: nothing here builds
/// a URL containing any, so its presence means the input did not come from where
/// this function's caller assumes, and that is worth refusing rather than
/// parsing around.
fn is_loopback_url(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    match parsed.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

impl Default for AntigravityProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for AntigravityProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    /// One handle per available lane.
    ///
    /// The local probe is always offered: it needs no credential, and when the
    /// editor is running it is the cheaper and more detailed answer. The cloud
    /// lane is offered whenever a vault credential exists, and is what keeps this
    /// provider reporting with nothing open.
    ///
    /// A credentialed lane REPLACES the local probe rather than joining it.
    ///
    /// The local probe can never resolve an account identity -- it asks a
    /// running editor what its quota is, and the answer names no account. The
    /// read path emits labeled entries only when EVERY handle resolves one, so
    /// keeping the probe beside identity-bearing lanes collapses the whole
    /// provider to a single unlabeled entry and discards the identity the other
    /// lanes did resolve.
    ///
    /// That is not theoretical here: it shipped that way, and the wire showed
    /// `account: null` for an account whose email the plugin lane had read
    /// correctly. Multiple logged-in accounts make it worse rather than
    /// differently wrong -- four accounts would publish as one unlabeled entry
    /// describing whichever the editor happened to be using.
    ///
    /// The probe remains the whole answer when nothing else is configured,
    /// which is the case it was built for. What is given up when a credential
    /// exists is its richer per-model detail and its offline reach; the
    /// refresher already serves the last healthy window through a cloud outage,
    /// so the exposure is a stale window rather than a blank provider.
    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        let mut credentialed = Vec::new();
        if self.credential_source.is_some() {
            credentialed.extend(self.handle_loader.antigravity_handles()?);
        }
        // Enumerate plugin accounts by genuine email. A sidebar read failure
        // fails the whole enumeration: returning only vault handles would omit
        // plugin accounts and make the refresher interpret them as signed out.
        let accounts = self
            .stored_accounts()
            .map_err(crate::provider::HandlesError::new)?;
        for account in &accounts {
            if let Some(name) = account_handle_name(account) {
                credentialed.push(CredentialHandle::new(name));
            }
        }

        if credentialed.is_empty() {
            return Ok(vec![CredentialHandle::implicit()]);
        }
        Ok(credentialed)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if let CredentialHandle::Named(name) = handle {
            return self.fetch_plugin_account(name).await;
        }
        if handle.is_vault() {
            return self.fetch_remote(handle).await;
        }

        let result: Result<ProviderUsage, FetchError> = async {
            let probe = self.probe_local_snapshots().await?;
            if let Some(snapshot) = probe.snapshots.into_iter().next() {
                return Ok(ProviderUsage::healthy(
                    PROVIDER_NAME,
                    None,
                    "oauth",
                    snapshot.usage,
                ));
            }

            // Decided from the scan the probe already ran. Scanning again here
            // only to pick a message would double the `ps` children per fetch,
            // and on a stalled process table every extra one is a stuck child.
            if !probe.servers_found {
                return Err(FetchError::LocalSourceUnavailable(
                    "no Antigravity language server or agy CLI process running".to_string(),
                ));
            }
            Err(FetchError::LocalSourceUnavailable(
                "no Antigravity loopback port served quota".to_string(),
            ))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn native_discovery_uses_one_matcher_and_owner_port_selection() {
        for command in [
            "/opt/Antigravity/language_server --csrf_token=editor-token",
            "C:\\Program Files\\Antigravity\\language_server.exe --csrf_token=editor-token",
        ] {
            assert_eq!(classify_command(command).as_deref(), Some("editor-token"));
        }
        for command in [
            "/usr/bin/agy serve",
            "C:\\bin\\agy.exe serve",
            "\"C:\\Program Files\\agy.exe\" serve",
        ] {
            assert_eq!(classify_command(command).as_deref(), Some(""));
        }
        assert_eq!(classify_command("/opt/Antigravity/language_server"), None);
        assert_eq!(
            classify_command("/opt/other/language_server --csrf_token=other"),
            None
        );
        assert_eq!(
            command_requires_csrf("C:\\OtherEditor\\language_server.exe"),
            None
        );
        assert_eq!(
            command_requires_csrf("C:\\Antigravity\\language_server.exe"),
            Some(true)
        );
        assert_eq!(
            select_ports(42, [(7, 8000), (42, 9000), (42, 9000), (42, 9001)]),
            vec![9000, 9001]
        );
    }

    #[test]
    fn windows_tcp_owner_fixtures_select_pid_and_network_order_ports() {
        // Literal DWORD rows from the documented layouts, independent of the decoder.
        let ipv4 = [
            2, 0, 0, 0, // Count.
            2, 0, 0, 0, 127, 0, 0, 1, 0x23, 0x28, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 42, 0, 0, 0, 2, 0,
            0, 0, 127, 0, 0, 1, 0x23, 0x29, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0,
        ];
        assert_eq!(
            select_ports(42, parse_tcp_owner_table(&ipv4, false).unwrap()),
            vec![9000]
        );
        let ipv6 = [
            1, 0, 0, 0, // Count.
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, // Local address.
            0, 0, 0, 0, 0x23, 0x29, 0, 0, // Scope and port.
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // Remote address.
            0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 42, 0, 0, 0, // Scope, port, state, pid.
        ];
        assert_eq!(
            select_ports(42, parse_tcp_owner_table(&ipv6, true).unwrap()),
            vec![9001]
        );
        assert!(parse_tcp_owner_table(&ipv4[..ipv4.len() - 1], false).is_err());
        assert!(parse_tcp_owner_table(&[255, 255, 255, 255], true).is_err());
        let mut connected = ipv4;
        connected[4] = 5;
        assert_eq!(
            select_ports(42, parse_tcp_owner_table(&connected, false).unwrap()),
            Vec::<u16>::new()
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_real_enumerator_reads_own_command_line_and_listener() {
        static PROCESSES: Gate = Gate::new("Windows own process control");
        static PORTS: Gate = Gate::new("Windows own listener control");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let pid = std::process::id();
        let processes = PROCESSES
            .run_blocking(PROCESS_SCAN_TIMEOUT, move |deadline| {
                windows_discovery::processes(deadline, Some(pid))
            })
            .await
            .unwrap();
        let command = &processes
            .iter()
            .find(|(owner, _)| *owner == pid as i32)
            .expect("Toolhelp must see the test process")
            .1;
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_string_lossy();
        assert!(
            command.contains(name.as_ref()),
            "PEB read must include the test executable name"
        );
        let ports = PORTS
            .run_blocking(PORT_SCAN_TIMEOUT, move |deadline| {
                native_ports(pid as i32, deadline)
            })
            .await
            .unwrap();
        assert!(
            ports.contains(&listener.local_addr().unwrap().port()),
            "TCP owner table must include the listener opened by this process"
        );
    }

    #[test]
    fn proc_cmdline_and_tcp_fixtures_preserve_arguments_and_listen_inodes() {
        let command =
            parse_proc_cmdline(b"/opt/Antigravity/language_server\0--csrf_token\0fixture-token\0");
        assert_eq!(classify_command(&command).as_deref(), Some("fixture-token"));
        assert_eq!(parse_proc_cmdline(b""), "");
        assert_eq!(
            parse_proc_cmdline(b"/usr/bin/agy\0serve\0"),
            "/usr/bin/agy serve"
        );
        let rows = " sl local_address rem_address st tx_queue rx tr tm retr uid timeout inode\n\
            0: 0100007F:2328 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 123\n\
            1: 00000000000000000000000001000000:2329 00000000000000000000000000000000:0000 0A 0:0 00:0 0 1000 0 124\n\
            2: 0100007F:232A 0100007F:232B 01 0:0 00:0 0 1000 0 125\n\
            3: 0100007F:ZZZZ 00000000:0000 0A 0:0 00:0 0 1000 0 126\n\
            malformed\n";
        assert_eq!(parse_proc_tcp(rows), vec![(123, 9000), (124, 9001)]);
    }

    #[test]
    fn proc_fixtures_enumerate_server_and_owned_listener() {
        let processes = enumerate_proc_cmdlines([(
            42,
            b"/opt/Antigravity/language_server\0--csrf_token=fixture-token\0".to_vec(),
        )]);
        assert_eq!(
            processes.len(),
            1,
            "procfs command-line enumeration must not silently return empty"
        );
        assert_eq!(processes[0].0, 42);
        assert_eq!(
            classify_command(&processes[0].1).as_deref(),
            Some("fixture-token")
        );
        let inodes: HashSet<_> = ["socket:[123]", "socket:[124]", "pipe:[999]", "not a socket"]
            .into_iter()
            .filter_map(proc_socket_inode)
            .collect();
        assert_eq!(inodes, HashSet::from([123, 124]));
        let tcp = "0: 0100007F:2328 00000000:0000 0A 0:0 00:0 0 1000 0 123\n1: 0100007F:232A 00000000:0000 0A 0:0 00:0 0 1000 0 999\n";
        let tcp6 = "0: 00000000000000000000000001000000:2329 00000000000000000000000000000000:0000 0A 0:0 00:0 0 1000 0 124\n";
        let rows = parse_proc_tcp(tcp).into_iter().chain(parse_proc_tcp(tcp6));
        assert_eq!(select_proc_ports(42, &inodes, rows), vec![9000, 9001]);
    }

    #[cfg(target_os = "linux")]
    struct ProcFixture(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl ProcFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("insula-proc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("42/fd")).unwrap();
            std::fs::create_dir_all(root.join("net")).unwrap();
            Self(root)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ProcFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_enumerator_discovers_fixture_server_and_its_listener() {
        use std::os::unix::fs::symlink;
        let proc = ProcFixture::new();
        std::fs::write(
            proc.0.join("42/cmdline"),
            b"/opt/Antigravity/language_server\0--csrf_token=fixture-token\0",
        )
        .unwrap();
        std::fs::create_dir(proc.0.join("43")).unwrap(); // Unreadable/disappeared cmdline is skipped.
        symlink("socket:[123]", proc.0.join("42/fd/3")).unwrap();
        symlink("socket:[124]", proc.0.join("42/fd/4")).unwrap();
        std::fs::write(proc.0.join("net/tcp"), "0: 0100007F:2328 00000000:0000 0A 0:0 00:0 0 1000 0 123\n1: 0100007F:232A 00000000:0000 0A 0:0 00:0 0 1000 0 999\n").unwrap();
        std::fs::write(proc.0.join("net/tcp6"), "0: 00000000000000000000000001000000:2329 00000000000000000000000000000000:0000 0A 0:0 00:0 0 1000 0 124\n").unwrap();
        let deadline = std::time::Instant::now() + PROCESS_SCAN_TIMEOUT;
        let processes = linux_processes(&proc.0, deadline).unwrap();
        assert_eq!(
            processes.len(),
            1,
            "Linux enumeration must not silently return empty"
        );
        assert_eq!(processes[0].0, 42);
        assert_eq!(
            classify_command(&processes[0].1).as_deref(),
            Some("fixture-token")
        );
        assert_eq!(
            linux_ports(&proc.0, 42, deadline).unwrap(),
            vec![9000, 9001]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_unreadable_procfs_is_could_not_look_and_deadlines_are_enforced() {
        static GATE: Gate = Gate::new("unreadable procfs fixture");
        let proc = ProcFixture::new();
        let root = proc.0.clone();
        let error = GATE
            .run_blocking(PROCESS_SCAN_TIMEOUT, move |deadline| {
                linux_processes(&root, deadline)
            })
            .await
            .unwrap_err();
        assert!(
            matches!(could_not_look("list processes", &error), FetchError::LocalSourceUnavailable(reason) if reason.contains("could not list processes"))
        );
        assert!(linux_processes(&proc.0, std::time::Instant::now()).is_err());
        assert!(linux_ports(&proc.0, 42, std::time::Instant::now()).is_err());
        let deadline = std::time::Instant::now() + PROCESS_SCAN_TIMEOUT;
        assert!(linux_processes(&proc.0.join("missing"), deadline).is_err());
        assert!(linux_ports(&proc.0, 42, deadline).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_real_enumerator_sees_own_pid_and_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let pid = std::process::id() as i32;
        let deadline = std::time::Instant::now() + PROCESS_SCAN_TIMEOUT;
        assert!(
            native_processes(deadline)
                .unwrap()
                .iter()
                .any(|(owner, _)| *owner == pid),
            "real procfs scan must see current test pid"
        );
        assert!(native_ports(pid, deadline)
            .unwrap()
            .contains(&listener.local_addr().unwrap().port()));
    }

    // From CodexBar Tests/CodexBarTests/AntigravityRemoteUsageFetcherTests.swift,
    // "remote fetch asks for sign in again when the oauth client cannot read consumer quota",
    // commit 837d30dff34ba4bd7a7b99abd120c37e2ae44c9f.
    fn consumer_client_refusal() -> Value {
        serde_json::json!({
            "allowedTiers": [{
                "id": "standard-tier",
                "isDefault": true,
                "userDefinedCloudaicompanionProject": true
            }],
            "ineligibleTiers": [{
                "reasonCode": "GOOGLE_TOS_NOT_SUPPORTED_BY_CLIENT",
                "reasonMessage": "Client does not support Google TOS.",
                "tierId": "free-tier"
            }]
        })
    }

    struct CloudPreflightMock {
        provider: AntigravityProvider,
        preflight_requests: Arc<AtomicUsize>,
        quota_requests: Arc<AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for CloudPreflightMock {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn cloud_preflight_mock(status: u16, body: Vec<u8>) -> CloudPreflightMock {
        let preflight_requests = Arc::new(AtomicUsize::new(0));
        let quota_requests = Arc::new(AtomicUsize::new(0));
        let preflights = Arc::clone(&preflight_requests);
        let quotas = Arc::clone(&quota_requests);
        let (port, server) =
            super::plugin_lane_tests::spawn_mock_server_with_request(move |request| {
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let (headers, request_body) = request.split_once("\r\n\r\n").unwrap();
                let headers = headers.to_ascii_lowercase();
                assert!(headers
                    .lines()
                    .any(|line| line == "authorization: bearer mock-token"));
                assert!(headers.lines().any(|line| line
                    == format!("user-agent: {}", remote_user_agent().to_ascii_lowercase())));
                assert!(headers
                    .lines()
                    .any(|line| line == "content-type: application/json"));
                let request_body: Value = serde_json::from_str(request_body).unwrap();
                match path {
                    "/load" => {
                        preflights.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(
                            request_body,
                            serde_json::json!({
                                "metadata": {
                                    "ideType": "ANTIGRAVITY",
                                    "platform": "PLATFORM_UNSPECIFIED",
                                    "pluginType": "GEMINI"
                                }
                            })
                        );
                        (status, body.clone())
                    }
                    "/summary" | "/quota" => {
                        quotas.fetch_add(1, Ordering::Relaxed);
                        (
                            200,
                            super::plugin_lane_tests::CLOUD_MOCK_QUOTA
                                .as_bytes()
                                .to_vec(),
                        )
                    }
                    "/quota-unauthorized" => {
                        quotas.fetch_add(1, Ordering::Relaxed);
                        (401, b"unauthorized".to_vec())
                    }
                    _ => panic!("unexpected request: {path}"),
                }
            })
            .await;
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(Vec::new());
        provider.load_code_assist_url = format!("http://127.0.0.1:{port}/load");
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/summary");
        provider.quota_url = format!("http://127.0.0.1:{port}/quota");
        CloudPreflightMock {
            provider,
            preflight_requests,
            quota_requests,
            server,
        }
    }

    async fn assert_preflight_serves_quota(status: u16, body: Vec<u8>) {
        let mock = cloud_preflight_mock(status, body).await;
        let usage = mock
            .provider
            .fetch_remote_quota("mock-token", None)
            .await
            .expect("a preflight without an affirmative consumer-client refusal must serve quota");
        assert_eq!(pool_window(&usage, "gemini-weekly").used_percent, 7.15);
        assert_eq!(mock.preflight_requests.load(Ordering::Relaxed), 1);
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn consumer_client_refusal_rejects_credentials_without_requesting_quota() {
        let mut mock =
            cloud_preflight_mock(200, consumer_client_refusal().to_string().into_bytes()).await;
        let reports = Arc::new(AtomicUsize::new(0));
        let handle = attach_vault(&mut mock.provider, None, Arc::clone(&reports));
        let error = mock
            .provider
            .fetch_handle(&handle)
            .await
            .usage
            .expect_err("consumer-client refusal must withhold placeholder quota");
        assert_eq!(error.error_class(), "credential_rejected");
        assert!(matches!(error, FetchError::Unauthorized(_)));
        assert!(error.to_string().contains("OAuth client"));
        assert!(error.to_string().contains("sign in again"));
        assert_eq!(mock.preflight_requests.load(Ordering::Relaxed), 1);
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 0);
        tokio::task::yield_now().await;
        assert_eq!(
            reports.load(Ordering::Relaxed),
            0,
            "a semantic refusal is not an HTTP 401"
        );
    }

    #[tokio::test]
    async fn consumer_client_refusal_with_a_current_tier_still_requests_quota() {
        let mut body = consumer_client_refusal();
        body["currentTier"] = serde_json::json!({"id": "free-tier"});
        assert_preflight_serves_quota(200, body.to_string().into_bytes()).await;
    }

    #[tokio::test]
    async fn consumer_client_refusal_with_a_response_project_still_requests_quota() {
        for project in [
            serde_json::json!("existing-project"),
            serde_json::json!({"id": "existing-project"}),
            serde_json::json!({"projectId": "existing-project"}),
        ] {
            let mut body = consumer_client_refusal();
            body["cloudaicompanionProject"] = project;
            assert_preflight_serves_quota(200, body.to_string().into_bytes()).await;
        }
    }

    #[tokio::test]
    async fn a_different_ineligible_reason_still_requests_quota() {
        let mut body = consumer_client_refusal();
        body["ineligibleTiers"][0]["reasonCode"] = serde_json::json!("OTHER_REASON");
        assert_preflight_serves_quota(200, body.to_string().into_bytes()).await;
    }

    #[tokio::test]
    async fn absent_ineligible_tiers_preserve_quota_fetching() {
        let mut body = consumer_client_refusal();
        body.as_object_mut().unwrap().remove("ineligibleTiers");
        assert_preflight_serves_quota(200, body.to_string().into_bytes()).await;
    }

    #[tokio::test]
    async fn malformed_ineligible_tiers_preserve_quota_fetching() {
        for inventory in [
            Value::Null,
            serde_json::json!("not an array"),
            serde_json::json!({}),
            serde_json::json!([null, false, {"reasonCode": 123}, {}]),
        ] {
            let mut body = consumer_client_refusal();
            body["ineligibleTiers"] = inventory;
            assert_preflight_serves_quota(200, body.to_string().into_bytes()).await;
        }
    }

    #[tokio::test]
    async fn a_preflight_http_500_still_requests_quota() {
        assert_preflight_serves_quota(500, consumer_client_refusal().to_string().into_bytes())
            .await;
    }

    #[tokio::test]
    async fn an_unparseable_preflight_still_requests_quota() {
        assert_preflight_serves_quota(200, b"not JSON".to_vec()).await;
    }

    #[tokio::test]
    async fn a_preflight_transport_failure_still_requests_quota() {
        let mut mock =
            cloud_preflight_mock(200, consumer_client_refusal().to_string().into_bytes()).await;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        mock.provider.load_code_assist_url = format!(
            "http://127.0.0.1:{}/load",
            listener.local_addr().unwrap().port()
        );
        drop(listener);
        let usage = mock
            .provider
            .fetch_remote_quota("mock-token", None)
            .await
            .unwrap();
        assert_eq!(pool_window(&usage, "gemini-weekly").used_percent, 7.15);
        assert_eq!(mock.preflight_requests.load(Ordering::Relaxed), 0);
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 1);
    }

    struct PreflightVaultSource {
        project: Option<String>,
        email: Option<String>,
        reports: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CredentialSource for PreflightVaultSource {
        async fn get(
            &self,
            _: &crate::credential_source::VaultCapability,
            _: u64,
        ) -> Result<crate::credential_source::VaultCredential, VaultGetError> {
            Ok(crate::credential_source::VaultCredential {
                payload: b"mock-token".to_vec(),
                expires_at_ms: None,
                record_version: 1,
                account_id: self.email.clone(),
                email: self.email.clone(),
                org_name: None,
                project_id: self.project.clone(),
            })
        }

        async fn report_auth_failure(
            &self,
            _: &crate::credential_source::VaultCapability,
            _: u16,
            _: u64,
        ) {
            self.reports.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn attach_vault(
        provider: &mut AntigravityProvider,
        project: Option<&str>,
        reports: Arc<AtomicUsize>,
    ) -> CredentialHandle {
        provider.credential_source = Some(Arc::new(PreflightVaultSource {
            project: project.map(str::to_string),
            email: None,
            reports,
        }));
        CredentialHandle::vault(
            "antigravity:test",
            crate::credential_source::VaultCapability::new("ckh_test"),
        )
    }

    #[tokio::test]
    async fn a_stored_project_handle_fetches_quota_without_a_preflight() {
        let mut mock =
            cloud_preflight_mock(200, consumer_client_refusal().to_string().into_bytes()).await;
        let handle = attach_vault(
            &mut mock.provider,
            Some("existing-project"),
            Arc::new(AtomicUsize::new(0)),
        );
        let usage = mock.provider.fetch_handle(&handle).await.usage.unwrap();
        assert_eq!(pool_window(&usage, "gemini-weekly").used_percent, 7.15);
        assert_eq!(mock.preflight_requests.load(Ordering::Relaxed), 0);
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_paid_sidebar_tier_withholds_the_vault_cloud_pool_and_serves_the_sidebar() {
        let now = Utc::now() - chrono::Duration::minutes(5);
        let account = super::plugin_lane_tests::sidebar_account_json(
            "paid@example.test",
            now.timestamp_millis(),
        );
        let (sidebar, _home) =
            super::plugin_lane_tests::sidebar_fixture("vault-paid", serde_json::json!([account]));
        let mut mock = cloud_preflight_mock(200, b"{}".to_vec()).await;
        mock.provider.override_store = None;
        mock.provider.env_lookup = sidebar.env_lookup;
        let handle = attach_vault(&mut mock.provider, None, Arc::new(AtomicUsize::new(0)));
        mock.provider.credential_source = Some(Arc::new(PreflightVaultSource {
            project: None,
            email: Some("paid@example.test".to_string()),
            reports: Arc::new(AtomicUsize::new(0)),
        }));
        let attempt = mock.provider.fetch_handle(&handle).await;
        let usage = attempt.usage.unwrap();
        assert!((pool_window(&usage, "gemini-weekly").used_percent - 6.1696).abs() < 1e-10);
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt.account_info.and_then(|info| info.email).as_deref(),
            Some("paid@example.test")
        );
        assert_eq!(
            attempt.value_observed_at.unwrap().timestamp_millis(),
            now.timestamp_millis()
        );
        assert_eq!(mock.preflight_requests.load(Ordering::Relaxed), 0);
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 0);
    }

    async fn assert_sidebar_tier_reaches_vault_cloud(label: &str, tier: Option<Value>) {
        let mut account = super::plugin_lane_tests::sidebar_account_json(
            "cloud@example.test",
            Utc::now().timestamp_millis(),
        );
        match tier {
            Some(tier) => account["tier"] = tier,
            None => {
                account.as_object_mut().unwrap().remove("tier");
            }
        }
        let (sidebar, _home) =
            super::plugin_lane_tests::sidebar_fixture(label, serde_json::json!([account]));
        let mut mock = cloud_preflight_mock(200, b"{}".to_vec()).await;
        mock.provider.override_store = None;
        mock.provider.env_lookup = sidebar.env_lookup;
        let handle = attach_vault(&mut mock.provider, None, Arc::new(AtomicUsize::new(0)));
        mock.provider.credential_source = Some(Arc::new(PreflightVaultSource {
            project: None,
            email: Some("cloud@example.test".to_string()),
            reports: Arc::new(AtomicUsize::new(0)),
        }));
        let attempt = mock.provider.fetch_handle(&handle).await;
        assert_eq!(
            pool_window(&attempt.usage.unwrap(), "gemini-weekly").used_percent,
            7.15
        );
        assert_eq!(attempt.value_observed_at, None);
        assert_eq!(mock.preflight_requests.load(Ordering::Relaxed), 1);
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn an_unknown_sidebar_tier_still_reaches_the_vault_cloud() {
        assert_sidebar_tier_reaches_vault_cloud("vault-unknown", None).await;
    }

    #[tokio::test]
    async fn a_free_sidebar_tier_still_reaches_the_vault_cloud() {
        assert_sidebar_tier_reaches_vault_cloud(
            "vault-free",
            Some(serde_json::json!({
                "id": "standard-tier", "paidId": "free-tier", "capturedAt": 1
            })),
        )
        .await;
    }

    /// The vault lane can't tell a paid account without the sidebar, so it keeps
    /// the last reading instead of publishing the cloud pool.
    #[tokio::test]
    async fn the_vault_lane_withholds_the_cloud_pool_when_the_sidebar_is_unreadable() {
        let mut mock =
            cloud_preflight_mock(200, consumer_client_refusal().to_string().into_bytes()).await;
        let handle = attach_vault(&mut mock.provider, Some("p"), Arc::new(AtomicUsize::new(0)));
        mock.provider.set_override_store_unreadable("moved");
        let error = mock
            .provider
            .fetch_handle(&handle)
            .await
            .usage
            .expect_err("the tier is unknown, so the cloud pool may be the wrong one");
        assert!(
            matches!(error, FetchError::LocalSourceUnavailable(_)),
            "{error:?}"
        );
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_preflight_http_401_still_serves_quota_without_reporting_to_the_vault() {
        let mut mock =
            cloud_preflight_mock(401, consumer_client_refusal().to_string().into_bytes()).await;
        let reports = Arc::new(AtomicUsize::new(0));
        let handle = attach_vault(&mut mock.provider, None, Arc::clone(&reports));
        let usage = mock.provider.fetch_handle(&handle).await.usage.unwrap();
        assert_eq!(pool_window(&usage, "gemini-weekly").used_percent, 7.15);
        assert_eq!(mock.preflight_requests.load(Ordering::Relaxed), 1);
        assert_eq!(mock.quota_requests.load(Ordering::Relaxed), 1);
        tokio::task::yield_now().await;
        assert_eq!(reports.load(Ordering::Relaxed), 0);
        // A real quota 401 still reaches the report hook; only the preflight is ignored.
        mock.provider.quota_summary_url = mock
            .provider
            .quota_url
            .replace("/quota", "/quota-unauthorized");
        let error = mock.provider.fetch_handle(&handle).await.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_rejected");
        tokio::task::yield_now().await;
        assert_eq!(reports.load(Ordering::Relaxed), 1);
    }

    /// The named window for one pool-and-cadence, which is how this provider
    /// publishes every limit it meters.
    ///
    /// Tests used to reach for `usage.primary` as a handle on "the Gemini pool",
    /// which worked only because the unnamed slot held a COPY of one named
    /// window. Asking by id says which limit is meant, so a test cannot silently
    /// start reading a different one.
    pub(super) fn pool_window<'a>(usage: &'a Usage, id: &str) -> &'a RateWindow {
        usage
            .extra_rate_windows
            .as_ref()
            .expect("the provider publishes named windows")
            .iter()
            .find(|extra| extra.id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no named window {id}"))
            .window
            .as_ref()
            .expect("a named window carries its rate window")
    }

    /// No unnamed slot is ever populated: every limit this provider meters is
    /// named, so a slot could only ever hold a copy of one of them.
    pub(super) fn assert_no_unnamed_slot(usage: &Usage) {
        assert!(
            usage.primary.is_none() && usage.secondary.is_none() && usage.tertiary.is_none(),
            "antigravity publishes named windows only: {usage:?}"
        );
    }

    /// The two lanes publish the SAME title for the same meter.
    ///
    /// Both reach `normalize_quota_summary`, and they arrive carrying different
    /// text for one window: the cloud lane brings upstream's `displayName`
    /// ("Weekly Limit Remaining"), the plugin cache has only its cadence key
    /// ("weekly"). Concatenating whatever arrived published one account as
    /// "Gemini Models weekly" and its sibling as "Gemini Models Weekly Limit
    /// Remaining" FOR THE SAME ID -- and made one account change label the moment
    /// an editor opened, since that is the event that switches its lane.
    ///
    /// Reported from a renderer, where the two spellings looked like six meters
    /// instead of four. Invisible from inside either lane: each is
    /// self-consistent, and only a comparison between them shows it.
    #[test]
    fn both_lanes_title_the_same_meter_identically() {
        let cloud = parse_quota_summary(
            r#"{"groups":[{"displayName":"Gemini Models","buckets":[
                {"bucketId":"gemini-weekly","displayName":"Weekly Limit Remaining",
                 "window":"weekly","remainingFraction":0.5,
                 "resetTime":"2026-09-17T18:41:37Z"}
            ]}]}"#,
        )
        .expect("the cloud body parses");

        let cached: CachedQuota = serde_json::from_str(
            r#"{"gemini":{"checkedAt":1000,"windows":[
                {"window":"weekly","remainingFraction":0.5,
                 "resetAt":1789670497000}
            ]}}"#,
        )
        .expect("the cache payload parses");
        let (cache, _) =
            parse_cached_quota(&cached, DateTime::from_timestamp_millis(2000).unwrap())
                .expect("the sidebar normalises");
        for usage in [&cloud, &cache] {
            assert_eq!(
                pool_window(usage, "gemini-weekly").window_kind.as_deref(),
                Some("weekly")
            );
        }

        let named = |usage: &Usage| -> Vec<(String, String)> {
            usage
                .extra_rate_windows
                .as_ref()
                .expect("named windows")
                .iter()
                .map(|extra| {
                    (
                        extra.id.clone().unwrap_or_default(),
                        extra.title.clone().unwrap_or_default(),
                    )
                })
                .collect()
        };

        assert_eq!(
            named(&cloud),
            named(&cache),
            "one meter must carry one id and one title however it was read; a lane \
             switch is not a reason for an account to change what its window is called"
        );
        assert_eq!(
            named(&cloud),
            vec![(
                "gemini-weekly".to_string(),
                "Gemini Models weekly".to_string()
            )],
            "and the title follows the cadence this module derived, not the wording \
             upstream happened to send"
        );
    }

    use super::*;

    /// The containment guard on the probe must read the URL's real host.
    ///
    /// The client behind this guard runs with certificate validation disabled,
    /// so it accepts any certificate from whatever it connects to. The guard is
    /// the only thing keeping that client on this machine, which makes it worth
    /// holding to a stricter standard than its caller currently needs: today the
    /// URL is assembled from a literal host and a `u16` port, so nothing hostile
    /// can reach it, but a guard that is only correct because its input is
    /// already trusted provides no containment at all.
    #[test]
    fn the_probe_guard_reads_the_real_host_not_the_string_prefix() {
        // Controls: the URLs this provider actually builds must still pass, or
        // the refusals below would hold for a guard that blocks everything.
        assert!(is_loopback_url("http://127.0.0.1:8080/quota"));
        assert!(is_loopback_url("https://127.0.0.1:9999/quota"));
        assert!(is_loopback_url("http://localhost:8080/quota"));
        assert!(is_loopback_url("https://[::1]:8080/quota"));

        // Userinfo comes before the host, so this string begins with a loopback
        // prefix while addressing another machine entirely. A prefix test accepts
        // it and the request -- with certificate checking off -- leaves the host.
        assert!(!is_loopback_url("http://localhost:8080@example.test/quota"));
        assert!(!is_loopback_url("https://127.0.0.1:443@example.test/quota"));

        // Ordinary non-loopback hosts, including one that merely starts with a
        // loopback-looking label.
        assert!(!is_loopback_url("https://example.test:8080/quota"));
        assert!(!is_loopback_url(
            "https://localhost.example.test:8080/quota"
        ));
        assert!(!is_loopback_url(
            "https://127.0.0.1.example.test:8080/quota"
        ));

        // A different scheme is not something this probe should ever speak, and
        // a file URL has no host to compare at all.
        assert!(!is_loopback_url("file:///etc/passwd"));
        assert!(!is_loopback_url("ftp://127.0.0.1:21/quota"));

        // Not a URL at all.
        assert!(!is_loopback_url("http://"));
        assert!(!is_loopback_url("127.0.0.1:8080/quota"));
    }

    // Captured from the live `agy` wire: groups under `response`, buckets
    // carrying an explicit `window` field. The identifiers here -- `gemini-5h`,
    // `gemini-weekly`, `3p-5h` -- are OBSERVED values, unlike the hand-written
    // `g-*` ones in the tests below, and are the forms a consumer can rely on
    // this lane publishing.
    const SUMMARY_FIXTURE: &str = r#"{
      "response": {
        "groups": [
          {
            "displayName": "Gemini Models",
            "buckets": [
              { "bucketId": "gemini-5h", "displayName": "Five Hour Limit", "window": "5h",
                "remainingFraction": 0.8, "resetTime": "2026-06-24T08:00:00Z" },
              { "bucketId": "gemini-weekly", "displayName": "Weekly Limit", "window": "weekly",
                "remainingFraction": 0.53, "resetTime": "2026-06-30T00:00:00Z" }
            ]
          },
          {
            "displayName": "Claude and GPT models",
            "buckets": [
              { "bucketId": "3p-5h", "displayName": "Five Hour Limit", "window": "5h",
                "remainingFraction": 0.95, "resetTime": "2026-06-24T08:00:00Z" }
            ]
          }
        ]
      }
    }"#;

    /// Only a REFUSAL drops to the per-model lane; a transient condition does not.
    ///
    /// Both directions are asserted together because either alone is satisfied by
    /// a constant: "always fall back" passes the first group, "never fall back"
    /// passes the second. What must hold is the SPLIT.
    ///
    /// The cost is asymmetric, which is why the predicate is narrow: falling back
    /// too eagerly publishes a collapsed window that reads as a real, lower
    /// number and says nothing about the window it dropped, while declining to
    /// fall back on a genuine refusal costs one degraded entry that names itself.
    #[test]
    fn only_a_refusal_drops_to_the_per_model_lane() {
        for refusal in [
            FetchError::ProviderStatus(403, String::new()),
            FetchError::ProviderStatus(404, String::new()),
            FetchError::Unauthorized("not entitled".to_string()),
        ] {
            assert!(
                summary_unavailable(&refusal),
                "a refusal must fall back to the per-model lane: {refusal:?}"
            );
        }
        for transient in [
            FetchError::ProviderStatus(500, String::new()),
            FetchError::ProviderStatus(429, String::new()),
            FetchError::Upstream("timed out".to_string()),
            FetchError::Decode("bad json".to_string()),
        ] {
            assert!(
                !summary_unavailable(&transient),
                "a transient failure must not degrade to the collapsed-window lane: {transient:?}"
            );
        }
    }

    #[test]
    fn every_bucket_is_published_as_its_own_named_window() {
        let usage = parse_quota_summary(SUMMARY_FIXTURE).unwrap();
        assert_eq!(
            pool_window(&usage, "gemini-5h").window_kind.as_deref(),
            Some("five_hour")
        );
        assert_eq!(
            pool_window(&usage, "gemini-weekly").window_kind.as_deref(),
            Some("weekly")
        );
        assert_eq!(
            pool_window(&usage, "3p-5h").window_kind.as_deref(),
            Some("five_hour")
        );

        // Each bucket keeps its own figure rather than being folded into one
        // headline: the Gemini pool meters 20% at five hours and 47% weekly, and
        // collapsing those to the worse one loses the cadence a reader needs.
        assert_eq!(pool_window(&usage, "gemini-5h").used_percent, 20.0);
        let weekly = super::tests::pool_window(&usage, "gemini-weekly");
        assert_eq!(weekly.used_percent, 47.0);
        assert_eq!(weekly.resets_at.as_deref(), Some("2026-06-30T00:00:00Z"));
        assert_eq!(weekly.window_kind.as_deref(), Some("weekly"));
        assert_eq!(weekly.window_minutes, Some(10080));

        assert_eq!(usage.extra_rate_windows.as_ref().unwrap().len(), 3);

        // The property the unnamed slot used to need policing: with no slot, a
        // walled external pool CANNOT be mistaken for the account's own capacity,
        // because there is nowhere unnamed for it to sit.
        assert_no_unnamed_slot(&usage);
    }

    /// The session alias is read from `window`, and the underscore label form
    /// (`Weekly_Limit`, observed in `displayName`) resolves through the id.
    ///
    /// `Weekly_Limit` normalizes to `weekly-limit`, which is not itself a cadence
    /// word: the weekly cadence here always came from the id `gemini-weekly`. So
    /// the second bucket carries no `window`, where the id is read. When the same
    /// label arrives AS the `window`, it is an explicit value this module does not
    /// recognise, and it gives no cadence rather than borrowing the id's -- the
    /// same answer upstream's `quotaCadenceCandidates` gives it.
    #[test]
    fn cadence_aliases_cover_session_and_underscore_weekly_limit() {
        let body = r#"{"groups":[{"displayName":"Gemini Models","buckets":[
            {"bucketId":"gemini-session","displayName":"Session Limit","window":"session",
             "remainingFraction":0.8,"resetTime":"2026-07-24T18:34:51Z"},
            {"bucketId":"gemini-weekly","displayName":"Weekly_Limit",
             "remainingFraction":0.6,"resetTime":"2026-07-30T18:34:51Z"}
        ]}]}"#;
        let usage = parse_quota_summary(body).unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(
            extras[0].window.as_ref().unwrap().window_kind.as_deref(),
            None
        );
        assert_eq!(extras[0].window.as_ref().unwrap().window_minutes, Some(300));
        assert_eq!(
            extras[1].window.as_ref().unwrap().window_minutes,
            Some(10080)
        );

        assert_eq!(
            cadence_of("gemini-weekly", "Weekly_Limit", Some("Weekly_Limit")),
            None,
            "a label form in `window` is an unrecognised explicit value"
        );
    }

    /// The cadence one bucket publishes, from a one-bucket Gemini summary.
    ///
    /// `window` is spliced in as raw JSON so a test can pass an absent field
    /// (`None`) as well as any string value.
    fn cadence_of(bucket_id: &str, display_name: &str, window: Option<&str>) -> Option<i64> {
        let window_field = window
            .map(|w| format!(r#","window":"{w}""#))
            .unwrap_or_default();
        let body = format!(
            r#"{{"groups":[{{"displayName":"Gemini Models","buckets":[{{
                "bucketId":"{bucket_id}","displayName":"{display_name}"{window_field},
                "remainingFraction":0.8}}]}}]}}"#
        );
        let usage = parse_quota_summary(&body).expect("a one-bucket summary parses");
        pool_window(&usage, bucket_id).window_minutes
    }

    /// An explicit `window` decides the cadence, over what the id says.
    ///
    /// Upstream's fixture (`explicit cadence takes precedence over legacy bucket
    /// names`, AntigravityQuotaSourceParityTests.swift at v0.70.0): a bucket whose
    /// id reads `gemini-5h` but whose `window` reads `weekly` is a weekly meter.
    /// Reading both and letting the session alias win published it as 300
    /// minutes, and consumers pace against `windowMinutes`.
    #[test]
    fn an_explicit_window_outranks_the_bucket_id() {
        assert_eq!(
            cadence_of("gemini-5h", "Five Hour Limit", Some("weekly")),
            Some(WEEKLY_WINDOW_MINUTES),
            "the stated window is weekly, whatever the legacy id says"
        );
    }

    /// An unrecognised explicit `window` gives no cadence, not the id's.
    ///
    /// Upstream runs the same fixture with `"window":"unknown"`: with the id
    /// `gemini-5h` it yields a nil `windowMinutes`. The bucket is still published
    /// (its fraction is real); only the pace is withheld, because the server named
    /// a cadence this module does not know and the id is not allowed to overrule it.
    #[test]
    fn an_unrecognised_explicit_window_gives_no_cadence() {
        assert_eq!(
            cadence_of("gemini-5h", "Five Hour Limit", Some("unknown")),
            None
        );
    }

    /// With no `window`, or a blank one, the id and display name still decide.
    ///
    /// Upstream treats a whitespace-only `window` as absent (it trims before the
    /// emptiness check), so both shapes fall back.
    #[test]
    fn an_absent_or_blank_window_falls_back_to_the_id_and_name() {
        assert_eq!(
            cadence_of("gemini-5h", "Five Hour Limit", None),
            Some(SESSION_WINDOW_MINUTES),
            "with no window, the id's 5h cadence is the only evidence"
        );
        assert_eq!(
            cadence_of("gemini-5h", "Five Hour Limit", Some("  ")),
            Some(SESSION_WINDOW_MINUTES),
            "a blank window is no statement, so the id still decides"
        );
        assert_eq!(
            cadence_of("gemini-allowance", "Weekly Limit", None),
            Some(WEEKLY_WINDOW_MINUTES),
            "and the display name is read when the id carries no cadence"
        );
    }

    /// An opaque bucket id takes its cadence from the explicit window.
    ///
    /// Upstream's `summary honors explicit cadence for opaque bucket IDs`: the id
    /// `gemini-allowance` names no cadence, so the window is the only evidence.
    #[test]
    fn an_opaque_bucket_id_takes_the_explicit_window() {
        assert_eq!(
            cadence_of("gemini-allowance", "Limit Remaining", Some("weekly")),
            Some(WEEKLY_WINDOW_MINUTES)
        );
        assert_eq!(
            cadence_of("gemini-allowance", "Limit Remaining", Some("5h")),
            Some(SESSION_WINDOW_MINUTES)
        );
    }

    /// The four live bucket shapes publish the cadence they did before.
    ///
    /// On today's wire every bucket's `window` agrees with its id, so reading
    /// the window alone must change nothing here. This is the control for the
    /// precedence rule above: a rule that broke the live shapes would pass the
    /// disagreement tests and still move every live account's pace.
    #[test]
    fn the_four_live_bucket_shapes_keep_their_cadence() {
        let body = r#"{"response":{"groups":[
            {"displayName":"Gemini Models","buckets":[
                {"bucketId":"gemini-5h","displayName":"Five Hour Limit","window":"5h",
                 "remainingFraction":0.8,"resetTime":"2026-06-24T08:00:00Z"},
                {"bucketId":"gemini-weekly","displayName":"Weekly Limit","window":"weekly",
                 "remainingFraction":0.53,"resetTime":"2026-06-30T00:00:00Z"}
            ]},
            {"displayName":"Claude and GPT models","buckets":[
                {"bucketId":"3p-5h","displayName":"Five Hour Limit","window":"5h",
                 "remainingFraction":0.95,"resetTime":"2026-06-24T08:00:00Z"},
                {"bucketId":"3p-weekly","displayName":"Weekly Limit","window":"weekly",
                 "remainingFraction":0.4,"resetTime":"2026-06-30T00:00:00Z"}
            ]}
        ]}}"#;
        let usage = parse_quota_summary(body).unwrap();
        let published: Vec<(String, String, Option<i64>)> = usage
            .extra_rate_windows
            .as_ref()
            .unwrap()
            .iter()
            .map(|extra| {
                (
                    extra.id.clone().unwrap(),
                    extra.title.clone().unwrap(),
                    extra.window.as_ref().unwrap().window_minutes,
                )
            })
            .collect();
        assert_eq!(
            published,
            vec![
                ("gemini-5h".into(), "Gemini Models 5h".into(), Some(300)),
                (
                    "gemini-weekly".into(),
                    "Gemini Models weekly".into(),
                    Some(10080)
                ),
                ("3p-5h".into(), "Claude and GPT models 5h".into(), Some(300)),
                (
                    "3p-weekly".into(),
                    "Claude and GPT models weekly".into(),
                    Some(10080)
                ),
            ]
        );
    }

    #[test]
    fn a_walled_external_pool_cannot_be_read_as_the_account_s_own_capacity() {
        // The exact live shape that once misled the headline: Gemini nearly free,
        // Claude/GPT walled at 100%. The headline slot used to have to be policed
        // so the walled pool could not claim it; now there is no unnamed slot at
        // all, so the hazard is structural rather than guarded -- and each pool
        // keeps its own figure under its own name.
        let body = r#"{"response":{"groups":[
            {"displayName":"Gemini Models","buckets":[
                {"bucketId":"gemini-weekly","displayName":"Weekly Limit","window":"weekly",
                 "remainingFraction":0.883,"resetTime":"2026-07-24T18:34:51Z"}
            ]},
            {"displayName":"Claude and GPT models","buckets":[
                {"bucketId":"3p-weekly","displayName":"Weekly Limit","window":"weekly",
                 "remainingFraction":0.0,"resetTime":"2026-07-18T13:08:36Z"}
            ]}
        ]}}"#;
        let usage = parse_quota_summary(body).unwrap();
        assert_no_unnamed_slot(&usage);
        assert_eq!(
            super::tests::pool_window(&usage, "gemini-weekly").used_percent,
            11.7,
            "the native pool reports its own figure"
        );
        assert_eq!(
            pool_window(&usage, "3p-weekly").used_percent,
            100.0,
            "and the walled external pool stays visible without standing for the account"
        );
        assert_eq!(usage.extra_rate_windows.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn an_account_with_no_native_pool_still_publishes_what_it_has() {
        let body = r#"{"groups":[{"displayName":"Claude and GPT models","buckets":[
            {"bucketId":"3p-5h","displayName":"Five Hour Limit","window":"5h",
             "remainingFraction":0.95,"resetTime":"2026-06-24T08:00:00Z"},
            {"bucketId":"3p-weekly","displayName":"Weekly Limit","window":"weekly",
             "remainingFraction":0.4,"resetTime":"2026-06-30T00:00:00Z"}
        ]}]}"#;
        let usage = parse_quota_summary(body).unwrap();
        // No Gemini pool at all. There used to be a fallback picking the most-used
        // window for the headline so such an account reported SOMETHING rather
        // than degrading; with every window named, it reports both directly and
        // the fallback has nothing left to do.
        assert_no_unnamed_slot(&usage);
        assert_eq!(pool_window(&usage, "3p-5h").used_percent, 5.0);
        assert_eq!(pool_window(&usage, "3p-weekly").used_percent, 60.0);
    }

    /// A window with usage but no reset is still published.
    ///
    /// Hand-written input: the identifiers and display names here were chosen to
    /// make the case reachable, not observed on a live server. Real captures use
    /// `gemini-5h` / `gemini-weekly` / `3p-*` (see `SUMMARY_FIXTURE`), so nothing
    /// downstream should treat `g-*` as a shape this upstream sends.
    #[test]
    fn bucket_without_reset_is_kept() {
        let body = r#"{"groups":[{"displayName":"Gemini","buckets":[
            {"bucketId":"g-5h","displayName":"5-hour","remainingFraction":0.5}
        ]}]}"#;
        let usage = parse_quota_summary(body).unwrap();
        let primary = pool_window(&usage, "g-5h");
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
    }

    /// An exhausted window with no reset is published rather than dropped.
    ///
    /// Hand-written input, like its sibling above: `g-*` identifiers are
    /// invented to reach the case, not observed.
    #[test]
    fn exhausted_bucket_without_reset_is_kept() {
        let body = r#"{"groups":[{"displayName":"Gemini","buckets":[
            {"bucketId":"g-5h","displayName":"5-hour","remainingFraction":0.0}
        ]}]}"#;
        let usage = parse_quota_summary(body).unwrap();
        let primary = pool_window(&usage, "g-5h");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.resets_at, None);
    }

    /// A bucket the server marks disabled contributes nothing.
    ///
    /// Hand-written input; `g-*` identifiers are invented, not observed.
    #[test]
    fn disabled_bucket_is_skipped() {
        let body = r#"{"groups":[{"displayName":"Gemini","buckets":[
            {"bucketId":"g-5h","displayName":"5-hour","disabled":true,
             "remainingFraction":0.5,"resetTime":"2026-06-24T08:00:00Z"}
        ]}]}"#;
        assert!(matches!(
            parse_quota_summary(body),
            Err(FetchError::Decode(_))
        ));
    }

    /// The nested fraction shape and an epoch-seconds reset both parse.
    ///
    /// Hand-written input covering encodings the server may use; the `g-*`
    /// identifier is invented, not observed.
    #[test]
    fn nested_remaining_fraction_and_epoch_reset() {
        let body = r#"{"groups":[{"displayName":"Gemini","buckets":[
            {"bucketId":"g-weekly","displayName":"weekly",
             "remaining":{"case":"remainingFraction","value":0.25},
             "resetTime":1788000000}
        ]}]}"#;
        let usage = parse_quota_summary(body).unwrap();
        let window = pool_window(&usage, "g-weekly");
        assert_eq!(window.used_percent, 75.0);
        assert_eq!(window.window_kind.as_deref(), Some("weekly"));
        assert_eq!(window.window_minutes, Some(10080));
    }

    #[test]
    fn csrf_extraction_handles_equals_and_space() {
        assert_eq!(
            extract_csrf_token("language_server --csrf_token=abc123 --foo").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            extract_csrf_token("language_server --csrf_token def456").as_deref(),
            Some("def456")
        );
        assert_eq!(extract_csrf_token("language_server --other x"), None);
    }

    #[test]
    fn classify_cli_needs_no_token_app_needs_token() {
        // CLI: empty token (none needed).
        assert_eq!(
            classify_command("/Applications/Antigravity.app/.../agy"),
            Some(String::new())
        );
        // App language server with token.
        assert_eq!(
            classify_command(
                "/Applications/Antigravity.app/language_server --csrf_token=tok antigravity"
            ),
            Some("tok".to_string())
        );
        // App language server WITHOUT token → cannot auth → skipped.
        assert_eq!(
            classify_command("/Applications/Antigravity.app/language_server antigravity"),
            None
        );
        // Unrelated process.
        assert_eq!(classify_command("/usr/bin/node server.js"), None);
    }

    #[test]
    fn parses_listening_ports_from_lsof() {
        let lsof = "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n\
            agy 123 user 7u IPv4 0x0 0t0 TCP 127.0.0.1:51234 (LISTEN)\n\
            agy 123 user 8u IPv6 0x0 0t0 TCP [::1]:51235 (LISTEN)\n\
            agy 123 user 9u IPv4 0x0 0t0 TCP 127.0.0.1:443 (ESTABLISHED)\n";
        let ports = parse_listening_ports(lsof);
        assert_eq!(ports, vec![51234, 51235]);
    }

    /// The CLI arm accepts a shell command that merely names the binary.
    ///
    /// Recorded as a KNOWN LIMIT rather than fixed, because the fix is worse
    /// than the fault. `ps` shows argv and nothing in it distinguishes "is this
    /// process the CLI" from "does this process mention the CLI" -- the kernel
    /// knows, through the executable path, but reading that per candidate costs
    /// a syscall per process on every refresh for a provider that is usually
    /// absent.
    ///
    /// The consequence is bounded: a wrongly-matched pid gets its ports probed,
    /// answers nothing, and the provider reports its local source unavailable.
    /// That is the same outcome as no editor running, which is the common case,
    /// so the cost is a wasted probe rather than a wrong reading. Nothing is
    /// published from a mismatched process, because the probe requires a
    /// response the wrong process cannot give.
    #[test]
    fn the_cli_arm_matches_a_shell_that_only_names_the_binary() {
        assert_eq!(
            classify_command("/bin/bash -c ls ~/bin/agy"),
            Some(String::new()),
            "documenting the limit: a command ending in /agy classifies as the CLI"
        );
        // What keeps it bounded: the port probe against such a pid finds
        // nothing, so the lane degrades rather than publishing a wrong figure.
    }

    /// A shell whose own command line mentions the words is not a server.
    ///
    /// `ps -ax -o command=` shows a process's whole argv, so any shell running a
    /// grep, an editor, or a build over this file carries both "antigravity" and
    /// "language_server" in its own line. The classifier is a substring match
    /// over that text, and the language-server arm is only closed because it
    /// also demands a `--csrf_token` -- which a shell command discussing the
    /// server does not have, unless the text it carries happens to contain one.
    ///
    /// The CLI arm has no such second condition, so it is the exposed one: it
    /// accepts any command line containing `/agy ` or ending in `/agy`. A probe
    /// against a wrongly-matched pid asks a loopback port that answers nothing
    /// useful, and the provider reports the local source unavailable -- honest,
    /// but attributed to a missing editor rather than a bad match.
    #[test]
    fn a_shell_mentioning_the_words_is_not_classified_as_a_server() {
        // The real shape: a shell running a search over this very file.
        let shell = "/bin/bash -c grep -n 'antigravity language_server' \
                     crates/quota-core/src/antigravity.rs";
        assert_eq!(
            classify_command(shell),
            None,
            "a shell command naming the server must not be probed as one"
        );

        // An editor holding the file open is the same hazard, different tool.
        assert_eq!(
            classify_command("vim crates/quota-core/src/antigravity.rs"),
            None
        );

        // The real server still classifies, so the guard above is not just
        // rejecting everything.
        let real = "/Applications/Antigravity.app/Contents/Resources/bin/\
                    language_server --csrf_token=abc123";
        assert_eq!(classify_command(real), Some("abc123".to_string()));
    }

    #[test]
    fn parses_process_list_keeps_only_antigravity_servers() {
        let ps = "  123 /Applications/Antigravity.app/Contents/MacOS/agy\n\
            456 /Applications/Antigravity.app/language_server --csrf_token=tok antigravity\n\
            789 /usr/bin/node unrelated.js\n\
            notapid garbage line\n";
        let servers = parse_process_list(ps);
        assert_eq!(servers.len(), 2);
        // CLI process: no token.
        assert_eq!(servers[0].pid, 123);
        assert_eq!(servers[0].csrf_token, "");
        // App language server: token extracted.
        assert_eq!(servers[1].pid, 456);
        assert_eq!(servers[1].csrf_token, "tok");
    }

    /// A live capture of the cloud response, trimmed to one model per distinct
    /// (pool, fraction, reset) so every shape below is one the endpoint really
    /// returned rather than one convenient to parse.
    const REMOTE_FIXTURE: &[u8] = br#"{
      "buckets": [
        { "tokenType": "WTUS", "modelId": "chat_20706", "remainingFraction": 1 },
        { "resetTime": "2026-08-06T11:51:11Z", "tokenType": "WTUS",
          "modelId": "claude-opus-4-6-thinking", "remainingFraction": 1 },
        { "resetTime": "2026-08-06T11:51:11Z", "tokenType": "WTUS",
          "modelId": "claude-sonnet-4-6", "remainingFraction": 0.5 },
        { "resetTime": "2026-08-06T09:38:35Z", "tokenType": "WTUS",
          "modelId": "gemini-2.5-flash", "remainingFraction": 0.98586655 },
        { "resetTime": "2026-08-06T09:38:35Z", "tokenType": "WTUS",
          "modelId": "gemini-3.1-pro-high", "remainingFraction": 0.98586655 },
        { "resetTime": "2026-08-06T11:51:11Z", "tokenType": "WTUS",
          "modelId": "gpt-oss-120b-medium", "remainingFraction": 1 },
        { "tokenType": "WTUS", "modelId": "tab_flash_lite_preview", "remainingFraction": 1 }
      ]
    }"#;

    /// The cloud lane publishes the same shape as the local probe.
    ///
    /// The two lanes describe one account and are handed different
    /// granularities: named pool groups locally, one bucket per model from the
    /// cloud. If they disagreed, this provider's reading would change according
    /// to whether an editor happened to be open, which is exactly the coupling
    /// the cloud lane exists to remove.
    #[test]
    fn the_remote_lane_folds_models_into_the_same_pools_the_local_probe_publishes() {
        let usage = parse_remote_quota(REMOTE_FIXTURE).expect("fixture parses");

        // The native Gemini pool owns the unnamed slot, as locally: an unnamed
        // window reads as the account's own capacity, and the external pool in
        // it would report the provider exhausted while Gemini is free.
        let primary = pool_window(&usage, "Gemini Models");
        assert_eq!(primary.used_percent, 1.41);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-08-06T09:38:35Z"));

        let extras = usage.extra_rate_windows.expect("both pools are published");
        let ids: Vec<&str> = extras.iter().filter_map(|x| x.id.as_deref()).collect();
        assert!(ids.contains(&"Gemini Models"), "{ids:?}");
        assert!(ids.contains(&"Claude and GPT models"), "{ids:?}");

        // The external pool reports its WORST model, not its average or its
        // first: one exhausted model in a pool is what constrains the account.
        let external = extras
            .iter()
            .find(|x| x.id.as_deref() == Some("Claude and GPT models"))
            .and_then(|x| x.window.as_ref())
            .expect("external pool window");
        assert_eq!(external.used_percent, 50.0);

        // Not vacuous: seventeen Gemini models collapse to one window rather
        // than seventeen near-identical rows.
        assert_eq!(extras.len(), 2, "{ids:?}");
    }

    /// Models the account is never metered on stay out of the pools.
    ///
    /// The always-available internal models (`chat_*`, `tab_*`) report a full
    /// fraction and no reset. Folding a permanently-idle bucket into a metered
    /// pool would drag that pool's worst-case reading toward zero and make a
    /// constrained account look free.
    #[test]
    fn a_bucket_with_no_reset_does_not_dilute_a_metered_pool() {
        let usage = parse_remote_quota(REMOTE_FIXTURE).unwrap();
        let extras = usage.extra_rate_windows.unwrap();

        assert!(
            !extras
                .iter()
                .any(|x| x.id.as_deref() == Some("Other models")),
            "an unmetered bucket became its own pool"
        );
        for extra in &extras {
            let window = extra.window.as_ref().unwrap();
            assert!(
                window.resets_at.is_some(),
                "{:?} has no reset",
                extra.id.as_deref()
            );
        }
    }

    /// A response carrying nothing usable is an error, not an empty success.
    ///
    /// An empty `Usage` would publish as a provider with no windows, which a
    /// consumer cannot tell from capacity nobody measured. A degraded entry says
    /// what happened.
    #[test]
    fn a_remote_response_with_no_metered_bucket_degrades() {
        // Every case still ERRORS -- that is what this test defends, and it is
        // unchanged: an unusable response must never publish an empty window
        // set, which a consumer cannot tell from capacity nobody measured.
        //
        // What differs is WHICH error, and the three inputs are not one case.
        // An empty list is the upstream stating this account has no buckets. An
        // absent field means our struct and their payload disagree. Buckets that
        // are present but unmetered could equally mean our metering predicate is
        // wrong, so that one stays a defect of ours until something proves
        // otherwise.
        let stated_empty = parse_remote_quota(&br#"{"buckets":[]}"#[..])
            .expect_err("an empty bucket list must not publish an empty window set");
        assert!(
            matches!(stated_empty, FetchError::NoQuotaReported(_)),
            "an upstream stating no buckets is an account fact: {stated_empty:?}"
        );

        for body in [
            &br#"{}"#[..],
            &br#"{"buckets":[{"modelId":"chat_1","remainingFraction":1}]}"#[..],
        ] {
            let error = parse_remote_quota(body)
                .expect_err("an unusable response must not publish an empty window set");
            assert!(
                matches!(error, FetchError::Decode(_)),
                "an absent field and an unmetered set both point at this repo: {error:?}"
            );
        }
    }

    /// The cloud lane states no cadence, because the response does not carry one.
    ///
    /// Its buckets have only a model id, a fraction, a reset and a token type.
    /// Deriving a length from time-to-reset would be a guess, and a wrong one:
    /// the local server meters each pool on both a five-hour and a weekly
    /// window, while the cloud returns a single reset per pool, so the reset
    /// alone does not say which meter it belongs to -- one three hours out could
    /// be either.
    ///
    /// Consumers read `windowMinutes` as a cadence, and one check uses it as the
    /// ceiling for a reset-plausibility test. A fabricated five-hour length on a
    /// weekly window would both misreport the pace and disable the check that
    /// would have caught it.
    #[test]
    fn the_remote_lane_publishes_no_cadence_it_cannot_know() {
        let usage = parse_remote_quota(REMOTE_FIXTURE).unwrap();

        assert_eq!(pool_window(&usage, "Gemini Models").window_minutes, None);
        for extra in usage.extra_rate_windows.as_ref().unwrap() {
            let window = extra.window.as_ref().unwrap();
            assert_eq!(
                window.window_minutes,
                None,
                "{:?} published a cadence the response never stated",
                extra.id.as_deref()
            );
            // Not vacuous: the reset the response DOES state is still carried,
            // so this cannot pass by dropping the window's timing entirely.
            assert!(window.resets_at.is_some());
        }
    }
}

#[cfg(test)]
mod plugin_lane_tests {
    use super::*;
    use crate::refresh::{classify, FetchClass};

    /// An unidentifiable credential beside a known paid account must withhold.
    ///
    /// THE DEFECT THIS PINS, found on live data rather than by reading: the tier
    /// lookup is by email, and `emails_match(None, _)` is false, so any fetch where
    /// the vault supplied no identity found no row and answered "not paid". The
    /// cloud then published the STANDARD-tier pool for an account that does not
    /// spend it -- the exact wrong-pool reading the guard exists to prevent,
    /// arriving through the one door it did not cover.
    ///
    /// It is intermittent, which is what made it expensive: it fires only on the
    /// fetches that resolve no identity, so the published percent alternates
    /// between two pools and every step down is recorded as a quota drop. Measured
    /// here: 28 false drops for one account in seventy minutes, against a ring
    /// whose entire purpose is reset detection.
    ///
    /// Both silent arms are asserted with it, because each is a way the fix could
    /// be wrong in the opposite direction -- and the third is load-bearing: it is
    /// the fleet host with a vault credential and no plugin install, which must
    /// keep serving rather than go dark on a suspicion.
    #[test]
    fn an_unidentifiable_credential_withholds_only_when_a_paid_account_is_known() {
        let paid = paid_account(Some("paid@example.test"));
        let free = account(Some("free@example.test"), Some(true));

        // The matched paid account: withheld, as before.
        assert!(
            cloud_lane_must_withhold(&[paid.clone(), free.clone()], Some(&paid)),
            "a matched paid account must not be served the standard pool"
        );

        // No row matched, but a paid account is known. This credential could
        // belong to that account, so publishing its standard pool is unsafe.
        assert!(
            cloud_lane_must_withhold(&[paid.clone(), free.clone()], None),
            "an unidentifiable credential beside a known paid account must withhold"
        );

        // Silent: the matched account is free, so the cloud reports its real pool.
        assert!(
            !cloud_lane_must_withhold(&[paid.clone(), free.clone()], Some(&free)),
            "a free-tier account's real pool IS the cloud's, so serving it is correct"
        );

        // Silent, and load-bearing: no paid account is known anywhere, so there is
        // no evidence to withhold against. A host with a vault credential and no
        // plugin install lives here and must keep serving.
        assert!(
            !cloud_lane_must_withhold(std::slice::from_ref(&free), None),
            "an unknown tier with no paid account anywhere must not take the lane dark"
        );
        assert!(
            !cloud_lane_must_withhold(&[], None),
            "a host with no plugin store at all must keep serving"
        );
    }

    fn account(email: Option<&str>, enabled: Option<bool>) -> StoredAccount {
        serde_json::from_value(serde_json::json!({
            "id": "vault-acct-0", "label": "Account 1", "email": email,
            "enabled": enabled, "health": 100, "current": false,
            "tier": { "id": "standard-tier", "paidId": "free-tier", "capturedAt": 1 },
            "quota": {}
        }))
        .expect("sidebar account fixture")
    }

    fn paid_account(email: Option<&str>) -> StoredAccount {
        let mut account = account(email, Some(true));
        account.tier.as_mut().unwrap().paid_id = Some("g1-ultra-lite-tier".to_string());
        account
    }

    pub(super) fn sidebar_account_json(email: &str, checked_at: i64) -> Value {
        serde_json::json!({
            "id": "vault-acct-0", "label": "Account 1", "email": email,
            "enabled": true, "health": 100, "current": false,
            "tier": { "id": "standard-tier", "paidId": "g1-ultra-lite-tier", "capturedAt": 1 },
            "quota": {
                "gemini": {
                    "remainingPercent": 94, "remainingFraction": 0.938304,
                    "checkedAt": checked_at, "resetAt": 1789670497000i64,
                    "windows": [
                        { "window": "5h", "remainingPercent": 100, "remainingFraction": 1.0,
                          "checkedAt": checked_at, "resetAt": 1789146520000i64 },
                        { "window": "weekly", "remainingPercent": 94, "remainingFraction": 0.938304,
                          "checkedAt": checked_at, "resetAt": 1789670497000i64 }
                    ]
                },
                "non-gemini": {
                    "remainingPercent": 100, "remainingFraction": 1.0, "checkedAt": checked_at,
                    "resetAt": 1789146520000i64,
                    "windows": [
                        { "window": "5h", "remainingPercent": 100, "remainingFraction": 1.0,
                          "checkedAt": checked_at, "resetAt": 1789146520000i64 },
                        { "window": "weekly", "remainingPercent": 100, "remainingFraction": 1.0,
                          "checkedAt": checked_at, "resetAt": 1789670497000i64 }
                    ]
                }
            }
        })
    }

    fn cached_paid_account(email: &str, updated_at: DateTime<Utc>) -> StoredAccount {
        serde_json::from_value(sidebar_account_json(email, updated_at.timestamp_millis()))
            .expect("sidebar quota fixture")
    }

    fn cached_free_account(email: &str, updated_at: DateTime<Utc>) -> StoredAccount {
        let mut account = cached_paid_account(email, updated_at);
        account.tier.as_mut().unwrap().paid_id = Some("free-tier".to_string());
        account
    }

    #[test]
    fn the_handle_name_keys_on_the_email_when_there_is_one() {
        let named = account(Some(" a@example.test "), None);
        assert_eq!(
            account_handle_name(&named).as_deref(),
            Some("plugin:a@example.test")
        );
        let mut provider = AntigravityProvider::new();
        provider.set_override_accounts(vec![account(Some("b@example.test"), None), named.clone()]);
        let before = provider.handles().unwrap();
        provider.set_override_accounts(vec![named, account(Some("b@example.test"), None)]);
        let after = provider.handles().unwrap();
        assert_eq!(before[1], after[0]);
    }

    #[test]
    fn the_local_probe_stands_alone_when_nothing_else_is_configured() {
        let provider = AntigravityProvider::new();
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    #[test]
    fn a_credentialed_lane_replaces_the_local_probe() {
        let mut provider = AntigravityProvider::new();
        provider.set_override_accounts(vec![account(Some("a@example.test"), None)]);
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::new("plugin:a@example.test")]
        );
    }

    /// The Hub user agent's platform and architecture spellings, including the
    /// two choices CodexBar doesn't make: `windows`, and the fallbacks for
    /// targets it never names.
    #[test]
    fn hub_identity_maps_supported_platforms_and_architectures() {
        assert_eq!(hub_target("macos", "aarch64"), ("darwin", "arm64"));
        assert_eq!(hub_target("macos", "x86_64"), ("darwin", "amd64"));
        assert_eq!(hub_target("linux", "aarch64"), ("linux", "arm64"));
        assert_eq!(hub_target("linux", "x86_64"), ("linux", "amd64"));
        assert_eq!(hub_target("windows", "aarch64"), ("windows", "arm64"));
        assert_eq!(hub_target("windows", "x86_64"), ("windows", "amd64"));
        assert_eq!(hub_target("freebsd", "riscv64"), ("darwin", "amd64"));
    }

    #[test]
    fn hub_identity_pins_the_host_user_agent() {
        let platform = match std::env::consts::OS {
            "macos" => "darwin",
            "linux" => "linux",
            "windows" => "windows",
            _ => "darwin",
        };
        let architecture = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "amd64",
            _ => "amd64",
        };
        let header = remote_user_agent_header();
        assert_eq!(header.name, "User-Agent");
        assert_eq!(
            header.value,
            format!("antigravity/hub/2.9.1 {platform}/{architecture}")
        );
    }

    /// The plugin lane publishes `source: "oauth"`, the value consumers already
    /// associate with the opencode plugin's login. Reading the sidebar file instead
    /// of the account store doesn't change which login a consumer asks the user to
    /// renew, so the published value stays the same.
    #[test]
    fn the_plugin_lane_uses_the_existing_oauth_source() {
        assert_eq!(PLUGIN_SOURCE, "oauth");
    }

    const LOCAL_MOCK_QUOTA: &str = r#"{
      "response": {
        "groups": [
          {
            "displayName": "Gemini Models",
            "buckets": [
              {
                "bucketId": "gemini-weekly",
                "displayName": "Weekly Limit",
                "window": "weekly",
                "remainingFraction": 0.416,
                "resetTime": "2026-09-10T18:41:37Z"
              }
            ]
          }
        ]
      }
    }"#;

    pub(super) const CLOUD_MOCK_QUOTA: &str = r#"{
      "response": {
        "groups": [
          {
            "displayName": "Gemini Models",
            "buckets": [
              {
                "bucketId": "gemini-weekly",
                "displayName": "Weekly Limit",
                "window": "weekly",
                "remainingFraction": 0.9285,
                "resetTime": "2026-09-08T18:02:09Z"
              }
            ]
          }
        ]
      }
    }"#;

    async fn spawn_mock_server<F>(handler: F) -> (u16, tokio::task::JoinHandle<()>)
    where
        F: Fn(&str) -> (u16, Vec<u8>) + Send + Sync + 'static,
    {
        spawn_mock_server_with_request(move |request| {
            let first_line = request.lines().next().unwrap_or("");
            let path = first_line.split_whitespace().nth(1).unwrap_or("/");
            handler(path)
        })
        .await
    }

    pub(super) async fn spawn_mock_server_with_request<F>(
        handler: F,
    ) -> (u16, tokio::task::JoinHandle<()>)
    where
        F: Fn(&str) -> (u16, Vec<u8>) + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = std::sync::Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let request = crate::loopback::read_request(&mut stream).await;
                if request.is_empty() {
                    break;
                }
                let (status, body) = handler(&request);
                let reason = if status == 200 { "OK" } else { "Error" };
                let headers = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, headers.as_bytes()).await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stream, &body).await;
            }
        });
        (port, task)
    }

    #[test]
    fn parse_user_status_extracts_email_and_handles_absent_or_malformed() {
        assert_eq!(
            parse_user_status_email(r#"{"userStatus":{"email":"beatricelau0414@gmail.com"}}"#),
            Some("beatricelau0414@gmail.com".to_string())
        );
        assert_eq!(
            parse_user_status_email(
                r#"{"response":{"userStatus":{"email":"beatricelau0414@gmail.com"}}}"#
            ),
            Some("beatricelau0414@gmail.com".to_string())
        );
        assert_eq!(
            parse_user_status_email(r#"{"userStatus":{"email":"   "}}"#),
            None
        );
        assert_eq!(parse_user_status_email(r#"{"userStatus":{}}"#), None);
        assert_eq!(parse_user_status_email(r#"{}"#), None);
        assert_eq!(parse_user_status_email("not json"), None);
    }

    #[test]
    fn emails_match_is_case_insensitive_and_handles_whitespace_and_absence() {
        assert!(emails_match(
            Some("foo@example.com"),
            Some("FOO@EXAMPLE.COM")
        ));
        assert!(emails_match(
            Some("  foo@example.com "),
            Some("foo@example.com")
        ));
        assert!(!emails_match(
            Some("foo@example.com"),
            Some("bar@example.com")
        ));
        assert!(!emails_match(Some("foo@example.com"), Some("   ")));
        assert!(!emails_match(Some("foo@example.com"), None));
        assert!(!emails_match(None, Some("foo@example.com")));
        assert!(!emails_match(None, None));
    }

    /// A matching live local session still outranks a fresh plugin cache.
    #[tokio::test]
    async fn a_live_local_session_still_wins_over_a_fresh_cache() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == QUOTA_SUMMARY_PATH {
                (200, LOCAL_MOCK_QUOTA.as_bytes().to_vec())
            } else if path == USER_STATUS_PATH {
                (
                    200,
                    br#"{"userStatus":{"email":"beatricelau0414@gmail.com"}}"#.to_vec(),
                )
            } else if path == "/cloud-quota" {
                (200, CLOUD_MOCK_QUOTA.as_bytes().to_vec())
            } else {
                (404, b"not found".to_vec())
            }
        })
        .await;

        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(vec![(
            LocalServer {
                pid: 100,
                csrf_token: String::new(),
            },
            port,
        )]);
        provider.set_override_accounts(vec![cached_paid_account(
            "beatricelau0414@gmail.com",
            Utc::now() - chrono::Duration::minutes(5),
        )]);
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");
        provider.load_code_assist_url = format!("http://127.0.0.1:{port}/load");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let usage = attempt.usage.expect("usage must be present");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");

        // Asserts LOCAL lane served (58.4% used, 2026-09-10 reset) rather than cloud (7.15%, 2026-09-08).
        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-10T18:41:37Z"));
        assert_eq!(primary.used_percent, 58.4);
        assert_eq!(primary.window_kind.as_deref(), Some("weekly"));
        assert_eq!(primary.window_minutes, Some(10080));
        assert_eq!(
            attempt
                .observed
                .as_ref()
                .and_then(|o| o.account_id.as_deref()),
            Some("beatricelau0414@gmail.com")
        );
        assert_eq!(attempt.source.as_deref(), Some("oauth"));
        assert_eq!(
            attempt.value_observed_at, None,
            "a cache response would carry the cache's earlier value-time"
        );
    }

    /// A different editor email must use the handle email's sidebar quota.
    #[tokio::test]
    async fn identity_mismatch_falls_through_to_the_sidebar_lane() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == QUOTA_SUMMARY_PATH {
                (200, LOCAL_MOCK_QUOTA.as_bytes().to_vec())
            } else if path == USER_STATUS_PATH {
                // Local editor is signed into a DIFFERENT account than the handle.
                (
                    200,
                    br#"{"userStatus":{"email":"different_user@gmail.com"}}"#.to_vec(),
                )
            } else if path == "/cloud-quota" {
                (200, CLOUD_MOCK_QUOTA.as_bytes().to_vec())
            } else {
                (404, b"not found".to_vec())
            }
        })
        .await;

        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(vec![(
            LocalServer {
                pid: 100,
                csrf_token: String::new(),
            },
            port,
        )]);
        provider.set_override_accounts(vec![cached_paid_account(
            "beatricelau0414@gmail.com",
            Utc::now() - chrono::Duration::minutes(5),
        )]);
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");
        provider.load_code_assist_url = format!("http://127.0.0.1:{port}/load");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let usage = attempt.usage.expect("usage must be present");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");

        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-17T18:41:37Z"));
        assert!((primary.used_percent - 6.1696).abs() < 1e-10);
        assert!(attempt.value_observed_at.is_some());
    }

    /// A local status response without email cannot attribute quota to this account.
    #[tokio::test]
    async fn local_snapshot_with_no_email_does_not_serve_a_named_handle() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == QUOTA_SUMMARY_PATH {
                (200, LOCAL_MOCK_QUOTA.as_bytes().to_vec())
            } else if path == USER_STATUS_PATH {
                // GetUserStatus call fails or omits email.
                (200, br#"{"userStatus":{}}"#.to_vec())
            } else if path == "/cloud-quota" {
                (200, CLOUD_MOCK_QUOTA.as_bytes().to_vec())
            } else {
                (404, b"not found".to_vec())
            }
        })
        .await;

        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(vec![(
            LocalServer {
                pid: 100,
                csrf_token: String::new(),
            },
            port,
        )]);
        provider.set_override_accounts(vec![cached_paid_account(
            "beatricelau0414@gmail.com",
            Utc::now() - chrono::Duration::minutes(5),
        )]);
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");
        provider.load_code_assist_url = format!("http://127.0.0.1:{port}/load");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let usage = attempt.usage.expect("usage must be present");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");

        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-17T18:41:37Z"));
        assert!((primary.used_percent - 6.1696).abs() < 1e-10);
        assert!(attempt.value_observed_at.is_some());
    }

    #[tokio::test]
    async fn local_lane_absent_on_a_plugin_handle_serves_the_sidebar() {
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![cached_paid_account(
            "a@example.test",
            Utc::now() - chrono::Duration::minutes(5),
        )]);
        let attempt = provider
            .fetch_handle(&CredentialHandle::new("plugin:a@example.test"))
            .await;
        let usage = attempt.usage.expect("sidebar reading");
        assert!(
            (super::tests::pool_window(&usage, "gemini-weekly").used_percent - 6.1696).abs()
                < 1e-10
        );
        assert!(attempt.value_observed_at.is_some());
    }

    #[tokio::test]
    async fn a_paid_tier_account_without_a_local_session_publishes_nothing() {
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![paid_account(Some("a@example.test"))]);
        let error = provider
            .fetch_handle(&CredentialHandle::new("plugin:a@example.test"))
            .await
            .usage
            .expect_err("without a reading the plugin lane is unavailable");
        assert!(matches!(error, FetchError::LocalSourceUnavailable(_)));
        assert_eq!(classify(&error), FetchClass::Transient);
    }

    fn plugin_home(label: &str) -> crate::tests::ResetTempDir {
        let home = crate::tests::ResetTempDir::with_prefix("antigravity-store", label);
        std::fs::create_dir_all(home.path().join(".config/opencode")).unwrap();
        home
    }

    pub(super) fn sidebar_fixture(
        label: &str,
        accounts: Value,
    ) -> (AntigravityProvider, crate::tests::ResetTempDir) {
        let home = plugin_home(label);
        let mut provider = AntigravityProvider::new();
        let path = home.path().to_path_buf();
        provider.env_lookup = Arc::new(move |key| match key {
            "HOME" | "USERPROFILE" => Some(path.clone().into_os_string()),
            _ => None,
        });
        let sidebar = sidebar_path(&provider.env_lookup).unwrap();
        std::fs::create_dir_all(sidebar.parent().unwrap()).unwrap();
        std::fs::write(
            sidebar,
            serde_json::json!({
                "version": 2, "checkedAt": Utc::now().timestamp_millis(),
                "accounts": accounts, "activeRouting": {}, "routingAuthoritative": false
            })
            .to_string(),
        )
        .unwrap();
        provider.set_local_endpoints(Vec::new());
        (provider, home)
    }

    #[test]
    fn an_absent_sidebar_without_plugin_evidence_is_empty() {
        let (provider, _home) = sidebar_fixture("absent", serde_json::json!([]));
        std::fs::remove_file(sidebar_path(&provider.env_lookup).unwrap()).unwrap();
        assert!(provider.stored_accounts().unwrap().is_empty());
    }

    #[test]
    fn an_absent_sidebar_with_plugin_evidence_is_unavailable_not_empty() {
        for (label, evidence) in [
            ("legacy", ACCOUNTS_FILE),
            ("migrated", MIGRATED_STORE_POINTER),
        ] {
            let (provider, home) = sidebar_fixture(label, serde_json::json!([]));
            std::fs::remove_file(sidebar_path(&provider.env_lookup).unwrap()).unwrap();
            // Legacy account and migration-pointer files are installation evidence
            // only; their contents must never be opened or parsed by this reader.
            std::fs::write(
                home.path().join(evidence),
                "credential store must not be read",
            )
            .unwrap();
            let error = provider
                .stored_accounts()
                .expect_err("installed plugin is not an empty roster");
            assert!(error.contains("sidebar state is absent"), "{error}");
            assert!(provider.handles().is_err());
        }
    }

    #[test]
    fn an_unreadable_sidebar_is_unavailable_not_empty() {
        let (provider, _home) = sidebar_fixture("unreadable", serde_json::json!([]));
        let path = sidebar_path(&provider.env_lookup).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(path).unwrap();
        assert!(provider
            .stored_accounts()
            .unwrap_err()
            .contains("unreadable"));
    }

    #[test]
    fn malformed_sidebar_json_is_unavailable_not_empty() {
        let (provider, _home) = sidebar_fixture("malformed", serde_json::json!([]));
        std::fs::write(sidebar_path(&provider.env_lookup).unwrap(), "{not json").unwrap();
        assert!(provider
            .stored_accounts()
            .unwrap_err()
            .contains("did not parse"));
    }

    #[test]
    fn unsupported_sidebar_versions_require_a_plugin_update() {
        let (provider, _home) = sidebar_fixture("version", serde_json::json!([]));
        for version in [
            serde_json::json!(1),
            serde_json::json!(3),
            serde_json::json!(2.5),
            serde_json::json!("2"),
        ] {
            std::fs::write(
                sidebar_path(&provider.env_lookup).unwrap(),
                serde_json::json!({
                    "version": version, "accounts": []
                })
                .to_string(),
            )
            .unwrap();
            let error = provider
                .stored_accounts()
                .expect_err("only integer major 2 is supported");
            assert!(error.contains(&format!("version {version}")), "{error}");
            assert!(error.contains("update the plugin"), "{error}");
            std::fs::write(
                sidebar_path(&provider.env_lookup).unwrap(),
                serde_json::json!({ "version": version, "accounts": "different schema" })
                    .to_string(),
            )
            .unwrap();
            let error = provider.stored_accounts().unwrap_err();
            assert!(error.contains(&format!("version {version}")), "{error}");
            assert!(error.contains("update the plugin"), "{error}");
        }
    }

    #[test]
    fn sidebar_v2_enumerates_email_handles_without_reading_credentials() {
        let (provider, home) = sidebar_fixture(
            "v2",
            serde_json::json!([
                { "id": "vault-acct-0", "label": "Account 1", "email": "a@example.test", "enabled": true, "quota": {} },
                { "id": "vault-acct-1", "label": "Account 2", "email": " b@example.test ", "quota": {} }
            ]),
        );
        std::fs::write(
            home.path().join(ACCOUNTS_FILE),
            "unparseable credential store",
        )
        .unwrap();
        assert_eq!(
            provider.handles().unwrap(),
            vec![
                CredentialHandle::new("plugin:a@example.test"),
                CredentialHandle::new("plugin:b@example.test")
            ]
        );
    }

    #[test]
    fn sidebar_accounts_without_nonempty_email_are_skipped() {
        let (provider, _home) = sidebar_fixture(
            "no-email",
            serde_json::json!([
                { "id": "acct-0", "label": "not-an-identity", "enabled": true, "quota": {} },
                { "id": "acct-1", "email": "   ", "enabled": true, "quota": {} }
            ]),
        );
        assert!(provider.stored_accounts().unwrap().is_empty());
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    #[test]
    fn disabled_sidebar_accounts_are_skipped() {
        let (provider, _home) = sidebar_fixture(
            "disabled",
            serde_json::json!([
                { "id": "acct-0", "email": "off@example.test", "enabled": false, "quota": {} },
                { "id": "acct-1", "email": "on@example.test", "enabled": true, "quota": {} }
            ]),
        );
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::new("plugin:on@example.test")]
        );
    }

    #[test]
    fn sidebar_path_resolution_uses_override_then_xdg_then_home() {
        let lookup = |vars: &[(&str, &str)]| -> EnvLookup {
            let vars: Vec<_> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), std::ffi::OsString::from(v)))
                .collect();
            Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
        };
        assert_eq!(
            sidebar_path(&lookup(&[
                (SIDEBAR_STATE_ENV, "/override/full.json"),
                ("XDG_STATE_HOME", "/state"),
                ("HOME", "/home")
            ]))
            .unwrap(),
            std::path::Path::new("/override/full.json")
        );
        assert_eq!(
            sidebar_path(&lookup(&[("XDG_STATE_HOME", "/state"), ("HOME", "/home")])).unwrap(),
            std::path::Path::new("/state").join(SIDEBAR_STATE_RELATIVE)
        );
        // A drive-less "/home" is relative on Windows, where the home resolver
        // rightly refuses it, so the home case uses an absolute path on any host.
        let home = std::env::temp_dir().join("sidebar-home");
        let home = home.to_str().expect("temp dir is UTF-8");
        assert_eq!(
            sidebar_path(&lookup(&[("HOME", home), ("USERPROFILE", home)])).unwrap(),
            std::path::Path::new(home)
                .join(".local/state")
                .join(SIDEBAR_STATE_RELATIVE)
        );
    }

    /// A sidebar read error must not enumerate an empty roster and prune accounts.
    #[test]
    fn an_unreadable_plugin_sidebar_fails_enumeration() {
        let mut provider = AntigravityProvider::new();
        provider.set_override_store_unreadable("moved");
        assert!(
            provider.handles().is_err(),
            "an error keeps the existing slots; an empty set would reap them"
        );
    }

    /// A plugin account whose sidebar can't be read keeps its last reading.
    #[tokio::test]
    async fn a_plugin_account_with_an_unreadable_sidebar_stays_transient() {
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_store_unreadable("moved");
        let error = provider
            .fetch_handle(&CredentialHandle::Named("plugin:a@example.test".into()))
            .await
            .usage
            .expect_err("no reading without the sidebar");
        assert!(
            matches!(error, FetchError::LocalSourceUnavailable(_)),
            "must not read as signed out (credential_absent), got {error:?}"
        );
    }

    /// A paid account with no local session serves its fresh plugin cache.
    #[tokio::test]
    async fn a_paid_tier_account_without_a_local_session_serves_the_fresh_cache() {
        let cache_time = Utc::now() - chrono::Duration::minutes(5);
        let cache_time = DateTime::<Utc>::from_timestamp_millis(cache_time.timestamp_millis())
            .expect("fixture timestamp");
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![cached_paid_account(
            "beatricelau0414@gmail.com",
            cache_time,
        )]);

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let usage = attempt.usage.as_ref().expect("fresh cache must serve");
        let weekly = usage
            .extra_rate_windows
            .as_ref()
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|window| window.id.as_deref() == Some("gemini-weekly"))
            })
            .and_then(|window| window.window.as_ref())
            .expect("gemini weekly cache window");

        assert_eq!(weekly.resets_at.as_deref(), Some("2026-09-17T18:41:37Z"));
        assert_eq!(
            attempt.value_observed_at,
            Some(cache_time),
            "the cache lane is the only lane carrying the plugin's value-time"
        );
    }

    /// A cache-served value keeps the cache's own read time on the published entry.
    #[tokio::test]
    async fn a_cache_served_entry_publishes_the_cache_timestamp_as_fetched_at() {
        let cache_time = Utc::now() - chrono::Duration::minutes(5);
        let cache_time = DateTime::<Utc>::from_timestamp_millis(cache_time.timestamp_millis())
            .expect("fixture timestamp");
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![cached_paid_account(
            "beatricelau0414@gmail.com",
            cache_time,
        )]);
        let registry = crate::Registry::new(vec![Box::new(provider)]);

        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let entries = registry.get_usage(Some(PROVIDER_NAME)).await;
        let entry = entries.first().expect("cache-served entry");

        assert_eq!(
            entry.fetched_at.as_deref(),
            Some(
                cache_time
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false)
                    .as_str()
            ),
            "fetchedAt must say when the plugin read the value, not when we read its cache"
        );
    }

    /// A cache older than the bound is ignored and a paid account stays withheld.
    #[tokio::test]
    async fn a_cache_older_than_one_hour_is_not_served_for_a_paid_tier_account() {
        let stale_time = Utc::now()
            - chrono::Duration::from_std(CACHED_QUOTA_MAX_AGE).expect("cache age")
            - chrono::Duration::seconds(1);
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![cached_paid_account(
            "beatricelau0414@gmail.com",
            stale_time,
        )]);

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let error = attempt
            .usage
            .expect_err("a stale cache must fall through to paid-tier withholding");

        assert!(matches!(error, FetchError::LocalSourceUnavailable(_)));
        assert_eq!(attempt.value_observed_at, None);
    }

    /// One account's fresh cache is never served under another email's row.
    #[tokio::test]
    async fn the_cached_quota_is_matched_to_the_handle_by_email() {
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![
            cached_paid_account(
                "account-a@example.com",
                Utc::now() - chrono::Duration::minutes(5),
            ),
            paid_account(Some("account-b@example.com")),
        ]);

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:account-b@example.com".into(),
            ))
            .await;
        let error = attempt
            .usage
            .expect_err("account A's cache must not be published for account B");

        assert!(matches!(error, FetchError::LocalSourceUnavailable(_)));
        assert_eq!(attempt.value_observed_at, None);
    }

    #[tokio::test]
    async fn a_free_tier_plugin_account_serves_the_sidebar_without_cloud_credentials() {
        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![cached_free_account(
            "free@example.test",
            Utc::now() - chrono::Duration::minutes(5),
        )]);
        let attempt = provider
            .fetch_handle(&CredentialHandle::new("plugin:free@example.test"))
            .await;
        let usage = attempt.usage.expect("the plugin polls free accounts too");
        assert!(
            (super::tests::pool_window(&usage, "gemini-weekly").used_percent - 6.1696).abs()
                < 1e-10
        );
        assert!(attempt.value_observed_at.is_some());
    }

    #[tokio::test]
    async fn sidebar_fraction_precision_is_preserved_not_the_rounded_percent() {
        let mut account = sidebar_account_json("a@example.test", Utc::now().timestamp_millis());
        account["quota"]["gemini"]["windows"][1]["remainingFraction"] =
            serde_json::json!(0.44850576);
        account["quota"]["gemini"]["windows"][1]["remainingPercent"] = serde_json::json!(45);
        let (provider, _home) = sidebar_fixture("precision", serde_json::json!([account]));
        let attempt = provider
            .fetch_handle(&CredentialHandle::new("plugin:a@example.test"))
            .await;
        let usage = attempt.usage.unwrap();
        assert!(
            (super::tests::pool_window(&usage, "gemini-weekly").used_percent - 55.149424).abs()
                < 1e-10
        );
        assert_eq!(
            attempt.account_info.and_then(|info| info.email).as_deref(),
            Some("a@example.test")
        );
    }

    #[tokio::test]
    async fn undated_sidebar_groups_are_not_served_even_with_fresh_publication() {
        let mut account = sidebar_account_json("a@example.test", Utc::now().timestamp_millis());
        for group in ["gemini", "non-gemini"] {
            account["quota"][group]
                .as_object_mut()
                .unwrap()
                .remove("checkedAt");
        }
        let (provider, _home) = sidebar_fixture("undated", serde_json::json!([account]));
        let attempt = provider
            .fetch_handle(&CredentialHandle::new("plugin:a@example.test"))
            .await;
        let error = attempt
            .usage
            .expect_err("publication and window clocks cannot date a group");
        assert!(matches!(error, FetchError::LocalSourceUnavailable(_)));
        assert_eq!(classify(&error), FetchClass::Transient);
        assert_eq!(attempt.value_observed_at, None);
    }

    #[tokio::test]
    async fn a_stale_sidebar_group_is_omitted_while_its_fresh_sibling_is_served() {
        let now = Utc::now();
        let mut account = sidebar_account_json("a@example.test", now.timestamp_millis());
        account["quota"]["gemini"]["checkedAt"] =
            serde_json::json!((now - chrono::Duration::seconds(3601)).timestamp_millis());
        let (provider, _home) = sidebar_fixture("fresh-sibling", serde_json::json!([account]));
        let attempt = provider
            .fetch_handle(&CredentialHandle::new("plugin:a@example.test"))
            .await;
        let usage = attempt.usage.unwrap();
        let ids: Vec<_> = usage
            .extra_rate_windows
            .unwrap()
            .into_iter()
            .map(|window| window.id.unwrap())
            .collect();
        assert_eq!(ids, ["3p-5h", "3p-weekly"]);
        assert_eq!(
            attempt.value_observed_at.unwrap().timestamp_millis(),
            now.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn sidebar_value_time_is_the_oldest_served_group_observation() {
        let newest = Utc::now() - chrono::Duration::minutes(1);
        let oldest = newest - chrono::Duration::minutes(4);
        let mut account = sidebar_account_json("a@example.test", newest.timestamp_millis());
        account["quota"]["gemini"]["checkedAt"] = serde_json::json!(oldest.timestamp_millis());
        let (provider, _home) = sidebar_fixture("oldest", serde_json::json!([account]));
        let attempt = provider
            .fetch_handle(&CredentialHandle::new("plugin:a@example.test"))
            .await;
        assert_eq!(attempt.usage.unwrap().extra_rate_windows.unwrap().len(), 4);
        assert_eq!(
            attempt.value_observed_at.unwrap().timestamp_millis(),
            oldest.timestamp_millis()
        );
    }

    #[tokio::test]
    async fn sidebar_windows_without_fractions_or_stable_keys_are_not_invented() {
        let mut account = sidebar_account_json("a@example.test", Utc::now().timestamp_millis());
        account["quota"]
            .as_object_mut()
            .unwrap()
            .remove("non-gemini");
        account["quota"]["gemini"]["windows"][0]
            .as_object_mut()
            .unwrap()
            .remove("remainingFraction");
        account["quota"]["gemini"]["windows"][1]["window"] = serde_json::json!("7d");
        let (provider, _home) = sidebar_fixture("missing-fraction", serde_json::json!([account]));
        assert!(matches!(
            provider
                .fetch_handle(&CredentialHandle::new("plugin:a@example.test"))
                .await
                .usage,
            Err(FetchError::LocalSourceUnavailable(_))
        ));
    }

    /// Implicit handle still serves the local lane with no identity to match.
    #[tokio::test]
    async fn implicit_handle_still_serves_the_local_lane_with_no_identity_to_match() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == QUOTA_SUMMARY_PATH {
                (200, LOCAL_MOCK_QUOTA.as_bytes().to_vec())
            } else if path == USER_STATUS_PATH {
                (200, br#"{"userStatus":{}}"#.to_vec())
            } else {
                (404, b"not found".to_vec())
            }
        })
        .await;

        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(vec![(
            LocalServer {
                pid: 100,
                csrf_token: String::new(),
            },
            port,
        )]);
        provider.set_override_accounts(Vec::new());

        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        let usage = attempt.usage.expect("usage must be present");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");

        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-10T18:41:37Z"));
        assert_eq!(primary.used_percent, 58.4);
        assert_eq!(
            attempt
                .observed
                .as_ref()
                .and_then(|o| o.account_id.as_deref()),
            None
        );
    }

    /// A lane switch is announced; a steady lane is silent.
    ///
    /// The silent arm is the load-bearing one. Announcing every answer would be a
    /// line a minute per account forever, which is the cadence at which logs stop
    /// being read -- and the first answer after a restart is not a switch, so
    /// logging it would put a line per account in every startup and make a real
    /// switch harder to see rather than easier.
    #[test]
    fn a_lane_switch_is_noted_once_and_a_steady_lane_is_silent() {
        let provider = AntigravityProvider::new();

        // First answer: nothing to compare against, so no switch.
        provider.note_lane("a@example.com", "plugin-cache");
        assert_eq!(
            provider.served_lane.lock().unwrap().get("a@example.com"),
            Some(&"plugin-cache")
        );

        // Same lane again: still recorded, still not a switch.
        provider.note_lane("a@example.com", "plugin-cache");
        assert_eq!(
            provider.served_lane.lock().unwrap().get("a@example.com"),
            Some(&"plugin-cache")
        );

        // A switch updates the record. The log line itself is stderr and not
        // asserted here; what is asserted is the state that decides whether one
        // is emitted at all.
        provider.note_lane("a@example.com", "local-probe");
        assert_eq!(
            provider.served_lane.lock().unwrap().get("a@example.com"),
            Some(&"local-probe")
        );

        // PER ACCOUNT, NOT GLOBAL: one account switching must not make another
        // account's next answer look like a switch. A single global slot would
        // announce a switch on every poll of a two-account host.
        provider.note_lane("b@example.com", "cloud-api");
        assert_eq!(
            provider.served_lane.lock().unwrap().get("a@example.com"),
            Some(&"local-probe")
        );
        assert_eq!(provider.served_lane.lock().unwrap().len(), 2);
    }

    /// A provider whose process scan is `command`, with no endpoints and no
    /// plugin accounts, so the implicit handle runs real discovery against it.
    #[cfg(unix)]
    fn provider_scanning_with(command: BoundedCommand) -> AntigravityProvider {
        let mut provider = AntigravityProvider::new();
        provider.process_scan = Some(command);
        provider.set_override_accounts(Vec::new());
        provider
    }

    #[cfg(unix)]
    fn local_unavailable_reason(attempt: FetchAttempt) -> String {
        match attempt.usage {
            Err(FetchError::LocalSourceUnavailable(reason)) => reason,
            other => panic!("expected LocalSourceUnavailable, got {other:?}"),
        }
    }

    /// A process scan that could not look reaches the provider as
    /// could-not-look, never as "no Antigravity running".
    ///
    /// The two read alike on the wire (both `local_source_unavailable`), so the
    /// REASON is what is asserted: "no server running" tells a reader the editor
    /// is closed, which a stalled process table does not establish. The first
    /// fetch's scan outlives its timeout; a second provider then finds the stuck
    /// scan still running and must report the skip without starting another.
    ///
    /// Unix only: the stand-in scans are `/usr/bin/true` and `/bin/sleep`, which
    /// a Windows CI runner does not have.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_scan_that_could_not_look_is_never_reported_as_no_server_running() {
        // Control: a scan that RUNS and lists nothing is "no server running".
        // Without it, the assertions below could pass because that message had
        // simply changed.
        static EMPTY: Gate = Gate::new("empty process list");
        let empty = provider_scanning_with(BoundedCommand::new(
            &EMPTY,
            "/usr/bin/true",
            Vec::<String>::new(),
            Duration::from_secs(5),
        ));
        let reason =
            local_unavailable_reason(empty.fetch_handle(&CredentialHandle::implicit()).await);
        assert!(
            reason.contains("process running"),
            "an empty process list is the no-server case: {reason}"
        );

        static STUCK: Gate = Gate::new("stuck process scan");
        let stuck = BoundedCommand::new(&STUCK, "/bin/sleep", ["2"], Duration::from_millis(200));

        let timed_out = provider_scanning_with(stuck.clone());
        let reason =
            local_unavailable_reason(timed_out.fetch_handle(&CredentialHandle::implicit()).await);
        assert!(
            reason.contains("timed out") && !reason.contains("process running"),
            "a timed-out scan must say it could not look: {reason}"
        );

        let skipped = provider_scanning_with(stuck);
        let reason =
            local_unavailable_reason(skipped.fetch_handle(&CredentialHandle::implicit()).await);
        assert!(
            reason.contains("still running") && !reason.contains("process running"),
            "a skipped scan must say it could not look: {reason}"
        );
        assert_eq!(
            STUCK.spawns(),
            1,
            "one fetch runs one scan, and the skipped one starts none"
        );
    }
}
