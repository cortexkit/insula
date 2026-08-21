//! Find where the qwencloud console's `sec_token` lives now.
//!
//! The token-plan page used to carry `SEC_TOKEN` inside an inline
//! `ONE_CONSOLE_TOOL` block, and the scrape read it from there. Alibaba
//! rebuilt the console as a JavaScript app: the page is now a small shell that
//! loads bundles, the token is not in it, and `qwen-cloud` went dark.
//!
//! A capture of the working browser (2026-08-20) settles what did NOT change:
//! the gateway host, the `usage` and `quota-config` request bodies, and the
//! `sec_token` form field are all exactly as this crate already sends them.
//! The token is not a cookie either -- the jar holds 21 cookies for this
//! domain and none is named for a token or an XSRF value.
//!
//! So the only open question is which document still states it. This walks the
//! candidate pages with the real session and reports, per page, whether a
//! `sec_token` appears and in what shape. It prints the token's LENGTH and
//! surrounding key, never the value: the token authenticates writes against
//! the account.
//!
//! Run with a signed-in Chrome profile on the host:
//!     cargo run -p quota-core --example qwen-token-hunt

use quota_core::browser_cookies::chrome_cookies_for;

/// Pages that could plausibly still inline the token, in the order a browser
/// would reach them: the console root first, since a shell that bootstraps the
/// app is the likeliest place for a value every later call needs.
const CANDIDATES: &[&str] = &[
    "https://home.qwencloud.com/",
    "https://home.qwencloud.com/console",
    "https://home.qwencloud.com/billing/subscription/token-plan-individual",
    "https://home.qwencloud.com/next/index.htm",
    "https://www.qwencloud.com/",
];

/// Every spelling the console has used for this value, searched for as keys
/// rather than by matching the token's own shape -- a 21-character alphanumeric
/// run appears in minified bundles by the hundred, so shape alone finds noise.
const KEYS: &[&str] = &["SEC_TOKEN", "sec_token", "secToken", "csrfToken", "XSRF"];

#[tokio::main]
async fn main() {
    let jar = match chrome_cookies_for("qwencloud.com") {
        Ok(jar) => jar,
        Err(error) => {
            eprintln!("  no cookie jar: {error}");
            std::process::exit(2);
        }
    };
    let header = jar.header();
    if header.is_empty() {
        eprintln!("  cookie jar is empty; sign in to qwencloud.com in Chrome first");
        std::process::exit(2);
    }
    println!("  session: {} cookie(s)\n", header.matches(';').count() + 1);

    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36")
        .build()
        .expect("client");

    let mut found_anywhere = false;
    for url in CANDIDATES {
        let response = client
            .get(*url)
            .header("cookie", header.clone())
            .header("accept", "text/html,application/xhtml+xml")
            .send()
            .await;
        let Ok(response) = response else {
            println!("  {url}\n    request failed");
            continue;
        };
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        print!("  {url}\n    HTTP {status}, {} bytes", body.len());

        let mut hits = Vec::new();
        for key in KEYS {
            // Report the assignment's shape, never the value: this token
            // authenticates writes against the account.
            if let Some(at) = body.find(key) {
                let tail = &body[at..body.len().min(at + 96)];
                let len = tail.split(['"', '\'']).nth(2).map(str::len).unwrap_or(0);
                // The SPELLING is the whole question: our extractor matches a
                // literal `SEC_TOKEN: "`, so a bundle emitting `SEC_TOKEN:"` or
                // single quotes fails while the token is right there. Print the
                // punctuation between the key and the value, never the value.
                let punct: String = tail[key.len()..]
                    .chars()
                    .take_while(|c| !c.is_alphanumeric())
                    .collect();
                hits.push(format!("{key} followed by {punct:?} (value ~{len} chars)"));
            }
        }
        if hits.is_empty() {
            println!("  -- no token key present");
        } else {
            found_anywhere = true;
            println!("  -- FOUND: {}", hits.join(", "));
        }
    }

    if !found_anywhere {
        println!(
            "\n  No candidate page states the token. It is minted by the bundle at\n  \
             runtime, which a scrape cannot reach -- that is a finding, not a\n  \
             failed search, and it means this provider needs a different lane."
        );
        std::process::exit(1);
    }
}
