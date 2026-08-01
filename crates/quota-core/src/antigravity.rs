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

use std::{collections::HashSet, time::Duration};

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

// ---- provider ---------------------------------------------------------------

/// The Antigravity usage provider (local-process probe).
pub struct AntigravityProvider {
    /// Loopback-only client: cert validation disabled because the editor's local
    /// server uses a self-signed cert. NEVER used for a non-loopback URL (guarded).
    http: reqwest::Client,
}

impl AntigravityProvider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http }
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

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let servers = tokio::task::spawn_blocking(discover_servers)
                .await
                .unwrap_or_else(|_join_error| Vec::new());
            if servers.is_empty() {
                return Err(FetchError::NoSession(
                    "no Antigravity language server or agy CLI process running".to_string(),
                ));
            }

            let mut last_err =
                FetchError::NoSession("no Antigravity loopback port served quota".to_string());
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

    // Envelope shape confirmed on the live `agy` wire: groups under `response`,
    // buckets carry an explicit `window` field.
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
}
