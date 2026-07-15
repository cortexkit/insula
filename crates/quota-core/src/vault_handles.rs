//! Secure enumeration of credential capability snapshots.
//!
//! The file is opened once, validated through metadata from that descriptor, and
//! read from the same descriptor. Invalid secret-file configuration is an
//! authoritative empty snapshot; genuine transient I/O keeps the prior registry
//! snapshot through `HandlesError`.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::{Error as _, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::credential_source::VaultCapability;
use crate::provider::{CredentialHandle, HandlesError};

pub const HANDLES_PATH_ENV: &str = "CK_QUOTA_VAULT_HANDLES_PATH";
const DEFAULT_RELATIVE_PATH: &str = ".config/cortexkit/ck-quota/vault-handles.json";

struct UniqueHandles(HashMap<String, String>);

impl<'de> Deserialize<'de> for UniqueHandles {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueHandlesVisitor;

        impl<'de> Visitor<'de> for UniqueHandlesVisitor {
            type Value = UniqueHandles;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a credential-id map with unique keys")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut handles = HashMap::new();
                while let Some((id, capability)) = map.next_entry::<String, String>()? {
                    if handles.insert(id.clone(), capability).is_some() {
                        return Err(A::Error::custom(format_args!(
                            "duplicate credential id {id:?}"
                        )));
                    }
                }
                Ok(UniqueHandles(handles))
            }
        }

        deserializer.deserialize_map(UniqueHandlesVisitor)
    }
}

struct HandleFile {
    handles: HashMap<String, String>,
}

impl<'de> Deserialize<'de> for HandleFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct HandleFileVisitor;

        impl<'de> Visitor<'de> for HandleFileVisitor {
            type Value = HandleFile;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object containing one handles map")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut seen = HashSet::new();
                let mut handles = None;
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(A::Error::custom(format_args!(
                            "duplicate top-level key {key:?}"
                        )));
                    }
                    if key == "handles" {
                        handles = Some(map.next_value::<UniqueHandles>()?.0);
                    } else {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(HandleFile {
                    handles: handles.ok_or_else(|| A::Error::missing_field("handles"))?,
                })
            }
        }

        deserializer.deserialize_map(HandleFileVisitor)
    }
}

/// Stateful warning suppression for the frequently-polled handles file.
pub struct VaultHandleLoader {
    path: Option<PathBuf>,
    last_warning: Mutex<Option<String>>,
}

impl VaultHandleLoader {
    pub fn from_env() -> Self {
        Self::new(vault_handles_path())
    }

    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            last_warning: Mutex::new(None),
        }
    }

    /// Return the authoritative Codex vault handle snapshot for this scheduler turn.
    pub fn codex_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        let Some(path) = self.path.as_deref() else {
            self.update_warning(None);
            return Ok(Vec::new());
        };
        self.interpret(path, load_file(path))
    }

    fn interpret(
        &self,
        path: &Path,
        result: LoadResult,
    ) -> Result<Vec<CredentialHandle>, HandlesError> {
        match result {
            LoadResult::Authoritative(handles) => {
                let (handles, warning) = map_codex_handles(handles);
                self.update_warning(warning);
                Ok(handles)
            }
            LoadResult::AuthoritativeEmpty(reason) => {
                self.update_warning(Some(format!("{}: {reason}", path.display())));
                Ok(Vec::new())
            }
            LoadResult::Transient(reason) => {
                let message = format!("{}: {reason}", path.display());
                self.update_warning(Some(message.clone()));
                Err(HandlesError::new(message))
            }
        }
    }

    fn update_warning(&self, warning: Option<String>) {
        let mut previous = self
            .last_warning
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *previous == warning {
            return;
        }
        if let Some(message) = warning.as_deref() {
            eprintln!("[ck-quota] warning: vault handles {message}");
        }
        *previous = warning;
    }
}

pub fn vault_handles_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(HANDLES_PATH_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_RELATIVE_PATH))
}

enum LoadResult {
    Authoritative(HashMap<String, String>),
    AuthoritativeEmpty(&'static str),
    Transient(&'static str),
}

fn load_file(path: &Path) -> LoadResult {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }

    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return LoadResult::Authoritative(HashMap::new())
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return LoadResult::AuthoritativeEmpty("refusing symbolic link")
        }
        Err(error) if transient_io(&error) => {
            return LoadResult::Transient("transient open failure")
        }
        Err(_) => return LoadResult::AuthoritativeEmpty("cannot securely open file"),
    };

    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) if transient_io(&error) => {
            return LoadResult::Transient("transient descriptor inspection failure")
        }
        Err(_) => return LoadResult::AuthoritativeEmpty("cannot inspect opened file"),
    };
    if !metadata.is_file() {
        return LoadResult::AuthoritativeEmpty("opened path is not a regular file");
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return LoadResult::AuthoritativeEmpty("file is group/world accessible");
        }
    }

    let mut bytes = Vec::new();
    match file.read_to_end(&mut bytes) {
        Ok(_) => {}
        Err(error) if transient_io(&error) => {
            return LoadResult::Transient("transient read failure")
        }
        Err(_) => return LoadResult::AuthoritativeEmpty("cannot read opened file"),
    }
    match serde_json::from_slice::<HandleFile>(&bytes) {
        Ok(parsed) => LoadResult::Authoritative(parsed.handles),
        Err(_) => LoadResult::AuthoritativeEmpty("file is malformed"),
    }
}

fn transient_io(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
    ) {
        return true;
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if error.raw_os_error().is_some_and(|code| {
        matches!(
            code,
            libc::EIO | libc::ESTALE | libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::EBUSY
        )
    }) {
        return true;
    }
    false
}

fn map_codex_handles(handles: HashMap<String, String>) -> (Vec<CredentialHandle>, Option<String>) {
    let mut invalid_ids = handles
        .iter()
        .filter_map(|(credential_id, capability)| {
            let invalid_id = credential_id.trim() != credential_id
                || credential_id.chars().any(char::is_control);
            let invalid_capability = capability.is_empty()
                || capability.trim() != capability
                || !capability.starts_with("ckh_");
            (invalid_id || invalid_capability).then(|| credential_id.clone())
        })
        .collect::<Vec<_>>();
    if !invalid_ids.is_empty() {
        invalid_ids.sort();
        let ids = invalid_ids
            .iter()
            .map(|id| id.escape_default().to_string())
            .collect::<Vec<_>>()
            .join(",");
        return (
            Vec::new(),
            Some(format!("rejected invalid vault handle entries [{ids}]")),
        );
    }

    let mut entries: Vec<_> = handles.into_iter().collect();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut unsupported = Vec::new();
    let mut by_capability: HashMap<String, Vec<String>> = HashMap::new();
    for (credential_id, capability) in entries {
        if credential_id == "chatgpt:openai" || credential_id.starts_with("chatgpt:openai:") {
            by_capability
                .entry(capability)
                .or_default()
                .push(credential_id);
        } else {
            unsupported.push(credential_id);
        }
    }

    let mut capabilities: Vec<_> = by_capability.into_iter().collect();
    capabilities.sort_by(|(_, left_ids), (_, right_ids)| left_ids[0].cmp(&right_ids[0]));
    let mut duplicate_groups = Vec::new();
    let mut mapped = Vec::with_capacity(capabilities.len());
    for (capability, ids) in capabilities {
        if ids.len() > 1 {
            duplicate_groups.push(ids.join(","));
        }
        mapped.push(CredentialHandle::vault(
            ids[0].clone(),
            VaultCapability::new(capability),
        ));
    }

    let mut warnings = Vec::new();
    if !duplicate_groups.is_empty() {
        warnings.push(format!(
            "deduplicated identical capabilities for ids [{}]",
            duplicate_groups.join("; ")
        ));
    }
    if !unsupported.is_empty() {
        warnings.push(format!(
            "ignored ids outside merge-1 codex mapping [{}]",
            unsupported.join(",")
        ));
    }
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    (mapped, warning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_ID: AtomicUsize = AtomicUsize::new(0);

    fn write_file(label: &str, body: &str) -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ck-quota-vault-handles-{label}-{}-{id}.json",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn duplicate_json_keys_are_authoritative_empty() {
        for body in [
            r#"{"handles":{"chatgpt:openai":"ckh_a","chatgpt:openai":"ckh_b"}}"#,
            r#"{"handles":{},"handles":{"chatgpt:openai":"ckh_a"}}"#,
        ] {
            let path = write_file("duplicate", body);
            let loader = VaultHandleLoader::new(Some(path.clone()));
            assert!(loader.codex_handles().unwrap().is_empty());
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn identical_capabilities_are_deduplicated_without_exposing_them() {
        let secret = "ckh_secret_dedup";
        let path = write_file(
            "dedup",
            &format!(
                r#"{{"handles":{{"chatgpt:openai":"{secret}","chatgpt:openai:gmail":"{secret}"}}}}"#
            ),
        );
        let handles = VaultHandleLoader::new(Some(path.clone()))
            .codex_handles()
            .unwrap();
        assert_eq!(handles.len(), 1);
        assert!(!format!("{:?}", handles[0]).contains(secret));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn symlink_and_insecure_mode_are_authoritative_empty() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let target = write_file("target", r#"{"handles":{"chatgpt:openai":"ckh_a"}}"#);
        let link = target.with_extension("link");
        symlink(&target, &link).unwrap();
        assert!(VaultHandleLoader::new(Some(link.clone()))
            .codex_handles()
            .unwrap()
            .is_empty());

        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(VaultHandleLoader::new(Some(target.clone()))
            .codex_handles()
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_file(link);
        let _ = std::fs::remove_file(target);
    }

    #[test]
    fn i7_surrounding_capability_whitespace_is_authoritative_empty() {
        let path = write_file(
            "semantic-invalid",
            r#"{"handles":{"chatgpt:openai":"  ckh_x "}}"#,
        );
        let loader = VaultHandleLoader::new(Some(path.clone()));
        assert!(loader.codex_handles().unwrap().is_empty());
        let warning = loader.last_warning.lock().unwrap().clone().unwrap();
        assert!(warning.contains("chatgpt:openai"));
        assert!(!warning.contains("ckh_x"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn transient_io_classifier_preserves_h5_path() {
        let transient = std::io::Error::new(ErrorKind::TimedOut, "secret must not appear");
        assert!(transient_io(&transient));
        let permanent = std::io::Error::new(ErrorKind::PermissionDenied, "denied");
        assert!(!transient_io(&permanent));

        let path = PathBuf::from("/test/vault-handles.json");
        let loader = VaultHandleLoader::new(Some(path.clone()));
        assert!(loader
            .interpret(&path, LoadResult::AuthoritativeEmpty("malformed"))
            .unwrap()
            .is_empty());
        assert!(loader
            .interpret(&path, LoadResult::Transient("read failed"))
            .is_err());
    }
}
