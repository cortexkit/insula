//! The usage output model — byte-shaped to what Alfonso already consumes.
//!
//! Alfonso's `codexbar-window-extractors.ts` reads exactly this JSON per
//! provider: `{ provider, account, source, usage: { primary, secondary,
//! tertiary, extraRateWindows? } }`, where each window is
//! `{ usedPercent, resetsAt (ISO8601), windowMinutes }`. Producing this shape
//! verbatim is what lets the consumer adapter swap `fetch()` for the subc client
//! without touching its extractor/pace path.
//!
//! Serialization rules that the consumer depends on:
//! - camelCase keys (`usedPercent`, `resetsAt`, `windowMinutes`,
//!   `extraRateWindows`).
//! - A healthy entry MUST NOT carry `error` (the consumer skips any entry whose
//!   `error` is truthy), so it is omitted when absent.
//! - A window is emitted when it has a `usedPercent`; `resetsAt` is OPTIONAL and
//!   omitted when the provider reports no reset, matching CodexBar's
//!   `makeWindow` (`ClaudeUsageFetcher.swift:945-956`), which builds a window
//!   from `utilization` alone and leaves `resetsAt` nil. A provider that
//!   genuinely reports no reset (e.g. an idle 0%-used session window) thus shows
//!   the real percent reset-less instead of vanishing. This is distinct from
//!   FABRICATING a reset (crof-class), which we still never do. NOTE: the pace
//!   consumer (`codexbar-window-extractors.ts`) still requires `resetsAt` to feed
//!   a window into pacing, so a reset-less window appears in the dump (parity
//!   with CodexBar's surface) but contributes no burn-rate projection.

use serde::{Deserialize, Serialize};

/// One rate-limit window: how much of a quota pool is spent and when it resets.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RateWindow {
    /// 0..100 percent of the window's quota consumed. This is the EFFECTIVE
    /// number consumers pace on: when banked-reset relaxation applies it is
    /// zeroed, and the provider-reported percent moves to `raw_used_percent`.
    pub used_percent: f64,
    /// The provider-reported percent when `used_percent` has been relaxed to
    /// an effective value (banked resets guarantee the window resets before
    /// the wall). Present only on relaxed windows; human-facing UIs should
    /// display this truth alongside the effective number.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw_used_percent: Option<f64>,
    /// ISO 8601 / RFC 3339 timestamp when the window resets. Omitted when the
    /// provider reports no reset (e.g. an idle session window with nothing
    /// pending) — never fabricated. Mirrors CodexBar's optional `resetsAt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// Window length in minutes. Omitted when the provider does not report one;
    /// the consumer then paces on utilization alone rather than a burn rate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
}

/// A per-model window bundled under one account (e.g. Antigravity's Geminis).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtraWindow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<RateWindow>,
}

/// The window topology for one account: up to three account-wide pools plus an
/// optional list of per-model pools.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<RateWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<RateWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tertiary: Option<RateWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_rate_windows: Option<Vec<ExtraWindow>>,
}

/// What a [`Balance`] amount is denominated in.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum BalanceKind {
    /// Opaque provider credits/points.
    Credits,
    /// A real currency (then `unit` is a currency code like "USD").
    Currency,
}

/// A prepaid balance signal — remaining credits or currency with NO reset window.
///
/// RESERVED SEAM — currently never populated by any provider. Some providers
/// report only a remaining balance with no reset/period (prepaid USD, pure
/// credits); the one thing we must never do is express that as a [`RateWindow`]
/// with a fabricated `resetsAt`, which would poison the consumer's pace
/// projection. This type is the future home for that signal so the balance axis
/// can be added later as a non-breaking change. No provider wires to it today and
/// no consumer reads it yet; it exists only to reserve the shape.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    /// Amount remaining, in `unit`.
    pub remaining: f64,
    /// Total/limit when the provider reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Denomination of `remaining`/`total` — a currency code for `Currency`, or a
    /// credit/point label for `Credits`.
    pub unit: String,
    pub kind: BalanceKind,
}

/// Account labels and subscription information supplied by a provider or vault.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub org_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plan_type: Option<String>,
}

impl AccountInfo {
    pub fn is_empty(&self) -> bool {
        self.email.is_none() && self.org_name.is_none() && self.plan_type.is_none()
    }
}

/// One saved reset credit and its expiry time.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CreditExpiry {
    pub expires_at: String,
}

/// Saved reset credits reported by Codex's read-only credits endpoint.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SavedResets {
    #[serde(default)]
    pub available_count: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub soonest_expires_at: Option<String>,
    #[serde(default)]
    pub credits: Vec<CreditExpiry>,
}

fn account_info_is_empty(value: &Option<AccountInfo>) -> bool {
    value.as_ref().map(AccountInfo::is_empty).unwrap_or(true)
}

/// One provider/account's usage entry. The `/usage` response is an array of
/// these. A fetch failure becomes an entry carrying `error` (silent-degrade),
/// never a failure of the whole array.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    /// CodexBar provider name (e.g. "codex"), which Alfonso maps to its own id.
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Which retrieval path produced this (e.g. "oauth") — observability only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "account_info_is_empty", default)]
    pub account_info: Option<AccountInfo>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fetched_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub saved_resets: Option<SavedResets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// RESERVED SEAM — a prepaid balance signal alongside (or instead of) the
    /// windows in `usage`. Currently never populated; see [`Balance`]. Omitted
    /// from the wire while absent, so today's consumer output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<Balance>,
    /// Present only on a degraded entry. The consumer skips any entry with a
    /// truthy `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ProviderUsage {
    /// A healthy entry with resolved windows.
    pub fn healthy(provider: &str, account: Option<String>, source: &str, usage: Usage) -> Self {
        Self {
            provider: provider.to_string(),
            account,
            source: Some(source.to_string()),
            account_info: None,
            fetched_at: None,
            saved_resets: None,
            usage: Some(usage),
            balance: None,
            error: None,
        }
    }

    /// A degraded entry: the provider is named so the consumer can correlate,
    /// but it carries only an error string and no windows.
    pub fn degraded(provider: &str, error: impl std::fmt::Display) -> Self {
        Self {
            provider: provider.to_string(),
            account: None,
            source: None,
            account_info: None,
            fetched_at: None,
            saved_resets: None,
            usage: None,
            balance: None,
            error: Some(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_seam_is_absent_from_the_wire_today() {
        // The reserved balance seam must not appear in any current provider's
        // serialized entry — today's consumer output is byte-identical to before.
        let entry = ProviderUsage::healthy(
            "codex",
            None,
            "oauth",
            Usage {
                primary: Some(RateWindow {
                    used_percent: 41.0,
                    raw_used_percent: None,
                    resets_at: Some("2026-06-22T13:44:39Z".to_string()),
                    window_minutes: Some(300),
                }),
                ..Usage::default()
            },
        );
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            json,
            r#"{"provider":"codex","source":"oauth","usage":{"primary":{"usedPercent":41.0,"resetsAt":"2026-06-22T13:44:39Z","windowMinutes":300}}}"#
        );
        assert!(!json.contains("balance"), "balance must be omitted: {json}");
    }

    #[test]
    fn account_info_is_omitted_when_empty_and_keeps_partial_labels() {
        let empty = ProviderUsage {
            provider: "codex".to_string(),
            account: None,
            source: None,
            account_info: Some(AccountInfo::default()),
            fetched_at: None,
            saved_resets: None,
            usage: None,
            balance: None,
            error: None,
        };
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"provider":"codex"}"#
        );

        let partial = ProviderUsage {
            account_info: Some(AccountInfo {
                email: Some("user@example.com".to_string()),
                org_name: None,
                plan_type: None,
            }),
            ..empty
        };
        assert_eq!(
            serde_json::to_string(&partial).unwrap(),
            r#"{"provider":"codex","accountInfo":{"email":"user@example.com"}}"#
        );
    }

    #[test]
    fn saved_resets_use_camel_case_and_round_trip() {
        let saved = SavedResets {
            available_count: 2,
            soonest_expires_at: Some("2026-07-15T12:00:00Z".to_string()),
            credits: vec![CreditExpiry {
                expires_at: "2026-07-15T12:00:00Z".to_string(),
            }],
        };
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(
            json,
            r#"{"availableCount":2,"soonestExpiresAt":"2026-07-15T12:00:00Z","credits":[{"expiresAt":"2026-07-15T12:00:00Z"}]}"#
        );
        assert_eq!(serde_json::from_str::<SavedResets>(&json).unwrap(), saved);
    }

    #[test]
    fn raw_used_percent_is_absent_from_unrelaxed_windows_and_camel_case_when_present() {
        // Unrelaxed windows (every provider except a relax-eligible codex read)
        // must serialize byte-identically to before the field existed.
        let plain = RateWindow {
            used_percent: 41.0,
            raw_used_percent: None,
            resets_at: None,
            window_minutes: None,
        };
        let json = serde_json::to_string(&plain).unwrap();
        assert_eq!(json, r#"{"usedPercent":41.0}"#);

        // A relaxed window carries the provider-reported truth camelCased, and
        // consumers that predate the field can still decode the entry.
        let relaxed = RateWindow {
            used_percent: 0.0,
            raw_used_percent: Some(49.0),
            resets_at: None,
            window_minutes: None,
        };
        let json = serde_json::to_string(&relaxed).unwrap();
        assert_eq!(json, r#"{"usedPercent":0.0,"rawUsedPercent":49.0}"#);
        let decoded: RateWindow = serde_json::from_str(r#"{"usedPercent":7.0}"#).unwrap();
        assert_eq!(decoded.raw_used_percent, None);
    }

    #[test]
    fn balance_seam_round_trips_when_present() {
        // When the future axis is populated it serializes camelCase and survives a
        // round-trip — proving adding the balance axis later is non-breaking.
        let balance = Balance {
            remaining: 12.5,
            total: Some(20.0),
            unit: "USD".to_string(),
            kind: BalanceKind::Currency,
        };
        let json = serde_json::to_string(&balance).unwrap();
        assert!(json.contains("\"kind\":\"currency\""), "{json}");
        let decoded: Balance = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, balance);
    }
}
