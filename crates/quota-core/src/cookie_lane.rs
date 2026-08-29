//! Vault cookie lanes for the browser-cookie provider cohort.
//!
//! Nine providers publish quota only on a logged-in web page, and read the
//! session cookie out of the live Chrome store. That route is CLOSED ON WINDOWS
//! BY DESIGN -- Chrome 127+ App-Bound Encryption hands the cookie key only to
//! Chrome, validating the calling executable -- and unavailable on headless hosts
//! with no browser at all. The vault cookie lane is how those hosts get quota:
//! a human inside Chrome copies the header, which is the only trusted path left.
//!
//! THIS LIVES IN ONE PLACE ON PURPOSE. The precedence rule below was specified
//! wrongly once and corrected in 1a17528; had it been copied into nine provider
//! files first, the correction would have been nine edits with nine chances to
//! leave one behind, and a provider left on the old rule would still fetch and
//! still publish -- silently serving the wrong account rather than failing.

use std::sync::Arc;

use crate::{
    credential_source::{CredentialSource, VaultCapability},
    provider::{CredentialHandle, FetchError, HandlesError},
    vault_handles::{cookie_lane, CookieLane, VaultHandleLoader},
};

/// The vault half of one cookie provider's credential story.
///
/// Providers hold one of these and delegate two decisions to it: which lanes to
/// enumerate, and which cookie a given handle should fetch with. Everything else
/// -- the endpoints, the parse, the window shapes -- stays in the provider.
#[derive(Clone)]
pub(crate) struct CookieVaultLane {
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
    /// The bare credential id for this provider's domain, e.g.
    /// `cookie:ollama.com`. Suffixed deposits under it name an account.
    family: &'static str,
}

impl CookieVaultLane {
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

    /// A lane with no vault behind it, for `Default`/`new()` construction paths
    /// and for tests that only exercise the local browser store.
    pub(crate) fn local_only(family: &'static str) -> Self {
        Self::new(None, Arc::new(VaultHandleLoader::from_env()), family)
    }

    /// Which lanes this provider should expose.
    ///
    /// PRECEDENCE IS EXPRESSED BY WHICH LANES EXIST, not by a choice made during
    /// the fetch, and that distinction is the whole design. Every handle a
    /// provider returns becomes its own SLOT and is fetched independently, so
    /// enumerating a local lane beside a vault lane does not mean "prefer one" --
    /// it means BOTH fetch, both produce identity-less entries (a cookie session
    /// discloses no account), and the emission gate collapses them to a single
    /// representative chosen by a tie-break no operator can see or influence.
    /// `anthropic.rs` states the same consequence at its own `handles()`.
    ///
    /// So:
    ///   - an ACCOUNT-SUFFIXED deposit takes the provider vault-only. The
    ///     operator named an account; the ambient browser session must not be
    ///     able to answer instead of the one they named.
    ///   - otherwise the local lane is the only lane, with a bare deposit
    ///     consulted as a fallback INSIDE that one fetch (see [`Self::cookie_for`]).
    ///
    /// One slot in both cases, so there is no tie-break to lose.
    pub(crate) fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError> {
        if self.credential_source.is_none() {
            return Ok(vec![CredentialHandle::implicit()]);
        }
        Ok(
            match cookie_lane(self.handle_loader.provider_handles_for(self.family)?, self.family) {
                CookieLane::VaultOnly(handles) => handles,
                CookieLane::LocalWithFallback(_) => vec![CredentialHandle::implicit()],
            },
        )
    }

    /// The cookie header to fetch with, and the `source` label to publish.
    ///
    /// `local` is the provider's own live-store read, passed as a closure because
    /// each provider names its own domain and some do more than one lookup. It is
    /// only invoked on the local lane, so a vault-only provider never touches the
    /// browser store.
    ///
    /// THE FALLBACK IS INSIDE THIS ONE FETCH rather than a second slot: a host
    /// that cannot read the live store at all would otherwise have no lane, while
    /// a host that can keeps the fresher source. The local handle is
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
            return Ok((self.fetch_vault_cookie(capability).await?, "vault"));
        }
        match local().await {
            Ok(cookie) => Ok((cookie, crate::browser_cookies::SOURCE_LABEL)),
            Err(local_error) => match self.bare_deposit()? {
                Some(capability) => Ok((self.fetch_vault_cookie(&capability).await?, "vault")),
                // No fallback configured: report what the live store said rather
                // than inventing a credential-absent verdict. The local failure is
                // the true one and already carries the right class.
                None => Err(local_error),
            },
        }
    }

    fn bare_deposit(&self) -> Result<Option<VaultCapability>, FetchError> {
        if self.credential_source.is_none() {
            return Ok(None);
        }
        let handles = self
            .handle_loader
            .provider_handles_for(self.family)
            .map_err(|error| FetchError::Internal(error.to_string()))?;
        Ok(match cookie_lane(handles, self.family) {
            CookieLane::LocalWithFallback(Some(handle)) => handle.vault_capability().cloned(),
            _ => None,
        })
    }

    async fn fetch_vault_cookie(&self, capability: &VaultCapability) -> Result<String, FetchError> {
        let source = self.credential_source.as_ref().ok_or_else(|| {
            FetchError::NoSession("no credential source configured".to_string())
        })?;
        // 120s matches the other vault providers; a bare literal here would be a
        // second answer to a question kimi_for_coding already answers.
        let mut credential = source
            .get(capability, 120_000)
            .await
            .map_err(|error| FetchError::Upstream(error.to_string()))?;
        crate::credential_source::take_utf8_payload(&mut credential.payload)
    }
}
