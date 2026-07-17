//! Synthetic usage fetcher — credential from an environment variable.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! SYNTHETIC_API_KEY available. Endpoint, headers, and response shape ported from
//! CodexBar (SyntheticUsageStats.swift:95,102-106,189-201,242-292,306-321,624-643,672-698
//! and SyntheticSettingsReader.swift:4,7). Rides the live-proven http.rs.

use async_trait::async_trait;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::JsonRequest,
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "synthetic";
const API_KEY_ENV: &[&str] = &["SYNTHETIC_API_KEY"];
const BASE_URL_ENV: &[&str] = &["SYNTHETIC_API_URL"];
const DEFAULT_BASE: &str = "https://api.synthetic.new/v2/quotas";

const PERCENT_USED_KEYS: &[&str] = &[
    "percentUsed",
    "usedPercent",
    "usagePercent",
    "usage_percent",
    "used_percent",
    "percent_used",
    "percent",
];
const PERCENT_REMAINING_KEYS: &[&str] = &[
    "percentRemaining",
    "remainingPercent",
    "remaining_percent",
    "percent_remaining",
];
const LIMIT_KEYS: &[&str] = &[
    "limit",
    "messageLimit",
    "message_limit",
    "messages",
    "maxRequests",
    "max_requests",
    "requestLimit",
    "request_limit",
    "quota",
    "max",
    "total",
    "capacity",
    "allowance",
];
const USED_KEYS: &[&str] = &[
    "used",
    "usage",
    "usedMessages",
    "used_messages",
    "messagesUsed",
    "messages_used",
    "requests",
    "requestCount",
    "request_count",
    "consumed",
    "spent",
];
const REMAINING_KEYS: &[&str] = &["remaining", "left", "available", "balance"];
const RESET_KEYS: &[&str] = &[
    "resetAt",
    "reset_at",
    "resetsAt",
    "resets_at",
    "renewAt",
    "renew_at",
    "renewsAt",
    "renews_at",
    "nextTickAt",
    "next_tick_at",
    "nextRegenAt",
    "next_regen_at",
    "periodEnd",
    "period_end",
    "expiresAt",
    "expires_at",
    "endAt",
    "end_at",
];
const WINDOW_MINUTES_KEYS: &[&str] = &[
    "windowMinutes",
    "window_minutes",
    "periodMinutes",
    "period_minutes",
];
const WINDOW_HOURS_KEYS: &[&str] = &["windowHours", "window_hours", "periodHours", "period_hours"];
const WINDOW_DAYS_KEYS: &[&str] = &["windowDays", "window_days", "periodDays", "period_days"];
const WINDOW_SECONDS_KEYS: &[&str] = &[
    "windowSeconds",
    "window_seconds",
    "periodSeconds",
    "period_seconds",
];
const WINDOW_STRING_KEYS: &[&str] = &[
    "window",
    "windowLabel",
    "window_label",
    "period",
    "periodLabel",
    "period_label",
];

fn first_double(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(val) = map.get(*key) {
            if let Some(n) = val.as_f64() {
                return Some(n);
            }
            if let Some(s) = val.as_str() {
                if let Ok(n) = s.trim().parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn first_int(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<i64> {
    for key in keys {
        if let Some(val) = map.get(*key) {
            if let Some(n) = val.as_i64() {
                return Some(n);
            }
            if let Some(s) = val.as_str() {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn first_string(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(val) = map.get(*key) {
            if let Some(s) = val.as_str() {
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

fn normalized_percent(val: Option<f64>) -> Option<f64> {
    let v = val?;
    if v <= 1.0 {
        Some(v * 100.0)
    } else {
        Some(v)
    }
}

fn first_date(map: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(val) = map.get(*key) {
            if let Some(date_str) = parse_date_value(val) {
                return Some(date_str);
            }
        }
    }
    None
}

fn parse_date_value(val: &serde_json::Value) -> Option<String> {
    if let Some(n) = val.as_f64() {
        return epoch_to_iso8601_f64(n);
    }
    if let Some(s) = val.as_str() {
        let trimmed = s.trim();
        if let Ok(n) = trimmed.parse::<f64>() {
            return epoch_to_iso8601_f64(n);
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
            return Some(
                dt.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string(),
            );
        }
    }
    None
}

fn epoch_to_iso8601_f64(n: f64) -> Option<String> {
    let secs = if n > 1_000_000_000_000.0 {
        (n / 1000.0) as i64
    } else if n > 1_000_000_000.0 {
        n as i64
    } else {
        return None;
    };
    env::epoch_to_iso8601(secs)
}

fn window_minutes(map: &serde_json::Map<String, serde_json::Value>) -> Option<i64> {
    if let Some(minutes) = first_int(map, WINDOW_MINUTES_KEYS) {
        return Some(minutes);
    }
    if let Some(hours) = first_double(map, WINDOW_HOURS_KEYS) {
        return Some((hours * 60.0).round() as i64);
    }
    if let Some(days) = first_double(map, WINDOW_DAYS_KEYS) {
        return Some((days * 24.0 * 60.0).round() as i64);
    }
    if let Some(seconds) = first_double(map, WINDOW_SECONDS_KEYS) {
        return Some((seconds / 60.0).round() as i64);
    }
    if let Some(text) = first_string(map, WINDOW_STRING_KEYS) {
        return window_minutes_from_text(&text);
    }
    None
}

fn window_minutes_from_text(text: &str) -> Option<i64> {
    let normalized: String = text
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if normalized.is_empty() {
        return None;
    }

    let mut suffix_multipliers = vec![
        ("minutes", 1.0),
        ("minute", 1.0),
        ("mins", 1.0),
        ("min", 1.0),
        ("m", 1.0),
        ("hours", 60.0),
        ("hour", 60.0),
        ("hrs", 60.0),
        ("hr", 60.0),
        ("h", 60.0),
        ("days", 24.0 * 60.0),
        ("day", 24.0 * 60.0),
        ("d", 24.0 * 60.0),
    ];
    suffix_multipliers.sort_by_key(|(suffix, _)| std::cmp::Reverse(suffix.len()));

    for (suffix, multiplier) in suffix_multipliers {
        if normalized.ends_with(suffix) {
            let value_text = &normalized[..normalized.len() - suffix.len()];
            if let Ok(value) = value_text.parse::<f64>() {
                if value > 0.0 {
                    return Some((value * multiplier).round() as i64);
                }
            }
        }
    }
    None
}

fn is_quota_payload(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    let checks = &[
        LIMIT_KEYS,
        USED_KEYS,
        REMAINING_KEYS,
        PERCENT_USED_KEYS,
        PERCENT_REMAINING_KEYS,
    ];
    checks.iter().any(|keys| first_double(map, keys).is_some())
}

fn parse_quota(map: &serde_json::Map<String, serde_json::Value>) -> Option<RateWindow> {
    let percent_used = normalized_percent(first_double(map, PERCENT_USED_KEYS));
    let percent_remaining = normalized_percent(first_double(map, PERCENT_REMAINING_KEYS));

    let mut used_percent = percent_used;
    if used_percent.is_none() {
        if let Some(remaining) = percent_remaining {
            used_percent = Some(100.0 - remaining);
        }
    }

    if used_percent.is_none() {
        let mut limit = first_double(map, LIMIT_KEYS);
        let mut used = first_double(map, USED_KEYS);
        let remaining = first_double(map, REMAINING_KEYS);

        if limit.is_none() {
            if let (Some(u), Some(r)) = (used, remaining) {
                limit = Some(u + r);
            }
        }
        if used.is_none() {
            if let (Some(l), Some(r)) = (limit, remaining) {
                used = Some(l - r);
            }
        }

        if let (Some(l), Some(u)) = (limit, used) {
            if l > 0.0 {
                used_percent = Some((u / l) * 100.0);
            }
        }
    }

    let used_percent = used_percent?;
    let clamped = used_percent.clamp(0.0, 100.0);

    let resets_at = first_date(map, RESET_KEYS)?;
    let window_minutes = window_minutes(map);

    Some(RateWindow {
        used_percent: clamped,
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes,
    })
}

fn named_quota(val: &serde_json::Value, _label: &str) -> Option<RateWindow> {
    let map = val.as_object()?;
    if is_quota_payload(map) {
        parse_quota(map)
    } else {
        None
    }
}

fn prioritized_quota_slots(
    root: &serde_json::Map<String, serde_json::Value>,
) -> Option<Vec<Option<RateWindow>>> {
    let data_dict = root.get("data").and_then(|v| v.as_object());

    let rolling = root
        .get("rollingFiveHourLimit")
        .or_else(|| data_dict.and_then(|d| d.get("rollingFiveHourLimit")))
        .and_then(|v| named_quota(v, "Rolling five-hour limit"));

    let weekly = root
        .get("weeklyTokenLimit")
        .or_else(|| data_dict.and_then(|d| d.get("weeklyTokenLimit")))
        .and_then(|v| named_quota(v, "Weekly token limit"));

    let search_hourly = root
        .get("search")
        .and_then(|v| v.as_object())
        .and_then(|s| s.get("hourly"))
        .or_else(|| {
            data_dict.and_then(|d| {
                d.get("search")
                    .and_then(|v| v.as_object())
                    .and_then(|s| s.get("hourly"))
            })
        })
        .and_then(|v| named_quota(v, "Search hourly"));

    if rolling.is_some() || weekly.is_some() || search_hourly.is_some() {
        Some(vec![rolling, weekly, search_hourly])
    } else {
        None
    }
}

fn fallback_quota_objects(
    root: &serde_json::Map<String, serde_json::Value>,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let data_dict = root.get("data").and_then(|v| v.as_object());

    let keys = &[
        "quotas",
        "quota",
        "limits",
        "usage",
        "entries",
        "subscription",
        "data",
    ];

    for key in keys {
        if let Some(candidate) = root.get(*key) {
            let extracted = extract_quota_objects(candidate);
            if !extracted.is_empty() {
                return extracted;
            }
        }
    }

    if let Some(data) = data_dict {
        for key in keys {
            if let Some(candidate) = data.get(*key) {
                let extracted = extract_quota_objects(candidate);
                if !extracted.is_empty() {
                    return extracted;
                }
            }
        }
    }

    Vec::new()
}

fn extract_quota_objects(
    val: &serde_json::Value,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    let mut results = Vec::new();
    match val {
        serde_json::Value::Array(arr) => {
            for item in arr {
                if let Some(map) = item.as_object() {
                    if is_quota_payload(map) {
                        results.push(map.clone());
                    } else {
                        results.extend(extract_quota_objects(item));
                    }
                } else {
                    results.extend(extract_quota_objects(item));
                }
            }
        }
        serde_json::Value::Object(map) => {
            if is_quota_payload(map) {
                results.push(map.clone());
            } else {
                let mut sorted_keys: Vec<&String> = map.keys().collect();
                sorted_keys.sort();
                for key in sorted_keys {
                    if let Some(child) = map.get(key) {
                        results.extend(extract_quota_objects(child));
                    }
                }
            }
        }
        _ => {}
    }
    results
}

pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let json_val: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("synthetic response not decodable: {e}")))?;

    let root = match json_val {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Array(arr) => {
            let mut map = serde_json::Map::new();
            map.insert("quotas".to_string(), serde_json::Value::Array(arr));
            map
        }
        _ => {
            return Err(FetchError::Decode(
                "synthetic response is neither object nor array".to_string(),
            ))
        }
    };

    if let Some(slots) = prioritized_quota_slots(&root) {
        let primary = slots.first().cloned().flatten();
        let secondary = slots.get(1).cloned().flatten();
        let tertiary = slots.get(2).cloned().flatten();
        return Ok(Usage {
            primary,
            secondary,
            tertiary,
            extra_rate_windows: None,
        });
    }

    let fallback_objects = fallback_quota_objects(&root);
    let mut quotas = Vec::new();
    for obj in fallback_objects {
        if let Some(window) = parse_quota(&obj) {
            quotas.push(window);
        }
    }

    let primary = quotas.first().cloned();
    let secondary = quotas.get(1).cloned();
    let tertiary = quotas.get(2).cloned();

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

pub struct SyntheticProvider {
    http: reqwest::Client,
}

impl SyntheticProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for SyntheticProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for SyntheticProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;
            let base = env::first_env(BASE_URL_ENV).unwrap_or_else(|| DEFAULT_BASE.to_string());

            let body = JsonRequest::get(base)
                .bearer(&api_key)
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
    fn test_normalize_object_form() {
        let body = br#"{
            "rollingFiveHourLimit": {
                "used": 20,
                "limit": 100,
                "resetAt": "2026-06-22T15:00:00Z",
                "window": "5hr"
            },
            "weeklyTokenLimit": {
                "usedPercent": 45.0,
                "resetAt": "2026-06-22T16:00:00Z",
                "window": "7 days"
            },
            "search": {
                "hourly": {
                    "remaining": 80,
                    "limit": 100,
                    "resetAt": "2026-06-22T17:00:00Z",
                    "window": "60 mins"
                }
            }
        }"#;

        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 20.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T15:00:00Z"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 45.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-06-22T16:00:00Z"));
        assert_eq!(secondary.window_minutes, Some(10080));

        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 20.0); // remaining 80 -> used 20
        assert_eq!(tertiary.resets_at.as_deref(), Some("2026-06-22T17:00:00Z"));
        assert_eq!(tertiary.window_minutes, Some(60));
    }

    #[test]
    fn test_normalize_array_form() {
        let body = br#"[
            {
                "used": 10,
                "limit": 100,
                "resetAt": "2026-06-22T15:00:00Z",
                "window": "5hr"
            },
            {
                "usedPercent": 30.0,
                "resetAt": "2026-06-22T16:00:00Z",
                "window": "7 days"
            }
        ]"#;

        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 10.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-06-22T15:00:00Z"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 30.0);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-06-22T16:00:00Z"));
        assert_eq!(secondary.window_minutes, Some(10080));

        assert!(usage.tertiary.is_none());
    }

    #[test]
    fn test_duration_string_parse() {
        assert_eq!(window_minutes_from_text("5hr"), Some(300));
        assert_eq!(window_minutes_from_text("30min"), Some(30));
        assert_eq!(window_minutes_from_text("2 days"), Some(2880));
        assert_eq!(window_minutes_from_text("1.5 hours"), Some(90));
        assert_eq!(window_minutes_from_text("10m"), Some(10));
        assert_eq!(window_minutes_from_text("invalid"), None);
    }

    #[test]
    fn test_missing_reset_drops_window() {
        let body = br#"{
            "rollingFiveHourLimit": {
                "used": 20,
                "limit": 100,
                "window": "5hr"
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
    }
}
