//! Alibaba Coding Plan usage fetcher — API key path only.
//!
//! POST `https://<gateway>/data/api.json` with query params for
//! `queryCodingPlanInstanceInfoV2`, reproducing CodexBar's API-mode client
//! fingerprint: `Authorization: Bearer`, `x-api-key`, and `X-DashScope-API-Key`
//! (all three carry the same key). Windows: 5-hour → primary (300 min), weekly →
//! secondary (10080 min), monthly → tertiary (43200 min); utilization is
//! `used/total*100` and resets come from the provider's next-refresh fields.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `ALIBABA_CODING_PLAN_API_KEY` available. Endpoint, headers, POST body, and
//! response shape (including stringified-JSON expansion) are ported from CodexBar
//! (`Providers/Alibaba/AlibabaCodingPlanUsageFetcher.swift:92-123, 172-178,
//! 245-294, 442-560, 698-715, 857-877, 948-971` and
//! `AlibabaCodingPlanSettingsReader.swift:4-11`, `AlibabaCodingPlanAPIRegion.swift:18-76,
//! 116-131`). Rides the live-proven `http.rs`.

use async_trait::async_trait;
use chrono::{NaiveDateTime, TimeZone, Utc};
use serde_json::{json, Value};

use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "alibaba";
const API_KEY_ENV: &[&str] = &[
    "ALIBABA_CODING_PLAN_API_KEY",
    "ALIBABA_QWEN_API_KEY",
    "DASHSCOPE_API_KEY",
];
const QUOTA_URL_ENV: &[&str] = &["ALIBABA_CODING_PLAN_QUOTA_URL"];
const HOST_ENV: &[&str] = &["ALIBABA_CODING_PLAN_HOST"];

const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

const INTL_GATEWAY: &str = "https://modelstudio.console.alibabacloud.com";
const CN_GATEWAY: &str = "https://bailian.console.aliyun.com";
const INTL_DASHBOARD: &str =
    "https://modelstudio.console.alibabacloud.com/ap-southeast-1/?tab=coding-plan#/efm/coding_plan";
const CN_DASHBOARD: &str =
    "https://bailian.console.aliyun.com/cn-beijing/?tab=model#/efm/coding_plan";

struct RegionConfig {
    gateway: &'static str,
    dashboard: &'static str,
    commodity_code: &'static str,
    current_region_id: &'static str,
}

const INTL_REGION: RegionConfig = RegionConfig {
    gateway: INTL_GATEWAY,
    dashboard: INTL_DASHBOARD,
    commodity_code: "sfm_codingplan_public_intl",
    current_region_id: "ap-southeast-1",
};

const CN_REGION: RegionConfig = RegionConfig {
    gateway: CN_GATEWAY,
    dashboard: CN_DASHBOARD,
    commodity_code: "sfm_codingplan_public_cn",
    current_region_id: "cn-beijing",
};

/// Recursively expand string values that contain embedded JSON objects/arrays.
fn expanded_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let expanded: serde_json::Map<String, Value> = map
                .into_iter()
                .map(|(k, v)| (k, expanded_json(v)))
                .collect();
            Value::Object(expanded)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(expanded_json).collect()),
        Value::String(s) => {
            if let Ok(nested) = serde_json::from_str::<Value>(&s) {
                if nested.is_object() || nested.is_array() {
                    return expanded_json(nested);
                }
            }
            Value::String(s)
        }
        other => other,
    }
}

fn parse_int(raw: &Value) -> Option<i64> {
    match raw {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_date_iso(raw: &Value) -> Option<String> {
    if let Some(int_value) = parse_int(raw) {
        let secs = if int_value > 1_000_000_000_000 {
            int_value / 1000
        } else if int_value > 1_000_000_000 {
            int_value
        } else {
            return None;
        };
        return env::epoch_to_iso8601(secs);
    }
    let s = raw.as_str()?.trim();
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
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Utc
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        }
    }
    None
}

fn any_int(keys: &[&str], dict: &serde_json::Map<String, Value>) -> Option<i64> {
    keys.iter()
        .find_map(|key| dict.get(*key).and_then(parse_int))
}

fn any_date(keys: &[&str], dict: &serde_json::Map<String, Value>) -> Option<String> {
    keys.iter()
        .find_map(|key| dict.get(*key).and_then(parse_date_iso))
}

fn find_first_dictionary<'a>(
    keys: &[&str],
    value: &'a Value,
) -> Option<&'a serde_json::Map<String, Value>> {
    if let Some(map) = value.as_object() {
        for key in keys {
            if let Some(Value::Object(nested)) = map.get(*key) {
                return Some(nested);
            }
        }
        for nested in map.values() {
            if let Some(found) = find_first_dictionary(keys, nested) {
                return Some(found);
            }
        }
    } else if let Some(array) = value.as_array() {
        for item in array {
            if let Some(found) = find_first_dictionary(keys, item) {
                return Some(found);
            }
        }
    }
    None
}

fn find_quota_info(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    if let Some(direct) =
        find_first_dictionary(&["codingPlanQuotaInfo", "coding_plan_quota_info"], value)
    {
        return Some(direct);
    }
    find_dictionary_matching_quota_keys(value)
}

fn find_dictionary_matching_quota_keys(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    const QUOTA_KEYS: &[&str] = &[
        "per5HourUsedQuota",
        "per5HourTotalQuota",
        "perWeekUsedQuota",
        "perWeekTotalQuota",
        "perBillMonthUsedQuota",
        "perBillMonthTotalQuota",
    ];
    if let Some(map) = value.as_object() {
        if QUOTA_KEYS.iter().any(|k| map.contains_key(*k)) {
            return Some(map);
        }
        for nested in map.values() {
            if let Some(found) = find_dictionary_matching_quota_keys(nested) {
                return Some(found);
            }
        }
    } else if let Some(array) = value.as_array() {
        for item in array {
            if let Some(found) = find_dictionary_matching_quota_keys(item) {
                return Some(found);
            }
        }
    }
    None
}

fn window_from_used_total_reset(
    used_keys: &[&str],
    total_keys: &[&str],
    reset_keys: &[&str],
    window_minutes: i64,
    quota: &serde_json::Map<String, Value>,
) -> Option<RateWindow> {
    let used = any_int(used_keys, quota)?;
    let total = any_int(total_keys, quota)?;
    if total <= 0 {
        return None;
    }
    let resets_at = any_date(reset_keys, quota)?;
    Some(RateWindow {
        used_percent: (used as f64 / total as f64 * 100.0).clamp(0.0, 100.0),
        resets_at: Some(resets_at),
        window_minutes: Some(window_minutes),
    })
}

/// Normalize the Coding Plan API body to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    if body.is_empty() {
        return Err(FetchError::Decode(
            "alibaba empty response body".to_string(),
        ));
    }
    let root: Value = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("alibaba response not decodable: {e}")))?;
    let expanded = expanded_json(root);
    if expanded.as_object().is_none() {
        return Err(FetchError::Decode(
            "alibaba unexpected payload (not an object)".to_string(),
        ));
    }

    let quota = find_quota_info(&expanded)
        .ok_or_else(|| FetchError::Decode("alibaba missing coding plan quota data".to_string()))?;

    let primary = window_from_used_total_reset(
        &["per5HourUsedQuota", "perFiveHourUsedQuota"],
        &["per5HourTotalQuota", "perFiveHourTotalQuota"],
        &[
            "per5HourQuotaNextRefreshTime",
            "perFiveHourQuotaNextRefreshTime",
        ],
        300,
        quota,
    );
    let secondary = window_from_used_total_reset(
        &["perWeekUsedQuota"],
        &["perWeekTotalQuota"],
        &["perWeekQuotaNextRefreshTime"],
        10_080,
        quota,
    );
    let tertiary = window_from_used_total_reset(
        &["perBillMonthUsedQuota", "perMonthUsedQuota"],
        &["perBillMonthTotalQuota", "perMonthTotalQuota"],
        &[
            "perBillMonthQuotaNextRefreshTime",
            "perMonthQuotaNextRefreshTime",
        ],
        43_200,
        quota,
    );

    if primary.is_none() && secondary.is_none() && tertiary.is_none() {
        return Err(FetchError::Decode(
            "alibaba no quota windows found in payload".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

fn cleaned_host(raw: &str) -> Option<String> {
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

fn quota_url_from_host(host: &str, region: &RegionConfig) -> Option<String> {
    let cleaned = cleaned_host(host)?;
    let base = if cleaned.starts_with("http://") || cleaned.starts_with("https://") {
        cleaned
    } else {
        format!("https://{cleaned}")
    };
    let mut url = url::Url::parse(&base).ok()?;
    url.set_path("/data/api.json");
    url.query_pairs_mut()
        .clear()
        .append_pair(
            "action",
            "zeldaEasy.broadscope-bailian.codingPlan.queryCodingPlanInstanceInfoV2",
        )
        .append_pair("product", "broadscope-bailian")
        .append_pair("api", "queryCodingPlanInstanceInfoV2")
        .append_pair("currentRegionId", region.current_region_id);
    Some(url.to_string())
}

fn resolve_quota_url(region: &RegionConfig) -> String {
    if let Some(raw) = env::first_env(QUOTA_URL_ENV) {
        if let Ok(url) = url::Url::parse(&raw) {
            if !url.scheme().is_empty() {
                return url.to_string();
            }
        }
        if let Ok(url) = url::Url::parse(&format!("https://{raw}")) {
            return url.to_string();
        }
    }
    if let Some(host) = env::first_env(HOST_ENV).and_then(|h| quota_url_from_host(&h, region)) {
        return host;
    }
    quota_url_from_host(region.gateway, region).expect("static gateway URL")
}

fn request_body(region: &RegionConfig) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "queryCodingPlanInstanceInfoRequest": {
            "commodityCode": region.commodity_code,
        }
    }))
    .unwrap_or_else(|_| br#"{"queryCodingPlanInstanceInfoRequest":{"commodityCode":""}}"#.to_vec())
}

fn should_retry_alternate_region(err: &FetchError) -> bool {
    match err {
        FetchError::Unauthorized(_) => true,
        FetchError::Upstream(msg) => msg.contains("HTTP 404") || msg.contains("HTTP 403"),
        FetchError::Decode(msg) => {
            msg.contains("missing coding plan quota data") || msg.contains("no quota windows found")
        }
        FetchError::NoSession(_) => false,
    }
}

async fn fetch_once(
    http: &reqwest::Client,
    api_key: &str,
    region: &RegionConfig,
) -> Result<Vec<u8>, FetchError> {
    let url = resolve_quota_url(region);
    let body = request_body(region);
    JsonRequest::post_json(url, body)
        .bearer(api_key)
        .header(Header::new("x-api-key", api_key))
        .header(Header::new("X-DashScope-API-Key", api_key))
        .header(Header::new("User-Agent", BROWSER_USER_AGENT))
        .header(Header::new("Origin", region.gateway))
        .header(Header::new("Referer", region.dashboard))
        .send(http)
        .await
}

/// The Alibaba Coding Plan usage provider (API key path).
pub struct AlibabaProvider {
    http: reqwest::Client,
}

impl AlibabaProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for AlibabaProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for AlibabaProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch(&self) -> Result<ProviderUsage, FetchError> {
        let api_key = env::first_env(API_KEY_ENV)
            .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;

        match fetch_once(&self.http, &api_key, &INTL_REGION).await {
            Ok(body) => {
                let usage = normalize_usage(&body)?;
                return Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage));
            }
            Err(err) if should_retry_alternate_region(&err) => {}
            Err(err) => return Err(err),
        }

        let body = fetch_once(&self.http, &api_key, &CN_REGION).await?;
        let usage = normalize_usage(&body)?;
        Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_quota() -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([
            ("per5HourUsedQuota".to_string(), Value::Number(25.into())),
            ("per5HourTotalQuota".to_string(), Value::Number(100.into())),
            (
                "per5HourQuotaNextRefreshTime".to_string(),
                Value::Number(1_782_135_879.into()),
            ),
            ("perWeekUsedQuota".to_string(), Value::Number(10.into())),
            ("perWeekTotalQuota".to_string(), Value::Number(50.into())),
            (
                "perWeekQuotaNextRefreshTime".to_string(),
                Value::String("2026-07-01T00:00:00Z".into()),
            ),
            (
                "perBillMonthUsedQuota".to_string(),
                Value::Number(200.into()),
            ),
            (
                "perBillMonthTotalQuota".to_string(),
                Value::Number(1000.into()),
            ),
            (
                "perBillMonthQuotaNextRefreshTime".to_string(),
                Value::String("2026-08-01 00:00:00".into()),
            ),
        ])
    }

    #[test]
    fn normalizes_three_windows() {
        let payload = json!({
            "codingPlanInstanceInfos": [{
                "status": "VALID",
                "codingPlanQuotaInfo": sample_quota()
            }]
        });
        let usage = normalize_usage(payload.to_string().as_bytes()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T13:44:39Z"));
        assert_eq!(primary.window_minutes, Some(300));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 20.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-01T00:00:00Z"));
        assert_eq!(secondary.window_minutes, Some(10_080));
        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 20.0);
        assert_eq!(tertiary.window_minutes, Some(43_200));
    }

    #[test]
    fn expands_stringified_json_quota_blob() {
        let inner = json!({ "codingPlanQuotaInfo": sample_quota() });
        let payload = json!({
            "data": serde_json::to_string(&inner).unwrap()
        });
        let usage = normalize_usage(payload.to_string().as_bytes()).unwrap();
        assert!(usage.primary.is_some());
        assert!(usage.secondary.is_some());
        assert!(usage.tertiary.is_some());
    }

    #[test]
    fn missing_reset_drops_that_window() {
        let mut quota = sample_quota();
        quota.remove("per5HourQuotaNextRefreshTime");
        let payload = json!({ "codingPlanQuotaInfo": quota });
        let usage = normalize_usage(payload.to_string().as_bytes()).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_some());
    }

    #[test]
    fn zero_total_drops_window() {
        let mut quota = sample_quota();
        quota.insert("perWeekTotalQuota".to_string(), Value::Number(0.into()));
        let payload = json!({ "codingPlanQuotaInfo": quota });
        let usage = normalize_usage(payload.to_string().as_bytes()).unwrap();
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn intl_quota_url_has_expected_query() {
        let url = quota_url_from_host(INTL_GATEWAY, &INTL_REGION).unwrap();
        assert!(url.contains("/data/api.json"));
        assert!(url.contains("queryCodingPlanInstanceInfoV2"));
        assert!(url.contains("ap-southeast-1"));
    }
}
