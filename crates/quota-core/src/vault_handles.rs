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

#[derive(Default)]
struct LoaderState {
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
        Ok(state.mapped.for_provider(provider).to_vec())
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
}
