//! Read a session cookie for a live probe from a file named by `--cookie-file`.
//!
//! The cookie is a live login, so it never travels on the command line or in
//! the environment: both show up in `ps` output and shell history. Only the
//! file's PATH is an argument. Capture the cookie with Cerebellum (or copy the
//! `Cookie:` request header from a signed-in browser) into a file readable only
//! by you, e.g. `umask 077; pbpaste > /tmp/ollama.cookie`.

use quota_core::cookie_jar::CookieJar;

/// The `Cookie:` header in the file named by `--cookie-file <path>`.
///
/// Exits with status 2 (the probes' "question unanswered" code) when the flag
/// is missing or the file is unreadable or empty, because a probe run without
/// a cookie says nothing about the page it was meant to look at.
pub fn cookie_header_from_args(probe: &str) -> String {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    while let Some(arg) = args.next() {
        if arg == "--cookie-file" {
            path = args.next();
        } else if let Some(value) = arg.strip_prefix("--cookie-file=") {
            path = Some(value.to_string());
        }
    }
    let Some(path) = path else {
        eprintln!("usage: cargo run -p quota-core --example {probe} -- --cookie-file <path>");
        eprintln!("  <path> holds the site's `Cookie:` request header; the cookie itself");
        eprintln!("  is never taken from argv or the environment");
        std::process::exit(2);
    };
    warn_if_shared(&path);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("  cannot check: the cookie file is unreadable ({error})");
            std::process::exit(2);
        }
    };
    // Tolerate a pasted header line, `Cookie: a=1; b=2`, as well as the bare value.
    let trimmed = text.trim();
    let header = trimmed
        .strip_prefix("Cookie:")
        .or_else(|| trimmed.strip_prefix("cookie:"))
        .unwrap_or(trimmed)
        .trim();
    if CookieJar::from_header(header).cookies.is_empty() {
        eprintln!("  cannot check: the cookie file holds no `name=value` pair");
        std::process::exit(2);
    }
    header.to_string()
}

/// Say so when other users can read the file, since it holds a live login.
fn warn_if_shared(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.permissions().mode() & 0o077 != 0 {
                eprintln!("  warning: {path} is readable by other users; chmod 600 it");
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}
