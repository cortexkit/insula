//! Bounded subprocesses for the children this crate spawns to look at the
//! machine: `ps` and `lsof` for the Antigravity local probe, and `security` for
//! Chrome's storage key.
//!
//! Two guarantees, and each one exists because its absence was observed:
//!
//! 1. THE CALLER GETS ITS ANSWER AT THE TIMEOUT. `ps -ax` has been seen hanging
//!    host-wide in uninterruptible sleep (state `U`), with the running module
//!    holding 21 stuck `/bin/ps` children, the oldest minutes old. The old code
//!    ran `.output()` on a `spawn_blocking` thread. When the fetch hit its
//!    deadline its future was dropped, but the pool thread stayed parked in
//!    `wait` on the stuck child, so every refresh tick leaked one blocking thread
//!    and one child. Tokio caps that pool, and once it fills, every other
//!    `spawn_blocking` user in the process (the cookie-store reads and the
//!    Keychain read) queues behind it. Here the child is awaited on the async
//!    runtime instead, so no pool thread waits on it, and the caller walks away
//!    at its timeout while tokio reaps the child whenever it finally exits.
//!
//! 2. ONE CHILD PER COMMAND, HOWEVER LONG THE STALL LASTS. A [`Gate`] admits a
//!    single child at a time and is released only when that child has exited
//!    and been reaped, NOT when the caller gives up. Releasing it at the caller's
//!    timeout would reopen the gate while the stuck child is still there, and
//!    they would pile up again at one per timeout. A call that finds the gate
//!    occupied returns [`SubprocessError::Busy`] at once.
//!
//! None of these outcomes means "the command found nothing". Callers must
//! report them as "could not look", never as an empty result.
//!
//! WHY A CHILD AT ALL, rather than reading the process table in-process with
//! `proc_listallpids` and `KERN_PROCARGS2`: the stall is the kernel reading
//! another process's arguments, and doing that read in-process would park an
//! insula thread uninterruptibly on the same stall. A child is something we can
//! walk away from; a thread of our own is not.

// The only production callers are macOS paths (the Antigravity probe and the
// Keychain read). On other platforms this module is exercised by its tests alone,
// and the build denies warnings.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::LOG_TAG;

/// Extra time a synchronous caller waits beyond the command's own timeout before
/// concluding the runtime never ran the command at all.
///
/// Only reachable when the caller is blocking the very thread that would drive
/// the command, for example synchronous code on a single-threaded runtime. The
/// async path enforces the real timeout; this is a backstop so such a caller
/// still returns.
const BLOCKING_GRACE: Duration = Duration::from_secs(1);

/// The single-flight slot for one kind of command.
///
/// One static per command kind, so a stuck `ps` never blocks `lsof` and vice
/// versa, while two `ps` children can never coexist.
pub(crate) struct Gate {
    /// Short human name for log lines and error text, e.g. `/bin/ps -ax`.
    label: &'static str,
    /// `Some(start)` while a child admitted by this gate has not yet been
    /// reaped.
    running_since: Mutex<Option<Instant>>,
    /// How many children this gate has actually spawned. Read by tests to
    /// prove that a refused call started no process, without asking `ps`
    /// (which is the very command that hangs).
    spawns: AtomicUsize,
}

impl Gate {
    pub(crate) const fn new(label: &'static str) -> Self {
        Self {
            label,
            running_since: Mutex::new(None),
            spawns: AtomicUsize::new(0),
        }
    }

    /// Claim the slot, or report how long the current child has been running.
    fn try_enter(&'static self) -> Result<Held, SubprocessError> {
        let mut slot = self
            .running_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(since) = *slot {
            return Err(SubprocessError::Busy {
                command: self.label,
                running_for: since.elapsed(),
            });
        }
        *slot = Some(Instant::now());
        Ok(Held { gate: self })
    }

    #[cfg(test)]
    pub(crate) fn spawns(&self) -> usize {
        self.spawns.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn is_occupied(&self) -> bool {
        self.running_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// Ownership of a [`Gate`]'s slot. Dropping it reopens the gate.
struct Held {
    gate: &'static Gate,
}

impl Drop for Held {
    fn drop(&mut self) {
        *self
            .gate
            .running_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Why a bounded command produced no output to read.
///
/// Every variant means "could not look", never "looked and found nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubprocessError {
    /// The child did not finish within its timeout. It keeps running, and keeps
    /// its gate closed, until it exits.
    Timeout {
        command: &'static str,
        after: Duration,
    },
    /// An earlier child of the same command has not exited yet, so no new one
    /// was started.
    Busy {
        command: &'static str,
        running_for: Duration,
    },
    /// The child could not be started, or waiting on it failed.
    Spawn {
        command: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for SubprocessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout { command, after } => {
                write!(f, "`{command}` timed out after {:.1}s", after.as_secs_f64())
            }
            Self::Busy {
                command,
                running_for,
            } => write!(
                f,
                "skipped `{command}`: the previous one is still running after {:.1}s",
                running_for.as_secs_f64()
            ),
            Self::Spawn { command, detail } => {
                write!(f, "`{command}` could not be run: {detail}")
            }
        }
    }
}

impl std::error::Error for SubprocessError {}

/// A command run with a timeout and at most one live child per [`Gate`].
#[derive(Clone, Debug)]
pub(crate) struct BoundedCommand {
    gate: &'static Gate,
    program: String,
    args: Vec<String>,
    timeout: Duration,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate").field("label", &self.label).finish()
    }
}

impl BoundedCommand {
    pub(crate) fn new<I, S>(
        gate: &'static Gate,
        program: impl Into<String>,
        args: I,
        timeout: Duration,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            gate,
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout,
        }
    }

    /// Run the command and collect its output, within the timeout.
    ///
    /// The exit status is returned as-is in [`Output`]; whether a non-zero
    /// status means failure is the caller's call (`lsof` exits 1 when a process
    /// simply has no listening sockets).
    pub(crate) async fn output(&self) -> Result<Output, SubprocessError> {
        let held = self.gate.try_enter()?;
        let label = self.gate.label;

        let child = tokio::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Only fires if the runtime shuts down with the child still alive.
            // It cannot stop a child in uninterruptible sleep, but it is the
            // right request for one that is merely slow.
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| SubprocessError::Spawn {
                command: label,
                detail: error.to_string(),
            })?;
        self.gate.spawns.fetch_add(1, Ordering::SeqCst);
        let started = Instant::now();
        let timeout = self.timeout;

        // The child is owned by its own task so that it outlives this call: the
        // caller below may give up at the timeout, and the task keeps waiting
        // until the child exits and tokio has reaped it.
        let (answer, answer_received) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            // The gate stays closed for exactly as long as this task holds the
            // slot, which ends when the child has exited, not when the caller
            // stops waiting.
            let _held_until_exit = held;
            let waited = child.wait_with_output();
            tokio::pin!(waited);
            // Logged here, once per stuck child, rather than by each caller:
            // callers that find the gate occupied say nothing, so a stall
            // lasting hours produces two lines rather than one per tick.
            let result = tokio::select! {
                result = &mut waited => result,
                () = tokio::time::sleep(timeout) => {
                    eprintln!(
                        "{LOG_TAG} warning: `{label}` still running after {:.1}s; \
                         further `{label}` runs are skipped until it exits",
                        started.elapsed().as_secs_f64()
                    );
                    let result = waited.await;
                    eprintln!(
                        "{LOG_TAG} `{label}` exited after {:.1}s; runs resume",
                        started.elapsed().as_secs_f64()
                    );
                    result
                }
            };
            // The caller may already have left; nobody to tell is fine.
            let _ = answer.send(result);
        });

        match tokio::time::timeout(timeout, answer_received).await {
            Ok(Ok(Ok(output))) => Ok(output),
            Ok(Ok(Err(error))) => Err(SubprocessError::Spawn {
                command: label,
                detail: format!("waiting for it failed: {error}"),
            }),
            Ok(Err(_dropped)) => Err(SubprocessError::Spawn {
                command: label,
                detail: "the task supervising it ended without an answer".to_string(),
            }),
            Err(_elapsed) => Err(SubprocessError::Timeout {
                command: label,
                after: timeout,
            }),
        }
    }

    /// [`Self::output`] for synchronous code that runs inside a tokio runtime,
    /// such as a `spawn_blocking` closure.
    ///
    /// The command is still awaited on the runtime, so this thread is held only
    /// until the timeout, never for as long as a stuck child lives. Outside any
    /// runtime there is nothing to supervise the child after a timeout, and
    /// walking away from it there would quietly reopen the gate, so that case is
    /// refused rather than run.
    pub(crate) fn output_blocking(&self) -> Result<Output, SubprocessError> {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Err(SubprocessError::Spawn {
                command: self.gate.label,
                detail: "no async runtime is available to supervise it".to_string(),
            });
        };
        let (answer, answer_received) = std::sync::mpsc::sync_channel(1);
        let command = self.clone();
        runtime.spawn(async move {
            let _ = answer.send(command.output().await);
        });
        answer_received
            .recv_timeout(self.timeout + BLOCKING_GRACE)
            .unwrap_or(Err(SubprocessError::Timeout {
                command: self.gate.label,
                after: self.timeout,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The helper's whole contract against a child that outlives its timeout:
    /// the caller is answered at the timeout, a second call while the child
    /// lives starts nothing, and the gate reopens once the child exits.
    #[tokio::test]
    async fn a_stuck_child_times_out_once_and_blocks_a_second_until_it_exits() {
        static GATE: Gate = Gate::new("/bin/sleep 2");
        let sleep = BoundedCommand::new(&GATE, "/bin/sleep", ["2"], Duration::from_millis(200));

        let began = Instant::now();
        let first = sleep.output().await;
        let waited = began.elapsed();
        assert!(
            matches!(first, Err(SubprocessError::Timeout { .. })),
            "a child sleeping past the timeout must time out, got {first:?}"
        );
        assert!(
            waited < Duration::from_millis(1500),
            "the caller must be answered at the timeout, not when the child exits; waited {waited:?}"
        );
        assert_eq!(GATE.spawns(), 1);

        let began = Instant::now();
        let second = sleep.output().await;
        assert!(
            matches!(second, Err(SubprocessError::Busy { .. })),
            "a call while the first child lives must be skipped, got {second:?}"
        );
        assert!(began.elapsed() < Duration::from_millis(100));
        assert_eq!(GATE.spawns(), 1, "the skipped call must start no process");

        // The child exits about two seconds after it started; the gate reopens
        // only then.
        let deadline = Instant::now() + Duration::from_secs(10);
        while GATE.is_occupied() {
            assert!(
                Instant::now() < deadline,
                "the gate never reopened after the child exited"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let quick = BoundedCommand::new(&GATE, "/bin/sleep", ["0"], Duration::from_secs(5));
        let third = quick.output().await;
        assert!(
            third.as_ref().is_ok_and(|output| output.status.success()),
            "once the child has exited the next call must run normally, got {third:?}"
        );
        assert_eq!(GATE.spawns(), 2);
    }

    #[tokio::test]
    async fn output_is_collected_and_the_gate_reopens_after_a_normal_run() {
        static GATE: Gate = Gate::new("/bin/echo");
        let echo = BoundedCommand::new(&GATE, "/bin/echo", ["hello"], Duration::from_secs(5));
        let output = echo.output().await.expect("echo runs");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
        assert!(!GATE.is_occupied());
        echo.output().await.expect("a second run is admitted");
        assert_eq!(GATE.spawns(), 2);
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error_and_releases_the_gate() {
        static GATE: Gate = Gate::new("missing");
        let missing = BoundedCommand::new(
            &GATE,
            "/nonexistent/insula-test-binary",
            Vec::<String>::new(),
            Duration::from_secs(5),
        );
        assert!(matches!(
            missing.output().await,
            Err(SubprocessError::Spawn { .. })
        ));
        assert!(!GATE.is_occupied());
        assert_eq!(GATE.spawns(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_blocking_form_times_out_from_a_blocking_thread() {
        static GATE: Gate = Gate::new("/bin/sleep 2 blocking");
        let sleep = BoundedCommand::new(&GATE, "/bin/sleep", ["2"], Duration::from_millis(200));
        let began = Instant::now();
        let result = tokio::task::spawn_blocking(move || sleep.output_blocking())
            .await
            .expect("the blocking task completes");
        assert!(matches!(result, Err(SubprocessError::Timeout { .. })));
        assert!(began.elapsed() < Duration::from_millis(1500));
    }

    #[test]
    fn the_blocking_form_refuses_without_a_runtime() {
        static GATE: Gate = Gate::new("no runtime");
        let echo = BoundedCommand::new(&GATE, "/bin/echo", ["x"], Duration::from_secs(5));
        assert!(matches!(
            echo.output_blocking(),
            Err(SubprocessError::Spawn { .. })
        ));
        assert_eq!(GATE.spawns(), 0);
    }
}
