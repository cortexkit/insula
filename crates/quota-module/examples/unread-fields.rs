//! What do the usage-response fields insula does not read actually hold?
//!
//! `unread_keys` logs the NAMES of response keys the typed structs skip, never
//! their values, so the log can say that Claude's windows carry
//! `limit_dollars` or that Codex sends a `code_review_rate_limit`, but not
//! whether those are real limits. Deciding whether to publish one needs its
//! values, and those arrive only in a live response.
//!
//! This fetches one live usage response with the provider's own request shape
//! and prints the fields under study in full, and every other key as a SHAPE
//! only (type, length, keys). Identity values -- ids, emails, org names -- are
//! redacted wherever they appear. Nothing is written anywhere; it prints to
//! the terminal that ran it.
//!
//! THE CREDENTIAL. A standalone binary reaches the vault as a direct
//! principal, which the scoped listing refuses, so this takes a capability
//! handle minted for one credential and read from a file (never argv, which
//! other processes can read). Revoke the handle afterwards; `ck auth
//! mint-handle` prints the exact revoke command on stderr.
//!
//!     umask 077; ck auth mint-handle --id oauth:anthropic > /tmp/h 2> /tmp/h.revoke
//!     QUOTA_PROBE_HANDLE_FILE=/tmp/h cargo run -p quota-module --example unread-fields -- claude
//!
//! Exit 0 printed a response; 2 could not get one (no daemon, refused
//! credential, non-2xx), so nothing was learned.

use quota_core::credential_source::{CredentialSource, VaultCapability, VAULT_READ_MIN_TTL_MS};
use quota_module::vault_client::VaultClient;
use serde_json::Value;

/// The keys whose VALUES are the question, per provider. Everything else in
/// the response prints as a shape.
const CLAUDE_KEYS: &[&str] = &[
    "five_hour",
    "seven_day",
    "seven_day_opus",
    "seven_day_sonnet",
    "seven_day_breakdown",
    "seven_day_oauth_apps",
    "seven_day_cowork",
    "seven_day_omelette",
    "limits",
    "member_dashboard_available",
    "extra_usage",
    "spend",
];
const CODEX_KEYS: &[&str] = &[
    "plan_type",
    "rate_limit",
    "code_review_rate_limit",
    "additional_rate_limits",
    "rate_limit_reset_credits",
    "rate_limit_reached_type",
    "promo",
    "credits",
    "spend_control",
];

/// Keys whose values identify an account, redacted at any depth.
fn is_identity_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "id"
        || key.ends_with("_id")
        || key.ends_with("uuid")
        || key.contains("email")
        || key.starts_with("org")
        || key.contains("name")
}

fn connection_file() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("SUBC_CONNECTION_FILE") {
        return std::path::PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join(".local/share/cortexkit/run/subc-connection.json")
}

fn refuse(message: &str) -> ! {
    eprintln!("{message}");
    eprintln!("nothing was learned: exit 2");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let provider = std::env::args().nth(1).unwrap_or_default();
    let keys = match provider.as_str() {
        "claude" => CLAUDE_KEYS,
        "codex" => CODEX_KEYS,
        _ => refuse("usage: unread-fields <claude|codex> (handle in QUOTA_PROBE_HANDLE_FILE)"),
    };
    let Ok(handle_file) = std::env::var("QUOTA_PROBE_HANDLE_FILE") else {
        refuse("QUOTA_PROBE_HANDLE_FILE is not set");
    };
    let handle = match std::fs::read_to_string(&handle_file) {
        Ok(text) => text.trim().to_string(),
        Err(error) => refuse(&format!("cannot read the handle file: {error}")),
    };
    if handle.is_empty() {
        refuse("the handle file is empty");
    }
    let path = connection_file();
    if !path.exists() {
        refuse(&format!("no daemon connection file at {}", path.display()));
    }

    let client = VaultClient::new(path);
    let mut credential = match client
        .get(&VaultCapability::new(handle), VAULT_READ_MIN_TTL_MS)
        .await
    {
        Ok(credential) => credential,
        Err(error) => refuse(&format!("the vault refused the credential: {error:?}")),
    };
    let bearer = match quota_core::credential_source::take_utf8_payload(&mut credential.payload) {
        Ok(value) => value,
        Err(error) => refuse(&format!("payload is not a usable bearer: {error}")),
    };

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("client");
    // THE PROVIDERS' OWN REQUEST SHAPE, copied from anthropic.rs and codex.rs:
    // a different request can get a different response, which would answer a
    // question about this probe rather than about what insula receives.
    let request = match provider.as_str() {
        "claude" => http
            .get("https://api.anthropic.com/api/oauth/usage")
            .bearer_auth(&bearer)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", "claude-code/2.1.0")
            .header("accept", "application/json"),
        _ => {
            let Some(account_id) = credential.account_id.clone() else {
                refuse("the codex credential carries no account id for ChatGPT-Account-Id");
            };
            http.get("https://chatgpt.com/backend-api/wham/usage")
                .bearer_auth(&bearer)
                .header("ChatGPT-Account-Id", account_id)
                .header("User-Agent", "ai-provider-quota")
                .header("accept", "application/json")
        }
    };
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => refuse(&format!("transport: {}", error.without_url())),
    };
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    if !status.is_success() {
        refuse(&format!("HTTP {status}, {} bytes", body.len()));
    }
    let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(&body) else {
        refuse(&format!("HTTP {status} but the body is not a JSON object"));
    };

    println!("{provider}: HTTP {status}, {} top-level keys", root.len());
    println!();
    println!("FIELDS UNDER STUDY (values, identity redacted):");
    for key in keys {
        match root.get(*key) {
            Some(value) => println!(
                "  {key} = {}",
                serde_json::to_string(&redact(value)).unwrap_or_default()
            ),
            None => println!("  {key}: absent"),
        }
    }
    println!();
    println!("EVERY OTHER KEY (shape only):");
    let mut others: Vec<_> = root
        .iter()
        .filter(|(k, _)| !keys.contains(&k.as_str()))
        .collect();
    others.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in others {
        // A key whose value has the shape of a usage window is printed in full
        // whatever its name: a window insula does not publish is exactly what
        // this probe exists to find, and a codename says nothing about whether
        // it limits the account.
        if is_window_shaped(value) {
            println!(
                "  {key} = {}   <- window-shaped",
                serde_json::to_string(&redact(value)).unwrap_or_default()
            );
        } else {
            println!("  {key}: {}", shape(value));
        }
    }
}

/// An object carrying a utilization figure: the shape of a rate window.
fn is_window_shaped(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.contains_key("utilization") || map.contains_key("used_percent"))
}

/// A copy of `value` with identity values replaced, at any depth.
fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| {
                    let child = if is_identity_key(key) && !child.is_null() {
                        Value::String("<redacted>".to_string())
                    } else {
                        redact(child)
                    };
                    (key.clone(), child)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::String(text) if text.contains('@') => Value::String("<redacted>".to_string()),
        other => other.clone(),
    }
}

/// The type and size of a value, never its content.
fn shape(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(text) => format!("string ({} chars)", text.chars().count()),
        Value::Array(items) => format!("array of {}", items.len()),
        Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            format!("object {{{}}}", keys.join(", "))
        }
    }
}
