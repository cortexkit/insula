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
    let final_url;
    let html = match client
        .get(SETTINGS_URL)
        .header("Cookie", jar.header())
        .send()
        .await
    {
        Ok(response) => {
            final_url = response.url().to_string();
            match response.text().await {
                Ok(body) => body,
                Err(error) => {
                    eprintln!("  cannot check: body unreadable ({error})");
                    std::process::exit(2);
                }
            }
        }
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

    // WHICH PAGE ANSWERED, not just how big it was. reqwest follows redirects by
    // default, so a moved settings page is fetched, parsed and reported against
    // silently -- and "the labels changed" and "the page moved" are different
    // findings with different fixes. The anonymous fetch of this URL answers 303
    // today, which is what made the question worth printing.
    println!("  final url: {final_url}");
    if final_url != SETTINGS_URL {
        println!("  NOTE: redirected -- the labels below are from that page, not {SETTINGS_URL}");
    }

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

    // RECOGNISING NOTHING IS A FINDING, and this probe reported "findings: none"
    // for it until 2026-09-05. The original rule only asked whether the page
    // carried a label we IGNORE, so a page carrying none of our labels at all --
    // the total-drift case this tool exists to catch -- rendered identically to a
    // healthy page. It printed a clean verdict on the morning ollama went dark.
    //
    // The two states are opposite and the distinction cannot come from the label
    // list: "no labels I ignore" and "no labels at all" are the same emptiness
    // counted differently. So the denominator is checked explicitly.
    let recognised = OUR_LABELS.iter().filter(|l| html.contains(**l)).count();
    if recognised == 0 {
        eprintln!("  FINDING: the page carries NONE of the labels this module parses.");
        eprintln!("  That is total drift, not a healthy page -- every window this");
        eprintln!("  provider publishes comes from one of them, so the fetch fails");
        eprintln!("  with `no usage windows in settings HTML`.");
        let candidates = candidate_labels(&html);
        if candidates.is_empty() {
            // NO USAGE-SHAPED TEXT AT ALL is a different diagnosis from a renamed
            // label, and it is the one that says re-anchoring cannot work. A page
            // that renders its numbers client-side carries none of them in the
            // HTML, so no label list can reach them -- the lane needs whatever
            // endpoint the browser calls, not a better string.
            // NO USAGE-SHAPED TEXT AT ALL has two causes with opposite remedies,
            // and the page's OTHER furniture is what separates them. A settings
            // page still carrying its own controls is a real page whose usage
            // section is gone -- an account with no cloud plan, which is
            // `no_quota_reported` and nothing to fix here. A page carrying none of
            // its furniture either is a client-rendered shell, where the numbers
            // are fetched by script and no label list can ever reach them.
            //
            // Printed as a table rather than a verdict because this probe cannot
            // adjudicate: it reports which shape the page has and lets a reader
            // conclude. Guessing here would produce a confident wrong diagnosis in
            // exactly the case where the two remedies differ most.
            eprintln!("  and NO usage-shaped text at all, which is not a rename.");
            eprintln!("  page furniture, which separates a shell from a plan-less account:");
            for marker in [
                "Settings",
                "Sign out",
                "API keys",
                "Upgrade",
                "Subscription",
                "__NEXT_DATA__",
            ] {
                eprintln!("      {:<16} {}", marker, mark(html.contains(marker)));
            }
            eprintln!("  script tags: {}", html.matches("<script").count());
            eprintln!("  '% used' occurrences: {}", html.matches("% used").count());
        } else {
            eprintln!("  Candidate labels on the page, for re-anchoring:");
            for candidate in candidates {
                eprintln!("      {candidate}");
            }
        }
        std::process::exit(1);
    }

    if dropped.is_empty() {
        println!(
            "  findings: none -- {recognised} of {} labels present, and the page",
            OUR_LABELS.len()
        );
        println!("  carries no label this module ignores");
        return;
    }
    println!(
        "  FINDING: the page carries {dropped:?}, which this module does not parse, \
         so that window is not published at all"
    );
    std::process::exit(1);
}

/// Short usage-shaped strings on the page, for re-anchoring after a drift.
///
/// Deliberately crude: any text node containing "usage" or "limit", bounded to
/// something label-sized. This exists so the probe ANSWERS the question it just
/// raised rather than sending a reader to open the page by hand -- which is the
/// step where account identifiers get pasted into a terminal.
///
/// Bounded to 60 characters and deduplicated, because an unbounded dump of a
/// 117 KB page is not a report.
fn candidate_labels(html: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for chunk in html.split('>') {
        let text = chunk.split('<').next().unwrap_or_default().trim();
        if text.is_empty() || text.len() > 60 {
            continue;
        }
        let lower = text.to_lowercase();
        if !(lower.contains("usage") || lower.contains("limit")) {
            continue;
        }
        let owned = text.to_string();
        if !seen.contains(&owned) {
            seen.push(owned);
        }
    }
    seen
}

fn mark(present: bool) -> &'static str {
    if present {
        "PRESENT"
    } else {
        "absent"
    }
}
