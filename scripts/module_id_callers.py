#!/usr/bin/env python3
"""Find every repository that names this module's subc id, and say what each hit is.

This module's daemon id is a TARGET reference: other repositories dial it, and
nothing here checks who calls. A target reference does not block a rename -- it
breaks when one lands -- so renaming the id means changing every caller in the
same pass, and a missed one is a dead route rather than a failed build.

The failure mode this exists to prevent is not a missed grep. It is a list that
LOOKS complete: a previous rename staged five callers from a narrowed sweep,
nothing recorded what the narrowing dropped, and the module that was dropped lost
every vault-served credential for hours. So this prints the wide denominator
first and never silently excludes.

WHAT MAKES A HIT COUNT IS ITS ROLE, NOT ITS PRESENCE. The same string appears in
four roles and only one of them breaks:

  route       a const or literal used to address this module -- ACT ON THESE
  doc         prose or a comment citing this module -- update for tidiness, since
              a comment naming a module that no longer exists sends the next
              reader to a dead name
  fixture     golden test vectors pinning a path format. Editing these breaks a
              golden comparison for no reason, and the failure then looks like
              the rename broke something real
  stale       an abandoned checkout. Edits here produce NO ERROR AND NO EFFECT,
              and someone who reached a real caller through such a checkout will
              believe it is handled

STALENESS IS JUDGED BY COMMIT RECENCY, NOT BY REMOTE CONFIGURATION, because the
obvious proxies are wrong in both directions and this script got both wrong
before they were checked. A repository with no remote can be under active
development -- one here was committed to the same day -- so "no remote" reported
as stale DROPS A LIVE CALLER, which is the dangerous direction. And a checkout on
a branch with no upstream cannot be compared against origin at all: the
comparison returns empty, which reads exactly like "up to date", so a tree three
weeks behind was reported as live.

Remote configuration answers "how would I publish a change here", which matters
and is reported separately. It does not answer "is anyone working here".

Run it before a rename window, and again during -- a caller added between the two
is invisible to a list frozen in advance, which is the gap a hand-reconciled
process is supposed to close.

    ./scripts/module_id_callers.py                    # from anywhere under the fleet root
    ./scripts/module_id_callers.py --root ~/src       # a different fleet root
    ./scripts/module_id_callers.py --id some-module   # a different module id
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from pathlib import Path

DEFAULT_ID = "ai-provider-quota"
# This module's own repository, whose single definition is not a caller.
SELF_REPO = "insula"

# Extensions where the id can be a live reference. Deliberately wide: the cost of
# examining a prose hit is one line of output, and the cost of excluding a real
# caller is a dead route.
CODE_SUFFIXES = {".rs", ".toml", ".jsonc", ".json", ".yml", ".yaml", ".sh", ".ts", ".py"}

# A checkout whose last commit is older than this is reported as abandoned rather
# than as a caller. Chosen loose: the point is to catch trees nobody is working
# in, not to police branch freshness.
STALE_DAYS = 14


def repos(root: Path) -> list[Path]:
    return sorted(p for p in root.iterdir() if (p / ".git").exists())


def git(repo: Path, *args: str) -> str:
    try:
        out = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            timeout=20,
        )
        return out.stdout.strip()
    except Exception:
        return ""


def days_since_commit(repo: Path) -> int | None:
    epoch = git(repo, "log", "-1", "--format=%ct")
    if not epoch.isdigit():
        return None
    return int((time.time() - int(epoch)) // 86400)


def staleness(repo: Path) -> str | None:
    """Why nobody is working in this checkout, or None if it is live.

    Judged on commit recency alone. See the module docstring for why the remote
    is not consulted here: both obvious remote-based proxies misclassified real
    repositories in this fleet, one of them in the direction that silently drops
    a caller.
    """
    age = days_since_commit(repo)
    if age is None:
        return "no commits"
    if age > STALE_DAYS:
        last = git(repo, "log", "-1", "--format=%cd", "--date=short")
        return f"last commit {last} ({age} days ago)"
    return None


def publish_note(repo: Path) -> str | None:
    """How a change here would reach anyone else, when that is not obvious.

    Reported beside a live caller rather than used to classify it: a repository
    with no remote still runs, still calls, and still breaks when the id changes
    -- it just cannot be updated by opening a pull request.
    """
    if not git(repo, "remote"):
        return "no remote configured -- local-only, coordinate directly"
    return None


def hits(repo: Path, needle: str) -> list[tuple[Path, int, str]]:
    out = subprocess.run(
        [
            "grep", "-rn", needle, str(repo),
            "--exclude-dir=target", "--exclude-dir=.git",
            "--exclude-dir=node_modules", "--exclude-dir=.cortexkit",
            "--exclude-dir=dist", "--binary-files=without-match",
        ],
        capture_output=True,
        text=True,
    )
    found = []
    for line in out.stdout.splitlines():
        path, _, rest = line.partition(":")
        num, _, text = rest.partition(":")
        if not num.isdigit():
            continue
        found.append((Path(path), int(num), text.strip()))
    return found


def role(path: Path, text: str) -> str:
    if "golden" in path.parts or "fixtures" in path.parts:
        return "fixture"
    if path.suffix not in CODE_SUFFIXES:
        return "doc"
    stripped = text.lstrip()
    if stripped.startswith(("//", "#", "*", "--", "/*")):
        return "doc"
    return "route"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=None, help="fleet root (default: this repo's parent)")
    ap.add_argument("--id", default=DEFAULT_ID, help=f"module id to find (default: {DEFAULT_ID})")
    args = ap.parse_args()

    root = Path(args.root).expanduser() if args.root else Path(__file__).resolve().parents[2]
    if not root.is_dir():
        print(f"fleet root not found: {root}", file=sys.stderr)
        return 2

    all_repos = repos(root)
    if not all_repos:
        print(f"no git repositories under {root}", file=sys.stderr)
        return 2

    routes: list[str] = []
    docs = 0
    fixtures = 0
    stale_repos: list[str] = []
    local_only: list[str] = []
    files_with_hits = 0

    for repo in all_repos:
        found = hits(repo, args.id)
        if not found:
            continue
        files_with_hits += len({p for p, _, _ in found})
        reason = staleness(repo)
        if reason:
            stale_repos.append(f"{repo.name}: {reason} ({len(found)} hit(s))")
            continue
        note = publish_note(repo)
        if note:
            local_only.append(f"{repo.name}: {note}")
        for path, num, text in found:
            kind = role(path, text)
            if kind == "route":
                rel = path.relative_to(root)
                marker = "  (this module's own definition)" if path.parts and SELF_REPO in path.parts else ""
                routes.append(f"{rel}:{num}{marker}")
            elif kind == "fixture":
                fixtures += 1
            else:
                docs += 1

    print(f"searched {len(all_repos)} repositories under {root} for {args.id!r}")
    print(f"  files containing it: {files_with_hits}")
    print()
    print(f"ROUTE -- change these WITH the rename ({len(routes)}):")
    for r in routes:
        print(f"    {r}")
    print()
    print(f"STALE CHECKOUTS -- edits here have no effect ({len(stale_repos)}):")
    for s in stale_repos:
        print(f"    {s}")
    print()
    if local_only:
        print(f"LIVE BUT LOCAL-ONLY -- cannot be updated by pull request ({len(local_only)}):")
        for entry in local_only:
            print(f"    {entry}")
        print()
    print(f"doc/comment hits (update for tidiness): {docs}")
    print(f"fixture hits (leave alone; editing breaks a golden comparison): {fixtures}")

    if not routes:
        print("\nno route references found at all -- suspect the search before "
              "concluding there are no callers", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
