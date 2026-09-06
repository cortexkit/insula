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
# Exit 0 clean, 1 on drift naming the condition, 2 if it cannot check.

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2

WORKFLOW=".github/workflows/ci.yml"
DEFAULT_BRANCH="master"

[ -f "$WORKFLOW" ] || { echo "cannot check: $WORKFLOW is absent" >&2; exit 2; }

python3 - "$WORKFLOW" "$DEFAULT_BRANCH" <<'PY'
import sys, yaml

path, default_branch = sys.argv[1], sys.argv[2]
try:
    doc = yaml.safe_load(open(path))
except Exception as exc:                      # noqa: BLE001 - report, do not mask
    print(f"cannot check: {path} did not parse: {exc}", file=sys.stderr)
    sys.exit(2)

# `on` is the YAML 1.1 boolean True, not the string, which is the trap that makes
# a hand-written scanner silently find no triggers at all.
triggers = doc.get(True, doc.get("on"))
if not isinstance(triggers, dict):
    print(f"cannot check: {path} has no mapping of triggers", file=sys.stderr)
    sys.exit(2)

failures = []

branches = (triggers.get("push") or {}).get("branches") or []
if not any("train/" in str(b) for b in branches):
    failures.append(
        f"push trigger does not include train/**: {branches!r} -- a train branch "
        "produces no check, so protection can never be satisfied"
    )
if default_branch not in [str(b) for b in branches]:
    failures.append(
        f"push trigger does not include {default_branch!r}: {branches!r} -- while "
        "this repo is unprotected that trigger is the only observer of an "
        "off-train push"
    )

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

conditions = list(walk(doc.get("jobs") or {}, "jobs"))
for where, value in conditions:
    if "github.ref" in value or "github.event_name" in value:
        failures.append(
            f"path-dependent condition at {where}: {value!r} -- fires on a train "
            "push and SKIPS, so the train looks checked while its gate never ran"
        )

group = str((doc.get("concurrency") or {}).get("group", ""))
if group and "github.ref" not in group:
    failures.append(
        f"concurrency.group {group!r} does not key on github.ref -- a train push "
        "would cancel an in-flight " + default_branch + " run"
    )

# The denominator, so a clean pass cannot be confused with a scan that examined
# nothing. A parse that silently found zero jobs would otherwise print the same
# reassuring line as a healthy workflow.
print(
    f"train preconditions: {path}, "
    f"push branches {branches!r}, "
    f"{len(conditions)} if-condition(s) read, "
    f"concurrency.group {group or 'none'!r}"
)

if failures:
    for failure in failures:
        print(f"  DRIFT: {failure}")
    sys.exit(1)
print("  ok: trigger covers train/**, no path-dependent gate, per-ref concurrency")
PY
