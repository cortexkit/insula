#!/usr/bin/env bash
# Push HEAD through a train branch, wait for the checks branch protection will
# actually read, then land master.
#
# This was a remembered incantation until 2026-09-13, when it cost four rejected
# pushes in one sitting and destroyed the evidence for one of them. Every rule
# below is one of those failures.
#
#   usage: scripts/train.sh <name>
#          scripts/train.sh refusal-body
set -uo pipefail

REPO="cortexkit/insula"
name="${1:-}"
if [ -z "$name" ]; then
  echo "usage: scripts/train.sh <branch-suffix>" >&2
  exit 2
fi
branch="train/$name"

sha=$(git rev-parse HEAD)
echo "  train: $branch @ ${sha:0:12}"

if [ -n "$(git status --porcelain)" ]; then
  echo "  REFUSED: the working tree is dirty, so the sha pushed is not the tree tested" >&2
  exit 2
fi

# A re-push to an existing train branch leaves TWO runs on one sha, and the
# cancelled one can be the newer. Delete first so the sha gets exactly one.
git push -q --delete "origin" "$branch" 2>/dev/null
sleep 2
if ! git push -q origin "HEAD:refs/heads/$branch"; then
  echo "  REFUSED: could not push the train branch" >&2
  exit 2
fi

# SELECT THE RUN BY SHA AND RECENCY, NOT `gh run list --limit 1`.
# Branch protection reads the newest run for the sha. Watching an older one
# reports success while the push is rejected for "required status checks are
# cancelled" -- which is exactly what happened, with a green run I had watched.
run=""
for _ in $(seq 1 15); do
  sleep 4
  run=$(gh api "repos/$REPO/actions/runs?head_sha=$sha" \
        --jq '[.workflow_runs[]] | sort_by(.created_at) | last | .id' 2>/dev/null)
  [ -n "$run" ] && [ "$run" != "null" ] && break
done

# AN EMPTY RUN ID MUST REFUSE, NOT WATCH.
# `gh run watch ""` exits 1, the same code as a failed run, so "no run yet" and
# "CI failed" become one answer. The branch is deliberately left in place.
if [ -z "$run" ] || [ "$run" = "null" ]; then
  echo "  INCONCLUSIVE: no workflow run appeared for ${sha:0:12} after 60s" >&2
  echo "  the branch is left at $branch so the state can be inspected" >&2
  exit 2
fi

echo "  watching run $run"
gh run watch "$run" --exit-status >/dev/null 2>&1
watch_rc=$?

# Read the conclusion back rather than trusting the exit code, for the same
# reason: one code, several meanings.
conclusion=$(gh api "repos/$REPO/actions/runs/$run" --jq '.conclusion' 2>/dev/null)
echo "  run $run: ${conclusion:-unknown} (watch rc=$watch_rc)"

if [ "$conclusion" != "success" ]; then
  echo "  NOT LANDED: the branch is left at $branch for inspection" >&2
  exit 1
fi

out=$(git push origin master 2>&1); push_rc=$?
echo "$out" | grep -E '\->|GH006|required status' | sed 's/^/  /'

# DELETE THE BRANCH ONLY ON SUCCESS.
# An unconditional delete on the failure path removed the run's own branch and
# made `gh run list --branch` return nothing -- the evidence for the failure was
# destroyed by the cleanup for it.
if [ $push_rc -eq 0 ]; then
  git push -q --delete origin "$branch" 2>/dev/null
  git fetch -q origin master
  echo "  landed: $(git rev-parse --short=8 origin/master)"
else
  echo "  NOT LANDED: the branch is left at $branch for inspection" >&2
fi
exit $push_rc
