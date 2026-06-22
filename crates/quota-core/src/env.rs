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

    #[test]
    fn epoch_converts_to_utc_iso8601() {
        // 1782135879 is the epoch-seconds value for 2026-06-22T13:44:39Z.
        assert_eq!(
            epoch_to_iso8601(1782135879).as_deref(),
            Some("2026-06-22T13:44:39Z")
        );
    }
}
