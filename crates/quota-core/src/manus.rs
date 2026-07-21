//! Manus usage fetcher — headless session from environment variables.
//!
//! Connect/JSON-RPC style POST to `GetAvailableCredits` with a Bearer session token.
//! Maps the refilling refresh allotment (not pro-monthly — that window has no reset in
//! source) to a single `primary` [`RateWindow`].
//!
//! Credential (first non-empty wins, faithful to CodexBar `ManusSettingsReader.swift:5-14`):
//! `MANUS_SESSION_TOKEN`, `MANUS_SESSION_ID`, `MANUS_COOKIE` (extracts `session_id` from
//! a cookie header when present). No browser cookie import.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no Manus session
//! was available. Endpoint, headers, `{}` body, envelope parsing, and refresh-window math
//! are ported from CodexBar `Sources/CodexBarCore/Providers/Manus/ManusUsageFetcher.swift`
//! (request `73-104`, response `7-17`, `129-160`, refresh window guard `177-190` in
//! `toUsageSnapshot`). `proMonthlyCredits` is intentionally omitted (`resetsAt: nil` at
//! `165-175`). Rides the shared `http.rs` transport.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "manus";
const SESSION_TOKEN_ENV: &[&str] = &["MANUS_SESSION_TOKEN", "MANUS_SESSION_ID", "MANUS_COOKIE"];
const CREDITS_URL: &str = "https://api.manus.im/user.v1.UserService/GetAvailableCredits";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

const EXPECTED_CREDITS_KEYS: &[&str] = &[
    "totalCredits",
    "freeCredits",
    "periodicCredits",
    "addonCredits",
    "refreshCredits",
    "maxRefreshCredits",
    "proMonthlyCredits",
    "eventCredits",
];

/// Parsed credits payload (fields we normalize).
#[derive(Debug, Default)]
struct ManusCreditsResponse {
    refresh_credits: f64,
    max_refresh_credits: f64,
    next_refresh_time: Option<String>,
    refresh_interval: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ManusCreditsEnvelope {
    data: Option<Value>,
    result: Option<Value>,
    response: Option<Value>,
    #[serde(rename = "availableCredits")]
    available_credits: Option<Value>,
}

/// Resolve Bearer token from env list (first set wins). `MANUS_COOKIE` values extract
/// `session_id` when the value looks like a cookie header (CodexBar `ManusCookieHeader`).
fn resolve_session_token() -> Option<String> {
    for name in SESSION_TOKEN_ENV {
        let raw = env::first_env(&[name])?;
        if let Some(token) = token_from_env_value(&raw) {
            return Some(token);
        }
    }
    None
}

fn cleaned_env(raw: &str) -> Option<String> {
    let mut value = raw.trim().to_string();
    if value.is_empty() {
        return None;
    }
    if (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''))
    {
        value = value[1..value.len() - 1].trim().to_string();
    }
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn token_from_env_value(raw: &str) -> Option<String> {
    let raw = cleaned_env(raw)?;
    if !raw.contains('=') && !raw.contains(';') {
        return Some(raw);
    }
    for part in raw.split(';') {
        let part = part.trim();
        if let Some((name, value)) = part.split_once('=') {
            if name.trim().eq_ignore_ascii_case("session_id") {
                let token = value.trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }
    None
}

fn lossy_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn flexible_date_iso(value: &Value) -> Option<String> {
    if let Some(n) = value.as_f64() {
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
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    None
}

fn object_has_credits_field(obj: &serde_json::Map<String, Value>) -> bool {
    EXPECTED_CREDITS_KEYS
        .iter()
        .any(|key| obj.contains_key(*key))
}

fn parse_credits_object(obj: &serde_json::Map<String, Value>) -> ManusCreditsResponse {
    let get = |key: &str| obj.get(key).and_then(lossy_f64).unwrap_or(0.0);
    ManusCreditsResponse {
        refresh_credits: get("refreshCredits"),
        max_refresh_credits: get("maxRefreshCredits"),
        next_refresh_time: obj.get("nextRefreshTime").and_then(flexible_date_iso),
        refresh_interval: obj
            .get("refreshInterval")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

fn parse_credits_value(value: &Value) -> Option<ManusCreditsResponse> {
    let obj = value.as_object()?;
    if !object_has_credits_field(obj) {
        return None;
    }
    Some(parse_credits_object(obj))
}

/// Decode credits from raw JSON (envelope-first, then direct), faithful to CodexBar
/// `parseResponse`.
fn decode_credits(body: &[u8]) -> Result<ManusCreditsResponse, FetchError> {
    let root: Value = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("manus response not JSON: {e}")))?;

    if let Ok(envelope) = serde_json::from_value::<ManusCreditsEnvelope>(root.clone()) {
        for nested in [
            envelope.data,
            envelope.result,
            envelope.response,
            envelope.available_credits,
        ]
        .into_iter()
        .flatten()
        {
            if let Some(parsed) = parse_credits_value(&nested) {
                return Ok(parsed);
            }
        }
    }

    parse_credits_value(&root)
        .ok_or_else(|| FetchError::Decode("manus: response missing expected credits fields".into()))
}

/// Map refresh allotment to [`Usage`]. Pure — unit-testable.
///
/// Guard from CodexBar `ManusUsageFetcher.swift:177-190`: emit `primary` only when
/// `maxRefreshCredits > 0` and `nextRefreshTime` is present; `used_percent` is
/// `(maxRefreshCredits - refreshCredits) / maxRefreshCredits * 100` clamped 0..100.
/// Skips `proMonthlyCredits` (no reset in source at `165-175`).
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response = decode_credits(body)?;

    // ManusUsageFetcher.swift:177-190 — refilling refresh window only when cap and reset exist.
    let primary = if response.max_refresh_credits > 0.0 {
        let resets_at = match response.next_refresh_time.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return Err(FetchError::Decode("manus: no refilling window".into()));
            }
        };
        let used_percent = ((response.max_refresh_credits - response.refresh_credits)
            / response.max_refresh_credits
            * 100.0)
            .clamp(0.0, 100.0);
        Some(RateWindow {
            used_percent,
            raw_used_percent: None,
            resets_at: Some(resets_at),
            window_minutes: window_minutes_from_refresh_interval(
                response.refresh_interval.as_deref(),
            ),
            used_count: None,
            total_count: None,
        })
    } else {
        return Err(FetchError::Decode("manus: no refilling window".into()));
    };

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// `refreshInterval` is a free-form label in source (`ManusUsageFetcher.swift:223-234`);
/// only map when it matches a known duration token (e.g. `daily` → 1440 minutes).
fn window_minutes_from_refresh_interval(interval: Option<&str>) -> Option<i64> {
    let text = interval?.trim();
    if text.is_empty() {
        return None;
    }
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let suffix_multipliers: &[(&str, f64)] = &[
        ("minutes", 1.0),
        ("minute", 1.0),
        ("mins", 1.0),
        ("min", 1.0),
        ("hours", 60.0),
        ("hour", 60.0),
        ("hrs", 60.0),
        ("hr", 60.0),
        ("days", 24.0 * 60.0),
        ("day", 24.0 * 60.0),
        ("daily", 24.0 * 60.0),
        ("weekly", 7.0 * 24.0 * 60.0),
    ];
    for (suffix, multiplier) in suffix_multipliers {
        if normalized == *suffix {
            return Some((1.0 * multiplier).round() as i64);
        }
        if normalized.ends_with(suffix) {
            let prefix = &normalized[..normalized.len() - suffix.len()];
            if let Ok(value) = prefix.parse::<f64>() {
                if value > 0.0 {
                    return Some((value * multiplier).round() as i64);
                }
            }
        }
    }
    None
}

/// The Manus usage provider.
pub struct ManusProvider {
    http: reqwest::Client,
}

impl ManusProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for ManusProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for ManusProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let token = resolve_session_token().ok_or_else(|| {
                FetchError::NoSession(format!("none of {SESSION_TOKEN_ENV:?} is set"))
            })?;

            let body = JsonRequest::post_json(CREDITS_URL, b"{}".to_vec())
                .bearer(&token)
                .header(Header::new("Origin", "https://manus.im"))
                .header(Header::new("Referer", "https://manus.im/"))
                .header(Header::new("Connect-Protocol-Version", "1"))
                .header(Header::new("User-Agent", USER_AGENT))
                .timeout(REQUEST_TIMEOUT)
                .send(&self.http)
                .await?;

            let usage = normalize_usage(&body)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_refresh_allotment_to_primary() {
        let body = br#"{
            "totalCredits": 120,
            "freeCredits": 20,
            "periodicCredits": 80,
            "addonCredits": 10,
            "refreshCredits": 30,
            "maxRefreshCredits": 300,
            "proMonthlyCredits": 100,
            "eventCredits": 10,
            "nextRefreshTime": "2026-04-13T00:00:00Z",
            "refreshInterval": "daily"
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert!((primary.used_percent - 90.0).abs() < 0.01);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-04-13T00:00:00Z"));
        assert_eq!(primary.window_minutes, Some(1440));
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn remaining_refresh_credits_drive_used_percent() {
        let body = br#"{
            "refreshCredits": 5,
            "maxRefreshCredits": 10,
            "nextRefreshTime": "2026-06-01T12:00:00Z"
        }"#;
        let primary = normalize_usage(body).unwrap().primary.unwrap();
        assert!((primary.used_percent - 50.0).abs() < 0.01);
    }

    #[test]
    fn missing_reset_yields_no_refilling_window_error() {
        let body = br#"{
            "refreshCredits": 1,
            "maxRefreshCredits": 10
        }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Decode(ref m) if m.contains("no refilling window")));
    }

    #[test]
    fn zero_max_refresh_yields_no_refilling_window_error() {
        let body = br#"{
            "refreshCredits": 0,
            "maxRefreshCredits": 0,
            "nextRefreshTime": "2026-04-13T00:00:00Z"
        }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Decode(ref m) if m.contains("no refilling window")));
    }

    #[test]
    fn garbage_payload_is_decode_error() {
        let body = br#"{"error":"unauthorized","message":"session expired"}"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn accepts_wrapped_envelope() {
        let body = br#"{
            "data": {
                "totalCredits": 100,
                "refreshCredits": 5,
                "maxRefreshCredits": 10,
                "nextRefreshTime": "2026-04-13T00:00:00Z"
            }
        }"#;
        let primary = normalize_usage(body).unwrap().primary.unwrap();
        assert!((primary.used_percent - 50.0).abs() < 0.01);
    }

    #[test]
    fn token_from_cookie_header_extracts_session_id() {
        assert_eq!(
            token_from_env_value("foo=bar; session_id=env-cookie-token; baz=qux").as_deref(),
            Some("env-cookie-token")
        );
    }

    #[test]
    fn sparse_live_shaped_payload_matches_codexbar_fixture() {
        let body = br#"{
          "totalCredits": 2869,
          "freeCredits": 1500,
          "periodicCredits": 1369,
          "proMonthlyCredits": 4000,
          "maxRefreshCredits": 300,
          "nextRefreshTime": "2026-04-13T00:00:00Z",
          "refreshInterval": "daily",
          "userFlag": { "drc16": true }
        }"#;
        let primary = normalize_usage(body).unwrap().primary.unwrap();
        assert!((primary.used_percent - 100.0).abs() < 0.01);
    }
}
