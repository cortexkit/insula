//! The provider fetcher abstraction.
//!
//! Each provider exposes credential handles as independent fetch units. The
//! background refresher owns labeling and serving entries, while providers only
//! resolve a credential, observe its account identity, and normalize its usage.

use async_trait::async_trait;

use crate::credential_source::{VaultCapability, VaultGetError};
use crate::model::{AccountInfo, ProviderUsage, SavedResets, Usage};

/// Stable identity for one credential fetch unit.
///
/// A vault handle includes the exact capability snapshot used by the fetch. The
/// capability participates in equality and hashing, while all formatting exposes
/// only the non-secret credential id.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum CredentialHandle {
    ImplicitLocal,
    Named(String),
    Vault {
        credential_id: String,
        capability: VaultCapability,
    },
}

impl CredentialHandle {
    /// Build a provider-defined stable local handle.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        if id.is_empty() || id == "implicit-local" {
            Self::ImplicitLocal
        } else {
            Self::Named(id)
        }
    }

    pub fn implicit() -> Self {
        Self::ImplicitLocal
    }

    pub fn vault(credential_id: impl Into<String>, capability: VaultCapability) -> Self {
        Self::Vault {
            credential_id: credential_id.into(),
            capability,
        }
    }

    pub fn stable_id(&self) -> &str {
        match self {
            Self::ImplicitLocal => "implicit-local",
            Self::Named(id) => id,
            Self::Vault { credential_id, .. } => credential_id,
        }
    }

    pub fn is_local(&self) -> bool {
        !matches!(self, Self::Vault { .. })
    }

    pub fn vault_capability(&self) -> Option<&VaultCapability> {
        match self {
            Self::Vault { capability, .. } => Some(capability),
            Self::ImplicitLocal | Self::Named(_) => None,
        }
    }

    pub fn sort_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.stable_id().cmp(other.stable_id())
    }
}

impl std::fmt::Debug for CredentialHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImplicitLocal => formatter.write_str("CredentialHandle::ImplicitLocal"),
            Self::Named(id) => formatter
                .debug_tuple("CredentialHandle::Named")
                .field(id)
                .finish(),
            Self::Vault { credential_id, .. } => formatter
                .debug_struct("CredentialHandle::Vault")
                .field("credential_id", credential_id)
                .field("capability", &"<redacted>")
                .finish(),
        }
    }
}

impl std::fmt::Display for CredentialHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.stable_id())
    }
}

/// A failed local handle-set read or parse.
///
/// This is distinct from an authoritative empty set so a transient config error
/// cannot accidentally reap every account for a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlesError(String);

impl HandlesError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for HandlesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HandlesError {}

/// Account identity observed while resolving one credential.
///
/// `record_version` is reserved for versioned credential stores. Local sources
/// leave it absent and re-resolve the observation on every fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountObservation {
    pub account_id: Option<String>,
    pub record_version: Option<u64>,
}

impl AccountObservation {
    pub fn new(account_id: Option<String>, record_version: Option<u64>) -> Self {
        Self {
            account_id,
            record_version,
        }
    }
}

/// Whether this tick verified the identity behind the credential handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialResolution {
    Verified,
    Unverified,
}

/// One handle-scoped fetch result.
///
/// The credential observation is independent of `usage`, so an account swap is
/// still visible when the upstream usage request fails.
#[derive(Debug)]
pub struct FetchAttempt {
    pub observed: Option<AccountObservation>,
    pub source: Option<String>,
    pub usage: Result<Usage, FetchError>,
    /// Optional provider/account labels attached to a successful usage fetch.
    pub account_info: Option<AccountInfo>,
    /// Optional read-only reset inventory attached to a successful usage fetch.
    pub saved_resets: Option<SavedResets>,
    /// The slot may relax raw percentages only while this success remains fresh.
    pub relax_eligible: bool,
    pub credential_resolution: CredentialResolution,
}

impl FetchAttempt {
    pub fn success(
        observed: Option<AccountObservation>,
        source: impl Into<String>,
        usage: Usage,
    ) -> Self {
        Self {
            observed,
            source: Some(source.into()),
            usage: Ok(usage),
            account_info: None,
            saved_resets: None,
            relax_eligible: false,
            credential_resolution: CredentialResolution::Verified,
        }
    }

    pub fn failure(
        observed: Option<AccountObservation>,
        source: Option<String>,
        error: FetchError,
    ) -> Self {
        Self {
            observed,
            source,
            usage: Err(error),
            account_info: None,
            saved_resets: None,
            relax_eligible: false,
            credential_resolution: CredentialResolution::Verified,
        }
    }

    pub fn unverified_vault_failure(error: VaultGetError) -> Self {
        let fetch_error = match error {
            VaultGetError::Transient => {
                FetchError::Upstream("credential vault temporarily unavailable".to_string())
            }
            VaultGetError::AuthRequired => {
                FetchError::NoSession("credential requires authentication".to_string())
            }
            VaultGetError::Permanent => {
                FetchError::NoSession("vault credential is unavailable".to_string())
            }
            VaultGetError::FailClosed => {
                FetchError::Decode("credential vault rejected the request".to_string())
            }
        };
        Self {
            observed: None,
            source: None,
            usage: Err(fetch_error),
            account_info: None,
            saved_resets: None,
            relax_eligible: false,
            credential_resolution: CredentialResolution::Unverified,
        }
    }

    /// Attach account labels discovered while resolving the credential.
    pub fn with_account_info(mut self, account_info: Option<AccountInfo>) -> Self {
        self.account_info = account_info.filter(|info| !info.is_empty());
        self
    }

    /// Attach a successful read-only reset inventory without changing mutation policy.
    pub fn with_saved_resets(mut self, saved_resets: Option<SavedResets>) -> Self {
        self.saved_resets = saved_resets;
        self
    }

    /// Set read-time relaxation eligibility without changing other providers'
    /// constructor call sites.
    pub fn with_relax_eligible(mut self, relax_eligible: bool) -> Self {
        self.relax_eligible = relax_eligible;
        self
    }

    /// Adapt an existing single-credential provider body while preserving its
    /// normalization and error mapping. The refresher still rebuilds the served
    /// entry from this envelope.
    pub fn from_provider_usage(result: Result<ProviderUsage, FetchError>) -> Self {
        match result {
            Ok(entry) => {
                // `Some(AccountObservation { account_id: None, .. })` means the
                // credential was resolved and explicitly exposes no account label.
                // Outer `None` is reserved for an unavailable observation.
                let observed = Some(AccountObservation::new(entry.account, None));
                let source = entry.source;
                let account_info = entry.account_info;
                let saved_resets = entry.saved_resets;
                let usage = entry.usage.ok_or_else(|| {
                    FetchError::Decode("provider returned a healthy entry without usage".into())
                });
                Self {
                    observed,
                    source,
                    usage,
                    account_info,
                    saved_resets,
                    relax_eligible: false,
                    credential_resolution: CredentialResolution::Verified,
                }
            }
            Err(error) => Self::failure(None, None, error),
        }
    }
}

/// A provider's handle enumeration and handle-scoped usage fetch.
#[async_trait]
pub trait UsageProvider: Send + Sync {
    /// Stable provider name (for example, `codex`).
    fn name(&self) -> &str;

    /// Read the provider's known credential handles from local configuration.
    /// This must never perform network I/O. Providers with one existing local
    /// credential source use the default implicit handle.
    fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        Ok(vec![CredentialHandle::implicit()])
    }

    /// Resolve and fetch one credential handle.
    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt;

    /// Whether this provider authenticates via a scraped local browser cookie.
    fn is_cookie_based(&self) -> bool {
        false
    }
}

/// Why a provider could not produce usage. Always foldable into a degraded
/// entry; never aborts the response array.
#[derive(Debug)]
pub enum FetchError {
    /// No usable session on disk (not logged in / file missing).
    NoSession(String),
    /// The session exists but is expired or rejected (401/403).
    Unauthorized(String),
    /// Numeric provider HTTP status retained for auth-failure reporting.
    ProviderStatus(u16),
    /// Transport or upstream error.
    Upstream(String),
    /// The response was not the shape we expected.
    Decode(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSession(m) => write!(f, "no session: {m}"),
            Self::Unauthorized(m) => write!(f, "unauthorized: {m}"),
            Self::ProviderStatus(status) => write!(f, "provider returned HTTP {status}"),
            Self::Upstream(m) => write!(f, "upstream error: {m}"),
            Self::Decode(m) => write!(f, "decode error: {m}"),
        }
    }
}

impl std::error::Error for FetchError {}
