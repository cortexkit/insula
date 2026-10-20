//! Which Antigravity pool does a cloud quota request reach for a paid account?
//!
//! For an account on a paid tier, the cloud summary endpoint has answered with a
//! different, standard-tier pool than the one the Antigravity editor reports
//! (measured 2026-09-08). So insula withholds its cloud reading for paid accounts
//! and relies on the editor or the plugin's sidebar file. The opencode
//! antigravity-auth plugin's vault-mode poller, though, reads the editor's own
//! pool with the same vault token (checked against the editor, 2026-10-12). Its
//! request differs from insula's in endpoint host and User-Agent.
//!
//! This sends both request shapes with the vault credential and prints each
//! bucket's remaining fraction and reset, so they can be compared against the
//! editor's own figures. The reset anchors are the discriminator: two pools can
//! share a percentage, never a weekly reset to the second.
//!
//! The credential is read through a capability handle named in the file at
//! `INSULA_PROBE_CAPABILITY_FILE`; mint it with `ck auth mint-handle --id
//! antigravity:google` and revoke it afterwards. The token is never printed.
//! Needs the daemon running.

use quota_core::credential_source::{CredentialSource, VaultCapability};
use quota_module::vault_client::VaultClient;

#[path = "support/connection.rs"]
mod connection;

struct Shape {
    label: &'static str,
    url: &'static str,
    user_agent: String,
}

#[tokio::main]
async fn main() {
    let (path, _) = connection::connection_and_args();
    let Some(file) = std::env::var_os("INSULA_PROBE_CAPABILITY_FILE") else {
        eprintln!("set INSULA_PROBE_CAPABILITY_FILE to a file holding the ckh_ handle");
        std::process::exit(2);
    };
    let capability = std::fs::read_to_string(&file)
        .map(|text| text.trim().to_string())
        .unwrap_or_default();
    if !capability.starts_with("ckh_") {
        eprintln!("the capability file holds no ckh_ handle: nothing was checked");
        std::process::exit(2);
    }

    let client = VaultClient::new(path);
    let mut credential = match client.get(&VaultCapability::new(capability), 120_000).await {
        Ok(credential) => credential,
        Err(error) => {
            eprintln!("vault refused the credential ({error:?}): nothing was checked");
            std::process::exit(2);
        }
    };
    let project = credential.project_id.clone();
    println!(
        "  credential: {} (record version {}), project {}",
        credential.email.as_deref().unwrap_or("<no email>"),
        credential.record_version,
        if project.is_some() {
            "present"
        } else {
            "absent"
        }
    );
    let token = match quota_core::credential_source::take_utf8_payload(&mut credential.payload) {
        Ok(token) => token,
        Err(error) => {
            eprintln!("credential payload unusable ({error})");
            std::process::exit(2);
        }
    };

    let (os, arch) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("darwin", "arm64"),
        ("macos", _) => ("darwin", "amd64"),
        (os, "aarch64") => (os, "arm64"),
        (os, _) => (os, "amd64"),
    };
    const PROD: &str = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
    const DAILY: &str =
        "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
    let hub = format!("antigravity/hub/2.9.1 {os}/{arch}");
    let cli = format!(
        "antigravity/cli/1.3.3 (aidev_client; os_type={os}; arch={arch}; \
         cl=996823801; auth_method=consumer)"
    );
    // The two real shapes, then the two mixed ones, so a disagreement between
    // the real shapes can be pinned on the host or on the User-Agent.
    let shapes = [
        Shape {
            label: "insula (prod, hub UA)",
            url: PROD,
            user_agent: hub.clone(),
        },
        Shape {
            label: "plugin vault poller (daily, cli UA)",
            url: DAILY,
            user_agent: cli.clone(),
        },
        Shape {
            label: "mixed (prod, cli UA)",
            url: PROD,
            user_agent: cli,
        },
        Shape {
            label: "mixed (daily, hub UA)",
            url: DAILY,
            user_agent: hub,
        },
    ];
    let body = match &project {
        Some(project) => serde_json::json!({ "project": project }),
        None => serde_json::json!({}),
    };
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .build()
        .expect("client");
    for shape in shapes {
        println!("\n  {}", shape.label);
        let response = http
            .post(shape.url)
            .bearer_auth(&token)
            .header("User-Agent", &shape.user_agent)
            .json(&body)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                println!("    transport error: {}", error.without_url());
                continue;
            }
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            println!("    HTTP {status}: {}", &text[..text.len().min(160)]);
            continue;
        }
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let groups = parsed
            .pointer("/groups")
            .or_else(|| parsed.pointer("/response/groups"))
            .and_then(|groups| groups.as_array())
            .cloned()
            .unwrap_or_default();
        for group in groups {
            for bucket in group
                .get("buckets")
                .and_then(|buckets| buckets.as_array())
                .cloned()
                .unwrap_or_default()
            {
                println!(
                    "    {:<15} remaining {:<12} reset {:<22} disabled {}",
                    bucket
                        .get("bucketId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?"),
                    bucket
                        .get("remainingFraction")
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "-".into()),
                    bucket
                        .get("resetTime")
                        .and_then(|v| v.as_str())
                        .unwrap_or("-"),
                    bucket
                        .get("disabled")
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "-".into()),
                );
            }
        }
    }
}
