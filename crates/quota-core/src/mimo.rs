//! MiMo usage — regional Token Plan API keys or a web session cookie, from the vault.
//!
//! VERIFICATION:
//! This port is fixture-verified against CodexBar source.
//! Ported from CodexBar:
//! - `Sources/CodexBarCore/Providers/MiMo/MiMoUsageFetcher.swift` (lines 88-92, 171-180, 188-193, 239-248, 280-289, 294-306)
//! - `Sources/CodexBarCore/Providers/MiMo/MiMoCookieImporter.swift` (lines 7-14, 106-109)
//! - `Sources/CodexBarCore/Providers/MiMo/MiMoUsageSnapshot.swift`
//!
//! `token_plan_tests` uses CodexBar v0.85.0 fixtures to verify regional routing,
//! credit counts, UTC period boundaries, and how refusals are classified.
//! Neither lane was verified with a live MiMo account.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;

use crate::{
    credential_source::{
        get_vault_credential, report_vault_auth_failure, take_utf8_payload, CredentialSource,
        VaultGetError, VAULT_READ_MIN_TTL_MS,
    },
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{
        AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
    },
    vault_handles::{handle_id_names_family, VaultHandleLoader},
};

pub const PROVIDER_NAME: &str = "mimo";
/// Base key for storing MiMo credentials in the provider vault. An account-specific
/// suffix identifies each account, and these credentials are read only from the provider vault.
const COOKIE_FAMILY: &str = "cookie:xiaomimimo.com";

const DETAIL_URL: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/detail";
const USAGE_URL: &str = "https://platform.xiaomimimo.com/api/v1/tokenPlan/usage";
const CHINA_TOKEN_PLAN_URL: &str = "https://token-plan-cn.xiaomimimo.com/v1/platform/token_plan";
const SINGAPORE_TOKEN_PLAN_URL: &str =
    "https://token-plan-sgp.xiaomimimo.com/v1/platform/token_plan";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const TOKEN_PLAN_CONFLICT: &str = "mimo-token-plan-conflict";
const MAX_SAFE_CREDIT_COUNT: u64 = (1 << 53) - 1;

#[derive(Clone, Copy)]
enum Region {
    China,
    Singapore,
}

impl Region {
    fn family(self) -> &'static str {
        match self {
            Self::China => "apikey:xiaomi-token-plan-cn",
            Self::Singapore => "apikey:xiaomi-token-plan-sgp",
        }
    }

    fn for_handle(handle: &CredentialHandle) -> Option<Self> {
        let id = handle.vault_credential_id()?;
        [Self::China, Self::Singapore]
            .into_iter()
            .find(|region| handle_id_names_family(id, region.family()))
    }

    fn url(self) -> &'static str {
        match self {
            Self::China => CHINA_TOKEN_PLAN_URL,
            Self::Singapore => SINGAPORE_TOKEN_PLAN_URL,
        }
    }
}

pub(crate) fn api_provider_for_handle(handle: &CredentialHandle) -> Option<&'static str> {
    Region::for_handle(handle)?.family().strip_prefix("apikey:")
}

#[derive(Deserialize)]
struct TokenPlan {
    object: String,
    quota: TokenPlanQuota,
    period: TokenPlanPeriod,
}

#[derive(Deserialize)]
struct TokenPlanQuota {
    total_credits: u64,
    used_credits: u64,
    remaining_credits: u64,
}

#[derive(Deserialize)]
struct TokenPlanPeriod {
    start_at: Option<String>,
    end_at: Option<String>,
}

fn token_plan_date(text: &str) -> Result<NaiveDateTime, FetchError> {
    const FORMAT: &str = "%Y-%m-%d %H:%M:%S";
    // The zoneless fields are UTC in both regions. Round-trip to reject
    // lenient forms, including leap seconds the upstream parser rejects.
    NaiveDateTime::parse_from_str(text, FORMAT)
        .ok()
        .filter(|date| {
            text.len() == 19
                && date.and_utc().timestamp_subsec_nanos() == 0
                && date.format(FORMAT).to_string() == text
        })
        .ok_or_else(|| FetchError::Decode("mimo: invalid Token Plan period boundary".to_string()))
}

fn normalize_token_plan(body: &[u8]) -> Result<Usage, FetchError> {
    let plan: TokenPlan = crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body)
        .map_err(|_| FetchError::Decode("mimo: invalid Token Plan response".to_string()))?;
    if plan.object != "token_plan" {
        return Err(FetchError::Decode(
            "mimo: unexpected Token Plan object".to_string(),
        ));
    }
    let TokenPlanQuota {
        total_credits: total,
        used_credits: used,
        remaining_credits: remaining,
    } = plan.quota;
    // Both upstream and the wire represent counts exactly in this range.
    if total > MAX_SAFE_CREDIT_COUNT
        || used > MAX_SAFE_CREDIT_COUNT
        || remaining != total.saturating_sub(used)
    {
        return Err(FetchError::Decode(
            "mimo: invalid Token Plan credit counts".to_string(),
        ));
    }
    let start = plan
        .period
        .start_at
        .as_deref()
        .map(token_plan_date)
        .transpose()?;
    let end = plan
        .period
        .end_at
        .as_deref()
        .map(token_plan_date)
        .transpose()?;
    let window_minutes = if let Some((start, end)) = start.zip(end) {
        let seconds = (end - start).num_seconds();
        if seconds <= 0 {
            return Err(FetchError::Decode(
                "mimo: Token Plan period end must follow its start".to_string(),
            ));
        }
        (seconds % 60 == 0).then_some(seconds / 60)
    } else {
        None
    };
    if total == 0 {
        return Err(FetchError::NoQuotaReported(
            "mimo: Token Plan states no credit allowance".to_string(),
        ));
    }

    Ok(Usage {
        primary: Some(RateWindow {
            window_kind: None,
            used_percent: (used.min(total) as f64 / total as f64) * 100.0,
            raw_used_percent: None,
            resets_at: end.map(|date| date.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            window_minutes,
            used_count: Some(used as f64),
            total_count: Some(total as f64),
            regeneration: None,
            breakdown: None,
        }),
        ..Usage::default()
    })
}

fn token_plan_status(status: u16) -> Result<(), FetchError> {
    match status {
        200 => Ok(()),
        401 => Err(FetchError::ProviderStatus(
            401,
            "mimo: Token Plan key rejected".to_string(),
        )),
        // Permission to read quota can be refused without invalidating the key.
        403 => Err(FetchError::Upstream(
            "mimo: Token Plan quota query forbidden (HTTP 403)".to_string(),
        )),
        _ => Err(FetchError::ProviderStatus(
            status,
            "mimo: Token Plan quota query failed".to_string(),
        )),
    }
}

#[derive(Deserialize)]
struct MimoEnvelope<T> {
    code: i32,
    data: Option<T>,
}

#[derive(Deserialize)]
struct MimoDetail {
    #[serde(rename = "currentPeriodEnd")]
    current_period_end: Option<String>,
}

#[derive(Deserialize)]
struct MimoUsage {
    #[serde(rename = "monthUsage")]
    month_usage: Option<MimoMonthUsage>,
}

#[derive(Deserialize)]
struct MimoMonthUsage {
    items: Option<Vec<MimoUsageItem>>,
}

#[derive(Deserialize)]
struct MimoUsageItem {
    percent: Option<f64>,
    used: Option<f64>,
    limit: Option<f64>,
}

fn parse_reset_time(s: &str) -> Option<String> {
    let naive = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let dt = Utc.from_utc_datetime(&naive);
    Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Normalize the detail and usage JSON responses to [`Usage`]. Pure — unit-testable.
pub fn normalize(detail_json: &str, usage_json: &str) -> Result<Usage, FetchError> {
    let detail_envelope: MimoEnvelope<MimoDetail> =
        crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, detail_json.as_bytes())
            .map_err(|e| FetchError::Decode(format!("failed to parse detail JSON: {e}")))?;

    if detail_envelope.code == 401 || detail_envelope.code == 403 {
        return Err(FetchError::Unauthorized(format!(
            "mimo API returned code {}",
            detail_envelope.code
        )));
    }
    if detail_envelope.code != 0 {
        return Err(FetchError::Decode(format!(
            "mimo API returned non-zero code {}",
            detail_envelope.code
        )));
    }

    let detail_data = detail_envelope
        .data
        .ok_or_else(|| FetchError::Decode("mimo detail response missing data field".to_string()))?;

    let usage_envelope: MimoEnvelope<MimoUsage> =
        crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, usage_json.as_bytes())
            .map_err(|e| FetchError::Decode(format!("failed to parse usage JSON: {e}")))?;

    if usage_envelope.code == 401 || usage_envelope.code == 403 {
        return Err(FetchError::Unauthorized(format!(
            "mimo API returned code {}",
            usage_envelope.code
        )));
    }
    if usage_envelope.code != 0 {
        return Err(FetchError::Decode(format!(
            "mimo API returned non-zero code {}",
            usage_envelope.code
        )));
    }

    let usage_data = usage_envelope
        .data
        .ok_or_else(|| FetchError::Decode("mimo usage response missing data field".to_string()))?;

    let mut used_percent = None;
    if let Some(month_usage) = usage_data.month_usage {
        if let Some(items) = month_usage.items {
            if let Some(item) = items.first() {
                if let Some(p) = item.percent {
                    used_percent = Some((p * 100.0).clamp(0.0, 100.0));
                } else if let (Some(used), Some(limit)) = (item.used, item.limit) {
                    if limit > 0.0 {
                        used_percent = Some(((used / limit) * 100.0).clamp(0.0, 100.0));
                    }
                }
            }
        }
    }

    let resets_at = detail_data
        .current_period_end
        .and_then(|s| parse_reset_time(&s));

    // The percent is load-bearing and the reset is optional: a window is emitted
    // from the percent alone, carrying the reset when the upstream reports one.
    // Requiring both would discard a fully-exhausted allowance whenever the
    // period end is missing, and a consumer reads an absent window as unused
    // capacity rather than as a wall.
    let primary = used_percent.map(|pct| RateWindow {
        window_kind: Some(cortexkit_provider_usage::window_kind::MONTHLY.to_string()),
        used_percent: pct,
        raw_used_percent: None,
        resets_at,
        window_minutes: Some(43200),
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    });

    if primary.is_none() {
        return Err(FetchError::Decode(
            "mimo: no valid usage window (missing percent)".to_string(),
        ));
    }

    Ok(Usage {
        primary,
        secondary: None,
        tertiary: None,
        extra_rate_windows: None,
    })
}

/// The MiMo usage provider.
pub struct MimoProvider {
    vault: crate::cookie_vault::CookieVault,
    http: reqwest::Client,
    token_plan_http: Option<reqwest::Client>,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    #[cfg(test)]
    token_plan_test_urls: Vec<(&'static str, String)>,
}

impl MimoProvider {
    // Builds its own client to turn redirects off, so a session cookie never
    // follows one. It does not set the shared pool idle bound from
    // `http::provider_client()`; see clippy.toml for why that bound matters.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| crate::http::provider_client());
        Self {
            http,
            vault: crate::cookie_vault::CookieVault::new(
                credential_source.clone(),
                Arc::clone(&handle_loader),
                COOKIE_FAMILY,
            ),
            token_plan_http: crate::http::provider_client_without_redirects().ok(),
            credential_source,
            handle_loader,
            #[cfg(test)]
            token_plan_test_urls: Vec::new(),
        }
    }

    async fn read_token_plan(&self, region: Region, key: &str) -> Result<Usage, FetchError> {
        let http = self.token_plan_http.as_ref().ok_or_else(|| {
            FetchError::Internal(
                "mimo: could not create a redirect-disabled Token Plan client".to_string(),
            )
        })?;
        let url = region.url();
        #[cfg(test)]
        let url = self
            .token_plan_test_urls
            .iter()
            .find(|(endpoint, _)| *endpoint == url)
            .map(|(_, target)| target.as_str())
            .ok_or_else(|| FetchError::Internal(format!("missing test route for {url}")))?;
        let response = JsonRequest::get(url)
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Accept", "application/json"))
            .header(Header::bearer(key))
            .send_raw(http)
            .await?;
        token_plan_status(response.status)?;
        normalize_token_plan(response.body_for_parsing()?)
    }

    async fn fetch_token_plan(&self, handle: &CredentialHandle, region: Region) -> FetchAttempt {
        let Some(source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(VaultGetError::Permanent);
        };
        let mut served = match get_vault_credential(source, handle, VAULT_READ_MIN_TTL_MS).await {
            Ok(served) => served,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let result = match take_utf8_payload(&mut served.payload) {
            Ok(key) if !key.trim().is_empty() => self.read_token_plan(region, key.trim()).await,
            Ok(_) => Err(FetchError::CredentialUnusable(
                "mimo: empty Token Plan key deposit".to_string(),
            )),
            Err(error) => Err(error),
        };
        match result {
            Ok(usage) => FetchAttempt::success(
                Some(AccountObservation::new(None, Some(served.record_version))),
                "vault",
                usage,
            ),
            Err(error) => {
                report_vault_auth_failure(Some(source), handle, served.record_version, &error);
                FetchAttempt::failure(None, Some("vault".to_string()), error)
            }
        }
    }
}

#[async_trait]
impl UsageProvider for MimoProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_none() {
            return Ok(Vec::new());
        }
        let mut keys = self
            .handle_loader
            .mimo_handles()?
            .into_iter()
            .filter(|handle| Region::for_handle(handle).is_some());
        let Some(key) = keys.next() else {
            return self.vault.handles();
        };
        // Neither endpoint identifies its owner; all deposits compete for one
        // row. More than one key is a configuration error, not a tie-break.
        if keys.next().is_some() {
            return Ok(vec![CredentialHandle::Named(
                TOKEN_PLAN_CONFLICT.to_string(),
            )]);
        }
        Ok(vec![key])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if matches!(handle, CredentialHandle::Named(name) if name == TOKEN_PLAN_CONFLICT) {
            return FetchAttempt::failure(None, Some("vault".to_string()), FetchError::CredentialUnusable(
                "mimo: conflicting Token Plan keys (xiaomi-token-plan-cn or xiaomi-token-plan-sgp); keep only one deposit".to_string(),
            ));
        }
        if let Some(region) = Region::for_handle(handle) {
            return self.fetch_token_plan(handle, region).await;
        }
        let result: Result<ProviderUsage, FetchError> = async {
            let (jar, source) = self.vault.jar_for(handle).await?;

            let has_token = jar.has_cookie_named(|n| n == "api-platform_serviceToken");
            let has_user_id = jar.has_cookie_named(|n| n == "userId");
            if !has_token || !has_user_id {
                return Err(FetchError::NoSession(
                    "missing required mimo session cookies (api-platform_serviceToken and userId)"
                        .to_string(),
                ));
            }

            // The usage response omits account metadata needed by normalization,
            // so validate and retain the detail response before fetching usage.
            let detail_req = JsonRequest::get(DETAIL_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Accept", "application/json, text/plain, */*"))
                .header(Header::new("Cookie", jar.header()))
                .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
                .header(Header::new("x-timeZone", "UTC+01:00"))
                .header(Header::new("Origin", "https://platform.xiaomimimo.com"))
                .header(Header::new(
                    "Referer",
                    "https://platform.xiaomimimo.com/#/console/balance",
                ))
                .header(Header::new("User-Agent", BROWSER_USER_AGENT));

            let detail_res = detail_req.send_raw(&self.http).await?;
            if (300..400).contains(&detail_res.status) || detail_res.status == 401 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    detail_res.status
                )));
            }
            if detail_res.status == 403 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    detail_res.status
                )));
            }
            if !(200..300).contains(&detail_res.status) {
                let excerpt: String = String::from_utf8_lossy(&detail_res.body)
                    .chars()
                    .take(200)
                    .collect();
                return Err(FetchError::Upstream(format!(
                    "HTTP {}: {excerpt}",
                    detail_res.status
                )));
            }

            let usage_req = JsonRequest::get(USAGE_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Accept", "application/json, text/plain, */*"))
                .header(Header::new("Cookie", jar.header()))
                .header(Header::new("Accept-Language", "en-US,en;q=0.9"))
                .header(Header::new("x-timeZone", "UTC+01:00"))
                .header(Header::new("Origin", "https://platform.xiaomimimo.com"))
                .header(Header::new(
                    "Referer",
                    "https://platform.xiaomimimo.com/#/console/balance",
                ))
                .header(Header::new("User-Agent", BROWSER_USER_AGENT));

            let usage_res = usage_req.send_raw(&self.http).await?;
            if (300..400).contains(&usage_res.status) || usage_res.status == 401 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    usage_res.status
                )));
            }
            if usage_res.status == 403 {
                return Err(FetchError::Unauthorized(format!(
                    "HTTP {}",
                    usage_res.status
                )));
            }
            if !(200..300).contains(&usage_res.status) {
                let excerpt: String = String::from_utf8_lossy(&usage_res.body)
                    .chars()
                    .take(200)
                    .collect();
                return Err(FetchError::Upstream(format!(
                    "HTTP {}: {excerpt}",
                    usage_res.status
                )));
            }

            let detail_json = String::from_utf8_lossy(detail_res.body_for_parsing()?);
            let usage_json = String::from_utf8_lossy(usage_res.body_for_parsing()?);

            let usage = normalize(&detail_json, &usage_json)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, source, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DETAIL_HEALTHY: &str = r#"{
        "code": 0,
        "data": {
            "currentPeriodEnd": "2026-07-24 12:00:00"
        }
    }"#;

    const USAGE_HEALTHY: &str = r#"{
        "code": 0,
        "data": {
            "monthUsage": {
                "items": [
                    {
                        "percent": 0.45,
                        "used": 45.0,
                        "limit": 100.0
                    }
                ]
            }
        }
    }"#;

    const ENVELOPE_401: &str = r#"{
        "code": 401,
        "data": null
    }"#;

    const DETAIL_MISSING_RESET: &str = r#"{
        "code": 0,
        "data": {
            "currentPeriodEnd": null
        }
    }"#;

    const USAGE_COUNTS_ONLY: &str = r#"{
        "code": 0,
        "data": {
            "monthUsage": {
                "items": [
                    {
                        "percent": null,
                        "used": 30.0,
                        "limit": 120.0
                    }
                ]
            }
        }
    }"#;

    #[test]
    fn test_healthy_parse() {
        let usage = normalize(DETAIL_HEALTHY, USAGE_HEALTHY).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 45.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T12:00:00Z"));
        assert_eq!(primary.window_kind.as_deref(), Some("monthly"));
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn test_unauthorized_envelope() {
        let res = normalize(ENVELOPE_401, USAGE_HEALTHY);
        assert!(matches!(res, Err(FetchError::Unauthorized(_))));

        let res2 = normalize(DETAIL_HEALTHY, ENVELOPE_401);
        assert!(matches!(res2, Err(FetchError::Unauthorized(_))));
    }

    #[test]
    fn missing_reset_keeps_the_window_with_its_percent() {
        // A missing period end must not discard a window built from real usage:
        // an absent window reads downstream as unused capacity, which is the
        // dangerous direction when the allowance is actually spent.
        let primary = normalize(DETAIL_MISSING_RESET, USAGE_HEALTHY)
            .expect("usage data should emit a window")
            .primary
            .expect("a real percent is a real window even with no reset reported");
        assert_eq!(primary.resets_at, None);
        assert_eq!(primary.window_kind.as_deref(), Some("monthly"));
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn test_counts_only_parse() {
        let usage = normalize(DETAIL_HEALTHY, USAGE_COUNTS_ONLY).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T12:00:00Z"));
        assert_eq!(primary.window_kind.as_deref(), Some("monthly"));
        assert_eq!(primary.window_minutes, Some(43200));
    }
}

#[cfg(test)]
mod token_plan_tests;
