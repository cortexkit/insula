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

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::path::PathBuf;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::Mutex;
#[cfg(any(target_os = "macos", target_os = "linux"))]
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
    /// Cookies for the domain exist, but every one is encrypted with a scheme
    /// this cannot read. Carries the scheme's own prefix.
    ///
    /// Distinct from [`Self::NoCookie`] because the two are opposite facts about
    /// the host: no cookie means nobody logged in, while this means somebody
    /// did and the values are sealed. Chrome's App-Bound Encryption (`v20`)
    /// holds its key in a service that validates the calling executable, so
    /// this is a permanent and correct outcome on such a profile rather than a
    /// fault -- but reporting it as an absent session would send someone to log
    /// in again, which cannot help.
    UnreadableScheme(&'static str),
    /// Reading the store or decrypting failed.
    Extract(String),
    /// This platform is not supported (not macOS).
    Unsupported,
}

/// Which encryption scheme a stored cookie value uses.
///
/// Chromium tags every encrypted value with a version prefix and the tag decides
/// the key source and cipher. Dispatch is per VALUE rather than per platform,
/// because one profile can legitimately hold several: a machine that gains a
/// keyring re-encrypts new cookies while old ones keep their old prefix.
///
/// Gated because the only caller is the macOS extraction path; on another
/// platform this would be dead code and the build denies warnings. The gate
/// includes `test` so the classification stays exercised on any host -- it is
/// pure, and it is the part a Linux port will reuse unchanged.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    /// AES-128-CBC. On macOS the key comes from the keychain; on Linux, from a
    /// fixed constant password when no keyring is available.
    V10,
    /// As `v10`, but the password comes from the Secret Service or KWallet.
    V11,
    /// The xdg Secret Portal scheme, AES-256-GCM. Recognised, not read.
    V12,
    /// Chrome's App-Bound Encryption. The key lives in an elevation service
    /// that validates the calling executable, so no other process can obtain
    /// it. Recognised, not read.
    V20,
    /// No recognised prefix. Either an unencrypted value from a very old
    /// profile or a scheme newer than this code.
    Unknown,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl Scheme {
    /// Classify by the value's own prefix.
    pub(crate) fn of(value: &[u8]) -> Self {
        if value.starts_with(b"v10") {
            Self::V10
        } else if value.starts_with(b"v11") {
            Self::V11
        } else if value.starts_with(b"v12") {
            Self::V12
        } else if value.starts_with(b"v20") {
            Self::V20
        } else {
            Self::Unknown
        }
    }

    /// The prefix, for reporting which scheme was refused.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::V10 => "v10",
            Self::V11 => "v11",
            Self::V12 => "v12",
            Self::V20 => "v20",
            Self::Unknown => "unrecognised",
        }
    }
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
            // Not NoSession: a session exists and is sealed. That class routes
            // the provider into the health bucket meaning "nothing configured
            // here", which would hide a host where somebody did log in and this
            // simply cannot read it. Not transient either -- retrying an
            // App-Bound-Encrypted profile never starts working.
            CookieError::UnreadableScheme(_) => {
                crate::provider::FetchError::CredentialUnusable(detail)
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
            Self::UnreadableScheme(scheme) => write!(
                f,
                "the browser session for this domain is encrypted with {scheme}, \
                 which only the browser itself can decrypt"
            ),
            Self::Extract(m) => write!(f, "cookie extraction failed: {m}"),
            Self::Unsupported => write!(
                f,
                "browser-cookie extraction is not supported on this platform"
            ),
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
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
const SNAPSHOT_TTL: Duration = Duration::from_secs(45);

/// A copy of the cookie store, plus the key that decrypts what is inside it.
///
/// The key is held with the snapshot rather than separately because the two are
/// acquired together and are useless apart. Deriving it means another `security`
/// subprocess, which was also being paid once per provider per tick.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct Snapshot {
    path: PathBuf,
    keys: CookieKeys,
    taken_at: Instant,
}

/// The keys a single profile needs, one per scheme it may contain.
///
/// Two rather than one because a profile legitimately holds a MIXTURE: a host
/// that gains a working keyring re-encrypts new cookies as `v11` while every
/// older value keeps its `v10` prefix. Measured on the Linux test VM, whose jar
/// read 89 `v10` beside 3 `v11` after the store was switched. Keying the whole
/// snapshot on one scheme would silently drop whichever half lost.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct CookieKeys {
    /// Decrypts `v10` values. Always derivable: on macOS from the keychain
    /// password, on Linux from a constant compiled into Chromium.
    v10: Vec<u8>,
    /// Decrypts `v11` values, when this host has the keyring password.
    ///
    /// `None` is the ordinary case rather than a fault: macOS has no `v11` at
    /// all, and on Linux the Secret Service may be absent, locked, or hold no
    /// Chrome entry. A `None` here refuses `v11` values exactly as before this
    /// key existed, which keeps a keyring problem from costing the `v10` half
    /// of the same jar.
    v11: Option<Vec<u8>>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Drop for Snapshot {
    fn drop(&mut self) {
        // Best-effort: a leftover copy is a stale temp file, not a correctness
        // problem, and there is nothing useful to do if removal fails.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Whether a snapshot taken at `taken_at` must be replaced before serving `now`.
///
/// Separated from the extraction path so the decision can be tested without a
/// cookie store, a keychain, or control over the clock.
#[cfg(any(target_os = "macos", target_os = "linux"))]
/// Take a fresh snapshot only when the held one has aged out.
///
/// Separate from its caller so the REUSE can be tested. `snapshot_is_stale`
/// answers whether a copy is needed and is easy to test on its own, but a
/// correct answer nobody consults suppresses nothing — and the caller cannot be
/// driven in a test, because it discovers and copies a real Chrome store. With
/// the acquisition injected, a test can call twice and assert one copy, which is
/// the property the reuse exists for: nine cookie providers fanning out in one
/// refresh tick share a single snapshot instead of copying the same database
/// nine times.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn refresh_snapshot_if_stale<F>(
    guard: &mut Option<Snapshot>,
    now: Instant,
    ttl: Duration,
    acquire: F,
) -> Result<(), CookieError>
where
    F: FnOnce() -> Result<Snapshot, CookieError>,
{
    if snapshot_is_stale(guard.as_ref().map(|s| s.taken_at), now, ttl) {
        // Dropped before the replacement is built so the old copy is removed
        // even if taking the new one fails.
        *guard = None;
        *guard = Some(acquire()?);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn chrome_cookies_for(domain_suffix: &str) -> Result<CookieJar, CookieError> {
    // The lock is held across the copy so that a cohort fanning out together
    // takes ONE snapshot rather than racing to take nine identical ones. These
    // callers already run on blocking threads, and the work serialised here is
    // the work being eliminated.
    let mut guard = SNAPSHOT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    refresh_snapshot_if_stale(&mut guard, Instant::now(), SNAPSHOT_TTL, || {
        let store = locate_chrome_cookie_store()?.ok_or(CookieError::NoStore)?;
        let keys = cookie_keys()?;
        let path = copy_cookie_store(&store)?;
        Ok(Snapshot {
            path,
            keys,
            taken_at: Instant::now(),
        })
    })?;

    let snapshot = guard
        .as_ref()
        .expect("a snapshot was just taken or was already fresh");
    let v10_key = snapshot.keys.v10.clone();
    let v11_key = snapshot.keys.v11.clone();
    let rows = read_encrypted_cookies(&snapshot.path, domain_suffix)?;
    drop(guard);

    let mut cookies = Vec::new();
    let mut refused = Vec::new();
    for (host_key, name, encrypted) in rows {
        // A cookie we can't decrypt is skipped, not fatal — a partial jar that
        // still carries the session cookie is usable.
        if let Some(value) = decrypt_value(&encrypted, &host_key, &v10_key, v11_key.as_deref()) {
            cookies.push(Cookie {
                name,
                value,
                host_key,
            });
        } else {
            refused.push(Scheme::of(&encrypted));
        }
    }
    if cookies.is_empty() {
        return Err(unreadable_or_absent(&refused));
    }
    Ok(CookieJar { cookies })
}

/// Decide what an empty jar means, given the schemes that were refused.
///
/// Reached only when nothing decrypted, and the two outcomes are opposite facts
/// about the host: no rows at all means nobody logged in, while rows that all
/// carry a scheme this cannot read mean somebody did and the values are sealed.
/// Collapsing the second into the first sends a reader to log in again, which
/// cannot help -- App-Bound Encryption will seal the new session exactly as it
/// sealed the old one.
///
/// A row that failed for any other reason (a corrupt blob, a wrong key) reports
/// as absent, which is the conservative direction: naming a scheme is a claim
/// that the scheme is the reason, and that claim is only safe when every refused
/// row agrees on it.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn unreadable_or_absent(refused: &[Scheme]) -> CookieError {
    let sealed = |scheme: &Scheme| matches!(scheme, Scheme::V12 | Scheme::V20);
    match refused.first() {
        Some(first) if sealed(first) && refused.iter().all(|s| s == first) => {
            CookieError::UnreadableScheme(first.label())
        }
        _ => CookieError::NoCookie,
    }
}

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
///
/// Gated to the platforms whose store layout and key source this understands,
/// like the rest of the extraction path. The gate has to be repeated on every
/// item using the gated imports above -- an item left ungated still compiles on
/// a platform where the import it needs exists, and fails only on the others.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn locate_chrome_cookie_store() -> Result<Option<PathBuf>, CookieError> {
    let Some(home) = crate::env::home_dir() else {
        return Ok(None);
    };
    locate_under(&home.join(CHROME_DATA_SUBPATH))
}

/// Where Chrome keeps its user data, relative to the home directory.
///
/// This follows CHROME's behaviour rather than the host's convention for
/// application data, because the directory belongs to Chrome. On Linux that is
/// `~/.config/google-chrome` even though the same tree also holds XDG config.
///
/// Defined per platform through [`chrome_data_subpath`] rather than as a bare
/// `cfg` constant, so every value is visible to a test on every host. A path
/// only compiled on one platform is a path only checkable there, and a wrong
/// one does not fail: it finds no store, which this module publishes as a host
/// where nobody logged in.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const CHROME_DATA_SUBPATH: &str = chrome_data_subpath(std::env::consts::OS);

/// The Chrome user-data directory for a given `std::env::consts::OS` value.
///
/// `const` so the caller above stays a compile-time constant, and takes the OS
/// as an argument so a test can ask for a platform it is not running on.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const fn chrome_data_subpath(os: &str) -> &'static str {
    // `match` on a &str is not const, so compare bytes.
    if matches!(os.as_bytes(), b"macos") {
        "Library/Application Support/Google/Chrome"
    } else {
        ".config/google-chrome"
    }
}

/// Search and classify an arbitrary profile directory.
///
/// Separate from [`locate_chrome_cookie_store`], which resolves one fixed
/// location under the home directory, so a test can pass a deliberately
/// unlistable path instead of altering the real Chrome installation to reach
/// that branch.
#[cfg(any(target_os = "macos", target_os = "linux"))]
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
    // Both spellings, and not because one is legacy: which one a profile uses
    // depends on the Chrome that created it. Measured on Chrome 151 for Linux,
    // a fresh profile has no `Network` directory at all and keeps the database
    // directly in the profile, while the documented layout puts it under
    // `Network/`. Searching only one finds nothing on the other, and finding
    // nothing here is reported as a host where nobody logged in.
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

    derive_key(&password, MACOS_PBKDF2_ROUNDS)
}

/// Derive the key for a `v10` cookie on Linux.
///
/// `v10` on Linux is the fallback Chrome uses when no keyring is available, and
/// its password is a literal constant compiled into Chromium rather than a
/// secret -- so this needs no keyring, no D-Bus, and cannot fail.
///
/// It is also more common than the `--password-store` flag suggests. Measured
/// on a host whose Secret Service was running but whose collection was LOCKED:
/// Chrome was told to use the keyring, silently fell back, and wrote `v10`.
/// Any host whose keyring is locked when the browser starts behaves this way.
///
/// `v11` -- the keyring-backed scheme -- is recognised and refused rather than
/// read: obtaining its password means a Secret Service conversation over D-Bus,
/// and an untested key derivation here would not fail, it would produce
/// plausible garbage. See [`Scheme`].
#[cfg(target_os = "linux")]
fn safe_storage_key() -> Result<Vec<u8>, CookieError> {
    derive_key(LINUX_FALLBACK_PASSWORD, LINUX_PBKDF2_ROUNDS)
}

/// The attributes Chrome stores its `v11` password under in the Secret Service.
///
/// Matched on rather than the item's LABEL, which is display text: it is
/// localised, and both Chrome and Chromium have shipped several ("Chrome Safe
/// Storage", "Chromium Safe Storage"). Observed verbatim on the Linux test VM.
#[cfg(target_os = "linux")]
const CHROME_SECRET_ATTRIBUTES: [(&str, &str); 2] = [
    ("application", "chrome"),
    ("xdg:schema", "chrome_libsecret_os_crypt_password_v2"),
];

/// Fetch Chrome's `v11` storage password from the Secret Service and derive its
/// key. `Ok(None)` means this host simply has no `v11` key to offer.
///
/// Every failure here is `Ok(None)` rather than an error, and that is the whole
/// design: a keyring that is absent, locked, or holds no Chrome entry is the
/// ORDINARY state of a Linux host, and the same jar's `v10` cookies stay
/// perfectly readable. Propagating a D-Bus failure would take a working cookie
/// cohort dark over a scheme the profile may not even contain.
///
/// Only unlocked items are considered. A locked item would raise a GUI unlock
/// prompt on the user's desktop — from a background daemon they did not
/// knowingly start, which is not a thing this module may do.
#[cfg(target_os = "linux")]
fn secret_service_key() -> Option<Vec<u8>> {
    use secret_service::blocking::SecretService;
    use secret_service::EncryptionType;

    let service = SecretService::connect(EncryptionType::Dh).ok()?;
    let attributes = std::collections::HashMap::from(CHROME_SECRET_ATTRIBUTES);
    let found = service.search_items(attributes).ok()?;
    let item = found.unlocked.first()?;
    let password = item.get_secret().ok()?;
    if password.is_empty() {
        return None;
    }
    // The stored secret is arbitrary bytes; Chrome derives over them directly.
    let password = String::from_utf8_lossy(&password).into_owned();
    derive_key(&password, LINUX_PBKDF2_ROUNDS).ok()
}

/// Assemble the keys for this host's profile.
#[cfg(target_os = "linux")]
fn cookie_keys() -> Result<CookieKeys, CookieError> {
    Ok(CookieKeys {
        v10: safe_storage_key()?,
        v11: secret_service_key(),
    })
}

/// Assemble the keys for this host's profile.
///
/// macOS has no `v11`: the scheme exists for the Secret Service and KWallet,
/// which this platform does not have, so the key is `None` by construction
/// rather than by a failed lookup.
#[cfg(target_os = "macos")]
fn cookie_keys() -> Result<CookieKeys, CookieError> {
    Ok(CookieKeys {
        v10: safe_storage_key()?,
        v11: None,
    })
}

/// The constant password Chromium uses for `v10` on Linux.
#[cfg(any(target_os = "linux", test))]
const LINUX_FALLBACK_PASSWORD: &str = "peanuts";

/// PBKDF2 rounds Chrome uses on Linux.
#[cfg(any(target_os = "linux", test))]
const LINUX_PBKDF2_ROUNDS: u32 = 1;

/// PBKDF2 rounds Chrome uses on macOS.
///
/// Platform-specific and not interchangeable: Chrome on Linux derives the same
/// way with ONE round. Deriving with the wrong count is the quiet failure this
/// scheme invites -- see [`derive_key`].
#[cfg(any(target_os = "macos", test))]
const MACOS_PBKDF2_ROUNDS: u32 = 1003;

/// Derive Chrome's AES key from its storage password.
///
/// `PBKDF2-HMAC-SHA1(password, "saltysalt", rounds)` to 16 bytes. The round
/// count is the caller's because it is the one part of this that varies by
/// platform, and the password's source varies with it: a keychain item on macOS,
/// the Secret Service or a fixed constant on Linux.
///
/// **A wrong round count does not fail.** It produces a different 16-byte key,
/// which then decrypts every cookie into plausible-looking bytes -- AES-CBC has
/// no integrity check here, so the only signal is that the plaintext is
/// nonsense. Nothing raises, and the cookie jar comes back full of garbage that
/// gets sent to the provider, which answers 401 and is published as a rejected
/// credential. So a test asserting merely that decryption "did not fail" passes
/// with the wrong constant, and only a known-plaintext vector per scheme can
/// tell the difference.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn derive_key(password: &str, rounds: u32) -> Result<Vec<u8>, CookieError> {
    use hmac::Hmac;
    use sha1::Sha1;

    let mut key = [0u8; 16];
    pbkdf2::pbkdf2::<Hmac<Sha1>>(password.as_bytes(), b"saltysalt", rounds, &mut key)
        .map_err(|e| CookieError::Extract(format!("pbkdf2: {e}")))?;
    Ok(key.to_vec())
}

/// Copy the (possibly locked) cookie DB to a temp path.
///
/// Chrome keeps the database open, so this reads a consistent snapshot without
/// contending on its lock. The caller owns the returned path and is responsible
/// for removing it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
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
#[cfg(any(target_os = "macos", target_os = "linux"))]
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

/// Decrypt one cookie value, choosing the key by the value's own prefix.
///
/// Returns `None` when the value's scheme is unreadable here or decryption
/// fails; the caller skips it and records the scheme, so a jar that is half
/// readable still serves. Pure given the keys — unit-testable.
///
/// Dispatch is per VALUE and not per host, because one profile holds a mixture.
///
/// The gate includes `test` so the dispatch stays exercised on every host: it is
/// pure given its keys, and a platform that compiles it out would report a green
/// suite while the choice between schemes went unchecked.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn decrypt_value(
    encrypted: &[u8],
    host_key: &str,
    v10_key: &[u8],
    v11_key: Option<&[u8]>,
) -> Option<String> {
    match Scheme::of(encrypted) {
        Scheme::V10 => decrypt_cbc(encrypted, b"v10", host_key, v10_key),
        Scheme::V11 => decrypt_cbc(encrypted, b"v11", host_key, v11_key?),
        _ => None,
    }
}

/// The shared AES-128-CBC body behind `v10` and `v11`.
///
/// The two schemes are the SAME cipher, IV, padding and host-hash prefix; they
/// differ only in where the password came from — a constant or the keychain for
/// `v10`, the Secret Service for `v11`. Writing it once means a fix to the
/// padding or the prefix cannot land on one scheme and miss the other.
///
/// `prefix` is passed rather than inferred so the caller's choice of key and the
/// blob actually decrypted cannot disagree: decrypting a `v11` value with the
/// `v10` key produces garbage rather than an error (see [`derive_key`]).
///
/// Factored out so tests can exercise it with a blob encrypted under a known
/// key, with no keychain and no D-Bus.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn decrypt_cbc(encrypted: &[u8], prefix: &[u8], host_key: &str, key: &[u8]) -> Option<String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    use sha2::{Digest, Sha256};

    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    let ciphertext = encrypted.strip_prefix(prefix)?;
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

    /// Every cookie failure is judged for which wire class it becomes.
    ///
    /// The class decides what a consumer does, and the three outcomes are not
    /// interchangeable: `credential_absent` says nobody logged in, and the
    /// health check keeps such providers out of its `degraded` list so that
    /// list stays a set of things somebody can act on; `credential_unusable`
    /// says somebody did log in and the credential cannot be used here, which
    /// does appear there; and a transient class leaves a window already being
    /// served in place rather than replacing it with a failure.
    ///
    /// Enumerated with an exhaustive match so a new variant stops compiling
    /// here instead of taking whichever arm it happens to fall into.
    #[test]
    fn every_cookie_failure_maps_to_the_class_a_consumer_acts_on() {
        use crate::provider::FetchError;

        let all = [
            CookieError::NoStore,
            CookieError::NoCookie,
            CookieError::Unsupported,
            CookieError::UnreadableScheme("v20"),
            CookieError::NoKeychainKey("locked".into()),
            CookieError::Extract("mid-write".into()),
        ];

        for case in &all {
            match case {
                CookieError::NoStore
                | CookieError::NoCookie
                | CookieError::Unsupported
                | CookieError::UnreadableScheme(_)
                | CookieError::NoKeychainKey(_)
                | CookieError::Extract(_) => {}
            }
        }

        // Nothing is configured on this host.
        for absent in [
            CookieError::NoStore,
            CookieError::NoCookie,
            CookieError::Unsupported,
        ] {
            let label = absent.to_string();
            assert!(
                matches!(FetchError::from(absent), FetchError::NoSession(_)),
                "{label} must report as an absent session"
            );
        }

        // A session exists and cannot be read. Not absent, and not transient:
        // retrying an App-Bound-Encrypted profile never starts working.
        let sealed = FetchError::from(CookieError::UnreadableScheme("v20"));
        assert!(
            matches!(sealed, FetchError::CredentialUnusable(_)),
            "a sealed session must not report as an absent one, got {sealed:?}"
        );

        // Conditions of the moment: a locked keychain or a store mid-write.
        for transient in [
            CookieError::NoKeychainKey("locked".into()),
            CookieError::Extract("mid-write".into()),
        ] {
            let label = transient.to_string();
            assert!(
                matches!(FetchError::from(transient), FetchError::Upstream(_)),
                "{label} must stay transient so a served window survives it"
            );
        }
    }

    /// A sealed session is not an absent one.
    ///
    /// Chrome's App-Bound Encryption keeps its key in a service that checks
    /// which executable is asking, so those cookies cannot be read by this
    /// process -- ever, on that profile. Reporting it as "no cookie" makes the
    /// provider read as an account nobody logged into, which after the health
    /// split lands it in the bucket operators are told not to watch, and points
    /// whoever does look at logging in again. Logging in again produces another
    /// sealed cookie.
    ///
    /// Asserted through the classifier rather than around it, so the mapping
    /// from prefix to outcome is what is under test.
    #[test]
    fn a_sealed_session_is_reported_differently_from_an_absent_one() {
        // Nothing was refused because nothing was there.
        assert!(matches!(unreadable_or_absent(&[]), CookieError::NoCookie));

        // Every row sealed by the same scheme: nameable, and named.
        let sealed = unreadable_or_absent(&[Scheme::V20, Scheme::V20]);
        assert!(
            matches!(sealed, CookieError::UnreadableScheme("v20")),
            "expected a named v20 refusal, got {sealed:?}"
        );
        assert!(matches!(
            unreadable_or_absent(&[Scheme::V12]),
            CookieError::UnreadableScheme("v12")
        ));

        // A scheme we CAN normally read failing is not a sealing claim: the key
        // may simply be wrong, and blaming the scheme would send the reader
        // somewhere there is nothing to find.
        assert!(matches!(
            unreadable_or_absent(&[Scheme::V10]),
            CookieError::NoCookie
        ));

        // Mixed rows: one sealed, one failed for its own reason. Naming a
        // scheme claims it is THE reason, which is only true when they agree.
        assert!(matches!(
            unreadable_or_absent(&[Scheme::V20, Scheme::V10]),
            CookieError::NoCookie
        ));
    }

    /// The scheme is read from the value, never assumed from the platform.
    ///
    /// One profile can hold several at once -- a machine that gains a keyring
    /// re-encrypts new cookies while old ones keep their old prefix -- so a
    /// per-platform assumption would misread the older half of a live profile.
    #[test]
    fn the_scheme_comes_from_the_value_prefix() {
        assert_eq!(Scheme::of(b"v10somecipher"), Scheme::V10);
        assert_eq!(Scheme::of(b"v11somecipher"), Scheme::V11);
        assert_eq!(Scheme::of(b"v12somecipher"), Scheme::V12);
        assert_eq!(Scheme::of(b"v20somecipher"), Scheme::V20);

        // An old profile can hold values with no prefix at all, and a future
        // Chrome can add one this does not know. Both are Unknown, and neither
        // is claimed as a sealed session.
        assert_eq!(Scheme::of(b"plaintextvalue"), Scheme::Unknown);
        assert_eq!(Scheme::of(b"v99future"), Scheme::Unknown);
        assert_eq!(Scheme::of(b""), Scheme::Unknown);
        assert!(matches!(
            unreadable_or_absent(&[Scheme::Unknown]),
            CookieError::NoCookie
        ));

        // Not vacuous: the labels are distinct, so a classifier collapsing two
        // schemes fails here rather than passing with a plausible name.
        let labels = [Scheme::V10, Scheme::V11, Scheme::V12, Scheme::V20].map(Scheme::label);
        let mut unique = labels.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "two schemes share a label");
    }

    /// Every platform's Chrome directory is pinned, including ones not running.
    ///
    /// These paths follow Chrome's own behaviour rather than the host's
    /// convention for application data, so neither can be derived from the
    /// other and both have to be stated. A wrong one produces no error: the
    /// search finds no store, and this module publishes that as a host where
    /// nobody logged in -- so the mistake reads as a true fact about the user.
    ///
    /// Asserted for both platforms from either, because a value behind a `cfg`
    /// is only checkable on the platform that compiles it, and that is the
    /// platform where someone is least likely to be looking when they change
    /// the other one.
    #[test]
    fn the_chrome_directory_is_pinned_for_every_platform() {
        assert_eq!(
            chrome_data_subpath("macos"),
            "Library/Application Support/Google/Chrome"
        );
        assert_eq!(chrome_data_subpath("linux"), ".config/google-chrome");

        // The one actually compiled in agrees with the table above, so this
        // cannot pass while the constant the code uses says something else.
        //
        // Gated because the constant exists only where extraction is supported,
        // while this test compiles everywhere. On a platform with no cookie
        // path the table above is still worth asserting -- it is the check that
        // survives someone editing a value they cannot compile.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert_eq!(
            CHROME_DATA_SUBPATH,
            chrome_data_subpath(std::env::consts::OS)
        );

        // Not vacuous: the two differ, so a function collapsing to one answer
        // fails here rather than passing with a plausible-looking path.
        assert_ne!(chrome_data_subpath("macos"), chrome_data_subpath("linux"));
    }

    /// The derived key is pinned to a known vector, per platform round count.
    ///
    /// This is the one property in the cookie path that cannot be checked by
    /// observing a failure, because a wrong round count does not produce one. It
    /// yields a valid 16-byte key that decrypts every cookie into nonsense, and
    /// AES-CBC carries no integrity check to notice -- the jar comes back full,
    /// the values are garbage, the provider answers 401, and it publishes as a
    /// credential the upstream rejected rather than as anything about this code.
    ///
    /// So the assertion is on the KEY BYTES against an independently computed
    /// vector, not on decryption succeeding. Chrome derives identically on Linux
    /// with a single round, which is why the count is pinned here rather than
    /// only used: the two schemes differ in nothing else, so a Linux port that
    /// reuses the macOS constant would be a one-word error with no symptom.
    #[test]
    fn the_derived_key_is_pinned_per_platform_round_count() {
        // PBKDF2-HMAC-SHA1("peanuts", "saltysalt", 1003, dkLen=16), computed with
        // Python's hashlib rather than by running this code, so the vector is not
        // whatever the implementation happens to produce.
        let macos = derive_key("peanuts", MACOS_PBKDF2_ROUNDS).expect("derives");
        assert_eq!(
            macos,
            [
                0xd9, 0xa0, 0x9d, 0x49, 0x9b, 0x4e, 0x1b, 0x74, 0x61, 0xf2, 0x8e, 0x67, 0x97, 0x2c,
                0x6d, 0xbd
            ],
            "the macOS key derivation changed"
        );

        // The Linux constants, named rather than repeated: this pins what the
        // Linux key path actually uses, so changing either constant fails here
        // instead of only changing behaviour on a platform this host cannot run.
        let linux = derive_key(LINUX_FALLBACK_PASSWORD, LINUX_PBKDF2_ROUNDS).expect("derives");
        assert_eq!(
            linux,
            [
                0xfd, 0x62, 0x1f, 0xe5, 0xa2, 0xb4, 0x02, 0x53, 0x9d, 0xfa, 0x14, 0x7c, 0xa9, 0x27,
                0x27, 0x78
            ],
            "the single-round derivation changed"
        );

        // The point of the whole test: the round count is load-bearing and the
        // two are not interchangeable.
        assert_ne!(
            macos, linux,
            "round counts must not produce the same key, or neither vector proves anything"
        );
    }

    /// A Chrome directory that cannot be listed must not read as Chrome absent.
    ///
    /// The two states reach consumers differently: absence is a permanent fact
    /// about the host and reports the account as never configured, while an
    /// unreadable store is a condition of the moment and keeps the last healthy
    /// window being served. Collapsing them takes all nine cookie-backed
    /// providers dark at once and reports it as nobody having logged in.
    ///
    /// The most recently written cookie database wins across profiles.
    ///
    /// Chrome keeps one database per profile, and a person who has ever created
    /// a second profile has several. Only one holds the session they are
    /// actually signed in with, and the others hold whatever was there when they
    /// stopped using them — so picking by anything other than recency reads a
    /// profile that may have no login at all, and this module publishes that as
    /// a host where nobody signed in.
    ///
    /// Both spellings are searched because which one a profile uses depends on
    /// the Chrome that created it, so the fixture places them in different
    /// profiles: an older `Network/Cookies` beside a newer bare `Cookies`. That
    /// also stops a rule preferring one layout over the other from passing.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_most_recently_written_profile_database_wins() {
        let root = std::env::temp_dir().join(format!("insula-newest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let stale = root.join("Profile 1").join("Network");
        std::fs::create_dir_all(&stale).unwrap();
        let stale_db = stale.join("Cookies");
        std::fs::write(&stale_db, b"old").unwrap();

        let live = root.join("Profile 2");
        std::fs::create_dir_all(&live).unwrap();
        let live_db = live.join("Cookies");
        std::fs::write(&live_db, b"new").unwrap();

        // Rewritten after a real pause so its mtime is genuinely later. Write
        // order alone is not enough: both files can land in the same filesystem
        // timestamp tick, and a fixture whose two mtimes are equal cannot
        // distinguish newest from oldest — it would pass under either rule.
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(&live_db, b"newer").unwrap();
        let stale_at = std::fs::metadata(&stale_db).unwrap().modified().unwrap();
        let live_at = std::fs::metadata(&live_db).unwrap().modified().unwrap();
        assert!(
            live_at > stale_at,
            "the fixture must actually separate the two mtimes, or it proves nothing"
        );

        let found = locate_under(&root)
            .expect("a listable directory must not error")
            .expect("a profile holding a database must be found");
        assert_eq!(
            found, live_db,
            "the newest database must win, not the first found"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

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
        encrypt_as(b"v10", value, host_key, key, with_prefix)
    }

    /// Encrypt a value the way Chrome does, under a chosen scheme prefix.
    fn encrypt_as(
        prefix: &[u8],
        value: &str,
        host_key: &str,
        key: &[u8],
        with_prefix: bool,
    ) -> Vec<u8> {
        let mut plaintext = Vec::new();
        if with_prefix {
            plaintext.extend_from_slice(&Sha256::digest(host_key.as_bytes()));
        }
        plaintext.extend_from_slice(value.as_bytes());
        let iv = [b' '; 16];
        let ct =
            Aes128CbcEnc::new(key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plaintext);
        let mut out = prefix.to_vec();
        out.extend_from_slice(&ct);
        out
    }

    /// `decrypt_value` must pick the key by the value's own prefix.
    ///
    /// The two keys are deliberately different, so a dispatch that sent `v11`
    /// values at the `v10` key would fail to recover the plaintext. Both
    /// directions are asserted from ONE jar, because the defect being fenced is
    /// a mixed profile — the state a host lands in the moment its keyring
    /// starts working — where keying everything one way silently drops half.
    #[test]
    fn each_scheme_is_decrypted_with_its_own_key() {
        let v10_key = [3u8; 16];
        let v11_key = [9u8; 16];
        let host = "ollama.com";

        let older = encrypt_as(b"v10", "from-the-constant", host, &v10_key, true);
        let newer = encrypt_as(b"v11", "from-the-keyring", host, &v11_key, true);

        assert_eq!(
            decrypt_value(&older, host, &v10_key, Some(&v11_key)).as_deref(),
            Some("from-the-constant")
        );
        assert_eq!(
            decrypt_value(&newer, host, &v10_key, Some(&v11_key)).as_deref(),
            Some("from-the-keyring")
        );
    }

    /// A host with no keyring key still reads the `v10` half of its jar.
    ///
    /// This is the ordinary Linux state, not an edge case: no Secret Service, a
    /// locked collection, or no Chrome entry in it. The `v11` values are refused
    /// — never decrypted under the `v10` key, which would yield garbage that
    /// reads as a real cookie value and gets sent to a provider.
    #[test]
    fn without_a_keyring_key_v10_still_reads_and_v11_is_refused() {
        let v10_key = [3u8; 16];
        let v11_key = [9u8; 16];
        let host = "ollama.com";

        let older = encrypt_as(b"v10", "still-readable", host, &v10_key, true);
        let newer = encrypt_as(b"v11", "needs-the-keyring", host, &v11_key, true);

        assert_eq!(
            decrypt_value(&older, host, &v10_key, None).as_deref(),
            Some("still-readable")
        );
        assert_eq!(decrypt_value(&newer, host, &v10_key, None), None);
    }

    #[test]
    fn decrypts_newer_chrome_value_with_domain_prefix() {
        let key = [7u8; 16];
        let enc = encrypt_v10("session-abc-123", "ollama.com", &key, true);
        assert_eq!(
            decrypt_cbc(&enc, b"v10", "ollama.com", &key).as_deref(),
            Some("session-abc-123")
        );
    }

    #[test]
    fn decrypts_older_chrome_value_without_prefix() {
        let key = [7u8; 16];
        let enc = encrypt_v10("plain-value", "ollama.com", &key, false);
        assert_eq!(
            decrypt_cbc(&enc, b"v10", "ollama.com", &key).as_deref(),
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
            decrypt_cbc(&enc, b"v10", "other.com", &key).as_deref(),
            Some("no-prefix-here")
        );
    }

    /// A blob whose prefix is not the one being decrypted must be refused
    /// rather than decrypted under the wrong key.
    ///
    /// This is the check that keeps the shared cipher body honest now that two
    /// schemes use it: `v10` and `v11` differ ONLY in key source, so a
    /// mis-dispatched blob decrypts into plausible garbage instead of failing.
    #[test]
    fn a_blob_of_another_scheme_returns_none() {
        let key = [7u8; 16];
        assert!(decrypt_cbc(b"v11garbage", b"v10", "ollama.com", &key).is_none());
        assert!(decrypt_cbc(b"v10garbage", b"v11", "ollama.com", &key).is_none());
        assert!(decrypt_cbc(b"", b"v10", "ollama.com", &key).is_none());
    }

    /// Decrypt a REAL `v11` cookie captured from Chrome on Linux.
    ///
    /// LIVE CAPTURE, not synthetic. Taken from Google Chrome 151 on Ubuntu
    /// 24.04 (ARM64) running with `--password-store=gnome-libsecret` against an
    /// unlocked Secret Service collection: the cookie is `SIDCC` for
    /// `.google.com`, and the password is the 24-byte secret Chrome itself
    /// stored under the item `Chrome Safe Storage`.
    ///
    /// This is the only test here that can tell a right PBKDF2 round count from
    /// a wrong one. A synthetic fixture is encrypted with whatever key the test
    /// derives, so it round-trips under ANY constant and proves nothing about
    /// the constant; this ciphertext was produced by Chrome, so only the real
    /// value decrypts it. The assertion rests on the 32-byte `SHA256(host_key)`
    /// prefix Chrome prepends — an integrity check that is astronomically
    /// unlikely to match under a wrong key, where "decryption did not error"
    /// would pass with any key at all, since AES-CBC has no integrity check and
    /// PKCS#7 unpadding succeeds on garbage about one time in 256.
    ///
    /// The password is a throwaway test VM's and decrypts nothing else; the
    /// cookie value is an expired session token from that VM's own browser.
    #[test]
    fn a_real_linux_v11_cookie_from_chrome_decrypts() {
        // Base64 of the raw secret bytes, exactly as read back from the
        // Secret Service item.
        let password = String::from_utf8(
            base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                "VGYrSnpaWWlBM09icTQ3QzBEcG1KZz09",
            )
            .expect("valid base64"),
        )
        .expect("the stored secret is ASCII");

        let key = derive_key(&password, LINUX_PBKDF2_ROUNDS).expect("derives");
        assert_eq!(
            hex(&key),
            "c61b2c9cdf89e77e930c9b104b77432b",
            "the Linux v11 key derivation drifted from the value Chrome uses"
        );

        let ciphertext = unhex(
            "7631319ef839f9d6d8e3fc98f79fbbb46af37245e80ca83d16045818d132f6f0             0ece908b3d517cc04f227c1a2f17bf54eb11c781a4848546b388c24764acf377             cdb4b8c59a0e187e583e1f5ac3f67b72904a2725c1fdd9f10c3fc67fca105943             43e4309886c9ffb8180724754e2a964d41b187",
        );

        let value = decrypt_cbc(&ciphertext, b"v11", ".google.com", &key)
            .expect("the captured v11 cookie decrypts under the derived key");
        assert!(
            value.starts_with("AKEyXzWfwKTCz8OicAgdJLoc"),
            "decrypted to unexpected plaintext: {value:?}"
        );
    }

    /// The macOS round count must NOT decrypt a Linux `v11` cookie.
    ///
    /// The paired negative for the test above, and the one that would catch the
    /// constants being swapped. Without it, a `derive_key` that ignored its
    /// `rounds` argument entirely would still pass the positive case.
    #[test]
    fn the_macos_round_count_does_not_decrypt_a_linux_cookie() {
        let password = "Tf+JzZYiA3Obq47C0DpmJg==";
        let wrong = derive_key(password, MACOS_PBKDF2_ROUNDS).expect("derives");
        assert_ne!(
            hex(&wrong),
            "c61b2c9cdf89e77e930c9b104b77432b",
            "1003 rounds must not produce the 1-round key"
        );

        let ciphertext = unhex(
            "7631319ef839f9d6d8e3fc98f79fbbb46af37245e80ca83d16045818d132f6f0             0ece908b3d517cc04f227c1a2f17bf54eb11c781a4848546b388c24764acf377             cdb4b8c59a0e187e583e1f5ac3f67b72904a2725c1fdd9f10c3fc67fca105943             43e4309886c9ffb8180724754e2a964d41b187",
        );

        // May unpad by chance, but must never yield the true plaintext.
        let decoded = decrypt_cbc(&ciphertext, b"v11", ".google.com", &wrong);
        assert!(
            !decoded.is_some_and(|v| v.starts_with("AKEyXzWfwKTCz8OicAgdJLoc")),
            "the wrong round count recovered the real value"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(text: &str) -> Vec<u8> {
        let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("valid hex"))
            .collect()
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
    /// A second caller inside the bound reuses the snapshot instead of copying.
    ///
    /// `snapshot_is_stale` is tested on its own and that is not the same
    /// property: a correct answer nobody consults suppresses nothing. The reuse
    /// exists because nine cookie providers fan out in one refresh tick, and
    /// each copying the same multi-megabyte database was reading roughly half a
    /// gigabyte an hour off disk to collect a handful of cookies.
    ///
    /// Both directions are needed. Asserting only that the second call skips is
    /// satisfied by an acquirer that never runs at all, so the third call —
    /// past the bound — has to be shown to copy again.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_second_caller_inside_the_bound_reuses_the_snapshot() {
        let mut held: Option<Snapshot> = None;
        let copies = std::cell::Cell::new(0);
        let start = Instant::now();

        let acquire = |guard: &mut Option<Snapshot>, now: Instant| {
            refresh_snapshot_if_stale(guard, now, Duration::from_secs(45), || {
                copies.set(copies.get() + 1);
                Ok(Snapshot {
                    path: std::path::PathBuf::from("/tmp/witness"),
                    keys: CookieKeys {
                        v10: Vec::new(),
                        v11: None,
                    },
                    taken_at: now,
                })
            })
            .expect("the injected acquirer cannot fail");
        };

        acquire(&mut held, start);
        assert_eq!(copies.get(), 1, "the first caller pays for a copy");

        // Every other provider in the cohort, arriving within the same tick.
        for offset in [1, 5, 20, 44] {
            acquire(&mut held, start + Duration::from_secs(offset));
        }
        assert_eq!(
            copies.get(),
            1,
            "callers inside the bound must share the first snapshot"
        );

        // Past the bound the store is read again, or the assertion above would
        // hold for an acquirer that had simply stopped working.
        acquire(&mut held, start + Duration::from_secs(45));
        assert_eq!(
            copies.get(),
            2,
            "a caller past the bound must take a fresh snapshot"
        );
    }

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
