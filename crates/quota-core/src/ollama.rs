//! Ollama usage — browser-cookie scrape of ollama.com/settings.
//!
//! Ollama has NO headless usage/quota API: its API key (OLLAMA_API_KEY) only
//! VERIFIES Cloud access (GET /api/tags returns a model list, zero quota) — quota
//! lives only on the authenticated settings page. CodexBar reads it by pulling the
//! session cookie from the browser and scraping the HTML; we replicate that via the
//! shared [`browser_cookies`] layer.
//!
//! Flow: pull ollama.com cookies from Chrome (decrypted) → GET
//! `https://ollama.com/settings` with the `Cookie:` header → parse the "Session
//! usage" + "Weekly usage" blocks (`N% used` + a `data-time="<ISO>"` reset).
//!
//! DESKTOP-COUPLED + BRITTLE (accepted): needs a local Chrome login + OS keychain,
//! and the session cookie rotates (no headless refresh), so it degrades to
//! unavailable when the cookie is dead/expired or the login page is served. The one
//! hard rule: degrade NEVER means a wrong/stale number — a dead cookie, a
//! login-redirect, or missing usage markers yield [`FetchError`] (a degraded entry),
//! never a fabricated window.
//!
//! VERIFICATION: LIVE-verified — the real cookie→GET→parse chain was proven on a
//! machine with a logged-in Chrome session (returns real Session/Weekly windows;
//! see `tests/ollama_live.rs`). The HTML parse is also unit-tested against a
//! captured real settings fixture. Decryption recipe + HTML field names ported from
//! CodexBar `Sources/CodexBarCore/Providers/Ollama/OllamaUsageFetcher.swift` +
//! `OllamaUsageParser.swift:28-131` (labels, `N% used` / `width:N%`, `data-time`).

use std::time::Duration;

use async_trait::async_trait;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "ollama";
/// The bare vault credential id for this domain; a suffixed deposit under it
/// names an account and takes the provider vault-only.
const COOKIE_FAMILY: &str = "cookie:ollama.com";

const DOMAIN: &str = "ollama.com";
const SETTINGS_URL: &str = "https://ollama.com/settings";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

const SESSION_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;
/// The longest cadence this page publishes, and the bound that replaced a byte cap.
///
/// The block used to stop after 4000 bytes when no further label followed. That
/// number is a proxy for "still inside this window's section", and it is a BAD
/// proxy because the section's length is DATA-DEPENDENT: the weekly block carries
/// a per-model breakdown, so it grows with how many models the operator has used.
/// Measured on this host 2026-09-20 -- the weekly `data-time` sat 4207 bytes after
/// its label, 207 past the cap, so the weekly window published a real percent with
/// no reset while the timestamp was on the page the whole time. Upstream carries
/// the same 4000 at v0.62.0, so this is a deliberate divergence.
///
/// A byte distance cannot be made right by choosing a bigger number: the next
/// operator with more models blows past whatever is chosen. The bound that does not
/// rot is the window's OWN cadence -- a reset for a 7-day window is at most 7 days
/// out, whatever the page's byte layout. That is what guards the last label now
/// that its block runs to the end of the page.
/// Sized to the LONGEST cadence this page can publish, which is the monthly block
/// upstream added at v0.56.x -- not the weekly one. I set this to a week first and
/// the suite rejected a legitimate monthly reset 11 days out, which is the guard
/// working: a fallback horizon has to cover every window that can reach it, and
/// the monthly block states no cadence of its own so it lands here.
///
/// Only windows with NO stated duration use this. A weekly window is bounded by
/// its own 7 days, which is tighter and more honest than any shared number.
const MAX_RESET_HORIZON_MINUTES: i64 = 31 * 24 * 60;

/// A recognized session-cookie name (any of these → treat the jar as a real login).
/// `wos-session` is Ollama's WorkOS session cookie, adopted when it moved auth to
/// WorkOS — a jar carrying only it is still a valid login.
fn is_session_cookie(name: &str) -> bool {
    matches!(
        name,
        "session" | "__Secure-session" | "ollama_session" | "__Host-ollama_session" | "wos-session"
    ) || name.starts_with("__Secure-next-auth.session-token")
        || name.starts_with("next-auth.session-token")
}

// ---- HTML parsing (pure) ----------------------------------------------------

/// All usage-block labels, used to bound one block's window at the next block.
const ALL_LABELS: &[&str] = &["Session usage", "Hourly usage", "Weekly usage"];

/// Slice from just after `label` to the next other-label, or to the end of the
/// page when no other label follows.
///
/// NO BYTE CAP. There was one, and it silently dropped a real reset -- see
/// `MAX_RESET_HORIZON_MINUTES` for the measurement. The next label is a true
/// boundary; a byte distance only approximates one, and the approximation fails
/// exactly when a section grows.
///
/// `floor_char_boundary` is retained though both bounds are now always on a
/// character boundary (a label match and the string end both are). It costs
/// nothing and it is the guard that would matter if a byte bound ever came back --
/// slicing mid-character panics, and a panicking fetch is classified non-transient,
/// so a working provider would read as absent rather than degraded.
fn block_after<'a>(html: &'a str, label: &str) -> Option<&'a str> {
    let start = html.find(label)? + label.len();
    let tail = &html[start..];
    let end = ALL_LABELS
        .iter()
        .filter(|l| **l != label)
        .filter_map(|l| tail.find(l))
        .min()
        .unwrap_or(tail.len());
    Some(&tail[..crate::text::floor_char_boundary(tail, end)])
}

/// Parse `N% used` (preferred) else `width: N%` from a block. Hand-scanned to avoid
/// a regex dependency: find the `% used` marker (whitespace-tolerant) and read the
/// number immediately before the `%`.
fn parse_percent(block: &str) -> Option<f64> {
    if let Some(p) = percent_before_marker(block, "used") {
        return Some(p);
    }
    parse_width_percent(block)
}

/// Find a `<number> % <marker>` occurrence (whitespace allowed around `%`) and
/// return the number.
fn percent_before_marker(block: &str, marker: &str) -> Option<f64> {
    let bytes = block.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = block[search_from..].find(marker) {
        let marker_at = search_from + rel;
        // Walk back over whitespace before the marker.
        let mut i = marker_at;
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        // Expect a '%' here.
        if i > 0 && bytes[i - 1] == b'%' {
            let percent_at = i - 1;
            // Walk back over whitespace before '%'.
            let mut j = percent_at;
            while j > 0 && bytes[j - 1].is_ascii_whitespace() {
                j -= 1;
            }
            // Collect the number (digits + one dot) ending at j.
            let num_end = j;
            while j > 0 && (bytes[j - 1].is_ascii_digit() || bytes[j - 1] == b'.') {
                j -= 1;
            }
            if j < num_end {
                if let Ok(value) = block[j..num_end].parse::<f64>() {
                    return Some(value.clamp(0.0, 100.0));
                }
            }
        }
        search_from = marker_at + marker.len();
    }
    None
}

/// Fallback: the bar's `width: N%` inline style.
fn parse_width_percent(block: &str) -> Option<f64> {
    let key = "width:";
    let at = block.find(key)? + key.len();
    let rest = block[at..].trim_start();
    let num_end = rest.find('%')?;
    rest[..num_end]
        .trim()
        .parse::<f64>()
        .ok()
        .map(|v| v.clamp(0.0, 100.0))
}

/// Parse the first `data-time="<value>"` reset timestamp in a block.
fn parse_reset(block: &str) -> Option<String> {
    let key = "data-time=\"";
    let at = block.find(key)? + key.len();
    let end = block[at..].find('"')?;
    let value = block[at..at + end].trim();
    // Sanity: an ISO8601-ish instant. Pass through (already `...Z`); never invent.
    if value.contains('T') && value.len() >= 16 {
        Some(value.to_string())
    } else {
        None
    }
}

/// The host part of a URL, for reporting where a redirect landed.
///
/// String-sliced rather than parsed because the only consumer is an error
/// message: a malformed URL yields `None` and the caller says "another host",
/// which is still true and still actionable.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let host = after_scheme.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// Whether the settings request was answered by somewhere else entirely.
///
/// An empty final URL is NOT a redirect. It means no transport recorded one --
/// every test fixture constructs a response that way -- and treating absence as
/// evidence would report every unit test's page as a sign-in redirect.
fn redirected_off_settings(final_url: &str) -> bool {
    match (host_of(final_url), host_of(SETTINGS_URL)) {
        (Some(actual), Some(expected)) => actual != expected,
        _ => false,
    }
}

/// Heuristic: the settings page was replaced by a sign-in page (dead cookie).
fn looks_signed_out(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let has_heading = lower.contains("sign in to ollama") || lower.contains("log in to ollama");
    let has_auth_route = lower.contains("/api/auth/signin") || lower.contains("/auth/signin");
    let has_form = lower.contains("<form");
    let has_password = lower.contains("type=\"password\"") || lower.contains("name=\"password\"");
    has_form && ((has_heading && has_password) || has_auth_route)
}

/// A reset from this block, kept only when it could belong to THIS window.
///
/// The positional bound that used to do this job was a byte cap, and removing it
/// lets the last label's block run to the end of the page -- which is what makes
/// the real reset reachable, and also what would let an unrelated timestamp
/// further down the page be attributed here. So the guard moved from WHERE the
/// timestamp sits to WHETHER IT COULD BE THIS WINDOW'S.
///
/// A window of duration D resets at most D from now, by definition. A page-footer
/// renewal date months out fails that for every window here; the genuine weekly
/// reset (measured 11 hours out against a 7-day window) passes comfortably.
///
/// TWO DELIBERATE CHOICES IN THE BOUNDS:
///
/// A reset already in the PAST is kept, not dropped. It means the window has
/// rolled and the page has not caught up, which is a real state with a real
/// percent beside it -- dropping the timestamp there would turn a stale reading
/// into a resetless one and hide that the page is behind.
///
/// A window with no stated duration (an "Hourly usage" block, which carries no
/// fixed length) is bounded by the longest cadence the page publishes rather than
/// waved through. Without a duration there is nothing tighter to say, and an
/// unbounded last block is the case this guard exists for.
fn plausible_reset(
    block: &str,
    window_minutes: Option<i64>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let raw = parse_reset(block)?;
    let parsed = chrono::DateTime::parse_from_rfc3339(&raw).ok()?;
    let horizon = window_minutes.unwrap_or(MAX_RESET_HORIZON_MINUTES);
    let ahead = parsed
        .with_timezone(&chrono::Utc)
        .signed_duration_since(now);
    (ahead <= chrono::Duration::minutes(horizon)).then_some(raw)
}

/// A window for the first matching label that carries a percent. The reset is
/// carried through when the block has a `data-time` and OMITTED otherwise — a
/// depleted window (e.g. a weekly quota at 100% used) shows no reset timestamp
/// on the settings page, but its percent is still real and must surface rather
/// than vanish (matching the fleet-wide rule: `usedPercent` is load-bearing,
/// `resetsAt` is optional, never fabricated). `window_minutes` is per-label: an
/// "Hourly usage" block carries no fixed length (matching CodexBar, which stamps
/// 5h only on "Session usage"), so a short hourly window is never mislabeled as
/// the 5-hour session window.
fn window_for(
    html: &str,
    labels: &[(&str, Option<i64>)],
    now: chrono::DateTime<chrono::Utc>,
) -> Option<RateWindow> {
    for (label, window_minutes) in labels {
        if let Some(block) = block_after(html, label) {
            if let Some(used_percent) = parse_percent(block) {
                return Some(RateWindow {
                    used_percent,
                    raw_used_percent: None,
                    resets_at: plausible_reset(block, *window_minutes, now),
                    window_minutes: *window_minutes,
                    used_count: None,
                    total_count: None,
                    regeneration: None,
                });
            }
        }
    }
    None
}

/// True when the session block carries a "Weekly limit reached" notice.
///
/// While the weekly quota is exhausted the settings page replaces the session
/// block's usual "Resets in N hours" caption with that notice, and the only
/// timestamp it then renders is the WEEKLY reset. The check names the weekly
/// limit specifically: a session-scoped notice must not move a session reset.
///
/// `any` rather than `all`: `SESSION_LABELS` holds two captions the page has used
/// for the session block at different times, so normally only one appears and the
/// choice does not arise. Should a page ever render both, a notice under either
/// caption still says the session reset is missing, and treating it as missing is
/// the safe reading. Requiring the notice under *both* captions would skip the
/// move below and leave the five-hour window holding a timestamp days away.
fn session_block_reports_weekly_limit(html: &str) -> bool {
    SESSION_LABELS
        .iter()
        .filter_map(|(label, _)| block_after(html, label))
        .any(|block| block.contains("Weekly limit reached"))
}

/// "Session usage" is the 5-hour window; "Hourly usage" is a distinct shorter
/// window with no fixed length on the wire (CodexBar leaves it nil).
///
/// `Monthly usage` is here for a label we have NOT observed on this host. Upstream
/// added it at v0.56.x as the primary label for accounts on monthly credits,
/// keeping the two above "for older pages", and a probe of the live page found
/// only the legacy pair — so this is a label the page can carry rather than one
/// it does. `crates/quota-core/examples/ollama-labels.rs` is that probe; re-run
/// it rather than re-deriving whether the page has moved.
///
/// LISTED ANYWAY, and the asymmetry is the whole reason. This provider is an HTML
/// scrape, so a label we do not recognise is not an error — the block is simply
/// not published, and an unpublished window reads downstream as capacity nobody
/// is consuming. That is exactly the failure that took the fleet down on
/// 2026-07-25. Recognising a label that never appears costs nothing; failing to
/// recognise one that does costs a silent overstatement of headroom.
///
/// Its cadence is `None` rather than 30 days. Upstream stamps a month sentinel
/// and resolves the real calendar month downstream from the reset date; we have
/// never seen the block, so its length is not ours to state. The percent is
/// load-bearing and publishes; the cadence is metadata and is omitted, which is
/// the standing rule for every reset-optional window here.
///
/// BOUND WORTH KNOWING: [`window_for`] takes the FIRST label that yields a
/// percent, so a page rendering both a session and a monthly block publishes only
/// the session one. That is the honest reading of what is observable — upstream
/// keeps the legacy labels "for older pages", which says the two are alternatives
/// rather than siblings, and they are ALSO the tighter window when present. If a
/// page ever carries both, this needs a second slot rather than a reordering, and
/// the probe is what would show it.
const SESSION_LABELS: &[(&str, Option<i64>)] = &[
    ("Session usage", Some(SESSION_WINDOW_MINUTES)),
    ("Hourly usage", None),
    ("Monthly usage", None),
];

/// Normalize the settings HTML to [`Usage`]. Pure — unit-testable against a fixture.
pub fn normalize_usage(html: &str) -> Result<Usage, FetchError> {
    normalize_usage_at(html, chrono::Utc::now())
}

/// The parser proper, with the clock injected.
///
/// Reset plausibility is measured against a horizon, so the parse depends on the
/// time it runs at. Taking `now` as an argument keeps that dependency explicit and
/// the function pure -- a test can place a page's timestamps either side of the
/// bound instead of manufacturing one relative to the wall clock and hoping the
/// margin holds.
pub fn normalize_usage_at(
    html: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Usage, FetchError> {
    let mut session = window_for(html, SESSION_LABELS, now);
    let mut weekly = window_for(html, &[("Weekly usage", Some(WEEKLY_WINDOW_MINUTES))], now);

    // Re-attribute the reset when the weekly quota is exhausted. In that state
    // the page stops stating when the session window rolls and renders only the
    // weekly reset — inside the session block, because that is where the notice
    // lives. Reading it positionally would claim a 5-hour window resets days
    // from now, which is impossible and reads as a mislabeled window. Move it to
    // the window it actually describes, and leave the session reset absent since
    // the page no longer reports it (never fabricated).
    if session_block_reports_weekly_limit(html) {
        let borrowed = session.as_mut().and_then(|window| window.resets_at.take());
        if let Some(weekly_window) = weekly.as_mut() {
            if weekly_window.resets_at.is_none() {
                weekly_window.resets_at = borrowed;
            }
        }
    }

    if session.is_none() && weekly.is_none() {
        if looks_signed_out(html) {
            return Err(FetchError::Unauthorized(
                "ollama session expired (settings page served a login)".to_string(),
            ));
        }
        return Err(FetchError::Decode(
            "ollama: no usage windows in settings HTML".to_string(),
        ));
    }

    Ok(Usage {
        primary: session,
        secondary: weekly,
        tertiary: None,
        extra_rate_windows: None,
    })
}

// ---- provider ---------------------------------------------------------------

/// The Ollama usage provider.
pub struct OllamaProvider {
    vault: crate::cookie_vault::CookieVault,
    http: reqwest::Client,
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(
            None,
            std::sync::Arc::new(crate::vault_handles::VaultHandleLoader::from_env()),
        )
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<std::sync::Arc<dyn crate::credential_source::CredentialSource>>,
        handle_loader: std::sync::Arc<crate::vault_handles::VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            vault: crate::cookie_vault::CookieVault::new(
                credential_source,
                handle_loader,
                COOKIE_FAMILY,
            ),
        }
    }
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for OllamaProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        self.vault.handles()
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let (jar, source) = self
                .vault
                .jar_for(handle, || async {
                    browser_cookies::chrome_cookies_for_async(DOMAIN)
                        .await
                        .map_err(FetchError::from)
                })
                .await?;

            // A jar without a recognized session cookie is not a usable login.
            if !jar.has_cookie_named(is_session_cookie) {
                return Err(FetchError::NoSession(format!(
                    "no ollama session cookie {} ({})",
                    crate::cookie_vault::source_phrase(source),
                    jar.session_absence_detail()
                )));
            }

            let html_bytes = JsonRequest::get(SETTINGS_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Cookie", jar.header()))
                .header(Header::new(
                    "User-Agent",
                    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36",
                ))
                .header(Header::new(
                    "Accept",
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ))
                .header(Header::new("Referer", SETTINGS_URL))
                .send_full(&self.http)
                .await?;

            // WHERE THE RESPONSE CAME FROM, CHECKED BEFORE WHAT IT SAYS.
            //
            // A dead browser session is 303'd to an auth host and the client
            // follows it, so a valid sign-in page arrives with a 200 and a
            // well-formed body that simply has no usage labels in it. Parsing
            // first turns that into `decode_failed` -- a verdict accusing THIS
            // REPOSITORY of a parser bug, when the remedy is for a human to sign
            // in again. That is what this provider published on 2026-09-05 after
            // ollama moved its sign-in to `signin.ollama.com`.
            //
            // The destination is in-band: it rides the same response as the body,
            // so the two cannot disagree. `looks_signed_out` below is a markup
            // heuristic over the body and stays as the fallback for a login served
            // WITHOUT a redirect -- but it could not see this one, because the new
            // page carries no `<form>` and no `/auth/signin` route.
            //
            // Host comparison rather than prefix matching: a redirect to any host
            // that is not the one we asked is a redirect away from the settings
            // page, and enumerating auth hostnames would need updating every time
            // an upstream changes identity provider -- which is the event this
            // exists to survive.
            if redirected_off_settings(&html_bytes.final_url) {
                return Err(FetchError::Unauthorized(format!(
                    "ollama session expired (settings redirected to {})",
                    // Host only. The full URL carries an authorization session id
                    // and a client id, which are per-attempt but still identifiers
                    // this module has no reason to publish.
                    host_of(&html_bytes.final_url).unwrap_or_else(|| "another host".to_string())
                )));
            }

            let html = String::from_utf8_lossy(&html_bytes.body);
            let usage = normalize_usage(&html)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, source, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured-real ollama.com/settings usage section (collapsed but structurally
    /// faithful: `N% used` spans + a `data-time` local-time div per window).
    const SETTINGS_FIXTURE: &str = r#"
      <div class="flex justify-between mb-2">
        <span class="text-sm">Session usage</span>
        <span class="text-sm "> 0% used </span>
      </div>
      <div class="relative h-3" data-usage-track aria-label="Session usage 0% used">
        <div style="width: 0%; "></div>
      </div>
      <div class="text-xs local-time" data-time="2026-06-24T03:00:00Z">Resets in 5 hours.</div>
      <div class="flex justify-between mb-2">
        <span class="text-sm">Weekly usage</span>
        <span class="text-sm " >30.8% used</span >
      </div>
      <div class="text-xs local-time" data-time="2026-06-29T00:00:00Z">Resets in 5 days.</div>
    "#;

    const SIGNIN_FIXTURE: &str = r#"
      <h1>Sign in to Ollama</h1>
      <form action="/api/auth/signin" method="post">
        <input type="email" name="email"/>
        <input type="password" name="password"/>
      </form>
    "#;

    #[test]
    fn parses_session_and_weekly_windows() {
        let usage = normalize_usage(SETTINGS_FIXTURE).unwrap();
        let session = usage.primary.unwrap();
        assert_eq!(session.used_percent, 0.0);
        assert_eq!(session.resets_at.as_deref(), Some("2026-06-24T03:00:00Z"));
        assert_eq!(session.window_minutes, Some(300));
        let weekly = usage.secondary.unwrap();
        assert_eq!(weekly.used_percent, 30.8);
        assert_eq!(weekly.resets_at.as_deref(), Some("2026-06-29T00:00:00Z"));
        assert_eq!(weekly.window_minutes, Some(10080));
    }

    /// A redirect off the settings host is an expired session, not a parse bug.
    ///
    /// THE DEFECT THIS PINS COST A LIVE PROVIDER. On 2026-09-05 ollama moved its
    /// sign-in to `signin.ollama.com`; a dead cookie was 303'd there, the client
    /// followed it, and a 117 KB sign-in page arrived with a 200. The body was
    /// well-formed and carried no usage labels, so this module published
    /// `decode error: no usage windows in settings HTML` -- which accuses this
    /// repository of a parser bug when the remedy is to sign in again.
    ///
    /// `looks_signed_out` could not catch it: the new page has no `<form>` and no
    /// `/auth/signin` route, so every conjunct of that heuristic was false ON A
    /// SIGN-IN PAGE. The redirect is the signal that cannot be fooled by markup.
    #[test]
    fn a_redirect_off_the_settings_host_is_an_expired_session() {
        assert!(redirected_off_settings(
            "https://signin.ollama.com/?client_id=abc&authorization_session_id=def"
        ));
        assert_eq!(
            host_of("https://signin.ollama.com/?client_id=abc"),
            Some("signin.ollama.com".to_string()),
            "the message names the host and never the session identifiers"
        );
    }

    /// The settings host itself, and an unrecorded URL, are not redirects.
    ///
    /// THE CONTROL, and the empty case is the one that matters: every fixture in
    /// this file constructs a response with no final URL, so treating absence as a
    /// redirect would make every parser test report a sign-in page. A guard that
    /// fires on missing evidence is worse than no guard -- it would take a healthy
    /// provider dark the moment a transport stopped recording the URL.
    #[test]
    fn the_settings_host_and_an_absent_url_are_not_redirects() {
        assert!(!redirected_off_settings("https://ollama.com/settings"));
        assert!(!redirected_off_settings(
            "https://ollama.com/settings?tab=usage"
        ));
        assert!(
            !redirected_off_settings(""),
            "an unrecorded URL is not evidence of a redirect"
        );
        assert!(
            !redirected_off_settings("not a url"),
            "an unparseable URL is not evidence of a redirect"
        );
    }

    #[test]
    fn signed_out_page_is_unauthorized() {
        assert!(matches!(
            normalize_usage(SIGNIN_FIXTURE),
            Err(FetchError::Unauthorized(_))
        ));
    }

    #[test]
    fn usage_page_without_markers_is_decode_error() {
        assert!(matches!(
            normalize_usage("<html><body>nothing useful here</body></html>"),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn percent_without_reset_keeps_the_window_with_no_resets_at() {
        // A Session block with a percent but NO data-time still surfaces (reset
        // omitted, never fabricated) — the percent is the load-bearing field.
        // Weekly is well-formed and carries its reset.
        let html = r#"
          <span>Session usage</span><span>50% used</span>
          <span>Weekly usage</span><span>10% used</span>
          <div data-time="2026-06-29T00:00:00Z">Resets in 5 days.</div>
        "#;
        let usage = normalize_usage(html).unwrap();
        let session = usage.primary.unwrap();
        assert_eq!(session.used_percent, 50.0);
        assert_eq!(
            session.resets_at, None,
            "no data-time → reset omitted, not dropped"
        );
        assert_eq!(usage.secondary.unwrap().used_percent, 10.0);
    }

    #[test]
    fn depleted_weekly_at_full_percent_without_a_reset_still_surfaces() {
        // Captured-live shape: a depleted weekly quota shows "100% used" (red) with
        // NO data-time reset on the settings page. It must surface as a 100% window
        // with no reset, not vanish — the old percent-and-reset-required rule dropped
        // it, hiding a real (exhausted) 7-day window.
        let html = r#"
          <span>Session usage</span>
          <div data-usage-track aria-label="Session usage 36.3% used">
            <div style="width: 100%; background: #d4d4d4;"></div>
          </div>
          <div class="text-xs local-time" data-time="2026-07-20T03:00:00Z">Resets in 5 hours.</div>
          <span>Weekly usage</span>
          <span class="text-red-500">100% used</span>
          <div data-usage-track aria-label="Weekly usage 100% used">
            <div style="width: 100%"></div>
          </div>
        "#;
        let usage = normalize_usage(html).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 36.3);
        let weekly = usage.secondary.expect("depleted weekly must surface");
        assert_eq!(weekly.used_percent, 100.0);
        assert_eq!(
            weekly.resets_at, None,
            "depleted window has no reset timestamp"
        );
        assert_eq!(weekly.window_minutes, Some(10080));
    }

    /// A reset is moved off the session window only when the notice sits in the
    /// session block.
    ///
    /// Each window takes its reset from the timestamp that follows its own caption
    /// on the page. When the weekly quota is spent, the page stops printing the
    /// session reset and prints the weekly one under the session caption instead --
    /// so that positional read hands a five-hour window a timestamp days away, and
    /// `normalize_usage` moves it to the weekly window to correct that.
    ///
    /// The notice is not fixed to the session block. Printed under the weekly
    /// caption instead, the session block still states its own real reset, and
    /// moving it would produce the mirror of the defect the move exists to fix: a
    /// seven-day window claiming a horizon five hours out, while the five-hour
    /// window reports none.
    #[test]
    fn a_notice_outside_the_session_block_leaves_both_resets_alone() {
        let html = r#"
          <span>Session usage</span>
          <div data-usage-track aria-label="Session usage 12% used"></div>
          <div class="text-xs local-time" data-time="2026-07-25T18:00:00Z">Resets in 5 hours.</div>
          <span>Weekly usage</span>
          <span class="text-red-500">100% used</span>
          <span class="text-sm text-neutral-500">Weekly limit reached</span>
          <div data-usage-track aria-label="Weekly usage 100% used"></div>
        "#;
        let usage = normalize_usage(html).unwrap();

        let session = usage.primary.expect("session window reported");
        assert_eq!(session.window_minutes, Some(300));
        assert_eq!(
            session.resets_at.as_deref(),
            Some("2026-07-25T18:00:00Z"),
            "the session block states its own reset here, so nothing may take it"
        );

        let weekly = usage.secondary.expect("weekly window reported");
        assert_eq!(weekly.used_percent, 100.0);
        assert_eq!(weekly.window_minutes, Some(10080));
        assert_eq!(
            weekly.resets_at, None,
            "the page reports no weekly horizon in this state, and a borrowed \
             session timestamp would describe the wrong window"
        );
    }

    /// A weekly reset the page prints itself is never replaced by the moved one.
    ///
    /// Moving the session block's timestamp fills a gap: the page normally stops
    /// printing a weekly reset in this state, so the weekly window would otherwise
    /// have none. When the page prints one under the weekly caption, that is its
    /// own account of when the weekly window rolls, and the timestamp taken from
    /// the session block can only be a worse copy of it.
    #[test]
    fn a_stated_weekly_reset_survives_the_re_attribution() {
        let html = r#"
          <span>Session usage</span>
          <span class="text-sm text-neutral-500">Weekly limit reached</span>
          <div data-usage-track aria-label="Session usage 49.8% used"></div>
          <div class="text-xs local-time" data-time="2026-07-27T00:00:00Z">Resets Monday.</div>
          <span>Weekly usage</span>
          <div data-usage-track aria-label="Weekly usage 100% used"></div>
          <div class="text-xs local-time" data-time="2026-07-28T09:30:00Z">Resets Tuesday.</div>
        "#;
        let usage = normalize_usage(html).unwrap();

        let weekly = usage.secondary.expect("weekly window reported");
        assert_eq!(
            weekly.resets_at.as_deref(),
            Some("2026-07-28T09:30:00Z"),
            "the weekly block states its own reset, so the borrowed one is discarded"
        );

        // The session reset is still surrendered: in this state the page is not
        // reporting when the session window rolls, so keeping it would leave a
        // five-hour window carrying a timestamp that describes the weekly one.
        let session = usage.primary.expect("session window reported");
        assert_eq!(session.window_minutes, Some(300));
        assert_eq!(session.resets_at, None);
    }

    #[test]
    fn weekly_limit_reached_moves_the_reset_off_the_session_window() {
        // Captured live 2026-07-25 while the weekly quota was exhausted. In that
        // state the settings page replaces the session block's "Resets in N hours"
        // caption with a "Weekly limit reached" notice, and the only timestamp it
        // renders sits inside the SESSION block while describing the WEEKLY reset.
        // Read positionally, that claimed a 5-hour window resetting ~36 hours out
        // — impossible for its length — while the exhausted weekly window, the one
        // a consumer must wait on, carried no horizon at all.
        let html = r#"
          <span>Session usage</span>
          <span class="text-sm text-neutral-500">Weekly limit reached</span>
          <div data-usage-track aria-label="Session usage 49.8% used">
            <div style="width: 100%; background: #d4d4d4;"></div>
          </div>
          <div class="text-xs local-time" data-time="2026-07-27T00:00:00Z">Resets Monday.</div>
          <span>Weekly usage</span>
          <span class="text-red-500">100% used</span>
          <div data-usage-track aria-label="Weekly usage 100% used">
            <div style="width: 100%"></div>
          </div>
        "#;
        let usage = normalize_usage(html).unwrap();

        let session = usage.primary.expect("session window still reported");
        assert_eq!(session.used_percent, 49.8);
        assert_eq!(session.window_minutes, Some(300));
        assert_eq!(
            session.resets_at, None,
            "a 5-hour window must not claim a reset ~36 hours out; the page stops \
             reporting the session reset in this state, so it is absent, not invented"
        );

        let weekly = usage.secondary.expect("exhausted weekly window");
        assert_eq!(weekly.used_percent, 100.0);
        assert_eq!(weekly.window_minutes, Some(10080));
        assert_eq!(
            weekly.resets_at.as_deref(),
            Some("2026-07-27T00:00:00Z"),
            "the timestamp describes the weekly reset, so it must ride the weekly \
             window — that horizon is what a blocked consumer waits on"
        );
    }

    /// A fixed clock for the reset-horizon tests.
    ///
    /// The parse now depends on the time it runs at, so these fixtures place
    /// their timestamps against a STATED instant rather than an offset from the
    /// wall clock. An offset-based fixture drifts: it passes when written and
    /// fails at whatever margin the next change chooses.
    fn at(stamp: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(stamp)
            .expect("a valid fixture instant")
            .with_timezone(&chrono::Utc)
    }

    /// THE REGRESSION: a reset further from its label than the old byte cap.
    ///
    /// Reported by the operator as a weekly window showing a percent and no
    /// countdown. The timestamp was on the page the whole time, 4207 bytes after
    /// the label, because the weekly section carries a PER-MODEL BREAKDOWN whose
    /// length grows with how many models the account has used. The old bound was
    /// 4000 bytes, so the page outgrew it.
    ///
    /// The filler here is 6000 bytes: comfortably past the retired cap, and past
    /// any cap that would have been chosen to replace it. That is the point --
    /// the fixture is sized against the CLASS of bound, not against the number
    /// that happened to be there, so it stays meaningful if someone reintroduces
    /// a larger one.
    #[test]
    fn a_reset_far_below_its_label_is_still_attributed() {
        let now = at("2026-09-20T12:00:00Z");
        let mut html = String::from("<span>Weekly usage</span><span>80% used</span>");
        // Stand-in for the per-model breakdown that pushed the real page over.
        html.push_str(&"<div>model row</div>".repeat(300));
        html.push_str(r#"<div data-time="2026-09-21T00:00:00Z">Resets in 12 hours.</div>"#);
        assert!(html.len() > 6000, "the fixture must clear any byte cap");

        let usage = normalize_usage_at(&html, now).expect("a valid page parses");
        let weekly = usage.secondary.expect("weekly window");
        assert_eq!(weekly.used_percent, 80.0);
        assert_eq!(
            weekly.resets_at.as_deref(),
            Some("2026-09-21T00:00:00Z"),
            "the reset is in the weekly section, however far down it sits"
        );
    }

    /// The guard that replaced the byte cap, and the reason removing it is safe.
    ///
    /// With the last label's block running to the end of the page, an unrelated
    /// timestamp further down could be attributed to it. A renewal date months
    /// out cannot be a 7-day window's reset, so the cadence refuses it while the
    /// test above still passes -- each survives the other's mutation.
    #[test]
    fn a_timestamp_beyond_the_window_cadence_is_not_attributed() {
        let now = at("2026-09-20T12:00:00Z");
        let html = concat!(
            "<span>Weekly usage</span><span>80% used</span>",
            r#"<div data-time="2026-12-01T00:00:00Z">Renews December 1.</div>"#
        );

        let usage = normalize_usage_at(html, now).expect("a valid page parses");
        let weekly = usage.secondary.expect("weekly window");
        assert_eq!(weekly.used_percent, 80.0, "the percent is still real");
        assert!(
            weekly.resets_at.is_none(),
            "a date 72 days out is not a 7-day window's reset: {:?}",
            weekly.resets_at
        );
    }

    /// A reset already PAST is kept rather than dropped.
    ///
    /// It means the window rolled and the page has not caught up. Dropping it
    /// would turn a stale reading into a resetless one and hide that the page is
    /// behind -- the same fabrication-by-omission the percent rule forbids.
    #[test]
    fn a_reset_in_the_past_is_kept() {
        let now = at("2026-09-20T12:00:00Z");
        let html = concat!(
            "<span>Weekly usage</span><span>80% used</span>",
            r#"<div data-time="2026-09-20T00:00:00Z">Reset earlier today.</div>"#
        );

        let usage = normalize_usage_at(html, now).expect("a valid page parses");
        assert_eq!(
            usage.secondary.expect("weekly").resets_at.as_deref(),
            Some("2026-09-20T00:00:00Z"),
            "a page that is behind still states when it last rolled"
        );
    }

    /// Multibyte page text must not panic the slicer.
    ///
    /// Both bounds are now character boundaries by construction (a label match
    /// and the string end), so this can no longer fail via the cap that used to
    /// cause it. Kept because the page genuinely carries user text in any UTF-8
    /// and a panicking fetch is classified non-transient -- a working provider
    /// would read as absent rather than degraded.
    #[test]
    fn multibyte_page_text_parses_without_panicking() {
        let now = at("2026-09-20T12:00:00Z");
        let mut html = String::from("<span>Session usage</span><span>42% used</span>");
        html.push_str(&"caf\u{e9} \u{1f600} ".repeat(400));
        html.push_str("<span>Weekly usage</span><span>50% used</span>");

        let usage = normalize_usage_at(&html, now).expect("a valid page must still parse");
        assert_eq!(usage.primary.expect("session window").used_percent, 42.0);
    }

    #[test]
    fn session_reset_is_untouched_when_no_weekly_limit_notice() {
        // Boundary: without the notice, the session block's own timestamp is a real
        // session reset and must stay on the session window.
        let html = r#"
          <span>Session usage</span><span>12% used</span>
          <div data-time="2026-07-25T16:00:00Z">Resets in 5 hours.</div>
          <span>Weekly usage</span><span>40% used</span>
          <div data-time="2026-07-27T00:00:00Z">Resets Monday.</div>
        "#;
        let usage = normalize_usage(html).unwrap();
        assert_eq!(
            usage.primary.unwrap().resets_at.as_deref(),
            Some("2026-07-25T16:00:00Z")
        );
        assert_eq!(
            usage.secondary.unwrap().resets_at.as_deref(),
            Some("2026-07-27T00:00:00Z")
        );
    }

    #[test]
    fn hourly_usage_window_has_no_fixed_length() {
        // An "Hourly usage" block (not "Session usage") is a distinct short window;
        // it must NOT be stamped with the 5-hour session length.
        let html = r#"
          <span>Hourly usage</span><span>2.5% used</span>
          <div data-time="2026-01-30T18:00:00Z">Resets in 3 hours</div>
          <span>Weekly usage</span><span>4.2% used</span>
          <div data-time="2026-02-02T00:00:00Z">Resets in 2 days</div>
        "#;
        let usage = normalize_usage(html).unwrap();
        let session = usage.primary.unwrap();
        assert_eq!(session.used_percent, 2.5);
        assert_eq!(session.window_minutes, None, "hourly has no fixed length");
        assert_eq!(usage.secondary.unwrap().window_minutes, Some(10080));
    }

    /// A page on monthly credits publishes its percent rather than nothing.
    ///
    /// SYNTHETIC FIXTURE, and deliberately labelled as one: this host's page
    /// carries only the legacy `Session usage` / `Weekly usage` pair, verified by
    /// `examples/ollama-labels.rs`, so the block below is built from upstream's
    /// v0.56.x label rather than from an observed capture. The percent shape is
    /// the page's own, which is what makes the fixture worth anything.
    ///
    /// The failure this defends is silent in the dangerous direction. An
    /// unrecognised label is not an error here — the block is skipped, no window
    /// is published, and a consumer reads absent capacity pressure as headroom.
    /// So the assertion is that SOMETHING is published, not that its cadence is
    /// known: `window_minutes` stays `None` because a month is upstream's
    /// sentinel to resolve and not a length we have observed.
    #[test]
    fn a_monthly_usage_block_publishes_its_percent_without_a_fabricated_cadence() {
        let html = r#"
          <span>Monthly usage</span><span>61.5% used</span>
          <div data-time="2026-10-01T00:00:00Z">Resets in 28 days</div>
        "#;
        let usage = normalize_usage(html).expect("a monthly block is a usable page");
        let monthly = usage
            .primary
            .expect("the monthly block must publish -- an unpublished window reads as headroom");
        assert_eq!(monthly.used_percent, 61.5);
        assert_eq!(
            monthly.window_minutes, None,
            "a month is upstream's sentinel to resolve, not a cadence we have observed"
        );
        assert_eq!(
            monthly.resets_at.as_deref(),
            Some("2026-10-01T00:00:00Z"),
            "the reset is stated by the page, so it is carried"
        );
    }

    /// The tighter legacy window still wins when a page carries both.
    ///
    /// Paired with the test above so the ORDER in `SESSION_LABELS` is load-bearing
    /// rather than incidental: reversing it would publish a monthly percent while a
    /// five-hour window was the binding constraint, which understates pressure on
    /// the window that actually refuses requests.
    #[test]
    fn a_session_block_outranks_a_monthly_one_when_both_are_present() {
        let html = r#"
          <span>Session usage</span><span>88.0% used</span>
          <div data-time="2026-09-03T04:00:00Z">Resets in 2 hours</div>
          <span>Monthly usage</span><span>12.0% used</span>
          <div data-time="2026-10-01T00:00:00Z">Resets in 28 days</div>
        "#;
        let usage = normalize_usage(html).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(
            primary.used_percent, 88.0,
            "the session block is the tighter window"
        );
        assert_eq!(primary.window_minutes, Some(SESSION_WINDOW_MINUTES));
    }

    #[test]
    fn recognizes_workos_session_cookie() {
        // Ollama moved auth to WorkOS; a jar carrying only `wos-session` is a login.
        assert!(is_session_cookie("wos-session"));
        assert!(is_session_cookie("__Secure-session"));
        assert!(!is_session_cookie("marketing_id"));
    }

    #[test]
    fn parses_decimal_and_spaced_percents() {
        assert_eq!(percent_before_marker("30.8% used", "used"), Some(30.8));
        assert_eq!(percent_before_marker(" 0% used ", "used"), Some(0.0));
        assert_eq!(
            percent_before_marker("foo 100 % used bar", "used"),
            Some(100.0)
        );
        assert_eq!(percent_before_marker("no percent here", "used"), None);
    }
}
