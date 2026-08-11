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
    model::{Pool, PoolBasis, PoolFunding, ProviderUsage, RateWindow, Usage},
    money::parse_amount,
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "minimax";

const API_KEY_ENV: &[&str] = &["MINIMAX_CODING_API_KEY", "MINIMAX_API_KEY"];
const REGION_ENV: &[&str] = &["MINIMAX_API_REGION"];

const REMAINS_PATH: &str = "/v1/api/openplatform/coding_plan/remains";
/// The account wallet, which reports prepaid balances rather than rate windows.
///
/// NOT in MiniMax's public documentation. The path and response shape come from
/// MiniMax's own CLI, so this is a reasonable source but not a promised
/// contract: it can change without a deprecation notice, and a failure here must
/// never take the rate windows down with it.
const BALANCE_PATH: &str = "/account/query_balance";
const GLOBAL_API_BASE: &str = "https://api.minimax.io";
const CHINA_API_BASE: &str = "https://api.minimaxi.com";

/// The wallet response.
///
/// Amounts arrive as decimal strings, e.g. `"98.00"`.
///
/// MiniMax separates these balances and publicly defines none of them.
/// `voucher_balance` is the plausible home for granted credit and MiniMax does
/// not say so, which is why every pool below is published under MiniMax's own
/// field name with `PoolFunding::Unknown`. Naming one of them "granted" would
/// invent exactly the label a spend policy keys on, and being wrong there spends
/// real money.
#[derive(Debug, Clone, Deserialize)]
struct WalletResponse {
    /// Present in the payload and deliberately unpublished: it reads as the
    /// spendable total across the balances below, so emitting it as a fourth
    /// pool would have a consumer counting the same money twice.
    ///
    /// Kept in the struct so the observed shape stays visible here, and so its
    /// absence from the output reads as a decision rather than an oversight.
    #[allow(dead_code)]
    available_amount: Option<String>,
    cash_balance: Option<String>,
    voucher_balance: Option<String>,
    credit_balance: Option<String>,
}

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

/// The window to publish for one cadence, chosen from the models the account
/// reports.
///
/// The general bucket is preferred because it is the account-wide figure, but
/// only when it actually yields a window: it can come back as a placeholder for
/// a lane the plan does not include, and that placeholder produces no window at
/// all. Selecting it and stopping would publish nothing while another model
/// states a real figure -- and a provider with no window is indistinguishable
/// from capacity nobody measured, whereas the account is genuinely being
/// metered.
///
/// So the choice is made over models that *produce* a window rather than over
/// the raw list: every candidate is tried in preference order and the first
/// usable one wins. The ordering is otherwise unchanged -- general first, then
/// the largest quota among models that declare one, then the largest of the
/// rest.
fn representative_window(
    models: &[ModelRemains],
    total_fn: fn(&ModelRemains) -> i64,
    build: impl Fn(&ModelRemains) -> Option<RateWindow>,
) -> Option<RateWindow> {
    let mut ranked: Vec<&ModelRemains> = Vec::with_capacity(models.len());

    if let Some(general) = models.iter().find(|m| is_general_model(m)) {
        ranked.push(general);
    }

    let mut rest: Vec<&ModelRemains> = models.iter().filter(|m| !is_general_model(m)).collect();
    // Largest declared quota first, matching the previous preference; models
    // declaring none sort last rather than being dropped, so an account whose
    // models all report zero still publishes a window.
    rest.sort_by_key(|m| std::cmp::Reverse(total_fn(m)));
    ranked.extend(rest);

    ranked.into_iter().find_map(build)
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
            used_count: None,
            total_count: None,
        });
    }

    let total = interval_total(m);
    let remaining = opt_int(&m.current_interval_usage_count)?;
    if total <= 0 {
        return None;
    }
    let resets_at = resets_at_iso(opt_int(&m.end_time), opt_int(&m.remains_time), now_secs);
    Some(RateWindow {
        used_percent: used_percent(total, remaining),
        raw_used_percent: None,
        resets_at,
        window_minutes: window_minutes(opt_int(&m.start_time), opt_int(&m.end_time)),
        used_count: None,
        total_count: None,
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
                    used_count: None,
                    total_count: None,
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
            used_count: None,
            total_count: None,
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
        used_count: None,
        total_count: None,
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

    let primary = representative_window(&text_models, interval_total, |m| {
        make_interval_window(m, now_secs)
    });

    let secondary = representative_window(&text_models, weekly_total, |m| {
        make_weekly_window(m, now_secs)
    });

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

/// Build the pools MiniMax's wallet reports.
///
/// Every pool is published under MiniMax's own field name and with
/// `PoolFunding::Unknown`, because MiniMax separates these balances without
/// publicly defining any of them. `voucher_balance` is where granted credit
/// plausibly lives, and "plausibly" is not a basis for a label a consumer will
/// key a spend policy on.
///
/// `available_amount` is deliberately not a pool. It reads as the spendable
/// total across the others -- the CLI's own figures add up that way -- so
/// publishing it beside them would have a consumer counting the same money
/// twice. Nothing here reads it.
fn wallet_pools(body: &[u8]) -> Result<Vec<Pool>, FetchError> {
    let wallet: WalletResponse = serde_json::from_slice(body)
        .map_err(|error| FetchError::Decode(format!("minimax wallet: {error}")))?;

    // MiniMax states no currency on this endpoint, and the account's billing
    // currency is not determinable from it. "credit" is therefore a generic
    // placeholder chosen here, not a denomination MiniMax reports -- a currency
    // code inferred from the region setting would look authoritative and be a
    // guess about somebody's money.
    const UNIT: &str = "credit";

    let mut pools = Vec::new();
    let mut push = |raw: &Option<String>, id: &str, label: &str| {
        let Some(text) = raw.as_deref() else {
            return;
        };
        let Some(amount) = parse_amount(text, UNIT) else {
            return;
        };
        pools.push(Pool {
            id: id.to_string(),
            label: label.to_string(),
            // Not a guess. MiniMax documents none of these three.
            funding: PoolFunding::Unknown,
            remaining: Some(amount),
            total: None,
            // Each balance is stated directly rather than derived from a total
            // and a consumption figure, so a policy naming one is exact.
            basis: PoolBasis::Reported,
            // No per-pool enable flag on this endpoint.
            spendable: None,
        });
    };

    push(&wallet.cash_balance, "cash_balance", "Cash balance");
    push(
        &wallet.voucher_balance,
        "voucher_balance",
        "Voucher balance",
    );
    push(&wallet.credit_balance, "credit_balance", "Credit balance");

    if pools.is_empty() {
        return Err(FetchError::Decode(
            "minimax wallet: no readable balance".to_string(),
        ));
    }
    Ok(pools)
}

fn balance_url(base: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), BALANCE_PATH)
}

/// Fetch the wallet, returning None on any failure.
///
/// Deliberately swallows its error. The caller has already published rate
/// windows by this point, and this endpoint is undocumented -- an outage or a
/// shape change here must cost the money signal only, never the capacity one.
///
/// Nothing is logged and no error reaches the wire, so a wallet outage is
/// invisible as a failure: it shows only as an entry that carries no pools,
/// which is indistinguishable from a provider that reports none. That is the
/// price of never letting this endpoint cost the capacity signal, and it is
/// worth knowing before trusting an absence here.
async fn fetch_wallet(client: &reqwest::Client, api_key: &str, base: &str) -> Option<Vec<Pool>> {
    let body = JsonRequest::get(balance_url(base))
        .bearer(api_key)
        .send(client)
        .await
        .ok()?;
    wallet_pools(&body).ok()
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
        let result: Result<(ProviderUsage, String, String), FetchError> = async {
            let api_key = env::first_env(API_KEY_ENV)
                .ok_or_else(|| FetchError::NoSession(format!("none of {API_KEY_ENV:?} is set")))?;

            let configured = api_base_url();
            // Which host actually answered, not which one was tried first. A key
            // minted in one region is rejected by the other, so a later request
            // on this account has to go to the host that accepted this one --
            // sending it elsewhere earns a 401 for a credential that is fine.
            let (body, base) = match fetch_remains_once(&self.http, &api_key, &configured).await {
                Ok(body) => (body, configured),
                Err(FetchError::Unauthorized(_)) if configured == GLOBAL_API_BASE => (
                    fetch_remains_once(&self.http, &api_key, CHINA_API_BASE).await?,
                    CHINA_API_BASE.to_string(),
                ),
                Err(e) => return Err(e),
            };

            let usage = normalize_usage(&body)?;
            Ok((
                ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage),
                api_key,
                base,
            ))
        }
        .await;

        let (entry, api_key, base) = match result {
            Ok(value) => value,
            Err(error) => return FetchAttempt::from_provider_usage(Err(error)),
        };

        // The wallet is fetched only after the windows are in the successful
        // usage entry, and its failure is discarded rather than propagated.
        //
        // Two reasons, and the second is the load-bearing one. The endpoint is
        // undocumented, so it can change or disappear without notice. And the
        // windows are what a router paces on: letting a wallet failure degrade
        // this entry would trade a working capacity signal for a missing money
        // one, which is strictly worse than not asking at all.
        //
        // A wallet that fails leaves `pools` as None -- nothing to report --
        // rather than an empty list, which would claim MiniMax holds no credit.
        let pools = fetch_wallet(&self.http, &api_key, &base).await;

        let mut attempt = FetchAttempt::from_provider_usage(Ok(entry));
        attempt.pools = pools;
        attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    /// A general bucket that yields no window must not hide a model that does.
    ///
    /// The general bucket is the account-wide figure and is preferred, but the
    /// upstream returns it as a placeholder for a lane the plan does not
    /// include -- status 3, zero counts, nothing consumed. That placeholder
    /// produces no window, so selecting it and stopping publishes nothing while
    /// another model states a real figure. A provider with no window reads as
    /// capacity nobody measured, when the account is in fact being metered.
    #[test]
    fn a_general_placeholder_does_not_hide_a_model_with_a_real_window() {
        let start = 1_700_000_000_000_i64;
        let end = start + 5 * 60 * 60 * 1000;
        let json = format!(
            r#"{{
              "base_resp": {{ "status_code": 0 }},
              "model_remains": [
                {{
                  "model_name": "general",
                  "current_interval_status": 3,
                  "current_interval_total_count": 0,
                  "current_interval_usage_count": 0,
                  "current_interval_remaining_percent": 100.0
                }},
                {{
                  "model_name": "MiniMax-M2.7",
                  "current_interval_total_count": 1000,
                  "current_interval_usage_count": 250,
                  "start_time": {start},
                  "end_time": {end},
                  "remains_time": 240000
                }}
              ]
            }}"#
        );

        let usage = normalize_usage_at(json.as_bytes(), NOW).unwrap();
        let primary = usage
            .primary
            .expect("the account is metered, so a window must be published");
        assert_eq!(primary.used_percent, 75.0);
        assert_eq!(primary.window_minutes, Some(300));
    }

    /// The general bucket still wins when it does yield a window.
    ///
    /// Without this, the fix above would pass equally if the preference were
    /// dropped altogether and the largest quota always chosen -- which would
    /// publish a single model's usage as the account's.
    #[test]
    fn the_general_bucket_is_preferred_when_it_yields_a_window() {
        let start = 1_700_000_000_000_i64;
        let end = start + 5 * 60 * 60 * 1000;
        let json = format!(
            r#"{{
              "base_resp": {{ "status_code": 0 }},
              "model_remains": [
                {{
                  "model_name": "general",
                  "current_interval_total_count": 100,
                  "current_interval_usage_count": 90,
                  "start_time": {start},
                  "end_time": {end},
                  "remains_time": 240000
                }},
                {{
                  "model_name": "MiniMax-M2.7",
                  "current_interval_total_count": 100000,
                  "current_interval_usage_count": 50000,
                  "start_time": {start},
                  "end_time": {end},
                  "remains_time": 240000
                }}
              ]
            }}"#
        );

        let usage = normalize_usage_at(json.as_bytes(), NOW).unwrap();
        // 10% used comes from the general bucket; the larger model would give 50%.
        assert_eq!(usage.primary.unwrap().used_percent, 10.0);
    }

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
    fn missing_reset_keeps_interval_window() {
        let body = br#"{
          "base_resp": { "status_code": 0 },
          "model_remains": [{
            "current_interval_total_count": 100,
            "current_interval_usage_count": 50
          }]
        }"#;
        let usage = normalize_usage_at(body, NOW).unwrap();
        let primary = usage.primary.expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 50.0);
        assert_eq!(primary.resets_at, None);
    }

    #[test]
    fn exhausted_interval_without_reset_is_kept() {
        let body = br#"{
          "base_resp": { "status_code": 0 },
          "model_remains": [{
            "current_interval_total_count": 100,
            "current_interval_usage_count": 0
          }]
        }"#;
        let primary = normalize_usage_at(body, NOW)
            .unwrap()
            .primary
            .expect("usage data should emit a window");
        assert_eq!(primary.used_percent, 100.0);
        assert_eq!(primary.resets_at, None);
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

    /// Every condition in the placeholder gate must be load-bearing.
    ///
    /// This upstream reports lanes that exist in its schema but are not part of
    /// the account's subscription, and marks them with status 3, a zero total, a
    /// zero count, and 100% remaining all at once. Suppressing that combination
    /// keeps a lane the account does not have from rendering as an idle one.
    ///
    /// Each condition is what stops the suppression from reaching a lane the
    /// account really does have, so dropping any one of them deletes a genuine
    /// window from the wire -- and a missing window reads as capacity that was
    /// never measured rather than as an error. A fixture that satisfies all four
    /// proves the gate fires; only a fixture that breaks exactly one proves the
    /// gate is narrow.
    #[test]
    fn each_condition_of_the_placeholder_gate_keeps_a_real_lane_visible() {
        let now = 1_780_347_620;

        // Exactly one field differs from the suppressed shape in each case, so a
        // failure names the condition that stopped being enforced.
        let cases = [
            ("status is not the unavailable marker", 1, 0, 0, 100.0),
            ("the lane has a real allowance", 3, 50, 0, 100.0),
            ("the lane has been used", 3, 0, 5, 100.0),
            ("the lane is not full", 3, 0, 0, 40.0),
        ];

        for (why, status, total, used, remaining) in cases {
            let body = format!(
                r#"{{
                  "base_resp": {{ "status_code": 0 }},
                  "model_remains": [
                    {{
                      "model_name": "general",
                      "current_interval_total_count": {total},
                      "current_interval_usage_count": {used},
                      "current_interval_status": {status},
                      "current_interval_remaining_percent": {remaining},
                      "start_time": 1780347600000,
                      "end_time": 1780365600000
                    }}
                  ]
                }}"#
            );
            let payload: CodingPlanPayload = serde_json::from_slice(body.as_bytes()).unwrap();
            let model = &model_remains_list(&payload)[0];
            assert!(
                make_interval_window(model, now).is_some(),
                "a lane was suppressed although {why}"
            );
        }

        // The control: with every condition met the window really is suppressed,
        // so the assertions above cannot pass because the gate stopped working
        // altogether.
        let suppressed = br#"{
              "base_resp": { "status_code": 0 },
              "model_remains": [
                {
                  "model_name": "general",
                  "current_interval_total_count": 0,
                  "current_interval_usage_count": 0,
                  "current_interval_status": 3,
                  "current_interval_remaining_percent": 100,
                  "start_time": 1780347600000,
                  "end_time": 1780365600000
                }
              ]
            }"#;
        let payload: CodingPlanPayload = serde_json::from_slice(suppressed).unwrap();
        let model = &model_remains_list(&payload)[0];
        assert!(
            make_interval_window(model, now).is_none(),
            "the unavailable-lane shape must still be suppressed"
        );
    }
}

#[cfg(test)]
mod wallet_tests {
    use super::*;

    /// CLI-observed shape: taken from MiniMax's own CLI, which is the only
    /// source for this endpoint. Not a documented contract.
    const WALLET: &[u8] = br#"{
        "available_amount": "98.00",
        "cash_balance": "0.00",
        "voucher_balance": "98.00",
        "credit_balance": "0.00",
        "owed_amount": "0.00",
        "base_resp": { "status_code": 0, "status_msg": "success" }
    }"#;

    /// Pools keep MiniMax's own names and claim no funding.
    ///
    /// The temptation is to publish `voucher_balance` as granted credit, since
    /// that is plausibly what it is and it is what a "spend only free credit"
    /// policy wants. MiniMax documents none of these three, so that label would
    /// be ours, and a consumer keying a spend policy on it would be spending
    /// real money on our guess.
    #[test]
    fn wallet_pools_carry_minimax_names_and_unknown_funding() {
        let pools = wallet_pools(WALLET).expect("the CLI-observed shape must parse");

        let ids: Vec<&str> = pools.iter().map(|pool| pool.id.as_str()).collect();
        assert_eq!(ids, ["cash_balance", "voucher_balance", "credit_balance"]);
        assert!(
            pools
                .iter()
                .all(|pool| pool.funding == PoolFunding::Unknown),
            "no pool may claim a funding MiniMax does not state: {pools:?}"
        );
        assert!(
            !pools
                .iter()
                .any(|pool| pool.id.contains("granted")
                    || pool.label.to_lowercase().contains("granted")),
            "a granted label would be ours, not MiniMax's: {pools:?}"
        );
    }

    /// The spendable total is not published beside its own parts.
    #[test]
    fn the_available_total_is_not_a_pool() {
        let pools = wallet_pools(WALLET).expect("parses");
        assert!(
            !pools.iter().any(|pool| pool.id.contains("available")),
            "available_amount is the sum of the parts: {pools:?}"
        );
        let summed: i64 = pools
            .iter()
            .filter_map(|pool| pool.remaining.as_ref().map(|amount| amount.minor))
            .sum();
        assert_eq!(summed, 9_800, "the pools must sum to the stated total");
    }

    /// The wallet must be asked on the host that accepted the credential.
    ///
    /// A key minted in one region is rejected by the other, and the rate-window
    /// request already falls back from global to China on a 401. If the wallet
    /// then used the originally configured host, an account that only works in
    /// China would have its windows fetched from China and its wallet asked in
    /// the wrong place -- earning a 401 for a credential that is perfectly good.
    ///
    /// Worse, the failure would be invisible: wallet errors are deliberately
    /// discarded, so this would present as "MiniMax reports no pools" forever
    /// rather than as anything anyone could debug.
    #[test]
    fn the_wallet_url_follows_the_host_that_answered() {
        assert_eq!(
            balance_url(CHINA_API_BASE),
            format!("{CHINA_API_BASE}{BALANCE_PATH}")
        );
        assert_eq!(
            balance_url(GLOBAL_API_BASE),
            format!("{GLOBAL_API_BASE}{BALANCE_PATH}")
        );
        // The two must differ, or the assertion above proves nothing about
        // which host a fallback ends up using.
        assert_ne!(balance_url(CHINA_API_BASE), balance_url(GLOBAL_API_BASE));
    }

    /// A wallet body that states nothing readable is an error, not an empty
    /// wallet: the two are opposite facts about an account's money.
    #[test]
    fn an_unreadable_wallet_is_an_error_rather_than_no_credit() {
        assert!(matches!(
            wallet_pools(b"not json"),
            Err(FetchError::Decode(_))
        ));
        assert!(matches!(
            wallet_pools(br#"{ "base_resp": { "status_code": 0 } }"#),
            Err(FetchError::Decode(_))
        ));
    }
}
