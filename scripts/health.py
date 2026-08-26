#!/usr/bin/env python3
"""Read this module's health metrics without inventing any.

WHY THIS EXISTS RATHER THAN A DISCIPLINE. Every ad-hoc probe of this surface has
been a hand-written `json.load` plus `metrics.get(key) or 0`, and that default
turns an ABSENT field into a stated zero. It produced a fabricated datum on
2026-08-26 -- `uptimeSecs` is not a published field, the probe printed `0.0h`, and
a restart that never happened was reported to a peer, who spent a log pass
disproving it.

The first remedy written down was "print the raw value before deriving anything",
which is vigilance: a rule whose only enforcement is care, and this repository has
a rule about those. This script is the mechanical version. It removes the site
where the mistake happens by never requiring anyone to name a key or supply a
default: it prints EVERY published metric with its raw value, and any key asked
for by name that is not published is rendered ABSENT rather than defaulted.

Exit codes follow the repo convention: 0 checked and clean, 1 checked with
findings, 2 could not check.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
from pathlib import Path

DAEMON_CLI_CANDIDATES = (
    Path.home() / "Work/Projects/CortexKit/subconscious/target/release/ck",
    Path.home() / ".local/share/cortexkit/bin/ck",
)
MODULE_ID = "insula"
CALL_TIMEOUT_SECS = 45


def locate_cli() -> Path | None:
    for candidate in DAEMON_CLI_CANDIDATES:
        if candidate.is_file():
            return candidate
    found = shutil.which("ck")
    return Path(found) if found else None


def render(value: object) -> str:
    """Render a metric value without collapsing absence into anything else.

    `None` on the wire is a STATED null -- the module said "no value" -- which is
    a different fact from a key that was never published, and both are different
    from zero. Absent is handled by the caller, which knows the key was missing;
    this only has to refuse to make a stated null look like a number.
    """
    if value is None:
        return "null (stated)"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (list, dict)):
        return json.dumps(value)
    return str(value)


def main() -> int:
    cli = locate_cli()
    if cli is None:
        print("could not check: no `ck` binary found", file=sys.stderr)
        return 2

    try:
        completed = subprocess.run(
            [str(cli), "module", "status", MODULE_ID, "--json"],
            capture_output=True,
            text=True,
            timeout=CALL_TIMEOUT_SECS,
        )
    except subprocess.TimeoutExpired:
        print(
            f"could not check: `ck module status` did not answer in "
            f"{CALL_TIMEOUT_SECS}s (the daemon control plane has stalled before, "
            f"and the data plane can be healthy while it does)",
            file=sys.stderr,
        )
        return 2

    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip().splitlines()
        first = detail[0] if detail else f"exit {completed.returncode}"
        print(f"could not check: {first}", file=sys.stderr)
        return 2

    try:
        payload = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        print(f"could not check: status was not JSON ({error})", file=sys.stderr)
        return 2

    health = payload.get("health")
    if not isinstance(health, dict):
        print("could not check: status carried no health object", file=sys.stderr)
        return 2

    metrics = health.get("metrics")
    if not isinstance(metrics, dict):
        print("could not check: health carried no metrics object", file=sys.stderr)
        return 2

    print(f"status: {render(health.get('status'))}")
    detail = health.get("detail")
    if detail is not None:
        print(f"detail: {render(detail)}")
    print(f"metrics ({len(metrics)} published):")
    for key in sorted(metrics):
        print(f"  {key} = {render(metrics[key])}")

    # Keys named on the command line are the ones a caller was about to read, and
    # naming a key that is not published is the exact mistake this script exists
    # to make loud. Reported as a finding rather than printed as a value, because
    # a caller asking for it has a derivation waiting for it.
    asked = sys.argv[1:]
    missing = [key for key in asked if key not in metrics]
    for key in asked:
        if key in metrics:
            print(f"asked: {key} = {render(metrics[key])}")
    for key in missing:
        print(f"asked: {key} = ABSENT (not a published metric)")

    if missing:
        print(
            f"\n{len(missing)} requested metric(s) are not published: "
            f"{', '.join(missing)}",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
