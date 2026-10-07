#!/usr/bin/env python3
"""Feed each Python and shell scanner a deliberately planted violation.

These tests check that each scanner refuses the violation. The mutations.toml
rows break each scanner to prove these tests catch it.
"""

import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest


# Mutation replays repeatedly reload different bytes at the same paths. Do not
# leave compiled scanner caches behind or let them hide a restored source edit.
sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / filename)
    assert spec is not None and spec.loader is not None, "scanner must be loadable"
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ScanControls(unittest.TestCase):
    def test_broad_report_planted_violation(self):
        load("mutation_report", "mutation-report.py").self_check()

    def test_path_dependency_planted_violation(self):
        load("path_dependencies", "path-dependencies.py").self_check()

    def test_production_body_planted_violation(self):
        load("prod_body", "prod_body.py").self_check()

    def test_train_preconditions_planted_violations(self):
        result = subprocess.run(
            ["bash", "scripts/check-train-preconditions.sh"],
            cwd=ROOT, text=True, capture_output=True, timeout=60,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("5 planted drifts each caught by their own rule only", result.stdout)


if __name__ == "__main__":
    print(sys.version.splitlines()[0], flush=True)
    unittest.main()
