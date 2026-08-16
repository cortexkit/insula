#!/usr/bin/env bash
#
# Run this workspace's gates in the order that makes them mean something.
#
# WHY THIS EXISTS RATHER THAN A LIST IN A DOC. docs/verifying.md has always said
# the freshness check comes first. On 2026-08-16 a sibling repo made three
# breaking protocol changes, and two of them reached master because the gates
# were run in the wrong order -- build first, measure after -- by which point
# cargo had already absorbed the change and the check could only say "not
# stale". The check was correct all three times. The ORDERING was the defect,
# and an ordering that depends on remembering is not an ordering.
#
# The other half is that a stale sibling now triggers the forced recompile HERE
# instead of printing advice for a human to follow. Advice at the moment of
# least suspicion is what failed twice.
#
# Usage:
#   scripts/gates.sh          fmt, clippy, unit tests
#   scripts/gates.sh --e2e    also the integration suites (adds ~80s)
#
# Exit codes match every other checker here: 0 clean, 1 a gate failed, 2 could
# not check.

set -u -o pipefail

cd "$(dirname "$0")/.." || { echo "  cannot reach the workspace root" >&2; exit 2; }

WITH_E2E=0
for arg in "$@"; do
    case "$arg" in
        --e2e) WITH_E2E=1 ;;
        *) echo "  unknown argument: $arg" >&2; exit 2 ;;
    esac
done

step() { printf '\n  == %s\n' "$1"; }
fail() { printf '\n  GATE FAILED: %s\n' "$1" >&2; exit 1; }

step "sibling freshness"
python3 scripts/sibling-freshness.py
freshness=$?
if [ "$freshness" -eq 2 ]; then
    # Could not check. Refusing rather than continuing: every gate after this
    # would carry an unknown, and a green run that could not verify its own
    # premise is the failure this script exists to prevent.
    fail "the freshness check could not run, so no later result can be trusted"
elif [ "$freshness" -eq 1 ]; then
    # A sibling moved after the last build, so cargo may answer from cache.
    # Forcing the recompile rather than advising it: the advice is correct and
    # was skipped twice in one day.
    printf '\n  sibling moved -- forcing a recompile before any gate runs\n'
    # No depth limit. The first version capped at 3 and missed two files --
    # including tests/common/mod.rs, the e2e harness's own wire driver, which is
    # exactly where a protocol change lands. A partial recompile that reports
    # clean is the failure this script exists to stop.
    find crates -name '*.rs' -exec touch {} + || fail "could not touch sources"
fi

step "cargo fmt --check"
cargo fmt --check || fail "formatting"

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings || fail "clippy"

step "cargo test --workspace --lib --bins"
cargo test --workspace --lib --bins || fail "unit tests"

if [ "$WITH_E2E" -eq 1 ]; then
    # The harness SPAWNS this binary; `cargo test --test` does not rebuild it,
    # and a stale one fails registration with no error output at all.
    step "cargo build -p quota-module --bins (the e2e harness spawns it)"
    cargo build -p quota-module --bins || fail "building the module binary"

    step "cargo test -p quota-module --test skeleton_e2e"
    cargo test -p quota-module --test skeleton_e2e || fail "skeleton_e2e"
fi

printf '\n  all gates passed\n'
