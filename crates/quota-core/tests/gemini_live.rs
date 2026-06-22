//! Live smoke check (ignored): proves the real gemini OAuth refresh + quota path.
use quota_core::gemini::GeminiProvider;
use quota_core::provider::UsageProvider;

#[tokio::test]
#[ignore = "requires a real ~/.gemini/oauth_creds.json"]
async fn gemini_live_returns_real_window() {
    let p = GeminiProvider::new();
    match p.fetch().await {
        Ok(entry) => {
            eprintln!("[gemini-live] {}", serde_json::to_string(&entry).unwrap());
            assert!(entry.error.is_none(), "expected healthy: {entry:?}");
        }
        Err(e) => panic!("gemini live fetch failed: {e}"),
    }
}
