//! Live proof (ignored): the real browser-cookie → decrypt → GET settings → parse
//! chain on a machine with a logged-in Chrome ollama session. Asserts a real
//! window OR a clean degrade — never a panic.
use quota_core::ollama::OllamaProvider;
use quota_core::provider::{FetchError, UsageProvider};

#[tokio::test]
#[ignore = "requires a logged-in ollama.com session in local Chrome + macOS keychain"]
async fn ollama_live_returns_real_window_or_degrades() {
    let p = OllamaProvider::new();
    match p.fetch().await {
        Ok(entry) => {
            eprintln!("[ollama-live] {}", serde_json::to_string(&entry).unwrap());
            assert!(entry.error.is_none(), "healthy entry expected: {entry:?}");
            assert!(
                entry
                    .usage
                    .as_ref()
                    .map(|u| u.primary.is_some() || u.secondary.is_some())
                    .unwrap_or(false),
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
