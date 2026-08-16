#!/usr/bin/env python3
"""Refuse a green gate that cargo answered from cache after a sibling repo moved.

THE FAILURE THIS EXISTS FOR, which happened here on 2026-08-16. This workspace
path-depends on ../subconscious. That repo removed a struct field, nothing in
this tree changed, so `cargo clippy` had nothing to redo and answered `0` from
cache. That zero was reported as a gate result, the commit went out, and CI --
which always builds cold -- failed on a compile error.

The repo already has a rule saying a clippy claim must be backed by a forced
recompile. The rule did not fire, because a rule is a thing someone has to
remember at the moment they are least suspicious: the build just said clean.

WHAT THIS CHECKS, from facts already on disk and with no stored state: is any
path-dependency sibling's HEAD commit NEWER than this workspace's newest build
artifact? If so, a cached gate result may predate a change that has not been
compiled here yet.

WHAT IT DOES NOT CHECK: whether the sibling change actually breaks anything.
It cannot, and claiming otherwise would be worse than silence -- the answer is
"your cache may be stale", and the remedy is to force a recompile and look.

Exit codes follow this repo's convention:
  0  checked, artifacts are newer than every sibling HEAD
  1  checked, at least one sibling moved after the last build
  2  could not check -- the run makes no claim either way
"""

import re
import subprocess
import sys
from pathlib import Path
from typing import List, Optional, Tuple

REPO = Path(__file__).resolve().parent.parent


def die(message: str) -> None:
    print(f"  refusing: {message}", file=sys.stderr)
    print("  this is not a clean result -- the question is unanswered.", file=sys.stderr)
    sys.exit(2)


def path_dependencies() -> List[Path]:
    """Sibling repos this workspace compiles against by path.

    Read from the workspace manifest rather than hardcoded, so a dependency
    added later is covered without anyone editing this file -- the same reason
    the other scripts here derive their inputs from source.
    """
    manifest = REPO / "Cargo.toml"
    if not manifest.is_file():
        die(f"no workspace manifest at {manifest}")
    text = manifest.read_text(encoding="utf-8")
    found: List[Path] = []
    for match in re.finditer(r'path\s*=\s*"([^"]+)"', text):
        candidate = (REPO / match.group(1)).resolve()
        # Only siblings matter. A path dep inside this workspace moves when this
        # tree moves, which cargo already notices.
        if REPO in candidate.parents or candidate == REPO:
            continue
        found.append(candidate)
    return found


def git_repo_root(path: Path) -> Optional[Path]:
    result = subprocess.run(
        ["git", "-C", str(path), "rev-parse", "--show-toplevel"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    return Path(result.stdout.strip())


def head_commit_epoch(repo: Path) -> Optional[Tuple[int, str]]:
    result = subprocess.run(
        ["git", "-C", str(repo), "log", "-1", "--format=%ct %h"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0 or not result.stdout.strip():
        return None
    epoch, sha = result.stdout.split()
    return int(epoch), sha


def newest_artifact_epoch() -> Optional[Tuple[int, str]]:
    """When this workspace last actually compiled something.

    Uses cargo's fingerprint directories rather than the binaries: a build that
    fails part-way still updates fingerprints for what it did compile, and the
    question here is when cargo last looked, not what it produced.
    """
    newest: Optional[Tuple[int, str]] = None
    for profile in ("debug", "release"):
        fingerprints = REPO / "target" / profile / ".fingerprint"
        if not fingerprints.is_dir():
            continue
        for entry in fingerprints.iterdir():
            try:
                stamp = int(entry.stat().st_mtime)
            except OSError:
                continue
            if newest is None or stamp > newest[0]:
                newest = (stamp, f"target/{profile}")
    return newest


def main() -> int:
    siblings = path_dependencies()
    if not siblings:
        # Not a clean pass: this workspace is known to path-depend on a sibling,
        # so finding none means the manifest parse is wrong, not that the risk
        # is gone. A zero here would look identical to a real all-clear.
        die("no sibling path dependencies found in Cargo.toml; the parse is broken")

    artifact = newest_artifact_epoch()
    if artifact is None:
        die("no build artifacts in target/; nothing has been compiled to be stale")

    # Deduplicate by REPOSITORY, not by path: the manifest names one sibling once
    # per crate it pulls from it, and reporting "4 siblings checked" for a single
    # repo overstates the population this check actually covers.
    roots = {}
    for sibling in siblings:
        root = git_repo_root(sibling)
        if root is None:
            die(f"{sibling} is not inside a git repository; cannot date it")
        roots.setdefault(root, []).append(sibling.name)

    stale: List[str] = []
    checked = 0
    for root, crates in sorted(roots.items()):
        head = head_commit_epoch(root)
        if head is None:
            die(f"could not read HEAD of {root}")
        checked += 1
        marker = "STALE" if head[0] > artifact[0] else "ok"
        # Sign-safe: the real STALE case is always positive, but formatting that
        # assumes it prints "+-42m" the moment anything changes the comparison.
        drift = abs(head[0] - artifact[0]) // 60
        detail = f"{drift}m after the last build" if marker == "STALE" else ""
        print(f"  {root.name:20} {head[1]}  {marker:6} {detail}")
        print(f"    {len(crates)} crate(s) used from it: {', '.join(sorted(crates))}")
        if marker == "STALE":
            stale.append(root.name)

    print(f"  sibling repositories checked: {checked} (newest build artifact: {artifact[1]})")

    if stale:
        print()
        print(f"  {len(stale)} sibling(s) moved after this workspace last compiled:")
        print(f"    {', '.join(sorted(set(stale)))}")
        print("  A cargo gate run now may answer from cache and report clean while")
        print("  CI, which always builds cold, fails. Force a recompile before")
        print("  claiming any gate result:")
        print("    touch crates/*/src/lib.rs crates/*/src/main.rs && cargo clippy \\")
        print("      --workspace --all-targets -- -D warnings")
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
