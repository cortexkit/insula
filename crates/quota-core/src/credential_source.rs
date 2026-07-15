//! Subc-free seam for resolving credentials from the external vault.
//!
//! Capability values and payloads are bearer secrets. Their formatting is
//! deliberately redacted, and vault errors are fixed classes with no upstream
//! text so provider degradation can never put secret material on the usage wire.

use async_trait::async_trait;

/// An owned snapshot of one opaque vault capability.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VaultCapability(String);

impl VaultCapability {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Expose the capability only to the transport that must put it on the wire.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for VaultCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("VaultCapability(<redacted>)")
    }
}

/// One credential result served by the vault.
#[derive(Clone, PartialEq, Eq)]
pub struct VaultCredential {
    pub payload: Vec<u8>,
    pub expires_at_ms: Option<i64>,
    pub record_version: u64,
    pub account_id: Option<String>,
    pub project_id: Option<String>,
}

impl std::fmt::Debug for VaultCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VaultCredential")
            .field("payload", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("record_version", &self.record_version)
            .field("account_id", &self.account_id)
            .field("project_id", &self.project_id)
            .finish()
    }
}

impl Drop for VaultCredential {
    fn drop(&mut self) {
        self.payload.fill(0);
    }
}

/// Secret-free behavior classes returned by a credential lookup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VaultGetError {
    Transient,
    AuthRequired,
    Permanent,
    FailClosed,
}

impl std::fmt::Debug for VaultGetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Transient => "Transient",
            Self::AuthRequired => "AuthRequired",
            Self::Permanent => "Permanent",
            Self::FailClosed => "FailClosed",
        })
    }
}

impl std::fmt::Display for VaultGetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Transient => "credential vault temporarily unavailable",
            Self::AuthRequired => "credential requires authentication",
            Self::Permanent => "credential is unavailable",
            Self::FailClosed => "credential vault rejected the request",
        })
    }
}

impl std::error::Error for VaultGetError {}

/// Vault access supplied by the subc-aware module crate.
#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn get(
        &self,
        capability: &VaultCapability,
        min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError>;

    /// CAS-guarded report for the exact record version served to this attempt.
    async fn report_auth_failure(
        &self,
        capability: &VaultCapability,
        provider_status: u16,
        record_version: u64,
    );
}
