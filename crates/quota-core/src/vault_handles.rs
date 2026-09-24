//! In-memory routing for the credential snapshot installed by the refresher.
//!
//! Providers enumerate synchronously, so the scheduler performs the one async
//! scoped-list call and installs its result here before asking any provider for
//! handles. Reads never perform I/O and never clear the retained snapshot.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::credential_source::{ScopedRowState, ScopedSnapshot};
use crate::provider::{CredentialHandle, HandlesError};

/// Credential-id families consumed by this module and their registry providers.
pub const CREDENTIAL_FAMILIES: &[(&str, &str)] = &[
    ("chatgpt:openai", "codex"),
    ("oauth:anthropic", "claude"),
    ("oauth:xai", "grok"),
    ("antigravity:google", "antigravity"),
    ("oauth:google", "gemini"),
    ("kimi-for-coding", "kimi-for-coding"),
    ("apikey:kimi-for-coding", "kimi-for-coding"),
    ("cookie:ampcode.com", "amp"),
    ("oauth:cursor", "cursor"),
    ("cookie:cursor.com", "cursor"),
    ("cookie:qwencloud.com", "qwen-cloud"),
    ("cookie:qoder.com", "qoder"),
    ("cookie:factory.ai", "factory"),
    ("cookie:xiaomimimo.com", "mimo"),
    ("cookie:ollama.com", "ollama"),
    ("cookie:opencode.ai", "opencode"),
    ("cookie:opencode.ai", "opencodego"),
    // The OpenCode Go usage API's key. Routed to opencodego only: `opencode`
    // (the Zen balance) has no API-key lane.
    ("apikey:opencode", "opencodego"),
    ("apikey:deepseek", "deepseek"),
    ("apikey:synthetic", "synthetic"),
    ("apikey:openrouter", "openrouter"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ProviderKind {
    Codex,
    Anthropic,
    Grok,
    Gemini,
    Antigravity,
    KimiForCoding,
    Amp,
    Cursor,
    QwenCloud,
    Qoder,
    Factory,
    Mimo,
    Ollama,
    OpenCode,
    OpenCodeGo,
    DeepSeek,
    Synthetic,
    OpenRouter,
}

#[derive(Clone, Default)]
struct ProviderHandleSnapshot {
    by_provider: HashMap<ProviderKind, Vec<CredentialHandle>>,
}

impl ProviderHandleSnapshot {
    fn push(&mut self, provider: ProviderKind, handle: CredentialHandle) {
        self.by_provider.entry(provider).or_default().push(handle);
    }

    fn for_provider(&self, provider: ProviderKind) -> &[CredentialHandle] {
        self.by_provider
            .get(&provider)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

/// Whether the loader's handle mapping can be read as a verdict about this host.
///
/// Two states are not enough. An empty mapping on a process that has never heard
/// from its vault means "could not look", and reading it as "no vault
/// credentials here" makes every vault-aware provider fall back to its local
/// lane -- which collapsed five labelled accounts into one unlabelled row on a
/// cold start where the vault answered `module_warming` first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VaultHandleState {
    /// This host has no vault for this module. The mapping is empty and
    /// providers use their local lanes. The default, because every build and
    /// test without a credential source constructs a loader and never installs
    /// anything into it.
    #[default]
    NoVault,
    /// A vault is wired but has not answered an enumeration yet. Provider
    /// handle reads fail, so the scheduler treats every vault-aware provider as
    /// unfinished rather than as holding no credentials.
    Awaiting,
    /// At least one authoritative snapshot has been installed.
    Answered,
}

impl VaultHandleState {
    /// Wire spelling for the health surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoVault => "no_vault",
            Self::Awaiting => "awaiting",
            Self::Answered => "answered",
        }
    }
}

#[derive(Default)]
struct LoaderState {
    phase: VaultHandleState,
    snapshot: Option<ScopedSnapshot>,
    mapped: ProviderHandleSnapshot,
    installed_at: Option<Instant>,
    enumeration_failure: Option<String>,
    warning: Option<String>,
}

/// Result of attempting to install one scoped-list reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotInstall {
    pub authoritative: bool,
    pub reactivated_ids: HashSet<String>,
}

/// Shared installed scoped snapshot read synchronously by every provider.
pub struct VaultHandleLoader {
    state: Mutex<LoaderState>,
}

impl Default for VaultHandleLoader {
    fn default() -> Self {
        Self::new(None)
    }
}

impl VaultHandleLoader {
    /// Compatibility constructor for callers that used to pass a file path.
    ///
    /// The argument is deliberately ignored: no runtime file source remains.
    pub fn new(_retired_path: Option<PathBuf>) -> Self {
        Self {
            state: Mutex::new(LoaderState::default()),
        }
    }

    /// Construct an empty in-memory loader. Environment variables are not read.
    pub fn from_env() -> Self {
        Self::default()
    }

    /// Declare that a real credential source is wired to this loader, so an
    /// empty mapping must not be read as "no vault credentials" until the vault
    /// has answered once.
    ///
    /// Has no effect once a snapshot has been installed: a warm loader already
    /// holds an answer, and going back to waiting would blank it.
    pub fn await_first_answer(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.phase != VaultHandleState::Answered {
            state.phase = VaultHandleState::Awaiting;
        }
    }

    /// Current state of the mapping, for the health surface.
    pub fn handle_state(&self) -> VaultHandleState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .phase
    }

    #[cfg(test)]
    pub(crate) fn install_rows_for_test(&self, rows: &[(&str, &str)]) {
        self.install_snapshot(
            ScopedSnapshot {
                grants: 1,
                rows: rows
                    .iter()
                    .map(|(credential_id, credential_type)| ScopedRowState {
                        credential_id: (*credential_id).to_string(),
                        credential_type: (*credential_type).to_string(),
                        record_version: 1,
                        state: "active".to_string(),
                        account_id: None,
                    })
                    .collect(),
            },
            Instant::now(),
        );
    }

    /// Install one authoritative list result.
    ///
    /// A zero-grant result is deauthorisation, not an inventory fact, so the
    /// previous snapshot is retained. Any positive grant count, including an
    /// explicit empty row list, is authoritative.
    pub fn install_snapshot(&self, snapshot: ScopedSnapshot, now: Instant) -> SnapshotInstall {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if snapshot.grants == 0 {
            state.enumeration_failure = Some("principal is not granted".to_string());
            // A vault that answers and grants nothing is a real answer for a
            // loader that never heard from it: local lanes are correct. Once a
            // grant has been seen, zero grants is deauthorisation and the
            // previous snapshot is retained instead.
            if state.phase == VaultHandleState::Awaiting {
                state.phase = VaultHandleState::NoVault;
            }
            return SnapshotInstall {
                authoritative: false,
                reactivated_ids: HashSet::new(),
            };
        }

        let previous_states: HashMap<&str, &str> = state
            .snapshot
            .as_ref()
            .map(|previous| {
                previous
                    .rows
                    .iter()
                    .map(|row| (row.credential_id.as_str(), row.state.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let reactivated_ids = snapshot
            .rows
            .iter()
            .filter(|row| {
                row.state != "needs_reauth"
                    && previous_states.get(row.credential_id.as_str()) == Some(&"needs_reauth")
            })
            .map(|row| row.credential_id.clone())
            .collect();
        let (mapped, mapping_warning) = map_handles(&snapshot.rows);
        state.warning = if snapshot.rows.is_empty() {
            Some("scoped grant is empty".to_string())
        } else {
            mapping_warning
        };
        state.snapshot = Some(snapshot);
        state.mapped = mapped;
        state.installed_at = Some(now);
        state.enumeration_failure = None;
        state.phase = VaultHandleState::Answered;
        SnapshotInstall {
            authoritative: true,
            reactivated_ids,
        }
    }

    /// Record a list failure without disturbing the retained snapshot.
    pub fn retain_after_failure(&self, error: impl Into<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.enumeration_failure = Some(error.into());
    }

    /// Record a list failure that says no vault serves this module here.
    ///
    /// Moves a loader that has never been answered to `NoVault`, so the local
    /// lanes serve. A loader that HAS been answered keeps its snapshot exactly
    /// as [`Self::retain_after_failure`] would: a warm vault going away must not
    /// blank the lanes it was serving.
    pub fn mark_unavailable(&self, error: impl Into<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.enumeration_failure = Some(error.into());
        if state.phase == VaultHandleState::Awaiting {
            state.phase = VaultHandleState::NoVault;
        }
    }

    pub fn snapshot(&self) -> Option<ScopedSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot
            .clone()
    }

    pub fn retained_snapshot_age(&self, now: Instant) -> Option<Duration> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .installed_at
            .map(|installed| now.saturating_duration_since(installed))
    }

    pub fn enumeration_failure(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .enumeration_failure
            .clone()
    }

    pub fn warning(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .warning
            .clone()
    }

    pub fn installed_credential_ids(&self) -> Vec<String> {
        self.snapshot()
            .map(|snapshot| {
                snapshot
                    .rows
                    .into_iter()
                    .map(|row| row.credential_id)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn codex_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Codex)
    }

    /// Account ids the vault reports for the rows of the installed snapshot that
    /// are routed to the registry provider named `provider`, skipping rows whose
    /// account is unknown. The ids are returned as the vault reported them; a
    /// caller comparing them to a provider's own ids normalises both sides.
    ///
    /// Rows are routed by the same family table the provider's handle
    /// enumeration uses, so the two can never disagree about which rows belong
    /// to which provider. Read from the retained snapshot whatever the loader
    /// phase: the answer is a statement about accounts the vault holds, not
    /// about which handles are currently enumerable, so it survives a failed
    /// listing (a vault restart) exactly when it is most needed.
    pub fn account_ids_for_provider(&self, provider: &str) -> Vec<String> {
        let Some(kind) = provider_kind(provider) else {
            return Vec::new();
        };
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(snapshot) = state.snapshot.as_ref() else {
            return Vec::new();
        };
        let mapped_ids: HashSet<&str> = state
            .mapped
            .for_provider(kind)
            .iter()
            .filter_map(CredentialHandle::vault_credential_id)
            .collect();
        snapshot
            .rows
            .iter()
            .filter(|row| mapped_ids.contains(row.credential_id.as_str()))
            .filter_map(|row| row.account_id.clone())
            .collect()
    }

    pub fn anthropic_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Anthropic)
    }

    pub fn grok_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Grok)
    }

    pub fn gemini_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Gemini)
    }

    pub fn antigravity_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Antigravity)
    }

    pub fn kimi_for_coding_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::KimiForCoding)
    }

    pub fn amp_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Amp)
    }

    pub fn cursor_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Cursor)
    }

    pub fn qwen_cloud_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::QwenCloud)
    }

    pub fn qoder_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Qoder)
    }

    pub fn factory_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Factory)
    }

    pub fn mimo_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Mimo)
    }

    pub fn ollama_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Ollama)
    }

    pub fn opencode_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::OpenCode)
    }

    pub fn opencodego_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::OpenCodeGo)
    }

    pub fn deepseek_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::DeepSeek)
    }

    pub fn synthetic_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::Synthetic)
    }

    pub fn openrouter_handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.provider_handles(ProviderKind::OpenRouter)
    }

    pub fn cookie_handles(&self, family: &str) -> Result<Vec<CredentialHandle>, HandlesError> {
        let mut handles = Vec::new();
        for kind in providers_for_id(family) {
            for handle in self.provider_handles(kind)? {
                if !handles.contains(&handle) {
                    handles.push(handle);
                }
            }
        }
        Ok(handles)
    }

    fn provider_handles(
        &self,
        provider: ProviderKind,
    ) -> Result<Vec<CredentialHandle>, HandlesError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match state.phase {
            // An error, not an empty list: the scheduler keeps a provider whose
            // enumeration failed as unfinished and publishes nothing new for it,
            // whereas an empty list would let it fall back to a local lane and
            // replace every labelled vault account with one unlabelled row.
            VaultHandleState::Awaiting => Err(HandlesError::new(
                "credential vault has not answered an enumeration yet",
            )),
            // No vault serves this module here, so there are no vault handles
            // whatever the mapping holds. Only reachable before any
            // authoritative install, when the mapping is empty anyway; stated
            // so the state means the same thing however it was reached.
            VaultHandleState::NoVault => Ok(Vec::new()),
            VaultHandleState::Answered => Ok(state.mapped.for_provider(provider).to_vec()),
        }
    }
}

/// Which credential lanes a cookie provider should expose from the installed snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieLane {
    VaultOnly(Vec<CredentialHandle>),
    LocalWithFallback(Option<CredentialHandle>),
}

pub fn cookie_lane(handles: Vec<CredentialHandle>, family: &str) -> CookieLane {
    let suffixed: Vec<_> = handles
        .iter()
        .filter(|handle| {
            handle
                .vault_credential_id()
                .is_some_and(|id| id != family && handle_id_names_family(id, family))
        })
        .cloned()
        .collect();
    if !suffixed.is_empty() {
        return CookieLane::VaultOnly(suffixed);
    }
    CookieLane::LocalWithFallback(
        handles
            .into_iter()
            .find(|handle| handle.vault_credential_id().is_some_and(|id| id == family)),
    )
}

pub fn cookie_family_for(provider: &str) -> Option<&'static str> {
    CREDENTIAL_FAMILIES
        .iter()
        .find(|(prefix, name)| *name == provider && prefix.starts_with("cookie:"))
        .map(|(prefix, _)| *prefix)
}

fn provider_kind(name: &str) -> Option<ProviderKind> {
    match name {
        "codex" => Some(ProviderKind::Codex),
        "claude" => Some(ProviderKind::Anthropic),
        "grok" => Some(ProviderKind::Grok),
        "gemini" => Some(ProviderKind::Gemini),
        "antigravity" => Some(ProviderKind::Antigravity),
        "kimi-for-coding" => Some(ProviderKind::KimiForCoding),
        "amp" => Some(ProviderKind::Amp),
        "cursor" => Some(ProviderKind::Cursor),
        "qwen-cloud" => Some(ProviderKind::QwenCloud),
        "qoder" => Some(ProviderKind::Qoder),
        "factory" => Some(ProviderKind::Factory),
        "mimo" => Some(ProviderKind::Mimo),
        "ollama" => Some(ProviderKind::Ollama),
        "opencode" => Some(ProviderKind::OpenCode),
        "opencodego" => Some(ProviderKind::OpenCodeGo),
        "deepseek" => Some(ProviderKind::DeepSeek),
        "synthetic" => Some(ProviderKind::Synthetic),
        "openrouter" => Some(ProviderKind::OpenRouter),
        _ => None,
    }
}

fn providers_for_id(id: &str) -> Vec<ProviderKind> {
    CREDENTIAL_FAMILIES
        .iter()
        .filter(|(prefix, _)| handle_id_names_family(id, prefix))
        .filter_map(|(_, provider)| provider_kind(provider))
        .collect()
}

pub fn handle_id_names_family(id: &str, prefix: &str) -> bool {
    id == prefix
        || id
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(':'))
}

fn map_handles(rows: &[ScopedRowState]) -> (ProviderHandleSnapshot, Option<String>) {
    let mut refused_families: Vec<(&str, Vec<&str>)> = CREDENTIAL_FAMILIES
        .iter()
        .filter(|(prefix, _)| prefix.starts_with("cookie:") || prefix.starts_with("apikey:"))
        .filter_map(|(prefix, _)| {
            let ids: Vec<_> = rows
                .iter()
                .filter(|row| handle_id_names_family(&row.credential_id, prefix))
                .map(|row| row.credential_id.as_str())
                .collect();
            (ids.len() > 1).then_some((*prefix, ids))
        })
        .collect();
    refused_families.dedup_by_key(|(family, _)| *family);
    let refused_ids: HashSet<&str> = refused_families
        .iter()
        .flat_map(|(_, ids)| ids.iter().copied())
        .collect();

    let cursor_oauth_present = rows
        .iter()
        .any(|row| handle_id_names_family(&row.credential_id, "oauth:cursor"));
    let mut unsupported = Vec::new();
    let mut ignored_cursor_cookie = Vec::new();
    let mut mapped = ProviderHandleSnapshot::default();
    for row in rows {
        if refused_ids.contains(row.credential_id.as_str()) {
            continue;
        }
        if cursor_oauth_present && handle_id_names_family(&row.credential_id, "cookie:cursor.com") {
            ignored_cursor_cookie.push(row.credential_id.clone());
            continue;
        }
        let providers = providers_for_id(&row.credential_id);
        if providers.is_empty() {
            unsupported.push(row.credential_id.clone());
            continue;
        }
        for provider in providers {
            mapped.push(
                provider,
                CredentialHandle::scoped(&row.credential_id, &row.credential_type),
            );
        }
    }

    let mut warnings = Vec::new();
    for (family, mut ids) in refused_families {
        ids.sort_unstable();
        warnings.push(format!(
            "multiple identity-less credentials name `{family}` [{}]",
            ids.join(",")
        ));
    }
    if !ignored_cursor_cookie.is_empty() {
        ignored_cursor_cookie.sort();
        warnings.push(format!(
            "ignored cursor cookie deposits because oauth:cursor is present [{}]",
            ignored_cursor_cookie.join(",")
        ));
    }
    if !unsupported.is_empty() {
        unsupported.sort();
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

    fn row(id: &str, credential_type: &str) -> ScopedRowState {
        ScopedRowState {
            credential_id: id.to_string(),
            credential_type: credential_type.to_string(),
            record_version: 1,
            state: "active".to_string(),
            account_id: None,
        }
    }

    fn install(loader: &VaultHandleLoader, rows: Vec<ScopedRowState>) -> SnapshotInstall {
        loader.install_snapshot(ScopedSnapshot { grants: 1, rows }, Instant::now())
    }

    #[test]
    fn distinct_scoped_ids_are_not_capability_deduplicated() {
        let loader = VaultHandleLoader::default();
        install(
            &loader,
            vec![
                row("oauth:anthropic", "oauth"),
                row("oauth:anthropic:second", "oauth"),
            ],
        );
        let handles = loader.anthropic_handles().unwrap();
        assert_eq!(handles.len(), 2);
        assert!(handles.iter().all(CredentialHandle::is_vault));
        assert!(!loader
            .warning()
            .is_some_and(|warning| warning.contains("deduplicated identical capabilities")));
    }

    #[test]
    fn two_cookie_deposits_darken_only_that_family_and_surface_health_warning() {
        let loader = VaultHandleLoader::default();
        install(
            &loader,
            vec![
                row("cookie:ampcode.com", "cookie"),
                row("cookie:ampcode.com:second", "cookie"),
                row("oauth:xai", "oauth"),
            ],
        );
        assert!(loader.amp_handles().unwrap().is_empty());
        assert_eq!(loader.grok_handles().unwrap().len(), 1);
        assert!(loader
            .warning()
            .is_some_and(|warning| warning.contains("multiple identity-less credentials")));
    }

    #[test]
    fn two_apikey_deposits_are_refused_but_two_oauth_ids_are_not() {
        let loader = VaultHandleLoader::default();
        install(
            &loader,
            vec![
                row("apikey:deepseek", "apikey"),
                row("apikey:deepseek:other", "apikey"),
                row("oauth:anthropic", "oauth"),
                row("oauth:anthropic:other", "oauth"),
            ],
        );
        assert!(loader.deepseek_handles().unwrap().is_empty());
        assert_eq!(loader.anthropic_handles().unwrap().len(), 2);
    }

    /// `apikey:opencode` routes to opencodego alone, and like every identity-less
    /// family a second deposit darkens it rather than racing the first.
    #[test]
    fn the_opencode_api_key_routes_to_opencodego_and_a_second_is_refused() {
        let loader = VaultHandleLoader::default();
        install(&loader, vec![row("apikey:opencode", "apikey")]);
        assert_eq!(
            loader.opencodego_handles().unwrap(),
            vec![CredentialHandle::scoped("apikey:opencode", "apikey")]
        );
        assert!(
            loader.opencode_handles().unwrap().is_empty(),
            "the Zen balance provider has no API-key lane"
        );

        let loader = VaultHandleLoader::default();
        install(
            &loader,
            vec![
                row("apikey:opencode", "apikey"),
                row("apikey:opencode:second", "apikey"),
            ],
        );
        assert!(loader.opencodego_handles().unwrap().is_empty());
        assert!(loader.warning().is_some_and(|warning| {
            warning.contains("multiple identity-less credentials name `apikey:opencode`")
        }));
    }

    #[test]
    fn oauth_cursor_wins_over_cookie_cursor_without_becoming_a_cookie_family() {
        let loader = VaultHandleLoader::default();
        install(
            &loader,
            vec![
                row("oauth:cursor", "oauth"),
                row("cookie:cursor.com", "cookie"),
            ],
        );
        let handles = loader.cursor_handles().unwrap();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].vault_credential_id(), Some("oauth:cursor"));
        assert_eq!(cookie_family_for("cursor"), Some("cookie:cursor.com"));
        assert!(loader
            .warning()
            .is_some_and(|warning| warning.contains("ignored cursor cookie")));
    }

    #[test]
    fn provider_reads_are_repeatable_and_cannot_clear_the_snapshot() {
        let loader = VaultHandleLoader::default();
        install(&loader, vec![row("oauth:xai", "oauth")]);
        assert_eq!(
            loader.grok_handles().unwrap(),
            loader.grok_handles().unwrap()
        );
        assert_eq!(loader.installed_credential_ids(), vec!["oauth:xai"]);
    }

    #[test]
    fn the_ten_legacy_file_ids_all_route_after_scoped_cutover() {
        let expected = [
            ("antigravity:google", "antigravity"),
            ("chatgpt:openai", "codex"),
            ("chatgpt:openai:gmail", "codex"),
            ("kimi-for-coding", "kimi-for-coding"),
            ("oauth:anthropic", "claude"),
            ("oauth:anthropic:ufuk2", "claude"),
            ("oauth:anthropic:umutaday", "claude"),
            ("oauth:anthropic:wwaxgmail", "claude"),
            ("oauth:anthropic:yiyi", "claude"),
            ("oauth:xai", "grok"),
        ];
        for (id, provider) in expected {
            assert!(
                CREDENTIAL_FAMILIES.iter().any(|(prefix, name)| {
                    *name == provider && handle_id_names_family(id, prefix)
                }),
                "{id} did not route to {provider}"
            );
        }
    }

    #[test]
    fn zero_grants_retains_the_previous_snapshot() {
        let loader = VaultHandleLoader::default();
        install(&loader, vec![row("oauth:xai", "oauth")]);
        let result = loader.install_snapshot(
            ScopedSnapshot {
                grants: 0,
                rows: Vec::new(),
            },
            Instant::now(),
        );
        assert!(!result.authoritative);
        assert_eq!(loader.grok_handles().unwrap().len(), 1);
        assert_eq!(
            loader.enumeration_failure().as_deref(),
            Some("principal is not granted")
        );
    }

    fn zero_grants(loader: &VaultHandleLoader) -> SnapshotInstall {
        loader.install_snapshot(
            ScopedSnapshot {
                grants: 0,
                rows: Vec::new(),
            },
            Instant::now(),
        )
    }

    fn awaiting() -> VaultHandleLoader {
        let loader = VaultHandleLoader::default();
        loader.await_first_answer();
        loader
    }

    #[test]
    fn a_loader_nobody_awaits_reads_as_no_vault() {
        let loader = VaultHandleLoader::default();
        assert_eq!(loader.handle_state(), VaultHandleState::NoVault);
        assert_eq!(loader.anthropic_handles().unwrap(), Vec::new());
        assert_eq!(
            loader.cookie_handles("cookie:opencode.ai").unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn an_unanswered_vault_is_an_enumeration_error_not_an_empty_inventory() {
        let loader = awaiting();
        assert_eq!(loader.handle_state(), VaultHandleState::Awaiting);
        assert!(loader.anthropic_handles().is_err());
        assert!(loader.cookie_handles("cookie:opencode.ai").is_err());
    }

    #[test]
    fn an_authoritative_install_answers_an_awaiting_loader() {
        let loader = awaiting();
        install(&loader, vec![row("oauth:anthropic:first", "oauth")]);
        assert_eq!(loader.handle_state(), VaultHandleState::Answered);
        assert_eq!(loader.anthropic_handles().unwrap().len(), 1);
    }

    #[test]
    fn zero_grants_before_any_answer_means_no_vault() {
        let loader = awaiting();
        assert!(!zero_grants(&loader).authoritative);
        assert_eq!(loader.handle_state(), VaultHandleState::NoVault);
        assert_eq!(loader.anthropic_handles().unwrap(), Vec::new());
    }

    #[test]
    fn an_unregistered_vault_before_any_answer_means_no_vault() {
        let loader = awaiting();
        loader.mark_unavailable("Unavailable");
        assert_eq!(loader.handle_state(), VaultHandleState::NoVault);
        assert_eq!(loader.anthropic_handles().unwrap(), Vec::new());
        assert_eq!(loader.enumeration_failure().as_deref(), Some("Unavailable"));
    }

    #[test]
    fn a_transient_failure_keeps_an_unanswered_loader_waiting() {
        let loader = awaiting();
        loader.retain_after_failure("Transient");
        assert_eq!(loader.handle_state(), VaultHandleState::Awaiting);
        assert!(loader.anthropic_handles().is_err());
    }

    #[test]
    fn a_warm_vault_going_away_keeps_its_mapped_handles() {
        let loader = awaiting();
        install(&loader, vec![row("oauth:anthropic:first", "oauth")]);
        loader.mark_unavailable("Unavailable");
        assert_eq!(loader.handle_state(), VaultHandleState::Answered);
        assert_eq!(
            loader.anthropic_handles().unwrap(),
            vec![CredentialHandle::scoped("oauth:anthropic:first", "oauth")]
        );
    }

    #[test]
    fn zero_grants_after_an_answer_keeps_its_mapped_handles() {
        let loader = awaiting();
        install(&loader, vec![row("oauth:anthropic:first", "oauth")]);
        assert!(!zero_grants(&loader).authoritative);
        assert_eq!(loader.handle_state(), VaultHandleState::Answered);
        assert_eq!(loader.anthropic_handles().unwrap().len(), 1);
    }

    #[test]
    fn awaiting_an_answered_loader_does_not_blank_it() {
        let loader = VaultHandleLoader::default();
        install(&loader, vec![row("oauth:anthropic:first", "oauth")]);
        loader.await_first_answer();
        assert_eq!(loader.handle_state(), VaultHandleState::Answered);
        assert_eq!(loader.anthropic_handles().unwrap().len(), 1);
    }
}
