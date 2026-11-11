#!/usr/bin/env python3
"""Offline policy tests; every shell side effect stays inside a disposable repo."""

import copy
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import zipfile
from contextlib import redirect_stdout
from typing import Optional
from unittest.mock import patch

import release
import release_ci


ROOT = Path(__file__).resolve().parents[2]
SCRATCH = ROOT / "target" / "scratch"
SCRATCH.mkdir(parents=True, exist_ok=True)


class MetadataTests(unittest.TestCase):
    def test_semver_order_is_numeric_and_strict(self):
        release.check_version("0.10.0", "v0.9.9\nv0.2.0\nvgarbage")
        for version in ("0.2.0", "0.1.9", "01.3.0", "1.2", "1.2.3-rc.1", "../1.2.3"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.check_version(version, "v0.2.0")

    def test_index_requires_version_and_floor_together(self):
        data = {"components": {"insula": {"version": "0.2.1", "requires_core": "0.21.3"}}}
        self.assertTrue(release.index_matches(data, "0.2.1", "0.21.3"))
        self.assertFalse(release.index_matches(data, "0.2.0", "0.21.3"))
        self.assertFalse(release.index_matches(data, "0.2.1", "0.21.4"))
        self.assertFalse(release.index_matches({}, "0.2.1", "0.21.3"))

    def test_core_checksum_and_named_root_binary(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temporary:
            directory = Path(temporary)
            archive = directory / "ck-subc-linux-x64.zip"
            with zipfile.ZipFile(archive, "w") as bundle:
                bundle.writestr("ck-subc", b"public core fixture")
                bundle.writestr("../../escaped", b"must never extract")
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            sidecar = Path(str(archive) + ".sha256")
            sidecar.write_text(f"{digest}  {archive.name}\n")
            binary = Path(release.unpack_core(archive, directory))
            self.assertEqual(binary.read_bytes(), b"public core fixture")
            self.assertEqual(sorted(p.name for p in directory.iterdir()), ["ck-subc", archive.name, sidecar.name])
            sidecar.write_text(f"{'0' * 64}  {archive.name}\n")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                release.unpack_core(archive, directory)
            sidecar.write_text(f"{digest}  wrong.zip\n")
            with self.assertRaisesRegex(ValueError, "must name"):
                release.unpack_core(archive, directory)

    def test_lock_refresh_rejects_dependency_drift(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as temporary:
            directory = Path(temporary)
            before = directory / "before"
            after = directory / "after"
            old = '\n'.join(f'[[package]]\nname = "{name}"\nversion = "{version}"' for name, version in (
                ("quota-core", "0.2.0"), ("quota-module", "0.2.0"), ("dependency", "1.0.0")))
            before.write_text(old)
            updated = old.replace('"0.2.0"', '"0.2.1"')
            after.write_text(updated)
            release.check_lock(before, after, "0.2.1")
            after.write_text(updated.replace('"1.0.0"', '"1.0.1"'))
            with self.assertRaisesRegex(ValueError, "more than"):
                release.check_lock(before, after, "0.2.1")

    def test_post_tag_status_distinguishes_absent_unknown_and_published_assets(self):
        for code, payload, expected in (
            (1, {"status": "404"}, "absent (API returned HTTP 404)"),
            (1, {"status": "403"}, "unknown; API query failed"),
            (0, {"html_url": "fixture URL", "assets": [{"name": "fixture.zip"}]}, "published asset: fixture.zip"),
            (0, {"html_url": "fixture URL", "assets": []}, "published assets: none"),
        ):
            result = subprocess.CompletedProcess([], code, json.dumps(payload), "fixture error")
            output = io.StringIO()
            with patch.object(release.subprocess, "run", return_value=result), redirect_stdout(output):
                release.publication_status("example/repository", "v0.2.1")
            self.assertIn(expected, output.getvalue())


class CiPolicyTests(unittest.TestCase):
    def setUp(self):
        self.ci_run = dict(databaseId=42, headSha="a" * 40, headBranch="master", event="push", status="completed", conclusion="success")

    def test_only_exact_sha_master_or_train_push_counts(self):
        self.assertTrue(release_ci.eligible(self.ci_run, "a" * 40))
        for field, value in (("headSha", "b" * 40), ("headBranch", "feature/topic"), ("event", "pull_request"),
                             ("event", "schedule"), ("event", "workflow_dispatch"), ("status", "in_progress"), ("conclusion", "failure")):
            with self.subTest(field=field, value=value):
                self.assertFalse(release_ci.eligible({**self.ci_run, field: value}, "a" * 40))
        self.assertTrue(release_ci.eligible({**self.ci_run, "headBranch": "train/release-0.2.1"}, "a" * 40))

    def test_missing_skipped_failed_or_duplicate_red_job_refuses(self):
        jobs: list[dict[str, Optional[str]]] = [dict(name=name, status="completed", conclusion="success") for name in ("Test (ubuntu)", "Test (windows)")]
        required = {"Test (ubuntu)", "Test (windows)"}
        self.assertEqual(release_ci.missing_checks(jobs, required), [])
        self.assertIn("Test (windows): missing", release_ci.missing_checks(jobs[:1], required))
        for conclusion in ("skipped", "failure", None, "neutral"):
            changed = copy.deepcopy(jobs)
            changed[1]["conclusion"] = conclusion
            self.assertTrue(release_ci.missing_checks(changed, required))
        self.assertTrue(release_ci.missing_checks(jobs + [dict(name="Test (ubuntu)", status="completed", conclusion="failure")], required))

    def test_required_names_cannot_disappear_when_protection_is_readable(self):
        listed = "# fallback\nTest (ubuntu)\nTest (windows)\n"
        self.assertEqual(release_ci.required_checks(None, listed), {"Test (ubuntu)", "Test (windows)"})
        self.assertEqual(release_ci.required_checks({"contexts": ["extra"], "checks": [{"context": "other"}]}, listed),
                         {"Test (ubuntu)", "Test (windows)", "extra", "other"})
        with self.assertRaisesRegex(ValueError, "no required"):
            release_ci.required_checks(None, "# empty")

    def test_gate_cannot_combine_checks_from_separate_runs(self):
        other = {**self.ci_run, "databaseId": 43}
        ubuntu = dict(name="Test (ubuntu)", status="completed", conclusion="success")
        windows = dict(name="Test (windows)", status="completed", conclusion="success")
        responses = [None, [self.ci_run, other], {**other, "jobs": [windows]}, {**self.ci_run, "jobs": [ubuntu]}]
        with patch.object(release_ci.Path, "read_text", return_value="Test (ubuntu)\nTest (windows)"), \
                patch.object(release_ci, "gh_json", side_effect=responses), \
                self.assertRaisesRegex(ValueError, "no eligible run has every required check green"):
            release_ci.main("a" * 40)

    def test_gate_accepts_train_and_rejects_a_rerun_that_changed_status(self):
        train = {**self.ci_run, "headBranch": "train/release-0.2.1"}
        jobs = [dict(name=name, status="completed", conclusion="success") for name in ("Test (ubuntu)", "Test (windows)")]
        for status in ("completed", "in_progress"):
            responses = [None, [train], {**train, "status": status, "jobs": jobs}]
            with patch.object(release_ci.Path, "read_text", return_value="Test (ubuntu)\nTest (windows)"), \
                    patch.object(release_ci, "gh_json", side_effect=responses):
                if status == "completed":
                    release_ci.main("a" * 40)
                else:
                    with self.assertRaisesRegex(ValueError, "no longer successful/eligible"):
                        release_ci.main("a" * 40)


@unittest.skipIf(os.name == "nt", "shell driver is exercised on Linux and macOS Bash 3.2; metadata policy tests run natively on Windows")
class DriverTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(dir=SCRATCH)
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.repo = self.directory / "repo"
        self.repo.mkdir()
        self.bin = self.directory / "bin"
        self.bin.mkdir()
        self.calls = self.directory / "calls"
        real_git = shutil.which("git")
        assert real_git is not None, "git must be installed"
        self.real_git = real_git
        self.env = {**os.environ, "PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
                    "CALLS": str(self.calls), "REAL_GIT": self.real_git, "FIXTURES": str(self.directory),
                    "PYTHONDONTWRITEBYTECODE": "1", "GIT_CONFIG_NOSYSTEM": "1"}
        for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_CONFIG_COUNT"):
            self.env.pop(key, None)
        (self.repo / "scripts/lib").mkdir(parents=True)
        (self.repo / ".cortexkit/release-notes").mkdir(parents=True)
        shutil.copy2(ROOT / "scripts/release.sh", self.repo / "scripts/release.sh")
        shutil.copy2(ROOT / "scripts/lib/release.py", self.repo / "scripts/lib/release.py")
        (self.repo / ".gitignore").write_text("/target\n__pycache__/\n")
        (self.repo / "release-manifest.json").write_text('{"requires_core":"0.21.3"}')
        (self.repo / "Cargo.lock").write_text('version = 4\n[[package]]\nname = "quota-core"\nversion = "0.2.0"\n[[package]]\nname = "quota-module"\nversion = "0.2.0"\n')
        for name in ("quota-core", "quota-module"):
            path = self.repo / f"crates/{name}/Cargo.toml"
            path.parent.mkdir(parents=True)
            path.write_text(f'[package]\nname = "{name}"\nversion = "0.2.0"\n')
        self.git("init", "-b", "master")
        # Reuse the script's configured release identity; these tests do not
        # depend on the identity's literal spelling.
        identity = (ROOT / "scripts/release.sh").read_text().split('[ "$(git config user.email)" = ', 1)[1].split("'", 2)[1]
        self.git("config", "user.email", identity)
        self.git("config", "user.name", "Release test")
        self.commit_fixture()
        self.write_mock("git", '''#!/usr/bin/env bash
printf 'git %s\\n' "$*" >> "$CALLS"
case "$1" in
  fetch) [ "${FAIL_FETCH:-0}" = 0 ]; exit $? ;;
  tag) if [ "$2" = --list ]; then printf '%s\\n' "${TAGS:-v0.2.0}"; exit 0; fi
       [ "${LIVE_FIXTURE:-0}" = 1 ] && [ "$2" = -a ]; exit $? ;;
  push) [ "${LIVE_FIXTURE:-0}" = 1 ]; exit $? ;;
esac
exec "$REAL_GIT" "$@"
''')
        self.write_mock("gh", '''#!/usr/bin/env bash
printf 'gh %s\\n' "$*" >> "$CALLS"
case "$1 $2" in
  'auth status') exit 0 ;;
  'run list') [ "${LIVE_FIXTURE:-0}" = 1 ] || exit 96; echo 42 ;;
  'run watch') exit "${WORKFLOW_RC:-0}" ;;
  'run view') if [ "${WORKFLOW_RC:-0}" = 0 ]; then echo completed/success; else echo completed/failure; fi ;;
  api*) echo '{"html_url":"fixture URL","assets":[{"name":"fixture.zip"}]}' ;;
  *) exit 97 ;;
esac
''')
        self.write_mock("cargo", '''#!/usr/bin/env bash
printf 'cargo %s\\n' "$*" >> "$CALLS"
if [ "$*" = 'update --workspace --offline' ] && [ "${LIVE_FIXTURE:-0}" = 1 ]; then
  python3 -c 'from pathlib import Path; p=Path("Cargo.lock"); p.write_text(p.read_text().replace("0.2.0", "0.2.1"))'
  exit 0
fi
[ "$*" = 'test -p quota-module --test it real_daemon_e2e -- --ignored' ] || exit 93
[ -f "$CK_SUBC_BIN" ] || exit 94
[ "${FOREIGN_EDIT:-0}" = 0 ] || echo foreign > foreign
echo 'floor fixture: 2 passed; 0 failed'
exit "${FLOOR_RC:-0}"
''')
        self.write_mock("curl", '''#!/usr/bin/env bash
printf 'curl %s\\n' "$*" >> "$CALLS"
url=''; output=''
while [ "$#" -gt 0 ]; do
  case "$1" in https://*) url="$1" ;; -o) shift; output="$1" ;; esac
  shift
done
case "$url" in
  https://github.com/cortexkit/subconscious/releases/download/subc-core-v0.21.3/ck-subc-*.zip*)
    cp "$FIXTURES/${url##*/}" "$output" ;;
  https://cortexkit.io/releases/v1/index.json)
    if [ ! -f "$FIXTURES/index-polled" ]; then
      echo '{"components":{"insula":{"version":"0.2.1","requires_core":"0.21.2"}}}' > "$output"
      touch "$FIXTURES/index-polled"
    else
      echo '{"components":{"insula":{"version":"0.2.1","requires_core":"0.21.3"}}}' > "$output"
    fi ;;
  *) echo 'unexpected URL' >&2; exit 95 ;;
esac
''')
        self.write_mock("sleep", '#!/usr/bin/env bash\nprintf "sleep %s\\n" "$*" >> "$CALLS"\n')
        (self.repo / "scripts/train.sh").write_text('#!/usr/bin/env bash\nprintf "train %s\\n" "$*" >> "$CALLS"\ngit update-ref refs/remotes/origin/master HEAD\n')
        (self.repo / "scripts/train.sh").chmod(0o755)
        for system in ("linux", "darwin"):
            for arch in ("x64", "arm64"):
                archive = self.directory / f"ck-subc-{system}-{arch}.zip"
                with zipfile.ZipFile(archive, "w") as bundle:
                    bundle.writestr("ck-subc", b"floor test fixture")
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                Path(str(archive) + ".sha256").write_text(f"{digest}  {archive.name}\n")
        self.commit_fixture()

    def git(self, *args):
        return subprocess.check_output([self.real_git, *args], cwd=self.repo, env=self.env, text=True, stderr=subprocess.STDOUT).strip()

    def commit_fixture(self):
        self.git("add", ".")
        subprocess.run([self.real_git, "commit", "-F", "-"], cwd=self.repo, env=self.env, input="Release fixture\n", text=True, check=True, capture_output=True)
        self.git("update-ref", "refs/remotes/origin/master", "HEAD")

    def write_mock(self, name, content):
        path = self.bin / name
        path.write_text(content)
        path.chmod(0o755)

    def notes(self):
        (self.repo / ".cortexkit/release-notes/v0.2.1.md").write_text("Temporary release notes fixture\n")
        self.commit_fixture()

    def invoke(self, *args):
        return subprocess.run([os.environ.get("RELEASE_TEST_BASH", "bash"), "scripts/release.sh", *(args or ("0.2.1", "--dry-run"))], cwd=self.repo, env=self.env,
                              text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    def test_notes_refusal_then_dry_run_passes_without_side_effects(self):
        refused = self.invoke()
        self.assertEqual(refused.returncode, 2, refused.stdout)
        self.assertIn("missing or empty .cortexkit/release-notes/v0.2.1.md", refused.stdout)
        print("notes refusal evidence:\n" + refused.stdout)
        self.notes()
        sha = self.git("rev-parse", "HEAD")
        lock = (self.repo / "Cargo.lock").read_bytes()
        passed = self.invoke()
        self.assertEqual(passed.returncode, 0, passed.stdout)
        self.assertIn("dry run passed for v0.2.1", passed.stdout)
        self.assertEqual(self.git("rev-parse", "HEAD"), sha)
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertEqual((self.repo / "Cargo.lock").read_bytes(), lock)
        calls = self.calls.read_text()
        self.assertNotIn("git push", calls)
        self.assertNotIn("git commit", calls)
        self.assertNotIn("cargo update", calls)
        self.assertNotIn("git tag -a", calls)
        self.assertIn("subc-core-v0.21.3/", calls)
        print("dry-run evidence (mocked downloads and floor test):\n" + passed.stdout)

    def test_refuses_dirty_nonmaster_diverged_and_failed_fetch(self):
        self.notes()
        self.git("checkout", "-b", "topic")
        result = self.invoke()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("checkout must be on master", result.stdout)
        self.git("checkout", "master")
        (self.repo / "foreign").write_text("dirty")
        self.assertIn("checkout is dirty", self.invoke().stdout)
        (self.repo / "foreign").unlink()
        self.git("update-ref", "refs/remotes/origin/master", "HEAD^")
        self.assertIn("master is not equal", self.invoke().stdout)
        self.git("update-ref", "refs/remotes/origin/master", "HEAD")
        self.env["FAIL_FETCH"] = "1"
        self.assertIn("fetch of origin/master and tags failed", self.invoke().stdout)

    def test_refuses_existing_or_older_tags_bad_manifest_and_failed_floor(self):
        self.notes()
        for tags in ("v0.2.1", "v0.3.0"):
            self.env["TAGS"] = tags
            result = self.invoke()
            self.assertEqual(result.returncode, 2, result.stdout)
            self.assertIn("version/tag check failed", result.stdout)
        self.env.pop("TAGS")
        self.env["FLOOR_RC"] = "1"
        result = self.invoke()
        self.assertEqual(result.returncode, 2, result.stdout)
        self.assertIn("real_daemon_e2e failed against public core 0.21.3", result.stdout)
        (self.repo / "release-manifest.json").write_text('{"requires_core":"invalid"}')
        self.commit_fixture()
        self.assertIn("invalid release-manifest.json", self.invoke().stdout)

    def test_live_orchestration_uses_pathspec_train_sha_and_checks_index_floor(self):
        self.notes()
        self.env["LIVE_FIXTURE"] = "1"
        passed = self.invoke("0.2.1")
        self.assertEqual(passed.returncode, 0, passed.stdout)
        self.assertIn("insula 0.2.1 requires_core 0.21.3 confirmed", passed.stdout)
        calls = self.calls.read_text()
        self.assertIn("git commit --only -F - -- crates/quota-core/Cargo.toml crates/quota-module/Cargo.toml Cargo.lock", calls)
        self.assertIn("train release-0.2.1", calls)
        sha = self.git("rev-parse", "HEAD")
        self.assertIn(f"git tag -a v0.2.1 {sha}", calls)
        self.assertIn("git push origin refs/tags/v0.2.1", calls)
        self.assertEqual(calls.count("https://cortexkit.io/releases/v1/index.json"), 2)
        self.assertIn("sleep 30", calls)
        # Tags and pushes are mocks even in the live-orchestration fixture.
        self.assertEqual(self.git("for-each-ref", "refs/tags"), "")
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_failure_after_mocked_tag_names_publication_boundary(self):
        self.notes()
        self.env.update(LIVE_FIXTURE="1", WORKFLOW_RC="1")
        failed = self.invoke("0.2.1")
        self.assertEqual(failed.returncode, 2, failed.stdout)
        self.assertIn("tag v0.2.1: pushed to origin", failed.stdout)
        self.assertIn("workflow: run 42 completed/failure; index: not confirmed", failed.stdout)
        self.assertIn("published asset: fixture.zip", failed.stdout)
        self.assertNotIn("https://cortexkit.io/releases/v1/index.json", self.calls.read_text())

    def test_foreign_edit_blocks_pathspec_commit(self):
        self.notes()
        self.env.update(LIVE_FIXTURE="1", FOREIGN_EDIT="1")
        failed = self.invoke("0.2.1")
        self.assertEqual(failed.returncode, 2, failed.stdout)
        self.assertIn("foreign changes appeared", failed.stdout)
        self.assertNotIn("git commit", self.calls.read_text())

    def test_unbound_variable_cannot_report_success_under_exit_trap(self):
        self.notes()
        script = self.repo / "scripts/release.sh"
        script.write_text(script.read_text().replace('trap on_exit EXIT', 'trap on_exit EXIT\necho "$release_unbound_probe"'))
        self.commit_fixture()
        failed = self.invoke()
        self.assertEqual(failed.returncode, 2, failed.stdout)
        self.assertIn("release did not complete", failed.stdout)


if __name__ == "__main__":
    unittest.main()
