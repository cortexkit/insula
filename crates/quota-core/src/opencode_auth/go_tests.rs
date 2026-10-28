use super::read_go_credential_from;
use crate::provider::FetchError;
use rusqlite::Connection;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const NOW: i64 = 1_800_000_000_000;

pub(crate) struct Store {
    home: crate::tests::ResetTempDir,
    overrides: Vec<(&'static str, OsString)>,
}

impl Store {
    pub(crate) fn new() -> Self {
        Self {
            home: crate::tests::ResetTempDir::with_prefix("ck-quota", "opencode-go-cli"),
            overrides: Vec::new(),
        }
    }

    fn directory(&self) -> PathBuf {
        self.home.path().join(".local/share/opencode")
    }

    pub(crate) fn database(&self) -> PathBuf {
        self.directory().join("opencode.db")
    }

    pub(crate) fn create_at(&self, path: &Path) -> Connection {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE credential (
                id TEXT PRIMARY KEY, integration_id TEXT, label TEXT, value TEXT,
                connector_id TEXT, method_id TEXT, active INTEGER,
                time_created INTEGER, time_updated INTEGER
            );",
        )
        .unwrap();
        db
    }

    pub(crate) fn row(db: &Connection, integration: &str, active: Option<i64>, value: &str) {
        db.execute(
            "INSERT INTO credential VALUES (lower(hex(randomblob(16))), ?1, 'fixture', ?2, NULL, NULL, ?3, 0, 0)",
            rusqlite::params![integration, value, active],
        ).unwrap();
    }

    fn legacy(&self, value: &str) {
        std::fs::create_dir_all(self.directory()).unwrap();
        std::fs::write(self.directory().join("auth.json"), value).unwrap();
    }

    pub(crate) fn read(&self) -> Result<Option<String>, FetchError> {
        read_go_credential_from(
            |name| {
                self.overrides
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone())
                    .or_else(|| {
                        matches!(name, "HOME" | "USERPROFILE").then(|| self.home.path().into())
                    })
            },
            NOW,
        )
    }
}

const GO_KEY: &str = r#"{"type":"key","key":"go-key"}"#;
const OAUTH: &str = r#"{"type":"oauth","access":"oauth-token","expires":1800000000001}"#;
const LEGACY: &str = r#"{"opencode-go":{"type":"api","key":"legacy-key"}}"#;

#[test]
fn active_go_row_beats_generic_oauth_and_ignores_inactive_rows() {
    let store = Store::new();
    let db = store.create_at(&store.database());
    Store::row(&db, "opencode", Some(1), OAUTH);
    Store::row(&db, "opencode-go", Some(0), "malformed");
    Store::row(&db, "opencode-go", None, "malformed");
    Store::row(&db, "opencode-go", Some(2), "malformed");
    assert_eq!(store.read().unwrap().as_deref(), Some("oauth-token"));
    Store::row(&db, "opencode-go", Some(1), GO_KEY);
    assert_eq!(store.read().unwrap().as_deref(), Some("go-key"));
    Store::row(&db, "opencode", Some(1), OAUTH);
    assert_eq!(store.read().unwrap().as_deref(), Some("go-key"));
}

#[test]
fn duplicate_active_rows_for_the_selected_integration_are_unusable() {
    for integration in ["opencode-go", "opencode"] {
        let store = Store::new();
        let db = store.create_at(&store.database());
        Store::row(&db, integration, Some(1), OAUTH);
        Store::row(&db, integration, Some(1), OAUTH);
        let error = store.read().unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable");
        assert!(error.to_string().contains("duplicate active rows"));
    }
}

#[test]
fn oauth_requires_a_future_integral_expiry_and_never_refreshes() {
    for integration in ["opencode", "opencode-go"] {
        for (expiry, reason) in [
            ("1800000000000", "expired"),
            ("1799999999999", "expired"),
            ("null", "missing"),
            ("-1", "malformed"),
            ("1800000000001.5", "malformed"),
        ] {
            let store = Store::new();
            let db = store.create_at(&store.database());
            store.legacy(LEGACY);
            if integration == "opencode-go" {
                Store::row(&db, "opencode", Some(1), OAUTH);
            }
            let value = format!(
                r#"{{"type":"oauth","access":"access-secret","refresh":"refresh-secret","expires":{expiry}}}"#
            );
            Store::row(&db, integration, Some(1), &value);
            let error = store.read().unwrap_err();
            assert_eq!(error.error_class(), "credential_unusable");
            let message = error.to_string();
            assert!(message.contains(reason), "{message}");
            assert!(!message.contains("secret"));
            let unchanged: String = db
                .query_row(
                    "SELECT value FROM credential WHERE integration_id = ?1",
                    [integration],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(unchanged, value);
        }
    }
}

#[test]
fn malformed_values_are_unusable_without_disclosing_tokens() {
    for value in [
        "not json",
        "[]",
        r#"{"type":"key"}"#,
        r#"{"type":"key","key":"secret token"}"#,
        r#"{"type":"key","key":""}"#,
        r#"{"type":"key","key":42}"#,
        r#"{"type":"oauth","access":"secret\n","expires":1800000000001}"#,
        r#"{"type":"unknown","key":"secret"}"#,
    ] {
        let store = Store::new();
        let db = store.create_at(&store.database());
        store.legacy(LEGACY);
        Store::row(&db, "opencode", Some(1), OAUTH);
        Store::row(&db, "opencode-go", Some(1), value);
        let error = store.read().unwrap_err();
        assert_eq!(error.error_class(), "credential_unusable", "{value}");
        assert!(!error.to_string().contains("secret"));
    }
}

#[test]
fn nontext_sqlite_values_are_unusable() {
    for value in ["NULL", "12", "X'7B7D'"] {
        let store = Store::new();
        let db = store.create_at(&store.database());
        Store::row(&db, "opencode-go", Some(1), GO_KEY);
        db.execute_batch(&format!("UPDATE credential SET value = {value}"))
            .unwrap();
        assert_eq!(
            store.read().unwrap_err().error_class(),
            "credential_unusable"
        );
    }
}

#[test]
fn legacy_is_used_only_when_the_database_or_table_is_absent() {
    let store = Store::new();
    assert!(store.read().unwrap().is_none());
    assert!(
        !store.database().exists(),
        "a read must not create a database"
    );
    store.legacy(LEGACY);
    assert_eq!(store.read().unwrap().as_deref(), Some("legacy-key"));
    Connection::open(store.database())
        .unwrap()
        .execute_batch("CREATE TABLE unrelated (id TEXT)")
        .unwrap();
    assert_eq!(store.read().unwrap().as_deref(), Some("legacy-key"));
    std::fs::remove_file(store.database()).unwrap();
    let db = store.create_at(&store.database());
    assert!(
        store.read().unwrap().is_none(),
        "an empty credential table retires auth.json"
    );
    Store::row(&db, "opencode", Some(1), GO_KEY);
    assert!(
        store.read().unwrap().is_none(),
        "a generic API key cannot authorize Go"
    );
    Store::row(&db, "opencode-go", Some(1), "malformed");
    assert_eq!(
        store.read().unwrap_err().error_class(),
        "credential_unusable"
    );
}

#[test]
fn legacy_accepts_only_a_dedicated_go_api_key() {
    for value in [
        r#"{"opencode":{"type":"api","key":"zen"}}"#,
        r#"{"opencode-go":{"type":"oauth","access":"token","expires":1800000000001}}"#,
        r#"{"opencode-go":{"type":"key","key":"go-key"}}"#,
    ] {
        let store = Store::new();
        store.legacy(value);
        assert!(store.read().unwrap().is_none());
    }
    for value in [
        "not json",
        "[]",
        r#"{"opencode-go":42}"#,
        r#"{"opencode-go":{"type":"api","key":"bad key"}}"#,
    ] {
        let store = Store::new();
        store.legacy(value);
        assert_eq!(
            store.read().unwrap_err().error_class(),
            "credential_unusable"
        );
    }
}

#[test]
fn unreadable_database_never_falls_back_to_legacy() {
    let store = Store::new();
    store.legacy(LEGACY);
    std::fs::write(store.database(), "not a SQLite database").unwrap();
    assert_eq!(
        store.read().unwrap_err().error_class(),
        "local_source_unavailable"
    );
    std::fs::remove_file(store.database()).unwrap();
    std::fs::create_dir(store.database()).unwrap();
    assert_eq!(
        store.read().unwrap_err().error_class(),
        "local_source_unavailable"
    );
}

#[test]
fn database_override_resolves_relative_absolute_and_memory_paths() {
    let mut store = Store::new();
    let default = store.create_at(&store.database());
    Store::row(&default, "opencode-go", Some(1), "malformed");
    let relative = store.directory().join("custom.db");
    let db = store.create_at(&relative);
    Store::row(&db, "opencode-go", Some(1), GO_KEY);
    store.overrides.push(("OPENCODE_DB", "custom.db".into()));
    assert_eq!(store.read().unwrap().as_deref(), Some("go-key"));
    let absolute = store.home.path().join("elsewhere.db");
    let db = store.create_at(&absolute);
    Store::row(&db, "opencode", Some(1), OAUTH);
    store.overrides[0].1 = absolute.into_os_string();
    assert_eq!(store.read().unwrap().as_deref(), Some("oauth-token"));
    store.legacy(LEGACY);
    store.overrides[0].1 = ":memory:".into();
    assert!(
        store.read().unwrap().is_none(),
        "memory mode disables both stores"
    );
}

#[test]
fn xdg_data_home_must_be_absolute() {
    let mut store = Store::new();
    let db = store.create_at(&store.database());
    Store::row(&db, "opencode-go", Some(1), GO_KEY);
    store.overrides.push(("XDG_DATA_HOME", "relative".into()));
    assert_eq!(store.read().unwrap().as_deref(), Some("go-key"));
    let data_home = store.home.path().join("xdg");
    let db = store.create_at(&data_home.join("opencode/opencode.db"));
    Store::row(&db, "opencode", Some(1), OAUTH);
    store.overrides[0].1 = data_home.into_os_string();
    assert_eq!(store.read().unwrap().as_deref(), Some("oauth-token"));
}

#[test]
fn live_wal_credentials_are_visible_to_the_shared_readonly_connection() {
    let store = Store::new();
    let db = store.create_at(&store.database());
    db.pragma_update(None, "journal_mode", "WAL").unwrap();
    Store::row(&db, "opencode-go", Some(1), GO_KEY);
    assert_eq!(store.read().unwrap().as_deref(), Some("go-key"));
}

#[test]
fn credential_reads_leave_the_main_database_unchanged() {
    let store = Store::new();
    let db = store.create_at(&store.database());
    Store::row(&db, "opencode-go", Some(1), GO_KEY);
    drop(db);
    let before = std::fs::read(store.database()).unwrap();
    assert_eq!(store.read().unwrap().as_deref(), Some("go-key"));
    assert_eq!(std::fs::read(store.database()).unwrap(), before);
    assert_eq!(std::fs::read_dir(store.directory()).unwrap().count(), 1);
}
