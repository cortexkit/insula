//! The provider fetcher abstraction.
//!
//! Each AI provider reuses its OWN existing session (OAuth token, browser
//! cookie, API key, or local app file) to fetch usage and normalize it to the
//! uniform [`ProviderUsage`] shape. This trait is that per-provider unit of
//! work; the codex fetcher is the first implementation and the archetype for
//! the "oauth-bearer-from-local-file" group.

use async_trait::async_trait;

use crate::model::ProviderUsage;

/// A single provider's auth + fetch + normalize.
///
/// Implementations never panic and never propagate transport errors as the
/// array's failure: a fetch problem is returned as `Err(FetchError)` and the
/// caller folds it into a degraded [`ProviderUsage`] entry (silent-degrade).
#[async_trait]
pub trait UsageProvider: Send + Sync {
    /// CodexBar provider name (e.g. "codex"). Stable; Alfonso maps it to its id.
    fn name(&self) -> &str;

    /// Fetch and normalize this provider's usage from its real session.
    async fn fetch(&self) -> Result<ProviderUsage, FetchError>;
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
