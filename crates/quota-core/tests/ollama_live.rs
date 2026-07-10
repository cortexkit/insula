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
