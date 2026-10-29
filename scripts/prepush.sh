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

# Internal entry point for the snapshot sent by ck motor. Each native runner
# updates stable just as dtolnay/rust-toolchain@stable does in GitHub Actions.
worker() {
    local platform="$1" e2e="$2" rig
    require cargo
    require rustup
    rustup toolchain install stable --profile minimal --component clippy --component rustfmt \
        || cannot_check "$platform stable toolchain setup"
    export RUSTUP_TOOLCHAIN=stable CARGO_TERM_COLOR=always
    gate "$platform toolchain" cargo --version
    gate "$platform format" cargo fmt --all -- --check
    gate "$platform clippy (committed lock)" cargo clippy --locked --workspace --all-targets -- -D warnings
    gate "$platform tests" cargo test --workspace --all-targets
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
        [ "$#" -eq 2 ] || cannot_check "invalid worker arguments"
        case "$2" in 0|1) ;; *) cannot_check "invalid worker e2e flag" ;; esac
        platform="${1#--worker-}"
        [ "$platform" = linux ] || [ "$2" -eq 0 ] || cannot_check "e2e is Linux-only"
        worker "$platform" "$2"
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
trap 'rm -rf "$scratch"' EXIT
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

remote() {
    local platform="$1" start=$SECONDS rc
    local args=(--platform "$platform" --net --timeout 1200)
    if [ "$platform" = linux ]; then
        args+=(--vcpus 16)
        [ "$E2E" -eq 0 ] || args+=(--sibling "$SIBLING")
    fi
    echo "  starting $platform CI leg"
    ck motor run "${args[@]}" -- bash scripts/prepush.sh "--worker-$platform" "$([ "$platform" = linux ] && echo "$E2E" || echo 0)"
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
