//! Reader for opencode's unified auth store (`~/.local/share/opencode/auth.json`).
//!
//! Unlike CodexBar — which reaches each provider's own native credential store
//! (macOS Keychain for Claude, bespoke CLI files, browser cookies) — this module
//! runs inside the CortexKit/opencode ecosystem, where opencode already holds
//! OAuth tokens and API keys for many providers in ONE cross-platform JSON file.
//! Preferring it (where it carries the provider) avoids the macOS-only Keychain
//! path and collapses several providers into a single session archetype.
//!
//! The file shape is `{ "<provider>": { "type": "oauth"|"api", ... } }`. For an
//! `oauth` entry we read `access` (+ `refresh`/`expires` for future refresh); for
//! an `api` entry we read `key`.

use std::path::PathBuf;

use serde::Deserialize;

use crate::provider::FetchError;

/// One provider's credential entry in opencode's auth.json.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OpencodeAuth {
    /// OAuth credential: a bearer `access` token, refreshable via `refresh`.
    Oauth {
        access: String,
        #[serde(default)]
        refresh: Option<String>,
        /// Expiry in epoch milliseconds, when present.
        #[serde(default)]
        expires: Option<i64>,
    },
    /// A static API key.
    Api { key: String },
}

impl OpencodeAuth {
    /// True when an OAuth token's `expires` is in the past relative to `now_ms`.
    /// Always false for API keys (no expiry) or when no expiry is recorded.
    ///
    /// **This is for attributing a failure, not for skipping a fetch.** The
    /// field records what the issuer said at grant time, and a token can outlive
    /// it -- so refusing to try would break a lane that is still working, which
    /// costs more than one wasted request. Attempt the fetch, and consult this
    /// only when the response is too ambiguous to attribute on its own.
    ///
    /// That case is real rather than hypothetical: at least one upstream answers
    /// an expired credential with an empty HTTP 200, byte-identical to what it
    /// returns during an edge flap. An empty body is classified transient so a
    /// flap does not discard a healthy window, so without this check a dead
    /// credential retries forever and never reaches a verdict -- and nothing
    /// ever tells the operator to sign in again.
    pub fn is_expired(&self, now_ms: i64) -> bool {
        match self {
            Self::Oauth {
                expires: Some(exp), ..
            } => *exp <= now_ms,
            _ => false,
        }
    }
}

/// Milliseconds since the Unix epoch, or 0 if the clock is before it.
///
/// Taken as an argument by [`OpencodeAuth::is_expired`] rather than read inside
/// it, so tests can pin a time; this is the caller-side default.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Resolve the opencode auth.json path (`$XDG_DATA_HOME` or `~/.local/share`).
pub fn auth_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("opencode/auth.json"));
    }
    crate::env::home_dir().map(|home| home.join(".local/share/opencode/auth.json"))
}

/// Read one provider's credential entry from opencode's auth.json.
///
/// `Ok(None)` means nothing is configured: no auth file, or no entry for this
/// provider. `Err` means the file is there and could not be used -- no
/// permission, a directory in its place, an I/O error, or contents that will not
/// parse.
///
/// The error type is [`FetchError`] rather than a string because the callers
/// were each choosing a class for it, and both chose `NoSession` -- so an
/// unreadable auth file reached the wire as `credential_absent`, the class that
/// tells a consumer the account was never configured and lets an operator
/// surface leave it out of the count it watches. Classifying here means a new
/// caller cannot make that choice again.
pub fn read_provider(provider: &str) -> Result<Option<OpencodeAuth>, FetchError> {
    let Some(path) = auth_path() else {
        return Ok(None);
    };
    read_provider_at(&path, provider)
}

/// The read and its classification, over an arbitrary path.
///
/// Separate from [`read_provider`], which resolves one fixed location, so a test
/// can point at a deliberately unreadable file instead of altering the real auth
/// store to reach that branch.
fn read_provider_at(
    path: &std::path::Path,
    provider: &str,
) -> Result<Option<OpencodeAuth>, FetchError> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // The path is deliberately not interpolated: this error string reaches
        // the wire, and it would carry the operating-system username.
        Err(e) => {
            return Err(FetchError::CredentialUnusable(format!(
                "reading the opencode auth store: {e}"
            )))
        }
    };
    parse_provider(&data, provider).map_err(FetchError::CredentialUnusable)
}

/// Parse one provider's entry from raw auth.json bytes. Pure — unit-testable.
pub fn parse_provider(data: &[u8], provider: &str) -> Result<Option<OpencodeAuth>, String> {
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(data)
        .map_err(|e| format!("opencode auth.json not valid JSON: {e}"))?;
    let Some(entry) = map.get(provider) else {
        return Ok(None);
    };
    let auth: OpencodeAuth = serde_json::from_value(entry.clone())
        .map_err(|e| format!("opencode auth.json '{provider}' entry not decodable: {e}"))?;
    Ok(Some(auth))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_oauth_entry() {
        let raw = br#"{ "anthropic": { "type": "oauth", "access": "a-tok", "refresh": "r-tok", "expires": 1782000000000 } }"#;
        let auth = parse_provider(raw, "anthropic").unwrap().unwrap();
        match auth {
            OpencodeAuth::Oauth {
                access,
                refresh,
                expires,
            } => {
                assert_eq!(access, "a-tok");
                assert_eq!(refresh.as_deref(), Some("r-tok"));
                assert_eq!(expires, Some(1782000000000));
            }
            _ => panic!("expected oauth"),
        }
    }

    #[test]
    fn reads_api_entry() {
        let raw = br#"{ "openai": { "type": "api", "key": "sk-xyz" } }"#;
        let auth = parse_provider(raw, "openai").unwrap().unwrap();
        assert!(matches!(auth, OpencodeAuth::Api { key } if key == "sk-xyz"));
    }

    /// An auth store that cannot be read must not report as one that is absent.
    ///
    /// Both reach a caller as "no credential", but they mean opposite things to
    /// whoever is looking at the output: an absent store is the ordinary state
    /// on a host that never configured these providers, while an unreadable one
    /// is a host somebody must fix. The classes they map to are treated
    /// differently all the way out to the health buckets, where absence is
    /// deliberately excluded from the count an operator watches.
    #[test]
    fn an_unreadable_auth_store_is_not_reported_as_an_absent_one() {
        let dir = std::env::temp_dir().join(format!("insula-oc-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Absent: no auth file at all.
        let missing = dir.join("no-such-auth.json");
        assert!(matches!(read_provider_at(&missing, "anthropic"), Ok(None)));

        // Present and unreadable. A directory where the file belongs, rather
        // than a permission bit, because a mode of 000 does not stop root -- so
        // on a root run a permission fixture would quietly exercise the absent
        // case while appearing to exercise this one.
        let occupied = dir.join("occupied.json");
        std::fs::create_dir_all(&occupied).unwrap();
        let error = read_provider_at(&occupied, "anthropic")
            .expect_err("an unreadable auth store must not report as absent");
        assert!(
            matches!(error, FetchError::CredentialUnusable(_)),
            "expected CredentialUnusable, got {error:?}"
        );

        // Unparseable contents are equally a host to fix, not a host with
        // nothing configured.
        let malformed = dir.join("malformed.json");
        std::fs::write(&malformed, b"{ not json").unwrap();
        let error = read_provider_at(&malformed, "anthropic")
            .expect_err("unparseable contents must not report as absent");
        assert!(matches!(error, FetchError::CredentialUnusable(_)));

        // And a well-formed store simply lacking this provider is a real
        // absence: the file belongs to a tool that holds many providers and this
        // one was never signed into.
        let without = dir.join("without.json");
        std::fs::write(&without, br#"{"openai":{"type":"api","key":"k"}}"#).unwrap();
        assert!(matches!(read_provider_at(&without, "anthropic"), Ok(None)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absent_provider_is_none() {
        let raw = br#"{ "openai": { "type": "api", "key": "sk" } }"#;
        assert!(parse_provider(raw, "anthropic").unwrap().is_none());
    }

    #[test]
    fn expiry_check() {
        let auth = OpencodeAuth::Oauth {
            access: "a".into(),
            refresh: None,
            expires: Some(1000),
        };
        assert!(auth.is_expired(2000));
        assert!(!auth.is_expired(500));
        assert!(!OpencodeAuth::Api { key: "k".into() }.is_expired(i64::MAX));
    }
}
