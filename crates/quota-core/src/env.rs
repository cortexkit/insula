//! Small helpers shared across providers.
//!
//! `first_env` resolves a credential from a priority-ordered list of env var
//! names (the `api-key-env` archetype: a provider accepts several env var
//! aliases, first non-empty wins). `epoch_to_iso8601` converts epoch seconds to
//! the RFC 3339 / ISO 8601 UTC string the consumer's `Date.parse` expects.

use chrono::{TimeZone, Utc};
use std::path::PathBuf;

/// The current user's home directory.
///
/// Every provider that reads a credential from disk starts here, so this is the
/// one place the platform difference is expressed. Unix publishes `$HOME`;
/// Windows normally does not, and a bare `HOME` read there returns `None` for
/// every provider at once -- which this module reports as a host where nothing
/// is configured. That failure is silent and looks exactly like the truth on a
/// machine where the user has genuinely logged into nothing, so it is worth
/// resolving centrally rather than at nine call sites.
///
/// `HOME` is still consulted first on every platform: a Unix-shaped environment
/// is authoritative where it exists, and Windows shells that set it (MSYS, Git
/// Bash, WSL interop) mean what they say. `USERPROFILE` is the native Windows
/// answer, and `HOMEDRIVE` + `HOMEPATH` is the fallback for the domain-joined
/// case where a profile lives on a mapped drive.
///
/// Note this resolves OUR home directory, not where another tool keeps its
/// files. Several third-party CLIs this module reads from are Node programs
/// that build POSIX-shaped paths under the home directory on every platform, so
/// their locations are `home_dir().join(".config/...")` even on Windows --
/// following the host convention there instead would look correct and find
/// nothing.
pub fn home_dir() -> Option<PathBuf> {
    home_dir_from(non_empty)
}

/// The resolution order, over an arbitrary environment.
///
/// Split from [`home_dir`] so the Windows branches can be exercised anywhere.
/// Reading the process environment directly would leave them testable only on
/// the platform they exist for -- and a rule that cannot be tested where it is
/// written is one nobody checks until a user reports that nothing resolves.
fn home_dir_from(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(home) = lookup("HOME") {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = lookup("USERPROFILE") {
        return Some(PathBuf::from(profile));
    }
    // Both halves are required: a drive with no path, or a path with no drive,
    // does not name a directory, and joining one to a relative credential path
    // would produce a location that happens to resolve against the process's
    // current directory.
    match (lookup("HOMEDRIVE"), lookup("HOMEPATH")) {
        (Some(mut drive), Some(path)) => {
            drive.push(path);
            Some(PathBuf::from(drive))
        }
        _ => None,
    }
}

/// Read an environment variable, treating an empty value as absent.
///
/// An empty `HOME` is not a home directory, and letting it through would build
/// paths relative to the process's current directory rather than failing.
fn non_empty(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// Return the value of the first non-empty env var in `names`, trimmed.
pub fn first_env(names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = std::env::var_os(name) {
            let value = value.to_string_lossy().trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Convert epoch seconds to an RFC 3339 / ISO 8601 UTC string (`...Z`).
///
/// Returns `None` for an out-of-range timestamp so a provider drops the window
/// rather than emit a malformed `resetsAt`.
/// Read a local credential file, describing a failure by the file's NAME rather
/// than its path.
///
/// Credential files live under the account's home directory, and a read failure
/// becomes the `error` string of a degraded entry, which is published to other
/// processes. Interpolating the path would put the operating-system username in
/// that string.
///
/// Nothing is lost by omitting it: `std::io::Error` does not name the path it
/// failed on (a missing file reports only `No such file or directory (os error
/// 2)`), so the path in these messages was contributed entirely by the caller's
/// own formatting.
///
/// Callers pass a `description` instead of the path, which is why this exists as
/// a helper rather than as a rule to remember: the unsafe version cannot be
/// written through it.
pub fn read_credential_file(
    path: &std::path::Path,
    description: &str,
) -> Result<Vec<u8>, crate::provider::FetchError> {
    std::fs::read(path).map_err(|error| {
        let message = format!("reading {description}: {error}");
        // A missing file means nobody configured this provider here, which is a
        // permanent and correct state. Any other read failure -- no permission,
        // a directory where a file belongs, an I/O error -- means the file is
        // there and we cannot use it, which someone can act on. Reporting both
        // as absent files the second under "nothing to fix", where nobody looks.
        if error.kind() == std::io::ErrorKind::NotFound {
            crate::provider::FetchError::NoSession(message)
        } else {
            crate::provider::FetchError::CredentialUnusable(message)
        }
    })
}

/// Render an epoch second as an RFC 3339 string for the wire.
///
/// Uses an explicit format rather than `to_rfc3339`, and that is the reason it
/// is safe to key on: `to_rfc3339` chooses precision from the VALUE, so a whole
/// second prints with no fractional part, nanoseconds ending in three zeros
/// print six digits, and everything else prints nine. One instant then has
/// several valid spellings and string equality stops meaning instant equality.
///
/// An explicit format cannot vary that way. Anything formatting a timestamp for
/// the wire should use either this or `rfc3339_canonical`, never the default.
pub fn epoch_to_iso8601(epoch_secs: i64) -> Option<String> {
    match Utc.timestamp_opt(epoch_secs, 0) {
        chrono::LocalResult::Single(dt) => Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_env_picks_first_non_empty() {
        // Use unique names to avoid cross-test env pollution.
        std::env::set_var("QUOTA_TEST_A", "");
        std::env::set_var("QUOTA_TEST_B", "  bee  ");
        std::env::set_var("QUOTA_TEST_C", "see");
        assert_eq!(
            first_env(&["QUOTA_TEST_A", "QUOTA_TEST_B", "QUOTA_TEST_C"]).as_deref(),
            Some("bee")
        );
        std::env::remove_var("QUOTA_TEST_A");
        std::env::remove_var("QUOTA_TEST_B");
        std::env::remove_var("QUOTA_TEST_C");
    }

    #[test]
    fn first_env_none_when_all_absent() {
        assert_eq!(first_env(&["QUOTA_TEST_DEFINITELY_UNSET_XYZ"]), None);
    }

    /// A missing credential file and an unreadable one are different states,
    /// and only the second is worth anyone's attention. Both used to report as
    /// absent, which filed a real problem under "nothing to fix".
    #[test]
    fn an_unreadable_credential_file_is_not_reported_as_an_absent_one() {
        let dir = std::env::temp_dir().join(format!(
            "qta-credfile-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let absent = read_credential_file(&dir.join("nothing-here.json"), "test creds")
            .expect_err("a missing file must fail");
        assert_eq!(absent.error_class(), "credential_absent");

        // A directory where a file belongs: present on disk, unreadable as a
        // file. Any non-NotFound I/O error would do; this one is portable.
        let unusable = read_credential_file(&dir, "test creds")
            .expect_err("reading a directory as a file must fail");
        assert_eq!(unusable.error_class(), "credential_unusable");

        // Not vacuous: both still describe the failure, so this cannot pass by
        // returning an empty or identical message.
        assert!(absent.to_string().contains("test creds"));
        assert!(unusable.to_string().contains("test creds"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fake environment, so the Windows branches run on every platform.
    fn env_of<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, value)| *key == name && !value.is_empty())
                .map(|(_, value)| std::ffi::OsString::from(*value))
        }
    }

    /// `HOME` wins wherever it is set, including on Windows.
    ///
    /// Windows shells that publish it (MSYS, Git Bash, WSL interop) mean what
    /// they say, and several third-party CLIs this module reads from are Node
    /// programs that build POSIX-shaped paths under it on every platform. A
    /// resolver preferring the native variable would send those reads to a
    /// directory the tool never writes -- which resolves, finds nothing, and
    /// reports as an account that was never configured.
    #[test]
    fn home_wins_over_the_windows_variables() {
        let resolved = home_dir_from(env_of(&[
            ("HOME", "/Users/someone"),
            ("USERPROFILE", r"C:\Users\someone"),
            ("HOMEDRIVE", "C:"),
            ("HOMEPATH", r"\Users\someone"),
        ]));
        assert_eq!(resolved, Some(PathBuf::from("/Users/someone")));
    }

    /// The ordinary Windows case: no `HOME`, so `USERPROFILE` answers.
    ///
    /// Without this branch every credential path on Windows resolves to `None`
    /// at once, and the module reports a host where nothing is configured --
    /// indistinguishable from the truth on a machine nobody has logged in on.
    #[test]
    fn userprofile_answers_when_home_is_absent() {
        let resolved = home_dir_from(env_of(&[
            ("USERPROFILE", r"C:\Users\someone"),
            ("HOMEDRIVE", "C:"),
            ("HOMEPATH", r"\Users\other"),
        ]));
        assert_eq!(resolved, Some(PathBuf::from(r"C:\Users\someone")));
    }

    /// The domain-joined fallback, and it needs BOTH halves.
    ///
    /// A drive with no path, or a path with no drive, does not name a directory:
    /// joining a relative credential path to either produces a location that
    /// resolves against the process's current directory instead of failing.
    #[test]
    fn homedrive_and_homepath_answer_only_together() {
        assert_eq!(
            home_dir_from(env_of(&[
                ("HOMEDRIVE", "C:"),
                ("HOMEPATH", r"\Users\someone")
            ])),
            Some(PathBuf::from(r"C:\Users\someone"))
        );
        assert_eq!(home_dir_from(env_of(&[("HOMEDRIVE", "C:")])), None);
        assert_eq!(
            home_dir_from(env_of(&[("HOMEPATH", r"\Users\someone")])),
            None
        );
    }

    /// An empty value is not a home directory.
    ///
    /// A variable set to the empty string reads as present, and joining a
    /// relative credential path to it produces a path resolved against the
    /// process's working directory instead of a failure -- so a read would land
    /// wherever the process happened to start.
    #[test]
    fn an_empty_value_is_treated_as_absent() {
        let resolved = home_dir_from(env_of(&[
            ("HOME", ""),
            ("USERPROFILE", r"C:\Users\someone"),
        ]));
        assert_eq!(resolved, Some(PathBuf::from(r"C:\Users\someone")));

        assert_eq!(home_dir_from(env_of(&[("HOME", "")])), None);
    }

    #[test]
    fn epoch_converts_to_utc_iso8601() {
        // 1782135879 is the epoch-seconds value for 2026-06-22T13:44:39Z.
        assert_eq!(
            epoch_to_iso8601(1782135879).as_deref(),
            Some("2026-06-22T13:44:39Z")
        );
    }

    /// A failed credential read is published to consumers, so it must describe
    /// the file without naming where it lives: the path runs through the
    /// account's home directory.
    #[test]
    fn a_failed_credential_read_names_the_file_and_not_its_path() {
        let path = std::path::Path::new("nonexistent-home-dir/alice/.gemini/oauth_creds.json");
        let error = read_credential_file(path, "gemini oauth_creds.json")
            .expect_err("a path that cannot exist must fail to read");

        let published = error.to_string();
        assert!(
            !published.contains("alice") && !published.contains("nonexistent-home-dir"),
            "the path reached the wire: {published}"
        );
        // Not vacuous: the message must still identify the file and carry the
        // reason, so this cannot pass by returning something empty or generic.
        assert!(published.contains("gemini oauth_creds.json"), "{published}");

        // The reason is compared against what this platform actually says rather
        // than against a fixed string. Every operating system words a missing
        // file differently, so asserting one wording tests the host it was
        // written on and fails everywhere else -- while asserting nothing about
        // the reason would let an empty or generic message pass.
        let os_wording = std::fs::read_to_string(path)
            .expect_err("the same read must fail the same way")
            .to_string();
        assert!(published.contains(&os_wording), "{published}");
    }
}
