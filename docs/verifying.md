# Verifying a change

The gate procedure for this workspace, in one place so a task prompt can point
at it instead of restating it. **Restating it is how it drifts**: three prompts
written on 2026-08-16 instructed a mutation-restore method this repo had already
documented as unsafe, twice, and a worker followed them correctly and lost its
own implementation. A procedure copied into a prompt ages separately from the
repo that knows better.

## The gates

```bash
scripts/gates.sh          # fmt, clippy, unit tests
scripts/gates.sh --e2e    # also the integration suites, ~80s more
```

That runs the steps below in order and stops at the first failure. **Use it
rather than the individual commands**: the ordering is load-bearing and this
document said so for weeks while two breaks still reached master, because an
ordering that depends on remembering is not an ordering. A stale sibling now
triggers the forced recompile inside the script instead of printing advice at
the moment of least suspicion.

What it runs:

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

## Why CI takes about three minutes, measured

Audited 2026-08-16 across three green runs, because a fleet relay ranked seven
pipeline mechanisms and the first was *measure before mechanism* — another
repo's two intuitive wins had been falsified by its own logs. Mine falsifies the
whole ranked list, which is worth more than adopting it would have been.

The ubuntu job, 177.6s end to end:

| phase | time |
|---|---|
| checkout, toolchain, clippy (33s), compile | 89.3s |
| test execution | 88.3s |
| — of which `i8_vault_stub_two_accounts_fail_closed_without_handle_reap` | **60.4s** |

**One test is a third of the job**, and it is not slow by accident. It kills the
vault stub and waits for the affected slot to fail closed, which happens on that
slot's next refresh — so it is waiting out `BASE_INTERVAL`, a 60-second
production cadence.

Every mechanism on that ranked list (shard the build, share binaries across
lanes, tune cache keys) attacks the 89s compile half. None touches the 60s,
because the 60s is not work — it is elapsed time.

**The obvious lever is a test-only override of `BASE_INTERVAL`, and it is
refused.** `FRESH_HORIZON` must exceed `BASE_INTERVAL + FETCH_DEADLINE`, and a
test running a one-second interval inverts that: the test would then pass under
a timing configuration production never runs, while claiming to verify
fail-closed behaviour that is entirely timing-dependent. A faster suite that
proves something else is not a faster suite.

Also worth the size check before optimising anything here: the relay's source
repo went from 19–26 minutes to 8. This pipeline is already 3–4 minutes, so the
mechanisms are sized for a problem this repo does not have.

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

## What the gate does NOT cover

Written down because `scripts/gates.sh` passing reads as "everything passed", and
on 2026-08-26 it did not: master was red on a doctest while the gate was green,
and the gap was found by a consumer running plain `cargo test --workspace`.

The trap generalises past that one target. **`--all-targets` excludes doctests.**
It reads as the widest possible flag and silently omits one target, which is why
both the local gate and CI missed the same break.

| target | covered by | note |
| --- | --- | --- |
| lib + bin unit tests | gate and CI | |
| doctests | gate and CI | added 2026-08-26; neither had it before |
| `skeleton_e2e` | gate and CI | |
| `real_daemon_e2e` | CI only | `#[ignore]`d, needs a live daemon |
| `*_live` provider tests | NEITHER | `#[ignore]`d by design, hit real providers |
| examples: compile | gate and CI | via `clippy --all-targets` |
| examples: run | CI only | `completeness-envelopes` alone |

The two NEITHER rows are deliberate, not gaps to close: the live provider tests
reach real upstreams and would make the gate depend on someone else's uptime.
Run them by hand with `--ignored` when touching a provider's request shape.

The rows that ARE worth watching are the CI-only ones, because a green local gate
says nothing about them. If a change touches the daemon handshake or an example's
behaviour, the local gate cannot tell you.
