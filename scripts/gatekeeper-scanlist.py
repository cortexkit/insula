#!/usr/bin/env python3
"""Report when macOS Gatekeeper's scan list has filled with dead build artifacts.

WHY THIS EXISTS. syspolicyd keeps a list of executables to assess in
`/var/db/SystemPolicyConfiguration/ExecPolicy`, table `scan_targets_v2`, keyed by
PATH. Rust builds write thousands of hash-named binaries; cargo later deletes
them; NOTHING PRUNES THE ROWS. syspolicyd then re-walks the directories looking
for files that are gone.

Measured on this machine 2026-08-26, with no build running at all:

    scan_targets_v2                104,634 rows
    of those under a target/ dir    95,628 (91%)
    sampled for existence           60 of 60 GONE
    syspolicyd                      40-137% of one core
    getdirentries64 in 12s          155,432  (~13k/sec)

After clearing the table the same measurement read 0.0-3.9%. That is the whole
case: the cost tracks the dead-entry count, not build activity.

WHAT IT DOES NOT CLAIM. A high count is not proof of a stall. It is a leading
indicator, and the reason to report a RATIO alongside the raw count is that a
machine with 100k LIVE entries is a different situation from one with 100k dead
ones and wants a different response.

WHY A SCRIPT RATHER THAN A NOTE. The failure is silent and gradual: nothing
alerts, the machine just gets slower over weeks, and by the time it is obvious
the cause looks like everything else that is running. The signal has to be
pulled, and it is one query.

EXIT CODES follow the repo convention:
    0  checked, healthy
    1  checked, maintenance indicated
    2  could not check (no sudo, missing db, unreadable)

CLEARING IT NEEDS RECOVERY. The file is SIP-protected while booted, so `rm`
fails even under sudo. Reboot holding the power button, Options, Utilities,
Terminal, then remove ExecPolicy* from the data volume and reboot normally. SIP
does NOT need to be disabled -- Recovery is not subject to it.
"""

from __future__ import annotations

import os
import subprocess
import sys

# Overridable so the detection path can be PROVED against a synthetic database.
# Without this the warning branch is only reachable on a machine that has already
# accumulated tens of thousands of dead entries -- i.e. exactly the state the
# check exists to prevent you from reaching. A guard whose firing path cannot be
# exercised is a guard nobody has measured.
DB = os.environ.get(
    "CK_SCANLIST_DB", "/var/db/SystemPolicyConfiguration/ExecPolicy"
)

# The real database is root-owned; a synthetic one under test is not, and
# demanding sudo for it would make the test prove something about sudo.
NEEDS_SUDO = DB == "/var/db/SystemPolicyConfiguration/ExecPolicy"

# Chosen from the measured curve rather than picked round. The machine was
# comfortable at a few thousand and unusable near 100k, and accumulation ran
# about 5k/day, so 40k is roughly a week of headroom before the symptom bites.
# It is a warning line, not a cliff: nothing changes character at 40,000.
DEAD_ENTRY_WARN = 40_000

# A ratio alone is misleading on a nearly-empty table -- 3 dead of 4 rows is
# 75% and matters to nobody. Both conditions must hold before reporting.
DEAD_RATIO_WARN = 0.5

# Sampling exists because stat()ing 100k paths is slow and pointless: the
# question is "roughly what share of these are gone", and a few hundred answers
# it. Reported as an estimate, never as a count.
SAMPLE_SIZE = 400


def query(sql: str) -> str | None:
    """Run one read-only query, or None if the database cannot be read."""
    # NOT `sudo -n`. This machine authenticates sudo with Touch ID, which is
    # interactive but not a password prompt -- `-n` refuses it outright and the
    # check reports "unreadable" on a perfectly readable database. Caught by
    # running this against a host whose state was already known.
    argv = (["sudo"] if NEEDS_SUDO else []) + ["sqlite3", DB, sql]
    try:
        out = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=120,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError):
        return None
    if out.returncode != 0:
        return None
    return out.stdout.strip()


def main() -> int:
    if sys.platform != "darwin":
        print("could not check: macOS only")
        return 2
    if not os.path.exists(DB):
        print(f"could not check: {DB} not present")
        return 2

    total = query("select count(*) from scan_targets_v2;")
    if total is None:
        print("could not check: database unreadable (sudo declined or timed out)")
        print("  run `sudo -v` first if the authentication prompt was missed")
        return 2

    total_n = int(total)
    if total_n == 0:
        print("scan list is empty -- healthy (0 entries)")
        return 0

    paths = query(
        f"select path from scan_targets_v2 order by random() limit {SAMPLE_SIZE};"
    )
    if paths is None:
        print("could not check: sampling query failed")
        return 2

    sample = [p for p in paths.splitlines() if p]
    if not sample:
        print("could not check: sample came back empty on a non-empty table")
        return 2

    gone = sum(1 for p in sample if not os.path.exists(p))
    ratio = gone / len(sample)
    dead_est = int(total_n * ratio)

    print(f"scan_targets_v2:      {total_n:,} rows")
    print(f"sampled:              {len(sample)} paths, {gone} gone ({ratio:.0%})")
    print(f"estimated dead:       ~{dead_est:,} entries")

    # Reported for orientation, never as the trigger: which repos dominate tells
    # you where the churn comes from, but the decision is on the dead count.
    top = query(
        """select count(*), substr(path, 1, instr(path,'/target/')+7)
           from scan_targets_v2 where path like '%/target/%'
           group by 2 order by 1 desc limit 3;"""
    )
    if top:
        for line in top.splitlines():
            if "|" in line:
                n, repo = line.split("|", 1)
                print(f"  {int(n):>7,}  {repo}")

    if dead_est >= DEAD_ENTRY_WARN and ratio >= DEAD_RATIO_WARN:
        print()
        print(f"MAINTENANCE INDICATED: ~{dead_est:,} dead entries at {ratio:.0%}")
        print("syspolicyd re-walks these directories; cost tracks this count.")
        print("Clear from Recovery (SIP blocks it while booted):")
        print("  reboot holding power -> Options -> Utilities -> Terminal")
        print("  rm /Volumes/*/private/var/db/SystemPolicyConfiguration/ExecPolicy*")
        return 1

    print()
    print(f"healthy (warns at {DEAD_ENTRY_WARN:,} dead and {DEAD_RATIO_WARN:.0%})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
