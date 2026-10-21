//! Vercel AI Gateway personal budget — a spend window with a period and a reset.
//!
//! - Endpoints, on `https://api.vercel.com` only, bearer Vercel access token:
//!   `GET /v2/user` for the token's user id, then
//!   `GET /ai-gateway/budgets/list?teamId=<team>&scopeType=user&limit=200`,
//!   following `nextCursor` for at most 20 pages.
//! - Reference: CodexBar v0.79.0
//!   `Sources/CodexBarCore/Resources/Plugins/vercel-budget.js` (new in v0.78.0,
//!   fixed in v0.79.0) and `docs/vercel.md`, which cite Vercel's budgets and
//!   list-budgets documentation. Tested against fixtures shaped from those
//!   sources and upstream's `VercelBudgetPluginTests.swift` only; no Vercel
//!   account was available when this was written.
//!
//! A budget belongs to this user when its `scopeType` is `user` and its
//! `scopeId` is the user's id or that id with a `usr_` prefix. Upstream's first
//! release matched the bare id only, so every user saw "no personal budget";
//! v0.79.0 added the prefixed form, and both are accepted here. Only active,
//! unarchived budgets count, and a custom budget (no `source`) wins over one
//! inherited with `source: "default"`. More than one applicable budget, flags
//! that are not Booleans, a `source` other than `default`, repeated or invalid
//! cursors and a 21st page all fail the reading rather than publish a guess.
//!
//! The window is `currentSpend / limitAmount`. `refreshPeriod` sets the reset at
//! Vercel's documented UTC boundaries: `daily` at the next midnight, `weekly` on
//! the next Monday, `monthly` on the 1st of next month. A `none` budget is
//! cumulative: it never resets, so it is published with no reset, no length and
//! no period name, never as some period it is not. A month has no fixed length,
//! so a monthly window carries `windowKind: monthly` and no `windowMinutes`, as
//! the wire contract asks (upstream shows the current month's minutes).
//!
//! No budget for this user is `no_quota_reported`, never a 0% window: Vercel
//! saying nothing is limited is not Vercel saying nothing was spent. The
//! remaining allowance is not what can be spent either: team, project and key
//! budgets, or the team's credit balance, can stop requests first.
//!
//! WHY A PROVIDER OF ITS OWN, beside `vercel`: that provider reads the team's
//! credit balance with an AI Gateway key, a different credential for a
//! different host, and its answer carries no account identity. Two handles of
//! one provider where one has no identity collapse into one unlabelled entry,
//! so the two readings could not both be published under one name. This one
//! carries no models.dev slug: slugs are unique per provider and `vercel`
//! holds it.
//!
//! Credential: a vault deposit under `apikey:vercel-budget` whose payload is the
//! JSON object `{"token": "<access token>", "team_id": "<team id or slug>"}`,
//! otherwise `VERCEL_ACCESS_TOKEN` together with `VERCEL_TEAM_ID`, the
//! variables upstream documents.

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

use crate::credential_source::CredentialSource;
use crate::http::JsonRequest;
use crate::model::{AccountInfo, RateWindow, Usage};
use crate::provider::{
    AccountObservation, CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider,
};
use crate::vault_handles::VaultHandleLoader;
use crate::vercel::{default_env, EnvLookup, VercelHosts, REQUEST_TIMEOUT};

pub const PROVIDER_NAME: &str = "vercel-budget";
const USER_PATH: &str = "/v2/user";
const BUDGETS_PATH: &str = "/ai-gateway/budgets/list";
const TOKEN_ENV: &str = "VERCEL_ACCESS_TOKEN";
const TEAM_ENV: &str = "VERCEL_TEAM_ID";
/// Upstream reads at most 20 pages of budgets and fails on a 21st.
const MAX_PAGES: usize = 20;
const DAY_MINUTES: i64 = 24 * 60;
const WEEK_MINUTES: i64 = 7 * DAY_MINUTES;

/// The access token and the team whose budgets it reads.
struct BudgetCredential {
    token: String,
    team: String,
}

/// Upstream's team-id rule: 1 to 128 letters, digits, `_` or `-`. Checked
/// before any request, because the team id is placed in a URL.
fn checked_team(team: &str) -> Result<String, FetchError> {
    let team = team.trim();
    let valid = (1..=128).contains(&team.len())
        && team
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if !valid {
        return Err(FetchError::CredentialUnusable(
            "the Vercel team id must be 1-128 letters, digits, `_` or `-`".to_string(),
        ));
    }
    Ok(team.to_string())
}

/// The credential inside an `apikey:vercel-budget` deposit.
///
/// The errors name what is missing and never quote the payload, which holds
/// the token.
fn credential_from_payload(payload: &str) -> Result<BudgetCredential, FetchError> {
    let unusable = |what: &str| {
        FetchError::CredentialUnusable(format!(
            "the apikey:vercel-budget deposit must be JSON {{\"token\", \"team_id\"}}: {what}"
        ))
    };
    let value: Value =
        serde_json::from_str(payload.trim()).map_err(|_| unusable("it is not JSON"))?;
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let token = field("token").ok_or_else(|| unusable("no token"))?;
    let team = field("team_id").ok_or_else(|| unusable("no team_id"))?;
    Ok(BudgetCredential {
        token,
        team: checked_team(&team)?,
    })
}

/// The token's user, from `GET /v2/user`.
#[derive(Debug, Clone, PartialEq)]
struct User {
    id: String,
    email: Option<String>,
}

/// Decode failures here never quote the body: an unexpected answer is not
/// something to publish.
fn decode(what: &str) -> FetchError {
    FetchError::Decode(format!("vercel budget: {what}"))
}

fn parse_user(body: &[u8]) -> Result<User, FetchError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| decode("the user answer is not JSON"))?;
    let user = value.get("user");
    let id = user
        .and_then(|user| user.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| decode("the user answer has no user.id"))?;
    let email = user
        .and_then(|user| user.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_string);
    Ok(User {
        id: id.to_string(),
        email,
    })
}

/// Whether a budget's scope names this user: the bare id, or the id with
/// Vercel's `usr_` prefix (the form upstream's v0.79.0 fix added).
fn names_user(budget: &Value, user_id: &str) -> bool {
    if budget.get("scopeType").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(scope_id) = budget.get("scopeId").and_then(Value::as_str) else {
        return false;
    };
    scope_id == user_id || scope_id.strip_prefix("usr_") == Some(user_id)
}

/// Read one page: push this user's active, unarchived budgets onto `matches`
/// and return the next cursor, if the page names one.
fn scan_page(
    body: &[u8],
    user_id: &str,
    matches: &mut Vec<Value>,
) -> Result<Option<String>, FetchError> {
    let page: Value =
        serde_json::from_slice(body).map_err(|_| decode("a budgets page is not JSON"))?;
    let Some(budgets) = page.get("budgets").and_then(Value::as_array) else {
        return Err(decode("a budgets page has no budgets list"));
    };
    for budget in budgets {
        // Other users' budgets are skipped unread, as upstream does.
        if !names_user(budget, user_id) {
            continue;
        }
        let (Some(active), Some(archived)) = (
            budget.get("active").and_then(Value::as_bool),
            budget.get("archived").and_then(Value::as_bool),
        ) else {
            // A budget of ours with unreadable flags cannot be ruled in or out,
            // so it must not read as "no budget configured".
            return Err(decode(
                "a budget for this user has non-Boolean active/archived",
            ));
        };
        match budget.get("source") {
            None => {}
            Some(Value::String(source)) if source == "default" => {}
            Some(_) => return Err(decode("a budget for this user has an unknown source")),
        }
        if active && !archived {
            matches.push(budget.clone());
        }
    }
    match page.get("nextCursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(cursor)) if !cursor.is_empty() => Ok(Some(cursor.clone())),
        Some(_) => Err(decode("a budgets page has an invalid nextCursor")),
    }
}

/// The one budget that applies: a custom budget over an inherited default.
fn select_budget(matches: Vec<Value>) -> Result<Option<Value>, FetchError> {
    let (custom, inherited): (Vec<Value>, Vec<Value>) = matches
        .into_iter()
        .partition(|budget| budget.get("source").is_none());
    let mut effective = if custom.is_empty() { inherited } else { custom };
    if effective.len() > 1 {
        return Err(decode("more than one budget applies to this user"));
    }
    Ok(effective.pop())
}

fn midnight(date: NaiveDate) -> Option<String> {
    let at = date.and_hms_opt(0, 0, 0)?.and_utc();
    crate::env::epoch_to_iso8601(at.timestamp())
}

/// Build the window for the applicable budget at `now`.
fn budget_window(budget: &Value, now: DateTime<Utc>) -> Result<RateWindow, FetchError> {
    let number = |name: &str| {
        budget
            .get(name)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
    };
    let limit = number("limitAmount")
        .filter(|limit| *limit > 0.0)
        .ok_or_else(|| decode("the budget's limitAmount is not a positive number"))?;
    let spend = number("currentSpend")
        .filter(|spend| *spend >= 0.0)
        .ok_or_else(|| decode("the budget's currentSpend is not a non-negative number"))?;

    let today = now.date_naive();
    let (resets_at, window_minutes, window_kind) =
        match budget.get("refreshPeriod").and_then(Value::as_str) {
            Some("daily") => (
                today.succ_opt().and_then(midnight),
                Some(DAY_MINUTES),
                Some(cortexkit_provider_usage::window_kind::DAILY),
            ),
            Some("weekly") => {
                let monday =
                    today - chrono::Days::new(u64::from(today.weekday().num_days_from_monday()));
                (
                    midnight(monday + chrono::Days::new(7)),
                    Some(WEEK_MINUTES),
                    Some(cortexkit_provider_usage::window_kind::WEEKLY),
                )
            }
            Some("monthly") => {
                let (year, month) = if today.month() == 12 {
                    (today.year() + 1, 1)
                } else {
                    (today.year(), today.month() + 1)
                };
                (
                    NaiveDate::from_ymd_opt(year, month, 1).and_then(midnight),
                    None,
                    Some(cortexkit_provider_usage::window_kind::MONTHLY),
                )
            }
            // A cumulative budget: no period, so no reset, no length and no
            // period name. Giving it any of those would tell a consumer the
            // allowance comes back when it never does.
            Some("none") => (None, None, None),
            _ => {
                return Err(decode(
                    "the budget's refreshPeriod is not one Vercel documents",
                ))
            }
        };
    Ok(RateWindow {
        used_percent: (spend / limit * 100.0).clamp(0.0, 100.0),
        raw_used_percent: None,
        resets_at,
        window_minutes,
        window_kind: window_kind.map(str::to_string),
        // Dollars, not a count of things, so neither count field applies.
        used_count: None,
        total_count: None,
        regeneration: None,
        breakdown: None,
    })
}

pub struct VercelBudgetProvider {
    hosts: VercelHosts,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    env: EnvLookup,
    now: fn() -> DateTime<Utc>,
}

impl VercelBudgetProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            hosts: VercelHosts::production(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
            env: default_env(),
            now: Utc::now,
        }
    }

    async fn get(&self, url: &str, token: &str, vault: bool) -> Result<Vec<u8>, FetchError> {
        let request = JsonRequest::get(url).bearer(token).timeout(REQUEST_TIMEOUT);
        // Only the vault lane keeps the numeric status, which the shared gate
        // needs to report a 401 to the vault; the environment has nobody to tell.
        if vault {
            request
                .send_provider_status_first(&self.http, PROVIDER_NAME)
                .await
                .map(|response| response.body)
        } else {
            request.send(&self.http).await
        }
    }

    /// The token's user, and this user's window (`None`: no budget applies).
    ///
    /// The user is returned beside the result so a later failure still shows
    /// which account it was for.
    async fn read(
        &self,
        credential: &BudgetCredential,
        vault: bool,
    ) -> (Option<User>, Result<Option<RateWindow>, FetchError>) {
        let user_url = format!("{}{USER_PATH}", self.hosts.api);
        let user = match self
            .get(&user_url, &credential.token, vault)
            .await
            .and_then(|body| parse_user(&body))
        {
            Ok(user) => user,
            Err(error) => return (None, Err(error.stage("v2/user"))),
        };
        let result = self.budget_for(&user, credential, vault).await;
        (Some(user), result)
    }

    async fn budget_for(
        &self,
        user: &User,
        credential: &BudgetCredential,
        vault: bool,
    ) -> Result<Option<RateWindow>, FetchError> {
        let base = url::Url::parse(&format!("{}{BUDGETS_PATH}", self.hosts.api))
            .map_err(|error| FetchError::Internal(format!("vercel budget url: {error}")))?;
        let mut matches = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor: Option<String> = None;
        for page in 0..MAX_PAGES {
            let mut url = base.clone();
            {
                let mut query = url.query_pairs_mut();
                // The same team on every page, as upstream sends it.
                query
                    .append_pair("teamId", &credential.team)
                    .append_pair("scopeType", "user")
                    .append_pair("limit", "200");
                if let Some(cursor) = &cursor {
                    query.append_pair("cursor", cursor);
                }
            }
            let body = self
                .get(url.as_str(), &credential.token, vault)
                .await
                .map_err(|error| error.stage("budgets/list"))?;
            let Some(next) = scan_page(&body, &user.id, &mut matches)? else {
                return select_budget(matches)?
                    .map(|budget| budget_window(&budget, (self.now)()))
                    .transpose();
            };
            // A repeated cursor would loop forever, and a last page that still
            // points onward means the list was not read whole.
            if !seen.insert(next.clone()) || page + 1 >= MAX_PAGES {
                return Err(decode(
                    "the budgets list repeats a cursor or runs past 20 pages",
                ));
            }
            cursor = Some(next);
        }
        Err(decode("the budgets list runs past 20 pages"))
    }

    async fn fetch_with(
        &self,
        credential: BudgetCredential,
        source: &str,
        record_version: Option<u64>,
        vault: bool,
    ) -> (FetchAttempt, Option<FetchError>) {
        let (user, result) = self.read(&credential, vault).await;
        let observed = user
            .as_ref()
            .map(|user| AccountObservation::new(Some(user.id.clone()), record_version));
        let info = user.as_ref().map(|user| AccountInfo {
            email: user.email.clone(),
            org_name: Some(credential.team.clone()),
            plan_type: None,
            subscription_renews_at: None,
            subscription_ends_at: None,
        });
        match result {
            Ok(Some(window)) => {
                let usage = Usage {
                    primary: Some(window),
                    secondary: None,
                    tertiary: None,
                    extra_rate_windows: None,
                };
                (
                    FetchAttempt::success(observed, source, usage).with_account_info(info),
                    None,
                )
            }
            Ok(None) => (
                FetchAttempt::failure(
                    observed,
                    Some(source.to_string()),
                    FetchError::NoQuotaReported(format!(
                        "no active personal budget is set for this user in team {}",
                        credential.team
                    )),
                )
                .with_account_info(info),
                None,
            ),
            Err(error) => {
                // Kept for the auth-failure gate, which reads only the status.
                let reportable = match &error {
                    FetchError::ProviderStatus(code, _) => {
                        Some(FetchError::ProviderStatus(*code, String::new()))
                    }
                    _ => None,
                };
                (
                    FetchAttempt::failure(observed, Some(source.to_string()), error)
                        .with_account_info(info),
                    reportable,
                )
            }
        }
    }

    async fn fetch_vault(&self, handle: &CredentialHandle) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut served = match crate::credential_source::get_vault_credential(
            credential_source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
            Ok(served) => served,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = served.record_version;
        let credential = match crate::credential_source::take_utf8_payload(&mut served.payload)
            .and_then(|payload| credential_from_payload(&payload))
        {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
        };
        let (attempt, reportable) = self
            .fetch_with(credential, "vault", Some(record_version), true)
            .await;
        if let Some(error) = reportable {
            crate::credential_source::report_vault_auth_failure(
                Some(credential_source),
                handle,
                record_version,
                &error,
            );
        }
        attempt
    }

    fn env_credential(&self) -> Result<BudgetCredential, FetchError> {
        match ((self.env)(TOKEN_ENV), (self.env)(TEAM_ENV)) {
            (Some(token), Some(team)) => Ok(BudgetCredential {
                token,
                team: checked_team(&team)?,
            }),
            // A token was configured, so this is not an absent credential: it
            // cannot be used without the team whose budgets it reads.
            (Some(_), None) => Err(FetchError::CredentialUnusable(format!(
                "{TOKEN_ENV} is set without {TEAM_ENV}"
            ))),
            (None, _) => Err(FetchError::NoSession(format!(
                "no {TOKEN_ENV} and no apikey:vercel-budget vault deposit"
            ))),
        }
    }
}

#[async_trait]
impl UsageProvider for VercelBudgetProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.vercel_budget_handles()?;
            if !vault.is_empty() {
                // A deposit replaces the environment lane, as for every apikey
                // family: two lanes would be two slots for one account.
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }
        match self.env_credential() {
            Ok(credential) => self.fetch_with(credential, "api", None, false).await.0,
            Err(error) => FetchAttempt::failure(None, None, error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vercel::testkit::{credential, routes, Log, MockSource};
    use crate::vercel::VercelProvider;
    use std::collections::HashMap;

    // Fixtures follow the shapes in upstream's `VercelBudgetPluginTests.swift`:
    // the same user, team, budget fields and page envelope.
    const USER: &str = r#"{"user":{"id":"fixtureUser","email":"fixture@example.com"}}"#;
    const TEAM: &str = "team_fixture";
    const TOKEN: &str = "fixture-token";

    /// Wednesday 2026-10-14 12:00 UTC, upstream's fixed test time.
    fn wednesday() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn budget(
        scope_id: &str,
        spend: &str,
        limit: &str,
        period: &str,
        source: Option<&str>,
    ) -> String {
        let source = source
            .map(|source| format!(r#","source":"{source}""#))
            .unwrap_or_default();
        format!(
            r#"{{"scopeType":"user","scopeId":"{scope_id}","currentSpend":{spend},"limitAmount":{limit},"refreshPeriod":"{period}","active":true,"archived":false{source}}}"#
        )
    }

    /// Upstream's default budget: the user's `usr_`-prefixed id, $30 of $120.
    fn own(period: &str) -> String {
        budget("usr_fixtureUser", "30", "120", period, None)
    }

    fn page(budgets: &[String], next: Option<&str>) -> String {
        let next = next
            .map(|next| format!(r#","nextCursor":"{next}""#))
            .unwrap_or_default();
        format!(r#"{{"budgets":[{}]{next}}}"#, budgets.join(","))
    }

    fn deposit() -> String {
        format!(r#"{{"token":"{TOKEN}","team_id":"{TEAM}"}}"#)
    }

    fn vault_handle() -> CredentialHandle {
        CredentialHandle::scoped("apikey:vercel-budget", "apikey")
    }

    /// An api.vercel.com stand-in: `/v2/user`, then budget pages keyed by the
    /// request's cursor (`""` for the first page). Anything else is a 404.
    async fn api(pages: HashMap<String, String>) -> (String, Log) {
        routes(move |path, query| {
            if path == USER_PATH {
                return (200, USER.as_bytes().to_vec());
            }
            if path == BUDGETS_PATH {
                let cursor = query
                    .iter()
                    .find(|(key, _)| key == "cursor")
                    .map(|(_, value)| value.as_str())
                    .unwrap_or("");
                if let Some(body) = pages.get(cursor) {
                    return (200, body.as_bytes().to_vec());
                }
            }
            (404, b"{}".to_vec())
        })
        .await
    }

    fn pages(entries: &[(&str, String)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(cursor, body)| (cursor.to_string(), body.clone()))
            .collect()
    }

    fn provider(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
        hosts: VercelHosts,
        env: &[(&str, &str)],
    ) -> VercelBudgetProvider {
        let mut provider = VercelBudgetProvider::new_with_handle_loader(source, loader);
        provider.hosts = hosts;
        provider.now = wednesday;
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        provider.env = Arc::new(move |key| {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        });
        provider
    }

    fn hosts(api: &str) -> VercelHosts {
        VercelHosts {
            // Nothing listens here: a request to it fails rather than lands.
            gateway: "http://127.0.0.1:9".to_string(),
            api: api.to_string(),
        }
    }

    /// Read through the vault lane against `pages`, with the deposit above.
    async fn fetch(pages: HashMap<String, String>) -> (FetchAttempt, Log) {
        let (base, log) = api(pages).await;
        let (source, _) = MockSource::new(&[("apikey:vercel-budget", credential(&deposit(), 5))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            hosts(&base),
            &[],
        );
        (provider.fetch_handle(&vault_handle()).await, log)
    }

    fn primary(attempt: &FetchAttempt) -> RateWindow {
        attempt
            .usage
            .as_ref()
            .unwrap_or_else(|error| panic!("expected a window, got {error:?}"))
            .primary
            .clone()
            .expect("a budget publishes a primary window")
    }

    fn class(attempt: &FetchAttempt) -> &'static str {
        match &attempt.usage {
            Ok(usage) => panic!("expected a failure, got {usage:?}"),
            Err(error) => error.error_class(),
        }
    }

    #[test]
    fn the_fixed_time_is_a_wednesday() {
        assert_eq!(wednesday().weekday(), chrono::Weekday::Wed);
    }

    /// Upstream's first test: across two pages, another user's budget is
    /// skipped and the custom budget on page two beats the inherited default on
    /// page one.
    #[tokio::test]
    async fn the_own_custom_budget_wins_across_pages_and_other_users_are_ignored() {
        let first = page(
            &[
                budget("user_other", "1", "2", "monthly", None),
                budget("usr_fixtureUser", "5", "50", "monthly", Some("default")),
            ],
            Some("page 2"),
        );
        let (attempt, log) = fetch(pages(&[
            ("", first),
            ("page 2", page(&[own("monthly")], None)),
        ]))
        .await;
        let window = primary(&attempt);
        assert_eq!(window.used_percent, 25.0);
        assert_eq!(window.resets_at.as_deref(), Some("2026-11-01T00:00:00Z"));
        assert_eq!(window.window_kind.as_deref(), Some("monthly"));
        assert_eq!(window.window_minutes, None, "a month has no fixed length");
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        let observed = attempt.observed.as_ref().unwrap();
        assert_eq!(observed.account_id.as_deref(), Some("fixtureUser"));
        assert_eq!(observed.record_version, Some(5));
        let info = attempt.account_info.as_ref().unwrap();
        assert_eq!(info.email.as_deref(), Some("fixture@example.com"));
        assert_eq!(info.org_name.as_deref(), Some(TEAM));

        let requests = log.lock().unwrap().clone();
        let paths: Vec<&str> = requests
            .iter()
            .map(|request| request.path.as_str())
            .collect();
        assert_eq!(paths, [USER_PATH, BUDGETS_PATH, BUDGETS_PATH]);
        for request in &requests {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer fixture-token")
            );
        }
        let query = |index: usize, key: &str| {
            requests[index]
                .query
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        for index in [1, 2] {
            assert_eq!(query(index, "teamId").as_deref(), Some(TEAM));
            assert_eq!(query(index, "scopeType").as_deref(), Some("user"));
            assert_eq!(query(index, "limit").as_deref(), Some("200"));
        }
        assert_eq!(query(1, "cursor"), None);
        assert_eq!(query(2, "cursor").as_deref(), Some("page 2"));
    }

    /// The v0.79.0 fix: Vercel names a user's budget scope `usr_<id>`, and the
    /// bare id upstream matched first is still accepted. A scope that merely
    /// contains the id is not the user's.
    #[test]
    fn a_budget_names_the_user_by_bare_or_usr_prefixed_id_only() {
        let scope = |id: &str| -> Value {
            serde_json::from_str(&budget(id, "30", "120", "monthly", None)).unwrap()
        };
        assert!(names_user(&scope("usr_fixtureUser"), "fixtureUser"));
        assert!(names_user(&scope("fixtureUser"), "fixtureUser"));
        for other in [
            "user_fixtureUser",
            "fixtureUser_usr",
            "usr_",
            "usr_usr_fixtureUser",
            "fixtureuser",
        ] {
            assert!(!names_user(&scope(other), "fixtureUser"), "{other}");
        }
        let team_scope: Value = serde_json::from_str(
            r#"{"scopeType":"team","scopeId":"fixtureUser","active":true,"archived":false}"#,
        )
        .unwrap();
        assert!(!names_user(&team_scope, "fixtureUser"));
    }

    #[tokio::test]
    async fn a_usr_prefixed_budget_is_read_through_the_provider() {
        let (attempt, _) = fetch(pages(&[("", page(&[own("monthly")], None))])).await;
        assert_eq!(primary(&attempt).used_percent, 25.0);
    }

    #[tokio::test]
    async fn a_native_user_scope_id_is_still_read() {
        let body = page(&[budget("fixtureUser", "30", "120", "monthly", None)], None);
        let (attempt, _) = fetch(pages(&[("", body)])).await;
        assert_eq!(primary(&attempt).used_percent, 25.0);
    }

    /// Vercel's documented UTC boundaries, at upstream's Wednesday.
    #[test]
    fn each_period_resets_at_its_documented_utc_boundary() {
        for (period, reset, minutes, kind) in [
            ("daily", "2026-10-15T00:00:00Z", Some(DAY_MINUTES), "daily"),
            (
                "weekly",
                "2026-10-19T00:00:00Z",
                Some(WEEK_MINUTES),
                "weekly",
            ),
            ("monthly", "2026-11-01T00:00:00Z", None, "monthly"),
        ] {
            let budget: Value = serde_json::from_str(&own(period)).unwrap();
            let window = budget_window(&budget, wednesday()).unwrap();
            assert_eq!(window.resets_at.as_deref(), Some(reset), "{period}");
            assert_eq!(window.window_minutes, minutes, "{period}");
            assert_eq!(window.window_kind.as_deref(), Some(kind), "{period}");
        }
    }

    /// The edges of each calculation: a week read on its own Monday and on its
    /// Sunday, and a month that rolls into the next year.
    #[test]
    fn period_boundaries_hold_at_the_edges_of_a_week_and_a_year() {
        let at = |text: &str| {
            DateTime::parse_from_rfc3339(text)
                .unwrap()
                .with_timezone(&Utc)
        };
        let reset = |period: &str, now: &str| {
            let budget: Value = serde_json::from_str(&own(period)).unwrap();
            budget_window(&budget, at(now)).unwrap().resets_at.unwrap()
        };
        assert_eq!(
            reset("weekly", "2026-10-19T00:00:00Z"),
            "2026-10-26T00:00:00Z"
        );
        assert_eq!(
            reset("weekly", "2026-10-25T23:59:59Z"),
            "2026-10-26T00:00:00Z"
        );
        assert_eq!(
            reset("monthly", "2026-12-31T23:00:00Z"),
            "2027-01-01T00:00:00Z"
        );
        assert_eq!(
            reset("daily", "2026-12-31T23:00:00Z"),
            "2027-01-01T00:00:00Z"
        );
    }

    /// A `none` budget is cumulative: it never comes back, so it has no reset,
    /// no length and no period name.
    #[tokio::test]
    async fn a_cumulative_none_budget_has_no_reset_length_or_period() {
        let (attempt, _) = fetch(pages(&[("", page(&[own("none")], None))])).await;
        let window = primary(&attempt);
        assert_eq!(window.used_percent, 25.0);
        assert_eq!(
            (window.resets_at, window.window_minutes, window.window_kind),
            (None, None, None),
            "a cumulative budget must not be published as a period that resets"
        );
    }

    #[test]
    fn spend_over_the_limit_reads_full_not_past_it() {
        let budget: Value =
            serde_json::from_str(&budget("usr_fixtureUser", "150", "120", "daily", None)).unwrap();
        assert_eq!(
            budget_window(&budget, wednesday()).unwrap().used_percent,
            100.0
        );
    }

    /// No budget for this user is a statement that nothing is configured, never
    /// a 0% window.
    #[tokio::test]
    async fn no_budget_for_this_user_is_no_quota_reported_not_zero() {
        let body = page(&[budget("user_other", "1", "2", "monthly", None)], None);
        let (attempt, _) = fetch(pages(&[("", body)])).await;
        assert_eq!(class(&attempt), "no_quota_reported");
        // The account is still known, so the entry names whose budget is absent.
        assert_eq!(
            attempt.observed.as_ref().unwrap().account_id.as_deref(),
            Some("fixtureUser")
        );
    }

    #[tokio::test]
    async fn an_inactive_or_archived_budget_does_not_apply() {
        let inactive = own("monthly").replace(r#""active":true"#, r#""active":false"#);
        let archived = own("monthly").replace(r#""archived":false"#, r#""archived":true"#);
        let (attempt, _) = fetch(pages(&[("", page(&[inactive, archived], None))])).await;
        assert_eq!(class(&attempt), "no_quota_reported");
    }

    /// Upstream's fail-closed cases, plus the type checks they imply: each is a
    /// decode failure, so none can masquerade as "no budget configured".
    #[tokio::test]
    async fn malformed_or_incomplete_budgets_are_decode_failures() {
        let cases: Vec<(&str, HashMap<String, String>)> = vec![
            (
                "own budget without active",
                pages(&[(
                    "",
                    page(&[own("monthly").replace(r#""active":true,"#, "")], None),
                )]),
            ),
            (
                "string archived flag",
                pages(&[(
                    "",
                    page(
                        &[own("monthly").replace(r#""archived":false"#, r#""archived":"no""#)],
                        None,
                    ),
                )]),
            ),
            (
                "repeated cursor",
                pages(&[("", page(&[], Some("a"))), ("a", page(&[], Some("a")))]),
            ),
            (
                "ambiguous custom",
                pages(&[("", page(&[own("monthly"), own("monthly")], None))]),
            ),
            (
                "ambiguous default",
                pages(&[(
                    "",
                    page(
                        &[
                            budget("usr_fixtureUser", "1", "10", "daily", Some("default")),
                            budget("fixtureUser", "1", "10", "daily", Some("default")),
                        ],
                        None,
                    ),
                )]),
            ),
            (
                "unknown source",
                pages(&[(
                    "",
                    page(
                        &[budget("usr_fixtureUser", "1", "10", "daily", Some("team"))],
                        None,
                    ),
                )]),
            ),
            (
                "null source",
                pages(&[(
                    "",
                    page(&[own("monthly").replace("}", r#","source":null}"#)], None),
                )]),
            ),
            (
                "negative spend",
                pages(&[(
                    "",
                    page(
                        &[budget("usr_fixtureUser", "-1", "120", "monthly", None)],
                        None,
                    ),
                )]),
            ),
            (
                "zero limit",
                pages(&[(
                    "",
                    page(
                        &[budget("usr_fixtureUser", "30", "0", "monthly", None)],
                        None,
                    ),
                )]),
            ),
            (
                "string limit",
                pages(&[(
                    "",
                    page(
                        &[budget("usr_fixtureUser", "30", r#""120""#, "monthly", None)],
                        None,
                    ),
                )]),
            ),
            (
                "unknown period",
                pages(&[("", page(&[own("hourly")], None))]),
            ),
            (
                "missing list",
                pages(&[("", r#"{"nextCursor":null}"#.to_string())]),
            ),
            (
                "empty cursor",
                pages(&[("", r#"{"budgets":[],"nextCursor":""}"#.to_string())]),
            ),
            (
                "numeric cursor",
                pages(&[("", r#"{"budgets":[],"nextCursor":7}"#.to_string())]),
            ),
            ("malformed", pages(&[("", "private-response".to_string())])),
        ];
        for (name, pages) in cases {
            let (attempt, _) = fetch(pages).await;
            let Err(error) = &attempt.usage else {
                panic!("{name} must fail, got {:?}", attempt.usage);
            };
            assert_eq!(error.error_class(), "decode_failed", "{name}: {error:?}");
            assert!(!error.to_string().contains("private-response"), "{name}");
        }
    }

    #[tokio::test]
    async fn a_user_answer_without_an_id_is_a_decode_failure() {
        let (base, _) =
            routes(|_, _| (200, br#"{"user":{"email":"x@example.com"}}"#.to_vec())).await;
        let (source, _) = MockSource::new(&[("apikey:vercel-budget", credential(&deposit(), 1))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            hosts(&base),
            &[],
        );
        let attempt = provider.fetch_handle(&vault_handle()).await;
        assert_eq!(class(&attempt), "decode_failed");
    }

    /// Upstream reads at most 20 pages: one user request and 20 page requests,
    /// then a list that still points onward fails rather than publish a part.
    #[tokio::test]
    async fn pagination_stops_after_twenty_pages() {
        let mut entries = vec![(String::new(), page(&[], Some("c1")))];
        for index in 1..=25 {
            entries.push((
                format!("c{index}"),
                page(&[], Some(&format!("c{}", index + 1))),
            ));
        }
        let (attempt, log) = fetch(entries.into_iter().collect()).await;
        assert_eq!(class(&attempt), "decode_failed");
        assert_eq!(log.lock().unwrap().len(), 21);
    }

    /// Each refusal on either request lands in its class; only a 401 on the
    /// served vault token is reported to the vault, through the shared gate.
    #[tokio::test]
    async fn each_status_maps_to_its_class_and_only_a_401_is_reported() {
        for failing in [USER_PATH, BUDGETS_PATH] {
            for (status, expected, reported) in [
                (401, "credential_rejected", true),
                (403, "credential_rejected", false),
                (429, "upstream_failed", false),
                (503, "upstream_failed", false),
            ] {
                let (base, _) = routes(move |path, _| {
                    if path == failing {
                        (status, b"private-response".to_vec())
                    } else if path == USER_PATH {
                        (200, USER.as_bytes().to_vec())
                    } else {
                        (200, page(&[own("monthly")], None).into_bytes())
                    }
                })
                .await;
                let (source, reports) =
                    MockSource::new(&[("apikey:vercel-budget", credential(&deposit(), 8))]);
                let provider = provider(
                    Some(source),
                    Arc::new(VaultHandleLoader::default()),
                    hosts(&base),
                    &[],
                );
                let attempt = provider.fetch_handle(&vault_handle()).await;
                let error = attempt.usage.as_ref().unwrap_err();
                assert_eq!(error.error_class(), expected, "{failing} HTTP {status}");
                assert_eq!(
                    crate::refresh::classify(error) == crate::refresh::FetchClass::Transient,
                    status == 429 || status >= 500,
                    "{failing} HTTP {status}"
                );
                for _ in 0..20 {
                    if !reports.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                let want = if reported {
                    vec![("apikey:vercel-budget".to_string(), 401, 8)]
                } else {
                    Vec::new()
                };
                assert_eq!(*reports.lock().unwrap(), want, "{failing} HTTP {status}");
            }
        }
    }

    /// Credentials that cannot be used are refused before any request: a
    /// deposit that is not the documented JSON, one without a team, and a team
    /// id outside upstream's rule.
    #[tokio::test]
    async fn an_unusable_credential_sends_nothing() {
        for payload in [
            "bare-token-without-a-team",
            r#"{"token":"fixture-token"}"#,
            r#"{"team_id":"team_fixture"}"#,
            r#"{"token":"fixture-token","team_id":"team fixture"}"#,
            r#"{"token":"fixture-token","team_id":"team/../x"}"#,
        ] {
            let (base, log) = api(HashMap::new()).await;
            let (source, _) = MockSource::new(&[("apikey:vercel-budget", credential(payload, 1))]);
            let provider = provider(
                Some(source),
                Arc::new(VaultHandleLoader::default()),
                hosts(&base),
                &[],
            );
            let attempt = provider.fetch_handle(&vault_handle()).await;
            let error = attempt.usage.as_ref().unwrap_err();
            assert_eq!(error.error_class(), "credential_unusable", "{payload}");
            assert!(!error.to_string().contains("fixture-token"), "{payload}");
            assert!(
                log.lock().unwrap().is_empty(),
                "{payload} reached the network"
            );
        }
    }

    #[tokio::test]
    async fn the_environment_lane_needs_both_variables() {
        let (base, log) = api(pages(&[("", page(&[own("daily")], None))])).await;
        let none = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            hosts(&base),
            &[],
        );
        let attempt = none.fetch_handle(&CredentialHandle::implicit()).await;
        assert_eq!(class(&attempt), "credential_absent");

        let token_only = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            hosts(&base),
            &[(TOKEN_ENV, "env-token")],
        );
        let attempt = token_only.fetch_handle(&CredentialHandle::implicit()).await;
        assert_eq!(class(&attempt), "credential_unusable");
        assert!(log.lock().unwrap().is_empty());

        let both = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            hosts(&base),
            &[(TOKEN_ENV, "env-token"), (TEAM_ENV, TEAM)],
        );
        let attempt = both.fetch_handle(&CredentialHandle::implicit()).await;
        assert_eq!(primary(&attempt).window_kind.as_deref(), Some("daily"));
        assert_eq!(attempt.source.as_deref(), Some("api"));
        let requests = log.lock().unwrap().clone();
        assert!(requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer env-token")));
    }

    /// A deposit replaces the environment lane; without one the environment
    /// lane is enumerated, which is what makes the first half meaningful.
    #[test]
    fn a_vault_deposit_replaces_the_environment_lane() {
        let env = [(TOKEN_ENV, "env-token"), (TEAM_ENV, TEAM)];
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:vercel-budget", "apikey")]);
        let (source, _) = MockSource::new(&[]);
        let with_vault = provider(Some(source), loader, hosts("http://127.0.0.1:9"), &env);
        assert_eq!(with_vault.handles().unwrap(), vec![vault_handle()]);

        let (source, _) = MockSource::new(&[]);
        let without_vault = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            hosts("http://127.0.0.1:9"),
            &env,
        );
        assert_eq!(
            without_vault.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );

        // And the gateway key's family is not this provider's.
        let loader = VaultHandleLoader::default();
        loader.install_rows_for_test(&[("apikey:vercel", "apikey")]);
        assert!(loader.vercel_budget_handles().unwrap().is_empty());
    }

    /// The two credentials never reach each other's host.
    ///
    /// Both providers read their deposits from one vault, each pointed at its
    /// own loopback stand-in for its host, and each stand-in records the
    /// `Authorization` header of every request it received. Both lanes must
    /// succeed, so a lane that sends nothing cannot pass the negative checks.
    #[tokio::test]
    async fn neither_credential_ever_reaches_the_other_lanes_host() {
        let (gateway, gateway_log) = routes(|path, _| {
            if path == "/v1/credits" {
                (200, br#"{"balance":"95.50","total_used":"4.50"}"#.to_vec())
            } else {
                (404, b"{}".to_vec())
            }
        })
        .await;
        let (api_base, api_log) = api(pages(&[("", page(&[own("weekly")], None))])).await;
        let shared = VercelHosts {
            gateway,
            api: api_base,
        };
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[
            ("apikey:vercel", "apikey"),
            ("apikey:vercel-budget", "apikey"),
        ]);
        let (source, _) = MockSource::new(&[
            ("apikey:vercel", credential("gateway-key", 1)),
            ("apikey:vercel-budget", credential(&deposit(), 1)),
        ]);

        let mut credits =
            VercelProvider::new_with_handle_loader(Some(Arc::clone(&source)), Arc::clone(&loader));
        credits.set_hosts_for_test(shared.clone());
        let budget = provider(Some(source), Arc::clone(&loader), shared, &[]);

        let credits_handles = credits.handles().unwrap();
        let budget_handles = budget.handles().unwrap();
        assert_eq!(credits_handles.len(), 1);
        assert_eq!(budget_handles.len(), 1);
        let credits_attempt = credits.fetch_handle(&credits_handles[0]).await;
        let budget_attempt = budget.fetch_handle(&budget_handles[0]).await;

        let gateway_seen = gateway_log.lock().unwrap().clone();
        let api_seen = api_log.lock().unwrap().clone();
        assert!(
            api_seen
                .iter()
                .all(|request| request.authorization.as_deref() != Some("Bearer gateway-key")),
            "the gateway key reached the budget host: {api_seen:?}"
        );
        assert!(
            gateway_seen
                .iter()
                .all(|request| request.authorization.as_deref() != Some("Bearer fixture-token")),
            "the budget token reached the gateway host: {gateway_seen:?}"
        );
        assert!(credits_attempt.usage.is_ok(), "{:?}", credits_attempt.usage);
        assert!(budget_attempt.usage.is_ok(), "{:?}", budget_attempt.usage);
        assert_eq!(gateway_seen.len(), 1);
        assert_eq!(
            gateway_seen[0].authorization.as_deref(),
            Some("Bearer gateway-key")
        );
        assert_eq!(api_seen.len(), 2);
        assert!(api_seen
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer fixture-token")));
    }

    /// One lane failing costs the other nothing: a rejected gateway key leaves
    /// the budget reading intact, and a failing budget leaves the balance.
    #[tokio::test]
    async fn a_failure_on_one_lane_never_costs_the_other_its_reading() {
        for (gateway_status, api_status) in [(401u16, 200u16), (200, 503)] {
            let (gateway, _) = routes(move |_, _| {
                (
                    gateway_status,
                    br#"{"balance":"1.00","total_used":"0"}"#.to_vec(),
                )
            })
            .await;
            let (api_base, _) = routes(move |path, _| {
                if api_status != 200 {
                    (api_status, b"{}".to_vec())
                } else if path == USER_PATH {
                    (200, USER.as_bytes().to_vec())
                } else {
                    (200, page(&[own("daily")], None).into_bytes())
                }
            })
            .await;
            let shared = VercelHosts {
                gateway,
                api: api_base,
            };
            let loader = Arc::new(VaultHandleLoader::default());
            loader.install_rows_for_test(&[
                ("apikey:vercel", "apikey"),
                ("apikey:vercel-budget", "apikey"),
            ]);
            let (source, _) = MockSource::new(&[
                ("apikey:vercel", credential("gateway-key", 1)),
                ("apikey:vercel-budget", credential(&deposit(), 1)),
            ]);
            let registry = crate::Registry::new(vec![
                Box::new({
                    let mut credits = VercelProvider::new_with_handle_loader(
                        Some(Arc::clone(&source)),
                        Arc::clone(&loader),
                    );
                    credits.set_hosts_for_test(shared.clone());
                    credits
                }),
                Box::new(provider(Some(source), Arc::clone(&loader), shared, &[])),
            ]);
            registry
                .refresh_tick(&tokio_util::sync::CancellationToken::new())
                .await;
            let entries = registry.get_usage(None).await;
            let entry = |name: &str| {
                entries
                    .iter()
                    .find(|entry| entry.provider == name)
                    .unwrap_or_else(|| panic!("{name} published no entry: {entries:?}"))
                    .clone()
            };
            let credits = entry("vercel");
            let budget = entry("vercel-budget");
            if gateway_status == 200 {
                assert!(credits.error.is_none(), "{credits:?}");
                assert_eq!(credits.spend.as_ref().map(Vec::len), Some(1), "{credits:?}");
                assert_eq!(
                    budget.error_class.as_deref(),
                    Some("upstream_failed"),
                    "{budget:?}"
                );
            } else {
                assert_eq!(
                    credits.error_class.as_deref(),
                    Some("credential_rejected"),
                    "{credits:?}"
                );
                assert!(budget.error.is_none(), "{budget:?}");
                assert!(
                    budget
                        .usage
                        .as_ref()
                        .and_then(|usage| usage.primary.as_ref())
                        .is_some(),
                    "{budget:?}"
                );
                assert_eq!(budget.account.as_deref(), Some("fixtureUser"));
            }
        }
    }
}
