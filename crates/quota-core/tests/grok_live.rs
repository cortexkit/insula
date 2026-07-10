//! Live smoke check (ignored): proves the real grok gRPC-web fetch + protobuf decode.
use quota_core::grok::GrokProvider;
use quota_core::provider::CredentialHandle;
use quota_core::provider::UsageProvider;

#[tokio::test]
#[ignore = "requires a real xai OAuth entry in opencode auth.json"]
async fn grok_live_returns_real_window() {
    let p = GrokProvider::new();
    match p.fetch_handle(&CredentialHandle::implicit()).await.usage {
        Ok(usage) => {
            eprintln!("[grok-live] {}", serde_json::to_string(&usage).unwrap());
        }
        Err(e) => panic!("grok live fetch failed: {e}"),
    }
}
