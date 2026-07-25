//! OpenCode Go usage — same browser session as OpenCode, HTML `/go` page scrape.
//!
//! Reuses workspace id fetch and window parsing from [`crate::opencode`]. Zen prepaid
//! balance is intentionally omitted (no reset window).
//!
//! VERIFICATION: fixture-verified against CodexBar source, NOT live-verified.
//! Ported from `OpenCodeGo/OpenCodeGoUsageFetcher.swift` (:406-430 page fetch,
//! :432-519 parse with monthly). Cookie filter shared with
//! `OpenCode/OpenCodeWebCookieSupport.swift:4`.

use std::time::Duration;

use async_trait::async_trait;

use crate::provider::{CredentialHandle, FetchAttempt};
use crate::{
    http::{Header, JsonRequest},
    model::ProviderUsage,
    opencode::{
        fetch_workspace_id, load_cookie_header_async, looks_signed_out, parse_windows, USER_AGENT,
    },
    provider::{FetchError, UsageProvider},
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
    if looks_signed_out(&text) {
        return Err(FetchError::Unauthorized(
            "opencodego session expired (go page)".to_string(),
        ));
    }
    let now = chrono::Utc::now().timestamp();
    if parse_windows(&text, now, true).is_err() {
        return Err(FetchError::Decode(
            "opencodego: usage fields missing on /go page".to_string(),
        ));
    }
    Ok(text)
}

pub struct OpenCodeGoProvider {
    http: reqwest::Client,
}

impl OpenCodeGoProvider {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
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

    async fn fetch_handle(&self, _handle: &CredentialHandle) -> FetchAttempt {
        let result: Result<ProviderUsage, FetchError> = async {
            let cookie = load_cookie_header_async().await?;
            let workspace_id = fetch_workspace_id(&self.http, &cookie).await?;
            let text = fetch_go_page_html(&self.http, &cookie, &workspace_id).await?;
            let now = chrono::Utc::now().timestamp();
            let usage = parse_windows(&text, now, true)?;
            Ok(ProviderUsage::healthy(PROVIDER_NAME, None, "api", usage))
        }
        .await;
        FetchAttempt::from_provider_usage(result)
    }
}

#[cfg(test)]
mod tests {
    use crate::opencode::parse_windows;

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
}
