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

IDENTIFIER NOTE, because the daemon side plans to join on correlation ids and THIS
INSTRUMENT CANNOT SUPPLY THEM FOR HEALTHY REQUESTS. The CLI prints `local_port=N
channel 0 corr N` only inside its own timeout error, so a request that answers
exposes no identifier at all, and one killed at THIS script's deadline never gets
to print one either. What the join can rely on:

    healthy request   wall-clock time and cadence position only
    CLI-timed-out     full `local_port` + `corr` in the preserved error text
    killed by us      nothing

So daemon-side arrival records must be joined to these samples BY TIME, not by id,
and the sample offsets are printed to make that possible. Raising this script's
timeout above the CLI's would convert the third row into the second and recover
ids for hung requests -- deliberately not done, because the CLI's timeout is 30s
and a longer one would stop distinguishing "the CLI gave up" from "the request is
still outstanding", which is a distinction the hunt needs.
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
IN_FLIGHT_ARM = "drain-with-in-flight"
# Sacrificial and cheap to bounce, chosen by the daemon's owner.
IN_FLIGHT_MODULE = "entorhinal"
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
        # No correlation id is recoverable here: the CLI prints one only in its own
        # timeout error, and killing it at OUR deadline means that error was never
        # written. Deliberately left as-is rather than raising this script's
        # timeout above the CLI's -- see IDENTIFIER NOTE below.
        return time.monotonic() - started, "HUNG"
    elapsed = time.monotonic() - started
    if completed.returncode != 0:
        detail = (completed.stderr or "").strip().splitlines()
        # The CLI's own timeout message carries `local_port=N channel 0 corr N`,
        # which is the only identifier this instrument can offer for a join
        # against daemon-side records. Preserved verbatim rather than summarised.
        return elapsed, f"REFUSED ({detail[0][:96]})" if detail else "REFUSED"
    return elapsed, "ok"


def in_flight_probe(cli: Path, module: str) -> subprocess.Popen[str]:
    """Start a health probe and return without waiting for it.

    `ck health <module>` issues a supervisor.health_probe: a fresh one-shot probe
    that waits on the MODULE's reply over the daemon's control path. Issued shortly
    before that module is drained, it is guaranteed to be mid-flight when the drain
    begins, because the module it is waiting on is the one being torn down.

    PURE CHANNEL 0, which is the point. A slow data-plane call would be mid-flight
    on a route channel and would test the adjacent question -- the defect under
    hunt is on the control dispatch path.
    """
    return subprocess.Popen(
        [str(cli), "health", module],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def resolve_probe(
    process: subprocess.Popen[str], started: float, resolved_at: float | None
) -> tuple[float, str]:
    """Wait for the in-flight probe and classify HOW it resolved, not just when.

    THE KIND IS WHERE A STALL SHOWS. A probe that answers after the module
    respawns and one that gives up at its deadline can take a similar time and mean
    opposite things: the first says the control path stayed alive across the
    teardown and delivered a real reply, the second says the request died waiting.
    Latency alone cannot separate them, so both are recorded.

    `resolved_at` IS THE PROBE'S OWN COMPLETION, observed by polling during the
    cadence loop. The first version of this timed the probe from issue to the
    moment the loop got round to reaping it, and reported 15.007s for a probe that
    had answered in milliseconds -- measuring this script's scheduling rather than
    the daemon's latency. That is the instrument-reports-on-its-own-subject defect
    this repository has written up three times, caught here only because the dry
    run had no drain and 15s was implausible.
    """
    try:
        stdout, stderr = process.communicate(timeout=REQUEST_TIMEOUT_SECS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate()
        return time.monotonic() - started, "HUNG (no resolution)"
    elapsed = (resolved_at - started) if resolved_at is not None else (
        time.monotonic() - started
    )
    if process.returncode == 0:
        first = (stdout or "").strip().splitlines()
        head = first[0][:56] if first else ""
        return elapsed, f"ANSWERED ({head})"
    detail = (stderr or stdout or "").strip().splitlines()
    head = detail[0][:56] if detail else f"exit {process.returncode}"
    return elapsed, f"UNRESOLVED ({head})"


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
    probe: subprocess.Popen[str] | None = None
    probe_started = 0.0
    probe_resolved_at: float | None = None
    results: list[tuple[float, float, str]] = []
    for index in range(samples):
        due = run_started + index * CADENCE_SECS
        # Sleep to the SCHEDULE, not for a fixed interval: a slow request must not
        # push later samples out of the drain window it is supposed to probe.
        # Poll the in-flight probe while waiting, so its resolution time is its
        # own rather than whenever this loop next looks at it.
        while (delay := due - time.monotonic()) > 0:
            if probe is not None and probe_resolved_at is None and probe.poll() is not None:
                probe_resolved_at = time.monotonic()
            time.sleep(min(delay, 0.02))
        offset = time.monotonic() - run_started
        # Launched one cadence tick before the operator triggers the drain, so it
        # is provably mid-flight when the teardown begins rather than probably so.
        if arm == IN_FLIGHT_ARM and index == samples // 2 - 1:
            probe_started = time.monotonic()
            probe = in_flight_probe(cli, IN_FLIGHT_MODULE)
            print(f"{offset:7.1f}  {'--':>8}  in-flight probe issued on "
                  f"{IN_FLIGHT_MODULE}; TRIGGER THE DRAIN NOW", flush=True)
        # BLOCKING, AND THAT BOUNDS WHAT THIS ARM CAN ANSWER. The loop cannot
        # issue sample N+1 until N returns, so a window that hangs requests
        # yields ONE sample inside it however short the cadence is, followed by
        # a catch-up burst as the missed schedule slots fire back to back.
        #
        # The burst is an artefact of this line and carries no information about
        # recovery -- bunched sub-10ms samples after an outlier are this loop
        # catching up, not the daemon healing. Any report from this arm must say
        # so in the body, because the tail looks exactly like a fast recovery.
        #
        # Deliberately kept for the binary question, which one sample settles:
        # the request is catalog.list, which names no module, so a single sample
        # hanging for a drain's duration proves a drain starves callers unrelated
        # to it. Anything distributional -- onset, clearance, total vs partial --
        # needs concurrent sampling and is a different instrument state, verified
        # between windows rather than inside one.
        elapsed, verdict = one_request(cli)
        results.append((offset, elapsed, verdict))
        print(f"{offset:7.1f}  {elapsed:8.3f}  {verdict}", flush=True)

    probe_result: tuple[float, float, str] | None = None
    if probe is not None:
        elapsed, kind = resolve_probe(probe, probe_started, probe_resolved_at)
        print(f"{'in-flight':>7}  {elapsed:8.3f}  {kind}", flush=True)
        probe_result = (probe_started - run_started, elapsed, kind)

    # The probe is reported separately and NOT folded into the cadence
    # distribution: it is a different operation with a different expected
    # duration, and averaging it in would corrupt the very baseline the cadence
    # samples exist to establish.
    good = [elapsed for _, elapsed, verdict in results if verdict == "ok"]
    bad = [(offset, elapsed, verdict) for offset, elapsed, verdict in results
           if verdict != "ok"]
    if probe_result is not None and not probe_result[2].startswith("ANSWERED"):
        bad.append(probe_result)

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
