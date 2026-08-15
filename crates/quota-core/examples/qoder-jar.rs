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
    match browser_cookies::chrome_cookies_for("qoder.com") {
        Ok(jar) => {
            let mut names: Vec<&str> = jar.cookies.iter().map(|c| c.name.as_str()).collect();
            names.sort_unstable();
            println!("  {} cookie(s) for qoder.com:", names.len());
            for name in names {
                println!("    {name}");
            }
        }
        Err(error) => {
            eprintln!("cookie store unreadable ({error}): the question is unanswered.");
            std::process::exit(2);
        }
    }
}
