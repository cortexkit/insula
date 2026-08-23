//! Synthetic usage fetcher — credential from an environment variable, or from
//! the shared opencode auth store.
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
    opencode_auth::{self, OpencodeAuth},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "synthetic";
const API_KEY_ENV: &[&str] = &["SYNTHETIC_API_KEY"];
/// This provider's key in opencode's auth store.
const OPENCODE_PROVIDER: &str = "synthetic";
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

/// Convert an upstream duration into a window length, refusing anything that is
/// not a plausible one.
///
/// The range is checked before the cast, because `as i64` saturates rather than
/// failing: an infinite or astronomically large value would otherwise become
/// `i64::MAX` and read as an ordinary length.
fn minutes_from(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let minutes = value.round();
    if !(1.0..=crate::wire_sanity::MAX_WINDOW_MINUTES as f64).contains(&minutes) {
        return None;
    }
    Some(minutes as i64)
}

fn window_minutes(map: &serde_json::Map<String, serde_json::Value>) -> Option<i64> {
    // A length stated directly in minutes is checked too: it arrives as an
    // integer, so no cast is involved, but nothing stops the upstream sending a
    // negative or absurd one.
    if let Some(minutes) = crate::json_scan::first_i64(map, WINDOW_MINUTES_KEYS) {
        return crate::wire_sanity::plausible_window_length(minutes).then_some(minutes);
    }
    if let Some(hours) = crate::json_scan::first_finite_f64(map, WINDOW_HOURS_KEYS) {
        return minutes_from(hours * 60.0);
    }
    if let Some(days) = crate::json_scan::first_finite_f64(map, WINDOW_DAYS_KEYS) {
        return minutes_from(days * 24.0 * 60.0);
    }
    if let Some(seconds) = crate::json_scan::first_finite_f64(map, WINDOW_SECONDS_KEYS) {
        return minutes_from(seconds / 60.0);
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
                    // Range-checked like the numeric paths: a textual length can
                    // carry an exponent, so "1e400d" parses as infinity and the
                    // cast would saturate into an ordinary-looking length.
                    return minutes_from(value * multiplier);
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
    checks
        .iter()
        .any(|keys| crate::json_scan::first_finite_f64(map, keys).is_some())
}

fn parse_quota(map: &serde_json::Map<String, serde_json::Value>) -> Option<RateWindow> {
    let percent_used =
        normalized_percent(crate::json_scan::first_finite_f64(map, PERCENT_USED_KEYS));
    let percent_remaining = normalized_percent(crate::json_scan::first_finite_f64(
        map,
        PERCENT_REMAINING_KEYS,
    ));

    let mut used_percent = percent_used;
    if used_percent.is_none() {
        if let Some(remaining) = percent_remaining {
            used_percent = Some(100.0 - remaining);
        }
    }

    if used_percent.is_none() {
        let mut limit = crate::json_scan::first_finite_f64(map, LIMIT_KEYS);
        let mut used = crate::json_scan::first_finite_f64(map, USED_KEYS);
        let remaining = crate::json_scan::first_finite_f64(map, REMAINING_KEYS);

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

    let resets_at = first_date(map, RESET_KEYS);
    let window_minutes = window_minutes(map);

    Some(RateWindow {
        used_percent: clamped,
        raw_used_percent: None,
        resets_at,
        window_minutes,
        used_count: None,
        total_count: None,
        regeneration: None,
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
            http: crate::http::provider_client(),
        }
    }
}

impl Default for SyntheticProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// The API key, from the environment or the shared opencode auth store.
///
/// The environment wins so an operator can point this at another account
/// without touching a file another tool owns. The store is the fallback because
/// this key already lives there on any machine where opencode is configured for
/// this provider, and several providers here read it for exactly that reason --
/// requiring an environment variable as well would report an account that is
/// configured on the host as one that is not.
fn resolve_api_key() -> Result<String, FetchError> {
    resolve_api_key_from(env::first_env(API_KEY_ENV), || {
        opencode_auth::read_provider(OPENCODE_PROVIDER)
    })
}

/// The precedence, over a supplied environment value and store lookup.
///
/// Split from [`resolve_api_key`] so both halves of the rule can be tested: that
/// the environment wins when set, and that the store is consulted when it is
/// not. Testing only the classification below would leave the fallback
/// unreached by any test -- the wiring is the part that was missing before, and
/// removing it is invisible to a suite that only exercises the classifier.
///
/// The lookup is taken lazily so an environment hit does not read a file.
fn resolve_api_key_from(
    from_env: Option<String>,
    lookup: impl FnOnce() -> Result<Option<OpencodeAuth>, FetchError>,
) -> Result<String, FetchError> {
    if let Some(key) = from_env {
        return Ok(key);
    }
    key_from_store(lookup()?)
}

/// Turn a store lookup into a key or a reason there is none.
///
/// Split from [`resolve_api_key`] so each outcome can be tested: the caller
/// reads the real store at a fixed path, and the three cases differ in which
/// error class they publish rather than in anything observable from a live run
/// on a host that has only one of them.
fn key_from_store(entry: Option<OpencodeAuth>) -> Result<String, FetchError> {
    match entry {
        Some(OpencodeAuth::Api { key }) => Ok(key),
        // An OAuth entry would be this provider changing its credential shape
        // rather than a key to send: the bearer belongs to a different grant, so
        // sending it would draw a 401 and be published as a credential the
        // upstream refused. Reported as unusable instead, which names what is
        // actually on the host.
        Some(OpencodeAuth::Oauth { .. }) => Err(FetchError::CredentialUnusable(
            "the opencode synthetic entry holds an OAuth token, not an API key".to_string(),
        )),
        None => Err(FetchError::NoSession(format!(
            "none of {API_KEY_ENV:?} is set and no synthetic entry in the opencode auth store"
        ))),
    }
}

#[async_trait]
impl UsageProvider for SyntheticProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let api_key = resolve_api_key()?;
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

    /// The environment wins, and the store is consulted when it is silent.
    ///
    /// Both halves matter. Without the first, an operator cannot point this at
    /// another account without editing a file another tool owns. Without the
    /// second, a host where opencode holds this credential reports the provider
    /// as never configured -- which is the state that made this provider dark
    /// on machines that had a working key all along.
    #[test]
    fn the_environment_wins_and_the_store_is_the_fallback() {
        let unread = || panic!("the store must not be read when the environment answers");
        let key = resolve_api_key_from(Some("from-env".to_string()), unread)
            .expect("an environment key is used as-is");
        assert_eq!(key, "from-env");

        // Not vacuous: with no environment value the store is reached, so
        // deleting the fallback fails here rather than passing quietly.
        let key = resolve_api_key_from(None, || {
            Ok(Some(OpencodeAuth::Api {
                key: "from-store".to_string(),
            }))
        })
        .expect("the store answers when the environment does not");
        assert_eq!(key, "from-store");

        // And a store that cannot be read is not a store with nothing in it.
        let error = resolve_api_key_from(None, || {
            Err(FetchError::CredentialUnusable("unreadable".to_string()))
        })
        .expect_err("a failed store read must not read as no credential");
        assert!(
            matches!(error, FetchError::CredentialUnusable(_)),
            "expected CredentialUnusable, got {error:?}"
        );
    }

    /// The store lookup publishes a different reason for each outcome.
    ///
    /// All three reach a consumer as "no usage", and they mean different things
    /// to whoever reads the output: a missing entry is a host where this
    /// provider was never configured, while an entry of the wrong shape is a
    /// host somebody must look at. The classes are treated differently as far
    /// out as the health buckets, where absence is deliberately left out of the
    /// count an operator watches.
    #[test]
    fn each_store_outcome_publishes_its_own_reason() {
        let key = key_from_store(Some(OpencodeAuth::Api {
            key: "sk-test".to_string(),
        }))
        .expect("an api entry yields its key");
        assert_eq!(key, "sk-test");

        // The provider is configured in opencode but with a credential this
        // endpoint cannot use. Not an absence: something is there.
        let error = key_from_store(Some(OpencodeAuth::Oauth {
            access: "token".to_string(),
            refresh: None,
            expires: None,
        }))
        .expect_err("an oauth entry is not an api key");
        assert!(
            matches!(error, FetchError::CredentialUnusable(_)),
            "expected CredentialUnusable, got {error:?}"
        );

        // No entry at all is the ordinary state on a host that does not use
        // this provider.
        let error = key_from_store(None).expect_err("no entry means no credential");
        assert!(
            matches!(error, FetchError::NoSession(_)),
            "expected NoSession, got {error:?}"
        );
    }

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
    fn exhausted_window_without_reset_is_kept() {
        let body = br#"{
            "rollingFiveHourLimit": {
                "used": 100,
                "limit": 100,
                "window": "5hr"
            }
        }"#;
        let primary = normalize_usage(body)
            .unwrap()
            .primary
            .expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.resets_at, None);
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
    fn test_missing_reset_keeps_window() {
        let body = br#"{
            "rollingFiveHourLimit": {
                "used": 20,
                "limit": 100,
                "window": "5hr"
            }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 20.0);
        assert_eq!(primary.resets_at, None);
    }

    /// A percent field may arrive as a fraction or as a percentage, so a value at
    /// or below 1.0 is read as a fraction and scaled. That guess is only safe for
    /// a value the upstream *labelled* as a percent.
    ///
    /// A used/limit ratio computed here is already a percentage, and applying the
    /// same guess to it turns a barely-touched account into an exhausted one: one
    /// request against a limit of a hundred is 1.0, which would be rescaled to
    /// 100. The upstream that reports counts rather than percentages is exactly
    /// the one where a single request is a plausible reading.
    ///
    /// The two are kept apart by which path produced the number, so this pins
    /// both: the ratio must survive untouched, and the labelled field must still
    /// be scaled.
    #[test]
    fn a_computed_ratio_is_not_rescaled_but_a_labelled_fraction_is() {
        // The five-hour key fills `usage.primary`; a weekly key would fill
        // `usage.secondary` instead, so the assertions below would read an empty
        // slot and fail for a reason unrelated to the rescaling under test.
        let counts = br#"{
            "rollingFiveHourLimit": {
                "used": 1,
                "limit": 100,
                "window": "5hr"
            }
        }"#;
        let percent = normalize_usage(counts)
            .unwrap()
            .primary
            .expect("counts must produce a window")
            .used_percent;
        assert_eq!(
            percent, 1.0,
            "one request in a hundred is 1%, not an exhausted account"
        );

        // The control: without it this test would also pass if the fraction
        // heuristic were deleted outright, which would misread every upstream
        // that reports a 0..1 fraction as a fully idle account.
        let fraction = br#"{
            "rollingFiveHourLimit": {
                "usedPercent": 0.42,
                "window": "5hr"
            }
        }"#;
        let scaled = normalize_usage(fraction)
            .unwrap()
            .primary
            .expect("a labelled percent must produce a window")
            .used_percent;
        assert_eq!(scaled, 42.0, "a labelled 0..1 fraction is still scaled");
    }

    /// A garbage percent alias must not cost the account its window.
    ///
    /// These upstreams publish the same figure under several names, so the
    /// percent is read by scanning a key list. Returning an unusable value
    /// rather than skipping it ends the scan at the garbage alias, and that
    /// value becomes the account's percent -- which is dropped before
    /// publication, so the account reports no window at all while the upstream
    /// did state a real figure under a later name.
    #[test]
    fn a_garbage_percent_alias_does_not_hide_a_real_one() {
        // Both aliases sit in the same object the window is read from, which is
        // where the scan runs.
        let body = br#"{
            "rollingFiveHourLimit": { "usedPercent": "NaN", "percent_used": 73.5 }
        }"#;

        let usage = normalize_usage(body).expect("the payload still parses");
        let primary = usage.primary.expect("the account must still get a window");

        assert_eq!(primary.used_percent, 73.5);
    }

    /// A window length that is not a duration is dropped rather than published.
    ///
    /// Four paths reach this: minutes, hours, days and seconds, plus a textual
    /// form. The numeric ones convert through a cast that saturates rather than
    /// failing, and the textual one can carry an exponent -- so "1e400d" parses
    /// as infinity, which saturates to an ordinary-looking length.
    ///
    /// Consumers read this field as a cadence, and the checker that would catch
    /// a bad one uses it as the ceiling for its own reset test, so a nonsense
    /// length silently disables that check for this window.
    #[test]
    fn a_window_length_that_is_not_a_duration_is_dropped() {
        // No numeric case carries an exponent: the JSON parser rejects a number
        // it cannot represent, so infinity reaches this code only as text.
        for field in [
            r#""windowMinutes": -5"#,
            r#""windowMinutes": 0"#,
            r#""windowHours": -3.0"#,
            r#""windowDays": 99999999.0"#,
            r#""windowSeconds": -60.0"#,
            r#""window": "1e400d""#,
        ] {
            // The five-hour key fills `usage.primary`; a bare percent outside a
            // recognised container fills no slot at all.
            let body =
                format!(r#"{{ "rollingFiveHourLimit": {{ "used": 42, "limit": 100, {field} }} }}"#)
                    .into_bytes();

            let usage = normalize_usage(&body).expect("the payload still parses");
            let primary = usage
                .primary
                .expect("the percent is load-bearing and must still be published");

            assert_eq!(
                primary.window_minutes, None,
                "{field} was published as a length"
            );
            // Not vacuous: the window itself survives, so this cannot pass by
            // dropping everything.
            assert_eq!(primary.used_percent, 42.0);
        }
    }

    /// Real cadences are still published on every path, so the guard cannot pass
    /// by refusing everything.
    #[test]
    fn ordinary_window_lengths_are_published() {
        for (field, expected) in [
            (r#""windowMinutes": 300"#, 300),
            (r#""windowHours": 5.0"#, 300),
            (r#""windowDays": 7.0"#, 10080),
            (r#""windowSeconds": 18000.0"#, 300),
            (r#""window": "7d""#, 10080),
        ] {
            let body =
                format!(r#"{{ "rollingFiveHourLimit": {{ "used": 42, "limit": 100, {field} }} }}"#)
                    .into_bytes();

            let usage = normalize_usage(&body).expect("the payload still parses");
            assert_eq!(
                usage.primary.expect("a window is published").window_minutes,
                Some(expected),
                "{field} was not published"
            );
        }
    }
}
