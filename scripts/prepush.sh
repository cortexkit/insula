#!/usr/bin/env bash
# One push gate: cheap local checks, then the CI matrix on native build servers.
set -u -o pipefail

script_dir="${BASH_SOURCE[0]%/*}"
[ "$script_dir" != "${BASH_SOURCE[0]}" ] || script_dir=.
cd "$script_dir/.." || exit 2
ROOT="$PWD"

usage() {
    echo "usage: scripts/prepush.sh [--fast] [--linux-only] [--e2e --sibling ../subconscious]"
}
cannot_check() { echo "  COULD NOT CHECK: $*" >&2; exit 2; }
require() { command -v "$1" >/dev/null 2>&1 || cannot_check "missing $1"; }
gate() {
    local label="$1" rc
    shift
    printf '\n  == %s\n' "$label"
    "$@"
    rc=$?
    case "$rc" in
        0) return 0 ;;
        2|126|127|200|201|202|203|204) cannot_check "$label (exit $rc)" ;;
        *) echo "  GATE FAILED: $label (exit $rc)" >&2; exit 1 ;;
    esac
}

workspace_tests() {
    local platform="$1" index_dir path
    local docs=()
    if [ -e .git ]; then
        gate "$platform tests" cargo test --workspace --all-targets
        return
    fi

    # Some motor snapshots omit .git. The documentation resolver deliberately
    # requires tracked targets, so give only the test command a real Git index
    # populated from the caller's tracked Markdown, never from untracked files.
    # Keep .git out of the workspace so build provenance still reports no_git_dir.
    for tool in git mktemp mkdir env; do require "$tool"; done
    [ -n "${DOC_MANIFEST:-}" ] && [ -s "$DOC_MANIFEST" ] \
        || cannot_check "$platform tracked Markdown manifest missing"
    mkdir -p target/scratch || cannot_check "$platform index scratch directory"
    index_dir="$(mktemp -d "$ROOT/target/scratch/prepush-index.XXXXXX")" \
        || cannot_check "$platform index scratch directory"
    while IFS= read -r -d '' path; do
        [ -f "$path" ] && docs+=("$path")
    done <"$DOC_MANIFEST"
    [ "${#docs[@]}" -gt 0 ] || cannot_check "$platform no tracked Markdown present in snapshot"
    git init --bare --quiet "$index_dir/git" || cannot_check "$platform temporary Git repository"
    git --git-dir="$index_dir/git" --work-tree="$ROOT" --literal-pathspecs \
        add --force -- "${docs[@]}" || cannot_check "$platform tracked Markdown index"
    printf '  %s tracked Markdown index: %s files\n' "$platform" "${#docs[@]}"
    gate "$platform tests" env GIT_DIR="$index_dir/git" GIT_WORK_TREE="$ROOT" \
        GIT_INDEX_FILE="$index_dir/git/index" cargo test --workspace --all-targets
}

# Internal entry point for the snapshot sent by ck motor. Build-server toolchains
# are read-only, so record their versions rather than trying to update rustup.
worker() {
    local platform="$1" e2e="$2" manifest="$3" rig
    for tool in mkdir mktemp mv; do require "$tool"; done
    [ -f "$manifest" ] || cannot_check "$platform tracked Markdown manifest missing"
    mkdir -p target/scratch || cannot_check "$platform manifest scratch directory"
    DOC_MANIFEST="$(mktemp "$ROOT/target/scratch/prepush-docs.XXXXXX")" \
        || cannot_check "$platform manifest scratch file"
    # The untracked file must travel in the snapshot, but must not make module
    # provenance dirty. Move it under ignored scratch before any Cargo command.
    mv -- "$manifest" "$DOC_MANIFEST" || cannot_check "$platform tracked Markdown manifest staging"
    require cargo
    require rustc
    unset RUSTUP_TOOLCHAIN
    export CARGO_TERM_COLOR=always
    gate "$platform rustc version" rustc --version
    gate "$platform cargo version" cargo --version
    gate "$platform format" cargo fmt --all -- --check
    gate "$platform clippy (committed lock)" cargo clippy --locked --workspace --all-targets -- -D warnings
    workspace_tests "$platform"
    gate "$platform doctests" cargo test --workspace --doc
    if [ "$e2e" -eq 1 ]; then
        # Keep toolchain paths before hiding HOME, exactly as CI's daemon step
        # does. The sibling builds its own ck-subc; no installed daemon is used.
        export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
        export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
        mkdir -p target/scratch || cannot_check "daemon scratch directory"
        rig="$(mktemp -d "$ROOT/target/scratch/prepush-daemon.XXXXXX")" \
            || cannot_check "daemon scratch directory"
        gate "$platform real daemon supervision" env HOME="$rig" XDG_CONFIG_HOME="$rig/config" \
            CK_QUOTA_STATE_DIR="$rig/state" \
            cargo test -p quota-module --test it -- --ignored real_daemon_e2e::
    fi
    gate "$platform completeness envelopes" cargo run -p quota-core --example completeness-envelopes
}

case "${1:-}" in
    --worker-linux|--worker-windows)
        [ "$#" -eq 3 ] || cannot_check "invalid worker arguments"
        case "$2" in 0|1) ;; *) cannot_check "invalid worker e2e flag" ;; esac
        platform="${1#--worker-}"
        [ "$platform" = linux ] || [ "$2" -eq 0 ] || cannot_check "e2e is Linux-only"
        worker "$platform" "$2" "$3"
        exit 0 ;;
esac

FAST=0
LINUX_ONLY=0
E2E=0
SIBLING=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --fast) FAST=1 ;;
        --linux-only) LINUX_ONLY=1 ;;
        --e2e) E2E=1 ;;
        --sibling)
            [ "$#" -ge 2 ] || cannot_check "--sibling needs a path"
            SIBLING="$2"
            shift ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; cannot_check "unknown argument: $1" ;;
    esac
    shift
done
[ "$FAST" -eq 0 ] || [ "$E2E" -eq 0 ] || cannot_check "--fast cannot run --e2e"
if [ "$E2E" -eq 1 ]; then
    [ -n "$SIBLING" ] || cannot_check "--e2e requires --sibling ../subconscious"
    SIBLING="$(cd "$SIBLING" && pwd)" || cannot_check "cannot read sibling"
    [ "${SIBLING##*/}" = subconscious ] && [ -f "$SIBLING/Cargo.toml" ] \
        || cannot_check "--sibling must be a subconscious checkout named subconscious"
elif [ -n "$SIBLING" ]; then
    cannot_check "--sibling requires --e2e"
fi

# Missing infrastructure is not a green fast check or a red code gate.
[ "$FAST" -eq 1 ] || require ck
for tool in cargo rustup python3 bash git shasum cut find sort wc tr basename mktemp mkdir rm grep sed; do
    require "$tool"
done
export RUSTUP_TOOLCHAIN=stable PYTHONDONTWRITEBYTECODE=1
rustup run stable rustfmt --version || cannot_check "install stable with rustfmt for phase 1"
mkdir -p "$ROOT/target/scratch" || cannot_check "local scratch directory"
scratch="$(mktemp -d "$ROOT/target/scratch/prepush.XXXXXX")" || cannot_check "local scratch directory"
manifest=""
trap 'rm -rf "$scratch"; [ -z "$manifest" ] || rm -f -- "$manifest"' EXIT
export TMPDIR="$scratch"

metadata() { cargo metadata --locked --format-version 1 >"$scratch/metadata.json"; }
phase_start=$SECONDS
gate "phase 1: format" cargo fmt --all -- --check
gate "phase 1: committed lock (metadata, no compilation)" metadata
gate "phase 1: repository checks" bash scripts/gates.sh --repo-only
for sums in test-vectors/*/SHA256SUMS; do
    [ -f "$sums" ] || cannot_check "no test-vector SHA256SUMS found"
    gate "phase 1: $sums" bash -c 'cd "$1" && shasum -a 256 -c SHA256SUMS' _ "${sums%/*}"
done
printf '\n  phase 1 passed in %ss\n' "$((SECONDS - phase_start))"
if [ "$FAST" -eq 1 ]; then
    echo "  --fast: remote matrix not requested (not a full prepush pass)"
    exit 0
fi

# Ignored paths are not transported by every runner. This unique root file is
# intentionally untracked and non-ignored until both snapshot jobs have finished.
manifest="$(mktemp .prepush-tracked-docs.XXXXXX)" || cannot_check "tracked Markdown manifest file"
git ls-files -z -- '*.md' >"$manifest" || cannot_check "capture tracked Markdown for snapshot tests"
[ -s "$manifest" ] || cannot_check "no tracked Markdown to send to snapshot tests"

remote() {
    local platform="$1" start=$SECONDS rc
    local args=(--platform "$platform" --timeout 1200)
    if [ "$platform" = linux ]; then
        args+=(--vcpus 16 --net)
        [ "$E2E" -eq 0 ] || args+=(--sibling "$SIBLING")
    fi
    echo "  starting $platform CI leg"
    ck motor run "${args[@]}" -- bash scripts/prepush.sh "--worker-$platform" "$([ "$platform" = linux ] && echo "$E2E" || echo 0)" "$manifest"
    rc=$?
    printf '\n  %s job finished in %ss (exit %s)\n' "$platform" "$((SECONDS - start))" "$rc"
    case "$rc" in
        0) return 0 ;;
        2|126|127|200|201|202|203|204) echo "  COULD NOT CHECK: $platform CI leg (exit $rc)" >&2; return 2 ;;
        *) echo "  GATE FAILED: $platform CI leg (exit $rc)" >&2; return 1 ;;
    esac
}

remote linux &
linux_pid=$!
windows_pid=""
if [ "$LINUX_ONLY" -eq 0 ]; then
    remote windows &
    windows_pid=$!
fi
# Do not cancel the other platform on failure: CI uses fail-fast: false too.
wait "$linux_pid"; linux_rc=$?
windows_rc=0
if [ -n "$windows_pid" ]; then
    wait "$windows_pid"; windows_rc=$?
fi
if [ "$linux_rc" -eq 2 ] || [ "$windows_rc" -eq 2 ]; then exit 2; fi
if [ "$linux_rc" -ne 0 ] || [ "$windows_rc" -ne 0 ]; then exit 1; fi
if [ "$LINUX_ONLY" -eq 1 ]; then
    echo "  Linux-only gate passed; Windows was not requested"
else
    echo "  prepush passed: phase 1 and both CI legs"
fi
[ "$E2E" -eq 1 ] || echo "  real_daemon_e2e not requested; use --e2e --sibling ../subconscious for that CI step"
