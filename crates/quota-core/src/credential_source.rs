//! Subc-free seam for resolving credentials from the external vault.
//!
//! Capability values and payloads are bearer secrets. Their formatting is
//! deliberately redacted, and vault errors are fixed classes with no upstream
//! text so provider degradation can never put secret material on the usage wire.

use std::sync::Arc;

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
    /// No credential exists for this handle at all.
    ///
    /// Distinct from [`Self::Permanent`], which is the general "no retry will
    /// help" answer, because this one says something sharper: the handle names
    /// nothing. It is what a handle becomes when the credential behind it is
    /// removed and the handle is left configured -- a state no login fixes,
    /// because there is no account to log in to.
    ///
    /// The vault produces it only on a clean zero-row lookup; any FAILURE to
    /// read the store maps to a transient class instead, and a vault that is
    /// down answers nothing at all. So this cannot appear during an outage,
    /// which is what makes it safe to act on rather than merely report.
    NotFound,
    /// The lookup succeeded and carried no credential bytes.
    ///
    /// Separate from [`Self::Permanent`], which means no record exists, and from
    /// [`Self::FailClosed`], which means the reply could not be understood. This
    /// reply was understood and reported success while carrying nothing, so the
    /// remedies differ: an absent credential is a configuration gap the operator
    /// closes by logging in, whereas an empty one means something wrote a value
    /// that should never have been writable, and the record itself is evidence
    /// of that. Folding it into either neighbour discards that evidence.
    EmptyPayload,
    /// A record exists and the vault refuses to serve it, having found it
    /// corrupt or quarantined it.
    ///
    /// Retried like [`Self::Permanent`] -- neither clears without someone acting
    /// -- but reported separately, because the actions are opposites: an absent
    /// credential is created by logging in, while this one already exists and
    /// something damaged it. Reporting it as absent would send an operator to
    /// re-authenticate an account whose record is the evidence of a fault.
    Corrupt,
    FailClosed,
}

impl std::fmt::Debug for VaultGetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Transient => "Transient",
            Self::AuthRequired => "AuthRequired",
            Self::Permanent => "Permanent",
            Self::NotFound => "NotFound",
            Self::EmptyPayload => "EmptyPayload",
            Self::Corrupt => "Corrupt",
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
            Self::NotFound => "no credential exists for this handle",
            Self::EmptyPayload => "credential vault served an empty credential",
            Self::Corrupt => "credential vault holds a corrupt or quarantined record",
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

/// Report a rejected vault credential to the store that issued it, if anyone is
/// listening.
///
/// WHY THIS IS SHARED RATHER THAN A METHOD ON EACH PROVIDER. Four vault lanes
/// carried byte-identical copies of this, and the gate inside it is subtle
/// enough that a fifth copy would be written wrong: it matches ONLY
/// `ProviderStatus(401 | 403)`, an upstream status that survives to this point
/// solely because the vault lane uses a send which PRESERVES it. The local lane
/// maps 401 to `Unauthorized`, correctly, because a local credential has no
/// custodian to tell.
///
/// So the gate depends on a transport decision made in another file, and
/// deleting that decision once left every test green while silently ending all
/// auth reporting. That is not a thing to re-derive per provider.
///
/// WHAT IT IS FOR, since the fire-and-forget shape hides the stakes: a static
/// API-key record in the credential store has no refresh adapter and no intent,
/// so nothing else ever marks it dead. This call is its only automatic
/// invalidation trigger. Dropping it means an operator is never prompted to
/// re-authenticate, and the lane simply fails on a fixed backoff forever.
///
/// Spawned rather than awaited on purpose: a usage fetch must not block on the
/// credential store's availability. The result is deliberately discarded --
/// nothing here can act on a failed report, and the next fetch reports again.
pub fn report_vault_auth_failure(
    source: Option<&Arc<dyn CredentialSource>>,
    capability: &VaultCapability,
    record_version: u64,
    error: &crate::provider::FetchError,
) {
    let crate::provider::FetchError::ProviderStatus(status @ (401 | 403)) = error else {
        return;
    };
    let Some(source) = source else {
        return;
    };
    let source = Arc::clone(source);
    let capability = capability.clone();
    let status = *status;
    tokio::spawn(async move {
        source
            .report_auth_failure(&capability, status, record_version)
            .await;
    });
}

/// Take a vault payload as a UTF-8 string, scrubbing it if it is not one.
///
/// The scrub is the reason this is shared. On the error path the bytes are a
/// credential that failed to decode, and `String::from_utf8` hands them back
/// inside the error -- so without an explicit fill they are dropped unzeroed.
/// Four lanes each remembered to do it; a fifth would look correct without it,
/// because the only difference is memory nobody inspects.
///
/// Takes the payload out of its source, so the caller cannot keep using a buffer
/// this may have zeroed.
pub fn take_utf8_payload(payload: &mut Vec<u8>) -> Result<String, crate::provider::FetchError> {
    match String::from_utf8(std::mem::take(payload)) {
        Ok(text) => Ok(text),
        Err(error) => {
            let mut bytes = error.into_bytes();
            bytes.fill(0);
            Err(crate::provider::FetchError::Decode(
                "vault credential payload is not valid UTF-8".to_string(),
            ))
        }
    }
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
            (VaultGetError::EmptyPayload, "EmptyPayload"),
            (VaultGetError::FailClosed, "FailClosed"),
        ] {
            assert_eq!(format!("{error:?}"), expected);
        }
    }
}
