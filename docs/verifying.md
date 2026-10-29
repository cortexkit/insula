# Verifying a change

## The pre-push gate

```sh
scripts/prepush.sh                       # local checks, then Linux + Windows in parallel
scripts/prepush.sh --fast                # local checks only; not a full delivery gate
scripts/prepush.sh --linux-only          # explicitly omit Windows
scripts/prepush.sh --e2e --sibling ../subconscious  # also CI's real-daemon proof on Linux
```

Phase 1 does not compile Rust. It uses installed stable Rust/rustfmt, Cargo,
Python 3.11+ (with PyYAML available to `python3`), Git, Bash and `shasum`.
It runs `gates.sh --repo-only` for path dependencies, scanner controls,
deploy-policy dry runs, Python syntax, `bash -n`, endpoint hosts, train
preconditions, public layout and orphan modules. The existing lock-write
announcement remains active; `cargo metadata --locked` checks resolution first.
It also verifies every `test-vectors/*/SHA256SUMS` from its own directory.
`models-dev-slugs.py` is a separate network-backed audit, not an existing gate
or CI step; it is not added to this seconds-long offline-source phase.

Phase 2 requires `ck motor run`. It sends the working checkout snapshot to a
16-vCPU Linux job and a native Windows job at the same time, streams both, and
waits for both even if one fails. Each job uses the runner's installed toolchain
with clippy and rustfmt, prints `rustc --version` and `cargo --version`, uses
`CARGO_TERM_COLOR=always`, and has CI's 20-minute timeout. Only Linux requests
network access (`--net`); Windows motor jobs do not support that flag and use
their pre-provisioned dependencies. Timings are printed for
phase 1 and each remote job. Motor must be configured and both platforms available;
there is no host compilation fallback or silent platform skip.

Motor can omit Git metadata from a snapshot (currently the Windows runner does).
The caller sends its NUL-delimited tracked Markdown list as a base64 motor
environment value. When `.git` is absent, the worker builds a temporary Git index
from only those paths still present in the snapshot and scopes it to the
all-targets test command. This lets the document-reference test use real
`git ls-files` without allowing an untracked document to legitimize a link.
No synthetic `.git` is placed in the workspace or exposed to other commands;
module build provenance continues to report the snapshot's missing metadata.

Exit 0 means every **requested** check passed; `--fast` and `--linux-only`
explicitly print their reduced coverage. Exit 1 names a failing gate. Exit 2
means a check could not run, including a missing command, motor refusal,
lost/unknown outcome, cancellation or timeout. Neither partial option claims
a full prepush pass. `train.sh` runs `prepush.sh --fast` before its first push;
run the full gate before starting the train.

### CI parity

Source of truth: [`.github/workflows/ci.yml`](../.github/workflows/ci.yml).
CI runs on master/train pushes, pull requests, daily schedules, manual dispatch
and reusable-workflow calls. It does not fail-fast across the two OS legs.

| CI step / condition | Prepush equivalent |
|---|---|
| Checkout insula (both) | Motor snapshots the current checkout; GitHub checkout is CI-only infrastructure. When motor omits `.git`, the caller's tracked Markdown list supplies a test-scoped temporary Git index for the document-reference resolver. |
| Train preconditions (Ubuntu only) | Phase 1, same checker, before any remote job. |
| Public `.cortexkit` layout (Ubuntu only) | Phase 1, same checker and ignore controls. |
| Checkout subconscious (Ubuntu only) | Opt-in `--e2e --sibling ../subconscious` sends the sibling snapshot to Linux. No sibling is needed by the default suites. |
| Stable toolchain with clippy + rustfmt (both) | Phase 2 uses each runner's installed, pinned toolchain and prints rustc/Cargo versions. CI tracks latest stable; the read-only runner may lag, so prepush cannot catch newly introduced toolchain lints. The legacy `gates.sh` newest-stable check remains available. Phase 1 uses installed stable. |
| Defender exclusions / disabling real-time monitoring (Windows only) | CI-only disposable-VM optimization. Prepush does not change build-server security settings. |
| Rust cache for insula and subconscious (both) | CI-only cache action; motor manages its own build caches. No assertion is skipped. |
| `cargo fmt --all -- --check` (both) | Phase 1; repeated in phase 2 on each CI toolchain/platform. |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` (both) | Phase 2, identical arguments on Linux and Windows. Phase 1 additionally checks the lock without compiling. |
| `cargo test --workspace --all-targets` (both) | Phase 2, identical arguments on Linux and Windows, including vector drift tests. |
| `cargo test --workspace --doc` (both) | Phase 2, separate from all-targets on both platforms. |
| Real daemon supervision (Ubuntu only) | Phase 2 Linux **only with `--e2e`**: `cargo test -p quota-module --test it -- --ignored real_daemon_e2e::`; preserves Cargo/Rustup homes and uses isolated HOME, XDG config and state directories like CI. Never the installed daemon. |
| `cargo run -p quota-core --example completeness-envelopes` (both) | Phase 2, identical arguments on Linux and Windows. |

CI's ref-keyed concurrency cancellation and trigger scheduling stay CI-only:
a local invocation is an explicit check of one snapshot, not a branch event.
The real-daemon step is the only CI assertion omitted by default, because it
requires the sibling checkout. Live provider/session tests remain ignored in
both places; they need credentials and are not CI assertions.

## Legacy host gates

`gates.sh` remains available for existing host/deploy workflows. The modes and
coverage below describe that script, not the prepush matrix above.

```sh
scripts/gates.sh --quick  # before deploy: host lint, units, launch/wire smoke
scripts/gates.sh          # repository checks, fmt, clippy, unit tests, doctests
scripts/gates.sh --e2e    # also the in-process wire tests, about 80 s more
```

Run the gate script rather than a hand-picked list of commands; it stops at the
first failure. [`CONTRIBUTING.md`](../CONTRIBUTING.md#the-gate) lists every step.
A bug fix gets a test that fails without the fix. Show that once, with
`scripts/probe.py`, which applies the break, runs the named test and
restores the file even if interrupted.

`--quick` runs fmt, workspace clippy on the host toolchain, **all** lib/bin
tests in a scratch `TMPDIR`, and the cheap repo checks (including deploy-policy
tests). The whole unit suite is cheap warm and avoids missing cross-crate
dependents when selecting touched crates; it covers manifest and Codex reset
journal invariants. Quick also builds the module, runs only
`skeleton_round_trips_usage_get_over_the_wire` to protect the launch/route path,
and runs `completeness-envelopes` to distinguish a withheld account from a
genuinely removed account in consumer replies. It leaves
newest-stable discovery/installation and duplicate clippy, doctests, and the
minute-long refresh integration tests to the post-deploy/full loop.
`--quick` and `--e2e` are separate modes, not combinable.

### Deploy then land

On a clean local `master`, run the tests for the changed provider, handler or
script (for example `cargo test -p quota-core codex::tests::`), then:

```sh
scripts/gates.sh --quick && scripts/deploy.sh

# After deploy succeeds, run full checks and push through CI in the background:
log=$(mktemp "${TMPDIR:-/tmp}/insula-land.XXXXXX")
nohup bash -c 'scripts/gates.sh && scripts/train.sh "$1"' _ usage-fix >"$log" 2>&1 </dev/null &
printf 'landing pid=%s log=%s\n' "$!" "$log"
```

Use a descriptive train name instead of `usage-fix`; it names the temporary
`train/usage-fix` branch CI checks before pushing master. Keep the tree unchanged
until the train logs its captured SHA; never start overlapping trains. Watch
the running module with the [post-deploy checkers](#checkers-after-deploying)
and read the landing log (`tail -f "$log"`); backgrounding is not a CI waiver.
If full gates or CI fail, fix forward on local `master`, rerun narrow tests,
redeploy, and retry the same train name after the previous process exits; this
replaces the failed train rather than leaving it stranded.
Run `scripts/gates.sh --e2e` after deploy when changing integration behavior.

`deploy.sh` fetches `origin/master` and accepts equal or ahead local master,
printing each commit running before it lands. It refuses dirty trees, other
branches, behind/diverged history, or an unavailable remote. The existing
`CK_DEPLOY_ALLOW_DIRTY=1` diagnostic escape still warns loudly, but cannot bypass
the ancestry/fetch check and is not the normal shipping path.
`scripts/deploy.sh --dry-run` exercises the same provenance checks and prints
the plan without building, signing, installing or restarting anything;
`--stage` still builds and signs a real staging card. Required CI checks remain
`Test (ubuntu)` and `Test (windows)`; only their wait moves off the local deploy
path.

### What the gate does not run

`--all-targets` does not include doctests, so they are a separate step in both
the gate and CI.

| Target | Local gate | CI | How to run it |
|---|---|---|---|
| lib and bin unit tests | yes | yes | `cargo test --workspace --lib --bins` |
| doctests | full only | yes | `cargo test --workspace --doc` |
| `skeleton_e2e::` | wire smoke in quick; whole suite with `--e2e` | yes | see below |
| `real_daemon_e2e::` | no | yes | `#[ignore]`d; needs a `ck-subc` build |
| `*_live::` provider tests | no | no | `#[ignore]`d; needs a real provider session |
| examples compile | yes | yes | via `clippy --all-targets` |
| examples run | `completeness-envelopes` in quick | `completeness-envelopes` only | `cargo run -p <crate> --example <name>` |

The live provider tests are left out of both on purpose: they reach real
upstreams, so a gate that ran them would depend on someone else's uptime. Run
them by hand when changing a provider's request shape.

### Integration tests need their binary built first

`cargo test -p quota-module --test it skeleton_e2e::` spawns the `ck-insula`
binary but does not rebuild it. A stale binary fails registration with no
error output, which looks like a hang:

```sh
cargo build -p quota-module --bins
cargo test -p quota-module --test it skeleton_e2e::
```

The suite takes about 80 seconds; one test waits out a full 60-second refresh
interval because it checks that a slot fails closed on its next refresh. Its
10-second registration wait can fail on a heavily loaded machine. Re-run that
test alone before treating a failure as real, and do not widen the wait to make
it pass.

### Tests that need a session or a daemon

```sh
# real provider windows through the wire (needs that provider's session on this host)
cargo test -p quota-core --test it -- --ignored --nocapture grok_live::
cargo test -p quota-core --test it -- --ignored --nocapture antigravity_live::
cargo test -p quota-core --test it -- --ignored --nocapture jetbrains_live::

# a standalone ck-subc daemon spawns the module from subc.jsonc and routes usage.get;
# builds ck-subc from a sibling ../subconscious checkout
cargo test -p quota-module --test it -- --ignored --nocapture real_daemon_e2e::

# In an isolated worktree, use a prebuilt daemon instead of building a sibling.
# The harness still executes a disposable ckdev copy, never the installed file.
CK_SUBC_BIN=/absolute/path/to/ck-subc cargo test -p quota-module --test it -- --ignored real_daemon_e2e::

# Disposable authenticated JetStream; requires nats-server >= 2.10 on PATH
# or CK_NATS_SERVER_BIN. Does not use a live daemon or provider credentials.
cargo test -p quota-module --test it -- --ignored --nocapture quota_bus::
```

### Updating subc dependencies

`subc-protocol`, `subc-transport`, `subc-daemon`, `subc-jsonc` and `subc-os`
come from crates.io, pinned by `Cargo.lock`. To take an upgrade, run
`cargo update -p <crate>`; for a new minor version, raise its requirement in
the root `Cargo.toml` first. Review the lock diff and
`cargo tree -e normal -p quota-module`, then:

```sh
scripts/gates.sh
cargo check --locked --workspace --all-targets
cargo test -p quota-module --test it -- --ignored real_daemon_e2e::
```

Land the manifest and lock change only after all three pass.

## Exit codes

Every checker and script in this repository uses three exit codes:

| Code | Meaning |
|---|---|
| 0 | checked, found nothing |
| 1 | checked, found something; the findings are the output |
| 2 | could not check; the run makes no claim either way |

`2` is not a severity between the other two. A missing daemon, an unreadable
handle file or an empty population is neither a finding nor a pass, and a
checker that examined nothing exits `2` rather than reporting clean.

## Checkers, after deploying

These read the deployed module through the daemon, so they run after a deploy:

```sh
# every live window on the deployed module checked for internal inconsistency:
# a reset further out than the window is long, counts that disagree with the
# percent, a percent out of range; plus cross-entry and health-identity checks
cargo run -p quota-module --example deployed-sanity --release

# every vault credential granted to insula against what is actually served
cargo run -p quota-module --example vault-lanes --release

# every health metric with its raw value; a metric named but not published
# reports ABSENT and exits 1
python3 scripts/health.py [metricName ...]
```

## Diagnostic examples

These run the real fetchers against this machine's credentials. Reads are
cache-only, so each example warms its own registry first.

| Command | What it answers |
|---|---|
| `cargo run -p quota-core --example wire-sanity` | the window rules against an in-process registry; needs no daemon, but without the vault it sees only local credentials |
| `cargo run -p quota-core --example accuracy-dump` | the `usage.get` array as JSON |
| `cargo run -p quota-core --example warmup-identity` | whether the health conservation identity holds through a real warm-up; exits non-zero when the warm-up was never observed |
| `cargo run -p quota-core --example sweep-probe` | how long a full refresh sweep takes against the per-turn admission cap |
| `cargo run -p quota-core --example completeness-envelopes` | reference `usage.get` envelopes for each account-completeness case |
| `cargo run -p quota-core --example ollama-labels -- --cookie-file <file>` | which usage labels the live ollama settings page renders; exits non-zero on a label the parser does not read |
| `cargo run -p quota-core --example opencode-stage -- --cookie-file <file>` | which of opencode's three server calls fails |
| `cargo run -p quota-module --example unread-fields -- <claude\|codex> <credential-id>` | what the response fields insula does not read hold, with identity values redacted; needs a scoped grant for the id |
| `cargo run -p quota-module --example scoped-list` | what `credential.list_scoped` returns to a standalone caller |

Cookie files are read from disk (create them with mode `600`), never from argv
or the environment, which leak into `ps` and shell history. Vault-backed examples
use the caller's scoped grants to authorize reads. A standalone
direct caller may be refused; a refusal does not mean the vault has no credentials.

## Repository checkers

| Command | What it checks |
|---|---|
| `python3 scripts/endpoint-hosts.py` | every provider endpoint constant against its recorded host; run by the gate |
| `python3 scripts/path-dependencies.py` | no Cargo dependency points outside the repository; run by the gate |
| `scripts/check-train-preconditions.sh` | the CI workflow still triggers on `train/**` and is not path- or ref-gated; run by the gate |
| `scripts/check-cortexkit-layout.sh` | nothing under `.cortexkit/` is tracked except the allowed folders; run by the gate |
| `python3 scripts/models-dev-slugs.py` | every published `apiProvider` slug still exists on models.dev; needs the network |
| `python3 scripts/unread-credentials.py` | providers reporting no credential while a key they could read sits in OpenCode's auth store |
| `python3 scripts/vault-id-families.py` | every vault credential id against the family table that routes it to a provider |
| `python3 scripts/module_id_callers.py` | callers of this module's id across sibling repositories, before a rename |
| `python3 scripts/witness-transient.py 1` | makes one provider fail transiently against a loopback endpoint, so stale-serving can be observed |
| `python3 scripts/prod_body.py <files>` | prints a Rust file with its test module removed, for source sweeps |
| `python3 scripts/public-flip-verifier.py` | scans every blob and commit message in history for credential and identity patterns |
