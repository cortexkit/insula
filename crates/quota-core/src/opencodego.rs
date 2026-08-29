//! OpenCode Go usage — same browser session as OpenCode, HTML `/go` page scrape.
//!
//! Reuses workspace id fetch and window parsing from [`crate::opencode`]. Zen prepaid
//! balance is intentionally omitted (no reset window).
//!
//! VERIFICATION: fixture-verified against CodexBar source, NOT live-verified.
//! Ported from `OpenCodeGo/OpenCodeGoUsageFetcher.swift` (:406-430 page fetch,
//! :432-519 parse with monthly). Cookie filter shared with
//! `OpenCode/OpenCodeWebCookieSupport.swift:4`.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;

use crate::{
    browser_cookies::SOURCE_LABEL,
    http::{Header, JsonRequest},
    model::ProviderUsage,
    opencode::{
        fetch_workspace_id, load_cookie_header_async, looks_signed_out, parse_windows, USER_AGENT,
    },
    provider::{FetchError, UsageProvider},
};
use crate::{
    credential_source::CredentialSource,
    provider::{CredentialHandle, FetchAttempt},
    vault_handles::VaultHandleLoader,
};

pub const PROVIDER_NAME: &str = "opencodego";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

async fn fetch_go_page_html(
    client: &reqwest::Client,
    cookie: &str,
    workspace_id: &str,
) -> Result<String, FetchError> {
    let url = format!("https://opencode.ai/workspace/{workspace_id}/go");
    let bytes = JsonRequest::get(&url)
        .timeout(REQUEST_TIMEOUT)
        .header(Header::new("Cookie", cookie.to_string()))
        .header(Header::new("User-Agent", USER_AGENT.to_string()))
        .header(Header::new(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".to_string(),
        ))
        .send(client)
        .await?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    classify_go_page(&text, chrono::Utc::now().timestamp())?;
    Ok(text)
}

/// Decide what a fetched `/go` page means, before anything tries to read
/// windows out of it.
///
/// Three outcomes share one symptom -- no windows -- and they need different
/// answers from whoever sees them, so the page is classified once here rather
/// than inferred from a parse failure downstream:
///
/// * signed out: the session is gone and a browser login fixes it.
/// * no Go plan: the login is fine and there is nothing to report, which is not
///   a failure at all.
/// * anything else that yields no windows: our parser and the page disagree,
///   which is a defect on this side.
///
/// The order is load-bearing. A signed-out page can carry an unsubscribed-looking
/// record, and a page with no plan does not parse, so each check must come before
/// the ones it would otherwise be mistaken for.
fn classify_go_page(text: &str, now_secs: i64) -> Result<(), FetchError> {
    if looks_signed_out(text) {
        return Err(FetchError::Unauthorized(
            "opencodego session expired (go page)".to_string(),
        ));
    }
    if looks_unsubscribed(text) {
        return Err(FetchError::NoQuotaReported(
            "opencodego: workspace has no Go subscription".to_string(),
        ));
    }
    if parse_windows(text, now_secs, true).is_err() {
        return Err(FetchError::Decode(
            "opencodego: usage fields missing on /go page".to_string(),
        ));
    }
    Ok(())
}

/// Whether the page is stating that this workspace has no Go plan.
///
/// The page renders the account record inline, and an unsubscribed workspace
/// carries an explicit `subscription:null` in it. That is the server making a
/// statement, not failing: there are no windows to report because there is no
/// plan, and the login is perfectly good.
///
/// Distinguishing it matters beyond the wording. Without this the page falls
/// through to the parser, fails to yield windows, and reports `decode_failed` —
/// which classifies as a stale browser login and tells an operator to sign in
/// again, when signing in changes nothing. The remedy for one is a login and for
/// the other is a subscription, so collapsing them sends people to the wrong
/// one.
///
/// Requires all three fields rather than any of them. A single null could be a
/// mid-rollout field or a lapsed payment method on a live plan, whereas a record
/// that names the subscription three ways and nulls each is unambiguous. And an
/// account that HAS a plan populates them, so a wrong positive here would hide a
/// real subscription's usage — the direction worth being strict about.
fn looks_unsubscribed(text: &str) -> bool {
    text.contains("subscription:null")
        && text.contains("subscriptionID:null")
        && text.contains("subscriptionPlan:null")
}

pub struct OpenCodeGoProvider {
    http: reqwest::Client,
    credential_source: Option<Arc<dyn CredentialSource>>,
    handle_loader: Arc<VaultHandleLoader>,
}

impl OpenCodeGoProvider {
    pub fn new() -> Self {
        Self::new_with_handle_loader(None, Arc::new(VaultHandleLoader::from_env()))
    }

    pub(crate) fn new_with_handle_loader(
        credential_source: Option<Arc<dyn CredentialSource>>,
        handle_loader: Arc<VaultHandleLoader>,
    ) -> Self {
        Self {
            http: crate::http::provider_client(),
            credential_source,
            handle_loader,
        }
    }

    async fn vault_cookie(
        &self,
        capability: &crate::credential_source::VaultCapability,
    ) -> Result<String, FetchError> {
        let source = self
            .credential_source
            .as_ref()
            .ok_or_else(|| FetchError::NoSession("no credential source configured".to_string()))?;
        let mut credential = source
            .get(capability, 120_000)
            .await
            .map_err(|error| FetchError::Upstream(error.to_string()))?;
        crate::credential_source::take_utf8_payload(&mut credential.payload)
    }
}

impl Default for OpenCodeGoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageProvider for OpenCodeGoProvider {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn is_cookie_based(&self) -> bool {
        true
    }

    fn handles(&self) -> Result<Vec<CredentialHandle>, crate::provider::HandlesError> {
        let mut handles = vec![CredentialHandle::implicit()];
        if self.credential_source.is_some() {
            handles.extend(self.handle_loader.opencodego_handles()?);
        }
        Ok(handles)
    }

    async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let (cookie, source) = if let Some(capability) = handle.vault_capability() {
                (self.vault_cookie(capability).await?, "vault")
            } else {
                (load_cookie_header_async().await?, SOURCE_LABEL)
            };
            let workspace_id = fetch_workspace_id(&self.http, &cookie).await?;
            let text = fetch_go_page_html(&self.http, &cookie, &workspace_id).await?;
            let now = chrono::Utc::now().timestamp();
            let usage = parse_windows(&text, now, true)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, source, usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_go_page, looks_unsubscribed};
    use crate::opencode::parse_windows;
    use crate::provider::FetchError;

    const GO_FIXTURE: &str = r#"
    rollingUsage: { usagePercent: 1, resetInSec: 100 },
    weeklyUsage: { usagePercent: 2, resetInSec: 200 },
    monthlyUsage: { usagePercent: 3, resetInSec: 300 }
    "#;

    #[test]
    fn go_fixture_yields_three_windows() {
        let usage = parse_windows(GO_FIXTURE, 1_000_000, true).unwrap();
        assert!(usage.primary.is_some());
        assert!(usage.secondary.is_some());
        assert!(usage.tertiary.is_some());
    }

    /// The account record as the live page renders it for a workspace with no Go
    /// plan. Captured from a real response and trimmed to the surrounding
    /// fields, because the neighbours are the point: a populated payment method
    /// beside the nulls shows the record rendered correctly and the plan is
    /// genuinely absent, rather than the page having failed.
    const LIVE_UNSUBSCRIBED_RECORD: &str = concat!(
        r#"paymentMethodType:"card",paymentMethodLast4:"4232",balance:0,"#,
        "reload:null,reloadAmount:20,monthlyLimit:null,monthlyUsage:null,",
        "timeMonthlyUsageUpdated:null,reloadError:null,subscription:null,",
        "subscriptionID:null,subscriptionPlan:null,timeSubscriptionBooked:null"
    );

    /// A workspace with no Go plan is reported as having no quota to report.
    ///
    /// Asserted through the classifier rather than the predicate, because the
    /// predicate being right is not the claim that matters -- the claim is that
    /// a page like this produces this error. Classified as a decode failure
    /// instead, it reads as a stale browser login and sends an operator to sign
    /// in again, which changes nothing.
    #[test]
    fn a_workspace_without_a_go_plan_reports_no_quota() {
        let err = classify_go_page(LIVE_UNSUBSCRIBED_RECORD, 1_000_000).unwrap_err();
        assert!(
            matches!(&err, FetchError::NoQuotaReported(m) if m.contains("no Go subscription")),
            "expected the no-plan verdict, got: {err}"
        );
    }

    /// A page that should parse is not diverted by the no-plan check.
    ///
    /// Without this the no-plan verdict could be returned for every page and the
    /// test above would still pass, which would silently replace every real
    /// window with a no-quota report.
    #[test]
    fn a_page_with_windows_is_accepted() {
        assert!(classify_go_page(GO_FIXTURE, 1_000_000).is_ok());
    }

    /// A page with neither windows nor a no-plan record is our defect to fix.
    #[test]
    fn a_page_with_no_windows_and_no_verdict_is_a_decode_failure() {
        let err = classify_go_page("<html>something else entirely</html>", 1_000_000).unwrap_err();
        assert!(
            matches!(&err, FetchError::Decode(m) if m.contains("usage fields missing")),
            "expected the decode failure, got: {err}"
        );
    }

    /// A subscribed workspace is not mistaken for an unsubscribed one.
    ///
    /// This is the direction that costs real data: a false positive here
    /// suppresses a live subscription's windows and reports the account as
    /// having no plan, which looks calm and is wrong.
    #[test]
    fn a_subscribed_workspace_is_not_treated_as_unsubscribed() {
        let subscribed = LIVE_UNSUBSCRIBED_RECORD
            .replace("subscription:null", r#"subscription:"active""#)
            .replace("subscriptionID:null", r#"subscriptionID:"sub_01ABC""#)
            .replace("subscriptionPlan:null", r#"subscriptionPlan:"go""#);
        assert!(!looks_unsubscribed(&subscribed));
    }

    /// One populated field is enough to withhold the conclusion.
    ///
    /// A single null is as consistent with a field being rolled out, or a lapsed
    /// payment method on a live plan, as it is with no subscription at all. The
    /// three together are what make the record unambiguous, so each is required
    /// and this proves none of them is decorative.
    #[test]
    fn a_single_populated_subscription_field_withholds_the_conclusion() {
        for field in ["subscription", "subscriptionID", "subscriptionPlan"] {
            let populated = LIVE_UNSUBSCRIBED_RECORD
                .replace(&format!("{field}:null"), &format!(r#"{field}:"x""#));
            assert!(
                !looks_unsubscribed(&populated),
                "concluded there is no plan while {field} was populated"
            );
        }
    }
}
