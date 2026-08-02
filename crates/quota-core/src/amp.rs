//! Amp usage — browser-cookie scrape of ampcode.com/settings.
//!
//! VERIFICATION: FIXTURE-VERIFIED — this port is fixture-verified against CodexBar source.
//! Ported from CodexBar `Sources/CodexBarCore/Providers/Amp/AmpUsageFetcher.swift` (lines 45-48, 103, 265-269, 302-310, 316-321, 360-367),
//! `AmpUsageParser.swift` (lines 4-10, 83-107), and `AmpUsageSnapshot.swift` (lines 69-72).
//! Note that `resets_at` is a COMPUTED/replenishment-derived PROJECTION (now + used/hourlyReplenishment*3600),
//! NOT a provider-reported instant.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies::{self, CookieError},
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "amp";
const DOMAIN: &str = "ampcode.com";
const SETTINGS_URL: &str = "https://ampcode.com/settings";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A recognized session-cookie name (any of these → treat the jar as a real login).
fn is_session_cookie(name: &str) -> bool {
    name == "session"
}

// ---- HTML parsing (pure) ----------------------------------------------------

/// Find `field_name`'s numeric value in an HTML/JS blob, requiring the match to
/// be a whole word.
///
/// `field_name` must be ASCII. The scan advances one byte past a rejected match
/// to allow an overlapping candidate, which is a character boundary only because
/// the byte at the match start is single-byte. A non-ASCII needle would make that
/// advance land inside a character and panic on the next slice — and the panic
/// would surface here rather than at the caller that chose the needle. All four
/// callers pass ASCII literals.
/// Convert an upstream hour count into a window length, refusing anything that
/// is not a plausible duration.
///
/// The range is checked before the cast, because `as i64` saturates rather than
/// failing: an infinite or astronomically large value would otherwise become
/// `i64::MAX` and read as an ordinary length.
fn window_minutes_from_hours(hours: f64) -> Option<i64> {
    if !hours.is_finite() {
        return None;
    }
    let minutes = (hours * 60.0).round();
    if !(1.0..=crate::wire_sanity::MAX_WINDOW_MINUTES as f64).contains(&minutes) {
        return None;
    }
    Some(minutes as i64)
}

fn find_numeric_field(html: &str, field_name: &str) -> Option<f64> {
    let bytes = html.as_bytes();
    let mut start = 0;
    while let Some(pos) = html[start..].find(field_name) {
        let match_pos = start + pos;
        let end_pos = match_pos + field_name.len();

        // Check preceding character
        let prev_ok = if match_pos > 0 {
            let prev_char = bytes[match_pos - 1];
            !prev_char.is_ascii_alphanumeric() && prev_char != b'_' && prev_char != b'$'
        } else {
            true
        };

        // Check succeeding character
        let next_ok = if end_pos < bytes.len() {
            let next_char = bytes[end_pos];
            !next_char.is_ascii_alphanumeric() && next_char != b'_' && next_char != b'$'
        } else {
            true
        };

        if prev_ok && next_ok {
            // Found a potential key match. Now look for ':'
            let after = &html[end_pos..];
            if let Some(colon_pos) = after.find(':') {
                if colon_pos <= 10 {
                    let after_colon = &after[colon_pos + 1..];
                    let after_bytes = after_colon.as_bytes();
                    let mut start_idx = None;
                    let mut end_idx = None;
                    for (i, &b) in after_bytes.iter().enumerate() {
                        if start_idx.is_none() {
                            if b.is_ascii_digit() || b == b'-' || b == b'.' {
                                start_idx = Some(i);
                            } else if b == b',' || b == b'}' || b == b']' || b == b';' {
                                break;
                            }
                        } else {
                            if b.is_ascii_digit() || b == b'.' {
                                // continue
                            } else {
                                end_idx = Some(i);
                                break;
                            }
                        }
                    }
                    if let Some(s) = start_idx {
                        let e = end_idx.unwrap_or(after_bytes.len());
                        if let Ok(val) = after_colon[s..e].parse::<f64>() {
                            return Some(val);
                        }
                    }
                }
            }
        }
        // One byte past the match start, so an overlapping candidate is still
        // found. Safe only while the needle is ASCII (see the doc comment).
        debug_assert!(
            field_name.is_ascii(),
            "find_numeric_field needs an ASCII needle"
        );
        start = match_pos + 1;
    }
    None
}

fn looks_signed_out(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("sign in")
        || lower.contains("log in")
        || lower.contains("login")
        || lower.contains("ampcode.com/login")
}

/// Normalize the settings HTML to [`Usage`]. Pure — unit-testable against a fixture.
pub fn normalize_usage(html: &str, now: DateTime<Utc>) -> Result<Usage, FetchError> {
    let block_pos = html
        .find("freeTierUsage")
        .or_else(|| html.find("getFreeTierUsage"));

    let block = match block_pos {
        Some(pos) => &html[pos..],
        None => {
            if looks_signed_out(html) {
                return Err(FetchError::Unauthorized(
                    "amp session expired (settings page served a login)".to_string(),
                ));
            }
            return Err(FetchError::Decode(
                "amp: freeTierUsage block not found in settings HTML".to_string(),
            ));
        }
    };

    let quota = find_numeric_field(block, "quota");
    let used = find_numeric_field(block, "used");
    let hourly_replenishment = find_numeric_field(block, "hourlyReplenishment");
    let window_hours = find_numeric_field(block, "windowHours");

    let primary = if let (Some(quota), Some(used), Some(hourly_replenishment), Some(window_hours)) =
        (quota, used, hourly_replenishment, window_hours)
    {
        if quota <= 0.0 || hourly_replenishment <= 0.0 {
            None
        } else {
            let used_percent = ((used / quota) * 100.0).clamp(0.0, 100.0);
            let seconds_to_reset = (used / hourly_replenishment) * 3600.0;
            let duration = chrono::Duration::try_seconds(seconds_to_reset.round() as i64);
            let resets_at = duration
                .and_then(|d| now.checked_add_signed(d))
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

            resets_at.map(|resets_at| RateWindow {
                used_percent,
                raw_used_percent: None,
                resets_at: Some(resets_at),
                // Dropped rather than emitted when the upstream number is not a
                // duration: the conversion to an integer saturates instead of
                // failing, so an absurd `windowHours` would otherwise arrive on
                // the wire looking like an ordinary length. The window itself is
                // still published -- the percent is the load-bearing part, and a
                // cadence nobody can state is better absent than wrong.
                window_minutes: window_minutes_from_hours(window_hours),
                used_count: None,
                total_count: None,
            })
        }
    } else {
        None
    };

    if primary.is_none() {
        return Err(FetchError::Decode(
            "amp: missing or invalid usage fields in settings HTML".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

// ---- provider ---------------------------------------------------------------

/// The Amp usage provider.
pub struct AmpProvider {
    http: reqwest::Client,
}

impl AmpProvider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { http }
    }
}

impl Default for AmpProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for AmpProvider {
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
                .map_err(|e| match e {
                    CookieError::NoStore | CookieError::NoCookie | CookieError::Unsupported => {
                        FetchError::NoSession(e.to_string())
                    }
                    CookieError::NoKeychainKey(_) | CookieError::Extract(_) => {
                        FetchError::Upstream(e.to_string())
                    }
                })?;

            if !jar.has_cookie_named(is_session_cookie) {
                return Err(FetchError::NoSession(
                    "no amp session cookie in browser".to_string(),
                ));
            }

            let response = JsonRequest::get(SETTINGS_URL)
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
                .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
                .header(Header::new("Origin", "https://ampcode.com"))
                .header(Header::new("Referer", SETTINGS_URL))
                .send_raw(&self.http)
                .await?;

            if response.status == 401 || response.status == 403 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    response.status
                )));
            }

            if (300..400).contains(&response.status) {
                if let Some(location) = response.header("Location") {
                    let loc_lower = location.to_ascii_lowercase();
                    if loc_lower.contains("auth.ampcode.com")
                        || loc_lower.contains("login")
                        || loc_lower.contains("signin")
                        || loc_lower.contains("sign-in")
                    {
                        return Err(FetchError::Unauthorized(format!(
                            "redirected to login: {}",
                            location
                        )));
                    }
                }
                return Err(FetchError::NoSession(format!(
                    "redirected to unknown location with status {}",
                    response.status
                )));
            }

            if !(200..300).contains(&response.status) {
                let excerpt: String = String::from_utf8_lossy(&response.body)
                    .chars()
                    .take(200)
                    .collect();
                return Err(FetchError::Upstream(format!(
                    "HTTP {}: {excerpt}",
                    response.status
                )));
            }

            let html = String::from_utf8_lossy(&response.body);
            let usage = normalize_usage(&html, chrono::Utc::now())?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const HEALTHY_FIXTURE: &str = r#"
      <script>
        const freeTierUsage = {
          quota: 100.0,
          used: 25.0,
          hourlyReplenishment: 10.0,
          windowHours: 5.0
        };
      </script>
    "#;

    const SIGNED_OUT_FIXTURE: &str = r#"
      <html>
        <body>
          <h1>Please Sign In</h1>
          <a href="https://ampcode.com/login">Login here</a>
        </body>
      </html>
    "#;

    const ZERO_REPLENISHMENT_FIXTURE: &str = r#"
      <script>
        const freeTierUsage = {
          quota: 100.0,
          used: 25.0,
          hourlyReplenishment: 0.0,
          windowHours: 5.0
        };
      </script>
    "#;

    const ZERO_QUOTA_FIXTURE: &str = r#"
      <script>
        const freeTierUsage = {
          quota: 0.0,
          used: 25.0,
          hourlyReplenishment: 10.0,
          windowHours: 5.0
        };
      </script>
    "#;

    /// A window length that is not a duration is dropped, not published.
    ///
    /// `windowHours` comes from the page, and the conversion to an integer
    /// saturates rather than failing -- so without a range check an absurd value
    /// arrives on the wire looking like an ordinary length. Consumers read that
    /// field as a cadence, and the checker that would catch it uses it as the
    /// ceiling for its own reset test, so a nonsense length silently disables
    /// that check for this window.
    #[test]
    fn an_absurd_window_length_is_dropped_and_the_window_still_published() {
        // Exponent notation is not among them: the field parser accepts only
        // digits, `-` and `.`, so "1e400" would read as 1 and prove nothing.
        // These are the shapes that reach the conversion.
        for hours in ["-5.0", "0.0", "999999999.0", "99999999999999999999.0"] {
            let html = format!(
                r#"<script>const freeTierUsage = {{ quota: 100.0, used: 25.0, hourlyReplenishment: 10.0, windowHours: {hours} }};</script>"#
            );
            let now = Utc.with_ymd_and_hms(2026, 7, 29, 10, 0, 0).unwrap();

            let usage = normalize_usage(&html, now).expect("the page still parses");
            let primary = usage
                .primary
                .expect("the percent is load-bearing and must still be published");

            assert_eq!(
                primary.window_minutes, None,
                "windowHours {hours} was published as a length"
            );
            // Not vacuous: the rest of the window is intact, so this cannot pass
            // by dropping the window altogether.
            assert_eq!(primary.used_percent, 25.0);
            assert!(primary.resets_at.is_some());
        }
    }

    /// A real cadence is still published, so the guard cannot pass by refusing
    /// everything.
    #[test]
    fn an_ordinary_window_length_is_published() {
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 10, 0, 0).unwrap();
        let usage = normalize_usage(HEALTHY_FIXTURE, now).unwrap();

        assert_eq!(usage.primary.unwrap().window_minutes, Some(300));
    }

    #[test]
    fn multibyte_html_around_the_needle_does_not_panic() {
        // The scan advances one byte past a rejected match, which is a character
        // boundary only because the needle is ASCII. Multibyte text elsewhere in
        // the document must not reach that cursor: a rejected candidate here is
        // one whose neighbouring character makes it part of a longer identifier,
        // and the walk past it has to stay on character boundaries.
        //
        // The real value is findable only AFTER a rejected candidate, so this
        // fails if the scan gives up on the first non-match rather than passing
        // because nothing was parsed at all.
        let html = "{\"caf\u{e9}Label\":\"caf\u{e9} \u{2014} plan\",\"myquota\":1,\"quota\":42}";
        assert!(!html.is_ascii(), "fixture must be multibyte");
        assert_eq!(find_numeric_field(html, "quota"), Some(42.0));
    }

    #[test]
    fn parses_healthy_usage() {
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 12, 0, 0).unwrap();
        let usage = normalize_usage(HEALTHY_FIXTURE, now).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        // resets_at = now + (25 / 10 * 3600) = now + 9000s = 2 hours 30 mins
        // 12:00:00 + 2h 30m = 14:30:00
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-24T14:30:00Z"));
        assert_eq!(primary.window_minutes, Some(300));
    }

    #[test]
    fn parses_get_free_tier_usage() {
        let now = Utc.with_ymd_and_hms(2026, 6, 24, 12, 0, 0).unwrap();
        let fixture = r#"
          const getFreeTierUsage = {
            quota: 50.0,
            used: 10.0,
            hourlyReplenishment: 5.0,
            windowHours: 2.0
          };
        "#;
        let usage = normalize_usage(fixture, now).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0);
        // resets_at = now + (10 / 5 * 3600) = now + 7200s = 2 hours
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-24T14:00:00Z"));
        assert_eq!(primary.window_minutes, Some(120));
    }

    #[test]
    fn signed_out_returns_unauthorized() {
        let now = Utc::now();
        let res = normalize_usage(SIGNED_OUT_FIXTURE, now);
        assert!(matches!(res, Err(FetchError::Unauthorized(_))));
    }

    #[test]
    fn zero_replenishment_drops_window_and_fails() {
        let now = Utc::now();
        let res = normalize_usage(ZERO_REPLENISHMENT_FIXTURE, now);
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }

    #[test]
    fn zero_quota_drops_window_and_fails() {
        let now = Utc::now();
        let res = normalize_usage(ZERO_QUOTA_FIXTURE, now);
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }

    #[test]
    fn missing_block_returns_decode_error() {
        let now = Utc::now();
        let res = normalize_usage("<html><body>no usage here</body></html>", now);
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }
}
