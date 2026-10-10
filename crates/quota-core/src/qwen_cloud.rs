//! Qwen Cloud token-plan usage — session-cookie + console-gateway scrape.
//!
//! The token-plan quota is available only to an authenticated Qwen Cloud web
//! session, deposited in the vault as `cookie:qwencloud.com`. Each personal
//! scrape loads the token-plan page to obtain a fresh `SEC_TOKEN`, then posts it with
//! the deposited cookies through the ONE_CONSOLE
//! `IntlBroadScopeAspnGateway` gateway.
//!
//! VERIFICATION: the personal path is fixture-verified from a live browser HAR
//! capture of `home.qwencloud.com`. The HAR verifies the
//! `zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage` endpoint via the
//! `IntlBroadScopeAspnGateway` console gateway, the session cookie + per-page
//! `SEC_TOKEN` authentication, and the `per5HourPercentage`,
//! `per5HourResetTime`, `per1WeekPercentage`, and `per1WeekResetTime` response
//! fields. `per5HourPercentage` and `per1WeekPercentage` are interpreted as USED
//! fractions (0–1) from their field semantics and observed values.
//!
//! A third, monthly window (`per1MonthPercentage`, a USED fraction like the other
//! two, and `per1MonthResetTime`, plus a `monthly` cap in `/quota-config`) is read
//! as CodexBar v0.66.0 reads it for the shared Aliyun ONE_CONSOLE token plan. It
//! is FIXTURE-VERIFIED ONLY: the account this was built against returns just the
//! five-hour and weekly fields. Placement follows upstream: the monthly window is
//! `primary` when neither rolling window is reported, and otherwise rides in
//! `extra_rate_windows` with id `monthly`, so it is never dropped. The gateway
//! call shape is HAR-verified.
//!
//! Team Token Plan discovery runs first, using the same deposited cookies. Its
//! home-console RPCs and fixtures follow CodexBar v0.74.0
//! `QwenCloudTeamFetchStrategy.swift` and `qwencloud-team.ts` (9fae8c8e4).
//! Team coverage is checked against test fixtures, not a live account: no Qwen
//! Cloud account is available on this host.
//! Failed discovery falls back to the unchanged personal path; explicit login
//! refusals instead report an expired session.
//!
//! The personal path uses two hosts: its token-plan PAGE (`home.qwencloud.com`)
//! supplies the per-session `SEC_TOKEN`; the quota call itself goes to the console data
//! gateway at `cs-data.qwencloud.com` (same-site, so the `qwencloud.com` cookies
//! apply), with `Origin: https://home.qwencloud.com`. Posting the quota action to
//! `home.qwencloud.com` instead returns an empty success — the gateway host matters.
//!
//! PERSONAL PLAN LIMITATION (entitled view; no enforced signal in the console): the
//! `/usage` percentages are the *entitled* view — consumed usage divided by the
//! current tier cap from `/quota-config`. The console exposes NO enforced-cap,
//! absolute-used, or exhausted/limited field: the live `/usage`, `/quota-config`,
//! and `/subscription` field sets are identical whether a window is healthy or
//! walled (`/usage` returns only the four per*Percentage/per*ResetTime fields;
//! `/quota-config` returns every tier's `{five_hour, weekly}` cap; `/subscription`
//! returns specCode/remainingDays/status). Absolute consumed is derivable as
//! `percentage * quota-config[specCode].window`, but a stateless reader still
//! cannot tell which cap the inference edge enforces. Consequence: on a
//! mid-window plan upgrade, *if* the edge keeps enforcing the old cap until the
//! reset while the console already divides by the new one, the percentage would
//! read healthy while the edge 429s, and that desync is undetectable from any
//! console read (probing the inference edge is out: token-plan ToS forbids
//! automated calls). Whether the edge actually lags an upgrade is provider-
//! dependent and was NOT observed on 2026-07-21: the edge kept up, so post-upgrade
//! the weekly window was honestly healthy at ~26% (matching Alibaba's dashboard);
//! the 429 seen that day predated the upgrade and was genuine exhaustion, and the
//! post-upgrade failures were 403 model-entitlement on newly-onboarded models, not
//! quota. The limitation is therefore structural/latent — the console gives no
//! signal to detect an edge lag — so routing consumers that observe a 429 must
//! apply their own cooldown. This module reports the console's entitled figure,
//! the most accurate value the console provides.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    env,
    http::{Header, JsonRequest},
    model::{AccountInfo, ExtraWindow, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "qwen-cloud";
/// Base key for storing Qwen Cloud credentials in the provider vault. An account-specific
/// suffix identifies each account, and these credentials are read only from the provider vault.
const COOKIE_FAMILY: &str = "cookie:qwencloud.com";

/// The script block the authenticated console shell emits. Its presence is what
/// separates "we were served the real page" from "we were not signed in".
const CONSOLE_BLOCK: &str = "ONE_CONSOLE_TOOL";

const TOKEN_PLAN_URL: &str =
    "https://home.qwencloud.com/billing/subscription/token-plan-individual";
const USAGE_URL: &str = "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage";
const QUOTA_CONFIG_URL: &str = "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/quota-config";
const SUBSCRIPTION_URL: &str = "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/subscription";
const TEAM_INFO_URL: &str = "https://home.qwencloud.com/tool/user/info.json";
#[rustfmt::skip]
const TEAM_HUMAN_URL: &str = "https://home.qwencloud.com/data/api.json?product=ea-service&action=LoadHumanInfo";
const TEAM_SUMMARY_URL: &str = "https://home.qwencloud.com/data/api.json?product=BssOpenAPI-V3&action=GetSeatSubscriptionSummary";
const TEAM_REFERER_URL: &str = "https://home.qwencloud.com/analytics/token-plan/team";
const TEAM_PRODUCT: &str = "sfm_tokenplanteams_dp_intl";
const TEAM_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const GATEWAY_PARAMS: &str = r#"{"Api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage","Data":{"cornerstoneParam":{"domain":"home.qwencloud.com","consoleSite":"QWENCLOUD","console":"ONE_CONSOLE","xsp_lang":"en-US","protocol":"V2","productCode":"p_efm"}},"V":"1.0"}"#;
const QUOTA_CONFIG_PARAMS: &str = r#"{"Api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/quota-config","Data":{"cornerstoneParam":{"domain":"home.qwencloud.com","consoleSite":"QWENCLOUD","console":"ONE_CONSOLE","xsp_lang":"en-US","protocol":"V2","productCode":"p_efm"}},"V":"1.0"}"#;
const SUBSCRIPTION_PARAMS: &str = r#"{"Api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/subscription","Data":{"cornerstoneParam":{"domain":"home.qwencloud.com","consoleSite":"QWENCLOUD","console":"ONE_CONSOLE","xsp_lang":"en-US","protocol":"V2","productCode":"p_efm"}},"V":"1.0"}"#;

const FIVE_HOUR_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;
/// Thirty days, the same span CodexBar gives this window.
const MONTHLY_WINDOW_MINUTES: i64 = 30 * 24 * 60;
/// The id and title the monthly window carries when it is an extra window.
const MONTHLY_WINDOW_ID: &str = "monthly";
const MONTHLY_WINDOW_TITLE: &str = "Monthly";

/// The wire names of every percentage field in [`TokenPlanUsage`].
///
/// This list decides whether a window-less plan block STATED its fields (the
/// account has no windows) or named none of them (a payload we no longer
/// understand). serde's `rename` only accepts a literal, so the struct cannot be
/// built from this list; the test `percentage_keys_match_the_struct_renames`
/// pins the two together instead. They drifted once already: the list said
/// `perWeekPercentage` while the wire and the struct said `per1WeekPercentage`.
const PERCENTAGE_KEYS: [&str; 3] = [
    "per5HourPercentage",
    "per1WeekPercentage",
    "per1MonthPercentage",
];

#[derive(Debug, Deserialize)]
struct GatewayResponse {
    #[serde(rename = "successResponse")]
    success_response: Option<bool>,
    data: Option<GatewayData>,
}

#[derive(Debug, Deserialize)]
struct GatewayData {
    #[serde(rename = "DataV2")]
    data_v2: Option<DataV2>,
}

#[derive(Debug, Deserialize)]
struct DataV2 {
    data: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TokenPlanResult {
    msg: Option<String>,
    code: Option<String>,
    success: Option<bool>,
    data: Option<TokenPlanUsage>,
}

#[derive(Debug, Deserialize)]
struct TokenPlanUsage {
    #[serde(rename = "per5HourPercentage")]
    per_five_hour_percentage: Option<f64>,
    #[serde(rename = "per5HourResetTime")]
    per_five_hour_reset_time: Option<i64>,
    #[serde(rename = "per1WeekPercentage")]
    per_week_percentage: Option<f64>,
    #[serde(rename = "per1WeekResetTime")]
    per_week_reset_time: Option<i64>,
    #[serde(rename = "per1MonthPercentage")]
    per_month_percentage: Option<f64>,
    #[serde(rename = "per1MonthResetTime")]
    per_month_reset_time: Option<i64>,
}

/// Per-tier quota caps from `/quota-config`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct QuotaConfigResult {
    msg: Option<String>,
    code: Option<String>,
    success: Option<bool>,
    data: Option<std::collections::HashMap<String, TierCaps>>,
}

#[derive(Debug, Deserialize)]
struct TierCaps {
    five_hour: Option<f64>,
    weekly: Option<f64>,
    monthly: Option<f64>,
}

/// Plan metadata from `/subscription`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SubscriptionResult {
    msg: Option<String>,
    code: Option<String>,
    success: Option<bool>,
    data: Option<SubscriptionData>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionData {
    #[serde(rename = "specCode")]
    spec_code: Option<String>,
    /// Names the product this record belongs to, e.g.
    /// `sfm_tokenplansolo_public_intl-sg-ycx4vlnxo0a`.
    #[serde(rename = "instanceCode")]
    instance_code: Option<String>,
}

/// The product whose caps `/quota-config` describes.
///
/// The console's own subscription call passes this as a `commodityCode` filter
/// and ours does not -- observed in a capture of the working browser,
/// 2026-08-20. We deliberately keep the unfiltered request: filtering is the
/// REJECTING direction, and an account on a different token-plan product would
/// get no record at all where today it gets its own.
///
/// The risk that creates is narrow and worth naming. `specCode` is a bare tier
/// name -- the captured value is `"pro"` -- and we use it to index the
/// quota-config cap table. If an unfiltered call ever returns another product's
/// subscription on a multi-subscription account, a generic tier name can hit a
/// real row and publish that product's cap as this window's `totalCount`. A
/// wrong absolute count is worse than none: a percentage that disagrees with its
/// own counts is visibly broken, while counts that agree with nothing are
/// believed.
const TOKEN_PLAN_COMMODITY: &str = "sfm_tokenplansolo_public_intl";

/// Whether a subscription record describes the token plan we publish.
///
/// Absent means UNVERIFIABLE, not wrong: only one payload has ever been
/// observed, so a record that omits `instanceCode` is enriched as before rather
/// than refused. The check exists to catch a record that names a DIFFERENT
/// product, which is the only case where a cap lookup can silently succeed with
/// the wrong table row.
fn record_is_the_token_plan(data: &SubscriptionData) -> bool {
    match data.instance_code.as_deref() {
        Some(code) => code.starts_with(TOKEN_PLAN_COMMODITY),
        None => true,
    }
}

/// Enrich windows with absolute counts from the quota-config + subscription
/// responses. Best-effort: if either call fails or the tier is unknown, the
/// windows keep `used_count: None, total_count: None` (the percentage is still
/// honest).
fn enrich_with_counts(usage: &mut Usage, quota_config_body: &[u8], subscription_body: &[u8]) {
    let config: GatewayResponse = match serde_json::from_slice(quota_config_body) {
        Ok(v) => v,
        Err(_) => return,
    };
    let sub: GatewayResponse = match serde_json::from_slice(subscription_body) {
        Ok(v) => v,
        Err(_) => return,
    };
    if config.success_response != Some(true) || sub.success_response != Some(true) {
        return;
    }
    let config_result: QuotaConfigResult = match config
        .data
        .as_ref()
        .and_then(|d| d.data_v2.as_ref())
        .and_then(|d| d.data.as_ref())
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(v) => v,
        None => return,
    };
    let sub_result: SubscriptionResult = match sub
        .data
        .as_ref()
        .and_then(|d| d.data_v2.as_ref())
        .and_then(|d| d.data.as_ref())
        .and_then(|v| serde_json::from_value(v.clone()).ok())
    {
        Some(v) => v,
        None => return,
    };
    if config_result.success != Some(true) || sub_result.success != Some(true) {
        return;
    }
    let caps = match config_result.data {
        Some(c) => c,
        None => return,
    };
    let Some(sub_data) = sub_result.data.as_ref() else {
        return;
    };
    if !record_is_the_token_plan(sub_data) {
        // Another product's subscription. Leave the percentages alone rather
        // than deriving counts from a cap table this record does not describe.
        return;
    }
    let spec = match sub_data.spec_code.as_deref() {
        Some(s) => s,
        None => return,
    };
    let tier = match caps.get(spec) {
        Some(t) => t,
        None => return,
    };
    // The cap is published; the consumed count is NOT reconstructed from it.
    //
    // `usedCount` is a count of things, so it is integral by contract, and
    // `percentage * cap` is fractional for almost every input. Rounding would be
    // worse than omitting: it would present an estimate in a field whose type
    // claims an exact measurement the console never reported.
    //
    // The estimate is also not merely un-rounded, it is measured against a
    // denominator that does not match. Live check on 2026-08-12: the console
    // returned 45.052361473854994% where this cap is 40000, and no integer count
    // over 40000 produces that percentage -- 18021/40000 is 45.0525%, off by
    // 1.4e-6, which is eleven orders of magnitude above f64 noise. So the
    // console divides by something other than the cap this endpoint reports,
    // exactly the entitled-versus-enforced gap described at the top of this
    // file. A derived count would carry that disagreement as if it were data.
    //
    // Each cap is matched to its window by span, not by slot: `primary` holds the
    // monthly window when no rolling window is reported, and a five-hour cap on
    // a monthly window would be a count that agrees with nothing.
    fn apply(window: &mut RateWindow, tier: &TierCaps) {
        let cap = match window.window_minutes {
            Some(FIVE_HOUR_WINDOW_MINUTES) => tier.five_hour,
            Some(WEEKLY_WINDOW_MINUTES) => tier.weekly,
            Some(MONTHLY_WINDOW_MINUTES) => tier.monthly,
            _ => None,
        };
        if let Some(cap) = cap {
            window.total_count = Some(cap);
        }
    }
    for window in [usage.primary.as_mut(), usage.secondary.as_mut()]
        .into_iter()
        .flatten()
    {
        apply(window, tier);
    }
    for extra in usage.extra_rate_windows.iter_mut().flatten() {
        if extra.id.as_deref() == Some(MONTHLY_WINDOW_ID) {
            if let Some(window) = extra.window.as_mut() {
                apply(window, tier);
            }
        }
    }
}

/// Why the token-plan page carried no `SEC_TOKEN`, as two different faults.
///
/// A LOGGED-OUT PAGE AND AN UNBOOTSTRAPPED ONE BOTH LACK THE TOKEN, and the
/// remedies are opposite: one is answered by signing in again, the other by
/// waiting. Both used to report `Decode`, which says the upstream sent something
/// unparseable -- and `Decode` is one of the two classes counted as a stale
/// browser login, so this asked an operator to re-authenticate a working
/// session.
///
/// THE SECOND CASE IS TRANSIENT, AND THAT WAS ESTABLISHED THE EXPENSIVE WAY.
/// On 2026-08-18 the console served a live session (HTTP 200, no redirect,
/// `ONE_CONSOLE_TOOL` present, ticket cookie valid) an 11.4 KB page with no
/// `SEC_TOKEN` anywhere. I read that as the console having been rebuilt as a
/// JavaScript app, and said so publicly. It was not: on 2026-08-21 the same URL
/// with the same session returned 21.3 KB containing `SEC_TOKEN: "` -- the exact
/// spelling this module already matches -- and the provider was serving again
/// with no change from us. A capture of the working browser confirmed the
/// gateway host and the `usage` and `quota-config` request bodies are byte for
/// byte what we already send.
///
/// So the console can transiently serve a shell that has not bootstrapped. That
/// makes this the same shape as an empty 2xx body, which this crate has
/// classified as transient since the grok flaps: the response is well formed and
/// content-free, and the next attempt is likely to differ. `Decode` would drop a
/// healthy cached window over a page that comes back.
///
/// WHAT PAYS FOR THE RISK. Classifying it transient means a genuine console
/// rebuild -- where the token really is gone for good -- stale-serves instead of
/// degrading. That is acceptable only because the wire now discloses it: the
/// entry carries `stale: { since, class }` for as long as the failure lasts, and
/// `staleEpisodes` counts the run. Before those existed, `Decode` was the only
/// way such a drift became visible at all.
///
/// The discriminator is the console block, not the word "login": a signed-in
/// console page contains "login" in its own navigation, so matching on that
/// would call every healthy page a logged-out one. The block is emitted by the
/// authenticated shell, so its ABSENCE marks a page we were not served as a
/// signed-in user.
fn missing_token_error(html: &str) -> FetchError {
    if html.contains(CONSOLE_BLOCK) {
        FetchError::Upstream("qwen-cloud token-plan page was served without SEC_TOKEN".to_string())
    } else {
        FetchError::Unauthorized(
            "qwen-cloud token-plan page was not served to a signed-in session".to_string(),
        )
    }
}

/// Extract the per-page CSRF token from Qwen Cloud's console configuration.
fn extract_sec_token(html: &str) -> Option<&str> {
    let remainder = html.split_once("SEC_TOKEN: \"")?.1;
    let token = remainder.split_once('"')?.0;
    (!token.is_empty()).then_some(token)
}

/// Convert the gateway's epoch-milliseconds reset value to the wire's UTC format.
fn epoch_ms_to_iso8601(epoch_ms: i64) -> Option<String> {
    (epoch_ms > 0)
        .then_some(epoch_ms / 1000)
        .and_then(env::epoch_to_iso8601)
}

/// Build one quota window from a used fraction. A percentage is load-bearing;
/// reset timestamps are preserved only when the gateway supplies a valid value.
fn window_from_fraction(
    used_fraction: Option<f64>,
    reset_epoch_ms: Option<i64>,
    window_minutes: i64,
    kind: &str,
) -> Option<RateWindow> {
    let used_percent = used_fraction.filter(|value| value.is_finite())? * 100.0;
    Some(RateWindow {
        window_kind: Some(kind.to_string()),
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: reset_epoch_ms.and_then(epoch_ms_to_iso8601),
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    })
}

/// True when the gateway envelope carries no diagnostic content at all: no
/// `code`, no `msg`, and no `data`.
///
/// This is the observed shape when the console gateway itself answers 200 but
/// the inner token-plan API never executed (`{}` with an empty `ret`) — i.e.
/// nothing came back to decode. It is deliberately NOT the same as a rejection
/// that names itself (`code`/`msg` present), which is a real provider answer.
fn envelope_is_empty(result: &serde_json::Value) -> bool {
    let has_code = result.get("code").and_then(|c| c.as_str()).is_some();
    let has_msg = result.get("msg").and_then(|m| m.as_str()).is_some();
    let has_data = result.get("data").is_some_and(|d| !d.is_null());
    !has_code && !has_msg && !has_data
}

fn degraded_response_error(result: Option<&serde_json::Value>, reason: &str) -> FetchError {
    let detail = result.and_then(|v| {
        let code = v.get("code").and_then(|c| c.as_str());
        let msg = v.get("msg").and_then(|m| m.as_str());
        let parts: Vec<&str> = [code, msg].into_iter().flatten().collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(": "))
        }
    });
    let suffix = detail
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default();
    FetchError::Decode(format!("qwen-cloud {reason}{suffix}"))
}

/// Normalize the Qwen Cloud console-gateway response to [`Usage`].
///
/// This is pure so it can be fixture-tested without a browser session.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let response: GatewayResponse =
        crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body).map_err(|error| {
            FetchError::Decode(format!("qwen-cloud response not decodable: {error}"))
        })?;

    let result = response
        .data
        .as_ref()
        .and_then(|data| data.data_v2.as_ref())
        .and_then(|data_v2| data_v2.data.as_ref());

    if response.success_response != Some(true) {
        return Err(degraded_response_error(
            result,
            "gateway did not report success",
        ));
    }

    // A success envelope with no inner payload at all means the gateway answered
    // but the token-plan API never ran — nothing was returned to decode. That is a
    // transient edge condition, so classify it Upstream and let the refresher keep
    // serving the last-healthy window rather than replacing it with a degraded
    // entry. Mirrors grok.rs's empty-frame path and alibaba.rs's empty body.
    let result = result.ok_or_else(|| {
        FetchError::Upstream(
            "qwen-cloud gateway returned an empty envelope (transient)".to_string(),
        )
    })?;
    let result_value = result.clone();
    let result: TokenPlanResult = serde_json::from_value(result_value.clone()).map_err(|e| {
        FetchError::Decode(format!("qwen-cloud token-plan result not decodable: {e}"))
    })?;
    if result.success != Some(true) || result.code.as_deref() != Some("SUCCESS") {
        // Same boundary as above: a content-free envelope is "nothing came back"
        // (transient), while a rejection that names itself with a code/msg is a
        // real provider answer and still degrades.
        if envelope_is_empty(&result_value) {
            return Err(FetchError::Upstream(
                "qwen-cloud gateway returned an empty token-plan result (transient)".to_string(),
            ));
        }
        return Err(degraded_response_error(
            Some(&result_value),
            "token-plan result did not report success",
        ));
    }

    // A SUCCESS envelope with no plan block. Two different facts arrive here as
    // one `None`, and they send a reader to opposite places:
    //
    //   "data": null   the gateway AFFIRMS there is no token plan -- the account
    //                  never had one, or the subscription ended. A fact about the
    //                  account, nothing to fix.
    //   no `data` key  their payload and our struct disagree about its shape,
    //                  which is the class that sends someone to this repo.
    //
    // Serde collapses both to `None`, so the raw value is consulted instead. The
    // distinction is load-bearing downstream: `decode_failed` reads as "cannot
    // read it just now" and a consumer retaining last-known-good keeps routing to
    // a plan that has ENDED -- reported on insula#11 after a day of it, with real
    // work sent to a dead subscription. `no_quota_reported` is the same shape as
    // opencodego's absent Go plan and jetbrains' inactive quota, and consumers
    // already treat that family as truth rather than a fetch problem.
    let Some(quota) = result.data.as_ref() else {
        // THIS API SAYS "NO PLAN" BY OMITTING THE BLOCK, which is not what the
        // first version of this guard assumed. It applied the general rule --
        // an explicit null is a statement, a missing key is a schema
        // disagreement -- and the live payload took the other arm: the gateway
        // affirms success at BOTH levels and simply leaves `data` out.
        //
        // Learned by deploying and reading the wire, not from the source. Two
        // independent observations agree on what the shape means: the router
        // seat's operator knowledge that the subscription ended (insula#11), and
        // this host reproducing the identical response for its own ended plan.
        //
        // THE RESIDUAL RISK, stated because it is real. A schema change that
        // renamed this block would look the same from here. What makes the trade
        // right is the direction of the costs, not certainty: `decode_failed` on
        // an ended plan reads downstream as "cannot read it just now", so a
        // consumer retaining its last healthy reading keeps routing to a
        // subscription that no longer exists -- which is the reported harm, a day
        // of it, with real work sent to a dead plan. The rename case would cost a
        // silently retired provider, and it announces itself the way schema
        // changes do: on every account at once, not on the one whose plan lapsed.
        //
        // The affirmed `code == "SUCCESS"` above is the guard that stays. A
        // response that does not claim success still degrades.
        return Err(FetchError::NoQuotaReported(
            "qwen-cloud: the account has no token plan (gateway affirmed success and reported no plan block)"
                .to_string(),
        ));
    };
    let primary = window_from_fraction(
        quota.per_five_hour_percentage,
        quota.per_five_hour_reset_time,
        FIVE_HOUR_WINDOW_MINUTES,
        cortexkit_provider_usage::window_kind::FIVE_HOUR,
    );
    let secondary = window_from_fraction(
        quota.per_week_percentage,
        quota.per_week_reset_time,
        WEEKLY_WINDOW_MINUTES,
        cortexkit_provider_usage::window_kind::WEEKLY,
    );
    let monthly = window_from_fraction(
        quota.per_month_percentage,
        quota.per_month_reset_time,
        MONTHLY_WINDOW_MINUTES,
        cortexkit_provider_usage::window_kind::MONTHLY,
    );
    if primary.is_none() && secondary.is_none() && monthly.is_none() {
        // The same discriminator one level down. A plan block that NAMES its
        // percentage keys and leaves them empty is the upstream stating there are
        // no windows; a block that mentions neither key is a payload we no longer
        // understand, and calling that "no quota" would retire a working provider
        // silently on the day they rename a field.
        let stated_the_keys = PERCENTAGE_KEYS.iter().any(|key| {
            result_value
                .get("data")
                .and_then(|data| data.get(key))
                .is_some()
        });
        return Err(if stated_the_keys {
            FetchError::NoQuotaReported(
                "qwen-cloud: the token plan reports no windows (percentage fields present and empty)"
                    .to_string(),
            )
        } else {
            FetchError::Decode("qwen-cloud plan block names neither percentage field".to_string())
        });
    }

    // Placement follows CodexBar v0.66.0 (`OneConsoleTokenPlanSnapshot`): the
    // monthly window leads only when it is the sole window, and otherwise sits
    // beside the rolling windows. Either way it is published, because a monthly
    // limit that binds first is exactly the headroom a reader must not miss.
    let (primary, extra_rate_windows) = match (primary, &secondary, monthly) {
        (None, None, monthly) => (monthly, None),
        (primary, _, Some(monthly)) => (
            primary,
            Some(vec![crate::model::ExtraWindow {
                id: Some(MONTHLY_WINDOW_ID.to_string()),
                title: Some(MONTHLY_WINDOW_TITLE.to_string()),
                window: Some(monthly),
            }]),
        ),
        (primary, _, None) => (primary, None),
    };

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows,
    })
}

#[derive(Debug)]
struct TeamPlan {
    usage: Usage,
    plan_type: String,
}

#[derive(Debug)]
enum TeamError {
    Expired(FetchError),
    Skipped(&'static str),
}

/// Discovery failures say nothing about entitlement. Only an affirmative empty
/// summary or an inactive period means no active team plan; both allow personal
/// usage to serve. Login refusals report `credential_rejected`, as the personal
/// path does, so consumers know the session needs to be renewed.
fn select_team(
    result: Result<Option<TeamPlan>, TeamError>,
    settle: impl FnOnce(Option<&'static str>),
) -> Result<Option<TeamPlan>, FetchError> {
    match result {
        Ok(Some(plan)) => {
            settle(None);
            Ok(Some(plan))
        }
        Ok(None) => {
            settle(Some("no active team plan"));
            Ok(None)
        }
        Err(TeamError::Skipped(reason)) => {
            settle(Some(reason));
            Ok(None)
        }
        Err(TeamError::Expired(error)) => {
            settle(None);
            Err(error)
        }
    }
}

async fn team_json(
    client: &reqwest::Client,
    request: JsonRequest,
    cookie: &str,
    stage: &'static str,
) -> Result<serde_json::Value, TeamError> {
    let response = request
        .timeout(TEAM_TIMEOUT)
        .header(Header::new("Cookie", cookie))
        .header(Header::new("Accept", "application/json"))
        .header(Header::new("Referer", TEAM_REFERER_URL))
        .send_raw(client)
        .await
        .map_err(|_| TeamError::Skipped(stage))?;
    let sign_in_redirect = (300..400).contains(&response.status)
        && response.header("location").is_some_and(|location| {
            let lower = location.to_ascii_lowercase();
            ["login", "signin", "sign-in"]
                .iter()
                .any(|s| lower.contains(s))
        });
    if response.status == 401 || response.status == 403 || sign_in_redirect {
        return Err(TeamError::Expired(FetchError::Unauthorized(
            "qwen-cloud Team session expired; sign in again".into(),
        )));
    }
    if response.status != 200 {
        return Err(TeamError::Skipped(stage));
    }
    let root: serde_json::Value = serde_json::from_slice(
        response
            .body_for_parsing()
            .map_err(|_| TeamError::Skipped(stage))?,
    )
    .map_err(|_| TeamError::Skipped(stage))?;
    if matches!(
        root.get("code").and_then(serde_json::Value::as_str),
        Some("ConsoleNeedLogin" | "BailianGateway.Login.NotLogined" | "NO_LOGIN")
    ) {
        return Err(TeamError::Expired(FetchError::Unauthorized(
            "qwen-cloud Team login required".into(),
        )));
    }
    root.is_object()
        .then_some(root)
        .ok_or(TeamError::Skipped(stage))
}

fn team_rpc_data(root: &serde_json::Value) -> Result<&serde_json::Value, TeamError> {
    let data = root.get("data").filter(|data| data.is_object());
    if root
        .get("successResponse")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
        || data
            .and_then(|data| data.get("Success"))
            .and_then(serde_json::Value::as_bool)
            == Some(false)
    {
        return Err(TeamError::Skipped("team RPC reported failure"));
    }
    if root
        .get("successResponse")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
        && root.get("code").and_then(serde_json::Value::as_str) != Some("200")
    {
        return Err(TeamError::Skipped("unreadable team gateway envelope"));
    }
    data.ok_or(TeamError::Skipped("unreadable team gateway envelope"))
}

/// Accept nonnegative finite JSON numbers or digit-only decimal strings, up to
/// JavaScript's maximum safe integer, as CodexBar's team plugin does. Do not
/// default a missing figure to zero: missing surplus is unknown, not exhausted.
fn team_number(value: Option<&serde_json::Value>) -> Result<f64, TeamError> {
    let number = match value {
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        Some(serde_json::Value::String(s)) => {
            let (whole, fraction) = s
                .split_once('.')
                .map_or((s.as_str(), None), |(w, f)| (w, Some(f)));
            let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
            if !digits(whole) || fraction.is_some_and(|f| !digits(f)) {
                return Err(TeamError::Skipped("invalid team credit count"));
            }
            s.parse().ok()
        }
        _ => None,
    };
    number
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 9_007_199_254_740_991.0)
        .ok_or(TeamError::Skipped("missing or invalid team credit count"))
}

fn team_millis(value: Option<&serde_json::Value>) -> Result<i64, TeamError> {
    let millis = team_number(value)?;
    if millis <= 0.0 || millis.fract() != 0.0 {
        return Err(TeamError::Skipped("invalid team timestamp"));
    }
    Ok(millis as i64)
}

fn normalize_team_summary(
    summary: &serde_json::Value,
    now_ms: i64,
) -> Result<Option<TeamPlan>, TeamError> {
    if summary.get("Data").is_some_and(serde_json::Value::is_null) {
        return Ok(None);
    }
    let data = summary
        .get("Data")
        .filter(|data| data.is_object())
        .ok_or(TeamError::Skipped("unreadable team subscription"))?;
    let groups = data
        .get("SubscriptionGroupList")
        .and_then(serde_json::Value::as_array)
        .ok_or(TeamError::Skipped("unreadable team subscription groups"))?;
    if groups.is_empty() {
        return Ok(None);
    }
    let start = team_millis(data.get("StartTime"))?;
    let end = team_millis(data.get("EndTime"))?;
    if end <= start {
        return Err(TeamError::Skipped("invalid team subscription period"));
    }
    if now_ms < start || now_ms >= end {
        return Ok(None);
    }
    if data.get("ProductCode").and_then(serde_json::Value::as_str) != Some(TEAM_PRODUCT) {
        return Err(TeamError::Skipped("unexpected team subscription product"));
    }
    if groups.len() != 1 {
        return Err(TeamError::Skipped("multiple team credit pools"));
    }
    let group = &groups[0];
    let equities = group
        .get("EquityList")
        .and_then(serde_json::Value::as_array)
        .ok_or(TeamError::Skipped("unreadable team credit equity"))?;
    let credits: Vec<_> = equities
        .iter()
        .filter(|equity| {
            equity.get("EquityCode").and_then(serde_json::Value::as_str) == Some("credit_value")
        })
        .collect();
    if credits.len() != 1 {
        return Err(TeamError::Skipped(
            "ambiguous or missing team credit equity",
        ));
    }
    let total = team_number(credits[0].get("TotalValue"))?;
    let surplus = team_number(credits[0].get("SurplusValue"))?;
    if total <= 0.0 || surplus > total {
        return Err(TeamError::Skipped("invalid team credit count"));
    }
    let resets_at = match group.get("NextCycleFlushTime").filter(|v| !v.is_null()) {
        Some(value) => Some(
            epoch_ms_to_iso8601(team_millis(Some(value))?)
                .ok_or(TeamError::Skipped("invalid team cycle reset"))?,
        ),
        None => None,
    };
    let spec = group
        .get("SpecType")
        .and_then(serde_json::Value::as_str)
        .filter(|s| {
            (1..=40).contains(&s.len())
                && s.as_bytes()[0].is_ascii_lowercase()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        });
    let plan_type = spec.map_or_else(
        || "Team Token Plan".into(),
        |spec| format!("{}{} Team", spec[..1].to_ascii_uppercase(), &spec[1..]),
    );
    // Team is the shared subscription's credit allowance, not the personal plan's
    // rolling five-hour/weekly/monthly token windows. Its percent is DERIVED from
    // stated total and surplus; no count is reconstructed. The named extra gives
    // it a distinct id/label. Publishing it again as primary/secondary/tertiary
    // or as a spend balance would describe the same allowance twice.
    let window = RateWindow {
        window_kind: None,
        used_percent: ((total - surplus) / total * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at,
        window_minutes: None,
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    };
    Ok(Some(TeamPlan {
        usage: Usage {
            extra_rate_windows: Some(vec![ExtraWindow {
                id: Some("team".into()),
                title: Some("Team".into()),
                window: Some(window),
            }]),
            ..Usage::default()
        },
        plan_type,
    }))
}

async fn fetch_team(
    client: &reqwest::Client,
    cookie: &str,
    urls: [&str; 3],
    now_ms: i64,
) -> Result<Option<TeamPlan>, TeamError> {
    let info = team_json(
        client,
        JsonRequest::get(urls[0]),
        cookie,
        "team user info request failed",
    )
    .await?;
    let token = info
        .get("data")
        .and_then(|data| data.get("secToken"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or(TeamError::Skipped("team user info has no secToken"))?;
    let human = team_json(
        client,
        JsonRequest::post_form(
            urls[1],
            &[
                ("product", "ea-service"),
                ("action", "LoadHumanInfo"),
                ("sec_token", token),
                ("region", "ap-southeast-1"),
                ("params", "{}"),
            ],
        )
        .header(Header::new("Origin", "https://home.qwencloud.com")),
        cookie,
        "team billing selector request failed",
    )
    .await?;
    let nbid = team_rpc_data(&human)?
        .get("Data")
        .and_then(|data| data.get("SellerInfoDto"))
        .and_then(|seller| seller.get("Nbid"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or(TeamError::Skipped("team billing selector has no Nbid"))?;
    // Nbid identifies the billing account returned for this session, not the
    // user's login UID. Without that selector, prefer personal usage rather than
    // query a plan whose billing-account attribution we cannot establish.
    let params = serde_json::json!({"productCode": TEAM_PRODUCT, "Nbid": nbid}).to_string();
    let summary = team_json(
        client,
        JsonRequest::post_form(
            urls[2],
            &[
                ("product", "BssOpenAPI-V3"),
                ("action", "GetSeatSubscriptionSummary"),
                ("sec_token", token),
                ("region", "cn-hangzhou"),
                ("params", &params),
                ("language", "zh-CN"),
            ],
        )
        .header(Header::new("Origin", "https://home.qwencloud.com")),
        cookie,
        "team summary request failed",
    )
    .await?;
    normalize_team_summary(team_rpc_data(&summary)?, now_ms)
}

/// The Qwen Cloud token-plan usage provider.
pub struct QwenCloudProvider {
    vault: crate::cookie_vault::CookieVault,
    http: reqwest::Client,
    team_http: Option<reqwest::Client>,
    team_skips: Mutex<HashMap<String, &'static str>>,
}

impl QwenCloudProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<std::sync::Arc<dyn crate::credential_source::CredentialSource>>,
        handle_loader: std::sync::Arc<crate::vault_handles::VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            // Inspect sign-in redirects before following them, without changing
            // the personal path's existing HTTP client or redirect behavior.
            team_http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .pool_idle_timeout(crate::http::POOL_IDLE_TIMEOUT)
                .tcp_keepalive(crate::http::POOL_IDLE_TIMEOUT)
                .build()
                .ok(),
            team_skips: Mutex::new(HashMap::new()),
            vault: crate::cookie_vault::CookieVault::new(
                credential_source,
                handle_loader,
                COOKIE_FAMILY,
            ),
        }
    }

    fn settle_team_skip(&self, handle_id: &str, reason: Option<&'static str>) {
        let mut skips = self
            .team_skips
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if skips.get(handle_id).copied() != reason {
            match reason {
                Some(reason) => eprintln!(
                    "{} qwen-cloud Team skipped ({handle_id}): {reason}; trying personal plan",
                    crate::LOG_TAG
                ),
                None => eprintln!(
                    "{} qwen-cloud Team skip cleared ({handle_id})",
                    crate::LOG_TAG
                ),
            }
        }
        match reason {
            Some(reason) => {
                skips.insert(handle_id.to_string(), reason);
            }
            None => {
                skips.remove(handle_id);
            }
        }
    }
}

#[async_trait]
impl UsageProvider for QwenCloudProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        self.vault.handles()
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let (jar, source) = self.vault.jar_for(handle).await?;
            if !jar.has_cookie_named(|name| name == "login_qwencloud_ticket") {
                return Err(FetchError::NoSession(format!(
                    "no Qwen Cloud login ticket {}",
                    crate::cookie_vault::DEPOSIT_PHRASE
                )));
            }

            let cookie_header = jar.header();
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|d| i64::try_from(d.as_millis()).ok());
            let team = match (&self.team_http, now_ms) {
                (Some(client), Some(now_ms)) => {
                    fetch_team(
                        client,
                        &cookie_header,
                        [TEAM_INFO_URL, TEAM_HUMAN_URL, TEAM_SUMMARY_URL],
                        now_ms,
                    )
                    .await
                }
                _ => Err(TeamError::Skipped("team client or clock unavailable")),
            };
            if let Some(team) = select_team(team, |reason| {
                self.settle_team_skip(handle.stable_id(), reason)
            })? {
                let mut entry = ProviderUsage::healthy(PROVIDER_NAME, None, source, team.usage);
                entry.account_info = Some(AccountInfo {
                    plan_type: Some(team.plan_type),
                    ..AccountInfo::default()
                });
                return Ok(entry);
            }
            let token_page = JsonRequest::get(TOKEN_PLAN_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Cookie", &cookie_header))
                .header(Header::new("User-Agent", BROWSER_USER_AGENT))
                .header(Header::new(
                    "Accept",
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                ))
                .send(&self.http)
                .await?;
            let html = String::from_utf8_lossy(&token_page);
            let sec_token = extract_sec_token(&html).ok_or_else(|| missing_token_error(&html))?;

            let body = JsonRequest::post_form(
                USAGE_URL,
                &[
                    ("product", "sfm_bailian"),
                    ("action", "IntlBroadScopeAspnGateway"),
                    ("sec_token", sec_token),
                    ("region", "ap-southeast-1"),
                    ("params", GATEWAY_PARAMS),
                ],
            )
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", &cookie_header))
            .header(Header::new("User-Agent", BROWSER_USER_AGENT))
            .header(Header::new("Origin", "https://home.qwencloud.com"))
            .header(Header::new("Referer", TOKEN_PLAN_URL))
            .send(&self.http)
            .await?;

            let usage = normalize_usage(&body)?;
            let mut enriched = usage;

            // Best-effort enrichment: fetch quota-config + subscription to derive
            // absolute counts. If either call fails the windows stay without counts
            // (the percentage is still honest).
            let config_body = JsonRequest::post_form(
                QUOTA_CONFIG_URL,
                &[
                    ("product", "sfm_bailian"),
                    ("action", "IntlBroadScopeAspnGateway"),
                    ("sec_token", sec_token),
                    ("region", "ap-southeast-1"),
                    ("params", QUOTA_CONFIG_PARAMS),
                ],
            )
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", &cookie_header))
            .header(Header::new("User-Agent", BROWSER_USER_AGENT))
            .header(Header::new("Origin", "https://home.qwencloud.com"))
            .header(Header::new("Referer", TOKEN_PLAN_URL))
            .send(&self.http)
            .await;
            let sub_body = JsonRequest::post_form(
                SUBSCRIPTION_URL,
                &[
                    ("product", "sfm_bailian"),
                    ("action", "IntlBroadScopeAspnGateway"),
                    ("sec_token", sec_token),
                    ("region", "ap-southeast-1"),
                    ("params", SUBSCRIPTION_PARAMS),
                ],
            )
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", &cookie_header))
            .header(Header::new("User-Agent", BROWSER_USER_AGENT))
            .header(Header::new("Origin", "https://home.qwencloud.com"))
            .header(Header::new("Referer", TOKEN_PLAN_URL))
            .send(&self.http)
            .await;
            if let (Ok(config_bytes), Ok(sub_bytes)) = (config_body, sub_body) {
                enrich_with_counts(&mut enriched, &config_bytes, &sub_bytes);
            }

            Ok(ProviderUsage::healthy(
                PROVIDER_NAME,
                None,
                source,
                enriched,
            ))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {

    // UPSTREAM-SHAPED fixtures: field names, envelopes and selection rules come
    // from CodexBar v0.74.0 qwencloud-team.ts. Values are synthetic; these are not
    // captured account balances or evidence of live Team access.
    const TEAM_NOW_MS: i64 = 1_791_000_000_000;
    const TEAM_INFO: &str = r#"{"data":{"secToken":"team-sec-token"}}"#;
    const TEAM_HUMAN: &str = r#"{"successResponse":true,"data":{"Success":true,"Data":{"SellerInfoDto":{"Nbid":"seller-123"}}}}"#;
    const TEAM_SUMMARY: &str = r#"{"successResponse":true,"data":{"Success":true,"Data":{"ProductCode":"sfm_tokenplanteams_dp_intl","StartTime":1790000000000,"EndTime":1792000000000,"SubscriptionGroupList":[{"SpecType":"pro","SubscriptionAssignedNumber":3,"SubscriptionTotalNumber":5,"NextCycleFlushTime":1791043200000,"EquityList":[{"EquityCode":"credit_value","TotalValue":"1000.50","SurplusValue":"750.375"}]}]}}}"#;

    async fn team_fixture(
        replies: Vec<(u16, String, Option<&'static str>)>,
    ) -> (Result<Option<TeamPlan>, TeamError>, Vec<String>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body, location) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(crate::loopback::read_request(&mut socket).await);
                let location = location
                    .map(|l| format!("Location: {l}\r\n"))
                    .unwrap_or_default();
                socket.write_all(format!("HTTP/1.1 {status} fixture\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        let urls = [
            format!("{base}/tool/user/info.json"),
            format!("{base}/data/api.json?product=ea-service&action=LoadHumanInfo"),
            format!("{base}/data/api.json?product=BssOpenAPI-V3&action=GetSeatSubscriptionSummary"),
        ];
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let result = fetch_team(
            &client,
            "login_qwencloud_ticket=fixture-ticket",
            [&urls[0], &urls[1], &urls[2]],
            TEAM_NOW_MS,
        )
        .await;
        let requests = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        (result, requests)
    }

    fn team_replies(summary: &str) -> Vec<(u16, String, Option<&'static str>)> {
        [TEAM_INFO, TEAM_HUMAN, summary]
            .into_iter()
            .map(|body| (200, body.into(), None))
            .collect()
    }

    fn team_summary_value() -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(TEAM_SUMMARY).unwrap()["data"].clone()
    }

    #[tokio::test]
    async fn active_team_plan_publishes_one_distinct_credit_window() {
        let (result, requests) = team_fixture(team_replies(TEAM_SUMMARY)).await;
        let plan = select_team(result, |reason| assert_eq!(reason, None))
            .unwrap()
            .unwrap();
        assert_eq!(plan.plan_type, "Pro Team");
        assert!(plan.usage.primary.is_none());
        assert!(plan.usage.secondary.is_none());
        assert!(plan.usage.tertiary.is_none());
        let extras = plan.usage.extra_rate_windows.unwrap();
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].id.as_deref(), Some("team"));
        assert_eq!(extras[0].title.as_deref(), Some("Team"));
        let window = extras[0].window.as_ref().unwrap();
        assert_eq!(window.used_percent, 25.0);
        assert_eq!(window.resets_at.as_deref(), Some("2026-10-03T16:00:00Z"));
        assert!(window.used_count.is_none());
        assert!(window.total_count.is_none());
        assert!(window.window_kind.is_none());
        assert!(window.window_minutes.is_none());
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /tool/user/info.json HTTP/1.1"));
        for request in &requests {
            let headers = request
                .split("\r\n\r\n")
                .next()
                .unwrap()
                .to_ascii_lowercase();
            assert!(headers.contains("cookie: login_qwencloud_ticket=fixture-ticket"));
            assert!(headers.contains("accept: application/json"));
            assert!(
                headers.contains("referer: https://home.qwencloud.com/analytics/token-plan/team")
            );
        }
        assert!(requests[1]
            .starts_with("POST /data/api.json?product=ea-service&action=LoadHumanInfo HTTP/1.1"));
        assert!(requests[2].starts_with(
            "POST /data/api.json?product=BssOpenAPI-V3&action=GetSeatSubscriptionSummary HTTP/1.1"
        ));
        let human: HashMap<_, _> =
            url::form_urlencoded::parse(requests[1].split_once("\r\n\r\n").unwrap().1.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(human.len(), 5);
        assert_eq!(human["product"], "ea-service");
        assert_eq!(human["action"], "LoadHumanInfo");
        assert_eq!(human["region"], "ap-southeast-1");
        assert_eq!(human["sec_token"], "team-sec-token");
        assert_eq!(human["params"], "{}");
        let summary: HashMap<_, _> =
            url::form_urlencoded::parse(requests[2].split_once("\r\n\r\n").unwrap().1.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(summary.len(), 6);
        assert_eq!(summary["product"], "BssOpenAPI-V3");
        assert_eq!(summary["action"], "GetSeatSubscriptionSummary");
        assert_eq!(summary["region"], "cn-hangzhou");
        assert_eq!(summary["language"], "zh-CN");
        assert_eq!(summary["sec_token"], "team-sec-token");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&summary["params"]).unwrap(),
            serde_json::json!({"productCode":"sfm_tokenplanteams_dp_intl","Nbid":"seller-123"})
        );
        for request in &requests[1..] {
            let headers = request
                .split("\r\n\r\n")
                .next()
                .unwrap()
                .to_ascii_lowercase();
            assert!(headers.contains("origin: https://home.qwencloud.com"));
            assert!(headers.contains("content-type: application/x-www-form-urlencoded"));
        }
    }

    #[tokio::test]
    async fn no_active_team_plan_falls_back_to_personal_windows() {
        let mut inactive = team_summary_value();
        inactive["Data"]["EndTime"] = serde_json::json!(TEAM_NOW_MS);
        for data in [
            serde_json::json!({"Data":null}),
            serde_json::json!({"Data":{"SubscriptionGroupList":[]}}),
            inactive,
        ] {
            let summary = serde_json::json!({"code":"200","data":data}).to_string();
            let (team, _) = team_fixture(team_replies(&summary)).await;
            let selected = select_team(team, |reason| {
                assert_eq!(reason, Some("no active team plan"))
            })
            .unwrap();
            assert!(selected.is_none());
            let personal = normalize_usage(HAR_RESPONSE.as_bytes()).unwrap();
            assert_eq!(personal.primary.unwrap().window_minutes, Some(300));
            assert_eq!(personal.secondary.unwrap().window_minutes, Some(10080));
            assert!(personal.extra_rate_windows.is_none());
        }
    }

    #[tokio::test]
    async fn team_discovery_failures_keep_personal_usage_serving() {
        let failures = [
            vec![(200, r#"{"data":{}}"#.into(), None)],
            vec![(500, "request failed".into(), None)],
            vec![
                (200, TEAM_INFO.into(), None),
                (
                    200,
                    r#"{"successResponse":true,"data":{"Data":{"SellerInfoDto":{}}}}"#.into(),
                    None,
                ),
            ],
            vec![
                (200, TEAM_INFO.into(), None),
                (503, "request failed".into(), None),
            ],
            team_replies("not JSON"),
            team_replies(r#"{"successResponse":false,"data":{"Data":null}}"#),
            team_replies(r#"{"successResponse":true,"data":{"Success":false,"Data":null}}"#),
            team_replies(r#"{"successResponse":true,"data":{}}"#),
        ];
        for replies in failures {
            let (team, _) = team_fixture(replies).await;
            let selected = select_team(team, |reason| {
                let reason = reason.expect("a failed request must explain the skip");
                assert_ne!(reason, "no active team plan");
            })
            .expect("a team discovery failure must not suppress personal usage");
            let usage = match selected {
                Some(plan) => plan.usage,
                None => normalize_usage(HAR_RESPONSE.as_bytes()).unwrap(),
            };
            assert!((usage.primary.unwrap().used_percent - 13.1177).abs() < 0.001);
            assert!(usage.secondary.is_some());
        }
    }

    #[test]
    fn team_missing_surplus_is_unknown_not_exhaustion() {
        for surplus in [None, Some(serde_json::Value::Null)] {
            let mut summary = team_summary_value();
            let equity = summary["Data"]["SubscriptionGroupList"][0]["EquityList"][0]
                .as_object_mut()
                .unwrap();
            equity.remove("SurplusValue");
            if let Some(surplus) = surplus {
                equity.insert("SurplusValue".into(), surplus);
            }
            assert!(
                matches!(
                    normalize_team_summary(&summary, TEAM_NOW_MS),
                    Err(TeamError::Skipped(_))
                ),
                "missing surplus cannot publish a 100% used window"
            );
        }
    }

    #[test]
    fn team_summary_rejects_another_products_credit_pool() {
        let mut summary = team_summary_value();
        summary["Data"]["ProductCode"] = serde_json::json!("sfm_tokenplansolo_public_intl");
        assert!(matches!(
            normalize_team_summary(&summary, TEAM_NOW_MS),
            Err(TeamError::Skipped("unexpected team subscription product"))
        ));
    }

    #[test]
    fn team_summary_refuses_unreadable_or_ambiguous_allowances() {
        for (pointer, value) in [
            ("/Data/StartTime", serde_json::json!(0)),
            ("/Data/EndTime", serde_json::json!(1790000000000i64)),
            ("/Data/SubscriptionGroupList", serde_json::json!([{}, {}])),
            (
                "/Data/SubscriptionGroupList/0/EquityList",
                serde_json::json!([]),
            ),
            (
                "/Data/SubscriptionGroupList/0/EquityList",
                serde_json::json!([{"EquityCode":"credit_value"},{"EquityCode":"credit_value"}]),
            ),
            (
                "/Data/SubscriptionGroupList/0/EquityList/0/TotalValue",
                serde_json::json!(0),
            ),
            (
                "/Data/SubscriptionGroupList/0/EquityList/0/SurplusValue",
                serde_json::json!(1001),
            ),
            (
                "/Data/SubscriptionGroupList/0/NextCycleFlushTime",
                serde_json::json!(1.5),
            ),
        ] {
            let mut summary = team_summary_value();
            *summary.pointer_mut(pointer).unwrap() = value;
            assert!(
                matches!(
                    normalize_team_summary(&summary, TEAM_NOW_MS),
                    Err(TeamError::Skipped(_))
                ),
                "{pointer}"
            );
        }
        assert!(matches!(
            normalize_team_summary(&serde_json::json!({}), TEAM_NOW_MS),
            Err(TeamError::Skipped(_))
        ));
        let mut summary = team_summary_value();
        summary["Data"]["SubscriptionGroupList"][0]
            .as_object_mut()
            .unwrap()
            .remove("NextCycleFlushTime");
        summary["Data"]["SubscriptionGroupList"][0]["SpecType"] = serde_json::json!("<unsafe>");
        let plan = normalize_team_summary(&summary, TEAM_NOW_MS)
            .unwrap()
            .unwrap();
        assert_eq!(plan.plan_type, "Team Token Plan");
        assert!(plan.usage.extra_rate_windows.unwrap()[0]
            .window
            .as_ref()
            .unwrap()
            .resets_at
            .is_none());
    }

    #[tokio::test]
    async fn team_sign_in_redirects_and_refusals_are_expired_sessions() {
        for step in 0..3 {
            for reply in [
                (
                    302,
                    String::new(),
                    Some("https://account.qwencloud.com/login"),
                ),
                (401, "expired".into(), None),
                (403, "expired".into(), None),
                (200, r#"{"code":"NO_LOGIN"}"#.into(), None),
                (200, r#"{"code":"ConsoleNeedLogin"}"#.into(), None),
                (
                    200,
                    r#"{"code":"BailianGateway.Login.NotLogined"}"#.into(),
                    None,
                ),
            ] {
                let mut replies = team_replies(TEAM_SUMMARY);
                replies.truncate(step);
                replies.push(reply);
                let (result, _) = team_fixture(replies).await;
                let error = select_team(result, |_| {}).unwrap_err();
                assert_eq!(error.error_class(), "credential_rejected");
            }
        }
    }

    #[test]
    fn team_credit_numbers_follow_the_plugins_safe_decimal_grammar() {
        for value in [
            serde_json::json!("-1"),
            serde_json::json!(" 1"),
            serde_json::json!("1."),
            serde_json::json!("1e3"),
            serde_json::json!("1,000"),
            serde_json::json!(true),
            serde_json::json!(9007199254740992u64),
        ] {
            assert!(team_number(Some(&value)).is_err(), "{value}");
        }
        for value in [serde_json::json!("1.25"), serde_json::json!(1.25)] {
            assert_eq!(team_number(Some(&value)).unwrap(), 1.25);
        }
    }

    /// A gateway success with an explicitly null plan block is NOT a decode error.
    ///
    /// LIVE SHAPE, observed on this host 2026-08-24: the account's token-plan
    /// subscription ended, and the console gateway kept answering `code: SUCCESS`,
    /// `msg: "Success."` with no plan data. That was published as `decode_failed`,
    /// which reads downstream as "cannot read it just now" -- so a consumer
    /// retaining its last healthy reading kept routing to a subscription that no
    /// longer existed, for a day, with real work sent to it (insula#11).
    ///
    /// The class is the fix. `no_quota_reported` is the same statement as
    /// opencodego's absent Go plan: the credential works, the account has nothing
    /// to report, and there is nothing for an operator to repair.
    #[test]
    fn a_success_envelope_with_a_null_plan_block_reports_no_quota() {
        let body = br#"{"successResponse":true,"data":{"DataV2":{"data":{"success":true,"code":"SUCCESS","msg":"Success.","data":null}}}}"#;
        match normalize_usage(body) {
            Err(FetchError::NoQuotaReported(message)) => {
                assert!(
                    message.contains("no token plan"),
                    "the message must say what the account lacks: {message}"
                );
            }
            other => panic!("expected NoQuotaReported, got {other:?}"),
        }
    }

    /// A response that does NOT affirm success still degrades.
    ///
    /// The control, and the guard that survived the correction below: an omitted
    /// plan block is read as "no plan" only under an affirmed `SUCCESS`. Without
    /// this case, treating every block-less response as absent quota would pass,
    /// and a provider-side failure would render as a healthy account with nothing
    /// to report.
    ///
    /// SYNTHETIC: constructed to exercise the rejecting arm.
    #[test]
    fn a_response_that_does_not_affirm_success_still_degrades() {
        let body = br#"{"successResponse":true,"data":{"DataV2":{"data":{"success":false,"code":"FORBIDDEN","msg":"denied"}}}}"#;
        match normalize_usage(body) {
            Err(FetchError::Decode(message)) => {
                assert!(
                    message.contains("did not report success"),
                    "the message must name the refusal: {message}"
                );
            }
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    /// An omitted plan block under an affirmed success is absent quota.
    ///
    /// LIVE SHAPE, read off the deployed wire on 2026-08-24 and NOT what the
    /// first version of this guard predicted. That version applied the general
    /// rule -- an explicit null is a statement, a missing key is a schema
    /// disagreement -- and this API takes the other arm: it affirms success at
    /// both levels and simply leaves `data` out.
    ///
    /// The correction came from deploying and reading the wire rather than from
    /// re-reading the source, which is the only place the difference was visible.
    #[test]
    fn an_omitted_plan_block_under_affirmed_success_reports_no_quota() {
        let body = br#"{"successResponse":true,"data":{"DataV2":{"data":{"success":true,"code":"SUCCESS","msg":"Success."}}}}"#;
        match normalize_usage(body) {
            Err(FetchError::NoQuotaReported(message)) => {
                assert!(
                    message.contains("no token plan"),
                    "the message must say what the account lacks: {message}"
                );
            }
            other => panic!("expected NoQuotaReported, got {other:?}"),
        }
    }

    /// A plan block naming its percentage keys but leaving them empty states
    /// "no windows" rather than failing to parse.
    ///
    /// SYNTHETIC: the same discriminator one level down, for the shape where the
    /// plan exists but reports nothing.
    #[test]
    fn a_plan_block_with_empty_percentage_fields_reports_no_quota() {
        let body = br#"{"successResponse":true,"data":{"DataV2":{"data":{"success":true,"code":"SUCCESS","data":{"per5HourPercentage":null,"per1WeekPercentage":null}}}}}"#;
        match normalize_usage(body) {
            Err(FetchError::NoQuotaReported(message)) => {
                assert!(message.contains("no windows"), "got {message}");
            }
            other => panic!("expected NoQuotaReported, got {other:?}"),
        }
    }

    /// Wrap a token-plan block in a SUCCESS usage envelope.
    fn usage_body(plan: &str) -> Vec<u8> {
        format!(
            r#"{{"successResponse":true,"data":{{"DataV2":{{"data":{{"success":true,"code":"SUCCESS","data":{plan}}}}}}}}}"#
        )
        .into_bytes()
    }

    /// A plan block naming ONLY an empty weekly field states "no windows".
    ///
    /// SYNTHETIC. This is the case the stated-keys list used to get wrong: it
    /// looked for `perWeekPercentage`, while the wire field (and the struct's
    /// own rename) is `per1WeekPercentage`, so this block read as a payload we
    /// could not understand and published `decode_failed`.
    #[test]
    fn a_plan_block_naming_only_an_empty_weekly_field_reports_no_quota() {
        match normalize_usage(&usage_body(r#"{"per1WeekPercentage":null}"#)) {
            Err(FetchError::NoQuotaReported(message)) => {
                assert!(message.contains("no windows"), "got {message}");
            }
            other => panic!("expected NoQuotaReported, got {other:?}"),
        }
    }

    /// A monthly-only plan block with its fields empty states "no windows".
    ///
    /// SYNTHETIC: the monthly key counts as a stated field like the other two.
    #[test]
    fn a_monthly_only_plan_block_with_empty_fields_reports_no_quota() {
        let body = usage_body(r#"{"per1MonthPercentage":null,"per1MonthResetTime":null}"#);
        match normalize_usage(&body) {
            Err(FetchError::NoQuotaReported(message)) => {
                assert!(message.contains("no windows"), "got {message}");
            }
            other => panic!("expected NoQuotaReported, got {other:?}"),
        }
    }

    /// Every key in `PERCENTAGE_KEYS` is a field the struct actually reads, and
    /// every percentage field the struct reads is in the list.
    ///
    /// serde's `rename` takes only a literal, so the list and the struct are two
    /// spellings of the same names; this is what keeps them from drifting apart
    /// the way `perWeekPercentage` once did.
    #[test]
    fn percentage_keys_match_the_struct_renames() {
        let read = |value: serde_json::Value| -> Vec<Option<f64>> {
            let usage: TokenPlanUsage = serde_json::from_value(value).unwrap();
            vec![
                usage.per_five_hour_percentage,
                usage.per_week_percentage,
                usage.per_month_percentage,
            ]
        };
        // One key at a time: each must land in exactly its own struct field.
        for (index, key) in PERCENTAGE_KEYS.iter().enumerate() {
            let fields = read(serde_json::json!({ *key: 0.5 }));
            let populated: Vec<usize> = fields
                .iter()
                .enumerate()
                .filter_map(|(i, f)| f.map(|_| i))
                .collect();
            assert_eq!(
                populated,
                vec![index],
                "`{key}` must be read by the struct's percentage field #{index}"
            );
        }
        // And the list covers every percentage field the struct has.
        assert_eq!(PERCENTAGE_KEYS.len(), read(serde_json::json!({})).len());
    }

    // The tests below check where the monthly window is placed (primary when it
    // is alone, an extra beside the rolling windows) and which cap it receives.
    // UPSTREAM-DERIVED fixtures: the monthly payload and expectations come from
    // CodexBar v0.66.0 `Tests/CodexBarTests/TokenPlanMonthlyWindowTests.swift`
    // (`monthly`, `monthly usage retains rolling windows`, and `web monthly
    // quota preserves totals and provider identity`), wrapped in the console
    // gateway envelope this module actually receives.
    const UPSTREAM_MONTHLY: &str =
        r#"{"per1MonthPercentage":0.25,"per1MonthResetTime":1791043200000}"#;

    /// A block reporting only the monthly window publishes it as `primary`.
    #[test]
    fn a_monthly_only_block_makes_the_monthly_window_primary() {
        let usage = normalize_usage(&usage_body(UPSTREAM_MONTHLY)).unwrap();
        let primary = usage.primary.expect("the monthly window leads when alone");
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.window_kind.as_deref(), Some("monthly"));
        assert_eq!(primary.window_minutes, Some(43_200));
        assert_eq!(primary.resets_at.as_deref(), Some("2026-10-03T16:00:00Z"));
        assert!(usage.secondary.is_none());
        assert!(usage.tertiary.is_none());
        assert!(
            usage.extra_rate_windows.is_none(),
            "a primary monthly window must not also be published as an extra"
        );
    }

    /// Beside the rolling windows, the monthly window is an extra with id
    /// `monthly`, and the rolling windows keep their slots.
    #[test]
    fn a_monthly_window_beside_the_rolling_windows_is_an_extra() {
        let usage = normalize_usage(&usage_body(
            r#"{"per5HourPercentage":0.1,"per1WeekPercentage":0.2,"per1MonthPercentage":0.3}"#,
        ))
        .unwrap();
        let primary = usage.primary.expect("five-hour window");
        assert_eq!(primary.used_percent, 10.0);
        assert_eq!(primary.window_kind.as_deref(), Some("five_hour"));
        assert_eq!(primary.window_minutes, Some(300));
        let secondary = usage.secondary.expect("weekly window");
        assert_eq!(secondary.used_percent, 20.0);
        assert_eq!(secondary.window_kind.as_deref(), Some("weekly"));
        assert_eq!(secondary.window_minutes, Some(10_080));
        assert!(usage.tertiary.is_none());
        let extras = usage
            .extra_rate_windows
            .expect("the monthly window is kept");
        assert_eq!(extras.len(), 1);
        assert_eq!(extras[0].id.as_deref(), Some("monthly"));
        assert_eq!(extras[0].title.as_deref(), Some("Monthly"));
        let monthly = extras[0].window.as_ref().expect("monthly window");
        assert!((monthly.used_percent - 30.0).abs() < 1e-9);
        assert_eq!(monthly.window_kind.as_deref(), Some("monthly"));
        assert_eq!(monthly.window_minutes, Some(43_200));
    }

    /// One rolling window is enough to push the monthly window into the extras:
    /// upstream promotes it only when BOTH rolling windows are absent.
    #[test]
    fn a_monthly_window_beside_only_the_weekly_window_is_an_extra() {
        let usage = normalize_usage(&usage_body(
            r#"{"per1WeekPercentage":0.2,"per1MonthPercentage":0.3}"#,
        ))
        .unwrap();
        assert!(usage.primary.is_none(), "no five-hour window was reported");
        assert_eq!(
            usage.secondary.expect("weekly").window_minutes,
            Some(10_080)
        );
        let extras = usage
            .extra_rate_windows
            .expect("the monthly window is kept");
        assert_eq!(extras[0].id.as_deref(), Some("monthly"));
    }

    /// Without a monthly field the live-shaped fixture has only its two named
    /// windows, with no extra-window key at all.
    ///
    /// The percentages are pinned as serde_json parses the fixture (its default
    /// float parser, not Rust's literal parser, so the weekly value's last digits
    /// differ from the fixture text).
    #[test]
    fn an_absent_monthly_window_leaves_the_output_unchanged() {
        let usage = normalize_usage(HAR_RESPONSE.as_bytes()).unwrap();
        assert_eq!(
            serde_json::to_string(&usage).unwrap(),
            r#"{"primary":{"usedPercent":13.117665718963334,"resetsAt":"2026-07-20T00:09:00Z","windowMinutes":300,"windowKind":"five_hour"},"secondary":{"usedPercent":5.3834972282899995,"resetsAt":"2026-07-26T14:09:00Z","windowMinutes":10080,"windowKind":"weekly"}}"#
        );
    }

    /// The `monthly` cap lands on the monthly window wherever it sits, and a
    /// rolling cap never lands on it.
    ///
    /// Upstream-derived: the `{"standard":{"monthly":45000}}` cap table is the
    /// one CodexBar's monthly test uses.
    #[test]
    fn the_monthly_cap_lands_on_the_monthly_window() {
        // Monthly as primary: a five-hour cap in the same row must not apply.
        let mut alone = normalize_usage(&usage_body(UPSTREAM_MONTHLY)).unwrap();
        let caps = gateway(
            r#"{"success":true,"data":{"standard":{"five_hour":1000,"weekly":10000,"monthly":45000}}}"#,
        );
        enrich_with_counts(&mut alone, &caps, &subscription_body("standard"));
        assert_eq!(alone.primary.unwrap().total_count, Some(45_000.0));

        // Monthly as an extra: each window gets its own cap.
        let mut mixed = normalize_usage(&usage_body(
            r#"{"per5HourPercentage":0.1,"per1WeekPercentage":0.2,"per1MonthPercentage":0.3}"#,
        ))
        .unwrap();
        enrich_with_counts(&mut mixed, &caps, &subscription_body("standard"));
        assert_eq!(mixed.primary.unwrap().total_count, Some(1000.0));
        assert_eq!(mixed.secondary.unwrap().total_count, Some(10_000.0));
        let extras = mixed.extra_rate_windows.unwrap();
        assert_eq!(
            extras[0].window.as_ref().unwrap().total_count,
            Some(45_000.0)
        );
    }

    /// A plan block mentioning neither percentage key degrades.
    ///
    /// The control for the test above. Without it, treating any window-less plan
    /// block as "no quota" would pass -- and that is exactly what a field rename
    /// looks like.
    ///
    /// SYNTHETIC.
    #[test]
    fn a_plan_block_naming_neither_percentage_key_degrades() {
        let body = br#"{"successResponse":true,"data":{"DataV2":{"data":{"success":true,"code":"SUCCESS","data":{"someRenamedField":0.42}}}}}"#;
        match normalize_usage(body) {
            Err(FetchError::Decode(message)) => {
                assert!(
                    message.contains("neither percentage field"),
                    "got {message}"
                );
            }
            other => panic!("expected Decode, got {other:?}"),
        }
    }
    use super::*;

    const HAR_RESPONSE: &str = r#"{
      "code": "200",
      "data": {
        "DataV2": {
          "ret": ["SUCCESS::接口调用成功"],
          "data": {
            "msg": "Success.",
            "code": "SUCCESS",
            "data": {
              "per5HourPercentage": 0.13117665718963334,
              "per1WeekResetTime": 1785074940000,
              "per5HourResetTime": 1784506140000,
              "per1WeekPercentage": 0.053834972282900004
            },
            "requestId": "...",
            "success": true
          }
        },
        "success": true,
        "httpStatus": 200,
        "api": "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage"
      },
      "successResponse": true
    }"#;

    #[test]
    fn normalizes_live_har_token_plan_response() {
        let usage = normalize_usage(HAR_RESPONSE.as_bytes()).unwrap();
        let primary = usage.primary.expect("five-hour quota window");
        assert!((primary.used_percent - 13.1177).abs() < 0.001);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-20T00:09:00Z"));
        assert_eq!(primary.window_kind.as_deref(), Some("five_hour"));
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.expect("weekly quota window");
        assert!((secondary.used_percent - 5.3835).abs() < 0.001);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-26T14:09:00Z"));
        assert_eq!(secondary.window_kind.as_deref(), Some("weekly"));
        assert_eq!(secondary.window_minutes, Some(10_080));
    }

    /// Captured from the live endpoint on 2026-08-08, after an upstream change:
    /// the five-hour window stopped being reported and only the weekly one
    /// remains, while the envelope `code` arrives as the string "200" rather
    /// than a number.
    ///
    /// Both halves must stay non-fatal. A window this API stops reporting is not
    /// an error — the account still has a real weekly figure, and refusing the
    /// whole payload would take a live provider dark over a window that no longer
    /// exists. And the absent window must be absent, never a zero: a consumer
    /// cannot distinguish a fabricated 0% from genuinely unused capacity, so it
    /// would read a silently-dropped window as full headroom.
    const DRIFTED_WEEKLY_ONLY_RESPONSE: &str = r#"{
      "code": "200",
      "data": {
        "DataV2": {
          "ret": ["SUCCESS::接口调用成功"],
          "data": {
            "msg": "Success.",
            "code": "SUCCESS",
            "data": { "per1WeekPercentage": 0.3 },
            "requestId": "test-fixture-request-id",
            "success": true
          }
        },
        "success": true,
        "httpStatus": 200,
        "errorCode": "",
        "api": "zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage",
        "errorMsg": ""
      },
      "httpStatusCode": "200",
      "requestId": "test-fixture-request-id",
      "successResponse": true
    }"#;

    #[test]
    fn weekly_only_response_keeps_the_window_it_still_reports() {
        let usage = normalize_usage(DRIFTED_WEEKLY_ONLY_RESPONSE.as_bytes())
            .expect("a payload reporting one window is a usable answer, not an error");

        let secondary = usage
            .secondary
            .expect("the weekly window is still reported");
        assert_eq!(secondary.used_percent, 30.0);
        assert_eq!(secondary.window_kind.as_deref(), Some("weekly"));
        assert_eq!(secondary.window_minutes, Some(10_080));
        assert_eq!(secondary.resets_at, None);

        assert!(
            usage.primary.is_none(),
            "an unreported window must be absent, never a fabricated zero: a \
             consumer reads 0% as full headroom"
        );
    }

    #[test]
    fn a_payload_reporting_no_window_at_all_is_an_error() {
        // The boundary beside the test above. One window missing is a narrower
        // answer; every window missing means nothing usable was delivered, and
        // publishing that as an empty success would read as an account with no
        // limits rather than as a failure to learn anything.
        let mut body: serde_json::Value =
            serde_json::from_str(DRIFTED_WEEKLY_ONLY_RESPONSE).unwrap();
        body["data"]["DataV2"]["data"]["data"] = serde_json::json!({});
        assert!(matches!(
            normalize_usage(body.to_string().as_bytes()),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn epoch_milliseconds_convert_to_utc() {
        assert_eq!(
            epoch_ms_to_iso8601(1_784_506_140_000).as_deref(),
            Some("2026-07-20T00:09:00Z")
        );
    }

    #[test]
    fn clamps_used_fraction_above_one() {
        let mut body: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        body["data"]["DataV2"]["data"]["data"]["per5HourPercentage"] = serde_json::json!(1.5);
        let usage = normalize_usage(body.to_string().as_bytes()).unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, 100.0);
    }

    #[test]
    fn non_success_gateway_response_is_decode_error() {
        let mut failed_gateway: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        failed_gateway["successResponse"] = serde_json::json!(false);
        assert!(matches!(
            normalize_usage(failed_gateway.to_string().as_bytes()),
            Err(FetchError::Decode(_))
        ));

        let mut failed_result: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        failed_result["data"]["DataV2"]["data"]["code"] = serde_json::json!("FAILED");
        assert!(matches!(
            normalize_usage(failed_result.to_string().as_bytes()),
            Err(FetchError::Decode(_))
        ));
    }

    /// A page from a LIVE session that lost the token must not degrade the lane.
    ///
    /// SHAPE FROM A LIVE FETCH, 2026-08-18: HTTP 200 on the real URL with no
    /// redirect, the console block present, the ticket cookie valid, and no
    /// `SEC_TOKEN` anywhere in 11.4 KB. Three days later the same URL and the
    /// same session returned 21.3 KB carrying the token, and the provider served
    /// again with no change from us -- so the shell is transient, and a class
    /// that drops the cached window over it manufactures an outage.
    ///
    /// Asserted as NOT-Unauthorized as well as transient, because the expensive
    /// direction is still the logout reading: it would count against the stale
    /// browser logins and send an operator to re-authenticate a working session.
    #[test]
    fn a_console_page_without_the_token_is_transient_not_a_logout() {
        let page =
            r#"<html><script>window.ONE_CONSOLE_TOOL={APP_ID:"x",LANG:"en"};</script></html>"#;
        let error = missing_token_error(page);
        assert!(
            matches!(error, FetchError::Upstream(_)),
            "a shell the signed-in console rendered comes back; it must stale-serve"
        );
        assert!(
            !matches!(error, FetchError::Unauthorized(_)),
            "a working session must never be reported as a stale login"
        );
    }

    /// A page the signed-in shell did not render is a session problem.
    #[test]
    fn a_page_without_the_console_block_is_unauthorized() {
        let page = r#"<html><body>Qwen Cloud login</body></html>"#;
        assert!(
            matches!(missing_token_error(page), FetchError::Unauthorized(_)),
            "no console block means we were not served as a signed-in user"
        );
    }

    /// The discriminator is the console block, never the word "login".
    ///
    /// A signed-in console page carries "login" in its own navigation, so a
    /// keyword match would classify every healthy page as logged out -- and that
    /// direction is the expensive one: it would tell an operator to
    /// re-authenticate a working session on every genuine drift.
    #[test]
    fn the_word_login_in_a_console_page_does_not_make_it_a_logout() {
        let page = r#"<html><nav><a href="/login">Sign in</a></nav>
            <script>window.ONE_CONSOLE_TOOL={APP_ID:"x"};</script></html>"#;
        assert!(
            matches!(missing_token_error(page), FetchError::Upstream(_)),
            "the navigation word must not outrank the console block"
        );
    }

    #[test]
    fn login_html_is_decode_error() {
        assert!(matches!(
            normalize_usage(b"<html><body>Qwen Cloud login</body></html>"),
            Err(FetchError::Decode(_))
        ));
    }

    #[test]
    fn drops_window_when_its_percentage_is_missing() {
        let mut body: serde_json::Value = serde_json::from_str(HAR_RESPONSE).unwrap();
        body["data"]["DataV2"]["data"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("per1WeekPercentage");
        let usage = normalize_usage(body.to_string().as_bytes()).unwrap();
        assert!(usage.primary.is_some());
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn extracts_sec_token_from_console_configuration() {
        assert_eq!(
            extract_sec_token(r#"window.ONE_CONSOLE_TOOL={SEC_TOKEN: "U19ojXS7pvhECD3W5IaVHA",};"#),
            Some("U19ojXS7pvhECD3W5IaVHA")
        );
        assert_eq!(extract_sec_token("window.ONE_CONSOLE_TOOL={};"), None);
    }

    /// Wrap a body in the console gateway's envelope.
    ///
    /// Both extra responses arrive nested this way, so a fixture omitting a
    /// layer would exercise an early return rather than the mapping under test.
    fn gateway(inner: &str) -> Vec<u8> {
        format!(r#"{{"successResponse":true,"data":{{"DataV2":{{"data":{inner}}}}}}}"#).into_bytes()
    }

    /// Counts are derived only from a record that belongs to the token plan.
    ///
    /// WHY THIS CAN HAPPEN AT ALL. The console filters its subscription call by
    /// `commodityCode` and we deliberately do not, because filtering would
    /// return nothing at all for an account on a different token-plan product.
    /// The cost of staying unfiltered is this case: on a multi-subscription
    /// account the gateway may answer with another product's record, and
    /// `specCode` is a bare tier name -- the captured value is `"pro"` -- so it
    /// can hit a real row of the token plan's cap table and publish that cap as
    /// this window's `totalCount`.
    ///
    /// A wrong absolute count is worse than no count. A percentage that
    /// disagrees with its own counts is visibly broken; counts that agree with
    /// nothing are believed.
    #[test]
    fn another_products_subscription_does_not_enrich_the_counts() {
        let mut usage = usage_at(50.0, 25.0);
        enrich_with_counts(
            &mut usage,
            &caps_body("pro", "1000", "40000"),
            &subscription_body_for("sfm_someotherproduct_public_intl", "pro"),
        );
        assert_eq!(
            usage.primary.as_ref().unwrap().total_count,
            None,
            "a cap table this record does not describe must not produce counts"
        );
        // The window itself survives: the percentage was never in question, and
        // dropping it would turn a missing enrichment into a missing provider.
        assert_eq!(usage.primary.as_ref().unwrap().used_percent, 50.0);
    }

    /// The token plan's own record still enriches, suffix and all.
    ///
    /// The control for the test above: without it, a guard that rejected
    /// everything would pass, and the enrichment would be silently dead.
    #[test]
    fn the_token_plans_own_subscription_still_enriches() {
        let mut usage = usage_at(50.0, 25.0);
        enrich_with_counts(
            &mut usage,
            &caps_body("pro", "1000", "40000"),
            &subscription_body_for("sfm_tokenplansolo_public_intl", "pro"),
        );
        assert_eq!(
            usage.primary.as_ref().unwrap().total_count,
            Some(1000.0),
            "the real record must still produce counts"
        );
    }

    /// A record that does not say which product it is gets enriched as before.
    ///
    /// Absent is UNVERIFIABLE, not wrong. One payload has ever been observed, so
    /// refusing on a missing field would trade a hypothetical wrong count for a
    /// certain lost one -- the over-rejecting direction, on the evidence we have.
    #[test]
    fn a_record_without_an_instance_code_is_enriched_as_before() {
        let mut usage = usage_at(50.0, 25.0);
        enrich_with_counts(
            &mut usage,
            &caps_body("pro", "1000", "40000"),
            &subscription_body("pro"),
        );
        assert_eq!(
            usage.primary.as_ref().unwrap().total_count,
            Some(1000.0),
            "a record that cannot be checked must not be refused"
        );
    }

    fn caps_body(spec: &str, five_hour: &str, weekly: &str) -> Vec<u8> {
        gateway(&format!(
            r#"{{"success":true,"data":{{"{spec}":{{"five_hour":{five_hour},"weekly":{weekly}}}}}}}"#
        ))
    }

    fn subscription_body(spec: &str) -> Vec<u8> {
        gateway(&format!(
            r#"{{"success":true,"data":{{"specCode":"{spec}"}}}}"#
        ))
    }

    /// A subscription record that names the product it belongs to, in the shape
    /// the console actually returns.
    ///
    /// LIVE-OBSERVED SHAPE (capture of the working browser, 2026-08-20): the
    /// real record carried `instanceCode` `sfm_tokenplansolo_public_intl-sg-…`
    /// beside `specCode: "pro"`. The suffix is an instance id and is not matched
    /// on -- only the product prefix is.
    fn subscription_body_for(commodity: &str, spec: &str) -> Vec<u8> {
        gateway(&format!(
            r#"{{"success":true,"data":{{"specCode":"{spec}","instanceCode":"{commodity}-sg-ycx4vlnxo0a"}}}}"#
        ))
    }

    fn usage_at(five_hour: f64, weekly: f64) -> Usage {
        Usage {
            primary: Some(RateWindow {
                window_kind: None,
                used_percent: five_hour,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: Some(300),
                used_count: None,
                total_count: None,
                regeneration: None,
                breakdown: None,
            }),
            secondary: Some(RateWindow {
                window_kind: None,
                used_percent: weekly,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: Some(10080),
                used_count: None,
                total_count: None,
                regeneration: None,
                breakdown: None,
            }),
            ..Usage::default()
        }
    }

    /// The caps applied are those of the account's own plan.
    ///
    /// The quota-config response lists every plan the service sells, so the
    /// subscription response is what says which row belongs to this account.
    /// Reading the wrong row yields counts that look ordinary and describe a
    /// plan the account is not on.
    #[test]
    fn counts_come_from_the_caps_of_the_subscribed_plan() {
        let mut usage = usage_at(27.8, 10.0);
        let caps = gateway(
            r#"{"success":true,"data":{"standard":{"five_hour":1000,"weekly":10000},"pro":{"five_hour":4000,"weekly":40000}}}"#,
        );

        enrich_with_counts(&mut usage, &caps, &subscription_body("pro"));

        let primary = usage.primary.expect("the window survives enrichment");
        assert_eq!(primary.total_count, Some(4000.0), "cap of the wrong plan");
        let secondary = usage.secondary.expect("the window survives enrichment");
        assert_eq!(secondary.total_count, Some(40000.0));
    }

    /// The consumed count is never reconstructed from the percentage.
    ///
    /// `usedCount` is a count of things and is integral by contract, while
    /// `percentage * cap` is fractional for almost every input — this fixture's
    /// 27.8% over 4000 is one of the rare percentages that divides cleanly,
    /// which is why the arithmetic looked sound for as long as it did. A
    /// consumer that validates integrality rejects the whole response over one
    /// such value, so the check uses a percentage of the ordinary kind.
    #[test]
    fn a_consumed_count_is_never_derived_from_the_percentage() {
        let mut usage = usage_at(45.052_361_473_854_994, 10.0);
        let caps = gateway(r#"{"success":true,"data":{"pro":{"five_hour":4000,"weekly":40000}}}"#);

        enrich_with_counts(&mut usage, &caps, &subscription_body("pro"));

        let primary = usage.primary.expect("the window survives enrichment");
        assert_eq!(
            primary.used_count, None,
            "a count derived from a percentage is an estimate, not a measurement"
        );
        assert_eq!(
            primary.total_count,
            Some(4000.0),
            "the cap is reported by the provider and stays"
        );
    }

    /// Enrichment is additive: when no cap can be resolved the percentage stands
    /// alone, rather than the window being dropped or annotated with a guess.
    ///
    /// Each case is a distinct way the two extra calls can fail to yield a cap,
    /// and every one must leave the window as the usage response described it.
    #[test]
    fn a_cap_that_cannot_be_resolved_leaves_the_window_untouched() {
        let cases: [(&str, Vec<u8>, Vec<u8>); 6] = [
            (
                "unparseable caps",
                b"not json".to_vec(),
                subscription_body("pro"),
            ),
            (
                "unparseable subscription",
                caps_body("pro", "4000", "40000"),
                b"not json".to_vec(),
            ),
            (
                "gateway reported failure",
                br#"{"successResponse":false}"#.to_vec(),
                subscription_body("pro"),
            ),
            (
                "inner result reported failure",
                gateway(r#"{"success":false,"data":{"pro":{"five_hour":4000,"weekly":40000}}}"#),
                subscription_body("pro"),
            ),
            (
                "the account's plan is absent from the cap table",
                caps_body("standard", "1000", "10000"),
                subscription_body("pro"),
            ),
            (
                "the plan states no cap for these windows",
                caps_body("pro", "null", "null"),
                subscription_body("pro"),
            ),
        ];

        for (name, caps, subscription) in cases {
            let mut usage = usage_at(27.8, 10.0);
            enrich_with_counts(&mut usage, &caps, &subscription);

            let primary = usage.primary.expect("the window must survive");
            assert_eq!(primary.used_count, None, "{name}: invented a used count");
            assert_eq!(primary.total_count, None, "{name}: invented a total");
            // Not vacuous: the percentage is untouched, so this cannot pass by
            // discarding the window.
            assert_eq!(primary.used_percent, 27.8, "{name}");
        }
    }

    #[test]
    fn handles_without_credential_source_are_empty() {
        let provider = QwenCloudProvider::new_with_handle_loader(
            None,
            std::sync::Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles, Vec::<CredentialHandle>::new());
    }
}
