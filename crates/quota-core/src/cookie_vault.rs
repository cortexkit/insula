//! Vault cookie lanes for the cookie provider cohort.
//!
//! Nine providers publish quota only on a logged-in web page, so their
//! credential is a session cookie. Insula never reads a browser to get one: the
//! cookie arrives as a deposit in the credential vault, `cookie:<domain>` or
//! `cookie:<domain>:<account>`, captured by Cerebellum in a throwaway browser.
//! A deposit is the only cookie source. A host with no deposit has no cookie
//! lane, and the provider is unconfigured there.
//!
//! THIS EXISTS BECAUSE THE DUPLICATION COST WAS MEASURED, NOT PREDICTED. The
//! precedence rule below was once specified wrongly, corrected, and the
//! correction was applied to `opencode` and MISSED `opencodego` -- in the same
//! session, by the person who had just written it. Two copies were enough to
//! lose a fix; nine would be a rule that is right in some providers and wrong in
//! others, with nothing failing to say which.

use std::sync::Arc;

use crate::{
    cookie_jar::CookieJar,
    credential_source::CredentialSource,
    provider::{CredentialHandle, FetchError, HandlesError},
    vault_handles::{cookie_lane, CookieLane, VaultHandleLoader},
};

/// The `source` every cookie lane publishes: the cookie came from a vault
/// deposit, because no other cookie source exists.
pub(crate) const SOURCE: &str = "vault";

/// Where a cookie provider's jar came from, for a "no session cookie ..."
/// diagnosis.
///
/// SHARED SO THE NINE PROVIDERS CANNOT DISAGREE. These providers report a
/// missing session from a point AFTER the jar was resolved, and the operator's
/// next action is to re-capture the login and re-deposit it, so the message
/// names the deposit rather than a browser.
pub(crate) const DEPOSIT_PHRASE: &str = "in the deposited cookie";

/// The vault half of one cookie provider's credential story.
///
/// A provider holds one of these and delegates two decisions: which lanes to
/// enumerate, and which cookie a given handle fetches with. Endpoints, parsing
/// and window shapes stay in the provider.
#[derive(Clone)]
pub(crate) struct CookieVault {
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    /// The bare credential id for this provider's domain, e.g.
    /// `cookie:ollama.com`. Deposits suffixed under it name an account.
    family: &'static str,
}

/// The precedence rule of [`CookieVault::handles`] over a list of deposits.
///
/// Separate from the loader because the loader already refuses a domain with
/// two deposits outright (cookies carry no identity, so two deposits are
/// ambiguous). This rule is the second line behind that refusal, and it has to
/// be testable on its own input.
fn deposit_lanes(deposits: Vec<CredentialHandle>, family: &str) -> Vec<CredentialHandle> {
    match cookie_lane(deposits, family) {
        CookieLane::Suffixed(handles) => handles,
        CookieLane::Bare(bare) => bare.into_iter().collect(),
    }
}

impl CookieVault {
    pub(crate) fn new(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
        family: &'static str,
    ) -> Self {
        Self {
            credential_source,
            handle_loader,
            family,
        }
    }

    /// Which deposits this provider fetches with.
    ///
    /// PRECEDENCE IS EXPRESSED BY WHICH LANES EXIST, not by a choice made during
    /// the fetch. Every handle a provider returns becomes its own SLOT and is
    /// fetched independently, so enumerating a bare deposit beside a suffixed
    /// one does not mean "prefer one" -- both fetch, both produce identity-less
    /// entries (a cookie session discloses no account), and the emission gate
    /// collapses them to a single representative chosen by a tie-break no
    /// operator can see. `anthropic.rs` states the same consequence at its own
    /// `handles()`.
    ///
    /// - ACCOUNT-SUFFIXED deposits win. The operator named an account; an
    ///   unnamed deposit must not answer instead of the one they named.
    /// - Otherwise a bare `cookie:<domain>` deposit is the one lane.
    /// - No deposit, or no credential source at all, is NO lane. The provider
    ///   is unconfigured on this host and counts as having no handles, rather
    ///   than publishing a degraded "credential absent" entry for a login
    ///   nobody deposited.
    ///
    /// The asymmetry that makes suffixed-wins correct: a stale deposit FAILS
    /// LOUDLY (401, marked, prompt to re-capture) while a wrong account
    /// SUCCEEDS, reporting a real current figure for somebody else's quota.
    ///
    /// A vault that has not answered yet is an enumeration ERROR, not an empty
    /// inventory, so the scheduler keeps the provider unfinished instead of
    /// forgetting the accounts it was serving.
    pub(crate) fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_none() {
            return Ok(Vec::new());
        }
        Ok(deposit_lanes(self.deposits()?, self.family))
    }

    /// The cookie header to fetch with, and the `source` label to publish.
    pub(crate) async fn cookie_for(
        &self,
        handle: &CredentialHandle,
    ) -> Result<(String, &'static str), FetchError> {
        Ok((self.fetch(handle).await?, SOURCE))
    }

    /// The cookie JAR to fetch with, and the `source` label to publish.
    ///
    /// Same lane as [`Self::cookie_for`]; the difference is shape. Seven of the
    /// nine cookie providers work from a jar rather than a header string,
    /// because they ask it whether a recognised session cookie is present and
    /// give a different diagnosis when it is not.
    ///
    /// THAT DIAGNOSIS IS WHY THIS RETURNS A JAR RATHER THAN A STRING. A captured
    /// header full of tracking cookies and no session is well-formed to the
    /// vault, deposits cleanly, and fails on first use. "Your session expired,
    /// sign in again" sends the operator to repeat the action that just failed;
    /// "no session was captured, make sure you are signed in before capturing"
    /// sends them to the actual cause.
    pub(crate) async fn jar_for(
        &self,
        handle: &CredentialHandle,
    ) -> Result<(CookieJar, &'static str), FetchError> {
        let header = self.fetch(handle).await?;
        Ok((CookieJar::from_header(&header), SOURCE))
    }

    fn deposits(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.handle_loader.cookie_handles(self.family)
    }

    async fn fetch(&self, handle: &CredentialHandle) -> Result<String, FetchError> {
        // Only a deposit carries a cookie. `handles()` never enumerates any
        // other kind, so this is a caller's mistake reported as absence rather
        // than a vault failure the provider did not have.
        if !handle.is_vault() {
            return Err(FetchError::NoSession(format!(
                "no {} deposit for this handle",
                self.family
            )));
        }
        let source = self
            .credential_source
            .as_ref()
            .ok_or_else(|| FetchError::NoSession("no credential source configured".to_string()))?;
        let mut credential = crate::credential_source::get_vault_credential(
            source,
            handle,
            crate::credential_source::VAULT_READ_MIN_TTL_MS,
        )
        .await
        .map_err(|error| FetchError::Upstream(error.to_string()))?;
        crate::credential_source::take_utf8_payload(&mut credential.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_source::{VaultCapability, VaultCredential, VaultGetError};
    use async_trait::async_trait;

    const FAMILY: &str = "cookie:ollama.com";

    /// Serves `session=<credential id>` for any scoped id, so a test can tell
    /// which deposit a fetch actually read.
    struct EchoSource;

    #[async_trait]
    impl CredentialSource for EchoSource {
        async fn get(
            &self,
            _capability: &VaultCapability,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            Err(VaultGetError::FailClosed)
        }

        async fn get_scoped(
            &self,
            credential_id: &str,
            _min_ttl_ms: u64,
        ) -> Result<VaultCredential, VaultGetError> {
            Ok(VaultCredential {
                payload: format!("session={credential_id}").into_bytes(),
                expires_at_ms: None,
                record_version: 1,
                account_id: None,
                email: None,
                org_name: None,
                project_id: None,
            })
        }

        async fn report_auth_failure(
            &self,
            _capability: &VaultCapability,
            _provider_status: u16,
            _record_version: u64,
        ) {
        }
    }

    fn vault_with(rows: &[(&str, &str)]) -> CookieVault {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(rows);
        CookieVault::new(Some(Arc::new(EchoSource)), loader, FAMILY)
    }

    fn ids(handles: &[CredentialHandle]) -> Vec<&str> {
        handles
            .iter()
            .map(|handle| handle.vault_credential_id().unwrap_or("<not a deposit>"))
            .collect()
    }

    /// No deposit is no lane: the provider is unconfigured here.
    ///
    /// Not an implicit handle that fetches and fails. That would publish a
    /// degraded "credential absent" entry on every host for nine providers
    /// nobody set up, and there is no other cookie source it could ever read.
    /// The vault has answered with another provider's row, so the empty result
    /// comes from the rule, not from an empty snapshot.
    #[test]
    fn no_deposit_enumerates_no_handle() {
        let vault = vault_with(&[("cookie:ampcode.com", "cookie")]);
        let handles = vault.handles().expect("an answered vault enumerates");
        assert!(
            handles.is_empty(),
            "no deposit must mean no lane, got {handles:?}"
        );
    }

    /// No credential source is no lane either, for the same reason.
    #[test]
    fn no_credential_source_enumerates_no_handle() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.install_rows_for_test(&[(FAMILY, "cookie")]);
        let vault = CookieVault::new(None, loader, FAMILY);
        let handles = vault.handles().expect("enumeration needs no I/O");
        assert!(
            handles.is_empty(),
            "nothing can fetch a deposit without a source, got {handles:?}"
        );
    }

    /// A bare deposit alone is served, as one vault handle.
    #[test]
    fn a_bare_deposit_alone_is_one_vault_handle() {
        let vault = vault_with(&[(FAMILY, "cookie")]);
        let handles = vault.handles().expect("an answered vault enumerates");
        assert_eq!(ids(&handles), vec![FAMILY]);
        assert!(handles[0].is_vault());
    }

    /// An account-suffixed deposit outranks a bare one, which is not enumerated.
    ///
    /// Both as separate slots would fetch twice and collapse to one row by a
    /// tie-break nobody can see; the operator named an account, so that is the
    /// one that answers.
    #[test]
    fn an_account_suffixed_deposit_outranks_a_bare_one() {
        let bare = CredentialHandle::scoped(FAMILY, "cookie");
        let named = CredentialHandle::scoped("cookie:ollama.com:ufuk", "cookie");
        // Both orders, so the rule cannot pass by taking whichever came first.
        for deposits in [
            vec![bare.clone(), named.clone()],
            vec![named.clone(), bare.clone()],
        ] {
            let lanes = deposit_lanes(deposits, FAMILY);
            assert_eq!(ids(&lanes), vec!["cookie:ollama.com:ufuk"]);
        }
        // Another domain's deposit is never a lane here.
        let other = CredentialHandle::scoped("cookie:ollama.community", "cookie");
        assert!(deposit_lanes(vec![other], FAMILY).is_empty());
    }

    /// A vault that has not answered yet is an error, not "no deposit".
    ///
    /// Reading it as empty would unconfigure every cookie provider on a cold
    /// start and forget the accounts the next answer would have served.
    #[test]
    fn an_unanswered_vault_is_an_enumeration_error() {
        let loader = Arc::new(VaultHandleLoader::default());
        loader.await_first_answer();
        let vault = CookieVault::new(Some(Arc::new(EchoSource)), loader, FAMILY);
        assert!(vault.handles().is_err());
    }

    /// The jar comes from the deposit the handle names, labelled `vault`.
    #[tokio::test]
    async fn the_jar_is_the_deposit_and_publishes_the_vault_label() {
        let vault = vault_with(&[("cookie:ollama.com:ufuk", "cookie")]);
        let handle = vault.handles().unwrap().remove(0);
        let (jar, source) = vault.jar_for(&handle).await.expect("the deposit answers");
        assert_eq!(jar.header(), "session=cookie:ollama.com:ufuk");
        assert_eq!(source, "vault");
        let (header, source) = vault
            .cookie_for(&handle)
            .await
            .expect("the deposit answers");
        assert_eq!(header, "session=cookie:ollama.com:ufuk");
        assert_eq!(source, "vault");
    }

    /// A handle that is not a deposit reads nothing and reports absence.
    #[tokio::test]
    async fn a_non_deposit_handle_reads_nothing() {
        let vault = vault_with(&[(FAMILY, "cookie")]);
        let error = vault
            .cookie_for(&CredentialHandle::implicit())
            .await
            .expect_err("only a deposit carries a cookie");
        assert!(matches!(error, FetchError::NoSession(_)), "{error:?}");
    }
}
