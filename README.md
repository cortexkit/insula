# ai-provider-quota

A subc-supervised module that knows every AI provider's usage limits and reset
windows — the headless engine that replaces the external **CodexBar** dependency.

Alfonso's model router needs to know how much quota each provider has left and when
each window resets, so it can route around exhaustion (the "provider is in a quota
cooldown" decisions). This module fetches each provider's usage by reusing the
user's OWN existing session (OAuth token, API key, local CLI file), normalizes it to
a uniform `ProviderUsage[]` shape, and serves it **through subc** — so Alfonso
connects to subc (not an external binary) for its quota signal.

**Adding or changing a provider: read `docs/provider-invariants.md` first.** It lists
the properties a normalizer must uphold, each recovered from a defect that shipped.
**Consuming `usage.get`: read `docs/consumer-contract.md`.** It states what you can
rely on and what you must not infer, and every rule in it was settled against a real
consumer.

### The rest of `docs/`

Current, and kept true against the code:

| Doc | What it answers |
|---|---|
| `provider-invariants.md` | what a provider normalizer must and must not do |
| `consumer-contract.md` | what `usage.get` promises a consumer, and what it does not |
| `deploying.md` | how to replace a running build, and how to verify which one is live |
| `provider-matrix.md` | per-provider auth archetype, endpoint, and verification status |
| `codex-banked-resets-design.md` | how the one mutating feature is fenced against double-spend |

Written before the code and kept for the reasoning, **not** as descriptions of
what is there — each carries a list of the details that shipped differently:

| Doc | What it argues |
|---|---|
| `charter.md` | the original mission and the contracts it was reverse-engineered from |
| `refresher-spike-design.md` | why reads are cache-only and a single background task owns fetching |
| `multi-account-fetch-design.md` | why slots key on credential handle rather than account |
| `vault-consumer-design.md` | how credentials are fetched from the vault, and what fails closed |

## What it serves

A subc `ManagementSurface` exposing one query operation, `usage.get`, which returns
`{ "result": ProviderUsage[] }`. Each entry is one provider/account's windows
(`primary`/`secondary`/`tertiary` `RateWindow`s with `usedPercent` + `resetsAt`), or
a silent-degraded entry carrying `error` when that provider has no usable session —
a single provider's failure never fails the whole array.

## Providers (window-bearing)

35 providers are registered, each fetching a real rate/usage window:

`codex`, `claude`, `antigravity`, `codebuff`, `copilot`, `cursor`, `doubao`,
`elevenlabs`, `factory`, `gemini`, `grok`, `jetbrains`, `kimi`, `kimi-for-coding`,
`clinepass`, `llmproxy`, `manus`, `mimo`, `minimax`, `neuralwatt`, `ollama`,
`opencode`, `opencodego`, `qoder`, `qwen-cloud`, `sakana`, `stepfun`, `sub2api`,
`warp`, `synthetic`, `zai`, `zenmux`, `kilo`, `alibaba`, `amp`.

A provider may serve more than one account, so the served entry count exceeds the
provider count.

Verification (see each module's `VERIFICATION:` doc block):
- **Live-verified** (real window proven through the wire): codex, claude, gemini,
  grok, ollama, antigravity.
- **Hybrid**: jetbrains (file-read/parse/degrade proven on real disk; active-window
  mapping fixture-verified).
- **Fixture-verified** (CodexBar-sourced port, no credential on the build machine;
  upgraded to live when a key is supplied): the rest.

Providers that produce no real reset window (prepaid balances, fabricated resets,
desktop-only browser-cookie sources) are deferred with rationale in
`docs/provider-matrix.md`.

## Build

```sh
cargo build --release            # builds quota-core + the ck-quota binary
cargo test -p quota-core         # provider normalizers (unit)
cargo test -p quota-module       # in-process wire e2e (skeleton_e2e)
```

Live and real-daemon proofs are `#[ignore]` (need real sessions / a built
`ck-subc`):

```sh
# real provider windows through the wire (needs the provider's real session):
cargo test -p quota-core --test gemini_live    -- --ignored --nocapture
cargo test -p quota-core --test grok_live      -- --ignored --nocapture
cargo test -p quota-module --test skeleton_e2e -- --ignored --nocapture

# real-daemon supervision: a standalone ck-subc daemon spawns the module from
# subc.jsonc and routes usage.get (builds ck-subc in ../subconscious):
cargo test -p quota-module --test real_daemon_e2e -- --ignored --nocapture
```

## Install as a supervised subc module

The subc daemon binary (`ck-subc`, built from the `subc-core` crate) spawns and
supervises modules listed in its config at
`$XDG_CONFIG_HOME/cortexkit/subc.jsonc` (`~/.config/cortexkit/subc.jsonc`). Add an
entry pointing `program` at the built binary — see `examples/subc.jsonc`:

```jsonc
{
  "version": 1,
  "modules": {
    "ai-provider-quota": {
      "program": "/abs/path/to/target/release/ck-quota"
    }
  }
}
```

Once it is installed, **`docs/deploying.md` is how you replace a running build** —
including why a bare `kill` is the wrong way to restart it, and how to verify from
the running process that the deployed binary is the one you think it is.

`args`/`env`/`enabled` are optional. The daemon appends `--subc <connection-file>`
and injects `SUBC_MODULE_ID` itself — do **not** put those in `args`/`env`. On its
next start the daemon spawns the module, which HELLO-registers its ManagementSurface;
a consumer then reaches it via `catalog.list` → `route.open` → `usage.get`.
