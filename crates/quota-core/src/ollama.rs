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
    browser_cookies::{self, SOURCE_LABEL},
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "ollama";
const DOMAIN: &str = "ollama.com";
const SETTINGS_URL: &str = "https://ollama.com/settings";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

const SESSION_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;
const MAX_BLOCK: usize = 4000;

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

/// Slice from just after `label` to the next other-label (or `MAX_BLOCK`), so a
/// percent/reset is attributed to the right window (mirrors CodexBar's bounding).
///
/// `MAX_BLOCK` is a BYTE budget, and the page carries user-facing text that can
/// hold any UTF-8, so the cutoff is rounded down to a character boundary before
/// slicing. A label match is always on a boundary; only the fixed cap can land
/// mid-character.
fn block_after<'a>(html: &'a str, label: &str) -> Option<&'a str> {
    let start = html.find(label)? + label.len();
    let tail = &html[start..];
    let end = ALL_LABELS
        .iter()
        .filter(|l| **l != label)
        .filter_map(|l| tail.find(l))
        .min()
        .unwrap_or(tail.len())
        .min(MAX_BLOCK);
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

/// Heuristic: the settings page was replaced by a sign-in page (dead cookie).
fn looks_signed_out(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let has_heading = lower.contains("sign in to ollama") || lower.contains("log in to ollama");
    let has_auth_route = lower.contains("/api/auth/signin") || lower.contains("/auth/signin");
    let has_form = lower.contains("<form");
    let has_password = lower.contains("type=\"password\"") || lower.contains("name=\"password\"");
    has_form && ((has_heading && has_password) || has_auth_route)
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
fn window_for(html: &str, labels: &[(&str, Option<i64>)]) -> Option<RateWindow> {
    for (label, window_minutes) in labels {
        if let Some(block) = block_after(html, label) {
            if let Some(used_percent) = parse_percent(block) {
                return Some(RateWindow {
                    used_percent,
                    raw_used_percent: None,
                    resets_at: parse_reset(block),
                    window_minutes: *window_minutes,
                    used_count: None,
                    total_count: None,
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
const SESSION_LABELS: &[(&str, Option<i64>)] = &[
    ("Session usage", Some(SESSION_WINDOW_MINUTES)),
    ("Hourly usage", None),
];

/// Normalize the settings HTML to [`Usage`]. Pure — unit-testable against a fixture.
pub fn normalize_usage(html: &str) -> Result<Usage, FetchError> {
    let mut session = window_for(html, SESSION_LABELS);
    let mut weekly = window_for(html, &[("Weekly usage", Some(WEEKLY_WINDOW_MINUTES))]);

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
    http: reqwest::Client,
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
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

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let jar = browser_cookies::chrome_cookies_for_async(DOMAIN)
                .await
                .map_err(FetchError::from)?;

            // A jar without a recognized session cookie is not a usable login.
            if !jar.has_cookie_named(is_session_cookie) {
                return Err(FetchError::NoSession(format!(
                    "no ollama session cookie in browser ({})",
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
                .send(&self.http)
                .await?;

            let html = String::from_utf8_lossy(&html_bytes);
            let usage = normalize_usage(&html)?;
            Ok(ProviderUsage::healthy(
                PROVIDER_NAME,
                None,
                SOURCE_LABEL,
                usage,
            ))
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

    #[test]
    fn a_multibyte_character_straddling_the_block_cap_does_not_panic() {
        // The block bound is a fixed BYTE budget, but the page carries user-facing
        // text that can hold any UTF-8. When a multibyte character straddles that
        // cutoff, slicing at the raw byte offset panics — and because a fetch panic
        // is classified non-transient, a working provider would lose its cached
        // window and read as absent rather than degraded. The percent sits before
        // the cap, so it must still parse.
        let mut html = String::from("Session usage");
        html.push_str(" 42% used ");
        let consumed = html.len() - "Session usage".len();
        html.push_str(&"a".repeat(MAX_BLOCK - consumed - 1));
        html.push('\u{e9}'); // straddles the cap: bytes (MAX_BLOCK - 1)..(MAX_BLOCK + 1)
        html.push_str(" Weekly usage 50% used ");

        let usage = normalize_usage(&html).expect("a valid page must still parse");
        assert_eq!(
            usage.primary.expect("session window").used_percent,
            42.0,
            "the percent precedes the cap, so truncation must not lose it"
        );
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
