#!/usr/bin/env python3
"""Exercise deploy provenance against real local Git histories, never the fleet."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SOURCE = Path(__file__).resolve().with_name("deploy.sh")


class DeployPolicyTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="insula-deploy-test-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.repo = self.root / "repo"
        self.remote = self.root / "origin.git"
        self.home = self.root / "home"
        self.home.mkdir()
        self.env = os.environ.copy()
        self.env.pop("CK_DEPLOY_ALLOW_DIRTY", None)
        for key in list(self.env):
            if key.startswith("GIT_"):
                self.env.pop(key)
        self.env.update(
            HOME=str(self.home),
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_AUTHOR_NAME="Deploy test",
            GIT_AUTHOR_EMAIL="deploy-test@example.invalid",
            GIT_COMMITTER_NAME="Deploy test",
            GIT_COMMITTER_EMAIL="deploy-test@example.invalid",
            GIT_TERMINAL_PROMPT="0",
        )
        self.git("init", "--bare", str(self.remote), cwd=self.root)
        self.repo.mkdir()
        self.git("init", "-b", "master")
        (self.repo / "scripts").mkdir()
        shutil.copyfile(SOURCE, self.repo / "scripts/deploy.sh")
        self.git("add", ".")
        self.git("commit", "-m", "initial deploy script")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "-u", "origin", "master")
        self.base = self.git("rev-parse", "HEAD").strip()

        # Any accidental build, signature, placement or restart must fail loudly.
        self.marker = self.root / "unexpected-side-effect"
        stubs = self.root / "bin"
        stubs.mkdir()
        for command in ("cargo", "codesign", "ck", "cp", "mv", "sleep"):
            stub = stubs / command
            stub.write_text(
                '#!/bin/bash\nprintf "%s\\n" "$0 $*" >> "$DEPLOY_TEST_MARKER"\nexit 97\n'
            )
            stub.chmod(0o755)
        self.env["PATH"] = f"{stubs}{os.pathsep}{self.env['PATH']}"
        self.env["DEPLOY_TEST_MARKER"] = str(self.marker)

    def git(self, *args, cwd=None):
        return subprocess.run(
            ["git", *args], cwd=cwd or self.repo, env=self.env,
            check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        ).stdout

    def commit(self, name):
        (self.repo / name).write_text(name)
        self.git("add", name)
        self.git("commit", "-m", name)
        return self.git("rev-parse", "HEAD").strip()

    def remote_commit(self):
        sha = self.commit("remote-change")
        self.git("push", "origin", "master")
        self.git("reset", "--hard", self.base)
        # Model another seat advancing master while this seat's tracking ref is stale.
        self.git("update-ref", "refs/remotes/origin/master", self.base)
        return sha

    def deploy(self, *args, override=False):
        env = self.env.copy()
        if override:
            env["CK_DEPLOY_ALLOW_DIRTY"] = "1"
        result = subprocess.run(
            ["bash", "scripts/deploy.sh", *args], cwd=self.repo, env=env,
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        print(f"{self._testMethodName}:\n{result.stdout}", flush=True)
        self.assertFalse(self.marker.exists(), "build/install/restart command executed")
        self.assertEqual(list(self.home.iterdir()), [], "dry-run wrote under HOME")
        return result

    def refuses(self, text, *args, override=False):
        result = self.deploy(*args, override=override)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(text, result.stdout)

    def test_dirty_tree_refuses_before_build(self):
        (self.repo / "uncommitted").write_text("not in HEAD")
        self.refuses("working tree is dirty")

    def test_non_master_refuses_before_build(self):
        self.git("switch", "-c", "feature")
        self.refuses("on branch 'feature', not master")

    def test_behind_master_refuses(self):
        self.remote_commit()
        self.refuses("behind origin/master")

    def test_diverged_master_refuses(self):
        self.remote_commit()
        self.commit("local-change")
        self.refuses("diverged from origin/master")

    def test_ahead_master_accepts_and_lists_unlanded_commits(self):
        first = self.commit("first-local-change")
        second = self.commit("second-local-change")
        result = self.deploy("--dry-run")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("commits not yet landed", result.stdout)
        self.assertIn(f"{first[:7]} first-local-change", result.stdout)
        self.assertIn(f"{second[:7]} second-local-change", result.stdout)
        self.assertIn(f"buildCommit={second[:12]}", result.stdout)
        self.assertIn("nothing built, signed, installed or restarted", result.stdout)

    def test_equal_master_accepts_without_unlanded_list(self):
        result = self.deploy("--dry-run")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("commits not yet landed", result.stdout)

    def test_fetch_failure_refuses_even_with_cached_origin(self):
        self.git("remote", "set-url", "origin", str(self.root / "missing.git"))
        self.refuses("cannot fetch origin/master")

    def test_dirty_override_cannot_deploy_behind(self):
        self.remote_commit()
        self.refuses("behind origin/master", override=True)

    def test_dirty_override_cannot_deploy_divergence(self):
        self.remote_commit()
        self.commit("local-change")
        self.refuses("diverged from origin/master", override=True)

    def test_stage_dry_run_does_not_write_a_card(self):
        result = self.deploy("--stage", "--dry-run")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("would write a staging card", result.stdout)

    def test_dry_run_still_refuses_dirty_tree(self):
        (self.repo / "uncommitted").write_text("not in HEAD")
        self.refuses("working tree is dirty", "--dry-run")

    def test_unknown_argument_refuses(self):
        result = self.deploy("--unknown")
        self.assertEqual(result.returncode, 2, result.stdout)
        # The whole usage line, so a usage that stopped naming the flags fails.
        self.assertIn("usage: scripts/deploy.sh [--stage] [--dry-run]", result.stdout)


if __name__ == "__main__":
    print(subprocess.check_output(["git", "--version"], text=True).strip(), flush=True)
    unittest.main(verbosity=2)
