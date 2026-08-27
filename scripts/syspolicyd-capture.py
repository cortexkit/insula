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
import subprocess
import sys
import time

# PERCENT OF ONE CORE, the `ps -o %cpu` convention -- so 137 means 1.37 cores, and
# on this 18-core machine full capacity is 1800. Healthy syspolicyd is 0-4;
# observed wedges ran 40-137 sustained.
#
# WHICH MEANS THE WEDGE IS NOT CPU STARVATION, and reading this number as load is
# the wrong lesson to draw from it. 1.37 cores out of 18 is 7.6% of the machine:
# it cannot stall anything by consuming capacity, and during the episodes the
# other 16 cores sat idle. syspolicyd stalls the machine because EVERY exec must
# be validated by IT SPECIFICALLY -- it is a serialisation point, not a hog. The
# remedy is therefore fewer or cheaper validations, never more cores.
#
# So this threshold is not a load measurement. It is a proxy for "syspolicyd is
# doing far more work than its steady state", which is the only externally visible
# sign that its queue is backed up. Verified that `ps -o %cpu` is current enough to
# trigger on: against a known 100%-of-one-core burner it read 98.4 within 2
# seconds, so it is not a long-decayed average.
TRIGGER_PCT = 60.0

# Two consecutive samples, so a momentary spike during normal validation work does
# not fire it. Deliberately much shorter than the watchdog's 120s sustain: this
# wants to catch the ONSET, and waiting two minutes to be sure would record the
# middle of an episode rather than its beginning.
SAMPLE_SECS = 5
CONSECUTIVE = 2

# Do not re-capture the same episode over and over.
COOLDOWN_SECS = 900

OUT_DIR = os.path.expanduser("~/.local/state/cortexkit/syspolicyd-captures")


def now() -> str:
    return datetime.datetime.now().strftime("%Y-%m-%dT%H:%M:%S")


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


def capture(pid: int, cpu: float) -> str:
    os.makedirs(OUT_DIR, exist_ok=True)
    path = os.path.join(OUT_DIR, f"wedge-{now().replace(':', '')}.txt")

    # Written FIRST, before anything is spawned, so that a capture which cannot
    # run its own commands still leaves the one fact it already knows.
    with open(path, "w") as fh:
        fh.write(f"syspolicyd wedge captured {now()}\n  pid {pid} at {cpu:.0f}%\n\n")
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
    print(f"{now()} capture armed (trigger {TRIGGER_PCT}% x{CONSECUTIVE} "
          f"@{SAMPLE_SECS}s, output {OUT_DIR})", flush=True)
    hot = 0
    last = 0.0
    while True:
        time.sleep(SAMPLE_SECS)
        state = syspolicyd()
        if state is None:
            hot = 0
            continue
        pid, cpu = state
        if cpu < TRIGGER_PCT:
            hot = 0
            continue
        hot += 1
        if hot < CONSECUTIVE:
            continue
        if time.monotonic() - last < COOLDOWN_SECS:
            continue
        print(f"{now()} TRIGGER pid {pid} at {cpu:.0f}%", flush=True)
        path = capture(pid, cpu)
        print(f"{now()} captured -> {path}", flush=True)
        last = time.monotonic()
        hot = 0


if __name__ == "__main__":
    sys.exit(main())
