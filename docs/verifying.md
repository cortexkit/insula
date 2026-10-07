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

That runs the checks together and stops at the first failure. Use it rather
than a partial list of individual commands: it also checks Python and shell
instruments, repository path dependencies (using Python 3.11+ for TOML),
endpoint hosts, CI push triggers for `master` and `train/**` branches (which
must both produce checks before a tested train can land), the newest stable clippy,
unit tests, doctests, and orphan modules.

### Updating subc dependencies

`subc-protocol`, `subc-transport`, `subc-daemon`, `subc-jsonc`, and `subc-os`
come from crates.io, pinned by `Cargo.lock`.
A sibling release does not change this workspace's dependency graph. To take
an upgrade, run `cargo update -p <crate>`; for a new minor, bump its requirement
in the root `Cargo.toml` first. Review the lock diff and the runtime versions in
`cargo tree -e normal -p quota-module`, then run:

```bash
scripts/gates.sh
cargo check --locked --workspace --all-targets
cargo test -p quota-module --test it -- --ignored real_daemon_e2e::
```

Land the manifest/lock change only after these pass. The real-daemon test still
needs `../subconscious`: it builds that checkout's `ck-subc` daemon binary to
verify process supervision and routing against the current daemon, not just the registry's dev-only in-process
`subc-daemon`. Ordinary builds and non-ignored tests need no sibling checkout.

### Integration tests need their binaries built first

`cargo test -p quota-module --test it skeleton_e2e::` does **not** rebuild the
`ck-insula` binary the harness spawns. A stale binary fails registration with no
error output at all, which reads as a hang rather than a build problem:

```bash
cargo build -p quota-module --bins
cargo test -p quota-module --test it skeleton_e2e::
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

Use the fleet runner **ckdev-mutate 0.8.0** to prove and replay costly guards.
The checked-in [`mutations.toml`](../mutations.toml) records source edits and
the exact tests that must catch them. Catalogue crash safety, exactly-once,
authorization/trust, wire contracts and data loss, not every ordinary logic test.
An exactly-once claim needs a row through the production provider/scheduler path,
not just its helper. Use `expect_message` to distinguish the intended assertion
from a timeout, fixture failure or unrelated panic.

```bash
cargo install --locked --git https://github.com/cortexkit/commons \
  --rev 46cc166b0df2edcfd14b3eb54ed6eeac588fed69 cortexkit-mutate
mkdir -p target/mutations
ckdev-mutate check
ckdev-mutate run --all --report target/mutations/all.json
ckdev-mutate run --diff origin/master --report target/mutations/diff.json
ckdev-mutate run --all --broad --report target/mutations/broad.json
python3 scripts/mutation-report.py target/mutations/broad.json
```

Create rows from code with `ckdev-mutate prove`, never by transcribing old evidence:

```bash
ckdev-mutate prove --id my-guard --guards 'the costly property protected' \
  --file crates/quota-core/src/example.rs --old 'live unique anchor' \
  --new 'independent break /* NON-VACUITY BREAK */' \
  --test-file crates/quota-core/src/example.rs --package quota-core \
  --target=--lib --expect-red example::tests::the_guard \
  --expect-message 'the intended assertion message' \
  --report target/mutations/proof.json
```

`prove` appends only on a named catch; `explore --append` discovers the guarding
tests when they are unknown. Replay a new row with `--broad` before committing.
Narrow cross-target catches, or record a reviewed HUB with the shared property
and the **other** stable target names. HUB is not a waiver for an unrelated red.
UNREACHABLE needs a reason showing no production caller; EQUIVALENT needs both
an explanation and the code fact that preserves behavior. Neither counts as a
catch. Use `platforms` for OS-specific tests instead of quietly skipping them.
Here, scanners are the source-walk tests and scripts that check code for forbidden
patterns, such as outside path dependencies, cookie providers reporting auth
failures, or direct launch-nonce environment reads. Each scanner must also run on
a deliberately planted violation on every run, so one that stops matching anything
fails instead of passing over apparently clean code.

Deadlines stop hangs; assertions prove order and outcome, never elapsed time. The
pinned runner's default deadlines are not measured performance budgets. Set budgets
only from clean CI measurements, not a loaded development Mac.

Read the [pinned runner README](https://github.com/cortexkit/commons/blob/46cc166b0df2edcfd14b3eb54ed6eeac588fed69/crates/cortexkit-mutate/README.md)
for multi-file edits, dispositions and prerequisites. Our root `prebuild`
refreshes `ck-insula` before baselines, after mutant builds and on restoration;
the broad module audit spawns that binary. These rows need no real-daemon test
or sibling checkout (the daemon tests remain ignored).

Run on a clean tree. ckdev-mutate refuses even staged edit targets differing from
HEAD unless explicitly passed `--allow-dirty`; with that opt-in it restores saved
local bytes, **not** HEAD or the index. It locks the tree, checks exact-once
anchors and `Cargo.lock`, separates build and test deadlines, kills child process
groups on timeout/interruption, restores bytes and refreshes mtimes. Never edit
or check out a target mid-run. A SIGKILL/power loss cannot be recovered in-process;
use disposable CI checkouts and inspect the tree before resuming.

`scripts/probe.py` remains a legacy one-off Cargo diagnostic, not a catalogue or
replay mechanism. ckdev-mutate now covers its safe restoration, mtime refresh and
named-outcome classification, and adds test identity/message checking, baselines,
platform gates and replay. The legacy tool still **stages all changes** before
editing, accepts arbitrary Cargo test arguments (including filters/ignored tests),
builds without a deadline and uses the old exit convention (1 means a named red,
0 means undefended, 2 means no proof/hung). ckdev-mutate never stages anything, uses
locked builds with a build deadline, and exits 0 when a proof succeeds. These are
different interfaces, not missing safety features; prefer ckdev-mutate for durable
proofs. If using the legacy tool, inspect its staging side effect and do not
hand-roll a restore that could discard unstaged implementation.

Read WHICH test reddened, not merely that something did. A mutation that reddens
an unrelated test says the mutation was wrong, not that the guard is defended;
one that reddens nothing may mean the mutation missed rather than that the guard
is unguarded. Both cases are findings about the proof, not about the code.

### Mutation CI

`.github/workflows/mutations.yml` adds four advisory jobs named
`Mutation replay (1/4)` through `Mutation replay (4/4)`. It does not rename or
add required checks: branch protection remains `Test (ubuntu)` and
`Test (windows)`. Each job has its own checkout and pinned, cached runner.
Pull requests and `train/**` pushes replay touched rows; `master` pushes replay
the full catalogue. Nightly and manual runs audit `--broad`. Diff selection sees
committed edit targets, test files and changed catalogue rows, not unlisted
helper/fixture dependencies; the nightly full audit covers that limitation.
With `--broad`, tests outside a row's named test target can also catch its mutant.
ckdev-mutate 0.8.0 reports `CAUGHT_BROADLY` and only warns.
`scripts/mutation-report.py` fails the job unless the mutant was narrowed to its
own test target or the row is marked `HUB`: a deliberately shared catch listing
the other allowed test targets and the property all those tests assert.

CI artifacts carry disposition counts and each shard's measured wall duration
in `ci-timing.txt` and the step summary. No full-run CI duration is claimed yet:
the first run after landing establishes it. For a sharded run report both the
job-span wall time and the sum of shard durations; do not call a single shard
the full catalogue's time.

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
| `skeleton_e2e::` (quota-module `--test it`) | gate and CI | |
| `real_daemon_e2e::` (quota-module `--test it`) | CI only | `#[ignore]`d, needs a live daemon |
| `*_live::` provider tests (quota-core `--test it`) | NEITHER | `#[ignore]`d by design, hit real providers |
| examples: compile | gate and CI | via `clippy --all-targets` |
| examples: run | CI only | `completeness-envelopes` alone |

The two NEITHER rows are deliberate, not gaps to close: the live provider tests
reach real upstreams and would make the gate depend on someone else's uptime.
Run them by hand with `--ignored` when touching a provider's request shape.

The rows that ARE worth watching are the CI-only ones, because a green local gate
says nothing about them. If a change touches the daemon handshake or an example's
behaviour, the local gate cannot tell you.
