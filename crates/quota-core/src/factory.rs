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
//! When only WorkOS refresh cookies are present and no direct bearer is
//! available, this provider resolves no credential and degrades: exchanging
//! those cookies at the WorkOS token endpoint is not implemented here, and a
//! refresh cookie is not itself usable as a bearer.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies::{self, CookieJar},
    http::{Header, JsonRequest},
    model::{ExtraWindow, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "factory";
/// Base key for storing Factory credentials in the provider vault. An account-specific
/// suffix identifies each account, and these credentials are read only from the provider vault.
const COOKIE_FAMILY: &str = "cookie:factory.ai";

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
    // No fallback when only wos-session / refresh cookies are present: exchanging
    // those for a bearer needs the WorkOS token endpoint, which is not
    // implemented here. Returning None degrades this provider with a reason
    // rather than guessing at a credential.
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
fn parse_flexible_factory_date(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(n) = flex_f64(value) {
        let secs = if n > 1_000_000_000_000.0 {
            (n / 1000.0) as i64
        } else if n > 1_000_000_000.0 {
            n as i64
        } else {
            return None;
        };
        return DateTime::from_timestamp(secs, 0);
    }
    let s = value.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn format_reset(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
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

/// How a window's reset metadata resolved, which decides both the reset we emit
/// and which utilization is truthful.
enum ResetState {
    /// A live reset: `secondsRemaining` in the future, or a `windowEnd` that
    /// parsed and is still ahead of `now`.
    Live(String),
    /// `windowEnd` parsed cleanly and lies in the past: the window really rolled.
    Expired,
    /// Reset metadata is present but unresolvable (unparseable `windowEnd`, or a
    /// non-finite/negative `secondsRemaining`). We cannot tell whether it expired.
    Unresolvable,
    /// No reset metadata at all. A legitimate provider answer: emit the window
    /// from the percent alone (ratified in db7a205 — percent is load-bearing,
    /// reset optional, never fabricated).
    Absent,
}

/// Resolve a window's reset, mirroring CodexBar's `resetAt` precedence —
/// `secondsRemaining` first, then `windowEnd`, and a `windowEnd` counts only
/// while it is still in the future (`FactoryStatusProbe.swift:260-268`).
fn reset_state_for_window(window: &FactoryBillingWindow, now: DateTime<Utc>) -> ResetState {
    if let Some(secs) = window.seconds_remaining {
        if secs.is_finite() && secs > 0.0 {
            let reset = now + chrono::Duration::milliseconds((secs * 1000.0).round() as i64);
            return ResetState::Live(format_reset(reset));
        }
    }
    let Some(ref end) = window.window_end else {
        // No windowEnd. Either secondsRemaining was absent entirely (no reset
        // metadata at all) or it was present but unusable.
        return if window.seconds_remaining.is_some() {
            ResetState::Unresolvable
        } else {
            ResetState::Absent
        };
    };
    match parse_flexible_factory_date(end) {
        Some(parsed) if parsed > now => ResetState::Live(format_reset(parsed)),
        Some(_) => ResetState::Expired,
        None => ResetState::Unresolvable,
    }
}

fn rate_window_from(
    window: &FactoryBillingWindow,
    window_minutes: i64,
    now: DateTime<Utc>,
) -> Option<RateWindow> {
    let used_percent = window.used_percent?;
    let (resets_at, used_percent) = match reset_state_for_window(window, now) {
        ResetState::Live(at) => (Some(at), used_percent),
        // Factory leaves a stale percent behind after a short rolling window
        // expires, and its own web UI treats that state as reset, so a window
        // proven to have rolled reports 0 rather than the expired figure
        // (CodexBar `effectiveUsedPercent`, FactoryStatusProbe.swift:270-277).
        ResetState::Expired => (None, 0.0),
        // DELIBERATE DIVERGENCE from CodexBar, which zeroes this case too: its
        // FlexibleFactoryDate cannot distinguish "unparseable" from "past", so it
        // treats both as expiry. We can tell them apart, and reporting 0% here
        // would fabricate good news out of a decode failure — announcing a
        // provider as fully available on the strength of a field we failed to
        // read. The two errors are not symmetric: a wrong 0% sends traffic into
        // a wall, while keeping the real percent only makes an available
        // provider look busy. When we cannot tell which case we are in, we take
        // the recoverable error and emit the reported percent with no reset.
        ResetState::Unresolvable => (None, used_percent),
        ResetState::Absent => (None, used_percent),
    };
    Some(RateWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at,
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
    })
}

fn pool_from_limits(limits: &FactoryBillingLimits) -> Option<&FactoryBillingPool> {
    limits.standard.as_ref().or(limits.core.as_ref())
}

/// Normalize the Core pool into named extra windows.
///
/// Core is an independent allowance from the standard pool, so it must stay
/// visible without claiming an unnamed slot: those slots are read as "this
/// provider's pressure", and letting a walled Core pool headline would report
/// the account as exhausted while standard-model traffic still flows. The
/// reverse error is what this exists to fix — dropping Core entirely hides a
/// genuine wall from anything routing Core-model traffic.
fn core_extra_windows(
    limits: &FactoryBillingLimits,
    now: DateTime<Utc>,
) -> Option<Vec<ExtraWindow>> {
    // Only when Core is a distinct pool. A payload carrying core alone routes it
    // into the unnamed slots instead, so emitting it here as well would report
    // the same allowance twice.
    let core = limits.core.as_ref().filter(|_| limits.standard.is_some())?;

    let windows = [
        (
            "factory-core-5h",
            "Core 5h",
            core.five_hour.as_ref(),
            FIVE_HOUR_MINUTES,
        ),
        (
            "factory-core-7d",
            "Core 7-day",
            core.weekly.as_ref(),
            WEEKLY_MINUTES,
        ),
        (
            "factory-core-monthly",
            "Core Monthly",
            core.monthly.as_ref(),
            MONTHLY_MINUTES,
        ),
    ];

    let extras: Vec<ExtraWindow> = windows
        .into_iter()
        .filter_map(|(id, title, window, minutes)| {
            let window = rate_window_from(window?, minutes, now)?;
            Some(ExtraWindow {
                title: Some(title.to_string()),
                id: Some(id.to_string()),
                window: Some(window),
            })
        })
        .collect();

    (!extras.is_empty()).then_some(extras)
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
            "factory: no usable usage windows".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: core_extra_windows(limits, now),
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
    vault: crate::cookie_vault::CookieVault,
    http: reqwest::Client,
}

impl FactoryProvider {
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

            if !jar.has_cookie_named(is_session_cookie) {
                return Err(FetchError::NoSession(format!(
                    "no factory session cookie in browser ({})",
                    jar.session_absence_detail()
                )));
            }

            // A session cookie was found immediately above, so the browser does
            // hold a factory login -- it just carries no cookie this provider
            // can use as a bearer. Found and unusable, not absent.
            let bearer = resolve_direct_bearer(&jar).ok_or_else(|| {
                FetchError::CredentialUnusable(
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

            let usage = normalize_billing_limits_bytes(response.body_for_parsing()?, Utc::now())?;
            Ok(ProviderUsage::healthy(
                PROVIDER_NAME,
                None,
                source,
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
        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 5.0);
        assert_eq!(tertiary.resets_at, None);
        assert_eq!(tertiary.window_minutes, Some(43200));
    }

    #[test]
    fn keeps_window_with_percent_but_no_reset() {
        let value: Value =
            serde_json::from_str(r#"{"limits":{"standard":{"fiveHour":{"usedPercent":50.0}}}}"#)
                .unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.window_minutes, Some(300));
    }

    #[test]
    fn exhausted_monthly_window_without_reset_is_kept() {
        let value: Value =
            serde_json::from_str(r#"{"limits":{"standard":{"monthly":{"usedPercent":100.0}}}}"#)
                .unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 100.0);
        assert_eq!(tertiary.resets_at, None);
        assert_eq!(tertiary.window_minutes, Some(43200));
    }

    #[test]
    fn unparseable_window_end_keeps_the_reported_percent_and_emits_no_reset() {
        // The deliberate divergence from CodexBar: it zeroes this case, we do not.
        // Reporting 0% off a field we failed to read would announce an exhausted
        // provider as fully available.
        let value: Value = serde_json::from_str(
            r#"{"limits":{"standard":{"fiveHour":{"usedPercent":97.0,"windowEnd":"not-a-date"}}}}"#,
        )
        .unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 97.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn expired_window_end_reports_zero_used_and_no_reset() {
        // A windowEnd that parsed cleanly and lies in the past proves the window
        // rolled, so the stale percent is not the truth: 0 is.
        let value: Value = serde_json::from_str(
            r#"{"limits":{"standard":{"fiveHour":{"usedPercent":93.0,"windowEnd":"2026-06-24T01:00:00Z"}}}}"#,
        )
        .unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 0.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn seconds_remaining_wins_over_window_end() {
        // CodexBar's precedence: secondsRemaining is consulted first, so a live
        // countdown beats an already-past windowEnd instead of reading as expiry.
        let value: Value = serde_json::from_str(
            r#"{"limits":{"standard":{"fiveHour":{"usedPercent":61.0,"windowEnd":"2026-06-24T01:00:00Z","secondsRemaining":3600.0}}}}"#,
        )
        .unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 61.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-24T04:00:00Z"));
    }

    #[test]
    fn unusable_seconds_remaining_keeps_the_reported_percent() {
        // Present-but-unusable reset metadata is Unresolvable, not expiry.
        let value: Value = serde_json::from_str(
            r#"{"limits":{"standard":{"fiveHour":{"usedPercent":88.0,"secondsRemaining":-5.0}}}}"#,
        )
        .unwrap();
        let usage = normalize_billing_limits(&value, fixed_now()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 88.0);
        assert_eq!(primary.resets_at, None);
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

    #[test]
    fn an_exhausted_core_pool_is_reported_as_named_extras() {
        // Core is an allowance independent of standard, so a walled Core pool must
        // stay visible. Dropping it reports the account as comfortable while
        // Core-model traffic is refused; letting it claim an unnamed slot would
        // report the opposite. Named extras are the only shape that says both.
        let now = Utc::now();
        let reset = (now + chrono::Duration::hours(3)).to_rfc3339();
        let value: Value = serde_json::from_str(&format!(
            r#"{{ "limits": {{
                "standard": {{
                    "fiveHour": {{ "usedPercent": 5, "windowEnd": "{reset}" }},
                    "weekly":   {{ "usedPercent": 10, "windowEnd": "{reset}" }}
                }},
                "core": {{
                    "fiveHour": {{ "usedPercent": 100, "windowEnd": "{reset}" }},
                    "monthly":  {{ "usedPercent": 80, "windowEnd": "{reset}" }}
                }}
            }} }}"#
        ))
        .unwrap();

        let usage = normalize_billing_limits(&value, now).expect("both pools are usable");

        assert_eq!(
            usage.primary.expect("standard 5h headlines").used_percent,
            5.0,
            "the standard pool keeps the unnamed slot a consumer reads as provider pressure"
        );

        let extras = usage
            .extra_rate_windows
            .expect("an exhausted core pool must not vanish");
        let core_5h = extras
            .iter()
            .find(|e| e.id.as_deref() == Some("factory-core-5h"))
            .expect("core 5h window");
        assert_eq!(
            core_5h.window.as_ref().expect("window").used_percent,
            100.0,
            "the core wall is the fact this test exists to keep visible"
        );
        assert!(extras
            .iter()
            .any(|e| e.id.as_deref() == Some("factory-core-monthly")));
        assert!(
            !extras
                .iter()
                .any(|e| e.id.as_deref() == Some("factory-core-7d")),
            "an absent core cadence is omitted rather than invented"
        );
    }

    #[test]
    fn a_core_only_payload_still_headlines_without_duplicating_itself() {
        // The pre-existing fallback: with no standard pool, core routes into the
        // unnamed slots. It must not ALSO appear as an extra, which would report
        // one allowance twice.
        let now = Utc::now();
        let reset = (now + chrono::Duration::hours(3)).to_rfc3339();
        let value: Value = serde_json::from_str(&format!(
            r#"{{ "limits": {{ "core": {{
                "fiveHour": {{ "usedPercent": 42, "windowEnd": "{reset}" }}
            }} }} }}"#
        ))
        .unwrap();

        let usage = normalize_billing_limits(&value, now).expect("core alone is usable");
        assert_eq!(usage.primary.expect("core headlines").used_percent, 42.0);
        assert!(
            usage.extra_rate_windows.is_none(),
            "core already occupies the unnamed slot, so it must not repeat as an extra"
        );
    }

    #[test]
    fn a_standard_only_payload_emits_no_extras() {
        let now = Utc::now();
        let reset = (now + chrono::Duration::hours(3)).to_rfc3339();
        let value: Value = serde_json::from_str(&format!(
            r#"{{ "limits": {{ "standard": {{
                "fiveHour": {{ "usedPercent": 7, "windowEnd": "{reset}" }}
            }} }} }}"#
        ))
        .unwrap();

        let usage = normalize_billing_limits(&value, now).unwrap();
        assert_eq!(usage.primary.expect("standard 5h").used_percent, 7.0);
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn handles_without_credential_source_return_only_implicit_local() {
        let provider = FactoryProvider::new_with_handle_loader(
            None,
            std::sync::Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles, vec![CredentialHandle::implicit()]);
    }
}
