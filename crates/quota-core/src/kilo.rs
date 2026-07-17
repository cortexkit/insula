//! Kilo usage fetcher — credential from an environment variable (optional CLI auth file).
//!
//! Kilo reports prepaid **credits** (micro-USD `creditBlocks`) and an optional **Kilo Pass**
//! subscription (`kiloPass.getState`). Credits have utilization but no billing reset; this
//! module maps only the subscription window (real `nextBillingAt` / renewal fields) to
//! `primary`. Credits-only accounts therefore emit no window, which is intentional.
//!
//! Endpoint: tRPC batch GET
//! `https://app.kilo.ai/api/trpc/user.getCreditBlocks,kiloPass.getState,user.getAutoTopUpPaymentMethod`
//! with `batch=1` and indexed `input` JSON (`{"0":{"json":null},...}`). Auth:
//! `Authorization: Bearer`, `Accept: application/json`, 15s timeout. Optional
//! `X-KILOCODE-ORGANIZATIONID` when `KILO_ORGANIZATION_ID` is set (org scope).
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! `KILO_API_KEY` available. Endpoint, headers, batch query shape, and response parsing
//! are ported from CodexBar (`Providers/Kilo/KiloUsageFetcher.swift:247-316, 472-539,
//! 705-720, 943-961, 1144-1177`; env key from `KiloSettingsReader.swift:4, 24-36`).
//! The batch still calls `user.getCreditBlocks` (faithful request) but pass-only parsing.
//! Rides the live-proven `http.rs`.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "kilo";
const API_KEY_ENV: &[&str] = &["KILO_API_KEY"];
const ORG_ID_ENV: &[&str] = &["KILO_ORGANIZATION_ID"];
const DEFAULT_TRPC_BASE: &str = "https://app.kilo.ai/api/trpc";

const PROCEDURES: [&str; 3] = [
    "user.getCreditBlocks",
    "kiloPass.getState",
    "user.getAutoTopUpPaymentMethod",
];

const OPTIONAL_PROCEDURES: [&str; 1] = ["user.getAutoTopUpPaymentMethod"];

pub fn batch_url(base: &str) -> Result<String, FetchError> {
    let trimmed = base.trim().trim_end_matches('/');
    let path_base = if trimmed.is_empty() {
        DEFAULT_TRPC_BASE
    } else {
        trimmed
    };
    let joined = PROCEDURES.join(",");
    let input_map: Value = json!({
        "0": { "json": null },
        "1": { "json": null },
        "2": { "json": null },
    });
    let input_string = serde_json::to_string(&input_map)
        .map_err(|e| FetchError::Decode(format!("kilo batch input not encodable: {e}")))?;
    Ok(format!(
        "{path_base}/{joined}?batch=1&input={}",
        percent_encode_query_component(&input_string)
    ))
}

fn percent_encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let root: Value = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("kilo batch not valid JSON: {e}")))?;

    let entries = response_entries_by_index(&root)?;
    let mut payloads: [Option<Value>; 3] = [None, None, None];

    for (index, procedure) in PROCEDURES.iter().enumerate() {
        let Some(entry) = entries.get(&index) else {
            continue;
        };
        if let Some(err) = trpc_error(entry) {
            if is_required_procedure(procedure) {
                return Err(err);
            }
            continue;
        }
        if let Some(payload) = result_payload(entry) {
            payloads[index] = Some(payload);
        }
    }

    let pass = pass_fields(payloads[1].as_ref());
    let primary = pass_window(&pass);

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

#[derive(Debug, Default)]
struct PassFields {
    used: Option<f64>,
    total: Option<f64>,
    remaining: Option<f64>,
    resets_at: Option<String>,
}

fn is_required_procedure(procedure: &str) -> bool {
    !OPTIONAL_PROCEDURES.contains(&procedure)
}

fn response_entries_by_index(root: &Value) -> Result<BTreeMap<usize, Value>, FetchError> {
    if let Some(arr) = root.as_array() {
        let mut map = BTreeMap::new();
        for (offset, entry) in arr.iter().take(PROCEDURES.len()).enumerate() {
            map.insert(offset, entry.clone());
        }
        return Ok(map);
    }

    if let Some(obj) = root.as_object() {
        if obj.contains_key("result") || obj.contains_key("error") {
            let mut map = BTreeMap::new();
            map.insert(0, root.clone());
            return Ok(map);
        }

        let mut map = BTreeMap::new();
        for (key, value) in obj {
            let Ok(index) = key.parse::<usize>() else {
                continue;
            };
            if index < PROCEDURES.len() {
                map.insert(index, value.clone());
            }
        }
        if !map.is_empty() {
            return Ok(map);
        }
    }

    Err(FetchError::Decode(
        "kilo tRPC batch has unexpected top-level shape".to_string(),
    ))
}

fn trpc_error(entry: &Value) -> Option<FetchError> {
    let error_obj = entry.get("error")?;
    let combined = [
        string_at_path(error_obj, &["json", "data", "code"]),
        string_at_path(error_obj, &["data", "code"]),
        error_obj
            .get("code")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        string_at_path(error_obj, &["json", "message"]),
        error_obj
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    ]
    .into_iter()
    .flatten()
    .map(|s| s.to_lowercase())
    .collect::<Vec<_>>()
    .join(" ");

    if combined.contains("unauthorized") || combined.contains("forbidden") {
        return Some(FetchError::Unauthorized(
            "kilo tRPC unauthorized/forbidden".to_string(),
        ));
    }
    if combined.contains("not_found") || combined.contains("not found") {
        return Some(FetchError::Upstream(
            "kilo tRPC endpoint not found".to_string(),
        ));
    }
    Some(FetchError::Decode("kilo tRPC error payload".to_string()))
}

fn result_payload(entry: &Value) -> Option<Value> {
    let result = entry.get("result")?;
    let data = result.get("data")?;
    if let Some(json_payload) = data.get("json") {
        if json_payload.is_null() {
            return None;
        }
        return Some(json_payload.clone());
    }
    Some(data.clone())
}

fn pass_fields(payload: Option<&Value>) -> PassFields {
    let Some(payload) = payload else {
        return PassFields::default();
    };

    if let Some(subscription) = subscription_data(payload) {
        let used = as_f64(subscription.get("currentPeriodUsageUsd")).map(|v| v.max(0.0));
        let base = as_f64(subscription.get("currentPeriodBaseCreditsUsd")).map(|v| v.max(0.0));
        let bonus = as_f64(subscription.get("currentPeriodBonusCreditsUsd"))
            .unwrap_or(0.0)
            .max(0.0);
        let total = base.map(|b| b + bonus);
        let remaining = match (total, used) {
            (Some(t), Some(u)) => Some((t - u).max(0.0)),
            _ => None,
        };
        let resets_at = ["nextBillingAt", "nextRenewalAt", "renewsAt", "renewAt"]
            .iter()
            .find_map(|key| parse_reset_time(subscription.get(*key)));

        return PassFields {
            used,
            total,
            remaining,
            resets_at,
        };
    }

    fallback_pass_fields(payload)
}

fn subscription_data(payload: &Value) -> Option<Value> {
    let obj = payload.as_object()?;
    if let Some(sub) = obj.get("subscription") {
        if sub.is_null() {
            return None;
        }
        if let Some(map) = sub.as_object() {
            return Some(Value::Object(map.clone()));
        }
    }

    let has_shape = obj.contains_key("currentPeriodUsageUsd")
        || obj.contains_key("currentPeriodBaseCreditsUsd")
        || obj.contains_key("currentPeriodBonusCreditsUsd")
        || obj.contains_key("tier");
    has_shape.then(|| payload.clone())
}

fn fallback_pass_fields(payload: &Value) -> PassFields {
    let contexts = dictionary_contexts(payload);
    if contexts.is_empty() {
        return PassFields::default();
    }

    let total = money_amount(
        &contexts,
        &["amount_mUsd", "total_mUsd", "planAmount_mUsd", "limit_mUsd"],
        &["amount", "total", "limit", "creditsTotal"],
    );
    let used = money_amount(
        &contexts,
        &["used_mUsd", "spent_mUsd", "consumed_mUsd"],
        &["used", "spent", "consumed", "creditsUsed"],
    );
    let remaining = money_amount(
        &contexts,
        &["remaining_mUsd", "available_mUsd", "balance_mUsd"],
        &["remaining", "available", "balance", "creditsRemaining"],
    );

    let (total, used, remaining) = resolve_money_triplet(total, used, remaining);
    let resets_at = [
        "resetAt",
        "resetsAt",
        "nextResetAt",
        "renewAt",
        "renewsAt",
        "nextRenewalAt",
        "currentPeriodEnd",
        "periodEndsAt",
        "expiresAt",
        "expiryAt",
    ]
    .iter()
    .find_map(|key| first_date_in_contexts(&contexts, key));

    PassFields {
        used,
        total,
        remaining,
        resets_at,
    }
}

fn pass_window(pass: &PassFields) -> Option<RateWindow> {
    let total = pass.total?;
    let used = pass
        .used
        .unwrap_or_else(|| pass.remaining.map(|r| (total - r).max(0.0)).unwrap_or(0.0));
    let resets_at = pass.resets_at.clone()?;
    let used_percent = if total > 0.0 {
        (used / total * 100.0).clamp(0.0, 100.0)
    } else {
        100.0
    };
    Some(RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: None,
    })
}

fn resolve_money_triplet(
    total: Option<f64>,
    used: Option<f64>,
    remaining: Option<f64>,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut total = total;
    let mut used = used;
    let mut remaining = remaining;
    if total.is_none() {
        if let (Some(u), Some(r)) = (used, remaining) {
            total = Some(u + r);
        }
    }
    if used.is_none() {
        if let (Some(t), Some(r)) = (total, remaining) {
            used = Some((t - r).max(0.0));
        }
    }
    if remaining.is_none() {
        if let (Some(t), Some(u)) = (total, used) {
            remaining = Some((t - u).max(0.0));
        }
    }
    (total, used, remaining)
}

fn dictionary_contexts(payload: &Value) -> Vec<Value> {
    let mut contexts = Vec::new();
    let mut queue: Vec<(Value, u8)> = vec![(payload.clone(), 0)];
    while let Some((current, depth)) = queue.pop() {
        if current.as_object().is_some() {
            contexts.push(current.clone());
        }
        if depth >= 2 {
            continue;
        }
        let Some(obj) = current.as_object() else {
            continue;
        };
        for value in obj.values() {
            if value.is_object() {
                queue.push((value.clone(), depth + 1));
            } else if let Some(arr) = value.as_array() {
                for item in arr {
                    if item.is_object() {
                        queue.push((item.clone(), depth + 1));
                    }
                }
            }
        }
    }
    contexts
}

fn money_amount(contexts: &[Value], milli_usd_keys: &[&str], plain_keys: &[&str]) -> Option<f64> {
    if let Some(v) = first_f64_in_contexts(contexts, milli_usd_keys) {
        return Some(v / 1_000_000.0);
    }
    first_f64_in_contexts(contexts, plain_keys)
}

fn first_f64_in_contexts(contexts: &[Value], keys: &[&str]) -> Option<f64> {
    for ctx in contexts {
        let Some(obj) = ctx.as_object() else {
            continue;
        };
        for key in keys {
            if let Some(v) = as_f64(obj.get(*key)) {
                return Some(v);
            }
        }
    }
    None
}

fn first_date_in_contexts(contexts: &[Value], key: &str) -> Option<String> {
    for ctx in contexts {
        let Some(obj) = ctx.as_object() else {
            continue;
        };
        if let Some(parsed) = parse_reset_time(obj.get(key)) {
            return Some(parsed);
        }
    }
    None
}

fn as_f64(value: Option<&Value>) -> Option<f64> {
    let v = value?;
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn string_at_path(value: &Value, path: &[&str]) -> Option<String> {
    let mut cur = value;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str().map(str::to_string)
}

fn parse_reset_time(raw: Option<&Value>) -> Option<String> {
    let v = raw?;
    match v {
        Value::Number(n) => epoch_to_rfc3339(n.as_f64()?),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(epoch) = trimmed.parse::<f64>() {
                if let Some(iso) = epoch_to_rfc3339(epoch) {
                    return Some(iso);
                }
            }
            chrono::DateTime::parse_from_rfc3339(trimmed)
                .ok()
                .map(|dt| dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
        }
        _ => None,
    }
}

fn epoch_to_rfc3339(value: f64) -> Option<String> {
    let seconds = if value.abs() > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    };
    env::epoch_to_iso8601(seconds.round() as i64)
}

fn kilo_auth_file_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/kilo/auth.json"))
}

fn read_auth_file_token() -> Option<String> {
    let path = kilo_auth_file_path()?;
    let data = std::fs::read(&path).ok()?;
    let root: Value = serde_json::from_slice(&data).ok()?;
    let access = root.get("kilo")?.get("access")?.as_str()?.trim();
    if access.is_empty() {
        None
    } else {
        Some(access.to_string())
    }
}

fn resolve_api_key() -> Result<String, FetchError> {
    if let Some(key) = env::first_env(API_KEY_ENV) {
        return Ok(key);
    }
    read_auth_file_token().ok_or_else(|| {
        FetchError::NoSession(format!(
            "none of {API_KEY_ENV:?} is set and no ~/.local/share/kilo/auth.json token"
        ))
    })
}

pub struct KiloProvider {
    http: reqwest::Client,
}

impl KiloProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for KiloProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for KiloProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = resolve_api_key()?;
            let url = batch_url(DEFAULT_TRPC_BASE)?;

            let mut request = JsonRequest::get(url)
                .bearer(&api_key)
                .timeout(Duration::from_secs(15));

            if let Some(org_id) = env::first_env(ORG_ID_ENV) {
                request = request.header(Header::new("X-KILOCODE-ORGANIZATIONID", org_id));
            }

            let body = request.send(&self.http).await?;
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
    fn batch_url_matches_codexbar_shape() {
        let url = batch_url("https://app.kilo.ai/api/trpc").unwrap();
        assert!(
            url.contains("user.getCreditBlocks,kiloPass.getState,user.getAutoTopUpPaymentMethod")
        );
        assert!(url.contains("batch=1"));
        assert!(url.contains("input="));
        assert!(url.contains("%22json%22"));
    }

    #[test]
    fn maps_kilo_pass_subscription_to_primary() {
        let body = br#"
        [
          {
            "result": {
              "data": {
                "creditBlocks": [
                  { "balance_mUsd": 19000000, "amount_mUsd": 19000000 }
                ],
                "totalBalance_mUsd": 19000000
              }
            }
          },
          {
            "result": {
              "data": {
                "subscription": {
                  "tier": "tier_19",
                  "currentPeriodUsageUsd": 0,
                  "currentPeriodBaseCreditsUsd": 19.0,
                  "currentPeriodBonusCreditsUsd": 9.5,
                  "nextBillingAt": "2026-03-28T04:00:00.000Z"
                }
              }
            }
          },
          { "result": { "data": { "enabled": false } } }
        ]
        "#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.expect("pass window with reset");
        assert_eq!(primary.used_percent, 0.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-03-28T04:00:00Z"));
        assert_eq!(primary.window_minutes, None);
    }

    #[test]
    fn credits_only_without_subscription_emits_no_window() {
        let body = br#"
        [
          { "result": { "data": { "creditBlocks": [], "totalBalance_mUsd": 0 } } },
          { "result": { "data": { "subscription": null } } },
          { "result": { "data": { "enabled": false } } }
        ]
        "#;
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn pass_without_reset_is_dropped() {
        let body = br#"
        [
          { "result": { "data": { "creditBlocks": [] } } },
          {
            "result": {
              "data": {
                "subscription": {
                  "currentPeriodUsageUsd": 1.0,
                  "currentPeriodBaseCreditsUsd": 19.0
                }
              }
            }
          },
          { "result": { "data": {} } }
        ]
        "#;
        assert!(normalize_usage(body).unwrap().primary.is_none());
    }

    #[test]
    fn fallback_pass_micro_usd_and_next_renewal() {
        let body = br#"
        [
          { "result": { "data": { "json": { "blocks": [] } } } },
          {
            "result": {
              "data": {
                "json": {
                  "amount_mUsd": 28500000,
                  "used_mUsd": 3500000,
                  "nextRenewalAt": "2026-03-28T04:00:00.000Z"
                }
              }
            }
          },
          { "result": { "data": { "json": {} } } }
        ]
        "#;
        let primary = normalize_usage(body).unwrap().primary.unwrap();
        let expected_used = (3.5 / 28.5) * 100.0;
        assert!((primary.used_percent - expected_used).abs() < 0.01);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-03-28T04:00:00Z"));
    }

    #[test]
    fn optional_auto_top_up_trpc_error_is_ignored() {
        let body = br#"
        [
          { "result": { "data": { "json": {} } } },
          {
            "result": {
              "data": {
                "json": {
                  "subscription": {
                    "currentPeriodUsageUsd": 5.0,
                    "currentPeriodBaseCreditsUsd": 10.0,
                    "nextBillingAt": "2026-06-01T00:00:00Z"
                  }
                }
              }
            }
          },
          {
            "error": {
              "json": {
                "message": "Internal server error",
                "data": { "code": "INTERNAL_SERVER_ERROR" }
              }
            }
          }
        ]
        "#;
        assert!(normalize_usage(body).unwrap().primary.is_some());
    }

    #[test]
    fn required_procedure_trpc_unauthorized_fails() {
        let body = br#"
        [
          { "result": { "data": { "json": {} } } },
          {
            "error": {
              "json": {
                "message": "Unauthorized",
                "data": { "code": "UNAUTHORIZED" }
              }
            }
          }
        ]
        "#;
        let err = normalize_usage(body).unwrap_err();
        assert!(matches!(err, FetchError::Unauthorized(_)));
    }
}
