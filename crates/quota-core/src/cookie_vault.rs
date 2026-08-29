//! Vault cookie lanes for the browser-cookie provider cohort.
//!
//! Nine providers publish quota only on a logged-in web page and read the
//! session cookie from the live Chrome store. That route is CLOSED ON WINDOWS BY
//! DESIGN -- Chrome 127+ App-Bound Encryption hands the cookie key only to
//! Chrome, validating the calling executable -- and absent on headless hosts.
//! The vault lane is how those hosts get quota at all: a human inside Chrome
//! copies the header, which is the only trusted path left.
//!
//! THIS EXISTS BECAUSE THE DUPLICATION COST WAS MEASURED, NOT PREDICTED. The
//! precedence rule below was specified wrongly, corrected in 1a17528, and the
//! correction was applied to `opencode` and MISSED `opencodego` -- in the same
//! session, by the person who had just written it. Two copies were enough to
//! lose a fix; nine would be a rule that is right in some providers and wrong in
//! others, with nothing failing to say which.

use std::sync::Arc;

use crate::{
    credential_source::{CredentialSource, VaultCapability},
    provider::{CredentialHandle, FetchError, HandlesError},
    vault_handles::{cookie_lane, CookieLane, VaultHandleLoader},
};

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

    /// Which lanes this provider exposes.
    ///
    /// PRECEDENCE IS EXPRESSED BY WHICH LANES EXIST, not by a choice made during
    /// the fetch, and that distinction is the whole design. Every handle a
    /// provider returns becomes its own SLOT and is fetched independently, so
    /// enumerating a local lane beside a vault lane does not mean "prefer one" --
    /// both fetch, both produce identity-less entries (a cookie session
    /// discloses no account), and the emission gate collapses them to a single
    /// representative chosen by a tie-break no operator can see. `anthropic.rs`
    /// states the same consequence at its own `handles()`.
    ///
    /// - An ACCOUNT-SUFFIXED deposit takes the provider vault-only. The operator
    ///   named an account; the ambient browser session must not answer instead
    ///   of the one they named.
    /// - Otherwise the local lane is the only lane, with a bare deposit
    ///   consulted as a fallback INSIDE that one fetch (see [`Self::cookie_for`]).
    ///
    /// One slot either way, so there is no tie-break to lose.
    ///
    /// The asymmetry that makes suffixed-wins correct: a stale deposit FAILS
    /// LOUDLY (401, marked, prompt to re-capture) while a wrong account
    /// SUCCEEDS, reporting a real current figure for somebody else's quota.
    pub(crate) fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_none() {
            return Ok(vec![CredentialHandle::implicit()]);
        }
        Ok(match cookie_lane(self.deposits()?, self.family) {
            CookieLane::VaultOnly(handles) => handles,
            CookieLane::LocalWithFallback(_) => vec![CredentialHandle::implicit()],
        })
    }

    /// The cookie header to fetch with, and the `source` label to publish.
    ///
    /// `local` is the provider's own live-store read, passed as a closure because
    /// each provider names its own domain. It is only invoked on the local lane,
    /// so a vault-only provider never touches the browser store.
    ///
    /// THE FALLBACK IS INSIDE THIS ONE FETCH rather than a second slot: a host
    /// that cannot read the live store at all would otherwise have no lane,
    /// while a host that can keeps the fresher source. The local handle is
    /// `ImplicitLocal` and carries no capability, so which bare deposit to fall
    /// back to is a property of the PROVIDER's configuration, not of the handle.
    pub(crate) async fn cookie_for<F, Fut>(
        &self,
        handle: &CredentialHandle,
        local: F,
    ) -> Result<(String, &'static str), FetchError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<String, FetchError>>,
    {
        if let Some(capability) = handle.vault_capability() {
            return Ok((self.fetch(capability).await?, "vault"));
        }
        match local().await {
            Ok(cookie) => Ok((cookie, crate::browser_cookies::SOURCE_LABEL)),
            Err(local_error) => match self.bare_deposit()? {
                Some(capability) => Ok((self.fetch(&capability).await?, "vault")),
                // No fallback configured: report what the live store said. The
                // local failure is the true one and already carries the right
                // class -- inventing a credential-absent verdict here would
                // replace a specific diagnosis with a vaguer one.
                None => Err(local_error),
            },
        }
    }

    fn deposits(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        self.handle_loader.cookie_handles(self.family)
    }

    fn bare_deposit(&self) -> Result<Option<VaultCapability>, FetchError> {
        if self.credential_source.is_none() {
            return Ok(None);
        }
        let deposits = self
            .deposits()
            .map_err(|error| FetchError::Internal(error.to_string()))?;
        Ok(match cookie_lane(deposits, self.family) {
            CookieLane::LocalWithFallback(Some(handle)) => handle.vault_capability().cloned(),
            _ => None,
        })
    }

    async fn fetch(&self, capability: &VaultCapability) -> Result<String, FetchError> {
        let source = self
            .credential_source
            .as_ref()
            .ok_or_else(|| FetchError::NoSession("no credential source configured".to_string()))?;
        // 120s matches the other vault providers; a bare literal here would be a
        // second answer to a question `kimi_for_coding` already answers.
        let mut credential = source
            .get(capability, 120_000)
            .await
            .map_err(|error| FetchError::Upstream(error.to_string()))?;
        crate::credential_source::take_utf8_payload(&mut credential.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_cookies::SOURCE_LABEL;

    /// The shared lane publishes the cohort's cookie label on the local branch.
    ///
    /// Load-bearing because the per-provider source-walk in `tests.rs` now
    /// accepts delegation to this type INSTEAD of a literal in the provider
    /// file. That widening is only sound if the delegate actually produces the
    /// label, so this is the other half of that check -- without it, a cohort
    /// that all delegate would satisfy the walk while publishing nothing.
    #[tokio::test]
    async fn the_local_branch_publishes_the_cookie_label() {
        let vault = CookieVault::new(None, Arc::new(VaultHandleLoader::new(None)), "cookie:test");
        let (cookie, source) = vault
            .cookie_for(&CredentialHandle::implicit(), || async {
                Ok("session=abc".to_string())
            })
            .await
            .expect("the local branch answers");
        assert_eq!(cookie, "session=abc");
        assert_eq!(
            source, SOURCE_LABEL,
            "a cookie fetched from the live browser store must publish the cookie label"
        );
    }

    /// A local failure with no bare deposit reports the LOCAL error.
    ///
    /// Not a credential-absent verdict invented here: the live-store failure
    /// already carries the right class, and replacing it would trade a specific
    /// diagnosis for a vaguer one at the moment a reader most needs the specific
    /// one.
    #[tokio::test]
    async fn a_local_failure_without_a_deposit_reports_the_local_error() {
        let vault = CookieVault::new(None, Arc::new(VaultHandleLoader::new(None)), "cookie:test");
        let error = vault
            .cookie_for(&CredentialHandle::implicit(), || async {
                Err(FetchError::Unauthorized("signed out".to_string()))
            })
            .await
            .expect_err("no lane can answer");
        assert!(
            matches!(error, FetchError::Unauthorized(ref message) if message == "signed out"),
            "the live-store failure must survive, got {error:?}"
        );
    }
}
