# Verifying a change

The gate procedure for this workspace, in one place so a task prompt can point
at it instead of restating it. **Restating it is how it drifts**: three prompts
written on 2026-08-16 instructed a mutation-restore method this repo had already
documented as unsafe, twice, and a worker followed them correctly and lost its
own implementation. A procedure copied into a prompt ages separately from the
repo that knows better.

## The gates

```bash
python3 scripts/sibling-freshness.py                              # see below
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings             # must be 0
cargo test --workspace --lib --bins
```

### Why the freshness check comes first

This workspace path-depends on `../subconscious`. That repo can change while
nothing in this tree does, and then cargo has nothing to redo and reports a
cached `0` — which is not the same as nothing being wrong. That put a compile
break on master on 2026-08-16, found by CI, which always builds cold.

`scripts/sibling-freshness.py` exits 1 when a sibling's HEAD is newer than this
workspace's newest build artifact, and prints the forced-recompile command. It
does not claim the sibling change breaks anything — the answer is "your cache may
predate a change".

### Integration tests need their binaries built first

`cargo test -p quota-module --test skeleton_e2e` does **not** rebuild the
`ck-insula` binary the harness spawns. A stale binary fails registration with no
error output at all, which reads as a hang rather than a build problem:

```bash
cargo build -p quota-module --bins
cargo test -p quota-module --test skeleton_e2e
```

That suite is slow by nature — around 80 seconds, one test alone taking 60 — and
the 10-second registration gate inside it is tight on a machine that has been
compiling for a while. A failure there is worth re-running alone before treating
it as real, and worth NOT "fixing" by widening the gate: CI passes it as-is, and
widening a gate to accommodate a busy dev machine is masking.

## Mutation proofs

Use `scripts/probe.py`. It stages first, restores in a `finally` and from signal
handlers, and classifies the outcome as reddened / undefended / NOT REACHED /
HUNG.

```bash
python3 scripts/probe.py <file> '<original>' '<mutated>' -- -p quota-core --lib
```

**Do not hand-roll the restore with `git checkout --`.** It restores from the
index, so with uncommitted work in the tree it reverts the whole file rather than
the mutation — and a proof run against a hand-rebuilt copy of the code under test
proves nothing about the code that ships.

Read WHICH test reddened, not merely that something did. A mutation that reddens
an unrelated test says the mutation was wrong, not that the guard is defended;
one that reddens nothing may mean the mutation missed rather than that the guard
is unguarded. Both cases are findings about the proof, not about the code.

## Checkers, after deploying

Both read the deployed module through the daemon, so they must run **after** the
deploy rather than before:

```bash
cargo run -p quota-module --example deployed-sanity --release
cargo run -p quota-module --example vault-lanes --release
```

Exit codes across every checker and script here: `0` checked and found nothing,
`1` checked and found something, `2` could not check — the run makes no claim
either way. The third is the one that matters, because a `2` read as a `0` is a
clean bill of health from an instrument that never looked.
