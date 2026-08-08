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

/// Every credential family this module consumes, with the provider name each
/// one feeds.
///
/// Published so the deployed-module checkers can ask "is every configured
/// credential actually being served" without restating this list. A restated
/// copy would drift, and the drift is silent in one direction: a family the copy
/// lacks looks like a stray credential nobody consumes rather than a gap in the
/// checker.
///
/// Sharing it costs nothing the checkers rely on. They compare what this host is
/// CONFIGURED for against what the wire is SERVING, and those remain independent
/// — a family mapped to the wrong provider still leaves the right provider with
/// no credential, so the lane goes dark and the check fires either way.
pub const CREDENTIAL_FAMILIES: &[(&str, &str)] = &[
    ("chatgpt:openai", "codex"),
    ("oauth:anthropic", "claude"),
    ("oauth:xai", "grok"),
    // Before the `oauth:google` entry: Antigravity's own Google credential is
    // not a Gemini CLI one. Both reach the same Code Assist API and see
    // different products.
    ("antigravity:google", "antigravity"),
    ("oauth:google", "gemini"),
    ("kimi-for-coding", "kimi-for-coding"),
];
/// The `ck-quota` segment is a literal and is deliberately not derived from the
/// binary or module name, both of which have since been renamed. Beside a binary
/// called `ck-insula` it reads like a leftover; it is the file an operator mints
/// vault handles into, and renaming the segment silently reverts every
/// vault-served provider to its local credential lane — which still fetches, so
/// the loss shows up as accounts quietly losing their labels rather than as a
/// failure. If it ever moves, move the file first.
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ProviderKind {
    Codex,
    Anthropic,
    Grok,
    Gemini,
    Antigravity,
    KimiForCoding,
}

#[derive(Clone, Default)]
struct ProviderHandleSnapshot {
    codex: Vec<CredentialHandle>,
    anthropic: Vec<CredentialHandle>,
    grok: Vec<CredentialHandle>,
    gemini: Vec<CredentialHandle>,
    antigravity: Vec<CredentialHandle>,
    kimi_for_coding: Vec<CredentialHandle>,
}

impl ProviderHandleSnapshot {
    fn for_provider(&self, provider: ProviderKind) -> &[CredentialHandle] {
        match provider {
            ProviderKind::Codex => &self.codex,
            ProviderKind::Anthropic => &self.anthropic,
            ProviderKind::Grok => &self.grok,
            ProviderKind::Gemini => &self.gemini,
            ProviderKind::Antigravity => &self.antigravity,
            ProviderKind::KimiForCoding => &self.kimi_for_coding,
        }
    }

    fn push(&mut self, provider: ProviderKind, handle: CredentialHandle) {
        match provider {
            ProviderKind::Codex => self.codex.push(handle),
            ProviderKind::Anthropic => self.anthropic.push(handle),
            ProviderKind::Grok => self.grok.push(handle),
            ProviderKind::Antigravity => self.antigravity.push(handle),
            ProviderKind::Gemini => self.gemini.push(handle),
            ProviderKind::KimiForCoding => self.kimi_for_coding.push(handle),
        }
    }
}

#[derive(Default)]
struct LoaderState {
    last_warning: Option<String>,
    cached: Option<Result<ProviderHandleSnapshot, HandlesError>>,
    served_providers: HashSet<ProviderKind>,
}

/// Stateful warning suppression and one-parse-per-enumeration-cycle caching.
pub struct VaultHandleLoader {
    path: Option<PathBuf>,
    state: Mutex<LoaderState>,
}

impl VaultHandleLoader {
    pub fn from_env() -> Self {
        Self::new(vault_handles_path())
    }

    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            state: Mutex::new(LoaderState::default()),
        }
    }

    /// Return the authoritative Codex vault handle snapshot for this scheduler turn.
    pub fn codex_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Codex)
    }

    /// Return the authoritative Anthropic vault handle snapshot for this scheduler turn.
    pub fn anthropic_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Anthropic)
    }

    /// Return the authoritative Grok vault handle snapshot for this scheduler turn.
    pub fn grok_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Grok)
    }

    /// Return the authoritative Gemini vault handle snapshot for this scheduler turn.
    pub fn gemini_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Gemini)
    }

    /// Return the authoritative Antigravity vault handle snapshot for this
    /// scheduler turn.
    pub fn antigravity_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Antigravity)
    }

    /// Return the authoritative Kimi coding-plan vault handle snapshot for this
    /// scheduler turn.
    pub fn kimi_for_coding_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::KimiForCoding)
    }

    fn provider_handles(
        &self,
        provider: ProviderKind,
    ) -> Result<Vec<CredentialHandle>, HandlesError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.cached.is_none() || state.served_providers.contains(&provider) {
            let (result, warning) = match self.path.as_deref() {
                Some(path) => Self::interpret(path, load_file(path)),
                None => (Ok(ProviderHandleSnapshot::default()), None),
            };
            Self::update_warning(&mut state, warning);
            state.cached = Some(result);
            state.served_providers.clear();
        }
        state.served_providers.insert(provider);
        let snapshot = state
            .cached
            .as_ref()
            .expect("vault handle snapshot initialized")
            .clone()?;
        Ok(snapshot.for_provider(provider).to_vec())
    }

    fn interpret(
        path: &Path,
        result: LoadResult,
    ) -> (Result<ProviderHandleSnapshot, HandlesError>, Option<String>) {
        match result {
            LoadResult::Authoritative(handles) => {
                let (handles, warning) = map_handles(handles);
                (Ok(handles), warning)
            }
            LoadResult::AuthoritativeEmpty(reason) => (
                Ok(ProviderHandleSnapshot::default()),
                Some(format!("{}: {reason}", path.display())),
            ),
            LoadResult::Transient(reason) => {
                let message = format!("{}: {reason}", path.display());
                (Err(HandlesError::new(message.clone())), Some(message))
            }
        }
    }

    fn update_warning(state: &mut LoaderState, warning: Option<String>) {
        if state.last_warning == warning {
            return;
        }
        if let Some(message) = warning.as_deref() {
            eprintln!("[ck-quota] warning: vault handles {message}");
        }
        state.last_warning = warning;
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

fn map_handles(handles: HashMap<String, String>) -> (ProviderHandleSnapshot, Option<String>) {
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
            ProviderHandleSnapshot::default(),
            Some(format!("rejected invalid vault handle entries [{ids}]")),
        );
    }

    /// Whether a handle id names this credential family.
    ///
    /// A family's first credential is the bare prefix, and each additional
    /// account appends `:<label>` -- so every arm must accept both forms or that
    /// provider silently supports exactly one account. An id matched exactly
    /// falls through to the unsupported list, where a second account is dropped
    /// with only a stderr warning: the provider keeps serving its first account
    /// and looks entirely healthy, so nothing on the wire or in the health
    /// report says an account is missing.
    fn prefixed_id(id: &str, prefix: &str) -> bool {
        id == prefix
            || id
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(':'))
    }

    /// Resolve a handle id to the provider that consumes it.
    ///
    /// Walks [`CREDENTIAL_FAMILIES`] rather than a private chain of arms, so the
    /// deployed-module checkers can ask the same question against the same list.
    /// A second copy of it would drift, and a family missing from the copy reads
    /// as a stray credential rather than as a gap in the checker.
    ///
    /// Matching stops at the first family whose prefix fits, so adding an entry
    /// that is a prefix of an existing one would shadow it. No current pair has
    /// that relationship — including the two Google families, whose ids are
    /// disjoint despite looking similar.
    fn provider_for_id(id: &str) -> Option<ProviderKind> {
        let name = CREDENTIAL_FAMILIES
            .iter()
            .find(|(prefix, _)| prefixed_id(id, prefix))
            .map(|(_, provider)| *provider)?;
        match name {
            "codex" => Some(ProviderKind::Codex),
            "claude" => Some(ProviderKind::Anthropic),
            "grok" => Some(ProviderKind::Grok),
            "antigravity" => Some(ProviderKind::Antigravity),
            "gemini" => Some(ProviderKind::Gemini),
            "kimi-for-coding" => Some(ProviderKind::KimiForCoding),
            // Unreachable while every family names a provider handled above.
            // Returning None rather than panicking keeps a future entry from
            // taking down the refresher: the credential is dropped with the
            // stderr warning the unsupported path already emits.
            _ => None,
        }
    }

    let mut entries: Vec<_> = handles.into_iter().collect();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut unsupported = Vec::new();
    let mut by_capability: HashMap<String, Vec<(String, ProviderKind)>> = HashMap::new();
    for (credential_id, capability) in entries {
        if let Some(provider) = provider_for_id(&credential_id) {
            by_capability
                .entry(capability)
                .or_default()
                .push((credential_id, provider));
        } else {
            unsupported.push(credential_id);
        }
    }

    let mut capabilities: Vec<_> = by_capability.into_iter().collect();
    capabilities.sort_by(|(_, left_ids), (_, right_ids)| left_ids[0].0.cmp(&right_ids[0].0));
    let mut duplicate_groups = Vec::new();
    let mut mapped = ProviderHandleSnapshot::default();
    for (capability, ids) in capabilities {
        if ids.len() > 1 {
            duplicate_groups.push(
                ids.iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        mapped.push(
            ids[0].1,
            CredentialHandle::vault(ids[0].0.clone(), VaultCapability::new(capability)),
        );
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
            "ignored ids outside supported vault mapping [{}]",
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

    /// Every credential family this module maps supports a second account.
    ///
    /// The vault mints a family's first credential under the bare id and each
    /// additional account under `<id>:<label>`. A mapping arm that matches the
    /// id exactly therefore accepts only the first: the labelled ids fall
    /// through to the unsupported list and are dropped with a stderr warning,
    /// while the provider keeps serving its first account and reports healthy.
    /// Nothing on the wire or in the health report states that an account is
    /// missing, so the loss shows up as capacity that was never mentioned.
    ///
    /// Written as an enumeration over the families rather than a case per
    /// provider, because the defect is an ARM being written differently from
    /// its neighbours -- which is invisible when each arm is read on its own,
    /// and is exactly how two of these came to be exact-matched while four were
    /// not.
    #[test]
    fn every_credential_family_accepts_a_second_account() {
        // Each family's bare id, and the accessor a provider reads it through.
        type Accessor = fn(&VaultHandleLoader) -> Result<Vec<CredentialHandle>, HandlesError>;
        let families: Vec<(&str, Accessor)> = vec![
            ("chatgpt:openai", |l| l.codex_handles()),
            ("oauth:anthropic", |l| l.anthropic_handles()),
            ("oauth:xai", |l| l.grok_handles()),
            ("oauth:google", |l| l.gemini_handles()),
            ("antigravity:google", |l| l.antigravity_handles()),
            ("kimi-for-coding", |l| l.kimi_for_coding_handles()),
        ];

        for (base, accessor) in families {
            // Distinct capabilities: identical ones are deduplicated by design,
            // which would mask the very difference under test.
            let body =
                format!(r#"{{"handles":{{"{base}":"ckh_first","{base}:second":"ckh_second"}}}}"#);
            let path = write_file("family", &body);
            let handles = accessor(&VaultHandleLoader::new(Some(path.clone())))
                .expect("a well-formed handle file enumerates");
            let ids: Vec<String> = handles.iter().map(|h| h.stable_id().to_string()).collect();
            let _ = std::fs::remove_file(path);

            assert_eq!(
                ids.len(),
                2,
                "{base}: a second account did not reach the provider (got {ids:?}) -- \
                 this family supports exactly one account, and the rest are dropped \
                 with no signal on the wire"
            );
            assert!(
                ids.contains(&format!("{base}:second")),
                "{base}: the labelled account is missing from {ids:?}"
            );
        }
    }

    /// A labelled Antigravity credential stays out of the Gemini lane.
    ///
    /// The two are separate products on one Google API, and their ids are close
    /// enough that a widened match could capture the wrong one. Accepting a
    /// second Antigravity account must not also route it to Gemini, which would
    /// publish Antigravity's model pool -- Claude and GPT included -- as Gemini
    /// capacity.
    #[test]
    fn a_labelled_antigravity_account_does_not_reach_the_gemini_lane() {
        let path = write_file(
            "agy-vs-gemini",
            r#"{"handles":{"antigravity:google:second":"ckh_agy","oauth:google":"ckh_gemini"}}"#,
        );
        let loader = VaultHandleLoader::new(Some(path.clone()));
        let antigravity: Vec<String> = loader
            .antigravity_handles()
            .unwrap()
            .iter()
            .map(|h| h.stable_id().to_string())
            .collect();
        let gemini: Vec<String> = loader
            .gemini_handles()
            .unwrap()
            .iter()
            .map(|h| h.stable_id().to_string())
            .collect();
        let _ = std::fs::remove_file(path);

        assert_eq!(antigravity, vec!["antigravity:google:second".to_string()]);
        assert_eq!(gemini, vec!["oauth:google".to_string()]);
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
        let warning = loader.state.lock().unwrap().last_warning.clone().unwrap();
        assert!(warning.contains("chatgpt:openai"));
        assert!(!warning.contains("ckh_x"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn all_supported_provider_ids_are_mapped_without_an_ignored_warning() {
        let path = write_file(
            "all-providers",
            r#"{"handles":{"chatgpt:openai":"ckh_codex","oauth:anthropic":"ckh_anthropic","oauth:anthropic:work":"ckh_anthropic_work","oauth:xai":"ckh_grok","oauth:xai:work":"ckh_grok_work","antigravity:google":"ckh_antigravity","oauth:google:cli":"ckh_google","kimi-for-coding":"ckh_kimi","unknown:provider":"ckh_unknown"}}"#,
        );
        let loader = VaultHandleLoader::new(Some(path.clone()));

        assert_eq!(loader.codex_handles().unwrap().len(), 1);
        assert_eq!(loader.anthropic_handles().unwrap().len(), 2);
        assert_eq!(loader.grok_handles().unwrap().len(), 2);
        assert_eq!(loader.kimi_for_coding_handles().unwrap().len(), 1);

        // Both are Google credentials reaching the same Code Assist API, and
        // they must not be pooled: the Antigravity login sees Antigravity's
        // model quota (Claude and GPT alongside Gemini), which a Gemini CLI
        // login cannot access. Pooling them lets one product's capacity be
        // published under the other's name.
        let gemini = loader.gemini_handles().unwrap();
        assert_eq!(gemini.len(), 1);
        assert_eq!(gemini[0].stable_id(), "oauth:google:cli");
        let antigravity = loader.antigravity_handles().unwrap();
        assert_eq!(antigravity.len(), 1);
        assert_eq!(antigravity[0].stable_id(), "antigravity:google");
        let warning = loader.state.lock().unwrap().last_warning.clone().unwrap();
        assert!(warning.contains("unknown:provider"));
        for mapped_id in [
            "oauth:anthropic",
            "oauth:xai",
            "antigravity:google",
            "kimi-for-coding",
        ] {
            assert!(!warning.contains(mapped_id));
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_accessors_share_one_parse_until_a_provider_repeats() {
        let path = write_file(
            "one-parse",
            r#"{"handles":{"chatgpt:openai":"ckh_codex","oauth:anthropic":"ckh_anthropic"}}"#,
        );
        let loader = VaultHandleLoader::new(Some(path.clone()));
        assert_eq!(loader.codex_handles().unwrap().len(), 1);

        std::fs::write(
            &path,
            r#"{"handles":{"chatgpt:openai":"ckh_codex","oauth:xai":"ckh_grok"}}"#,
        )
        .unwrap();
        assert_eq!(loader.anthropic_handles().unwrap().len(), 1);
        assert!(loader.grok_handles().unwrap().is_empty());

        assert_eq!(loader.codex_handles().unwrap().len(), 1);
        assert_eq!(loader.grok_handles().unwrap().len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn transient_io_classifier_preserves_h5_path() {
        let transient = std::io::Error::new(ErrorKind::TimedOut, "secret must not appear");
        assert!(transient_io(&transient));
        let permanent = std::io::Error::new(ErrorKind::PermissionDenied, "denied");
        assert!(!transient_io(&permanent));

        let path = PathBuf::from("/test/vault-handles.json");
        let (empty, _) =
            VaultHandleLoader::interpret(&path, LoadResult::AuthoritativeEmpty("malformed"));
        assert!(empty.unwrap().codex.is_empty());
        let (transient, _) =
            VaultHandleLoader::interpret(&path, LoadResult::Transient("read failed"));
        assert!(transient.is_err());
    }
}
