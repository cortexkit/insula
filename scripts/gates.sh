#!/usr/bin/env bash
#
# Run this workspace's checks together so local verification covers the same
# source, tooling and test targets every time.
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

step "repository path dependencies"
# Python 3.11's standard TOML parser also sees unused Cargo patches, which
# resolved metadata omits. Select an installed parser rather than fetching one.
TOML_PYTHON=""
for candidate in python3 python3.14 python3.13 python3.12 python3.11; do
    if "$candidate" -c 'import tomllib' >/dev/null 2>&1; then
        TOML_PYTHON="$candidate"
        break
    fi
done
[ -n "$TOML_PYTHON" ] || fail "path dependency check requires Python 3.11 or newer"
"$TOML_PYTHON" --version
"$TOML_PYTHON" scripts/path-dependencies.py || fail "repository path dependencies"

# Cargo commands without --locked can rewrite Cargo.lock after a local manifest
# edit. Announce any write so it is reviewed as a dependency change rather than
# swept into an unrelated commit. Deliberate upgrades use cargo update -p <crate>
# before the gates.
LOCK_BEFORE="$(shasum -a 256 Cargo.lock 2>/dev/null | cut -d' ' -f1)"

announce_lock_write() {
    local after
    after="$(shasum -a 256 Cargo.lock 2>/dev/null | cut -d' ' -f1)"
    [ "$after" = "$LOCK_BEFORE" ] && return 0
    printf '\n  NOTE: the gates rewrote Cargo.lock.\n' >&2
    printf '  Review and commit it as a deliberate dependency change, or restore it;\n' >&2
    printf '  do NOT let it ride along in a commit about something else:\n' >&2
    git --no-pager diff --stat -- Cargo.lock >&2
}
trap announce_lock_write EXIT

step "python instruments compile"
# Compile every Python instrument so diagnostic tools do not first reveal a
# syntax error when someone needs them during an incident. Sub-second and fails
# closed when no instruments are found.
#
# Compilation only: this checks syntax, not runtime behavior or imports.
python3 - <<'PYGATE' || fail "a python instrument does not compile"
import pathlib, py_compile, sys, tempfile
broken = []
scripts = sorted(pathlib.Path("scripts").glob("*.py"))
if not scripts:
    print("  no python instruments found -- refusing rather than passing")
    sys.exit(1)
for script in scripts:
    try:
        py_compile.compile(str(script), cfile=tempfile.mktemp(), doraise=True)
    except py_compile.PyCompileError as error:
        broken.append(f"{script.name}: {str(error).splitlines()[0][:70]}")
for entry in broken:
    print(f"  {entry}")
print(f"  {len(scripts) - len(broken)}/{len(scripts)} compile")
sys.exit(1 if broken else 0)
PYGATE

step "shell instruments parse"
# Same question as the python gate one step up, for the other half of the toolbox.
# `bash -n` proves the file will START -- it is the shell equivalent of a compile,
# not a linter, and it catches the failure that actually happens: an incident tool
# edited under pressure and never run, dying on a syntax error at the moment it is
# needed. Added when scripts/train.sh landed, because the release and train
# scripts had no such check at all while every python script did.
broken_shell=0
shell_scripts=$(find scripts -name '*.sh' -type f | sort)
if [ -z "$shell_scripts" ]; then
  fail "no shell instruments found -- refusing rather than passing"
fi
for script in $shell_scripts; do
  if ! bash -n "$script" 2>/dev/null; then
    echo "  $script does not parse"
    broken_shell=$((broken_shell + 1))
  fi
done
echo "  $(echo "$shell_scripts" | wc -l | tr -d ' ') shell instrument(s), $broken_shell broken"
[ "$broken_shell" -eq 0 ] || fail "a shell instrument does not parse"

step "endpoint host manifest"
# RUN, not merely compiled. This checker existed for two weeks reporting a real
# finding that nobody saw, because the gate only py_compile'd it: the openrouter
# provider shipped on 2026-08-16 and its host never entered the manifest. A
# checker that is only syntax-checked finds nothing, and its green compile reads
# like coverage.
#
# Cheap enough to belong here: it parses source for `const *_URL: &str = "https…"`
# and compares against a recorded host per module. No network, no build, sub-second.
# It matches a CONSTRUCT rather than a name, so a comment mentioning a host cannot
# manufacture an entry -- the failure mode that broke a fleet census the same day.
python3 scripts/endpoint-hosts.py || fail "endpoint host manifest is out of date"

step "train preconditions"
# Same reasoning as the endpoint manifest above: RUN rather than trusted. These
# three facts about ci.yml were each verified by hand once and then relied on
# indefinitely, and a narrowed trigger or a newly path-dependent job stays
# invisible until branch protection is enabled -- at which point main becomes
# unpushable and it presents as a protection fault.
#
# Here as well as in CI because a workflow edit should fail before the push that
# carries it, not on the run it breaks.
./scripts/check-train-preconditions.sh || fail "train preconditions drifted"

step "cargo fmt --check"
cargo fmt --check || fail "formatting"

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings || fail "clippy"

step "clippy on the newest stable (what CI runs)"
# CI installs `dtolnay/rust-toolchain@stable`, so it lints with the NEWEST stable
# the day it runs, while this machine lints with whatever stable was installed
# last. Every six weeks a release adds lints, and until someone updates locally the
# gates pass here and fail in CI on code nobody touched. It happened with Rust 1.99
# (2026-09-28): clippy::double_must_use fired on async-trait 0.1.89's generated
# code, ten errors, all gates green on 1.98.
#
# So when a newer stable exists, clippy also runs on it, as a separate toolchain.
# The default stable is left alone: every module on this host builds with it, and
# moving it would make all of them recompile at once.
#
# If rustup cannot reach the network, this step says so and continues. The
# default-toolchain clippy above has already run, and CI checks the newest stable
# anyway; refusing here would make the gates unusable offline for coverage CI
# already provides.
#
# DECIDED BY THE OUTPUT, NOT THE EXIT CODE. `rustup check` exits 100 when an update
# IS available, which is exactly the case this step exists for. The first version
# of this step branched on the exit code, read 100 as "could not check", and
# printed the offline note on the one day it mattered. The stable line is present
# when rustup reached the network and absent when it did not.
newest_check="$(rustup check 2>/dev/null)"
stable_line="$(printf '%s\n' "$newest_check" | grep '^stable-' | head -1)"
if [ -n "$stable_line" ]; then
    newest="$(printf '%s\n' "$stable_line" \
        | sed -n 's/^stable-[^ ]* - update available: .* -> \([0-9][0-9.]*\) .*/\1/p')"
    if [ -z "$newest" ]; then
        echo "  local stable is the newest stable: ${stable_line#*- }"
    else
        echo "  local stable is behind; CI lints with $newest, so clippy runs on it too"
        rustup toolchain install "$newest" --profile minimal -c clippy >/dev/null 2>&1 \
            || fail "could not install Rust $newest to lint with what CI uses"
        cargo "+$newest" clippy --workspace --all-targets -- -D warnings \
            || fail "clippy on Rust $newest (CI's toolchain)"
    fi
else
    echo "  NOTE: rustup could not check for a newer stable (offline?); CI will lint with it"
fi

step "cargo test --workspace --lib --bins"
cargo test --workspace --lib --bins || fail "unit tests"

# DOCTESTS ARE A SEPARATE TARGET AND --lib --bins DOES NOT INCLUDE THEM. Neither
# does --all-targets, which is the trap: it reads as "everything" and silently
# omits exactly this. An indented block inside a /// comment is a Rust code block
# to rustdoc, so an ASCII table in a doc comment is compiled -- and this gate ran
# green over a broken master until a consumer ran plain `cargo test --workspace`
# and reported it (insula#13).
step "cargo test --workspace --doc"
cargo test --workspace --doc || fail "doctests"

if [ "$WITH_E2E" -eq 1 ]; then
    # The harness SPAWNS this binary; `cargo test --test` does not rebuild it,
    # and a stale one fails registration with no error output at all.
    step "cargo build -p quota-module --bins (the e2e harness spawns it)"
    cargo build -p quota-module --bins || fail "building the module binary"

    step "cargo test -p quota-module --test it skeleton_e2e::"
    cargo test -p quota-module --test it skeleton_e2e:: || fail "skeleton_e2e"
fi

# A .rs file that no `mod` declaration reaches is compiled by NOTHING: cargo never
# reads it, so clippy reports no dead code in it, the formatter still formats it,
# and every gate passes over a file that does not exist as far as the crate is
# concerned. That is how 150 lines of a half-finished refactor rode into a commit
# whose message described something else -- present in the tree, absent from the
# build, and invisible to every check that was supposed to notice.
#
# Cheap, and it fails closed: a src/*.rs with no matching `mod` in the crate root
# is either work someone forgot to wire or a leftover that should have been
# deleted. Both want a human, and neither announces itself.
printf '\n  == orphan module check\n'
orphans=0
for crate_src in crates/*/src; do
  root="$crate_src/lib.rs"
  [ -f "$root" ] || root="$crate_src/main.rs"
  [ -f "$root" ] || continue
  for f in "$crate_src"/*.rs; do
    [ -e "$f" ] || continue
    m="$(basename "$f" .rs)"
    case "$m" in lib|main) continue;; esac
    if ! grep -q "mod $m;" "$root"; then
      printf '    ORPHAN: %s is declared by no mod in %s\n' "$f" "$root"
      orphans=$((orphans + 1))
    fi
  done
done
if [ "$orphans" -ne 0 ]; then
  printf '\n  GATE FAILED: %s orphan module(s)\n' "$orphans"
  exit 1
fi
printf '    no orphan modules\n'

printf '\n  all gates passed\n'
