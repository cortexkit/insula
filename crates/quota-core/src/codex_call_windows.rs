use crate::model::{RateWindow, Usage};
use chrono::{DateTime, Utc};

/// Parse only the default Codex quota family. Other named limits, credits and
/// turn state describe different resources and must not override these periods.
pub fn call_windows(
    headers: &std::collections::BTreeMap<String, String>,
    frame: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Usage {
    let from_frame = frame
        .filter(|f| f.get("type").and_then(|v| v.as_str()) == Some("codex.rate_limits"))
        .and_then(|f| f.get("rate_limits"));
    let parse = |slot: &str| {
        let prefix = format!("x-codex-{slot}-");
        let header = |key: &str| headers.get(&format!("{prefix}{key}"));
        let window = from_frame.and_then(|r| r.get(slot));
        let number = |key: &str| window.and_then(|w| w.get(key)).and_then(|v| v.as_f64());
        // Do not mix a partial header period with a frame period. A family with
        // a used-percent header is authoritative for that slot.
        let (used, minutes, reset) = if let Some(used) = header("used-percent") {
            (
                used.parse::<f64>().ok(),
                header("window-minutes").and_then(|v| v.parse::<i64>().ok()),
                header("reset-at").cloned(),
            )
        } else {
            (
                number("used_percent"),
                window
                    .and_then(|w| w.get("window_minutes"))
                    .and_then(|v| v.as_i64()),
                window.and_then(|w| w.get("reset_at")).map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                }),
            )
        };
        let used = used.filter(|v| v.is_finite() && (0.0..=100.0).contains(v))?;
        let minutes = minutes.filter(|v| *v > 0);
        let reset = reset.and_then(|value| {
            DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|v| v.with_timezone(&Utc))
                .or_else(|| {
                    value.parse::<i64>().ok().and_then(|v| {
                        if v > 100_000_000_000 {
                            DateTime::from_timestamp_millis(v)
                        } else {
                            DateTime::from_timestamp(v, 0)
                        }
                    })
                })
                .map(|v| v.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        });
        if used == 0.0 && minutes.is_none() && reset.is_none() {
            return None;
        }
        Some(RateWindow {
            used_percent: used,
            window_minutes: minutes,
            resets_at: reset,
            window_kind: None,
            raw_used_percent: None,
            used_count: None,
            total_count: None,
            regeneration: None,
            breakdown: None,
        })
    };
    Usage {
        primary: parse("primary"),
        secondary: parse("secondary"),
        ..Usage::default()
    }
}
