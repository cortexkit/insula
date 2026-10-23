use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;

const ID: &str = "phone-request-0001";
const ACCOUNT: &str = "synthetic-account";

fn now() -> DateTime<Utc> {
    "2026-07-14T12:00:00Z".parse().unwrap()
}

fn journal() -> (crate::tests::ResetTempDir, RedemptionJournal) {
    let temp = crate::tests::ResetTempDir::with_prefix("ck-manual-reset", "journal");
    let journal = RedemptionJournal::new(temp.path().join("redemptions.json"));
    (temp, journal)
}

struct Server {
    url: String,
    posts: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn new(status: u16, body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/backend-api", listener.local_addr().unwrap());
        let posts = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let count = posts.clone();
        let captured = requests.clone();
        let body = body.to_owned();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = crate::loopback::read_request(&mut socket).await;
                assert!(request.starts_with(&format!("POST /backend-api{CONSUME_PATH} ")));
                captured.lock().unwrap().push(request);
                count.fetch_add(1, Ordering::SeqCst);
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        Self {
            url,
            posts,
            requests,
            task,
        }
    }

    fn request(&self) -> ResetRequest {
        ResetRequest {
            base_url: self.url.clone(),
            bearer: "synthetic-token".into(),
            account_id: ACCOUNT.into(),
            auth_failure: None,
        }
    }

    fn count(&self) -> usize {
        self.posts.load(Ordering::SeqCst)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn redeem(coordinator: &ResetCoordinator, server: &Server, id: &str) -> SavedResetResult {
    coordinator
        .use_saved_reset(
            ACCOUNT,
            id,
            now(),
            &ReqwestResetTransport::new(crate::http::provider_client()),
            &server.request(),
            Duration::from_secs(2),
        )
        .await
}

#[tokio::test]
async fn saved_reset_resend_after_restart_returns_stored_outcome_without_second_post() {
    let (_temp, journal) = journal();
    let server = Server::new(200, r#"{"code":"reset"}"#).await;
    let first = ResetCoordinator::new(journal.clone()).unwrap();
    assert_eq!(
        redeem(&first, &server, ID).await.result,
        Ok(ConsumeOutcome::Reset)
    );
    drop(first);
    let restarted =
        ResetCoordinator::new(RedemptionJournal::new(journal.path().to_path_buf())).unwrap();
    let replay = redeem(&restarted, &server, ID).await;
    assert_eq!(replay.result, Ok(ConsumeOutcome::Reset));
    assert!(!replay.consume_attempted);
    assert_eq!(
        server.count(),
        1,
        "a resolved id must not issue another consume POST"
    );
    assert_eq!(
        journal.records().unwrap()[0].redeem_request_id,
        ID,
        "the client id is the durable redeem id"
    );
    restarted
        .journal
        .inspect_account(ACCOUNT, now() + chrono::Duration::days(30))
        .unwrap();
    assert_eq!(
        redeem(&restarted, &server, ID).await.result,
        Ok(ConsumeOutcome::Reset)
    );
    assert_eq!(
        server.count(),
        1,
        "automatic pruning must preserve manual replay ids"
    );
}

#[tokio::test]
async fn saved_reset_pending_resumes_with_same_redeem_request_id() {
    let (_temp, journal) = journal();
    journal.reserve_manual(ACCOUNT, ID, now()).unwrap();
    let coordinator =
        ResetCoordinator::new(RedemptionJournal::new(journal.path().to_path_buf())).unwrap();
    let server = Server::new(200, r#"{"code":"already_redeemed"}"#).await;
    assert_eq!(
        redeem(&coordinator, &server, ID).await.result,
        Ok(ConsumeOutcome::AlreadyRedeemed)
    );
    assert_eq!(server.count(), 1);
    let requests = server.requests.lock().unwrap();
    let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap()["redeem_request_id"],
        ID,
        "pending retries must reuse the original redeem id"
    );
    let headers = headers.to_ascii_lowercase();
    for header in [
        "authorization: bearer synthetic-token",
        "chatgpt-account-id: synthetic-account",
        "openai-beta: codex-1",
        "originator: codex desktop",
        "user-agent: ai-provider-quota",
    ] {
        assert!(
            headers.contains(header),
            "missing automatic consume header: {header}"
        );
    }
}

#[tokio::test]
async fn saved_reset_fresh_id_within_bound_refuses_with_reservation_time_and_no_post() {
    let (_temp, journal) = journal();
    let at = now() - chrono::Duration::minutes(12);
    journal
        .reserve_manual(ACCOUNT, "previous-request", at)
        .unwrap();
    let coordinator = ResetCoordinator::new(journal).unwrap();
    let server = Server::new(200, r#"{"code":"reset"}"#).await;
    let result = redeem(&coordinator, &server, ID).await;
    assert_eq!(
        result.result,
        Err(SavedResetError::RecentlyReset(at)),
        "a fresh id must obey the manual 30-minute bound"
    );
    assert!(!result.consume_attempted);
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn saved_reset_ignores_sibling_headroom_and_provider_floor() {
    let (_temp, journal) = journal();
    journal
        .reserve_manual(
            "sibling",
            "sibling-request",
            now() - chrono::Duration::minutes(1),
        )
        .unwrap();
    journal
        .resolve("sibling", "sibling-request", ConsumeOutcome::Reset)
        .unwrap();
    let coordinator = ResetCoordinator::new(journal).unwrap();
    coordinator.observe_headroom(
        "sibling",
        &UsageFacts {
            raw_percents: vec![5.0],
            any_used_floor: true,
            at_wall: false,
            wall_clear: true,
            wall_lifts_at: None,
        },
        true,
        Instant::now(),
    );
    let server = Server::new(200, r#"{"code":"reset"}"#).await;
    assert!(coordinator
        .sibling_with_headroom(ACCOUNT, Instant::now())
        .is_some());
    assert_eq!(
        redeem(&coordinator, &server, ID).await.result,
        Ok(ConsumeOutcome::Reset)
    );
    assert_eq!(server.count(), 1);
}

#[test]
fn saved_reset_manual_reservation_blocks_automatic_bound_and_floor() {
    let (_temp, journal) = journal();
    journal.reserve_manual(ACCOUNT, ID, now()).unwrap();
    journal
        .resolve(ACCOUNT, ID, ConsumeOutcome::NoCredit)
        .unwrap();
    assert_eq!(
        journal.reserve(ACCOUNT, now()).unwrap(),
        Reservation::SpendBound
    );
    assert!(matches!(
        journal.reserve("other", now()).unwrap(),
        Reservation::ProviderFloor(_)
    ));
    assert!(matches!(
        journal
            .reserve(ACCOUNT, now() + chrono::Duration::minutes(30))
            .unwrap(),
        Reservation::New(_)
    ));
}

#[tokio::test]
async fn saved_reset_all_provider_outcomes_and_refusals_are_mapped() {
    for outcome in [
        ConsumeOutcome::Reset,
        ConsumeOutcome::NothingToReset,
        ConsumeOutcome::NoCredit,
        ConsumeOutcome::AlreadyRedeemed,
    ] {
        let (_temp, journal) = journal();
        let coordinator = ResetCoordinator::new(journal.clone()).unwrap();
        let server = Server::new(200, &format!(r#"{{"code":"{}"}}"#, outcome.as_code())).await;
        let result = redeem(&coordinator, &server, ID).await;
        assert_eq!(result.result, Ok(outcome));
        assert!(result.consume_attempted);
        assert_eq!(
            journal.records().unwrap()[0].status,
            JournalStatus::Resolved
        );
    }
    for status in [400, 401, 403, 429, 500, 503] {
        let (_temp, journal) = journal();
        let coordinator = ResetCoordinator::new(journal.clone()).unwrap();
        let server = Server::new(status, &"拒".repeat(230)).await;
        let result = redeem(&coordinator, &server, ID).await;
        if status < 500 {
            assert_eq!(
                result.result,
                Err(SavedResetError::ProviderRefused("拒".repeat(200)))
            );
        } else {
            assert!(matches!(
                result.result,
                Err(SavedResetError::ProviderUnavailable(_))
            ));
        }
        assert!(result.consume_attempted);
        assert_eq!(journal.records().unwrap()[0].status, JournalStatus::Pending);
    }
}

struct Hanging;
#[async_trait]
impl ResetTransport for Hanging {
    async fn fetch_credits(&self, _: &ResetRequest) -> Result<CreditsHttpResponse, FetchError> {
        unreachable!()
    }
    async fn consume(&self, _: &ResetRequest, _: &str) -> Result<Vec<u8>, FetchError> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn saved_reset_timeout_keeps_pending_id_for_retry() {
    let (_temp, journal) = journal();
    let coordinator = ResetCoordinator::new(journal.clone()).unwrap();
    let request = ResetRequest {
        base_url: "https://example.invalid".into(),
        bearer: "synthetic".into(),
        account_id: ACCOUNT.into(),
        auth_failure: None,
    };
    let result = coordinator
        .use_saved_reset(
            ACCOUNT,
            ID,
            now(),
            &Hanging,
            &request,
            Duration::from_secs(1),
        )
        .await;
    assert!(matches!(
        result.result,
        Err(SavedResetError::ProviderUnavailable(_))
    ));
    assert!(result.consume_attempted);
    let record = journal.records().unwrap().remove(0);
    assert_eq!(record.redeem_request_id, ID);
    assert_eq!(record.status, JournalStatus::Pending);
}
