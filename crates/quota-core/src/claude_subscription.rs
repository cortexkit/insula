//! Optional Claude web billing metadata; never a usage lane or a usage failure.
//!
//! The OAuth bearer used for usage supplies the authenticated owner; the vault
//! credential's account suffix only selects a candidate web cookie. Billing dates
//! are accepted only after matching the owner and organization on both sides.
//! Date precedence and owner matching follow CodexBar v0.74.0's
//! ClaudeSubscriptionMetadata.swift and ClaudeVerifiedAccountOwner.swift
//! (5349f629a, 36bf01ace).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    cookie_jar::CookieJar,
    credential_source::{CredentialSource, VaultCredential, VAULT_READ_MIN_TTL_MS},
    http::{Header, JsonRequest},
    provider::{CredentialHandle, FetchAttempt},
    vault_handles::VaultHandleLoader,
};

const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const ACCOUNT_URL: &str = "https://claude.ai/api/account";
const ORGANIZATIONS_URL: &str = "https://claude.ai/api/organizations";
const INTERVAL: Duration = Duration::from_secs(3600);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Dates {
    renews: Option<String>,
    ends: Option<String>,
}

fn parse_dates(body: &[u8]) -> Result<Dates, &'static str> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| "unreadable subscription details")?;
    let fields = value.as_object().ok_or("unreadable subscription details")?;
    let status = fields
        .get("status")
        .and_then(Value::as_str)
        .ok_or("unrecognized subscription status")?;
    if !["active", "trialing", "canceled"].contains(&status)
        || ![
            "next_charge_at",
            "next_charge_date",
            "plan_ending_at",
            "plan_ending_before",
        ]
        .iter()
        .all(|key| fields.contains_key(*key))
    {
        return Err("unrecognized subscription details");
    }
    let date = |key: &str| -> Result<Option<String>, &'static str> {
        match &fields[key] {
            Value::Null => Ok(None),
            Value::String(raw) => {
                if raw.len() == 10 {
                    let parsed = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                        .map_err(|_| "malformed billing date")?;
                    if parsed.format("%Y-%m-%d").to_string() != *raw {
                        return Err("malformed billing date");
                    }
                    // A date names a day, not midnight in an invented timezone.
                    Ok(Some(raw.clone()))
                } else {
                    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
                        .map_err(|_| "malformed billing date")?;
                    Ok(Some(parsed.to_rfc3339()))
                }
            }
            _ => Err("malformed billing date"),
        }
    };
    let ends = match date("plan_ending_at")? {
        Some(end) => Some(end),
        None => date("plan_ending_before")?,
    };
    let renews = if ends.is_none() && ["active", "trialing"].contains(&status) {
        match date("next_charge_at")? {
            Some(renewal) => Some(renewal),
            None => date("next_charge_date")?,
        }
    } else {
        None
    };
    Ok(Dates { renews, ends })
}

fn label(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Principal and organization obtained from authenticated profile responses.
/// UUID takes precedence over normalized email, so a shared email cannot hide
/// a UUID mismatch. The tuple is compared privately, never emitted as identity.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Owner {
    principal: String,
    by_uuid: bool,
    organization: String,
}

impl Owner {
    fn new(uuid: Option<&str>, email: Option<&str>, organization: Option<&str>) -> Option<Self> {
        let organization = label(organization)?;
        let uuid = label(uuid);
        let by_uuid = uuid.is_some();
        let principal = uuid.or_else(|| label(email).map(|s| s.to_lowercase()))?;
        Some(Self {
            principal,
            by_uuid,
            organization,
        })
    }

    fn profile(body: &[u8]) -> Option<Self> {
        let value: Value = serde_json::from_slice(body).ok()?;
        let string = |object: &Value, keys: &[&str]| {
            keys.iter().find_map(|key| {
                object
                    .get(*key)
                    .and_then(Value::as_str)
                    .and_then(|s| label(Some(s)))
            })
        };
        let uuid = string(&value["account"], &["uuid"])
            .or_else(|| string(&value, &["accountUuid", "account_uuid"]));
        let email = string(
            &value["account"],
            &["emailAddress", "email_address", "email"],
        )
        .or_else(|| string(&value, &["emailAddress", "email_address", "email"]));
        let org = string(&value["organization"], &["uuid"])
            .or_else(|| string(&value, &["organizationUuid", "organization_uuid"]));
        Self::new(uuid.as_deref(), email.as_deref(), org.as_deref())
    }
}

fn verified_organization(body: &[u8], owner: &Owner) -> Result<String, &'static str> {
    let account: Value = serde_json::from_slice(body).map_err(|_| "unverifiable web owner")?;
    let email = account["email_address"]
        .as_str()
        .ok_or("unverifiable web owner")?;
    let memberships = account["memberships"]
        .as_array()
        .ok_or("unverifiable web owner")?;
    let matches: Vec<_> = memberships
        .iter()
        .filter_map(|membership| {
            let id = membership["organization"]["uuid"].as_str()?;
            uuid::Uuid::parse_str(id).ok()?;
            let matches_owner = [None, account["uuid"].as_str()]
                .into_iter()
                .any(|uuid| Owner::new(uuid, Some(email), Some(id)).as_ref() == Some(owner));
            matches_owner.then(|| id.to_string())
        })
        .collect();
    if matches.len() != 1 {
        return Err("web owner mismatch or ambiguous organization");
    }
    Ok(matches[0].clone())
}

#[derive(Clone, PartialEq, Eq)]
struct Binding {
    oauth_id: String,
    oauth_version: u64,
    cookie_id: String,
    cookie_version: u64,
}

#[derive(Default)]
struct Cached {
    binding: Option<Binding>,
    attempted_at: Option<Instant>,
    good: Option<Dates>,
    refusal: Option<&'static str>,
}

/// Background work keeps billing timeouts off the usage poll's critical path.
/// The cache contains no bearer or cookie, and is shared by account rather than
/// credential suffix: two differently labelled vault credentials for the same
/// verified account must not each trigger an hourly billing request.
pub(crate) struct SubscriptionCache {
    http: reqwest::Client,
    profile_url: String,
    account_url: String,
    organizations_url: String,
    state: Mutex<HashMap<String, Cached>>,
}

impl SubscriptionCache {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            http: crate::http::provider_client(),
            profile_url: PROFILE_URL.into(),
            account_url: ACCOUNT_URL.into(),
            organizations_url: ORGANIZATIONS_URL.into(),
            state: Mutex::new(HashMap::new()),
        })
    }

    fn binding(
        loader: &VaultHandleLoader,
        handle: &CredentialHandle,
        credential: &VaultCredential,
    ) -> Result<Binding, &'static str> {
        let oauth_id = handle
            .vault_credential_id()
            .ok_or("not a scoped OAuth credential")?;
        let suffix = oauth_id
            .strip_prefix("oauth:anthropic:")
            .filter(|s| !s.is_empty())
            .ok_or("OAuth credential has no account suffix")?;
        let cookie_id = format!("cookie:claude.ai:{suffix}");
        let snapshot = loader.snapshot().ok_or("cookie snapshot unavailable")?;
        let row = snapshot
            .rows
            .iter()
            .find(|row| row.credential_id == cookie_id)
            .ok_or("no matching claude.ai deposit")?;
        if row.credential_type != "cookie" || row.state != "active" {
            return Err("claude.ai deposit unavailable");
        }
        Ok(Binding {
            oauth_id: oauth_id.into(),
            oauth_version: credential.record_version,
            cookie_id,
            cookie_version: row.record_version,
        })
    }

    fn settle(cached: &mut Cached, result: Result<Dates, &'static str>) {
        let refusal = result.as_ref().err().copied();
        if cached.refusal != refusal {
            match refusal {
                Some(reason) => eprintln!(
                    "{} claude subscription dates unavailable: {reason}",
                    crate::LOG_TAG
                ),
                None => eprintln!(
                    "{} claude subscription dates readable again",
                    crate::LOG_TAG
                ),
            }
        }
        cached.refusal = refusal;
        match result {
            Ok(dates) => cached.good = Some(dates),
            Err(
                "web owner mismatch or ambiguous organization"
                | "unverifiable web owner"
                | "unverifiable OAuth owner"
                | "OAuth owner changed",
            ) => cached.good = None,
            Err(_) => {} // A dates transport/parse failure keeps the last verified answer.
        }
    }

    pub(crate) fn queue(
        self: &Arc<Self>,
        source: Arc<dyn CredentialSource>,
        loader: &VaultHandleLoader,
        handle: &CredentialHandle,
        credential: &VaultCredential,
        bearer: &str,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let Some(account) = label(credential.account_id.as_deref()) else {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cached = state
                .entry(format!("unverified:{}", handle.stable_id()))
                .or_default();
            cached.good = None;
            Self::settle(cached, Err("OAuth row has no verified account identity"));
            return None;
        };
        let binding = Self::binding(loader, handle, credential);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = state.entry(account.clone()).or_default();
        let binding = match binding {
            Ok(binding) => binding,
            Err(reason) => {
                cached.good = None;
                cached.binding = None;
                Self::settle(cached, Err(reason));
                return None;
            }
        };
        if cached.binding.as_ref() != Some(&binding) {
            cached.good = None;
            cached.binding = Some(binding.clone());
        }
        let now = Instant::now();
        if cached
            .attempted_at
            .is_some_and(|last| now.duration_since(last) < INTERVAL)
        {
            return None;
        }
        cached.attempted_at = Some(now);
        drop(state);
        let this = Arc::clone(self);
        let bearer = bearer.to_string();
        Some(tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(12),
                this.fetch(&source, &binding, &bearer),
            )
            .await
            .unwrap_or(Err("billing request timed out"));
            let mut state = this
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = state.get_mut(&account) {
                // A credential replacement during the request cannot publish an
                // answer verified against the old token or cookie.
                if cached.binding.as_ref() == Some(&binding) {
                    Self::settle(cached, result);
                }
            }
        }))
    }

    pub(crate) fn apply(
        &self,
        loader: &VaultHandleLoader,
        handle: &CredentialHandle,
        credential: &VaultCredential,
        attempt: &mut FetchAttempt,
    ) {
        let Some(account) = label(credential.account_id.as_deref()) else {
            return;
        };
        let Ok(binding) = Self::binding(loader, handle, credential) else {
            return;
        };
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(cached) = state
            .get(&account)
            .filter(|cached| cached.binding.as_ref() == Some(&binding))
        else {
            return;
        };
        // Billing refusals do not alter usage or its error classification.
        let Some(dates) = &cached.good else {
            return;
        };
        let mut info = attempt.account_info.take().unwrap_or_default();
        info.subscription_renews_at = dates.renews.clone();
        info.subscription_ends_at = dates.ends.clone();
        attempt.account_info = (!info.is_empty()).then_some(info);
    }

    async fn oauth_owner(&self, bearer: &str) -> Result<Owner, &'static str> {
        let body = JsonRequest::get(&self.profile_url)
            .timeout(REQUEST_TIMEOUT)
            .bearer(bearer)
            .header(Header::new("Accept", "application/json"))
            .header(Header::new("Content-Type", "application/json"))
            .send_full(&self.http)
            .await
            .map_err(|_| "unverifiable OAuth owner")?;
        if body.status != 200 {
            return Err("unverifiable OAuth owner");
        }
        Owner::profile(&body.body).ok_or("unverifiable OAuth owner")
    }

    async fn web_get(&self, url: &str, cookie: &str) -> Result<Vec<u8>, &'static str> {
        let response = JsonRequest::get(url)
            .timeout(REQUEST_TIMEOUT)
            .header(Header::new("Cookie", cookie))
            .header(Header::new("Accept", "application/json"))
            .send_full(&self.http)
            .await
            .map_err(|_| "web billing request failed")?;
        if response.status != 200 {
            return Err("web billing request failed");
        }
        Ok(response.body)
    }

    async fn fetch(
        &self,
        source: &Arc<dyn CredentialSource>,
        binding: &Binding,
        bearer: &str,
    ) -> Result<Dates, &'static str> {
        let handle = CredentialHandle::scoped(&binding.cookie_id, "cookie");
        let mut credential =
            crate::credential_source::get_vault_credential(source, &handle, VAULT_READ_MIN_TTL_MS)
                .await
                .map_err(|_| "claude.ai deposit unreadable")?;
        if credential.record_version != binding.cookie_version {
            return Err("claude.ai deposit changed");
        }
        let header = crate::credential_source::take_utf8_payload(&mut credential.payload)
            .map_err(|_| "claude.ai deposit unreadable")?;
        let jar = CookieJar::from_header(&header);
        let key = jar
            .cookies
            .iter()
            .find(|cookie| cookie.name == "sessionKey" && cookie.value.starts_with("sk-ant-"))
            .map(|cookie| cookie.value.as_str())
            .ok_or("no sessionKey in claude.ai deposit")?;
        let cookie = format!("sessionKey={key}");
        let owner = self.oauth_owner(bearer).await?;
        let organization =
            verified_organization(&self.web_get(&self.account_url, &cookie).await?, &owner)?;
        let url = format!(
            "{}/{organization}/subscription_details",
            self.organizations_url
        );
        let dates = parse_dates(&self.web_get(&url, &cookie).await?)?;
        if verified_organization(&self.web_get(&self.account_url, &cookie).await?, &owner)?
            != organization
        {
            return Err("web owner mismatch or ambiguous organization");
        }
        if self.oauth_owner(bearer).await? != owner {
            return Err("OAuth owner changed");
        }
        Ok(dates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_source::{VaultCapability, VaultGetError};
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    const UUID: &str = "11111111-1111-4111-8111-111111111111";
    const ORG: &str = "22222222-2222-4222-8222-222222222222";

    fn details(status: &str, renewal: Value, date: Value, end: Value, before: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"status":status, "next_charge_at":renewal,
            "next_charge_date":date, "plan_ending_at":end, "plan_ending_before":before}))
        .unwrap()
    }

    fn active() -> Vec<u8> {
        details(
            "active",
            json!("2026-11-01T12:34:56Z"),
            Value::Null,
            Value::Null,
            Value::Null,
        )
    }

    #[test]
    fn active_next_charge_at_is_renewal() {
        assert_eq!(
            parse_dates(&active()).unwrap(),
            Dates {
                renews: Some("2026-11-01T12:34:56+00:00".into()),
                ends: None
            }
        );
    }

    #[test]
    fn date_only_renewal_stays_a_full_date() {
        let parsed = parse_dates(&details(
            "active",
            Value::Null,
            json!("2026-11-01"),
            Value::Null,
            Value::Null,
        ))
        .unwrap();
        assert_eq!(
            parsed.renews.as_deref(),
            Some("2026-11-01"),
            "a date-only value must remain a day, not an invented timestamp"
        );
    }

    #[test]
    fn canceled_scheduled_end_wins_over_leftover_renewal() {
        let parsed = parse_dates(&details(
            "canceled",
            json!("2026-11-01T12:34:56Z"),
            Value::Null,
            json!("2026-10-31T23:00:00Z"),
            Value::Null,
        ))
        .unwrap();
        assert_eq!(parsed.ends.as_deref(), Some("2026-10-31T23:00:00+00:00"));
        assert_eq!(
            parsed.renews, None,
            "a scheduled end must suppress a leftover next charge"
        );
        let active_end = parse_dates(&details(
            "active",
            json!("2026-11-01T12:34:56Z"),
            Value::Null,
            Value::Null,
            json!("2026-10-31"),
        ))
        .unwrap();
        assert_eq!(active_end.renews, None);
        assert_eq!(active_end.ends.as_deref(), Some("2026-10-31"));
    }

    #[test]
    fn trialing_can_renew_but_canceled_without_end_cannot() {
        let parsed = parse_dates(&details(
            "trialing",
            Value::Null,
            json!("2026-11-01"),
            Value::Null,
            Value::Null,
        ))
        .unwrap();
        assert_eq!(parsed.renews.as_deref(), Some("2026-11-01"));
        assert_eq!(
            parse_dates(&details(
                "canceled",
                json!("invalid unused renewal"),
                Value::Null,
                Value::Null,
                Value::Null
            ))
            .unwrap(),
            Dates::default()
        );
    }

    #[test]
    fn unknown_subscription_status_is_rejected() {
        assert!(parse_dates(&details(
            "paused",
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ))
        .is_err());
    }

    #[test]
    fn missing_required_subscription_key_is_rejected() {
        let mut value: Value = serde_json::from_slice(&active()).unwrap();
        value.as_object_mut().unwrap().remove("plan_ending_before");
        assert!(parse_dates(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn malformed_billing_date_is_rejected() {
        for bad in [
            json!("2026-02-30"),
            json!("2026-1-001"),
            json!("not-a-date"),
            json!(42),
            json!("2026-11-01TnoonZ"),
        ] {
            assert!(
                parse_dates(&details(
                    "active",
                    Value::Null,
                    bad.clone(),
                    Value::Null,
                    Value::Null
                ))
                .is_err(),
                "accepted {bad}"
            );
        }
        // A valid next_charge_at takes precedence over next_charge_date; the
        // unused date fallback is not parsed, even when malformed.
        assert!(parse_dates(&details(
            "active",
            json!("2026-11-01T12:34:56Z"),
            json!("unused invalid"),
            Value::Null,
            Value::Null
        ))
        .is_ok());
    }

    fn profile() -> Vec<u8> {
        serde_json::to_vec(
            &json!({"account":{"uuid":UUID, "email_address":"user@example.test"},
            "organization":{"uuid":ORG}}),
        )
        .unwrap()
    }

    fn web(uuid: Option<&str>) -> Vec<u8> {
        serde_json::to_vec(&json!({"uuid":uuid, "email_address":" USER@example.test ",
            "memberships":[{"organization":{"uuid":ORG}}]}))
        .unwrap()
    }

    #[test]
    fn owner_prefers_uuid_and_requires_a_unique_matching_organization() {
        let owner = Owner::profile(&profile()).unwrap();
        assert!(verified_organization(&web(Some("different-uuid")), &owner).is_err());
        assert!(
            verified_organization(&web(None), &owner).is_err(),
            "email must not override an OAuth UUID"
        );
        let email_owner = Owner::profile(
            &serde_json::to_vec(
                &json!({"emailAddress":"user@example.test", "organizationUuid":ORG}),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            verified_organization(&web(None), &email_owner).unwrap(),
            ORG
        );
        let mut duplicate: Value = serde_json::from_slice(&web(Some(UUID))).unwrap();
        duplicate["memberships"]
            .as_array_mut()
            .unwrap()
            .push(json!({"organization":{"uuid":ORG}}));
        assert!(verified_organization(&serde_json::to_vec(&duplicate).unwrap(), &owner).is_err());
        duplicate["memberships"] = json!([{"organization":{"uuid":"not-a-uuid"}}]);
        assert!(verified_organization(&serde_json::to_vec(&duplicate).unwrap(), &owner).is_err());
        assert!(Owner::profile(br#"{"account":{"uuid":"a"}}"#).is_none());
    }

    struct Source {
        reports: Mutex<usize>,
    }
    fn credential() -> VaultCredential {
        VaultCredential {
            payload: b"usage-bearer".to_vec(),
            expires_at_ms: None,
            record_version: 1,
            account_id: Some("verified-vault-account".into()),
            project_id: None,
            email: Some("user@example.test".into()),
            org_name: None,
        }
    }
    #[async_trait]
    impl CredentialSource for Source {
        async fn get(&self, _: &VaultCapability, _: u64) -> Result<VaultCredential, VaultGetError> {
            Err(VaultGetError::FailClosed)
        }
        async fn get_scoped(&self, id: &str, ttl: u64) -> Result<VaultCredential, VaultGetError> {
            assert_eq!(ttl, VAULT_READ_MIN_TTL_MS);
            let mut credential = credential();
            if id == "cookie:claude.ai:label" {
                credential.payload = b"sessionKey=sk-ant-test; tracking=not-forwarded".to_vec();
            } else {
                assert_eq!(id, "oauth:anthropic:label");
            }
            Ok(credential)
        }
        async fn report_auth_failure(&self, _: &VaultCapability, _: u16, _: u64) {
            *self.reports.lock().unwrap() += 1;
        }
        async fn report_auth_failure_scoped(&self, _: &str, _: u16, _: u64) {
            *self.reports.lock().unwrap() += 1;
        }
    }

    struct Fixture {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
        billing_status: Arc<Mutex<u16>>,
        account: Arc<Mutex<Vec<u8>>>,
        profile: Arc<Mutex<Vec<u8>>>,
        account_after_billing: Arc<Mutex<Option<Vec<u8>>>>,
        profile_after_billing: Arc<Mutex<Option<Vec<u8>>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    impl Fixture {
        async fn new(account: Vec<u8>, billing_status: u16) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let status = Arc::new(Mutex::new(billing_status));
            let account = Arc::new(Mutex::new(account));
            let profile = Arc::new(Mutex::new(profile()));
            let recorded = Arc::clone(&requests);
            let served_status = Arc::clone(&status);
            let served_account = Arc::clone(&account);
            let served_profile = Arc::clone(&profile);
            let account_after_billing = Arc::new(Mutex::new(None));
            let profile_after_billing = Arc::new(Mutex::new(None));
            let next_account = Arc::clone(&account_after_billing);
            let next_profile = Arc::clone(&profile_after_billing);
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let request = crate::loopback::read_request(&mut socket).await;
                    let path = request.split_whitespace().nth(1).unwrap();
                    let (status, body) = match path {
                        "/api/oauth/profile" => (200, served_profile.lock().unwrap().clone()),
                        "/api/account" => (200, served_account.lock().unwrap().clone()),
                        p if p.ends_with("/subscription_details") => {
                            if let Some(body) = next_account.lock().unwrap().take() { *served_account.lock().unwrap() = body; }
                            if let Some(body) = next_profile.lock().unwrap().take() { *served_profile.lock().unwrap() = body; }
                            (*served_status.lock().unwrap(), active())
                        },
                        "/usage?cedar_ember=1" => (200, br#"{"five_hour":{"utilization":12,"resets_at":null},"seven_day":{"utilization":34,"resets_at":null}}"#.to_vec()),
                        _ => panic!("unexpected fixture request: {request}"),
                    };
                    recorded.lock().unwrap().push(request);
                    socket.write_all(format!("HTTP/1.1 {status} Response\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                }
            });
            Self {
                base,
                requests,
                billing_status: status,
                account,
                profile,
                account_after_billing,
                profile_after_billing,
                task,
            }
        }
        fn cache(&self) -> Arc<SubscriptionCache> {
            Arc::new(SubscriptionCache {
                http: crate::http::provider_client(),
                profile_url: format!("{}/api/oauth/profile", self.base),
                account_url: format!("{}/api/account", self.base),
                organizations_url: format!("{}/api/organizations", self.base),
                state: Mutex::new(HashMap::new()),
            })
        }
        fn billing_count(&self) -> usize {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.contains("/subscription_details"))
                .count()
        }
    }

    fn setup() -> (Arc<Source>, Arc<VaultHandleLoader>, CredentialHandle) {
        let source = Arc::new(Source {
            reports: Mutex::new(0),
        });
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[
            ("oauth:anthropic:label", "oauth"),
            ("cookie:claude.ai:label", "cookie"),
        ]);
        (
            source,
            loader,
            CredentialHandle::scoped("oauth:anthropic:label", "oauth"),
        )
    }

    async fn warm(
        cache: &Arc<SubscriptionCache>,
        source: &Arc<Source>,
        loader: &VaultHandleLoader,
        handle: &CredentialHandle,
    ) {
        let source: Arc<dyn CredentialSource> = source.clone();
        cache
            .queue(source, loader, handle, &credential(), "usage-bearer")
            .expect("first poll queues billing")
            .await
            .unwrap();
    }

    async fn row(
        cache: Arc<SubscriptionCache>,
        fixture: &Fixture,
        source: Arc<Source>,
        loader: Arc<VaultHandleLoader>,
    ) -> crate::model::ProviderUsage {
        let provider = crate::anthropic::AnthropicProvider::subscription_fixture(
            source,
            loader,
            format!("{}/usage", fixture.base),
            cache,
        );
        let registry = crate::Registry::new(vec![Box::new(provider)]);
        registry
            .refresh_tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let mut entries = registry.get_usage(Some("claude")).await;
        assert_eq!(
            entries.len(),
            1,
            "cookie enrichment must not become another Claude row"
        );
        entries.remove(0)
    }

    #[tokio::test]
    async fn matching_owner_attaches_dates_to_the_oauth_row() {
        let fixture = Fixture::new(web(Some(UUID)), 200).await;
        let cache = fixture.cache();
        let (source, loader, handle) = setup();
        warm(&cache, &source, &loader, &handle).await;
        let entry = row(cache, &fixture, source.clone(), loader).await;
        assert_eq!(entry.account.as_deref(), Some("verified-vault-account"));
        assert_eq!(
            entry
                .account_info
                .unwrap()
                .subscription_renews_at
                .as_deref(),
            Some("2026-11-01T12:34:56+00:00")
        );
        assert_eq!(entry.usage.unwrap().primary.unwrap().used_percent, 12.0);
        assert_eq!(fixture.billing_count(), 1);
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.contains("/api/account"))
                .count(),
            2,
            "web principal must be rechecked"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.contains("/api/oauth/profile"))
                .count(),
            2,
            "OAuth principal must be rechecked"
        );
        for request in requests
            .iter()
            .filter(|r| r.contains("/api/account") || r.contains("/subscription_details"))
        {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("cookie: sessionkey=sk-ant-test"));
            assert!(!lower.contains("tracking="));
            assert!(!lower.contains("authorization:"));
        }
        assert_eq!(*source.reports.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn mismatched_owner_attaches_nothing() {
        let fixture = Fixture::new(web(Some("33333333-3333-4333-8333-333333333333")), 200).await;
        let cache = fixture.cache();
        let (source, loader, handle) = setup();
        warm(&cache, &source, &loader, &handle).await;
        let entry = row(cache, &fixture, source, loader).await;
        assert_eq!(
            entry.account_info.unwrap().subscription_renews_at,
            None,
            "a mismatched web owner must not attach billing dates"
        );
        assert_eq!(fixture.billing_count(), 0);
    }

    #[tokio::test]
    async fn unverifiable_owner_attaches_nothing() {
        let fixture = Fixture::new(br#"{"email_address":"user@example.test"}"#.to_vec(), 200).await;
        let cache = fixture.cache();
        let (source, loader, handle) = setup();
        warm(&cache, &source, &loader, &handle).await;
        let entry = row(cache, &fixture, source, loader).await;
        assert_eq!(entry.account_info.unwrap().subscription_renews_at, None);
        assert_eq!(fixture.billing_count(), 0);
    }

    #[tokio::test]
    async fn dates_failure_leaves_windows_and_error_class_unchanged() {
        let fixture = Fixture::new(web(Some(UUID)), 401).await;
        let cache = fixture.cache();
        let (source, loader, handle) = setup();
        warm(&cache, &source, &loader, &handle).await;
        let entry = row(cache, &fixture, source.clone(), loader).await;
        assert_eq!(
            entry.error_class, None,
            "a dates failure must never set the Claude row's errorClass"
        );
        let usage = entry.usage.expect("billing failure cannot lose windows");
        assert_eq!(usage.primary.unwrap().used_percent, 12.0);
        assert_eq!(usage.secondary.unwrap().used_percent, 34.0);
        assert_eq!(entry.account_info.unwrap().subscription_renews_at, None);
        assert_eq!(
            *source.reports.lock().unwrap(),
            0,
            "cookie failures must not report auth failure to the vault"
        );
    }

    #[tokio::test]
    async fn a_second_poll_inside_the_hour_makes_no_dates_request() {
        let fixture = Fixture::new(web(Some(UUID)), 200).await;
        let cache = fixture.cache();
        let (source, loader, handle) = setup();
        warm(&cache, &source, &loader, &handle).await;
        assert!(cache
            .queue(
                source.clone(),
                &loader,
                &handle,
                &credential(),
                "usage-bearer"
            )
            .is_none());
        let entry = row(cache, &fixture, source, loader).await;
        assert!(entry.account_info.unwrap().subscription_renews_at.is_some());
        assert_eq!(fixture.billing_count(), 1);
    }

    #[tokio::test]
    async fn later_dates_failure_keeps_last_good_but_changed_owner_withholds_it() {
        let fixture = Fixture::new(web(Some(UUID)), 200).await;
        let cache = fixture.cache();
        let (source, loader, handle) = setup();
        warm(&cache, &source, &loader, &handle).await;
        let expire = || {
            cache
                .state
                .lock()
                .unwrap()
                .get_mut("verified-vault-account")
                .unwrap()
                .attempted_at = Some(Instant::now() - INTERVAL)
        };
        *fixture.billing_status.lock().unwrap() = 503;
        expire();
        warm(&cache, &source, &loader, &handle).await;
        let entry = row(cache.clone(), &fixture, source.clone(), loader.clone()).await;
        assert!(entry.account_info.unwrap().subscription_renews_at.is_some());
        *fixture.account.lock().unwrap() = web(Some("wrong-owner"));
        expire();
        warm(&cache, &source, &loader, &handle).await;
        let entry = row(cache, &fixture, source, loader).await;
        assert_eq!(entry.account_info.unwrap().subscription_renews_at, None);
    }

    #[tokio::test]
    async fn an_owner_change_during_billing_withholds_dates() {
        for change_oauth in [false, true] {
            let fixture = Fixture::new(web(Some(UUID)), 200).await;
            if change_oauth {
                let mut changed: Value = serde_json::from_slice(&profile()).unwrap();
                changed["account"]["uuid"] = json!("changed-owner");
                *fixture.profile_after_billing.lock().unwrap() =
                    Some(serde_json::to_vec(&changed).unwrap());
            } else {
                *fixture.account_after_billing.lock().unwrap() = Some(web(Some("changed-owner")));
            }
            let cache = fixture.cache();
            let (source, loader, handle) = setup();
            warm(&cache, &source, &loader, &handle).await;
            let entry = row(cache, &fixture, source, loader).await;
            assert_eq!(entry.account_info.unwrap().subscription_renews_at, None);
            assert_eq!(
                fixture.billing_count(),
                1,
                "the owner change must be detected after a real dates request"
            );
        }
    }

    #[tokio::test]
    async fn a_stalled_billing_owner_request_does_not_delay_usage() {
        let fixture = Fixture::new(web(Some(UUID)), 200).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let profile_url = format!(
            "http://{}/api/oauth/profile",
            listener.local_addr().unwrap()
        );
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let stalled = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            crate::loopback::read_request(&mut socket).await;
            entered_tx.send(()).unwrap();
            let _ = release_rx.await;
        });
        let mut cache = fixture.cache();
        Arc::get_mut(&mut cache).unwrap().profile_url = profile_url;
        let (source, loader, _) = setup();
        let entry =
            tokio::time::timeout(Duration::from_secs(1), row(cache, &fixture, source, loader))
                .await
                .expect("OAuth windows must publish without waiting for billing");
        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.error_class, None);
        assert_eq!(entry.usage.unwrap().primary.unwrap().used_percent, 12.0);
        stalled.abort();
    }

    #[tokio::test]
    async fn missing_oauth_owner_and_changed_credentials_withhold_cached_dates() {
        let fixture = Fixture::new(web(Some(UUID)), 200).await;
        let cache = fixture.cache();
        let (source, loader, handle) = setup();
        *fixture.profile.lock().unwrap() = br#"{"email_address":"user@example.test"}"#.to_vec();
        warm(&cache, &source, &loader, &handle).await;
        assert_eq!(fixture.billing_count(), 0);
        *fixture.profile.lock().unwrap() = profile();
        cache
            .state
            .lock()
            .unwrap()
            .get_mut("verified-vault-account")
            .unwrap()
            .attempted_at = Some(Instant::now() - INTERVAL);
        warm(&cache, &source, &loader, &handle).await;
        let mut changed = credential();
        changed.record_version = 2;
        assert!(
            cache
                .queue(source.clone(), &loader, &handle, &changed, "new-bearer")
                .is_none(),
            "credential rotation must not bypass the per-account hour"
        );
        let mut attempt = FetchAttempt::success(None, "vault", crate::model::Usage::default());
        cache.apply(&loader, &handle, &changed, &mut attempt);
        assert_eq!(attempt.account_info, None);
    }
}
