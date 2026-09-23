//! Subc-free seam for resolving credentials from the external vault.
//!
//! Capability values and payloads are bearer secrets. Their formatting is
//! deliberately redacted, and vault errors are fixed classes with no upstream
//! text so provider degradation can never put secret material on the usage wire.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::model::AccountInfo;
use crate::provider::CredentialHandle;

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
    /// The credential module is not registered on this daemon at all.
    ///
    /// A fact about the HOST rather than about any credential: the daemon
    /// answered that no module by that id exists (`unknown_module`) or that it
    /// was removed from the configuration (`module_removed`). This is what every
    /// host without a credential vault receives, which is the ordinary state of
    /// a fresh install -- so on a list it means "there is no vault here, use the
    /// local lanes", not "try again".
    ///
    /// Separate from [`Self::Transient`] because the two lead to opposite
    /// enumeration verdicts. A transient failure says the vault may answer next
    /// turn, so a process that has never heard from it must wait rather than
    /// read the silence as an empty inventory. This one says no vault will
    /// answer, and waiting would keep every local lane dark forever.
    ///
    /// For a per-credential fetch it is still retried like a transient
    /// condition: a vault that disappears mid-life should stale-serve the last
    /// window, not condemn the credential.
    Unavailable,
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
            Self::Unavailable => "Unavailable",
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
            Self::Unavailable => "credential vault is not registered on this daemon",
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

/// One secret-free credential row visible through the caller's scoped grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedRowState {
    pub credential_id: String,
    pub credential_type: String,
    pub record_version: u64,
    pub state: String,
}

/// One complete scoped-enumeration result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedSnapshot {
    pub grants: u64,
    pub rows: Vec<ScopedRowState>,
}

/// Vault access supplied by the subc-aware module crate.
#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn get(
        &self,
        capability: &VaultCapability,
        min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError>;

    /// Resolve a credential by id under the caller's scoped grant.
    async fn get_scoped(
        &self,
        _credential_id: &str,
        _min_ttl_ms: u64,
    ) -> Result<VaultCredential, VaultGetError> {
        Err(VaultGetError::FailClosed)
    }

    /// Enumerate the credentials visible through the caller's scoped grants.
    async fn list_scoped(&self) -> Result<ScopedSnapshot, VaultGetError> {
        Err(VaultGetError::FailClosed)
    }

    /// CAS-guarded report for the exact capability-addressed record version served.
    async fn report_auth_failure(
        &self,
        capability: &VaultCapability,
        provider_status: u16,
        record_version: u64,
    );

    /// CAS-guarded report for the exact scoped record version served.
    async fn report_auth_failure_scoped(
        &self,
        _credential_id: &str,
        _provider_status: u16,
        _record_version: u64,
    ) {
    }

    /// Handle resolution plus plaintext metadata. No decrypt, no refresh, no audit.
    async fn status(
        &self,
        _capability: &VaultCapability,
    ) -> Result<CredentialStatus, VaultGetError> {
        Err(VaultGetError::FailClosed)
    }

    /// Scoped status lookup mirrors [`Self::status`]. An error means only that
    /// the lookup could not answer, not that the credential is invalid.
    async fn status_scoped(&self, _credential_id: &str) -> Result<CredentialStatus, VaultGetError> {
        Err(VaultGetError::FailClosed)
    }
}

/// Fetch through the address actually carried by a vault handle.
pub async fn get_vault_credential(
    source: &Arc<dyn CredentialSource>,
    handle: &CredentialHandle,
    min_ttl_ms: u64,
) -> Result<VaultCredential, VaultGetError> {
    match handle {
        CredentialHandle::Vault { credential_id, .. } => {
            source.get_scoped(credential_id, min_ttl_ms).await
        }
        CredentialHandle::LegacyVault { capability, .. } => {
            source.get(capability, min_ttl_ms).await
        }
        CredentialHandle::ImplicitLocal | CredentialHandle::Named(_) => {
            Err(VaultGetError::FailClosed)
        }
    }
}
/// Auth failures this process has reported to the vault.
///
/// THE MOST DESTRUCTIVE THING THIS MODULE DOES, and until now the least observed.
/// A report latches the credential record: the vault marks it `needs_reauth`, and
/// nothing clears that but a human logging in. Every other outbound effect here is
/// a read.
///
/// It had no counter while transport niceties had four (connection drops, route
/// warming retries, unmatched drops, stale generation drops). So when an account
/// went dark overnight there was no way to answer the first question an operator
/// asks -- did WE do this, or did the credential die on its own -- and the report
/// is fire-and-forget, so nothing else records it either.
///
/// Deliberately a bare count rather than a per-credential map: the capability is a
/// bearer secret and the credential id is not in scope at the gate. A count
/// separates "this module latched something" from "this module has never reported
/// anything", which is the distinction that decides where to look next.
static AUTH_FAILURES_REPORTED: AtomicU64 = AtomicU64::new(0);

/// How many auth failures this process has reported to the vault.
pub fn auth_failures_reported() -> u64 {
    AUTH_FAILURES_REPORTED.load(Ordering::Relaxed)
}

#[doc(hidden)]
pub enum VaultAuthFailureAddress {
    Capability(VaultCapability),
    Scoped(String),
}

/// An address accepted by the shared auth-failure reporting gate.
#[doc(hidden)]
pub trait VaultAuthFailureTarget {
    fn auth_failure_address(&self) -> Option<VaultAuthFailureAddress>;
}

impl VaultAuthFailureTarget for VaultCapability {
    fn auth_failure_address(&self) -> Option<VaultAuthFailureAddress> {
        Some(VaultAuthFailureAddress::Capability(self.clone()))
    }
}

impl VaultAuthFailureTarget for CredentialHandle {
    fn auth_failure_address(&self) -> Option<VaultAuthFailureAddress> {
        if !self.is_vault() {
            return None;
        }
        match self {
            CredentialHandle::Vault { credential_id, .. } => {
                Some(VaultAuthFailureAddress::Scoped(credential_id.clone()))
            }
            CredentialHandle::LegacyVault { capability, .. } => {
                Some(VaultAuthFailureAddress::Capability(capability.clone()))
            }
            CredentialHandle::ImplicitLocal | CredentialHandle::Named(_) => None,
        }
    }
}

/// Report a rejected vault credential to the store that issued it.
///
/// Only a provider 401 is evidence that the served credential itself was
/// rejected. A 403 can describe entitlement instead, so reporting it would
/// invalidate a credential that may still be healthy.
pub fn report_vault_auth_failure<T: VaultAuthFailureTarget + ?Sized>(
    source: Option<&Arc<dyn CredentialSource>>,
    target: &T,
    record_version: u64,
    error: &crate::provider::FetchError,
) {
    let crate::provider::FetchError::ProviderStatus(status @ 401, _) = error else {
        return;
    };
    let Some(source) = source else {
        return;
    };
    let Some(address) = target.auth_failure_address() else {
        return;
    };
    let source = Arc::clone(source);
    let status = *status;
    AUTH_FAILURES_REPORTED.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        match address {
            VaultAuthFailureAddress::Capability(capability) => {
                source
                    .report_auth_failure(&capability, status, record_version)
                    .await;
            }
            VaultAuthFailureAddress::Scoped(credential_id) => {
                source
                    .report_auth_failure_scoped(&credential_id, status, record_version)
                    .await;
            }
        }
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

    /// Serialises the tests that move the process-wide report counter.
    ///
    /// The counter is deliberately process-wide -- the gate is a free function with
    /// nothing to hang state off -- and cargo runs these tests in parallel threads
    /// of ONE process. Without this, a sibling test's 401 lands between another
    /// test's read and its assertion, and the failure is a rare flake that reads
    /// like a real defect in the gate.
    ///
    /// One lock in one module, covering every caller here. Two locks would be worse
    /// than none: the pair would look like protection while guarding nothing
    /// against each other.
    /// A tokio mutex rather than a std one: these tests await, and holding a std
    /// guard across an await point blocks the executor thread rather than the
    /// task -- correct here only by luck, and a lint that would be silenced
    /// rather than fixed.
    static COUNTER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The counter moves with the decision, and only with it.
    ///
    /// The report is fire-and-forget, so nothing downstream records that a
    /// credential was latched. This counter is the only trace, and the reading that
    /// matters is the ZERO -- when an account is found dark, it rules this module
    /// out as the cause.
    ///
    /// The 403 arm is the load-bearing half. A refused call is not a dead
    /// credential: an entitlement withdrawal, a suspension and a network challenge
    /// all arrive as 403, and none is fixed by logging in again. A counter that
    /// moved on those would report this module as the cause of latches it never
    /// performed, which is worse than no counter -- it would send an operator to
    /// the wrong repository with evidence in hand.
    #[tokio::test]
    async fn the_counter_moves_only_when_a_credential_is_actually_latched() {
        let _serial = COUNTER_LOCK.lock().await;
        let source = RecordingSource::default();
        let source: Arc<dyn CredentialSource> = Arc::new(source);
        let capability = VaultCapability::new("ckh_counter");

        let before = auth_failures_reported();

        // A 403 must not move it, however many times it arrives.
        for _ in 0..3 {
            report_vault_auth_failure(
                Some(&source),
                &capability,
                1,
                &crate::provider::FetchError::ProviderStatus(403, String::new()),
            );
        }
        // Nor an error that never reaches the gate at all.
        report_vault_auth_failure(
            Some(&source),
            &capability,
            1,
            &crate::provider::FetchError::Upstream("timeout".into()),
        );
        assert_eq!(
            auth_failures_reported(),
            before,
            "only a latch may move this counter, and none of those latched anything"
        );

        // A 401 with a served bearer does.
        report_vault_auth_failure(
            Some(&source),
            &capability,
            1,
            &crate::provider::FetchError::ProviderStatus(401, String::new()),
        );
        assert_eq!(
            auth_failures_reported(),
            before + 1,
            "a latch must leave a trace, because nothing else in the process does"
        );
    }

    /// A 401 from the usage endpoint is reported: it is the death proxy.
    ///
    /// This is the arm that detected revocation-on-rotation all month, and for a
    /// static API-key record it is the ONLY automatic invalidation trigger in
    /// the system -- nothing else ever marks such a record dead.
    #[tokio::test]
    async fn a_401_is_reported_as_credential_death() {
        let _serial = COUNTER_LOCK.lock().await;
        let source = RecordingSource::default();
        let reports = source.reports.clone();
        let source: Arc<dyn CredentialSource> = Arc::new(source);

        report_vault_auth_failure(
            Some(&source),
            &VaultCapability::new("ckh_test"),
            7,
            &crate::provider::FetchError::ProviderStatus(401, String::new()),
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
        let _serial = COUNTER_LOCK.lock().await;
        let source = RecordingSource::default();
        let reports = source.reports.clone();
        let source: Arc<dyn CredentialSource> = Arc::new(source);

        report_vault_auth_failure(
            Some(&source),
            &VaultCapability::new("ckh_test"),
            7,
            &crate::provider::FetchError::ProviderStatus(403, String::new()),
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
            &crate::provider::FetchError::ProviderStatus(500, String::new()),
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(reports.lock().unwrap().is_empty());
    }

    /// A credential store that records what it was told.
    #[derive(Default)]
    struct RecordingSource {
        reports: Arc<std::sync::Mutex<Vec<(u16, u64)>>>,
        scoped_reports: Arc<std::sync::Mutex<Vec<(String, u16, u64)>>>,
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

        async fn report_auth_failure_scoped(
            &self,
            credential_id: &str,
            status: u16,
            record_version: u64,
        ) {
            self.scoped_reports.lock().unwrap().push((
                credential_id.to_string(),
                status,
                record_version,
            ));
        }
    }

    #[tokio::test]
    async fn scoped_401_reports_id_and_served_version_but_other_errors_do_not() {
        let source = RecordingSource::default();
        let reports = Arc::clone(&source.scoped_reports);
        let source: Arc<dyn CredentialSource> = Arc::new(source);
        let handle = CredentialHandle::scoped("oauth:anthropic:test", "oauth");
        for error in [
            crate::provider::FetchError::ProviderStatus(401, String::new()),
            crate::provider::FetchError::ProviderStatus(403, String::new()),
            crate::provider::FetchError::Upstream("timeout".to_string()),
        ] {
            report_vault_auth_failure(Some(&source), &handle, 41, &error);
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(
            reports.lock().unwrap().as_slice(),
            &[("oauth:anthropic:test".to_string(), 401, 41)]
        );
    }

    #[tokio::test]
    async fn a_non_vault_source_defaults_scoped_listing_to_fail_closed() {
        let source = RecordingSource::default();
        assert_eq!(source.list_scoped().await, Err(VaultGetError::FailClosed));
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
