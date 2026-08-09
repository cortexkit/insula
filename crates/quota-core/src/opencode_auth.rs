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

/// Read one provider's credential entry from opencode's auth.json. Returns
/// `Ok(None)` when the file or the provider entry is absent (a normal "no
/// session" condition the caller folds into silent-degrade).
pub fn read_provider(provider: &str) -> Result<Option<OpencodeAuth>, String> {
    let Some(path) = auth_path() else {
        return Ok(None);
    };
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    parse_provider(&data, provider)
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
