//! Warp usage fetcher — credential from an environment variable, GraphQL POST.
//!
//! Warp is the POST archetype in this group: a GraphQL query to
//! `app.warp.dev/graphql/v2`, which the edge limiter rejects with HTTP 429 unless
//! the request carries a Warp-client `User-Agent` + `x-warp-client-id` — so this
//! exercises `http::JsonRequest::post_json` plus several custom headers.
//!
//! Credential: `WARP_API_KEY` / `WARP_TOKEN` as `Authorization: Bearer`. Window:
//! the GraphQL `requestLimitInfo` — `requestsUsedSinceLastRefresh / requestLimit`
//! → utilization, `nextRefreshTime` (RFC 3339) → `resetsAt`. `isUnlimited` → 0%
//! used and no reset. No fixed window length is reported (`windowMinutes` omitted).
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `WARP_API_KEY` available. Endpoint, the client-UA/x-warp-client-id/x-warp-os-*
//! headers, the populated osContext, the GraphQL query, and the
//! `data.user.user.requestLimitInfo.{isUnlimited, requestLimit,
//! requestsUsedSinceLastRefresh, nextRefreshTime}` response shape are ported from
//! CodexBar (`Providers/Warp/WarpUsageFetcher.swift:132-205, 259-290` and
//! `WarpUsageFetcher.swift:42-60`). Because this is fixture-only, the request
//! construction reproduces CodexBar's FULL client fingerprint faithfully (we can't
//! live-verify which parts the edge limiter requires, so we drop nothing). Rides
//! the live-proven `http.rs`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "warp";
const API_KEY_ENV: &[&str] = &["WARP_API_KEY", "WARP_TOKEN"];
const API_URL: &str = "https://app.warp.dev/graphql/v2?op=GetRequestLimitInfo";
const CLIENT_ID: &str = "warp-app";
const USER_AGENT: &str = "Warp/1.0";
// Warp's edge limiter 429s a request unless it carries the official client's
// fingerprint: the client id, UA, AND the os-context headers + body fields below.
// We reproduce CodexBar's known-good fingerprint verbatim (it is what passes the
// limiter). The os-category/name match CodexBar's macOS client; the version value
// is not checked for an exact match (presence/shape is), so a static value is
// fine — but the fields must be present.
const OS_CATEGORY: &str = "macOS";
const OS_NAME: &str = "macOS";
const OS_VERSION: &str = "1.0.0";

const GRAPHQL_QUERY: &str = "query GetRequestLimitInfo($requestContext: RequestContext!) { \
user(requestContext: $requestContext) { __typename ... on UserOutput { user { \
requestLimitInfo { isUnlimited nextRefreshTime requestLimit requestsUsedSinceLastRefresh } } } } }";

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: Option<DataField>,
}

#[derive(Debug, Deserialize)]
struct DataField {
    user: Option<UserOutput>,
}

#[derive(Debug, Deserialize)]
struct UserOutput {
    user: Option<InnerUser>,
}

#[derive(Debug, Deserialize)]
struct InnerUser {
    #[serde(rename = "requestLimitInfo")]
    request_limit_info: Option<RequestLimitInfo>,
}

#[derive(Debug, Deserialize)]
struct RequestLimitInfo {
    #[serde(rename = "isUnlimited")]
    is_unlimited: Option<bool>,
    #[serde(rename = "nextRefreshTime")]
    next_refresh_time: Option<String>,
    #[serde(rename = "requestLimit")]
    request_limit: Option<f64>,
    #[serde(rename = "requestsUsedSinceLastRefresh")]
    requests_used: Option<f64>,
}

/// Normalize the GraphQL body to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: GraphQlResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("warp response not decodable: {e}")))?;
    let info = response
        .data
        .and_then(|d| d.user)
        .and_then(|u| u.user)
        .and_then(|u| u.request_limit_info);

    let primary = info.and_then(|info| {
        if info.is_unlimited == Some(true) {
            // Unlimited plan: 0% used, no reset window.
            return None;
        }
        let limit = info.request_limit.filter(|l| *l > 0.0)?;
        let used = info.requests_used.unwrap_or(0.0);
        let resets_at = info.next_refresh_time?;
        Some(RateWindow {
            used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
            resets_at,
            window_minutes: None,
        })
    });

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The Warp usage provider.
pub struct WarpProvider {
    http: reqwest::Client,
}

impl WarpProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for WarpProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for WarpProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let api_key = env::first_env(API_KEY_ENV)
            .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;

        // Reproduce CodexBar's request construction faithfully: a populated
        // osContext in the variables AND the matching x-warp-os-* headers, both
        // part of the client fingerprint the edge limiter checks.
        let body = json!({
            "query": GRAPHQL_QUERY,
            "operationName": "GetRequestLimitInfo",
            "variables": { "requestContext": {
                "clientContext": {},
                "osContext": { "category": OS_CATEGORY, "name": OS_NAME, "version": OS_VERSION },
            } },
        });
        let body = serde_json::to_vec(&body).map_err(|e| FetchError::Decode(e.to_string()))?;

        let response = JsonRequest::post_json(API_URL, body)
            .bearer(&api_key)
            .header(Header::new("x-warp-client-id", CLIENT_ID))
            .header(Header::new("x-warp-os-category", OS_CATEGORY))
            .header(Header::new("x-warp-os-name", OS_NAME))
            .header(Header::new("x-warp-os-version", OS_VERSION))
            .header(Header::new("User-Agent", USER_AGENT))
            .send(&self.http)
            .await?;

        let usage = normalize_usage(&response)?;
        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_request_limit_info() {
        // Shaped like the real GraphQL response.
        let body = br#"{
            "data": { "user": { "__typename": "UserOutput", "user": {
                "requestLimitInfo": {
                    "isUnlimited": false,
                    "nextRefreshTime": "2026-07-01T00:00:00Z",
                    "requestLimit": 2500,
                    "requestsUsedSinceLastRefresh": 500
                }
            } } }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0);
        assert_eq!(primary.resets_at, "2026-07-01T00:00:00Z");
        assert_eq!(primary.window_minutes, None);
    }

    #[test]
    fn unlimited_plan_has_no_window() {
        let body = br#"{ "data": { "user": { "user": { "requestLimitInfo": {
            "isUnlimited": true, "requestLimit": 0, "requestsUsedSinceLastRefresh": 0
        } } } } }"#;
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn missing_user_yields_no_window() {
        let body = br#"{ "data": { "user": null } }"#;
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }
}
