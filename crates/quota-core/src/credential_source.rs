//! Subc-free seam for resolving credentials from the external vault.
//!
//! Capability values and payloads are bearer secrets. Their formatting is
//! deliberately redacted, and vault errors are fixed classes with no upstream
//! text so provider degradation can never put secret material on the usage wire.

use async_trait::async_trait;

use crate::model::AccountInfo;

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
    pub email: Option<String>,
    pub org_name: Option<String>,
}

impl VaultCredential {
    /// Convert optional vault labels into the public account metadata shape.
    pub fn account_info(&self) -> Option<AccountInfo> {
        let info = AccountInfo {
            email: canonical_label(self.email.clone()),
            org_name: canonical_label(self.org_name.clone()),
            plan_type: None,
        };
        (!info.is_empty()).then_some(info)
    }
}

fn canonical_label(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
            .field("email", &self.email)
            .field("org_name", &self.org_name)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability is the bearer of vault authority: anything holding it can
    /// fetch the credential. Its `Debug` is the redaction site, and it is
    /// reached by every `{:?}` of a type that merely *contains* a capability,
    /// so a leak here escapes through diagnostics that never mention secrets.
    #[test]
    fn a_capability_never_appears_in_its_own_debug() {
        let capability = VaultCapability::new("ckh_capability_secret");
        let debug = format!("{capability:?}");

        assert!(!debug.contains("ckh_capability_secret"));
        // Not vacuous: the value really is retrievable, so the assertion above
        // is about redaction rather than about an empty capability.
        assert_eq!(capability.expose_secret(), "ckh_capability_secret");
        // And the debug output is present rather than blank, so a formatter
        // that wrote nothing at all could not pass this.
        assert!(debug.contains("VaultCapability"));
        assert!(debug.contains("redacted"));
    }

    /// The payload is the credential itself. It is redacted at this type rather
    /// than at each caller, so this test guards every lane that formats one.
    #[test]
    fn a_credential_payload_never_appears_in_its_own_debug() {
        let credential = VaultCredential {
            payload: b"vault-payload-secret".to_vec(),
            expires_at_ms: Some(1_800_000),
            record_version: 7,
            account_id: Some("acct-1".to_string()),
            project_id: None,
            email: None,
            org_name: None,
        };
        let debug = format!("{credential:?}");

        assert!(!debug.contains("vault-payload-secret"));
        // Non-vacuity: the payload is really carried, and the fields that are
        // safe to print still are -- so this cannot pass by printing nothing.
        assert_eq!(credential.payload, b"vault-payload-secret");
        assert!(debug.contains("redacted"));
        assert!(debug.contains("acct-1"));
        assert!(debug.contains('7'));
    }

    /// The error classes are deliberately fixed and secret-free: upstream text
    /// never rides them, so a diagnostic printing an error cannot leak whatever
    /// the vault said.
    #[test]
    fn error_debug_is_a_fixed_class_tag() {
        for (error, expected) in [
            (VaultGetError::Transient, "Transient"),
            (VaultGetError::AuthRequired, "AuthRequired"),
            (VaultGetError::Permanent, "Permanent"),
            (VaultGetError::FailClosed, "FailClosed"),
        ] {
            assert_eq!(format!("{error:?}"), expected);
        }
    }
}
