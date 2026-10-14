# Verifying a change

## The gates

```sh
scripts/gates.sh          # repository checks, fmt, clippy, unit tests, doctests
scripts/gates.sh --e2e    # also the in-process wire tests, about 80 s more
```

Run the gate script rather than a hand-picked list of commands; it stops at the
first failure. [`CONTRIBUTING.md`](../CONTRIBUTING.md#the-gate) lists every step.
A change that guards a costly property also gets a mutation row; see
[`mutations.md`](mutations.md).

### What the gate does not run

`--all-targets` does not include doctests, so they are a separate step in both
the gate and CI.

| Target | Local gate | CI | How to run it |
|---|---|---|---|
| lib and bin unit tests | yes | yes | `cargo test --workspace --lib --bins` |
| doctests | yes | yes | `cargo test --workspace --doc` |
| `skeleton_e2e::` | with `--e2e` | yes | see below |
| `real_daemon_e2e::` | no | yes | `#[ignore]`d; needs a `ck-subc` build |
| `*_live::` provider tests | no | no | `#[ignore]`d; needs a real provider session |
| examples compile | yes | yes | via `clippy --all-targets` |
| examples run | no | `completeness-envelopes` only | `cargo run -p <crate> --example <name>` |

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
| `cargo run -p quota-module --example grok-resets` | whether grok's reset endpoint answers the vault credential, with a control call first |
| `cargo run -p quota-module --example antigravity-pool` | which Antigravity pool the vault's cloud credential reaches |
| `cargo run -p quota-module --example unread-fields -- <claude\|codex>` | what the response fields insula does not read hold, with identity values redacted |
| `cargo run -p quota-module --example scoped-list` | what `credential.list_scoped` returns to a standalone caller |

Cookie files are read from disk (create them with mode `600`), never from argv
or the environment, which leak into `ps` and shell history. The examples that
need a vault credential take a capability handle in a file, minted with
`ck auth mint-handle --id <credential-id>`. Revoke it afterwards using only the
`revoke with:` line `mint-handle` printed for that handle.

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
| `python3 scripts/audit-checker-rules.py` | every rule in the wire checkers has a test that fails without it |
| `python3 scripts/module_id_callers.py` | callers of this module's id across sibling repositories, before a rename |
| `python3 scripts/witness-transient.py 1` | makes one provider fail transiently against a loopback endpoint, so stale-serving can be observed |
| `python3 scripts/witness-gap-drop.py` | makes a quota drop happen across a gap, so `observedContinuously: false` can be observed |
| `python3 scripts/prod_body.py <files>` | prints a Rust file with its test module removed, for source sweeps |
| `python3 scripts/public-flip-verifier.py` | scans every blob and commit message in history for credential and identity patterns |
