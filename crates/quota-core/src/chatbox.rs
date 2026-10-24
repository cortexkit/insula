//! Chatbox AI — a license's plan allowance and its separate credit pools, read
//! with the license key.
//!
//! - Endpoint, on `https://api.chatboxai.app` only:
//!   `GET /api/license/detail/realtime`, with the license key as the whole
//!   `Authorization` header value (no `Bearer` prefix, as Chatbox's own client
//!   sends it) and `Cache-Control: no-store`.
//! - Reference: CodexBar v0.81.0
//!   `Sources/CodexBarCore/Resources/Plugins/chatbox.ts` and `docs/chatbox.md`,
//!   which cite Chatbox's public client for the request and the response shape.
//!   Tested against fixtures from upstream's `ChatboxPluginTests.swift` only; no
//!   Chatbox license was available when this was written.
//!
//! The answer is `{"data": {...}}`, or an `error` object. `data` carries
//! several counters that overlap, so each is published apart and none is added
//! to another:
//!
//! - The plan: the one `type: "plan"` item in `unified_token_usage_details`
//!   (`token_usage` / `token_limit`). While the plan is usable it is the
//!   `primary` window. Usable means `status` is absent or `active`, the plan has
//!   not expired, and its limit is positive. The window's `resetsAt` is
//!   `token_next_refresh_time`, but only when that time is in the future and
//!   falls before the plan's expiry. Chatbox names no cadence, so the window has
//!   no `windowMinutes` and no `windowKind`. A plan that is not usable publishes
//!   nothing: its remainder cannot be drawn on, and it is not a credit balance.
//! - The plan's expiry (`token_expire_time`, else `expires_at`) is when the
//!   license ends. It is published as `accountInfo.subscriptionEndsAt` and is
//!   never a reset.
//! - Credits that do not refill are `Pool`s, in their own unit, never carrying
//!   the plan's reset: rewards (`aggregated_reward_details`, or else the sum of
//!   the unexpired non-plan detail items), top-up credits
//!   (`expansion_pack_usage` / `expansion_pack_limit`) and image generations
//!   (`image_used_count` / `image_total_quota`).
//! - The combined `unified_token_usage` / `unified_token_limit` already
//!   includes the plan and the rewards. It is published as a pool only when no
//!   plan item exists (older answers), exactly as upstream shows it, so the plan
//!   and the combined total are never both published.
//!
//! A pool whose used and limit are both zero is not published, as upstream
//! hides it. When nothing at all is left to publish (no usable plan and no
//! non-empty pool), the reading is `no_quota_reported`. An answer upstream
//! refuses (not JSON, no `data` object, an `error`, a counter that is not a
//! non-negative safe integer, two plan items, or no counter at all) is
//! `decode_failed`, and the body is never quoted.
//!
//! Credential: a vault deposit under `apikey:chatbox` when one exists, otherwise
//! `CHATBOX_LICENSE_KEY`, the only variable upstream reads.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::sync::Arc;

use crate::credential_source::CredentialSource;
use crate::http::{Header, JsonRequest};
use crate::model::{AccountInfo, Amount, Pool, PoolBasis, PoolFunding, RateWindow, Usage};
use crate::provider::{CredentialHandle, FetchAttempt, FetchError, HandlesError, UsageProvider};
use crate::vault_handles::VaultHandleLoader;
use crate::vercel::{default_env, EnvLookup};

pub const PROVIDER_NAME: &str = "chatbox";
/// The only origin the license key is ever sent to.
const API_BASE: &str = "https://api.chatboxai.app";
const DETAIL_PATH: &str = "/api/license/detail/realtime";
const KEY_ENV: &str = "CHATBOX_LICENSE_KEY";
/// Upstream refuses a longer key before sending it (counted in UTF-16 units,
/// as JavaScript counts string length).
const MAX_KEY_UTF16_UNITS: usize = 4096;
/// The largest integer a JavaScript number holds exactly. Upstream refuses any
/// counter, and any sum of reward counters, above it.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// Upstream refuses a detail list longer than this.
const MAX_DETAIL_ITEMS: usize = 100;
/// Upstream keeps at most this many characters of a name or status.
const MAX_TEXT_CHARS: usize = 100;
/// Chatbox calls the token allowances compute points; they are neither money
/// nor raw model tokens.
const POINT_UNIT: &str = "chatbox-compute-point";
const IMAGE_UNIT: &str = "chatbox-image";

/// Reads the current time. Injected so tests can pin the fixture's date.
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Decode failures never quote the body: an unexpected answer may echo the
/// license or the license's own identifiers.
fn decode(what: &str) -> FetchError {
    FetchError::Decode(format!("chatbox: {what}"))
}

/// Whether upstream would refuse this key before sending it: longer than 4096
/// UTF-16 units, or containing whitespace or a control character.
///
/// JavaScript's `\s` also matches U+FEFF, which Rust does not count as
/// whitespace, so it is checked by name.
fn key_is_malformed(key: &str) -> bool {
    key.is_empty()
        || key.encode_utf16().count() > MAX_KEY_UTF16_UNITS
        || key.chars().any(|c| {
            let code = c as u32;
            c.is_whitespace() || c == '\u{feff}' || code < 32 || (127..=159).contains(&code)
        })
}

/// JavaScript truthiness, which upstream's `root.error` test relies on: an
/// `error` of `null`, `false`, `0` or `""` does not fail the answer.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A counter as upstream accepts it: absent or null is `None`; anything other
/// than a non-negative safe integer fails the answer.
fn counter(value: Option<&Value>, field: &str) -> Result<Option<u64>, FetchError> {
    let refused = || decode(&format!("{field} is not a non-negative safe integer"));
    let number = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(number)) => number,
        Some(_) => return Err(refused()),
    };
    let whole = match number.as_u64() {
        Some(whole) => Some(whole),
        // JSON may spell a whole number with a fraction or an exponent
        // (`1.0`, `1e3`), which JavaScript reads as the same integer.
        None => number
            .as_f64()
            .filter(|n| n.fract() == 0.0 && *n >= 0.0 && *n <= MAX_SAFE_INTEGER as f64)
            .map(|n| n as u64),
    };
    match whole {
        Some(whole) if whole <= MAX_SAFE_INTEGER => Ok(Some(whole)),
        _ => Err(refused()),
    }
}

/// A used figure and a limit, from one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counted {
    used: u64,
    limit: u64,
}

impl Counted {
    /// Upstream hides a pool whose used and limit are both zero.
    fn is_empty(self) -> bool {
        self.used == 0 && self.limit == 0
    }
}

/// Both counters, or `None` when either is absent. Each is checked even when
/// the other is absent, as upstream checks both.
fn counted(
    object: &Map<String, Value>,
    used: &str,
    limit: &str,
) -> Result<Option<Counted>, FetchError> {
    let used_value = counter(object.get(used), used)?;
    let limit_value = counter(object.get(limit), limit)?;
    Ok(used_value
        .zip(limit_value)
        .map(|(used, limit)| Counted { used, limit }))
}

/// A short label as upstream cleans it: control characters become spaces, the
/// result is trimmed and cut to 100 characters, and an empty one is absent.
fn text(value: Option<&Value>) -> Option<String> {
    let cleaned: String = value?
        .as_str()?
        .chars()
        .map(|c| if (c as u32) < 32 || c as u32 == 127 { ' ' } else { c })
        .collect();
    let trimmed: String = cleaned.trim().chars().take(MAX_TEXT_CHARS).collect();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// A timestamp as upstream accepts it: a string starting `YYYY-MM-DDT` that
/// parses, in 1970 or later. Anything else is absent, never a failure.
///
/// Read at millisecond precision, the precision upstream compares at. A
/// timestamp with no zone offset is absent here: JavaScript reads it in the
/// host's local zone, which would make the reset depend on where this runs.
fn date(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let text = value?.as_str()?;
    let bytes = text.as_bytes();
    let shaped = bytes.len() > 10
        && bytes[..10]
            .iter()
            .enumerate()
            .all(|(i, b)| if i == 4 || i == 7 { *b == b'-' } else { b.is_ascii_digit() })
        && bytes[10] == b'T';
    if !shaped {
        return None;
    }
    let parsed = DateTime::parse_from_rfc3339(text).ok()?.with_timezone(&Utc);
    let parsed = DateTime::from_timestamp_millis(parsed.timestamp_millis())?;
    (parsed.timestamp() >= 0).then_some(parsed)
}

fn iso(at: DateTime<Utc>) -> Option<String> {
    crate::env::epoch_to_iso8601(at.timestamp())
}

/// What one license answer publishes.
#[derive(Debug)]
struct Reading {
    usage: Usage,
    pools: Vec<Pool>,
    account_info: Option<AccountInfo>,
}

/// One credit pool. None of them carries a reset: only the plan refills, and
/// the plan is the window, not a pool.
fn pool(id: &str, label: &str, funding: PoolFunding, unit: &str, counted: Counted) -> Pool {
    let amount = |value: u64| Amount {
        // Every counter is at most 2^53 - 1, so it fits an i64.
        minor: value as i64,
        exponent: 0,
        unit: unit.to_string(),
    };
    Pool {
        id: id.to_string(),
        label: label.to_string(),
        funding,
        remaining: Some(amount(counted.limit.saturating_sub(counted.used))),
        total: Some(amount(counted.limit)),
        // Chatbox states a limit and a usage figure, not a remainder.
        basis: PoolBasis::Derived,
        spendable: None,
        resets_at: None,
        expires_at: None,
        unlimited: None,
    }
}

fn parse(body: &[u8], now: DateTime<Utc>) -> Result<Reading, FetchError> {
    let root = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(root)) => root,
        Ok(_) => return Err(decode("the answer is not a JSON object")),
        Err(_) => return Err(decode("the answer is not JSON")),
    };
    let Some(Value::Object(data)) = root.get("data") else {
        return Err(decode("the answer carries no data object"));
    };
    if root.get("error").is_some_and(truthy) {
        return Err(decode("the answer carries an error"));
    }

    let mut plan: Option<Counted> = None;
    let mut detail_rewards: Option<Counted> = None;
    match data.get("unified_token_usage_details") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) if items.len() <= MAX_DETAIL_ITEMS => {
            for item in items {
                let Some(item) = item.as_object() else {
                    return Err(decode("a usage detail is not an object"));
                };
                let Some(kind) = item.get("type").and_then(Value::as_str) else {
                    return Err(decode("a usage detail has no type"));
                };
                let value = counted(item, "token_usage", "token_limit")?;
                if kind == "plan" {
                    // Upstream refuses a second plan only after one with
                    // readable counters; a plan item without them is no plan.
                    if plan.is_some() {
                        return Err(decode("the usage details list two plans"));
                    }
                    plan = value;
                } else if let Some(value) = value {
                    if date(item.get("expires_at")).is_some_and(|at| at <= now) {
                        continue;
                    }
                    let sum = detail_rewards.map_or(value, |held| Counted {
                        used: held.used.saturating_add(value.used),
                        limit: held.limit.saturating_add(value.limit),
                    });
                    if sum.used > MAX_SAFE_INTEGER || sum.limit > MAX_SAFE_INTEGER {
                        return Err(decode("the reward details sum past a safe integer"));
                    }
                    detail_rewards = Some(sum);
                }
            }
        }
        Some(_) => return Err(decode("unified_token_usage_details is not a short list")),
    }

    let total = counted(data, "unified_token_usage", "unified_token_limit")?;
    let rewards = match data.get("aggregated_reward_details") {
        None | Some(Value::Null) => detail_rewards,
        Some(Value::Object(aggregated)) => counted(aggregated, "token_usage", "token_limit")?,
        Some(_) => return Err(decode("aggregated_reward_details is not an object")),
    };
    let expansion = counted(data, "expansion_pack_usage", "expansion_pack_limit")?;
    let images = counted(data, "image_used_count", "image_total_quota")?;
    if plan.is_none()
        && total.is_none()
        && rewards.is_none()
        && expansion.is_none()
        && images.is_none()
    {
        return Err(decode("the answer states no counter"));
    }

    let name = text(data.get("name"));
    let status = text(data.get("status"));
    let expires = date(data.get("token_expire_time")).or_else(|| date(data.get("expires_at")));
    let next = date(data.get("token_next_refresh_time"));
    let usable = (status.is_none() || status.as_deref() == Some("active"))
        && expires.is_none_or(|at| at > now);

    let primary = plan
        .filter(|plan| usable && plan.limit > 0)
        .map(|plan| {
            // The next refresh is a reset only while it is still ahead and
            // comes before the license ends; a refresh after the end never
            // happens.
            let resets_at = next
                .filter(|at| *at > now && expires.is_none_or(|end| *at < end))
                .and_then(iso);
            RateWindow {
                used_percent: (plan.used as f64 / plan.limit as f64 * 100.0).clamp(0.0, 100.0),
                raw_used_percent: None,
                resets_at,
                // Chatbox states when the plan refreshes, never how often.
                window_minutes: None,
                window_kind: None,
                used_count: Some(plan.used as f64),
                total_count: Some(plan.limit as f64),
                regeneration: None,
                breakdown: None,
            }
        });

    let mut pools = Vec::new();
    // The combined total includes the plan, so it stands in only when no plan
    // item exists; publishing both would count the plan twice.
    if plan.is_none() {
        if let Some(total) = total.filter(|c| !c.is_empty()) {
            pools.push(pool(
                "unified_token",
                "Quota",
                PoolFunding::Unknown,
                POINT_UNIT,
                total,
            ));
        }
    }
    if let Some(rewards) = rewards.filter(|c| !c.is_empty()) {
        pools.push(pool(
            "aggregated_reward",
            "Credits",
            // Invitation rewards and trials: given by Chatbox, not bought.
            PoolFunding::Granted,
            POINT_UNIT,
            rewards,
        ));
    }
    if let Some(expansion) = expansion.filter(|c| !c.is_empty()) {
        pools.push(pool(
            "expansion_pack",
            "Top-up credits",
            // Expansion packs are bought on top of the plan.
            PoolFunding::Purchased,
            POINT_UNIT,
            expansion,
        ));
    }
    if let Some(images) = images.filter(|c| !c.is_empty()) {
        pools.push(pool(
            "image",
            "Image Generation",
            // Chatbox does not say whether image generations come with the
            // plan, are given, or are bought.
            PoolFunding::Unknown,
            IMAGE_UNIT,
            images,
        ));
    }

    if primary.is_none() && pools.is_empty() {
        return Err(FetchError::NoQuotaReported(
            "chatbox: the license states no usable plan and no remaining credit".to_string(),
        ));
    }

    let account_info = AccountInfo {
        email: None,
        org_name: None,
        plan_type: name,
        subscription_renews_at: None,
        subscription_ends_at: expires.and_then(iso),
    };
    Ok(Reading {
        usage: Usage {
            primary,
            secondary: None,
            tertiary: None,
            extra_rate_windows: None,
        },
        pools,
        account_info: (!account_info.is_empty()).then_some(account_info),
    })
}

/// The error for a refused request. The body is never quoted: Chatbox's
/// refusals are not known to be free of the license or its identifiers.
fn refusal(status: u16) -> FetchError {
    // `ProviderStatus` keeps the number, which decides the class (401 and 403
    // are credential_rejected, everything else upstream_failed) and lets the
    // vault lane report a 401.
    FetchError::ProviderStatus(status, "chatbox refusal (body not quoted)".to_string())
}

pub struct ChatboxProvider {
    base: String,
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    env: EnvLookup,
    clock: Clock,
}

impl ChatboxProvider {
    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            base: API_BASE.to_string(),
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
            env: default_env(),
            clock: Arc::new(Utc::now),
        }
    }

    async fn read(&self, key: &str) -> Result<Reading, FetchError> {
        // Refused before sending, as upstream does: a key with whitespace or a
        // control character cannot be a license key, and sending it would put
        // a malformed header on the wire. The key is not quoted.
        if key_is_malformed(key) {
            return Err(FetchError::CredentialUnusable(
                "the Chatbox license key is malformed (whitespace, control characters or too long)"
                    .to_string(),
            ));
        }
        let response = JsonRequest::get(format!("{}{DETAIL_PATH}", self.base))
            // The whole header value is the key: Chatbox's client sends no
            // `Bearer` prefix.
            .header(Header::new("Authorization", key))
            .header(Header::new("Cache-Control", "no-store"))
            .send_raw(&self.http)
            .await?;
        // Upstream accepts exactly 200; any other status is a refusal.
        if response.status != 200 {
            return Err(refusal(response.status));
        }
        parse(response.body_for_parsing()?, (self.clock)())
    }

    async fn fetch_vault(&self, handle: &CredentialHandle) -> FetchAttempt {
        let Some(credential_source) = self.credential_source.as_ref() else {
            return FetchAttempt::unverified_vault_failure(
                crate::credential_source::VaultGetError::Permanent,
            );
        };
        let mut credential = match crate::credential_source::get_vault_credential(
            credential_source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        {
            Ok(credential) => credential,
            Err(error) => return FetchAttempt::unverified_vault_failure(error),
        };
        let record_version = credential.record_version;
        let key = match crate::credential_source::take_utf8_payload(&mut credential.payload) {
            Ok(key) => key.trim().to_string(),
            Err(error) => return FetchAttempt::failure(None, Some("vault".to_string()), error),
        };
        if key.is_empty() {
            return FetchAttempt::failure(
                None,
                Some("vault".to_string()),
                FetchError::CredentialUnusable(
                    "the apikey:chatbox deposit holds no license key".to_string(),
                ),
            );
        }
        let result = self.read(&key).await;
        if let Err(error) = &result {
            // The shared gate reports only a 401 on this served key: a 403 is
            // Chatbox refusing access to a license, not a dead key.
            crate::credential_source::report_vault_auth_failure(
                Some(credential_source),
                handle,
                record_version,
                error,
            );
        }
        attempt_from(result, "vault")
    }

    async fn fetch_env(&self) -> FetchAttempt {
        let Some(key) = (self.env)(KEY_ENV) else {
            return FetchAttempt::failure(
                None,
                None,
                FetchError::NoSession(format!("no {KEY_ENV} and no apikey:chatbox vault deposit")),
            );
        };
        attempt_from(self.read(&key).await, "api")
    }
}

fn attempt_from(result: Result<Reading, FetchError>, source: &str) -> FetchAttempt {
    match result {
        Ok(reading) => {
            let mut attempt = FetchAttempt::success(None, source, reading.usage);
            // Always present, even empty: Chatbox was asked, and a license with
            // only a plan states that it holds no other credit.
            attempt.pools = Some(reading.pools);
            attempt.account_info = reading.account_info;
            attempt
        }
        Err(error) => FetchAttempt::failure(None, Some(source.to_string()), error),
    }
}

#[async_trait]
impl UsageProvider for ChatboxProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_some() {
            let vault = self.handle_loader.chatbox_handles()?;
            if !vault.is_empty() {
                // Vault-only custody once a deposit exists. The key carries no
                // account identity, so an environment lane beside it would be a
                // second identity-less row that the read path collapses into one
                // with an arbitrary winner.
                return Ok(vault);
            }
        }
        Ok(vec![CredentialHandle::implicit()])
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        if handle.is_vault() {
            return self.fetch_vault(handle).await;
        }
        self.fetch_env().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vercel::testkit::{credential, MockSource};
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    // Upstream's own fixture, from `ChatboxPluginTests.swift`, and its clock:
    // 2026-10-20 00:00 UTC.
    const FIXTURE: &str = r#"{"data":{"key":"private-response-key","name":"Chatbox AI Pro","plan":"pro","status":"active",
    "unified_token_usage":300,"unified_token_limit":1500,
    "unified_token_usage_details":[{"type":"plan","token_usage":200,"token_limit":1000},
    {"type":"invitation_reward","token_usage":100,"token_limit":500}],
    "aggregated_reward_details":{"token_usage":100,"token_limit":500},
    "expansion_pack_usage":50,"expansion_pack_limit":400,"image_used_count":3,"image_total_quota":20,
    "token_refreshed_time":"2026-10-01T00:00:00Z","token_next_refresh_time":"2026-11-01T00:00:00Z",
    "token_expire_time":"2027-01-01T00:00:00Z"}}"#;
    const NOW_EPOCH: i64 = 1_792_454_400;
    const KEY: &str = "fixture-key";

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(NOW_EPOCH, 0).unwrap()
    }

    /// The fixture's `data` object, for the cases upstream builds by editing it.
    fn data() -> Map<String, Value> {
        let root: Value = serde_json::from_str(FIXTURE).unwrap();
        root["data"].as_object().unwrap().clone()
    }

    fn body(data: Map<String, Value>) -> String {
        serde_json::json!({ "data": data }).to_string()
    }

    fn read(body: &str) -> Result<Reading, FetchError> {
        parse(body.as_bytes(), now())
    }

    fn find<'a>(pools: &'a [Pool], id: &str) -> Option<&'a Pool> {
        pools.iter().find(|pool| pool.id == id)
    }

    /// `(remaining, total)` of a pool, in its own unit.
    fn left(pool: &Pool) -> (i64, i64) {
        (
            pool.remaining.as_ref().unwrap().minor,
            pool.total.as_ref().unwrap().minor,
        )
    }

    fn provider(
        source: Option<Arc<dyn CredentialSource>>,
        loader: Arc<VaultHandleLoader>,
        base: &str,
        env: &[(&str, &str)],
    ) -> ChatboxProvider {
        let mut provider = ChatboxProvider::new_with_handle_loader(source, loader);
        provider.base = base.to_string();
        provider.clock = Arc::new(now);
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

    fn vault_handle() -> CredentialHandle {
        CredentialHandle::scoped("apikey:chatbox", "apikey")
    }

    type Requests = Arc<Mutex<Vec<String>>>;

    /// A loopback Chatbox answering every request with `status` and `body`,
    /// recording each complete request as received.
    async fn chatbox(status: u16, body: &str) -> (String, Requests) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let requests: Requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let body = body.as_bytes().to_vec();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request = crate::loopback::read_request(&mut stream).await;
                log.lock().unwrap().push(request);
                let head = format!(
                    "HTTP/1.1 {status} Status\r\ncontent-type: application/json\r\n\
                     retry-after: 30\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });
        (format!("http://{address}"), requests)
    }

    /// The value of one request header, matched case-insensitively.
    fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    async fn fetch_env(status: u16, body: &str) -> (FetchAttempt, Requests) {
        let (base, requests) = chatbox(status, body).await;
        let provider = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, KEY)],
        );
        (
            provider.fetch_handle(&CredentialHandle::implicit()).await,
            requests,
        )
    }

    #[tokio::test]
    async fn the_fixture_publishes_the_plan_window_and_each_credit_pool_apart() {
        let (attempt, requests) = fetch_env(200, FIXTURE).await;
        assert_eq!(attempt.source.as_deref(), Some("api"));
        let usage = attempt.usage.as_ref().expect("the fixture reads");

        let window = usage.primary.as_ref().expect("an active plan is a window");
        assert_eq!(window.used_percent, 20.0);
        assert_eq!(window.resets_at.as_deref(), Some("2026-11-01T00:00:00Z"));
        // Chatbox states when the plan refreshes, never a cadence.
        assert_eq!(window.window_minutes, None);
        assert_eq!(window.window_kind, None);
        // Top-up credits are a pool here, not upstream's secondary meter.
        assert!(usage.secondary.is_none() && usage.tertiary.is_none());
        assert!(usage.extra_rate_windows.is_none());

        let pools = attempt.pools.as_ref().unwrap();
        let ids: Vec<&str> = pools.iter().map(|pool| pool.id.as_str()).collect();
        assert_eq!(ids, vec!["aggregated_reward", "expansion_pack", "image"]);
        let rewards = find(pools, "aggregated_reward").unwrap();
        assert_eq!(left(rewards), (400, 500));
        assert_eq!(rewards.funding, PoolFunding::Granted);
        assert_eq!(rewards.remaining.as_ref().unwrap().unit, POINT_UNIT);
        let top_up = find(pools, "expansion_pack").unwrap();
        assert_eq!(left(top_up), (350, 400));
        assert_eq!(top_up.funding, PoolFunding::Purchased);
        let images = find(pools, "image").unwrap();
        assert_eq!(left(images), (17, 20));
        assert_eq!(images.funding, PoolFunding::Unknown);
        assert_eq!(images.total.as_ref().unwrap().unit, IMAGE_UNIT);
        for pool in pools {
            assert_eq!(pool.basis, PoolBasis::Derived, "{}", pool.id);
            assert_eq!(pool.remaining.as_ref().unwrap().exponent, 0, "{}", pool.id);
            assert_eq!(pool.spendable, None, "{}", pool.id);
        }

        // The license's end is account information, never a reset.
        let info = attempt.account_info.as_ref().unwrap();
        assert_eq!(info.plan_type.as_deref(), Some("Chatbox AI Pro"));
        assert_eq!(
            info.subscription_ends_at.as_deref(),
            Some("2027-01-01T00:00:00Z")
        );
        assert_eq!(info.subscription_renews_at, None);
        assert_eq!(info.email, None);
        // The license's own key field is never published.
        assert!(!format!("{attempt:?}").contains("private-response-key"));

        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(
            request.starts_with(&format!("GET {DETAIL_PATH} ")),
            "{request}"
        );
        // The bare key, with no `Bearer` prefix, as Chatbox's client sends it.
        assert_eq!(header(request, "authorization"), Some(KEY));
        assert_eq!(header(request, "cache-control"), Some("no-store"));
    }

    /// The plan reset belongs to the plan alone. Every pool is published
    /// without one, including when the plan's reset is set.
    #[test]
    fn no_pool_ever_carries_the_plan_reset() {
        let mut free = data();
        free.insert("plan".into(), "free".into());
        free.remove("token_expire_time");
        free.insert("token_next_refresh_time".into(), "2026-10-21T16:00:00Z".into());
        let mut legacy = data();
        legacy.remove("unified_token_usage_details");
        for body in [FIXTURE.to_string(), body(free), body(legacy)] {
            let reading = read(&body).unwrap();
            assert!(!reading.pools.is_empty(), "{body}");
            for pool in &reading.pools {
                assert_eq!(pool.resets_at, None, "{} must not reset", pool.id);
                assert_eq!(pool.expires_at, None, "{}", pool.id);
            }
        }
    }

    /// The combined total already includes the plan, so the window is the plan
    /// item's own figures and the combined total is not published beside it.
    #[test]
    fn the_plan_and_the_combined_total_are_never_added_together() {
        let reading = read(FIXTURE).unwrap();
        let window = reading.usage.primary.unwrap();
        assert_eq!(
            (window.used_count, window.total_count),
            (Some(200.0), Some(1000.0)),
            "the window is the plan item alone"
        );
        assert!(find(&reading.pools, "unified_token").is_none());
    }

    #[test]
    fn a_free_plan_reset_is_the_windows_alone() {
        let mut data = data();
        data.insert("plan".into(), "free".into());
        data.remove("token_expire_time");
        data.insert("token_next_refresh_time".into(), "2026-10-21T16:00:00Z".into());
        let reading = read(&body(data)).unwrap();
        let window = reading.usage.primary.unwrap();
        assert_eq!(window.resets_at.as_deref(), Some("2026-10-21T16:00:00Z"));
        assert_eq!(window.window_minutes, None);
        assert!(reading.usage.tertiary.is_none());
        assert!(reading.usage.extra_rate_windows.is_none());
        assert!(reading.account_info.unwrap().subscription_ends_at.is_none());
    }

    /// A trial is not a recurring plan, and an older answer with only the
    /// combined total publishes it as a pool, never as a window.
    #[test]
    fn trials_and_the_legacy_total_never_invent_a_recurring_plan() {
        let mut trial = data();
        trial.insert(
            "unified_token_usage_details".into(),
            serde_json::json!([
                {"type": "plan", "token_usage": 0, "token_limit": 0},
                {"type": "trial", "token_usage": 100, "token_limit": 500},
            ]),
        );
        let reading = read(&body(trial.clone())).unwrap();
        assert!(reading.usage.primary.is_none());
        // A plan item exists (empty), so the combined total stays unpublished.
        assert!(find(&reading.pools, "unified_token").is_none());

        trial.remove("unified_token_usage_details");
        let legacy = read(&body(trial)).unwrap();
        assert!(legacy.usage.primary.is_none());
        let quota = &legacy.pools[0];
        assert_eq!(quota.id, "unified_token");
        assert_eq!(left(quota), (1200, 1500));
        assert_eq!(quota.funding, PoolFunding::Unknown);
    }

    /// A plan that is not active publishes no window and no reset, while the
    /// separate credit pools stay.
    #[test]
    fn expired_disabled_and_unknown_plans_keep_their_pools_without_a_window() {
        for status in ["expired", "disabled", "refunded", "unknown"] {
            let mut data = data();
            data.insert("status".into(), status.into());
            let reading = read(&body(data)).unwrap();
            assert!(reading.usage.primary.is_none(), "{status}");
            assert_eq!(
                left(find(&reading.pools, "expansion_pack").unwrap()),
                (350, 400),
                "{status}"
            );
        }
        // A refresh time that is unreadable, before 1970, already past, or not
        // before the license ends is no reset.
        for next in [
            "0001-01-01T00:00:00Z",
            "invalid",
            "2026-09-01T00:00:00Z",
            "2027-01-01T00:00:00Z",
        ] {
            let mut data = data();
            data.insert("token_next_refresh_time".into(), next.into());
            let window = read(&body(data)).unwrap().usage.primary.unwrap();
            assert_eq!(window.resets_at, None, "{next}");
            assert_eq!(window.used_percent, 20.0, "{next}");
        }
    }

    #[test]
    fn an_expired_plan_date_publishes_no_window_and_keeps_its_end() {
        let mut data = data();
        data.insert("token_expire_time".into(), "2026-10-19T00:00:00Z".into());
        let reading = read(&body(data)).unwrap();
        assert!(reading.usage.primary.is_none());
        assert!(find(&reading.pools, "expansion_pack").is_some());
        assert_eq!(
            reading.account_info.unwrap().subscription_ends_at.as_deref(),
            Some("2026-10-19T00:00:00Z")
        );
    }

    /// An expired plan with no other credit has nothing to report: the key
    /// works, so that is no quota, not a rejected credential.
    #[tokio::test]
    async fn an_expired_plan_with_nothing_else_is_no_quota_reported() {
        let (attempt, _) = fetch_env(
            200,
            r#"{"data":{"status":"expired","unified_token_usage_details":
            [{"type":"plan","token_usage":200,"token_limit":1000}]}}"#,
        )
        .await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "no_quota_reported", "{error:?}");
    }

    #[tokio::test]
    async fn zero_allowances_are_no_quota_reported_not_an_unlimited_window() {
        let (attempt, _) = fetch_env(
            200,
            r#"{"data":{"unified_token_usage":0,"unified_token_limit":0}}"#,
        )
        .await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "no_quota_reported", "{error:?}");
    }

    #[test]
    fn an_exhausted_plan_reads_full_and_absent_pools_stay_absent() {
        let reading = read(
            r#"{"data":{"name":"Fixture plan","status":"active","unified_token_usage_details":
            [{"type":"plan","token_usage":1200,"token_limit":1000}]}}"#,
        )
        .unwrap();
        assert_eq!(reading.usage.primary.unwrap().used_percent, 100.0);
        assert!(reading.pools.is_empty(), "{:?}", reading.pools);
    }

    #[test]
    fn empty_pools_are_hidden_and_exhausted_ones_stay() {
        let mut data = data();
        data.insert("expansion_pack_usage".into(), 0.into());
        data.insert("expansion_pack_limit".into(), 0.into());
        data.insert("image_used_count".into(), 20.into());
        let reading = read(&body(data)).unwrap();
        assert!(find(&reading.pools, "expansion_pack").is_none());
        assert_eq!(left(find(&reading.pools, "image").unwrap()), (0, 20));
    }

    /// Without an aggregate, rewards are the sum of the unexpired non-plan
    /// detail items.
    #[test]
    fn the_reward_fallback_skips_expired_grants() {
        let mut data = data();
        data.remove("aggregated_reward_details");
        data.insert(
            "unified_token_usage_details".into(),
            serde_json::json!([
                {"type": "plan", "token_usage": 200, "token_limit": 1000},
                {"type": "invitation_reward", "token_usage": 100, "token_limit": 500},
                {"type": "trial", "token_usage": 25, "token_limit": 200,
                 "expires_at": "2026-11-01T00:00:00Z"},
                {"type": "invitation_reward", "token_usage": 0, "token_limit": 900,
                 "expires_at": "2026-10-19T00:00:00Z"},
            ]),
        );
        let reading = read(&body(data)).unwrap();
        assert_eq!(
            left(find(&reading.pools, "aggregated_reward").unwrap()),
            (575, 700)
        );
        assert_eq!(reading.usage.primary.unwrap().used_percent, 20.0);
        assert!(reading.usage.extra_rate_windows.is_none());
    }

    #[test]
    fn an_absent_status_is_a_usable_plan() {
        let mut data = data();
        data.remove("status");
        let window = read(&body(data)).unwrap().usage.primary.unwrap();
        assert_eq!(window.used_percent, 20.0);
        assert_eq!(window.resets_at.as_deref(), Some("2026-11-01T00:00:00Z"));
    }

    /// Every body upstream refuses is decode_failed through the provider, and
    /// none is echoed into the error.
    #[tokio::test]
    async fn malformed_answers_are_decode_failed_without_echoing_them() {
        for body in [
            "private-response-key",
            "null",
            "[]",
            "{}",
            r#"{"data":{}}"#,
            r#"{"data":{"unified_token_usage":-1,"unified_token_limit":100}}"#,
            r#"{"data":{"unified_token_usage":true,"unified_token_limit":100}}"#,
            r#"{"data":{"unified_token_usage":9007199254740992,"unified_token_limit":100}}"#,
            r#"{"data":{"unified_token_usage_details":{}}}"#,
            r#"{"data":null,"error":{"message":"private-response-key"}}"#,
            r#"{"data":{"unified_token_usage":0,"unified_token_limit":100},"error":{"code":"fixture-error"}}"#,
            r#"{"data":{"unified_token_usage_details":[
            {"type":"plan","token_usage":0,"token_limit":1},
            {"type":"plan","token_usage":0,"token_limit":1}]}}"#,
            r#"{"data":{"unified_token_usage_details":[
            {"type":"reward","token_usage":0,"token_limit":9007199254740991},
            {"type":"trial","token_usage":0,"token_limit":1}]}}"#,
        ] {
            let (attempt, _) = fetch_env(200, body).await;
            match attempt.usage {
                Err(error @ FetchError::Decode(_)) => {
                    assert_eq!(error.error_class(), "decode_failed");
                    let shown = error.to_string();
                    assert!(!shown.contains("private-response"), "{shown}");
                    assert!(!shown.contains("fixture-error"), "{shown}");
                }
                other => panic!("{body} must be decode_failed, got {other:?}"),
            }
        }
    }

    /// Each refusal lands in its class, only a 401 on a served vault key is
    /// reported, and no refusal body or key is quoted.
    #[tokio::test]
    async fn each_status_maps_to_its_class_and_only_a_vault_401_is_reported() {
        for (status, class, reported) in [
            (401, "credential_rejected", true),
            (403, "credential_rejected", false),
            (409, "upstream_failed", false),
            (429, "upstream_failed", false),
            (500, "upstream_failed", false),
            (502, "upstream_failed", false),
            (503, "upstream_failed", false),
        ] {
            let (base, requests) = chatbox(status, "private-response-key").await;
            let (source, reports) =
                MockSource::serving(&[("apikey:chatbox", credential(KEY, 11))]);
            let provider = provider(
                Some(source),
                Arc::new(VaultHandleLoader::default()),
                &base,
                &[],
            );
            let attempt = provider.fetch_handle(&vault_handle()).await;
            assert_eq!(attempt.source.as_deref(), Some("vault"));
            let error = attempt.usage.unwrap_err();
            assert_eq!(error.error_class(), class, "HTTP {status}: {error:?}");
            // 429 and 5xx keep the last reading served while they last.
            if status == 429 || status >= 500 {
                assert_eq!(
                    crate::refresh::classify(&error),
                    crate::refresh::FetchClass::Transient,
                    "HTTP {status}"
                );
            } else if status == 401 || status == 403 {
                assert_ne!(
                    crate::refresh::classify(&error),
                    crate::refresh::FetchClass::Transient,
                    "a refused key must not be served stale as an outage: HTTP {status}"
                );
            }
            let shown = error.to_string();
            assert!(!shown.contains("private-response"), "{shown}");
            assert!(!shown.contains(KEY), "{shown}");
            // One request: a conflict is not retried.
            assert_eq!(requests.lock().unwrap().len(), 1, "HTTP {status}");
            for _ in 0..20 {
                if !reports.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let expected = if reported {
                vec![("apikey:chatbox".to_string(), 401, 11)]
            } else {
                Vec::new()
            };
            assert_eq!(*reports.lock().unwrap(), expected, "HTTP {status}");
        }
    }

    /// A 401 on the environment key is rejected, and nothing is reported: the
    /// vault did not serve that key.
    #[tokio::test]
    async fn an_environment_401_is_rejected_and_never_reported_to_the_vault() {
        let (base, _) = chatbox(401, "").await;
        let (source, reports) = MockSource::serving(&[("apikey:chatbox", credential(KEY, 5))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, "env-license-key")],
        );
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        let error = attempt.usage.unwrap_err();
        assert_eq!(error.error_class(), "credential_rejected", "{error:?}");
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(reports.lock().unwrap().is_empty());
    }

    /// A key upstream would refuse is never sent, and is not quoted.
    #[tokio::test]
    async fn malformed_keys_are_unusable_and_send_nothing() {
        let long = "x".repeat(4097);
        for key in [
            "fixture key",
            "fixture\r\nheader",
            "fixture\u{0}suffix",
            "fixture\u{85}suffix",
            "fixture\u{feff}suffix",
            long.as_str(),
        ] {
            let (base, requests) = chatbox(200, FIXTURE).await;
            let (source, _) = MockSource::serving(&[("apikey:chatbox", credential(key, 1))]);
            let provider = provider(
                Some(source),
                Arc::new(VaultHandleLoader::default()),
                &base,
                &[],
            );
            let error = provider
                .fetch_handle(&vault_handle())
                .await
                .usage
                .unwrap_err();
            assert_eq!(error.error_class(), "credential_unusable", "{key:?}");
            assert!(!error.to_string().contains("fixture"), "{error}");
            assert!(requests.lock().unwrap().is_empty(), "{key:?} was sent");
        }
        // The longest key upstream accepts is sent.
        let (base, requests) = chatbox(200, FIXTURE).await;
        let longest = "x".repeat(4096);
        let provider = provider(
            None,
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, longest.as_str())],
        );
        let attempt = provider.fetch_handle(&CredentialHandle::implicit()).await;
        assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_vault_deposit_replaces_the_environment_lane() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[("apikey:chatbox", "apikey")]);
        let (source, _) = MockSource::serving(&[]);
        let with_vault = provider(
            Some(source),
            Arc::clone(&loader),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-license-key")],
        );
        assert_eq!(
            with_vault.handles().unwrap(),
            vec![vault_handle()],
            "a chatbox deposit must replace the environment lane"
        );

        // The control: the same environment with no deposit enumerates the
        // environment lane, so the line above is the vault replacing it.
        let (source, _) = MockSource::serving(&[]);
        let without_vault = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            "http://127.0.0.1:9",
            &[(KEY_ENV, "env-license-key")],
        );
        assert_eq!(
            without_vault.handles().unwrap(),
            vec![CredentialHandle::implicit()]
        );
    }

    /// The vault lane sends the deposited key, and the environment key never
    /// leaves the host on it.
    #[tokio::test]
    async fn the_vault_lane_sends_the_vault_key_and_not_the_environment_key() {
        let (base, requests) = chatbox(200, FIXTURE).await;
        let (source, reports) =
            MockSource::serving(&[("apikey:chatbox", credential(" vault-license-key ", 3))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[(KEY_ENV, "env-license-key")],
        );
        let attempt = provider.fetch_handle(&vault_handle()).await;
        assert!(attempt.usage.is_ok(), "{:?}", attempt.usage);
        assert_eq!(attempt.source.as_deref(), Some("vault"));
        let requests = requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            header(&requests[0], "authorization"),
            Some("vault-license-key")
        );
        assert!(!requests[0].contains("env-license-key"));
        assert!(reports.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_key_anywhere_is_credential_absent_and_sends_nothing() {
        let (base, requests) = chatbox(200, FIXTURE).await;
        let provider = provider(None, Arc::new(VaultHandleLoader::default()), &base, &[]);
        let error = provider
            .fetch_handle(&CredentialHandle::implicit())
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "credential_absent", "{error:?}");
        assert!(requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_vault_deposit_is_unusable_and_sends_nothing() {
        let (base, requests) = chatbox(200, FIXTURE).await;
        let (source, _) = MockSource::serving(&[("apikey:chatbox", credential("  ", 1))]);
        let provider = provider(
            Some(source),
            Arc::new(VaultHandleLoader::default()),
            &base,
            &[],
        );
        let error = provider
            .fetch_handle(&vault_handle())
            .await
            .usage
            .unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable", "{error:?}");
        assert!(requests.lock().unwrap().is_empty());
    }
}
