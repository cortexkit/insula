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

use crate::{
    browser_cookies::{self, CookieError},
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
fn is_session_cookie(name: &str) -> bool {
    matches!(
        name,
        "session" | "__Secure-session" | "ollama_session" | "__Host-ollama_session"
    ) || name.starts_with("__Secure-next-auth.session-token")
        || name.starts_with("next-auth.session-token")
}

// ---- HTML parsing (pure) ----------------------------------------------------

/// All usage-block labels, used to bound one block's window at the next block.
const ALL_LABELS: &[&str] = &["Session usage", "Hourly usage", "Weekly usage"];

/// Slice from just after `label` to the next other-label (or `MAX_BLOCK`), so a
/// percent/reset is attributed to the right window (mirrors CodexBar's bounding).
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
    Some(&tail[..end])
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
    let has_heading =
        lower.contains("sign in to ollama") || lower.contains("log in to ollama");
    let has_auth_route = lower.contains("/api/auth/signin") || lower.contains("/auth/signin");
    let has_form = lower.contains("<form");
    let has_password = lower.contains("type=\"password\"") || lower.contains("name=\"password\"");
    has_form && ((has_heading && has_password) || has_auth_route)
}

/// A window for a label, only when BOTH a percent AND a real reset are present —
/// a percent without a reset is dropped (degrade-never-wrong: no fabricated reset).
fn window_for(html: &str, labels: &[&str], window_minutes: i64) -> Option<RateWindow> {
    for label in labels {
        if let Some(block) = block_after(html, label) {
            if let (Some(used_percent), Some(resets_at)) =
                (parse_percent(block), parse_reset(block))
            {
                return Some(RateWindow {
                    used_percent,
                    resets_at,
                    window_minutes: Some(window_minutes),
                });
            }
        }
    }
    None
}

/// Normalize the settings HTML to [`Usage`]. Pure — unit-testable against a fixture.
pub fn normalize_usage(html: &str) -> Result<Usage, FetchError> {
    let session = window_for(html, &["Session usage", "Hourly usage"], SESSION_WINDOW_MINUTES);
    let weekly = window_for(html, &["Weekly usage"], WEEKLY_WINDOW_MINUTES);

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
            http: reqwest::Client::new(),
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

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let jar = browser_cookies::chrome_cookies_for(DOMAIN).map_err(|e| match e {
            // No store / no cookie / unsupported platform → simply not logged in here.
            CookieError::NoStore | CookieError::NoCookie | CookieError::Unsupported => {
                FetchError::NoSession(e.to_string())
            }
            CookieError::NoKeychainKey(_) | CookieError::Extract(_) => {
                FetchError::Upstream(e.to_string())
            }
        })?;

        // A jar without a recognized session cookie is not a usable login.
        if !jar.has_cookie_named(is_session_cookie) {
            return Err(FetchError::NoSession(
                "no ollama session cookie in browser".to_string(),
            ));
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
        // source "api": ollama is a credentialed (non-oauth) fetch; kept within the
        // existing source vocabulary until the consumer's source handling is
        // verified opaque (the cookie cohort may later warrant a distinct label).
        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
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
        assert_eq!(session.resets_at, "2026-06-24T03:00:00Z");
        assert_eq!(session.window_minutes, Some(300));
        let weekly = usage.secondary.unwrap();
        assert_eq!(weekly.used_percent, 30.8);
        assert_eq!(weekly.resets_at, "2026-06-29T00:00:00Z");
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
    fn percent_without_reset_drops_that_window() {
        // A Session block with a percent but NO data-time → no window (never a
        // fabricated reset). Weekly is well-formed and still emitted.
        let html = r#"
          <span>Session usage</span><span>50% used</span>
          <span>Weekly usage</span><span>10% used</span>
          <div data-time="2026-06-29T00:00:00Z">Resets in 5 days.</div>
        "#;
        let usage = normalize_usage(html).unwrap();
        assert!(usage.primary.is_none(), "session has no reset → dropped");
        assert_eq!(usage.secondary.unwrap().used_percent, 10.0);
    }

    #[test]
    fn parses_decimal_and_spaced_percents() {
        assert_eq!(percent_before_marker("30.8% used", "used"), Some(30.8));
        assert_eq!(percent_before_marker(" 0% used ", "used"), Some(0.0));
        assert_eq!(percent_before_marker("foo 100 % used bar", "used"), Some(100.0));
        assert_eq!(percent_before_marker("no percent here", "used"), None);
    }
}
