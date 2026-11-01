//! LiteLLM proxy budgets, ported from CodexBar v0.83.0
//! `Sources/CodexBarCore/Resources/Plugins/litellm.ts`.
//!
//! Positive personal (or team-member) and team caps become separate derived USD
//! pools. Spend without a cap is not capacity: no pools means `no_quota_reported`.
//! Fixture-verified only; no LiteLLM proxy was available for live verification.

use async_trait::async_trait;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Number, Value};
use std::{net::IpAddr, sync::Arc};

use crate::config::LiveQuotaConfig;
use crate::credential_source::CredentialSource;
use crate::http::JsonRequest;
use crate::model::{Amount, Pool, PoolBasis, PoolFunding, Usage};
use crate::money::parse_amount;
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;
use crate::vercel::{default_env, EnvLookup};

pub const PROVIDER_NAME: &str = "litellm";
const KEY_ENV: &str = "LITELLM_API_KEY";

fn decode() -> FetchError {
    FetchError::Decode("LiteLLM returned invalid budget or identity data".into())
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => ip.to_ipv4_mapped().map_or_else(
            || ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local(),
            |ip| private_ip(IpAddr::V4(ip)),
        ),
    }
}

fn base_url(raw: Option<&str>) -> Result<Url, FetchError> {
    let invalid = || {
        FetchError::CredentialUnusable(
        "set litellm.base_url to HTTPS, or HTTP on localhost, a private/link-local address or a .local host; credentials, query and fragment are not allowed".into(),
    )
    };
    let mut url = Url::parse(raw.unwrap_or_default().trim()).map_err(|_| invalid())?;
    let host = url.host_str().ok_or_else(invalid)?.trim_end_matches('.');
    let private = host == "localhost"
        || host.ends_with(".local")
        || host.trim_matches(['[', ']']).parse().is_ok_and(private_ip);
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && private))
    {
        return Err(invalid());
    }
    let mut path = url.path().trim_end_matches('/').to_string();
    // Management endpoints live above the optional OpenAI-compatible /v1 suffix.
    if path.ends_with("/v1") {
        path.truncate(path.rfind('/').unwrap_or(0));
    }
    url.set_path(&format!("{path}/"));
    Ok(url)
}

#[derive(Default, Deserialize)]
struct Budget {
    spend: Option<Number>,
    max_budget: Option<Number>,
    budget_reset_at: Option<String>,
}

fn amount(number: Option<&Number>) -> Result<Amount, FetchError> {
    let text = number.map_or_else(|| "0".into(), ToString::to_string);
    // JSON can spell small money amounts with an exponent; expand the decimal
    // text rather than round a binary float into the money representation.
    let plain = if let Some((mantissa, shift)) = text.split_once(['e', 'E']) {
        let shift: i32 = shift.parse().map_err(|_| decode())?;
        if !(-40..=40).contains(&shift) {
            return Err(decode());
        }
        let (sign, mantissa) = mantissa
            .strip_prefix('-')
            .map_or(("", mantissa), |m| ("-", m));
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        let digits = format!("{whole}{fraction}");
        let point = whole.len() as i32 + shift;
        if point <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!(
                "{sign}{digits}{}",
                "0".repeat(point as usize - digits.len())
            )
        } else {
            let (a, b) = digits.split_at(point as usize);
            format!("{sign}{a}.{b}")
        }
    } else {
        text
    };
    parse_amount(&plain, "USD").ok_or_else(decode)
}

impl Budget {
    fn pool(&self, id: &str, label: &str) -> Result<Option<Pool>, FetchError> {
        let cap = amount(self.max_budget.as_ref())?;
        let spent = amount(self.spend.as_ref())?;
        if cap.minor <= 0 {
            return Ok(None);
        }
        let exponent = cap.exponent.max(spent.exponent);
        let scale = |a: &Amount| {
            a.minor
                .max(0)
                .checked_mul(10i64.pow((exponent - a.exponent).into()))
        };
        let total = scale(&cap).ok_or_else(decode)?;
        let used = scale(&spent).ok_or_else(decode)?;
        let money = |minor| Amount {
            minor,
            exponent,
            unit: "USD".into(),
        };
        Ok(Some(Pool {
            id: id.into(),
            label: label.into(),
            funding: PoolFunding::Unknown,
            remaining: Some(money((total - used).max(0))),
            total: Some(money(total)),
            basis: PoolBasis::Derived,
            spendable: None,
            resets_at: self
                .budget_reset_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.to_rfc3339()),
            expires_at: None,
            unlimited: None,
        }))
    }
}

#[derive(Deserialize)]
struct KeyRoot {
    info: KeyInfo,
}
#[derive(Deserialize)]
struct KeyInfo {
    user_id: Option<String>,
    team_id: Option<String>,
}
#[derive(Deserialize)]
struct UserRoot {
    user_id: Option<String>,
    user_info: Value,
    #[serde(default)]
    teams: Option<Vec<Value>>,
}

fn nonempty(id: Option<&str>) -> Option<&str> {
    id.map(str::trim).filter(|id| !id.is_empty())
}

fn identity(value: &Value, field: &str, expected: &str) -> Result<(), FetchError> {
    match value.get(field).filter(|v| !v.is_null()) {
        None => Ok(()),
        Some(Value::String(id)) if id == expected => Ok(()),
        _ => Err(decode()),
    }
}

fn budget(value: &Value) -> Result<Budget, FetchError> {
    serde_json::from_value(value.clone()).map_err(|_| decode())
}

fn member<'a>(rows: Option<&'a Value>, user: &str, team: &str) -> Option<&'a Value> {
    rows?.as_array()?.iter().find(|row| {
        row.get("user_id").and_then(Value::as_str) == Some(user)
            && row
                .get("team_id")
                .is_none_or(|id| id.is_null() || id.as_str() == Some(team))
    })
}

pub struct LiteLLMProvider {
    config: LiveQuotaConfig,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    env: EnvLookup,
}

impl LiteLLMProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
        config: LiveQuotaConfig,
    ) -> Self {
        Self {
            config,
            credential_source,
            handle_loader,
            env: default_env(),
        }
    }

    async fn request(
        http: &reqwest::Client,
        base: &Url,
        path: &str,
        query: Option<(&str, &str)>,
        key: &str,
    ) -> Result<Value, FetchError> {
        let mut url = base.join(path).map_err(|_| decode())?;
        if let Some((name, value)) = query {
            url.query_pairs_mut().append_pair(name, value);
        }
        let response = JsonRequest::get(url.as_str())
            .bearer(key)
            .send_provider_status_first(http, PROVIDER_NAME)
            .await?;
        serde_json::from_slice(&response.body).map_err(|_| decode())
    }

    async fn read(&self, key: &str) -> Result<Vec<Pool>, FetchError> {
        let base = base_url(self.config.litellm().base_url.as_deref())?;
        let http = crate::http::provider_client_without_redirects()?;
        let root = Self::request(&http, &base, "key/info", None, key).await?;
        let root: KeyRoot = serde_json::from_value(root).map_err(|_| decode())?;
        let user = nonempty(root.info.user_id.as_deref());
        let team = nonempty(root.info.team_id.as_deref());
        let mut pools = Vec::new();
        if let Some(user) = user {
            let root =
                Self::request(&http, &base, "user/info", Some(("user_id", user)), key).await?;
            let root: UserRoot = serde_json::from_value(root).map_err(|_| decode())?;
            identity(&root.user_info, "user_id", user)?;
            if root.user_info.get("user_id").is_none_or(Value::is_null)
                && root.user_id.as_deref().is_some_and(|id| id != user)
            {
                return Err(decode());
            }
            let personal = budget(&root.user_info)?;
            let personal = personal.pool("personal", "Personal budget")?;
            let teams = root.teams.unwrap_or_default();
            let matching =
                team.and_then(|id| teams.iter().find(|row| row["team_id"].as_str() == Some(id)));
            if let Some(pool) = personal {
                pools.push(pool);
            } else if let Some(team) = team {
                // Membership discovery is optional. Its failure must not discard
                // the personal/team budgets already returned by /user/info.
                if let Ok(Some(pool)) =
                    Self::member_pool(&http, &base, key, user, team, matching).await
                {
                    pools.push(pool);
                }
            }
            if let Some(team) = matching {
                if let Some(pool) = budget(team)?.pool("team", "Team budget")? {
                    pools.push(pool);
                }
            }
        } else if let Some(team) = team {
            let root =
                Self::request(&http, &base, "team/info", Some(("team_id", team)), key).await?;
            let info = root.get("team_info").ok_or_else(decode)?;
            identity(info, "team_id", team)?;
            if info.get("team_id").is_none_or(Value::is_null) {
                identity(&root, "team_id", team)?;
            }
            if let Some(pool) = budget(info)?.pool("team", "Team budget")? {
                pools.push(pool);
            }
        } else {
            return Err(decode());
        }
        if pools.is_empty() {
            Err(FetchError::NoQuotaReported(
                "LiteLLM reports no positive budget cap with scoped spend".into(),
            ))
        } else {
            Ok(pools)
        }
    }

    async fn member_pool(
        http: &reqwest::Client,
        base: &Url,
        key: &str,
        user: &str,
        team: &str,
        matching: Option<&Value>,
    ) -> Result<Option<Pool>, FetchError> {
        let mine = member(matching.and_then(|v| v.get("team_memberships")), user, team);
        let mut table = mine
            .and_then(|v| v.get("litellm_budget_table"))
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let mut spend = mine
            .and_then(|v| v.get("spend"))
            .filter(|v| !v.is_null())
            .cloned();
        if table.get("max_budget").is_none_or(Value::is_null) || spend.is_none() {
            let root = Self::request(http, base, "team/info", Some(("team_id", team)), key).await?;
            let info = root
                .get("team_info")
                .filter(|v| v.is_object())
                .ok_or_else(decode)?;
            identity(info, "team_id", team)?;
            if info.get("team_id").is_none_or(Value::is_null) {
                identity(&root, "team_id", team)?;
            }
            let fallback = member(
                info.get("team_memberships")
                    .filter(|v| !v.is_null())
                    .or_else(|| root.get("team_memberships")),
                user,
                team,
            );
            if table.get("max_budget").is_none_or(Value::is_null) {
                table = fallback
                    .and_then(|v| v.get("litellm_budget_table"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if table.get("max_budget").is_none_or(Value::is_null) {
                    table = info
                        .get("team_member_budget_table")
                        .cloned()
                        .unwrap_or(Value::Null);
                }
            }
            if spend.is_none() {
                spend = fallback
                    .and_then(|v| v.get("spend"))
                    .filter(|v| !v.is_null())
                    .cloned();
            }
        }
        // User-wide spend must never be paired with a team-member cap.
        let Some(spend) = spend else { return Ok(None) };
        if table.is_null() {
            return Ok(None);
        }
        let mut member_budget = budget(&table)?;
        member_budget.spend = Some(serde_json::from_value(spend).map_err(|_| decode())?);
        member_budget.pool("member", "Team member budget")
    }

    async fn fetch_vault(&self, handle: &CredentialHandle) -> FetchAttempt {
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
        let key = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(key) if !key.trim().is_empty() => key,
            _ => {
                return attempt_from(
                    Err(FetchError::CredentialUnusable(
                        "the apikey:litellm deposit holds no UTF-8 API key".into(),
                    )),
                    "vault",
                )
            }
        };
        let result = self.read(key.trim()).await;
        if let Err(error) = &result {
            crate::credential_source::report_vault_auth_failure(
                Some(source),
                handle,
                credential.record_version,
                error,
            );
        }
        attempt_from(result, "vault")
    }
}

fn attempt_from(result: Result<Vec<Pool>, FetchError>, source: &str) -> FetchAttempt {
    match result {
        Ok(pools) => {
            let mut attempt = FetchAttempt::success(None, source, Usage::default());
            attempt.pools = Some(pools);
            attempt
        }
        Err(error) => FetchAttempt::failure(None, Some(source.into()), error),
    }
}

#[async_trait]
impl UsageProvider for LiteLLMProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.litellm_handles()?;
            if !vault.is_empty() {
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }
        // `LITELLM_API_KEY` is also what LiteLLM's own SDK and proxy read, so it
        // is often set on hosts that never asked insula to watch a proxy. Without
        // a configured `litellm.base_url` the environment key is not a setup for
        // this provider, so it reads as unconfigured rather than as a broken
        // credential. A deliberate vault deposit with no URL still reports the
        // missing URL (fetch_vault).
        let configured = self.config.litellm().base_url.is_some();
        match (self.env)(KEY_ENV).filter(|_| configured) {
            Some(key) => attempt_from(self.read(&key).await, "api"),
            None => attempt_from(
                Err(FetchError::NoSession(
                    "no apikey:litellm vault deposit, and no LITELLM_API_KEY with litellm.base_url set"
                        .into(),
                )),
                "api",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LiteLLMConfig, QuotaConfig};
    use crate::vercel::testkit::{credential, routes, Log, MockSource};
    use serde_json::json;

    // Fixtures and member-budget cases ported from CodexBar v0.83.0
    // Tests/CodexBarTests/LiteLLMMemberBudgetTests.swift.
    fn key() -> Value {
        json!({"info":{"user_id":"user-1","team_id":"team-1","spend":96.0,"max_budget":null}})
    }

    fn user() -> Value {
        json!({"user_id":"user-1", "user_info":{"user_id":"user-1","max_budget":null,"spend":140.0},
            "teams":[{"team_id":"team-1","team_alias":"Platform","max_budget":5000.0,"spend":1200.0,
                "budget_reset_at":"2026-11-15T00:00:00Z",
                "team_memberships":[{"user_id":"user-1","spend":96.0,
                    "litellm_budget_table":{"max_budget":null,"budget_reset_at":"2026-11-20T00:00:00Z"}}]}]})
    }

    fn team() -> Value {
        json!({"team_id":"team-1", "team_info":{"team_id":"team-1","team_alias":"Platform",
            "max_budget":5000.0,"spend":1200.0,
            "team_member_budget_table":{"max_budget":250.0,"budget_duration":"30d","budget_reset_at":"2026-11-01T00:00:00Z"}},
            "team_memberships":[]})
    }

    fn provider(base: &str) -> LiteLLMProvider {
        LiteLLMProvider::new_with_handle_loader(
            None,
            Arc::new(VaultHandleLoader::default()),
            QuotaConfig {
                litellm: LiteLLMConfig {
                    base_url: Some(base.into()),
                },
                ..Default::default()
            }
            .into(),
        )
    }

    async fn fetch(
        k: Value,
        u: Value,
        t: Value,
        status: u16,
    ) -> (Result<Vec<Pool>, FetchError>, Log) {
        let (base, log) = routes(move |path, query| {
            let (status, value) = match path {
                "/key/info" => {
                    assert!(query.is_empty());
                    (200, &k)
                }
                "/user/info" => {
                    assert_eq!(query, &[("user_id".into(), "user-1".into())]);
                    (200, &u)
                }
                "/team/info" => {
                    assert_eq!(query, &[("team_id".into(), "team-1".into())]);
                    (status, &t)
                }
                _ => panic!("unexpected request: {path}"),
            };
            (status, serde_json::to_vec(value).unwrap())
        })
        .await;
        (provider(&base).read("fixture-key").await, log)
    }

    fn paths(log: &Log) -> Vec<String> {
        let log = log.lock().unwrap();
        assert!(log
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer fixture-key")));
        log.iter().map(|r| r.path.clone()).collect()
    }

    fn pool<'a>(
        pools: &'a [Pool],
        id: &str,
        remaining: &str,
        total: &str,
        reset: Option<&str>,
    ) -> &'a Pool {
        let p = pools.iter().find(|p| p.id == id).unwrap();
        let exponent = p.total.as_ref().unwrap().exponent;
        let expected = |s: &str| {
            let a = parse_amount(s, "USD").unwrap();
            Amount {
                minor: a.minor * 10i64.pow((exponent - a.exponent).into()),
                exponent,
                unit: "USD".into(),
            }
        };
        assert_eq!(p.total, Some(expected(total)));
        assert_eq!(p.remaining, Some(expected(remaining)));
        assert_eq!(p.resets_at.as_deref(), reset);
        assert_eq!(p.basis, PoolBasis::Derived);
        assert_eq!(p.funding, PoolFunding::Unknown);
        assert_eq!(p.spendable, None);
        p
    }

    #[tokio::test]
    async fn team_default_member_budget_becomes_personal_capacity() {
        let (result, log) = fetch(key(), user(), team(), 200).await;
        let pools = result.unwrap();
        assert_eq!(paths(&log), ["/key/info", "/user/info", "/team/info"]);
        assert_eq!(pools.len(), 2);
        pool(
            &pools,
            "member",
            "154",
            "250",
            Some("2026-11-01T00:00:00+00:00"),
        );
        pool(
            &pools,
            "team",
            "3800",
            "5000",
            Some("2026-11-15T00:00:00+00:00"),
        );
    }

    #[tokio::test]
    async fn own_member_override_needs_no_team_request() {
        let mut u = user();
        u["teams"][0]["team_memberships"][0]["spend"] = json!(10.0);
        u["teams"][0]["team_memberships"][0]["litellm_budget_table"]["max_budget"] = json!(40.0);
        let (result, log) = fetch(key(), u, team(), 200).await;
        pool(
            &result.unwrap(),
            "member",
            "30",
            "40",
            Some("2026-11-20T00:00:00+00:00"),
        );
        assert_eq!(paths(&log), ["/key/info", "/user/info"]);
    }

    #[tokio::test]
    async fn configured_personal_budget_is_untouched() {
        let mut u = user();
        u["user_info"]["max_budget"] = json!(300.0);
        let (result, log) = fetch(key(), u, team(), 200).await;
        let pools = result.unwrap();
        assert_eq!(pools.len(), 2);
        pool(&pools, "personal", "160", "300", None);
        assert_eq!(paths(&log), ["/key/info", "/user/info"]);
    }

    #[tokio::test]
    async fn missing_or_unusable_member_caps_keep_the_known_team_budget() {
        for t in [
            {
                let mut t = team();
                t["team_info"]["team_member_budget_table"] = Value::Null;
                t
            },
            {
                let mut t = team();
                t["team_info"]["team_member_budget_table"] = json!({"max_budget":null});
                t
            },
            {
                let mut t = team();
                t["team_info"]["team_member_budget_table"] = json!({"max_budget":0});
                t
            },
            {
                let mut t = team();
                t["team_info"]["team_id"] = json!("team-other");
                t
            },
            json!({"team_id":"team-1"}),
        ] {
            let (result, log) = fetch(key(), user(), t, 200).await;
            let pools = result.unwrap();
            assert_eq!(pools.len(), 1);
            pool(
                &pools,
                "team",
                "3800",
                "5000",
                Some("2026-11-15T00:00:00+00:00"),
            );
            assert_eq!(paths(&log), ["/key/info", "/user/info", "/team/info"]);
        }
    }

    #[tokio::test]
    async fn optional_team_info_failures_preserve_known_budgets() {
        for status in [401, 403, 404, 429, 500, 503] {
            let (result, _) = fetch(key(), user(), json!({"detail":"nope"}), status).await;
            let pools = result.unwrap();
            assert_eq!(pools.len(), 1);
            pool(
                &pools,
                "team",
                "3800",
                "5000",
                Some("2026-11-15T00:00:00+00:00"),
            );
        }
    }

    #[tokio::test]
    async fn user_without_team_does_not_request_member_budget() {
        let (result, log) = fetch(
            json!({"info":{"user_id":"user-1","spend":96.0}}),
            user(),
            team(),
            200,
        )
        .await;
        assert_eq!(result.unwrap_err().error_class(), "no_quota_reported");
        assert_eq!(paths(&log), ["/key/info", "/user/info"]);
    }

    #[tokio::test]
    async fn user_wide_spend_is_never_used_for_a_member_budget() {
        let mut u = user();
        u["teams"][0]["team_memberships"] = json!([]);
        let (result, _) = fetch(key(), u, team(), 200).await;
        let pools = result.unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id, "team");
    }

    #[tokio::test]
    async fn missing_member_spend_is_read_from_team_memberships() {
        let mut u = user();
        u["teams"][0]["team_memberships"] = json!([]);
        let mut t = team();
        t["team_memberships"] = json!([
            {"user_id":"someone-else","spend":999.0,"team_id":"team-1"},
            {"user_id":"user-1","spend":10.0,"team_id":"team-1"}
        ]);
        let (result, _) = fetch(key(), u, t, 200).await;
        pool(
            &result.unwrap(),
            "member",
            "240",
            "250",
            Some("2026-11-01T00:00:00+00:00"),
        );
    }

    #[tokio::test]
    async fn memberships_for_other_users_or_teams_are_ignored() {
        let mut u = user();
        u["teams"][0]["team_memberships"] = json!([]);
        let mut t = team();
        t["team_memberships"] = json!([
            {"user_id":"user-1","spend":10.0,"team_id":"team-other"},
            {"user_id":"someone-else","spend":20.0,"team_id":"team-1"}
        ]);
        let (result, _) = fetch(key(), u, t, 200).await;
        let pools = result.unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].id, "team");
    }

    #[tokio::test]
    async fn user_info_memberships_from_other_teams_are_ignored() {
        let mut u = user();
        u["teams"][0]["team_memberships"] = json!([{"user_id":"user-1","team_id":"team-other","spend":90.0,"litellm_budget_table":{"max_budget":100.0}}]);
        let mut t = team();
        t["team_memberships"] = json!([{"user_id":"user-1","team_id":"team-1","spend":10.0}]);
        let (result, _) = fetch(key(), u, t, 200).await;
        pool(
            &result.unwrap(),
            "member",
            "240",
            "250",
            Some("2026-11-01T00:00:00+00:00"),
        );
    }

    #[tokio::test]
    async fn fallback_overrides_precede_defaults_but_not_user_overrides() {
        let mut t = team();
        t["team_memberships"] = json!([{"user_id":"user-1","team_id":"team-1","spend":10.0,
            "litellm_budget_table":{"max_budget":40.0,"budget_reset_at":"2026-11-09T00:00:00Z"}}]);
        let mut u = user();
        u["teams"][0]["team_memberships"] = json!([]);
        let (result, _) = fetch(key(), u, t.clone(), 200).await;
        pool(
            &result.unwrap(),
            "member",
            "30",
            "40",
            Some("2026-11-09T00:00:00+00:00"),
        );
        let mut u = user();
        u["teams"][0]["team_memberships"][0]["spend"] = Value::Null;
        u["teams"][0]["team_memberships"][0]["litellm_budget_table"]["max_budget"] = json!(20.0);
        let (result, _) = fetch(key(), u, t, 200).await;
        pool(
            &result.unwrap(),
            "member",
            "10",
            "20",
            Some("2026-11-20T00:00:00+00:00"),
        );
    }

    #[tokio::test]
    async fn team_only_keys_validate_identity_and_publish_only_team_capacity() {
        let k = json!({"info":{"team_id":"team-1"}});
        let mut t = team();
        t["team_info"]["spend"] = json!(6000);
        let (result, log) = fetch(k.clone(), user(), t.clone(), 200).await;
        let pools = result.unwrap();
        assert_eq!(pools.len(), 1);
        pool(&pools, "team", "0", "5000", None);
        assert_eq!(paths(&log), ["/key/info", "/team/info"]);
        t["team_info"]["team_id"] = json!("other-team");
        assert_eq!(
            fetch(k, user(), t, 200).await.0.unwrap_err().error_class(),
            "decode_failed"
        );
    }

    #[tokio::test]
    async fn no_positive_cap_is_not_headroom_and_wrong_user_is_rejected() {
        let mut u = user();
        u["teams"][0]["max_budget"] = json!(0);
        let mut t = team();
        t["team_info"]["team_member_budget_table"] = json!({"max_budget":-1});
        assert_eq!(
            fetch(key(), u.clone(), t, 200)
                .await
                .0
                .unwrap_err()
                .error_class(),
            "no_quota_reported"
        );
        u["user_info"]["user_id"] = json!("other-user");
        assert_eq!(
            fetch(key(), u, team(), 200)
                .await
                .0
                .unwrap_err()
                .error_class(),
            "decode_failed"
        );
    }

    #[test]
    fn configured_url_allows_https_and_private_http_only() {
        for raw in [
            "https://proxy.example.com",
            "http://127.0.0.1:4000/v1",
            "http://localhost",
            "http://localhost.",
            "http://10.0.0.1",
            "http://172.16.0.1",
            "http://192.168.1.1",
            "http://169.254.1.1",
            "http://[::1]",
            "http://[fc00::1]",
            "http://[fe80::1]",
            "http://[::ffff:192.168.1.1]",
            "http://proxy.local",
        ] {
            assert!(base_url(Some(raw)).is_ok(), "{raw}");
        }
        for raw in [
            "http://proxy.example.com",
            "http://8.8.8.8",
            "http://172.15.0.1",
            "http://172.32.0.1",
            "http://192.169.0.1",
            "http://[2001:4860:4860::8888]",
            "http://[::ffff:8.8.8.8]",
            "http://localhost.example.com",
            "http://proxy.local.example.com",
            "http://0.0.0.0",
            "http://[::]",
            "ftp://localhost",
            "https://user:secret@proxy.example.com",
            "https://proxy.example.com?key=secret",
            "https://proxy.example.com#secret",
            "not a URL",
            "",
        ] {
            let error = base_url(Some(raw)).unwrap_err();
            assert_eq!(error.error_class(), "credential_unusable", "{raw}");
            assert!(!error.to_string().contains("secret"));
        }
        assert_eq!(
            base_url(Some("https://proxy.example.com/prefix/v1/"))
                .unwrap()
                .as_str(),
            "https://proxy.example.com/prefix/"
        );
        assert!(base_url(None).is_err());
    }

    #[tokio::test]
    async fn key_refusals_and_transport_failures_are_not_spend_fallbacks() {
        for (status, class) in [
            (401, "credential_rejected"),
            (403, "credential_rejected"),
            (404, "upstream_failed"),
            (500, "upstream_failed"),
        ] {
            let (base, log) = routes(move |_, _| (status, b"{}".to_vec())).await;
            assert_eq!(
                provider(&base)
                    .read("fixture-key")
                    .await
                    .unwrap_err()
                    .error_class(),
                class
            );
            assert_eq!(paths(&log), ["/key/info"]);
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            provider(&base)
                .read("fixture-key")
                .await
                .unwrap_err()
                .error_class(),
            "upstream_failed"
        );
    }

    #[tokio::test]
    async fn redirects_are_never_followed_to_another_host() {
        use tokio::io::AsyncWriteExt;
        let (target, log) = routes(|_, _| (200, b"{}".to_vec())).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::loopback::read_request(&mut stream).await;
            let destination = target.replace("127.0.0.1", "localhost");
            stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {destination}/key/info\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        assert_eq!(
            provider(&base)
                .read("fixture-key")
                .await
                .unwrap_err()
                .error_class(),
            "upstream_failed"
        );
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn precise_amounts_do_not_round_and_unrepresentable_amounts_fail() {
        let b: Budget = serde_json::from_str(r#"{"max_budget":1,"spend":1e-7}"#).unwrap();
        let p = b.pool("personal", "Personal budget").unwrap().unwrap();
        assert_eq!(
            p.remaining.unwrap(),
            Amount {
                minor: 9_999_999,
                exponent: 7,
                unit: "USD".into()
            }
        );
        for body in [
            r#"{"max_budget":1e100}"#,
            r#"{"max_budget":1,"spend":1e-10}"#,
            r#"{"max_budget":9223372036854775807,"spend":0.1}"#,
        ] {
            assert!(serde_json::from_str::<Budget>(body)
                .unwrap()
                .pool("p", "P")
                .is_err());
        }
    }

    #[tokio::test]
    async fn vault_rows_replace_the_environment_and_report_only_primary_401s() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:litellm:fixture", "apikey")]);
        let (source, reports) =
            MockSource::serving(&[("apikey:litellm:fixture", credential("vault-key", 7))]);
        let (base, log) = routes(|_, _| (401, b"{}".to_vec())).await;
        let mut p = provider(&base);
        p.credential_source = Some(source);
        p.handle_loader = loader;
        p.env = Arc::new(|_| Some("environment-key".into()));
        let handles = p.handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert!(handles[0].is_vault());
        assert_eq!(
            p.fetch_handle(&handles[0])
                .await
                .usage
                .unwrap_err()
                .error_class(),
            "credential_rejected"
        );
        assert_eq!(
            log.lock().unwrap()[0].authorization.as_deref(),
            Some("Bearer vault-key")
        );
        // Auth reporting is fire-and-forget; yield until the spawned report runs.
        tokio::task::yield_now().await;
        assert_eq!(
            *reports.lock().unwrap(),
            [("apikey:litellm:fixture".into(), 401, 7)]
        );
    }

    /// LiteLLM's own SDK and proxy read `LITELLM_API_KEY` too, so the key alone
    /// is not a setup for this provider. Without `litellm.base_url` it reads as
    /// unconfigured, never as a broken credential, and sends nothing.
    #[tokio::test]
    async fn an_environment_key_without_a_base_url_is_unconfigured() {
        let (base, log) = routes(|_, _| (200, b"{}".to_vec())).await;
        let mut p = provider(&base);
        p.config = QuotaConfig::default().into();
        p.env = Arc::new(|_| Some("sdk-key".into()));
        let error = p
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "credential_absent");
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_config_is_read_again_on_the_next_fetch() {
        let (base, log) = routes(|_, _| (401, b"{}".to_vec())).await;
        let mut p = provider("http://public.example.com");
        p.env = Arc::new(|_| Some("fixture-key".into()));
        let dir = crate::tests::ResetTempDir::with_prefix("ck-quota-litellm", "config");
        let path = dir.path().join("config.jsonc");
        let mut reloader = crate::config::ConfigReloader::new(
            path.clone(),
            Box::new(|s| serde_json::from_str(s).map_err(|e| e.to_string())),
            p.config.clone(),
        );
        assert_eq!(
            p.fetch_handle(&CredentialHandle::implicit())
                .await
                .usage
                .unwrap_err()
                .error_class(),
            "credential_unusable",
            "a configured public HTTP URL is refused, not silently unconfigured"
        );
        std::fs::write(&path, json!({"litellm":{"base_url":base}}).to_string()).unwrap();
        reloader.check();
        assert_eq!(
            p.fetch_handle(&CredentialHandle::implicit())
                .await
                .usage
                .unwrap_err()
                .error_class(),
            "credential_rejected"
        );
        assert_eq!(paths(&log), ["/key/info"]);
    }
}
