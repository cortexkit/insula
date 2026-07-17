//! MiniMax coding-plan usage — API key from environment variables.
//!
//! Credential: `MINIMAX_CODING_API_KEY` then `MINIMAX_API_KEY` (`env::first_env`).
//! Request: `GET {apiHost}/v1/api/openplatform/coding_plan/remains` with
//! `Authorization: Bearer`, `accept`/`Content-Type: application/json`, and
//! `MM-API-Source: CodexBar` (ported from CodexBar's API-token path).
//!
//! Window mapping: the `general` Token Plan model supplies the primary interval
//! and secondary weekly percent windows; the first available non-text model is
//! tertiary. Percent windows use `100 - remaining_percent`. Legacy count windows
//! keep their existing remaining-count semantics and epoch-derived durations.
//! `resets_at` comes from the reported end epoch, or the remaining-time offset.
//! Unlimited weekly windows intentionally omit `resets_at`.
//!
//! VERIFICATION: fixture-verified (CodexBar-sourced), NOT live-verified — no
//! MiniMax API key was available. Endpoint, auth headers, env key order, and
//! `model_remains` field semantics are ported from CodexBar
//! `MiniMaxUsageFetcher.swift:109-120` (request), `939-970` (remaining counts and
//! percent fields), `1018-1030` (percent conversion), `1594-1616,1648-1665`
//! (unlimited weekly quota and unavailable placeholders); lane mapping/order comes
//! from `MiniMaxUsageFetcher+ModelMapping.swift:4-12,43-50`,
//! `MiniMaxUsageSnapshot.swift:18-65,116-135`, and the fixture/assertions in
//! `MiniMaxTokenPlanChangeTests.swift:76-116,175-242`. See also
//! `MiniMaxAPIRegion.swift:9,30-37,56-58` (URLs), `MiniMaxAPISettingsReader.swift:17-24`
//! (env vars); cross-checked with OmniRoute `usage.ts:296-310`, `465-518`, `395`.
//! Region defaults to global; optional `MINIMAX_API_REGION=cn` selects China API
//! host. On global-only fetch, 401/403 retries China mainland (CodexBar
//! `MiniMaxUsageFetcher.swift:86-99`).

use async_trait::async_trait;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "minimax";

const API_KEY_ENV: &[&str] = &["MINIMAX_CODING_API_KEY", "MINIMAX_API_KEY"];
const REGION_ENV: &[&str] = &["MINIMAX_API_REGION"];

const REMAINS_PATH: &str = "/v1/api/openplatform/coding_plan/remains";
const GLOBAL_API_BASE: &str = "https://api.minimax.io";
const CHINA_API_BASE: &str = "https://api.minimaxi.com";

#[derive(Debug, Clone, Deserialize)]
struct BaseResp {
    #[serde(rename = "status_code")]
    status_code: Option<serde_json::Value>,
    #[serde(rename = "status_msg")]
    status_msg: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ModelRemains {
    #[serde(rename = "model_name")]
    model_name: Option<String>,
    #[serde(rename = "current_interval_total_count")]
    current_interval_total_count: Option<serde_json::Value>,
    #[serde(rename = "current_interval_usage_count")]
    current_interval_usage_count: Option<serde_json::Value>,
    #[serde(rename = "current_interval_status")]
    current_interval_status: Option<serde_json::Value>,
    #[serde(rename = "current_interval_remaining_percent")]
    current_interval_remaining_percent: Option<serde_json::Value>,
    #[serde(rename = "start_time")]
    start_time: Option<serde_json::Value>,
    #[serde(rename = "end_time")]
    end_time: Option<serde_json::Value>,
    #[serde(rename = "remains_time")]
    remains_time: Option<serde_json::Value>,
    #[serde(rename = "current_weekly_total_count")]
    current_weekly_total_count: Option<serde_json::Value>,
    #[serde(rename = "current_weekly_usage_count")]
    current_weekly_usage_count: Option<serde_json::Value>,
    #[serde(rename = "current_weekly_status")]
    current_weekly_status: Option<serde_json::Value>,
    #[serde(rename = "current_weekly_remaining_percent")]
    current_weekly_remaining_percent: Option<serde_json::Value>,
    #[serde(rename = "weekly_start_time")]
    weekly_start_time: Option<serde_json::Value>,
    #[serde(rename = "weekly_end_time")]
    weekly_end_time: Option<serde_json::Value>,
    #[serde(rename = "weekly_remains_time")]
    weekly_remains_time: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CodingPlanData {
    #[serde(rename = "base_resp")]
    base_resp: Option<BaseResp>,
    #[serde(rename = "model_remains", default)]
    model_remains: Vec<ModelRemains>,
}

#[derive(Debug, Deserialize)]
struct CodingPlanPayload {
    #[serde(rename = "base_resp")]
    base_resp: Option<BaseResp>,
    data: Option<CodingPlanData>,
    #[serde(rename = "model_remains", default)]
    model_remains_root: Vec<ModelRemains>,
}

fn decode_int(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn opt_int(field: &Option<serde_json::Value>) -> Option<i64> {
    field.as_ref().and_then(decode_int)
}

fn decode_float(value: &serde_json::Value) -> Option<f64> {
    let decoded = match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }?;
    decoded.is_finite().then_some(decoded)
}

fn opt_float(field: &Option<serde_json::Value>) -> Option<f64> {
    field.as_ref().and_then(decode_float)
}

fn epoch_to_secs(raw: i64) -> Option<i64> {
    if raw > 1_000_000_000_000 {
        Some(raw / 1000)
    } else if raw > 1_000_000_000 {
        Some(raw)
    } else {
        None
    }
}

fn used_percent(total: i64, remaining: i64) -> f64 {
    let used = (total - remaining).max(0);
    ((used as f64 / total as f64) * 100.0).clamp(0.0, 100.0)
}

fn remaining_percent_to_used(remaining_percent: f64) -> f64 {
    (100.0 - remaining_percent).clamp(0.0, 100.0)
}

fn window_minutes(start_raw: Option<i64>, end_raw: Option<i64>) -> Option<i64> {
    let start = start_raw.and_then(epoch_to_secs)?;
    let end = end_raw.and_then(epoch_to_secs)?;
    let minutes = (end - start) / 60;
    if minutes > 0 {
        Some(minutes)
    } else {
        None
    }
}

fn resets_at_iso(end_raw: Option<i64>, remains_raw: Option<i64>, now_secs: i64) -> Option<String> {
    if let Some(end_secs) = end_raw.and_then(epoch_to_secs) {
        if end_secs > now_secs {
            return env::epoch_to_iso8601(end_secs);
        }
    }
    let remains = remains_raw?;
    if remains <= 0 {
        return None;
    }
    let offset_secs = if remains > 1_000_000 {
        remains / 1000
    } else {
        remains
    };
    env::epoch_to_iso8601(now_secs + offset_secs)
}

fn is_text_quota_model(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    lower == "general"
        || lower.starts_with("minimax-m")
        || lower.starts_with("m2.")
        || lower.starts_with("coding-plan")
}

fn is_general_model(m: &ModelRemains) -> bool {
    m.model_name
        .as_deref()
        .is_some_and(|name| name.trim().eq_ignore_ascii_case("general"))
}

fn interval_total(m: &ModelRemains) -> i64 {
    opt_int(&m.current_interval_total_count).unwrap_or(0).max(0)
}

fn weekly_total(m: &ModelRemains) -> i64 {
    opt_int(&m.current_weekly_total_count).unwrap_or(0).max(0)
}

fn pick_representative(
    models: &[ModelRemains],
    total_fn: fn(&ModelRemains) -> i64,
) -> Option<&ModelRemains> {
    if let Some(general) = models.iter().find(|m| is_general_model(m)) {
        return Some(general);
    }

    let with_quota: Vec<_> = models.iter().filter(|m| total_fn(m) > 0).collect();
    let pool: Vec<_> = if with_quota.is_empty() {
        models.iter().collect()
    } else {
        with_quota
    };
    pool.into_iter().max_by_key(|m| total_fn(m))
}

fn make_interval_window(m: &ModelRemains, now_secs: i64) -> Option<RateWindow> {
    if let Some(remaining_percent) = opt_float(&m.current_interval_remaining_percent) {
        let unavailable_placeholder = opt_int(&m.current_interval_status) == Some(3)
            && interval_total(m) == 0
            && opt_int(&m.current_interval_usage_count).unwrap_or(0) == 0
            && remaining_percent >= 100.0;
        if unavailable_placeholder {
            return None;
        }

        return Some(RateWindow {
            used_percent: remaining_percent_to_used(remaining_percent),
            raw_used_percent: None,
            resets_at: resets_at_iso(opt_int(&m.end_time), opt_int(&m.remains_time), now_secs),
            window_minutes: window_minutes(opt_int(&m.start_time), opt_int(&m.end_time)),
        });
    }

    let total = interval_total(m);
    let remaining = opt_int(&m.current_interval_usage_count)?;
    if total <= 0 {
        return None;
    }
    let resets_at = resets_at_iso(opt_int(&m.end_time), opt_int(&m.remains_time), now_secs)?;
    Some(RateWindow {
        used_percent: used_percent(total, remaining),
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: window_minutes(opt_int(&m.start_time), opt_int(&m.end_time)),
    })
}

fn make_weekly_window(m: &ModelRemains, now_secs: i64) -> Option<RateWindow> {
    let model_name = m.model_name.as_deref().unwrap_or("");
    if !is_text_quota_model(model_name) {
        return None;
    }

    if let Some(remaining_percent) = opt_float(&m.current_weekly_remaining_percent) {
        if opt_int(&m.current_weekly_status) == Some(3) && remaining_percent >= 100.0 {
            if is_general_model(m) {
                return Some(RateWindow {
                    used_percent: 0.0,
                    raw_used_percent: None,
                    resets_at: None,
                    window_minutes: Some(7 * 24 * 60),
                });
            }
            if weekly_total(m) == 0 && opt_int(&m.current_weekly_usage_count).unwrap_or(0) == 0 {
                return None;
            }
        }

        return Some(RateWindow {
            used_percent: remaining_percent_to_used(remaining_percent),
            raw_used_percent: None,
            resets_at: resets_at_iso(
                opt_int(&m.weekly_end_time),
                opt_int(&m.weekly_remains_time),
                now_secs,
            ),
            window_minutes: Some(7 * 24 * 60),
        });
    }

    let total = weekly_total(m);
    if total <= 0 {
        return None;
    }
    let remaining = opt_int(&m.current_weekly_usage_count)?;
    let resets_at = resets_at_iso(
        opt_int(&m.weekly_end_time),
        opt_int(&m.weekly_remains_time),
        now_secs,
    )?;
    Some(RateWindow {
        used_percent: used_percent(total, remaining),
        raw_used_percent: None,
        resets_at: Some(resets_at),
        window_minutes: window_minutes(opt_int(&m.weekly_start_time), opt_int(&m.weekly_end_time)),
    })
}

fn check_base_resp(base: &Option<BaseResp>) -> Result<(), FetchError> {
    let Some(base) = base else {
        return Ok(());
    };
    let status = base.status_code.as_ref().and_then(decode_int).unwrap_or(0);
    if status == 0 {
        return Ok(());
    }
    let message = base
        .status_msg
        .clone()
        .unwrap_or_else(|| format!("status_code {status}"));
    let lower = message.to_lowercase();
    if status == 1004
        || lower.contains("cookie")
        || lower.contains("log in")
        || lower.contains("login")
    {
        return Err(FetchError::Unauthorized(message));
    }
    Err(FetchError::Upstream(message))
}

fn model_remains_list(payload: &CodingPlanPayload) -> Vec<ModelRemains> {
    if let Some(data) = &payload.data {
        if !data.model_remains.is_empty() {
            return data.model_remains.clone();
        }
    }
    payload.model_remains_root.clone()
}

fn current_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    normalize_usage_at(body, current_epoch_secs())
}

fn normalize_usage_at(body: &[u8], now_secs: i64) -> Result<Usage, FetchError> {
    let payload: CodingPlanPayload = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("minimax remains not decodable: {e}")))?;

    let base = payload
        .data
        .as_ref()
        .and_then(|d| d.base_resp.as_ref())
        .or(payload.base_resp.as_ref());
    check_base_resp(&base.cloned())?;

    let models = model_remains_list(&payload);
    if models.is_empty() {
        return Err(FetchError::Decode(
            "minimax response missing model_remains".to_string(),
        ));
    }

    let text_models: Vec<_> = models
        .iter()
        .filter(|m| {
            m.model_name
                .as_deref()
                .map(is_text_quota_model)
                .unwrap_or(true)
        })
        .cloned()
        .collect();

    let session_model = pick_representative(&text_models, interval_total);
    let primary = session_model.and_then(|m| make_interval_window(m, now_secs));

    let weekly_model = pick_representative(&text_models, weekly_total);
    let secondary = weekly_model.and_then(|m| make_weekly_window(m, now_secs));

    let tertiary = models
        .iter()
        .filter(|m| {
            m.model_name
                .as_deref()
                .is_some_and(|name| !is_text_quota_model(name))
        })
        .find_map(|m| make_interval_window(m, now_secs));

    Ok(Usage {
        primary,
        secondary,
        tertiary,
        extra_rate_windows: None,
    })
}

fn api_base_url() -> String {
    let region = env::first_env(REGION_ENV)
        .map(|v| v.trim().to_lowercase())
        .unwrap_or_default();
    if region == "cn" || region == "china" || region == "china_mainland" {
        CHINA_API_BASE.to_string()
    } else {
        GLOBAL_API_BASE.to_string()
    }
}

fn remains_url(base: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), REMAINS_PATH)
}

async fn fetch_remains_once(
    client: &reqwest::Client,
    api_key: &str,
    base: &str,
) -> Result<Vec<u8>, FetchError> {
    let url = remains_url(base);
    JsonRequest::get(url)
        .bearer(api_key)
        .header(Header::new("accept", "application/json"))
        .header(Header::new("Content-Type", "application/json"))
        .header(Header::new("MM-API-Source", "CodexBar"))
        .send(client)
        .await
}

pub struct MinimaxProvider {
    http: reqwest::Client,
}

impl MinimaxProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for MinimaxProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for MinimaxProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;

            let base = api_base_url();
            let body = match fetch_remains_once(&self.http, &api_key, &base).await {
                Ok(body) => body,
                Err(FetchError::Unauthorized(_)) if base == GLOBAL_API_BASE => {
                    fetch_remains_once(&self.http, &api_key, CHINA_API_BASE).await?
                }
                Err(e) => return Err(e),
            };

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

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn normalizes_coding_plan_remains_payload() {
        let start = 1_700_000_000_000_i64;
        let end = start + 5 * 60 * 60 * 1000;
        let json = format!(
            r#"{{
              "base_resp": {{ "status_code": 0 }},
              "current_subscribe_title": "Max",
              "model_remains": [{{
                "model_name": "M2.7-highspeed",
                "current_interval_total_count": 1000,
                "current_interval_usage_count": 250,
                "start_time": {start},
                "end_time": {end},
                "remains_time": 240000
              }}]
            }}"#
        );
        let usage = normalize_usage_at(json.as_bytes(), NOW).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 75.0);
        assert_eq!(primary.window_minutes, Some(300));
        let expected_end_secs = end / 1000;
        assert_eq!(primary.resets_at, env::epoch_to_iso8601(expected_end_secs));
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn remaining_count_converts_to_used_percent() {
        let body = br#"{
          "base_resp": { "status_code": 0 },
          "model_remains": [{
            "current_interval_total_count": 100,
            "current_interval_usage_count": 80,
            "end_time": 2000000000
          }]
        }"#;
        let usage = normalize_usage_at(body, 1_000_000_000).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 20.0);
    }

    #[test]
    fn missing_reset_drops_window() {
        let body = br#"{
          "base_resp": { "status_code": 0 },
          "model_remains": [{
            "current_interval_total_count": 100,
            "current_interval_usage_count": 50
          }]
        }"#;
        let usage = normalize_usage_at(body, NOW).unwrap();
        assert!(usage.primary.is_none());
    }

    #[test]
    fn garbage_body_is_decode_error() {
        let err = normalize_usage(b"not json").unwrap_err();
        assert!(matches!(err, FetchError::Decode(_)));
    }

    #[test]
    fn weekly_window_maps_to_secondary() {
        let start = 1_700_000_000_000_i64;
        let end = start + 5 * 60 * 60 * 1000;
        let week_start = start - 2 * 24 * 60 * 60 * 1000;
        let week_end = week_start + 7 * 24 * 60 * 60 * 1000;
        let json = format!(
            r#"{{
              "base_resp": {{ "status_code": 0 }},
              "model_remains": [{{
                "model_name": "MiniMax-M1",
                "current_interval_total_count": 1000,
                "current_interval_usage_count": 250,
                "start_time": {start},
                "end_time": {end},
                "current_weekly_total_count": 6000,
                "current_weekly_usage_count": 5376,
                "weekly_start_time": {week_start},
                "weekly_end_time": {week_end}
              }}]
            }}"#
        );
        let usage = normalize_usage_at(json.as_bytes(), NOW).unwrap();
        assert!(usage.primary.is_some());
        let secondary = usage.secondary.unwrap();
        assert!((secondary.used_percent - 10.4).abs() < 0.01);
        assert_eq!(secondary.resets_at, env::epoch_to_iso8601(week_end / 1000));
    }

    #[test]
    fn token_plan_orders_general_windows_before_video() {
        let now = 1_780_282_340;
        let body = br#"{
          "base_resp": { "status_code": "0" },
          "model_remains": [
            {
              "model_name": "video",
              "current_interval_total_count": 100,
              "current_interval_usage_count": 70,
              "current_interval_remaining_percent": 30,
              "start_time": 1780243200000,
              "end_time": 1780329600000
            },
            {
              "model_name": "general",
              "current_interval_total_count": 0,
              "current_interval_usage_count": 0,
              "current_interval_remaining_percent": 96,
              "start_time": 1780279200000,
              "end_time": 1780297200000,
              "current_weekly_total_count": 0,
              "current_weekly_usage_count": 0,
              "current_weekly_remaining_percent": 99,
              "weekly_start_time": 1780243200000,
              "weekly_end_time": 1780848000000
            }
          ]
        }"#;

        let usage = normalize_usage_at(body, now).unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 4.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.resets_at, env::epoch_to_iso8601(1_780_297_200));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, 1.0);
        assert_eq!(secondary.window_minutes, Some(10_080));
        assert_eq!(secondary.resets_at, env::epoch_to_iso8601(1_780_848_000));

        let tertiary = usage.tertiary.unwrap();
        assert_eq!(tertiary.used_percent, 70.0);
        assert_eq!(tertiary.window_minutes, Some(1_440));
        assert_eq!(tertiary.resets_at, env::epoch_to_iso8601(1_780_329_600));
    }

    #[test]
    fn token_plan_unlimited_weekly_window_has_no_reset() {
        let now = 1_780_347_620;
        let body = br#"{
          "base_resp": { "status_code": 0, "status_msg": "success" },
          "model_remains": [{
            "model_name": "general",
            "current_interval_total_count": 0,
            "current_interval_usage_count": 0,
            "current_interval_status": 1,
            "current_interval_remaining_percent": 99,
            "start_time": 1780347600000,
            "end_time": 1780365600000,
            "current_weekly_total_count": 0,
            "current_weekly_usage_count": 0,
            "current_weekly_status": 3,
            "current_weekly_remaining_percent": 100,
            "weekly_start_time": 1780243200000,
            "weekly_end_time": 1780848000000
          }]
        }"#;

        let usage = normalize_usage_at(body, now).unwrap();
        let secondary = usage.secondary.unwrap();

        assert_eq!(secondary.used_percent, 0.0);
        assert_eq!(secondary.window_minutes, Some(10_080));
        assert_eq!(secondary.resets_at, None);
    }

    #[test]
    fn token_plan_drops_non_general_weekly_placeholder() {
        let now = 1_780_347_620;
        let body = br#"{
          "base_resp": { "status_code": 0 },
          "model_remains": [
            {
              "model_name": "minimax-m2",
              "current_interval_total_count": 0,
              "current_interval_usage_count": 0,
              "current_interval_status": 3,
              "current_interval_remaining_percent": 100,
              "current_weekly_total_count": 0,
              "current_weekly_usage_count": 0,
              "current_weekly_status": 3,
              "current_weekly_remaining_percent": 100
            },
            {
              "model_name": "general",
              "current_interval_total_count": 0,
              "current_interval_usage_count": 0,
              "current_interval_status": 1,
              "current_interval_remaining_percent": 99,
              "start_time": 1780347600000,
              "end_time": 1780365600000,
              "current_weekly_total_count": 0,
              "current_weekly_usage_count": 0,
              "current_weekly_status": 3,
              "current_weekly_remaining_percent": 100
            }
          ]
        }"#;

        let payload: CodingPlanPayload = serde_json::from_slice(body).unwrap();
        let models = model_remains_list(&payload);
        let placeholder = models
            .iter()
            .find(|model| model.model_name.as_deref() == Some("minimax-m2"))
            .unwrap();
        assert!(make_weekly_window(placeholder, now).is_none());

        let usage = normalize_usage_at(body, now).unwrap();
        assert!(usage.primary.is_some());
        assert!(usage.secondary.is_some());
    }
}
