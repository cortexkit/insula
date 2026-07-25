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
use serde_json::{json, Value};

use crate::provider::{CredentialHandle, FetchAttempt};
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
const MAX_GRAPHQL_ERROR_MESSAGES: usize = 3;
const MAX_GRAPHQL_ERROR_SUMMARY_BYTES: usize = 512;

const GRAPHQL_QUERY: &str = "query GetRequestLimitInfo($requestContext: RequestContext!) { \
user(requestContext: $requestContext) { __typename ... on UserOutput { user { \
requestLimitInfo { isUnlimited nextRefreshTime requestLimit requestsUsedSinceLastRefresh } } } } }";

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    data: Option<DataField>,
    errors: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
struct DataField {
    user: Option<UserOutput>,
}

#[derive(Debug, Deserialize)]
struct UserOutput {
    #[serde(rename = "__typename")]
    type_name: Option<String>,
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

fn graph_ql_error_message(value: &Value) -> Option<&str> {
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
        .map(str::trim)
        .filter(|message| !message.is_empty())
}

fn graph_ql_error_summary(errors: &[Value]) -> String {
    let mut summary = errors
        .iter()
        .filter_map(graph_ql_error_message)
        .take(MAX_GRAPHQL_ERROR_MESSAGES)
        .collect::<Vec<_>>()
        .join(" | ");
    if summary.is_empty() {
        summary = "GraphQL request failed".to_string();
    }
    if summary.len() > MAX_GRAPHQL_ERROR_SUMMARY_BYTES {
        let budget = MAX_GRAPHQL_ERROR_SUMMARY_BYTES.saturating_sub('…'.len_utf8());
        let end = crate::text::floor_char_boundary(&summary, budget);
        summary.truncate(end);
        summary.push('…');
    }
    summary
}

/// Normalize the GraphQL body to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: GraphQlResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("warp response not decodable: {e}")))?;

    // HTTP 200 GraphQL errors and missing required links are real provider answers,
    // not transport flaps. Classify them as Decode so recurring failures degrade
    // with a visible reason; only an endpoint that returned no body is transient.
    if let Some(errors) = response
        .errors
        .as_deref()
        .filter(|errors| !errors.is_empty())
    {
        return Err(FetchError::Decode(format!(
            "warp GraphQL request failed: {}",
            graph_ql_error_summary(errors)
        )));
    }

    let data = response
        .data
        .ok_or_else(|| FetchError::Decode("warp response missing data".to_string()))?;
    let user = data
        .user
        .ok_or_else(|| FetchError::Decode("warp response missing data.user".to_string()))?;
    let type_name = user
        .type_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let inner_user = user.user.ok_or_else(|| {
        let type_detail = type_name
            .filter(|name| *name != "UserOutput")
            .map(|name| format!("; unexpected data.user.__typename {name:?}"))
            .unwrap_or_default();
        FetchError::Decode(format!("warp response missing data.user.user{type_detail}"))
    })?;
    let info = inner_user.request_limit_info.ok_or_else(|| {
        FetchError::Decode("warp response missing data.user.user.requestLimitInfo".to_string())
    })?;

    let primary = if info.is_unlimited == Some(true) {
        // Unlimited plan: 0% used, no reset window.
        Some(RateWindow {
            used_percent: 0.0,
            raw_used_percent: None,
            resets_at: None,
            window_minutes: None,
            used_count: None,
            total_count: None,
        })
    } else {
        // The percent is the load-bearing field: a window is emitted from the
        // limit alone, and the reset is carried through when present and omitted
        // when absent rather than being fabricated. A metered plan that reports
        // its limit but not its next refresh still has real usage to report, and
        // dropping the window would make a used-up plan read as no signal.
        info.request_limit
            .filter(|limit| *limit > 0.0)
            .map(|limit| {
                let used = info.requests_used.unwrap_or(0.0);
                RateWindow {
                    used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
                    raw_used_percent: None,
                    resets_at: info.next_refresh_time,
                    window_minutes: None,
                    used_count: None,
                    total_count: None,
                }
            })
    };

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

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
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
            let body =
                serde_json::to_vec(&body).map_err(|e| FetchError::Decode(e.to_string()))?;

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

    #[test]
    fn well_formed_metered_response_remains_unchanged() {
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
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(primary.window_minutes, None);
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn unlimited_plan_emits_zero_percent_window_without_reset() {
        let body = br#"{ "data": { "user": { "user": { "requestLimitInfo": {
            "isUnlimited": true, "requestLimit": 0, "requestsUsedSinceLastRefresh": 0
        } } } } }"#;
        let primary = normalize_usage(body).unwrap().primary.unwrap();
        assert_eq!(primary.used_percent, 0.0);
        assert!(primary.resets_at.is_none());
    }

    #[test]
    fn missing_data_user_is_a_decode_error() {
        let body = br#"{ "data": { "user": null } }"#;
        let error = normalize_usage(body).unwrap_err();
        match error {
            FetchError::Decode(message) => assert!(
                message.contains("data.user"),
                "missing-link error was opaque: {message}"
            ),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn metered_window_without_a_refresh_time_still_reports_its_percent() {
        // The percent is load-bearing and the reset is optional: a plan that
        // reports its limit but not its next refresh still has real usage. The
        // dangerous direction is a used-up plan vanishing, so this pins the 100%
        // case specifically.
        let body = br#"{
            "data": { "user": { "__typename": "UserOutput", "user": {
              "requestLimitInfo": {
                "isUnlimited": false,
                "requestLimit": 250,
                "requestsUsedSinceLastRefresh": 250
              }
            } } }
        }"#;
        let primary = normalize_usage(body)
            .expect("a limit without a reset is still a usable window")
            .primary
            .expect("window must survive an absent refresh time");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(
            primary.resets_at, None,
            "an absent reset is carried as absent, never fabricated"
        );
    }

    #[test]
    fn graphql_errors_are_decode_errors_with_provider_message() {
        let body = br#"{
            "errors": [{"message": "failed to resolve requestLimitInfo"}],
            "data": null
        }"#;
        let error = normalize_usage(body).unwrap_err();
        match error {
            FetchError::Decode(message) => assert!(
                message.contains("failed to resolve requestLimitInfo"),
                "provider error text was lost: {message}"
            ),
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn every_missing_required_graphql_link_is_named() {
        let cases: &[(&[u8], &str)] = &[
            (br#"{}"#, "data"),
            (
                br#"{ "data": { "user": { "__typename": "UserOutput", "user": null } } }"#,
                "data.user.user",
            ),
            (
                br#"{ "data": { "user": { "user": {} } } }"#,
                "data.user.user.requestLimitInfo",
            ),
        ];

        for (body, missing_link) in cases {
            let error = normalize_usage(body).unwrap_err();
            match error {
                FetchError::Decode(message) => assert!(
                    message.contains(missing_link),
                    "missing-link error did not name {missing_link}: {message}"
                ),
                other => panic!("expected Decode for {missing_link}, got {other:?}"),
            }
        }
    }

    #[test]
    fn graphql_error_summary_is_bounded_and_caps_message_count() {
        let four_messages = serde_json::to_vec(&json!({
            "errors": [
                {"message": "first"},
                {"message": "second"},
                {"message": "third"},
                {"message": "fourth"}
            ],
            "data": null
        }))
        .unwrap();
        let FetchError::Decode(message) = normalize_usage(&four_messages).unwrap_err() else {
            panic!("expected Decode");
        };
        assert!(message.contains("first | second | third"));
        assert!(!message.contains("fourth"));

        let long_message = "é".repeat(MAX_GRAPHQL_ERROR_SUMMARY_BYTES);
        let oversized = serde_json::to_vec(&json!({
            "errors": [{"message": long_message}],
            "data": null
        }))
        .unwrap();
        let FetchError::Decode(message) = normalize_usage(&oversized).unwrap_err() else {
            panic!("expected Decode");
        };
        assert!(message.len() <= MAX_GRAPHQL_ERROR_SUMMARY_BYTES + 40);
        assert!(message.ends_with('…'));
    }
}
