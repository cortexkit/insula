//! Quota notices carry identities and digests, not bodies. The bounded local
//! queue and body cache let serving and credential refresh outlive a bus outage.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use cortexkit_bus_naming::AccountNames;
use cortexkit_bus_nats::{ConnectConfig, NatsConnection};
use cortexkit_bus_trait::{ContentDigest, Headers, Stream};
use quota_core::model::ProviderUsage;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::vault_client::BusRoute;

pub const EVENT_NAME: &str = "quota_sampled";
pub const EVENTS_GET: &str = "events_get";
const QUEUE_CAP: usize = 256;
const BODY_CAP: usize = 1024;
const PER_ACCOUNT_CAP: usize = 16;
type Key = (String, Option<String>);

#[derive(Clone)]
struct Sample {
    key: Key,
    body: Value,
    id: String,
    digest: ContentDigest,
    headers: Headers,
    published_at: i64,
}

#[derive(Default)]
struct Pending {
    queue: VecDeque<Sample>,
    current: BTreeMap<Key, Value>,
    seen: HashMap<Key, Value>,
    seq: HashMap<Key, u64>,
    bodies: VecDeque<Sample>,
}

/// Process-lifetime metrics and queue; no network operation holds its mutex.
pub struct QuotaSamples {
    pub watches: crate::usage_watch::UsageWatches,
    epoch: String,
    capacity: usize,
    pending: Mutex<Pending>,
    wake: Notify,
    state: Mutex<(&'static str, &'static str)>,
    published: AtomicU64,
    dropped: AtomicU64,
}

impl Default for QuotaSamples {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl std::fmt::Debug for QuotaSamples {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaSamples")
            .field("metrics", &self.metrics())
            .finish_non_exhaustive()
    }
}

impl QuotaSamples {
    pub fn new(epoch: String) -> Self {
        Self::with_capacity(epoch, QUEUE_CAP)
    }

    fn with_capacity(epoch: String, capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            watches: crate::usage_watch::UsageWatches::new(epoch.clone()),
            epoch,
            capacity,
            pending: Mutex::new(Pending::default()),
            wake: Notify::new(),
            state: Mutex::new(("disabled", "not_started")),
            published: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    /// Called once after a refresher turn, with the same filtered entries that
    /// usage.get serves. Only local enqueue work is allowed at this boundary.
    pub async fn observe(&self, entries: Vec<ProviderUsage>) {
        self.offer(entries);
    }

    fn offer(&self, entries: Vec<ProviderUsage>) {
        let current = entries
            .into_iter()
            .filter_map(|entry| {
                let mut value = serde_json::to_value(entry).ok()?;
                value.as_object_mut()?.remove("accountInfo");
                let key = (
                    value["provider"].as_str()?.to_owned(),
                    value["account"].as_str().map(str::to_owned),
                );
                Some((key, value))
            })
            .collect::<BTreeMap<_, _>>();
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let absent = pending
            .queue
            .iter()
            .filter(|s| !current.contains_key(&s.key))
            .count();
        self.dropped.fetch_add(absent as u64, Ordering::Relaxed);
        pending.queue.retain(|s| current.contains_key(&s.key));
        pending.current = current;
        let changed = pending
            .current
            .iter()
            .filter(|(key, entry)| pending.seen.get(*key) != Some(*entry))
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect::<Vec<_>>();
        for (key, entry) in changed {
            self.watches.changed(&key.0, key.1.as_deref());
            pending.seen.insert(key.clone(), entry.clone());
            self.enqueue(&mut pending, key, entry);
        }
        drop(pending);
        self.wake.notify_one();
    }

    fn enqueue(&self, pending: &mut Pending, key: Key, mut body: Value) {
        let seq = pending.seq.entry(key.clone()).or_default();
        *seq = seq.checked_add(1).expect("quota sample sequence exhausted");
        body["epoch"] = json!(self.epoch);
        body["seq"] = json!(*seq);
        let mut identity = BTreeMap::from([
            ("provider", json!(key.0)),
            ("epoch", json!(self.epoch)),
            ("seq", json!(*seq)),
        ]);
        if let Some(account) = &key.1 {
            identity.insert("account", json!(account));
        }
        // These keys are ASCII, values are strings or exact unsigned integers.
        // The sorted map is their JCS spelling, without float rounding
        // that would collapse adjacent process sequences above 2^53.
        let id = format!(
            "{}",
            ContentDigest::of_bytes(&serde_json::to_vec(&identity).unwrap())
        );
        let bytes = serde_json::to_vec(&body).expect("serialized usage entry");
        let digest = ContentDigest::of_bytes(&bytes);
        let mut headers = Headers::from([
            ("kind".into(), EVENT_NAME.into()),
            ("provider".into(), key.0.clone()),
            ("epoch".into(), self.epoch.clone()),
            ("seq".into(), seq.to_string()),
        ]);
        if let Some(account) = &key.1 {
            headers.insert("account".into(), account.clone());
        }
        let sample = Sample {
            key,
            body,
            id,
            digest,
            headers,
            published_at: now_ms(),
        };
        if pending.queue.len() == self.capacity {
            pending.queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        pending.queue.push_back(sample);
    }

    fn replay_current(&self) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        // New credentials mean a new session. Keep queued first readings on the
        // first connection; on reconnection the current snapshot replaces history.
        let current = pending.current.clone();
        self.dropped
            .fetch_add(pending.queue.len() as u64, Ordering::Relaxed);
        pending.queue.clear();
        for (key, entry) in current {
            self.enqueue(&mut pending, key, entry);
        }
    }

    fn pop(&self) -> Option<Sample> {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .queue
            .pop_front()
    }

    fn retain_body(&self, sample: &Sample) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.bodies.push_back(sample.clone());
        let mut count = 0;
        pending.bodies = std::mem::take(&mut pending.bodies)
            .into_iter()
            .rev()
            .filter(|s| {
                if s.key != sample.key {
                    return true;
                }
                count += 1;
                count <= PER_ACCOUNT_CAP
            })
            .collect::<VecDeque<_>>()
            .into_iter()
            .rev()
            .collect();
        while pending.bodies.len() > BODY_CAP {
            pending.bodies.pop_front();
        }
    }

    /// Cache-only body retrieval. A missed/evicted key requires a usage.get
    /// snapshot, not an attempt to reconstruct history from the latest reading.
    pub fn events_get(&self, key: &str, name: &str) -> Value {
        if key.len() != 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return json!({"error":{"code":"invalid_request","message":"event_key must be 64 lowercase hex characters"}});
        }
        if name != EVENT_NAME {
            return json!({"error":{"code":"event_name_mismatch","message":"expected quota_sampled"}});
        }
        let pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        match pending.bodies.iter().find(|s| s.id == key) {
            Some(s) => json!({"result": {"body": s.body, "digest": s.digest.to_string(),
                "event_key": s.id, "event_name": EVENT_NAME, "published_at": s.published_at}}),
            None => {
                json!({"error":{"code":"event_not_found","message":"event expired or unknown; read usage.get"}})
            }
        }
    }

    fn set_state(&self, state: &'static str, reason: &'static str) {
        let mut previous = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if *previous != (state, reason) {
            eprintln!("{} bus: {state} ({reason})", quota_core::LOG_TAG);
            *previous = (state, reason);
        }
    }

    pub fn metrics(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut metrics = json!({"busPublishState": {"state": state.0, "reason": state.1},
            "quotaSamplesPublished": self.published.load(Ordering::Relaxed),
            "quotaSamplesDropped": self.dropped.load(Ordering::Relaxed)});
        metrics
            .as_object_mut()
            .unwrap()
            .extend(self.watches.metrics().as_object().unwrap().clone());
        metrics
    }

    pub fn spawn(
        self: &Arc<Self>,
        path: PathBuf,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let samples = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = samples.run(path) => {},
            }
            samples.set_state("disabled", "shutdown");
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// Only these structural fields cross the credential boundary; credential values
// are never Debug-formatted or copied into health/error messages.
#[derive(Clone, Deserialize)]
struct Credential {
    jwt: String,
    exp: i64,
    acct: String,
    inbox_prefix: String,
    server_url: String,
    credential_public: String,
    spawn_generation: u64,
    credential_epoch: u64,
}

impl Credential {
    fn validate(&self) -> Result<(), &'static str> {
        AccountNames::derive(&self.acct).map_err(|_| "bus_invalid_account")?;
        let config = ConnectConfig::new(&self.credential_public).map_err(|_| "bus_invalid_key")?;
        if config.inbox_prefix() != self.inbox_prefix {
            return Err("bus_invalid_inbox");
        }
        let address = self
            .server_url
            .parse::<async_nats::ServerAddr>()
            .map_err(|_| "bus_invalid_url")?;
        if address.scheme() != "nats"
            || address.username().is_some()
            || address.password().is_some()
            || !matches!(address.host(), "127.0.0.1" | "localhost" | "::1")
        {
            return Err("bus_nonlocal_url");
        }
        if self.exp <= now_ms() / 1000 {
            return Err("bus_expired_credential");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Renewal {
    jwt: String,
    exp: i64,
    credential_public: String,
    spawn_generation: u64,
    credential_epoch: u64,
}

struct Session {
    credential: Credential,
    connection: NatsConnection,
    names: AccountNames,
    route: Arc<BusRoute>,
    transport: Option<SessionRuntime>,
}

// async-nats has graceful drain but no abortive close. A session-owned runtime
// lets shutdown and renewal stop reconnect tasks and buffered commands instead
// of leaving them alive with an expired credential snapshot.
struct SessionRuntime(Option<tokio::runtime::Runtime>);

impl Drop for SessionRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.transport.take());
    }
}

impl Session {
    async fn open(route: Arc<BusRoute>) -> Result<Self, &'static str> {
        let credential = serde_json::from_value(route.call("ckbus.credential", json!({})).await?)
            .map_err(|_| "bus_invalid_credential")?;
        Self::connect(route, credential).await
    }

    async fn connect(route: Arc<BusRoute>, credential: Credential) -> Result<Self, &'static str> {
        credential.validate()?;
        let signer = Arc::clone(&route);
        let key = credential.credential_public.clone();
        let jwt = credential.jwt.clone();
        let options =
            async_nats::ConnectOptions::with_auth_callback(move |nonce| {
                let signer = Arc::clone(&signer);
                let key = key.clone();
                let jwt = jwt.clone();
                async move {
                    let signed = signer.call("ckbus.nonce_sign", json!({
                    "credential_public": key,
                    "nonce_b64": base64::engine::general_purpose::STANDARD.encode(nonce),
                })).await.map_err(async_nats::AuthError::new)?;
                    let signature = signed["signature_b64"]
                        .as_str()
                        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                        .filter(|bytes| bytes.len() == 64)
                        .ok_or_else(|| async_nats::AuthError::new("bus_invalid_signature"))?;
                    let mut auth = async_nats::Auth::new();
                    auth.jwt = Some(jwt);
                    auth.signature = Some(signature);
                    Ok(auth)
                }
            })
            .max_reconnects(1)
            .reconnect_delay_callback({
                let first = std::sync::atomic::AtomicBool::new(true);
                move |_| {
                    if first.swap(false, Ordering::Relaxed) {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(365 * 24 * 3600)
                    }
                }
            })
            .ignore_discovered_servers()
            .connection_timeout(Duration::from_secs(2));
        let transport = SessionRuntime(Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .map_err(|_| "bus_runtime_failed")?,
        ));
        let config =
            ConnectConfig::new(&credential.credential_public).map_err(|_| "bus_invalid_key")?;
        let url = credential.server_url.clone();
        let connection = transport
            .0
            .as_ref()
            .unwrap()
            .spawn(async move { NatsConnection::connect(url, options, config).await })
            .await
            .map_err(|_| "bus_connect_join_failed")?
            .map_err(|_| "bus_connect_failed")?;
        let names = AccountNames::derive(&credential.acct).map_err(|_| "bus_invalid_account")?;
        Ok(Self {
            credential,
            connection,
            names,
            route,
            transport: Some(transport),
        })
    }

    async fn renew(&mut self) -> Result<(), &'static str> {
        let response = self
            .route
            .call(
                "ckbus.credential_renew",
                json!({"credential_public": self.credential.credential_public}),
            )
            .await;
        let mut credential = self.credential.clone();
        match response {
            Ok(value) => {
                let renewal: Renewal =
                    serde_json::from_value(value).map_err(|_| "bus_invalid_renewal")?;
                if renewal.credential_public != credential.credential_public
                    || renewal.spawn_generation != credential.spawn_generation
                    || renewal.credential_epoch != credential.credential_epoch
                    || renewal.exp <= credential.exp
                {
                    return Err("bus_renewal_mismatch");
                }
                credential.jwt = renewal.jwt;
                credential.exp = renewal.exp;
            }
            Err(
                "ckbus_credential_superseded"
                | "ckbus_credential_revoked"
                | "ckbus_generation_not_live",
            ) => {
                credential =
                    serde_json::from_value(self.route.call("ckbus.credential", json!({})).await?)
                        .map_err(|_| "bus_invalid_credential")?;
            }
            Err(error) => return Err(error),
        }
        // The nonce callback captures a JWT snapshot. A renewal therefore needs
        // a new transport, not just a later expiry on the old session metadata.
        drop(self.transport.take());
        *self = Self::connect(Arc::clone(&self.route), credential).await?;
        Ok(())
    }
}

impl QuotaSamples {
    /// Publish at most one queued notice through an already authenticated bus.
    /// Provisioning and credential lifecycle belong to the caller; the production
    /// drain and private-bus integration tests share this exact publishing path.
    pub async fn publish_pending(
        &self,
        connection: &NatsConnection,
        names: &AccountNames,
    ) -> Result<bool, &'static str> {
        let Some(sample) = self.pop() else {
            return Ok(false);
        };
        if let Err(reason) = self.publish(connection, names, sample).await {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(reason);
        }
        Ok(true)
    }

    async fn publish(
        &self,
        connection: &NatsConnection,
        names: &AccountNames,
        sample: Sample,
    ) -> Result<(), &'static str> {
        let subject = names
            .event_subject("insula", EVENT_NAME, 1)
            .map_err(|_| "bus_invalid_subject")?;
        // Install the immutable body before publishing so a fast consumer can
        // fetch it as soon as JetStream delivers the notice, even before our ack.
        self.retain_body(&sample);
        let receipt = tokio::time::timeout(
            Duration::from_secs(5),
            connection.stream(&names.streams().event).publish(
                &subject,
                &sample.id,
                sample.digest,
                sample.headers,
            ),
        )
        .await
        .map_err(|_| "bus_publish_timeout")?
        .map_err(|_| "bus_publish_failed")?;
        // A duplicate ack still confirms this immutable key is stored. Sequence
        // allocation never reuses a key, so it can only be an idempotent retry.
        let _duplicate = receipt.duplicate();
        self.published.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn run(&self, path: PathBuf) {
        self.set_state("connecting", "startup");
        // The shared transport keeps reader tasks for its process-lifetime
        // connection. Reuse one bus route rather than orphaning one per retry.
        let route = Arc::new(BusRoute::new(path));
        let mut failures = 0u32;
        let mut connected_before = false;
        loop {
            let result = async {
                let mut session = Session::open(Arc::clone(&route)).await?;
                if connected_before {
                    self.replay_current();
                }
                connected_before = true;
                self.set_state("publishing", "connected");
                loop {
                    if session.credential.exp.saturating_sub(now_ms() / 1000) < 300 {
                        session.renew().await?;
                        self.replay_current();
                    }
                    if session.connection.client().connection_state()
                        != async_nats::connection::State::Connected
                    {
                        return Err("bus_disconnected");
                    }
                    let wake = self.wake.notified();
                    if self
                        .publish_pending(&session.connection, &session.names)
                        .await?
                    {
                        failures = 0;
                    } else {
                        tokio::select! {
                            _ = wake => {},
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                        }
                    }
                }
            }
            .await;
            let Err(reason): Result<(), &'static str> = result else {
                unreachable!()
            };
            self.set_state("unavailable", reason);
            failures = failures.saturating_add(1);
            let delay = (1u64 << failures.min(9)).min(300);
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(account: Option<&str>, at: &str) -> ProviderUsage {
        let mut value = json!({"provider":"deepseek", "source":"api", "fetchedAt":at,
            "usage":{"primary":{"usedPercent":25.0}},
            "accountInfo":{"email":"private@example.com","orgName":"private org"}});
        if let Some(account) = account {
            value["account"] = json!(account);
        }
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn account_metadata_never_enters_a_sample() {
        let samples = QuotaSamples::new("epoch".into());
        samples
            .observe(vec![entry(Some("a"), "2026-01-01T00:00:00Z")])
            .await;
        let s = samples.pop().unwrap();
        assert!(
            s.body.get("accountInfo").is_none(),
            "accountInfo must not leave usage.get"
        );
        let encoded = serde_json::to_string(&s.body).unwrap();
        assert!(!encoded.contains("private@example.com"));
        assert!(!s.headers.values().any(|v| v.contains("private")));
    }

    #[tokio::test]
    async fn unchanged_turn_emits_nothing_and_changed_reading_advances_per_key() {
        let samples = QuotaSamples::new("epoch".into());
        let first = entry(Some("a"), "2026-01-01T00:00:00Z");
        samples.observe(vec![first.clone(), first.clone()]).await;
        assert_eq!(samples.pop().unwrap().body["seq"], 1);
        assert!(samples.pop().is_none(), "one sample per key per turn");
        samples.observe(vec![first.clone()]).await;
        assert!(
            samples.pop().is_none(),
            "an unchanged turn must publish nothing"
        );
        samples
            .observe(vec![first, entry(Some("b"), "2026-01-01T00:00:00Z")])
            .await;
        let b = samples.pop().unwrap();
        assert_eq!(b.key.1.as_deref(), Some("b"));
        assert_eq!(b.body["seq"], 1);
        samples
            .observe(vec![entry(Some("a"), "2026-01-01T00:01:00Z")])
            .await;
        assert_eq!(samples.pop().unwrap().body["seq"], 2);
    }

    #[tokio::test]
    async fn unlabelled_rows_are_keys_and_absent_rows_are_withheld() {
        let samples = QuotaSamples::new("epoch".into());
        samples
            .observe(vec![entry(None, "2026-01-01T00:00:00Z")])
            .await;
        let first = samples.pop().unwrap();
        assert_eq!(first.key, ("deepseek".into(), None));
        assert!(!first.headers.contains_key("account"));
        samples
            .observe(vec![entry(None, "2026-01-01T00:01:00Z")])
            .await;
        assert_eq!(samples.pop().unwrap().body["seq"], 2);
        samples
            .observe(vec![entry(None, "2026-01-01T00:02:00Z")])
            .await;
        samples.observe(vec![]).await;
        assert!(samples.pop().is_none());
        samples.replay_current();
        assert!(samples.pop().is_none());
    }

    #[tokio::test]
    async fn overflow_drops_oldest_and_counts() {
        let samples = QuotaSamples::with_capacity("epoch".into(), 2);
        for minute in 0..3 {
            samples
                .observe(vec![entry(None, &format!("2026-01-01T00:0{minute}:00Z"))])
                .await;
        }
        assert_eq!(samples.metrics()["quotaSamplesDropped"], 1);
        assert_eq!(samples.pop().unwrap().body["seq"], 2);
        assert_eq!(samples.pop().unwrap().body["seq"], 3);
        assert!(samples.pop().is_none());
    }

    #[tokio::test]
    async fn refresher_never_waits_for_a_full_publish_queue() {
        let samples = QuotaSamples::with_capacity("epoch".into(), 1);
        samples
            .observe(vec![entry(None, "2026-01-01T00:00:00Z")])
            .await;
        let turn = samples.observe(vec![entry(None, "2026-01-01T00:01:00Z")]);
        tokio::time::timeout(Duration::from_millis(100), turn)
            .await
            .expect("refresher must not await publish queue capacity");
        assert_eq!(samples.pop().unwrap().body["seq"], 2);
    }

    #[tokio::test]
    async fn cached_body_digest_matches_notice() {
        let samples = QuotaSamples::new("epoch".into());
        samples
            .observe(vec![entry(Some("a"), "2026-01-01T00:00:00Z")])
            .await;
        let s = samples.pop().unwrap();
        samples.retain_body(&s);
        let reply = samples.events_get(&s.id, EVENT_NAME);
        let digest =
            ContentDigest::of_bytes(&serde_json::to_vec(&reply["result"]["body"]).unwrap());
        assert_eq!(
            digest.to_string(),
            reply["result"]["digest"],
            "served body must match the notice digest"
        );
        assert_eq!(digest, s.digest);
        assert_eq!(reply["result"]["event_key"], s.id);
        assert!(reply["result"]["published_at"].as_i64().unwrap() > 0);
    }

    #[tokio::test]
    async fn bodies_have_account_and_global_bounds_and_reconnect_replays_current() {
        let samples = QuotaSamples::new("epoch".into());
        samples
            .observe(vec![entry(None, "2026-01-01T00:00:00Z")])
            .await;
        let first = samples.pop().unwrap();
        samples.retain_body(&first);
        for _ in 0..16 {
            samples.replay_current();
            samples.retain_body(&samples.pop().unwrap());
        }
        assert_eq!(
            samples.events_get(&first.id, EVENT_NAME)["error"]["code"],
            "event_not_found"
        );
        assert_eq!(samples.pending.lock().unwrap().bodies.len(), 16);
        for account in 0..BODY_CAP {
            samples
                .observe(vec![entry(
                    Some(&format!("a{account}")),
                    "2026-01-01T00:00:00Z",
                )])
                .await;
            samples.retain_body(&samples.pop().unwrap());
        }
        assert_eq!(samples.pending.lock().unwrap().bodies.len(), BODY_CAP);
        assert_eq!(
            samples.events_get("invalid", EVENT_NAME)["error"]["code"],
            "invalid_request"
        );
        assert_eq!(
            samples.events_get(&first.id, "other")["error"]["code"],
            "event_name_mismatch"
        );
    }

    #[tokio::test]
    async fn unavailable_bus_does_not_block_registry_reads() {
        let samples = Arc::new(QuotaSamples::new("epoch".into()));
        let cancel = CancellationToken::new();
        let task = samples.spawn(PathBuf::from("/does/not/exist/subc.json"), cancel.clone());
        tokio::time::timeout(Duration::from_secs(3), async {
            while samples.metrics()["busPublishState"]["state"] != "unavailable" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unavailable bus must be reported");
        let registry = quota_core::Registry::new(vec![]);
        tokio::time::timeout(Duration::from_millis(100), registry.get_usage(None))
            .await
            .expect("usage.get must answer during bus failure");
        assert_eq!(samples.metrics()["busPublishState"]["state"], "unavailable");
        cancel.cancel();
        task.await.unwrap();
    }
}
