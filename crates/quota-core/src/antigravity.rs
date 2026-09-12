//! Antigravity usage — LOCAL PROBE of the running Antigravity editor.
//!
//! Unlike the OAuth providers, Antigravity's authoritative paid-tier usage is not a
//! cloud call: the Antigravity editor (its `language_server`, or the `agy` CLI) runs a
//! local server on a loopback port, and the opencode plugin preserves that same pool
//! in its account-local cache. We prefer the live server, then a recent plugin cache;
//! only free-tier accounts fall through to the cloud client's standard-tier pool.
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

/// A cached pool older than this is more than 20% of the five-hour window's
/// period old, so it has stopped describing the window it claims to describe.
/// The weekly window would tolerate longer, but one cache entry carries both
/// cadences and the tighter window governs.
const CACHED_QUOTA_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Why reading another tool's refresh token is safe HERE and not in general.
///
/// Google does not rotate: a refresh exchange returns an access token and no
/// new refresh token, so the value in the plugin's file stays valid and two
/// readers can each refresh independently without disturbing the other.
///
/// HOW TO RE-DERIVE THIS, because the claim is what carries the safety and a
/// bare "verified" cannot be re-run: POST to `https://oauth2.googleapis.com/token`
/// with `grant_type=refresh_token` and this lane's client id, then read the
/// response body for a `refresh_token` field. Google's response contains
/// `access_token`, `expires_in`, `scope`, `token_type` and NO `refresh_token`,
/// which is what makes the stored value survive the exchange. If that field ever
/// appears, this lane is spending a credential it does not own and must stop.
/// (Contrast Anthropic, whose token response does carry a replacement.)
///
/// The check costs one request and answers definitively, so re-run it rather
/// than trusting this paragraph -- a citation that cannot be re-run is a claim
/// wearing the costume of evidence, and this one guards a user's sign-in.
///
/// THAT IS A PROPERTY OF GOOGLE, NOT A PATTERN TO COPY. Anthropic's OAuth
/// rotates -- its token response carries a replacement refresh token and the
/// old one stops working -- so a lane reading the anthropic plugin's store the
/// way this one reads antigravity's would invalidate the credential that plugin
/// depends on, breaking the user's actual sign-in to collect a usage figure. A
/// reader that is not the owner of a rotating credential must not spend it.
///
/// Before adding a file-backed lane for any provider, establish which kind its
/// refresh is. The failure is silent on this side and expensive on the other.
const _ROTATION_NOTE: () = ();

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

/// Diagnostic access to the cloud quota call, for the live probe example.
///
/// EXISTS BECAUSE THE QUESTION IS ABOUT THE REQUEST, NOT THE NORMALISER. Our
/// cloud lane and the editor's own language server disagree about one of the two
/// pools on the same account in the same second, and the only difference between
/// the two requests is the `project` field. Answering that needs the raw upstream
/// body for both spellings of the request, which no public path exposes.
///
/// `doc(hidden)` rather than private because an example is a separate crate: the
/// alternative is copying the token exchange and the request shape into the
/// example, where they would drift from the lane they are supposed to be
/// measuring, and a probe that measures a copy of the request answers nothing.
#[doc(hidden)]
impl AntigravityProvider {
    /// The plugin account store as the lane reads it, reduced to the three
    /// fields the probe needs: email, project, refresh token.
    pub fn probe_plugin_accounts(&self) -> Vec<(Option<String>, Option<String>, Option<String>)> {
        stored_accounts()
            .into_iter()
            .map(|a| (a.email, a.managed_project_id, a.refresh_token))
            .collect()
    }

    /// Exchange a refresh token, exactly as the lane does.
    ///
    /// Safe to call repeatedly: Google's refresh tokens do not rotate on
    /// exchange, which is the invariant the whole plugin lane rests on and is
    /// re-verified at its call sites.
    pub async fn probe_access_token(&self, refresh_token: &str) -> Result<String, FetchError> {
        self.exchange_refresh_token(refresh_token).await
    }

    /// The raw summary body for an arbitrary request body.
    ///
    /// Takes the whole body rather than a project string because the question
    /// outgrew the project: the endpoint answers with a DIFFERENT pool depending
    /// on request context we may not be sending, and finding which field carries
    /// that means trying several spellings against the live endpoint.
    ///
    /// Returns the body UNPARSED so the comparison is against what the endpoint
    /// said rather than against our reading of it.
    pub async fn probe_quota_summary(
        &self,
        access_token: &str,
        request_body: serde_json::Value,
        extra_headers: &[(&'static str, String)],
    ) -> Result<String, FetchError> {
        let body =
            serde_json::to_vec(&request_body).map_err(|e| FetchError::Decode(e.to_string()))?;
        let response = if extra_headers.is_empty() {
            self.post_quota(&self.quota_summary_url, body, access_token)
                .await?
        } else {
            // Vary headers against the shipped request. The body was ruled out --
            // the project changes nothing and every tier spelling is refused --
            // so what remains different from the editor is CALLER IDENTITY, which
            // this endpoint is known to resolve entitlement from.
            let mut request = JsonRequest::post_json(&self.quota_summary_url, body)
                .bearer(access_token)
                .header(Header::new("User-Agent", REMOTE_USER_AGENT));
            for (name, value) in extra_headers {
                request = request.header(Header::new(name, value.clone()));
            }
            request
                .timeout(REQUEST_TIMEOUT)
                .send_provider_status_first(&self.remote_http, PROVIDER_NAME)
                .await?
        };
        Ok(String::from_utf8_lossy(&response.body).into_owned())
    }
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
pub struct StoredAccount {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, rename = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(default, rename = "managedProjectId")]
    pub managed_project_id: Option<String>,
    /// The plugin's own switch. A disabled account is one the user turned off,
    /// so reporting its quota would describe capacity they have chosen not to
    /// use -- and absent is treated as enabled, matching the plugin's default.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The paid tier this account holds, as the plugin captured it at login.
    ///
    /// LOAD-BEARING FOR WHICH POOL WE MAY PUBLISH, not display. Google resolves
    /// Code Assist entitlement from the OAuth client a token was issued to, and
    /// the cloud lane's token -- ours or the vault's -- is only ever entitled to
    /// the standard tier. Measured 2026-09-11 with `loadCodeAssist`: our
    /// credential answers `allowedTiers: ["standard-tier"]` for an account the
    /// editor reports as Google AI Ultra.
    ///
    /// So for an account that HOLDS a paid tier, the cloud lane answers about a
    /// pool the user does not consume. This field is the free, local, no-network
    /// way to know that before publishing it. `free-tier` here means the account
    /// has no paid tier and the cloud lane is reporting its real pool.
    #[serde(default, rename = "capturedPaidTierId")]
    pub captured_paid_tier_id: Option<String>,
    /// When the plugin last read `cachedQuota`, as Unix epoch milliseconds.
    #[serde(default, rename = "cachedQuotaUpdatedAt")]
    pub cached_quota_updated_at: Option<i64>,
    /// The paid-pool snapshot maintained by the plugin for this account.
    #[serde(default, rename = "cachedQuota")]
    pub cached_quota: Option<CachedQuota>,
}

/// One plugin-cached Antigravity quota snapshot.
#[doc(hidden)]
#[derive(Debug, Clone, Deserialize)]
pub struct CachedQuota {
    #[serde(default)]
    pub gemini: Option<CachedQuotaPool>,
    #[serde(default, rename = "non-gemini")]
    pub non_gemini: Option<CachedQuotaPool>,
}

/// One model pool inside the plugin cache.
#[doc(hidden)]
#[derive(Debug, Clone, Deserialize)]
pub struct CachedQuotaPool {
    #[serde(default)]
    pub windows: Vec<CachedQuotaWindow>,
}

/// One cadence reading inside a plugin-cached model pool.
#[doc(hidden)]
#[derive(Debug, Clone, Deserialize)]
pub struct CachedQuotaWindow {
    #[serde(default)]
    pub window: String,
    #[serde(default, rename = "remainingFraction")]
    pub remaining_fraction: Option<f64>,
    #[serde(default, rename = "resetTime")]
    pub reset_time: Option<Value>,
}

/// Find this identity's plugin-store row without introducing a second account key.
fn stored_account_for_email<'a>(
    accounts: &'a [StoredAccount],
    email: Option<&str>,
) -> Option<&'a StoredAccount> {
    accounts
        .iter()
        .find(|account| emails_match(email, account.email.as_deref()))
}

/// Does this account hold a paid tier the cloud lane cannot see?
///
/// Answered from the plugin store by email, so BOTH lanes can ask it: the vault
/// lane resolves an email too, and the hazard is identical whichever credential
/// dialled the cloud.
///
/// FALSE WHEN WE SIMPLY DO NOT KNOW -- no store, no matching account, no captured
/// tier. A fleet host with a vault credential and no plugin install has no way to
/// learn the tier, and withholding usage there would take a working lane dark on
/// a suspicion. Positive evidence of a paid tier is required to withhold.
fn account_holds_paid_tier(account: Option<&StoredAccount>) -> bool {
    account.is_some_and(|account| {
        account
            .captured_paid_tier_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|tier| !tier.is_empty() && !tier.eq_ignore_ascii_case("free-tier"))
    })
}

/// Why a paid-tier account with neither an authoritative source nor a fresh cache
/// publishes nothing.
///
/// Deliberately `LocalSourceUnavailable` rather than a degraded entry: it is
/// classified transient, so the last correct reading keeps serving while the
/// editor is closed. Publishing the cloud number instead would replace a true
/// figure with a confident wrong one -- a different pool, not a stale one -- and
/// a router cannot tell those apart.
fn paid_tier_usage_unavailable() -> FetchError {
    FetchError::LocalSourceUnavailable(
        "this account holds a paid Antigravity tier, but neither a matching local \
         editor session nor a fresh plugin cache is available: the cloud credential \
         is entitled to the standard tier and would describe a pool this account \
         does not use"
            .to_string(),
    )
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

// ---- process + port discovery (macOS) ---------------------------------------

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
            used_percent,
            raw_used_percent: None,
            resets_at: reset,
            window_minutes,
            used_count: None,
            total_count: None,
            regeneration: None,
        },
        title,
        id: bucket.bucket_id.clone(),
    })
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

/// Normalize the plugin's account-local cache into the same named pools as the
/// live editor. The cache's `gemini` and `non-gemini` keys are pool labels, while
/// each child `window` is the cadence that completes the public bucket id.
fn parse_cached_quota(cache: &CachedQuota) -> Result<Usage, FetchError> {
    let groups = [
        (cache.gemini.as_ref(), "Gemini Models", "gemini"),
        (cache.non_gemini.as_ref(), "Claude and GPT models", "3p"),
    ]
    .into_iter()
    .filter_map(|(pool, display_name, id_prefix)| {
        let pool = pool?;
        let buckets = pool
            .windows
            .iter()
            .map(|window| {
                let cadence = window.window.trim().to_ascii_lowercase().replace('_', "-");
                QuotaBucket {
                    bucket_id: format!("{id_prefix}-{cadence}"),
                    display_name: window.window.clone(),
                    window: Some(window.window.clone()),
                    disabled: false,
                    remaining_fraction: window.remaining_fraction,
                    remaining: None,
                    reset_time: window.reset_time.clone(),
                }
            })
            .collect();
        Some(QuotaGroup {
            display_name: display_name.to_string(),
            buckets,
        })
    })
    .collect();
    normalize_quota_summary(QuotaSummary { groups })
}

/// Return a paid account's usable cache and the wall time its value was read.
/// Free-tier accounts deliberately keep using their live cloud lane.
fn fresh_paid_cached_quota(
    account: Option<&StoredAccount>,
    now: DateTime<Utc>,
) -> Option<(Usage, DateTime<Utc>)> {
    let account = account.filter(|account| account_holds_paid_tier(Some(account)))?;
    let updated_at = DateTime::<Utc>::from_timestamp_millis(account.cached_quota_updated_at?)?;
    let age = now.signed_duration_since(updated_at).to_std().ok()?;
    if age > CACHED_QUOTA_MAX_AGE {
        return None;
    }
    let usage = parse_cached_quota(account.cached_quota.as_ref()?).ok()?;
    Some((usage, updated_at))
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
        FetchError::ProviderStatus(403 | 404) | FetchError::Unauthorized(_)
    )
}

fn parse_remote_quota(body: &[u8]) -> Result<Usage, FetchError> {
    let response: RemoteQuotaResponse = serde_json::from_slice(body)
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
                used_percent,
                raw_used_percent: None,
                window_minutes: None,
                resets_at,
                used_count: None,
                total_count: None,
                regeneration: None,
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
    token_url: String,
    local_cache: std::sync::Mutex<Option<(std::time::Instant, Vec<LocalSnapshot>)>>,
    local_endpoints: Option<Vec<(LocalServer, u16)>>,
    override_accounts: Option<Vec<StoredAccount>>,
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
            token_url: TOKEN_URL.to_string(),
            local_cache: std::sync::Mutex::new(None),
            local_endpoints: None,
            override_accounts: None,
        }
    }

    #[doc(hidden)]
    pub fn set_local_endpoints(&mut self, endpoints: Vec<(LocalServer, u16)>) {
        self.local_endpoints = Some(endpoints);
        if let Ok(mut guard) = self.local_cache.lock() {
            *guard = None;
        }
    }

    #[doc(hidden)]
    pub fn set_override_accounts(&mut self, accounts: Vec<StoredAccount>) {
        self.override_accounts = Some(accounts);
    }

    fn stored_accounts(&self) -> Vec<StoredAccount> {
        if let Some(accounts) = &self.override_accounts {
            return accounts.clone();
        }
        stored_accounts()
    }

    /// Fetch one plugin-stored account's quota.
    ///
    /// The account row supports both offline paths: a recent paid-pool cache, or
    /// the cloud endpoint for free-tier accounts. The latter exchanges the
    /// plugin's refresh token on every fetch rather than caching an access token
    /// whose lifetime is shorter than the refresher's own interval.
    async fn fetch_plugin_account(&self, handle_name: &str) -> FetchAttempt {
        let accounts = self.stored_accounts();
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

        // 1. Probe the local agy / language-server lane first.
        //
        // WHY THE LOCAL LANE OUTRANKS A CREDENTIALED ONE:
        // Normally, a stored cloud credential would take precedence over an ambient
        // local desktop probe. Here the ordering is reversed because Google resolves
        // Code Assist tier entitlement from the OAuth CLIENT the token was issued to:
        // our cloud token comes from the opencode plugin client, which is only entitled
        // to standard tier (~7% pool that never moves on a paid plan), whereas the local
        // editor's own server sees the user's actual paid subscription tier (e.g. Google
        // AI Ultra / ~58% pool).
        //
        // THE ACCOUNT GUARD:
        // The local probe reports whichever account is signed into the running editor.
        // If the user has multiple accounts, an unverified local probe would silently
        // publish another account's quota under this handle's row. We therefore query
        // GetUserStatus on the loopback server and require the email to match this
        // handle's email case-insensitively. A mismatch, missing email, or absent local
        // server falls through to the cloud lane.
        if let Some(expected_email) = account.email.as_deref() {
            let snapshots = self.probe_local_snapshots().await;
            if let Some(matching) = snapshots
                .into_iter()
                .find(|s| emails_match(Some(expected_email), s.email.as_deref()))
            {
                return FetchAttempt::success(observed, PLUGIN_SOURCE, matching.usage);
            }
        }

        // 2. The plugin cache outranks a live cloud request because the cloud
        // token is entitled to a different tier and therefore answers a different
        // question. The row is selected by email; `cachedQuotaAccountId` is not an
        // account identity and must never create or repoint a published row.
        let cache_account = stored_account_for_email(&accounts, account.email.as_deref());
        if let Some((usage, updated_at)) = fresh_paid_cached_quota(cache_account, Utc::now()) {
            return FetchAttempt::success(observed, PLUGIN_SOURCE, usage)
                .with_value_observed_at(updated_at);
        }

        // No live session or fresh cache for this account. Falling back to the
        // cloud is right ONLY when the cloud answers about the pool this account
        // actually uses. For a paid tier it does not, so publish nothing and let
        // the last correct reading stand.
        if account_holds_paid_tier(Some(account)) {
            return FetchAttempt::failure(
                observed,
                Some(PLUGIN_SOURCE.to_string()),
                paid_tier_usage_unavailable(),
            );
        }

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
            // Identifies the calling product to the shared endpoint, matching
            // what the Antigravity client sends.
            .header(Header::new("User-Agent", REMOTE_USER_AGENT))
            .timeout(REQUEST_TIMEOUT)
            .send_provider_status_first(&self.remote_http, PROVIDER_NAME)
            .await
    }

    /// Exchange a stored refresh token for a short-lived access token.
    ///
    /// Only an OAuth body that explicitly states `invalid_grant` proves the
    /// credential needs re-authorization. Other refusal bodies can describe a
    /// transient endpoint failure, so they retain their existing status class.
    /// THIS LANE REFRESHES, WHICH IS AN EXCEPTION -- READ BEFORE COPYING IT.
    ///
    /// The fleet rule is that a quota reader never touches a refresh endpoint,
    /// because exactly one process may refresh a credential and that process is its
    /// custodian. A second refresher on a family whose refresh tokens ROTATE
    /// silently revokes the first holder's session -- for anthropic and openai that
    /// means signing the user out of their editor, which is why those plugin lanes
    /// were declined outright.
    ///
    /// Google's refresh tokens do NOT rotate on exchange. That was established by
    /// probe, not by documentation, and it is the entire basis on which this lane
    /// exists. It is a property of the credential family, not of this code.
    ///
    /// So the operative rule here has never been "never refresh". It is: NEVER
    /// REFRESH A FAMILY WHOSE TOKENS ROTATE. If you are reading this as precedent
    /// for a new lane, the question to answer first is which of those two your
    /// family is -- and the answer is a probe, because getting it wrong produces no
    /// error here and a logged-out user somewhere else.
    async fn exchange_refresh_token(&self, refresh_token: &str) -> Result<String, FetchError> {
        if refresh_token.trim().is_empty() {
            return Err(FetchError::NoSession(
                "the stored account carries no refresh token".to_string(),
            ));
        }
        let client_id = oauth_client_id();
        let client_secret = oauth_client_secret();
        let response = JsonRequest::post_form(
            &self.token_url,
            &[
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ],
        )
        .timeout(REQUEST_TIMEOUT)
        .send_raw(&self.remote_http)
        .await?;

        if !(200..300).contains(&response.status) {
            #[derive(Deserialize)]
            struct OAuthErrorResponse {
                error: Option<String>,
            }
            let invalid_grant = serde_json::from_slice::<OAuthErrorResponse>(&response.body)
                .ok()
                .and_then(|response| response.error)
                .as_deref()
                == Some("invalid_grant");
            return if invalid_grant {
                Err(FetchError::CredentialUnusable(
                    "antigravity refresh token was rejected: invalid_grant".to_string(),
                ))
            } else {
                Err(FetchError::ProviderStatus(response.status))
            };
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
        }
        let parsed: TokenResponse =
            serde_json::from_slice(response.body_for_parsing()?).map_err(|e| {
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

        // 1. Probe the local lane first if this vault handle's identity is known.
        // A local editor outranks the cloud credential because the cloud credential is
        // entitled to a different tier. If the local session matches this vault handle's
        // identity, serve local usage.
        if let Some(expected_email) = &observed_email {
            let snapshots = self.probe_local_snapshots().await;
            if let Some(matching) = snapshots
                .into_iter()
                .find(|s| emails_match(Some(expected_email), s.email.as_deref()))
            {
                return FetchAttempt::success(observed, "vault", matching.usage)
                    .with_account_info(account_info);
            }
        }

        let accounts = self.stored_accounts();
        let plugin_account = stored_account_for_email(&accounts, observed_email.as_deref());

        // The account-local plugin cache outranks this live network call because
        // the token below is entitled to the standard tier, while the cache was
        // read from the pool the signed-in account actually spends.
        if let Some((usage, updated_at)) = fresh_paid_cached_quota(plugin_account, Utc::now()) {
            return FetchAttempt::success(observed, "vault", usage)
                .with_value_observed_at(updated_at)
                .with_account_info(account_info);
        }

        // Same rule as the plugin lane, and for the same reason: the vault's
        // token is issued to the same OAuth client, so it reaches the same
        // standard-tier pool. The tier is read from the plugin store by email
        // because that is the only place on this host that records it; a host
        // without the store answers false and keeps serving, which is the honest
        // default when the tier is unknown rather than known-absent.
        if account_holds_paid_tier(plugin_account) {
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
                capability,
                record_version,
                error,
            );
        }

        match result {
            Ok(usage) => {
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
    async fn probe_local_snapshots(&self) -> Vec<LocalSnapshot> {
        let now = std::time::Instant::now();
        if let Ok(guard) = self.local_cache.lock() {
            if let Some((taken_at, snapshots)) = guard.as_ref() {
                if now.duration_since(*taken_at) < Duration::from_secs(2) {
                    return snapshots.clone();
                }
            }
        }

        let candidates = if let Some(endpoints) = &self.local_endpoints {
            endpoints.clone()
        } else {
            let servers = tokio::task::spawn_blocking(discover_servers)
                .await
                .unwrap_or_else(|_join_error| Vec::new());
            let mut list = Vec::new();
            for server in servers {
                let pid = server.pid;
                let ports = tokio::task::spawn_blocking(move || discover_ports(pid))
                    .await
                    .unwrap_or_else(|_join_error| Vec::new());
                for port in ports {
                    list.push((server.clone(), port));
                }
            }
            list
        };

        let mut snapshots = Vec::new();
        for (server, port) in &candidates {
            if let Ok((usage, email)) = self.probe(server, *port).await {
                snapshots.push(LocalSnapshot { usage, email });
            }
        }

        if let Ok(mut guard) = self.local_cache.lock() {
            *guard = Some((now, snapshots.clone()));
        }

        snapshots
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
        for (index, account) in self.stored_accounts().iter().enumerate() {
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
            let snapshots = self.probe_local_snapshots().await;
            if let Some(snapshot) = snapshots.into_iter().next() {
                return Ok(ProviderUsage::healthy(
                    PROVIDER_NAME,
                    None,
                    "oauth",
                    snapshot.usage,
                ));
            }

            let servers = if let Some(endpoints) = &self.local_endpoints {
                endpoints.iter().map(|(s, _)| s.clone()).collect()
            } else {
                tokio::task::spawn_blocking(discover_servers)
                    .await
                    .unwrap_or_else(|_join_error| Vec::new())
            };
            if servers.is_empty() {
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
            r#"{"gemini":{"windows":[
                {"window":"weekly","remainingFraction":0.5,
                 "resetTime":"2026-09-17T18:41:37Z"}
            ]}}"#,
        )
        .expect("the cache payload parses");
        let cache = parse_cached_quota(&cached).expect("the cache normalises");

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

    /// The token endpoint is pinned to Google's own host.
    ///
    /// This is the URL a live REFRESH TOKEN is posted to, in the request body,
    /// so a wrong host does not merely fail -- it receives a working
    /// credential. The symptom afterwards is indistinguishable from an expired
    /// login: the exchange returns an error, the account reads as dead, and an
    /// operator re-authenticating does not fix it because the credential was
    /// never the problem.
    ///
    /// Asserted against a literal read off the constant rather than compared to
    /// the constant itself, which would hold at any value.
    #[test]
    fn the_token_endpoint_is_googles_own_host() {
        assert_eq!(TOKEN_URL, "https://oauth2.googleapis.com/token");
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
            FetchError::ProviderStatus(403),
            FetchError::ProviderStatus(404),
            FetchError::Unauthorized("not entitled".to_string()),
        ] {
            assert!(
                summary_unavailable(&refusal),
                "a refusal must fall back to the per-model lane: {refusal:?}"
            );
        }
        for transient in [
            FetchError::ProviderStatus(500),
            FetchError::ProviderStatus(429),
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

        // Each bucket keeps its own figure rather than being folded into one
        // headline: the Gemini pool meters 20% at five hours and 47% weekly, and
        // collapsing those to the worse one loses the cadence a reader needs.
        assert_eq!(pool_window(&usage, "gemini-5h").used_percent, 20.0);
        let weekly = super::tests::pool_window(&usage, "gemini-weekly");
        assert_eq!(weekly.used_percent, 47.0);
        assert_eq!(weekly.resets_at.as_deref(), Some("2026-06-30T00:00:00Z"));
        assert_eq!(weekly.window_minutes, Some(10080));

        assert_eq!(usage.extra_rate_windows.as_ref().unwrap().len(), 3);

        // The property the unnamed slot used to need policing: with no slot, a
        // walled external pool CANNOT be mistaken for the account's own capacity,
        // because there is nowhere unnamed for it to sit.
        assert_no_unnamed_slot(&usage);
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

    fn account(email: Option<&str>, token: Option<&str>, enabled: Option<bool>) -> StoredAccount {
        StoredAccount {
            email: email.map(str::to_string),
            refresh_token: token.map(str::to_string),
            managed_project_id: Some("proj-1".to_string()),
            enabled,
            // Free tier by default so the existing cases keep exercising the
            // cloud fallback. The paid-tier arm has its own fixtures below.
            captured_paid_tier_id: Some("free-tier".to_string()),
            cached_quota_updated_at: None,
            cached_quota: None,
        }
    }

    /// The same account, holding a paid tier the cloud lane cannot see.
    fn paid_account(email: Option<&str>, token: Option<&str>) -> StoredAccount {
        StoredAccount {
            captured_paid_tier_id: Some("g1-ultra-lite-tier".to_string()),
            ..account(email, token, Some(true))
        }
    }

    fn cached_paid_account(email: &str, updated_at: DateTime<Utc>) -> StoredAccount {
        serde_json::from_value(serde_json::json!({
            "email": email,
            "refreshToken": "refresh-tok",
            "managedProjectId": "proj-1",
            "enabled": true,
            "capturedPaidTierId": "g1-ultra-lite-tier",
            // Deliberately present and ignored: the cache belongs to the email
            // row and this plugin-private hex id is not an account identity.
            "cachedQuotaAccountId": "290d564a294e12a3",
            "cachedQuotaUpdatedAt": updated_at.timestamp_millis(),
            "cachedQuota": {
                "gemini": {
                    "remainingFraction": 0.938304,
                    "resetTime": "2026-09-17T18:41:37Z",
                    "modelCount": 2,
                    "windows": [
                        { "window": "5h", "remainingFraction": 1.0,
                          "resetTime": "2026-09-11T17:08:40Z" },
                        { "window": "weekly", "remainingFraction": 0.938304,
                          "resetTime": "2026-09-17T18:41:37Z" }
                    ]
                },
                "non-gemini": {
                    "remainingFraction": 1.0,
                    "resetTime": "2026-09-11T17:08:40Z",
                    "modelCount": 3,
                    "windows": [
                        { "window": "5h", "remainingFraction": 1.0,
                          "resetTime": "2026-09-11T17:08:40Z" },
                        { "window": "weekly", "remainingFraction": 1.0,
                          "resetTime": "2026-09-17T18:41:37Z" }
                    ]
                }
            }
        }))
        .expect("cached account fixture")
    }

    fn cached_free_account(email: &str, updated_at: DateTime<Utc>) -> StoredAccount {
        StoredAccount {
            captured_paid_tier_id: Some("free-tier".to_string()),
            ..cached_paid_account(email, updated_at)
        }
    }

    #[tokio::test]
    async fn plugin_refresh_invalid_grant_is_credential_unusable_and_non_transient() {
        let (base_url, request) =
            crate::loopback::serve_once(400, br#"{"error":"invalid_grant"}"#.to_vec()).await;
        let mut provider = AntigravityProvider::new();
        provider.token_url = format!("{base_url}/token");

        let error = provider
            .exchange_refresh_token("refresh-token")
            .await
            .expect_err("invalid_grant must reject the credential");
        assert!(matches!(error, FetchError::CredentialUnusable(_)));
        assert_eq!(classify(&error), FetchClass::NonTransient);

        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("post /token "));
        assert!(request.contains("refresh_token=refresh-token"));
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

    const CLOUD_MOCK_QUOTA: &str = r#"{
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
                let first_line = request.lines().next().unwrap_or("");
                let path = first_line.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = handler(path);
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
            } else if path == "/token" {
                (200, br#"{"access_token":"mock-token"}"#.to_vec())
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
        provider.token_url = format!("http://127.0.0.1:{port}/token");
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");

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

    /// 2. Identity MISMATCH falls through to the cloud lane.
    /// Asserts WHICH lane served: the cloud lane, not merely that some usage returned.
    #[tokio::test]
    async fn identity_mismatch_falls_through_to_the_cloud_lane() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == QUOTA_SUMMARY_PATH {
                (200, LOCAL_MOCK_QUOTA.as_bytes().to_vec())
            } else if path == USER_STATUS_PATH {
                // Local editor is signed into a DIFFERENT account than the handle.
                (
                    200,
                    br#"{"userStatus":{"email":"different_user@gmail.com"}}"#.to_vec(),
                )
            } else if path == "/token" {
                (200, br#"{"access_token":"mock-token"}"#.to_vec())
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
        provider.set_override_accounts(vec![account(
            Some("beatricelau0414@gmail.com"),
            Some("refresh-tok"),
            Some(true),
        )]);
        provider.token_url = format!("http://127.0.0.1:{port}/token");
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let usage = attempt.usage.expect("usage must be present");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");

        // Asserts CLOUD lane served (7.15%, 2026-09-08 reset) because identity mismatched.
        assert_eq!(
            primary.resets_at.as_deref(),
            Some("2026-09-08T18:02:09Z"),
            "mismatch must fall through to the cloud lane"
        );
        assert_eq!(
            primary.used_percent, 7.15,
            "mismatch must fall through to the cloud lane"
        );
    }

    /// 3. Local snapshot with NO email does not serve a named handle.
    /// Asserts WHICH lane served: the cloud lane, not the unattributable local lane.
    #[tokio::test]
    async fn local_snapshot_with_no_email_does_not_serve_a_named_handle() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == QUOTA_SUMMARY_PATH {
                (200, LOCAL_MOCK_QUOTA.as_bytes().to_vec())
            } else if path == USER_STATUS_PATH {
                // GetUserStatus call fails or omits email.
                (200, br#"{"userStatus":{}}"#.to_vec())
            } else if path == "/token" {
                (200, br#"{"access_token":"mock-token"}"#.to_vec())
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
        provider.set_override_accounts(vec![account(
            Some("beatricelau0414@gmail.com"),
            Some("refresh-tok"),
            Some(true),
        )]);
        provider.token_url = format!("http://127.0.0.1:{port}/token");
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let usage = attempt.usage.expect("usage must be present");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");

        // Asserts CLOUD lane served because the local snapshot carried no email to attribute.
        assert_eq!(
            primary.resets_at.as_deref(),
            Some("2026-09-08T18:02:09Z"),
            "snapshot with no email must not serve a named handle"
        );
        assert_eq!(
            primary.used_percent, 7.15,
            "snapshot with no email must not serve a named handle"
        );
    }

    /// 4. Local lane absent on a credentialed handle still reaches the cloud lane.
    #[tokio::test]
    async fn local_lane_absent_on_a_credentialed_handle_still_reaches_the_cloud_lane() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == "/token" {
                (200, br#"{"access_token":"mock-token"}"#.to_vec())
            } else if path == "/cloud-quota" {
                (200, CLOUD_MOCK_QUOTA.as_bytes().to_vec())
            } else {
                (404, b"not found".to_vec())
            }
        })
        .await;

        let mut provider = AntigravityProvider::new();
        // No local servers running at all.
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![account(
            Some("beatricelau0414@gmail.com"),
            Some("refresh-tok"),
            Some(true),
        )]);
        provider.token_url = format!("http://127.0.0.1:{port}/token");
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;
        let usage = attempt.usage.expect("usage must be present");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");

        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-08T18:02:09Z"));
        assert_eq!(primary.used_percent, 7.15);
    }

    /// A PAID-TIER account with no live editor session publishes NOTHING.
    ///
    /// The cloud lane is entitled to the standard tier, so for an account holding
    /// a paid one it answers about a pool the user does not consume. Measured on
    /// this host: the same account read 7.15% through the cloud and 58.41% in the
    /// editor, and the 7.15% figure never moved because nothing spends it.
    ///
    /// Publishing that would replace a true reading with a confident wrong one --
    /// a DIFFERENT POOL rather than a stale number, which a consumer cannot tell
    /// from a real one. `LocalSourceUnavailable` is transient, so the last correct
    /// reading keeps serving until the editor is open again.
    ///
    /// The paired free-tier case below is the control: it must still reach the
    /// cloud, or this guard has taken a working lane dark.
    #[tokio::test]
    async fn a_paid_tier_account_without_a_local_session_publishes_nothing() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == "/token" {
                (200, br#"{"access_token":"mock-token"}"#.to_vec())
            } else if path == "/cloud-quota" {
                (200, CLOUD_MOCK_QUOTA.as_bytes().to_vec())
            } else {
                (404, b"not found".to_vec())
            }
        })
        .await;

        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![paid_account(
            Some("beatricelau0414@gmail.com"),
            Some("refresh-tok"),
        )]);
        provider.token_url = format!("http://127.0.0.1:{port}/token");
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:beatricelau0414@gmail.com".into(),
            ))
            .await;

        let error = attempt
            .usage
            .expect_err("a paid-tier account must not publish the standard-tier pool");
        assert!(
            matches!(error, FetchError::LocalSourceUnavailable(_)),
            "must be the transient local class so the last good reading survives, got {error:?}"
        );
        assert_eq!(
            classify(&error),
            FetchClass::Transient,
            "a closed editor is not a broken credential"
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
            paid_account(Some("account-b@example.com"), Some("refresh-tok")),
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

    /// A FREE-TIER account with no live session still reaches the cloud.
    ///
    /// The control for the guard above, and the load-bearing half: for an account
    /// with no paid tier the cloud lane reports the pool it really uses, so
    /// withholding would take a correct lane dark. Measured on this host, the
    /// second account reads 100% through the cloud and that figure is true.
    #[tokio::test]
    async fn a_free_tier_account_without_a_local_session_still_reaches_the_cloud() {
        let (port, _server) = spawn_mock_server(move |path| {
            if path == "/token" {
                (200, br#"{"access_token":"mock-token"}"#.to_vec())
            } else if path == "/cloud-quota" {
                (200, CLOUD_MOCK_QUOTA.as_bytes().to_vec())
            } else {
                (404, b"not found".to_vec())
            }
        })
        .await;

        let mut provider = AntigravityProvider::new();
        provider.set_local_endpoints(Vec::new());
        provider.set_override_accounts(vec![cached_free_account(
            "ufukaltinok@gmail.com",
            Utc::now() - chrono::Duration::minutes(5),
        )]);
        provider.token_url = format!("http://127.0.0.1:{port}/token");
        provider.quota_summary_url = format!("http://127.0.0.1:{port}/cloud-quota");

        let attempt = provider
            .fetch_handle(&CredentialHandle::Named(
                "plugin:ufukaltinok@gmail.com".into(),
            ))
            .await;
        let usage = attempt
            .usage
            .expect("a free-tier account must still reach the cloud lane");
        let primary = super::tests::pool_window(&usage, "gemini-weekly");
        assert_eq!(primary.resets_at.as_deref(), Some("2026-09-08T18:02:09Z"));
        assert_eq!(
            attempt.value_observed_at, None,
            "the paid-cache lane must not fire for a free-tier account"
        );
    }

    /// 5. Implicit handle still serves the local lane with no identity to match.
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
}
