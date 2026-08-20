//! Cursor usage — browser-cookie or local app-auth scrape of cursor.com/api/usage-summary.
//!
//! A browser session remains the first credential surface. When it has no recognized
//! session cookie, Cursor's local app store supplies an access-token-backed cookie.
//! Browser usage gets an account ID and optional cached email only when the user ID
//! in its WorkOS session cookie matches the ID decoded from Cursor's local app-auth
//! access token.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    browser_cookies::{self, CookieJar, SOURCE_LABEL},
    http::{Header, JsonRequest},
    model::{AccountInfo, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
};

pub const PROVIDER_NAME: &str = "cursor";
const DOMAIN: &str = "cursor.com";
const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const APP_AUTH_SOURCE_LABEL: &str = "cursor-app-auth";
const ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";
const CACHED_EMAIL_KEY: &str = "cursorAuth/cachedEmail";

/// A recognized session-cookie name (any of these → treat the jar as a real login).
fn is_session_cookie(name: &str) -> bool {
    matches!(
        name,
        "WorkosCursorSessionToken"
            | "__Secure-next-auth.session-token"
            | "next-auth.session-token"
            | "wos-session"
            | "__Secure-wos-session"
            | "authjs.session-token"
            | "__Secure-authjs.session-token"
    )
}

/// Resolve Cursor's application-state database for the host platform.
fn app_auth_db_path() -> Option<PathBuf> {
    app_auth_db_path_from(crate::env::home_dir(), |key| std::env::var_os(key))
}

/// Resolve the app-state path from an injected environment, keeping the platform
/// layouts independently testable without reading the caller's home directory.
fn app_auth_db_path_from(
    home: Option<PathBuf>,
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let _ = lookup;
        home.map(|home| {
            home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
        })
    }

    #[cfg(target_os = "linux")]
    {
        lookup("XDG_CONFIG_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or(home.map(|home| home.join(".config")))
            .map(|config| config.join("Cursor/User/globalStorage/state.vscdb"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (home, lookup);
        None
    }
}

struct CursorAppAuth {
    access_token: String,
    user_id: String,
    email: Option<String>,
}

/// Browser account identity retained only after the app-auth access-token ID matches
/// the user ID in the browser's WorkOS session cookie.
struct CursorBrowserIdentity {
    user_id: String,
    email: Option<String>,
}

enum CursorCredential {
    Browser {
        cookie_header: String,
        identity: Option<CursorBrowserIdentity>,
    },
    App(CursorAppAuth),
}

impl CursorCredential {
    fn cookie_header(&self) -> String {
        match self {
            Self::Browser { cookie_header, .. } => cookie_header.clone(),
            Self::App(auth) => app_auth_cookie_header(&auth.user_id, &auth.access_token),
        }
    }

    fn observed_account(&self) -> Option<&str> {
        match self {
            Self::Browser { identity, .. } => {
                identity.as_ref().map(|identity| identity.user_id.as_str())
            }
            Self::App(auth) => auth.email.as_deref(),
        }
    }

    fn account_email(&self) -> Option<&str> {
        match self {
            Self::Browser { identity, .. } => identity
                .as_ref()
                .and_then(|identity| identity.email.as_deref()),
            Self::App(auth) => auth.email.as_deref(),
        }
    }

    fn source_label(&self) -> &'static str {
        match self {
            Self::Browser { .. } => SOURCE_LABEL,
            Self::App(_) => APP_AUTH_SOURCE_LABEL,
        }
    }
}

#[derive(Deserialize)]
struct JwtPayload {
    sub: Option<String>,
}

/// Decode the JWT payload and derive the Cursor user id from its `sub` claim.
fn cursor_user_id_from_access_token(access_token: &str) -> Result<String, FetchError> {
    let mut segments = access_token.split('.');
    let _header = segments.next();
    let payload = segments.next().ok_or_else(|| {
        FetchError::CredentialUnusable("cursor app auth JWT payload step failed".to_string())
    })?;
    if segments.next().is_none() {
        return Err(FetchError::CredentialUnusable(
            "cursor app auth JWT payload step failed".to_string(),
        ));
    }

    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| {
        FetchError::CredentialUnusable(
            "cursor app auth JWT payload base64url decode failed".to_string(),
        )
    })?;
    let payload: JwtPayload = serde_json::from_slice(&payload).map_err(|_| {
        FetchError::CredentialUnusable("cursor app auth JWT payload JSON decode failed".to_string())
    })?;
    let user_id = payload
        .sub
        .as_deref()
        .and_then(|sub| sub.rsplit('|').next())
        .filter(|user_id| !user_id.is_empty())
        .ok_or_else(|| {
            FetchError::CredentialUnusable("cursor app auth sub extraction failed".to_string())
        })?;

    Ok(user_id.to_string())
}

/// Build Cursor's expected session-cookie value without percent-encoding it twice.
fn app_auth_cookie_header(user_id: &str, access_token: &str) -> String {
    format!("WorkosCursorSessionToken={user_id}%3A%3A{access_token}")
}

/// Take only the user-id prefix from Cursor's encoded WorkOS session cookie.
fn cursor_user_id_from_session_cookie(cookie_value: &str) -> Option<&str> {
    cookie_value
        .split_once("%3A%3A")
        .map(|(user_id, _)| user_id)
        .filter(|user_id| !user_id.is_empty())
}

fn workos_session_cookie_value(jar: &CookieJar) -> Option<&str> {
    jar.cookies
        .iter()
        .find(|cookie| cookie.name == "WorkosCursorSessionToken")
        .map(|cookie| cookie.value.as_str())
}

/// Attach the browser cookie's account ID and the app store's optional cached email
/// only when the ID decoded from the access token matches the WorkOS cookie's ID.
///
/// The app store only enriches browser usage; if it is missing, unreadable, or
/// belongs to another user, leave usage unlabelled instead of returning an error.
fn browser_identity_from_app_auth(
    browser_user_id: Option<&str>,
    app_auth: Result<Option<CursorAppAuth>, FetchError>,
) -> Option<CursorBrowserIdentity> {
    let browser_user_id = browser_user_id?;
    let app_auth = match app_auth {
        Ok(Some(app_auth)) => app_auth,
        Ok(None) | Err(_) => return None,
    };

    if app_auth.user_id == browser_user_id {
        Some(CursorBrowserIdentity {
            user_id: browser_user_id.to_string(),
            email: app_auth.email,
        })
    } else {
        None
    }
}

fn immutable_app_auth_uri(path: &Path) -> Result<String, FetchError> {
    let mut uri = url::Url::from_file_path(path).map_err(|_| {
        FetchError::CredentialUnusable("cursor app auth store URI construction failed".to_string())
    })?;
    uri.set_query(Some("immutable=1"));
    Ok(uri.into())
}

fn read_app_auth_value(
    connection: &Connection,
    key: &'static str,
    stage: &'static str,
) -> Result<Option<String>, FetchError> {
    connection
        .query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|_| FetchError::CredentialUnusable(format!("cursor app auth {stage} failed")))
}

/// Read only the access-token and optional email records. Refresh tokens are never
/// queried because exchanging one can invalidate the editor's active sign-in.
fn load_app_auth_from_path(path: Option<&Path>) -> Result<Option<CursorAppAuth>, FetchError> {
    let Some(path) = path else {
        return Ok(None);
    };

    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(FetchError::CredentialUnusable(
                "cursor app auth store metadata read failed".to_string(),
            ));
        }
    };
    if !metadata.is_file() {
        return Err(FetchError::CredentialUnusable(
            "cursor app auth store is not a file".to_string(),
        ));
    }

    let uri = immutable_app_auth_uri(path)?;
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|_| FetchError::CredentialUnusable("cursor app auth store open failed".to_string()))?;

    let Some(access_token) =
        read_app_auth_value(&connection, ACCESS_TOKEN_KEY, "access-token read")?
    else {
        return Ok(None);
    };
    let user_id = cursor_user_id_from_access_token(&access_token)?;
    let email = read_app_auth_value(&connection, CACHED_EMAIL_KEY, "cached-email read")?
        .map(|email| email.trim().to_string())
        .filter(|email| !email.is_empty());

    Ok(Some(CursorAppAuth {
        access_token,
        user_id,
        email,
    }))
}

fn resolve_cursor_credential(
    jar: &CookieJar,
    app_auth_path: Option<&Path>,
) -> Result<CursorCredential, FetchError> {
    if jar.has_cookie_named(is_session_cookie) {
        let browser_user_id =
            workos_session_cookie_value(jar).and_then(cursor_user_id_from_session_cookie);
        let app_auth = if browser_user_id.is_some() {
            load_app_auth_from_path(app_auth_path)
        } else {
            Ok(None)
        };
        let identity = browser_identity_from_app_auth(browser_user_id, app_auth);
        return Ok(CursorCredential::Browser {
            cookie_header: jar.header(),
            identity,
        });
    }

    load_app_auth_from_path(app_auth_path)?
        .map(CursorCredential::App)
        .ok_or_else(|| {
            FetchError::NoSession(format!(
                "no cursor session cookie in browser ({}); Cursor app store was also checked",
                jar.session_absence_detail()
            ))
        })
}

async fn resolve_cursor_credential_async(
    jar: CookieJar,
    app_auth_path: Option<PathBuf>,
) -> Result<CursorCredential, FetchError> {
    tokio::task::spawn_blocking(move || resolve_cursor_credential(&jar, app_auth_path.as_deref()))
        .await
        .map_err(|_| {
            FetchError::CredentialUnusable("cursor app auth store read task failed".to_string())
        })?
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CursorUsageSummary {
    billing_cycle_end: Option<String>,
    individual_usage: Option<IndividualUsage>,
}

#[derive(Debug, Deserialize)]
struct IndividualUsage {
    plan: Option<PlanUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanUsage {
    total_percent_used: Option<f64>,
    used: Option<f64>,
    limit: Option<f64>,
}

fn parse_billing_cycle_end(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.fZ") {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%SZ") {
        return Some(
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        );
    }
    None
}

/// Normalize the usage summary JSON to [`Usage`]. Pure — unit-testable.
pub fn normalize_usage(body: &[u8]) -> Result<Usage, FetchError> {
    let summary: CursorUsageSummary = serde_json::from_slice(body)
        .map_err(|e| FetchError::Decode(format!("cursor usage summary not decodable: {e}")))?;

    let resets_at = summary
        .billing_cycle_end
        .as_deref()
        .and_then(parse_billing_cycle_end);

    let mut used_percent = None;
    if let Some(ind) = &summary.individual_usage {
        if let Some(plan) = &ind.plan {
            if let Some(pct) = plan.total_percent_used {
                used_percent = Some(pct.clamp(0.0, 100.0));
            } else if let (Some(used), Some(limit)) = (plan.used, plan.limit) {
                if limit > 0.0 {
                    let pct = (used / limit) * 100.0;
                    used_percent = Some(pct.clamp(0.0, 100.0));
                } else {
                    used_percent = Some(0.0);
                }
            }
        }
    }

    match (used_percent, resets_at) {
        (Some(pct), Some(reset)) => Ok(Usage {
            primary: Some(RateWindow {
                used_percent: pct,
                raw_used_percent: None,
                resets_at: Some(reset),
                window_minutes: Some(43200),
                used_count: None,
                total_count: None,
            }),
            secondary: None,
            tertiary: None,
            extra_rate_windows: None,
        }),
        (Some(_), None) => Err(FetchError::Decode(
            "cursor: percent present but billingCycleEnd is missing or invalid".to_string(),
        )),
        _ => Err(FetchError::Decode(
            "cursor: no valid usage window found".to_string(),
        )),
    }
}

fn provider_usage_from_credential(credential: &CursorCredential, usage: Usage) -> ProviderUsage {
    let mut provider_usage = ProviderUsage::healthy(
        PROVIDER_NAME,
        credential.observed_account().map(str::to_string),
        credential.source_label(),
        usage,
    );
    if let Some(email) = credential.account_email() {
        provider_usage.account_info = Some(AccountInfo {
            email: Some(email.to_string()),
            org_name: None,
            plan_type: None,
        });
    }
    provider_usage
}

/// The Cursor usage provider.
pub struct CursorProvider {
    http: reqwest::Client,
}

impl CursorProvider {
    pub fn new() -> Self {
        Self {
            http: crate::http::provider_client(),
        }
    }
}

impl Default for CursorProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for CursorProvider {
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
            let credential = resolve_cursor_credential_async(jar, app_auth_db_path()).await?;
            let cookie = credential.cookie_header();

            let body_bytes = JsonRequest::get(USAGE_URL)
                .timeout(REQUEST_TIMEOUT)
                .header(Header::new("Cookie", cookie))
                .send(&self.http)
                .await?;

            let usage = normalize_usage(&body_bytes)?;
            Ok(provider_usage_from_credential(&credential, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use rusqlite::Connection;

    use super::*;

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    fn synthetic_jwt(sub: &str) -> String {
        let payload = serde_json::json!({ "sub": sub });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        format!("header.{payload}.signature")
    }

    fn test_path(label: &str) -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "cursor-app-auth-{label}-{}-{id}",
            std::process::id()
        ))
    }

    fn empty_jar() -> CookieJar {
        CookieJar { cookies: vec![] }
    }

    fn browser_jar(session_cookie_value: &str) -> CookieJar {
        CookieJar {
            cookies: vec![browser_cookies::Cookie {
                name: "WorkosCursorSessionToken".to_string(),
                value: session_cookie_value.to_string(),
                host_key: DOMAIN.to_string(),
            }],
        }
    }

    fn healthy_usage() -> Usage {
        normalize_usage(
            br#"{
                "billingCycleEnd": "2026-07-24T03:00:00Z",
                "individualUsage": { "plan": { "totalPercentUsed": 45.5 } }
            }"#,
        )
        .unwrap()
    }

    fn create_state_db(path: &Path, access_token: &str, email: Option<&str>) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .unwrap();
        connection
            .execute(
                "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                [ACCESS_TOKEN_KEY, access_token],
            )
            .unwrap();
        if let Some(email) = email {
            connection
                .execute(
                    "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
                    [CACHED_EMAIL_KEY, email],
                )
                .unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_app_auth_store_path_uses_the_injected_home() {
        let path = app_auth_db_path_from(Some(PathBuf::from("/Users/synthetic")), |_| None);

        assert_eq!(
            path.as_deref(),
            Some(Path::new(
                "/Users/synthetic/Library/Application Support/Cursor/User/globalStorage/state.vscdb"
            ))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_app_auth_store_path_prefers_xdg_then_home_config() {
        let xdg = app_auth_db_path_from(Some(PathBuf::from("/home/synthetic")), |key| {
            (key == "XDG_CONFIG_HOME").then(|| std::ffi::OsString::from("/xdg"))
        });
        let fallback = app_auth_db_path_from(Some(PathBuf::from("/home/synthetic")), |_| None);

        assert_eq!(
            xdg.as_deref(),
            Some(Path::new("/xdg/Cursor/User/globalStorage/state.vscdb"))
        );
        assert_eq!(
            fallback.as_deref(),
            Some(Path::new(
                "/home/synthetic/.config/Cursor/User/globalStorage/state.vscdb"
            ))
        );
    }

    #[test]
    fn app_auth_cookie_has_one_preencoded_delimiter() {
        let access_token = synthetic_jwt("organization|user-id");
        let user_id = cursor_user_id_from_access_token(&access_token).unwrap();
        let header = app_auth_cookie_header(&user_id, &access_token);
        let expected = format!("WorkosCursorSessionToken=user-id%3A%3A{access_token}");

        assert!(header == expected, "cookie header changed shape");
        assert_eq!(header.matches("%3A%3A").count(), 1);
        assert!(!header.contains("%253A"));
    }

    #[test]
    fn cursor_user_id_uses_last_sub_segment_or_the_whole_sub() {
        let with_separator = synthetic_jwt("organization|user-id");
        let without_separator = synthetic_jwt("user-id");

        assert_eq!(
            cursor_user_id_from_access_token(&with_separator).unwrap(),
            "user-id"
        );
        assert_eq!(
            cursor_user_id_from_access_token(&without_separator).unwrap(),
            "user-id"
        );
    }

    #[test]
    fn undecodable_app_auth_jwt_is_credential_unusable_at_payload_decode() {
        let error = cursor_user_id_from_access_token("header.not-base64!.signature").unwrap_err();

        match error {
            FetchError::CredentialUnusable(message) => {
                assert!(message.contains("JWT payload base64url decode"));
            }
            FetchError::NoSession(_) => panic!("malformed JWT must not look absent"),
            _ => panic!("malformed JWT must be locally unusable"),
        }
    }

    #[test]
    fn app_auth_store_reads_access_token_and_optional_email() {
        let path = test_path("state.vscdb");
        let access_token = synthetic_jwt("organization|user-id");
        create_state_db(&path, &access_token, Some("  account@example.test  "));

        let credential = resolve_cursor_credential(&empty_jar(), Some(&path)).unwrap();
        let CursorCredential::App(auth) = credential else {
            panic!("app auth store should provide the fallback credential");
        };
        assert_eq!(auth.user_id, "user-id");
        assert_eq!(auth.email.as_deref(), Some("account@example.test"));
        assert!(
            auth.access_token == access_token,
            "access token was not read exactly"
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn absent_app_auth_store_keeps_browser_detail_in_no_session() {
        let path = test_path("missing.vscdb");
        let jar = CookieJar {
            cookies: vec![browser_cookies::Cookie {
                name: "analytics".to_string(),
                value: "synthetic".to_string(),
                host_key: DOMAIN.to_string(),
            }],
        };

        let error = match resolve_cursor_credential(&jar, Some(&path)) {
            Err(error) => error,
            Ok(_) => panic!("missing database must not resolve a credential"),
        };
        match error {
            FetchError::NoSession(message) => {
                assert!(message.contains("1 cookie present, not recognised as a session"));
                assert!(message.contains("app store was also checked"));
            }
            _ => panic!("missing database must keep the absent-credential classification"),
        }
    }

    #[test]
    fn app_auth_store_directory_is_credential_unusable() {
        let path = test_path("directory");
        fs::create_dir(&path).unwrap();

        let error = match resolve_cursor_credential(&empty_jar(), Some(&path)) {
            Err(error) => error,
            Ok(_) => panic!("a directory must not resolve a credential"),
        };
        match error {
            FetchError::CredentialUnusable(message) => {
                assert!(message.contains("app auth store is not a file"));
            }
            FetchError::NoSession(_) => panic!("a present directory must not look absent"),
            _ => panic!("a present directory must be locally unusable"),
        }

        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn browser_session_precedes_an_unusable_app_auth_store() {
        let path = test_path("unusable-directory");
        fs::create_dir(&path).unwrap();
        let jar = CookieJar {
            cookies: vec![browser_cookies::Cookie {
                name: "WorkosCursorSessionToken".to_string(),
                value: "synthetic-browser-session".to_string(),
                host_key: DOMAIN.to_string(),
            }],
        };

        let credential = resolve_cursor_credential(&jar, Some(&path)).unwrap();
        assert!(
            matches!(credential, CursorCredential::Browser { .. }),
            "the browser session must pre-empt an unusable app store"
        );

        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn matching_browser_and_app_user_ids_attach_identity_to_browser_usage() {
        let path = test_path("matching-browser-app.vscdb");
        let access_token = synthetic_jwt("organization|user-id");
        create_state_db(&path, &access_token, Some("account@example.test"));

        let credential = resolve_cursor_credential(
            &browser_jar("user-id%3A%3Asynthetic-browser-session"),
            Some(&path),
        )
        .unwrap();
        let provider_usage = provider_usage_from_credential(&credential, healthy_usage());

        assert_eq!(provider_usage.account.as_deref(), Some("user-id"));
        assert_eq!(provider_usage.source.as_deref(), Some(SOURCE_LABEL));
        assert_eq!(
            provider_usage
                .account_info
                .as_ref()
                .and_then(|info| info.email.as_deref()),
            Some("account@example.test")
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn differing_browser_and_app_user_ids_leave_browser_usage_unlabelled() {
        let path = test_path("differing-browser-app.vscdb");
        let access_token = synthetic_jwt("organization|app-user-id");
        create_state_db(&path, &access_token, Some("account@example.test"));

        let credential = resolve_cursor_credential(
            &browser_jar("browser-user-id%3A%3Asynthetic-browser-session"),
            Some(&path),
        )
        .unwrap();
        let provider_usage = provider_usage_from_credential(&credential, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(SOURCE_LABEL));
        assert!(
            provider_usage.account.is_none(),
            "different accounts must not label usage"
        );
        assert!(provider_usage.account_info.is_none());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn absent_app_auth_store_leaves_browser_usage_unlabelled() {
        let path = test_path("missing-browser-app.vscdb");
        let credential = resolve_cursor_credential(
            &browser_jar("user-id%3A%3Asynthetic-browser-session"),
            Some(&path),
        )
        .unwrap();
        let provider_usage = provider_usage_from_credential(&credential, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(SOURCE_LABEL));
        assert!(provider_usage.account.is_none());
        assert!(provider_usage.account_info.is_none());
    }

    #[test]
    fn unreadable_app_auth_store_leaves_browser_usage_unlabelled() {
        let path = test_path("unreadable-browser-app");
        fs::create_dir(&path).unwrap();

        let credential = resolve_cursor_credential(
            &browser_jar("user-id%3A%3Asynthetic-browser-session"),
            Some(&path),
        )
        .unwrap();
        let provider_usage = provider_usage_from_credential(&credential, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(SOURCE_LABEL));
        assert!(provider_usage.account.is_none());
        assert!(provider_usage.account_info.is_none());

        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn malformed_workos_cookie_leaves_browser_usage_unlabelled() {
        let path = test_path("malformed-browser-cookie.vscdb");
        let access_token = synthetic_jwt("organization|user-id");
        create_state_db(&path, &access_token, Some("account@example.test"));

        let credential = resolve_cursor_credential(&browser_jar("user-id"), Some(&path)).unwrap();
        let provider_usage = provider_usage_from_credential(&credential, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(SOURCE_LABEL));
        assert!(provider_usage.account.is_none());
        assert!(provider_usage.account_info.is_none());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn app_auth_email_labels_the_observed_account_and_account_info() {
        let credential = CursorCredential::App(CursorAppAuth {
            access_token: synthetic_jwt("user-id"),
            user_id: "user-id".to_string(),
            email: Some("account@example.test".to_string()),
        });
        let provider_usage = provider_usage_from_credential(&credential, healthy_usage());
        assert_eq!(
            provider_usage.account.as_deref(),
            Some("account@example.test")
        );
        assert_eq!(
            provider_usage
                .account_info
                .as_ref()
                .and_then(|info| info.email.as_deref()),
            Some("account@example.test")
        );
    }

    #[test]
    fn parses_healthy_total_percent_used() {
        let json = r#"{
            "billingCycleEnd": "2026-07-24T03:00:00Z",
            "individualUsage": {
                "plan": {
                    "totalPercentUsed": 45.5
                }
            }
        }"#;
        let usage = normalize_usage(json.as_bytes()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 45.5);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T03:00:00Z"));
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn parses_healthy_used_limit_cents() {
        let json = r#"{
            "billingCycleEnd": "2026-07-24T03:00:00.123Z",
            "individualUsage": {
                "plan": {
                    "used": 2000,
                    "limit": 8000
                }
            }
        }"#;
        let usage = normalize_usage(json.as_bytes()).unwrap();
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, 25.0);
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-24T03:00:00Z"));
        assert_eq!(primary.window_minutes, Some(43200));
    }

    #[test]
    fn percent_without_billing_cycle_end_drops_window() {
        let json = r#"{
            "individualUsage": {
                "plan": {
                    "totalPercentUsed": 45.5
                }
            }
        }"#;
        let res = normalize_usage(json.as_bytes());
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }

    #[test]
    fn missing_usage_window_is_decode_error() {
        let json = r#"{
            "billingCycleEnd": "2026-07-24T03:00:00Z"
        }"#;
        let res = normalize_usage(json.as_bytes());
        assert!(matches!(res, Err(FetchError::Decode(_))));
    }
}
