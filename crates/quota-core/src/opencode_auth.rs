//! Reader for OpenCode's credential stores in `~/.local/share/opencode/`.
//!
//! Unlike CodexBar — which reaches each provider's own native credential store
//! (macOS Keychain for Claude, bespoke CLI files, browser cookies) — this module
//! runs inside the CortexKit/opencode ecosystem, where opencode already holds
//! OAuth tokens and API keys for many providers in cross-platform stores.
//! Preferring it (where it carries the provider) avoids the macOS-only Keychain
//! path and collapses several providers into a single session archetype.
//!
//! OpenCode 2 stores live credentials in `opencode.db`'s `credential` table;
//! OpenCode 1 uses `auth.json`. Both use the same credential JSON shape. We
//! compare freshness when both hold a provider, without ever refreshing tokens:
//! using a rotating refresh token would invalidate OpenCode's own session.

use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;

use crate::provider::FetchError;

/// One provider's credential entry in either OpenCode store.
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
    /// A static API key. OpenCode 1's auth.json tags it `api`; OpenCode 2's
    /// `credential` table tags the same shape `key` (measured 2026-10-28 on a
    /// live store: every API-key row read `"type":"key"`).
    #[serde(alias = "key")]
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
    auth_path_from(|name| std::env::var_os(name))
}

/// The same resolution against an injected environment.
///
/// Exists so the ABSENT case is testable. Reading the process environment
/// directly would leave the unresolvable-home arm reachable only by unsetting
/// every home variable in a live process, and this crate deliberately has no
/// env-mutating tests -- twenty such sites were removed for the flakiness they
/// caused across a shared thread pool. Mirrors `env::home_dir_from`, which exists
/// for the same reason.
pub(crate) fn auth_path_from(
    lookup: impl Fn(&str) -> Option<std::ffi::OsString> + Copy,
) -> Option<PathBuf> {
    if let Some(xdg) = lookup("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("opencode").join("auth.json"));
    }
    crate::env::home_dir_from(lookup).map(|home| {
        home.join(".local")
            .join("share")
            .join("opencode")
            .join("auth.json")
    })
}

/// Classify a resolved-or-not auth path, so the refusal is testable apart from
/// the environment that produces it.
pub(crate) fn path_or_refusal(path: Option<PathBuf>) -> Result<PathBuf, FetchError> {
    path.ok_or_else(|| {
        FetchError::LocalSourceUnavailable(
            "cannot locate opencode's auth file: neither XDG_DATA_HOME nor a home \
             directory could be resolved for this process"
                .to_string(),
        )
    })
}

/// Read one provider's freshest credential from OpenCode's local stores.
///
/// `Ok(None)` means neither store has this provider. An unavailable database is
/// `LocalSourceUnavailable`, not an absent credential. A usable entry in the
/// other store can still be used if one store fails.
pub fn read_provider(provider: &str) -> Result<Option<OpencodeAuth>, FetchError> {
    // An unresolved home means we could not look, not that no credential exists.
    // The remedy is fixing the process environment, not signing in again.
    let path = path_or_refusal(auth_path())?;
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
    let database = read_database_provider_at(&path.with_file_name("opencode.db"), provider);
    if let Err(error) = &database {
        warn_database_once(error);
    }
    let json = read_json_provider_at(path, provider);
    match (database, json) {
        (Ok(Some(database)), Ok(Some(json))) => {
            let database_is_newer = match (&database.auth, &json) {
                (
                    OpencodeAuth::Oauth {
                        expires: Some(db_expires),
                        ..
                    },
                    OpencodeAuth::Oauth {
                        expires: Some(json_expires),
                        ..
                    },
                ) if db_expires != json_expires => db_expires > json_expires,
                _ => database.updated_ms >= json_modified_ms(path)?,
            };
            Ok(Some(if database_is_newer {
                database.auth
            } else {
                json
            }))
        }
        (Err(_), Ok(Some(json))) => Ok(Some(json)),
        (Ok(Some(database)), _) => Ok(Some(database.auth)),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(None), json) => json,
    }
}

struct DatabaseCredential {
    auth: OpencodeAuth,
    updated_ms: i64,
}

fn read_database_provider_at(
    path: &std::path::Path,
    provider: &str,
) -> Result<Option<DatabaseCredential>, FetchError> {
    let unavailable = |error| {
        FetchError::LocalSourceUnavailable(format!("reading OpenCode opencode.db: {error}"))
    };
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unavailable(error.to_string())),
    }

    // Do not use SQLite's immutable mode: OpenCode keeps writing to the WAL,
    // and ignoring it could return an older credential from the main database.
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| unavailable(error.to_string()))?;
    let row: Option<(String, i64)> = connection
        .query_row(
            "SELECT value, time_updated FROM credential
             WHERE integration_id = ?1 AND (active IS NULL OR active <> 0)
             ORDER BY time_updated DESC LIMIT 1",
            [provider],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| unavailable(error.to_string()))?;
    row.map(|(value, updated_ms)| {
        let auth = serde_json::from_str(&value)
            .map_err(|error| unavailable(format!("credential entry not decodable: {error}")))?;
        Ok(DatabaseCredential { auth, updated_ms })
    })
    .transpose()
}

fn json_modified_ms(path: &std::path::Path) -> Result<i64, FetchError> {
    let modified = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| {
            FetchError::CredentialUnusable(format!(
                "reading the opencode auth store timestamp: {error}"
            ))
        })?;
    let elapsed = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| {
            FetchError::CredentialUnusable(format!(
                "opencode auth store timestamp before epoch: {error}"
            ))
        })?;
    i64::try_from(elapsed.as_millis()).map_err(|error| {
        FetchError::CredentialUnusable(format!(
            "opencode auth store timestamp out of range: {error}"
        ))
    })
}

fn warn_database_once(error: &FetchError) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!("[insula] warning: {error}; trying OpenCode auth.json instead");
    });
}

fn read_json_provider_at(
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

    /// An unresolvable home is "I could not look", never "nothing is configured".
    ///
    /// insula#18. `Ok(None)` from this function is a CONTRACT VALUE meaning the
    /// user never logged in, and it publishes `credential_absent` -- the class
    /// that authorises a consumer to prune the account. Returning it because the
    /// process has no home directory reports an account state on evidence nobody
    /// gathered.
    ///
    /// The doc comment on `read_provider` records the SAME defect being fixed for
    /// unreadable files, and the unresolvable-path arm three lines below it kept
    /// returning `Ok(None)`. Fixing an instance is not closing a class.
    ///
    /// Injected rather than env-mutating: this crate removed twenty env-mutating
    /// test sites for the cross-test flakiness they caused, so the absent case is
    /// reached through `auth_path_from` instead of by unsetting HOME in a live
    /// process.
    #[test]
    fn an_unresolvable_home_refuses_rather_than_reporting_no_account() {
        let nothing_set = |_: &str| None;
        assert!(
            auth_path_from(nothing_set).is_none(),
            "precondition: with no home variables there is no path to read"
        );

        let error = path_or_refusal(auth_path_from(nothing_set))
            .expect_err("an unresolvable path must refuse, not report an absence");
        assert_eq!(
            error.error_class(),
            "local_source_unavailable",
            "the subject is this machine's environment, not the account: \
             `credential_absent` would tell a consumer to prune an account nobody \
             looked for"
        );

        // THE CONTROL: a resolvable home still yields a path, or the refusal
        // above would fire on every healthy host and take five providers dark.
        // An absolute path on the host platform, offered under both names: Windows
        // resolves USERPROFILE first and refuses a drive-less "/home/..." as relative.
        let home = std::env::temp_dir().join("someone");
        let home = &home;
        let home_set = |name: &str| {
            matches!(name, "HOME" | "USERPROFILE").then(|| home.clone().into_os_string())
        };
        let path = auth_path_from(home_set).expect("a resolvable home yields a path");
        assert!(
            path.ends_with(".local/share/opencode/auth.json"),
            "and it is opencode's auth file: {path:?}"
        );
        assert!(path_or_refusal(Some(path)).is_ok());
    }

    use super::*;

    fn fixture(label: &str) -> crate::tests::ResetTempDir {
        crate::tests::ResetTempDir::with_prefix("insula-opencode-auth", label)
    }

    fn create_database(dir: &std::path::Path) -> Connection {
        let connection = Connection::open(dir.join("opencode.db")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE credential (
                    id TEXT PRIMARY KEY, integration_id TEXT, label TEXT NOT NULL,
                    value TEXT NOT NULL, connector_id TEXT, method_id TEXT, active INTEGER,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL
                );",
            )
            .unwrap();
        connection
    }

    fn insert_credential(
        connection: &Connection,
        provider: &str,
        active: Option<i64>,
        updated_ms: i64,
        value: serde_json::Value,
    ) {
        connection
            .execute(
                "INSERT INTO credential
                 (id, integration_id, label, value, active, time_created, time_updated)
                 VALUES (?1, ?2, 'fixture', ?3, ?4, ?5, ?5)",
                rusqlite::params![
                    format!("{provider}-{updated_ms}"),
                    provider,
                    value.to_string(),
                    active,
                    updated_ms,
                ],
            )
            .unwrap();
    }

    fn oauth(access: &str, expires: Option<i64>) -> serde_json::Value {
        serde_json::json!({
            "type": "oauth", "methodID": "fixture", "access": access,
            "refresh": "must-not-be-used", "expires": expires,
        })
    }

    fn write_json(dir: &std::path::Path, value: serde_json::Value) -> i64 {
        let path = dir.join("auth.json");
        std::fs::write(&path, serde_json::json!({"anthropic": value}).to_string()).unwrap();
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    fn token_at(dir: &std::path::Path) -> String {
        match read_provider_at(&dir.join("auth.json"), "anthropic")
            .unwrap()
            .unwrap()
        {
            OpencodeAuth::Oauth { access, .. } => access,
            OpencodeAuth::Api { key } => key,
        }
    }

    #[test]
    fn database_only_reads_live_credential() {
        let dir = fixture("database-only");
        let connection = create_database(dir.path());
        insert_credential(
            &connection,
            "anthropic",
            None,
            10,
            oauth("database", Some(20)),
        );
        drop(connection);

        assert_eq!(token_at(dir.path()), "database");
        assert!(read_provider_at(&dir.path().join("auth.json"), "openai")
            .unwrap()
            .is_none());
    }

    #[test]
    fn auth_json_only_keeps_opencode_one_working() {
        let dir = fixture("json-only");
        write_json(dir.path(), oauth("json", Some(10)));
        assert_eq!(token_at(dir.path()), "json");
        assert!(!dir.path().join("opencode.db").exists());
    }

    #[test]
    fn database_newer_oauth_expiry_wins() {
        let dir = fixture("database-newer-expiry");
        write_json(dir.path(), oauth("stale-json", Some(10)));
        let connection = create_database(dir.path());
        insert_credential(
            &connection,
            "anthropic",
            None,
            1,
            oauth("fresh-database", Some(20)),
        );

        // Both recorded expiries are in the past, and the JSON file's mtime is
        // newer. Expiry still decides freshness, without suppressing the token.
        assert_eq!(token_at(dir.path()), "fresh-database");
    }

    #[test]
    fn auth_json_newer_oauth_expiry_wins() {
        let dir = fixture("json-newer-expiry");
        let mtime = write_json(dir.path(), oauth("fresh-json", Some(20)));
        let connection = create_database(dir.path());
        insert_credential(
            &connection,
            "anthropic",
            None,
            mtime + 60_000,
            oauth("stale-database", Some(10)),
        );
        assert_eq!(token_at(dir.path()), "fresh-json");
    }

    #[test]
    fn active_null_is_live_and_zero_is_excluded() {
        let dir = fixture("active");
        let connection = create_database(dir.path());
        insert_credential(&connection, "anthropic", None, 10, oauth("live", Some(10)));
        insert_credential(
            &connection,
            "anthropic",
            Some(0),
            20,
            oauth("inactive", Some(20)),
        );
        insert_credential(
            &connection,
            "openai",
            Some(1),
            30,
            oauth("other-provider", Some(30)),
        );
        assert_eq!(token_at(dir.path()), "live");

        connection
            .execute("DELETE FROM credential WHERE active IS NULL", [])
            .unwrap();
        assert!(read_provider_at(&dir.path().join("auth.json"), "anthropic")
            .unwrap()
            .is_none());
    }

    #[test]
    fn newest_database_row_wins_before_comparing_sources() {
        let dir = fixture("newest-row");
        let connection = create_database(dir.path());
        insert_credential(
            &connection,
            "anthropic",
            Some(1),
            10,
            oauth("old-row", Some(200)),
        );
        insert_credential(
            &connection,
            "anthropic",
            Some(1),
            20,
            oauth("new-row", Some(100)),
        );
        assert_eq!(token_at(dir.path()), "new-row");
    }

    #[test]
    fn api_keys_are_compared_by_timestamp() {
        let dir = fixture("api-timestamp");
        let mtime = write_json(
            dir.path(),
            serde_json::json!({"type": "api", "key": "json-key"}),
        );
        let connection = create_database(dir.path());
        insert_credential(
            &connection,
            "anthropic",
            None,
            mtime + 60_000,
            // OpenCode 2 tags API keys `key`, unlike auth.json's `api`.
            serde_json::json!({"type": "key", "key": "database-key"}),
        );
        assert_eq!(token_at(dir.path()), "database-key");

        connection
            .execute("UPDATE credential SET time_updated = ?1", [mtime - 60_000])
            .unwrap();
        assert_eq!(token_at(dir.path()), "json-key");
    }

    #[test]
    fn missing_oauth_expiry_falls_back_to_timestamp() {
        let dir = fixture("missing-expiry");
        let connection = create_database(dir.path());
        for (db_expires, json_expires) in [(None, Some(20)), (Some(20), None), (None, None)] {
            connection.execute("DELETE FROM credential", []).unwrap();
            let mtime = write_json(dir.path(), oauth("json", json_expires));
            insert_credential(
                &connection,
                "anthropic",
                None,
                mtime + 60_000,
                oauth("database", db_expires),
            );
            assert_eq!(token_at(dir.path()), "database");
            connection
                .execute("UPDATE credential SET time_updated = ?1", [mtime - 60_000])
                .unwrap();
            assert_eq!(token_at(dir.path()), "json");
        }
    }

    #[test]
    fn unreadable_database_falls_back_without_becoming_absent() {
        for label in ["directory", "corrupt", "missing-table"] {
            let dir = fixture(label);
            let db_path = dir.path().join("opencode.db");
            match label {
                "directory" => std::fs::create_dir(&db_path).unwrap(),
                "corrupt" => std::fs::write(&db_path, b"not a SQLite database").unwrap(),
                _ => drop(Connection::open(&db_path).unwrap()),
            }
            write_json(dir.path(), oauth("json-fallback", Some(10)));
            assert_eq!(token_at(dir.path()), "json-fallback");

            std::fs::remove_file(dir.path().join("auth.json")).unwrap();
            let error = read_provider_at(&dir.path().join("auth.json"), "anthropic")
                .expect_err("a broken database is unavailable, not absent");
            assert!(matches!(error, FetchError::LocalSourceUnavailable(_)));
            assert!(error.to_string().contains("opencode.db"));
        }
    }

    #[test]
    fn database_only_read_leaves_bytes_and_mtime_unchanged() {
        let dir = fixture("read-only");
        let connection = create_database(dir.path());
        insert_credential(
            &connection,
            "anthropic",
            None,
            10,
            oauth("database", Some(20)),
        );
        drop(connection);
        let path = dir.path().join("opencode.db");
        let bytes_before = std::fs::read(&path).unwrap();
        let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert_eq!(token_at(dir.path()), "database");
        assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            mtime_before
        );
    }

    #[test]
    fn reads_current_credentials_from_live_wal() {
        let dir = fixture("wal");
        let connection = create_database(dir.path());
        insert_credential(
            &connection,
            "anthropic",
            None,
            10,
            oauth("before-wal", Some(10)),
        );
        connection
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .unwrap();
        insert_credential(
            &connection,
            "anthropic",
            None,
            20,
            oauth("in-wal", Some(20)),
        );
        assert!(dir.path().join("opencode.db-wal").exists());

        // The writer stays open so the fresh row has not been checkpointed into
        // the main database; a reader ignoring the WAL would see the old row.
        assert_eq!(token_at(dir.path()), "in-wal");
    }

    #[test]
    fn injected_xdg_path_keeps_database_beside_auth_json() {
        let dir = fixture("xdg");
        let path = auth_path_from(|name| {
            (name == "XDG_DATA_HOME").then(|| dir.path().as_os_str().to_owned())
        })
        .unwrap();
        let data_dir = dir.path().join("opencode");
        assert_eq!(path, data_dir.join("auth.json"));
        std::fs::create_dir(&data_dir).unwrap();
        let connection = create_database(&data_dir);
        insert_credential(
            &connection,
            "anthropic",
            None,
            10,
            oauth("xdg-database", Some(20)),
        );
        assert!(matches!(read_provider_at(&path, "anthropic").unwrap(),
            Some(OpencodeAuth::Oauth { access, .. }) if access == "xdg-database"));
    }

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
        // A provider the store does not hold is absent, not an error.
        assert!(parse_provider(raw, "anthropic").unwrap().is_none());
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
