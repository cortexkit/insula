//! Qoder credit usage — browser-cookie request to the member quota API.
//!
//! Chrome cookies for the exact `qoder.com` and `www.qoder.com` hosts are sent
//! with Qoder's browser headers. The base quota and optional shared quota are
//! merged into one primary percentage window.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! logged-in Qoder browser session. Endpoint and request headers, base/shared
//! quota merging, reset parsing, and snake/camel-case response aliases are from
//! CodexBar `Sources/CodexBarCore/Providers/Qoder/QoderUsageFetcher.swift:10-17,47-129,140-207,210-315`.
//! The primary window mapping is from `QoderUsageSnapshot.swift:30-49`; cookie
//! header construction and exact domains are from `QoderCookieImporter.swift:23-25,37-44,83-101`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies::{self, CookieJar, SOURCE_LABEL},
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "qoder";
const DOMAIN: &str = "qoder.com";
const COOKIE_DOMAINS: &[&str] = &["qoder.com", "www.qoder.com"];
const USAGE_URL: &str = "https://qoder.com/api/v2/me/usages/big_model_credits";
const ORIGIN: &str = "https://qoder.com";
const REFERER: &str = "https://qoder.com/account/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

#[derive(Debug, Deserialize)]
struct QoderUsageResponse {
    #[serde(rename = "totalQuota", alias = "total_quota")]
    total_quota: Option<QoderQuotaContainer>,
    #[serde(rename = "sharedQuota", alias = "shared_quota")]
    shared_quota: Option<QoderQuotaContainer>,
    #[serde(rename = "nextResetAt", alias = "next_reset_at")]
    next_reset_at: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct QoderQuotaContainer {
    #[serde(rename = "quotaSummary", alias = "quota_summary")]
    quota_summary: Option<QoderQuotaSummary>,
}

#[derive(Debug, Deserialize)]
struct QoderQuotaSummary {
    #[serde(rename = "usedValue", alias = "used_value")]
    used_value: f64,
    #[serde(rename = "limitValue", alias = "limit_value")]
    limit_value: f64,
    #[serde(rename = "remainingValue", alias = "remaining_value")]
    remaining_value: Option<f64>,
    #[serde(rename = "usagePercentage", alias = "usage_percentage")]
    usage_percentage: Option<f64>,
}

fn remaining_credits(summary: &QoderQuotaSummary) -> Result<f64, FetchError> {
    if summary.used_value < 0.0
        || summary.limit_value < 0.0
        || summary.remaining_value.is_some_and(|value| value < 0.0)
    {
        return Err(FetchError::Decode(
            "qoder: quota values must be nonnegative".to_string(),
        ));
    }

    Ok(summary
        .remaining_value
        .unwrap_or_else(|| (summary.limit_value - summary.used_value).max(0.0)))
}

fn usage_percentage(
    used: f64,
    total: f64,
    remaining: f64,
    provided: Option<f64>,
) -> Result<f64, FetchError> {
    if used < 0.0 || total < 0.0 || remaining < 0.0 {
        return Err(FetchError::Decode(
            "qoder: quota values must be nonnegative".to_string(),
        ));
    }
    if total == 0.0 {
        if used != 0.0 || remaining != 0.0 {
            return Err(FetchError::Decode(
                "qoder: zero total quota must have zero usage and remaining".to_string(),
            ));
        }
        return Ok(provided.unwrap_or(100.0));
    }

    Ok(provided.unwrap_or((used / total) * 100.0))
}

fn merged_usage_percentage(
    base: &QoderQuotaSummary,
    shared: Option<&QoderQuotaSummary>,
) -> Result<f64, FetchError> {
    let base_remaining = remaining_credits(base)?;
    let Some(shared) = shared else {
        return usage_percentage(
            base.used_value,
            base.limit_value,
            base_remaining,
            base.usage_percentage,
        );
    };

    let shared_remaining = remaining_credits(shared)?;
    usage_percentage(
        base.used_value + shared.used_value,
        base.limit_value + shared.limit_value,
        base_remaining + shared_remaining,
        None,
    )
}

fn parse_reset_at(value: &Value) -> Option<String> {
    if let Some(value) = value.as_str() {
        let date = chrono::DateTime::parse_from_rfc3339(value.trim()).ok()?;
        return Some(
            date.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }

    let value = value.as_f64()?;
    let seconds = if value > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    };
    env::epoch_to_iso8601(seconds as i64)
}

/// Normalize Qoder's member quota response into one primary usage window.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: QoderUsageResponse = serde_json::from_slice(body).map_err(|error| {
        FetchError::Decode(format!("qoder usage response not decodable: {error}"))
    })?;
    let base = response
        .total_quota
        .as_ref()
        .and_then(|quota| quota.quota_summary.as_ref())
        .ok_or_else(|| FetchError::Decode("qoder: missing totalQuota.quotaSummary".to_string()))?;
    let shared = response
        .shared_quota
        .as_ref()
        .and_then(|quota| quota.quota_summary.as_ref());
    let used_percent = merged_usage_percentage(base, shared)?.clamp(0.0, 100.0);
    let resets_at = response.next_reset_at.as_ref().and_then(parse_reset_at);

    Ok(Usage {
        primary: Some(RateWindow {
            used_percent,
            raw_used_percent: None,
            resets_at,
            window_minutes: None,
            used_count: None,
            total_count: None,
            regeneration: None,
        }),
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

fn request_cookie_header(jar: &CookieJar) -> Option<String> {
    let parts: Vec<String> = jar
        .cookies
        .iter()
        .filter(|cookie| {
            let host = cookie
                .host_key
                .strip_prefix('.')
                .unwrap_or(&cookie.host_key);
            COOKIE_DOMAINS
                .iter()
                .any(|domain| host.eq_ignore_ascii_case(domain))
        })
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect();

    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The Qoder browser-cookie usage provider.
pub struct QoderProvider {
    http: reqwest::Client,
}

impl QoderProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
        }
    }
}

impl Default for QoderProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Cookies any visit sets, which prove nothing about being signed in.
///
/// Measured on this host: a browser that has merely loaded a Qoder page holds
/// exactly one cookie, Alibaba's `tfstk`. Sending it produces a 401, which this
/// module reported as a rejected credential -- so an account nobody ever signed
/// into appeared in `cookieLoginsStale`, telling an operator a working login had
/// expired.
///
/// Grows only with MEASURED evidence, never with plausible-looking names. The
/// two directions are not symmetric: leaving a tracking cookie off this list
/// keeps today's wrong-but-visible report, while wrongly listing a real session
/// cookie makes a signed-in account report as never configured, which is the
/// class that authorises a consumer to forget it.
///
/// THE MEASUREMENT THAT ADMITTED THE REST (2026-08-22). Qoder runs on Alibaba
/// infrastructure, so the tempting move is to add every name that LOOKS like an
/// Alibaba tracker -- which is documentation, not evidence. The check that is
/// actually available: does the same cookie NAME appear on unrelated Alibaba
/// domains in this browser? A name shared with `alibabacloud.com`,
/// `aliexpress.com`, `mmstat.com`, `qwen.ai` and `qwencloud.com` is shared
/// infrastructure and cannot be a Qoder session. Counted on this host:
///
///   cna     7 other domains        isg     4 other domains
///   tfstk   6 other domains        xlly_s  2 other domains
///
/// Two names in the same jar were NOT admitted, and the reason is the point:
/// `_c_WBKFRo` and `_nb_ioWEgULi` appear on qoder.com and nowhere else. They
/// have tracker-shaped randomised suffixes, but this test cannot separate a
/// per-site tracker from a session cookie, so they stay off. They are also why
/// this list does not change Qoder's verdict today -- the jar still holds
/// unattributable cookies, the guard still declines to fire, and the upstream
/// still decides. Recorded so the next reader does not re-run the same probe.
const TRACKING_ONLY_COOKIES: &[&str] = &["cna", "isg", "tfstk", "xlly_s"];

/// Whether every cookie present is one that proves nothing.
///
/// Requires ALL of them to be known-tracking. A single unrecognised cookie could
/// be the session, so the jar is sent as before and the upstream decides -- the
/// judgement stays with the only party that can actually make it.
fn jar_is_tracking_only(jar: &browser_cookies::CookieJar) -> bool {
    !jar.cookies.is_empty()
        && jar
            .cookies
            .iter()
            .all(|cookie| TRACKING_ONLY_COOKIES.contains(&cookie.name.as_str()))
}

#[async_trait]
impl UsageProvider for QoderProvider {
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
            // Qoder's importer does not designate one session-cookie name, so send
            // every cookie from its two exact international hosts.
            let cookie = request_cookie_header(&jar).ok_or_else(|| {
                FetchError::NoSession("no cookies for an exact Qoder browser host".to_string())
            })?;
            if jar_is_tracking_only(&jar) {
                return Err(FetchError::NoSession(
                    "only tracking cookies for Qoder: nobody is signed in".to_string(),
                ));
            }

            let body = JsonRequest::get(USAGE_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Cookie", cookie))
                .header(Header::new("Accept", "application/json, text/plain, */*"))
                .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
                .header(Header::new("User-Agent", USER_AGENT))
                .header(Header::new("Origin", ORIGIN))
                .header(Header::new("Referer", REFERER))
                .header(Header::new("X-Requested-With", "XMLHttpRequest"))
                .header(Header::new("Bx-V", "2.5.35"))
                .send(&self.http)
                .await?;
            let usage = normalize_usage(&body)?;

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

    fn jar_of(names: &[&str]) -> browser_cookies::CookieJar {
        browser_cookies::CookieJar {
            cookies: names
                .iter()
                .map(|name| browser_cookies::Cookie {
                    name: (*name).to_string(),
                    value: "x".to_string(),
                    host_key: "qoder.com".to_string(),
                })
                .collect(),
        }
    }

    /// A jar holding only tracking cookies means nobody signed in.
    ///
    /// LIVE MEASUREMENT, this host: exactly one cookie, `tfstk`. Sending it
    /// returns 401, which was published as a rejected credential and put qoder
    /// in `cookieLoginsStale` -- telling an operator that a working login had
    /// expired, for an account that never existed.
    #[test]
    fn a_tracking_only_jar_is_nobody_signed_in() {
        assert!(jar_is_tracking_only(&jar_of(&["tfstk"])));
    }

    /// One unrecognised cookie is enough to send the jar as before.
    ///
    /// The asymmetry is deliberate and is the whole shape of this guard. An
    /// unknown cookie could be the session, and reporting a signed-in account as
    /// never configured is the class that authorises a consumer to forget it --
    /// strictly worse than today's wrong-but-visible report. So the judgement
    /// goes back to the upstream, which is the only party that can make it.
    /// The real jar this host holds reads as nobody signed in, once the
    /// shared-infrastructure names are known.
    ///
    /// SHAPE OBSERVED ON THIS HOST 2026-08-22, minus the two names that could
    /// not be attributed. Each of these appears on unrelated Alibaba domains in
    /// the same browser -- `alibabacloud.com`, `aliexpress.com`, `mmstat.com`,
    /// `qwen.ai`, `qwencloud.com` -- which is what admitted them: a name shared
    /// across unrelated properties is infrastructure, not a Qoder session.
    #[test]
    fn a_jar_of_shared_alibaba_infrastructure_is_nobody_signed_in() {
        assert!(jar_is_tracking_only(&jar_of(&[
            "cna", "isg", "tfstk", "xlly_s"
        ])));
        // Order must not matter: the jar arrives in whatever order the store
        // returns, and a rule that depended on it would pass here and fail live.
        assert!(jar_is_tracking_only(&jar_of(&[
            "xlly_s", "tfstk", "isg", "cna"
        ])));
    }

    /// A qoder-only cookie keeps the jar in play, which is why the real jar
    /// still reaches the upstream.
    ///
    /// THE CONTROL FOR THE TEST ABOVE, and the reason this provider still
    /// reports a rejected credential rather than an absent one. `_c_WBKFRo` and
    /// `_nb_ioWEgULi` sit in the live jar and appear on no other domain, so the
    /// shared-name test cannot tell a per-site tracker from a session. Without
    /// this case, widening the list until the guard fired would look like
    /// progress -- and would report a signed-in account as never configured.
    #[test]
    fn a_cookie_seen_only_on_this_domain_is_not_assumed_to_be_tracking() {
        assert!(!jar_is_tracking_only(&jar_of(&[
            "cna",
            "isg",
            "tfstk",
            "xlly_s",
            "_c_WBKFRo",
            "_nb_ioWEgULi"
        ])));
    }

    #[test]
    fn an_unrecognised_cookie_keeps_the_jar_in_play() {
        assert!(!jar_is_tracking_only(&jar_of(&["tfstk", "qoder_session"])));
        assert!(!jar_is_tracking_only(&jar_of(&["qoder_session"])));
    }

    /// An empty jar is not "tracking only" -- it is handled before this runs.
    ///
    /// Without the emptiness check the predicate answers true for no cookies at
    /// all, which is a different state with its own message, and folding them
    /// would report "only tracking cookies" for a jar that has none.
    #[test]
    fn an_empty_jar_is_a_different_state() {
        assert!(!jar_is_tracking_only(&jar_of(&[])));
    }

    #[test]
    fn maps_snake_case_percentage_and_provider_reset_to_primary() {
        let payload = br#"{
            "next_reset_at": "2024-09-01T00:00:00Z",
            "total_quota": {
                "quota_summary": {
                    "used_value": 125,
                    "limit_value": 500,
                    "remaining_value": 375,
                    "usage_percentage": 37.5,
                    "unit": "credit"
                }
            }
        }"#;

        let usage = normalize_usage(payload).unwrap();
        assert_eq!(
            usage,
            Usage {
                primary: Some(RateWindow {
                    used_percent: 37.5,
                    raw_used_percent: None,
                    resets_at: Some("2024-09-01T00:00:00Z".to_string()),
                    window_minutes: None,
                    used_count: None,
                    total_count: None,
                    regeneration: None,
                }),
                secondary: None,
                tertiary: None,
                extra_rate_windows: None,
            }
        );
    }

    #[test]
    fn merges_base_and_shared_quota_before_computing_percentage() {
        let payload = br#"{
            "totalQuota": {
                "quotaSummary": {
                    "usedValue": 1500,
                    "limitValue": 1500,
                    "remainingValue": 0,
                    "usagePercentage": 100
                }
            },
            "sharedQuota": {
                "quotaSummary": {
                    "usedValue": 200,
                    "limitValue": 1000,
                    "remainingValue": 800,
                    "usagePercentage": 20
                }
            }
        }"#;

        let usage = normalize_usage(payload).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 68.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.window_minutes, None);
        assert_eq!(usage.secondary, None);
        assert_eq!(usage.tertiary, None);
        assert_eq!(usage.extra_rate_windows, None);
    }

    #[test]
    fn emits_percentage_window_when_reset_is_absent() {
        let payload = br#"{
            "total_quota": {
                "quota_summary": {
                    "used_value": 75,
                    "limit_value": 500,
                    "remaining_value": 425,
                    "usage_percentage": 15
                }
            }
        }"#;

        let usage = normalize_usage(payload).unwrap();
        assert_eq!(
            usage,
            Usage {
                primary: Some(RateWindow {
                    used_percent: 15.0,
                    raw_used_percent: None,
                    resets_at: None,
                    window_minutes: None,
                    used_count: None,
                    total_count: None,
                    regeneration: None,
                }),
                secondary: None,
                tertiary: None,
                extra_rate_windows: None,
            }
        );
    }
}
