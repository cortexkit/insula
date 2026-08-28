#!/usr/bin/env python3
"""Restart syspolicyd when it wedges, so a human does not have to.

THE PROBLEM THIS SOLVES IS NOT THE SPIN, IT IS THE RECOVERY. When syspolicyd
wedges on this machine, every code-signature evaluation on the system blocks --
which means NO NEW PROCESS CAN LAUNCH. The remedy is one command, and the state
that requires it is precisely the state in which you cannot open a terminal to
type it. Measured here 2026-08-26: Ufuk could not open Terminal, and could not
reach the agent to ask, until he killed it from an already-running shell.

`sudo killall syspolicyd` is safe and immediate: launchd restarts it within a
second, and it was done four times during diagnosis with no ill effect. The only
cost is that in-flight assessments are re-done.

WHAT IT WATCHES, AND A PREMISE THAT WAS WRONG. This first triggered on syspolicyd
CPU alone. The operator killed that premise with one sentence -- the lockups
happen "when the wedge happens, there's not much cpu load, it doesn't happen on
cpu load" -- and the mechanism measured afterwards agrees: first-exec validation
is a SERIALISATION POINT, so a process waiting on a round trip burns no CPU while
every exec on the machine queues behind it.

CPU was not disproved, it was demoted. 40-137% of a core was real in several
episodes. But a proxy that correlates in some episodes and reads healthy through
others cannot be the sole trigger for the instrument that ACTS. So the primary
trigger is now EXEC LATENCY, which is the symptom itself and therefore cannot
miss an episode, with CPU kept as a second sufficient arm.

Either arm firing is enough; both must SUSTAIN. A brief burst is NORMAL and must
not trigger a kill: syspolicyd legitimately burns ~29s of CPU in its first 49
seconds after a restart while it rebuilds state. Killing during that would produce
a restart loop that looks exactly like the fault it is meant to cure.

WHY NOT WATCH THE DIRECTORY HANDLES INSTEAD. The wedge correlates with syspolicyd
holding open `target/debug/deps` handles, which is a sharper signal -- but reading
it needs `lsof` against a root process on every sample, which is heavy enough to
matter on a machine already in trouble.

THE PROBE IS SHARED WITH THE CAPTURE SCRIPT, not copied. They must agree on what a
wedge is: one records an episode, this one acts on it. Two implementations would
be two answers to that question, free to drift while each kept passing its own
checks.

FAILURE DIRECTION. This errs toward NOT killing. A missed wedge costs what today
already costs; a spurious kill costs re-assessment plus the risk of a loop. Hence
a high threshold, a long confirmation window, a grace period after any restart,
and a hard rate limit.
"""

from __future__ import annotations

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from syspolicyd_probe import PROBE_TRIGGER_MS, probe_exec_ms  # noqa: E402

# Healthy is 0-4% of a core. Observed wedges ran 40-137% sustained. 80 sits well
# above normal noise and well below the observed floor of a real episode, so the
# gap is wide in both directions rather than tuned to one sample.
#
# SECOND ARM, not the definition. See the module docstring: CPU reads healthy
# through a serialisation stall, which is the shape the operator reported.
CPU_THRESHOLD_PCT = 80.0

# Where the probe writes its fixed-path binary. Under /var/tmp rather than a home
# directory because this runs as root: writing into a user's tree as root leaves
# root-owned files behind that the user then cannot clean up.
PROBE_DIR = "/var/tmp/syspolicyd-watchdog"

# It must stay hot this long before we act. The legitimate startup burst lasted
# ~49s, so 120 clears it with margin. This is the single most important constant
# for not producing a restart loop.
SUSTAIN_SECS = 120

SAMPLE_SECS = 10

# Never act on a syspolicyd younger than this: it is either still doing startup
# work or we just restarted it ourselves.
STARTUP_GRACE_SECS = 180

# A hard ceiling on our own aggression. If the wedge recurs faster than this,
# killing more often is not the answer and the log is the evidence for that.
MIN_SECS_BETWEEN_KILLS = 600

LOG = "/var/log/syspolicyd-watchdog.log"


def log(msg: str) -> None:
    line = f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}"
    print(line, flush=True)
    try:
        with open(LOG, "a") as fh:
            fh.write(line + "\n")
    except OSError:
        # Losing the log must never stop the watchdog doing its job.
        pass


def syspolicyd() -> tuple[int, float, float] | None:
    """Return (pid, cpu_percent, elapsed_secs), or None if not running."""
    try:
        out = subprocess.run(
            ["ps", "-Ao", "pid=,pcpu=,etime=,comm="],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return None

    for line in out.stdout.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 4 or not parts[3].endswith("/syspolicyd"):
            continue
        try:
            pid = int(parts[0])
            cpu = float(parts[1])
        except ValueError:
            continue
        return pid, cpu, parse_etime(parts[2])
    return None


def parse_etime(raw: str) -> float:
    """ps elapsed time: [[dd-]hh:]mm:ss."""
    days = 0
    if "-" in raw:
        d, raw = raw.split("-", 1)
        days = int(d)
    bits = [float(b) for b in raw.split(":")]
    while len(bits) < 3:
        bits.insert(0, 0.0)
    return days * 86400 + bits[0] * 3600 + bits[1] * 60 + bits[2]


def main() -> int:
    if os.geteuid() != 0:
        print("must run as root: it restarts a system daemon", file=sys.stderr)
        return 2

    log(
        f"watchdog started (exec probe >{PROBE_TRIGGER_MS:.0f}ms OR cpu "
        f">{CPU_THRESHOLD_PCT}%, sustained {SUSTAIN_SECS}s, grace "
        f"{STARTUP_GRACE_SECS}s, min gap {MIN_SECS_BETWEEN_KILLS}s)"
    )

    hot_since: float | None = None
    hot_pid: int | None = None
    last_kill = 0.0

    while True:
        time.sleep(SAMPLE_SECS)
        state = syspolicyd()
        if state is None:
            hot_since = None
            continue

        pid, cpu, elapsed = state

        # A different pid means it restarted; any hot streak we were tracking
        # belonged to a process that no longer exists.
        if pid != hot_pid:
            hot_pid, hot_since = pid, None

        # PRIMARY ARM. -1.0 means the probe could not run at all, which says
        # nothing about the machine and must never be read as a large latency --
        # otherwise a broken probe becomes a kill trigger.
        probe_ms = probe_exec_ms(PROBE_DIR)
        probe_hot = probe_ms >= PROBE_TRIGGER_MS
        cpu_hot = cpu >= CPU_THRESHOLD_PCT

        if not (probe_hot or cpu_hot):
            if hot_since is not None:
                log(
                    f"pid {pid} cooled (exec {probe_ms:.0f}ms, cpu {cpu:.0f}%) "
                    "-- streak cleared"
                )
            hot_since = None
            continue

        reason = (
            f"exec {probe_ms:.0f}ms" if probe_hot else f"cpu {cpu:.0f}%"
        )
        now = time.monotonic()
        if hot_since is None:
            hot_since = now
            log(f"pid {pid} hot ({reason}) -- watching")
            continue

        if now - hot_since < SUSTAIN_SECS:
            continue
        if elapsed < STARTUP_GRACE_SECS:
            log(f"pid {pid} hot ({reason}) but only {elapsed:.0f}s old -- grace")
            continue
        if now - last_kill < MIN_SECS_BETWEEN_KILLS:
            log(f"pid {pid} hot ({reason}) but killed recently -- rate limited")
            continue

        log(
            f"RESTARTING pid {pid}: {reason} for {now - hot_since:.0f}s, "
            f"age {elapsed:.0f}s (exec {probe_ms:.0f}ms, cpu {cpu:.0f}%)"
        )
        try:
            subprocess.run(["/usr/bin/killall", "syspolicyd"], timeout=15)
            last_kill = now
        except (subprocess.TimeoutExpired, FileNotFoundError) as exc:
            log(f"restart FAILED: {exc}")
        hot_since = None


if __name__ == "__main__":
    sys.exit(main())
