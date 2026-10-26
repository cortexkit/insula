//! Configuration for quota providers, reloaded while the module runs.
//!
//! The wire module owns the file path and the JSONC parser. This crate defines
//! the plain, subc-independent configuration, the [`LiveQuotaConfig`] handle that
//! every consumer reads at the moment it decides, and the [`ConfigReloader`]
//! that keeps the handle in step with the file.
//!
//! A reload changes behaviour with no restart: the Codex provider reads
//! `codex.auto_use_resets` from the handle on every tick, so once the reloader
//! has applied an edit that sets it to 0, no later tick redeems, and once it
//! applies one that sets it back, later ticks may. That setting spends real
//! credit, so the reloader only ever fails toward the value already in force:
//! a file it cannot read or parse leaves the last good config in place, and
//! only a file that is truly gone falls back to the defaults (resets
//! disarmed).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

use crate::LOG_TAG;

/// Longest useful reset-expiry threshold: banked credits expire after 30 days.
pub const MAX_AUTO_USE_RESETS_SECS: u64 = 30 * 24 * 60 * 60;

/// Configuration for all providers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct QuotaConfig {
    pub codex: CodexConfig,
    pub muse: MuseConfig,
}

/// The team is an explicit choice: a web session can see several subscriptions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MuseConfig {
    pub team_id: Option<String>,
}

/// Codex banked-reset policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    /// Seconds before credit expiry at which a used window may consume a reset.
    /// Zero disables both mutation and relaxed reporting.
    #[serde(default, deserialize_with = "deserialize_auto_use_resets")]
    pub auto_use_resets: u64,
}

impl CodexConfig {
    pub fn is_enabled(&self) -> bool {
        self.auto_use_resets > 0
    }
}

fn deserialize_auto_use_resets<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let seconds = value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map(|seconds| seconds as u64)
    });
    Ok(seconds.unwrap_or_default().min(MAX_AUTO_USE_RESETS_SECS))
}

/// "armed" or "disarmed", the word every config log line uses for the Codex
/// reset policy, so an operator can grep one term for both directions.
fn armed_word(codex: &CodexConfig) -> &'static str {
    if codex.is_enabled() {
        "armed"
    } else {
        "disarmed"
    }
}

/// What the live handle holds: the config in force and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConfigStatus {
    pub config: QuotaConfig,
    /// When `config` was last taken from the config file, or from the file's
    /// absence (the defaults). `None` until that has happened once: a handle
    /// built in code, or a file that could not be used at startup.
    pub loaded_at: Option<DateTime<Utc>>,
    /// Why the latest look at the file could not be used, while that is still
    /// so. `config` is then the last good config, not what the file says.
    pub last_error: Option<String>,
}

/// The one shared copy of the configuration in force.
///
/// Cloning shares it: every clone sees a reload. A consumer must read it at
/// the moment it decides rather than keep a copy, or a reload never reaches
/// that decision. Built from a plain [`QuotaConfig`] or [`CodexConfig`] when
/// nothing will reload it (tests, examples).
#[derive(Debug, Clone)]
pub struct LiveQuotaConfig {
    inner: Arc<RwLock<LiveConfigStatus>>,
}

impl LiveQuotaConfig {
    pub fn new(config: QuotaConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(LiveConfigStatus {
                config,
                loaded_at: None,
                last_error: None,
            })),
        }
    }

    /// The config in force right now.
    #[cfg(test)]
    pub fn current(&self) -> QuotaConfig {
        self.status().config
    }

    /// The Codex reset policy in force right now.
    pub fn codex(&self) -> CodexConfig {
        self.read().config.codex.clone()
    }

    pub fn muse(&self) -> MuseConfig {
        self.read().config.muse.clone()
    }

    /// The config in force, when it was loaded, and any pending load error.
    pub fn status(&self) -> LiveConfigStatus {
        self.read().clone()
    }

    // A poisoned lock means a writer panicked while holding it. Every write
    // here is a plain field assignment that cannot panic halfway, so the
    // status is still whole: keep serving it rather than taking every
    // provider down with the panicked task.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, LiveConfigStatus> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, LiveConfigStatus> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Put `config` in force as loaded at `at`, returning the config it replaces.
    fn apply(&self, config: QuotaConfig, at: DateTime<Utc>) -> QuotaConfig {
        let mut status = self.write();
        status.loaded_at = Some(at);
        status.last_error = None;
        std::mem::replace(&mut status.config, config)
    }

    /// Record that the file could not be used, leaving the config in force.
    fn record_error(&self, reason: &str) {
        self.write().last_error = Some(reason.to_string());
    }
}

impl From<QuotaConfig> for LiveQuotaConfig {
    fn from(config: QuotaConfig) -> Self {
        Self::new(config)
    }
}

impl From<CodexConfig> for LiveQuotaConfig {
    fn from(codex: CodexConfig) -> Self {
        Self::new(QuotaConfig {
            codex,
            ..QuotaConfig::default()
        })
    }
}

/// Turns the config file's text into a config, or says why it cannot.
pub type ConfigParser = Box<dyn Fn(&str) -> Result<QuotaConfig, String> + Send>;

/// The file facts compared between checks to decide whether to read it again.
///
/// The file's bytes, not its length and modification time. Windows doesn't
/// reliably advance a file's modification time on a quick rewrite, so a
/// same-length edit such as `3600` to `7200` could go unnoticed and leave a
/// stale setting live. The file is a few hundred bytes read once per check, so
/// comparing its contents costs nothing, and only a real change is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileStamp {
    Missing,
    Present(Vec<u8>),
    /// The file could not be read for a reason other than absence (a
    /// permission error, say).
    Unreadable(String),
}

fn stamp_of(path: &Path) -> FileStamp {
    match std::fs::read(path) {
        Ok(bytes) => FileStamp::Present(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileStamp::Missing,
        Err(error) => FileStamp::Unreadable(format!("read failed: {error}")),
    }
}

/// What one look at the file found.
enum FileRead {
    /// No file: the documented default, so the defaults apply.
    Missing,
    Loaded(QuotaConfig),
    /// The file could not be statted, read or parsed.
    Failed(String),
}

/// Keeps a [`LiveQuotaConfig`] in step with the config file.
///
/// Polled rather than watched: a check reads the small file, and it is parsed
/// again only when its bytes changed. Each change of
/// state produces exactly one log line, returned from [`Self::check`].
///
/// THE FAILURE DIRECTION IS THE POINT. `codex.auto_use_resets` spends real
/// credit, so the reloader never moves the config on the strength of a file it
/// could not read: an unreadable or unparseable file keeps the last good
/// config, armed or not. Only a file that is gone applies the defaults, because
/// a missing config is the documented "no overrides" state and the default
/// disarms resets.
pub struct ConfigReloader {
    path: PathBuf,
    parse: ConfigParser,
    live: LiveQuotaConfig,
    /// The file as last examined. `None` before the first check.
    seen: Option<FileStamp>,
    /// The failure last logged, so a file that stays broken is reported once
    /// rather than on every check, and a different failure is reported again.
    reported_failure: Option<String>,
}

impl ConfigReloader {
    pub fn new(path: PathBuf, parse: ConfigParser, live: LiveQuotaConfig) -> Self {
        Self {
            path,
            parse,
            live,
            seen: None,
            reported_failure: None,
        }
    }

    #[cfg(test)]
    fn live(&self) -> &LiveQuotaConfig {
        &self.live
    }

    /// Look at the file once and bring the live config in step with it.
    ///
    /// Returns the line to log when something changed: the config itself, or
    /// whether the file can be used. `None` when the file is as it was at the
    /// last check (it is then not read at all), or when it was rewritten with
    /// the same settings.
    pub fn check(&mut self) -> Option<String> {
        let stamp = stamp_of(&self.path);
        if self.seen.as_ref() == Some(&stamp) {
            return None;
        }
        let first = self.seen.is_none();
        // Recorded before reading so a file that stays broken is not read and
        // reported again on every check. A file still being written when it is
        // read changes its stamp once the write lands, so it is read again then.
        self.seen = Some(stamp.clone());
        let read = match stamp {
            FileStamp::Missing => FileRead::Missing,
            FileStamp::Unreadable(reason) => FileRead::Failed(reason),
            FileStamp::Present(bytes) => match std::str::from_utf8(&bytes) {
                Ok(contents) => match (self.parse)(contents) {
                    Ok(config) => FileRead::Loaded(config),
                    Err(error) => FileRead::Failed(format!("parse failed: {error}")),
                },
                Err(error) => FileRead::Failed(format!("not UTF-8: {error}")),
            },
        };
        match read {
            FileRead::Missing => self.adopt(QuotaConfig::default(), first, true),
            FileRead::Loaded(config) => self.adopt(config, first, false),
            FileRead::Failed(reason) => self.keep_last_good(reason, first),
        }
    }

    fn adopt(&mut self, config: QuotaConfig, first: bool, missing: bool) -> Option<String> {
        let next = config.codex.clone();
        let previous = self.live.apply(config, Utc::now()).codex;
        let recovered = self.reported_failure.take().is_some();
        let path = self.path.display();
        let value = format!(
            "codex.auto_use_resets {}s ({})",
            next.auto_use_resets,
            armed_word(&next)
        );
        if first {
            let origin = if missing {
                format!("config {path} absent; defaults apply")
            } else {
                format!("config loaded from {path}")
            };
            return Some(format!(
                "{LOG_TAG} {origin}: {value}; edits apply without a restart; \
                 arm one host per account"
            ));
        }
        if previous != next {
            let removed = if missing {
                format!(" ({path} removed; defaults apply)")
            } else {
                String::new()
            };
            return Some(format!(
                "{LOG_TAG} config reloaded: codex.auto_use_resets {} -> {} ({}){removed}",
                previous.auto_use_resets,
                next.auto_use_resets,
                armed_word(&next)
            ));
        }
        if missing {
            Some(format!(
                "{LOG_TAG} config {path} removed; defaults already in force: {value}"
            ))
        } else if recovered {
            Some(format!(
                "{LOG_TAG} config {path} readable again; unchanged: {value}"
            ))
        } else {
            None
        }
    }

    fn keep_last_good(&mut self, reason: String, first: bool) -> Option<String> {
        self.live.record_error(&reason);
        if self.reported_failure.as_deref() == Some(reason.as_str()) {
            return None;
        }
        let kept = self.live.codex();
        // At the first check nothing was ever loaded, so what is kept is the
        // defaults the handle started with.
        let what = if first {
            "defaults"
        } else {
            "last good config"
        };
        let line = format!(
            "{LOG_TAG} warning: config {} unreadable or malformed ({reason}); keeping {what}: \
             codex.auto_use_resets {}s ({})",
            self.path.display(),
            kept.auto_use_resets,
            armed_word(&kept)
        );
        self.reported_failure = Some(reason);
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &str) -> QuotaConfig {
        serde_json::from_str(input).unwrap()
    }

    #[test]
    fn auto_use_resets_validates_and_clamps_without_rejecting_the_file() {
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":86400}}"#)
                .codex
                .auto_use_resets,
            86_400
        );
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":-1}}"#)
                .codex
                .auto_use_resets,
            0
        );
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":"soon"}}"#)
                .codex
                .auto_use_resets,
            0
        );
        // Compared against a literal rather than against the constant. Asserting
        // the clamped value equals `MAX_AUTO_USE_RESETS_SECS` is satisfied by
        // any ceiling at all -- both sides move together, so the test proves
        // clamping happens and never that it happens at a useful size. A ceiling
        // raised past a credit's 30-day lifetime would arm the feature
        // permanently: every credit is always "about to expire", so one is spent
        // on the first tick that finds any.
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":999999999}}"#)
                .codex
                .auto_use_resets,
            2_592_000,
            "a config value above the ceiling must clamp to 30 days"
        );
        assert_eq!(
            parse(r#"{"codex":{"auto_use_resets":1e30}}"#)
                .codex
                .auto_use_resets,
            2_592_000,
            "a float beyond u64 range must clamp to 30 days, not wrap"
        );
    }

    #[test]
    fn absent_and_unknown_fields_keep_the_feature_off() {
        assert_eq!(parse("{}").codex, CodexConfig::default());
        assert_eq!(
            parse(r#"{"unknown":true,"codex":{"future_option":1}}"#).codex,
            CodexConfig::default()
        );
    }

    /// Banked resets must default to disabled (0 seconds).
    ///
    /// Comparing against `CodexConfig::default()` alone only asserts agreement;
    /// if the default is raised above zero, both sides move together and the
    /// test passes while the feature arms unconditionally on every unconfigured
    /// account -- spending real user credit without opt-in.
    #[test]
    fn codex_banked_resets_default_to_disabled_and_zero_seconds() {
        assert_eq!(CodexConfig::default().auto_use_resets, 0);
        assert!(!CodexConfig::default().is_enabled());

        let empty = parse("{}");
        assert_eq!(empty.codex.auto_use_resets, 0);
        assert!(!empty.codex.is_enabled());

        let explicit_empty = parse(r#"{"codex":{}}"#);
        assert_eq!(explicit_empty.codex.auto_use_resets, 0);
        assert!(!explicit_empty.codex.is_enabled());
    }

    // ---- live reload ----

    use std::sync::atomic::{AtomicUsize, Ordering};

    const ARMED_3600: &str = r#"{"codex":{"auto_use_resets":3600}}"#;

    /// A reloader over `path` whose parser counts its calls, so a test can tell
    /// a file that was parsed again from one that was left alone.
    fn counting_reloader(path: &Path) -> (ConfigReloader, Arc<AtomicUsize>) {
        let parses = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&parses);
        let reloader = ConfigReloader::new(
            path.to_path_buf(),
            Box::new(move |text: &str| {
                counter.fetch_add(1, Ordering::SeqCst);
                serde_json::from_str(text).map_err(|error| error.to_string())
            }),
            LiveQuotaConfig::new(QuotaConfig::default()),
        );
        (reloader, parses)
    }

    /// A broken edit leaves the armed value in force and is reported once.
    ///
    /// Falling back to the defaults here would disarm a host whose operator
    /// saved a typo, and the reverse failure -- adopting whatever a broken file
    /// seems to say -- could arm one. Keeping the last good config is the only
    /// direction that cannot spend credit nobody configured.
    #[test]
    fn an_unparseable_edit_keeps_the_previous_value_and_logs_once() {
        let temp = crate::tests::ResetTempDir::with_prefix("ck-quota-config", "unparseable");
        let path = temp.path().join("ck-quota.jsonc");
        std::fs::write(&path, ARMED_3600).unwrap();
        let (mut reloader, _) = counting_reloader(&path);
        reloader.check();
        assert_eq!(reloader.live().codex().auto_use_resets, 3600);

        // A different length from the good file, so the edit is seen whatever
        // the filesystem's timestamp resolution.
        std::fs::write(&path, r#"{"codex":{"auto_use_resets":0"#).unwrap();
        let line = reloader.check().expect("a broken edit must be logged");
        assert!(
            line.contains(&path.display().to_string()) && line.contains("parse failed"),
            "the log line must name the file and the reason: {line}"
        );
        assert!(
            line.contains("keeping last good config: codex.auto_use_resets 3600s (armed)"),
            "the log line must say what stays in force: {line}"
        );
        assert_eq!(
            reloader.live().codex().auto_use_resets,
            3600,
            "an unparseable edit must keep the previous value"
        );
        assert!(reloader
            .live()
            .status()
            .last_error
            .is_some_and(|error| error.contains("parse failed")));
        assert_eq!(
            reloader.check(),
            None,
            "a file that stays broken is reported once, not on every check"
        );

        std::fs::write(&path, ARMED_3600).unwrap();
        let line = reloader.check().expect("recovery must be logged");
        assert!(line.contains("readable again"), "{line}");
        assert_eq!(reloader.live().status().last_error, None);
    }

    /// A deleted file applies the defaults, which disarm resets.
    ///
    /// Unlike an unreadable file, absence is a state the file documents: no
    /// config means no overrides. Keeping the last value would leave a host
    /// spending credit after its operator removed the setting.
    #[test]
    fn deleting_the_config_file_disarms_resets() {
        let temp = crate::tests::ResetTempDir::with_prefix("ck-quota-config", "deleted");
        let path = temp.path().join("ck-quota.jsonc");
        std::fs::write(&path, ARMED_3600).unwrap();
        let (mut reloader, _) = counting_reloader(&path);
        reloader.check();
        assert!(reloader.live().codex().is_enabled());

        std::fs::remove_file(&path).unwrap();
        let line = reloader.check().expect("the removal must be logged");
        assert!(
            line.contains("codex.auto_use_resets 3600 -> 0 (disarmed)") && line.contains("removed"),
            "{line}"
        );
        assert_eq!(reloader.live().codex(), CodexConfig::default());
        assert!(!reloader.live().codex().is_enabled());
        assert!(reloader.live().status().loaded_at.is_some());
    }

    /// A check that finds the file as it was does not read it again.
    ///
    /// The check runs every few seconds for the life of the process; parsing
    /// an unchanged file each time is wasted work, and would log again for a
    /// file that stays broken.
    #[test]
    fn an_unchanged_config_file_is_not_parsed_again() {
        let temp = crate::tests::ResetTempDir::with_prefix("ck-quota-config", "unchanged");
        let path = temp.path().join("ck-quota.jsonc");
        std::fs::write(&path, ARMED_3600).unwrap();
        let (mut reloader, parses) = counting_reloader(&path);
        assert!(
            reloader.check().is_some(),
            "the first check logs what it loaded"
        );
        assert_eq!(parses.load(Ordering::SeqCst), 1);

        for _ in 0..3 {
            assert_eq!(reloader.check(), None);
        }
        assert_eq!(
            parses.load(Ordering::SeqCst),
            1,
            "an unchanged file must not be parsed again"
        );

        // Not vacuous: a changed file is parsed, so the count above is not
        // stuck at one for some other reason.
        std::fs::write(&path, r#"{"codex":{"auto_use_resets":7200}}"#).unwrap();
        assert!(reloader.check().is_some());
        assert_eq!(parses.load(Ordering::SeqCst), 2);
        assert_eq!(reloader.live().codex().auto_use_resets, 7200);
    }
}
