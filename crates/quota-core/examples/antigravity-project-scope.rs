//! Why does the cloud lane report a different Gemini pool than the editor?
//!
//! Our cloud lane and the editor's own language server disagree about ONE of the
//! two pools on the SAME account, read in the same second:
//!
//! ```text
//! 3p weekly       agy 0.90637 / 2026-09-09T18:02:29Z   ours 0.9064 / same     AGREE
//! gemini weekly   agy 0.41587 / 2026-09-10T18:41:37Z   ours 0.9285 / 09-08    DIFFER
//! ```
//!
//! One pool agreeing to the second rules out a different account and a different
//! endpoint, so the disagreement lives inside one request. This varies the parts
//! of that request one at a time and prints the bucket under investigation.
//!
//! Ruled out so far, by running it: the `project` field (both spellings return
//! identical numbers), a tier id in the body (every spelling refused with HTTP
//! 400, so the tier is not a request field), and the editor's `User-Agent`.
//!
//! Read-only: quota reads and a token refresh, no mutation and nothing written.
//!
//! ```text
//! cargo run -p quota-core --example antigravity-project-scope
//! ```

use quota_core::antigravity::AntigravityProvider;

/// One row of the request matrix: a label, the body, and any extra headers.
///
/// Named because the whole point is to vary the parts of the request one at a
/// time, so the shape of a row IS the experiment.
type Attempt = (&'static str, serde_json::Value, Vec<(&'static str, String)>);

#[tokio::main]
async fn main() {
    let provider = AntigravityProvider::new();

    let accounts = provider.probe_plugin_accounts();
    if accounts.is_empty() {
        println!("no plugin accounts on this host: nothing to probe");
        std::process::exit(2);
    }

    println!("plugin accounts: {}", accounts.len());

    for (email, project, refresh_token) in &accounts {
        let email = email.clone().unwrap_or_else(|| "<no email>".into());
        println!("\naccount: {email}");
        println!(
            "managedProjectId: {}",
            project.as_deref().unwrap_or("<absent>")
        );

        let refresh = refresh_token.clone().unwrap_or_default();
        if refresh.is_empty() {
            println!("  no refresh token on this account");
            continue;
        }
        let token = match provider.probe_access_token(&refresh).await {
            Ok(token) => token,
            Err(error) => {
                println!("  token refresh failed: {error}");
                continue;
            }
        };

        // WHO IS THIS TOKEN, actually? The store's `email` field is a label the
        // plugin wrote; it is not evidence about the credential. Everything
        // downstream rests on the token belonging to the account the editor
        // shows, and that was inferred from one pool agreeing rather than
        // checked. Ask Google.
        match userinfo(&token).await {
            Ok(who) => println!("  token resolves to: {who}"),
            Err(error) => println!("  token identity unknown: {error}"),
        }

        // The account's `fingerprint` records exactly the caller-identity fields
        // the editor presents: an api client string and a clientMetadata block
        // naming the IDE. Those are the remaining difference now that the body
        // and the User-Agent are both ruled out.
        let editor_agent = "antigravity/cli/1.1.24 (aidev_client; os_type=darwin; \
                            arch=arm64; cl=974782877; auth_method=consumer)";
        let api_client = "google-cloud-sdk vscode/1.86.0";
        let ide_meta = serde_json::json!({
            "ideType": "ANTIGRAVITY",
            "platform": "DARWIN",
            "pluginType": "GEMINI"
        })
        .to_string();

        let with_project = serde_json::json!({ "project": project });
        let with_metadata = serde_json::json!({
            "project": project,
            "metadata": {
                "ideType": "ANTIGRAVITY",
                "platform": "DARWIN",
                "pluginType": "GEMINI"
            }
        });

        let attempts: Vec<Attempt> = vec![
            ("baseline (shipped)", with_project.clone(), vec![]),
            (
                "x-goog-api-client",
                with_project.clone(),
                vec![("X-Goog-Api-Client", api_client.to_string())],
            ),
            (
                "client-metadata hdr",
                with_project.clone(),
                vec![("Client-Metadata", ide_meta.clone())],
            ),
            ("metadata in body", with_metadata.clone(), vec![]),
            (
                "all of them",
                with_metadata.clone(),
                vec![
                    ("X-Goog-Api-Client", api_client.to_string()),
                    ("Client-Metadata", ide_meta.clone()),
                    ("User-Agent", editor_agent.to_string()),
                ],
            ),
        ];

        for (label, body, headers) in attempts {
            match provider.probe_quota_summary(&token, body, &headers).await {
                Ok(raw) => print_gemini_weekly(label, &raw),
                Err(error) => println!("  {label:<22}: {error}"),
            }
        }

        // DOES GOOGLE CONSIDER THIS CREDENTIAL ULTRA OR FREE? Every request shape
        // returns the same pool, so the tier is resolved from the token rather
        // than from anything we send. `loadCodeAssist` reports the tier directly,
        // which turns the conclusion from "nothing I send changes it" into a
        // statement about what the credential IS -- the difference between a
        // well-supported inference and a measurement.
        match load_code_assist(&token, project.as_deref()).await {
            Ok(tier) => println!("  loadCodeAssist        : {tier}"),
            Err(error) => println!("  loadCodeAssist        : {error}"),
        }
    }
}

/// What tier does Google resolve for THIS credential?
///
/// The editor's own server states `Google AI Ultra` for the same email. If this
/// answers a free tier, the tier follows the credential and no request shape can
/// reach the paid pool; if it answers Ultra, the tier is right and the quota
/// endpoint is choosing a different pool for another reason. Those are opposite
/// conclusions with opposite fixes, which is why it is worth one more call.
async fn load_code_assist(access_token: &str, project: Option<&str>) -> Result<String, String> {
    let body = match project {
        Some(project) => {
            serde_json::json!({ "cloudaicompanionProject": project, "metadata": {} })
        }
        None => serde_json::json!({ "metadata": {} }),
    };
    let response = reqwest::Client::new()
        .post("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist")
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", &text[..text.len().min(200)]));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let current = parsed
        .pointer("/currentTier/id")
        .and_then(|v| v.as_str())
        .unwrap_or("<absent>");
    let name = parsed
        .pointer("/currentTier/name")
        .and_then(|v| v.as_str())
        .unwrap_or("<absent>");
    let allowed: Vec<&str> = parsed
        .get("allowedTiers")
        .and_then(|v| v.as_array())
        .map(|tiers| {
            tiers
                .iter()
                .filter_map(|t| t.get("id").and_then(|v| v.as_str()))
                .collect()
        })
        .unwrap_or_default();
    Ok(format!(
        "currentTier={current} ({name})  allowedTiers={allowed:?}"
    ))
}

/// Ask Google which account an access token belongs to.
///
/// Deliberately not derived from the account store: the question is whether the
/// store's label and the credential agree, and reading the label to answer that
/// assumes what is being tested.
async fn userinfo(access_token: &str) -> Result<String, String> {
    let response = reqwest::Client::new()
        .get("https://www.googleapis.com/oauth2/v3/userinfo")
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", &body[..body.len().min(160)]));
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let email = parsed
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("<no email claim>");
    let sub = parsed.get("sub").and_then(|v| v.as_str()).unwrap_or("?");
    Ok(format!("{email}  (sub {sub})"))
}

/// Print only the bucket under investigation, so the matrix stays readable.
///
/// The other three buckets already agree with the editor, so reprinting them
/// once per attempt would bury the one number that is in question.
fn print_gemini_weekly(label: &str, raw: &str) {
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            println!("  {label:<22}: unparseable: {error}");
            return;
        }
    };
    // The cloud endpoint answers with `groups` at the top level; the local
    // language server wraps the same shape in a `response` envelope. Accept
    // either, because the point of comparison is the buckets, and keying on one
    // spelling would report a real answer as an empty one.
    let groups = parsed
        .pointer("/response/groups")
        .or_else(|| parsed.pointer("/groups"))
        .and_then(|g| g.as_array())
        .cloned()
        .unwrap_or_default();
    if groups.is_empty() {
        println!("  {label:<22}: no groups in the response");
        return;
    }
    for group in groups {
        for bucket in group
            .get("buckets")
            .and_then(|b| b.as_array())
            .cloned()
            .unwrap_or_default()
        {
            if bucket.get("bucketId").and_then(|v| v.as_str()) != Some("gemini-weekly") {
                continue;
            }
            let reset = bucket
                .get("resetTime")
                .and_then(|v| v.as_str())
                .unwrap_or("<none>");
            match bucket
                .get("remainingFraction")
                .and_then(serde_json::Value::as_f64)
            {
                Some(fraction) => println!(
                    "  {label:<22}: gemini-weekly used={:>6.2}%  reset={reset}",
                    (1.0 - fraction) * 100.0
                ),
                None => println!("  {label:<22}: gemini-weekly used=<absent>"),
            }
        }
    }
}
