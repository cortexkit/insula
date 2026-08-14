//! Which OpenCode call fails, and does the billing call answer where it does.
//!
//! The provider makes two server-function calls and reports one error for both,
//! so a persistent HTTP 500 says nothing about which stage produced it. That
//! ambiguity is what made "the site is having an outage" the natural reading:
//! an error with no stage in it is compatible with every explanation, and the
//! cheapest one wins by default.
//!
//! CodexBar v0.49.6 added a fallback whose stated trigger is a subscription call
//! that "answers with null or fails outright" on pay-as-you-go workspaces, which
//! is a different reading of the same symptom -- not an outage, a workspace type
//! that has no subscription object to fetch. This probe separates them from the
//! live host rather than by reasoning: it runs the stages one at a time and then
//! asks the billing server function whether it answers with the same cookie.
//!
//! Reads the real Chrome cookie for opencode.ai. Prints no cookie material.

use quota_core::browser_cookies;
use quota_core::opencode;

#[tokio::main]
async fn main() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .build()
        .expect("client");

    let jar = match browser_cookies::chrome_cookies_for("opencode.ai") {
        Ok(jar) => jar,
        Err(error) => {
            eprintln!("cookie store unreadable ({error}): the question is unanswered.");
            std::process::exit(2);
        }
    };
    let Some(cookie) = opencode::request_cookie_header(&jar) else {
        eprintln!("no opencode.ai session cookie in the Chrome store: nothing to probe.");
        eprintln!("this is not a clean result -- the question is unanswered.");
        std::process::exit(2);
    };
    println!("  cookie: present ({} bytes, not printed)", cookie.len());

    print!("  stage 1  workspaces  ");
    let workspace = match opencode::fetch_workspace_id(&client, &cookie).await {
        Ok(id) => {
            println!("OK (workspace resolved)");
            id
        }
        Err(error) => {
            println!("FAILED: {error}");
            println!("\n  The failure is in the workspaces call, before any subscription");
            println!("  request is made. The pay-as-you-go reading does not apply.");
            std::process::exit(1);
        }
    };

    print!("  stage 2  subscription  ");
    match opencode::fetch_subscription_text(&client, &cookie, &workspace).await {
        Ok(text) => {
            println!("OK ({} bytes)", text.len());
            println!("\n  Both stages answer. Whatever the module reported is not reproduced");
            println!("  here, so the transient reading is the live one.");
            return;
        }
        Err(error) => println!("FAILED: {error}"),
    }

    println!("\n  The workspaces call answers and the subscription call does not, which is");
    println!("  the exact shape upstream's fallback was added for. Asking billing:");

    print!("  stage 3  billing  ");
    match opencode::fetch_billing_text(&client, &cookie, &workspace).await {
        Ok(text) => {
            println!("OK ({} bytes)", text.len());
            println!("    --- first 400 bytes ---");
            let head = &text[..text.len().min(400)];
            println!("    {}", head.replace('\n', " "));
            println!("    --- fields ---");
            for field in ["monthlyUsage", "monthlyLimit", "balance", "subscription"] {
                // The payload is a JS object literal, not JSON: keys are unquoted.
                // Searching for a quoted key finds nothing and reads as an absent
                // field, which is the same output a genuinely empty payload gives.
                let shown = text
                    .find(&format!("{field}:"))
                    .map(|at| {
                        let end = (at + 46).min(text.len());
                        text[at..end].replace('\n', " ")
                    })
                    .unwrap_or_else(|| "(absent)".to_string());
                println!("    {field:14} {shown}");
            }
            // Keyed on the payload carrying spend, not on the call returning 200.
            // An empty 200 is the shape that made the first version of this probe
            // print "answered" for a workspace with nothing in it.
            let has_spend = text
                .find("monthlyUsage:")
                .is_some_and(|at| !text[at..].starts_with("monthlyUsage:null"));
            if has_spend {
                println!("\n  ANSWERED WITH SPEND. The subscription failure is a workspace-type");
                println!("  fact rather than an outage, and this account can be reported from");
                println!("  the billing payload instead of degraded.");
            } else {
                println!("\n  ANSWERED, WITH NO SPEND TO REPORT. monthlyUsage is null, so the");
                println!("  upstream fallback would return nothing here too and rethrow the");
                println!("  subscription error. Pay-as-you-go does not explain this host.");
                println!("  What IS established: workspaces and billing both answer on this");
                println!("  cookie, so the subscription failure is specific to that call --");
                println!("  not a session, a cookie, or a site-wide outage.");
            }
        }
        Err(error) => {
            println!("FAILED: {error}");
            println!("\n  Billing does not answer either. Two failing calls with one cookie");
            println!("  is consistent with an outage, and the pay-as-you-go reading is not");
            println!("  supported on this host.");
        }
    }
}
