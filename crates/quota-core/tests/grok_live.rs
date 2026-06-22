//! Live smoke check (ignored): proves the real grok gRPC-web fetch + protobuf decode.
use quota_core::grok::GrokProvider;
use quota_core::provider::UsageProvider;

#[tokio::test]
#[ignore = "requires a real xai OAuth entry in opencode auth.json"]
async fn grok_live_returns_real_window() {
    let p = GrokProvider::new();
    match p.fetch().await {
        Ok(entry) => {
            eprintln!("[grok-live] {}", serde_json::to_string(&entry).unwrap());
            assert!(entry.error.is_none(), "expected healthy: {entry:?}");
        }
        Err(e) => panic!("grok live fetch failed: {e}"),
    }
}
