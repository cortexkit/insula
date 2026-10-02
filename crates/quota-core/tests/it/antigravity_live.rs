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
            // Every failure this provider can reach is recoverable without
            // changing anything about the account: the editor is not running,
            // its local server refused or answered oddly, or the credential
            // needs refreshing. What must not appear is an internal class, which
            // means this crate caught its own panic rather than handling a
            // condition -- the one outcome a live probe should never normalise.
            assert!(
                !matches!(e, FetchError::Internal(_)),
                "a live probe must not surface an internal failure: {e:?}"
            );
        }
    }
}
