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
#
# `--stage` builds, signs and checks the same binary but places nothing and
# restarts nothing: it writes a card under the fleet staging directory for the
# daemon owner to place during a combined restart window, so insula restarts once
# with everyone else instead of on its own.
set -euo pipefail

cd "$(dirname "$0")/.."

BIN=ck-insula
DEST="$HOME/.local/share/cortexkit/bin/$BIN"
STAGING="$HOME/.local/share/cortexkit/staging"
ALLOW_DIRTY="${CK_DEPLOY_ALLOW_DIRTY:-0}"
MODE=deploy
case "${1:-}" in
  "") ;;
  --stage) MODE=stage ;;
  *) echo "usage: $0 [--stage]" >&2; exit 2 ;;
esac

# Sign with the hardened runtime, under a fixed identifier, and refuse if either
# did not take.
#
# HARDENED because this process holds secrets in memory: the launch token the
# supervisor hands it at start, and the provider credentials it fetches from
# the vault. macOS lets any process running as the same user attach a debugger
# and read another process's memory unless that binary is signed with the
# hardened runtime. The ad-hoc signature the linker applies does not set it.
#
# THE IDENTIFIER IS NAMED, NOT DEFAULTED, because `codesign` otherwise takes it
# from the FILE NAME, and this signs a scratch copy (`ck-insula.new` or a
# staging name). macOS keys privacy grants such as Full Disk Access on the
# signing identifier, so a binary signed under a scratch name silently stops
# matching a grant the user gave to `ck-insula`; the fleet daemon lost its grant
# exactly this way.
#
# Ad-hoc is enough: the protection is the runtime flag, not a team identity.
# insula needs no entitlements -- it links only system libraries, uses no JIT,
# and reaches the Keychain through the Apple-signed `security` tool.
harden() {
  local file="$1" info
  codesign --force --sign - --options runtime --identifier "$BIN" "$file"
  info="$(codesign -dvv "$file" 2>&1)"
  if ! grep -q "^Identifier=$BIN\$" <<<"$info"; then
    echo "REFUSING: signed identifier is not $BIN:" >&2
    grep '^Identifier=' <<<"$info" | sed 's/^/  /' >&2
    exit 1
  fi
  if ! grep -qE '^CodeDirectory .*flags=0x[0-9a-f]+\([^)]*runtime' <<<"$info"; then
    echo "REFUSING: the hardened runtime flag did not take:" >&2
    grep '^CodeDirectory' <<<"$info" | sed 's/^/  /' >&2
    exit 1
  fi
  codesign --verify --strict "$file"
}

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

echo "  ${MODE}ing $head_sha (branch $branch)"
cargo build --release -q

if [ "$MODE" = "stage" ]; then
  full_sha="$(git rev-parse HEAD)"
  card="$STAGING/$BIN.${full_sha:0:8}"
  mkdir -p "$STAGING"
  cp "target/release/$BIN" "$card.new"
  harden "$card.new"
  if ! "$card.new" --version >/dev/null 2>&1; then
    echo "REFUSING: the signed binary will not execute." >&2
    rm -f "$card.new"
    exit 1
  fi
  mv "$card.new" "$card"
  (cd "$STAGING" && shasum -a 256 "$(basename "$card")" >"$(basename "$card").sha256")
  {
    echo "stage=$card"
    echo "revision=$full_sha"
    echo "declared_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } >"$STAGING/$BIN.current.new"
  mv "$STAGING/$BIN.current.new" "$STAGING/$BIN.current"
  echo "  staged: $card"
  echo "  sha256: $(cut -d' ' -f1 "$card.sha256")"
  codesign -dvv "$card" 2>&1 | grep -E '^(Identifier|CodeDirectory)' | sed 's/^/  /'
  echo "  card:   $STAGING/$BIN.current (nothing placed, nothing restarted)"
  exit 0
fi

# Copy to scratch and mv, never cp over the destination: macOS caches the code
# signature per VNODE, so writing new bytes into the existing inode gets the
# process SIGKILLed on exec while the supervisor keeps the old image running.
cp "target/release/$BIN" "$DEST.new"
harden "$DEST.new"
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
