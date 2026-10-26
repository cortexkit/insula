//! Optional team quota for idle Muse logins, from a deposited dev.meta.ai session.
//! The Muse API response remains authoritative for account identity and plan.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::LiveQuotaConfig;
use crate::cookie_lifetime::{render_age, CookieLifetimes, LifetimeEvent};
use crate::cookie_vault::CookieVault;
use crate::credential_source::CredentialSource;
use crate::http::{Header, JsonRequest};
use crate::model::Usage;
use crate::provider::CredentialHandle;
use crate::vault_handles::VaultHandleLoader;

const TIMEOUT: Duration = Duration::from_secs(8);
const FAMILY: &str = "cookie:dev.meta.ai";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Team {
    id: String,
    name: String,
}

#[derive(Default)]
struct State {
    teams: HashMap<String, Vec<Team>>,
    chosen: HashMap<String, Team>,
    expired: HashSet<(String, u64)>,
    lifetimes: CookieLifetimes,
}

pub(super) struct TeamQuota {
    pub(super) base: String,
    http: reqwest::Client,
    vault: CookieVault,
    config: LiveQuotaConfig,
    state: Mutex<State>,
}

#[derive(Debug, PartialEq)]
enum Unavailable {
    Expired,
    Other,
}

impl TeamQuota {
    // Redirects are turned off so a copied session never travels to another
    // host; the pool idle bound is set below the refresh cadence.
    #[allow(clippy::disallowed_methods)]
    pub(super) fn new(
        config: LiveQuotaConfig,
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            base: super::WEB_URL.into(),
            // Do not follow a login redirect: a copied session must never travel
            // to another host, and a login page is not a malformed quota answer.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .pool_idle_timeout(Duration::from_secs(15))
                .build()
                .expect("Muse web HTTP client"),
            vault: CookieVault::new(source, loader, FAMILY),
            config,
            state: Mutex::new(State::default()),
        }
    }

    async fn get(
        &self,
        cookie: &str,
        path: &str,
        members: bool,
        budget: &mut usize,
    ) -> Result<Value, Unavailable> {
        if *budget == 0 {
            return Err(Unavailable::Other);
        }
        *budget -= 1;
        let response = JsonRequest::get(format!("{}{path}", self.base))
            .header(Header::new("Cookie", cookie))
            .timeout(TIMEOUT)
            .send_raw(&self.http)
            .await
            .map_err(|_| Unavailable::Other)?;
        if response.status == 401
            || (response.status == 403 && !members)
            || ((300..400).contains(&response.status)
                && response.header("location").is_some_and(|url| {
                    url.contains("login") || url.contains("signin") || url.contains("/auth")
                }))
        {
            return Err(Unavailable::Expired);
        }
        if response.status != 200 {
            return Err(Unavailable::Other);
        }
        let value: Value =
            serde_json::from_slice(&response.body).map_err(|_| Unavailable::Other)?;
        value.is_object().then_some(value).ok_or(Unavailable::Other)
    }

    fn note_session(
        &self,
        handle: &CredentialHandle,
        version: u64,
        result: &Result<Option<Usage>, Unavailable>,
    ) {
        let Some(id) = handle.vault_credential_id() else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let rejected = *result == Err(Unavailable::Expired);
        if rejected {
            if state.expired.insert((id.to_string(), version)) {
                eprintln!("{} deposited cookie {id} (version {version}) expired; recapture dev.meta.ai login", crate::LOG_TAG);
            }
        } else if result.is_ok() {
            state.expired.retain(|(seen, _)| seen != id);
        }
        if let LifetimeEvent::FirstRejection {
            record_version,
            age,
        } = state
            .lifetimes
            .observe(id, Some(version), result.is_ok(), rejected, Instant::now())
        {
            eprintln!("{} deposited cookie {id} (version {}) first rejected after working {} since first served",
                crate::LOG_TAG, record_version.unwrap_or(version), render_age(age));
        }
    }

    fn team_listing(&self, email: &str, teams: &[Team]) -> Option<String> {
        let mut state = self.state.lock().ok()?;
        let mut teams = teams.to_vec();
        teams.sort();
        if state.teams.get(email) != Some(&teams) {
            let listing: Vec<_> = teams
                .iter()
                .map(|team| format!("{} ({})", team.id, team.name))
                .collect();
            let line = format!(
                "{} muse browser teams for {email}: [{}]; no single team on the login's plan, \
                 so set muse.team_id to choose one",
                crate::LOG_TAG,
                listing.join(", ")
            );
            state.teams.insert(email.to_string(), teams);
            return Some(line);
        }
        None
    }

    pub(super) async fn fetch(&self, email: Option<&str>, plan: Option<&str>) -> Option<Usage> {
        let email = email?.trim().to_lowercase();
        if email.is_empty() {
            return None;
        }
        let selected = self
            .config
            .muse()
            .team_id
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut budget = 5;
        let mut unverified = Vec::new();
        for handle in self.vault.enrichment_deposits() {
            if budget == 0 {
                break;
            }
            let Some((cookie, version)) = self
                .vault
                .session_cookie_for(&handle, "llama_dev_sess")
                .await
            else {
                continue;
            };
            let me = match self.get(&cookie, "/api/auth/me", false, &mut budget).await {
                Ok(me) => me,
                Err(error) => {
                    self.note_session(&handle, version, &Err(error));
                    continue;
                }
            };
            let web_email = me["email"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            if web_email.is_empty() {
                if let Some(user) = numeric_id(&me["userId"]) {
                    unverified.push((handle, cookie, version, user));
                }
                continue;
            }
            if web_email != email {
                continue;
            }
            let result = self
                .team_quota(&cookie, &email, plan, &selected, None, &mut budget)
                .await;
            self.note_session(&handle, version, &result);
            if let Ok(quota) = result {
                return quota;
            }
        }
        // Direct email matches go first: a blank-email session needs one more
        // request to prove ownership, and all deposits share a five-request cap.
        for (handle, cookie, version, user) in unverified {
            let result = self
                .team_quota(&cookie, &email, plan, &selected, Some(&user), &mut budget)
                .await;
            self.note_session(&handle, version, &result);
            if let Ok(quota) = result {
                return quota;
            }
        }
        None
    }

    async fn team_quota(
        &self,
        cookie: &str,
        email: &str,
        plan: Option<&str>,
        selected: &str,
        user: Option<&str>,
        budget: &mut usize,
    ) -> Result<Option<Usage>, Unavailable> {
        let response = self.get(cookie, "/api/portal/teams", false, budget).await?;
        let listed = response["teams"].as_array().ok_or(Unavailable::Other)?;
        let teams: Vec<_> = listed
            .iter()
            .filter_map(|entry| {
                let id = numeric_id(&entry["team_id"])?;
                let name = entry["team_name"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&id)
                    .to_string();
                Some(Team { id, name })
            })
            .collect();
        let team = teams.iter().find(|team| team.id == selected);
        if let Some(user) = user {
            // When the configured team is not visible, the first listed team
            // can prove session identity but cannot select a quota. Some roles
            // cannot list members; a 403 here does not expire the cookie.
            let probe = team.or_else(|| teams.first()).ok_or(Unavailable::Other)?;
            let members = self
                .get(
                    cookie,
                    &format!("/api/portal/teams/{}/members", probe.id),
                    true,
                    budget,
                )
                .await?;
            let own = members["members"].as_array().and_then(|members| {
                members
                    .iter()
                    .find(|member| numeric_id(&member["user_id"]).as_deref() == Some(user))
            });
            if own
                .and_then(|member| member["email"].as_str())
                .map(|email| email.trim().to_lowercase())
                .as_deref()
                != Some(email)
            {
                return Err(Unavailable::Other);
            }
        }
        let Some(plan) = plan else { return Ok(None) };
        if !selected.is_empty() {
            // An explicit choice: only that team, and only if visible.
            let Some(team) = team else { return Ok(None) };
            let quota = self.subscription_quota(cookie, team, budget).await?;
            if quota["tier"].as_str().map(str::trim) != Some(plan) {
                return Ok(None);
            }
            return Ok(parse_quota(&quota, chrono::Utc::now().timestamp() as f64));
        }
        // No team configured: pick the team whose subscription is on the
        // login's plan, when exactly one is. The session is already proven to
        // be the login's own account, and the login's subscription is one of
        // the teams it can see, so a single plan match is that subscription.
        // Two or more on the same plan are ambiguous: serve nothing, and list
        // them so `muse.team_id` can choose. Each team checked costs one
        // request from the shared budget; teams past it are not guessed at.
        let mut matches: Vec<(Team, Value)> = Vec::new();
        let mut checked_all = true;
        for team in &teams {
            if *budget == 0 {
                checked_all = false;
                break;
            }
            let quota = self.subscription_quota(cookie, team, budget).await?;
            if quota["tier"].as_str().map(str::trim) == Some(plan) {
                matches.push((team.clone(), quota));
            }
        }
        if !checked_all || matches.len() != 1 {
            let listed: Vec<Team> = if matches.is_empty() {
                teams
            } else {
                matches.into_iter().map(|(team, _)| team).collect()
            };
            if let Some(line) = self.team_listing(email, &listed) {
                eprintln!("{line}");
            }
            return Ok(None);
        }
        let (team, quota) = matches.pop().expect("exactly one match");
        self.note_chosen(email, &team);
        Ok(parse_quota(&quota, chrono::Utc::now().timestamp() as f64))
    }

    async fn subscription_quota(
        &self,
        cookie: &str,
        team: &Team,
        budget: &mut usize,
    ) -> Result<Value, Unavailable> {
        let mut response = self
            .get(
                cookie,
                &format!("/api/portal/teams/{}/subscription-quota", team.id),
                false,
                budget,
            )
            .await?;
        Ok(response["subscription_quota"].take())
    }

    /// Log, once per account and team, which team was chosen by plan.
    fn note_chosen(&self, email: &str, team: &Team) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.chosen.get(email) != Some(team) {
            eprintln!(
                "{} muse: using dev.meta.ai team {} ({}) for {email}, the only visible team on \
                 its plan; set muse.team_id to choose another",
                crate::LOG_TAG,
                team.id,
                team.name
            );
            state.chosen.insert(email.to_string(), team.clone());
        }
    }
}

fn numeric_id(value: &Value) -> Option<String> {
    let id = match value {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n
            .as_u64()
            .filter(|n| *n <= 9_007_199_254_740_991)?
            .to_string(),
        _ => return None,
    };
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

fn amount(value: &Value) -> Option<f64> {
    let number = match value {
        Value::String(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.parse().ok()?
        }
        Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    (number.is_finite() && number >= 0.0).then_some(number)
}

fn parse_quota(quota: &Value, now: f64) -> Option<Usage> {
    let percent = |used: &str, limit: &str| {
        let limit = amount(&quota[limit])?;
        (limit > 0.0).then_some((amount(&quota[used])? / limit * 100.0).min(100.0))
    };
    let reset = |field: &str| amount(&quota[field]).filter(|n| *n > 0.0 && *n <= 64_092_211_200.0);
    let weekly = percent("weekly_weighted_used", "weekly_weighted_limit")?;
    let weekly_reset = reset("weekly_resets_at").filter(|n| *n > now)?;
    let seconds = amount(&quota["window_duration_secs"])?;
    if seconds.fract() != 0.0 || !(60.0..=9_007_199_254_740_991.0).contains(&seconds) {
        return None;
    }
    let mut primary = percent("window_weighted_used", "window_weighted_limit")?;
    let short_reset = reset("window_resets_at");
    let missing = quota["window_resets_at"].is_null();
    if (!missing && short_reset.is_none()) || (missing && primary != 0.0) {
        return None;
    }
    let active_reset = short_reset.filter(|n| *n > now);
    if active_reset.is_none() {
        primary = 0.0;
    }
    let minutes = (seconds / 60.0).round() as i64;
    Some(Usage {
        primary: Some(super::window(
            primary,
            minutes,
            (minutes == 300).then_some(cortexkit_provider_usage::window_kind::FIVE_HOUR),
            active_reset,
        )),
        secondary: Some(super::window(
            weekly,
            super::WEEKLY_MINUTES,
            Some(cortexkit_provider_usage::window_kind::WEEKLY),
            Some(weekly_reset),
        )),
        tertiary: None,
        extra_rate_windows: None,
    })
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
