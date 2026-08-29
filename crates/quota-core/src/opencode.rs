//! OpenCode subscription usage — browser cookies + Next.js `_server` actions.
//!
//! Flow: Chrome cookies for `opencode.ai` (`auth` / `__Host-auth` only) → workspace
//! id via `_server` → subscription payload → parse `rollingUsage` + `weeklyUsage`.
//!
//! DESKTOP-COUPLED: needs a logged-in Chrome session on macOS. Dead cookie, login
//! markers, or missing windows → [`FetchError`] (degrade-never-wrong).
//!
//! VERIFICATION: fixture-verified against CodexBar source, NOT live-verified (no
//! logged-in browser on the build machine). Ported from
//! `OpenCode/OpenCodeUsageFetcher.swift` (cookies/headers :112-310, window keys
//! :32-64, parse :312-756, signed-out :451-462, workspace :361-401) and
//! `OpenCode/OpenCodeWebCookieSupport.swift:4-15` (cookie names).

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{
    browser_cookies::{self, CookieJar, SOURCE_LABEL},
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};
use crate::{
    credential_source::CredentialSource,
    provider::{CredentialHandle, FetchAttempt},
    vault_handles::{cookie_lane, CookieLane, VaultHandleLoader},
};

/// The bare vault credential id for this domain. A suffixed deposit
/// (`cookie:opencode.ai:work`) names an account and takes the provider
/// vault-only; this bare one is the fallback for hosts that cannot read the live
/// browser store at all.
///
/// Note `opencode` and `opencodego` deliberately share this id: they are two
/// plans on ONE session, so there is one record and both providers consume it.
pub const COOKIE_FAMILY: &str = "cookie:opencode.ai";

const PROVIDER_NAME: &str = "opencode";
const DOMAIN: &str = "opencode.ai";
const SERVER_BASE: &str = "https://opencode.ai/_server";
const ORIGIN: &str = "https://opencode.ai";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Hash naming the server function that lists workspaces. Rotates whenever they
/// rebundle, and a stale one surfaces only as a rejected request -- which reads
/// exactly like an outage while the defect is this line.
pub const WORKSPACES_SERVER_ID: &str =
    "def39973159c7f0483d8793a822b8dbb10d067e12c65455fcb4608459ba0234f";
/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Customer/billing server function, carrying the monthly spend a pay-as-you-go
/// workspace bills against.
///
/// A workspace on that plan has no subscription object at all, so the
/// subscription function answers null or fails outright -- which this module
/// reported as an upstream failure indistinguishable from an outage. Added at
/// CodexBar v0.49.6 for the same reason.
pub const BILLING_SERVER_ID: &str =
    "c83b78a614689c38ebee981f9b39a8b377716db85c1fd7dbab604adc02d3313d";

/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Hash naming the subscription server function. Same rotation risk as the
/// workspaces id.
pub const SUBSCRIPTION_SERVER_ID: &str =
    "7abeebee372f304e050aaaf92be863f4a86490e382f8c79db68fd94040d691b4";

const ROLLING_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;
const MONTHLY_WINDOW_MINUTES: i64 = 30 * 24 * 60;

pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

const PERCENT_KEYS: &[&str] = &[
    "usagePercent",
    "usedPercent",
    "percentUsed",
    "percent",
    "usage_percent",
    "used_percent",
    "utilization",
    "utilizationPercent",
    "utilization_percent",
    "usage",
];
const RESET_IN_KEYS: &[&str] = &[
    "resetInSec",
    "resetInSeconds",
    "resetSeconds",
    "reset_sec",
    "reset_in_sec",
    "resetsInSec",
    "resetsInSeconds",
    "resetIn",
    "resetSec",
];
const RESET_AT_KEYS: &[&str] = &[
    "resetAt",
    "resetsAt",
    "reset_at",
    "resets_at",
    "nextReset",
    "next_reset",
    "renewAt",
    "renew_at",
];

const SESSION_COOKIE_NAMES: &[&str] = &["auth", "__Host-auth"];

pub fn has_session_cookie(jar: &CookieJar) -> bool {
    jar.has_cookie_named(|n| SESSION_COOKIE_NAMES.contains(&n))
}

pub fn request_cookie_header(jar: &CookieJar) -> Option<String> {
    let parts: Vec<String> = jar
        .cookies
        .iter()
        .filter(|c| SESSION_COOKIE_NAMES.contains(&c.name.as_str()))
        .map(|c| format!("{}={}", c.name, c.value))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

pub fn looks_signed_out(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("login")
        || lower.contains("sign in")
        || lower.contains("auth/authorize")
        || lower.contains("not associated with an account")
        || lower.contains("actor of type \"public\"")
}

fn server_instance_header() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("server-fn:{nanos:x}{:x}", std::process::id())
}

pub fn server_get_url(server_id: &str, args: Option<&[String]>) -> String {
    let mut url = match url::Url::parse(SERVER_BASE) {
        Ok(u) => u,
        Err(_) => return SERVER_BASE.to_string(),
    };
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("id", server_id);
        if let Some(args) = args {
            if !args.is_empty() {
                if let Ok(encoded) = serde_json::to_string(args) {
                    pairs.append_pair("args", &encoded);
                }
            }
        }
    }
    url.to_string()
}

fn common_server_headers(cookie: &str, server_id: &str, referer: &str) -> Vec<Header> {
    vec![
        Header::new("Cookie", cookie.to_string()),
        Header::new("X-Server-Id", server_id.to_string()),
        Header::new("X-Server-Instance", server_instance_header()),
        Header::new("User-Agent", USER_AGENT.to_string()),
        Header::new("Origin", ORIGIN.to_string()),
        Header::new("Referer", referer.to_string()),
        Header::new(
            "Accept",
            "text/javascript, application/json;q=0.9, */*;q=0.8".to_string(),
        ),
    ]
}

fn apply_headers(req: JsonRequest, headers: Vec<Header>) -> JsonRequest {
    headers.into_iter().fold(req, |r, h| r.header(h))
}

pub fn parse_workspace_ids(text: &str) -> Vec<String> {
    let mut ids = scan_wrk_ids(text);
    if ids.is_empty() {
        if let Ok(v) = serde_json::from_str::<Value>(text) {
            collect_workspace_ids(&v, &mut ids);
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Scan a raw response for `wrk_...` identifiers.
///
/// The cursor walks one BYTE at a time, so every comparison and slice here must
/// be byte-based: a workspace name is user-chosen, so the response can carry any
/// UTF-8, and slicing the `&str` at a cursor that has landed inside a multibyte
/// scalar panics. The matched id itself is always ASCII (`wrk_` plus alphanumerics
/// and underscores), so it is safe to build from bytes.
fn scan_wrk_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = b"wrk_";
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if bytes[i..].starts_with(needle) {
            let start = i;
            i += needle.len();
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            // ASCII by construction, so this is a lossless conversion.
            let id = String::from_utf8_lossy(&bytes[start..i]).into_owned();
            if id.len() > 4 && !out.contains(&id) {
                out.push(id);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn collect_workspace_ids(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) if s.starts_with("wrk_") && s.len() > 4 => {
            if !out.contains(s) {
                out.push(s.clone());
            }
        }
        Value::Array(a) => {
            for v in a {
                collect_workspace_ids(v, out);
            }
        }
        Value::Object(m) => {
            for v in m.values() {
                collect_workspace_ids(v, out);
            }
        }
        _ => {}
    }
}

pub async fn fetch_workspace_id(
    client: &reqwest::Client,
    cookie: &str,
) -> Result<String, FetchError> {
    let get_url = server_get_url(WORKSPACES_SERVER_ID, None);
    let text = server_get(
        client,
        &get_url,
        common_server_headers(cookie, WORKSPACES_SERVER_ID, ORIGIN),
        "workspaces",
    )
    .await?;
    if looks_signed_out(&text) {
        return Err(FetchError::Unauthorized(
            "opencode session expired (workspace fetch)".to_string(),
        ));
    }
    // Checked before the retry, for the same reason it is checked on the
    // subscription call: an empty result and a stated "there is nothing here"
    // are different answers that both yield no ids. Without this, an account
    // with no workspace retries, fails to parse again, and is published as
    // Decode -- our parser blamed for a fact about the account, and counted as a
    // stale browser login on an entirely working session.
    if is_explicit_null(&text) {
        return Err(FetchError::NoQuotaReported(
            "opencode: this account has no workspace".to_string(),
        ));
    }
    let mut ids = parse_workspace_ids(&text);
    if ids.is_empty() {
        let post_req = apply_headers(
            JsonRequest::post_json(SERVER_BASE, b"[]".to_vec()).timeout(REQUEST_TIMEOUT),
            common_server_headers(cookie, WORKSPACES_SERVER_ID, ORIGIN),
        );
        let body = post_req
            .send(client)
            .await
            .map_err(|error| error.stage("workspaces POST"))?;
        let fallback = String::from_utf8_lossy(&body);
        if looks_signed_out(&fallback) {
            return Err(FetchError::Unauthorized(
                "opencode session expired (workspace POST)".to_string(),
            ));
        }
        if is_explicit_null(&fallback) {
            return Err(FetchError::NoQuotaReported(
                "opencode: this account has no workspace".to_string(),
            ));
        }
        ids = parse_workspace_ids(&fallback);
    }
    ids.into_iter().next().ok_or_else(|| {
        FetchError::Decode("opencode: missing workspace id in _server response".to_string())
    })
}

/// Fetch the billing payload for a workspace.
///
/// Separate from the subscription call rather than folded into it: the two
/// answer for different workspace types, and a helper that tried both would
/// report one error for two questions -- which is the ambiguity that made a
/// persistent failure here read as an outage for weeks.
pub async fn fetch_billing_text(
    client: &reqwest::Client,
    cookie: &str,
    workspace_id: &str,
) -> Result<String, FetchError> {
    let referer = format!("{ORIGIN}/workspace/{workspace_id}");
    let args = vec![workspace_id.to_string()];
    let text = server_get(
        client,
        &server_get_url(BILLING_SERVER_ID, Some(&args)),
        common_server_headers(cookie, BILLING_SERVER_ID, &referer),
        "billing",
    )
    .await?;
    if looks_signed_out(&text) {
        return Err(FetchError::Unauthorized(
            "opencode session expired (billing)".to_string(),
        ));
    }
    Ok(text)
}

/// Call one server function, naming the stage in any error it produces.
///
/// This provider makes several of these calls per fetch and every one can fail
/// the same way, so an unnamed error is compatible with every explanation --
/// which is how a persistent HTTP 500 here read as a site outage for weeks. It
/// was in fact one specific call: a live probe found the workspaces and billing
/// functions answering on the same cookie while the subscription function
/// returned 500, and none of that was visible in the published error.
///
/// The stage name goes in the error rather than only in a log line because the
/// published `error` string is what a consumer and an operator both see.
async fn server_get(
    client: &reqwest::Client,
    url: &str,
    headers: Vec<Header>,
    stage: &str,
) -> Result<String, FetchError> {
    let req = apply_headers(JsonRequest::get(url).timeout(REQUEST_TIMEOUT), headers);
    let bytes = req.send(client).await.map_err(|error| error.stage(stage))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Fetch only the subscription GET, without the POST retry behind it.
///
/// The provider's own path retries when the GET body does not parse and then
/// reports one error for both, so a diagnostic cannot tell a GET that answered
/// something we misread from a retry the site refused. Those have different
/// fixes -- one is ours and one is theirs.
pub async fn fetch_subscription_get(
    client: &reqwest::Client,
    cookie: &str,
    workspace_id: &str,
) -> Result<String, FetchError> {
    let referer = format!("{ORIGIN}/workspace/{workspace_id}/billing");
    let args = vec![workspace_id.to_string()];
    server_get(
        client,
        &server_get_url(SUBSCRIPTION_SERVER_ID, Some(&args)),
        common_server_headers(cookie, SUBSCRIPTION_SERVER_ID, &referer),
        "subscription GET",
    )
    .await
}

/// Whether the subscription parser accepts a body, for diagnostics.
///
/// Exposed so a probe asks the REAL parser rather than reimplementing its
/// acceptance -- a second copy would answer for itself rather than for the
/// provider, which is the failure mode being investigated here.
pub fn subscription_get_parses(text: &str) -> bool {
    parse_windows(text, Utc::now().timestamp(), false).is_ok()
}

pub async fn fetch_subscription_text(
    client: &reqwest::Client,
    cookie: &str,
    workspace_id: &str,
) -> Result<String, FetchError> {
    let referer = format!("{ORIGIN}/workspace/{workspace_id}/billing");
    let args = vec![workspace_id.to_string()];
    let get_url = server_get_url(SUBSCRIPTION_SERVER_ID, Some(&args));
    let text = server_get(
        client,
        &get_url,
        common_server_headers(cookie, SUBSCRIPTION_SERVER_ID, &referer),
        "subscription",
    )
    .await?;
    if looks_signed_out(&text) {
        return Err(FetchError::Unauthorized(
            "opencode session expired (subscription)".to_string(),
        ));
    }
    if is_explicit_null(&text) {
        // Not a Decode: the payload is well formed and states that this
        // workspace has no subscription, which is a fact about the account
        // rather than a failure of ours or theirs. The class is load-bearing --
        // Decode is counted as a stale browser login, so reporting it that way
        // sends an operator to re-authenticate a session that is working.
        // opencodego already answers its own no-plan case this way.
        return Err(FetchError::NoQuotaReported(format!(
            "opencode: workspace {workspace_id} has no subscription"
        )));
    }
    let now = Utc::now().timestamp();
    if parse_windows(&text, now, false).is_err() {
        let post_body = serde_json::to_vec(&args).map_err(|e| FetchError::Decode(e.to_string()))?;
        let post_req = apply_headers(
            JsonRequest::post_json(SERVER_BASE, post_body).timeout(REQUEST_TIMEOUT),
            common_server_headers(cookie, SUBSCRIPTION_SERVER_ID, &referer),
        );
        let body = post_req
            .send(client)
            .await
            .map_err(|error| error.stage("subscription POST"))?;
        let fallback = String::from_utf8_lossy(&body);
        if looks_signed_out(&fallback) {
            return Err(FetchError::Unauthorized(
                "opencode session expired (subscription POST)".to_string(),
            ));
        }
        if is_explicit_null(&fallback) {
            return Err(FetchError::NoQuotaReported(format!(
                "opencode: workspace {workspace_id} has no subscription"
            )));
        }
        return Ok(fallback.into_owned());
    }
    Ok(text)
}

/// Whether a server-function response states "there is nothing here".
///
/// The upstream says this three ways and only two were recognised: a bare
/// `null`, a JSON null, and -- the one that reached production -- a null in the
/// VALUE SLOT of the server-function envelope, which is a JavaScript statement
/// rather than JSON and so parses as neither:
///
/// ```text
/// ;0x41;((self.$R=self.$R||{})["server-fn:18cb..."]=[],null)
/// ```
///
/// Missing it is not inert. The caller retries as a POST when the body does not
/// parse, that retry answers HTTP 500, and the account is published as an
/// upstream failure -- so a workspace with no subscription reads as a broken
/// provider indefinitely. This host carried that for weeks, and the published
/// error supported an outage reading the whole time.
///
/// Matched TIGHTLY, on the envelope's closing `,null)` rather than on `null`
/// appearing anywhere, because the two directions cost differently: failing to
/// recognise a null costs the current false failure, while wrongly recognising
/// one would publish "no quota here" for an account that has some, and that
/// reads as a true fact about the user rather than as an error.
fn is_explicit_null(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("null") {
        return true;
    }
    if serde_json::from_str::<Value>(trimmed)
        .ok()
        .map(|v| v.is_null())
        .unwrap_or(false)
    {
        return true;
    }
    // The value slot is the last element of the envelope's comma expression, so
    // a null there ends the statement exactly this way.
    trimmed.contains("server-fn:") && trimmed.ends_with(",null)")
}

fn parse_date_value(val: &Value) -> Option<i64> {
    if let Some(n) = val.as_f64() {
        // A non-finite or absurd magnitude (e.g. "1e308") is not a real timestamp;
        // reject it explicitly rather than letting a saturating float→int cast
        // produce a garbage epoch (the window is then dropped, never fabricated).
        if !n.is_finite() || n >= i64::MAX as f64 {
            return None;
        }
        if n > 1_000_000_000_000.0 {
            return Some((n / 1000.0) as i64);
        }
        if n > 1_000_000_000.0 {
            return Some(n as i64);
        }
    }
    if let Some(s) = val.as_str() {
        let t = s.trim();
        if let Ok(n) = t.parse::<f64>() {
            return parse_date_value(&Value::from(n));
        }
        if let Ok(dt) = DateTime::parse_from_rfc3339(t) {
            return Some(dt.timestamp());
        }
    }
    None
}

fn reset_epoch_for_map(map: &serde_json::Map<String, Value>, now_secs: i64) -> Option<i64> {
    if let Some(sec) = crate::json_scan::first_i64(map, RESET_IN_KEYS) {
        return Some(now_secs + sec.max(0));
    }
    for key in RESET_AT_KEYS {
        if let Some(v) = map.get(*key) {
            if let Some(epoch) = parse_date_value(v) {
                return Some(epoch);
            }
        }
    }
    None
}

fn percent_from_map(map: &serde_json::Map<String, Value>) -> Option<f64> {
    // A direct percent field (e.g. `usagePercent`) may arrive as a 0..1 fraction
    // or a 0..100 percent, so it goes through the fraction heuristic below. A
    // computed used/limit ratio is already 0..100 and must NOT be rescaled,
    // otherwise a genuine sub-1% account (used=1, limit=100 -> 1.0) reads as a
    // false 100% exhausted. Track which path produced the value (CodexBar
    // v0.45.2 `percentIsDirect` gate).
    let direct = crate::json_scan::first_finite_f64(map, PERCENT_KEYS);
    let mut p = direct.or_else(|| {
        const USED: &[&str] = &["used", "usage", "consumed", "count", "usedTokens"];
        const LIMIT: &[&str] = &["limit", "total", "quota", "max", "cap", "tokenLimit"];
        let used = crate::json_scan::first_finite_f64(map, USED)?;
        let limit = crate::json_scan::first_finite_f64(map, LIMIT)?;
        if limit > 0.0 {
            Some((used / limit * 100.0).clamp(0.0, 100.0))
        } else {
            None
        }
    })?;
    if !p.is_finite() {
        return None;
    }
    if direct.is_some() && (0.0..=1.0).contains(&p) {
        p *= 100.0;
    }
    Some(p.clamp(0.0, 100.0))
}

fn window_from_map(
    map: &serde_json::Map<String, Value>,
    now_secs: i64,
    window_minutes: i64,
) -> Option<RateWindow> {
    let used_percent = percent_from_map(map)?;
    let resets_at = reset_epoch_for_map(map, now_secs).and_then(env::epoch_to_iso8601);
    Some(RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at,
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
    })
}

fn first_window_dict<'a>(
    dict: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    keys.iter()
        .find_map(|k| dict.get(*k).and_then(|v| v.as_object()))
}

fn parse_usage_dict(
    dict: &serde_json::Map<String, Value>,
    now_secs: i64,
    include_monthly: bool,
) -> Option<Usage> {
    let rolling_keys = [
        "rollingUsage",
        "rolling",
        "rolling_usage",
        "rollingWindow",
        "rolling_window",
    ];
    let weekly_keys = [
        "weeklyUsage",
        "weekly",
        "weekly_usage",
        "weeklyWindow",
        "weekly_window",
    ];
    let monthly_keys = [
        "monthlyUsage",
        "monthly",
        "monthly_usage",
        "monthlyWindow",
        "monthly_window",
    ];

    let rolling = first_window_dict(dict, &rolling_keys);
    let weekly = first_window_dict(dict, &weekly_keys);
    let monthly = first_window_dict(dict, &monthly_keys);

    let primary = rolling.and_then(|m| window_from_map(m, now_secs, ROLLING_WINDOW_MINUTES));
    let secondary = weekly.and_then(|m| window_from_map(m, now_secs, WEEKLY_WINDOW_MINUTES));
    let tertiary = if include_monthly {
        monthly.and_then(|m| window_from_map(m, now_secs, MONTHLY_WINDOW_MINUTES))
    } else {
        None
    };

    if primary.is_none() && secondary.is_none() && tertiary.is_none() {
        return None;
    }

    Some(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

fn try_parse_json_usage(text: &str, now_secs: i64, include_monthly: bool) -> Option<Usage> {
    let root: Value = serde_json::from_str(text).ok()?;
    let root = root.as_object()?;

    if let Some(u) = parse_usage_dict(root, now_secs, include_monthly) {
        if u.primary.is_some() || u.secondary.is_some() || u.tertiary.is_some() {
            return Some(u);
        }
    }

    for key in ["data", "result", "usage", "billing", "payload"] {
        if let Some(nested) = root.get(key).and_then(|v| v.as_object()) {
            if let Some(u) = parse_usage_dict(nested, now_secs, include_monthly) {
                if u.primary.is_some() || u.secondary.is_some() || u.tertiary.is_some() {
                    return Some(u);
                }
            }
        }
    }
    None
}

/// Bound one window's block at its closing brace, or at a fixed byte budget when
/// the brace is missing.
///
/// That fallback is a BYTE budget and the response can carry user-chosen text
/// (workspace and plan names), so it is rounded down to a character boundary
/// before slicing — an offset landing inside a multibyte character would panic.
/// The brace position needs no rounding, since a match is always on a boundary.
fn window_block<'a>(text: &'a str, window_key: &str) -> Option<&'a str> {
    let pos = text.find(window_key)?;
    let slice = &text[pos..];
    let end = slice
        .find('}')
        .unwrap_or_else(|| crate::text::floor_char_boundary(slice, slice.len().min(2500)));
    Some(&slice[..end])
}

fn field_after_key(block: &str, key: &str) -> Option<f64> {
    let mut search = block;
    while let Some(pos) = search.find(key) {
        let after = &search[pos + key.len()..];
        let after = after.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        let num_end = after
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit() && *c != '.')
            .map(|(i, _)| i)
            .unwrap_or(after.len());
        if num_end > 0 {
            return after[..num_end].trim().parse().ok();
        }
        if search.len() <= pos + 1 {
            break;
        }
        search = &search[pos + 1..];
    }
    None
}

fn field_after_key_i64(block: &str, key: &str) -> Option<i64> {
    field_after_key(block, key).map(|n| n as i64)
}

fn parse_windows_regex(text: &str, now_secs: i64, include_monthly: bool) -> Option<Usage> {
    let rolling_block = window_block(text, "rollingUsage")?;
    let rolling_p = field_after_key(rolling_block, "usagePercent")
        .or_else(|| field_after_key(rolling_block, "usedPercent"))?;
    let rolling_r = RESET_IN_KEYS
        .iter()
        .find_map(|k| field_after_key_i64(rolling_block, k))?;

    // Weekly is optional: a response with only the rolling window must still serve
    // the primary, rather than dropping everything when weekly is absent.
    let weekly = window_block(text, "weeklyUsage").and_then(|weekly_block| {
        let weekly_p = field_after_key(weekly_block, "usagePercent")
            .or_else(|| field_after_key(weekly_block, "usedPercent"))?;
        let weekly_r = RESET_IN_KEYS
            .iter()
            .find_map(|k| field_after_key_i64(weekly_block, k))?;
        window_from_parts(weekly_p, weekly_r, now_secs, WEEKLY_WINDOW_MINUTES)
    });

    let primary = window_from_parts(rolling_p, rolling_r, now_secs, ROLLING_WINDOW_MINUTES);
    let secondary = weekly;
    let tertiary = if include_monthly {
        window_block(text, "monthlyUsage").and_then(|block| {
            let p = field_after_key(block, "usagePercent")?;
            let r = RESET_IN_KEYS
                .iter()
                .find_map(|k| field_after_key_i64(block, k))?;
            window_from_parts(p, r, now_secs, MONTHLY_WINDOW_MINUTES)
        })
    } else {
        None
    };

    if primary.is_none() && secondary.is_none() && tertiary.is_none() {
        return None;
    }
    Some(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

fn window_from_parts(
    percent: f64,
    reset_in_sec: i64,
    now_secs: i64,
    window_minutes: i64,
) -> Option<RateWindow> {
    let mut p = percent;
    if (0.0..=1.0).contains(&p) {
        p *= 100.0;
    }
    let reset_epoch = now_secs + reset_in_sec.max(0);
    let resets_at = env::epoch_to_iso8601(reset_epoch)?;
    Some(RateWindow {
        used_percent: p.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
    })
}

pub fn parse_windows(
    text: &str,
    now_secs: i64,
    include_monthly: bool,
) -> Result<Usage, FetchError> {
    if looks_signed_out(text) {
        return Err(FetchError::Unauthorized(
            "opencode: signed-out response body".to_string(),
        ));
    }

    if let Some(usage) = try_parse_json_usage(text, now_secs, include_monthly) {
        if usage.primary.is_some() || usage.secondary.is_some() || usage.tertiary.is_some() {
            return Ok(usage);
        }
    }

    if let Some(usage) = parse_windows_regex(text, now_secs, include_monthly) {
        if usage.primary.is_some() || usage.secondary.is_some() || usage.tertiary.is_some() {
            return Ok(usage);
        }
    }

    Err(FetchError::Decode(
        "opencode: no usage windows in response".to_string(),
    ))
}

pub fn load_cookie_header() -> Result<String, FetchError> {
    let jar = browser_cookies::chrome_cookies_for(DOMAIN).map_err(FetchError::from)?;
    cookie_header_from_jar(&jar)
}

pub async fn load_cookie_header_async() -> Result<String, FetchError> {
    let jar = browser_cookies::chrome_cookies_for_async(DOMAIN)
        .await
        .map_err(FetchError::from)?;
    cookie_header_from_jar(&jar)
}

fn cookie_header_from_jar(jar: &CookieJar) -> Result<String, FetchError> {
    if !has_session_cookie(jar) {
        return Err(FetchError::NoSession(format!(
            "no opencode auth cookie in browser ({})",
            jar.session_absence_detail()
        )));
    }
    // A session cookie exists but nothing survives the filter, so the browser
    // holds a login that cannot be used: found, not absent.
    request_cookie_header(jar).ok_or_else(|| {
        FetchError::CredentialUnusable("opencode auth cookies empty after filter".to_string())
    })
}

pub struct OpenCodeProvider {
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
}

impl OpenCodeProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::from_env()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
        }
    }

    /// The bare `cookie:<domain>` deposit, if one exists.
    ///
    /// Looked up here rather than carried on the handle because the local lane's
    /// handle is `ImplicitLocal` and holds no capability -- the fallback is a
    /// property of the PROVIDER's configuration, not of the handle being fetched.
    fn bare_vault_handle(
        &self,
    ) -> Result<Option<crate::credential_source::VaultCapability>, FetchError> {
        if self.credential_source.is_none() {
            return Ok(None);
        }
        let handles = self
            .handle_loader
            .opencode_handles()
            .map_err(|error| FetchError::Internal(error.to_string()))?;
        Ok(match cookie_lane(handles, COOKIE_FAMILY) {
            CookieLane::LocalWithFallback(Some(handle)) => handle.vault_capability().cloned(),
            _ => None,
        })
    }

    async fn vault_cookie(
        &self,
        capability: &crate::credential_source::VaultCapability,
    ) -> Result<String, FetchError> {
        let source = self
            .credential_source
            .as_ref()
            .ok_or_else(|| FetchError::NoSession("no credential source configured".to_string()))?;
        let mut credential = source
            .get(capability, 120_000)
            .await
            .map_err(|error| FetchError::Upstream(error.to_string()))?;
        crate::credential_source::take_utf8_payload(&mut credential.payload)
    }
}

impl Default for OpenCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for OpenCodeProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        if self.credential_source.is_none() {
            return Ok(vec![CredentialHandle::implicit()]);
        }
        // Precedence is expressed by WHICH lanes exist, not by a choice inside the
        // fetch: every handle returned here becomes its own slot and is fetched
        // independently, so enumerating both would make both serve and leave the
        // emission gate to pick one identity-less entry by an invisible tie-break.
        // See `vault_handles::cookie_lane`.
        Ok(
            match cookie_lane(self.handle_loader.opencode_handles()?, COOKIE_FAMILY) {
                CookieLane::VaultOnly(handles) => handles,
                CookieLane::LocalWithFallback(_) => vec![CredentialHandle::implicit()],
            },
        )
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let (cookie, source) = if let Some(capability) = handle.vault_capability() {
                (self.vault_cookie(capability).await?, "vault")
            } else {
                // Local lane primary, bare deposit as the fallback INSIDE this one
                // fetch rather than as a second slot -- a host where the live store
                // cannot be read at all (Windows App-Bound Encryption, or no
                // browser) otherwise has no lane, while a host where it can keeps
                // the fresher source.
                match load_cookie_header_async().await {
                    Ok(cookie) => (cookie, SOURCE_LABEL),
                    Err(local_error) => match self.bare_vault_handle()? {
                        Some(capability) => (self.vault_cookie(&capability).await?, "vault"),
                        None => return Err(local_error),
                    },
                }
            };
            let workspace_id = fetch_workspace_id(&self.http, &cookie).await?;
            let text = fetch_subscription_text(&self.http, &cookie, &workspace_id).await?;
            let now = Utc::now().timestamp();
            let usage = parse_windows(&text, now, false)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, source, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod stage_tests {
    use super::*;

    /// The stage name reaches the published error, for every call this provider makes.
    ///
    /// Without it an HTTP 500 from any of three server functions produces one
    /// indistinguishable string, and the reading that requires no evidence --
    /// "the site is down" -- is the one a reader lands on. A live probe found
    /// the workspaces and billing functions answering while subscription
    /// returned 500; none of that was visible in what we published.
    #[test]
    fn every_stage_names_itself_in_the_published_error() {
        for stage in ["workspaces", "subscription", "billing"] {
            let published = FetchError::Upstream("HTTP 500".to_string())
                .stage(stage)
                .to_string();
            assert!(
                published.contains(stage),
                "the {stage} call must name itself: {published}"
            );
            // Not vacuous: the original message survives beside the stage, so
            // this cannot pass by replacing the error with its stage name.
            assert!(
                published.contains("HTTP 500"),
                "the upstream message must survive: {published}"
            );
        }
    }

    /// The stage reaches the error through the real call, not just the helper.
    ///
    /// Testing `FetchError::stage` alone proves the helper works and says
    /// nothing about whether any call site uses it -- deleting the `.stage()`
    /// from `server_get` leaves those tests green. This drives a real HTTP 500
    /// through the same function the provider calls.
    #[tokio::test]
    async fn a_failing_call_names_its_stage_through_server_get() {
        let (base, task) = crate::loopback::serve_once(500, b"{\"status\":500}".to_vec()).await;
        let client = reqwest::Client::new();
        let error = server_get(&client, &base, Vec::new(), "subscription")
            .await
            .expect_err("a 500 must fail");
        let _ = task.await;

        let published = error.to_string();
        assert!(
            published.contains("subscription"),
            "the failing stage must name itself on the wire: {published}"
        );
        // Not vacuous: the upstream detail survives beside the stage name, so a
        // call site that replaced the error entirely would fail here.
        assert!(
            published.contains("500"),
            "the upstream status must survive: {published}"
        );
    }

    /// A null in the server-function envelope is recognised as "nothing here".
    ///
    /// The live capture from a workspace with no subscription. Before this, the
    /// body parsed as neither a bare null nor JSON, so the caller retried as a
    /// POST, that retry answered 500, and the account was published as an
    /// upstream failure for weeks.
    #[test]
    fn an_enveloped_null_states_that_there_is_nothing_here() {
        // LIVE CAPTURE, 2026-08-14, subscription server function.
        let live = ";0x00000041;((self.$R=self.$R||{})\
[\"server-fn:18cbb183f80b87d88d59\"]=[],null)";
        assert!(
            is_explicit_null(live),
            "the enveloped null must be recognised: {live}"
        );
    }

    /// Every server-function body this provider parses is checked for a null.
    ///
    /// The subscription call was fixed first and the workspaces call had the
    /// identical defect one level up: a stated "nothing here" yields no ids,
    /// falls through, and is published as our parse failure. Counting the sites
    /// rather than testing one, so the next call added to this provider cannot
    /// quietly reintroduce it.
    #[test]
    fn every_parsed_server_body_is_checked_for_a_null() {
        let source = include_str!("opencode.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        // Four bodies get parsed: workspaces GET and its POST retry,
        // subscription GET and its POST retry.
        // Calls take a reference, so this counts uses and not the definition.
        let checks = production.matches("is_explicit_null(&").count();
        assert_eq!(
            checks, 4,
            "{checks} null check(s) for 4 parsed bodies: an unchecked body \
             publishes a fact about the account as a defect of ours"
        );
    }

    /// A payload carrying data is not mistaken for a null.
    ///
    /// The must-not-fire control, and it holds a real builder rather than a
    /// hand-made blob: the two directions cost differently. Missing a null
    /// produces the false failure above, while a false null publishes "no quota
    /// here" for an account that has some -- which reads as a true fact about
    /// the user and is the more expensive mistake.
    #[test]
    fn a_payload_with_data_is_not_read_as_null() {
        // LIVE CAPTURE, 2026-08-14, billing server function on the same host.
        let live = ";0x0000024c;((self.$R=self.$R||{})\
[\"server-fn:18cbabdd5145c658f84d\"]=[],($R=>$R[0]={customerID:\"cus_x\",\
balance:0,monthlyLimit:null,monthlyUsage:null})(self.$R))";
        assert!(
            !is_explicit_null(live),
            "a payload with fields must not read as null: {live}"
        );
        // The word appears inside it as a FIELD VALUE, which is exactly what a
        // looser match on "null" anywhere would trip over.
        assert!(live.contains("null"), "the control must contain the word");
    }

    /// Every send in this provider names its stage, including the POST retries.
    ///
    /// Found by deploying the first version and reading the wire: the published
    /// error was unchanged, because both server functions retry as a POST when
    /// the GET parses to nothing and those two sends bypass `server_get`. The
    /// live 500 comes from the subscription RETRY, not the call, which is a
    /// materially different fact -- the GET answers.
    ///
    /// Counts sites rather than asserting on one, so a new unstaged send fails
    /// here instead of silently publishing an anonymous error.
    #[test]
    fn every_send_in_this_provider_names_its_stage() {
        let source = include_str!("opencode.rs");
        let production = source
            .split_once("#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        let sends = production.matches(".send(client)").count();
        let staged = production.matches("error.stage(").count();
        assert_eq!(
            sends, staged,
            "{sends} send(s) and {staged} stage name(s): an unstaged send publishes \
             an error naming no call, which is what made a retry failure read as a \
             site outage"
        );
    }

    /// Naming a stage must not change how the failure is classified.
    ///
    /// The variant decides transient versus non-transient, which decides whether
    /// a cached window survives. A helper that rebuilt the error into one
    /// variant would move every stage-named failure onto the wrong side of that
    /// line, and nothing about the published string would show it.
    #[test]
    fn naming_a_stage_preserves_the_variant() {
        assert!(matches!(
            FetchError::Upstream("x".into()).stage("s"),
            FetchError::Upstream(_)
        ));
        assert!(matches!(
            FetchError::Unauthorized("x".into()).stage("s"),
            FetchError::Unauthorized(_)
        ));
        assert!(matches!(
            FetchError::Decode("x".into()).stage("s"),
            FetchError::Decode(_)
        ));
        // A bare status has no message to prefix and must come back untouched:
        // widening it to a string would lose the code auth reporting reads.
        assert!(matches!(
            FetchError::ProviderStatus(500).stage("s"),
            FetchError::ProviderStatus(500)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_cookies::Cookie;

    const SUBSCRIPTION_FIXTURE: &str = r#"{
      "rollingUsage": { "usagePercent": 42.5, "resetInSec": 7200 },
      "weeklyUsage": { "usagePercent": 10, "resetInSec": 86400 }
    }"#;

    const GO_HTML_FIXTURE: &str = r#"
    rollingUsage: { usagePercent: 5, resetInSec: 18000 },
    weeklyUsage: { usagePercent: 20, resetInSec: 500000 },
    monthlyUsage: { usagePercent: 55, resetInSec: 2000000 }
    "#;

    const SIGNED_OUT: &str = r#"{"error":"Please sign in to continue","login":true}"#;

    #[test]
    fn brace_less_block_with_a_multibyte_character_at_the_cap_does_not_panic() {
        // window_block falls back to a fixed BYTE budget when no closing brace is
        // found. The response carries user-chosen text (workspace and plan names),
        // so a multibyte character can straddle that cutoff; slicing at the raw
        // byte offset would panic, and a fetch panic is classified non-transient,
        // which would make a working provider read as absent rather than degraded.
        let mut text = String::from("rollingUsage");
        text.push_str(&"a".repeat(2500 - "rollingUsage".len() - 1));
        text.push('\u{e9}'); // straddles the 2500-byte fallback cap
        text.push_str("trailing");
        assert!(
            !text.contains('}'),
            "the fallback path requires a brace-less block"
        );

        let block = window_block(&text, "rollingUsage").expect("block is present");
        assert!(
            block.len() < 2500,
            "the cap must round back off the split character"
        );
    }

    #[test]
    fn parses_subscription_rolling_and_weekly() {
        let now = 1_700_000_000_i64;
        let usage = parse_windows(SUBSCRIPTION_FIXTURE, now, false).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 42.5);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, env::epoch_to_iso8601(now + 7200));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 10.0);
        assert_eq!(secondary.window_minutes, Some(10080));
        assert!(usage.tertiary.is_none());
    }

    #[test]
    fn parses_go_html_with_monthly() {
        let now = 1_700_000_000_i64;
        let usage = parse_windows(GO_HTML_FIXTURE, now, true).unwrap();
        assert!(usage.primary.is_some());
        assert!(usage.secondary.is_some());
        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 55.0);
        assert_eq!(tertiary.window_minutes, Some(43200));
    }

    /// A healthy payload must not be read as a sign-in page.
    ///
    /// This predicate is consulted *before* parsing, so unlike the other HTML
    /// providers here -- where it is a last resort after extraction has already
    /// failed -- a false positive costs a working provider its usage. It would
    /// report the credential as rejected, which asks someone to re-login a
    /// session that never expired.
    ///
    /// The needles are broad by necessity (`login` matches anywhere in the
    /// body), so this asserts the real fixtures against them: adding a needle
    /// that a healthy payload contains has to fail here rather than in
    /// production. The signed-out fixture is the control, so this cannot pass by
    /// a predicate that never matches anything.
    #[test]
    fn a_healthy_payload_is_not_mistaken_for_a_sign_in_page() {
        for (name, fixture) in [
            ("subscription", SUBSCRIPTION_FIXTURE),
            ("go html", GO_HTML_FIXTURE),
        ] {
            assert!(
                !looks_signed_out(fixture),
                "{name} fixture classified as signed out"
            );
            // Not vacuous: the fixture really is one this provider parses, so a
            // pass here is about the predicate and not about an inert string.
            assert!(
                parse_windows(fixture, 1_700_000_000, false).is_ok(),
                "{name} fixture must be parseable"
            );
        }

        assert!(
            looks_signed_out(SIGNED_OUT),
            "control: a real sign-in body must still be detected"
        );
    }

    #[test]
    fn signed_out_body_is_unauthorized() {
        assert!(matches!(
            parse_windows(SIGNED_OUT, 0, false),
            Err(FetchError::Unauthorized(_))
        ));
    }

    #[test]
    fn percent_without_reset_keeps_window() {
        let html = r#"{"rollingUsage":{"usagePercent":50},"weeklyUsage":{"usagePercent":10,"resetInSec":3600}}"#;
        let usage = parse_windows(html, 1_700_000_000, false).unwrap();
        let primary = usage.primary.expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(usage.secondary.unwrap().used_percent, 10.0);
    }

    #[test]
    fn exhausted_percent_without_reset_is_kept() {
        let html = r#"{"rollingUsage":{"usagePercent":100}}"#;
        let primary = parse_windows(html, 1_700_000_000, false)
            .unwrap()
            .primary
            .expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn nonfinite_percent_drops_that_window() {
        // A "NaN" string percent must never become a served value; the rolling
        // window is dropped while a well-formed weekly still emits.
        let json = r#"{"rollingUsage":{"usagePercent":"NaN"},
                       "weeklyUsage":{"usagePercent":10,"resetInSec":3600}}"#;
        let usage = parse_windows(json, 1_700_000_000, false).unwrap();
        assert!(usage.primary.is_none(), "NaN percent → no window");
        assert_eq!(usage.secondary.unwrap().used_percent, 10.0);
    }

    #[test]
    fn out_of_range_reset_omits_reset_without_dropping_window() {
        // An absurd resetAt ("1e308") is not a real timestamp; the window remains
        // available with no reset rather than receiving a fabricated timestamp.
        let json = r#"{"rollingUsage":{"usagePercent":50,"resetAt":1e308},
                       "weeklyUsage":{"usagePercent":10,"resetInSec":3600}}"#;
        let usage = parse_windows(json, 1_700_000_000, false).unwrap();
        let primary = usage.primary.expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(usage.secondary.unwrap().used_percent, 10.0);
    }

    #[test]
    fn go_html_with_only_rolling_still_serves_primary() {
        // A response missing the weekly block must still serve the rolling/primary
        // window instead of dropping everything.
        let html = r#" rollingUsage: { usagePercent: 5, resetInSec: 18000 } "#;
        let usage = parse_windows(html, 1_700_000_000, false).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 5.0);
        assert!(usage.secondary.is_none(), "no weekly block → no secondary");
    }

    #[test]
    fn request_cookie_header_filters_names() {
        let jar = CookieJar {
            cookies: vec![
                Cookie {
                    name: "auth".into(),
                    value: "tok".into(),
                    host_key: "opencode.ai".into(),
                },
                Cookie {
                    name: "other".into(),
                    value: "x".into(),
                    host_key: "opencode.ai".into(),
                },
            ],
        };
        assert_eq!(request_cookie_header(&jar).as_deref(), Some("auth=tok"));
    }

    #[test]
    fn parse_workspace_ids_from_serialized() {
        let text = r#"id:"wrk_abc123xyz",name:"Main""#;
        let ids = parse_workspace_ids(text);
        assert_eq!(ids, vec!["wrk_abc123xyz"]);
    }

    #[test]
    fn parse_workspace_ids_handles_multibyte_names() {
        // A workspace name is user-chosen, so the response can contain any
        // UTF-8. Scanning must use byte comparisons, because slicing a &str at
        // an arbitrary byte offset panics if the offset lands inside a multibyte
        // character. The refresher contains such a panic rather than crashing,
        // but records it as a permanent provider failure, which discards the
        // usage figures cached from the last successful fetch.
        let text = "{\"name\":\"Café 中文 😀\",\"id\":\"wrk_abc123xyz\"}";
        let ids = parse_workspace_ids(text);
        assert_eq!(ids, vec!["wrk_abc123xyz"]);
    }

    #[test]
    fn computed_sub_one_percent_is_not_rescaled_to_exhausted() {
        // used=1, limit=100 -> a genuine 1% computed ratio. The fraction
        // heuristic must NOT touch the computed path, else 1.0 is misread as a
        // 0..1 fraction and rescaled to a false 100% (CodexBar v0.45.2 fix).
        let map = serde_json::json!({ "used": 1, "limit": 100 })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(percent_from_map(&map), Some(1.0));
    }

    #[test]
    fn computed_half_percent_stays_half() {
        let map = serde_json::json!({ "used": 1, "limit": 200 })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(percent_from_map(&map), Some(0.5));
    }

    #[test]
    fn direct_fraction_is_scaled_but_direct_percent_is_not() {
        // A direct percent field may be a 0..1 fraction (scaled to a percent) or
        // already a 0..100 percent (left alone). `percent` is a PERCENT_KEYS
        // entry that is not a used/limit key, so it routes through the direct
        // path and exercises the gated heuristic.
        let frac = serde_json::json!({ "percent": 0.5 })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(percent_from_map(&frac), Some(50.0));
        let pct = serde_json::json!({ "percent": 50.0 })
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(percent_from_map(&pct), Some(50.0));
    }
}
