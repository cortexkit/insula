#!/usr/bin/env python3
"""Check gate orchestration with fake tools, never starting a remote job."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SOURCE = Path(__file__).resolve().with_name("prepush.sh")
ROOT = SOURCE.parent.parent
BASH = shutil.which("bash") or ""
assert BASH, "bash is required"


class PrepushTests(unittest.TestCase):
    def setUp(self):
        scratch = ROOT / "target/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="prepush-test-", dir=scratch)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = os.environ.copy()
        for key in list(self.env):
            if key.startswith("GIT_"):
                self.env.pop(key)
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
        self.manifest = b"docs/space name.md\0docs/tracked.md\0"
        (self.root / "scripts").mkdir()
        shutil.copyfile(SOURCE, self.root / "scripts/prepush.sh")
        (self.root / "scripts/gates.sh").write_text(
            '#!/bin/bash\nprintf "repo checks %s\\n" "$*"\nexit "${REPO_RC:-0}"\n'
        )
        (self.root / "docs").mkdir()
        (self.root / "docs/tracked.md").write_text("tracked snapshot content\n")
        (self.root / "docs/space name.md").write_text("another tracked document\n")
        subprocess.run(["git", "init", "--quiet", str(self.root)], env=self.env, check=True)
        subprocess.run(["git", "-C", str(self.root), "add", "--",
                        "docs/tracked.md", "docs/space name.md"], env=self.env, check=True)
        for version in ("v1", "v2"):
            folder = self.root / "test-vectors" / version
            folder.mkdir(parents=True)
            (folder / "wire.json").write_bytes(b'{}\n')
            checksum = hashlib.sha256(b'{}\n').hexdigest()
            (folder / "SHA256SUMS").write_text(f"{checksum}  wire.json\n")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        # A closed PATH proves the missing-ck case even on a configured host.
        for tool in ("bash", "python3", "git", "shasum", "cut", "find", "sort",
                     "wc", "tr", "basename", "mktemp", "mkdir", "rm", "grep",
                     "sed", "env", "dirname", "mv"):
            location = shutil.which(tool)
            assert location is not None, f"{tool} is required"
            (self.bin / tool).symlink_to(location)
        self.env.update(PATH=str(self.bin), PREPUSH_TEST_ROOT=str(self.root))
        for key in ("FMT_RC", "REPO_RC", "LINUX_RC", "WINDOWS_RC", "PARALLEL"):
            self.env.pop(key, None)
        self.stub("cargo", '''
import json, os, subprocess, sys
from pathlib import Path
root = Path(os.environ["PREPUSH_TEST_ROOT"])
tracked = None
if os.environ.get("VERIFY_DOC_INDEX") and sys.argv[1:] == ["test", "--workspace", "--all-targets"]:
    tracked = subprocess.check_output(["git", "ls-files"], text=True).splitlines()
with (root / "cargo.jsonl").open("a") as output:
    output.write(json.dumps({"args": sys.argv[1:], "home": os.environ.get("HOME"),
                             "cargo_home": os.environ.get("CARGO_HOME"),
                             "state": os.environ.get("CK_QUOTA_STATE_DIR"),
                             "tracked": tracked,
                             "git_dir": os.environ.get("GIT_DIR"),
                             "transport_files": [p.name for p in root.glob(".prepush-tracked-docs.*")]}) + "\\n")
sys.exit(int(os.environ.get("FMT_RC", "0")) if sys.argv[1] == "fmt" else 0)
''')
        self.stub("rustup", '''
import os, sys
from pathlib import Path
Path(os.environ["PREPUSH_TEST_ROOT"], "rustup-called").touch()
print("fake stable toolchain")
''')
        self.stub("rustc", 'print("fake installed rustc")\n')
        self.stub("ck", '''
import json, os, sys, time
from pathlib import Path
root = Path(os.environ["PREPUSH_TEST_ROOT"])
platform = sys.argv[sys.argv.index("--platform") + 1]
with (root / (platform + ".json")).open("w") as output:
    json.dump(sys.argv[1:], output)
(root / (platform + ".manifest")).write_bytes(Path(sys.argv[-1]).read_bytes())
if os.environ.get("PARALLEL") == "1":
    other = "windows" if platform == "linux" else "linux"
    deadline = time.monotonic() + 5
    while not (root / (other + ".json")).exists():
        if time.monotonic() > deadline:
            print("other platform never started", flush=True)
            sys.exit(98)
        time.sleep(0.01)
print(platform + " streamed output", flush=True)
(root / (platform + ".finished")).touch()
sys.exit(int(os.environ.get(platform.upper() + "_RC", "0")))
''')

    def stub(self, name, body):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o755)

    def run_gate(self, *args):
        if args and args[0].startswith("--worker-"):
            path = self.root / ".prepush-tracked-docs.fixture"
            if self.manifest is not None:
                path.write_bytes(self.manifest)
            args += (path.name,)
        result = subprocess.run(
            [BASH, "scripts/prepush.sh", *args], cwd=self.root, env=self.env,
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=15,
        )
        print(f"{self._testMethodName}: exit {result.returncode}", flush=True)
        return result

    def cargo_calls(self):
        path = self.root / "cargo.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_fast_runs_local_checks_without_compiling_or_motor(self):
        (self.bin / "ck").unlink()
        result = self.run_gate("--fast")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("repo checks --repo-only", result.stdout)
        self.assertIn("v1/SHA256SUMS", result.stdout)
        self.assertIn("v2/SHA256SUMS", result.stdout)
        self.assertEqual([call["args"] for call in self.cargo_calls()], [
            ["fmt", "--all", "--", "--check"],
            ["metadata", "--locked", "--format-version", "1"],
        ])
        self.assertIn("not a full prepush pass", result.stdout)

    def test_both_jobs_start_in_parallel_and_are_streamed(self):
        self.env["PARALLEL"] = "1"
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stdout)
        for platform in ("linux", "windows"):
            self.assertIn(platform + " streamed output", result.stdout)
            self.assertIn(platform + " job finished in", result.stdout)
            self.assertTrue((self.root / (platform + ".finished")).exists())
        linux = json.loads((self.root / "linux.json").read_text())
        windows = json.loads((self.root / "windows.json").read_text())
        self.assertEqual(linux[linux.index("--vcpus") + 1], "16")
        self.assertIn("--net", linux)
        self.assertNotIn("--net", windows)
        self.assertEqual(linux[linux.index("--timeout") + 1], "1200")
        for platform in ("linux", "windows"):
            self.assertEqual((self.root / (platform + ".manifest")).read_bytes(), self.manifest)
        self.assertEqual(list(self.root.glob(".prepush-tracked-docs.*")), [], "caller must clean up transport files")

    def test_linux_only_does_not_claim_full_coverage(self):
        result = self.run_gate("--linux-only")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertFalse((self.root / "windows.json").exists())
        self.assertIn("Windows was not requested", result.stdout)
        self.assertNotIn("prepush passed:", result.stdout)

    def test_either_platform_failure_fails_but_waits_for_both(self):
        for platform in ("LINUX", "WINDOWS"):
            with self.subTest(platform=platform):
                self.env[platform + "_RC"] = "101"
                result = self.run_gate()
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("GATE FAILED: " + platform.lower(), result.stdout)
                for name in ("linux", "windows"):
                    self.assertTrue((self.root / (name + ".finished")).exists())
                del self.env[platform + "_RC"]

    def test_motor_incomplete_outcomes_exit_two_even_with_other_leg_red(self):
        self.env["WINDOWS_RC"] = "101"
        for status in (200, 201, 202, 203, 204):
            with self.subTest(status=status):
                self.env["LINUX_RC"] = str(status)
                result = self.run_gate()
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("COULD NOT CHECK: linux", result.stdout)
                self.assertNotIn("prepush passed:", result.stdout)
                self.assertEqual(list(self.root.glob(".prepush-tracked-docs.*")), [])

    def test_missing_ck_exits_two(self):
        (self.bin / "ck").unlink()
        result = self.run_gate()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("missing ck", result.stdout)

    def test_fmt_failure_stops_before_metadata_or_remote(self):
        self.env["FMT_RC"] = "1"
        result = self.run_gate()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("GATE FAILED: phase 1: format", result.stdout)
        self.assertEqual(len(self.cargo_calls()), 1)
        self.assertFalse((self.root / "linux.json").exists())

    def test_repo_cannot_check_is_not_flattened_to_gate_failure(self):
        self.env["REPO_RC"] = "2"
        result = self.run_gate("--fast")
        self.assertEqual(result.returncode, 2, result.stdout)

    def test_corrupt_vector_is_rejected_before_remote(self):
        (self.root / "test-vectors/v2/wire.json").write_text("corrupt")
        result = self.run_gate()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("GATE FAILED: phase 1: test-vectors/v2/SHA256SUMS", result.stdout)
        self.assertFalse((self.root / "linux.json").exists())

    def test_e2e_requires_sibling_and_cannot_be_silently_skipped_by_fast(self):
        for args in (("--e2e",), ("--fast", "--e2e"), ("--sibling", "missing")):
            with self.subTest(args=args):
                result = self.run_gate(*args)
                self.assertEqual(result.returncode, 2, result.stdout)

    def test_e2e_sibling_is_sent_only_to_linux(self):
        sibling = self.root / "subconscious"
        sibling.mkdir()
        (sibling / "Cargo.toml").write_text("[workspace]\n")
        result = self.run_gate("--e2e", "--sibling", str(sibling))
        self.assertEqual(result.returncode, 0, result.stdout)
        linux = json.loads((self.root / "linux.json").read_text())
        windows = json.loads((self.root / "windows.json").read_text())
        self.assertIn(str(sibling), linux)
        self.assertEqual(linux[-3:-1], ["--worker-linux", "1"])
        self.assertNotIn("--sibling", windows)
        self.assertEqual(windows[-3:-1], ["--worker-windows", "0"])

    def test_workers_run_the_exact_ci_rust_commands(self):
        for platform in ("linux", "windows"):
            with self.subTest(platform=platform):
                (self.root / "cargo.jsonl").unlink(missing_ok=True)
                result = self.run_gate("--worker-" + platform, "0")
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertIn("fake installed rustc", result.stdout)
                self.assertFalse((self.root / "rustup-called").exists())
                self.assertEqual([call["args"] for call in self.cargo_calls()], [
                    ["--version"], ["fmt", "--all", "--", "--check"],
                    ["clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"],
                    ["test", "--workspace", "--all-targets"],
                    ["test", "--workspace", "--doc"],
                    ["run", "-p", "quota-core", "--example", "completeness-envelopes"],
                ])

    def test_e2e_worker_uses_isolated_home_and_the_ignored_ci_test(self):
        original_home = self.env["HOME"]
        self.env.pop("CARGO_HOME", None)
        result = self.run_gate("--worker-linux", "1")
        self.assertEqual(result.returncode, 0, result.stdout)
        daemon = next(call for call in self.cargo_calls() if "--ignored" in call["args"])
        self.assertEqual(daemon["args"], ["test", "-p", "quota-module", "--test", "it",
                                         "--", "--ignored", "real_daemon_e2e::"])
        self.assertNotEqual(daemon["home"], original_home)
        self.assertEqual(daemon["state"], daemon["home"] + "/state")
        self.assertEqual(daemon["cargo_home"], original_home + "/.cargo")

    def test_snapshot_without_git_gets_only_present_tracked_docs_during_tests(self):
        shutil.rmtree(self.root / ".git")
        (self.root / "docs/untracked.md").write_text("must not become a valid target\n")
        self.manifest = b"docs/tracked.md\0docs/space name.md\0docs/deleted.md\0"
        self.env["VERIFY_DOC_INDEX"] = "1"
        result = self.run_gate("--worker-windows", "0")
        self.assertEqual(result.returncode, 0, result.stdout)
        calls = self.cargo_calls()
        test = next(call for call in calls if call["args"] == ["test", "--workspace", "--all-targets"])
        self.assertEqual(test["tracked"], ["docs/space name.md", "docs/tracked.md"])
        self.assertIsNotNone(test["git_dir"])
        self.assertTrue(all(call["git_dir"] is None for call in calls if call is not test))
        self.assertTrue(all(call["transport_files"] == [] for call in calls))
        self.assertFalse((self.root / ".git").exists(), "build provenance must not see a synthetic .git")

    def test_snapshot_without_git_refuses_missing_or_empty_tracked_manifest(self):
        shutil.rmtree(self.root / ".git")
        for manifest in (None, b"docs/deleted.md\0"):
            with self.subTest(manifest=manifest):
                self.manifest = manifest
                result = self.run_gate("--worker-windows", "0")
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("COULD NOT CHECK: windows", result.stdout)
                self.assertFalse(any(call["args"] == ["test", "--workspace", "--all-targets"]
                                     for call in self.cargo_calls()))

    def test_train_stops_before_any_push_when_fast_gate_cannot_pass(self):
        shutil.copyfile(SOURCE.with_name("train.sh"), self.root / "scripts/train.sh")
        (self.root / "scripts/prepush.sh").write_text(
            '#!/bin/bash\nprintf "fast gate args: %s\\n" "$*"\nexit 1\n'
        )
        (self.bin / "git").unlink()
        self.stub("git", '''
import os, sys
from pathlib import Path
if sys.argv[1] == "rev-parse":
    print("abcdef123456")
if sys.argv[1] == "push":
    Path(os.environ["PREPUSH_TEST_ROOT"], "unexpected-push").touch()
    sys.exit(97)
''')
        self.stub("sleep", "pass\n")
        result = subprocess.run(
            [BASH, "scripts/train.sh", "gate-test"], cwd=self.root, env=self.env,
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=15,
        )
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("fast gate args: --fast", result.stdout)
        self.assertFalse((self.root / "unexpected-push").exists(), result.stdout)


if __name__ == "__main__":
    print(sys.version, flush=True)
    unittest.main(verbosity=2)
