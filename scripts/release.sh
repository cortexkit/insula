#!/usr/bin/env bash
# Cut a release only from a clean, landed master; dry runs still test the core floor.
set -euo pipefail

finished=0
scratch=""
tag=""
tag_state="not created"
landed="not landed"
workflow="not started"
index_state="not confirmed"
repo="cortexkit/insula"
index_url="https://cortexkit.io/releases/v1/index.json"

refuse() { printf 'release: REFUSED: %s\n' "$*" >&2; exit 2; }
on_exit() {
    local rc=$?
    trap - EXIT
    if [ "$finished" != 1 ]; then
        printf 'release: REFUSED: release did not complete (exit %s)\n' "$rc" >&2
        printf 'release: stopped (exit %s); master: %s; tag %s: %s; workflow: %s; index: %s\n' \
            "$rc" "$landed" "$tag" "$tag_state" "$workflow" "$index_state" >&2
        if [ "$tag_state" != "not created" ]; then
            # A failed publish can leave assets behind. Report observed state, not
            # an assumption that workflow failure means nothing was published.
            if ! "$python" "$helper" publication "$repo" "$tag" >&2; then
                echo 'release: GitHub release/assets could not be verified (possibly absent); inspect before retrying.' >&2
            fi
            echo 'release: no rollback performed; never move or replace a published tag.' >&2
        else
            echo 'release: no release tag or GitHub release was published by this command.' >&2
        fi
        [ -z "$scratch" ] || rm -rf "$scratch"
        exit 2
    fi
    [ -z "$scratch" ] || rm -rf "$scratch"
    exit 0
}
trap on_exit EXIT
trap 'refuse "interrupted"' INT TERM

cd "$(dirname "$0")/.." || refuse "cannot reach repository root"
helper="$PWD/scripts/lib/release.py"
version="${1:-}"
dry_run=0
if [ "$#" -eq 2 ] && [ "$2" = --dry-run ]; then
    dry_run=1
elif [ "$#" -ne 1 ]; then
    refuse "usage: scripts/release.sh X.Y.Z [--dry-run]"
fi
for tool in git cargo curl gh; do
    command -v "$tool" >/dev/null || refuse "missing command: $tool"
done
python=""
for candidate in python3 python3.14 python3.13 python3.12 python3.11; do
    if "$candidate" -c 'import tomllib' >/dev/null 2>&1; then
        python="$candidate"
        break
    fi
done
[ -n "$python" ] || refuse "Python 3.11+ is required"
"$python" "$helper" check-version "$version" </dev/null || refuse "invalid release version"
tag="v$version"
[ "$(git symbolic-ref --quiet --short HEAD)" = master ] || refuse "checkout must be on master"
status=$(git status --porcelain --untracked-files=all) || refuse "cannot inspect checkout status"
[ -z "$status" ] || refuse "checkout is dirty; commit the release notes first"
git fetch --prune origin '+refs/heads/master:refs/remotes/origin/master' --tags || refuse "fetch of origin/master and tags failed"
start_sha=$(git rev-parse HEAD)
[ "$start_sha" = "$(git rev-parse refs/remotes/origin/master)" ] || refuse "master is not equal to freshly fetched origin/master"
tags=$(git tag --list 'v*')
printf '%s\n' "$tags" | "$python" "$helper" check-version "$version" || refuse "version/tag check failed"
notes=".cortexkit/release-notes/$tag.md"
[ -s "$notes" ] || refuse "missing or empty $notes"
floor=$("$python" "$helper" floor release-manifest.json) || refuse "invalid release-manifest.json requires_core"
"$python" "$helper" check-crates "$version" || refuse "crate version bump is not valid"
[ "$(git config user.email)" = '94532+ualtinok@users.noreply.github.com' ] || refuse "git user.email is not the release identity"
gh auth status || refuse "GitHub CLI is not authenticated"

case "$(uname -s)" in
    Darwin) os=darwin ;;
    Linux) os=linux ;;
    MINGW*|MSYS*|CYGWIN*) os=windows ;;
    *) refuse "unsupported host operating system" ;;
esac
case "$(uname -m)" in
    arm64|aarch64) arch=arm64 ;;
    x86_64|amd64) arch=x64 ;;
    *) refuse "unsupported host architecture" ;;
esac
mkdir -p target/scratch
scratch=$(mktemp -d "$PWD/target/scratch/release.XXXXXX")
asset="ck-subc-$os-$arch.zip"
# The index contains only the latest core. Resolve the declared floor by its
# immutable release tag instead, even when a newer core is already indexed.
core_url="https://github.com/cortexkit/subconscious/releases/download/subc-core-v$floor/$asset"
curl --fail --location --silent --show-error --max-time 120 "$core_url" -o "$scratch/$asset" || refuse "cannot download public core $floor ($asset)"
curl --fail --location --silent --show-error --max-time 60 "$core_url.sha256" -o "$scratch/$asset.sha256" || refuse "cannot download core checksum"
core_bin=$("$python" "$helper" unpack-core "$scratch/$asset" "$scratch") || refuse "public core checksum/executable verification failed"

if [ "$dry_run" = 0 ]; then
    cp Cargo.lock "$scratch/Cargo.lock.before"
    "$python" "$helper" bump "$version" || refuse "crate version bump failed"
    cargo update --workspace --offline || refuse "offline workspace lock refresh failed"
    "$python" "$helper" check-lock "$scratch/Cargo.lock.before" Cargo.lock "$version" || refuse "lock refresh was not workspace-only"
fi
cp Cargo.lock "$scratch/Cargo.lock.tested"
cp crates/quota-core/Cargo.toml "$scratch/quota-core.tested"
cp crates/quota-module/Cargo.toml "$scratch/quota-module.tested"

echo "release: testing public core $floor from subc-core-v$floor ($asset)"
CK_SUBC_BIN="$core_bin" cargo test -p quota-module --test it real_daemon_e2e -- --ignored || refuse "real_daemon_e2e failed against public core $floor"
[ "$(git rev-parse HEAD)" = "$start_sha" ] || refuse "HEAD changed during the floor test"
if ! { cmp -s Cargo.lock "$scratch/Cargo.lock.tested" && \
    cmp -s crates/quota-core/Cargo.toml "$scratch/quota-core.tested" && \
    cmp -s crates/quota-module/Cargo.toml "$scratch/quota-module.tested"; }; then
    refuse "manifests or lock changed during the floor test"
fi
if [ "$dry_run" = 1 ]; then
    [ -z "$(git status --porcelain --untracked-files=all)" ] || refuse "floor test dirtied the checkout"
    echo "release: dry run passed for $tag; no versions, commits, branches, tags or releases changed"
    finished=1
    exit 0
fi

# Commit only the intended bump. Foreign edits must not hitch a ride on a release.
foreign=$(git status --porcelain --untracked-files=all -- . ':!Cargo.lock' ':!crates/quota-core/Cargo.toml' ':!crates/quota-module/Cargo.toml')
[ -z "$foreign" ] || refuse "foreign changes appeared during release preparation: $foreign"
git diff --check || refuse "version bump has whitespace errors"
git add -- crates/quota-core/Cargo.toml crates/quota-module/Cargo.toml Cargo.lock || refuse "cannot stage version bump"
if ! git commit --only -F - -- crates/quota-core/Cargo.toml crates/quota-module/Cargo.toml Cargo.lock <<MSG
release: $tag
MSG
then
    refuse "cannot commit version bump"
fi
release_sha=$(git rev-parse HEAD)
scripts/train.sh "release-$version" || refuse "release train failed; bump commit $release_sha remains local or on its train branch"
git fetch origin '+refs/heads/master:refs/remotes/origin/master' || refuse "cannot verify landed master"
[ "$(git rev-parse origin/master)" = "$release_sha" ] || refuse "origin/master is not the release commit $release_sha"
landed="$release_sha"
[ "$(git rev-parse HEAD)" = "$release_sha" ] && [ -z "$(git status --porcelain)" ] || refuse "checkout changed while train was running"
git tag -a "$tag" "$release_sha" -m "insula $version" || refuse "cannot create annotated tag $tag"
tag_state="local only; not pushed"
tag_state="push attempted; remote state unconfirmed"
git push origin "refs/tags/$tag" || refuse "tag push failed; remote state may be ambiguous"
tag_state="pushed to origin at $release_sha"

run=""
for attempt in 1 2 3 4 5 6; do
    echo "release: locating $tag workflow (attempt $attempt/6)"
    runs=$(gh run list -R "$repo" --workflow release.yml --commit "$release_sha" --event push \
        --limit 100 --json databaseId,headSha,headBranch,event --jq \
        "[.[] | select(.headSha == \"$release_sha\" and .headBranch == \"$tag\" and .event == \"push\")] | sort_by(.databaseId) | last | .databaseId") || refuse "cannot list release workflow runs after tag push"
    case "$runs" in
        ''|null|*[!0-9]*) ;;
        *) run="$runs"; break ;;
    esac
    sleep 10
done
[ -n "$run" ] || refuse "no release.yml push run appeared for $tag at $release_sha after 60 seconds"
workflow="run $run in progress"
# `gh run watch` can return early while the run is still going (a dropped or
# rate-limited request ends it with rc 1). Watch again until GitHub reports the
# run completed, so a slow macOS leg isn't read as a failed release.
conclusion=""
watch_deadline=$((SECONDS + 5400))
while [ "$SECONDS" -lt "$watch_deadline" ]; do
    gh run watch "$run" -R "$repo" --exit-status --interval 30 >/dev/null 2>&1 || true
    conclusion=$(gh run view "$run" -R "$repo" --json status,conclusion --jq '.status + "/" + .conclusion' 2>/dev/null) || conclusion=""
    case "$conclusion" in
        completed/*) break ;;
    esac
    sleep 30
done
[ -n "$conclusion" ] || refuse "cannot read release workflow conclusion"
workflow="run $run $conclusion"
[ "$conclusion" = completed/success ] || refuse "release workflow did not succeed ($workflow); tag is already pushed"

deadline=$((SECONDS + 900))
while :; do
    remaining=$((deadline - SECONDS))
    [ "$remaining" -gt 0 ] || refuse "GitHub workflow succeeded, but index has not confirmed insula $version requires_core $floor within 15 minutes"
    request_timeout=20
    [ "$remaining" -ge 20 ] || request_timeout="$remaining"
    if curl --fail --location --silent --show-error --max-time "$request_timeout" --header 'Cache-Control: no-cache' \
        "$index_url" -o "$scratch/index.json" && \
        "$python" "$helper" index "$scratch/index.json" "$version" "$floor"; then
        index_state="insula $version requires_core $floor confirmed"
        break
    fi
    remaining=$((deadline - SECONDS))
    pause=30
    [ "$remaining" -ge 30 ] || pause="$remaining"
    [ "$pause" -le 0 ] || sleep "$pause"
done
echo "release: shipped $tag ($release_sha); $index_state"
finished=1
