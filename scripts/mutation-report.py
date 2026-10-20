#!/usr/bin/env python3
"""Summarize replay dispositions and refuse unreviewed broad catches."""

from collections import Counter
import json
from pathlib import Path
import sys


PASSING = {"CAUGHT", "HUB", "EQUIVALENT", "UNREACHABLE", "SKIPPED_PLATFORM", "DESK_ONLY"}


def unreviewed(rows):
    return [row["id"] for row in rows if row["outcome"] not in PASSING]


def self_check():
    assert unreviewed([{"id": "planted", "outcome": "CAUGHT_BROADLY"}]) == ["planted"], "unreviewed broad catch must be refused"
    assert not unreviewed([{"id": "reviewed", "outcome": "HUB"}]), "reviewed HUB must pass"


def main():
    self_check()
    report = Path(sys.argv[1])
    if not report.is_file():
        # The replay stops before grading when its unmutated prebuild fails (a
        # crates.io download error, say), so it writes no report. Say that,
        # rather than a traceback that reads like a defect in this script.
        print(f"no mutation report at {report}: the replay stopped before grading any row; see its log above")
        return 2
    rows = json.loads(report.read_text())
    if not isinstance(rows, list):
        raise ValueError("expected a replay report array")
    counts = Counter(row["outcome"] for row in rows)
    print(f"mutation report: {len(rows)} rows, dispositions {dict(sorted(counts.items()))}")
    refused = unreviewed(rows)
    if refused:
        print(f"refused mutation rows (narrow broad catches or review a shared-property HUB): {refused}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
