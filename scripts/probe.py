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

Exit codes: 0 nothing reddened, 1 A NAMED TEST reddened (the usual proof), 2 no
proof could be established -- the mutation would not build, the tree could not be
restored, or the suite hung.

FILTER THIS TOOL'S OUTPUT BY POSITION, NOT BY PATTERN -- `| tail -3`, never
`| grep <verdict-words>`. Every run ends with exactly one verdict line, so a tail
cannot miss it whatever that verdict turns out to be. A grep is an ALLOW-LIST OVER
OUTCOMES: it shows only what matches, so an outcome you did not anticipate prints
NOTHING, and nothing reads as quiet success rather than as a failure to classify.

SUBC lost a real refusal to exactly this on their placement gate tonight -- their
habitual grep returned empty against a refusal from a code path they had not met,
because the tool carried TWO refusal vocabularies and their pattern knew one. No
pattern could have been right there; the tool disagreed with itself.

This tool avoids that at the source by classifying its own outcome and carrying it
in the EXIT CODE, which is a closed set a caller cannot mis-spell. The positional
filter is the second line, for the human reading along. Stated here rather than
left to habit because the safe shape was originally chosen for being short, and
nobody remembers that a property was luck.

HUNG EXITS 2, NOT 1, and the reason is worth stating because the text and the exit
code used to disagree. A hang IS evidence that the mutated thing is load-bearing,
so reporting it as a finding is right in prose. But the proof this tool exists to
produce is "a named test asserts this", and a hang produces no name -- so a caller
reading only `$? == 1` as "mutation proven" would have been given a proof that
nobody wrote. The exit code names the mechanism; only the text carries the intent.
"""

import signal
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
    if old == new:
        # A NO-OP MUTATION IS INDISTINGUISHABLE FROM AN UNDEFENDED GUARD, and it
        # is the one failure this script cannot detect after the fact: the write
        # succeeds, the suite runs green, and the verdict printed is "NOTHING
        # REDDENED -- no test asserts this" about code nobody changed.
        #
        # Reachable by ordinary means rather than by carelessness: a copy-paste
        # where only one side was edited, or a rewrite that normalises whitespace
        # the file already had. Verified on this repo 2026-09-19 -- passing one
        # constant as both arguments printed the undefended verdict.
        #
        # Exit 2 rather than 0, because "could not check" is not "nothing
        # defends it". (CKCRED hit the same shape from the other direction: a
        # mutation that failed to APPLY left their arm reporting ok from
        # unmutated code.)
        print("  old and new are identical; nothing would be mutated", file=sys.stderr)
        return 2
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
    #
    # `finally` covers Ctrl-C, which arrives as an exception and unwinds. It does
    # NOT cover SIGTERM, which terminates the process without unwinding -- so a
    # `timeout` around this script, or any supervisor killing it, leaves the
    # mutation in place. That is not hypothetical: it happened to the sibling
    # audit script today, and a neutralised checker rule reached master under a
    # `git add -A`. Both paths now reach the same restore.
    def _restore_on_signal(signum, _frame):
        run(["git", "checkout", "--", rel])
        print(f"  restored {rel} on signal {signum}", file=sys.stderr)
        sys.exit(128 + signum)

    previous_handlers = {
        sig: signal.signal(sig, _restore_on_signal)
        for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP)
    }

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
        for sig, handler in previous_handlers.items():
            signal.signal(sig, handler)
        restored = run(["git", "checkout", "--", rel])
        # BUMP THE MTIME AFTER RESTORING. `git checkout --` writes the original
        # bytes back, and a build system keying on mtime can then leave the
        # MUTANT's compiled artefacts in place: the source is correct, the binary
        # is not, and every later run in this session describes code that is no
        # longer on disk. Hit for real on 2026-09-02, where a restored build.rs
        # did not re-run its build script.
        #
        # Unconditional rather than gated on a comparison: a spurious rebuild
        # costs seconds, and a skipped one costs a wrong verdict that looks
        # exactly like a right one.
        if path.exists():
            path.touch()
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
        #
        # Exits 2 rather than 1: load-bearing is not the same claim as defended,
        # and 1 is reserved for "a named test reddened". See the module docstring.
        print(f"  HUNG after {TIMEOUT_SECS}s -- load-bearing, but no test named it")
        return 2
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
