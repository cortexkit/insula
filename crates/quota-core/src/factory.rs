//! Factory.ai usage — browser-cookie session + billing limits API.
//!
//! Flow (cookie-direct path): pull `factory.ai` cookies from Chrome → resolve a
//! direct bearer from `access-token` or `session` when present → `GET
//! https://api.factory.ai/api/billing/limits` with `Cookie:` and
//! `Authorization: Bearer` → map `limits.standard` (or `core`) pool windows to
//! [`Usage`].
//!
//! DESKTOP-COUPLED: needs a local Chrome login + OS keychain (shared
//! [`browser_cookies`] layer). Dead/missing session, 401/403, or JSON without
//! usable windows → [`FetchError`] (silent degrade), never a fabricated window.
//!
//! VERIFICATION: fixture-verified against CodexBar source, NOT live-verified —
//! no logged-in Factory browser session on the build machine. Cookie domain,
//! session cookie names, billing limits URL/headers, bearer-from-cookie rules,
//! `FactoryBillingLimitsResponse` field mapping, and `FlexibleFactoryDate` reset
//! parsing are ported from CodexBar
//! `Sources/CodexBarCore/Providers/Factory/FactoryStatusProbe.swift`
//! (`:14-23` session names, `:80` domain, `:204-243` limits model, `:253-264`
//! window reset, `:285-307` flexible date, `:1253-1267` billing request,
//! `:1315-1324` auth failures, `:1424-1439` direct bearer). WorkOS token
//! exchange (`:807-816`, `:1488-1576`) is deferred — see `resolve_direct_bearer`.
//!
//! `// TODO(primary): WorkOS-exchange fallback` when only WorkOS refresh cookies
//! exist and no direct bearer is available.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies::{self, CookieError, CookieJar},
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "factory";
const DOMAIN: &str = "factory.ai";
const BILLING_LIMITS_URL: &str = "https://api.factory.ai/api/billing/limits";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

const FIVE_HOUR_MINUTES: i64 = 5 * 60;
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;
const MONTHLY_MINUTES: i64 = 30 * 24 * 60;

/// Session cookie names (CodexBar `FactoryStatusProbe.swift:14-23`).
fn is_session_cookie(name: &str) -> bool {
    matches!(
        name,
        "wos-session"
            | "session"
            | "access-token"
            | "__Secure-next-auth.session-token"
            | "next-auth.session-token"
            | "__Secure-authjs.session-token"
            | "authjs.session-token"
    )
}

/// Direct-path bearer cookies (CodexBar `:1424-1439`).
const DIRECT_BEARER_COOKIE_NAMES: &[&str] = &["access-token", "session"];

fn cookie_value<'a>(jar: &'a CookieJar, name: &str) -> Option<&'a str> {
    jar.cookies
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.value.as_str())
        .filter(|v| !v.trim().is_empty())
}

/// Bearer from cookie value (JWT-shaped values use the raw cookie string as bearer).
fn bearer_from_cookie_value(value: &str) -> String {
    value.trim().to_string()
}

/// Resolve bearer for the cookie-direct billing path. WorkOS exchange not implemented.
fn resolve_direct_bearer(jar: &CookieJar) -> Option<String> {
    for name in DIRECT_BEARER_COOKIE_NAMES {
        if let Some(value) = cookie_value(jar, name) {
            return Some(bearer_from_cookie_value(value));
        }
    }
    // TODO(primary): WorkOS-exchange fallback when only wos-session / refresh cookies exist.
    None
}

fn flex_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parse `windowEnd` — epoch seconds, epoch milliseconds, or ISO8601 (CodexBar
/// `FlexibleFactoryDate` `:285-307`).
fn parse_flexible_factory_date(value: &Value) -> Option<String> {
    if let Some(n) = flex_f64(value) {
        let secs = if n > 1_000_000_000_000.0 {
            (n / 1000.0) as i64
        } else if n > 1_000_000_000.0 {
            n as i64
        } else {
            return None;
        };
        return env::epoch_to_iso8601(secs);
    }
    let s = value.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(
            dt.with_timezone(&Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    None
}

#[derive(Debug, Deserialize)]
struct FactoryBillingWindow {
    #[serde(rename = "usedPercent", default)]
    used_percent: Option<f64>,
    #[serde(rename = "windowEnd", default)]
    window_end: Option<Value>,
    #[serde(rename = "secondsRemaining", default)]
    seconds_remaining: Option<f64>,
}

#[derive(Debug, Deserialize, Default)]
struct FactoryBillingPool {
    #[serde(rename = "fiveHour", default)]
    five_hour: Option<FactoryBillingWindow>,
    #[serde(default)]
    weekly: Option<FactoryBillingWindow>,
    #[serde(default)]
    monthly: Option<FactoryBillingWindow>,
}

#[derive(Debug, Deserialize, Default)]
struct FactoryBillingLimits {
    #[serde(default)]
    standard: Option<FactoryBillingPool>,
    #[serde(default)]
    core: Option<FactoryBillingPool>,
}

#[derive(Debug, Deserialize, Default)]
struct FactoryBillingLimitsResponse {
    #[serde(default)]
    limits: Option<FactoryBillingLimits>,
}

fn reset_at_for_window(window: &FactoryBillingWindow, now: DateTime<Utc>) -> Option<String> {
    if let Some(ref end) = window.window_end {
        if let Some(iso) = parse_flexible_factory_date(end) {
            return Some(iso);
        }
    }
    let secs = window.seconds_remaining?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    let reset = now + chrono::Duration::milliseconds((secs * 1000.0).round() as i64);
    Some(reset.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn rate_window_from(
    window: &FactoryBillingWindow,
    window_minutes: i64,
    now: DateTime<Utc>,
) -> Option<RateWindow> {
    let used_percent = window.used_percent?;
    let resets_at = reset_at_for_window(window, now)?;
    Some(RateWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: Some(window_minutes),
    })
}

fn pool_from_limits(limits: &FactoryBillingLimits) -> Option<&FactoryBillingPool> {
    limits.standard.as_ref().or(limits.core.as_ref())
}

/// Normalize billing limits JSON to [`Usage`] (pure — unit-testable).
pub fn normalize_billing_limits(value: &Value, now: DateTime<Utc>) -> Result<Usage, FetchError> {
    let root: FactoryBillingLimitsResponse = serde_json::from_value(value.clone())
        .map_err(|e| FetchError::Decode(format!("factory billing limits not decodable: {e}")))?;

    let limits = root
        .limits
        .as_ref()
        .ok_or_else(|| FetchError::Decode("factory: response missing limits".to_string()))?;
    let pool = pool_from_limits(limits).ok_or_else(|| {
        FetchError::Decode("factory: limits missing standard and core pools".to_string())
    })?;

    let primary = pool
        .five_hour
        .as_ref()
        .and_then(|w| rate_window_from(w, FIVE_HOUR_MINUTES, now));
    let secondary = pool
        .weekly
        .as_ref()
        .and_then(|w| rate_window_from(w, WEEKLY_MINUTES, now));
    let tertiary = pool
        .monthly
        .as_ref()
        .and_then(|w| rate_window_from(w, MONTHLY_MINUTES, now));

    if primary.is_none() && secondary.is_none() && tertiary.is_none() {
        return Err(FetchError::Decode(
            "factory: no usage windows with both usedPercent and reset".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

/// Decode raw billing limits bytes then normalize.
pub fn normalize_billing_limits_bytes(
    body: &[u8],
    now: DateTime<Utc>,
) -> Result<Usage, FetchError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("factory billing limits not JSON: {e}")))?;
    normalize_billing_limits(&value, now)
}

/// Map billing limits HTTP status (401/403 → Unauthorized; tested for fixture contract).
fn map_billing_http_status(status: u16, body_excerpt: &str) -> Result<(), FetchError> {
    if status == 401 || status == 403 {
        return Err(FetchError::Unauthorized(format!("HTTP {status}")));
    }
    if !(200..300).contains(&status) {
        return Err(FetchError::Upstream(format!(
            "HTTP {status}: {body_excerpt}"
        )));
    }
    Ok(())
}

// ---- provider ---------------------------------------------------------------

/// The Factory.ai usage provider (cookie-direct billing limits path).
pub struct FactoryProvider {
    http: reqwest::Client,
}

impl FactoryProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for FactoryProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for FactoryProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let jar = browser_cookies::chrome_cookies_for(DOMAIN).map_err(|e| match e {
                CookieError::NoStore | CookieError::NoCookie | CookieError::Unsupported => {
                    FetchError::NoSession(e.to_string())
                }
                CookieError::NoKeychainKey(_) | CookieError::Extract(_) => {
                    FetchError::Upstream(e.to_string())
                }
            })?;

            if !jar.has_cookie_named(is_session_cookie) {
                return Err(FetchError::NoSession(
                    "no factory session cookie in browser".to_string(),
                ));
            }

            let bearer = resolve_direct_bearer(&jar).ok_or_else(|| {
                FetchError::NoSession(
                    "factory: no direct bearer cookie (access-token/session); WorkOS exchange not implemented"
                        .to_string(),
                )
            })?;

            let response = JsonRequest::get(BILLING_LIMITS_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Accept", "application/json"))
                .header(Header::new("Content-Type", "application/json"))
                .header(Header::new("Origin", "https://app.factory.ai"))
                .header(Header::new("Referer", "https://app.factory.ai/"))
                .header(Header::new("x-factory-client", "web-app"))
                .header(Header::new("Cookie", jar.header()))
                .bearer(&bearer)
                .send_raw(&self.http)
                .await?;

            let excerpt: String = String::from_utf8_lossy(&response.body)
                .chars()
                .take(200)
                .collect();
            map_billing_http_status(response.status, &excerpt)?;

            let usage = normalize_billing_limits_bytes(&response.body, Utc::now())?;
            Ok(ProviderUsage::healthy(
                PROVIDER_NAME,
                None,
                "api",
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
    use chrono::TimeZone;

    const HEALTHY_FIXTURE: &str = r#"{
      "limits": {
        "standard": {
          "fiveHour": {
            "usedPercent": 12.5,
            "windowEnd": "2026-06-24T08:00:00Z"
          },
          "weekly": {
            "usedPercent": 40.0,
            "windowEnd": 1785000000
          },
          "monthly": {
            "usedPercent": 5.0
          }
        }
      }
    }"#;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 24, 3, 0, 0).unwrap()
    }

    #[test]
    fn parses_primary_and_secondary_from_standard_pool() {
        let value: Value = serde_json::from_str(HEALTHY_FIXTURE).unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 12.5);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-24T08:00:00Z"));
        assert_eq!(primary.window_minutes, Some(300));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 40.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-25T17:20:00Z"));
        assert_eq!(secondary.window_minutes, Some(10080));
        assert!(usage.tertiary.is_none(), "monthly has no reset → dropped");
    }

    #[test]
    fn drops_window_with_percent_but_no_reset() {
        let value: Value =
            serde_json::from_str(r#"{"limits":{"standard":{"fiveHour":{"usedPercent":50.0}}}}"#)
                .unwrap();
        assert!(matches!(
            normalize_billing_limits(&value, fixed_now()),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn seconds_remaining_computes_reset_from_now() {
        let value: Value = serde_json::from_str(
            r#"{"limits":{"standard":{"fiveHour":{"usedPercent":10.0,"secondsRemaining":3600.0}}}}"#,
        )
        .unwrap();
        let now = fixed_now();
        let usage = normalize_billing_limits(&value, now).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 10.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-24T04:00:00Z"));
        assert_eq!(primary.window_minutes, Some(300));
    }

    #[test]
    fn billing_limits_401_is_unauthorized() {
        assert!(matches!(
            map_billing_http_status(401, ""),
            Err(FetchError::Unauthorized(_))
        ));
        assert!(matches!(
            map_billing_http_status(403, ""),
            Err(FetchError::Unauthorized(_))
        ));
    }

    #[test]
    fn falls_back_to_core_pool_when_standard_missing() {
        let value: Value = serde_json::from_str(
            r#"{"limits":{"core":{"weekly":{"usedPercent":1.0,"windowEnd":"2026-06-30T00:00:00Z"}}}}"#,
        )
        .unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        assert!(usage.primary.is_none());
        assert_eq!(usage.secondary.unwrap().used_percent, 1.0);
    }

    #[test]
    fn direct_bearer_prefers_access_token() {
        let jar = CookieJar {
            cookies: vec![
                browser_cookies::Cookie {
                    name: "access-token".to_string(),
                    value: "eyJhb.header.sig".to_string(),
                    host_key: "app.factory.ai".to_string(),
                },
                browser_cookies::Cookie {
                    name: "session".to_string(),
                    value: "other".to_string(),
                    host_key: "app.factory.ai".to_string(),
                },
            ],
        };
        assert_eq!(
            resolve_direct_bearer(&jar).as_deref(),
            Some("eyJhb.header.sig")
        );
    }
}
