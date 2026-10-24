# insula

insula reads how much quota is left on each AI provider account a machine is
signed in to, and when each window resets. It runs as a module under
[subc](https://github.com/cortexkit/subconscious), the CortexKit supervisor:
a background refresher fetches every provider's own usage report with the
credential that already exists on the host (a vault login, a provider CLI's
token file, an API key in the environment), normalizes each answer into rate
windows and prepaid pools, and serves the result from memory over the subc
`usage.get` operation, so a caller never waits on a provider's network. A
provider that has no usable credential on the host reports that in its own
entry; it never fails the request for the others.

| Crate | Role |
|---|---|
| `crates/quota-core` | Provider adapters, normalization, the slot store and the background refresher, behind a wire-agnostic `Registry`. |
| `crates/quota-module` | The `ck-insula` binary: registers with the subc daemon, dials the credential vault, and answers `usage.get`, `usage.drops` and `health.check` from the registry's cache. |

## Build and test

Needs only a Rust toolchain:

```sh
cargo build --release                   # quota-core and the ck-insula binary
cargo test --workspace --lib --bins     # unit tests
cargo test --workspace --doc            # doctests
```

The full local gate (formatting, clippy, unit and doc tests, and the
repository's own checks) also needs Python 3.11 or newer with PyYAML:

```sh
scripts/gates.sh            # what a change must pass before it lands
scripts/gates.sh --e2e      # also the in-process wire tests (skeleton_e2e)
```

Tests that need a real provider session or a running subc daemon are
`#[ignore]`d; [`docs/verifying.md`](docs/verifying.md) lists them with their
commands.

## Providers

The [provider matrix](docs/provider-matrix.md) lists all 44 registered providers.
Balance-only providers include `deepseek`, `openrouter`, `vercel`, and `claude-console`.
`soniox` publishes an organization's prepaid balance beside a monthly budget
window, read with a Soniox organization API key.
`claude-console` reads a Console organization's subscription, purchased, and
promotional API credit from a `cookie:platform.claude.com:<account>` vault deposit
captured by Cerebellum; several accounts can be deposited separately. It never
reads a browser or an API key. See the [capture instructions](docs/consumer-contract.md#depositing-a-session-cookie-which-url-to-capture-against).

## Documentation

| Doc | What it answers |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | how the two crates fit together, from enumeration to serving |
| [`docs/consumer-contract.md`](docs/consumer-contract.md) | what `usage.get`, `usage.drops` and the health metrics promise a consumer, and what they don't |
| [`docs/provider-matrix.md`](docs/provider-matrix.md) | every registered provider: credential sources, endpoints, published windows |
| [`docs/credentials.md`](docs/credentials.md) | how to make an account show up |
| [`docs/installing.md`](docs/installing.md) | installing, upgrading and removing the module |
| [`docs/verifying.md`](docs/verifying.md) | the gates, the ignored tests, and the checkers to run after a deploy |
| [`docs/mutations.md`](docs/mutations.md) | the mutation catalogue in `mutations.toml` and how to prove a guard |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | how to make and land a change, and where code goes |
| [`SECURITY.md`](SECURITY.md) | reporting a vulnerability, and what the module does with credentials |
| [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md) | licenses of code adapted from other projects |

insula is MIT licensed; see [`LICENSE`](LICENSE).
