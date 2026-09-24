//! Anthropic (claude) usage fetcher — the OAuth-bearer archetype, 2nd instance.
//!
//! This is the deliberate 2nd spike: it shares codex's "OAuth bearer → one GET →
//! decode JSON" skeleton but differs on every detail that could break the
//! abstraction, which is exactly why it validates it:
//!   - Implicit-local session source: opencode's unified auth.json (`anthropic`
//!     OAuth entry), NOT a provider-native file. Vault handles use the bare bearer
//!     bytes served by the injected credential source. CodexBar reads the macOS
//!     Keychain; we prefer
//!     opencode's cross-platform store, which already holds the same token.
//!   - Endpoint: `GET https://api.anthropic.com/api/oauth/usage` with the beta
//!     header `anthropic-beta: oauth-2025-04-20` and a `claude-code/<ver>` UA.
//!   - Response: NAMED windows (`five_hour`, `seven_day`, `seven_day_sonnet`, ...)
//!     where `utilization` is ALREADY a 0-100 percent and `resets_at` is ALREADY
//!     ISO 8601 — unlike codex's int-percent + epoch. So normalization is a
//!     near-passthrough here, mapping window names to known window lengths.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde::Deserialize;

use crate::credential_source::CredentialSource;
#[cfg(test)]
use crate::credential_source::VaultCapability;
use crate::provider::{AccountObservation, CredentialHandle, FetchAttempt};
use crate::vault_handles::VaultHandleLoader;
use crate::LOG_TAG;
use crate::{
    http::{Header, JsonRequest},
    model::{Amount, Pool, PoolBasis, PoolFunding, RateWindow, Usage},
    opencode_auth::{self, OpencodeAuth},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "claude";
const OPENCODE_PROVIDER: &str = "anthropic";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// OPAQUE-UPSTREAM-CONSTANT: copied from the upstream, unvalidatable here.
///
/// Dated opt-in header. Dated values get superseded, and the supersession is
/// invisible here until a request is refused.
const BETA_HEADER: &str = "oauth-2025-04-20";
const CLAUDE_CODE_UA: &str = "claude-code/2.1.0";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const FIVE_HOUR_MINUTES: i64 = 5 * 60;
const SEVEN_DAY_MINUTES: i64 = 7 * 24 * 60;

/// One named window in the response. `utilization` is already a 0-100 percent.
#[derive(Debug, Deserialize)]
struct OAuthWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// The `/api/oauth/usage` response (the windows we normalize).
#[derive(Debug, Deserialize)]
struct OAuthUsageResponse {
    five_hour: Option<OAuthWindow>,
    seven_day: Option<OAuthWindow>,
    seven_day_opus: Option<OAuthWindow>,
    seven_day_sonnet: Option<OAuthWindow>,
    limits: Option<Vec<ApiLimitEntry>>,
    /// Paid overage ("extra usage"), read into a spend pool by [`overage_pool`].
    ///
    /// Held as raw JSON and decoded separately, so a shape this decoder does not
    /// expect costs the pool and never the rate windows above. Money is the
    /// secondary fact on this endpoint; the windows are the primary one.
    spend: Option<serde_json::Value>,
    /// Only `spend_limit_reached` is read from here; see [`overage_pool`].
    extra_usage: Option<serde_json::Value>,
}

/// One money figure in the `spend` object: integer minor units, a currency, and
/// the number of decimal places the minor units carry.
#[derive(Debug, Deserialize)]
struct SpendAmount {
    amount_minor: Option<i64>,
    currency: Option<String>,
    exponent: Option<u8>,
}

/// The `spend` object, as observed on an account with extra usage enabled. It
/// states no period and no reset, so the overage it describes is a pool rather
/// than a window.
#[derive(Debug, Deserialize)]
struct OverageSpend {
    used: Option<SpendAmount>,
    limit: Option<SpendAmount>,
    enabled: Option<bool>,
}

/// The one field read from `extra_usage`.
#[derive(Debug, Deserialize)]
struct ExtraUsage {
    spend_limit_reached: Option<bool>,
}

/// Why a `spend` the provider did send was not turned into a pool.
///
/// Kept apart from "nothing stated" (`spend` absent or `null`) so the two kinds
/// of silence can be told apart in the log. Every variant still publishes no
/// pool; only the diagnostic differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefusalReason {
    /// `spend` is not an object of the expected field types.
    Unparseable,
    /// `limit` is absent, or lacks its amount, currency or exponent.
    LimitMissing,
    /// `used` is absent, or lacks its amount, currency or exponent.
    UsedMissing,
    /// `limit` or `used` carries an amount below zero.
    NegativeAmount,
    /// `limit` and `used` are in different currencies.
    CurrencyMismatch,
    /// `limit` and `used` carry a different number of decimal places.
    ExponentMismatch,
}

impl RefusalReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unparseable => "unparseable",
            Self::LimitMissing => "limit missing",
            Self::UsedMissing => "used missing",
            Self::NegativeAmount => "negative amount",
            Self::CurrencyMismatch => "currency mismatch",
            Self::ExponentMismatch => "exponent mismatch",
        }
    }
}

/// A present `spend` that was refused: the reason, plus the sorted key names of
/// the object so the log can show its shape. Key names only, never values, so
/// no amount or provider text ever reaches the log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OverageRefusal {
    reason: RefusalReason,
    spend_keys: Vec<String>,
}

/// What is wrong with one money figure, before it is known whether it is the
/// limit or the usage.
enum AmountProblem {
    Missing,
    Negative,
}

/// A usable money figure: every part present and the amount not negative.
fn spend_amount(amount: Option<SpendAmount>) -> Result<(i64, u8, String), AmountProblem> {
    let amount = amount.ok_or(AmountProblem::Missing)?;
    let minor = amount.amount_minor.ok_or(AmountProblem::Missing)?;
    let exponent = amount.exponent.ok_or(AmountProblem::Missing)?;
    let currency = amount.currency.ok_or(AmountProblem::Missing)?;
    if minor < 0 {
        return Err(AmountProblem::Negative);
    }
    Ok((minor, exponent, currency))
}

/// Build the paid-overage pool, or say why there is none.
///
/// `Ok(None)` means the provider stated nothing (`spend` absent or `null`);
/// `Err` means it sent a `spend` this code cannot use. Neither is an error to
/// the fetch, because failing the fetch would take the rate windows down with
/// it: both publish no pool. That includes an account without extra usage: its
/// `spend` shape has not been observed, so rather than guess at it, anything
/// without a readable `limit` publishes no pool.
fn overage_pool(
    spend: Option<serde_json::Value>,
    extra_usage: Option<serde_json::Value>,
) -> Result<Option<Pool>, OverageRefusal> {
    let spend = match spend {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let mut spend_keys: Vec<String> = spend
        .as_object()
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default();
    spend_keys.sort();
    let refuse = |reason| OverageRefusal {
        reason,
        spend_keys: spend_keys.clone(),
    };
    let spend: OverageSpend =
        serde_json::from_value(spend).map_err(|_| refuse(RefusalReason::Unparseable))?;
    let (limit, limit_exponent, limit_unit) = spend_amount(spend.limit).map_err(|problem| {
        refuse(match problem {
            AmountProblem::Missing => RefusalReason::LimitMissing,
            AmountProblem::Negative => RefusalReason::NegativeAmount,
        })
    })?;
    let (used, used_exponent, used_unit) = spend_amount(spend.used).map_err(|problem| {
        refuse(match problem {
            AmountProblem::Missing => RefusalReason::UsedMissing,
            AmountProblem::Negative => RefusalReason::NegativeAmount,
        })
    })?;
    // The remainder is only meaningful when both figures are in the same unit at
    // the same scale. Nothing is converted: a mismatch publishes no pool.
    if limit_unit != used_unit {
        return Err(refuse(RefusalReason::CurrencyMismatch));
    }
    if limit_exponent != used_exponent {
        return Err(refuse(RefusalReason::ExponentMismatch));
    }
    // Overage can run past its limit; a pool never goes below empty.
    let remaining = limit.saturating_sub(used).max(0);
    let spend_limit_reached = extra_usage
        .and_then(|value| serde_json::from_value::<ExtraUsage>(value).ok())
        .and_then(|extra| extra.spend_limit_reached);
    // Read from the provider's own switches, never from `remaining`: a pool with
    // money left can still be switched off. When either switch is missing the
    // answer is left unstated rather than guessed.
    let spendable = match (spend.enabled, spend_limit_reached) {
        (Some(enabled), Some(reached)) => Some(enabled && !reached),
        _ => None,
    };
    Ok(Some(Pool {
        id: "extra_usage".to_string(),
        label: "Extra usage".to_string(),
        // Overage is billed after it is used. None of granted, purchased or
        // subscription says that (purchased would claim it was bought in
        // advance), and there is no post-paid kind to use instead. `Unknown` is
        // the value every consumer already reads conservatively.
        funding: PoolFunding::Unknown,
        remaining: Some(Amount {
            minor: remaining,
            exponent: limit_exponent,
            unit: limit_unit.clone(),
        }),
        total: Some(Amount {
            minor: limit,
            exponent: limit_exponent,
            unit: limit_unit,
        }),
        // A limit minus a usage figure, not a remainder the provider states.
        basis: PoolBasis::Derived,
        spendable,
    }))
}

/// Whether a handle's refusal state change deserves a log line.
///
/// The provider polls every account every minute, so logging each refusal
/// would repeat the same line forever. Instead a line is written when a refusal
/// first appears, when its reason changes, and when it clears (the `spend`
/// became readable or stopped being stated). `None` means not refused.
fn refusal_needs_log(previous: Option<RefusalReason>, current: Option<RefusalReason>) -> bool {
    previous != current
}

#[derive(Debug, Deserialize)]
struct ApiLimitEntry {
    kind: Option<String>,
    group: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<ApiLimitScope>,
}

#[derive(Debug, Deserialize)]
struct ApiLimitScope {
    model: Option<ApiLimitModel>,
}

#[derive(Debug, Deserialize)]
struct ApiLimitModel {
    id: Option<String>,
    display_name: Option<String>,
}

fn slug(value: &str) -> String {
    let mut result = String::new();
    let mut last_was_dash = false;
    for character in value.trim().to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            last_was_dash = false;
        } else if !last_was_dash {
            result.push('-');
            last_was_dash = true;
        }
    }
    result.trim_matches('-').to_string()
}

fn is_all_models_scope(model_id: Option<&str>, model_name: &str) -> bool {
    if slug(model_name) == "all-models" {
        return true;
    }
    let Some(model_id) = model_id else {
        return false;
    };
    let id_slug = slug(model_id);
    id_slug == "all-models" || id_slug.ends_with("-all-models")
}

fn scoped_weekly_extras(
    limits: Option<&[ApiLimitEntry]>,
) -> Option<Vec<crate::model::ExtraWindow>> {
    let mut seen = HashSet::new();
    let extras: Vec<_> = limits
        .into_iter()
        .flatten()
        // Match CodexBar's ClaudeScopedWeeklyLimitMapper: filter on group +
        // kind only. Do NOT filter on `is_active` — that flag marks which single
        // limit is *currently binding* (the tightest one), not whether a window
        // is valid to show. A Fable window with `is_active:false` still carries a
        // real percent and reset and must be reported (the account has that
        // weekly limit); dropping it hides Fable for every account not currently
        // walled on Fable. The named `seven_day`/`five_hour` windows are shown
        // regardless of their own `is_active`, so honoring it only here was an
        // inconsistency, not a rule.
        .filter(|entry| {
            entry.kind.as_deref() == Some("weekly_scoped")
                && entry.group.as_deref().unwrap_or("weekly") == "weekly"
        })
        .filter_map(|entry| {
            let model = entry.scope.as_ref()?.model.as_ref()?;
            let display_name = model
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())?;
            let model_id = model
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty());
            if is_all_models_scope(model_id, display_name) {
                return None;
            }
            let identity = model_id.unwrap_or(display_name).to_string();
            let percent = entry.percent?;
            if !percent.is_finite() || !seen.insert(identity.clone()) {
                return None;
            }
            Some(crate::model::ExtraWindow {
                title: Some(format!("7 Day ({display_name})")),
                id: Some(identity),
                window: Some(RateWindow {
                    used_percent: percent.clamp(0.0, 100.0),
                    raw_used_percent: None,
                    resets_at: entry.resets_at.clone(),
                    window_minutes: Some(SEVEN_DAY_MINUTES),
                    used_count: None,
                    total_count: None,
                    regeneration: None,
                }),
            })
        })
        .collect();
    (!extras.is_empty()).then_some(extras)
}

fn to_window(window: Option<&OAuthWindow>, window_minutes: i64) -> Option<RateWindow> {
    let window = window?;
    // CodexBar's makeWindow (ClaudeUsageFetcher.swift:945-956) builds a window
    // from `utilization` alone and leaves resetsAt nil when absent — an idle
    // session window reports `utilization: 0.0, resets_at: null` (nothing pending
    // to reset), and CodexBar shows it. So require only the percent; carry the
    // reset through when present, omit it otherwise. Never fabricate a reset.
    let used_percent = window.utilization?;
    Some(RateWindow {
        used_percent,
        raw_used_percent: None,
        resets_at: window.resets_at.clone(),
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
    })
}

/// Normalize the `/api/oauth/usage` body to [`Usage`]. Pure — unit-testable.
///
/// Mapping: `five_hour` → primary (the session window), `seven_day` → secondary
/// (the weekly all-models window), and the model-scoped weekly (`seven_day_opus`
/// preferred, else `seven_day_sonnet`) → tertiary. Account-wide windows only;
/// per-model routing is a later concern (the consumer's extractor handles that).
///
/// **This is where slot holes come from.** Each slot is filled from its own
/// independent optional field, so an account with a model-scoped weekly limit and
/// no all-models one emits `secondary: None` beside `tertiary: Some(..)`. The
/// three slots are positions, not a ranking, and nothing here fills a gap or
/// compacts the rest upward.
///
/// Anything walking these slots must therefore visit all three rather than stop
/// at the first absent one — see [`crate::model::windows`], which exists to make
/// that structural. A walk written as a search rather than a filter reads a
/// sparse account as having less usage than it does, and a consumer that stops
/// early reports the wrong constraint as binding.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    normalize_response(body).map(|(usage, _)| usage)
}

/// Normalize the body to its rate windows and the paid-overage pool, if any.
///
/// The pool never fails this call: see [`overage_pool`].
fn normalize_response(body: &[u8]) -> Result<(Usage, Option<Pool>), FetchError> {
    normalize_with_overage(body).map(|(usage, overage)| (usage, overage.unwrap_or(None)))
}

/// [`normalize_response`], keeping the reason a present `spend` was refused so
/// the provider can log it.
fn normalize_with_overage(
    body: &[u8],
) -> Result<(Usage, Result<Option<Pool>, OverageRefusal>), FetchError> {
    let response: OAuthUsageResponse = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("anthropic usage not decodable: {e}")))?;
    let pool = overage_pool(response.spend, response.extra_usage);
    let usage = Usage {
        primary: to_window(response.five_hour.as_ref(), FIVE_HOUR_MINUTES),
        secondary: to_window(response.seven_day.as_ref(), SEVEN_DAY_MINUTES),
        tertiary: to_window(
            response
                .seven_day_opus
                .as_ref()
                .or(response.seven_day_sonnet.as_ref()),
            SEVEN_DAY_MINUTES,
        ),
        extra_rate_windows: scoped_weekly_extras(response.limits.as_deref()),
    };
    Ok((usage, pool))
}

/// A successful attempt carrying the windows and, when there is one, the pool.
///
/// No pool leaves `pools` absent rather than empty: an empty list would state
/// that the provider reports no pools, which an unreadable or unobserved
/// `spend` does not establish.
fn success_attempt(
    observed: Option<AccountObservation>,
    source: &str,
    usage: Usage,
    pool: Option<Pool>,
) -> FetchAttempt {
    let mut attempt = FetchAttempt::success(observed, source, usage);
    attempt.pools = pool.map(|pool| vec![pool]);
    attempt
}

fn canonical_account_id(account_id: Option<String>) -> Option<String> {
    account_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn usage_request(url: &str, bearer: &str) -> JsonRequest {
    JsonRequest::get(url)
        .timeout(REQUEST_TIMEOUT)
        .bearer(bearer)
        .header(Header::new("anthropic-beta", BETA_HEADER))
        .header(Header::new("User-Agent", CLAUDE_CODE_UA))
}

/// The anthropic usage provider.
pub struct AnthropicProvider {
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    usage_url: String,
    /// The last `spend` refusal logged per handle stable id; a handle whose
    /// `spend` is readable or unstated has no entry. See [`refusal_needs_log`].
    overage_refusals: Mutex<HashMap<String, RefusalReason>>,
}

impl AnthropicProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
            usage_url: USAGE_URL.to_string(),
            overage_refusals: Mutex::new(HashMap::new()),
        }
    }

    /// Log a handle's `spend` refusal when it first appears, changes, or clears,
    /// and hand back the pool to publish (none when refused).
    fn settle_overage(
        &self,
        handle_id: &str,
        overage: Result<Option<Pool>, OverageRefusal>,
    ) -> Option<Pool> {
        let current = overage.as_ref().err().map(|refusal| refusal.reason);
        let mut refusals = self
            .overage_refusals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = refusals.get(handle_id).copied();
        if refusal_needs_log(previous, current) {
            match &overage {
                Err(refusal) => eprintln!(
                    "{LOG_TAG} warning: anthropic spend refused ({handle_id}): {}; spend keys [{}]; no overage pool published",
                    refusal.reason.as_str(),
                    refusal.spend_keys.join(", ")
                ),
                Ok(_) => eprintln!(
                    "{LOG_TAG} anthropic spend no longer refused ({handle_id})"
                ),
            }
        }
        match current {
            Some(reason) => refusals.insert(handle_id.to_string(), reason),
            None => refusals.remove(handle_id),
        };
        overage.unwrap_or(None)
    }

    fn report_auth_failure(
        &self,
        handle: &CredentialHandle,
        record_version: u64,
        error: &FetchError,
    ) {
        crate::credential_source::report_vault_auth_failure(
            self.credential_source.as_ref(),
            handle,
            record_version,
            error,
        );
    }

    async fn fetch_local_bearer(&self, handle_id: &str, bearer: &str) -> FetchAttempt {
        let result = usage_request(&self.usage_url, bearer)
            .send(&self.http)
            .await
            .and_then(|body| normalize_with_overage(&body));
        match result {
            Ok((usage, overage)) => success_attempt(
                Some(AccountObservation::new(None, None)),
                "oauth",
                usage,
                self.settle_overage(handle_id, overage),
            ),
            Err(error) => FetchAttempt::failure(None, None, error),
        }
    }

    async fn fetch_vault(&self, handle: &CredentialHandle) -> FetchAttempt {
        let handle_id = handle.stable_id();
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match crate::credential_source::get_vault_credential(
            credential_source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => {
                // Name the failure class: the F1 fence collapses any get error into
                // "unverified", so without this the cause (dead handle vs transient
                // vs fail-closed timeout) is invisible when a lane goes dark.
                eprintln!(
                    "{LOG_TAG} warning: anthropic vault credential.get failed ({handle_id}): {error:?}"
                );
                return FetchAttempt::unverified_vault_failure(error);
            }
        };
        let record_version = credential.record_version;
        let account_info = credential.account_info();
        let observed = Some(AccountObservation::new(
            canonical_account_id(credential.account_id.clone()),
            Some(record_version),
        ));
        let bearer = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(value) => value,
            Err(error) => return FetchAttempt::failure(observed, None, error),
        };

        let result = usage_request(&self.usage_url, &bearer)
            .send_provider_status_first(&self.http, PROVIDER_NAME)
            .await
            .map(|response| response.body)
            .and_then(|body| normalize_with_overage(&body));
        if let Err(error) = &result {
            self.report_auth_failure(handle, record_version, error);
        }
        match result {
            Ok((usage, overage)) => {
                let pool = self.settle_overage(handle_id, overage);
                success_attempt(observed, "vault", usage, pool).with_account_info(account_info)
            }
            Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error),
        }
    }
}

#[async_trait]
impl UsageProvider for AnthropicProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.anthropic_handles()?;
            if !vault.is_empty() {
                // Vault-only custody once anthropic vault handles exist: the
                // implicit-local lane (opencode auth.json) can never resolve an
                // account_id (opaque token, no identity in the usage payload),
                // so keeping it alongside labeled vault lanes would force the
                // emission gate to collapse every account into one unlabeled
                // entry. The vault also owns refresh, so its lanes strictly
                // dominate the local token for availability.
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }

        let access = match opencode_auth::read_provider(OPENCODE_PROVIDER) {
            Ok(Some(OpencodeAuth::Oauth { access, .. })) => access,
            Ok(Some(OpencodeAuth::Api { key })) => key,
            Ok(None) => {
                return FetchAttempt::failure(
                    None,
                    None,
                    FetchError::NoSession("no anthropic entry in opencode auth.json".to_string()),
                );
            }
            Err(error) => return FetchAttempt::failure(None, None, error),
        };
        self.fetch_local_bearer(handle.stable_id(), &access).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::credential_source::{VaultCredential, VaultGetError};
    use crate::provider::CredentialResolution;
    use crate::refresh::{next_slot_after_attempt, Incarnation, ProviderSlot};

    type Reports = Arc<Mutex<Vec<(u16, u64)>>>;

    struct MockCredentialSource {
        get_result: Result<VaultCredential, VaultGetError>,
        reports: Reports,
    }

    #[async_trait]
    impl CredentialSource for MockCredentialSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            assert_eq!(min_ttl_ms, 120_000);
            self.get_result.clone()
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            provider_status: u16,
            record_version: u64,
        ) {
            self.reports
                .lock()
                .unwrap()
                .push((provider_status, record_version));
        }
    }

    fn source(
        get_result: Result<VaultCredential, VaultGetError>,
    ) -> (Arc<dyn CredentialSource>, Reports) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(MockCredentialSource {
                get_result,
                reports: Arc::clone(&reports),
            }),
            reports,
        )
    }

    fn credential(payload: &[u8], record_version: u64) -> VaultCredential {
        VaultCredential {
            payload: payload.to_vec(),
            expires_at_ms: None,
            record_version,
            account_id: Some("   ".to_string()),
            email: None,
            org_name: None,
            project_id: None,
        }
    }

    fn test_provider(source: Arc<dyn CredentialSource>, usage_url: String) -> AnthropicProvider {
        let mut provider = AnthropicProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::new(None)),
        );
        provider.usage_url = usage_url;
        provider
    }

    struct VaultOnlyProvider {
        provider: AnthropicProvider,
        handle: CredentialHandle,
    }

    #[async_trait]
    impl UsageProvider for VaultOnlyProvider {
        fn name(&self) -> &str {
            PROVIDER_NAME
        }

        fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
            Ok(vec![self.handle.clone()])
        }

        async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
            self.provider.fetch_handle(handle).await
        }
    }

    /// Serve one request and hand back what was sent, at this provider's path.
    ///
    /// Wraps the shared helper so the URL keeps the shape this provider
    /// builds against. The shared reader is used because a single socket read
    /// returns one TCP segment rather than the whole request, which makes any
    /// assertion about what was NOT sent pass without reading it.
    async fn serve_once(status: u16, body: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let (base, task) = crate::loopback::serve_once(status, body).await;
        (format!("{base}/usage"), task)
    }

    #[test]
    fn vault_handles_replace_the_implicit_local_lane() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[
            ("oauth:anthropic", "oauth"),
            ("oauth:anthropic:ufuk2", "oauth"),
            ("oauth:xai", "oauth"),
        ]);
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = AnthropicProvider::new_with_handle_loader(Some(source), loader);
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 2);
        assert!(handles.iter().all(CredentialHandle::is_vault));
        assert_eq!(handles[0].stable_id(), "oauth:anthropic");
        assert_eq!(handles[1].stable_id(), "oauth:anthropic:ufuk2");
    }

    #[test]
    fn implicit_local_lane_survives_when_no_vault_handles_are_mapped() {
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = AnthropicProvider::new_with_handle_loader(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
        );
        assert_eq!(
            provider.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    #[tokio::test]
    async fn vault_happy_path_uses_served_bearer_and_record_version() {
        let body = br#"{"five_hour":{"utilization":12.0,"resets_at":null}}"#.to_vec();
        let (url, request) = serve_once(200, body).await;
        let mut vault_credential = credential(b"anthropic-vault-token", 27);
        vault_credential.email = Some("user@example.com".to_string());
        vault_credential.org_name = Some("Example Org".to_string());
        let (source, _) = source(Ok(vault_credential));
        let provider = test_provider(source, url);
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;

        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt.observed.unwrap(),
            AccountObservation::new(None, Some(27))
        );
        let account_info = attempt.account_info.as_ref().unwrap();
        assert_eq!(account_info.email.as_deref(), Some("user@example.com"));
        assert_eq!(account_info.org_name.as_deref(), Some("Example Org"));
        assert_eq!(account_info.plan_type, None);
        assert_eq!(attempt.usage.unwrap().primary.unwrap().used_percent, 12.0);
        assert!(request
            .await
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization: bearer anthropic-vault-token"));
    }

    #[tokio::test]
    async fn vault_happy_path_serves_one_unlabeled_entry() {
        let body = br#"{"five_hour":{"utilization":7.0,"resets_at":null}}"#.to_vec();
        let (url, _) = serve_once(200, body).await;
        let (source, _) = source(Ok(credential(b"anthropic-vault-token", 28)));
        let handle =
            CredentialHandle::vault("oauth:anthropic", VaultCapability::new("ckh_anthropic"));
        let registry = crate::Registry::new(vec![Box::new(VaultOnlyProvider {
            provider: test_provider(source, url),
            handle,
        })]);

        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let entries = registry.get_usage(Some(PROVIDER_NAME)).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].account, None);
        assert_eq!(
            entries[0]
                .usage
                .as_ref()
                .unwrap()
                .primary
                .as_ref()
                .unwrap()
                .used_percent,
            7.0
        );
    }

    #[tokio::test]
    async fn failed_get_is_unverified_and_clears_prior_observation() {
        let (source, _) = source(Err(VaultGetError::Transient));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Unverified
        );

        let now = std::time::Instant::now();
        let cold = ProviderSlot::due_now(now, Incarnation::from_counter(1));
        let prior = next_slot_after_attempt(
            &cold,
            PROVIDER_NAME,
            FetchAttempt::success(
                Some(AccountObservation::new(
                    Some("prior-account".to_string()),
                    Some(1),
                )),
                "vault",
                Usage::default(),
            ),
            now,
            now,
        );
        let next = next_slot_after_attempt(&prior, PROVIDER_NAME, attempt, now, now);
        // The prior account's USAGE must not survive an unverified identity. The
        // entry itself does survive, as a verdict carrying no account and no
        // windows: dropping it too publishes the ABSENT shape for a credential
        // this module reached and found unusable, which a consumer reads as "not
        // fetched yet" (insula#8, measured).
        let entry = next.entry.as_ref().expect("the verdict stays visible");
        assert!(entry.usage.is_none(), "no window may cross an identity");
        assert!(entry.account.is_none(), "and it attributes nothing");
        assert!(entry.error.is_some(), "the failure is stated, not implied");
        assert!(next.label_in_flux);
        assert!(next.last_success_at.is_none());
    }

    #[tokio::test]
    async fn vault_401_reports_served_version_while_local_keeps_legacy_error() {
        let (vault_url, _) = serve_once(401, Vec::new()).await;
        let (source, reports) = source(Ok(credential(b"anthropic-vault-token", 44)));
        let mut provider = test_provider(Arc::clone(&source), vault_url);
        let vault = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;
        assert!(matches!(
            vault.usage,
            Err(FetchError::ProviderStatus(401, _))
        ));
        for _ in 0..20 {
            if !reports.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*reports.lock().unwrap(), vec![(401, 44)]);

        reports.lock().unwrap().clear();
        let (local_url, _) = serve_once(401, Vec::new()).await;
        provider.usage_url = local_url;
        let local = provider
            .fetch_local_bearer("implicit", "anthropic-local-token")
            .await;
        assert!(matches!(
            local.usage,
            Err(FetchError::Unauthorized(message)) if message == "HTTP 401 (no response body)"
        ));
        tokio::task::yield_now().await;
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_utf8_vault_payload_is_a_verified_decode_failure() {
        let (source, _) = source(Ok(credential(&[0xff, 0xfe], 8)));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let attempt = provider
            .fetch_handle(&CredentialHandle::vault(
                "oauth:anthropic",
                VaultCapability::new("ckh_anthropic"),
            ))
            .await;
        assert_eq!(
            attempt.credential_resolution,
            CredentialResolution::Verified
        );
        assert_eq!(attempt.observed.unwrap().record_version, Some(8));
        assert!(matches!(attempt.usage, Err(FetchError::Decode(_))));
    }

    #[test]
    fn normalizes_real_shaped_payload() {
        // Shaped exactly like the live HTTP 200 we captured: utilization is
        // already a percent, resets_at already ISO8601, named windows.
        let body = br#"{
            "five_hour": { "utilization": 16.0, "resets_at": "2026-06-22T17:00:00.175593+00:00" },
            "seven_day": { "utilization": 48.0, "resets_at": "2026-06-24T14:00:00.175619+00:00" },
            "seven_day_oauth_apps": null,
            "seven_day_opus": null,
            "seven_day_sonnet": { "utilization": 4.0, "resets_at": "2026-06-24T14:00:00.175629+00:00" }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 16.0); // already a percent, NOT /100
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(
            primary.resets_at.as_deref(),
            Some("2026-06-22T17:00:00.175593+00:00")
        );
        assert_eq!(usage.secondary.unwrap().used_percent, 48.0);
        assert!(usage.extra_rate_windows.is_none());
        // opus is null, so tertiary falls back to sonnet.
        assert_eq!(usage.tertiary.unwrap().used_percent, 4.0);
    }

    /// The upstream reports each window independently, so a filled `tertiary`
    /// beside an empty `secondary` is a shape this provider really produces.
    ///
    /// The slots are positions rather than a contiguous list, and this is the
    /// payload that proves it: anything walking them until the first empty one
    /// would miss the 91% window entirely and read this account as idle.
    #[test]
    fn an_absent_weekly_leaves_a_hole_above_a_filled_tertiary() {
        // A live-shaped body with seven_day absent while an opus weekly is
        // present -- each field is independently optional upstream.
        let body = br#"{
            "five_hour": { "utilization": 3.0, "resets_at": "2026-06-22T17:00:00Z" },
            "seven_day": null,
            "seven_day_opus": { "utilization": 91.0, "resets_at": "2026-06-24T14:00:00Z" },
            "seven_day_sonnet": null
        }"#;

        let usage = normalize_usage(body).unwrap();

        assert_eq!(usage.primary.as_ref().unwrap().used_percent, 3.0);
        assert!(usage.secondary.is_none(), "the hole is the point");
        assert_eq!(usage.tertiary.as_ref().unwrap().used_percent, 91.0);

        // What any consumer of this shape must see: the worst window is past the
        // hole, so the account is at 91%, not the 3% its first slot reports.
        let worst = crate::model::windows(&usage)
            .map(|window| window.used_percent)
            .fold(f64::MIN, f64::max);
        assert_eq!(worst, 91.0);
    }

    #[test]
    fn scoped_weekly_limits_become_named_extra_windows() {
        let usage = normalize_usage(
            br#"{
                "five_hour": {"utilization": 12.0, "resets_at": "2026-07-18T12:00:00Z"},
                "seven_day": {"utilization": 60.0, "resets_at": "2026-07-20T12:00:00Z"},
                "limits": [
                    {
                        "kind": "weekly_scoped",
                        "percent": 100.0,
                        "resets_at": "2026-07-18T12:00:00Z",
                        "scope": {"model": {"display_name": "Fable"}},
                        "is_active": true
                    },
                    {
                        "kind": "weekly_scoped",
                        "percent": 20.0,
                        "resets_at": "2026-07-18T12:00:00Z",
                        "scope": {"model": {"display_name": "Fable"}},
                        "is_active": true
                    },
                    {
                        "kind": "weekly_scoped",
                        "percent": 80.0,
                        "scope": {"model": {}},
                        "is_active": true
                    }
                ]
            }"#,
        )
        .unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].title.as_deref(), Some("7 Day (Fable)"));
        assert_eq!(extras[0].id.as_deref(), Some("Fable"));
        let window = extras[0].window.as_ref().unwrap();
        assert_eq!(window.used_percent, 100.0);
        assert_eq!(window.window_minutes, Some(SEVEN_DAY_MINUTES));
    }

    #[test]
    fn inactive_scoped_weekly_window_is_still_reported() {
        // The exact live bug: an account not currently walled on Fable returns
        // its Fable weekly_scoped entry with `is_active: false` and a real
        // percent. It must still be reported — `is_active` marks which limit is
        // currently binding, not whether a window is valid. CodexBar's mapper
        // does not filter on it.
        let usage = normalize_usage(
            br#"{
                "seven_day": {"utilization": 5.0, "resets_at": "2026-07-29T04:00:00Z"},
                "limits": [{
                    "kind": "weekly_scoped",
                    "group": "weekly",
                    "percent": 6.0,
                    "resets_at": "2026-07-29T04:00:00Z",
                    "scope": {"model": {"display_name": "Fable"}},
                    "is_active": false
                }]
            }"#,
        )
        .unwrap();
        let extras = usage.extra_rate_windows.expect("Fable window must be kept");
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].title.as_deref(), Some("7 Day (Fable)"));
        assert_eq!(extras[0].window.as_ref().unwrap().used_percent, 6.0);
    }

    #[test]
    fn all_models_scopes_are_skipped() {
        let usage = normalize_usage(
            br#"{
                "seven_day": {"utilization": 10.0, "resets_at": "2026-07-20T12:00:00Z"},
                "limits": [{
                    "kind": "weekly_scoped",
                    "group": "weekly",
                    "percent": 50.0,
                    "scope": {"model": {"id": "claude-all-models", "display_name": "All Models"}}
                }]
            }"#,
        )
        .unwrap();
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn non_weekly_scopes_are_skipped() {
        let usage = normalize_usage(
            br#"{
                "limits": [{
                    "kind": "weekly_scoped",
                    "group": "session",
                    "percent": 50.0,
                    "scope": {"model": {"id": "fable", "display_name": "Fable"}}
                }]
            }"#,
        )
        .unwrap();
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn absent_group_keeps_weekly_scoped_window() {
        let usage = normalize_usage(
            br#"{
                "limits": [{
                    "kind": "weekly_scoped",
                    "percent": 25.0,
                    "scope": {"model": {"id": "fable-v1", "display_name": "Fable"}}
                }]
            }"#,
        )
        .unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].title.as_deref(), Some("7 Day (Fable)"));
    }

    #[test]
    fn scoped_windows_deduplicate_by_model_id() {
        let usage = normalize_usage(
            br#"{
                "limits": [
                    {
                        "kind": "weekly_scoped", "group": "weekly", "percent": 10.0,
                        "scope": {"model": {"id": "fable-v1", "display_name": "Fable"}}
                    },
                    {
                        "kind": "weekly_scoped", "group": "weekly", "percent": 20.0,
                        "scope": {"model": {"id": "fable-v1", "display_name": "Fable"}}
                    },
                    {
                        "kind": "weekly_scoped", "group": "weekly", "percent": 30.0,
                        "scope": {"model": {"id": "fable-v2", "display_name": "Fable"}}
                    }
                ]
            }"#,
        )
        .unwrap();
        let extras = usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].id.as_deref(), Some("fable-v1"));
        assert_eq!(extras[1].id.as_deref(), Some("fable-v2"));
    }

    #[test]
    fn missing_anthropic_limits_do_not_add_extra_windows() {
        let usage =
            normalize_usage(br#"{"five_hour":{"utilization":1.0,"resets_at":null}}"#).unwrap();
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn window_without_utilization_is_dropped() {
        let body = br#"{ "five_hour": { "resets_at": "2026-06-22T17:00:00Z" } }"#;
        let usage = normalize_usage(body).unwrap();
        assert!(usage.primary.is_none());
    }

    #[test]
    fn idle_zero_percent_window_with_null_reset_is_kept() {
        // The exact live shape Anthropic returns for an idle session: five_hour
        // utilization 0.0 with resets_at: null (nothing pending to reset). CodexBar
        // shows this 0% window; we keep it reset-less rather than dropping it, so
        // the headline session window does not vanish when simply empty.
        let body = br#"{
            "five_hour": { "utilization": 0.0, "resets_at": null },
            "seven_day": { "utilization": 91.0, "resets_at": "2026-06-24T14:00:00Z" }
        }"#;
        let usage = normalize_usage(body).unwrap();
        let primary = usage.primary.expect("idle 0% window kept");
        assert_eq!(primary.used_percent, 0.0);
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.window_minutes, Some(300));
        // The active weekly window is unaffected.
        assert_eq!(usage.secondary.unwrap().used_percent, 91.0);
    }

    #[test]
    fn missing_windows_yield_empty_usage() {
        let usage = normalize_usage(br#"{}"#).unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());
    }

    /// `extra_usage` exactly as captured from an account with extra usage enabled
    /// (2026-09-24T20:37:23Z).
    const CAPTURED_EXTRA_USAGE: &str = r#"{"is_enabled":true,"monthly_limit":10000,"used_credits":1277,"utilization":12.770000000000001,"currency":"USD","decimal_places":2,"disabled_reason":null,"user_disabled":false,"spend_limit_reached":false,"credits_ever_enabled":true,"daily":null,"weekly":null}"#;

    /// `spend` exactly as captured in the same response.
    const CAPTURED_SPEND: &str = r#"{"used":{"amount_minor":1277,"currency":"USD","exponent":2},"limit":{"amount_minor":10000,"currency":"USD","exponent":2},"percent":13,"severity":"normal","enabled":true,"disabled_reason":null,"cap":{"money":null,"credits":{"amount_minor":10000,"exponent":2}},"balance":null,"auto_reload":null,"disclaimer":"…","can_purchase_credits":false,"can_toggle":false}"#;

    /// A usage body with a live session window, the given `extra_usage`, and the
    /// given `spend` (omitted entirely when `None`).
    fn overage_body(extra_usage: &str, spend: Option<&str>) -> Vec<u8> {
        let spend = spend
            .map(|spend| format!(r#","spend":{spend}"#))
            .unwrap_or_default();
        format!(
            r#"{{"five_hour":{{"utilization":42.0,"resets_at":"2026-09-24T22:00:00Z"}},"extra_usage":{extra_usage}{spend}}}"#
        )
        .into_bytes()
    }

    fn usd(minor: i64) -> Amount {
        Amount {
            minor,
            exponent: 2,
            unit: "USD".to_string(),
        }
    }

    /// Normalize a body, asserting the session window survived whatever the
    /// money fields held, and return the pool.
    fn pool_beside_windows(body: &[u8]) -> Option<Pool> {
        let (usage, pool) = normalize_response(body).expect("the windows must still parse");
        assert_eq!(usage.primary.expect("five_hour").used_percent, 42.0);
        pool
    }

    #[test]
    fn captured_overage_account_publishes_one_extra_usage_pool() {
        let pool = pool_beside_windows(&overage_body(CAPTURED_EXTRA_USAGE, Some(CAPTURED_SPEND)))
            .expect("an overage-enabled account publishes its pool");
        assert_eq!(pool.id, "extra_usage");
        assert_eq!(pool.label, "Extra usage");
        assert_eq!(pool.total, Some(usd(10000)));
        assert_eq!(pool.remaining, Some(usd(8723)));
        assert_eq!(pool.basis, PoolBasis::Derived);
        assert_eq!(pool.funding, PoolFunding::Unknown);
        assert_eq!(pool.spendable, Some(true));
    }

    #[test]
    fn overage_at_its_spend_limit_is_not_spendable() {
        let extra_usage = CAPTURED_EXTRA_USAGE.replace(
            r#""spend_limit_reached":false"#,
            r#""spend_limit_reached":true"#,
        );
        let pool = pool_beside_windows(&overage_body(&extra_usage, Some(CAPTURED_SPEND))).unwrap();
        // Money remains, so only the provider's own flag can say this is closed.
        assert_eq!(pool.remaining, Some(usd(8723)));
        assert_eq!(pool.spendable, Some(false));
    }

    #[test]
    fn disabled_overage_is_not_spendable() {
        let spend = CAPTURED_SPEND.replace(r#""enabled":true"#, r#""enabled":false"#);
        let pool = pool_beside_windows(&overage_body(CAPTURED_EXTRA_USAGE, Some(&spend))).unwrap();
        assert_eq!(pool.spendable, Some(false));
    }

    #[test]
    fn spendable_is_unstated_when_a_switch_is_missing() {
        let extra_usage = CAPTURED_EXTRA_USAGE.replace(r#""spend_limit_reached":false,"#, "");
        let pool = pool_beside_windows(&overage_body(&extra_usage, Some(CAPTURED_SPEND))).unwrap();
        assert_eq!(pool.spendable, None);
    }

    #[test]
    fn unusable_spend_publishes_no_pool_and_keeps_the_windows() {
        let limit_null = CAPTURED_SPEND.replace(
            r#""limit":{"amount_minor":10000,"currency":"USD","exponent":2}"#,
            r#""limit":null"#,
        );
        let currency_mismatch = CAPTURED_SPEND.replace(
            r#""limit":{"amount_minor":10000,"currency":"USD""#,
            r#""limit":{"amount_minor":10000,"currency":"EUR""#,
        );
        let exponent_mismatch = CAPTURED_SPEND.replace(
            r#""limit":{"amount_minor":10000,"currency":"USD","exponent":2}"#,
            r#""limit":{"amount_minor":100000,"currency":"USD","exponent":3}"#,
        );
        let cases: [(&str, Option<&str>); 6] = [
            ("spend null", Some("null")),
            ("spend absent", None),
            ("limit null", Some(&limit_null)),
            ("currency mismatch", Some(&currency_mismatch)),
            ("exponent mismatch", Some(&exponent_mismatch)),
            (
                "spend unparseable",
                Some(r#"{"used":"a lot","enabled":"yes"}"#),
            ),
        ];
        for (case, spend) in cases {
            let body = overage_body(CAPTURED_EXTRA_USAGE, spend);
            let (usage, pool) = normalize_response(&body)
                .unwrap_or_else(|error| panic!("{case}: the windows must still parse: {error:?}"));
            assert_eq!(
                usage.primary.expect("five_hour").used_percent,
                42.0,
                "{case}"
            );
            assert!(pool.is_none(), "{case}: no pool may be published");
        }
    }

    #[test]
    fn overage_past_its_limit_leaves_nothing_rather_than_a_negative() {
        let spend = CAPTURED_SPEND.replace(r#""amount_minor":1277"#, r#""amount_minor":12500"#);
        let pool = pool_beside_windows(&overage_body(CAPTURED_EXTRA_USAGE, Some(&spend))).unwrap();
        assert_eq!(pool.remaining, Some(usd(0)));
        assert_eq!(pool.total, Some(usd(10000)));
    }

    /// Run `overage_pool` on a `spend` fixture (absent when `None`) beside the
    /// captured `extra_usage`.
    fn overage_verdict(spend: Option<&str>) -> Result<Option<Pool>, OverageRefusal> {
        overage_pool(
            spend.map(|spend| serde_json::from_str(spend).unwrap()),
            Some(serde_json::from_str(CAPTURED_EXTRA_USAGE).unwrap()),
        )
    }

    #[test]
    fn unstated_spend_is_no_pool_rather_than_a_refusal() {
        assert_eq!(overage_verdict(None), Ok(None), "spend absent");
        assert_eq!(overage_verdict(Some("null")), Ok(None), "spend null");
    }

    #[test]
    fn each_unusable_spend_names_its_refusal_reason() {
        let limit_null = CAPTURED_SPEND.replace(
            r#""limit":{"amount_minor":10000,"currency":"USD","exponent":2}"#,
            r#""limit":null"#,
        );
        let used_without_currency = CAPTURED_SPEND.replace(
            r#""used":{"amount_minor":1277,"currency":"USD","exponent":2}"#,
            r#""used":{"amount_minor":1277,"exponent":2}"#,
        );
        let negative_used =
            CAPTURED_SPEND.replace(r#""amount_minor":1277"#, r#""amount_minor":-5"#);
        let currency_mismatch = CAPTURED_SPEND.replace(
            r#""limit":{"amount_minor":10000,"currency":"USD""#,
            r#""limit":{"amount_minor":10000,"currency":"EUR""#,
        );
        let exponent_mismatch = CAPTURED_SPEND.replace(
            r#""limit":{"amount_minor":10000,"currency":"USD","exponent":2}"#,
            r#""limit":{"amount_minor":100000,"currency":"USD","exponent":3}"#,
        );
        let cases: [(&str, RefusalReason); 6] = [
            (
                r#"{"used":"a lot","enabled":"yes"}"#,
                RefusalReason::Unparseable,
            ),
            (&limit_null, RefusalReason::LimitMissing),
            (&used_without_currency, RefusalReason::UsedMissing),
            (&negative_used, RefusalReason::NegativeAmount),
            (&currency_mismatch, RefusalReason::CurrencyMismatch),
            (&exponent_mismatch, RefusalReason::ExponentMismatch),
        ];
        for (spend, expected) in cases {
            let refusal = overage_verdict(Some(spend)).expect_err(expected.as_str());
            assert_eq!(refusal.reason, expected, "{}", expected.as_str());
        }
    }

    #[test]
    fn a_refusal_carries_only_the_sorted_spend_key_names() {
        let refusal = overage_verdict(Some(r#"{"used":"a lot","enabled":"yes"}"#)).unwrap_err();
        assert_eq!(refusal.spend_keys, vec!["enabled", "used"]);
        // A `spend` that is not an object has no key names to report.
        let refusal = overage_verdict(Some(r#""a lot""#)).unwrap_err();
        assert_eq!(refusal.reason, RefusalReason::Unparseable);
        assert!(refusal.spend_keys.is_empty());
    }

    #[test]
    fn first_refusal_is_logged() {
        assert!(refusal_needs_log(None, Some(RefusalReason::LimitMissing)));
    }

    #[test]
    fn the_same_refusal_again_is_silent() {
        assert!(!refusal_needs_log(
            Some(RefusalReason::LimitMissing),
            Some(RefusalReason::LimitMissing)
        ));
    }

    #[test]
    fn a_different_refusal_is_logged() {
        assert!(refusal_needs_log(
            Some(RefusalReason::LimitMissing),
            Some(RefusalReason::CurrencyMismatch)
        ));
    }

    #[test]
    fn a_refusal_clearing_is_logged_and_readable_stays_silent() {
        assert!(refusal_needs_log(Some(RefusalReason::Unparseable), None));
        assert!(!refusal_needs_log(None, None));
    }

    #[test]
    fn refusal_state_is_held_per_handle_and_cleared_when_readable() {
        let (source, _) = source(Err(VaultGetError::Permanent));
        let provider = test_provider(source, "http://unused.invalid".to_string());
        let refused = overage_verdict(Some(r#"{"used":"a lot"}"#));
        assert_eq!(provider.settle_overage("oauth:anthropic", refused), None);
        assert_eq!(
            provider
                .overage_refusals
                .lock()
                .unwrap()
                .get("oauth:anthropic"),
            Some(&RefusalReason::Unparseable)
        );
        assert!(!provider
            .overage_refusals
            .lock()
            .unwrap()
            .contains_key("oauth:anthropic:other"));
        let pool =
            provider.settle_overage("oauth:anthropic", overage_verdict(Some(CAPTURED_SPEND)));
        assert!(pool.is_some(), "a readable spend still publishes its pool");
        assert!(provider.overage_refusals.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn overage_pool_reaches_the_published_spend() {
        // The session window resets an hour from now, so the wire rules below
        // judge a live window rather than one whose reset has already passed.
        let reset = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let body = String::from_utf8(overage_body(CAPTURED_EXTRA_USAGE, Some(CAPTURED_SPEND)))
            .unwrap()
            .replace("2026-09-24T22:00:00Z", &reset)
            .into_bytes();
        let (url, _) = serve_once(200, body).await;
        let (source, _) = source(Ok(credential(b"anthropic-vault-token", 31)));
        let handle =
            CredentialHandle::vault("oauth:anthropic", VaultCapability::new("ckh_anthropic"));
        let registry = crate::Registry::new(vec![Box::new(VaultOnlyProvider {
            provider: test_provider(source, url),
            handle,
        })]);

        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let entries = registry.get_usage(Some(PROVIDER_NAME)).await;
        assert_eq!(entries.len(), 1);
        let spend = entries[0].spend.as_ref().expect("the pool is published");
        assert_eq!(spend.len(), 1);
        assert_eq!(spend[0].id, "extra_usage");
        assert_eq!(spend[0].remaining, Some(usd(8723)));
        assert_eq!(spend[0].spendable, Some(true));
        // The windows are published beside it, not replaced by it.
        assert_eq!(
            entries[0]
                .usage
                .as_ref()
                .unwrap()
                .primary
                .as_ref()
                .unwrap()
                .used_percent,
            42.0
        );
        // The published pool also satisfies the wire rules, remaining within
        // total among them.
        let report = crate::wire_sanity::check_entries(&entries, crate::wire_sanity::now());
        assert_eq!(report.pool_comparisons, 1);
        assert!(report.findings.is_empty(), "{:?}", report.findings);
    }
}
