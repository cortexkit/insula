//! Which cookie NAMES the Qoder jar holds, to tell a tracking-only jar from a session.
//!
//! `cookieLoginsStale` names qoder on this host, which claims a browser login has
//! expired. The 401 may instead mean nobody ever logged in and the jar holds only
//! the tracking cookies any visit sets -- a different fact, with a different
//! remedy, reported as the wrong one.
//!
//! Prints names only, never values.

use quota_core::browser_cookies;

fn main() {
    let domains = [
        "qoder.com",
        "ollama.com",
        "factory.ai",
        "xiaomimimo.com",
        "opencode.ai",
        "cursor.com",
        "ampcode.com",
        "qwencloud.com",
    ];
    for domain in domains {
        match browser_cookies::chrome_cookies_for(domain) {
            Ok(jar) => {
                let mut names: Vec<&str> = jar.cookies.iter().map(|c| c.name.as_str()).collect();
                names.sort_unstable();
                println!("  {domain}: {} cookie(s)", names.len());
                for name in names {
                    println!("      {name}");
                }
            }
            // A domain with no cookie is an ordinary state on any host, so it is
            // reported and the sweep continues. Exiting here made the whole run
            // describe whichever domain happened to sort first.
            Err(error) => println!("  {domain}: none ({error})"),
        }
    }
}
