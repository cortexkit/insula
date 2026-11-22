//! Claim RPCs shared by the primary module loop and its consumer connections.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use subc_protocol::{
    claim::{
        Claim, ClaimMintReply, ClaimMintRequest, ClaimPresentReply, ClaimPresentRequest,
        ClaimPurpose, ClaimRateLimitedDetail,
    },
    error_codes::CLAIM_RATE_LIMITED,
    ErrorBody, Flags, Frame, FrameType, Priority,
};
use subc_transport::{read_frame, write_frame};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};

type PendingMints = HashMap<u64, oneshot::Sender<Frame>>;

/// Sends claim requests through the registered module's writer. Only the module
/// loop reads that stream, so serving requests can continue during a mint.
#[derive(Clone)]
pub struct ClaimMinter {
    writer: mpsc::WeakSender<Frame>,
    state: Arc<MintState>,
}

struct MintState {
    next_corr: AtomicU64,
    pending: Mutex<Option<PendingMints>>,
}

impl ClaimMinter {
    pub fn new(writer: &mpsc::Sender<Frame>) -> Self {
        Self {
            writer: writer.downgrade(),
            state: Arc::new(MintState {
                // HELLO used correlation 1 on the same control lane.
                next_corr: AtomicU64::new(2),
                pending: Mutex::new(Some(HashMap::new())),
            }),
        }
    }

    pub(crate) async fn mint(&self, deadline: Duration) -> Option<Claim> {
        tokio::time::timeout(deadline, self.mint_inner())
            .await
            .ok()
            .flatten()
    }

    async fn mint_inner(&self) -> Option<Claim> {
        let writer = self.writer.upgrade()?;
        let corr = self.state.next_corr.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::to_vec(&ClaimMintRequest::new(ClaimPurpose::SelfConnection)).ok()?;
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            corr,
            body,
        )
        .ok()?;
        let (tx, rx) = oneshot::channel();
        self.state.pending.lock().ok()?.as_mut()?.insert(corr, tx);
        let _pending = PendingMint { minter: self, corr };
        writer.send(frame).await.ok()?;
        drop(writer);
        let reply = rx.await.ok()?;
        if reply.header.ty == FrameType::Error {
            let error: ErrorBody = serde_json::from_slice(&reply.body).ok()?;
            if error.code == CLAIM_RATE_LIMITED {
                if let Some(delay) = error.detail.and_then(|detail| {
                    serde_json::from_value::<ClaimRateLimitedDetail>(detail).ok()
                }) {
                    // Honor the daemon's delay within the mint budget, then let
                    // the caller's normal connection backoff schedule a new attempt.
                    tokio::time::sleep(Duration::from_millis(delay.retry_after_ms)).await;
                }
            }
            return None;
        }
        // The daemon enforces expiry on its monotonic clock. The wall-clock
        // timestamp in the reply is informational and the claim is never cached.
        serde_json::from_slice::<ClaimMintReply>(&reply.body)
            .ok()
            .map(|reply| reply.claim)
    }

    /// Deliver only terminal replies on the primary connection's control lane.
    pub fn dispatch(&self, frame: Frame) {
        if let Some(tx) = self.remove_pending(frame.header.corr) {
            let _ = tx.send(frame);
        }
    }

    /// Wake in-flight callers and prevent new mints when the module loop exits.
    pub fn close(&self) {
        if let Ok(mut pending) = self.state.pending.lock() {
            pending.take();
        }
    }

    fn remove_pending(&self, corr: u64) -> Option<oneshot::Sender<Frame>> {
        self.state.pending.lock().ok()?.as_mut()?.remove(&corr)
    }
}

struct PendingMint<'a> {
    minter: &'a ClaimMinter,
    corr: u64,
}

impl Drop for PendingMint<'_> {
    fn drop(&mut self) {
        self.minter.remove_pending(self.corr);
    }
}

pub(crate) async fn present_claim<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    corr: u64,
    claim: Claim,
    deadline: Duration,
) -> Option<()> {
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&ClaimPresentRequest::new(claim)).ok()?,
    )
    .ok()?;
    let reply = tokio::time::timeout(deadline, async {
        write_frame(stream, &frame).await.ok()?;
        read_frame(stream).await.ok().flatten()
    })
    .await
    .ok()??;
    if reply.header.channel != 0
        || reply.header.epoch != 0
        || reply.header.corr != corr
        || reply.header.ty != FrameType::Response
    {
        return None;
    }
    serde_json::from_slice::<ClaimPresentReply>(&reply.body).ok()?;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn claim_mints_correlate_out_of_order_and_stop_with_the_module_loop() {
        let (writer, mut outgoing) = mpsc::channel(4);
        let owner = ClaimMinter::new(&writer);
        let budget = Duration::from_secs(2);
        let ask = |minter: ClaimMinter| tokio::spawn(async move { minter.mint(budget).await });
        let first = ask(owner.clone());
        let first_corr = outgoing.recv().await.unwrap().header.corr;
        let second = ask(owner.clone());
        let second_corr = outgoing.recv().await.unwrap().header.corr;
        assert_ne!(first_corr, second_corr);
        for (corr, token) in [(second_corr, "second"), (first_corr, "first")] {
            let frame = Frame::build(
                FrameType::Response,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                corr,
                serde_json::to_vec(&ClaimMintReply::new(Claim::new(token), 0)).unwrap(),
            )
            .unwrap();
            owner.dispatch(frame);
        }
        assert_eq!(first.await.unwrap(), Some(Claim::new("first")));
        assert_eq!(second.await.unwrap(), Some(Claim::new("second")));

        let pending = ask(owner.clone());
        outgoing.recv().await.unwrap();
        owner.close();
        assert_eq!(pending.await.unwrap(), None);
        assert_eq!(owner.mint(budget).await, None);
        assert!(outgoing.try_recv().is_err(), "a closed module cannot mint");
    }
}
