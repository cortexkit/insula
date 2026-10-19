//! Lossy route notices share quota-sample change detection, not bus delivery.
//!
//! The writer pulls from these queues only when ordinary replies are ready to
//! yield. No watcher owns a sending task or a place in the reply channel, so a
//! stalled reader cannot make the refresher await egress capacity.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

use serde_json::{json, Value};
use subc_protocol::{AdmissionClass, Flags, Frame, FrameType, Priority};
use tokio::sync::Notify;

pub const USAGE_WATCH_OP: &str = "usage.watch";
pub const MAX_WATCHERS: usize = 64;
pub const NOTICE_CAPACITY: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    ver: u8,
    channel: u16,
    epoch: u32,
    corr: u64,
}

impl Target {
    fn of(frame: &Frame) -> Self {
        Self {
            ver: frame.header.ver,
            channel: frame.header.channel,
            epoch: frame.header.epoch,
            corr: frame.header.corr,
        }
    }

    fn frame(self, ty: FrameType, body: Vec<u8>) -> Frame {
        let flags = Flags::new(false, Priority::Interactive, ty == FrameType::StreamEnd);
        let flags = if ty == FrameType::StreamData {
            flags.with_admission_class(AdmissionClass::Sheddable)
        } else {
            flags
        };
        Frame::build_with_version(
            self.ver,
            ty,
            flags,
            self.channel,
            self.epoch,
            self.corr,
            body,
        )
        .expect("watch frames have a bounded, serialized notice body")
    }
}

struct Watch {
    target: Target,
    seq: u64,
    queue: VecDeque<Value>,
}

#[derive(Default)]
struct State {
    streams: VecDeque<Watch>,
    ends: VecDeque<Target>,
    stopped: bool,
}

pub struct UsageWatches {
    epoch: String,
    state: Mutex<State>,
    wake: Notify,
    dropped: AtomicU64,
}

impl UsageWatches {
    pub fn new(epoch: String) -> Self {
        Self {
            epoch,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn subscribe(&self, frame: &Frame) -> Result<(), (&'static str, &'static str)> {
        // The method alone makes this a subscription; the SUBSCRIPTION flag
        // (bit 7) is accepted but not required. The TypeScript subc client
        // deliberately doesn't send bit 7 yet, because decoders built against
        // subc-protocol 0.20.0 and older reject it as a reserved bit, so
        // requiring it would refuse every host plugin. Claustrum's
        // auth.watch_scoped accepts the same unflagged request.
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopped {
            return Err(("shutting_down", "usage watches are stopping"));
        }
        let target = Target::of(frame);
        if state.streams.iter().any(|watch| watch.target == target) {
            return Err((
                "duplicate_watch",
                "this correlation already has a usage watch",
            ));
        }
        if state.streams.len() >= MAX_WATCHERS {
            return Err(("usage_watch_limit", "at most 64 usage watches may be open"));
        }
        let mut watch = Watch {
            target,
            seq: 0,
            queue: VecDeque::new(),
        };
        self.enqueue(&mut watch, None);
        state.streams.push_back(watch);
        self.wake.notify_one();
        Ok(())
    }

    fn enqueue(&self, watch: &mut Watch, key: Option<(&str, Option<&str>)>) {
        watch.seq = watch
            .seq
            .checked_add(1)
            .expect("usage watch sequence exhausted");
        let mut notice = json!({"kind":"changed", "epoch":self.epoch, "seq":watch.seq});
        if let Some((provider, account)) = key {
            notice["provider"] = json!(provider);
            if let Some(account) = account {
                notice["account"] = json!(account);
            }
        }
        if watch.queue.len() == NOTICE_CAPACITY {
            watch.queue.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        watch.queue.push_back(notice);
    }

    pub fn changed(&self, provider: &str, account: Option<&str>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for watch in &mut state.streams {
            self.enqueue(watch, Some((provider, account)));
        }
        self.wake.notify_one();
    }

    pub fn cancel(&self, frame: &Frame) {
        let target = Target::of(frame);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.streams.retain(|watch| watch.target != target);
        state.ends.retain(|end| *end != target);
    }

    pub fn close_route(&self, frame: &Frame) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let matches = |target: Target| {
            target.channel == frame.header.channel && target.epoch == frame.header.epoch
        };
        state.streams.retain(|watch| !matches(watch.target));
        state.ends.retain(|end| !matches(*end));
    }

    pub fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.stopped = true;
        while let Some(watch) = state.streams.pop_front() {
            // Terminals are not lossy notices: reserve one per admitted stream,
            // discard stale changes, and let the writer drain these on shutdown.
            state.ends.push_back(watch.target);
        }
        self.wake.notify_one();
    }

    pub fn try_next(&self) -> Option<Frame> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(target) = state.ends.pop_front() {
            return Some(target.frame(FrameType::StreamEnd, Vec::new()));
        }
        // Rotate even idle streams so one busy account cannot starve a watch
        // behind it. The scan and all retained state are bounded by MAX_WATCHERS.
        for _ in 0..state.streams.len() {
            let mut watch = state.streams.pop_front().unwrap();
            let frame = watch.queue.pop_front().map(|notice| {
                watch
                    .target
                    .frame(FrameType::StreamData, serde_json::to_vec(&notice).unwrap())
            });
            state.streams.push_back(watch);
            if frame.is_some() {
                return frame;
            }
        }
        None
    }

    pub async fn next(&self) -> Frame {
        loop {
            let notified = self.wake.notified();
            if let Some(frame) = self.try_next() {
                return frame;
            }
            notified.await;
        }
    }

    pub fn metrics(&self) -> Value {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        json!({"usageWatchers":state.streams.len(),
            "usageWatchNoticesDropped":self.dropped.load(Ordering::Relaxed)})
    }
}
