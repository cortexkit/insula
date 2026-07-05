//! Cursor usage — browser-cookie scrape of cursor.com/api/usage-summary.
//!
//! Reads a browser session cookie and calls Cursor's JSON API directly.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! logged-in browser session. Endpoint, headers, and response shape ported from
//! CodexBar `Sources/CodexBarCore/Providers/Cursor/CursorStatusProbe.swift:20-30,33-38,192-253,1176-1190`.
//! Unit-tested against a CodexBar-shaped CursorUsageSummary payload.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::{
    browser_cookies::{self, CookieError},
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "cursor";
const DOMAIN: &str = "cursor.com";
const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A recognized session-cookie name (any of these → treat the jar as a real login).
fn is_session_cookie(name: &str) -> bool {
    matches!(
        name,
        "WorkosCursorSessionToken"
            | "__Secure-next-auth.session-token"
            | "next-auth.session-token"
            | "wos-session"
            | "__Secure-wos-session"
            | "authjs.session-token"
            | "__Secure-authjs.session-token"
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorUsageSummary {
    billing_cycle_end: Option<String>,
    individual_usage: Option<IndividualUsage>,
}

#[derive(Debug, Deserialize)]
struct IndividualUsage {
    plan: Option<PlanUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanUsage {
    total_percent_used: Option<f64>,
    used: Option<f64>,
    limit: Option<f64>,
}

fn parse_billing_cycle_end(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    None
}

/// Normalize the usage summary JSON to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let summary: CursorUsageSummary = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("cursor usage summary not decodable: {e}")))?;

    let resets_at = summary
        .billing_cycle_end
        .as_deref()
        .and_then(parse_billing_cycle_end);

    let mut used_percent = None;
    if let Some(ind) = &summary.individual_usage {
        if let Some(plan) = &ind.plan {
            if let Some(pct) = plan.total_percent_used {
                used_percent = Some(pct.clamp(0.0, 100.0));
            } else if let (Some(used), Some(limit)) = (plan.used, plan.limit) {
                if limit > 0.0 {
                    let pct = (used / limit) * 100.0;
                    used_percent = Some(pct.clamp(0.0, 100.0));
                } else {
                    used_percent = Some(0.0);
                }
            }
        }
    }

    match (used_percent, resets_at) {
        (Some(pct), Some(reset)) => Ok(Usage {
            primary: Some(RateWindow {
                used_percent: pct,
                resets_at: Some(reset),
                window_minutes: Some(43200),
            }),
            secondary: None,
            tertiary: None,
            extra_rate_windows: None,
        }),
        (Some(_), None) => Err(FetchError::Decode(
            "cursor: percent present but billingCycleEnd is missing or invalid".to_string(),
        )),
        _ => Err(FetchError::Decode(
            "cursor: no valid usage window found".to_string(),
        )),
    }
}

/// The Cursor usage provider.
pub struct CursorProvider {
    http: reqwest::Client,
}

impl CursorProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for CursorProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
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
                "no cursor session cookie in browser".to_string(),
            ));
        }

        let body_bytes = JsonRequest::get(USAGE_URL)
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", jar.header()))
            .send(&self.http)
            .await?;

        let usage = normalize_usage(&body_bytes)?;
        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_healthy_total_percent_used() {
        let json = r#"{
            "billingCycleEnd": "2026-07-24T03:00:00Z",
            "individualUsage": {
                "plan": {
                    "totalPercentUsed": 45.5
                }
            }
        }"#;
        let usage = normalize_usage(json.as_bytes()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 45.5);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T03:00:00Z"));
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn parses_healthy_used_limit_cents() {
        let json = r#"{
            "billingCycleEnd": "2026-07-24T03:00:00.123Z",
            "individualUsage": {
                "plan": {
                    "used": 2000,
                    "limit": 8000
                }
            }
        }"#;
        let usage = normalize_usage(json.as_bytes()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T03:00:00Z"));
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn percent_without_billing_cycle_end_drops_window() {
        let json = r#"{
            "individualUsage": {
                "plan": {
                    "totalPercentUsed": 45.5
                }
            }
        }"#;
        let res = normalize_usage(json.as_bytes());
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }

    #[test]
    fn missing_usage_window_is_decode_error() {
        let json = r#"{
            "billingCycleEnd": "2026-07-24T03:00:00Z"
        }"#;
        let res = normalize_usage(json.as_bytes());
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }
}
