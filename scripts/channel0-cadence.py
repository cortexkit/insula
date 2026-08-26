#!/usr/bin/env python3
"""Drive daemon control-plane requests at a fixed cadence and time every one.

Built for a subc-core defect hunt: on 2026-08-26 the daemon's control plane
stopped answering for about two minutes while the data plane kept serving, and the
window was later convicted to a module drain/teardown. The question the hunt turns
on is whether a drain starves EVERY control caller or only callers of the
restarting module, and that needs latency per request rather than a summary.

THREE ARMS, and the script is the same instrument for all three:

    baseline              no drain anywhere. Establishes what normal looks like,
                          without which "hung" has no quantitative meaning.
    drain                 a drain triggered on a sacrificial module mid-run.
    drain-with-in-flight  as above, plus a request deliberately issued before the
                          drain so it is provably mid-flight when it begins.

EVERY REQUEST USES A FRESH CLI PROCESS. That is the arm that killed the first
explanation of the incident -- a long-lived client could be blamed for riding the
restart, and a process that has just started has no history with any module.

Reports every sample rather than an aggregate: a distribution with one 30s outlier
and a distribution of uniformly slow requests have the same mean and different
causes. Exit codes follow the repo convention: 0 checked and clean, 1 checked with
findings (a hang or a refusal), 2 could not check.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
import time
from pathlib import Path

DAEMON_CLI_CANDIDATES = (
    Path.home() / "Work/Projects/CortexKit/subconscious/target/release/ck",
    Path.home() / ".local/share/cortexkit/bin/ck",
)
# Shorter than the daemon's 30s route-drain timeout, so several requests land
# fully INSIDE a drain window rather than straddling its edges. A cadence at or
# above the drain timeout cannot distinguish "slow during" from "slow across".
CADENCE_SECS = 5.0
# Above the daemon's own 30s drain timeout, so a request that is merely waiting
# for a drain to finish is recorded as a long latency rather than truncated into
# a timeout -- the duration is the measurement.
REQUEST_TIMEOUT_SECS = 45.0
DEFAULT_SAMPLES = 12


def locate_cli() -> Path | None:
    for candidate in DAEMON_CLI_CANDIDATES:
        if candidate.is_file():
            return candidate
    found = shutil.which("ck")
    return Path(found) if found else None


def one_request(cli: Path) -> tuple[float, str]:
    """One control-plane request in a fresh process. Returns (seconds, verdict)."""
    started = time.monotonic()
    try:
        completed = subprocess.run(
            [str(cli), "module", "list", "--json"],
            capture_output=True,
            text=True,
            timeout=REQUEST_TIMEOUT_SECS,
        )
    except subprocess.TimeoutExpired:
        return time.monotonic() - started, "HUNG"
    elapsed = time.monotonic() - started
    if completed.returncode != 0:
        detail = (completed.stderr or "").strip().splitlines()
        return elapsed, f"REFUSED ({detail[0][:48]})" if detail else "REFUSED"
    return elapsed, "ok"


def main() -> int:
    cli = locate_cli()
    if cli is None:
        print("could not check: no `ck` binary found", file=sys.stderr)
        return 2

    arm = sys.argv[1] if len(sys.argv) > 1 else "baseline"
    try:
        samples = int(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_SAMPLES
    except ValueError:
        print("could not check: sample count must be an integer", file=sys.stderr)
        return 2

    print(f"arm={arm} cadence={CADENCE_SECS}s samples={samples} "
          f"timeout={REQUEST_TIMEOUT_SECS}s")
    print(f"{'t+':>7}  {'elapsed':>8}  verdict")

    run_started = time.monotonic()
    results: list[tuple[float, float, str]] = []
    for index in range(samples):
        due = run_started + index * CADENCE_SECS
        # Sleep to the SCHEDULE, not for a fixed interval: a slow request must not
        # push later samples out of the drain window it is supposed to probe.
        delay = due - time.monotonic()
        if delay > 0:
            time.sleep(delay)
        offset = time.monotonic() - run_started
        elapsed, verdict = one_request(cli)
        results.append((offset, elapsed, verdict))
        print(f"{offset:7.1f}  {elapsed:8.3f}  {verdict}", flush=True)

    good = [elapsed for _, elapsed, verdict in results if verdict == "ok"]
    bad = [(offset, elapsed, verdict) for offset, elapsed, verdict in results
           if verdict != "ok"]

    print()
    if good:
        ordered = sorted(good)
        print(f"answered {len(good)}/{len(results)}  "
              f"min {ordered[0]:.3f}s  median {ordered[len(ordered) // 2]:.3f}s  "
              f"max {ordered[-1]:.3f}s")
    else:
        print(f"answered 0/{len(results)}")
    for offset, elapsed, verdict in bad:
        print(f"  t+{offset:.1f}s  {elapsed:.3f}s  {verdict}")

    if bad:
        print(f"\n{len(bad)} request(s) did not answer normally", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
