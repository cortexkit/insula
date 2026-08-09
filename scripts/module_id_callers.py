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
  copy        a second working tree of a repository already in the list. Edits
              here produce NO ERROR AND NO EFFECT, and someone who reached a real
              caller through such a tree will believe it is handled

A TREE IS DISCOUNTED FOR BEING A COPY, NOT FOR BEING QUIET. That distinction cost
three wrong predicates to reach, each failing in a way the output did not show:

  "no remote"        A repository with no remote can be under active development.
                     One here was committed to on the day this was written, and
                     calling it abandoned DROPS A LIVE CALLER -- the direction
                     that causes the outage this script exists to prevent.
  "behind upstream"  A branch with no upstream cannot be compared against one.
                     The comparison returns empty, which reads exactly like "up
                     to date", so a tree weeks behind was reported as live.
  "last commit age"  A repository is quiet when its module is not deployed yet.
                     Quiet is not abandoned, and this misclassified a real
                     repository in the same dangerous direction as the first.

The property that actually matters is whether an edit can reach anyone, and a
copy is the only case where it cannot. Two tells, both exact: a remote URL shared
with another scanned tree, or -- for a tree with no remote at all -- a HEAD commit
that is an ANCESTOR of another scanned tree's HEAD.

Ancestry, not object existence. `git cat-file -e` answers yes for any commit the
repository has ever fetched, including a branch that was pulled and never merged
-- so a tree holding genuinely unmerged work would be discounted as a copy, which
is the same caller-dropping direction as the three predicates above. That one is
the subtlest of the four, because it is RIGHT ON EVERY TREE IN THIS FLEET TODAY
and would only fail once someone has work in flight.

Remote configuration is still reported, separately, because it answers HOW a
change would be published. A live repository with no remote still runs, still
calls, and still breaks when the id changes; it just cannot be updated by opening
a pull request, which is worth knowing before a rename window rather than when a
pull request has nowhere to go.

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
from pathlib import Path

# The id this module is currently served under. It is only the default for the
# --id flag: the script's job is finding who names a given string, so pointing it
# at a superseded id is a legitimate use -- that is how you find callers left
# behind by a rename.
DEFAULT_ID = "insula"


def self_repo() -> str:
    """This repository's directory name, whose own hits are not caller hits.

    Derived from this file's location rather than written down. A constant would
    be a second place the repository's name lives, and it would go stale the next
    time the directory is renamed -- silently, because a name that no longer
    matches anything simply annotates nothing. The annotation would vanish and
    this module's own definition would read as somebody else's caller.
    """
    return Path(__file__).resolve().parents[1].name

# Extensions where the id can be a live reference. Deliberately wide: the cost of
# examining a prose hit is one line of output, and the cost of excluding a real
# caller is a dead route.
CODE_SUFFIXES = {".rs", ".toml", ".jsonc", ".json", ".yml", ".yaml", ".sh", ".ts", ".py"}




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


def contains(repo: Path, commit: str) -> bool:
    """Whether `commit` is an ancestor of this repository's HEAD.

    Deliberately not `cat-file -e`, which reports on any object the repository
    holds -- including a fetched branch nobody merged. See the module docstring:
    that weaker check discounts a tree with unmerged work as a duplicate.

    This command answers with THREE exit codes, not two: 0 means yes, 1 means no,
    and 128 means it could not tell -- an unknown commit, an unborn HEAD, a
    directory that is not a repository. Only 0 is a yes. Reading "not 1" as yes,
    or "not 0" as no, would turn "I could not look" into a confident answer, and
    the caller uses this to decide whether a tree is a discardable copy of
    another.
    """
    out = subprocess.run(
        ["git", "-C", str(repo), "merge-base", "--is-ancestor", commit, "HEAD"],
        capture_output=True,
        text=True,
    )
    if out.returncode not in (0, 1):
        # Not silently false: a tree this scan cannot read is not a tree whose
        # history demonstrably excludes the commit, and treating it as one would
        # quietly widen the set of repositories reported as live callers.
        print(
            f"  cannot determine ancestry in {repo}: git exited {out.returncode} "
            f"-- treated as NOT containing, so this tree may be reported as a "
            f"separate caller rather than a copy",
            file=sys.stderr,
        )
        return False
    return out.returncode == 0


def duplicates(all_repos: list[Path]) -> dict[Path, str]:
    """Trees that are second copies of a repository already present.

    See the module docstring for why this replaced three activity-based
    predicates. Both tells are exact rather than heuristic, so a tree is only
    discounted when another tree in the same scan can receive the edit instead.
    """
    by_url: dict[str, list[Path]] = {}
    for repo in all_repos:
        url = git(repo, "remote", "get-url", "origin")
        if url:
            by_url.setdefault(url, []).append(repo)

    found: dict[Path, str] = {}
    for url, group in by_url.items():
        if len(group) < 2:
            continue
        # The tree whose directory matches the repository name is the canonical
        # one; the rest are working copies of it.
        name = url.rstrip("/").rsplit("/", 1)[-1].removesuffix(".git")
        canonical = next((r for r in group if r.name == name), group[0])
        for repo in group:
            if repo is not canonical:
                found[repo] = f"shares a remote with {canonical.name}"

    # A tree with no remote is a copy only if its history is already contained in
    # another scanned tree -- which is checkable, unlike its activity.
    #
    # Prefer a container that is not itself a copy: the point of naming one is to
    # say where the edit should go instead, and pointing at another copy sends the
    # reader one step further from the tree that can actually receive it.
    for repo in all_repos:
        if repo in found or git(repo, "remote"):
            continue
        head = git(repo, "rev-parse", "HEAD")
        if not head:
            continue
        containers = [
            other for other in all_repos if other is not repo and contains(other, head)
        ]
        canonical = next((c for c in containers if c not in found), None)
        if canonical is not None:
            found[repo] = f"history already present in {canonical.name}"
        elif containers:
            found[repo] = f"history already present in {containers[0].name}"
    return found


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

    mine = self_repo()
    copies = duplicates(all_repos)
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
        reason = copies.get(repo)
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
                marker = "  (this module's own definition)" if mine in path.parts else ""
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
    print(f"DUPLICATE TREES -- edits here have no effect ({len(stale_repos)}):")
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
