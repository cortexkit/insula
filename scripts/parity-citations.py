#!/usr/bin/env python3
"""Report provider source citations whose upstream file no longer exists.

WHY THIS EXISTS. Provider modules cite CodexBar source by file and line, as the
provenance for a fixture-verified port. Those citations were accurate when
written, against whatever tag the port was verified at -- and upstream keeps
moving. When a cited file is DELETED upstream, the citation does not merely
drift, it becomes unfollowable: a reader at the current anchor finds nothing and
cannot tell whether the citation was wrong, the file moved, or the whole
mechanism was replaced.

Eight were already in this state when this script was written, all from the same
cause: the upstream QuickJS/TypeScript plugin-engine migration replaced several
Swift fetchers with plugin JS. Nothing said so, because nothing was looking.

THIS IS A PARITY-ROUND INSTRUMENT, not a gate. A dead citation is not a defect in
our code -- the port it documents is still correct and still verified -- so
failing a build on it would be wrong. What it does is tell the parity round which
citations to re-anchor or annotate, at the one moment someone has the upstream
checkout open.

The line numbers are deliberately NOT checked. A line number's accuracy is only
meaningful against a specific tag, and re-deriving which tag each citation was
verified at is not something this script can do. File existence is the signal
that is unambiguous either way.

Exit codes follow this repo's convention: 0 clean, 1 findings, 2 uncheckable.
"""

from __future__ import annotations

import pathlib
import re
import subprocess
import sys

# `+` is here because Swift extension files are named
# `MiniMaxUsageFetcher+ModelMapping.swift`, and digits because of
# `Sub2APIUsageFetcher.swift`. BOTH WERE MISSING FROM EARLIER VERSIONS OF THIS
# PATTERN AND BOTH FAILED THE SAME WAY: the regex matched a SUFFIX of the real
# filename, yielding a name that looks entirely plausible, is absent upstream,
# and is therefore reported as a finding.
#
# So a too-narrow class here does not under-report, it INVENTS -- the worst
# direction for a checker, because a phantom is indistinguishable from a real hit
# and costs someone a hunt through a repository they do not own. The digits
# omission was caught within the hour; the `+` omission shipped.
CITATION = re.compile(r"([A-Za-z_][A-Za-z0-9_+]*\.swift)")


def anchored_tag(repo_root: pathlib.Path) -> str | None:
    """The parity tag docs/provider-matrix.md currently claims.

    Read from the doc rather than passed in, so the script and the recorded
    anchor cannot disagree -- checking against a tag nobody anchored to would
    produce findings that mean nothing.
    """
    matrix = repo_root / "docs" / "provider-matrix.md"
    if not matrix.is_file():
        return None
    tags = re.findall(r"\bv\d+\.\d+\.\d+\b", matrix.read_text())
    if not tags:
        return None
    # The newest by version order, not by position: the file records a history of
    # rounds, and the most recent anchor is the one to check against.
    return max(tags, key=lambda t: [int(p) for p in t.lstrip("v").split(".")])


def main() -> int:
    repo_root = pathlib.Path(__file__).resolve().parents[1]
    upstream = pathlib.Path.home() / "Work" / "OSS" / "CodexBar"

    if not (upstream / ".git").exists():
        print(f"refusing: no CodexBar checkout at {upstream}", file=sys.stderr)
        print("this check needs the upstream tree; it says nothing without it", file=sys.stderr)
        return 2

    tag = anchored_tag(repo_root)
    if tag is None:
        print("refusing: no parity tag found in docs/provider-matrix.md", file=sys.stderr)
        return 2

    listing = subprocess.run(
        ["git", "-C", str(upstream), "ls-tree", "-r", "--name-only", tag],
        capture_output=True,
        text=True,
    )
    if listing.returncode != 0:
        print(f"refusing: cannot list {tag} in {upstream}", file=sys.stderr)
        print(listing.stderr.strip(), file=sys.stderr)
        return 2
    tree = [line for line in listing.stdout.split("\n") if line]

    cited: dict[str, set[str]] = {}
    for path in sorted((repo_root / "crates").rglob("src/*.rs")):
        for name in CITATION.findall(path.read_text()):
            cited.setdefault(name, set()).add(path.name)

    if not cited:
        print("refusing: found no upstream citations at all -- the pattern is broken,", file=sys.stderr)
        print("not the codebase; a zero here reads exactly like a clean run", file=sys.stderr)
        return 2

    present = {name for name in cited if any(entry.endswith("/" + name) for entry in tree)}
    gone = {name: sorted(where) for name, where in cited.items() if name not in present}

    # A module that NAMES THIS SCRIPT has been through the annotation: its dead
    # citations carry the tag they were verified at, or an explanation of what
    # replaced them upstream. Those are answered, not outstanding.
    #
    # WITHOUT THIS SPLIT THE INSTRUMENT IS PERMANENTLY RED, and a check that
    # cannot reach zero under healthy operation is one its reader learns to skip
    # -- the same cry-wolf shape that rules out a standing alarm here. It also
    # makes the script find its OWN explanations: the note recording that
    # `LLMProxyUsageSnapshot.swift` never existed necessarily contains that name,
    # so documenting a dead citation re-reported it as a live one.
    acknowledged = {}
    for name in list(gone):
        citers = gone[name]
        if all(
            "parity-citations.py" in (repo_root / "crates" / "quota-core" / "src" / c).read_text()
            for c in citers
            if (repo_root / "crates" / "quota-core" / "src" / c).is_file()
        ):
            acknowledged[name] = gone.pop(name)

    print(
        f"anchor: {tag}   cited upstream files: {len(cited)}   "
        f"present: {len(present)}   answered: {len(acknowledged)}"
    )
    for name, where in sorted(acknowledged.items()):
        print(f"  answered: {name}  ({', '.join(where)} carries its verification tag)")
    if not gone:
        print("findings: none")
        return 0

    print(f"findings: {len(gone)} cited file(s) absent at {tag}")
    for name, where in sorted(gone.items()):
        print(f"  {name}  cited by {', '.join(where)}")
    print()
    print("These ports are still correct; their provenance is unfollowable. Either")
    print("re-anchor the citation to the tag it was verified at, or note what")
    print("replaced the file upstream, so the next round does not hunt for it.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
