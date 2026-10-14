# Mutation proofs

`mutations.toml` is a catalogue of deliberate breakages. Each row names a
source edit that would remove a guard, and the tests that must fail when that
edit is made. Replaying the catalogue proves the guards still have tests that
catch their removal. The runner is `ckdev-mutate`, from
[cortexkit/commons](https://github.com/cortexkit/commons).

The catalogue covers costly properties: crash safety, exactly-once spending of
Codex credit, authorization and credential trust, wire contracts, and data
loss. Ordinary logic is left to ordinary tests.

## Install the runner

The catalogue is replayed with a pinned runner:

```sh
cargo install --locked --git https://github.com/cortexkit/commons \
  --rev 0c99c7e16d8ae22b6e114136b6b7b68be6f1394e cortexkit-mutate
ckdev-mutate --version
```

## Replay the catalogue

Run on a clean tree:

```sh
mkdir -p target/mutations
ckdev-mutate check                                                   # rows parse, anchors match exactly once
ckdev-mutate run --all --report target/mutations/all.json           # every row
ckdev-mutate run --diff origin/master --report target/mutations/diff.json   # rows touched since master
ckdev-mutate run --all --broad --report target/mutations/broad.json # also run tests outside each row's target
python3 scripts/mutation-report.py target/mutations/broad.json
```

`ckdev-mutate run` applies one row's edit, builds, runs the named tests,
records the outcome and restores the original bytes before the next row. The
catalogue's `prebuild` step rebuilds `ck-insula` before baselines, after each
mutant build and on restoration, because the `skeleton_e2e` tests some rows
name spawn that binary.

`scripts/mutation-report.py` summarizes a report and exits `1` unless every row
is one of `CAUGHT`, `HUB`, `EQUIVALENT`, `UNREACHABLE`, `SKIPPED_PLATFORM` or
`DESK_ONLY`. In particular it refuses `CAUGHT_BROADLY`: a mutant caught only by
a test outside the row's own test target, which the runner itself reports as a
warning.

## Add a row for a new guard

Create rows from the code with `ckdev-mutate prove`, never by copying old
evidence:

```sh
ckdev-mutate prove --id my-guard --guards 'the costly property protected' \
  --file crates/quota-core/src/example.rs --old 'live unique anchor' \
  --new 'independent break /* NON-VACUITY BREAK */' \
  --test-file crates/quota-core/src/example.rs --package quota-core \
  --target=--lib --expect-red example::tests::the_guard \
  --expect-message 'the intended assertion message' \
  --report target/mutations/proof.json
ckdev-mutate run --diff origin/master --broad --report target/mutations/new.json   # replay it with --broad
```

`prove` appends the row only when the named test fails with the expected
message. When the guarding tests are unknown, `ckdev-mutate explore --append`
finds them. Rules for a row:

- `old` must match exactly once in `file`.
- `expect_message` must distinguish the intended assertion from a timeout, a
  fixture failure or an unrelated panic.
- An exactly-once or spending property needs a row through the production
  provider and scheduler path, not only its helper.
- A row whose mutant is also caught by other test targets is either narrowed to
  its own target or marked `hub`, with the shared property in `hub` and the
  other allowed targets in `hub_targets`. `HUB` is not a waiver for an
  unrelated failure.
- `UNREACHABLE` needs a reason showing there is no production caller;
  `EQUIVALENT` needs the code fact that keeps behaviour unchanged. Neither
  counts as a catch.
- An OS-specific test uses `platforms` rather than being skipped silently.
- Deadlines (`timeout_s`, `build_timeout_s`) stop hangs; they are not
  performance budgets.

Read which test failed, not just that one did. A mutation that makes an
unrelated test fail means the mutation was wrong; one that makes no test fail
may mean the edit missed the guard rather than that the guard is untested.

## Source scanners

Some guards are scanners: tests and scripts that walk the source for forbidden
patterns (outside path dependencies, cookie providers reporting auth failures,
direct reads of the launch nonce). A scanner that stops matching anything
passes over everything, so `scripts/mutation-scans.sh` plants a violation for
each scanner on every gate run and fails if any scanner misses it.

## In CI

`.github/workflows/mutations.yml` runs four advisory jobs, `Mutation replay
(1/4)` to `(4/4)`; they are not required checks. Pull requests and `train/**`
pushes replay the rows touched by the change, `master` pushes replay the whole
catalogue, and the nightly and manual runs replay with `--broad`. Diff
selection sees edited targets, test files and changed rows, not helpers a test
depends on without naming; the nightly full run covers those. Each shard
uploads its report and its wall time.

## Safety

`ckdev-mutate` refuses to run when an edit target differs from `HEAD`, unless
given `--allow-dirty`, in which case it restores the bytes it saved rather than
`HEAD`. It locks the tree, kills child process groups on timeout or
interruption, and restores bytes and modification times after each row. Do not
edit or check out a target while a run is in progress. If a run is killed
outright, inspect `git status` and `git diff` before doing anything else;
`git checkout -- <file>` restores a target to `HEAD`.

`scripts/probe.py` is an older one-off tool kept for arbitrary Cargo test
filters. It stages all changes before editing and uses a different exit
convention (`1` means the named test failed); prefer `ckdev-mutate` for
anything that should be replayable.
