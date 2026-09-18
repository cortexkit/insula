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

# THE LAST MOMENT A DAMAGED COMMIT MESSAGE IS STILL FREE TO FIX.
#
# `git commit -m "...`ident`..."` runs each backticked word as command
# substitution and replaces it with empty output, so the message lands with holes
# where the identifiers were. Silent: git exits 0, the push succeeds, CI is
# green. Once it reaches a protected branch there is no amend -- force-push is
# refused and the record is permanent. Here the commit exists and the push has
# not happened, which is the only point where `git commit --amend` still costs
# nothing.
#
# The discriminator is GAP WIDTH, not gap count. A substitution leaves EXACTLY
# the two spaces that surrounded the eaten word; a deliberately aligned table
# pads to a width and uses three or more. Measured across 946 commits: this
# separates the one real instance from seven aligned tables, and a gap-count
# rule missed the two-identifier case entirely while returning the same total.
if git log -1 --format=%B | grep -qE '[a-z,)]  [a-z(]' &&
   ! git log -1 --format=%B | grep -qE '   +'; then
  echo "  REFUSED: the commit message looks like it lost a backticked identifier" >&2
  git log -1 --format=%B | grep -nE '[a-z,)]  [a-z(]' | sed 's/^/    /' >&2
  echo "  Rewrite it with a QUOTED heredoc, which is still free before the push:" >&2
  echo "      git commit --amend -F - <<'MSG'" >&2
  echo "  The quotes on the delimiter are the point: <<'MSG' does not expand." >&2
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

# EVERY REQUIRED CHECK MUST BE A NAME THIS SHA ACTUALLY PRODUCED.
#
# Branch protection matches required checks BY NAME. A matrix leg renamed in the
# workflow produces a new name, the old one is never reported again, and the rule
# keeps waiting for it -- so main becomes unpushable while CI is green. The push
# fails with "Expected -- Waiting for status to be reported", which reads like CI
# being slow rather than like a name nobody will ever send.
#
# Asked HERE because both facts are known and neither is guessed: the run has
# finished, so its check names are observed rather than parsed out of YAML with
# matrix expansion, and the required set is read from the API rather than assumed.
#
# One direction only. Every required name must have been produced; a workflow may
# produce extra jobs that are not required, which is ordinary.
required=$(gh api "repos/$REPO/branches/$(git rev-parse --abbrev-ref origin/HEAD | sed 's|^origin/||')/protection" \
           --jq '.required_status_checks.contexts[]?' 2>/dev/null)
if [ -z "$required" ]; then
  # Unprotected, or this token cannot read protection. Both are fine and neither
  # is a finding -- but say which question went unanswered rather than printing
  # nothing, so a silent skip is not mistaken for a clean check.
  echo "  (no required checks readable for the default branch -- protection off, or the token lacks admin)"
else
  produced=$(gh api "repos/$REPO/commits/$sha/check-runs?per_page=100" \
             --jq '.check_runs[].name' 2>/dev/null | sort -u)
  missing=""
  while IFS= read -r name; do
    [ -z "$name" ] && continue
    printf '%s\n' "$produced" | grep -qxF "$name" || missing="$missing$name\n"
  done <<< "$required"
  if [ -n "$missing" ]; then
    echo "  REFUSED: a required check was never produced by this sha" >&2
    printf "$missing" | sed 's/^/    missing: /' >&2
    echo "  Produced names:" >&2
    printf '%s\n' "$produced" | sed 's/^/      /' >&2
    echo "  A renamed job is the usual cause. Fix the workflow name or the" >&2
    echo "  protection rule BEFORE pushing -- the push would otherwise hang on a" >&2
    echo "  status nobody will ever report." >&2
    echo "  NOT LANDED: the branch is left at $branch for inspection" >&2
    exit 1
  fi
  echo "  required checks produced: $(printf '%s\n' "$required" | grep -c .) of $(printf '%s\n' "$required" | grep -c .)"
fi

    before_push=$(git rev-parse origin/master)
    out=$(git push origin master 2>&1); push_rc=$?
echo "$out" | grep -E '\->|GH006|required status' | sed 's/^/  /'

# DELETE THE BRANCH ONLY ON SUCCESS.
# An unconditional delete on the failure path removed the run's own branch and
# made `gh run list --branch` return nothing -- the evidence for the failure was
# destroyed by the cleanup for it.
    if [ $push_rc -eq 0 ]; then
      git push -q --delete origin "$branch" 2>/dev/null
      git fetch -q origin master
      # NAME EVERY COMMIT THAT LANDED, NOT JUST THE TIP.
      #
      # This printed only `origin/master` after the push, which is a TRUE sha
      # answering a question nobody asked. A train usually carries more than one
      # commit -- a change plus a lock absorb is the common shape here -- and the
      # tip is whichever went last, not the one the run was about.
      #
      # I quoted that tip to a peer as the sha carrying a wire pin. It was the lock
      # absorb. They went looking for the pin, in the wrong repository, and found
      # nothing -- which reads identically to an unpushed commit, so the next
      # question was whether I had failed to push at all.
      #
      # A confirmation that prints a correct value for a different subject is worse
      # than one that prints nothing, because it is quoted onward with confidence.
      echo "  landed on origin/master:"
      git --no-pager log --oneline "$before_push"..origin/master | sed 's/^/    /'
else
  echo "  NOT LANDED: the branch is left at $branch for inspection" >&2
fi
exit $push_rc
