#!/usr/bin/env bash
# Build and place the supervised module, refusing when what is on disk is not
# what is on master.
#
# WHY THIS IS A SCRIPT AND NOT A RUNBOOK LINE. The deploy was a hand-typed
# sequence, and hand-typed sequences are narrated rather than checked. I stashed
# held work, landed an unrelated commit, restored the stash, then built and
# staged a binary while writing "built from HEAD, which excludes the held work".
# False: `cargo build` builds the WORKING TREE. The staged binary contained the
# exact change I had told a peer I was holding for their review.
#
# The sentence was wrong and nothing could contradict it, because nothing was
# looking. That is the whole argument for a guard over a note.
#
# TWO CHECKS, BECAUSE THEY CATCH DIFFERENT DIVERGENCES and one feels like both:
#
#   dirty tree      uncommitted work that the build would include
#   wrong branch    COMMITTED work that a clean tree cannot reveal
#
# The second is not hypothetical either: a peer spent hours running "master
# checks" on a feature branch with a spotless tree, and shipped a CLI built from
# it that had never been through CI. A porcelain check would have passed all day.
#
# Both refuse rather than warn. A warning at build time is read, and reading is
# exactly what failed in my case -- I read my own correct sentence about the
# wrong artefact.
#
# Override with CK_DEPLOY_ALLOW_DIRTY=1, loudly: deploying a work-in-progress to
# watch it run is legitimate, and a guard with no escape gets deleted rather than
# respected.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=ck-insula
DEST="$HOME/.local/share/cortexkit/bin/$BIN"
ALLOW_DIRTY="${CK_DEPLOY_ALLOW_DIRTY:-0}"

branch="$(git rev-parse --abbrev-ref HEAD)"
dirty="$(git status --porcelain)"

if [ "$ALLOW_DIRTY" = "1" ]; then
  echo "  !! CK_DEPLOY_ALLOW_DIRTY=1 -- deploying whatever is in the tree"
  echo "  !! branch: $branch"
  [ -n "$dirty" ] && echo "$dirty" | sed 's/^/  !! uncommitted: /'
else
  if [ -n "$dirty" ]; then
    echo "REFUSING: the working tree is dirty, so the build would not be HEAD." >&2
    echo "$dirty" | sed 's/^/  /' >&2
    echo >&2
    echo "cargo builds the TREE, not HEAD. Commit, stash, or set" >&2
    echo "CK_DEPLOY_ALLOW_DIRTY=1 if you mean to deploy work in progress." >&2
    exit 1
  fi
  if [ "$branch" != "master" ]; then
    echo "REFUSING: on branch '$branch', not master." >&2
    echo "A clean tree cannot reveal committed divergence: every check you run" >&2
    echo "here reads this branch, and the binary would too." >&2
    exit 1
  fi
fi

# Against the REMOTE, not a local ref: the claim a deploy makes is "this is what
# others can fetch", and a local master can be ahead, behind, or rebased.
git fetch -q origin 2>/dev/null || echo "  (could not fetch; the remote comparison below may be stale)"
head_sha="$(git rev-parse --short=12 HEAD)"
origin_sha="$(git rev-parse --short=12 origin/master 2>/dev/null || echo unknown)"
if [ "$ALLOW_DIRTY" != "1" ] && [ "$head_sha" != "$origin_sha" ]; then
  echo "REFUSING: HEAD $head_sha is not origin/master $origin_sha." >&2
  echo "Land the train first, or the running image is one nobody else has." >&2
  exit 1
fi

echo "  deploying $head_sha (branch $branch)"
cargo build --release -q

# Copy to scratch and mv, never cp over the destination: macOS caches the code
# signature per VNODE, so writing new bytes into the existing inode gets the
# process SIGKILLed on exec while the supervisor keeps the old image running.
cp "target/release/$BIN" "$DEST.new"
if ! "$DEST.new" --version >/dev/null 2>&1; then
  echo "REFUSING: the freshly built binary will not execute." >&2
  rm -f "$DEST.new"
  exit 1
fi
mv "$DEST.new" "$DEST"

ck module restart insula >/dev/null 2>&1
echo "  restarted; waiting out the health refresh"
# Past the first refresh: a module answers `unknown` for the first seconds after
# a restart, which is honest and NOT a verdict. Sampling inside that window and
# treating absence of `healthy` as a fault is a misread the vault owner measured
# at ~20s on this host.
sleep 50

stamped="$(python3 scripts/health.py 2>/dev/null | grep -oE 'buildCommit = [0-9a-f]+' | awk '{print $3}')"
echo "  buildCommit: ${stamped:-<unread>}"
if [ "$stamped" != "$head_sha" ]; then
  echo "REFUSING TO CLAIM SUCCESS: running $stamped, expected $head_sha." >&2
  exit 1
fi
echo "  deployed and verified: $head_sha"
