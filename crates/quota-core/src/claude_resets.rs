//! Claude saved resets: what each one restores, and the pieces of spending one.
//!
//! Anthropic documents none of this. The shapes and rules below were read from
//! Claude Code 2.1.318's bundled client, which reads the same `cedar_ember`
//! block from `/api/oauth/usage` and redeems through
//! `POST /api/organizations/{organization}/reset_rate_limits`.
//!
//! A grant's `id` is a redemption handle: whoever holds it can spend the
//! reset. It is kept in memory only, for the request that spends it, and never
//! reaches the wire, a log line or the journal. The same goes for `label`
//! (the account's own text) and the block's `next_grant_id`.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::codex_resets::{ConsumeOutcome, SavedResetError};
use crate::model::saved_reset_kind;
use crate::provider::FetchError;

/// The window keys Claude Code keeps from a grant's `clears` and `blocking`
/// lists. Anything else in those lists is dropped before a kind is chosen.
const WINDOW_KEYS: [&str; 8] = [
    "five_hour",
    "seven_day",
    "seven_day_overage_included",
    "seven_day_opus",
    "seven_day_sonnet",
    "seven_day_cowork",
    "seven_day_omelette",
    "seven_day_oauth_apps",
];

/// The kinds a caller may ask to spend, in the order they are published.
pub const KINDS: [&str; 4] = [
    saved_reset_kind::SESSION,
    saved_reset_kind::WEEKLY,
    saved_reset_kind::FULL,
    saved_reset_kind::MODEL_WEEKLY,
];

/// The published kind named `kind`, or `None` for a word this module does not
/// spend.
pub fn known_kind(kind: &str) -> Option<&'static str> {
    KINDS.iter().copied().find(|known| *known == kind)
}

/// What one grant restores, by Claude Code's own rule.
///
/// A grant that gives back the weekly model limit is Claude Code's "fable"
/// kind, published as `model_weekly`. Otherwise a stated `type` decides.
/// Without one, the windows it clears do: only the five-hour window is a
/// session reset, weekly windows without the five-hour one are a weekly reset,
/// and anything else (both, or nothing recognisable) is a full reset. A `type`
/// this module does not know falls through to the `clears` rule rather than
/// becoming a kind nobody can spend.
pub(crate) fn grant_kind(grant: &Value) -> &'static str {
    if grant.get("gives_back_weekly").and_then(Value::as_bool) == Some(true) {
        return saved_reset_kind::MODEL_WEEKLY;
    }
    match grant.get("type").and_then(Value::as_str) {
        Some("session") => return saved_reset_kind::SESSION,
        Some("weekly") => return saved_reset_kind::WEEKLY,
        Some("full") => return saved_reset_kind::FULL,
        _ => {}
    }
    let clears: Vec<&str> = grant
        .get("clears")
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .filter(|key| WINDOW_KEYS.contains(key))
                .collect()
        })
        .unwrap_or_default();
    let has_five_hour = clears.contains(&"five_hour");
    if !clears.is_empty() && clears.iter().all(|key| *key == "five_hour") {
        saved_reset_kind::SESSION
    } else if !clears.is_empty() && !has_five_hour {
        saved_reset_kind::WEEKLY
    } else {
        saved_reset_kind::FULL
    }
}

/// Claude's grant and next-grant ids: `^[a-z0-9_-]{1,40}$`.
fn valid_grant_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Claude's redemption request ids: `^[A-Za-z0-9_-]{1,64}$`.
pub fn valid_request_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One grant as held for redemption. Its `Debug` leaves the id out.
#[derive(Clone)]
pub(crate) struct HeldGrant {
    pub(crate) id: String,
    pub(crate) kind: &'static str,
    pub(crate) resets_left: i64,
    pub(crate) starts_at: Option<DateTime<Utc>>,
    pub(crate) ends_at: Option<DateTime<Utc>>,
    pub(crate) paused: bool,
    pub(crate) usable_now: bool,
    pub(crate) use_requires_limit: bool,
    pub(crate) unavailable_reason: Option<String>,
}

impl std::fmt::Debug for HeldGrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HeldGrant")
            .field("id", &"<redacted>")
            .field("kind", &self.kind)
            .field("resets_left", &self.resets_left)
            .field("starts_at", &self.starts_at)
            .field("ends_at", &self.ends_at)
            .field("paused", &self.paused)
            .field("usable_now", &self.usable_now)
            .field("use_requires_limit", &self.use_requires_limit)
            .field("unavailable_reason", &self.unavailable_reason)
            .finish()
    }
}

impl HeldGrant {
    /// Whether Claude said, at the last poll, that this grant can be spent now.
    fn available(&self, now: DateTime<Utc>) -> bool {
        !self.paused
            && self.resets_left > 0
            && self.usable_now
            && self.unavailable_reason.is_none()
            && self.starts_at.is_none_or(|start| start <= now)
            && self.ends_at.is_none_or(|end| end > now)
    }
}

/// One account's grants as its latest poll stated them.
#[derive(Clone, Debug, Default)]
pub(crate) struct HeldResets {
    pub(crate) grants: Vec<HeldGrant>,
    /// Claude refuses any grant but this one with `not_next_grant` when set.
    pub(crate) next_grant_id: Option<String>,
}

/// The grants of a readable, eligible `cedar_ember` block. A grant whose id or
/// shape is not what Claude Code accepts is left out, so it can never be
/// spent; an ineligible or unreadable block holds nothing.
pub(crate) fn held_resets(raw: Option<&Value>) -> Option<HeldResets> {
    let raw = raw?;
    if raw.get("eligible").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let grants = raw
        .get("grants")
        .and_then(Value::as_array)?
        .iter()
        .take(200)
        .filter_map(held_grant)
        .collect();
    let next_grant_id = raw
        .get("next_grant_id")
        .and_then(Value::as_str)
        .filter(|id| valid_grant_id(id))
        .map(str::to_string);
    Some(HeldResets {
        grants,
        next_grant_id,
    })
}

fn held_grant(raw: &Value) -> Option<HeldGrant> {
    let grant = raw.as_object()?;
    let id = grant
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_grant_id(id))?;
    let bound = |field: &str| match grant.get(field) {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(raw)) => DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|date| Some(date.with_timezone(&Utc))),
        Some(_) => None,
    };
    let unavailable_reason = match grant.get("unavailable_reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(reason)) => Some(bounded_word(reason)),
        Some(_) => Some("unreadable".to_string()),
    };
    Some(HeldGrant {
        id: id.to_string(),
        kind: grant_kind(raw),
        resets_left: grant
            .get("resets_left")
            .and_then(Value::as_i64)
            .filter(|left| *left >= 0)?,
        starts_at: bound("starts_at")?,
        ends_at: bound("ends_at")?,
        paused: grant.get("paused").and_then(Value::as_bool)?,
        // An absent `usable_now` is not a promise that the grant is usable.
        usable_now: grant
            .get("usable_now")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        use_requires_limit: grant
            .get("use_requires_limit")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        unavailable_reason,
    })
}

/// The grant to spend for `kind`: of the grants of that kind available now,
/// `next_grant_id` when it is one of them, since Claude refuses any other with
/// `not_next_grant`; otherwise the one that expires soonest, a grant with no
/// expiry last. `None` when no grant of that kind is available.
pub(crate) fn choose_grant<'a>(
    held: &'a HeldResets,
    kind: &str,
    now: DateTime<Utc>,
) -> Option<&'a HeldGrant> {
    let mut candidates: Vec<&HeldGrant> = held
        .grants
        .iter()
        .filter(|grant| grant.kind == kind && grant.available(now))
        .collect();
    if let Some(next) = held.next_grant_id.as_deref() {
        if let Some(grant) = candidates.iter().find(|grant| grant.id == next) {
            return Some(grant);
        }
    }
    // Sorted by id after expiry only so the choice is the same every time.
    candidates.sort_by(|left, right| {
        let expiry = |grant: &HeldGrant| (grant.ends_at.is_none(), grant.ends_at);
        expiry(left)
            .cmp(&expiry(right))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.first().copied()
}

/// The JSON body of a redemption request.
pub(crate) fn redeem_body(grant_id: &str, request_id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "program": "cedar_ember",
        "grant_id": grant_id,
        "request_id": request_id,
    }))
    .expect("a JSON object of strings always encodes")
}

#[derive(Deserialize)]
struct RedeemReply {
    result: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    cooldown_until: Option<String>,
}

/// A reason word as published: Claude's codes are short snake_case words, and
/// anything else is replaced rather than echoed.
fn bounded_word(word: &str) -> String {
    let readable = (1..=40).contains(&word.len())
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if readable {
        word.to_string()
    } else {
        "unrecognized".to_string()
    }
}

/// Map a 2xx redemption reply to the published outcome.
///
/// `already_used` is the same request id answered before; `not_limited` means
/// the grant only works while its limit is hit, which it is not, so nothing
/// was reset. `cooldown` is the account having reset recently. `ineligible`
/// and `unavailable` are decided refusals and carry Claude's reason code. A
/// reply this module cannot read is a refusal quoting its excerpt.
pub(crate) fn map_reply(body: &[u8]) -> Result<ConsumeOutcome, SavedResetError> {
    let Ok(reply) = serde_json::from_slice::<RedeemReply>(body) else {
        return Err(SavedResetError::ProviderRefused(
            crate::http::refusal_excerpt(body),
        ));
    };
    match reply.result.as_str() {
        "reset" => Ok(ConsumeOutcome::Reset),
        "already_used" => Ok(ConsumeOutcome::AlreadyRedeemed),
        "not_limited" => Ok(ConsumeOutcome::NothingToReset),
        "cooldown" => Err(SavedResetError::ProviderCooldown(
            reply
                .cooldown_until
                .as_deref()
                .and_then(|until| DateTime::parse_from_rfc3339(until).ok())
                .map(|until| until.with_timezone(&Utc)),
        )),
        result @ ("ineligible" | "unavailable") => Err(SavedResetError::ProviderDeclined {
            result: result.to_string(),
            reason: reply.reason.as_deref().map(bounded_word),
        }),
        _ => Err(SavedResetError::ProviderRefused(
            crate::http::refusal_excerpt(body),
        )),
    }
}

/// Map a failed redemption request. A 429, a 5xx or a transport failure may
/// clear on a retry of the same request id; any other refusal (401 and 403
/// included) is the provider's answer.
pub(crate) fn map_failure(error: FetchError) -> SavedResetError {
    match error {
        FetchError::ProviderStatus(429, excerpt) => {
            SavedResetError::ProviderUnavailable(format!("HTTP 429: {excerpt}"))
        }
        FetchError::ProviderStatus(status, excerpt) if status < 500 => {
            SavedResetError::ProviderRefused(excerpt)
        }
        FetchError::Unauthorized(excerpt) => SavedResetError::ProviderRefused(excerpt),
        error => SavedResetError::ProviderUnavailable(error.to_string()),
    }
}

#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
#[path = "claude_resets/mock.rs"]
pub mod mock;

#[cfg(test)]
#[path = "claude_resets/tests.rs"]
mod tests;
