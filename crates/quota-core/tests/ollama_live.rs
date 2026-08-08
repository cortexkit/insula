//! Live proof (ignored): the real browser-cookie → decrypt → GET settings → parse
//! chain on a machine with a logged-in Chrome ollama session. Asserts a real
//! window OR a clean degrade — never a panic.
use quota_core::ollama::OllamaProvider;
use quota_core::provider::CredentialHandle;
use quota_core::provider::{FetchError, UsageProvider};

#[tokio::test]
#[ignore = "requires a logged-in ollama.com session in local Chrome + macOS keychain"]
async fn ollama_live_returns_real_window_or_degrades() {
    let p = OllamaProvider::new();
    match p.fetch_handle(&CredentialHandle::implicit()).await.usage {
        Ok(usage) => {
            eprintln!("[ollama-live] {}", serde_json::to_string(&usage).unwrap());
            assert!(
                usage.primary.is_some() || usage.secondary.is_some(),
                "expected at least one window"
            );
        }
        Err(e) => {
            eprintln!("[ollama-live] degraded: {e}");
            // This provider reads a browser session cookie and scrapes a page,
            // so every failure it can reach is an ordinary condition of that
            // arrangement: no cookie, a rejected one, an unreachable site, or
            // markup that no longer parses. An internal class is not, and means
            // this crate caught its own panic rather than handling a condition.
            assert!(
                !matches!(e, FetchError::Internal(_)),
                "a live probe must not surface an internal failure: {e:?}"
            );
        }
    }
}
