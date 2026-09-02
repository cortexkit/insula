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

/// Name the place a jar came from, for an operator's next action.
///
/// SHARED SO THE NINE PROVIDERS CANNOT DISAGREE, and because the wrong answer is
/// worse than a vague one. These providers all reported "no session cookie in
/// browser" from a point AFTER the jar was resolved -- true when the browser
/// store was the only lane, and false the moment a deposit answers instead. It
/// sends the operator to check a browser session that is not what failed.
///
/// It is worst on exactly the hosts the deposit lane exists for. Windows cannot
/// read Chrome's cookie store at all (App-Bound Encryption hands the key only to
/// chrome.exe), so "in browser" there names a lane that cannot work, about a
/// credential the operator pasted in by hand.
pub(crate) fn source_phrase(source: &str) -> &'static str {
    match source {
        "vault" => "in the deposited cookie",
        _ => "in browser",
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

    /// The cookie JAR to fetch with, and the `source` label to publish.
    ///
    /// Same precedence as [`Self::cookie_for`]; the difference is shape. Seven
    /// of the nine cookie providers work from a jar rather than a header string,
    /// because they ask it whether a recognised session cookie is present and
    /// give a different diagnosis when it is not.
    ///
    /// THAT DIAGNOSIS IS WHY THE VAULT LANE RETURNS A JAR RATHER THAN A STRING.
    /// A pasted header full of tracking cookies and no session is well-formed to
    /// the vault, deposits cleanly, and fails on first use. "Your session
    /// expired, sign in again" sends the operator to repeat the action that just
    /// failed; "no session was captured, make sure you are signed in before
    /// copying" sends them to the actual cause. Handing back an opaque string
    /// would lose that distinction for exactly the hosts this lane exists for.
    pub(crate) async fn jar_for<F, Fut>(
        &self,
        handle: &CredentialHandle,
        local: F,
    ) -> Result<(crate::browser_cookies::CookieJar, &'static str), FetchError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<crate::browser_cookies::CookieJar, FetchError>>,
    {
        if let Some(capability) = handle.vault_capability() {
            let header = self.fetch(capability).await?;
            return Ok((
                crate::browser_cookies::CookieJar::from_header(&header),
                "vault",
            ));
        }
        match local().await {
            Ok(jar) => Ok((jar, crate::browser_cookies::SOURCE_LABEL)),
            Err(local_error) => match self.bare_deposit()? {
                Some(capability) => {
                    let header = self.fetch(&capability).await?;
                    Ok((
                        crate::browser_cookies::CookieJar::from_header(&header),
                        "vault",
                    ))
                }
                None => Err(local_error),
            },
        }
    }

    /// Name the place a jar came from, for an operator's next action.
    ///
    /// SHARED SO THE NINE PROVIDERS CANNOT DISAGREE, and because the wrong answer
    /// is worse than a vague one. These providers all reported "no session cookie
    /// in browser" from a point AFTER the jar was resolved -- true when the
    /// browser store was the only lane, and false the moment a deposit answers
    /// instead. It sends the operator to check a browser session that is not what
    /// failed.
    ///
    /// It is worst on exactly the hosts the deposit lane exists for. Windows
    /// cannot read Chrome's cookie store at all (App-Bound Encryption hands the
    /// key only to chrome.exe), so "in browser" there names a lane that cannot
    /// work, about a credential the operator pasted in by hand.
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

    /// The absence diagnosis names the lane that actually answered.
    ///
    /// These providers report "no session cookie ..." from a point AFTER the jar
    /// is resolved, so the phrase has to follow the lane. Saying "in browser"
    /// about a deposited cookie sends an operator to re-check a browser session
    /// that is not what failed -- and on Windows it names a lane that CANNOT
    /// work, since App-Bound Encryption hands Chrome's cookie key only to
    /// chrome.exe. The wrong word lands hardest on the platform the deposit lane
    /// exists for.
    ///
    /// Pinned because a revert here is otherwise silent: nothing parses this
    /// message, so no other test in the suite would notice it going wrong.
    #[test]
    fn the_absence_message_names_the_lane_that_answered() {
        assert_eq!(source_phrase("vault"), "in the deposited cookie");
        assert_eq!(source_phrase(SOURCE_LABEL), "in browser");
    }

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
