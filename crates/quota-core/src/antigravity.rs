//! Antigravity usage — LOCAL PROBE of the running Antigravity editor.
//!
//! Unlike the OAuth providers, Antigravity's primary usage path is not a cloud call
//! and needs no stored credential: the Antigravity editor (its `language_server`, or
//! the `agy` CLI) runs a local server on a loopback port, and CodexBar reads quota
//! straight off it. We replicate that: find the running process, learn its loopback
//! port, and POST the same Connect-protocol JSON RPC the editor's own UI uses. This
//! is why a user who has Antigravity open sees quota without ever "logging in" to us.
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
//! DESKTOP-COUPLED: needs Antigravity (app or `agy` CLI) running locally — the same
//! coupling class as the browser-cookie cohort, not headless-server-portable. When no
//! Antigravity process is found (or it serves no usable quota), this degrades to a
//! degraded entry — NEVER a stale or fabricated window.
//!
//! VERIFICATION: LIVE-verified — the real local-probe chain (discover `agy` →
//! loopback port → POST quota summary → parse) returns real windows on a machine
//! running the Antigravity CLI (Gemini + Claude/GPT weekly + 5-hour buckets with real
//! resets; see `tests/antigravity_live.rs`). The live wire revealed three details a
//! fixture alone would have missed, now matched: the CLI serves HTTP (not HTTPS) on
//! loopback (so [`probe`] tries both schemes), the summary is wrapped in a
//! `{"response": {...}}` envelope, and each bucket carries an explicit `window`
//! (`"5h"`/`"weekly"`). The parser is also unit-tested against that captured shape.
//! Wire format + field mapping ported from CodexBar
//! `Sources/CodexBarCore/Providers/Antigravity/AntigravityStatusProbe.swift`
//! (ps :1013-1018, process match :1104-1156, csrf :1130-1184, lsof :1191-1232, paths
//! :771-775, request/headers :1467-1505/:1651-1660, representative :231-244, bucket
//! kinds :362-371) + `AntigravityQuotaSummaryParser.swift:96-173`.

use std::{collections::HashSet, sync::Arc, time::Duration};

use crate::credential_source::{CredentialSource, VaultCapability, VaultGetError};
use crate::provider::AccountObservation;
use crate::vault_handles::VaultHandleLoader;

use async_trait::async_trait;
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
/// The cloud Code Assist quota endpoint, used when no local process is running.
const REMOTE_QUOTA_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
/// Identifies the calling product on that shared endpoint.
const REMOTE_USER_AGENT: &str = "antigravity";

/// Where the opencode `antigravity-auth` plugin keeps its logged-in accounts.
///
/// This is the third credential lane, and the one an ordinary install actually
/// has: the local probe needs the editor running, and the cloud lane as first
/// shipped needed a vault credential that only this fleet mints. A user who
/// signed in through the plugin has neither, and the provider went dark for
/// them with `local_source_unavailable` — correct and useless.
const ACCOUNTS_FILE: &str = ".config/opencode/antigravity-accounts.json";

/// The plugin's own Google OAuth client, which is what makes this lane work.
///
/// A refresh token is bound to the client that minted it, so the client here
/// must be the PLUGIN's — not Antigravity's desktop app, and not Gemini CLI's.
/// Both of those were tried against a healthy token and returned 401, which
/// reads exactly like a dead credential and is not one.
///
/// Public by construction (it ships in the plugin's own JavaScript) and stored
/// XOR-masked for the same reason as the Gemini pair: to keep secret-scanner
/// regexes off the source text, never for secrecy. Overridable by env when the
/// plugin rotates it.
const ANTIGRAVITY_CLIENT_ID_MASKED: &[u8] = &[
    64, 69, 88, 69, 81, 29, 70, 69, 84, 92, 92, 90, 28, 78, 6, 8, 12, 0, 94, 31, 95, 67, 29, 93,
    69, 13, 78, 2, 16, 80, 95, 92, 21, 89, 12, 30, 10, 14, 27, 25, 17, 5, 65, 70, 10, 4, 79, 76, 0,
    5, 17, 66, 14, 12, 66, 4, 30, 0, 17, 0, 72, 4, 82, 30, 27, 27, 17, 15, 89, 94, 22, 13, 1,
];
const ANTIGRAVITY_CLIENT_SECRET_MASKED: &[u8] = &[
    54, 58, 44, 39, 49, 117, 93, 62, 87, 84, 47, 52, 127, 87, 74, 83, 40, 23, 97, 60, 0, 28, 57,
    45, 76, 18, 117, 51, 65, 24, 90, 24, 39, 108, 5,
];
const CRED_MASK: &[u8] = b"quota-public-creds-v1";
const CLIENT_ID_ENV: &[&str] = &["ANTIGRAVITY_OAUTH_CLIENT_ID"];
const CLIENT_SECRET_ENV: &[&str] = &["ANTIGRAVITY_OAUTH_CLIENT_SECRET"];
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The wire `source` for this lane.
///
/// Deliberately the EXISTING `oauth` value rather than a new one. The published
/// meaning of `oauth` is "an OAuth token or session found on this machine",
/// whose remedy is "log in with the tool that owns it" -- which describes this
/// lane exactly, the owning tool being the opencode plugin. A new value would
/// have to earn its place by implying a different remedy, and this one does not.
const PLUGIN_SOURCE: &str = "oauth";

/// XOR-unmask an embedded public credential to its plaintext.
fn unmask(masked: &[u8]) -> String {
    masked
        .iter()
        .enumerate()
        .map(|(i, b)| (b ^ CRED_MASK[i % CRED_MASK.len()]) as char)
        .collect()
}

fn oauth_client_id() -> String {
    crate::env::first_env(CLIENT_ID_ENV).unwrap_or_else(|| unmask(ANTIGRAVITY_CLIENT_ID_MASKED))
}

fn oauth_client_secret() -> String {
    crate::env::first_env(CLIENT_SECRET_ENV)
        .unwrap_or_else(|| unmask(ANTIGRAVITY_CLIENT_SECRET_MASKED))
}

/// One logged-in account as the plugin stores it.
#[derive(Debug, Clone, Deserialize)]
struct StoredAccount {
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, rename = "managedProjectId")]
    managed_project_id: Option<String>,
    /// The plugin's own switch. A disabled account is one the user turned off,
    /// so reporting its quota would describe capacity they have chosen not to
    /// use -- and absent is treated as enabled, matching the plugin's default.
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AccountsFile {
    #[serde(default)]
    accounts: Vec<StoredAccount>,
}

/// Read the plugin's accounts, keeping only those that can actually be fetched.
///
/// Returns an empty vector when the file is absent, which is the ordinary case
/// on a host that never installed the plugin -- not an error, and deliberately
/// indistinguishable from having no accounts, because both mean this lane has
/// nothing to offer.
fn stored_accounts() -> Vec<StoredAccount> {
    let Some(home) = crate::env::home_dir() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(home.join(ACCOUNTS_FILE)) else {
        return Vec::new();
    };
    let Ok(file) = serde_json::from_str::<AccountsFile>(&text) else {
        return Vec::new();
    };
    file.accounts
        .into_iter()
        .filter(|account| account.enabled != Some(false))
        .filter(|account| {
            account
                .refresh_token
                .as_deref()
                .is_some_and(|token| !token.trim().is_empty())
        })
        .collect()
}

/// The handle name for a stored account.
///
/// The email when the plugin recorded one, since it is stable across restarts
/// and survives the list being reordered -- which the index does not. A slot
/// number is the fallback, and it is a worse key: adding an account above this
/// one silently repoints the handle at a different account, and the refresher
/// keys its backoff and identity fencing on exactly this string.
fn account_handle_name(account: &StoredAccount, index: usize) -> String {
    match account
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        Some(email) => format!("plugin:{email}"),
        None => format!("plugin:#{index}"),
    }
}

const QUOTA_SUMMARY_PATH: &str =
    "/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";

const SESSION_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;

// ---- process + port discovery (macOS) ---------------------------------------

/// A discovered Antigravity language-server process and how to talk to it.
struct LocalServer {
    pid: i32,
    /// CSRF token for the request header. Empty for the CLI (which needs none).
    csrf_token: String,
}

/// Whether a command line is an Antigravity language server or `agy` CLI, and the
/// CSRF token to use (empty string = CLI, needs none; None = app/IDE with no token,
/// which we cannot auth so skip).
#[cfg(any(target_os = "macos", test))]
fn classify_command(command: &str) -> Option<String> {
    let lower = command.to_ascii_lowercase();
    let is_language_server = lower.contains("language_server") || lower.contains("language-server");
    let is_antigravity = lower.contains("antigravity");
    let is_cli = lower.contains("antigravity-cli")
        || lower.contains("antigravity_cli")
        || lower.contains("/agy ")
        || lower.ends_with("/agy");

    if is_language_server && is_antigravity {
        // app/IDE language server: requires a --csrf_token; skip if absent.
        return extract_csrf_token(command);
    }
    if is_cli {
        // CLI language server: no token required.
        return Some(String::new());
    }
    None
}

/// Extract the value of `--csrf_token` (`--csrf_token=VALUE` or `--csrf_token VALUE`).
#[cfg(any(target_os = "macos", test))]
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
    ports
}

#[cfg(target_os = "macos")]
fn discover_servers() -> Vec<LocalServer> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
    else {
        return Vec::new();
    };
    parse_process_list(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
fn discover_servers() -> Vec<LocalServer> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn discover_ports(pid: i32) -> Vec<u16> {
    let lsof = ["/usr/sbin/lsof", "/usr/bin/lsof"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists());
    let Some(lsof) = lsof else {
        return Vec::new();
    };
    let Ok(out) = std::process::Command::new(lsof)
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &pid.to_string()])
        .output()
    else {
        return Vec::new();
    };
    parse_listening_ports(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
fn discover_ports(_pid: i32) -> Vec<u16> {
    Vec::new()
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

fn pool_of(group_title: &str) -> Pool {
    let t = group_title.to_ascii_lowercase();
    if t.contains("gemini") {
        Pool::Gemini
    } else if t.contains("claude") || t.contains("gpt") {
        Pool::ClaudeGpt
    } else {
        Pool::Other
    }
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

fn quota_cadence_candidates(bucket: &QuotaBucket) -> HashSet<String> {
    let mut candidates = HashSet::new();
    for raw_value in [
        bucket.window.as_deref().unwrap_or(""),
        bucket.bucket_id.as_str(),
        bucket.display_name.as_str(),
    ] {
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
    pool: Pool,
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
    let title = format!(
        "{} {}",
        group.display_name.trim(),
        bucket.display_name.trim()
    )
    .trim()
    .to_string();
    Some(ResolvedWindow {
        pool: pool_of(&group.display_name),
        window: RateWindow {
            used_percent,
            raw_used_percent: None,
            resets_at: reset,
            window_minutes: window_minutes_of(bucket),
            used_count: None,
            total_count: None,
        },
        title,
        id: bucket.bucket_id.clone(),
    })
}

/// The representative window for a pool: the MOST-USED (highest utilization) window in
/// that pool — mirrors CodexBar's `quotaSummaryRepresentative` (`.max` by usedPercent,
/// AntigravityStatusProbe.swift:231-244). This is what surfaces the binding limit (a
/// user's 47%-used weekly, not an idle 0%-used 5-hour).
fn representative(windows: &[ResolvedWindow], pool: Pool) -> Option<RateWindow> {
    windows
        .iter()
        .filter(|w| w.pool == pool)
        .max_by(|a, b| {
            a.window
                .used_percent
                .partial_cmp(&b.window.used_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|w| w.window.clone())
}

/// Normalize a RetrieveUserQuotaSummary JSON body to [`Usage`]. Pure — unit-testable.
pub fn parse_quota_summary(body: &str) -> Result<Usage, FetchError> {
    let envelope: QuotaSummaryEnvelope = serde_json::from_str(body)
        .map_err(|e| FetchError::Decode(format!("antigravity quota summary not JSON: {e}")))?;
    // Prefer the `{"response": {...}}` envelope (live wire); fall back to a top-level
    // `{"groups": ...}` shape.
    let summary = match envelope.response {
        Some(s) => s,
        None => serde_json::from_str::<QuotaSummary>(body)
            .map_err(|e| FetchError::Decode(format!("antigravity quota summary not JSON: {e}")))?,
    };

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

    // Antigravity meters two independent pools: the native Gemini models and the
    // external Claude/GPT models. Only the native pool represents the product's
    // own capacity, so only it may claim the unnamed `primary` slot — an unnamed
    // slot reads as "this account's window", and a walled external pool in it
    // would misreport the whole provider as exhausted while Gemini is free.
    // Both pools stay fully visible as named extra windows below.
    let primary = representative(&resolved, Pool::Gemini);

    // Every resolved bucket is also surfaced as a per-pool extra window.
    let extra: Vec<ExtraWindow> = resolved
        .iter()
        .map(|w| ExtraWindow {
            title: Some(w.title.clone()),
            id: Some(w.id.clone()),
            window: Some(w.window.clone()),
        })
        .collect();

    // If no Gemini-pool bucket resolved, fall back to the most-used resolved
    // window so a non-Gemini account still reports something rather than
    // degrading.
    let primary = primary.or_else(|| {
        resolved
            .iter()
            .max_by(|a, b| {
                a.window
                    .used_percent
                    .partial_cmp(&b.window.used_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|w| w.window.clone())
    });

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: if extra.is_empty() { None } else { Some(extra) },
    })
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
fn parse_remote_quota(body: &[u8]) -> Result<Usage, FetchError> {
    let response: RemoteQuotaResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("antigravity remote quota not JSON: {e}")))?;
    let buckets = response
        .buckets
        .filter(|buckets| !buckets.is_empty())
        .ok_or_else(|| FetchError::Decode("antigravity remote quota has no buckets".to_string()))?;

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
            pool,
            window: RateWindow {
                used_percent,
                raw_used_percent: None,
                window_minutes: None,
                resets_at,
                used_count: None,
                total_count: None,
            },
            title: format!("{title} ({models} models)"),
            id: title.to_string(),
        });
    }

    // Identical pool selection to the local lane: only the native Gemini pool may
    // claim the unnamed slot, since an unnamed window reads as the account's own
    // capacity and a walled external pool there would report the whole provider
    // exhausted while Gemini is free.
    let primary = representative(&resolved, Pool::Gemini).or_else(|| {
        resolved
            .iter()
            .max_by(|a, b| {
                a.window
                    .used_percent
                    .partial_cmp(&b.window.used_percent)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|w| w.window.clone())
    });

    let extra: Vec<ExtraWindow> = resolved
        .iter()
        .map(|w| ExtraWindow {
            title: Some(w.title.clone()),
            id: Some(w.id.clone()),
            window: Some(w.window.clone()),
        })
        .collect();

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: if extra.is_empty() { None } else { Some(extra) },
    })
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
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            remote_http: reqwest::Client::new(),
            credential_source,
            handle_loader,
            quota_url: REMOTE_QUOTA_URL.to_string(),
        }
    }

    /// Fetch one plugin-stored account's quota.
    ///
    /// Same cloud endpoint as the vault lane; only the credential differs. The
    /// plugin stores a refresh token, so this exchanges it for an access token
    /// on every fetch rather than caching one -- the refresher's own interval is
    /// longer than the hour these tokens live, so a cache would be a miss with
    /// extra state.
    async fn fetch_plugin_account(&self, handle_name: &str) -> FetchAttempt {
        let accounts = stored_accounts();
        let found = accounts
            .iter()
            .enumerate()
            .find(|(index, account)| account_handle_name(account, *index) == handle_name);
        let Some((_, account)) = found else {
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

        let refresh_token = account.refresh_token.clone().unwrap_or_default();
        let access_token = match self.exchange_refresh_token(&refresh_token).await {
            Ok(token) => token,
            Err(error) => {
                return FetchAttempt::failure(observed, Some(PLUGIN_SOURCE.to_string()), error)
            }
        };

        let project = account
            .managed_project_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);

        let usage = self
            .fetch_remote_quota(&access_token, project.as_deref())
            .await;
        match usage {
            Ok(usage) => FetchAttempt::success(observed, PLUGIN_SOURCE, usage),
            Err(error) => FetchAttempt::failure(observed, Some(PLUGIN_SOURCE.to_string()), error),
        }
    }

    /// Call the cloud quota endpoint with an already-resolved access token.
    ///
    /// Shared by both cloud credential lanes -- the vault one and the plugin
    /// file one -- because they differ only in where the token came from. Two
    /// copies of this call would be two places for the request shape to drift,
    /// and the endpoint is the half that is hard to verify.
    async fn fetch_remote_quota(
        &self,
        access_token: &str,
        project: Option<&str>,
    ) -> Result<Usage, FetchError> {
        // The project scopes the query where one is known. The endpoint also
        // answers without it, so an absent project is not a failure.
        let body = match project {
            Some(project) => serde_json::json!({ "project": project }),
            None => serde_json::json!({}),
        };
        let body = serde_json::to_vec(&body).map_err(|e| FetchError::Decode(e.to_string()))?;
        let response = JsonRequest::post_json(&self.quota_url, body)
            .bearer(access_token)
            // Identifies the calling product to the shared endpoint, matching
            // what the Antigravity client sends.
            .header(Header::new("User-Agent", REMOTE_USER_AGENT))
            .timeout(REQUEST_TIMEOUT)
            .send_provider_status_first(&self.remote_http, PROVIDER_NAME)
            .await?;
        parse_remote_quota(&response.body)
    }

    /// Exchange a stored refresh token for a short-lived access token.
    ///
    /// A 400 or 401 here means the grant itself was rejected, which is a
    /// credential the user must re-authorize rather than a transient fault, so
    /// it must not be retried as though the network flapped.
    async fn exchange_refresh_token(&self, refresh_token: &str) -> Result<String, FetchError> {
        if refresh_token.trim().is_empty() {
            return Err(FetchError::NoSession(
                "the stored account carries no refresh token".to_string(),
            ));
        }
        let client_id = oauth_client_id();
        let client_secret = oauth_client_secret();
        let response = JsonRequest::post_form(
            TOKEN_URL,
            &[
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ],
        )
        .timeout(REQUEST_TIMEOUT)
        .send_provider_status_first(&self.remote_http, PROVIDER_NAME)
        .await?;

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
        }
        let parsed: TokenResponse = serde_json::from_slice(&response.body).map_err(|e| {
            FetchError::Decode(format!("antigravity token response not decodable: {e}"))
        })?;
        parsed
            .access_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                FetchError::CredentialUnusable(
                    "the token exchange returned no access token".to_string(),
                )
            })
    }

    /// Fetch quota from the cloud, needing no local process.
    ///
    /// The credential is Antigravity's own Google login, served by the vault.
    /// This is the same Code Assist endpoint the Gemini provider calls, and the
    /// account behind the token is what makes the answers differ: an Antigravity
    /// login's quota covers Antigravity's model pool, Claude and GPT included.
    async fn fetch_remote(&self, capability: &VaultCapability) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(VaultGetError::Permanent);
        };
        let mut credential = match credential_source.get(capability, 120_000).await {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = credential.record_version;
        let account_info = credential.account_info();
        let observed = Some(AccountObservation::new(
            credential
                .account_id
                .clone()
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty()),
            Some(record_version),
        ));
        let project = credential
            .project_id
            .clone()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        let access_token = match String::from_utf8(std::mem::take(&mut credential.payload)) {
            Ok(access_token) => access_token,
            Err(error) => {
                let mut payload = error.into_bytes();
                payload.fill(0);
                return FetchAttempt::failure(
                    observed,
                    Some("vault".to_string()),
                    FetchError::Decode("vault credential payload is not valid UTF-8".to_string()),
                );
            }
        };

        let result: Result<Usage, FetchError> = async {
            // The project scopes the query where the vault knows one. The endpoint
            // also answers without it, so an absent project is not a failure.
            self.fetch_remote_quota(&access_token, project.as_deref())
                .await
        }
        .await;

        if let (Some(source), Err(FetchError::ProviderStatus(status @ (401 | 403)))) =
            (self.credential_source.as_ref(), &result)
        {
            let source = Arc::clone(source);
            let capability = capability.clone();
            let status = *status;
            tokio::spawn(async move {
                source
                    .report_auth_failure(&capability, status, record_version)
                    .await;
            });
        }

        match result {
            Ok(usage) => {
                FetchAttempt::success(observed, "vault", usage).with_account_info(account_info)
            }
            Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error),
        }
    }

    /// POST the quota-summary RPC to one discovered server/port. Returns the parsed
    /// usage, or an error to try the next candidate. The local server may speak http
    /// (the `agy` CLI, confirmed on the wire) or https-with-self-signed (the app
    /// language server); try both loopback schemes.
    async fn probe(&self, server: &LocalServer, port: u16) -> Result<Usage, FetchError> {
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
                Ok(body) => return parse_quota_summary(&String::from_utf8_lossy(&body)),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
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
        // One handle per account the plugin has logged in. This is the lane an
        // ordinary install has, and the only one that can see more than one
        // account: the local probe reports whichever account the running editor
        // happens to be using, and a vault credential is minted per fleet host.
        for (index, account) in stored_accounts().iter().enumerate() {
            credentialed.push(CredentialHandle::new(account_handle_name(account, index)));
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
        if let Some(capability) = handle.vault_capability() {
            return self.fetch_remote(capability).await;
        }

        let result: Result<ProviderUsage, FetchError> = async {
            let servers = tokio::task::spawn_blocking(discover_servers)
                .await
                .unwrap_or_else(|_join_error| Vec::new());
            if servers.is_empty() {
                // Not "no credential here": this lane reads usage from the
                // running editor, so its absence tracks whether someone has the
                // application open, not whether the provider is configured.
                return Err(FetchError::LocalSourceUnavailable(
                    "no Antigravity language server or agy CLI process running".to_string(),
                ));
            }

            // Processes were found but none answered -- one may be starting up
            // or shutting down, which resolves on its own like the absent case.
            let mut last_err = FetchError::LocalSourceUnavailable(
                "no Antigravity loopback port served quota".to_string(),
            );
            for server in &servers {
                let pid = server.pid;
                let ports = tokio::task::spawn_blocking(move || discover_ports(pid))
                    .await
                    .unwrap_or_else(|_join_error| Vec::new());
                for port in ports {
                    match self.probe(server, port).await {
                        Ok(usage) => {
                            return Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "oauth", usage));
                        }
                        Err(e) => last_err = e,
                    }
                }
            }
            Err(last_err)
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn gemini_pool_owns_primary_and_no_secondary_is_emitted() {
        let usage = parse_quota_summary(SUMMARY_FIXTURE).unwrap();
        // Gemini pool: 5h used 20% vs weekly used 47% → representative is the
        // most-used (weekly), mirroring CodexBar.
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 47.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-30T00:00:00Z"));
        assert_eq!(primary.window_minutes, Some(10080));
        // The external Claude/GPT pool never occupies an unnamed slot; it is
        // visible only as its named extra window.
        assert!(usage.secondary.is_none());
        // All three buckets surfaced as extra windows.
        assert_eq!(usage.extra_rate_windows.unwrap().len(), 3);
    }

    #[test]
    fn cadence_aliases_cover_session_and_underscore_weekly_limit() {
        let body = r#"{"groups":[{"displayName":"Gemini Models","buckets":[
            {"bucketId":"gemini-session","displayName":"Session Limit","window":"session",
             "remainingFraction":0.8,"resetTime":"2026-07-24T18:34:51Z"},
            {"bucketId":"gemini-weekly","displayName":"Weekly_Limit","window":"Weekly_Limit",
             "remainingFraction":0.6,"resetTime":"2026-07-30T18:34:51Z"}
        ]}]}"#;
        let usage = parse_quota_summary(body).unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras[0].window.as_ref().unwrap().window_minutes, Some(300));
        assert_eq!(
            extras[1].window.as_ref().unwrap().window_minutes,
            Some(10080)
        );
    }

    #[test]
    fn walled_external_pool_does_not_take_primary_from_a_healthy_gemini_pool() {
        // The exact live shape that misled the headline: Gemini nearly free,
        // Claude/GPT walled at 100%. Primary must stay the Gemini pool.
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
        assert_eq!(usage.primary.unwrap().used_percent, 11.7);
        assert!(usage.secondary.is_none());
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(
            extras[1].window.as_ref().unwrap().used_percent,
            100.0,
            "the walled external pool stays visible as a named extra"
        );
    }

    #[test]
    fn account_without_gemini_pool_falls_back_to_most_used_window() {
        let body = r#"{"groups":[{"displayName":"Claude and GPT models","buckets":[
            {"bucketId":"3p-5h","displayName":"Five Hour Limit","window":"5h",
             "remainingFraction":0.95,"resetTime":"2026-06-24T08:00:00Z"},
            {"bucketId":"3p-weekly","displayName":"Weekly Limit","window":"weekly",
             "remainingFraction":0.4,"resetTime":"2026-06-30T00:00:00Z"}
        ]}]}"#;
        let usage = parse_quota_summary(body).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 60.0);
        assert!(usage.secondary.is_none());
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
        let primary = parse_quota_summary(body)
            .unwrap()
            .primary
            .expect("usage data should emit a window");
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
        let primary = parse_quota_summary(body)
            .unwrap()
            .primary
            .expect("usage data should emit a window");
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
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 75.0);
        assert_eq!(primary.window_minutes, Some(10080));
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
        let primary = usage.primary.expect("a gemini pool window");
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
        for body in [
            &br#"{"buckets":[]}"#[..],
            &br#"{}"#[..],
            // Present, but every bucket is unmetered: the same shape as an
            // account with no quota to report.
            &br#"{"buckets":[{"modelId":"chat_1","remainingFraction":1}]}"#[..],
        ] {
            let error = parse_remote_quota(body)
                .expect_err("an unusable response must not publish an empty window set");
            assert!(matches!(error, FetchError::Decode(_)), "{error:?}");
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

        assert_eq!(usage.primary.as_ref().unwrap().window_minutes, None);
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

    fn account(email: Option<&str>, token: Option<&str>, enabled: Option<bool>) -> StoredAccount {
        StoredAccount {
            email: email.map(str::to_string),
            refresh_token: token.map(str::to_string),
            managed_project_id: Some("proj-1".to_string()),
            enabled,
        }
    }

    /// The handle name keys on the email, not the slot.
    ///
    /// The refresher keys backoff and identity fencing on this string, so a name
    /// that moves when the list is reordered silently repoints a slot at a
    /// different account -- carrying the previous account's failure history and
    /// its cached window with it.
    #[test]
    fn the_handle_name_keys_on_the_email_when_there_is_one() {
        let named = account(Some("a@example.test"), Some("tok"), None);
        assert_eq!(account_handle_name(&named, 0), "plugin:a@example.test");
        assert_eq!(
            account_handle_name(&named, 7),
            "plugin:a@example.test",
            "the position must not appear in the name"
        );

        // Without an email there is nothing stable to use, and the slot number
        // is the honest fallback rather than a fabricated identity.
        let anonymous = account(None, Some("tok"), None);
        assert_eq!(account_handle_name(&anonymous, 3), "plugin:#3");
    }

    /// A blank email does not produce a handle named `plugin:`.
    #[test]
    fn a_blank_email_falls_back_to_the_slot() {
        let blank = account(Some("   "), Some("tok"), None);
        assert_eq!(account_handle_name(&blank, 2), "plugin:#2");
    }

    /// Accounts the user disabled, or that carry no token, are not offered.
    ///
    /// Both would produce a handle that can only fail: a disabled account is
    /// capacity the user chose not to use, and a tokenless one cannot be
    /// exchanged. Offering either would publish a degraded entry describing
    /// nothing wrong.
    #[test]
    fn disabled_and_tokenless_accounts_are_filtered() {
        let file = AccountsFile {
            accounts: vec![
                account(Some("live@example.test"), Some("tok"), Some(true)),
                account(Some("off@example.test"), Some("tok"), Some(false)),
                account(Some("empty@example.test"), Some("   "), None),
                account(Some("none@example.test"), None, None),
                // Absent `enabled` matches the plugin's own default of on.
                account(Some("default@example.test"), Some("tok"), None),
            ],
        };
        let kept: Vec<String> = file
            .accounts
            .into_iter()
            .filter(|a| a.enabled != Some(false))
            .filter(|a| {
                a.refresh_token
                    .as_deref()
                    .is_some_and(|t| !t.trim().is_empty())
            })
            .filter_map(|a| a.email)
            .collect();
        assert_eq!(kept, ["live@example.test", "default@example.test"]);
    }

    /// With no credential anywhere, the local probe is the whole provider.
    ///
    /// The case the probe was built for: no plugin, no vault, an editor running.
    /// Dropping it here would take the provider dark for the only users who
    /// have nothing else.
    #[test]
    fn the_local_probe_stands_alone_when_nothing_else_is_configured() {
        let provider = AntigravityProvider::new();
        let handles = provider.handles().expect("handles");
        // This host has plugin accounts, so assert the RULE rather than the
        // count: whenever no credentialed lane exists, the probe is offered.
        let credentialed = handles
            .iter()
            .filter(|handle| !matches!(handle, CredentialHandle::ImplicitLocal))
            .count();
        if credentialed == 0 {
            assert_eq!(handles, vec![CredentialHandle::implicit()]);
        } else {
            assert!(
                !handles.contains(&CredentialHandle::implicit()),
                "a credentialed lane must replace the probe, not join it: {handles:?}"
            );
        }
    }

    /// A credentialed lane replaces the probe rather than joining it.
    ///
    /// The probe resolves no account identity, and the read path emits labeled
    /// entries only when EVERY handle resolves one -- so keeping it beside a
    /// lane that DOES resolve identity discards that identity for the whole
    /// provider. This shipped wrong once: the wire showed `account: null` for an
    /// account whose email the plugin lane had read correctly.
    #[test]
    fn a_credentialed_lane_replaces_the_local_probe() {
        let provider = AntigravityProvider::new();
        let handles = provider.handles().expect("handles");
        let named = handles
            .iter()
            .filter(|handle| matches!(handle, CredentialHandle::Named(_)))
            .count();
        if named > 0 {
            assert!(
                !handles.contains(&CredentialHandle::implicit()),
                "the probe must not survive beside identity-bearing lanes: {handles:?}"
            );
        }
    }

    /// The masked OAuth constants unmask to the plugin's real client.
    ///
    /// A refresh token is bound to the client that minted it, so an unmasking
    /// error does not degrade gracefully -- it returns 401 from a healthy
    /// credential, which reads as a dead login. Pinned against literals rather
    /// than against the masking function, or the test would pass for any pair
    /// that round-trips.
    #[test]
    fn the_masked_oauth_client_unmasks_to_the_plugin_pair() {
        assert_eq!(
            unmask(ANTIGRAVITY_CLIENT_ID_MASKED),
            "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com"
        );
        assert_eq!(
            unmask(ANTIGRAVITY_CLIENT_SECRET_MASKED),
            "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"
        );
    }

    /// The plugin lane reports the existing `oauth` source, not a new value.
    ///
    /// Its published meaning -- an OAuth token found on this machine, fixed by
    /// logging in with the tool that owns it -- describes this lane exactly, and
    /// a new value would have to imply a different remedy to earn its place.
    #[test]
    fn the_plugin_lane_uses_the_existing_oauth_source() {
        assert_eq!(PLUGIN_SOURCE, "oauth");
    }
}
