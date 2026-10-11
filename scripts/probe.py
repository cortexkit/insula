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
    probe.py --self-test  # checks restoration when signal.SIGHUP is unavailable

Exit codes: 0 nothing reddened, 1 A NAMED TEST reddened (the usual proof), 2 no
proof could be established -- the mutation would not build, the tree could not be
restored, or the suite hung.

FILTER THIS TOOL'S OUTPUT BY POSITION, NOT BY PATTERN -- `| tail -3`, never
`| grep <verdict-words>`. Every run ends with exactly one verdict line, so a tail
cannot miss it whatever that verdict turns out to be. A grep is an ALLOW-LIST OVER
OUTCOMES: it shows only what matches, so an outcome you did not anticipate prints
NOTHING, and nothing reads as quiet success rather than as a failure to classify.

This tool avoids that at the source by classifying its own outcome and carrying it
in the EXIT CODE, which is a closed set a caller cannot mis-spell. The positional
filter lets a reader see the final verdict without suppressing unfamiliar outcomes.

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
from contextlib import contextmanager
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TIMEOUT_SECS = 600


def run(args, **kwargs):
    return subprocess.run(args, cwd=REPO, capture_output=True, text=True, **kwargs)


@contextmanager
def restoring_source(rel, path, source):
    """Arm restoration before signal registration or a source write can fail."""
    previous_handlers = {}
    received_signal = None
    restoring = False

    def _restore_on_signal(signum, _frame):
        nonlocal received_signal
        if restoring:
            return
        received_signal = signum
        sys.exit(128 + signum)

    try:
        for name in ("SIGTERM", "SIGINT", "SIGHUP"):
            sig = getattr(signal, name, None)
            if sig is not None:
                previous_handlers[sig] = signal.signal(sig, _restore_on_signal)
        yield
    finally:
        # Repeated interrupts must not interrupt the restore itself.
        restoring = True
        # Restore the file before resetting handlers: either operation can fail,
        # but a handler-registration error must never strand mutated source.
        try:
            restored = run(["git", "checkout", "--", rel])
            # Invalidate compiled mutants even when checkout restores identical bytes.
            if path.exists():
                path.touch()
            if restored.returncode != 0 or path.read_text() != source:
                raise RuntimeError(f"RESTORE FAILED for {rel} -- fix the tree before continuing")
            if received_signal is not None:
                print(f"  restored {rel} on signal {received_signal}", file=sys.stderr)
            else:
                print("  restored")
            diff = run(["git", "diff", "--stat"])
            print(f"  git diff --stat after restore: {diff.stdout.strip() or '(empty)'}")
        finally:
            for sig, handler in previous_handlers.items():
                signal.signal(sig, handler)


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
    diff = run(["git", "diff", "--stat"])
    if diff.returncode != 0 or diff.stdout.strip():
        print("  staging did not leave an empty working diff; nothing mutated", file=sys.stderr)
        return 2
    print("  git diff --stat before mutation: (empty)")

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

    # BUILD FIRST, WITH NO DEADLINE, THEN RUN THE TESTS UNDER ONE. The deadline
    # exists to catch a mutant that makes a test loop forever. Applied to build
    # and run together, it also fires on a slow COMPILE, and a loaded host makes
    # that common: on 2026-10-02 a one-line mutation timed out at 600s while the
    # host load average sat above 80, and the old message called that
    # "load-bearing" about code whose tests had never started.
    cargo_args = test_args[: test_args.index("--")] if "--" in test_args else test_args
    try:
        with restoring_source(rel, path, source):
            path.write_text(source.replace(old, new, 1))
            print(f"  mutated {rel}")
            diff = run(["git", "diff", "--stat"])
            if diff.returncode != 0 or not diff.stdout.strip():
                raise RuntimeError("mutation did not produce a non-empty working diff")
            print(f"  git diff --stat during mutation:\n{diff.stdout.rstrip()}")
            built = run(["cargo", "test", *cargo_args, "--no-run"])
            if built.returncode != 0:
                ran, failed, outcome = False, [], "did-not-build"
            else:
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
    except (OSError, RuntimeError, ValueError) as error:
        print(f"  !! {error}", file=sys.stderr)
        return 2

    if outcome == "hung":
        # A third outcome, distinct from red and green: the build finished and
        # the TESTS did not, so removing the thing under test changed control
        # flow enough that the suite never finished. It is load-bearing, but
        # nothing reported on it.
        #
        # Exits 2 rather than 1: load-bearing is not the same claim as defended,
        # and 1 is reserved for "a named test reddened". See the module docstring.
        print(f"  HUNG: the tests ran past {TIMEOUT_SECS}s after building -- load-bearing, but no test named it")
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


def self_test():
    """Exercise restoration with Windows-like signals and interrupted setup."""
    import tempfile
    import types
    import unittest
    from unittest.mock import patch

    class RestorationTests(unittest.TestCase):
        def setUp(self):
            self.temp = tempfile.TemporaryDirectory()
            self.addCleanup(self.temp.cleanup)
            self.root = Path(self.temp.name)
            self.path = self.root / "source.txt"
            self.path.write_text("original")
            for args in (["git", "init", "-q"], ["git", "add", "source.txt"]):
                subprocess.run(args, cwd=self.root, check=True, capture_output=True)
            self.repo_patch = patch.dict(globals(), REPO=self.root)
            self.repo_patch.start()
            self.addCleanup(self.repo_patch.stop)

        def test_missing_sighup_restores_mutated_file(self):
            registered = []
            windows_signals = types.SimpleNamespace(
                SIGTERM=15, SIGINT=2,
                signal=lambda sig, handler: registered.append(sig),
            )
            real_run = run
            cargo_calls = []

            def fake_cargo(args, **kwargs):
                if args[0] != "cargo":
                    return real_run(args, **kwargs)
                self.assertEqual(self.path.read_text(), "mutated")
                cargo_calls.append(args)
                return subprocess.CompletedProcess(args, 0, stdout="test result: ok. 1 passed\n", stderr="")

            with patch.dict(globals(), signal=windows_signals, run=fake_cargo):
                self.assertEqual(main(["source.txt", "original", "mutated"]), 0)
            self.assertEqual(self.path.read_text(), "original")
            self.assertEqual(len(cargo_calls), 2, "both build and test must observe the mutant")
            self.assertEqual(registered, [15, 2, 15, 2])

        def test_registration_failure_restores_file(self):
            def fail_registration(_sig, _handler):
                # Simulate a setup failure after a write to check the finally guard.
                self.path.write_text("mutated")
                raise ValueError("registration failed")

            windows_signals = types.SimpleNamespace(SIGTERM=15, signal=fail_registration)
            with patch.dict(globals(), signal=windows_signals):
                with self.assertRaisesRegex(ValueError, "registration failed"):
                    with restoring_source("source.txt", self.path, "original"):
                        self.fail("setup should fail before yielding")
            self.assertEqual(self.path.read_text(), "original")

        def test_signal_during_mutation_restores_file(self):
            handlers = []
            supported = types.SimpleNamespace(
                SIGTERM=15,
                signal=lambda _sig, handler: handlers.append(handler),
            )
            with patch.dict(globals(), signal=supported):
                with self.assertRaises(SystemExit) as exit:
                    with restoring_source("source.txt", self.path, "original"):
                        self.path.write_text("mutated")
                        handlers[0](15, None)
            self.assertEqual(exit.exception.code, 143)
            self.assertEqual(self.path.read_text(), "original")

    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RestorationTests)
    return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(self_test() if sys.argv[1:] == ["--self-test"] else main(sys.argv[1:]))
