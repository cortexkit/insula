//! Browser-cookie extraction (macOS Chrome) — the shared layer for the cookie
//! cohort (ollama + cursor/factory/mimo/opencode/opencodego/amp).
//!
//! Some providers expose usage ONLY through their website, authenticated by a
//! session cookie the user's browser holds — there is no headless token API. To
//! read that usage we replicate what CodexBar does: pull the session cookie from
//! the local Chrome cookie store, decrypt it, and send it as a `Cookie:` header.
//!
//! macOS Chrome stores cookies in a SQLite DB with each value encrypted. The
//! scheme (verified against the real store): the value is `v10` + AES-128-CBC
//! ciphertext; the key is `PBKDF2-HMAC-SHA1(safe_storage_password, "saltysalt",
//! 1003, 16)` where `safe_storage_password` is the "Chrome Safe Storage" generic
//! password in the login keychain; the IV is 16 spaces. Newer Chrome additionally
//! prepends a 32-byte `SHA256(host_key)` domain hash to the PLAINTEXT (an integrity
//! binding), which we strip when present.
//!
//! DESKTOP-COUPLED BY NATURE: this needs a local browser store + the OS keychain,
//! so it only works on a desktop where the user logged in via Chrome. On a headless
//! server/container there is no store and no keychain, so callers degrade to
//! "unavailable" — never to a wrong value. This is the same constraint CodexBar has.
//!
//! Scope: macOS + Chrome only for now (the deployment target). Other Chromium
//! browsers share the scheme but differ in path/keychain-service; add them when a
//! provider needs them. Non-macOS returns `Unsupported` (callers degrade).

#[cfg(target_os = "macos")]
use std::path::PathBuf;

/// Why cookie extraction could not produce a session cookie. All variants are
/// "no usable cookie" outcomes the caller folds into a degraded entry.
#[derive(Debug)]
pub enum CookieError {
    /// No Chrome cookie store on disk (Chrome not installed / never run).
    NoStore,
    /// The OS keychain key ("Chrome Safe Storage") could not be read.
    NoKeychainKey(String),
    /// No cookie matching the requested domain in the store.
    NoCookie,
    /// Reading the store or decrypting failed.
    Extract(String),
    /// This platform is not supported (not macOS).
    Unsupported,
}

impl std::fmt::Display for CookieError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStore => write!(f, "no Chrome cookie store found"),
            Self::NoKeychainKey(m) => {
                write!(f, "Chrome Safe Storage keychain key unavailable: {m}")
            }
            Self::NoCookie => write!(f, "no matching cookie for domain"),
            Self::Extract(m) => write!(f, "cookie extraction failed: {m}"),
            Self::Unsupported => write!(f, "browser-cookie extraction is macOS+Chrome only"),
        }
    }
}

impl std::error::Error for CookieError {}

/// One decrypted cookie.
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub host_key: String,
}

/// Decrypted cookies for a domain, plus a `Cookie:` header built from them.
pub struct CookieJar {
    pub cookies: Vec<Cookie>,
}

impl CookieJar {
    /// `name=value; name=value` header from all cookies.
    pub fn header(&self) -> String {
        self.cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// True if any decrypted cookie name matches `predicate` — used to confirm a
    /// real SESSION cookie is present (not just incidental analytics cookies)
    /// before treating the jar as a usable login.
    pub fn has_cookie_named(&self, predicate: impl Fn(&str) -> bool) -> bool {
        self.cookies.iter().any(|c| predicate(&c.name))
    }
}

/// Extract + decrypt all Chrome cookies whose `host_key` ends with `domain_suffix`
/// (e.g. `"ollama.com"` matches `ollama.com` and `signin.ollama.com`).
#[cfg(not(target_os = "macos"))]
pub fn chrome_cookies_for(_domain_suffix: &str) -> Result<CookieJar, CookieError> {
    Err(CookieError::Unsupported)
}

/// Extract + decrypt all Chrome cookies whose `host_key` ends with `domain_suffix`.
#[cfg(target_os = "macos")]
pub fn chrome_cookies_for(domain_suffix: &str) -> Result<CookieJar, CookieError> {
    let store = locate_chrome_cookie_store().ok_or(CookieError::NoStore)?;
    let key = safe_storage_key()?;
    let rows = read_encrypted_cookies(&store, domain_suffix)?;
    let mut cookies = Vec::new();
    for (host_key, name, encrypted) in rows {
        // A cookie we can't decrypt is skipped, not fatal — a partial jar that
        // still carries the session cookie is usable.
        if let Some(value) = decrypt_value(&encrypted, &host_key, &key) {
            cookies.push(Cookie {
                name,
                value,
                host_key,
            });
        }
    }
    if cookies.is_empty() {
        return Err(CookieError::NoCookie);
    }
    Ok(CookieJar { cookies })
}

/// Candidate Chrome cookie-store paths (Default + numbered profiles).
#[cfg(target_os = "macos")]
fn locate_chrome_cookie_store() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let base = home.join("Library/Application Support/Google/Chrome");
    // Chrome stores the cookie DB under each profile's "Network" dir (newer) or
    // directly in the profile dir (older). Prefer the most-recently-modified.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            candidates.push(p.join("Network/Cookies"));
            candidates.push(p.join("Cookies"));
        }
    }
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((mtime, p))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, p)| p)
}

/// Read the "Chrome Safe Storage" generic password from the login keychain via the
/// `security` CLI (zero-dependency, and the path proven against the real keychain).
#[cfg(target_os = "macos")]
fn safe_storage_key() -> Result<Vec<u8>, CookieError> {
    use hmac::Hmac;
    use sha1::Sha1;

    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Chrome Safe Storage",
            "-a",
            "Chrome",
            "-w",
        ])
        .output()
        .map_err(|e| CookieError::NoKeychainKey(e.to_string()))?;
    if !output.status.success() {
        return Err(CookieError::NoKeychainKey(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if password.is_empty() {
        return Err(CookieError::NoKeychainKey("empty password".to_string()));
    }

    // PBKDF2-HMAC-SHA1(password, "saltysalt", 1003) -> 16-byte AES key.
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2::<Hmac<Sha1>>(password.as_bytes(), b"saltysalt", 1003, &mut key)
        .map_err(|e| CookieError::Extract(format!("pbkdf2: {e}")))?;
    Ok(key.to_vec())
}

/// Copy the (possibly locked) cookie DB to a temp path and read the encrypted
/// cookies whose host_key ends with `domain_suffix`.
#[cfg(target_os = "macos")]
fn read_encrypted_cookies(
    store: &std::path::Path,
    domain_suffix: &str,
) -> Result<Vec<(String, String, Vec<u8>)>, CookieError> {
    // Chrome keeps the DB open; copy it so we read a consistent snapshot without
    // contending on its lock.
    let tmp = std::env::temp_dir().join(format!(
        "quota-chrome-cookies-{}-{}.db",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::copy(store, &tmp).map_err(|e| CookieError::Extract(format!("copy store: {e}")))?;

    let result = (|| {
        let conn =
            rusqlite::Connection::open_with_flags(&tmp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| CookieError::Extract(format!("open store: {e}")))?;
        let mut stmt = conn
            .prepare("SELECT host_key, name, encrypted_value FROM cookies WHERE host_key LIKE ?1")
            .map_err(|e| CookieError::Extract(format!("prepare: {e}")))?;
        let like = format!("%{domain_suffix}");
        let rows = stmt
            .query_map([like], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(|e| CookieError::Extract(format!("query: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| CookieError::Extract(format!("rows: {e}")))?;
        Ok::<_, CookieError>(rows)
    })();

    let _ = std::fs::remove_file(&tmp);
    result
}

/// Decrypt one `v10` cookie value. Returns None if it isn't a `v10` blob or fails
/// to decrypt (caller skips it). Pure given the key — unit-testable.
#[cfg(target_os = "macos")]
fn decrypt_value(encrypted: &[u8], host_key: &str, key: &[u8]) -> Option<String> {
    decrypt_v10(encrypted, host_key, key)
}

/// The `v10` decryption, factored out so tests can exercise it with a synthetic
/// blob encrypted under a known key (no real keychain needed).
fn decrypt_v10(encrypted: &[u8], host_key: &str, key: &[u8]) -> Option<String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    use sha2::{Digest, Sha256};

    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    let ciphertext = encrypted.strip_prefix(b"v10")?;
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return None;
    }
    let iv = [b' '; 16];
    let decryptor = Aes128CbcDec::new(key.into(), &iv.into());
    let mut buf = ciphertext.to_vec();
    let plaintext = decryptor.decrypt_padded_mut::<Pkcs7>(&mut buf).ok()?;

    // Newer Chrome prepends SHA256(host_key) to the plaintext; strip it when the
    // leading 32 bytes match (older cookies without it pass through unchanged).
    let body = if plaintext.len() >= 32 {
        let expected = Sha256::digest(host_key.as_bytes());
        if plaintext[..32] == expected[..] {
            &plaintext[32..]
        } else {
            plaintext
        }
    } else {
        plaintext
    };

    Some(String::from_utf8_lossy(body).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    use sha2::{Digest, Sha256};

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    /// Encrypt a value the way Chrome does, to test the decrypt path with a known
    /// key. `with_prefix` prepends the SHA256(host_key) domain hash (newer Chrome).
    fn encrypt_v10(value: &str, host_key: &str, key: &[u8], with_prefix: bool) -> Vec<u8> {
        let mut plaintext = Vec::new();
        if with_prefix {
            plaintext.extend_from_slice(&Sha256::digest(host_key.as_bytes()));
        }
        plaintext.extend_from_slice(value.as_bytes());
        let iv = [b' '; 16];
        let ct =
            Aes128CbcEnc::new(key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
        let mut out = b"v10".to_vec();
        out.extend_from_slice(&ct);
        out
    }

    #[test]
    fn decrypts_newer_chrome_value_with_domain_prefix() {
        let key = [7u8; 16];
        let enc = encrypt_v10("session-abc-123", "ollama.com", &key, true);
        assert_eq!(
            decrypt_v10(&enc, "ollama.com", &key).as_deref(),
            Some("session-abc-123")
        );
    }

    #[test]
    fn decrypts_older_chrome_value_without_prefix() {
        let key = [7u8; 16];
        let enc = encrypt_v10("plain-value", "ollama.com", &key, false);
        assert_eq!(
            decrypt_v10(&enc, "ollama.com", &key).as_deref(),
            Some("plain-value")
        );
    }

    #[test]
    fn wrong_host_key_does_not_strip_a_real_prefix() {
        // A value that genuinely starts with 32 bytes that are NOT SHA256(host) is
        // left intact (we only strip on an exact domain-hash match).
        let key = [7u8; 16];
        let enc = encrypt_v10("no-prefix-here", "ollama.com", &key, false);
        // Decrypt claiming a different host: the leading bytes won't match
        // SHA256("other.com"), so nothing is stripped.
        assert_eq!(
            decrypt_v10(&enc, "other.com", &key).as_deref(),
            Some("no-prefix-here")
        );
    }

    #[test]
    fn non_v10_blob_returns_none() {
        let key = [7u8; 16];
        assert!(decrypt_v10(b"v11garbage", "ollama.com", &key).is_none());
        assert!(decrypt_v10(b"", "ollama.com", &key).is_none());
    }

    #[test]
    fn jar_header_and_session_detection() {
        let jar = CookieJar {
            cookies: vec![
                Cookie {
                    name: "aid".into(),
                    value: "1".into(),
                    host_key: "ollama.com".into(),
                },
                Cookie {
                    name: "__Secure-session".into(),
                    value: "tok".into(),
                    host_key: "ollama.com".into(),
                },
            ],
        };
        assert_eq!(jar.header(), "aid=1; __Secure-session=tok");
        assert!(jar.has_cookie_named(|n| n.contains("session")));
        assert!(!jar.has_cookie_named(|n| n == "missing"));
    }
}
