//! Langdock personal usage from a `cookie:langdock.com[:<account>]` vault deposit.
//!
//! VERIFICATION: FIXTURE-VERIFIED, not live-verified; no Langdock account exists
//! on this host. Wire contract and inline fixtures come from CodexBar v0.73.0
//! (`1d313fe50a361fc0a12383da0cdc11a75f59daa5`):
//! - `Sources/CodexBarCore/Providers/Langdock/LangdockProviderDescriptor.swift`
//! - `Sources/CodexBarCore/Resources/Plugins/langdock.ts`
//! - `Tests/CodexBarTests/LangdockPluginTests.swift` and `LangdockUsageTests.swift`
//!
//! Upstream binds `auth_token` to a selected browser profile and revalidates its
//! owner before publishing. Insula never reads a browser: CookieVault selects
//! one deposited session per family, preferring an account suffix to a bare id.
//! Re-capture replaces that deposit when switching accounts. The personal-usage
//! response carries no stable user id, so neither a deposit suffix nor a shared
//! workspace id is published as account identity.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{CredentialHandle, FetchAttempt, FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "langdock";
const COOKIE_FAMILY: &str = "cookie:langdock.com";
const USAGE_URL: &str = "https://app.langdock.com/api/trpc/usageSettings.getPersonalUsage";
const REFERER_URL: &str = "https://app.langdock.com/settings/account/usage";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const TRPC_INPUT: &str = r#"{"0":{"json":null,"meta":{"values":["undefined"],"v":1}}}"#;

fn is_session_cookie(name: &str) -> bool {
    name == "auth_token"
}

fn request_url() -> String {
    let mut url = reqwest::Url::parse(USAGE_URL).expect("Langdock usage URL is a valid constant");
    url.query_pairs_mut()
        .extend_pairs([("batch", "1"), ("input", TRPC_INPUT)]);
    url.into()
}

#[derive(Deserialize)]
struct TrpcEnvelope {
    result: Option<TrpcResult>,
    error: Option<TrpcError>,
}

#[derive(Deserialize)]
struct TrpcResult {
    data: TrpcData,
}

#[derive(Deserialize)]
struct TrpcData {
    json: Value,
}

#[derive(Deserialize)]
struct TrpcError {
    json: TrpcErrorJson,
}

#[derive(Deserialize)]
struct TrpcErrorJson {
    data: TrpcErrorData,
}

#[derive(Deserialize)]
struct TrpcErrorData {
    code: String,
}

fn window(
    plan: &Value,
    percent_key: &str,
    reset_key: &str,
    window_kind: Option<&str>,
    minutes: i64,
) -> Result<RateWindow, FetchError> {
    let used_percent = plan
        .get(percent_key)
        .and_then(Value::as_f64)
        .filter(|percent| percent.is_finite())
        .ok_or_else(|| FetchError::Decode(format!("langdock: invalid {percent_key}")))?;
    let resets_at = match plan.get(reset_key) {
        None | Some(Value::Null) => None,
        Some(Value::String(reset)) => Some(
            chrono::DateTime::parse_from_rfc3339(reset)
                .map_err(|_| FetchError::Decode(format!("langdock: invalid {reset_key}")))?
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        ),
        Some(_) => return Err(FetchError::Decode(format!("langdock: invalid {reset_key}"))),
    };
    Ok(RateWindow {
        window_kind: window_kind.map(str::to_string),
        used_percent,
        raw_used_percent: None,
        resets_at,
        window_minutes: Some(minutes),
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    })
}

/// Decode one personal-usage tRPC batch without inventing counts or reset dates.
pub fn normalize(body: &[u8]) -> Result<Usage, FetchError> {
    let batch: Vec<TrpcEnvelope> = crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body)
        .map_err(|error| FetchError::Decode(format!("langdock: invalid tRPC JSON: {error}")))?;
    let [envelope]: [TrpcEnvelope; 1] = batch
        .try_into()
        .map_err(|_| FetchError::Decode("langdock: expected one tRPC result".into()))?;
    if let Some(error) = envelope.error {
        return Err(match error.json.data.code.as_str() {
            "UNAUTHORIZED" => FetchError::Unauthorized("langdock session expired".into()),
            "FORBIDDEN" => FetchError::Upstream("langdock permission denied".into()),
            "TOO_MANY_REQUESTS" => FetchError::ProviderStatus(429, "langdock rate limited".into()),
            "INTERNAL_SERVER_ERROR" | "TIMEOUT" => {
                FetchError::Upstream("langdock provider unavailable".into())
            }
            "" => FetchError::Decode("langdock: missing tRPC error code".into()),
            _ => FetchError::Upstream("langdock tRPC request failed".into()),
        });
    }
    let payload = envelope
        .result
        .ok_or_else(|| FetchError::Decode("langdock: missing tRPC result".into()))?
        .data
        .json;
    let data = payload
        .as_object()
        .ok_or_else(|| FetchError::Decode("langdock: missing usage object".into()))?;
    let included = match data.get("hasIncludedUsageLimits") {
        None => None,
        Some(Value::Bool(included)) => Some(*included),
        Some(_) => {
            return Err(FetchError::Decode(
                "langdock: invalid hasIncludedUsageLimits".into(),
            ));
        }
    };
    let plan = data.get("planUsage");
    if included == Some(false) || plan.is_none_or(Value::is_null) {
        return Err(FetchError::NoQuotaReported(
            "langdock: no included usage limits available".into(),
        ));
    }
    let plan = plan.expect("non-null plan checked above");
    let session_enabled = plan
        .get("sessionUsageLimitsEnabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| FetchError::Decode("langdock: invalid sessionUsageLimitsEnabled".into()))?;
    // The plugin defines a 300-minute session, but the response names only
    // "session", not a five-hour period. Weekly is explicitly named in its keys.
    let primary = session_enabled
        .then(|| window(plan, "sessionUsagePercent", "sessionResetsAt", None, 300))
        .transpose()?;
    let secondary = Some(window(
        plan,
        "weeklyUsagePercent",
        "weeklyResetsAt",
        Some(cortexkit_provider_usage::window_kind::WEEKLY),
        10080,
    )?);
    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

fn is_sign_in_redirect(final_url: &str) -> bool {
    reqwest::Url::parse(final_url).is_ok_and(|url| {
        url.host_str() != Some("app.langdock.com")
            || url.path().split('/').any(|part| {
                matches!(
                    part.to_ascii_lowercase().as_str(),
                    "login" | "signin" | "sign-in" | "auth"
                )
            })
    })
}

fn check_response(status: u16, final_url: &str) -> Result<(), FetchError> {
    if status == 401 {
        return Err(FetchError::Unauthorized(
            "langdock session expired (HTTP 401)".into(),
        ));
    }
    if is_sign_in_redirect(final_url) {
        return Err(FetchError::Unauthorized(
            "langdock redirected to sign-in".into(),
        ));
    }
    if !(200..300).contains(&status) {
        // A 403 is permission denied upstream, not proof that the cookie expired.
        return Err(FetchError::Upstream(format!("langdock HTTP {status}")));
    }
    Ok(())
}

pub struct LangdockProvider {
    vault: crate::cookie_vault::CookieVault,
    http: reqwest::Client,
}

impl LangdockProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn crate::credential_source::CredentialSource>>,
        handle_loader: Arc<crate::vault_handles::VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            vault: crate::cookie_vault::CookieVault::new(
                credential_source,
                handle_loader,
                COOKIE_FAMILY,
            ),
        }
    }
}

#[async_trait]
impl UsageProvider for LangdockProvider {
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
        let result = async {
            let (jar, source) = self.vault.jar_for(handle).await?;
            if !jar.has_cookie_named(is_session_cookie) {
                return Err(FetchError::NoSession(format!(
                    "no langdock auth_token session cookie {} ({})",
                    crate::cookie_vault::DEPOSIT_PHRASE,
                    jar.session_absence_detail()
                )));
            }
            let response = JsonRequest::get(request_url())
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Cookie", jar.header()))
                .header(Header::new("Accept", "application/json"))
                .header(Header::new("Referer", REFERER_URL))
                .send_raw(&self.http)
                .await?;
            check_response(response.status, &response.final_url)?;
            let usage = normalize(response.body_for_parsing()?)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, source, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Included-limit, no-limit and malformed responses pin the plugin's wire
    // contract without requiring a live account. Inline fixtures from CodexBar v0.73.0, commit
    // 1d313fe50a361fc0a12383da0cdc11a75f59daa5:
    // Tests/CodexBarTests/LangdockPluginTests.swift:7-15,98-121.
    const NORMAL: &[u8] = br#"[{"result":{"data":{"json":{
        "hasIncludedUsageLimits":true,"planUsage":{
            "sessionUsageLimitsEnabled":true,"sessionUsagePercent":12.5,
            "sessionResetsAt":"2026-09-25T12:00:00.123Z",
            "weeklyUsagePercent":104.2,"weeklyResetsAt":"2026-09-28T12:00:00Z"
        }
    }}}}]"#;

    #[test]
    fn normal_account_reports_used_percent_and_reset() {
        let usage = normalize(NORMAL).unwrap();
        let session = usage.primary.unwrap();
        assert_eq!(session.used_percent, 12.5);
        assert_eq!(
            session.resets_at.as_deref(),
            Some("2026-09-25T12:00:00.123Z")
        );
        assert_eq!(session.window_minutes, Some(300));
        assert_eq!(session.window_kind, None);
        assert_eq!((session.used_count, session.total_count), (None, None));
        let weekly = usage.secondary.unwrap();
        assert_eq!(weekly.used_percent, 104.2);
        assert_eq!(weekly.resets_at.as_deref(), Some("2026-09-28T12:00:00Z"));
        assert_eq!(weekly.window_minutes, Some(10080));
        assert_eq!(weekly.window_kind.as_deref(), Some("weekly"));
    }

    #[test]
    fn no_included_limits_reports_no_quota_not_an_idle_window() {
        for body in [
            br#"[{"result":{"data":{"json":{"hasIncludedUsageLimits":false}}}}]"#.as_slice(),
            br#"[{"result":{"data":{"json":{"hasIncludedUsageLimits":true,"planUsage":null}}}}]"#,
            br#"[{"result":{"data":{"json":{}}}}]"#,
        ] {
            assert_eq!(
                normalize(body).unwrap_err().error_class(),
                "no_quota_reported"
            );
        }
    }

    #[test]
    fn signed_out_redirect_reports_credential_rejected() {
        // A final-URL landing is our transport fixture, not an upstream live
        // capture. Even a 200 with quota-shaped JSON must not hide a login redirect.
        for url in [
            "https://app.langdock.com/login?returnTo=%2Fsettings",
            "https://app.langdock.com/signin",
            "https://auth.langdock.com/",
        ] {
            let result = check_response(200, url).and_then(|()| normalize(NORMAL));
            assert_eq!(result.unwrap_err().error_class(), "credential_rejected");
        }
        assert!(check_response(200, USAGE_URL).is_ok());
    }

    #[test]
    fn missing_reset_stays_absent_for_both_windows() {
        // A null reset means no date was reported, not "now" or a previous
        // snapshot's date. LangdockUsageTests.swift:13-22 at the same v0.73.0
        // commit supplies the null-reset fixture.
        for reset in ["null", "\"2026-09-25T12:00:00.123Z\""] {
            let body = format!(
                r#"[{{"result":{{"data":{{"json":{{"planUsage":{{
                    "sessionUsageLimitsEnabled":true,"sessionUsagePercent":0,
                    "sessionResetsAt":{reset},"weeklyUsagePercent":0,"weeklyResetsAt":null
                }}}}}}}}}}]"#
            );
            let usage = normalize(body.as_bytes()).unwrap();
            assert_eq!(usage.secondary.unwrap().resets_at, None);
            if reset == "null" {
                assert_eq!(usage.primary.unwrap().resets_at, None);
            }
        }
        let body = br#"[{"result":{"data":{"json":{"planUsage":{
            "sessionUsageLimitsEnabled":true,"sessionUsagePercent":0,"weeklyUsagePercent":0
        }}}}}]"#;
        let usage = normalize(body).unwrap();
        assert_eq!(usage.primary.unwrap().resets_at, None);
        assert_eq!(usage.secondary.unwrap().resets_at, None);
    }

    #[test]
    fn garbage_and_invalid_fields_report_decode_failed() {
        for body in [
            "not JSON",
            "[]",
            "[{},{}]",
            r#"[{"result":{"data":null}}]"#,
            r#"[{"error":{}}]"#,
            r#"[{"error":{"json":{"data":{"code":403}}}}]"#,
            r#"[{"result":{"data":{"json":{"hasIncludedUsageLimits":null}}}}]"#,
            r#"[{"result":{"data":{"json":{"planUsage":{"sessionUsageLimitsEnabled":true,"weeklyUsagePercent":4}}}}}]"#,
            r#"[{"result":{"data":{"json":{"planUsage":{"sessionUsageLimitsEnabled":true,"sessionUsagePercent":"5","weeklyUsagePercent":4}}}}}]"#,
            r#"[{"result":{"data":{"json":{"planUsage":{"sessionUsageLimitsEnabled":false,"weeklyUsagePercent":"4"}}}}}]"#,
            r#"[{"result":{"data":{"json":{"planUsage":{"sessionUsageLimitsEnabled":false,"weeklyUsagePercent":4,"weeklyResetsAt":"tomorrow"}}}}}]"#,
        ] {
            assert_eq!(
                normalize(body.as_bytes()).unwrap_err().error_class(),
                "decode_failed",
                "{body}"
            );
        }
    }

    #[test]
    fn only_auth_token_counts_as_a_session_not_trackers() {
        // The plugin's cookiePolicy.requiredNames and ProviderBrowserSessionTests.swift
        // at v0.73.0 require auth_token; preference cookies are not session identity.
        assert!(
            crate::cookie_jar::CookieJar::from_header("auth_token=fixture")
                .has_cookie_named(is_session_cookie)
        );
        for name in [
            "_ga",
            "_gid",
            "preferences",
            "session",
            "auth_token_extra",
            "AUTH_TOKEN",
        ] {
            assert!(
                !crate::cookie_jar::CookieJar::from_header(&format!("{name}=fixture"))
                    .has_cookie_named(is_session_cookie)
            );
        }
    }

    #[test]
    fn http_and_trpc_refusals_keep_auth_permission_and_rate_limits_distinct() {
        assert_eq!(
            check_response(401, USAGE_URL).unwrap_err().error_class(),
            "credential_rejected"
        );
        for status in [403, 429, 500] {
            let error = check_response(status, USAGE_URL).unwrap_err();
            assert_eq!(error.error_class(), "upstream_failed");
            assert!(matches!(
                crate::refresh::classify(&error),
                crate::refresh::FetchClass::Transient
            ));
        }
        for (code, class) in [
            ("UNAUTHORIZED", "credential_rejected"),
            ("FORBIDDEN", "upstream_failed"),
            ("TOO_MANY_REQUESTS", "upstream_failed"),
            ("TIMEOUT", "upstream_failed"),
            ("INTERNAL_SERVER_ERROR", "upstream_failed"),
        ] {
            let body = format!(r#"[{{"error":{{"json":{{"data":{{"code":"{code}"}}}}}}}}]"#);
            assert_eq!(normalize(body.as_bytes()).unwrap_err().error_class(), class);
        }
    }

    #[test]
    fn disabled_session_reports_weekly_only() {
        let body = br#"[{"result":{"data":{"json":{"planUsage":{
            "sessionUsageLimitsEnabled":false,"weeklyUsagePercent":4
        }}}}}]"#;
        let usage = normalize(body).unwrap();
        assert!(usage.primary.is_none());
        assert_eq!(usage.secondary.unwrap().used_percent, 4.0);
    }

    #[test]
    fn request_targets_personal_usage_with_the_upstream_batch_input() {
        let url = reqwest::Url::parse(&request_url()).unwrap();
        assert_eq!(url.host_str(), Some("app.langdock.com"));
        assert_eq!(url.path(), "/api/trpc/usageSettings.getPersonalUsage");
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.get("batch").map(|value| value.as_ref()), Some("1"));
        assert_eq!(
            query.get("input").map(|value| value.as_ref()),
            Some(r#"{"0":{"json":null,"meta":{"values":["undefined"],"v":1}}}"#)
        );
    }
}
