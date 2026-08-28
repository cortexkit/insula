#!/usr/bin/env python3
"""Capture what syspolicyd is doing WHEN IT WEDGES, so the next episode is evidence.

THE PROBLEM IS THAT WE KEEP ARRIVING AFTER THE FACT. Three episodes have now been
investigated on this machine and every one was diagnosed from the wreckage: CPU
totals after the fact, a crash report, a scan list read hours later. The onset --
the thing that would name the trigger -- has never been observed, because by the
time a human notices the machine is stuck, no new process can launch to look at it.

So this runs CONTINUOUSLY at negligible cost and only starts recording when the
wedge signature appears. It is the opposite of the watchdog next to it: the
watchdog RESTARTS syspolicyd to end an episode, this one WATCHES one happen. Run
the capture alone if you want to understand the cause; run both if you want the
cause and a short episode, accepting that the restart truncates what is recorded.

WHAT IT RECORDS, in the order that matters:

    1. the open file handles     which paths syspolicyd is walking. This is the
                                 signal that cracked the scan-list finding, and it
                                 is the first thing to go stale after a restart.
    2. a 10s syscall sample      getdirentries64 storms versus real validation work
    3. its own log lines         -67062 (errSecCSUnsigned) rate, and anything else
    4. what was building         fresh Mach-O executables per repo in the window,
                                 which is the correlation nobody has established

WHY A CPU TRIGGER RATHER THAN A LOG PREDICATE. The -67062 storm and the CPU spike
have always appeared together, but the storm also appears at low rates during
normal operation, so triggering on it would record mostly noise. CPU has been
unambiguous in every observed episode: 0-4% healthy, 40-137% wedged.

THE CAPTURE ITSELF NEEDS TO RUN DURING A WEDGE, which is exactly when new
processes cannot start. So the sampling loop spawns nothing in its hot path -- it
reads /proc-equivalent state through ps, which is already running, and only shells
out AFTER the trigger fires, accepting that those may be slow or fail. A capture
that cannot run during the event it captures is the failure mode this whole file
exists to avoid, and it is not fully avoidable here: if exec is completely blocked,
the post-trigger commands will block too. What survives in that case is the
trigger record itself, which is written before anything is spawned.
"""

from __future__ import annotations

import datetime
import os
import shutil
import subprocess
import sys

# The probe lives in a shared module because the capture and the watchdog must
# agree on what a wedge IS -- one records it, the other acts on it. Two copies
# would be two answers, free to drift invisibly.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from syspolicyd_probe import PROBE_TRIGGER_MS, probe_exec_ms, probe_source  # noqa: E402
import sys
import time

# EXEC LATENCY IS THE TRIGGER, NOT CPU. The first version of this file fired on
# syspolicyd CPU >= 60% and would have sat armed through the very episodes it was
# built for: the operator reported that during a real wedge there is NO notable
# CPU load, and measurement backed them up.
#
# What is actually expensive is the FIRST exec of a freshly written binary --
# roughly 250-620 ms against 4 ms to re-exec the same file, measured on real Rust
# test binaries. That cost is a wait, not a spin, which is exactly why the machine
# looks idle while nothing can start. A CPU threshold cannot see a queue of waits.
#
# So this probes the symptom itself: write a small fresh binary, exec it, time it.
# It fires whether syspolicyd spins, blocks, or dies, and it needs no privileged
# read to decide. The healthy figure has a wide margin below the trigger, and
# during a wedge the probe's own exec is subject to the wedge -- so a probe that
# takes many seconds IS the measurement rather than a failure of it.

# Retained only for the post-trigger record, never as the trigger.
BUSY_PCT = 60.0

# Two consecutive samples, so a momentary spike during normal validation work does
# not fire it. Deliberately much shorter than the watchdog's 120s sustain: this
# wants to catch the ONSET, and waiting two minutes to be sure would record the
# middle of an episode rather than its beginning.
# The probe itself costs ~250-400 ms of real validation work, so its cadence is a
# cost decision rather than a resolution one. At 30s that is under 1.5% duty, and
# an episode lasting less than one interval is not one anybody noticed.
SAMPLE_SECS = 30

# Do not re-capture the same episode over and over.
COOLDOWN_SECS = 900

OUT_DIR = os.path.expanduser("~/.local/state/cortexkit/syspolicyd-captures")


def now() -> str:
    return datetime.datetime.now().strftime("%Y-%m-%dT%H:%M:%S")


_PROBE_SRC: str | None = None




def syspolicyd() -> tuple[int, float] | None:
    try:
        out = subprocess.run(
            ["ps", "-Ao", "pid=,pcpu=,comm="],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return None
    for line in out.stdout.splitlines():
        parts = line.split(None, 2)
        if len(parts) < 3 or not parts[2].endswith("/syspolicyd"):
            continue
        try:
            return int(parts[0]), float(parts[1])
        except ValueError:
            return None
    return None


def run(cmd: list[str], timeout: int) -> str:
    """Best-effort. During a wedge these may block or fail; that is data too."""
    try:
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        return out.stdout or out.stderr
    except subprocess.TimeoutExpired:
        return f"<<< TIMED OUT after {timeout}s -- itself a symptom >>>\n"
    except Exception as exc:  # noqa: BLE001 - a capture must never crash
        return f"<<< FAILED: {exc} >>>\n"


def capture(pid: int, cpu: float, probe_ms: float) -> str:
    os.makedirs(OUT_DIR, exist_ok=True)
    path = os.path.join(OUT_DIR, f"wedge-{now().replace(':', '')}.txt")

    # Written FIRST, before anything is spawned, so that a capture which cannot
    # run its own commands still leaves the one fact it already knows.
    with open(path, "w") as fh:
        fh.write(f"syspolicyd wedge captured {now()}\n"
                 f"  fresh-binary exec took {probe_ms:.0f} ms "
                 f"(healthy is single-digit)\n"
                 f"  syspolicyd pid {pid} at {cpu:.0f}% of one core\n\n")
        fh.flush()

        fh.write("=== open handles (which paths is it walking) ===\n")
        fh.write(run(["sudo", "-n", "lsof", "-p", str(pid)], 60))
        fh.flush()

        fh.write("\n=== syscalls, 10s ===\n")
        fh.write(run(["sudo", "-n", "fs_usage", "-w", "-f", "filesys", str(pid)], 12))
        fh.flush()

        fh.write("\n=== its own log, 10s ===\n")
        fh.write(
            run(
                [
                    "log", "stream", "--style", "compact",
                    "--predicate", 'process == "syspolicyd"',
                ],
                12,
            )
        )
        fh.flush()

        fh.write("\n=== fresh executables per repo, last 15 min ===\n")
        fh.write(
            run(
                [
                    "bash", "-c",
                    "cd ~/Work/Projects/CortexKit 2>/dev/null || exit 0; "
                    "for d in */target/debug/deps; do [ -d \"$d\" ] || continue; "
                    "n=$(find \"$d\" -maxdepth 1 -type f -perm +111 -mmin -15 "
                    "2>/dev/null | wc -l | tr -d ' '); "
                    "[ \"$n\" -gt 0 ] && echo \"${d%%/*} $n\"; done",
                ],
                60,
            )
        )
    return path


def main() -> int:
    print(
        f"{now()} capture armed (fires when a fresh-binary exec exceeds "
        f"{PROBE_TRIGGER_MS:.0f} ms; output {OUT_DIR})",
        flush=True,
    )
    last = 0.0
    while True:
        time.sleep(SAMPLE_SECS)
        elapsed = probe_exec_ms(OUT_DIR)
        if elapsed < 0:
            continue
        if elapsed < PROBE_TRIGGER_MS:
            continue
        # One consecutive reading is enough here, unlike the CPU version: a
        # multi-second exec of /bin/echo is not something a healthy machine does
        # transiently, and waiting for a second sample spends another interval of
        # an episode that is already underway.
        if time.monotonic() - last < COOLDOWN_SECS:
            continue
        state = syspolicyd()
        pid, cpu = state if state else (0, 0.0)
        print(f"{now()} TRIGGER exec took {elapsed:.0f} ms "
              f"(syspolicyd pid {pid} at {cpu:.0f}%)", flush=True)
        path = capture(pid, cpu, elapsed)
        print(f"{now()} captured -> {path}", flush=True)
        last = time.monotonic()


if __name__ == "__main__":
    sys.exit(main())
