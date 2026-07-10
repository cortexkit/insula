//! Live proof (ignored): the real local-probe chain on a machine with Antigravity
//! (app or `agy` CLI) running. Asserts a real window OR a clean degrade — never a panic.
use quota_core::antigravity::AntigravityProvider;
use quota_core::provider::CredentialHandle;
use quota_core::provider::{FetchError, UsageProvider};

#[tokio::test]
#[ignore = "requires a running Antigravity editor / agy CLI on this machine"]
async fn antigravity_live_returns_real_window_or_degrades() {
    let p = AntigravityProvider::new();
    match p.fetch_handle(&CredentialHandle::implicit()).await.usage {
        Ok(usage) => {
            eprintln!(
                "[antigravity-live] {}",
                serde_json::to_string(&usage).unwrap()
            );
            assert!(
                usage.primary.is_some() || usage.secondary.is_some(),
                "expected at least one window"
            );
        }
        Err(e) => {
            eprintln!("[antigravity-live] degraded: {e}");
            assert!(matches!(
                e,
                FetchError::NoSession(_)
                    | FetchError::Unauthorized(_)
                    | FetchError::Upstream(_)
                    | FetchError::Decode(_)
            ));
        }
    }
}
