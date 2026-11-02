use super::call_windows;
use serde_json::{json, Value};
use std::collections::BTreeMap;

// Quota reply captures from HTTP 429, HTTP 200 and WebSocket on 2026-11-02.
// Credit balances are synthetic; no account or credential identities are kept.
pub(crate) fn captured_headers(status: u16) -> BTreeMap<String, String> {
    serde_json::from_value(json!({
        "x-codex-active-limit":"premium",
        "x-codex-credits-balance":if status == 429 { "0" } else { "1234.5000000000" },
        "x-codex-credits-has-credits":if status == 429 { "False" } else { "True" },
        "x-codex-credits-unlimited":"False","x-codex-plan-type":"pro",
        "x-codex-primary-over-secondary-limit-percent":"0",
        "x-codex-primary-reset-after-seconds":if status == 429 { "154246" } else { "154200" },
        "x-codex-primary-reset-at":if status == 429 { "1793774135" } else { "1793774164" },
        "x-codex-primary-used-percent":"100","x-codex-primary-window-minutes":"10080",
        "x-codex-secondary-reset-after-seconds":"0","x-codex-secondary-reset-at":"",
        "x-codex-secondary-used-percent":"0","x-codex-secondary-window-minutes":"0"
    }))
    .unwrap()
}

pub(crate) fn captured_frame() -> Value {
    json!({"type":"codex.rate_limits","plan_type":"pro",
        "rate_limits":{"allowed":false,"limit_reached":true,
            "primary":{"used_percent":100,"window_minutes":10080,"reset_after_seconds":154198,"reset_at":1793774164},"secondary":null},
        "code_review_rate_limits":null,"additional_rate_limits":null,
        "credits":{"has_credits":true,"unlimited":false,"balance":"1234.5000000000"},"promo":null})
}

#[test]
fn real_429_200_and_frame_preserve_only_the_weekly_window() {
    for status in [429, 200] {
        let usage = call_windows(&captured_headers(status), None);
        let weekly = usage.primary.unwrap();
        assert_eq!(weekly.used_percent, 100.0);
        assert_eq!(weekly.window_minutes, Some(10080));
        let expected = if status == 429 {
            "2026-11-04T06:35:35Z"
        } else {
            "2026-11-04T06:36:04Z"
        };
        assert_eq!(weekly.resets_at.as_deref(), Some(expected));
        assert!(usage.secondary.is_none());
    }
    let frame = captured_frame();
    let usage = call_windows(&BTreeMap::new(), frame.as_object());
    assert_eq!(usage.primary.unwrap().used_percent, 100.0);
    assert!(usage.secondary.is_none());
}

#[test]
fn codex_zero_placeholder_is_absent() {
    let usage = call_windows(&captured_headers(200), None);
    assert!(
        usage.secondary.is_none(),
        "zero minutes and empty reset are a placeholder"
    );
    let mut h = BTreeMap::new();
    h.insert("x-codex-primary-used-percent".into(), "0".into());
    assert!(call_windows(&h, None).primary.is_none());
    h.insert("x-codex-primary-window-minutes".into(), "300".into());
    assert_eq!(call_windows(&h, None).primary.unwrap().used_percent, 0.0);
}

#[test]
fn non_default_families_never_override_default_codex_windows() {
    let mut h = captured_headers(200);
    h.insert("x-codex-secondary-primary-used-percent".into(), "12".into());
    h.insert(
        "x-codex-secondary-primary-window-minutes".into(),
        "300".into(),
    );
    h.insert(
        "x-codex-secondary-limit-name".into(),
        "codex_secondary".into(),
    );
    h.insert("x-codex-turn-state".into(), "opaque".into());
    let usage = call_windows(&h, None);
    assert_eq!(usage.primary.unwrap().used_percent, 100.0);
    assert!(usage.secondary.is_none());
    let mut f = captured_frame();
    f["additional_rate_limits"] =
        json!({"some-model":{"primary":{"used_percent":1,"window_minutes":300}}});
    assert_eq!(
        call_windows(&BTreeMap::new(), f.as_object())
            .primary
            .unwrap()
            .window_minutes,
        Some(10080)
    );
}

#[test]
fn codex_percent_validation_and_reset_units() {
    let mut h = captured_headers(200);
    for invalid in ["NaN", "inf", "-1", "101", "garbage"] {
        h.insert("x-codex-primary-used-percent".into(), invalid.into());
        assert!(call_windows(&h, None).primary.is_none(), "{invalid}");
    }
    h.insert("x-codex-primary-used-percent".into(), "37".into());
    for reset in ["1793774164", "1793774164000", "2026-11-04T06:36:04Z"] {
        h.insert("x-codex-primary-reset-at".into(), reset.into());
        assert_eq!(
            call_windows(&h, None).primary.unwrap().resets_at.as_deref(),
            Some("2026-11-04T06:36:04Z")
        );
    }
    for invalid in [-1, 101] {
        let mut f = captured_frame();
        f["rate_limits"]["primary"]["used_percent"] = json!(invalid);
        assert!(call_windows(&BTreeMap::new(), f.as_object())
            .primary
            .is_none());
    }
}
