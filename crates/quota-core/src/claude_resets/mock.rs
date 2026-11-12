//! A local stand-in for Anthropic's usage, profile and redemption endpoints,
//! and a vault serving one synthetic Claude credential. For tests only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::credential_source::{CredentialSource, VaultCredential, VaultGetError};

pub const ACCOUNT: &str = "synthetic-claude-account";
pub const ORGANIZATION: &str = "00000000-0000-4000-8000-0000000000a1";
pub const BEARER: &str = "synthetic-claude-token";
pub const RECORD_VERSION: u64 = 7;
pub const CREDENTIAL_ID: &str = "oauth:anthropic:synthetic";

/// A served usage body: both windows at their limit, with `cedar_ember`.
pub fn usage_body(cedar_ember: Value) -> String {
    json!({
        "five_hour": {"utilization": 100, "resets_at": "2099-01-01T00:00:00Z"},
        "seven_day": {"utilization": 100, "resets_at": "2099-01-02T00:00:00Z"},
        "cedar_ember": cedar_ember,
    })
    .to_string()
}

/// One usable grant: started, unexpired, unpaused, one reset left. `extra`
/// is merged in last, so it can name the kind or override any field.
pub fn grant(id: &str, ends_at: &str, extra: Value) -> Value {
    let mut grant = json!({
        "id": id,
        "label": "synthetic grant label",
        "resets_total": 1,
        "resets_left": 1,
        "starts_at": "2020-01-01T00:00:00Z",
        "ends_at": ends_at,
        "paused": false,
        "usable_now": true,
        "use_requires_limit": true,
        "unavailable_reason": null,
    });
    if let (Some(grant), Some(extra)) = (grant.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            grant.insert(key.clone(), value.clone());
        }
    }
    grant
}

/// An eligible block holding `grants`.
pub fn eligible(grants: Vec<Value>, next_grant_id: Option<&str>) -> Value {
    json!({"eligible": true, "grants": grants, "next_grant_id": next_grant_id})
}

pub struct MockAnthropic {
    pub base: String,
    redeems: Arc<Mutex<Vec<String>>>,
    reply: Arc<Mutex<(u16, String)>>,
    usage: Arc<Mutex<String>>,
    profiles: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockAnthropic {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockAnthropic {
    /// Serve `usage` from the usage endpoint and `{"result":"reset"}` from
    /// the redemption endpoint until told otherwise.
    pub async fn start(usage: String) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let redeems = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new((200, r#"{"result":"reset"}"#.to_string())));
        let usage = Arc::new(Mutex::new(usage));
        let profiles = Arc::new(AtomicUsize::new(0));
        let (served_redeems, served_reply, served_usage, served_profiles) = (
            Arc::clone(&redeems),
            Arc::clone(&reply),
            Arc::clone(&usage),
            Arc::clone(&profiles),
        );
        let redeem_path = format!("/api/organizations/{ORGANIZATION}/reset_rate_limits");
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = crate::loopback::read_request(&mut socket).await;
                let mut words = request.split_whitespace();
                let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
                let (status, body) = match (method, path) {
                    ("GET", "/api/oauth/usage?cedar_ember=1") => {
                        (200, served_usage.lock().unwrap().clone())
                    }
                    ("GET", "/api/oauth/profile") => {
                        served_profiles.fetch_add(1, Ordering::SeqCst);
                        (
                            200,
                            json!({"account": {"uuid": "00000000-0000-4000-8000-0000000000b2",
                                    "email_address": "claude@example.com"},
                                   "organization": {"uuid": ORGANIZATION}})
                            .to_string(),
                        )
                    }
                    ("POST", path) if path == redeem_path => {
                        served_redeems.lock().unwrap().push(request.clone());
                        served_reply.lock().unwrap().clone()
                    }
                    _ => (404, "{}".to_string()),
                };
                let _ = socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} Mock\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        Self {
            base,
            redeems,
            reply,
            usage,
            profiles,
            task,
        }
    }

    /// Answer the next redemption requests with `status` and `body`.
    pub fn set_reply(&self, status: u16, body: &str) {
        *self.reply.lock().unwrap() = (status, body.to_string());
    }

    pub fn set_usage(&self, usage: String) {
        *self.usage.lock().unwrap() = usage;
    }

    /// Every redemption request received, as raw HTTP text.
    pub fn redeems(&self) -> Vec<String> {
        self.redeems.lock().unwrap().clone()
    }

    pub fn profile_reads(&self) -> usize {
        self.profiles.load(Ordering::SeqCst)
    }
}

/// A vault serving one Claude OAuth credential for [`ACCOUNT`], recording the
/// auth failures reported against it.
#[derive(Default)]
pub struct MockSource {
    pub reports: Mutex<Vec<(u16, u64)>>,
}

#[async_trait]
impl CredentialSource for MockSource {
    async fn get_scoped(
        &self,
        _credential_id: &str,
        _min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError> {
        Ok(VaultCredential {
            payload: BEARER.as_bytes().to_vec(),
            expires_at_ms: None,
            record_version: RECORD_VERSION,
            account_id: Some(ACCOUNT.to_string()),
            project_id: None,
            email: Some("claude@example.com".to_string()),
            org_name: None,
        })
    }

    async fn report_auth_failure_scoped(
        &self,
        _credential_id: &str,
        provider_status: u16,
        record_version: u64,
    ) {
        self.reports
            .lock()
            .unwrap()
            .push((provider_status, record_version));
    }
}
