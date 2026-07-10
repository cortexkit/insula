//! The provider fetcher abstraction.
//!
//! Each provider exposes credential handles as independent fetch units. The
//! background refresher owns labeling and serving entries, while providers only
//! resolve a credential, observe its account identity, and normalize its usage.

use async_trait::async_trait;

use crate::model::{ProviderUsage, Usage};

/// Stable identity for one credential fetch unit.
///
/// A handle is deliberately not an account identity: a credential can be
/// replaced behind the same handle. The account label is observed separately on
/// every fetch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CredentialHandle(String);

impl CredentialHandle {
    /// Build a provider-defined stable handle.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The single local credential source used by providers that do not yet
    /// enumerate multiple credentials.
    pub fn implicit() -> Self {
        Self("implicit-local".to_string())
    }

    /// Stable provider-local identifier used for deterministic response order.
    pub fn stable_id(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CredentialHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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

/// One handle-scoped fetch result.
///
/// The credential observation is independent of `usage`, so an account swap is
/// still visible when the upstream usage request fails.
#[derive(Debug)]
pub struct FetchAttempt {
    pub observed: Option<AccountObservation>,
    pub source: Option<String>,
    pub usage: Result<Usage, FetchError>,
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
        }
    }

    /// Adapt an existing single-credential provider body while preserving its
    /// normalization and error mapping. The refresher still rebuilds the served
    /// entry from this envelope; only Codex currently has an observable account
    /// identity and therefore uses the explicit constructors above.
    pub fn from_provider_usage(result: Result<ProviderUsage, FetchError>) -> Self {
        match result {
            Ok(entry) => {
                let observed = entry
                    .account
                    .map(|account_id| AccountObservation::new(Some(account_id), None));
                let source = entry.source;
                let usage = entry.usage.ok_or_else(|| {
                    FetchError::Decode("provider returned a healthy entry without usage".into())
                });
                Self {
                    observed,
                    source,
                    usage,
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
            Self::Upstream(m) => write!(f, "upstream error: {m}"),
            Self::Decode(m) => write!(f, "decode error: {m}"),
        }
    }
}

impl std::error::Error for FetchError {}
