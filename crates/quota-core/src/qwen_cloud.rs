//! Qwen Cloud token-plan usage — browser-cookie + console-gateway scrape.
//!
//! The token-plan quota is available only to an authenticated Qwen Cloud browser
//! session. Each scrape loads the token-plan page to obtain a fresh `SEC_TOKEN`,
//! then posts that token with the Chrome cookie jar through the ONE_CONSOLE
//! `IntlBroadScopeAspnGateway` gateway.
//!
//! VERIFICATION: fixture-verified from a live browser HAR capture of
//! `home.qwencloud.com`, not a CodexBar port (Qwen Cloud has no CodexBar
//! equivalent). The HAR verifies the
//! `zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage` endpoint via the
//! `IntlBroadScopeAspnGateway` console gateway, the Chrome cookie + per-page
//! `SEC_TOKEN` authentication, and the `per5HourPercentage`,
//! `per5HourResetTime`, `per1WeekPercentage`, and `per1WeekResetTime` response
//! fields. `per5HourPercentage` and `per1WeekPercentage` are interpreted as USED
//! fractions (0–1) from their field semantics and observed values. The shared
//! cookie transport in `browser_cookies.rs` is live-proven; the gateway call shape
//! is HAR-verified.
//!
//! Two hosts: the token-plan PAGE (`home.qwencloud.com`) is loaded only to extract
//! the per-session `SEC_TOKEN`; the quota call itself goes to the console data
//! gateway at `cs-data.qwencloud.com` (same-site, so the `qwencloud.com` cookies
//! apply), with `Origin: https://home.qwencloud.com`. Posting the quota action to
//! `home.qwencloud.com` instead returns an empty success — the gateway host matters.
//!
//! KNOWN LIMITATION (entitled view; no enforced signal in the console): the
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

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies::{self, SOURCE_LABEL},
    env,
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "qwen-cloud";
const DOMAIN: &str = "qwencloud.com";
/// The script block the authenticated console shell emits. Its presence is what
/// separates "we were served the real page" from "we were not signed in".
const CONSOLE_BLOCK: &str = "ONE_CONSOLE_TOOL";

const TOKEN_PLAN_URL: &str =
    "https://home.qwencloud.com/billing/subscription/token-plan-individual";
const USAGE_URL: &str = "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage";
const QUOTA_CONFIG_URL: &str = "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/quota-config";
const SUBSCRIPTION_URL: &str = "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/subscription";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const GATEWAY_PARAMS: &str = r#"{"Api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/usage","Data":{"cornerstoneParam":{"domain":"home.qwencloud.com","consoleSite":"QWENCLOUD","console":"ONE_CONSOLE","xsp_lang":"en-US","protocol":"V2","productCode":"p_efm"}},"V":"1.0"}"#;
const QUOTA_CONFIG_PARAMS: &str = r#"{"Api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/quota-config","Data":{"cornerstoneParam":{"domain":"home.qwencloud.com","consoleSite":"QWENCLOUD","console":"ONE_CONSOLE","xsp_lang":"en-US","protocol":"V2","productCode":"p_efm"}},"V":"1.0"}"#;
const SUBSCRIPTION_PARAMS: &str = r#"{"Api":"zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/subscription","Data":{"cornerstoneParam":{"domain":"home.qwencloud.com","consoleSite":"QWENCLOUD","console":"ONE_CONSOLE","xsp_lang":"en-US","protocol":"V2","productCode":"p_efm"}},"V":"1.0"}"#;

const FIVE_HOUR_WINDOW_MINUTES: i64 = 5 * 60;
const WEEKLY_WINDOW_MINUTES: i64 = 7 * 24 * 60;

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
    if let Some(ref mut primary) = usage.primary {
        if let Some(cap) = tier.five_hour {
            primary.total_count = Some(cap);
        }
    }
    if let Some(ref mut secondary) = usage.secondary {
        if let Some(cap) = tier.weekly {
            secondary.total_count = Some(cap);
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
) -> Option<RateWindow> {
    let used_percent = used_fraction.filter(|value| value.is_finite())? * 100.0;
    Some(RateWindow {
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: reset_epoch_ms.and_then(epoch_ms_to_iso8601),
        window_minutes: Some(window_minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
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
    let response: GatewayResponse = serde_json::from_slice(body).map_err(|error| {
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
    );
    let secondary = window_from_fraction(
        quota.per_week_percentage,
        quota.per_week_reset_time,
        WEEKLY_WINDOW_MINUTES,
    );
    if primary.is_none() && secondary.is_none() {
        // The same discriminator one level down. A plan block that NAMES its
        // percentage keys and leaves them empty is the upstream stating there are
        // no windows; a block that mentions neither key is a payload we no longer
        // understand, and calling that "no quota" would retire a working provider
        // silently on the day they rename a field.
        let stated_the_keys = ["per5HourPercentage", "perWeekPercentage"]
            .iter()
            .any(|key| {
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

    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The Qwen Cloud token-plan usage provider.
pub struct QwenCloudProvider {
    http: reqwest::Client,
}

impl QwenCloudProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
        }
    }
}

impl Default for QwenCloudProvider {
    fn default() -> Self {
        Self::new()
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

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let jar = browser_cookies::chrome_cookies_for_async(DOMAIN)
                .await
                .map_err(FetchError::from)?;
            if !jar.has_cookie_named(|name| name == "login_qwencloud_ticket") {
                return Err(FetchError::NoSession(
                    "no Qwen Cloud login ticket in browser".to_string(),
                ));
            }

            let cookie_header = jar.header();
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
                SOURCE_LABEL,
                enriched,
            ))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {

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
        let body = br#"{"successResponse":true,"data":{"DataV2":{"data":{"success":true,"code":"SUCCESS","data":{"per5HourPercentage":null,"perWeekPercentage":null}}}}}"#;
        match normalize_usage(body) {
            Err(FetchError::NoQuotaReported(message)) => {
                assert!(message.contains("no windows"), "got {message}");
            }
            other => panic!("expected NoQuotaReported, got {other:?}"),
        }
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
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = usage.secondary.expect("weekly quota window");
        assert!((secondary.used_percent - 5.3835).abs() < 0.001);
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-07-26T14:09:00Z"));
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
                used_percent: five_hour,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: Some(300),
                used_count: None,
                total_count: None,
                regeneration: None,
            }),
            secondary: Some(RateWindow {
                used_percent: weekly,
                raw_used_percent: None,
                resets_at: None,
                window_minutes: Some(10080),
                used_count: None,
                total_count: None,
                regeneration: None,
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
}
