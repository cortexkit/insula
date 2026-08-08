//! Live check (ignored): exercises real JetBrains XML discovery and parsing
//! against whatever IDE configuration exists on the machine running it.
//!
//! Both outcomes are legitimate and depend entirely on that machine: an active
//! AI subscription yields a window, and an IDE installed without one yields a
//! degraded entry. What is asserted is the part that must hold either way --
//! that the provider reads local files without panicking, and attributes a
//! failure to the local source rather than to a network it never contacted.
use quota_core::jetbrains::JetBrainsProvider;
use quota_core::provider::CredentialHandle;
use quota_core::provider::{FetchError, UsageProvider};

#[tokio::test]
#[ignore = "requires JetBrains IDE config files on disk"]
async fn jetbrains_live_reads_and_degrades_or_reports() {
    let p = JetBrainsProvider::new();
    match p.fetch_handle(&CredentialHandle::implicit()).await.usage {
        Ok(entry) => eprintln!(
            "[jetbrains-live] active window: {}",
            serde_json::to_string(&entry).unwrap()
        ),
        Err(e) => {
            eprintln!("[jetbrains-live] degraded: {e}");
            // This provider reads files on disk and makes no network request, so
            // only local-source failures can arise: the files are missing, they
            // are present but unreadable, they parse but declare no active
            // quota, or they are malformed.
            //
            // Each failure is published with a machine-readable class that tells
            // an operator what to do about it, so a transport or authentication
            // class here would send them to check a network or a credential this
            // provider never touches.
            assert!(
                matches!(
                    e,
                    FetchError::NoSession(_)
                        | FetchError::LocalSourceUnavailable(_)
                        | FetchError::NoQuotaReported(_)
                        | FetchError::Decode(_)
                ),
                "a local file read must not report a network or auth failure: {e:?}"
            );
        }
    }
}
