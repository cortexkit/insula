//! OpenDesign workspace coding-plan credits from a vault-deposited Cookie header.
//!
//! VERIFICATION: FIXTURE-VERIFIED, not live-verified; no OpenDesign account is
//! available. The protocol and fixtures are ported from CodexBar v0.83.0:
//! `Sources/CodexBarCore/Resources/Plugins/opendesign.js`,
//! `Sources/CodexBarCore/Providers/OpenDesign/OpenDesignProviderDescriptor.swift`,
//! and `TestsPlugin/OpenDesignPluginTests.swift`.
//!
//! Only `cookie:amr-api.open-design.ai[:<account>]` is read, never a browser.
//! The whole deposited header is sent; the endpoint validates the opaque session.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    http::{Header, JsonRequest},
    model::{ProviderUsage, RateWindow, Usage},
    provider::{CredentialHandle, FetchAttempt, FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "opendesign";
const COOKIE_FAMILY: &str = "cookie:amr-api.open-design.ai";
const CURRENT_URL: &str = "https://amr-api.open-design.ai/api/v1/workspaces/current";
const SESSION_COOKIE: &str = "__Secure-better-auth.session_token";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SAFE_CREDITS: f64 = 9_007_199_254_740_991.0;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Workspace {
    workspace_id: String,
    // Declared so the unread-field log doesn't report it, and deliberately not
    // published: a workspace name isn't an account identity (see fetch_handle).
    #[allow(dead_code)]
    workspace_name: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodingUsage {
    workspace_id: String,
    eligible: bool,
    windows: Vec<Value>,
}

fn malformed() -> FetchError {
    FetchError::Decode("OpenDesign usage response format changed".into())
}

fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn workspace(body: &[u8]) -> Result<Workspace, FetchError> {
    let mut workspace: Workspace = crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body)
        .map_err(|_| malformed())?;
    workspace.workspace_id = workspace.workspace_id.trim().to_string();
    if !(1..=128).contains(&workspace.workspace_id.len())
        || !workspace
            .workspace_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(malformed());
    }
    Ok(workspace)
}

fn credits(value: &Value) -> Result<f64, FetchError> {
    let value = match value {
        Value::String(value) => {
            let mut parts = value.split('.');
            let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            if !digits(parts.next().unwrap_or_default())
                || parts.next().is_some_and(|part| !digits(part))
                || parts.next().is_some()
            {
                return Err(malformed());
            }
            value.parse::<f64>().map_err(|_| malformed())?
        }
        Value::Number(value) => value.as_f64().ok_or_else(malformed)?,
        _ => return Err(malformed()),
    };
    if !value.is_finite() || !(0.0..=MAX_SAFE_CREDITS).contains(&value) {
        return Err(malformed());
    }
    Ok(value)
}

fn window(windows: &[Value], seconds: u64, kind: &str) -> Result<Option<RateWindow>, FetchError> {
    let mut matches = windows
        .iter()
        .filter(|item| item.get("durationSeconds").and_then(Value::as_f64) == Some(seconds as f64));
    let Some(item) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        return Err(malformed());
    }
    let used = credits(&item["usedCredits"])?;
    let limit = credits(&item["limitCredits"])?;
    if limit <= 0.0 {
        return Err(malformed());
    }
    let used_percent = used / limit * 100.0;
    if !used_percent.is_finite() {
        return Err(malformed());
    }
    let resets_at = item
        .get("resetsAt")
        .and_then(text)
        .and_then(|reset| chrono::DateTime::parse_from_rfc3339(reset).ok())
        .map(|reset| {
            reset
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        });
    Ok(Some(RateWindow {
        window_kind: Some(kind.to_string()),
        used_percent,
        raw_used_percent: None,
        resets_at,
        window_minutes: Some((seconds / 60) as i64),
        used_count: Some(used),
        total_count: Some(limit),
        regeneration: None,
        breakdown: None,
    }))
}

fn normalize(body: &[u8], workspace_id: &str) -> Result<Usage, FetchError> {
    let usage: CodingUsage = crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body)
        .map_err(|_| malformed())?;
    if usage.workspace_id != workspace_id {
        return Err(malformed());
    }
    let unavailable = || {
        FetchError::NoQuotaReported(
            "OpenDesign subscription usage is unavailable for this workspace".into(),
        )
    };
    if !usage.eligible {
        return Err(unavailable());
    }
    let primary = window(
        &usage.windows,
        18_000,
        cortexkit_provider_usage::window_kind::FIVE_HOUR,
    )?;
    let secondary = window(
        &usage.windows,
        604_800,
        cortexkit_provider_usage::window_kind::WEEKLY,
    )?;
    if primary.is_none() && secondary.is_none() {
        return Err(unavailable());
    }
    Ok(Usage {
        primary,
        secondary,
        tertiary: None,
        extra_rate_windows: None,
    })
}

fn check_status(status: u16) -> Result<(), FetchError> {
    match status {
        200 => Ok(()),
        401 => Err(FetchError::Unauthorized(
            "OpenDesign session expired; sign in again at https://open-design.ai/cloud/login"
                .into(),
        )),
        // Signing in again cannot grant billing permission to the same login.
        403 => Err(FetchError::CredentialUnusable(
            "OpenDesign login lacks billing access; ask the workspace owner for billing access"
                .into(),
        )),
        _ => Err(FetchError::Upstream(format!(
            "OpenDesign request failed (HTTP {status}); try again later"
        ))),
    }
}

pub struct OpenDesignProvider {
    vault: crate::cookie_vault::CookieVault,
    http: reqwest::Client,
    workspaces_url: String,
}

impl OpenDesignProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn crate::credential_source::CredentialSource>>,
        handle_loader: Arc<crate::vault_handles::VaultHandleLoader>,
    ) -> Self {
        Self {
            vault: crate::cookie_vault::CookieVault::new(
                credential_source,
                handle_loader,
                COOKIE_FAMILY,
            ),
            http: crate::http::provider_client(),
            workspaces_url: CURRENT_URL.trim_end_matches("/current").to_string(),
        }
    }

    async fn request(
        &self,
        url: &str,
        cookie: &str,
        workspace_id: Option<&str>,
    ) -> Result<Vec<u8>, FetchError> {
        let mut request = JsonRequest::get(url)
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", cookie))
            .header(Header::new("Accept", "application/json"));
        if let Some(id) = workspace_id {
            request = request.header(Header::new("X-Vela-Workspace-Id", id));
        }
        let response = request.send_raw(&self.http).await?;
        check_status(response.status)?;
        Ok(response.body_for_parsing()?.to_vec())
    }
}

#[async_trait]
impl UsageProvider for OpenDesignProvider {
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
            if !jar.has_cookie_named(|name| name == SESSION_COOKIE) {
                return Err(FetchError::NoSession(format!(
                    "no OpenDesign {SESSION_COOKIE} session cookie {}; sign in before capturing ({})",
                    crate::cookie_vault::DEPOSIT_PHRASE,
                    jar.session_absence_detail()
                )));
            }
            let cookie = jar.header();
            let current = self
                .request(&format!("{}/current", self.workspaces_url), &cookie, None)
                .await?;
            let workspace = workspace(&current)?;
            let body = self
                .request(
                    &format!(
                        "{}/{}/billing/coding-plan-usage",
                        self.workspaces_url, workspace.workspace_id
                    ),
                    &cookie,
                    Some(&workspace.workspace_id),
                )
                .await?;
            let usage = normalize(&body, &workspace.workspace_id)?;
            // No account label. `account` is the identity consumers join on, and
            // a workspace name is a shared, renamable display string: several
            // users can see the same workspace, so naming the row after it would
            // let two people's readings collapse into one account.
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, source, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        credential_source::{CredentialSource, VaultCredential, VaultGetError},
        vault_handles::VaultHandleLoader,
    };
    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle};

    // Fixtures ported from TestsPlugin/OpenDesignPluginTests.swift at CodexBar
    // v0.83.0. Variants below exercise the same eligibility and malformed inputs.
    const CURRENT: &str = r#"{"workspaceId":"workspace-fixture","workspaceName":"Synthetic workspace","personalMembershipTier":"go"}"#;
    const FIVE_HOUR: &str = r#"{"durationSeconds":18000,"usedCredits":"500","limitCredits":"50000",
        "remainingCredits":"49500","resetsAt":"2026-10-22T04:29:30Z"}"#;
    const WEEKLY: &str = r#"{"durationSeconds":604800,"usedCredits":"10451","limitCredits":"150000",
        "remainingCredits":"139549","resetsAt":"2026-10-28T18:29:30Z"}"#;
    const COOKIE: &str =
        "__Secure-better-auth.session_token=synthetic-cookie; preference=synthetic";
    const CURRENT_PATH: &str = "/api/v1/workspaces/current";
    const USAGE_PATH: &str = "/api/v1/workspaces/workspace-fixture/billing/coding-plan-usage";

    fn usage(windows: &[&str]) -> String {
        format!(
            r#"{{"workspaceId":"workspace-fixture","eligible":true,"tier":"go","windows":[{}]}}"#,
            windows.join(",")
        )
    }

    #[test]
    fn workspace_quota_windows_use_duration_rather_than_response_order() {
        let usage = normalize(usage(&[WEEKLY, FIVE_HOUR]).as_bytes(), "workspace-fixture").unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 1.0);
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(primary.window_kind.as_deref(), Some("five_hour"));
        assert_eq!(
            (primary.used_count, primary.total_count),
            (Some(500.0), Some(50_000.0))
        );
        assert_eq!(primary.resets_at.as_deref(), Some("2026-10-22T04:29:30Z"));
        let secondary = usage.secondary.unwrap();
        assert!((secondary.used_percent - 6.967_333_333).abs() < 0.000_001);
        assert_eq!(secondary.window_minutes, Some(10080));
        assert_eq!(secondary.window_kind.as_deref(), Some("weekly"));
        assert_eq!(
            (secondary.used_count, secondary.total_count),
            (Some(10451.0), Some(150_000.0))
        );
        assert_eq!(secondary.resets_at.as_deref(), Some("2026-10-28T18:29:30Z"));
        assert!(usage.tertiary.is_none());
        assert!(usage.extra_rate_windows.is_none());
    }

    #[test]
    fn zero_usage_is_measured_and_zero_or_missing_resets_stay_unknown() {
        for reset in [
            None,
            Some(json!(0)),
            Some(json!("0")),
            Some(Value::Null),
            Some(json!("invalid")),
        ] {
            let mut window =
                json!({"durationSeconds":18000,"usedCredits":"0","limitCredits":"50000"});
            if let Some(reset) = reset {
                window["resetsAt"] = reset;
            }
            let body = usage(&[&window.to_string()]);
            let result = normalize(body.as_bytes(), "workspace-fixture").unwrap();
            let primary = result.primary.unwrap();
            assert_eq!(primary.used_percent, 0.0);
            assert_eq!(primary.used_count, Some(0.0));
            assert!(primary.resets_at.is_none());
            assert!(result.secondary.is_none());
        }
        let weekly_only = normalize(usage(&[WEEKLY]).as_bytes(), "workspace-fixture").unwrap();
        assert!(weekly_only.primary.is_none());
        assert!(weekly_only.secondary.is_some());
    }

    #[test]
    fn mismatched_workspaces_cannot_produce_a_snapshot() {
        let body = usage(&[FIVE_HOUR]).replace("workspace-fixture", "another-workspace");
        assert_eq!(
            normalize(body.as_bytes(), "workspace-fixture")
                .unwrap_err()
                .error_class(),
            "decode_failed"
        );
    }

    #[test]
    fn invalid_counters_and_duplicate_durations_cannot_produce_a_snapshot() {
        assert_eq!(
            normalize(
                usage(&[FIVE_HOUR, FIVE_HOUR]).as_bytes(),
                "workspace-fixture"
            )
            .unwrap_err()
            .error_class(),
            "decode_failed"
        );
        for field in ["usedCredits", "limitCredits"] {
            for invalid in [
                Value::Null,
                json!("invalid"),
                json!(-1),
                json!("-1"),
                json!(true),
                json!("1e3"),
                json!(" 1"),
                json!("1."),
                json!("1.2.3"),
                json!(9007199254740992_u64),
            ] {
                let mut window: Value = serde_json::from_str(FIVE_HOUR).unwrap();
                window[field] = invalid;
                let body = usage(&[&window.to_string()]);
                assert_eq!(
                    normalize(body.as_bytes(), "workspace-fixture")
                        .unwrap_err()
                        .error_class(),
                    "decode_failed",
                    "{body}"
                );
            }
        }
        for limit in [json!(0), json!("0")] {
            let body = usage(&[
                &json!({"durationSeconds":18000,"usedCredits":0,"limitCredits":limit}).to_string(),
            ]);
            assert_eq!(
                normalize(body.as_bytes(), "workspace-fixture")
                    .unwrap_err()
                    .error_class(),
                "decode_failed"
            );
        }
        let body = usage(&[r#"{"durationSeconds":18000,"usedCredits":0.5,"limitCredits":"2.5"}"#]);
        assert_eq!(
            normalize(body.as_bytes(), "workspace-fixture")
                .unwrap()
                .primary
                .unwrap()
                .used_percent,
            20.0
        );
    }

    #[test]
    fn ineligible_or_unknown_windows_report_no_quota_not_empty_allowances() {
        for body in [
            usage(&[]),
            usage(&[FIVE_HOUR]).replace("\"eligible\":true", "\"eligible\":false"),
            usage(&[r#"{"durationSeconds":3600,"usedCredits":0,"limitCredits":1}"#]),
            usage(&[r#"{"durationSeconds":"18000","usedCredits":0,"limitCredits":1}"#]),
        ] {
            assert_eq!(
                normalize(body.as_bytes(), "workspace-fixture")
                    .unwrap_err()
                    .error_class(),
                "no_quota_reported"
            );
        }
    }

    #[test]
    fn malformed_json_and_unsafe_workspace_ids_fail_closed() {
        for body in [
            "private-response-sentinel",
            "{}",
            "null",
            r#"{"workspaceId":"workspace-fixture","eligible":1,"windows":[]}"#,
        ] {
            let error = normalize(body.as_bytes(), "workspace-fixture").unwrap_err();
            assert_eq!(error.error_class(), "decode_failed");
            assert!(!error.to_string().contains("private-response-sentinel"));
        }
        for id in [
            "",
            "../escape",
            "space here",
            "id?query",
            "id\r\nheader",
            &"a".repeat(129),
        ] {
            assert!(
                workspace(json!({"workspaceId":id}).to_string().as_bytes()).is_err(),
                "{id}"
            );
        }
        assert_eq!(
            workspace(CURRENT.as_bytes()).unwrap().workspace_id,
            "workspace-fixture"
        );
    }

    struct CookieSource(&'static str);

    #[async_trait]
    impl CredentialSource for CookieSource {
        async fn get_scoped(&self, id: &str, ttl: u64) -> Result<VaultCredential, VaultGetError> {
            assert!(id == COOKIE_FAMILY || id == format!("{COOKIE_FAMILY}:fixture"));
            assert_eq!(ttl, 120_000);
            Ok(VaultCredential {
                payload: self.0.as_bytes().to_vec(),
                expires_at_ms: None,
                record_version: 1,
                account_id: None,
                email: None,
                org_name: None,
                project_id: None,
            })
        }

        async fn report_auth_failure_scoped(&self, _: &str, _: u16, _: u64) {
            panic!("cookie providers must not report authentication failures to the vault");
        }
    }

    async fn server(steps: Vec<(&str, u16, String)>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let steps: Vec<_> = steps
            .into_iter()
            .map(|(path, status, body)| (path.to_string(), status, body))
            .collect();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (path, status, body) in steps {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = crate::loopback::read_request(&mut stream).await;
                assert!(
                    request.starts_with(&format!("GET {path} HTTP/1.1\r\n")),
                    "{request}"
                );
                let lower = request.to_ascii_lowercase();
                assert!(lower.contains("accept: application/json\r\n"));
                assert!(request.contains(&format!("{COOKIE}\r\n")));
                assert!(!lower.contains("authorization:"));
                if path == USAGE_PATH {
                    assert!(lower.contains("x-vela-workspace-id: workspace-fixture\r\n"));
                } else {
                    assert!(!lower.contains("x-vela-workspace-id:"));
                }
                stream.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                requests.push(request);
            }
            requests
        });
        (format!("http://{address}/api/v1/workspaces"), task)
    }

    async fn fetch(steps: Vec<(&str, u16, String)>) -> FetchAttempt {
        let (url, task) = server(steps).await;
        let mut provider = OpenDesignProvider::new_with_handle_loader(
            Some(Arc::new(CookieSource(COOKIE))),
            Arc::new(VaultHandleLoader::default()),
        );
        provider.workspaces_url = url;
        let attempt = provider
            .fetch_handle(&CredentialHandle::scoped(COOKIE_FAMILY, "browser-session"))
            .await;
        let requests = task.await.unwrap();
        assert!(!requests.is_empty());
        attempt
    }

    #[tokio::test]
    async fn requests_validate_the_workspace_and_send_the_whole_cookie_header() {
        let attempt = fetch(vec![
            (CURRENT_PATH, 200, CURRENT.into()),
            (USAGE_PATH, 200, usage(&[WEEKLY, FIVE_HOUR])),
        ])
        .await;
        assert!(
            attempt.observed.unwrap().account_id.is_none(),
            "a workspace name is not an account identity"
        );
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(attempt.usage.unwrap().primary.unwrap().used_percent, 1.0);
        assert!(attempt.pools.is_none());
        let attempt = fetch(vec![
            (
                CURRENT_PATH,
                200,
                r#"{"workspaceId":"workspace-fixture"}"#.into(),
            ),
            (USAGE_PATH, 200, usage(&[FIVE_HOUR])),
        ])
        .await;
        assert!(attempt.observed.unwrap().account_id.is_none());
        assert!(attempt.usage.is_ok());
        assert_eq!(
            reqwest::Url::parse(CURRENT_URL).unwrap().host_str(),
            Some("amr-api.open-design.ai")
        );
    }

    #[tokio::test]
    async fn rejected_session_stops_without_retrying_or_reporting_to_the_vault() {
        for billing in [false, true] {
            let steps = if billing {
                vec![
                    (CURRENT_PATH, 200, CURRENT.into()),
                    (USAGE_PATH, 401, "private-response-sentinel".into()),
                ]
            } else {
                vec![(CURRENT_PATH, 401, "private-response-sentinel".into())]
            };
            let error = fetch(steps).await.usage.unwrap_err();
            assert_eq!(error.error_class(), "credential_rejected");
            assert!(error.to_string().contains("sign in again"));
            assert!(!error.to_string().contains("private-response-sentinel"));
            assert!(!error.to_string().contains("synthetic-cookie"));
        }
    }

    #[tokio::test]
    async fn billing_http_classes_preserve_permission_and_temporary_failures() {
        for status in [403, 408, 429, 500, 503, 404, 201, 200] {
            for billing in [false, true] {
                let steps = if billing {
                    vec![
                        (CURRENT_PATH, 200, CURRENT.into()),
                        (USAGE_PATH, status, "private-response-sentinel".into()),
                    ]
                } else {
                    vec![(CURRENT_PATH, status, "private-response-sentinel".into())]
                };
                let error = fetch(steps).await.usage.unwrap_err();
                assert!(!error.to_string().contains("private-response-sentinel"));
                assert!(!error.to_string().contains("synthetic-cookie"));
                if status == 403 {
                    assert_eq!(error.error_class(), "credential_unusable");
                    assert!(error.to_string().contains("lacks billing access"));
                    assert!(!error.to_string().contains("sign in"));
                } else if status == 200 {
                    assert_eq!(error.error_class(), "decode_failed");
                } else {
                    assert_eq!(error.error_class(), "upstream_failed");
                    assert!(matches!(
                        crate::refresh::classify(&error),
                        crate::refresh::FetchClass::Transient
                    ));
                }
            }
        }
    }

    #[tokio::test]
    async fn missing_deposit_or_session_cookie_never_sends_a_request() {
        let mut provider = OpenDesignProvider::new_with_handle_loader(
            None,
            Arc::new(VaultHandleLoader::default()),
        );
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert_eq!(
            attempt.usage.unwrap_err().error_class(),
            "credential_absent"
        );
        provider = OpenDesignProvider::new_with_handle_loader(
            Some(Arc::new(CookieSource("preference=synthetic"))),
            Arc::new(VaultHandleLoader::default()),
        );
        provider.workspaces_url = "invalid URL must not be requested".into();
        let error = provider
            .fetch_handle(&CredentialHandle::scoped(COOKIE_FAMILY, "browser-session"))
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "credential_absent");
        assert!(error.to_string().contains(SESSION_COOKIE));
    }
}
