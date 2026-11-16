#!/bin/bash
# Measure incremental test rebuild times after a one-line code edit, on a
# build server whose checkout is read-only: copy the source to scratch and
# build there against the job's own warm target directory.
set -uo pipefail
ROOT="$(pwd)"
TARGET="$ROOT/target"
SCRATCH="$TARGET/scratch/rebuild-timing"
rm -rf "$SCRATCH" && mkdir -p "$SCRATCH"
tar --exclude=./target --exclude=./.git -cf - . | (cd "$SCRATCH" && tar -xf -)
cd "$SCRATCH" || exit 2
export CARGO_TARGET_DIR="$TARGET" CARGO_INCREMENTAL=1
nproc 2>/dev/null; rustc --version
t() { local s=$SECONDS; "$@" >/dev/null 2>&1; echo "  rc=$? $((SECONDS - s))s: $*"; }
echo "warm-up (cold build)"
t cargo test --locked --workspace --no-run
t cargo test --locked --workspace --no-run
edit() { printf '\n#[allow(dead_code)]\nfn rebuild_probe_%s() -> u32 { %s }\n' "$2" "$RANDOM" >> "$1"; }
for f in crates/quota-core/src/lib.rs crates/quota-core/src/antigravity.rs crates/quota-core/src/codex.rs crates/quota-module/src/main.rs; do
  echo "edit $f"
  edit "$f" a
  case "$f" in
    crates/quota-core/*)
      t cargo test --locked -p quota-core --lib --no-run
      t cargo test --locked -p quota-module --no-run ;;
    *) t cargo test --locked -p quota-module --no-run ;;
  esac
done
