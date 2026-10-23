//! Cursor usage — session-cookie or local app-auth scrape of cursor.com/api/usage-summary.
//!
//! Three lanes, in precedence order:
//! - an `oauth:cursor` vault deposit (the Cursor app's access token);
//! - a `cookie:cursor.com` vault deposit (a browser session captured by
//!   Cerebellum). When the deposit holds no recognized session cookie, Cursor's
//!   local app store supplies an access-token-backed cookie instead;
//! - with neither deposit, Cursor's own app store on this machine
//!   (`state.vscdb`, the editor's file, not a browser's).
//!
//! Deposited-session usage gets an account ID and optional cached email only when
//! the user ID in its WorkOS session cookie matches the ID decoded from Cursor's
//! local app-auth access token.
//!
//! Unified personal accounts (`membershipType` `unified_*` with `limitType`
//! `user`) also get their monthly on-demand spend, read from a second dashboard
//! endpoint and published as the `on_demand` pool. That read is optional: when it
//! fails the plan window is published exactly as it would have been without it.
//!
//! Accounts on The X Subscription (Cursor and Grok billed together) draw on a
//! shared weekly pool, while the usage summary keeps reporting a stale monthly
//! cycle for them, often at 100%. After a successful summary read, the provider
//! therefore also reads Cursor's Grok Bot ("Sand") usage status; when that
//! reading flags the account as unified and describes a paid weekly pool that
//! has not yet reset, the pool replaces the monthly window as `primary`. Any
//! other answer, or a failed read, leaves the monthly window exactly as the
//! summary gives it.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    cookie_jar::CookieJar,
    http::{Header, JsonRequest},
    model::{AccountInfo, Amount, Pool, PoolBasis, PoolFunding, ProviderUsage, RateWindow, Usage},
    provider::{FetchError, UsageProvider},
    LOG_TAG,
};

pub const PROVIDER_NAME: &str = "cursor";
/// Base key for storing Cursor credentials in the provider vault. An account-specific
/// suffix identifies each account, and these credentials are read only from the provider vault.
const COOKIE_FAMILY: &str = "cookie:cursor.com";

/// The `source` published when the credential is Cursor's own app sign-in on
/// this machine: a session found locally, fixed by signing in to the Cursor app.
const APP_STORE_SOURCE: &str = "oauth";
const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Cursor's dashboard reading of a unified personal account's monthly spend.
/// Unified accounts are billed for on-demand use through this reading; the
/// usage summary can report a false zero for them.
const USAGE_STATUS_URL: &str = "https://cursor.com/api/dashboard/v2/usage-status?scope=user";
/// The spend read is optional, so it gets a 5-second timeout of its own. If it
/// times out or fails in any other way, no `on_demand` pool is published and the
/// plan window, already read, is returned unchanged.
const USAGE_STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// Cursor's dashboard reading of the Grok Bot (internally "Sand") allowance.
/// For an X Subscription account the same reading is the weekly pool shared
/// across Cursor and Grok, which is the limit that actually binds it.
const SAND_USAGE_URL: &str = "https://cursor.com/api/dashboard/get-sand-usage-status";
/// The dashboard answers this POST from its own page, so the request carries the
/// dashboard's origin, as the browser would.
const SAND_USAGE_ORIGIN: &str = "https://cursor.com";
/// The Sand read is optional, like the spend read: a timeout or any other
/// failure leaves the monthly window in place.
const SAND_USAGE_TIMEOUT: Duration = Duration::from_secs(5);
/// Cursor states the pool's next reset but not its length. Paid Grok Bot
/// allowances reset every seven days (CodexBar models them the same way), and
/// the reading's `currentPeriodStart` can fall mid-week, so it cannot supply the
/// length either.
const SHARED_WEEKLY_POOL_MINUTES: i64 = 7 * 24 * 60;
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
fn app_auth_db_path() -> Result<Option<PathBuf>, FetchError> {
    let platform = if cfg!(target_os = "macos") {
        AppPlatform::Macos
    } else if cfg!(windows) {
        AppPlatform::Windows
    } else {
        AppPlatform::Linux
    };
    app_auth_db_path_from(
        crate::env::home_dir(),
        |key| std::env::var_os(key),
        platform,
    )
}

#[derive(Clone, Copy)]
enum AppPlatform {
    Macos,
    Linux,
    Windows,
}

/// Resolve Cursor's own app folder, not CortexKit's configuration directory.
fn app_auth_db_path_from(
    home: Option<PathBuf>,
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
    platform: AppPlatform,
) -> Result<Option<PathBuf>, FetchError> {
    let base = match platform {
        AppPlatform::Macos => home.map(|home| home.join("Library").join("Application Support")),
        AppPlatform::Linux => lookup("XDG_CONFIG_HOME")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .or(home.map(|home| home.join(".config"))),
        AppPlatform::Windows => {
            // A relative APPDATA means the store can't be located, not that
            // nobody signed in: report "couldn't look" so the last reading is
            // kept rather than an absent or broken credential.
            cortexkit_paths::env_absolute_path(&crate::env::LookupEnv(lookup), "APPDATA")
                .map_err(|_| {
                    FetchError::LocalSourceUnavailable(
                        "cursor app auth store: APPDATA is set but not an absolute path"
                            .to_string(),
                    )
                })?
                .or(home.map(|home| home.join("AppData").join("Roaming")))
        }
    };
    Ok(base.map(|base| {
        base.join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }))
}

struct CursorAppAuth {
    access_token: String,
    user_id: String,
    email: Option<String>,
}

/// Session account identity retained only after the app-auth access-token ID matches
/// the user ID in the deposited WorkOS session cookie.
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
            Self::App(auth) => Some(auth.user_id.as_str()),
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

/// Attach the deposited cookie's account ID and the app store's optional cached email
/// only when the ID decoded from the access token matches the WorkOS cookie's ID.
///
/// The app store only enriches deposited-session usage; if it is missing, unreadable, or
/// belongs to another user, leave usage unlabelled instead of returning an error.
///
/// RETURNING `None` HERE ALSO WITHHOLDS A PRUNING AUTHORISATION, AND THAT IS
/// PROTECTION BY COINCIDENCE RATHER THAN BY DESIGN -- worth knowing before making
/// this function more determined about finding an identity.
///
/// `completeProviders` is the only thing that authorises a consumer to replace a
/// provider's stored account set, and a provider is complete only when EVERY
/// handle resolved an identity. Cursor is the one cookie provider that resolves
/// one at all (the other eight are identity-less by contract and can never be
/// complete), so cursor is the only one where a cookie that comes back
/// ambiguous -- a deposit captured with no session cookie in it, which is
/// indistinguishable from a signed-out account -- is in reach of a destructive
/// act.
///
/// What prevents it is this `None`: no identity, so not complete, so no prune.
/// But the refusal is for an unrelated reason. Nothing here knows it is standing
/// between an ambiguous cookie and someone deleting a live account, and a
/// change that made identity resolution succeed more often -- a fallback to the
/// app store's user when the cookie has none, say -- would remove that cover
/// silently while looking like an improvement.
///
/// So if this ever grows a path that yields an identity WITHOUT a confirmed
/// session cookie, the completeness gate needs its own reason to refuse a
/// provider whose credential could not be read.
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
    #[cfg(windows)]
    if let Some(host) = uri.host_str() {
        // SQLite rejects non-local URI authorities in its default build. An
        // empty authority with a //server/share path expresses the same UNC file.
        return Ok(format!("file:////{host}{}?immutable=1", uri.path()));
    }
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
                // Tells the operator both places were tried, which is the part
                // that changes what they do next.
                "no cursor session cookie {} ({}); Cursor app store was also checked",
                crate::cookie_vault::DEPOSIT_PHRASE,
                jar.session_absence_detail()
            ))
        })
}

/// The app-store lane on its own: Cursor's sign-in on this machine, used when no
/// cookie deposit exists.
fn resolve_app_store_credential(
    app_auth_path: Option<&Path>,
) -> Result<CursorCredential, FetchError> {
    load_app_auth_from_path(app_auth_path)?
        .map(CursorCredential::App)
        .ok_or_else(|| {
            FetchError::NoSession(
                "no cursor cookie deposit and no Cursor app sign-in on this host".to_string(),
            )
        })
}

/// Resolve off the async runtime, because the app store is a SQLite read.
/// `None` for the jar means there is no cookie deposit, so only the app store
/// can answer.
async fn resolve_cursor_credential_async(
    jar: Option<CookieJar>,
    app_auth_path: Option<PathBuf>,
) -> Result<CursorCredential, FetchError> {
    tokio::task::spawn_blocking(move || match jar {
        Some(jar) => resolve_cursor_credential(&jar, app_auth_path.as_deref()),
        None => resolve_app_store_credential(app_auth_path.as_deref()),
    })
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
    membership_type: Option<String>,
    limit_type: Option<String>,
}

impl CursorUsageSummary {
    /// Whether the account is billed under Cursor's unified personal plan, the
    /// one kind of account whose on-demand spend is read from the dashboard's
    /// usage-status endpoint. A team limit (`limitType: team`) is a different
    /// budget that endpoint does not describe, so it does not qualify.
    fn uses_unified_personal_billing(&self) -> bool {
        let unified = self
            .membership_type
            .as_deref()
            .is_some_and(|kind| kind.to_ascii_lowercase().starts_with("unified_"));
        let personal = self
            .limit_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("user"));
        unified && personal
    }
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
    normalize_summary(body).map(|summary| summary.usage)
}

/// What one usage-summary response says: the plan window, and whether the
/// account needs the separate monthly spend read.
struct SummaryReading {
    usage: Usage,
    unified_personal: bool,
}

fn normalize_summary(body: &[u8]) -> Result<SummaryReading, FetchError> {
    let summary: CursorUsageSummary =
        crate::unread_keys::decode_reporting_unread(PROVIDER_NAME, body)
            .map_err(|e| FetchError::Decode(format!("cursor usage summary not decodable: {e}")))?;
    let unified_personal = summary.uses_unified_personal_billing();
    let usage = plan_usage(summary)?;
    Ok(SummaryReading {
        usage,
        unified_personal,
    })
}

fn plan_usage(summary: CursorUsageSummary) -> Result<Usage, FetchError> {
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
                window_kind: None,
                used_percent: pct,
                raw_used_percent: None,
                resets_at: Some(reset),
                window_minutes: Some(43200),
                used_count: None,
                total_count: None,
                regeneration: None,
                breakdown: None,
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

/// The usage-status response for a unified personal account. Every field is
/// optional here so a missing one is a stated refusal reason rather than a
/// decode error; a field of the wrong type is still a decode error.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnifiedUsageStatus {
    availability: Option<String>,
    monthly_usage: Option<MonthlyUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MonthlyUsage {
    used_cents: Option<f64>,
    limit_cents: Option<f64>,
    period_start: Option<String>,
    resets_at: Option<String>,
}

/// Why a unified account's monthly spend was not published. Every variant
/// publishes no pool and leaves the plan window alone; the reason only names
/// the cause in the log. It never carries a value from the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpendRefusal {
    /// The request failed: transport error, timeout, or a non-2xx status.
    RequestFailed,
    /// The body is not the expected JSON shape.
    Undecodable,
    /// `availability` is absent or anything other than `available`.
    NotAvailable,
    /// `monthlyUsage` is absent.
    MonthlyUsageMissing,
    /// A cents figure is absent, negative, not finite, fractional, or above
    /// 2^53.
    AmountUnusable,
    /// `periodStart` or `resetsAt` is absent or not an RFC 3339 time.
    DateUnreadable,
    /// The reading describes a period that does not contain the present.
    OutsidePeriod,
}

impl SpendRefusal {
    fn as_str(self) -> &'static str {
        match self {
            Self::RequestFailed => "request failed",
            Self::Undecodable => "undecodable",
            Self::NotAvailable => "not available",
            Self::MonthlyUsageMissing => "monthly usage missing",
            Self::AmountUnusable => "amount unusable",
            Self::DateUnreadable => "date unreadable",
            Self::OutsidePeriod => "outside its period",
        }
    }
}

/// A cents figure as integer minor units, or `None` when it cannot be one.
///
/// The pool states amounts in exact minor units, so a fractional cent would
/// have to be rounded one way or the other; that shape has not been observed,
/// so it publishes nothing rather than a guess. The upper bound keeps the
/// conversion exact: past 2^53 an `f64` no longer holds every whole number.
fn whole_cents(cents: Option<f64>) -> Option<i64> {
    const MAX_EXACT: f64 = 9_007_199_254_740_992.0;
    let cents = cents?;
    if !cents.is_finite() || cents < 0.0 || cents.fract() != 0.0 || cents > MAX_EXACT {
        return None;
    }
    Some(cents as i64)
}

fn parse_instant(text: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(text?.trim())
        .ok()
        .map(|instant| instant.with_timezone(&chrono::Utc))
}

/// Turn a usage-status body into the monthly on-demand pool, or say why not.
///
/// Accepted only when Cursor says the reading is `available`, both cents
/// figures are usable, and `now` falls inside the stated period. A reading for
/// another month would publish last month's spend as this month's.
fn on_demand_pool(body: &[u8], now: chrono::DateTime<chrono::Utc>) -> Result<Pool, SpendRefusal> {
    let status: UnifiedUsageStatus =
        serde_json::from_slice(body).map_err(|_| SpendRefusal::Undecodable)?;
    if status.availability.as_deref() != Some("available") {
        return Err(SpendRefusal::NotAvailable);
    }
    let monthly = status
        .monthly_usage
        .ok_or(SpendRefusal::MonthlyUsageMissing)?;
    let used = whole_cents(monthly.used_cents).ok_or(SpendRefusal::AmountUnusable)?;
    let limit = whole_cents(monthly.limit_cents).ok_or(SpendRefusal::AmountUnusable)?;
    let start =
        parse_instant(monthly.period_start.as_deref()).ok_or(SpendRefusal::DateUnreadable)?;
    let reset = parse_instant(monthly.resets_at.as_deref()).ok_or(SpendRefusal::DateUnreadable)?;
    if !(start <= now && now < reset) {
        return Err(SpendRefusal::OutsidePeriod);
    }
    let usd = |minor: i64| Amount {
        minor,
        exponent: 2,
        unit: "USD".to_string(),
    };
    Ok(Pool {
        id: "on_demand".to_string(),
        label: "On-demand spend (monthly)".to_string(),
        // On-demand use is billed after it happens. None of granted, purchased
        // or subscription says that, and `unknown` is the value every consumer
        // already reads as "spending this may cost money".
        funding: PoolFunding::Unknown,
        // Spend can run past the limit; a pool never goes below empty.
        remaining: Some(usd(limit.saturating_sub(used).max(0))),
        total: Some(usd(limit)),
        // The usage figure covers this pool alone, so the limit minus it is the
        // exact remainder, not a ceiling shared with other pools.
        basis: PoolBasis::Reported,
        // Cursor states no on/off switch for on-demand spend here.
        spendable: None,
        resets_at: Some(reset.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
        expires_at: None,
        unlimited: None,
    })
}

/// The fields of Cursor's Grok Bot ("Sand") usage status that decide whether
/// it is an X Subscription account's shared weekly pool. The reply carries more
/// (upsell links, plan labels, on-demand settings); none of it bears on the
/// pool. Every field is optional so an absent one simply fails the check below.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SandUsageStatus {
    next_reset_timestamp_utc: Option<String>,
    usage_percent: Option<f64>,
    has_non_zero_included_limit: Option<bool>,
    included_limit_zero: Option<bool>,
    is_unified: Option<bool>,
}

/// The X Subscription's shared weekly pool, read from a Sand usage-status body,
/// or `None` when the body does not describe one that applies now.
///
/// Mirrors CodexBar's rule: the account must be flagged unified, hold a paid
/// allowance (`includedLimitZero: false`, or when that field is absent
/// `hasNonZeroIncludedLimit: true`), report a percentage, and state a next
/// reset that is still in the future. A free trial has no recurring reset, and
/// a reset already past means the reading is stale, so neither replaces the
/// monthly window.
///
/// The reply names no period, only a reset time, so `window_kind` stays unset;
/// the seven-day length is the known cadence of paid Grok Bot allowances.
fn shared_weekly_pool(body: &[u8], now: chrono::DateTime<chrono::Utc>) -> Option<RateWindow> {
    let status: SandUsageStatus = serde_json::from_slice(body).ok()?;
    if status.is_unified != Some(true) {
        return None;
    }
    // The newer `includedLimitZero` field wins when Cursor sends both.
    let paid_allowance = status
        .included_limit_zero
        .map(|zero| !zero)
        .or(status.has_non_zero_included_limit);
    if paid_allowance != Some(true) {
        return None;
    }
    let used_percent = status.usage_percent?;
    let reset = parse_instant(status.next_reset_timestamp_utc.as_deref())?;
    if reset <= now {
        return None;
    }
    Some(RateWindow {
        window_kind: None,
        used_percent: used_percent.clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at: Some(reset.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)),
        window_minutes: Some(SHARED_WEEKLY_POOL_MINUTES),
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    })
}

fn provider_usage_from_credential(
    credential: &CursorCredential,
    source: &'static str,
    usage: Usage,
) -> ProviderUsage {
    let mut provider_usage = ProviderUsage::healthy(
        PROVIDER_NAME,
        credential.observed_account().map(str::to_string),
        source,
        usage,
    );
    if let Some(email) = credential.account_email() {
        provider_usage.account_info = Some(AccountInfo {
            email: Some(email.to_string()),
            org_name: None,
            plan_type: None,
            subscription_renews_at: None,
            subscription_ends_at: None,
        });
    }
    provider_usage
}

/// The Cursor usage provider.
pub struct CursorProvider {
    vault: crate::cookie_vault::CookieVault,
    credential_source: Option<std::sync::Arc<dyn crate::credential_source::CredentialSource>>,
    handle_loader: std::sync::Arc<crate::vault_handles::VaultHandleLoader>,
    http: reqwest::Client,
    usage_url: String,
    usage_status_url: String,
    sand_usage_url: String,
    /// The last spend refusal per credential handle, so a refusal is logged
    /// when it appears, changes or clears rather than on every poll.
    spend_refusals: Mutex<HashMap<String, SpendRefusal>>,
}

impl CursorProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<std::sync::Arc<dyn crate::credential_source::CredentialSource>>,
        handle_loader: std::sync::Arc<crate::vault_handles::VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            vault: crate::cookie_vault::CookieVault::new(
                credential_source.clone(),
                std::sync::Arc::clone(&handle_loader),
                COOKIE_FAMILY,
            ),
            credential_source,
            handle_loader,
            usage_url: USAGE_URL.to_string(),
            usage_status_url: USAGE_STATUS_URL.to_string(),
            sand_usage_url: SAND_USAGE_URL.to_string(),
            spend_refusals: Mutex::new(HashMap::new()),
        }
    }

    /// Read the usage summary with `cookie`, the Grok Bot ("Sand") usage status
    /// that holds an X Subscription account's shared weekly pool, and for a
    /// unified personal account also its monthly on-demand spend.
    ///
    /// Only the summary can fail this call. The other two reads are optional
    /// requests, made side by side once the summary has answered: whatever goes
    /// wrong with either leaves the plan window, the pool list and the error
    /// class exactly as the summary alone would give them.
    async fn fetch_usage_and_spend(
        &self,
        handle_key: &str,
        cookie: String,
    ) -> Result<(Usage, Option<Pool>), FetchError> {
        let body = JsonRequest::get(&self.usage_url)
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", cookie.clone()))
            .send(&self.http)
            .await?;
        let summary = normalize_summary(&body)?;
        let unified_personal = summary.unified_personal;
        let mut usage = summary.usage;
        let spend = async {
            // Accounts on any other plan have no reading at that endpoint, so
            // they are never asked.
            if !unified_personal {
                return None;
            }
            let spend = match JsonRequest::get(&self.usage_status_url)
                .timeout(USAGE_STATUS_TIMEOUT)
                .header(Header::new("Cookie", cookie.clone()))
                .send(&self.http)
                .await
            {
                Ok(body) => on_demand_pool(&body, chrono::Utc::now()),
                Err(_) => Err(SpendRefusal::RequestFailed),
            };
            self.settle_spend(handle_key, spend)
        };
        // No plan field in the summary identifies an X Subscription account
        // (the captured one reads `enterprise`/`team`), so every account whose
        // summary succeeded is asked. The reply's own `isUnified` flag is
        // required; `shared_weekly_pool` also checks it is a current paid pool.
        let sand = async {
            let body = JsonRequest::post_json(&self.sand_usage_url, b"{}".to_vec())
                .timeout(SAND_USAGE_TIMEOUT)
                .header(Header::new("Cookie", cookie.clone()))
                .header(Header::new("Origin", SAND_USAGE_ORIGIN))
                .send(&self.http)
                .await
                .ok()?;
            shared_weekly_pool(&body, chrono::Utc::now())
        };
        let (pool, weekly) = tokio::join!(spend, sand);
        // The summary's monthly figure for these accounts is a stale cycle
        // that commonly reads 100% used; the weekly pool is the limit that
        // actually throttles them, so it takes the primary slot.
        if let Some(weekly) = weekly {
            usage.primary = Some(weekly);
        }
        Ok((usage, pool))
    }

    /// Log a handle's spend refusal when it first appears, changes, or clears,
    /// and hand back the pool to publish (none when refused). Accounts are
    /// polled repeatedly, so logging every refusal would repeat one line
    /// forever.
    fn settle_spend(&self, handle_key: &str, spend: Result<Pool, SpendRefusal>) -> Option<Pool> {
        let current = spend.as_ref().err().copied();
        let mut refusals = self
            .spend_refusals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = refusals.get(handle_key).copied();
        if previous != current {
            match current {
                Some(reason) => eprintln!(
                    "{LOG_TAG} warning: cursor monthly on-demand spend refused ({handle_key}): {}; no on_demand pool published",
                    reason.as_str()
                ),
                None => eprintln!(
                    "{LOG_TAG} cursor monthly on-demand spend readable again ({handle_key})"
                ),
            }
        }
        match current {
            Some(reason) => refusals.insert(handle_key.to_string(), reason),
            None => refusals.remove(handle_key),
        };
        spend.ok()
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

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        if self.credential_source.is_some() {
            let oauth: Vec<_> = self
                .handle_loader
                .cursor_handles()?
                .into_iter()
                .filter(|handle| {
                    handle
                        .vault_credential_id()
                        .is_some_and(|id| id.starts_with("oauth:"))
                })
                .collect();
            if !oauth.is_empty() {
                return Ok(oauth);
            }
        }
        let deposits = self.vault.handles()?;
        if !deposits.is_empty() {
            return Ok(deposits);
        }
        // No deposit of either kind: the one remaining lane is Cursor's own app
        // store on this machine. Never beside a deposit -- every lane here
        // becomes its own slot, and two slots for one account would collapse to
        // a single row by a tie-break nobody can see.
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault()
            && handle
                .vault_credential_id()
                .is_some_and(|id| id.starts_with("oauth:"))
        {
            let Some(source) = self.credential_source.as_ref() else {
                return FetchAttempt::unverified_vault_failure(
                    crate::credential_source::VaultGetError::Permanent,
                );
            };
            let mut credential = match crate::credential_source::get_vault_credential(
                source,
                handle,
                crate::credential_source::VAULT_READ_MIN_TTL_MS,
            )
            .await
            {
                Ok(credential) => credential,
                Err(error) => return FetchAttempt::unverified_vault_failure(error),
            };
            let record_version = credential.record_version;
            let account_info = credential.account_info();
            let token = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
                Ok(token) => token,
                Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
            };
            let user_id = match cursor_user_id_from_access_token(&token) {
                Ok(user_id) => user_id,
                Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
            };
            let observed = Some(crate::provider::AccountObservation::new(
                Some(user_id.clone()),
                Some(record_version),
            ));
            let cookie = app_auth_cookie_header(&user_id, &token);
            let result = self.fetch_usage_and_spend(handle.stable_id(), cookie).await;
            if let Err(error) = &result {
                crate::credential_source::report_vault_auth_failure(
                    self.credential_source.as_ref(),
                    handle,
                    record_version,
                    error,
                );
            }
            return match result {
                Ok((usage, pool)) => {
                    let mut attempt = FetchAttempt::success(observed, "vault", usage)
                        .with_account_info(account_info);
                    // No pool leaves `pools` absent rather than empty: an empty
                    // list would state the account has no pools, which a
                    // failed or skipped spend read does not establish.
                    attempt.pools = pool.map(|pool| vec![pool]);
                    attempt
                }
                Err(error) => FetchAttempt::failure(observed, Some("vault".to_string()), error)
                    .with_account_info(account_info),
            };
        }

        let result: Result<ProviderUsage, FetchError> = async {
            let (jar, source) = if handle.is_vault() {
                let (jar, source) = self.vault.jar_for(handle).await?;
                (Some(jar), source)
            } else {
                (None, APP_STORE_SOURCE)
            };
            let app_path = match app_auth_db_path() {
                Ok(path) => path,
                // A session cookie remains usable even if the optional app database
                // path needed to enrich it with account identity cannot be resolved.
                Err(_)
                    if jar.as_ref().is_some_and(|jar| {
                        jar.cookies
                            .iter()
                            .any(|cookie| is_session_cookie(&cookie.name))
                    }) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            let credential = resolve_cursor_credential_async(jar, app_path).await?;
            let cookie = credential.cookie_header();
            let (usage, pool) = self
                .fetch_usage_and_spend(handle.stable_id(), cookie)
                .await?;
            let mut provider_usage = provider_usage_from_credential(&credential, source, usage);
            provider_usage.spend = pool.map(|pool| vec![pool]);
            Ok(provider_usage)
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

    use crate::cookie_vault::SOURCE as DEPOSIT_SOURCE;

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
            cookies: vec![crate::cookie_jar::Cookie {
                name: "WorkosCursorSessionToken".to_string(),
                value: session_cookie_value.to_string(),
                host_key: "cursor.com".to_string(),
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

    #[test]
    fn macos_app_auth_store_path_uses_the_injected_home() {
        let path = app_auth_db_path_from(
            Some(PathBuf::from("/Users/synthetic")),
            |_| None,
            AppPlatform::Macos,
        )
        .unwrap();

        assert_eq!(
            path.as_deref(),
            Some(Path::new(
                "/Users/synthetic/Library/Application Support/Cursor/User/globalStorage/state.vscdb"
            ))
        );
    }

    #[test]
    fn linux_app_auth_store_path_prefers_xdg_then_home_config() {
        let xdg = app_auth_db_path_from(
            Some(PathBuf::from("/home/synthetic")),
            |key| (key == "XDG_CONFIG_HOME").then(|| std::ffi::OsString::from("/xdg")),
            AppPlatform::Linux,
        )
        .unwrap();
        let fallback = app_auth_db_path_from(
            Some(PathBuf::from("/home/synthetic")),
            |_| None,
            AppPlatform::Linux,
        )
        .unwrap();

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
    fn windows_app_auth_store_prefers_appdata_then_profile_roaming() {
        let home = test_path("profile");
        let appdata = test_path("appdata");
        let tail = Path::new("Cursor/User/globalStorage/state.vscdb");
        let path = app_auth_db_path_from(
            Some(home.clone()),
            |key| (key == "APPDATA").then(|| appdata.clone().into_os_string()),
            AppPlatform::Windows,
        )
        .unwrap();
        assert_eq!(path, Some(appdata.join(tail)));
        for appdata in [None, Some(std::ffi::OsString::new())] {
            let fallback = app_auth_db_path_from(
                Some(home.clone()),
                |key| {
                    if key == "APPDATA" {
                        appdata.clone()
                    } else {
                        None
                    }
                },
                AppPlatform::Windows,
            )
            .unwrap();
            assert_eq!(fallback, Some(home.join("AppData/Roaming").join(tail)));
        }
        assert!(app_auth_db_path_from(
            Some(home),
            |key| (key == "APPDATA").then(|| "relative-appdata".into()),
            AppPlatform::Windows
        )
        .is_err());
    }

    #[test]
    fn immutable_uri_escapes_filename_and_opens_the_database_read_only() {
        let dir = test_path("uri");
        fs::create_dir(&dir).unwrap();
        let path = dir.join("state # % unicode-é.vscdb");
        create_state_db(&path, &synthetic_jwt("user"), None);
        let uri = immutable_app_auth_uri(&path).unwrap();
        assert!(
            uri.contains("%23") && uri.contains("%25") && uri.ends_with("?immutable=1"),
            "URI must escape filename syntax and retain immutable mode: {uri}"
        );
        let auth = load_app_auth_from_path(Some(&path)).unwrap().unwrap();
        assert_eq!(auth.user_id, "user");
        let connection = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        assert!(
            connection.execute("DELETE FROM ItemTable", []).is_err(),
            "app auth reads must not enable SQLite writes"
        );
        drop(connection);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_immutable_uri_handles_drive_and_unc_paths() {
        assert_eq!(
            immutable_app_auth_uri(Path::new(r"C:\Users\someone\state #.vscdb")).unwrap(),
            "file:///C:/Users/someone/state%20%23.vscdb?immutable=1"
        );
        assert_eq!(
            immutable_app_auth_uri(Path::new(r"\\server\share\state #.vscdb")).unwrap(),
            "file:////server/share/state%20%23.vscdb?immutable=1"
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
            cookies: vec![crate::cookie_jar::Cookie {
                name: "analytics".to_string(),
                value: "synthetic".to_string(),
                host_key: "cursor.com".to_string(),
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
            cookies: vec![crate::cookie_jar::Cookie {
                name: "WorkosCursorSessionToken".to_string(),
                value: "synthetic-browser-session".to_string(),
                host_key: "cursor.com".to_string(),
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
        let provider_usage =
            provider_usage_from_credential(&credential, DEPOSIT_SOURCE, healthy_usage());

        assert_eq!(provider_usage.account.as_deref(), Some("user-id"));
        assert_eq!(provider_usage.source.as_deref(), Some(DEPOSIT_SOURCE));
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
        let provider_usage =
            provider_usage_from_credential(&credential, DEPOSIT_SOURCE, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(DEPOSIT_SOURCE));
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
        let provider_usage =
            provider_usage_from_credential(&credential, DEPOSIT_SOURCE, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(DEPOSIT_SOURCE));
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
        let provider_usage =
            provider_usage_from_credential(&credential, DEPOSIT_SOURCE, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(DEPOSIT_SOURCE));
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
        let provider_usage =
            provider_usage_from_credential(&credential, DEPOSIT_SOURCE, healthy_usage());

        assert!(
            provider_usage.usage.is_some(),
            "browser usage must still publish"
        );
        assert_eq!(provider_usage.source.as_deref(), Some(DEPOSIT_SOURCE));
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
        let provider_usage =
            provider_usage_from_credential(&credential, DEPOSIT_SOURCE, healthy_usage());
        assert_eq!(provider_usage.account.as_deref(), Some("user-id"));
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

    /// With no deposit, the one lane is Cursor's own app store on this machine.
    ///
    /// Cursor is the cookie provider with a local lane that is not a browser:
    /// `state.vscdb` is the editor's own file, so it survives the vault-only
    /// rule every other cookie provider follows.
    #[test]
    fn without_a_deposit_the_app_store_is_the_only_lane() {
        let provider = CursorProvider::new_with_handle_loader(
            None,
            std::sync::Arc::new(crate::vault_handles::VaultHandleLoader::new(None)),
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles, vec![CredentialHandle::implicit()]);
    }

    /// A cookie deposit replaces the app-store lane instead of sitting beside it.
    ///
    /// Two lanes for one account would be two slots collapsed to one row by a
    /// tie-break nobody can see; the deposit is what the operator configured.
    #[test]
    fn a_cookie_deposit_replaces_the_app_store_lane() {
        let loader = std::sync::Arc::new(crate::vault_handles::VaultHandleLoader::default());
        loader.install_rows_for_test(&[("cookie:cursor.com", "cookie")]);
        let provider = CursorProvider::new_with_handle_loader(
            Some(std::sync::Arc::new(ScopedCursorSource::default())),
            loader,
        );
        let handles = provider.handles().unwrap();
        let ids: Vec<_> = handles
            .iter()
            .map(|handle| handle.vault_credential_id())
            .collect();
        assert_eq!(ids, vec![Some("cookie:cursor.com")]);
    }

    /// The app-store lane alone reads Cursor's sign-in, and says so when absent.
    #[test]
    fn the_app_store_lane_reads_the_app_sign_in_or_reports_absence() {
        let path = test_path("app-only.vscdb");
        let access_token = synthetic_jwt("organization|user-id");
        create_state_db(&path, &access_token, None);
        let credential = resolve_app_store_credential(Some(&path)).unwrap();
        assert!(matches!(credential, CursorCredential::App(ref auth) if auth.user_id == "user-id"));
        fs::remove_file(&path).unwrap();

        let error = match resolve_app_store_credential(Some(&path)) {
            Err(error) => error,
            Ok(_) => panic!("a missing app store must not resolve a credential"),
        };
        assert!(
            matches!(error, FetchError::NoSession(ref message) if message.contains("no cursor cookie deposit")),
            "{error:?}"
        );
    }

    #[derive(Default)]
    struct ScopedCursorSource {
        calls: std::sync::Mutex<Vec<(String, u64)>>,
    }

    #[async_trait]
    impl crate::credential_source::CredentialSource for ScopedCursorSource {
        async fn get(
            &self,
            _capability: &crate::credential_source::VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<
            crate::credential_source::VaultCredential,
            crate::credential_source::VaultGetError,
        > {
            Err(crate::credential_source::VaultGetError::FailClosed)
        }

        async fn get_scoped(
            &self,
            credential_id: &str,
            min_ttl_ms: u64,
        ) -> Result<
            crate::credential_source::VaultCredential,
            crate::credential_source::VaultGetError,
        > {
            self.calls
                .lock()
                .unwrap()
                .push((credential_id.to_string(), min_ttl_ms));
            Ok(crate::credential_source::VaultCredential {
                payload: synthetic_jwt("auth0|cursor-user").into_bytes(),
                expires_at_ms: None,
                record_version: 17,
                account_id: None,
                project_id: None,
                email: None,
                org_name: None,
            })
        }

        async fn report_auth_failure(
            &self,
            _capability: &crate::credential_source::VaultCapability,
            _provider_status: u16,
            _record_version: u64,
        ) {
        }
    }

    async fn serve_cursor_usage() -> (String, tokio::sync::oneshot::Receiver<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let read = stream.read(&mut request).await.unwrap();
            request.truncate(read);
            let _ = sent.send(String::from_utf8_lossy(&request).to_string());
            let body = br#"{
                "billingCycleEnd":"2026-07-24T03:00:00Z",
                "individualUsage":{"plan":{"totalPercentUsed":45.5}}
            }"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        (format!("http://{address}/usage"), received)
    }

    /// A loopback URL nothing listens on: its port was bound and released.
    async fn closed_loopback_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{address}/sand-usage")
    }

    #[tokio::test]
    async fn oauth_cursor_fetches_scoped_jwt_and_records_sub_derived_account() {
        let loader = std::sync::Arc::new(crate::vault_handles::VaultHandleLoader::default());
        loader.install_rows_for_test(&[("oauth:cursor", "oauth"), ("cookie:cursor.com", "cookie")]);
        let source = std::sync::Arc::new(ScopedCursorSource::default());
        let mut provider = CursorProvider::new_with_handle_loader(
            Some(source.clone() as std::sync::Arc<dyn crate::credential_source::CredentialSource>),
            loader,
        );
        let handles = provider.handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].vault_credential_id(), Some("oauth:cursor"));
        let (url, request) = serve_cursor_usage().await;
        provider.usage_url = url;
        // The loopback server answers one request; the Sand read goes to a
        // closed port so it fails at once instead of reaching cursor.com.
        provider.sand_usage_url = closed_loopback_url().await;

        let attempt = provider.fetch_handle(&handles[0]).await;

        assert!(attempt.usage.is_ok());
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        assert_eq!(
            attempt
                .observed
                .as_ref()
                .and_then(|observed| observed.account_id.as_deref()),
            Some("cursor-user")
        );
        assert_eq!(
            attempt
                .observed
                .as_ref()
                .and_then(|observed| observed.record_version),
            Some(17)
        );
        assert_eq!(
            source.calls.lock().unwrap().as_slice(),
            &[("oauth:cursor".to_string(), 120_000)]
        );
        let request = request.await.unwrap();
        assert!(request.contains("WorkosCursorSessionToken=cursor-user%3A%3Aheader."));
    }

    // ---- Monthly on-demand spend for unified personal accounts ----

    /// A unified personal plan's usage summary, as Cursor sends it.
    const UNIFIED_SUMMARY: &str = r#"{
        "membershipType": "unified_ultra",
        "limitType": "user",
        "billingCycleEnd": "2026-07-24T03:00:00Z",
        "individualUsage": { "plan": { "totalPercentUsed": 45.5 } }
    }"#;

    /// A usage-status body matching a captured unified-account response, with
    /// the period dates passed in.
    fn usage_status(period_start: &str, resets_at: &str) -> String {
        format!(
            r#"{{"availability":"available","monthlyUsage":{{"usedCents":93,"limitCents":1000,"creditsUsedCents":0,"periodStart":"{period_start}","resetsAt":"{resets_at}"}}}}"#
        )
    }

    /// A usage-status body whose period contains the real present, for tests
    /// that go through the provider, which reads the wall clock.
    fn current_usage_status() -> String {
        let now = chrono::Utc::now();
        let format = "%Y-%m-%dT%H:%M:%S%.3fZ";
        usage_status(
            &(now - chrono::Duration::days(1)).format(format).to_string(),
            &(now + chrono::Duration::days(1)).format(format).to_string(),
        )
    }

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn usd(minor: i64) -> Amount {
        Amount {
            minor,
            exponent: 2,
            unit: "USD".to_string(),
        }
    }

    /// A loopback server answering the summary at `/usage-summary`, the spend
    /// read at `/usage-status` and the Sand read at `/sand-usage`, recording the
    /// head of every request it receives so a test can count them. The Sand
    /// read answers 404, which publishes no weekly pool, so these tests see the
    /// summary and spend reads alone.
    async fn serve_cursor_routes(
        summary: String,
        status_code: u16,
        status_body: String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        serve_cursor_routes_with_sand(summary, status_code, status_body, 404, String::new()).await
    }

    async fn serve_cursor_routes_with_sand(
        summary: String,
        status_code: u16,
        status_body: String,
        sand_code: u16,
        sand_body: String,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&requests);
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut chunk = [0; 4096];
                while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let request_line = head.lines().next().unwrap_or_default().to_string();
                seen.lock().unwrap().push(head);
                let (code, body) = if request_line.contains(" /usage-status") {
                    (status_code, status_body.clone())
                } else if request_line.contains(" /sand-usage") {
                    (sand_code, sand_body.clone())
                } else {
                    (200, summary.clone())
                };
                let response = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), requests)
    }

    /// The recorded requests whose request line starts with `prefix`, compared
    /// case-insensitively (for example `"get /usage-status"`).
    fn requests_starting(requests: &[String], prefix: &str) -> Vec<String> {
        requests
            .iter()
            .map(|head| head.to_ascii_lowercase())
            .filter(|head| head.starts_with(prefix))
            .collect()
    }

    /// A Cursor provider on its app-token lane, pointed at a loopback server.
    fn provider_against(base: &str) -> (CursorProvider, CredentialHandle) {
        let loader = std::sync::Arc::new(crate::vault_handles::VaultHandleLoader::default());
        loader.install_rows_for_test(&[("oauth:cursor", "oauth")]);
        let mut provider = CursorProvider::new_with_handle_loader(
            Some(std::sync::Arc::new(ScopedCursorSource::default())),
            loader,
        );
        provider.usage_url = format!("{base}/usage-summary");
        provider.usage_status_url = format!("{base}/usage-status?scope=user");
        provider.sand_usage_url = format!("{base}/sand-usage");
        let handle = provider.handles().unwrap().remove(0);
        (provider, handle)
    }

    fn recorded_refusal(
        provider: &CursorProvider,
        handle: &CredentialHandle,
    ) -> Option<SpendRefusal> {
        provider
            .spend_refusals
            .lock()
            .unwrap()
            .get(handle.stable_id())
            .copied()
    }

    /// The captured unified reading publishes limit and remainder in USD cents,
    /// with Cursor's own reset.
    #[test]
    fn captured_unified_reading_publishes_the_on_demand_pool() {
        let body = usage_status("2026-10-01T00:00:00.000Z", "2026-11-01T00:00:00.000Z");
        let pool = on_demand_pool(body.as_bytes(), fixed_now()).unwrap();
        assert_eq!(
            pool,
            Pool {
                id: "on_demand".to_string(),
                label: "On-demand spend (monthly)".to_string(),
                funding: PoolFunding::Unknown,
                remaining: Some(usd(907)),
                total: Some(usd(1000)),
                basis: PoolBasis::Reported,
                spendable: None,
                resets_at: Some("2026-11-01T00:00:00Z".to_string()),
                expires_at: None,
                unlimited: None,
            }
        );
    }

    /// Spend past the limit leaves the pool empty, never negative.
    #[test]
    fn overspent_reading_floors_remaining_at_zero() {
        let body = usage_status("2026-10-01T00:00:00Z", "2026-11-01T00:00:00Z")
            .replace("\"usedCents\":93", "\"usedCents\":1250");
        let pool = on_demand_pool(body.as_bytes(), fixed_now()).unwrap();
        assert_eq!(pool.remaining, Some(usd(0)));
        assert_eq!(pool.total, Some(usd(1000)));
    }

    /// Only a unified membership with a personal (`user`) limit qualifies; a
    /// unified team budget and every older plan do not.
    #[test]
    fn unified_personal_gate_needs_unified_membership_and_a_user_limit() {
        let gate = |membership: Option<&str>, limit: Option<&str>| {
            CursorUsageSummary {
                billing_cycle_end: None,
                individual_usage: None,
                membership_type: membership.map(str::to_string),
                limit_type: limit.map(str::to_string),
            }
            .uses_unified_personal_billing()
        };
        assert!(gate(Some("unified_ultra"), Some("user")));
        assert!(gate(Some("UNIFIED_pro"), Some("user")));
        assert!(!gate(Some("unified_ultra"), Some("team")));
        assert!(!gate(Some("unified_ultra"), None));
        assert!(!gate(Some("pro"), Some("user")));
        assert!(!gate(Some("unifiedpro"), Some("user")));
        assert!(!gate(None, Some("user")));
    }

    /// Through the provider: a unified personal account makes one extra,
    /// cookie-authenticated JSON request and publishes the pool beside the
    /// unchanged plan window.
    #[tokio::test]
    async fn a_unified_account_publishes_the_on_demand_pool_beside_the_window() {
        let (base, requests) =
            serve_cursor_routes(UNIFIED_SUMMARY.to_string(), 200, current_usage_status()).await;
        let (provider, handle) = provider_against(&base);

        let attempt = provider.fetch_handle(&handle).await;

        assert_eq!(
            attempt.usage.as_ref().unwrap(),
            &normalize_usage(UNIFIED_SUMMARY.as_bytes()).unwrap()
        );
        let pools = attempt.pools.expect("a valid reading publishes a pool");
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id, "on_demand");
        assert_eq!(pools[0].total, Some(usd(1000)));
        assert_eq!(pools[0].remaining, Some(usd(907)));
        assert_eq!(recorded_refusal(&provider, &handle), None);

        // Summary, spend and the Sand read every account gets; the spend and
        // Sand reads run side by side, so their order is not fixed.
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "{requests:?}");
        let spend = requests_starting(&requests, "get /usage-status?scope=user ");
        assert_eq!(spend.len(), 1, "{requests:?}");
        let status = &spend[0];
        assert!(status.contains("accept: application/json"), "{status}");
        assert!(
            status.contains("cookie: workoscursorsessiontoken=cursor-user%3a%3aheader."),
            "{status}"
        );
    }

    /// Any other plan never asks for the spend reading. (Every account still
    /// gets the summary and the Sand read.)
    #[tokio::test]
    async fn a_non_unified_account_makes_no_spend_request() {
        let summaries = [
            UNIFIED_SUMMARY.replace("unified_ultra", "pro"),
            UNIFIED_SUMMARY.replace("\"user\"", "\"team\""),
        ];
        for summary in summaries {
            let (base, requests) =
                serve_cursor_routes(summary.clone(), 200, current_usage_status()).await;
            let (provider, handle) = provider_against(&base);

            let attempt = provider.fetch_handle(&handle).await;

            assert!(attempt.usage.is_ok(), "{summary}");
            let requests = requests.lock().unwrap();
            assert!(
                requests_starting(&requests, "get /usage-status").is_empty(),
                "a non-unified account must make no spend request: {summary}"
            );
            assert!(requests[0].starts_with("GET /usage-summary "));
            assert!(
                attempt.pools.is_none(),
                "a non-unified account publishes no pool: {summary}"
            );
        }
    }

    /// Fetch a unified account whose spend endpoint answers `status_body` with
    /// `status_code`, and check that no pool is published, the window and
    /// error class are what the summary alone gives, and `expected` is the
    /// recorded reason.
    async fn assert_refused(status_code: u16, status_body: String, expected: SpendRefusal) {
        let (base, requests) = serve_cursor_routes(
            UNIFIED_SUMMARY.to_string(),
            status_code,
            status_body.clone(),
        )
        .await;
        let (provider, handle) = provider_against(&base);

        let attempt = provider.fetch_handle(&handle).await;

        assert_eq!(
            attempt.usage.as_ref().ok(),
            Some(&normalize_usage(UNIFIED_SUMMARY.as_bytes()).unwrap()),
            "the window must survive a refused spend reading: {status_body}"
        );
        assert!(
            attempt.pools.is_none(),
            "a refused spend reading must publish no pool: {status_body}"
        );
        assert_eq!(
            recorded_refusal(&provider, &handle),
            Some(expected),
            "{status_body}"
        );
        assert_eq!(
            requests_starting(&requests.lock().unwrap(), "get /usage-status").len(),
            1
        );
    }

    /// A reading for a period that does not contain the present (last month's
    /// or next month's) publishes no pool and leaves the window intact.
    #[tokio::test]
    async fn a_reading_outside_its_period_publishes_no_pool_and_keeps_the_window() {
        let now = chrono::Utc::now();
        let at = |offset_days: i64| {
            (now + chrono::Duration::days(offset_days))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        };
        // Already reset: the present is at or after `resetsAt`.
        assert_refused(
            200,
            usage_status(&at(-30), &at(-1)),
            SpendRefusal::OutsidePeriod,
        )
        .await;
        // Not started: the present is before `periodStart`.
        assert_refused(
            200,
            usage_status(&at(1), &at(30)),
            SpendRefusal::OutsidePeriod,
        )
        .await;
    }

    /// Every other unusable reading also publishes no pool and leaves the
    /// window and error class exactly as the summary gives them.
    #[tokio::test]
    async fn an_unusable_spend_reading_publishes_no_pool_and_keeps_the_window() {
        let valid = current_usage_status();
        let cases = [
            (
                200,
                valid.replace("\"available\"", "\"identity_unlinked\""),
                SpendRefusal::NotAvailable,
            ),
            (
                200,
                valid.replace("\"availability\":\"available\",", ""),
                SpendRefusal::NotAvailable,
            ),
            (
                200,
                r#"{"availability":"available"}"#.to_string(),
                SpendRefusal::MonthlyUsageMissing,
            ),
            (
                200,
                valid.replace("\"usedCents\":93", "\"usedCents\":-1"),
                SpendRefusal::AmountUnusable,
            ),
            (
                200,
                valid.replace("\"limitCents\":1000", "\"limitCents\":-1"),
                SpendRefusal::AmountUnusable,
            ),
            (
                200,
                valid.replace("\"limitCents\":1000", "\"limitCents\":null"),
                SpendRefusal::AmountUnusable,
            ),
            (
                200,
                valid.replace("\"usedCents\":93", "\"usedCents\":93.5"),
                SpendRefusal::AmountUnusable,
            ),
            (
                200,
                valid.replace("\"usedCents\":93", "\"usedCents\":\"93\""),
                SpendRefusal::Undecodable,
            ),
            (
                200,
                usage_status("not-a-date", "2099-01-01T00:00:00Z"),
                SpendRefusal::DateUnreadable,
            ),
            (
                200,
                usage_status("2000-01-01T00:00:00Z", "bad-date"),
                SpendRefusal::DateUnreadable,
            ),
            (200, "not json".to_string(), SpendRefusal::Undecodable),
            (403, valid.clone(), SpendRefusal::RequestFailed),
            (500, valid.clone(), SpendRefusal::RequestFailed),
        ];
        for (status_code, body, expected) in cases {
            assert_refused(status_code, body, expected).await;
        }
    }

    // ---- The X Subscription's shared weekly pool ----

    /// The usage summary from CodexBar's `CursorSandUsageTests` (CodexBar
    /// issue #4545),
    /// captured from one Enterprise account on The X Subscription. Its monthly
    /// plan reads 100% used even though the account's real limit is the
    /// weekly pool below.
    const X_SUBSCRIPTION_SUMMARY: &str = r#"{
      "billingCycleStart": "2026-10-01T00:00:00.000Z",
      "billingCycleEnd": "2026-11-01T00:00:00.000Z",
      "membershipType": "enterprise",
      "limitType": "team",
      "isUnlimited": false,
      "individualUsage": {
        "plan": {
          "enabled": true,
          "used": 4615,
          "limit": 4615,
          "remaining": 0,
          "breakdown": { "included": 4615, "bonus": 2288, "total": 6903 },
          "autoPercentUsed": 62.860238353196095,
          "apiPercentUsed": 0,
          "totalPercentUsed": 100
        },
        "onDemand": { "enabled": true, "used": 447, "limit": null, "remaining": null }
      },
      "teamUsage": {
        "onDemand": { "enabled": true, "used": 447, "limit": 0, "remaining": 0 }
      }
    }"#;

    /// The reset the captured Sand reading states.
    const CAPTURED_WEEKLY_RESET: &str = "2026-10-22T10:27:36.178Z";

    /// The Sand usage status from the same capture. `is_unified: None` drops
    /// the flag entirely; `next_reset` replaces the captured reset so tests
    /// that go through the provider, which reads the wall clock, can state one
    /// that is still in the future.
    fn x_subscription_sand_status(is_unified: Option<bool>, next_reset: &str) -> String {
        let flag = is_unified
            .map(|value| format!("\n      \"isUnified\": {value},"))
            .unwrap_or_default();
        format!(
            r#"{{
      "currentPeriodStart": "2026-10-15T10:27:36.178Z",
      "nextResetTimestampUtc": "{next_reset}",
      "usagePercent": 10.205850487540628,
      "hasAvailableUsage": true,
      "hasNonZeroIncludedLimit": true,
      "onDemandSettings": {{
        "visible": true,
        "eligible": true,
        "enabled": true,
        "dashboardUrl": "https://cursor.com/dashboard/spending?for=redacted"
      }},
      "grokPlanLabel": "Grok Bot Plan",
      "cursorPlanName": "Premium",{flag}
      "billingBrand": "SAND_BILLING_BRAND_GROK",
      "unifiedBillingUrl": "https://cursor.com/dashboard?tab=billing",
      "unifiedUpsellButtons": [
        {{ "label": "Buy Credits", "url": {{ "url": "https://cursor.com/dashboard?tab=billing" }} }}
      ]
    }}"#
        )
    }

    /// The moment CodexBar's test evaluates the captured reading at.
    fn capture_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-10-16T06:50:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    /// A weekly reset three days after the real present, in Cursor's format.
    fn current_weekly_reset() -> String {
        (chrono::Utc::now() + chrono::Duration::days(3))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    }

    /// The weekly window the captured reading describes, with `resets_at` as
    /// given.
    fn weekly_window(resets_at: &str) -> RateWindow {
        RateWindow {
            window_kind: None,
            used_percent: 10.205850487540628,
            raw_used_percent: None,
            resets_at: Some(resets_at.to_string()),
            window_minutes: Some(10_080),
            used_count: None,
            total_count: None,
            regeneration: None,
            breakdown: None,
        }
    }

    /// The captured unified reading is a paid pool with a future reset, so it
    /// is the shared weekly pool: about 10% used, seven days long, resetting
    /// when Cursor says. The reply names no period, so no `window_kind`.
    #[test]
    fn the_captured_x_subscription_reading_is_the_shared_weekly_pool() {
        let body = x_subscription_sand_status(Some(true), CAPTURED_WEEKLY_RESET);
        assert_eq!(
            shared_weekly_pool(body.as_bytes(), capture_now()),
            Some(weekly_window(CAPTURED_WEEKLY_RESET))
        );
    }

    /// Without `isUnified: true` the same reading is only the Grok Bot
    /// allowance, never the account's pool.
    #[test]
    fn a_sand_reading_without_the_unified_flag_is_no_weekly_pool() {
        for flag in [Some(false), None] {
            let body = x_subscription_sand_status(flag, CAPTURED_WEEKLY_RESET);
            assert_eq!(
                shared_weekly_pool(body.as_bytes(), capture_now()),
                None,
                "a reading without the unified flag must not become the weekly pool: {flag:?}"
            );
        }
    }

    /// A unified reading still has to be a paid allowance with a percentage
    /// and a reset that has not passed; a trial, a missing figure or a stale
    /// reset leaves the monthly window in place.
    #[test]
    fn unified_readings_that_are_not_a_current_paid_pool_are_ignored() {
        let unified = x_subscription_sand_status(Some(true), CAPTURED_WEEKLY_RESET);
        let at = |body: String| shared_weekly_pool(body.as_bytes(), capture_now());
        // The paid-allowance fields, alone and together (`includedLimitZero`
        // is the newer field and wins), and a trial with no paid allowance.
        let no_allowance = unified.replace("\"hasNonZeroIncludedLimit\": true,", "");
        assert_eq!(at(no_allowance.clone()), None, "no allowance field");
        assert_eq!(
            at(no_allowance.replace(
                "\"hasAvailableUsage\": true,",
                "\"includedLimitZero\": false,"
            )),
            Some(weekly_window(CAPTURED_WEEKLY_RESET)),
            "includedLimitZero: false alone is a paid allowance"
        );
        assert_eq!(
            at(unified.replace(
                "\"hasNonZeroIncludedLimit\": true,",
                "\"hasNonZeroIncludedLimit\": true, \"includedLimitZero\": true,"
            )),
            None,
            "includedLimitZero: true overrides hasNonZeroIncludedLimit"
        );
        assert_eq!(
            at(unified.replace(
                "\"hasNonZeroIncludedLimit\": true,",
                "\"includedLimitZero\": true, \"sandTrialExpiresAt\": \"2026-10-30T00:00:00Z\","
            )),
            None,
            "an unexpired trial is not a recurring weekly pool"
        );
        assert_eq!(
            at(unified.replace("\"usagePercent\": 10.205850487540628,", "")),
            None,
            "no percentage"
        );
        // The reset: absent, unreadable, already passed (CodexBar's own two
        // cases are the empty string and a 2020 date), and exactly now.
        for reset in [
            "",
            "not-a-date",
            "2020-01-01T00:00:00Z",
            "2026-10-16T06:50:00Z",
        ] {
            assert_eq!(
                at(x_subscription_sand_status(Some(true), reset)),
                None,
                "reset {reset:?}"
            );
        }
        assert_eq!(
            at(unified.replace(
                &format!("\"nextResetTimestampUtc\": \"{CAPTURED_WEEKLY_RESET}\","),
                ""
            )),
            None,
            "no reset"
        );
        assert_eq!(at("not json".to_string()), None);
        // A percentage past the ends is clamped like the plan's.
        assert_eq!(
            at(unified.replace("10.205850487540628", "140")).map(|window| window.used_percent),
            Some(100.0)
        );
    }

    /// Through the provider: an X Subscription account's weekly pool replaces
    /// the stale 100% monthly window, read with one cookie-authenticated JSON
    /// POST that carries the dashboard's origin. The account is not a unified
    /// personal one, so it makes no monthly spend request and publishes no
    /// on-demand pool.
    #[tokio::test]
    async fn an_x_subscription_account_publishes_the_weekly_pool_as_primary() {
        let reset = current_weekly_reset();
        let (base, requests) = serve_cursor_routes_with_sand(
            X_SUBSCRIPTION_SUMMARY.to_string(),
            200,
            current_usage_status(),
            200,
            x_subscription_sand_status(Some(true), &reset),
        )
        .await;
        let (provider, handle) = provider_against(&base);

        let attempt = provider.fetch_handle(&handle).await;

        let expected_reset = chrono::DateTime::parse_from_rfc3339(&reset)
            .unwrap()
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
        assert_eq!(
            attempt.usage.as_ref().unwrap(),
            &Usage {
                primary: Some(weekly_window(&expected_reset)),
                secondary: None,
                tertiary: None,
                extra_rate_windows: None,
            }
        );
        assert!(attempt.pools.is_none());

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "{requests:?}");
        let sand = requests_starting(&requests, "post /sand-usage ");
        assert_eq!(sand.len(), 1, "{requests:?}");
        let sand = &sand[0];
        for header in [
            "accept: application/json",
            "content-type: application/json",
            "content-length: 2",
            "origin: https://cursor.com",
            "cookie: workoscursorsessiontoken=cursor-user%3a%3aheader.",
        ] {
            assert!(sand.contains(header), "missing {header}: {sand}");
        }
    }

    /// Accounts the Sand reading does not flag as unified -- or whose Sand
    /// read fails or is not a current paid pool -- publish exactly what they
    /// published before that read existed, byte for byte, on both the captured
    /// X Subscription summary and this module's existing summary fixtures.
    #[tokio::test]
    async fn an_account_without_the_unified_flag_keeps_its_monthly_window_byte_for_byte() {
        let reset = current_weekly_reset();
        let summaries = [
            X_SUBSCRIPTION_SUMMARY.to_string(),
            UNIFIED_SUMMARY.to_string(),
            UNIFIED_SUMMARY.replace("unified_ultra", "pro"),
        ];
        let sand_replies = [
            (200, x_subscription_sand_status(Some(false), &reset)),
            (200, x_subscription_sand_status(None, &reset)),
            (
                200,
                x_subscription_sand_status(Some(true), "2020-01-01T00:00:00Z"),
            ),
            (200, "not json".to_string()),
            (500, x_subscription_sand_status(Some(true), &reset)),
            (403, x_subscription_sand_status(Some(true), &reset)),
        ];
        for summary in &summaries {
            let today =
                serde_json::to_string(&normalize_usage(summary.as_bytes()).unwrap()).unwrap();
            for (sand_code, sand_body) in &sand_replies {
                let (base, _requests) = serve_cursor_routes_with_sand(
                    summary.clone(),
                    200,
                    current_usage_status(),
                    *sand_code,
                    sand_body.clone(),
                )
                .await;
                let (provider, handle) = provider_against(&base);

                let attempt = provider.fetch_handle(&handle).await;

                let published = serde_json::to_string(attempt.usage.as_ref().unwrap()).unwrap();
                assert_eq!(
                    published, today,
                    "an account without the unified flag must keep today's monthly window: \
                     Sand {sand_code} {sand_body}"
                );
            }
        }
        // Pinned literally as well, so the comparison cannot pass by both sides
        // changing together: the captured summary's monthly window.
        assert_eq!(
            normalize_usage(X_SUBSCRIPTION_SUMMARY.as_bytes()).unwrap(),
            Usage {
                primary: Some(RateWindow {
                    window_kind: None,
                    used_percent: 100.0,
                    raw_used_percent: None,
                    resets_at: Some("2026-11-01T00:00:00Z".to_string()),
                    window_minutes: Some(43200),
                    used_count: None,
                    total_count: None,
                    regeneration: None,
                    breakdown: None,
                }),
                secondary: None,
                tertiary: None,
                extra_rate_windows: None,
            }
        );
    }
}
