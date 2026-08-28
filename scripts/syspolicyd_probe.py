#!/usr/bin/env python3
"""Shared exec-latency probe for the syspolicyd watcher and watchdog.

ONE IMPLEMENTATION, TWO READERS, DELIBERATELY. The capture script records a wedge
and the watchdog acts on one, so they must agree on what a wedge IS. Two copies of
this logic would be two answers to that question, free to drift -- and the drift
would be invisible, because each script would keep passing its own tests. This
repo has already been bitten by exactly that shape: a checker re-implemented the
module's credential-file permission rules and reported three lanes healthy while
the module served none.

WHY EXEC LATENCY AND NOT CPU. The first version of both instruments triggered on
syspolicyd CPU. The operator's correction killed that premise: the lockups happen
"when the wedge happens, there's not much cpu load, it doesn't happen on cpu
load". The mechanism measured afterwards agrees -- first-exec validation is a
SERIALISATION POINT, and a process waiting on a round trip burns no CPU while
every exec on the machine queues behind it. CPU was a proxy that happens to
correlate in some episodes (40-137% of a core was real) and to read healthy
through others. Exec latency IS the symptom, so it cannot miss one.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import time

# Healthy first-exec of a real Rust binary measured 400-1052 ms here (median 594).
# 3000 sits ~5x above the healthy median and far below a wedge, where execs run to
# tens of seconds. Wide in both directions rather than tuned to one sample.
PROBE_TRIGGER_MS = 3000.0

_PROBE_SRC = ""


def probe_source() -> str:
    """A binary from the EXPENSIVE set, resolved once.

    THE SOURCE BINARY IS NOT ARBITRARY, and picking it wrong makes the probe read
    healthy through an episode. Measured on this machine: fresh copies of
    /bin/echo, /bin/ls and /usr/bin/git cost ~1.5 ms, while fresh copies of
    /usr/bin/true and of real Rust test binaries cost 250-620 ms. Something
    distinguishes those two sets that nobody has identified -- not size, not
    signature presence, not content caching, all tested and ruled out.

    So this prefers a REAL Rust test binary from the fleet, which is both the
    population that actually stalls and a measured member of the expensive set.
    `/usr/bin/true` is the fallback because it is the only expensive member
    guaranteed to exist. `/bin/echo` is deliberately NOT used: an earlier version
    of the probe used it, read 2 ms, and would have reported health straight
    through a wedge.

    Cached because the search walks the fleet's build directories, and doing that
    every few seconds would make the watcher part of the load it watches for.
    """
    global _PROBE_SRC
    if _PROBE_SRC:
        return _PROBE_SRC
    root = os.path.expanduser("~/Work/Projects/CortexKit")
    skip = (".rlib", ".rmeta", ".d", ".o", ".dylib", ".so", ".a")
    if os.path.isdir(root):
        for repo in sorted(os.listdir(root)):
            deps = os.path.join(root, repo, "target", "debug", "deps")
            if not os.path.isdir(deps):
                continue
            try:
                names = os.listdir(deps)
            except OSError:
                continue
            for f in names:
                cand = os.path.join(deps, f)
                if f.endswith(skip):
                    continue
                try:
                    ok = (os.path.isfile(cand) and os.access(cand, os.X_OK)
                          and os.path.getsize(cand) > 100_000)
                except OSError:
                    continue
                if ok:
                    _PROBE_SRC = cand
                    return _PROBE_SRC
    _PROBE_SRC = "/usr/bin/true"
    return _PROBE_SRC


def probe_exec_ms(work_dir: str) -> float:
    """Time the first exec of a freshly written binary -- the thing that stalls.

    Returns milliseconds, 120_000.0 if the exec itself timed out (the strongest
    possible reading, not an instrument failure), or -1.0 if the probe could not
    run at all. Callers MUST distinguish -1.0 from a large number: one says the
    machine is stalled, the other says nothing was measured.

    Copies a system binary rather than compiling one: the wedge blocks exec, and a
    probe needing a toolchain would be the first thing to stop working.

    A FIXED PATH, REWRITTEN, not a fresh name each time. This matters more than it
    looks: `scan_targets_v2` has PRIMARY KEY (path), so a unique probe name every
    interval would add a row every interval -- about 17k/day at a 5s cadence,
    roughly three times this machine's natural accumulation rate. The watcher would
    then be feeding the exact pile it was written to diagnose.

    Rewriting one path avoids that AND still pays the full cost: measured 244 ms
    for a rewritten same-path binary versus 4 ms to re-exec an untouched one.
    Validation keys on the BYTES, the scan list keys on the NAME, and this probe
    sits deliberately on the right side of both.

    `--list` IS LOAD-BEARING AND NEARLY SHIPPED MISSING. The preferred source is a
    Rust TEST binary, and exec'ing one with no arguments RUNS ITS TEST SUITE. On an
    idle machine that measured 31,944 ms against a 3,000 ms trigger -- so the
    watchdog would have killed syspolicyd every ten minutes, forever, on a
    perfectly healthy host, while every number in the log looked like a wedge.
    With `--list` the same binary reads 414 ms: it enumerates tests and exits,
    which is exactly the first-exec cost this is meant to time. The fallback
    `/usr/bin/true` ignores arguments, so one call site serves both.

    The general form, which this repo keeps re-learning: an instrument that
    returns a PLAUSIBLE number for the wrong reason is worse than one that fails,
    because everything downstream stays internally consistent.
    """
    path = os.path.join(work_dir, "probe-fixed-path")
    try:
        os.makedirs(work_dir, exist_ok=True)
        shutil.copy(probe_source(), path)
        os.chmod(path, 0o755)
        start = time.perf_counter()
        subprocess.run([path, "--list"], capture_output=True, timeout=120)
        return (time.perf_counter() - start) * 1000.0
    except subprocess.TimeoutExpired:
        return 120_000.0
    except Exception:  # noqa: BLE001 - a probe must never take its caller down
        return -1.0
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass
