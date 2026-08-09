#!/usr/bin/env python3
"""Run one mutation safely: stage, edit, test, restore, report.

WHY THIS EXISTS. The safe way to mutate is to stage first, so `git checkout --`
returns the file to the state you meant. Doing that by hand fails in a specific
window: a one-line probe feels too small to warrant ceremony, so the staging step
is skipped, and the restore then reverts to whatever the index last held --
deleting whatever was written since. The cost of skipping the guard is lowest
exactly when the edit is smallest, and small edits are most of them.

So the guard has to live in the probe rather than in the discipline. This stages
everything before touching the file, and restores from that index afterwards,
which makes the destructive version unreachable rather than discouraged.

    probe.py <file> <old-text> <new-text> [-- cargo test args...]

Exit codes: 0 nothing reddened, 1 something reddened (the usual proof), 2 the
mutation could not be applied or the tree could not be restored.
"""

import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TIMEOUT_SECS = 600


def run(args, **kwargs):
    return subprocess.run(args, cwd=REPO, capture_output=True, text=True, **kwargs)


def main(argv):
    if "--" in argv:
        split = argv.index("--")
        positional, test_args = argv[:split], argv[split + 1 :]
    else:
        positional, test_args = argv, ["-p", "quota-core", "--lib"]

    if len(positional) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    rel, old, new = positional
    path = REPO / rel

    if not path.exists():
        print(f"  no such file: {rel}", file=sys.stderr)
        return 2

    # Stage everything BEFORE the edit. This is the whole point: the restore
    # below is only correct if the index already holds the state to return to.
    staged = run(["git", "add", "-A"])
    if staged.returncode != 0:
        print(f"  could not stage: {staged.stderr.strip()}", file=sys.stderr)
        return 2

    source = path.read_text()
    if old not in source:
        print(f"  pattern not found in {rel}, nothing mutated", file=sys.stderr)
        return 2
    occurrences = source.count(old)
    if occurrences > 1:
        # Ambiguity is refused rather than resolved by position: mutating a
        # different site than intended produces a verdict about the wrong code,
        # which is indistinguishable from a verdict about the right one.
        print(f"  pattern occurs {occurrences} times in {rel}; make it unique", file=sys.stderr)
        return 2

    path.write_text(source.replace(old, new, 1))
    print(f"  mutated {rel}")

    # The restore runs in `finally` so that an interrupt cannot leave the tree
    # mutated. Without it, Ctrl-C during a slow suite exits with the mutation
    # still applied and nothing saying so -- and the next thing anyone runs
    # reports on code they did not write.
    try:
        result = run(["cargo", "test", *test_args], timeout=TIMEOUT_SECS)
        ran = "\ntest result:" in result.stdout or result.stdout.startswith("test result:")
        failed = [
            line.split()[1]
            for line in result.stdout.splitlines()
            if line.startswith("test ") and line.endswith("FAILED")
        ]
        outcome = "ran" if ran else "did-not-build"
    except subprocess.TimeoutExpired:
        ran, failed, outcome = False, [], "hung"
    finally:
        restored = run(["git", "checkout", "--", rel])
        if restored.returncode != 0 or path.read_text() != source:
            # Louder than a failed proof: the tree is now wrong, and every later
            # result in this session would describe mutated code.
            print(
                f"  !! RESTORE FAILED for {rel} -- fix the tree before continuing",
                file=sys.stderr,
            )
            return 2
        print("  restored")

    if outcome == "hung":
        # A third outcome, distinct from red and green: removing the thing under
        # test changed control flow enough that the suite never finished, so it
        # is load-bearing but nothing reported on it.
        print(f"  HUNG after {TIMEOUT_SECS}s -- load-bearing, but no test named it")
        return 1
    if outcome == "did-not-build":
        print("  DID NOT BUILD -- the mutation was malformed, so this says nothing about coverage")
        return 2
    if failed:
        print(f"  {len(failed)} test(s) reddened:")
        for name in failed:
            print(f"    {name}")
        return 1
    print("  NOTHING REDDENED -- whatever this mutation removed, no test asserts it")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
