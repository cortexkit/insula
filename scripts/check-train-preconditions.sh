#!/usr/bin/env bash
# Assert the three facts the CI-gated push shape depends on, every run.
#
# WHY THIS IS A CI STEP AND NOT A NOTE. Each of these was verified by hand once,
# reported, and then trusted. They are facts about a file that keeps being
# edited, and nothing would notice a narrowed trigger until the day branch
# protection is enabled -- which is the worst possible moment to learn it, since
# the symptom is main becoming unpushable and it presents as a protection fault
# rather than a workflow one.
#
# The three, and what each failure costs:
#
#   1. the push trigger includes train/**   a train branch otherwise produces NO
#                                           check at all, so protection can never
#                                           be satisfied
#   2. no job/step `if` on github.ref       the workflow fires on a train and the
#      or github.event_name                 gated job SKIPS, so the train looks
#                                           checked while its gate never ran
#   3. concurrency.group keys on            a shared group makes a train push
#      github.ref                           CANCEL an in-flight master run
#
# SCOPED TO THE CHECK-PRODUCING WORKFLOW, which is the design point worth
# stating. release.yml is tag- and dispatch-driven, produces no check on a train
# push, and legitimately has no concurrency group -- asserting these three across
# every workflow would fail on it permanently. A check that is red by
# construction gets muted, and takes the real signal with it.
#
# Reads each `if` VALUE by parsing rather than grepping: a folded `if: >-` puts
# the condition on the lines BELOW the key, so `grep 'if:.*github.ref'` returns
# clean on a workflow that is entirely path-dependent.
#
# BEFORE READING THE REAL WORKFLOW IT CHECKS ITSELF against planted workflows:
# a clean one must pass, and each planted drift must be caught by its own rule
# and no other. A rule that silently matches nothing (a walker that no longer
# finds the `if` key, a trigger lookup that reads the wrong field) reports
# clean on the real file for the wrong reason, and only a planted violation can
# tell those apart. A failed self-check exits 2, as "cannot check".
#
# Exit 0 clean, 1 on drift naming the condition, 2 if it cannot check.

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2

WORKFLOW=".github/workflows/ci.yml"
DEFAULT_BRANCH="master"

[ -f "$WORKFLOW" ] || { echo "cannot check: $WORKFLOW is absent" >&2; exit 2; }

python3 - "$WORKFLOW" "$DEFAULT_BRANCH" <<'PY'
import sys, yaml

path, default_branch = sys.argv[1], sys.argv[2]


def walk(node, where):
    """Yield every `if` value, reading folded scalars the parser resolved."""
    if isinstance(node, dict):
        for key, value in node.items():
            if key == "if":
                yield where, str(value).strip()
            yield from walk(value, f"{where}.{key}")
    elif isinstance(node, list):
        for index, value in enumerate(node):
            yield from walk(value, f"{where}[{index}]")


def check(doc):
    """Return (failures as (rule, message), summary) for one parsed workflow.

    Returns None when the workflow has no trigger mapping, which is "cannot
    check" rather than a drift.
    """
    # `on` is the YAML 1.1 boolean True, not the string, which is the trap that
    # makes a hand-written scanner silently find no triggers at all.
    triggers = doc.get(True, doc.get("on"))
    if not isinstance(triggers, dict):
        return None
    failures = []

    branches = (triggers.get("push") or {}).get("branches") or []
    if not any("train/" in str(b) for b in branches):
        failures.append(("train-trigger",
            f"push trigger does not include train/**: {branches!r} -- a train branch "
            "produces no check, so protection can never be satisfied"))
    if default_branch not in [str(b) for b in branches]:
        failures.append(("default-trigger",
            f"push trigger does not include {default_branch!r}: {branches!r} -- while "
            "this repo is unprotected that trigger is the only observer of an "
            "off-train push"))

    conditions = list(walk(doc.get("jobs") or {}, "jobs"))
    for where, value in conditions:
        if "github.ref" in value or "github.event_name" in value:
            failures.append(("path-dependent-if",
                f"path-dependent condition at {where}: {value!r} -- fires on a train "
                "push and SKIPS, so the train looks checked while its gate never ran"))

    group = str((doc.get("concurrency") or {}).get("group", ""))
    if group and "github.ref" not in group:
        failures.append(("concurrency",
            f"concurrency.group {group!r} does not key on github.ref -- a train push "
            "would cancel an in-flight " + default_branch + " run"))

    # The denominator, so a clean pass cannot be confused with a scan that
    # examined nothing. A parse that silently found zero jobs would otherwise
    # print the same reassuring line as a healthy workflow.
    summary = (f"push branches {branches!r}, {len(conditions)} if-condition(s) read, "
               f"concurrency.group {group or 'none'!r}")
    return failures, summary


# Self-check. Each drift breaks exactly one rule. The two `if` cases sit at
# different depths (job and step) and one is a folded `>-` scalar, because those
# are the shapes a walker or a grep has missed before.
CLEAN = f"""
on:
  push:
    branches: [{default_branch}, 'train/**']
concurrency:
  group: ci-${{{{ github.ref }}}}
jobs:
  test:
    if: always()
    steps:
      - run: cargo test
        if: success()
"""
DRIFTS = {
    "train-trigger": CLEAN.replace(", 'train/**'", ""),
    "default-trigger": CLEAN.replace(f"[{default_branch}, ", "["),
    "path-dependent-if": CLEAN.replace(
        "        if: success()",
        "        if: >-\n          github.ref == 'refs/heads/" + default_branch + "'"),
    "concurrency": CLEAN.replace("ci-${{ github.ref }}", "ci-shared"),
}
blind = []
clean_result = check(yaml.safe_load(CLEAN))
if clean_result is None or clean_result[0]:
    blind.append(f"the clean planted workflow did not pass: {clean_result!r}")
for rule, text in DRIFTS.items():
    result = check(yaml.safe_load(text))
    caught = [] if result is None else sorted({r for r, _ in result[0]})
    if caught != [rule]:
        blind.append(f"planted {rule} drift was reported as {caught or 'clean'}")
# A job-level event condition, so both the variable and the depth are covered.
job_level = check(yaml.safe_load(CLEAN.replace(
    "    if: always()", "    if: github.event_name == 'push'")))
if job_level is None or sorted({r for r, _ in job_level[0]}) != ["path-dependent-if"]:
    blind.append(f"planted job-level event condition was reported as {job_level!r}")
if blind:
    for line in blind:
        print(f"cannot check: checker is BLIND -- {line}", file=sys.stderr)
    sys.exit(2)
print(f"  self-check: clean planted workflow passes, {len(DRIFTS) + 1} planted drifts each "
      "caught by their own rule only")

try:
    doc = yaml.safe_load(open(path))
except Exception as exc:                      # noqa: BLE001 - report, do not mask
    print(f"cannot check: {path} did not parse: {exc}", file=sys.stderr)
    sys.exit(2)

result = check(doc)
if result is None:
    print(f"cannot check: {path} has no mapping of triggers", file=sys.stderr)
    sys.exit(2)
failures, summary = result
print(f"train preconditions: {path}, {summary}")

if failures:
    for _rule, failure in failures:
        print(f"  DRIFT: {failure}")
    sys.exit(1)
print("  ok: trigger covers train/**, no path-dependent gate, per-ref concurrency")
PY
