//! Small helpers shared across providers.
//!
//! `first_env` resolves a credential from a priority-ordered list of env var
//! names (the `api-key-env` archetype: a provider accepts several env var
//! aliases, first non-empty wins). `epoch_to_iso8601` converts epoch seconds to
//! the RFC 3339 / ISO 8601 UTC string the consumer's `Date.parse` expects.

use chrono::{TimeZone, Utc};

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
        let path = std::path::Path::new("/nonexistent-home-dir/alice/.gemini/oauth_creds.json");
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
        assert!(published.contains("No such file"), "{published}");
    }
}
