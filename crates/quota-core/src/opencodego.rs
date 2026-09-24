//! OpenCode Go usage — an API key when one is configured; otherwise the console
//! JSON API first, the legacy HTML `/go` page as fallback.
//!
//! API-KEY LANE. `GET https://opencode.ai/zen/go/v1/usage` with
//! `Authorization: Bearer <key>`, the key coming from a vault `apikey:opencode`
//! credential or the `OPENCODE_API_KEY` environment variable. It needs no
//! browser, so it is the lane for Windows and headless hosts that cannot read
//! Chrome's cookie store. When a key is present it is the ONLY lane this
//! provider enumerates (see [`OpenCodeGoProvider::handles`]).
//! Ported from CodexBar v0.65.0 `OpenCodeGoUsageFetcher.fetchAPIUsage` /
//! `parseAPIUsage` (present upstream since v0.54.0); payload shapes come from
//! `Tests/CodexBarTests/OpenCodeGoUsageFetcherErrorTests.swift` and
//! `OpenCodeGoWebOverlayTests.swift` at that tag. VERIFICATION of this lane:
//! fixture-verified only, NOT live-verified -- no OpenCode API key exists on the
//! host it was written on.
//!
//! What the API lane does NOT do, deliberately:
//! - It reports no "no subscription" verdict. Upstream's API lane has none: every
//!   body without `usage.rolling` is a parse failure there, and no captured
//!   response shows what an unsubscribed key receives. So such a body is
//!   `decode_failed` here too, until someone captures the real answer.
//! - It publishes no account identity: the response carries none.
//! - It does not publish upstream's extra "Renews" window built from
//!   `renewAt`; the console lane publishes no such window either.
//!
//! OpenCode has migrated workspaces to a new console. For a migrated workspace
//! the legacy `/workspace/<id>/go` page redirects to `/console/login` and serves
//! an empty shell, so the payload this provider used to scrape no longer exists
//! there -- and that redirect is NOT an expired session, however much it looks
//! like one. The console answers the same questions over JSON: workspace ids
//! from `/console/api/orgs`, Go meters from `/console/api/go/status`. The legacy
//! scrape stays as the fallback for workspaces that have not migrated.
//!
//! VERIFICATION: fixture-verified against CodexBar v0.64.1, NOT live-verified --
//! the cookie lane is blocked on this host by a macOS permission, so no live
//! check is available. Ported from
//! `OpenCodeGo/OpenCodeGoUsageFetcher.swift` (console API :428-575, legacy page
//! :402-426, micro-cent meters :533-575), `OpenCodeGo/OpenCodeGoLegacyFallback.swift`
//! (when the legacy page is tried and which error wins), and
//! `OpenCode/OpenCodeWebCookieSupport.swift` (cookie names, shared with
//! `opencode`). JSON fixtures come from
//! `Tests/CodexBarTests/OpenCodeGoConsoleMigrationTests.swift` at the same tag.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    credential_source::CredentialSource,
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    opencode::{
        fetch_workspace_id_at, load_cookie_header_async, looks_signed_out, parse_date_value,
        parse_windows, CONSOLE_SESSION_COOKIE_NAMES, LEGACY_SESSION_COOKIE_NAMES,
        MONTHLY_WINDOW_MINUTES, PERCENT_KEYS, RESET_AT_KEYS, RESET_IN_KEYS, ROLLING_WINDOW_MINUTES,
        USER_AGENT, WEEKLY_WINDOW_MINUTES,
    },
    provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider},
    vault_handles::{handle_id_names_family, VaultHandleLoader},
    LOG_TAG,
};

pub const PROVIDER_NAME: &str = "opencodego";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Usage endpoint for the API-key lane. Answers 401 or 403 for a key it
/// rejects.
const API_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
/// Vault credential family holding an OpenCode API key.
pub const API_KEY_FAMILY: &str = "apikey:opencode";
/// Environment variable upstream reads the API key from.
const API_KEY_ENV: &[&str] = &["OPENCODE_API_KEY"];
/// `source` published for a fetch made with the environment's API key.
const API_SOURCE: &str = "api";
/// `source` published for a fetch made with a vault-held API key.
const VAULT_SOURCE: &str = "vault";

/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Console workspace list, fetched on the session cookie with no workspace
/// header. A moved or renamed route reads as an outage while the defect is
/// this line.
pub const CONSOLE_WORKSPACES_PATH: &str = "/console/api/orgs";
/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Go subscription meters for the workspace named by
/// [`CONSOLE_WORKSPACE_HEADER`]. Same rotation risk as the orgs route.
pub const CONSOLE_GO_STATUS_PATH: &str = "/console/api/go/status";
/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Header naming the workspace a console request is about. The console answers
/// HTTP 400 when it is missing, which reads as an endpoint failure while the
/// defect is the absent header.
pub const CONSOLE_WORKSPACE_HEADER: &str = "x-org-id";

/// Keys a console meter names its fill with, in upstream's order. The console's
/// own spellings are the micro-cent pair; the generic ones ride along because
/// upstream's `parseWindow` accepts them.
const METER_USED_KEYS: &[&str] = &[
    "used",
    "usage",
    "consumed",
    "count",
    "usedTokens",
    "usedMicroCents",
];
const METER_LIMIT_KEYS: &[&str] = &[
    "limit",
    "total",
    "quota",
    "max",
    "cap",
    "tokenLimit",
    "limitMicroCents",
];

/// Distinctive text inside the [`FetchError::Decode`] a redirected `/go` page
/// produces, so the console-first fallback can recognise that verdict without
/// re-deriving it from the URL.
const REDIRECT_VERDICT: &str = "redirected off the /go page";

/// Where both lanes live, in one place so a test can point the whole fetch at
/// a loopback server. Production never overrides this.
struct Endpoints {
    /// The console JSON API and the legacy pages share one origin.
    origin: String,
    /// The legacy Next.js server-function endpoint the workspace id comes from.
    server_base: String,
    /// The API-key lane's usage endpoint.
    api_usage_url: String,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            origin: crate::opencode::ORIGIN.to_string(),
            server_base: crate::opencode::SERVER_BASE.to_string(),
            api_usage_url: API_USAGE_URL.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Console lane
// ---------------------------------------------------------------------------

/// GET a console endpoint and return the body, owning the status policy.
///
/// Console responses are JSON and report a signed-out session as HTTP 401, so
/// unlike the legacy pages the body must NOT be classified for sign-out: a
/// console payload can mention login routes without saying anything about the
/// session. Only the status line speaks, and only a 401 proves an expired
/// session.
async fn fetch_console_text(
    client: &reqwest::Client,
    cookie: &str,
    url: &str,
    workspace_id: Option<&str>,
) -> Result<String, FetchError> {
    // `JsonRequest::get` already accepts JSON, which is exactly what the
    // console answers.
    let mut request = JsonRequest::get(url)
        .timeout(REQUEST_TIMEOUT)
        .header(Header::new("Cookie", cookie.to_string()))
        .header(Header::new("User-Agent", USER_AGENT.to_string()));
    if let Some(workspace_id) = workspace_id {
        request = request.header(Header::new(
            CONSOLE_WORKSPACE_HEADER,
            workspace_id.to_string(),
        ));
    }
    let response = request.send_raw(client).await?;
    if response.status == 401 {
        return Err(FetchError::Unauthorized(
            "opencodego session expired (console API answered 401)".to_string(),
        ));
    }
    if !(200..300).contains(&response.status) {
        let excerpt: String = String::from_utf8_lossy(&response.body)
            .chars()
            .take(200)
            .collect();
        return Err(FetchError::Upstream(if excerpt.is_empty() {
            format!("HTTP {}", response.status)
        } else {
            format!("HTTP {}: {excerpt}", response.status)
        }));
    }
    let body = response.body_for_parsing()?;
    Ok(String::from_utf8_lossy(body).into_owned())
}

/// Workspace ids out of the console orgs list: the `id` of every row shaped
/// like a console workspace (`wrk_…` or `org_…`), in the order served.
fn parse_console_workspace_ids(text: &str) -> Vec<String> {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str))
        .filter(|id| is_console_workspace_id(id))
        .map(str::to_string)
        .collect()
}

/// `^(?:wrk_|org_)[A-Za-z0-9_-]+$` -- the shape a console workspace id takes,
/// so an unrelated row (an account id, say) is never picked as the workspace.
fn is_console_workspace_id(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("wrk_")
        .or_else(|| value.strip_prefix("org_"))
    else {
        return false;
    };
    !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Also called by `opencode`, which asks the console whether it accepts a
/// cookie before it publishes a signed-out verdict from its legacy workspaces
/// call. A console 401 comes back as `Unauthorized`.
pub(crate) async fn fetch_console_workspace_id(
    client: &reqwest::Client,
    cookie: &str,
    origin: &str,
) -> Result<String, FetchError> {
    let url = format!("{origin}{CONSOLE_WORKSPACES_PATH}");
    let text = fetch_console_text(client, cookie, &url, None)
        .await
        .map_err(|error| error.stage("console workspace list"))?;
    parse_console_workspace_ids(&text)
        .into_iter()
        .next()
        .ok_or_else(|| {
            FetchError::Decode("opencodego: no workspace id in the console orgs list".to_string())
        })
}

async fn fetch_console_go_usage(
    client: &reqwest::Client,
    cookie: &str,
    origin: &str,
    workspace_id: &str,
) -> Result<Usage, FetchError> {
    let url = format!("{origin}{CONSOLE_GO_STATUS_PATH}");
    let text = fetch_console_text(client, cookie, &url, Some(workspace_id))
        .await
        .map_err(|error| error.stage("console go status"))?;
    // A JSON null body, or `access: null`, is the console stating this
    // workspace has no Go plan: a fact about the account, not a failure, and
    // not a reason to try the legacy page.
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Null) => {
            return Err(FetchError::NoQuotaReported(
                "opencodego: workspace has no Go subscription (console)".to_string(),
            ));
        }
        Ok(ref body) if body.get("access").is_some_and(Value::is_null) => {
            return Err(FetchError::NoQuotaReported(
                "opencodego: workspace has no Go subscription (console)".to_string(),
            ));
        }
        _ => {}
    }
    let now = chrono::Utc::now().timestamp();
    parse_console_go_status(&text, now).ok_or_else(|| {
        FetchError::Decode("opencodego: console go status carries no usable meters".to_string())
    })
}

/// The console's Go status payload as usage windows: `fiveHour` is required and
/// becomes the 5h window, `week` the weekly, `month` the monthly -- the same
/// slots and window lengths the legacy scrape publishes, since consumers key on
/// them.
///
/// Returns None (a decode failure to the caller) when the required shape is
/// absent. A present-but-unreadable weekly meter fails the whole payload, as
/// upstream's `buildSnapshot` does: that is a different fact from a meter that
/// was never there.
fn parse_console_go_status(text: &str, now_secs: i64) -> Option<Usage> {
    let root: Value = serde_json::from_str(text).ok()?;
    let access = root.get("access")?.as_object()?;
    let meters = access.get("meters")?.as_object()?;
    let five_hour = meters.get("fiveHour")?.as_object()?;
    // The month meter carries no reset timestamp, so the billing period end
    // stands in for it.
    let period_end = access.get("endsAt").and_then(parse_date_value);

    let encoding = DirectPercent::FractionOrPercent;
    let primary =
        console_meter_window(five_hour, now_secs, ROLLING_WINDOW_MINUTES, None, encoding)?;
    let secondary = match meters.get("week").and_then(Value::as_object) {
        Some(week) => Some(console_meter_window(
            week,
            now_secs,
            WEEKLY_WINDOW_MINUTES,
            None,
            encoding,
        )?),
        None => None,
    };
    let tertiary = meters
        .get("month")
        .and_then(Value::as_object)
        .and_then(|month| {
            console_meter_window(
                month,
                now_secs,
                MONTHLY_WINDOW_MINUTES,
                period_end,
                encoding,
            )
        });
    Some(Usage {
        primary: Some(primary),
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

/// How a meter's direct percent field is to be read.
///
/// Mirrors upstream's `DirectPercentEncoding`. The console and dashboard
/// payloads may send a 0..1 fraction; the usage API always sends 0..100. Reading
/// the API's `0.5` as a fraction would publish 50% for half a percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirectPercent {
    /// A value in 0..=1 is a fraction and is rescaled to a percent.
    FractionOrPercent,
    /// The value is already a percent, whatever its size.
    Percent,
}

/// One meter as a rate window, shared by the console lane and the API lane.
///
/// The console reports micro-cent meters (`usedMicroCents` / `limitMicroCents`,
/// as strings), which the generic window parser cannot key on, so the percent
/// is computed here exactly as upstream's `parseWindow` does: a direct percent
/// field wins (rescaled from a 0..1 fraction only under
/// [`DirectPercent::FractionOrPercent`]), otherwise used/limit -- and the
/// `* 100.0` is what turns that ratio into a percent.
fn console_meter_window(
    meter: &serde_json::Map<String, Value>,
    now_secs: i64,
    window_minutes: i64,
    fallback_reset_epoch: Option<i64>,
    encoding: DirectPercent,
) -> Option<RateWindow> {
    let direct = crate::json_scan::first_finite_f64(meter, PERCENT_KEYS);
    let mut percent = match direct {
        Some(p) => p,
        None => {
            let used = crate::json_scan::first_finite_f64(meter, METER_USED_KEYS)?;
            let limit = crate::json_scan::first_finite_f64(meter, METER_LIMIT_KEYS)?;
            if limit <= 0.0 {
                return None;
            }
            (used / limit) * 100.0
        }
    };
    if direct.is_some()
        && encoding == DirectPercent::FractionOrPercent
        && (0.0..=1.0).contains(&percent)
    {
        percent *= 100.0;
    }
    if !percent.is_finite() {
        return None;
    }
    let reset_epoch = crate::json_scan::first_i64(meter, RESET_IN_KEYS)
        .map(|secs| now_secs + secs.max(0))
        .or_else(|| {
            RESET_AT_KEYS
                .iter()
                .find_map(|key| meter.get(*key))
                .and_then(parse_date_value)
        })
        .or(fallback_reset_epoch);
    Some(RateWindow {
        used_percent: percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: reset_epoch.and_then(crate::env::epoch_to_iso8601),
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
    })
}

// ---------------------------------------------------------------------------
// Legacy lane: the HTML `/go` page, for workspaces that have not migrated
// ---------------------------------------------------------------------------

async fn fetch_go_page_html(
    client: &reqwest::Client,
    cookie: &str,
    origin: &str,
    workspace_id: &str,
) -> Result<String, FetchError> {
    let url = format!("{origin}/workspace/{workspace_id}/go");
    let response = JsonRequest::get(&url)
        .timeout(REQUEST_TIMEOUT)
        .header(Header::new("Cookie", cookie.to_string()))
        .header(Header::new("User-Agent", USER_AGENT.to_string()))
        .header(Header::new(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".to_string(),
        ))
        .send_full(client)
        .await?;
    let text = String::from_utf8_lossy(&response.body).into_owned();
    classify_go_page(
        &text,
        &response.final_url,
        workspace_id,
        chrono::Utc::now().timestamp(),
    )?;
    Ok(text)
}

/// Whether the `/go` request was answered by a different page.
///
/// Checks the PATH rather than the host, which is where this differs from the
/// same defect in `ollama`: that redirect crosses to another host, while this one
/// stays on `opencode.ai` and only moves from `/workspace/<id>/go` to
/// `/console/login`. A host comparison would see no change at all.
///
/// Asks whether the answer still names the resource requested, rather than
/// looking for "login" in the destination. A keyword test on the URL is the same
/// heuristic that failed on the body, one layer up: it recognises only the
/// sign-in spellings someone thought of, where this recognises ANY page that is
/// not the one asked for.
///
/// An empty final URL is NOT a redirect. It means no transport recorded one --
/// every unit fixture constructs a response that way -- and treating absence as
/// evidence would report every test's page as a sign-in.
///
/// WHAT THE VERDICT MEANS CHANGED on 2026-09-23. This predicate was written on
/// 2026-09-19, when a redirect could only be the sign-in page of an expired
/// session. OpenCode has since migrated workspaces to the console, and a
/// migrated workspace redirects here on a session that is perfectly valid, so
/// the verdict now feeds the console-first fallback (see [`fetch_go_usage`])
/// instead of standing as an auth verdict on its own.
fn redirected_off_go_page(final_url: &str, workspace_id: &str) -> bool {
    if final_url.is_empty() {
        return false;
    }
    let expected = format!("/workspace/{workspace_id}/go");
    match final_url.split_once("://") {
        Some((_, rest)) => match rest.split_once('/') {
            Some((_, path)) => !format!("/{path}").starts_with(&expected),
            // A bare origin with no path cannot be the go page.
            None => true,
        },
        // Not a URL we can read: say nothing rather than guess.
        None => false,
    }
}

/// Decide what a fetched `/go` page means, before anything tries to read
/// windows out of it.
///
/// Four outcomes share one symptom -- no windows -- and they need different
/// answers from whoever sees them, so the page is classified once here rather
/// than inferred from a parse failure downstream:
///
/// * redirected off the page: the workspace does not serve this page at all.
///   Until 2026-09-19 this returned `Unauthorized`, written when a redirect
///   could only mean a signed-out session. OpenCode's console migration makes
///   that reading false in the expensive direction: a migrated workspace
///   redirects on a VALID session, and telling the operator to sign in again
///   cannot help, because the fault is that the old endpoint no longer serves
///   the workspace. Only the console API's 401 proves an expired session now.
/// * signed out: the page itself carries login markers -- the session is gone
///   and a browser login fixes it.
/// * no Go plan: the login is fine and there is nothing to report, which is not
///   a failure at all.
/// * anything else that yields no windows: our parser and the page disagree,
///   which is a defect on this side.
///
/// The order is load-bearing. A redirected page is answered before its body is
/// consulted (the sign-in shell arrives with a 200 and parseable-looking
/// nonsense), a signed-out page can carry an unsubscribed-looking record, and a
/// page with no plan does not parse, so each check must come before the ones it
/// would otherwise be mistaken for.
fn classify_go_page(
    text: &str,
    final_url: &str,
    workspace_id: &str,
    now_secs: i64,
) -> Result<(), FetchError> {
    // FIRST, because it is the only check that does not depend on the body.
    // Decode, not Unauthorized: the redirect says the page moved, and says
    // nothing about the session. The console-first fallback recognises this
    // verdict by its marker and answers with what the console said instead.
    if redirected_off_go_page(final_url, workspace_id) {
        return Err(FetchError::Decode(format!(
            "opencodego: {REDIRECT_VERDICT} ({final_url}); the workspace may \
             have migrated to the console"
        )));
    }
    if looks_signed_out(text) {
        return Err(FetchError::Unauthorized(
            "opencodego session expired (go page)".to_string(),
        ));
    }
    if looks_unsubscribed(text) {
        return Err(FetchError::NoQuotaReported(
            "opencodego: workspace has no Go subscription".to_string(),
        ));
    }
    if parse_windows(text, now_secs, true).is_err() {
        return Err(FetchError::Decode(
            "opencodego: usage fields missing on /go page".to_string(),
        ));
    }
    Ok(())
}

/// Whether the page is stating that this workspace has no Go plan.
///
/// The page renders the account record inline, and an unsubscribed workspace
/// carries an explicit `subscription:null` in it. That is the server making a
/// statement, not failing: there are no windows to report because there is no
/// plan, and the login is perfectly good.
///
/// Distinguishing it matters beyond the wording. Without this the page falls
/// through to the parser, fails to yield windows, and reports `decode_failed` —
/// which classifies as a stale browser login and tells an operator to sign in
/// again, when signing in changes nothing. The remedy for one is a login and for
/// the other is a subscription, so collapsing them sends people to the wrong
/// one.
///
/// Requires all three fields rather than any of them. A single null could be a
/// mid-rollout field or a lapsed payment method on a live plan, whereas a record
/// that names the subscription three ways and nulls each is unambiguous. And an
/// account that HAS a plan populates them, so a wrong positive here would hide a
/// real subscription's usage — the direction worth being strict about.
fn looks_unsubscribed(text: &str) -> bool {
    text.contains("subscription:null")
        && text.contains("subscriptionID:null")
        && text.contains("subscriptionPlan:null")
}

// ---------------------------------------------------------------------------
// Console first, legacy as the fallback
// ---------------------------------------------------------------------------

/// Whether a `name=value; name=value` cookie header carries a non-empty cookie
/// under one of the given names. Read off the HEADER rather than a jar because
/// the vault lane serves a pasted header and never builds one.
fn has_named_cookie(header: &str, names: &[&str]) -> bool {
    header.split(';').any(|pair| {
        pair.split_once('=')
            .is_some_and(|(name, value)| names.contains(&name.trim()) && !value.trim().is_empty())
    })
}

/// Whether the legacy page is worth trying after the console lane failed.
///
/// Ported from CodexBar v0.64.1 `OpenCodeGoLegacyFallback.shouldTryLegacy`:
/// "no Go plan" is a verdict about the account, which the legacy page has
/// nothing to add to, and without a legacy session cookie the attempt cannot
/// succeed -- its error would only mask the console's.
fn should_try_legacy(console_error: &FetchError, cookie: &str) -> bool {
    if matches!(console_error, FetchError::NoQuotaReported(_)) {
        return false;
    }
    has_named_cookie(cookie, LEGACY_SESSION_COOKIE_NAMES)
}

/// Which lane's error to publish when both failed.
///
/// Ported from CodexBar v0.64.1 `OpenCodeGoLegacyFallback`, with one case
/// stronger than upstream's generic rule and checked FIRST: a redirect off the
/// `/go` page is what a MIGRATED workspace looks like from the legacy side, so
/// the console's verdict stands whether or not the console cookie is visible
/// in the header -- and in BOTH directions. A console 401 there is the one
/// proof of an expired session, and any other console answer is the truth about
/// this fetch; the redirect itself says nothing about the session in either
/// case.
///
/// For every other legacy failure the upstream rules apply unchanged: the
/// console's own 401 is its own session's verdict, so the legacy session
/// answers for itself; otherwise a failed legacy read must not turn a console
/// access failure into invalid auth or absent Go usage, and the console's
/// error stands when the jar carries the console session cookie.
fn resolve_dual_failure(
    console_error: FetchError,
    legacy_error: FetchError,
    cookie: &str,
) -> FetchError {
    // The redirect verdict is checked FIRST, so it never outranks the console
    // in either direction. A redirect off the `/go` page is what a MIGRATED
    // workspace looks like from the legacy side: the page moved, and the
    // console -- the workspace's real home -- has the say. If the console
    // answered 401 that is the one proof of an expired session; publishing the
    // redirect's Decode instead would send a reader hunting a parser bug while
    // hiding the one fact they need. Any other console answer stands for the
    // same reason.
    if is_redirect_verdict(&legacy_error) {
        return console_error;
    }
    // For every OTHER legacy failure, upstream's rule: the console's own 401 is
    // its own session's verdict, so the legacy session answers for itself.
    if matches!(console_error, FetchError::Unauthorized(_)) {
        return legacy_error;
    }
    // A failed legacy read cannot turn a console access failure into invalid
    // auth or absent Go usage: the console's error stands when the jar carries
    // the console session cookie.
    if has_named_cookie(cookie, CONSOLE_SESSION_COOKIE_NAMES) {
        return console_error;
    }
    legacy_error
}

fn is_redirect_verdict(error: &FetchError) -> bool {
    matches!(error, FetchError::Decode(message) if message.contains(REDIRECT_VERDICT))
}

/// The whole fetch: console lane first, legacy page as the fallback, with the
/// fallback decision made per stage (workspace id, then usage) exactly as
/// upstream wraps each call.
async fn fetch_go_usage(
    client: &reqwest::Client,
    cookie: &str,
    endpoints: &Endpoints,
) -> Result<Usage, FetchError> {
    let workspace_id = match fetch_console_workspace_id(client, cookie, &endpoints.origin).await {
        Ok(id) => id,
        Err(console_error) => {
            if !should_try_legacy(&console_error, cookie) {
                return Err(console_error);
            }
            // No console check inside: the console has just been asked and failed.
            match fetch_workspace_id_at(client, cookie, &endpoints.server_base, None).await {
                Ok(id) => id,
                Err(legacy_error) => {
                    return Err(resolve_dual_failure(console_error, legacy_error, cookie));
                }
            }
        }
    };
    match fetch_console_go_usage(client, cookie, &endpoints.origin, &workspace_id).await {
        Ok(usage) => Ok(usage),
        Err(console_error) => {
            if !should_try_legacy(&console_error, cookie) {
                return Err(console_error);
            }
            match fetch_legacy_go_usage(client, cookie, &endpoints.origin, &workspace_id).await {
                Ok(usage) => Ok(usage),
                Err(legacy_error) => Err(resolve_dual_failure(console_error, legacy_error, cookie)),
            }
        }
    }
}

async fn fetch_legacy_go_usage(
    client: &reqwest::Client,
    cookie: &str,
    origin: &str,
    workspace_id: &str,
) -> Result<Usage, FetchError> {
    let text = fetch_go_page_html(client, cookie, origin, workspace_id).await?;
    let now = chrono::Utc::now().timestamp();
    parse_windows(&text, now, true)
}

// ---------------------------------------------------------------------------
// API-key lane
// ---------------------------------------------------------------------------

/// The usage API's answer as windows: `usage.rolling` is required and becomes
/// the 5h window, `usage.weekly` the weekly, `usage.monthly` the monthly -- the
/// same slots and window lengths the cookie lanes publish.
///
/// Ported from upstream's `parseAPIUsage` + `buildSnapshot(directPercentEncoding:
/// .percent)`: the API's percents are always 0..100, a present-but-unreadable
/// weekly window fails the whole payload, and an unreadable monthly one is
/// dropped. Every failure is `Decode`; see the module doc for why there is no
/// "no subscription" verdict on this lane.
fn parse_api_usage(text: &str, now_secs: i64) -> Result<Usage, FetchError> {
    let missing =
        || FetchError::Decode("opencodego: usage API response carries no usage fields".to_string());
    let root: Value = serde_json::from_str(text).map_err(|_| missing())?;
    let usage = root
        .get("usage")
        .and_then(Value::as_object)
        .ok_or_else(missing)?;
    let rolling = usage
        .get("rolling")
        .and_then(Value::as_object)
        .ok_or_else(missing)?;
    let encoding = DirectPercent::Percent;
    let primary = console_meter_window(rolling, now_secs, ROLLING_WINDOW_MINUTES, None, encoding)
        .ok_or_else(missing)?;
    let secondary = match usage.get("weekly").and_then(Value::as_object) {
        Some(weekly) => Some(
            console_meter_window(weekly, now_secs, WEEKLY_WINDOW_MINUTES, None, encoding)
                .ok_or_else(missing)?,
        ),
        None => None,
    };
    let tertiary = usage
        .get("monthly")
        .and_then(Value::as_object)
        .and_then(|monthly| {
            console_meter_window(monthly, now_secs, MONTHLY_WINDOW_MINUTES, None, encoding)
        });
    Ok(Usage {
        primary: Some(primary),
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

/// The request both API-key sub-lanes send.
fn api_usage_request(url: &str, key: &str) -> JsonRequest {
    // `JsonRequest::get` already sends `Accept: application/json`, which is
    // the other header upstream sets.
    JsonRequest::get(url).timeout(REQUEST_TIMEOUT).bearer(key)
}

/// Whether a vault handle carries the API key rather than a cookie deposit.
fn is_api_key_handle(handle: &CredentialHandle) -> bool {
    handle
        .vault_credential_id()
        .is_some_and(|id| handle_id_names_family(id, API_KEY_FAMILY))
}

fn env_api_key() -> Option<String> {
    env::first_env(API_KEY_ENV)
}

pub struct OpenCodeGoProvider {
    http: reqwest::Client,
    vault: crate::cookie_vault::CookieVault,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    /// Reads the environment's API key. A field so tests can supply a key
    /// without mutating the process environment other tests share.
    env_api_key: fn() -> Option<String>,
    endpoints: Endpoints,
}

impl OpenCodeGoProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            vault: crate::cookie_vault::CookieVault::new(
                credential_source.clone(),
                Arc::clone(&handle_loader),
                crate::opencode::COOKIE_FAMILY,
            ),
            credential_source,
            handle_loader,
            env_api_key,
            endpoints: Endpoints::production(),
        }
    }

    /// Vault handles in the `apikey:opencode` family. The loader's opencodego
    /// mapping also holds the shared `cookie:opencode.ai` deposits, so the
    /// family is filtered here.
    fn vault_api_key_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_none() {
            return Ok(Vec::new());
        }
        Ok(self
            .handle_loader
            .opencodego_handles()?
            .into_iter()
            .filter(is_api_key_handle)
            .collect())
    }

    /// The environment-key lane: 401/403 become `Unauthorized`, which is the
    /// rejected-key class, through the shared request helper.
    async fn fetch_env_api(&self, key: &str) -> Result<ProviderUsage, FetchError> {
        let body = api_usage_request(&self.endpoints.api_usage_url, key)
            .send(&self.http)
            .await?;
        let usage = parse_api_usage(
            &String::from_utf8_lossy(&body),
            chrono::Utc::now().timestamp(),
        )?;
        Ok(ProviderUsage::healthy(
            PROVIDER_NAME,
            None,
            API_SOURCE,
            usage,
        ))
    }

    /// The vault-key lane. The status-first helper keeps the HTTP status, so a
    /// 401/403 is `ProviderStatus`, which classifies as rejected just as
    /// `Unauthorized` does, and a 401 is reported back to the vault so it can
    /// mark the stored key.
    async fn fetch_vault_api(&self, handle: &CredentialHandle) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match crate::credential_source::get_vault_credential(
            credential_source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => {
                eprintln!(
                    "{LOG_TAG} warning: opencodego vault credential.get failed ({}): {error:?}",
                    handle.stable_id()
                );
                return FetchAttempt::unverified_vault_failure(error);
            }
        };
        let record_version = credential.record_version;
        let key = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(key) => key,
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        let result = async {
            let response = api_usage_request(&self.endpoints.api_usage_url, key.trim())
                .send_provider_status_first(&self.http, PROVIDER_NAME)
                .await?;
            parse_api_usage(
                &String::from_utf8_lossy(&response.body),
                chrono::Utc::now().timestamp(),
            )
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
        FetchAttempt::from_provider_usage(
            result.map(|usage| ProviderUsage::healthy(PROVIDER_NAME, None, VAULT_SOURCE, usage)),
        )
    }
}

#[async_trait]
impl UsageProvider for OpenCodeGoProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    /// An API key, when present, is the ONLY lane.
    ///
    /// Every handle becomes its own slot and every lane here is identity-less,
    /// so enumerating the key beside a cookie lane would publish two unlabelled
    /// rows that the registry's emission gate collapses to one by a tie-break
    /// nobody can see. An explicitly configured key beats an ambient browser
    /// session, the same rule `CookieVault` applies to a named deposit and the
    /// static-key providers apply to a vault key. A vault key comes before the
    /// environment's, as in `deepseek`. With no key, the cookie lanes are
    /// exactly what they were before the API lane existed.
    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        let vault_keys = self.vault_api_key_handles()?;
        if !vault_keys.is_empty() {
            return Ok(vault_keys);
        }
        if (self.env_api_key)().is_some() {
            // The implicit handle: `fetch_handle` sends it down the API lane
            // while the environment still holds a key.
            return Ok(vec![CredentialHandle::implicit()]);
        }
        self.vault.handles()
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if is_api_key_handle(handle) {
            return self.fetch_vault_api(handle).await;
        }
        if !handle.is_vault() {
            if let Some(key) = (self.env_api_key)() {
                return FetchAttempt::from_provider_usage(self.fetch_env_api(key.trim()).await);
            }
        }
        let result: Result<ProviderUsage, FetchError> = async {
            let (cookie, source) = self
                .vault
                .cookie_for(handle, load_cookie_header_async)
                .await?;
            let usage = fetch_go_usage(&self.http, &cookie, &self.endpoints).await?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, source, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_go_page, is_console_workspace_id, looks_unsubscribed, parse_api_usage,
        parse_console_go_status, parse_console_workspace_ids, redirected_off_go_page, Endpoints,
        OpenCodeGoProvider, API_KEY_FAMILY, API_USAGE_URL, CONSOLE_GO_STATUS_PATH,
        CONSOLE_WORKSPACES_PATH, CONSOLE_WORKSPACE_HEADER,
    };
    use crate::opencode::parse_windows;
    use crate::provider::CredentialHandle;
    use crate::provider::{FetchError, UsageProvider};
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// A sign-in redirect is not an expired session, and not a usable page.
    ///
    /// THE SHAPE CHANGED on 2026-09-23. This test was written 2026-09-19 around
    /// a live capture -- `/go` redirected to `https://opencode.ai/console/login`,
    /// 200, 1415 bytes, no login markers in the body -- and asserted the redirect
    /// is `Unauthorized`, because a redirect could only be the sign-in page of an
    /// expired session. OpenCode's console migration falsified that: a migrated
    /// workspace redirects on a VALID session, and the page it lands on is an
    /// empty shell. The redirect now classifies as Decode carrying the redirect
    /// verdict, and only the console API's 401 proves an expired session.
    ///
    /// Both arms, because each survives the other's mutation. Widening the check
    /// to fire on the real go page would take a healthy account dark; removing it
    /// restores the misdiagnosis that the fetch-level test
    /// `a_console_failure_and_a_legacy_redirect_publish_the_consoles_error`
    /// guards at the classification level.
    #[test]
    fn a_redirect_off_the_go_page_is_not_an_expired_session() {
        let ws = "ws_abc123";

        assert!(
            redirected_off_go_page("https://opencode.ai/console/login", ws),
            "a redirect to the console login must be recognised, and it stays on \
             the same host so only the path can say so"
        );

        assert!(
            !redirected_off_go_page(&format!("https://opencode.ai/workspace/{ws}/go"), ws),
            "the real go page must NOT be treated as a redirect, or a healthy \
             account goes dark"
        );

        assert!(
            !redirected_off_go_page("", ws),
            "an empty final URL means no transport recorded one, which every unit \
             fixture does: absence is not evidence of a redirect"
        );

        // THE PREDICATE BEING RIGHT IS HALF THE PROPERTY. The three assertions
        // above all passed while the guard was not wired into the classifier at
        // all -- proved by mutation: replacing the call with `if false` reddened
        // NOTHING. So the classifier is asserted here too, which is what a caller
        // actually reaches.
        let signed_in_page = GO_FIXTURE;
        let err = classify_go_page(
            signed_in_page,
            "https://opencode.ai/console/login",
            ws,
            1_700_000_000,
        )
        .expect_err("a redirect off the go page must not classify as a usable page");
        assert!(
            matches!(&err, FetchError::Decode(m) if m.contains("redirected off the /go page")),
            "a redirect names the moved page, not the session: got {err:?}"
        );
        assert_ne!(
            err.error_class(),
            "credential_rejected",
            "a redirect alone must never publish an expired session -- signing in \
             cannot fix a migrated workspace"
        );

        // And the body is only consulted for a page that IS the go page: this
        // same fixture at its own URL parses, so the assertion above is about the
        // redirect rather than about the fixture being unparseable.
        assert!(
            classify_go_page(
                signed_in_page,
                &format!("https://opencode.ai/workspace/{ws}/go"),
                ws,
                1_700_000_000
            )
            .is_ok(),
            "the control: this fixture is a healthy go page when it comes from \
             the go page"
        );
    }

    const GO_FIXTURE: &str = r#"
    rollingUsage: { usagePercent: 1, resetInSec: 100 },
    weeklyUsage: { usagePercent: 2, resetInSec: 200 },
    monthlyUsage: { usagePercent: 3, resetInSec: 300 }
    "#;

    #[test]
    fn go_fixture_yields_three_windows() {
        let usage = parse_windows(GO_FIXTURE, 1_000_000, true).unwrap();
        assert!(usage.primary.is_some());
        assert!(usage.secondary.is_some());
        assert!(usage.tertiary.is_some());
    }

    /// UPSTREAM FIXTURE: CodexBar v0.64.1
    /// `Tests/CodexBarTests/OpenCodeGoConsoleMigrationTests.swift` `goStatusJSON`,
    /// "matching the shape the console returns for a Go subscription". Micro-cent
    /// meters as strings; the month meter carries no reset timestamp, so
    /// `access.endsAt` (2026-10-19T00:00:00Z, epoch 1792368000) stands in for it.
    const CONSOLE_GO_STATUS: &str = concat!(
        r#"{"renewalCurrency":"usd","useBalance":false,"cancelAtPeriodEnd":false,"#,
        r#""access":{"startsAt":"2026-09-19T00:00:00.000Z","endsAt":"2026-10-19T00:00:00.000Z","#,
        r#""meters":{"fiveHour":{"startsAt":"2026-09-19T23:00:00.000Z","#,
        r#""resetsAt":"2026-09-20T03:00:00.000Z","limitMicroCents":"1200000000","#,
        r#""usedMicroCents":"300000000"},"week":{"startsAt":"2026-09-14T00:00:00.000Z","#,
        r#""resetsAt":"2026-09-21T00:00:00.000Z","limitMicroCents":"3000000000","#,
        r#""usedMicroCents":"1200000000"},"month":{"limitMicroCents":"6000000000","#,
        r#""usedMicroCents":"600000000"}}}}"#,
    );

    /// UPSTREAM FIXTURE: the same file's `consoleShellHTML` -- the empty shell
    /// every console route serves, including the login redirect target.
    const CONSOLE_SHELL: &str = concat!(
        r#"<!DOCTYPE html><html><head><title>OpenCode Console</title>"#,
        r#"<script type="module" src="/console/assets/index.js"></script></head>"#,
        r#"<body><div id="app"></div></body></html>"#,
    );

    /// UPSTREAM FIXTURE: the same file's `legacyUsagePageHTML` -- the
    /// server-rendered payload a workspace that has not migrated still returns.
    const LEGACY_GO_PAGE: &str = concat!(
        r#"<script>$R[41]={rollingUsage:$R[42]={status:"ok",resetInSec:5944,usagePercent:17},"#,
        r#"weeklyUsage:$R[43]={status:"ok",resetInSec:278201,usagePercent:75},"#,
        r#"monthlyUsage:$R[44]={status:"ok",resetInSec:880201,usagePercent:91}};</script>"#,
    );

    /// The workspace id the upstream fixtures use.
    const WORKSPACE_ID: &str = "wrk_TEST123";

    /// One scripted reply from the loopback console/legacy server.
    enum Reply {
        Body(u16, &'static str),
        Redirect(&'static str),
    }

    /// Stand up a loopback server answering every request by dispatching on its
    /// path AND full text (the workspace-header rule needs the headers),
    /// recording what was asked. Covers the console routes, the legacy
    /// `_server` function and the legacy page from one server, because the fetch
    /// under test walks across all of them.
    async fn serve(
        respond: impl Fn(&str, &str) -> Reply + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request = crate::loopback::read_request(&mut stream).await;
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                recorded.lock().unwrap().push(request.clone());
                let response = match respond(&path, &request) {
                    Reply::Body(status, body) => format!(
                        "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                    Reply::Redirect(location) => format!(
                        "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    ),
                };
                if stream.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        (format!("http://{address}"), requests)
    }

    /// The paths the loopback server saw, in order.
    fn paths(requests: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|request| request.split_whitespace().nth(1).map(str::to_string))
            .collect()
    }

    /// A vault-backed provider pointed at the loopback server, and its one
    /// handle. The credential is a deposited cookie header, so the fetch is the
    /// provider's real one from the vault lane down.
    fn loopback_provider(
        cookie: &str,
        base: &str,
    ) -> (OpenCodeGoProvider, crate::provider::CredentialHandle) {
        let mut provider = provider_with_rows(&[("cookie:opencode.ai:test", "cookie")], cookie);
        point_at(&mut provider, base);
        let handle = provider.handles().unwrap().into_iter().next().unwrap();
        (provider, handle)
    }

    /// Aim every lane of `provider` at the loopback server at `base`.
    fn point_at(provider: &mut OpenCodeGoProvider, base: &str) {
        provider.endpoints = Endpoints {
            origin: base.to_string(),
            server_base: format!("{base}/_server"),
            api_usage_url: format!("{base}{API_USAGE_PATH}"),
        };
    }

    /// A vault-backed provider whose installed snapshot holds `rows`, whose
    /// every vault read returns `payload`, and which sees NO environment key
    /// (so a key in the test runner's own environment cannot change a result).
    fn provider_with_rows(rows: &[(&str, &str)], payload: &str) -> OpenCodeGoProvider {
        struct MockCookieSource {
            cookie: String,
        }

        #[async_trait::async_trait]
        impl crate::credential_source::CredentialSource for MockCookieSource {
            async fn get(
                &self,
                _capability: &crate::credential_source::VaultCapability,
                _min_ttl_ms: u64,
            ) -> Result<
                crate::credential_source::VaultCredential,
                crate::credential_source::VaultGetError,
            > {
                Err(crate::credential_source::VaultGetError::FailClosed)
            }

            async fn get_scoped(
                &self,
                _credential_id: &str,
                _min_ttl_ms: u64,
            ) -> Result<
                crate::credential_source::VaultCredential,
                crate::credential_source::VaultGetError,
            > {
                Ok(crate::credential_source::VaultCredential {
                    payload: self.cookie.clone().into_bytes(),
                    expires_at_ms: None,
                    record_version: 1,
                    account_id: None,
                    email: None,
                    org_name: None,
                    project_id: None,
                })
            }

            async fn report_auth_failure(
                &self,
                _capability: &crate::credential_source::VaultCapability,
                _provider_status: u16,
                _record_version: u64,
            ) {
            }
        }

        let loader = Arc::new(crate::vault_handles::VaultHandleLoader::default());
        loader.install_rows_for_test(rows);
        let mut provider = OpenCodeGoProvider::new_with_handle_loader(
            Some(Arc::new(MockCookieSource {
                cookie: payload.to_string(),
            })),
            loader,
        );
        provider.env_api_key = || None;
        provider
    }

    /// Drive the provider's real fetch against the script and return the error
    /// it published.
    async fn fetch_error(
        cookie: &str,
        respond: impl Fn(&str, &str) -> Reply + Send + Sync + 'static,
    ) -> (FetchError, Arc<Mutex<Vec<String>>>) {
        let (base, requests) = serve(respond).await;
        let (provider, handle) = loopback_provider(cookie, &base);
        let attempt = provider.fetch_handle(&handle).await;
        (
            attempt.usage.expect_err("the script must fail the fetch"),
            requests,
        )
    }

    /// A migrated workspace reads its usage from the console API.
    ///
    /// Driven through `fetch_handle` rather than a helper, so the assertion is
    /// the usage the provider publishes -- this repository was burned by a test
    /// that asserted a predicate while the production call site ignored it.
    #[tokio::test]
    async fn a_migrated_workspace_reads_usage_from_the_console() {
        let (base, requests) = serve(|path, request| {
            if path == CONSOLE_WORKSPACES_PATH {
                return Reply::Body(200, r#"[{"id":"wrk_TEST123","name":"Default"}]"#);
            }
            if path == CONSOLE_GO_STATUS_PATH {
                // The console answers HTTP 400 when the workspace header is
                // missing, so the loopback refuses too: a fetch that never
                // sent it must fail here rather than pass by the server's
                // generosity.
                if !request
                    .to_ascii_lowercase()
                    .contains(&format!("{CONSOLE_WORKSPACE_HEADER}: "))
                {
                    return Reply::Body(400, r#"{"_tag":"BadRequest"}"#);
                }
                return Reply::Body(200, CONSOLE_GO_STATUS);
            }
            // A migrated workspace serves the empty console shell on every
            // legacy route.
            Reply::Body(200, CONSOLE_SHELL)
        })
        .await;
        let (provider, handle) =
            loopback_provider("auth=test; __Host-console_session=synthetic", &base);

        let attempt = provider.fetch_handle(&handle).await;
        let usage = attempt
            .usage
            .expect("the console lane must serve the fetch");

        let primary = usage.primary.expect("the five-hour meter is the primary");
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.window_minutes, Some(300));
        let secondary = usage.secondary.expect("the week meter is the secondary");
        assert_eq!(secondary.used_percent, 40.0);
        assert_eq!(secondary.window_minutes, Some(10080));
        let tertiary = usage.tertiary.expect("the month meter is the tertiary");
        assert_eq!(tertiary.used_percent, 10.0);
        assert_eq!(tertiary.window_minutes, Some(43200));
        // The month meter carries no reset, so the billing period end stands in.
        assert_eq!(
            tertiary.resets_at,
            crate::env::epoch_to_iso8601(1_792_368_000)
        );

        let seen = paths(&requests);
        assert_eq!(
            seen,
            vec![
                CONSOLE_WORKSPACES_PATH.to_string(),
                CONSOLE_GO_STATUS_PATH.to_string()
            ],
            "a healthy console lane never touches a legacy route"
        );
        // The console answers HTTP 400 without this header, so the loopback
        // must have seen it: an assertion on the outcome alone would also pass
        // if the header were never sent and the status route answered anyway.
        let status_request = requests
            .lock()
            .unwrap()
            .iter()
            .find(|request| request.starts_with(&format!("GET {CONSOLE_GO_STATUS_PATH} ")))
            .expect("the go status was requested")
            .to_ascii_lowercase();
        let expected_header =
            format!("{CONSOLE_WORKSPACE_HEADER}: {WORKSPACE_ID}").to_ascii_lowercase();
        assert!(
            status_request.contains(&expected_header),
            "the status request must name the workspace: {status_request}"
        );
    }

    /// A console 401 is the one verdict that proves an expired session.
    ///
    /// With only the console cookie in the jar there is no legacy lane to try,
    /// so the 401 publishes directly -- asserted on the top-level class, which
    /// is what the stale-login metric counts.
    #[tokio::test]
    async fn a_console_401_is_the_expired_session_verdict() {
        let (error, requests) = fetch_error("__Host-console_session=expired", |_path, _request| {
            Reply::Body(401, r#"{"message":"Unauthorized"}"#)
        })
        .await;

        assert!(
            matches!(&error, FetchError::Unauthorized(_)),
            "expected the expired-session verdict, got: {error}"
        );
        assert_eq!(error.error_class(), "credential_rejected");
        assert_eq!(
            paths(&requests),
            vec![CONSOLE_WORKSPACES_PATH.to_string()],
            "a console-only session must not attempt legacy authentication"
        );
    }

    /// A console `null`, or `access: null`, is the console stating there is no
    /// Go plan -- not a failure, and not a reason to try the legacy page.
    #[tokio::test]
    async fn a_console_null_means_no_go_plan() {
        for body in ["null", r#"{"renewalCurrency":"usd","access":null}"#] {
            let (base, requests) = serve(move |path, _request| {
                if path == CONSOLE_WORKSPACES_PATH {
                    return Reply::Body(200, r#"[{"id":"wrk_TEST123"}]"#);
                }
                if path == CONSOLE_GO_STATUS_PATH {
                    return Reply::Body(200, body);
                }
                Reply::Body(200, LEGACY_GO_PAGE)
            })
            .await;
            // The legacy cookie rides along to prove the fallback is not taken:
            // the legacy page WOULD parse, so a wrong fallback reads as success.
            let (provider, handle) =
                loopback_provider("auth=test; __Host-console_session=synthetic", &base);
            let attempt = provider.fetch_handle(&handle).await;
            let error = attempt.usage.expect_err("no plan is not usage");
            assert!(
                matches!(&error, FetchError::NoQuotaReported(_)),
                "a console null must be the no-plan verdict, got: {error}"
            );
            assert_eq!(error.error_class(), "no_quota_reported");
            assert!(
                paths(&requests)
                    .iter()
                    .all(|p| !p.starts_with("/workspace/") && !p.starts_with("/_server")),
                "no legacy request may follow the console's no-plan verdict: {:?}",
                paths(&requests)
            );
        }
    }

    /// A console payload without the required meters is our decode failure,
    /// neither an expired session nor a no-plan verdict.
    #[tokio::test]
    async fn a_console_payload_without_meters_is_a_decode_failure() {
        let (error, _) = fetch_error("__Host-console_session=synthetic", |path, _request| {
            if path == CONSOLE_WORKSPACES_PATH {
                return Reply::Body(200, r#"[{"id":"wrk_TEST123"}]"#);
            }
            Reply::Body(200, r#"{"access":{}}"#)
        })
        .await;

        assert!(
            matches!(&error, FetchError::Decode(_)),
            "expected the decode failure, got: {error}"
        );
        assert_eq!(error.error_class(), "decode_failed");
    }

    /// The failure pairing this provider was rewritten for: the console fails
    /// with a non-401 error AND the legacy page redirects to the console login.
    /// Before the console lane existed, the redirect alone produced
    /// `credential_rejected` and told the operator to sign in again, which cannot
    /// help: the fault is that the old endpoint no longer serves a migrated
    /// workspace. What publishes now is the console's own error.
    ///
    /// The jar deliberately carries ONLY the legacy cookie, so the console's
    /// error cannot win by the generic both-lanes-failed rule -- it wins because
    /// a redirect off the `/go` page names a moved page, not a session.
    #[tokio::test]
    async fn a_console_failure_and_a_legacy_redirect_publish_the_consoles_error() {
        let (error, _) = fetch_error("auth=legacy", |path, _request| {
            if path.starts_with("/console/api/") {
                return Reply::Body(500, r#"{"error":"console unavailable"}"#);
            }
            if path.starts_with("/_server") {
                return Reply::Body(200, r#"{"data":[{"id":"wrk_TEST123"}]}"#);
            }
            if path == format!("/workspace/{WORKSPACE_ID}/go") {
                return Reply::Redirect("/console/login");
            }
            // The login route serves the empty console shell.
            Reply::Body(200, CONSOLE_SHELL)
        })
        .await;

        assert_eq!(
            error.error_class(),
            "upstream_failed",
            "the console's error must publish, got: {error}"
        );
        assert!(
            error.to_string().contains("console go status"),
            "the published error is the console's, not the legacy page's: {error}"
        );
        assert_ne!(
            error.error_class(),
            "credential_rejected",
            "the redirect must not turn this back into an expired session"
        );
    }

    /// A console 401 outranks a legacy redirect.
    ///
    /// The common expired-session shape: a signed-out user whose jar still
    /// carries the old `auth` cookie, on a migrated workspace. The console has
    /// said authoritatively that the session is expired; the legacy page's
    /// redirect to the console login says only that the page moved. Publishing
    /// the redirect's Decode would send a reader hunting a parser bug while
    /// hiding the one fact they need -- the mirror image of the misreading this
    /// provider's console lane was added to fix.
    #[tokio::test]
    async fn a_console_401_outranks_a_legacy_redirect() {
        let (error, _) = fetch_error(
            "auth=legacy; __Host-console_session=expired",
            |path, _request| {
                if path == CONSOLE_WORKSPACES_PATH {
                    return Reply::Body(200, r#"[{"id":"wrk_TEST123"}]"#);
                }
                if path == CONSOLE_GO_STATUS_PATH {
                    return Reply::Body(401, r#"{"message":"Unauthorized"}"#);
                }
                if path == format!("/workspace/{WORKSPACE_ID}/go") {
                    return Reply::Redirect("/console/login");
                }
                Reply::Body(200, CONSOLE_SHELL)
            },
        )
        .await;

        assert!(
            matches!(&error, FetchError::Unauthorized(_)),
            "the console's 401 is the expired-session verdict, got: {error}"
        );
        assert_eq!(error.error_class(), "credential_rejected");
        assert!(
            error.to_string().contains("console go status"),
            "the published error is the console's, not the legacy page's: {error}"
        );
    }

    /// A workspace that has not migrated keeps working: the console does not
    /// know it, and the legacy page still parses.
    #[tokio::test]
    async fn an_unmigrated_workspace_falls_back_to_the_legacy_page() {
        for console_status in [401_u16, 404] {
            let (base, requests) = serve(move |path, _request| {
                if path.starts_with("/console/api/") {
                    // Workspaces that have not migrated are unknown to the
                    // console API, and the console's rejection says nothing
                    // about the legacy session.
                    return Reply::Body(console_status, r#"{"_tag":"NotFound"}"#);
                }
                if path.starts_with("/_server") {
                    return Reply::Body(200, r#"{"data":[{"id":"wrk_TEST123"}]}"#);
                }
                Reply::Body(200, LEGACY_GO_PAGE)
            })
            .await;
            let (provider, handle) = loopback_provider("auth=test", &base);
            let attempt = provider.fetch_handle(&handle).await;
            let usage = attempt
                .usage
                .expect("the legacy page must serve the fetch when the console cannot");

            assert_eq!(usage.primary.unwrap().used_percent, 17.0);
            assert_eq!(usage.secondary.unwrap().used_percent, 75.0);
            assert_eq!(usage.tertiary.unwrap().used_percent, 91.0);
            assert!(
                paths(&requests)
                    .iter()
                    .any(|p| p == &format!("/workspace/{WORKSPACE_ID}/go")),
                "the legacy page must actually have been read"
            );
        }
    }

    /// Both lanes rejecting the session is the real expired-session verdict.
    ///
    /// The console's 401 is its own session's verdict, so the legacy lane still
    /// answers for itself; when it also rejects, the legacy error publishes.
    #[tokio::test]
    async fn rejection_by_both_lanes_is_an_expired_session() {
        let (error, _) = fetch_error(
            "auth=legacy; __Host-console_session=expired",
            |_path, _request| Reply::Body(401, r#"{"message":"Unauthorized"}"#),
        )
        .await;

        assert!(
            matches!(&error, FetchError::Unauthorized(_)),
            "expected the expired-session verdict, got: {error}"
        );
        assert_eq!(error.error_class(), "credential_rejected");
    }

    /// A failed legacy read cannot turn a console access failure into invalid
    /// auth.
    ///
    /// The legacy page reports the session signed out while the console -- the
    /// workspace's real home -- is simply unreachable. With the console session
    /// cookie in the jar, the console's error publishes rather than the legacy
    /// lane's auth verdict (CodexBar v0.64.1 `OpenCodeGoLegacyFallback`).
    #[tokio::test]
    async fn a_console_outage_is_not_relabelled_by_a_signed_out_legacy_page() {
        let (error, _) = fetch_error(
            "auth=legacy; __Host-console_session=synthetic",
            |path, _request| {
                if path.starts_with("/console/api/") {
                    return Reply::Body(500, r#"{"error":"console unavailable"}"#);
                }
                if path.starts_with("/_server") {
                    return Reply::Body(200, r#"{"data":[{"id":"wrk_TEST123"}]}"#);
                }
                // The legacy session really is dead: the page itself says so.
                Reply::Body(
                    200,
                    r#"{"error":"Please sign in to continue","login":true}"#,
                )
            },
        )
        .await;

        assert_eq!(
            error.error_class(),
            "upstream_failed",
            "the console's error must publish over the legacy auth verdict, got: {error}"
        );
    }

    /// Console workspace ids keep their shape: `wrk_` and `org_` rows are
    /// workspaces, anything else is not.
    #[test]
    fn console_workspace_ids_keep_their_shape() {
        assert_eq!(
            parse_console_workspace_ids(
                r#"[{"id":"wrk_TEST123","name":"Default"},{"id":"org_TEST456"}]"#
            ),
            vec!["wrk_TEST123".to_string(), "org_TEST456".to_string()]
        );
        assert!(parse_console_workspace_ids(r#"{"error":"nope"}"#).is_empty());
        assert!(parse_console_workspace_ids(r#"[{"id":"acc_TEST"}]"#).is_empty());
        assert!(!is_console_workspace_id("wrk_"));
        assert!(!is_console_workspace_id("wrk two"));
    }

    /// The console payload parses into the same windows the legacy scrape
    /// publishes -- the pure-parser half of the happy path, including the month
    /// reset standing in from the billing period end.
    #[test]
    fn console_micro_cent_meters_become_the_three_windows() {
        let usage = parse_console_go_status(CONSOLE_GO_STATUS, 1_789_862_400)
            .expect("the console status must parse");
        assert_eq!(usage.primary.unwrap().used_percent, 25.0);
        assert_eq!(usage.secondary.unwrap().used_percent, 40.0);
        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 10.0);
        assert_eq!(
            tertiary.resets_at,
            crate::env::epoch_to_iso8601(1_792_368_000)
        );

        // A missing month meter keeps weekly reporting rather than failing.
        let without_month = CONSOLE_GO_STATUS.replace(
            r#","month":{"limitMicroCents":"6000000000","usedMicroCents":"600000000"}"#,
            "",
        );
        let usage = parse_console_go_status(&without_month, 1_789_862_400)
            .expect("a status without a month meter still parses");
        assert!(usage.tertiary.is_none());
        assert_eq!(usage.secondary.unwrap().used_percent, 40.0);
    }

    /// The account record as the live page renders it for a workspace with no Go
    /// plan. Captured from a real response and trimmed to the surrounding
    /// fields, because the neighbours are the point: a populated payment method
    /// beside the nulls shows the record rendered correctly and the plan is
    /// genuinely absent, rather than the page having failed.
    const LIVE_UNSUBSCRIBED_RECORD: &str = concat!(
        r#"paymentMethodType:"card",paymentMethodLast4:"4232",balance:0,"#,
        "reload:null,reloadAmount:20,monthlyLimit:null,monthlyUsage:null,",
        "timeMonthlyUsageUpdated:null,reloadError:null,subscription:null,",
        "subscriptionID:null,subscriptionPlan:null,timeSubscriptionBooked:null"
    );

    /// A workspace with no Go plan is reported as having no quota to report.
    ///
    /// Asserted through the classifier rather than the predicate, because the
    /// predicate being right is not the claim that matters -- the claim is that
    /// a page like this produces this error. Classified as a decode failure
    /// instead, it reads as a stale browser login and sends an operator to sign
    /// in again, which changes nothing.
    #[test]
    fn a_workspace_without_a_go_plan_reports_no_quota() {
        let err = classify_go_page(
            LIVE_UNSUBSCRIBED_RECORD,
            "https://opencode.ai/workspace/ws_test/go",
            "ws_test",
            1_000_000,
        )
        .unwrap_err();
        assert!(
            matches!(&err, FetchError::NoQuotaReported(m) if m.contains("no Go subscription")),
            "expected the no-plan verdict, got: {err}"
        );
    }

    /// A page that should parse is not diverted by the no-plan check.
    ///
    /// Without this the no-plan verdict could be returned for every page and the
    /// test above would still pass, which would silently replace every real
    /// window with a no-quota report.
    #[test]
    fn a_page_with_windows_is_accepted() {
        assert!(classify_go_page(
            GO_FIXTURE,
            "https://opencode.ai/workspace/ws_test/go",
            "ws_test",
            1_000_000
        )
        .is_ok());
    }

    /// A page with neither windows nor a no-plan record is our defect to fix.
    #[test]
    fn a_page_with_no_windows_and_no_verdict_is_a_decode_failure() {
        let err = classify_go_page(
            "<html>something else entirely</html>",
            "https://opencode.ai/workspace/ws_test/go",
            "ws_test",
            1_000_000,
        )
        .unwrap_err();
        assert!(
            matches!(&err, FetchError::Decode(m) if m.contains("usage fields missing")),
            "expected the decode failure, got: {err}"
        );
    }

    /// A subscribed workspace is not mistaken for an unsubscribed one.
    ///
    /// This is the direction that costs real data: a false positive here
    /// suppresses a live subscription's windows and reports the account as
    /// having no plan, which looks calm and is wrong.
    #[test]
    fn a_subscribed_workspace_is_not_treated_as_unsubscribed() {
        let subscribed = LIVE_UNSUBSCRIBED_RECORD
            .replace("subscription:null", r#"subscription:"active""#)
            .replace("subscriptionID:null", r#"subscriptionID:"sub_01ABC""#)
            .replace("subscriptionPlan:null", r#"subscriptionPlan:"go""#);
        assert!(!looks_unsubscribed(&subscribed));
    }

    /// One populated field is enough to withhold the conclusion.
    ///
    /// A single null is as consistent with a field being rolled out, or a lapsed
    /// payment method on a live plan, as it is with no subscription at all. The
    /// three together are what make the record unambiguous, so each is required
    /// and this proves none of them is decorative.
    #[test]
    fn a_single_populated_subscription_field_withholds_the_conclusion() {
        for field in ["subscription", "subscriptionID", "subscriptionPlan"] {
            let populated = LIVE_UNSUBSCRIBED_RECORD
                .replace(&format!("{field}:null"), &format!(r#"{field}:"x""#));
            assert!(
                !looks_unsubscribed(&populated),
                "concluded there is no plan while {field} was populated"
            );
        }
    }

    // -----------------------------------------------------------------------
    // API-key lane
    // -----------------------------------------------------------------------

    /// The path of the usage endpoint, which the loopback server is asked on.
    const API_USAGE_PATH: &str = "/zen/go/v1/usage";

    /// Upstream's reset timestamps for the three windows
    /// (`OpenCodeGoUsageFetcherErrorTests`, "public usage API sends bearer token
    /// and preserves percent units").
    const ROLLING_RESET: &str = "2026-08-12T02:00:00.000Z";
    const WEEKLY_RESET: &str = "2026-08-18T00:00:00.000Z";
    const MONTHLY_RESET: &str = "2026-09-01T00:00:00.000Z";
    /// That test's `now`.
    const UPSTREAM_NOW: i64 = 1_786_493_600;

    /// The payload that upstream test builds: `{"usage": {window: {"percent",
    /// "resetsAt"}}}`, weekly and monthly present only when given.
    fn api_payload(rolling: f64, weekly: Option<f64>, monthly: Option<f64>) -> String {
        let mut windows = serde_json::Map::new();
        windows.insert(
            "rolling".into(),
            serde_json::json!({ "percent": rolling, "resetsAt": ROLLING_RESET }),
        );
        if let Some(weekly) = weekly {
            windows.insert(
                "weekly".into(),
                serde_json::json!({ "percent": weekly, "resetsAt": WEEKLY_RESET }),
            );
        }
        if let Some(monthly) = monthly {
            windows.insert(
                "monthly".into(),
                serde_json::json!({ "percent": monthly, "resetsAt": MONTHLY_RESET }),
            );
        }
        serde_json::json!({ "usage": windows }).to_string()
    }

    fn iso(rfc3339: &str) -> Option<String> {
        crate::env::epoch_to_iso8601(
            chrono::DateTime::parse_from_rfc3339(rfc3339)
                .unwrap()
                .timestamp(),
        )
    }

    #[test]
    fn the_api_usage_url_is_the_upstream_endpoint() {
        assert_eq!(
            API_USAGE_URL,
            format!("https://opencode.ai{API_USAGE_PATH}")
        );
        assert_eq!(API_KEY_FAMILY, "apikey:opencode");
    }

    /// Rolling only: one window, and the API's percent is already a percent.
    ///
    /// Upstream case `(1, nil, nil)`. A `1` read as a 0..1 fraction would
    /// publish 100% -- the dashboard payload's rule, which this lane must not
    /// inherit.
    #[test]
    fn api_usage_with_only_a_rolling_window_publishes_only_the_primary() {
        let usage = parse_api_usage(&api_payload(1.0, None, None), UPSTREAM_NOW)
            .expect("a rolling-only payload parses");
        let primary = usage.primary.expect("rolling is the primary");
        assert_eq!(primary.used_percent, 1.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, iso(ROLLING_RESET));
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());
    }

    /// Rolling + weekly + monthly, with upstream's percent-unit cases: every
    /// value, including the sub-1 ones, is published as sent.
    #[test]
    fn api_usage_with_all_three_windows_preserves_percent_units() {
        for (rolling, weekly, monthly) in [
            (12.0, 8.0, 35.0),
            (3.0, 1.0, 0.0),
            (1.0, 1.0, 1.0),
            (0.0, 0.0, 0.0),
            (100.0, 100.0, 100.0),
            (0.5, 0.5, 0.5),
        ] {
            let usage = parse_api_usage(
                &api_payload(rolling, Some(weekly), Some(monthly)),
                UPSTREAM_NOW,
            )
            .expect("a three-window payload parses");
            let primary = usage.primary.unwrap();
            let secondary = usage.secondary.expect("weekly is the secondary");
            let tertiary = usage.tertiary.expect("monthly is the tertiary");
            assert_eq!(
                (
                    primary.used_percent,
                    secondary.used_percent,
                    tertiary.used_percent
                ),
                (rolling, weekly, monthly),
                "percents must be published in the units the API sends"
            );
            assert_eq!(secondary.window_minutes, Some(10_080));
            assert_eq!(tertiary.window_minutes, Some(43_200));
            assert_eq!(secondary.resets_at, iso(WEEKLY_RESET));
            assert_eq!(tertiary.resets_at, iso(MONTHLY_RESET));
        }
    }

    /// The seconds-until-reset shape, from upstream's
    /// `OpenCodeGoWebOverlayTests` ("local strategy prefers API windows").
    #[test]
    fn api_usage_reads_reset_in_seconds() {
        let payload = r#"{"usage": {
          "rolling": {"percent": 3, "resetInSec": 18100},
          "weekly": {"percent": 1, "resetInSec": 266500},
          "monthly": {"percent": 0, "resetInSec": 1539100}
        }}"#;
        let usage = parse_api_usage(payload, UPSTREAM_NOW).expect("the payload parses");
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 3.0);
        assert_eq!(
            primary.resets_at,
            crate::env::epoch_to_iso8601(UPSTREAM_NOW + 18_100)
        );
        assert_eq!(usage.secondary.unwrap().used_percent, 1.0);
        assert_eq!(usage.tertiary.unwrap().used_percent, 0.0);
    }

    /// A success with no usage fields is our decode failure.
    ///
    /// Includes the null shapes: upstream's API lane has no "no subscription"
    /// verdict and no captured response shows one, so `null` is not read as
    /// "no plan" -- publishing that on a guessed shape would stop consumers
    /// routing here for an account that may well have a plan.
    #[test]
    fn api_usage_without_usage_fields_is_a_decode_failure() {
        for body in [
            r#"{"renewAt":"2026-09-01T00:00:00.000Z"}"#,
            r#"{"usage":{}}"#,
            r#"{"usage":{"weekly":{"percent":1}}}"#,
            r#"{"usage":null}"#,
            "null",
            "not json",
        ] {
            let error = parse_api_usage(body, UPSTREAM_NOW).expect_err(body);
            assert_eq!(error.error_class(), "decode_failed", "{body}: {error}");
        }
        // A present-but-unreadable weekly window fails the whole payload, as
        // upstream's `buildSnapshot` does.
        let error = parse_api_usage(
            r#"{"usage":{"rolling":{"percent":1},"weekly":{}}}"#,
            UPSTREAM_NOW,
        )
        .expect_err("an unreadable weekly window");
        assert_eq!(error.error_class(), "decode_failed");
    }

    fn ids(handles: Vec<CredentialHandle>) -> Vec<String> {
        handles
            .iter()
            .map(|handle| handle.stable_id().to_string())
            .collect()
    }

    /// An API key is the only lane; without one, the cookie lanes are exactly
    /// what they were.
    ///
    /// A named cookie deposit is present in the key cases because it is the
    /// cookie lane that would otherwise enumerate as a vault handle of its own:
    /// kept beside the key, it would be a second identity-less slot for this
    /// provider.
    #[test]
    fn an_api_key_replaces_the_cookie_lanes() {
        let cookie = ("cookie:opencode.ai:acct", "cookie");
        let key = ("apikey:opencode", "apikey");

        // A vault key: only the key's handle.
        let provider = provider_with_rows(&[cookie, key], "");
        assert_eq!(ids(provider.handles().unwrap()), vec!["apikey:opencode"]);

        // An environment key: only the implicit handle, which fetches with it.
        let mut provider = provider_with_rows(&[cookie], "");
        provider.env_api_key = || Some("go_secret".to_string());
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );

        // No key: the named cookie deposit, as before.
        let provider = provider_with_rows(&[cookie], "");
        assert_eq!(
            ids(provider.handles().unwrap()),
            vec!["cookie:opencode.ai:acct"]
        );
        // No key and only a bare cookie deposit: the local lane, as before.
        let provider = provider_with_rows(&[("cookie:opencode.ai", "cookie")], "");
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    /// An env-key fetch sends the bearer key to the usage path and publishes
    /// the windows -- the full route, not just the parser.
    #[tokio::test]
    async fn an_env_key_fetch_reads_the_usage_api() {
        let body: &'static str =
            Box::leak(api_payload(12.0, Some(8.0), Some(35.0)).into_boxed_str());
        let (base, requests) = serve(move |_path, _request| Reply::Body(200, body)).await;
        let mut provider = provider_with_rows(&[("cookie:opencode.ai:acct", "cookie")], "");
        provider.env_api_key = || Some("go_secret".to_string());
        point_at(&mut provider, &base);

        let handle = provider.handles().unwrap().remove(0);
        let attempt = provider.fetch_handle(&handle).await;
        let usage = attempt.usage.expect("the API lane serves the fetch");
        assert_eq!(usage.primary.unwrap().used_percent, 12.0);
        assert_eq!(usage.secondary.unwrap().used_percent, 8.0);
        assert_eq!(usage.tertiary.unwrap().used_percent, 35.0);

        assert_eq!(paths(&requests), vec![API_USAGE_PATH.to_string()]);
        let request = requests.lock().unwrap()[0].to_ascii_lowercase();
        assert!(
            request.contains("authorization: bearer go_secret"),
            "the key must travel as a bearer token: {request}"
        );
    }

    /// A 401 from the usage API is the rejected-key class, for both the vault
    /// key and the environment key -- never a decode failure or an outage.
    #[tokio::test]
    async fn a_401_from_the_usage_api_is_a_rejected_key() {
        let (base, requests) =
            serve(|_path, _request| Reply::Body(401, r#"{"error":"unauthorized"}"#)).await;

        let mut vault = provider_with_rows(&[("apikey:opencode", "apikey")], "bad");
        point_at(&mut vault, &base);
        let handle = vault.handles().unwrap().remove(0);
        let error = vault
            .fetch_handle(&handle)
            .await
            .usage
            .expect_err("a rejected key is not usage");
        assert_eq!(error.error_class(), "credential_rejected", "{error}");

        let mut env = provider_with_rows(&[], "");
        env.env_api_key = || Some("bad".to_string());
        point_at(&mut env, &base);
        let handle = env.handles().unwrap().remove(0);
        let error = env
            .fetch_handle(&handle)
            .await
            .usage
            .expect_err("a rejected key is not usage");
        assert_eq!(error.error_class(), "credential_rejected", "{error}");

        assert_eq!(
            paths(&requests),
            vec![API_USAGE_PATH.to_string(), API_USAGE_PATH.to_string()],
            "both fetches must have asked the usage API and nothing else"
        );
    }
}
