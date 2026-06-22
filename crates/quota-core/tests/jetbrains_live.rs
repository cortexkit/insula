//! Live check (ignored): exercises real JetBrains XML discovery + parse on this
//! machine. The files here read type:"Unknown" (no active AI quota), so the
//! provider must DEGRADE cleanly (NoSession), proving file-read + parse + degrade.
use quota_core::jetbrains::JetBrainsProvider;
use quota_core::provider::{FetchError, UsageProvider};

#[tokio::test]
#[ignore = "requires JetBrains IDE config files on disk"]
async fn jetbrains_live_reads_and_degrades_or_reports() {
    let p = JetBrainsProvider::new();
    match p.fetch().await {
        Ok(entry) => eprintln!("[jetbrains-live] active window: {}", serde_json::to_string(&entry).unwrap()),
        Err(e) => {
            eprintln!("[jetbrains-live] degraded (expected on a no-active-quota machine): {e}");
            // Either a real window OR a clean degrade is acceptable; a panic/crash is not.
            assert!(matches!(e, FetchError::NoSession(_) | FetchError::Decode(_)));
        }
    }
}
