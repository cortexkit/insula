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

/// Non-secret health of one vault record. Never carries credential bytes.
///
/// `record_version` is the change cursor: every import, replace, and refresh
/// commit bumps it, monotonically per record. `ready` is whether the record is
/// usable now, and is consulted only when neither side has a version to compare.
/// `stale_pending` is a latency predictor for the next get, not a health signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialStatus {
    pub ready: bool,
    pub record_version: Option<u64>,
    pub stale_pending: Option<bool>,
}

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

    /// Handle resolution plus plaintext metadata. No decrypt, no refresh, no audit.
    ///
    /// A source that has not grown this method cannot accelerate a backoff: the
    /// default refuses, and a refused status poll leaves the slot exactly as it
    /// was. Implementors that can answer must not let a failure here condemn a
    /// credential -- the poll is an accelerator, and an accelerator that can make
    /// things worse is a defect.
    async fn status(
        &self,
        _capability: &VaultCapability,
    ) -> Result<CredentialStatus, VaultGetError> {
        Err(VaultGetError::FailClosed)
    }
}

/// Report a rejected vault credential to the store that issued it, if anyone is
/// listening.
///
/// WHY THIS IS SHARED RATHER THAN A METHOD ON EACH PROVIDER. Seven vault lanes
/// carried copies of this, and the gate inside it is subtle enough that an
/// eighth would be written wrong: it matches ONLY `ProviderStatus(401)`, an
/// upstream status that survives to this point solely because the vault lane
/// uses a send which PRESERVES it. The local lane maps 401 to `Unauthorized`,
/// correctly, because a local credential has no custodian to tell.
///
/// 403 IS DELIBERATELY EXCLUDED, AND THAT IS THE LOAD-BEARING PART.
/// The vault's contract asks for reports when the credential is BELIEVED DEAD,
/// never merely because a call was refused — and a report is terminal there: it
/// latches the record and forecloses any later refresh, for every consumer, not
/// just this one. So the question is not "was I refused" but "do I believe this
/// credential is gone", and only one of the two statuses answers it.
///
/// Measured counterexample, on this host, 2026-08-21: gemini's Code Assist quota
/// endpoint returned 403 to a credential whose refresh SUCCEEDED moments before.
/// Google had withdrawn the entitlement, not the credential — the token renews
/// perfectly and is refused this one resource. Reporting that as death would
/// have killed a live Google credential for every consumer on the box, and
/// antigravity's vault lane rides the same API family.
///
/// A 401 with a served bearer remains a reasonable death proxy: it is how
/// revocation-on-rotation was correctly detected all month.
///
/// NOT gated on the response body instead. Distinguishing "invalid credential"
/// from "insufficient permission" that way means parsing seven providers' error
/// prose, which carries no stability promise from any of them — the same reason
/// this wire tells consumers never to branch on its own error strings.
///
/// The cost of being wrong runs one way. Withholding a report leaves the lane
/// failing visibly as `credential_rejected` on this wire, which is what an
/// operator sees anyway; sending a wrong one destroys a working credential
/// silently.
///
/// SIZED FROM THE CUSTODY SIDE, which this module cannot see from where it sits
/// (reported on insula#10, 2026-08-24, from the vault's append-only audit
/// chain). The `antigravity:google` record — the one a 403 arm would have
/// reported, since it rides the same `cloudcode-pa` family as the gemini lane
/// that produced the live counterexample — had accumulated hundreds of
/// `refresh_commit` entries and ZERO auth-failure reports, renewing continuously.
/// The exact count is deliberately not repeated here: it was stale before it was
/// written, and a number that keeps moving invites a reader to check it rather
/// than the ratio, which is the durable part.
///
/// A PREVIOUS VERSION OF THIS COMMENT SAID A REPORT LATCHES THE RECORD. It does
/// not, and the correction is worth keeping because of how the wrong version got
/// here: it was written from a reading of the deployed schema, and the migration
/// that changed the answer had landed twenty-four minutes earlier. Measured end
/// to end afterwards (insula#10, 2026-08-24) on a genuine 401:
///
///   report_auth_failure  ->  stale marker set, record stays ACTIVE
///   +300s, vault's own forced refresh  ->  invalid_grant  ->  needs_reauth
///
/// So the report does not kill the credential; the vault's next failed refresh
/// does. A wrong report costs a forced refresh and, if the credential is
/// genuinely healthy, that refresh SUCCEEDS and the record survives.
///
/// THE ASYMMETRY IS THEREFORE WEAKER THAN THE ARGUMENT THAT SHIPPED THE FIX, and
/// the fix is still right on the remaining margin. A wrong report on a live
/// credential still forces an unnecessary refresh against a rotation-sensitive
/// endpoint, and this module cannot see whether that costs anything on the
/// custody side. Withholding it costs a lane that fails visibly as
/// `credential_rejected`, which an operator sees anyway. The direction survives
/// even though the magnitude did not.
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
    let crate::provider::FetchError::ProviderStatus(status @ 401) = error else {
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

    /// A 401 from the usage endpoint is reported: it is the death proxy.
    ///
    /// This is the arm that detected revocation-on-rotation all month, and for a
    /// static API-key record it is the ONLY automatic invalidation trigger in
    /// the system -- nothing else ever marks such a record dead.
    #[tokio::test]
    async fn a_401_is_reported_as_credential_death() {
        let source = RecordingSource::default();
        let reports = source.reports.clone();
        let source: Arc<dyn CredentialSource> = Arc::new(source);

        report_vault_auth_failure(
            Some(&source),
            &VaultCapability::new("ckh_test"),
            7,
            &crate::provider::FetchError::ProviderStatus(401),
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            reports.lock().unwrap().as_slice(),
            &[(401u16, 7u64)],
            "a 401 with a served bearer must reach the credential store"
        );
    }

    /// A 403 is NOT reported, and this is the arm that costs money to get wrong.
    ///
    /// A report is terminal in the vault: it latches the record and forecloses
    /// any later refresh, for every consumer on the host. So it must mean "this
    /// credential is dead", not "this call was refused".
    ///
    /// MEASURED COUNTEREXAMPLE, this host, 2026-08-21. Gemini's Code Assist quota
    /// endpoint returned 403 to a credential whose refresh had just SUCCEEDED --
    /// Google withdrew the entitlement, not the credential. Reporting that as
    /// death would have destroyed a working Google credential for everyone, and
    /// antigravity's vault lane rides the same API family.
    #[tokio::test]
    async fn a_403_is_not_reported_because_it_can_mean_a_live_credential() {
        let source = RecordingSource::default();
        let reports = source.reports.clone();
        let source: Arc<dyn CredentialSource> = Arc::new(source);

        report_vault_auth_failure(
            Some(&source),
            &VaultCapability::new("ckh_test"),
            7,
            &crate::provider::FetchError::ProviderStatus(403),
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            reports.lock().unwrap().is_empty(),
            "a 403 can be an entitlement refusal against a live credential; \
             reporting it kills a working record for every consumer"
        );
    }

    /// Anything that is not a refusal at all is left alone.
    ///
    /// The control for both tests above. Without it, a gate that reported
    /// NOTHING would satisfy the 403 case and look correct.
    #[tokio::test]
    async fn an_ordinary_upstream_failure_is_not_a_credential_report() {
        let source = RecordingSource::default();
        let reports = source.reports.clone();
        let source: Arc<dyn CredentialSource> = Arc::new(source);

        report_vault_auth_failure(
            Some(&source),
            &VaultCapability::new("ckh_test"),
            7,
            &crate::provider::FetchError::ProviderStatus(500),
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(reports.lock().unwrap().is_empty());
    }

    /// A credential store that records what it was told.
    #[derive(Default)]
    struct RecordingSource {
        reports: Arc<std::sync::Mutex<Vec<(u16, u64)>>>,
    }

    #[async_trait::async_trait]
    impl CredentialSource for RecordingSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            Err(VaultGetError::Transient)
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            status: u16,
            record_version: u64,
        ) {
            self.reports.lock().unwrap().push((status, record_version));
        }
    }
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
