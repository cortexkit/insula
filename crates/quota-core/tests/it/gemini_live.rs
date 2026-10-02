//! Live smoke check (ignored): proves the real gemini OAuth refresh + quota path.
use quota_core::gemini::GeminiProvider;
use quota_core::provider::CredentialHandle;
use quota_core::provider::UsageProvider;

#[tokio::test]
#[ignore = "requires a real ~/.gemini/oauth_creds.json"]
async fn gemini_live_returns_real_window() {
    let p = GeminiProvider::new();
    match p.fetch_handle(&CredentialHandle::implicit()).await.usage {
        Ok(usage) => {
            eprintln!("[gemini-live] {}", serde_json::to_string(&usage).unwrap());
        }
        Err(e) => panic!("gemini live fetch failed: {e}"),
    }
}
