# insula

Reads how much quota is left on every AI provider you have an account with, and
when each window resets.

If you route work across several providers, the thing you need before dispatching
is not a price list — it is whether the account you are about to use has capacity
left, and if not, when it comes back. That answer exists behind ~37 different
endpoints, in ~37 different shapes, behind ~37 different kinds of credential.
This module fetches all of them, normalizes the result to one shape, and answers
from cache so a caller never blocks on the network.

**It reuses the session you already have.** No new credential is issued and no
password is asked for: it reads the OAuth token your provider CLI already wrote,
the API key already in your environment, the browser cookie your logged-in
session already holds. A provider you are not signed in to simply reports that,
rather than failing the request for the others.

It runs as a module under [subc](https://github.com/cortexkit/subconscious), a
local supervisor that spawns it, health-checks it, and routes requests to it — so
a consumer talks to one daemon rather than to a binary per concern.

Prior art worth naming: [CodexBar](https://github.com/steipete/CodexBar) solves
the display half of this problem on macOS, and its provider fetchers are the
reference this module's normalizers are checked against on every upstream
release. The difference is that this one is headless, cross-platform, and
answers over a wire rather than drawing a menu bar.

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
| `verifying.md` | the gate procedure, in one place so a task prompt points at it rather than restating it |
| `deploying.md` | how to replace a running build, and how to verify which one is live |
| `provider-matrix.md` | per-provider auth archetype, endpoint, and verification status |
| `codex-banked-resets-design.md` | how the one mutating feature is fenced against double-spend |
| `cross-platform-design.md` | what Windows and Linux parity requires, and where it is not achievable |
| `balance-axis-design.md` | why prepaid balances are published apart from rate windows, and how the shape was chosen |

Written before the code and kept for the reasoning, **not** as descriptions of
what is there — each carries a list of the details that shipped differently:

| Doc | What it argues |
|---|---|
| `charter.md` | the original mission and the contracts it was reverse-engineered from |
| `refresher-spike-design.md` | why reads are cache-only and a single background task owns fetching |
| `multi-account-fetch-design.md` | why slots key on credential handle rather than account |
| `vault-consumer-design.md` | how credentials are fetched from the vault, and what fails closed |

**Reporting a bug that involves a provider payload:** read `SECURITY.md`
first. A captured response often carries a live token, and this project's whole
subject matter makes that the easy mistake to make.

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
cargo build --release            # builds quota-core + the ck-insula binary
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

Two examples run the real fetchers against this machine's credentials — they
warm the cache themselves, since reads are cache-only and a cold registry serves
nothing:

```sh
# ask whether every credential this host is configured for is actually being
# served. Every other check here measures internal agreement -- and a set that
# shrinks stays consistent, so a credential lane can go dark with all of them
# green, which has happened. Reports how many lanes it checked, since providers
# with a second non-vault lane cannot be covered this way:
cargo run -p quota-module --example vault-lanes

# check every live window on the DEPLOYED module for internal inconsistency (a
# reset further out than the window is long, counts that disagree with the
# percent, a percent out of range), plus cross-entry and health-identity checks.
# It reads the running module through the daemon, so it sees vault-served
# accounts. Exits non-zero on a finding, and when it examined nothing:
cargo run -p quota-module --example deployed-sanity

# the same window rules against a registry built in-process. Needs no running
# daemon, so it is what to use while changing a normalizer -- but it has no
# vault client, so vault-served providers fall back to local credentials or do
# not report at all. It examined 12 windows here where the deployed checker saw
# 29, and the difference is entirely accounts it cannot reach:
cargo run -p quota-core --example wire-sanity

# dump the live usage.get array as JSON, for eyeballing or diffing:
cargo run -p quota-core --example accuracy-dump

# check the health conservation identity across a real warm-up, when every
# provider is changing bucket at once. Exits non-zero on an imbalance, and also
# when the warm-up state never occurred and so was not observed:
cargo run -p quota-core --example warmup-identity

# measure how long a full refresh sweep takes, and how much headroom the
# per-turn admission cap leaves before the refresh interval degrades. Run it
# when the provider set grows:
cargo run -p quota-core --example sweep-probe

# print reference usage.get envelopes for the completeness cases, for a consumer
# pinning its account reconciliation against real producer output:
cargo run -p quota-core --example completeness-envelopes

# run opencode's server-function calls one at a time, when its published error
# says only that something returned 500. The provider makes three calls and any
# of them can fail that way, so the entry alone cannot say which -- this reports
# each stage separately and asks the billing function whether it answers on the
# same cookie:
cargo run -p quota-core --example opencode-stage
```

### Repository sweeps

Every checker here — the two Rust ones and the Python tools — uses three exit
codes, and the third is the one that matters:

| code | meaning |
| --- | --- |
| 0 | checked, found nothing |
| 1 | checked, found something — the findings are the output |
| 2 | **could not check** — the run makes no claim either way |

The 2 is not a severity level between the other two. It is the difference
between "I looked and it was fine" and "I could not look", and collapsing it
into either neighbour is how a checker starts lying: reported as 0 it claims a
clean bill it never earned, and reported as 1 it becomes a failure someone
retries until it passes. A missing daemon, an unreadable handle file, an empty
population — none of those are findings, and none of them are passes.

Python tools under `scripts/`, for questions the Rust checkers cannot answer
because they are about the source, the host, or the fleet rather than the wire.
Each exits non-zero on a finding and refuses to report a clean result when it
examined nothing — a sweep that finds nothing because it looked at nothing is
indistinguishable from a real pass, and that failure is silent.

```sh
# every provider endpoint against the host it is supposed to reach. Each one
# receives a credential, and the OAuth token endpoints receive a live refresh
# token in the request body -- so a host that drifts does not fail, it receives
# a working credential:
python3 scripts/endpoint-hosts.py
`scripts/parity-citations.py` — reports provider source citations whose upstream CodexBar file no longer exists at the anchored parity tag. A parity-round instrument, not a gate: the ports stay correct, but their provenance stops being followable.

# providers reporting an absent credential while a key they could read sits in
# the shared opencode auth store. That combination reads to an operator as
# "never configured", so nobody investigates it:
python3 scripts/unread-credentials.py

# read the production half of a Rust file, with the test module cut off. Any
# sweep asking "does every provider do X" needs this, or assertions inside
# run every gate in the order that makes them mean something:
scripts/gates.sh

# refuse a cargo gate result that may have come from cache. This workspace
# path-depends on a sibling repo, so that repo can change while nothing here
# does -- cargo then has nothing to redo and reports clean, and CI, which always
# builds cold, fails. Exits 1 when a sibling moved after the last local build:
python3 scripts/sibling-freshness.py

# tests count as production code:
python3 scripts/prod_body.py crates/quota-core/src/*.rs

# apply a mutation, run the tests, and restore the file whatever happens.
# Reports whether the mutation reddened a test, was never reached, or hung:
python3 scripts/probe.py <file> <before> <after>

# print every published health metric with its raw value. A metric asked for by
# name that is not published reports ABSENT and exits 1, rather than defaulting
# to zero -- which is how a hand-written probe once reported a restart that had
# not happened:
python3 scripts/health.py [metricName ...]

# drive daemon control-plane requests at a fixed cadence, one fresh CLI process
# each, and time every one. Built for the subc-core control-plane starvation hunt:
# a distribution with one 30s outlier and a uniformly slow one have the same mean
# and different causes, so it reports every sample rather than an aggregate:
python3 scripts/channel0-cadence.py <baseline|drain|drain-with-in-flight> [samples]

# make a provider fail transiently against a loopback endpoint, so
# preserve-the-window behaviour can be witnessed on the wire rather than
# waited for. A field that only appears during a failure cannot be verified
# on a healthy host: absent is what correct and never-populated both look like:
python3 scripts/witness-transient.py 1

# make a quota drop happen ACROSS A GAP, so `observedContinuously: false` can be
# witnessed rather than waited for. Thirteen recorded drops across two hosts have
# all read true, and a field that has only ever taken one value is
# indistinguishable from one that is stuck. Reads the continuity horizon from
# source, and refuses if either constant was renamed:
python3 scripts/witness-gap-drop.py

# check that every rule in the wire checkers has a test that fails when the
# rule is deleted, and a control that does not fire when it should not:
python3 scripts/audit-checker-rules.py

# find every caller of this module's id across the fleet, so a rename can be
# sequenced rather than discovered:
python3 scripts/module_id_callers.py

# measure this process's real disk reads and writes, with a control write to
# prove the counter moves before reporting anything:
python3 scripts/measure-disk-io.py

# report when macOS Gatekeeper's scan list has filled with dead Rust build
# artifacts. syspolicyd re-walks target/debug/deps looking for binaries cargo
# already deleted -- measured here at 13k directory syscalls a second. The
# database survives OS upgrades, which is why reinstalling does not clear it.
# Exit 1 means maintenance is indicated; clearing needs Recovery, because SIP
# protects the file while booted.
#
# A CONTRIBUTOR, NOT THE CAUSE: clearing this dropped syspolicyd to idle and the
# whole-machine lockups recurred within the hour. The dominant cost is the FIRST
# exec of any freshly written binary (~300ms + 5ms/MB, against ~4ms to re-exec
# the same bytes), serialised through one validator -- so a gate that rebuilds
# and warm-executes ~100 test binaries queues the whole host for ~90 seconds. See
# the capture script below:
python3 scripts/gatekeeper-scanlist.py

# whole-history exposure audit: scans every blob on disk (reachable or not) and
# every commit message against narrow credential/identity classes. Proved in both
# directions before its verdict is trusted -- synthetic positives must fire,
# example/test/loopback specimens must stay silent. Exit 1 means hits to read:
python3 scripts/public-flip-verifier.py

# restart syspolicyd when it wedges, so recovery does not require a terminal --
# which is exactly what a wedge prevents you from opening. Errs toward NOT
# killing: high threshold, 120s sustain, startup grace, hard rate limit. See the
# plist header for install and removal:
sudo /usr/bin/python3 scripts/syspolicyd-watchdog.py   # foreground, to watch it

# capture what syspolicyd is doing WHEN it wedges, instead of diagnosing the
# wreckage afterwards. Arms at negligible cost and only records once the CPU
# signature appears; writes the trigger record BEFORE spawning anything, because
# during a wedge the follow-up commands may block too:
sudo /usr/bin/python3 scripts/syspolicyd-capture.py

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
    "insula": {
      "program": "/abs/path/to/target/release/ck-insula"
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
