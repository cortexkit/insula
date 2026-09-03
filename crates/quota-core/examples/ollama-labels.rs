//! Which usage labels and plan headings the live ollama settings page carries.
//!
//! This provider is an HTML scrape, so its parser names the page's own strings —
//! and the page has now moved twice. Each time, the failure was SILENT in the
//! direction that matters: a block whose label we no longer recognise is not an
//! error, it is a window that stops being published, and a window that stops
//! being published reads downstream as capacity that is not being consumed.
//! That is the shape that cost the fleet an outage on 2026-07-25, where an
//! exhausted weekly window went out with no reset and read as fully open.
//!
//! Upstream's v0.56.x parser added a `Monthly usage` label and renamed the plan
//! heading from `Cloud Usage` to `Included usage <plan>`, retaining the legacy
//! labels "for older pages". Whether THIS host's page renders the new labels,
//! the old ones, or both is a question about the page rather than about either
//! implementation, so it is answered by looking rather than by reading a diff.
//!
//! Reports which strings are PRESENT. Never prints page content, since the
//! settings page carries account identifiers.

use quota_core::browser_cookies;

const DOMAIN: &str = "ollama.com";
const SETTINGS_URL: &str = "https://ollama.com/settings";

/// Every label either parser knows about, ours first.
///
/// Ours is the authority for what we currently publish; theirs is the authority
/// for what the page can contain. A label present on the page and absent from
/// our list is a dropped window; one in our list and absent from the page is a
/// window that has gone away, which is a different and less dangerous fact.
const OUR_LABELS: &[&str] = &["Session usage", "Hourly usage", "Weekly usage"];
const THEIR_ADDED_LABELS: &[&str] = &["Monthly usage"];
const PLAN_HEADINGS: &[&str] = &["Cloud Usage", "Included usage"];

#[tokio::main]
async fn main() {
    let jar = match browser_cookies::chrome_cookies_for(DOMAIN) {
        Ok(jar) => jar,
        Err(error) => {
            eprintln!("  cannot check: no {DOMAIN} cookie on this host ({error})");
            eprintln!("  a missing cookie is not evidence about the page");
            std::process::exit(2);
        }
    };
    if jar.cookies.is_empty() {
        eprintln!("  cannot check: the {DOMAIN} jar is empty");
        std::process::exit(2);
    }

    // Requests directly rather than through this crate's `JsonRequest`, whose
    // send helpers are crate-private. Deliberately NOT widening that privacy for
    // a diagnostic: those helpers carry the production invariants (one pooled
    // client below the poll interval, URL stripping on transport errors, empty-2xx
    // classification), and every one of them exists for the refresher's fetch
    // path rather than for a one-shot look at a page. The source-walk test that
    // forbids a bare client covers provider modules, which is where those
    // invariants must hold.
    let client = quota_core::http::provider_client();
    let html = match client
        .get(SETTINGS_URL)
        .header("Cookie", jar.header())
        .send()
        .await
    {
        Ok(response) => match response.text().await {
            Ok(body) => body,
            Err(error) => {
                eprintln!("  cannot check: body unreadable ({error})");
                std::process::exit(2);
            }
        },
        Err(error) => {
            // Printed without the URL, matching the production rule: a reqwest
            // error's Display appends the request URL, and this one carries no
            // query string today but the habit is what keeps that true.
            eprintln!(
                "  cannot check: {SETTINGS_URL} did not answer ({})",
                error.without_url()
            );
            std::process::exit(2);
        }
    };

    // A refused or redirected page parses as "no labels present", which would
    // read exactly like a page that dropped them all. Bound that before
    // reporting anything: the denominator is the page's own size.
    println!("  page: {} bytes", html.len());
    if html.len() < 2_000 {
        eprintln!("  cannot check: page too small to be the settings page --");
        eprintln!("  a login redirect answers 200 and would report every label absent");
        std::process::exit(2);
    }

    let mut dropped = Vec::new();
    println!("  labels this module parses:");
    for label in OUR_LABELS {
        println!("      {:<16} {}", label, mark(html.contains(label)));
    }
    println!("  labels upstream added at v0.56.x:");
    for label in THEIR_ADDED_LABELS {
        let present = html.contains(label);
        println!("      {:<16} {}", label, mark(present));
        if present {
            dropped.push(*label);
        }
    }
    println!("  plan headings:");
    for heading in PLAN_HEADINGS {
        println!("      {:<16} {}", heading, mark(html.contains(heading)));
    }

    if dropped.is_empty() {
        println!("  findings: none -- the page carries no label this module ignores");
        return;
    }
    println!(
        "  FINDING: the page carries {dropped:?}, which this module does not parse, \
         so that window is not published at all"
    );
    std::process::exit(1);
}

fn mark(present: bool) -> &'static str {
    if present {
        "PRESENT"
    } else {
        "absent"
    }
}
