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

/// The `source` a provider publishes when its credential is a browser session
/// cookie.
///
/// `source` is observability only, and its job is to tell a reader how a figure
/// was obtained so they know what to do when it stops arriving. The cohort
/// previously published `api`, which names the wrong remedy: an API credential
/// is fixed by supplying a key, while these are fixed only by logging into the
/// site in Chrome on this machine. That distinction is the whole reason these
/// providers behave differently from the rest -- they cannot work headless, and
/// they break when a browser session expires rather than when a key is revoked.
///
/// It is also the one field that separates them once a fetch SUCCEEDS. A failure
/// says "no matching cookie for domain" in its error text, but a healthy entry
/// published `api` and was indistinguishable from a key-based provider.
pub const SOURCE_LABEL: &str = "cookie";

#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::Mutex;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

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

impl From<CookieError> for crate::provider::FetchError {
    /// Classify a cookie failure for the refresher.
    ///
    /// The distinction is whether retrying could succeed, because that decides
    /// what happens to a window already being served: a non-transient failure
    /// replaces it with a degraded entry, while a transient one keeps serving
    /// it. Absent store, absent cookie and an unsupported platform are all
    /// stable facts about this host -- nobody is logged in here, and the next
    /// attempt will say the same. A keychain that will not open or a store that
    /// will not decrypt is a condition of the moment: the keychain may be
    /// locked, or the browser may be mid-write on the file being copied.
    ///
    /// Kept here, beside the enum, so the arms are enumerated in one place:
    /// adding a variant fails to compile here rather than silently taking some
    /// caller's default.
    fn from(error: CookieError) -> Self {
        let detail = error.to_string();
        match error {
            CookieError::NoStore | CookieError::NoCookie | CookieError::Unsupported => {
                crate::provider::FetchError::NoSession(detail)
            }
            CookieError::NoKeychainKey(_) | CookieError::Extract(_) => {
                crate::provider::FetchError::Upstream(detail)
            }
        }
    }
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

/// Extract cookies without running keychain, filesystem, and SQLite work on an
/// async runtime worker.
pub async fn chrome_cookies_for_async(domain_suffix: &str) -> Result<CookieJar, CookieError> {
    let domain_suffix = domain_suffix.to_owned();
    tokio::task::spawn_blocking(move || chrome_cookies_for(&domain_suffix))
        .await
        .map_err(|error| CookieError::Extract(format!("extraction task failed: {error}")))?
}

/// Extract + decrypt all Chrome cookies whose `host_key` ends with `domain_suffix`
/// (e.g. `"ollama.com"` matches `ollama.com` and `signin.ollama.com`).
#[cfg(not(target_os = "macos"))]
pub fn chrome_cookies_for(_domain_suffix: &str) -> Result<CookieJar, CookieError> {
    Err(CookieError::Unsupported)
}

/// How long one copy of the cookie store may be reused across providers.
///
/// Nine providers in this cohort each need a cookie out of the SAME store, and
/// every fetch used to copy the whole database and re-read the keychain for
/// itself. Measured on a live machine that came to 0.55 GB of disk reads per
/// hour: roughly 7 MB per fetch to obtain a few hundred bytes of cookie.
///
/// The bound is taken from the refresher's own cadence and is load-bearing in
/// BOTH directions. It has to exceed the time one refresh tick takes to fan out
/// across the cohort, or providers within a single tick stop sharing and the
/// amplification returns. It has to stay BELOW the refresher's 60s base
/// interval, so every tick still reads the store at least once: a snapshot that
/// outlived a tick would let a provider stamp `fetchedAt` for a fetch whose
/// underlying data was read during an earlier one, overstating freshness on the
/// wire.
#[cfg(target_os = "macos")]
const SNAPSHOT_TTL: Duration = Duration::from_secs(45);

/// A copy of the cookie store, plus the key that decrypts what is inside it.
///
/// The key is held with the snapshot rather than separately because the two are
/// acquired together and are useless apart. Deriving it means another `security`
/// subprocess, which was also being paid once per provider per tick.
#[cfg(target_os = "macos")]
struct Snapshot {
    path: PathBuf,
    key: Vec<u8>,
    taken_at: Instant,
}

#[cfg(target_os = "macos")]
impl Drop for Snapshot {
    fn drop(&mut self) {
        // Best-effort: a leftover copy is a stale temp file, not a correctness
        // problem, and there is nothing useful to do if removal fails.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(target_os = "macos")]
static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Whether a snapshot taken at `taken_at` must be replaced before serving `now`.
///
/// Separated from the extraction path so the decision can be tested without a
/// cookie store, a keychain, or control over the clock.
#[cfg(target_os = "macos")]
fn snapshot_is_stale(taken_at: Option<Instant>, now: Instant, ttl: Duration) -> bool {
    match taken_at {
        // Nothing cached yet: the first caller after start always pays for one.
        None => true,
        // Saturating rather than subtracting: a clock that appears to move
        // backwards must not read as an arbitrarily FRESH snapshot, which would
        // pin one stale copy in place indefinitely.
        Some(taken_at) => now.saturating_duration_since(taken_at) >= ttl,
    }
}

/// Extract + decrypt all Chrome cookies whose `host_key` ends with `domain_suffix`.
#[cfg(target_os = "macos")]
pub fn chrome_cookies_for(domain_suffix: &str) -> Result<CookieJar, CookieError> {
    // The lock is held across the copy so that a cohort fanning out together
    // takes ONE snapshot rather than racing to take nine identical ones. These
    // callers already run on blocking threads, and the work serialised here is
    // the work being eliminated.
    let mut guard = SNAPSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if snapshot_is_stale(
        guard.as_ref().map(|s| s.taken_at),
        Instant::now(),
        SNAPSHOT_TTL,
    ) {
        // Dropped before the replacement is built so the old copy is removed
        // even if taking the new one fails.
        *guard = None;

        let store = locate_chrome_cookie_store()?.ok_or(CookieError::NoStore)?;
        let key = safe_storage_key()?;
        let path = copy_cookie_store(&store)?;
        *guard = Some(Snapshot {
            path,
            key,
            taken_at: Instant::now(),
        });
    }

    let snapshot = guard
        .as_ref()
        .expect("a snapshot was just taken or was already fresh");
    let key = snapshot.key.clone();
    let rows = read_encrypted_cookies(&snapshot.path, domain_suffix)?;
    drop(guard);

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
/// Find the most recently written Chrome cookie database.
///
/// `Ok(None)` means Chrome is not installed for this user, which is an ordinary
/// and permanent state. `Err` means the profile directory is there and could not
/// be listed -- a permission change, a stale mount, an I/O error -- which is a
/// different fact about the host and one somebody can act on.
///
/// The distinction reaches the wire: absence becomes `credential_absent`, which
/// tells a consumer no browser session exists here, while an unreadable store
/// is reported as a condition of the moment and keeps serving the last healthy
/// window rather than replacing it. Collapsing them would take the whole cookie
/// cohort dark and report it as nobody having logged in.
fn locate_chrome_cookie_store() -> Result<Option<PathBuf>, CookieError> {
    let Some(home) = crate::env::home_dir() else {
        return Ok(None);
    };
    locate_under(&home.join("Library/Application Support/Google/Chrome"))
}

/// Search and classify an arbitrary profile directory.
///
/// Separate from [`locate_chrome_cookie_store`], which resolves one fixed
/// location under the home directory, so a test can pass a deliberately
/// unlistable path instead of altering the real Chrome installation to reach
/// that branch.
fn locate_under(base: &std::path::Path) -> Result<Option<PathBuf>, CookieError> {
    // Chrome stores the cookie DB under each profile's "Network" dir (newer) or
    // directly in the profile dir (older). Prefer the most-recently-modified.
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(CookieError::Extract(format!(
                "listing the Chrome profile directory: {error}"
            )))
        }
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        candidates.push(p.join("Network/Cookies"));
        candidates.push(p.join("Cookies"));
    }
    Ok(candidates
        .into_iter()
        .filter(|p| p.is_file())
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((mtime, p))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, p)| p))
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

/// Copy the (possibly locked) cookie DB to a temp path.
///
/// Chrome keeps the database open, so this reads a consistent snapshot without
/// contending on its lock. The caller owns the returned path and is responsible
/// for removing it.
#[cfg(target_os = "macos")]
fn copy_cookie_store(store: &std::path::Path) -> Result<PathBuf, CookieError> {
    let tmp = std::env::temp_dir().join(format!(
        "quota-chrome-cookies-{}-{}.db",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::copy(store, &tmp).map_err(|e| CookieError::Extract(format!("copy store: {e}")))?;
    Ok(tmp)
}

/// Read the encrypted cookies whose host_key ends with `domain_suffix` from a
/// snapshot taken by [`copy_cookie_store`].
#[cfg(target_os = "macos")]
fn read_encrypted_cookies(
    snapshot: &std::path::Path,
    domain_suffix: &str,
) -> Result<Vec<(String, String, Vec<u8>)>, CookieError> {
    let result = (|| {
        let conn = rusqlite::Connection::open_with_flags(
            snapshot,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| CookieError::Extract(format!("open store: {e}")))?;
        // Deliberately unfiltered on expiry. An expired cookie is sent, the
        // upstream rejects it, and that arrives as a rejected credential --
        // which is the honest reading: the login is no longer usable.
        //
        // Filtering here would be easy to get wrong in the expensive direction.
        // `expires_utc` is 0 for a SESSION cookie, which is valid until the
        // browser closes, so the obvious `expires_utc < now` predicate discards
        // live logins and reports a working provider as never configured. The
        // gain would be turning one rejected fetch into a slightly different
        // label; the loss is a provider that silently stops reporting.
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
#[cfg(any(target_os = "macos", test))]
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

    /// A Chrome directory that cannot be listed must not read as Chrome absent.
    ///
    /// The two states reach consumers differently: absence is a permanent fact
    /// about the host and reports the account as never configured, while an
    /// unreadable store is a condition of the moment and keeps the last healthy
    /// window being served. Collapsing them takes all nine cookie-backed
    /// providers dark at once and reports it as nobody having logged in.
    ///
    /// The unreadable directory is one whose contents cannot be listed. A
    /// permission bit would not stop root, so on a root run the fixture would
    /// quietly exercise the absent case while appearing to exercise this one.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unlistable_profile_directory_is_not_reported_as_no_chrome() {
        let root = std::env::temp_dir().join(format!("insula-cookies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Absent: the ordinary state where Chrome was never installed.
        let missing = root.join("no-such-chrome");
        assert!(
            matches!(locate_under(&missing), Ok(None)),
            "a missing profile directory must read as no Chrome installed"
        );

        // Present and unlistable: a file where the directory belongs, which is
        // unreadable-as-a-directory for every user including root.
        let occupied = root.join("occupied");
        std::fs::write(&occupied, b"not a directory").unwrap();
        let error = locate_under(&occupied)
            .expect_err("an unlistable profile directory must not report as absent");
        assert!(
            matches!(error, CookieError::Extract(_)),
            "expected Extract, got {error:?}"
        );

        // And an empty but readable directory is a real absence: Chrome is
        // installed and holds no profile with a cookie store.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(locate_under(&empty), Ok(None)));

        std::fs::remove_dir_all(&root).ok();
    }

    use super::*;
    use crate::provider::FetchError;
    use crate::refresh::{classify, FetchClass};

    /// Every cookie failure is classified, and by whether retrying could help.
    ///
    /// The classification decides the fate of a window already being served: a
    /// non-transient failure replaces it with a degraded entry, a transient one
    /// keeps serving it. So calling a locked keychain permanent would discard a
    /// healthy window over a momentary condition, and calling an absent login
    /// transient would keep serving usage for a session that no longer exists.
    ///
    /// Matched exhaustively rather than sampled: a variant added later must be
    /// judged here, not fall into whichever arm happens to catch it.
    #[test]
    fn every_cookie_failure_is_classified_by_whether_a_retry_could_help() {
        let cases = [
            // Stable facts about this host: nobody is logged in here, and the
            // next attempt reports the same.
            (CookieError::NoStore, FetchClass::NonTransient),
            (CookieError::NoCookie, FetchClass::NonTransient),
            (CookieError::Unsupported, FetchClass::NonTransient),
            // Conditions of the moment: the keychain may be locked, or the
            // browser may be mid-write on the file being copied.
            (
                CookieError::NoKeychainKey("locked".into()),
                FetchClass::Transient,
            ),
            (
                CookieError::Extract("database is locked".into()),
                FetchClass::Transient,
            ),
        ];

        for (error, expected) in cases {
            let label = format!("{error:?}");
            let converted = FetchError::from(error);
            assert_eq!(classify(&converted), expected, "{label} -> {converted:?}");
            // Not vacuous: the failure's own text survives the conversion, so a
            // reader can tell which condition occurred.
            assert!(!converted.to_string().is_empty(), "{label}");
        }
    }

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

    /// The reuse bound has to hold in both directions, so both are asserted with
    /// violating inputs rather than only the expiry side.
    ///
    /// Gated with the code it covers: the snapshot cache exists only where a
    /// cookie store can be read, so off that platform these names do not resolve
    /// and the crate fails to compile for its own test target.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_snapshot_is_reused_until_the_bound_and_replaced_after_it() {
        let ttl = Duration::from_secs(45);
        let taken = Instant::now();

        // Within the bound: the whole point of the cache. If this ever reports
        // stale, every provider in a tick copies the store again.
        assert!(!snapshot_is_stale(
            Some(taken),
            taken + Duration::from_secs(44),
            ttl
        ));

        // At and past the bound: a snapshot must not outlive it, or a provider
        // stamps a fresh fetch time onto data read during an earlier tick.
        assert!(snapshot_is_stale(Some(taken), taken + ttl, ttl));
        assert!(snapshot_is_stale(
            Some(taken),
            taken + Duration::from_secs(120),
            ttl
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_missing_snapshot_is_stale_so_the_first_caller_takes_one() {
        assert!(snapshot_is_stale(None, Instant::now(), SNAPSHOT_TTL));
    }

    /// `Instant` is monotonic per the standard library, but the arithmetic here
    /// still has to be saturating: a plain subtraction of a later instant would
    /// panic in debug and, if it silently wrapped, would produce a gigantic
    /// elapsed time -- reading as PERMANENTLY fresh and pinning one stale copy
    /// forever. That failure is silent and unbounded, so it is fenced.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_apparently_backwards_clock_does_not_pin_a_stale_snapshot() {
        let now = Instant::now();
        let taken_in_the_future = now + Duration::from_secs(600);
        assert!(!snapshot_is_stale(
            Some(taken_in_the_future),
            now,
            SNAPSHOT_TTL
        ));
    }

    /// The bound is only correct relative to the refresher's cadence: below the
    /// 60s base interval so every tick reads the store at least once, and far
    /// enough above zero to actually be shared within one tick.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_reuse_bound_stays_inside_the_refresher_base_interval() {
        assert!(
            SNAPSHOT_TTL < crate::refresh::BASE_INTERVAL,
            "a snapshot outliving a tick would overstate fetchedAt freshness"
        );
        assert!(SNAPSHOT_TTL >= Duration::from_secs(10));
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
