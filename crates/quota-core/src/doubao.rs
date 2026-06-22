//! Doubao (Volcengine Ark) usage fetcher — credential from environment variables.
//!
//! A minimal `POST` chat-completions probe returns rate-limit windows in **response
//! headers** (`x-ratelimit-remaining-requests`, `x-ratelimit-limit-requests`,
//! `x-ratelimit-reset-requests`), not in the JSON body.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `ARK_API_KEY` / `VOLCENGINE_API_KEY` / `DOUBAO_API_KEY` available. Endpoint,
//! probe body, Bearer auth, header parsing, and reset-time formats are ported from
//! CodexBar (`DoubaoUsageFetcher.swift:89-182`, reset parser `216-247`;
//! `DoubaoSettingsReader.swift:4-25` for env keys). HTTP accepts 200 and 429 like
//! CodexBar (`141-146`); shared `http::JsonRequest::send_full` only maps 2xx, so the
//! probe uses the same headers/timeout/body via `reqwest` directly.
//!
//! Window mapping: request-rate pool → `primary` only (`DoubaoUsageSnapshot.toUsageSnapshot`
//! `29-63`); `window_minutes` omitted (CodexBar passes `nil`).

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::{
    env,
    http::Header,
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "doubao";
const API_KEY_ENV: &[&str] = &["ARK_API_KEY", "VOLCENGINE_API_KEY", "DOUBAO_API_KEY"];
const API_URL: &str = "https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Models to probe, ordered by likelihood (CodexBar `DoubaoUsageFetcher.swift:93-97`).
const PROBE_MODELS: &[&str] = &[
    "doubao-seed-2.0-code",
    "doubao-1.5-pro-32k",
    "doubao-lite-32k",
];

/// Header snapshot from a successful probe (200 or 429), for normalization tests.
#[derive(Debug, Clone, PartialEq)]
pub struct DoubaoHeaderSnapshot {
    pub remaining_requests: Option<i64>,
    pub limit_requests: Option<i64>,
    pub reset_requests: Option<String>,
}

/// Normalize rate-limit headers to [`Usage`]. Pure — unit-testable.
///
/// Requires `x-ratelimit-reset-requests` (CodexBar `151-153`); without it the
/// response has no window. `used_percent` follows CodexBar `33-35` when
/// `limit_requests > 0`; otherwise `0` when a reset is present (key valid, no limit
/// header).
pub fn normalize_usage(headers: &DoubaoHeaderSnapshot) -> Result<Usage, FetchError> {
    let reset_raw = headers
        .reset_requests
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FetchError::Decode("no rate-limit headers".to_string()))?;

    let resets_at = parse_reset_time(reset_raw)
        .ok_or_else(|| FetchError::Decode(format!("unparseable x-ratelimit-reset-requests: {reset_raw}")))?;

    let (remaining, limit) = (
        headers.remaining_requests.unwrap_or(0),
        headers.limit_requests.unwrap_or(0),
    );

    let used_percent = if limit > 0 {
        let used = (limit - remaining).max(0);
        ((used as f64 / limit as f64) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    let primary = Some(RateWindow {
        used_percent,
        resets_at,
        window_minutes: None,
    });

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// Parse `x-ratelimit-reset-requests` — ISO8601, `1d2h3m4s` components, or bare
/// seconds until reset. Mirrors CodexBar `DoubaoUsageFetcher.parseResetTime`
/// (`216-247`); duration-to-instant logic parallels `synthetic.rs` unit suffix
/// handling (`211-249`) but uses compact `d/h/m/s` tokens per the Ark header format.
fn parse_reset_time(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Some(dt.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(dt.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }

    let mut seconds: f64 = 0.0;
    let mut pos = 0;
    let bytes = trimmed.as_bytes();
    while pos < bytes.len() {
        let start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == start {
            break;
        }
        if pos >= bytes.len() {
            break;
        }
        let num: f64 = trimmed[start..pos].parse().ok()?;
        let unit = bytes[pos] as char;
        pos += 1;
        seconds += match unit {
            'd' => num * 86400.0,
            'h' => num * 3600.0,
            'm' => num * 60.0,
            's' => num,
            _ => return None,
        };
    }
    if seconds > 0.0 {
        let reset = Utc::now() + chrono::Duration::milliseconds((seconds * 1000.0).round() as i64);
        return Some(reset.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }

    if let Ok(secs) = trimmed.parse::<f64>() {
        if secs > 0.0 {
            let reset = Utc::now() + chrono::Duration::milliseconds((secs * 1000.0).round() as i64);
            return Some(reset.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
    }

    None
}

fn int_header(headers: &[(String, String)], name: &str) -> Option<i64> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.trim().parse().ok())
}

fn string_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn probe_body(model: &str) -> Vec<u8> {
    serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "hi" }]
    })
    .to_string()
    .into_bytes()
}

async fn probe_model(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
) -> Result<Vec<(String, String)>, FetchError> {
    let body = probe_body(model);
    let mut builder = client.post(API_URL).timeout(PROBE_TIMEOUT).body(body);
    for header in [
        Header::new("Accept", "application/json"),
        Header::new("Content-Type", "application/json"),
        Header::bearer(api_key),
    ] {
        builder = builder.header(header.name, header.value);
    }

    let response = builder
        .send()
        .await
        .map_err(|e| FetchError::Upstream(e.to_string()))?;

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body_bytes = response
        .bytes()
        .await
        .map_err(|e| FetchError::Upstream(format!("reading body: {e}")))?
        .to_vec();

    if status == 401 {
        return Err(FetchError::Unauthorized(format!("HTTP {status}")));
    }
    if status != 200 && status != 429 {
        let excerpt: String = String::from_utf8_lossy(&body_bytes).chars().take(200).collect();
        return Err(FetchError::Upstream(format!("HTTP {status}: {excerpt}")));
    }

    Ok(headers)
}

fn headers_to_snapshot(headers: &[(String, String)]) -> DoubaoHeaderSnapshot {
    DoubaoHeaderSnapshot {
        remaining_requests: int_header(headers, "x-ratelimit-remaining-requests"),
        limit_requests: int_header(headers, "x-ratelimit-limit-requests"),
        reset_requests: string_header(headers, "x-ratelimit-reset-requests"),
    }
}

pub struct DoubaoProvider {
    http: reqwest::Client,
}

impl DoubaoProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for DoubaoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for DoubaoProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let api_key = env::first_env(API_KEY_ENV)
            .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;

        let mut last_error: Option<FetchError> = None;
        for model in PROBE_MODELS {
            match probe_model(&self.http, &api_key, model).await {
                Ok(headers) => {
                    let snapshot = headers_to_snapshot(&headers);
                    let usage = normalize_usage(&snapshot)?;
                    return Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage));
                }
                Err(err) => {
                    if let FetchError::Upstream(ref msg) = err {
                        if msg.starts_with("HTTP 404") || msg.starts_with("HTTP 403") {
                            last_error = Some(err);
                            continue;
                        }
                    }
                    return Err(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            FetchError::Upstream("all probe models failed".to_string())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_window_from_remaining_and_limit() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(80),
            limit_requests: Some(100),
            reset_requests: Some("2026-06-22T18:00:00Z".to_string()),
        };
        let usage = normalize_usage(&headers).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0);
        assert_eq!(primary.resets_at, "2026-06-22T18:00:00Z");
        assert_eq!(primary.window_minutes, None);
    }

    #[test]
    fn remaining_to_used_conversion() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(0),
            limit_requests: Some(50),
            reset_requests: Some("2026-06-22T12:00:00Z".to_string()),
        };
        let usage = normalize_usage(&headers).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 100.0);
    }

    #[test]
    fn missing_reset_header_errors() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(10),
            limit_requests: Some(100),
            reset_requests: None,
        };
        let err = normalize_usage(&headers).unwrap_err();
        assert!(matches!(err, FetchError::Decode(ref m) if m == "no rate-limit headers"));
    }

    #[test]
    fn garbage_reset_errors() {
        let headers = DoubaoHeaderSnapshot {
            remaining_requests: Some(1),
            limit_requests: Some(10),
            reset_requests: Some("not-a-date-or-duration".to_string()),
        };
        assert!(normalize_usage(&headers).is_err());
    }

    #[test]
    fn parse_reset_iso8601() {
        assert_eq!(
            parse_reset_time("2026-06-22T15:30:00Z").as_deref(),
            Some("2026-06-22T15:30:00Z")
        );
    }

    #[test]
    fn parse_reset_duration_components() {
        let parsed = parse_reset_time("1h30m").expect("duration parse");
        let dt = DateTime::parse_from_rfc3339(&parsed).unwrap();
        let now = Utc::now();
        let delta = dt.with_timezone(&Utc) - now;
        assert!(delta.num_minutes() >= 89 && delta.num_minutes() <= 91);
    }

    #[test]
    fn parse_reset_bare_seconds() {
        let parsed = parse_reset_time("120").expect("seconds parse");
        let dt = DateTime::parse_from_rfc3339(&parsed).unwrap();
        let delta = dt.with_timezone(&Utc) - Utc::now();
        assert!(delta.num_seconds() >= 115 && delta.num_seconds() <= 125);
    }

    #[test]
    fn probe_body_matches_codexbar() {
        let body = probe_body("doubao-seed-2.0-code");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "doubao-seed-2.0-code");
        assert_eq!(json["max_tokens"], 1);
        assert_eq!(json["messages"][0]["content"], "hi");
    }
}