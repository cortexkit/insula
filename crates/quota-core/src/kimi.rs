//! Kimi (official coding plan) usage fetcher — credential from environment JWT.
//!
//! POST `BillingService/GetUsages` with Bearer + `kimi-auth` cookie and the
//! browser-style client fingerprint CodexBar sends. Weekly quota → `primary`,
//! first 5h rate limit → `secondary`; a best-effort `GetSubscriptionStats` POST
//! adds the monthly and Code 7-day windows to `extra_rate_windows`.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `KIMI_AUTH_TOKEN` on this machine. Endpoint, headers, body, and response
//! normalization ported from CodexBar (`Providers/Kimi/KimiUsageFetcher.swift:9-13,
//! 78-172, 200-251`, `KimiModels.swift:3-40`,
//! `KimiUsageSnapshot.swift:34-118`, `KimiSettingsReader.swift:4-22`).
//! Reset parsing also cross-checked with
//! OmniRoute `open-sse/services/usage.ts` `parseResetTime` (~1178-1199) and
//! `getKimiUsage` limit `resetTime` fields (~2492-2517). Rides live-proven `http.rs`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ExtraWindow, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "kimi";
const AUTH_TOKEN_ENV: &[&str] = &["KIMI_AUTH_TOKEN", "kimi_auth_token"];
const USAGE_URL: &str =
    "https://www.kimi.com/apiv2/kimi.gateway.billing.v1.BillingService/GetUsages";
const SUBSCRIPTION_STATS_URL: &str =
    "https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStats";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const RATE_LIMIT_WINDOW_MINUTES: i64 = 300;

#[derive(Debug, Deserialize)]
struct KimiUsageResponse {
    usages: Vec<KimiUsage>,
}

#[derive(Debug, Deserialize)]
struct KimiUsage {
    scope: String,
    detail: KimiUsageDetail,
    limits: Option<Vec<KimiRateLimit>>,
}

#[derive(Debug, Deserialize)]
struct KimiUsageDetail {
    limit: String,
    used: Option<String>,
    remaining: Option<String>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KimiRateLimit {
    detail: KimiUsageDetail,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KimiSubscriptionStatsResponse {
    subscription_balance: Option<KimiSubscriptionBalance>,
    ratelimit_code7d: Option<KimiSubscriptionRateLimit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KimiSubscriptionBalance {
    feature: Option<String>,
    #[serde(rename = "type")]
    balance_type: Option<String>,
    amount_used_ratio: Option<f64>,
    expire_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KimiSubscriptionRateLimit {
    ratio: Option<f64>,
    enabled: Option<bool>,
    reset_time: Option<String>,
}

/// Trim env token and strip surrounding quotes (CodexBar `KimiSettingsReader.cleaned`).
fn clean_auth_token(raw: &str) -> Option<String> {
    crate::text::strip_wrapping_quotes(raw)
}

fn resolve_auth_token() -> Option<String> {
    env::first_env(AUTH_TOKEN_ENV).and_then(|v| clean_auth_token(&v))
}

struct SessionInfo {
    device_id: Option<String>,
    session_id: Option<String>,
    traffic_id: Option<String>,
}

/// Decode JWT payload for optional `x-msh-*` headers (CodexBar `decodeSessionInfo`).
fn decode_session_info(jwt: &str) -> Option<SessionInfo> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut payload = parts[1].replace('-', "+").replace('_', "/");
    while !payload.len().is_multiple_of(4) {
        payload.push('=');
    }
    let payload_bytes = base64_decode(&payload)?;
    let json: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    let obj = json.as_object()?;
    Some(SessionInfo {
        device_id: obj
            .get("device_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        session_id: obj.get("ssid").and_then(|v| v.as_str()).map(str::to_string),
        traffic_id: obj.get("sub").and_then(|v| v.as_str()).map(str::to_string),
    })
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const DECODE: [u8; 128] = {
        let mut table = [255u8; 128];
        let mut i = 0u8;
        while i < 26 {
            table[(b'A' + i) as usize] = i;
            table[(b'a' + i) as usize] = i + 26;
            i += 1;
        }
        let mut d = 0u8;
        while d < 10 {
            table[(b'0' + d) as usize] = d + 52;
            d += 1;
        }
        table[b'+' as usize] = 62;
        table[b'/' as usize] = 63;
        table
    };

    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in bytes {
        if b == b'=' {
            break;
        }
        let val = if b < 128 { DECODE[b as usize] } else { 255 };
        if val == 255 {
            continue;
        }
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Parse `resetTime` ISO / epoch (OmniRoute `parseResetTime` + CodexBar ISO8601).
fn parse_reset_time(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(epoch) = trimmed.parse::<f64>() {
        if epoch <= 0.0 {
            return None;
        }
        let secs = if epoch < 1e12 {
            epoch.round() as i64
        } else {
            (epoch / 1000.0).round() as i64
        };
        return env::epoch_to_iso8601(secs);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
    chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.fZ")
        .or_else(|_| chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%SZ"))
        .ok()
        .map(|dt| dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn parse_i64(s: &str) -> Option<i64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse().ok()
}

/// Used count and used-percent from string limit/used/remaining (CodexBar `toUsageSnapshot`).
fn detail_used_percent(detail: &KimiUsageDetail) -> Option<f64> {
    let limit = parse_i64(&detail.limit)?;
    if limit <= 0 {
        return None;
    }
    let used = if let Some(u) = detail.used.as_deref().and_then(parse_i64) {
        u
    } else if let Some(rem) = detail.remaining.as_deref().and_then(parse_i64) {
        (limit - rem).max(0)
    } else {
        0
    };
    let used_clamped = used.clamp(0, limit);
    Some((used_clamped as f64 / limit as f64) * 100.0)
}

fn detail_to_window(detail: &KimiUsageDetail, window_minutes: Option<i64>) -> Option<RateWindow> {
    let used_percent = detail_used_percent(detail)?;
    let resets_at = parse_reset_time(detail.reset_time.as_deref());
    Some(RateWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at,
        window_minutes,
        used_count: None,
        total_count: None,
    })
}

/// Normalize GetUsages JSON to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: KimiUsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("kimi usages not decodable: {e}")))?;

    let coding = response
        .usages
        .into_iter()
        .find(|u| u.scope == "FEATURE_CODING")
        .ok_or_else(|| {
            FetchError::Decode("FEATURE_CODING scope not found in response".to_string())
        })?;

    let primary = detail_to_window(&coding.detail, None);
    let secondary = coding
        .limits
        .as_ref()
        .and_then(|limits| limits.first())
        .and_then(|rl| detail_to_window(&rl.detail, Some(RATE_LIMIT_WINDOW_MINUTES)));

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

fn subscription_balance_to_window(balance: KimiSubscriptionBalance) -> Option<ExtraWindow> {
    let matches_feature = balance
        .feature
        .as_deref()
        .is_none_or(|value| value == "FEATURE_OMNI");
    let matches_type = balance
        .balance_type
        .as_deref()
        .is_none_or(|value| value == "SUBSCRIPTION");
    if !matches_feature || !matches_type {
        return None;
    }

    let ratio = balance
        .amount_used_ratio
        .filter(|ratio| ratio.is_finite())?;
    Some(ExtraWindow {
        title: Some("Monthly".to_string()),
        id: Some("kimi-monthly".to_string()),
        window: Some(RateWindow {
            used_percent: (ratio * 100.0).clamp(0.0, 100.0),
            raw_used_percent: None,
            resets_at: parse_reset_time(balance.expire_time.as_deref()),
            window_minutes: None,
            used_count: None,
            total_count: None,
        }),
    })
}

fn subscription_rate_limit_to_window(limit: KimiSubscriptionRateLimit) -> Option<ExtraWindow> {
    if limit.enabled == Some(false) {
        return None;
    }

    let ratio = limit.ratio.filter(|ratio| ratio.is_finite())?;
    Some(ExtraWindow {
        title: Some("Code 7-day".to_string()),
        id: Some("kimi-code-7d".to_string()),
        window: Some(RateWindow {
            used_percent: (ratio * 100.0).clamp(0.0, 100.0),
            raw_used_percent: None,
            resets_at: parse_reset_time(limit.reset_time.as_deref()),
            window_minutes: Some(7 * 24 * 60),
            used_count: None,
            total_count: None,
        }),
    })
}

/// Normalize GetSubscriptionStats JSON into optional named quota windows.
fn normalize_subscription_stats(body: &[u8]) -> Result<Option<Vec<ExtraWindow>>, FetchError> {
    let response: KimiSubscriptionStatsResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("kimi subscription stats not decodable: {e}")))?;
    let windows: Vec<_> = [
        response
            .subscription_balance
            .and_then(subscription_balance_to_window),
        response
            .ratelimit_code7d
            .and_then(subscription_rate_limit_to_window),
    ]
    .into_iter()
    .flatten()
    .collect();

    Ok((!windows.is_empty()).then_some(windows))
}

fn request_timezone() -> String {
    std::env::var("TZ").unwrap_or_else(|_| "UTC".to_string())
}

/// Build either Kimi POST with the same JWT-derived headers CodexBar uses.
fn web_request(
    url: &'static str,
    body: Vec<u8>,
    auth_token: &str,
    session: Option<&SessionInfo>,
) -> JsonRequest {
    let mut req = JsonRequest::post_json(url, body)
        .bearer(auth_token)
        .header(Header::new("Cookie", format!("kimi-auth={auth_token}")))
        .header(Header::new("Origin", "https://www.kimi.com"))
        .header(Header::new("Referer", "https://www.kimi.com/code/console"))
        .header(Header::new("Accept", "*/*"))
        .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
        .header(Header::new("User-Agent", USER_AGENT))
        .header(Header::new("connect-protocol-version", "1"))
        .header(Header::new("x-language", "en-US"))
        .header(Header::new("x-msh-platform", "web"))
        .header(Header::new("r-timezone", request_timezone()));

    if let Some(device_id) = session.and_then(|value| value.device_id.as_deref()) {
        req = req.header(Header::new("x-msh-device-id", device_id));
    }
    if let Some(session_id) = session.and_then(|value| value.session_id.as_deref()) {
        req = req.header(Header::new("x-msh-session-id", session_id));
    }
    if let Some(traffic_id) = session.and_then(|value| value.traffic_id.as_deref()) {
        req = req.header(Header::new("x-traffic-id", traffic_id));
    }

    req
}

/// The Kimi official usage provider.
pub struct KimiProvider {
    http: reqwest::Client,
}

impl KimiProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
        }
    }
}

impl Default for KimiProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for KimiProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let auth_token = resolve_auth_token().ok_or_else(|| {
                FetchError::NoSession(format!("none of {AUTH_TOKEN_ENV:?} is set"))
            })?;

            let body = serde_json::to_vec(&json!({ "scope": ["FEATURE_CODING"] }))
                .map_err(|e| FetchError::Decode(e.to_string()))?;

            let session = decode_session_info(&auth_token);
            let response = web_request(USAGE_URL, body, &auth_token, session.as_ref())
                .send(&self.http)
                .await?;
            let mut usage = normalize_usage(&response)?;

            // Subscription data is optional enrichment: a failed request or malformed
            // response must not discard the already-normalized GetUsages windows.
            if let Ok(response) = web_request(
                SUBSCRIPTION_STATS_URL,
                b"{}".to_vec(),
                &auth_token,
                session.as_ref(),
            )
            .send(&self.http)
            .await
            {
                if let Ok(extra_rate_windows) = normalize_subscription_stats(&response) {
                    usage.extra_rate_windows = extra_rate_windows;
                }
            }

            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "web", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "usages": [{
                "scope": "FEATURE_CODING",
                "detail": {
                    "limit": "100",
                    "used": "25",
                    "remaining": "75",
                    "resetTime": "2026-07-01T12:00:00Z"
                },
                "limits": [{
                    "window": { "duration": 5, "timeUnit": "HOUR" },
                    "detail": {
                        "limit": "50",
                        "used": null,
                        "remaining": "40",
                        "resetTime": "2026-06-22T18:00:00Z"
                    }
                }]
            }]
        }))
        .unwrap()
    }

    fn subscription_fixture(enabled: bool) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "ratelimitCode5h": {
                "ratio": 0.4689,
                "enabled": true,
                "resetTime": "2026-07-02T11:56:36.876796734Z"
            },
            "ratelimitCode7d": {
                "ratio": 0.0946,
                "enabled": enabled,
                "resetTime": "2026-07-09T06:56:36.876796734Z"
            },
            "subscriptionBalance": {
                "id": "19eee1de-9092-8315-8000-0000e4e34d79",
                "feature": "FEATURE_OMNI",
                "type": "SUBSCRIPTION",
                "unit": "UNIT_CREDIT",
                "amountUsedRatio": 1.0,
                "kimiCodeUsedRatio": 0.2854,
                "expireTime": "2026-07-23T00:00:00Z"
            }
        }))
        .unwrap()
    }

    #[test]
    fn normalizes_weekly_and_rate_limit_windows() {
        let usage = normalize_usage(&fixture_body()).unwrap();
        let primary = usage.primary.as_ref().unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-01T12:00:00Z"));
        assert_eq!(primary.window_minutes, None);

        let secondary = usage.secondary.as_ref().unwrap();
        assert_eq!(secondary.used_percent, 20.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-06-22T18:00:00Z"));
        assert_eq!(secondary.window_minutes, Some(300));
    }

    #[test]
    fn normalizes_subscription_stats_into_two_extra_windows() {
        let windows = normalize_subscription_stats(&subscription_fixture(true))
            .unwrap()
            .unwrap();
        assert_eq!(windows.len(), 2);

        let monthly = &windows[0];
        assert_eq!(monthly.id.as_deref(), Some("kimi-monthly"));
        assert_eq!(monthly.title.as_deref(), Some("Monthly"));
        let monthly_window = monthly.window.as_ref().unwrap();
        assert_eq!(monthly_window.used_percent, 100.0);
        assert_eq!(
            monthly_window.resets_at.as_deref(),
            Some("2026-07-23T00:00:00Z")
        );
        assert_eq!(monthly_window.window_minutes, None);

        let code_weekly = &windows[1];
        assert_eq!(code_weekly.id.as_deref(), Some("kimi-code-7d"));
        assert_eq!(code_weekly.title.as_deref(), Some("Code 7-day"));
        let code_weekly_window = code_weekly.window.as_ref().unwrap();
        assert_eq!(code_weekly_window.used_percent, 9.46);
        assert_eq!(
            code_weekly_window.resets_at.as_deref(),
            Some("2026-07-09T06:56:36Z")
        );
        assert_eq!(code_weekly_window.window_minutes, Some(10_080));
    }

    #[test]
    fn omits_disabled_subscription_code_window() {
        let windows = normalize_subscription_stats(&subscription_fixture(false))
            .unwrap()
            .unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id.as_deref(), Some("kimi-monthly"));
        assert!(windows
            .iter()
            .all(|window| window.id.as_deref() != Some("kimi-code-7d")));
    }

    #[test]
    fn remaining_derives_used_when_used_absent() {
        let body = br#"{
            "usages": [{
                "scope": "FEATURE_CODING",
                "detail": {
                    "limit": "10",
                    "remaining": "3",
                    "resetTime": "2026-06-23T00:00:00Z"
                },
                "limits": []
            }]
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 70.0);
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn missing_reset_keeps_primary_window() {
        let body = br#"{
            "usages": [{
                "scope": "FEATURE_CODING",
                "detail": { "limit": "10", "used": "1" },
                "limits": [{
                    "detail": { "limit": "5", "used": "1", "resetTime": "2026-06-23T00:00:00Z" }
                }]
            }]
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 10.0);
        assert_eq!(primary.resets_at, None);
        assert!(usage.secondary.is_some());
    }

    #[test]
    fn exhausted_usage_without_reset_is_kept() {
        let body = br#"{
            "usages": [{
                "scope": "FEATURE_CODING",
                "detail": { "limit": "10", "used": "10" },
                "limits": []
            }]
        }"#;
        let primary = normalize_usage(body)
            .unwrap()
            .primary
            .expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn garbage_body_is_decode_error() {
        let err = normalize_usage(b"not json").unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn missing_feature_coding_scope_is_decode_error() {
        let body = br#"{ "usages": [{ "scope": "OTHER", "detail": { "limit": "1" } }] }"#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn clean_auth_token_strips_quotes() {
        assert_eq!(clean_auth_token("\"tok\"").as_deref(), Some("tok"));
        assert_eq!(clean_auth_token("  'x'  ").as_deref(), Some("x"));
    }

    #[test]
    fn parse_reset_time_accepts_epoch_seconds() {
        assert_eq!(
            parse_reset_time(Some("1782135879")).as_deref(),
            Some("2026-06-22T13:44:39Z")
        );
    }
}
