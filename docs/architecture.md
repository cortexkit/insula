# Architecture

insula is two crates. `quota-core` knows providers and nothing about subc;
`quota-module` knows subc and the credential vault and nothing about any
particular provider. Everything a consumer can rely on is stated in
[`consumer-contract.md`](consumer-contract.md); this page describes how the
code produces it.

## The two crates

### `quota-core`

The library. Its public entry point is `Registry` in
`crates/quota-core/src/lib.rs`.

| Module | Role |
|---|---|
| one file per provider (`codex.rs`, `anthropic.rs`, `antigravity.rs`, …) | an adapter implementing `UsageProvider`; the full list is in [`provider-matrix.md`](provider-matrix.md) |
| `provider.rs` | the `UsageProvider` trait, `CredentialHandle`, `FetchAttempt`, and `FetchError` with its published `error_class()` strings |
| `refresh.rs` | pure scheduling state: the per-slot state machine, failure classification (`classify`), backoff, and the timing constants |
| `store.rs` | `SlotStore`, the in-memory map of slots behind one mutex, plus the drop ring and the process-lifetime counters |
| `health.rs` | `HealthSnapshot`, the provider buckets behind the health metrics |
| `quota_drop.rs` | decides whether two consecutive readings show a decrease and whether it was observed continuously |
| `vault_handles.rs` | `CREDENTIAL_FAMILIES` (vault id prefix → provider) and `VaultHandleLoader`, which routes the vault's listing to providers |
| `credential_source.rs` | the `CredentialSource` trait providers call to read a vault credential |
| `cookie_vault.rs`, `cookie_jar.rs` | session-cookie lanes, read only from vault deposits |
| `codex_resets.rs` | the banked-reset policy and the redemption journal |
| `model.rs`, `money.rs` | wire types re-exported from `cortexkit-provider-usage`, and exact money amounts |
| `http.rs`, `env.rs`, `text.rs`, `json_scan.rs`, `subprocess.rs` | shared request, error-excerpt, environment, file and parsing helpers |
| `config.rs` | `QuotaConfig`, the startup configuration (today only `codex.auto_use_resets`) |
| `wire_sanity.rs`, `unread_keys.rs` | internal-consistency rules for published windows, and logging of response fields no adapter reads |

### `quota-module`

The `ck-insula` binary.

| File | Role |
|---|---|
| `src/main.rs` | startup, the module manifest, the subc frame loop, the `usage.get` / `usage.drops` handlers and the `health.check` report |
| `src/vault_client.rs` | the client for the credential vault module; implements `CredentialSource` |
| `src/ids.rs` | the module ids this module answers as and dials |
| `src/lib.rs` | publishes `vault_client` and `ids` so diagnostic examples use the same client as the module |

## From enumeration to serving

```
vault listing ─┐
local files ───┼─> handles() ─> SlotStore ─> due slots ─> fetch_handle() ─> slot publish
environment ───┘                    ^                                         │
                                    └──────────── usage.get / health.check <──┘
```

### 1. Start

`main.rs` reads the subc connection file the daemon passes as `--subc`, loads
the quota config from `$XDG_CONFIG_HOME/cortexkit/ck-quota.jsonc` (default
`~/.config/cortexkit/ck-quota.jsonc`; a missing or malformed file means the
defaults, with banked resets off), builds `Registry::with_defaults` with the
vault client as its credential source, sends `HELLO` with the manifest, and
spawns `Registry::refresh_loop`. The manifest declares one management surface
with two query operations, `usage.get` and `usage.drops`, and declares that the
module consumes the credential vault.

### 2. Enumerate

Each refresher turn (`refresh_tick_with_deadline` in `lib.rs`) starts by asking
the vault for the credentials granted to this module
(`credential.list_scoped`) and installing the answer in `VaultHandleLoader`.
It then calls every provider's `handles()`. A provider builds its handles from
the installed vault rows for its id families, from local credential files, and
from environment variables. `handles()` never touches the network.

A provider that enumerates successfully returns its complete set. One that
fails may still name handles it knows exist; those get slots, but the provider
is not counted as complete that turn. `SlotStore::reconcile_batch` creates a
slot for each new `(provider, handle)` key and removes slots whose keys are no
longer enumerated.

### 3. Schedule

A slot is due `BASE_INTERVAL` (60 s) after its last completion. A failed slot
waits out a backoff instead: `NON_TRANSIENT_BACKOFF` (300 s) for a failure
retrying cannot fix, or an exponential backoff capped at
`MAX_TRANSIENT_BACKOFF` (900 s) for a transient one. A vault row that comes
back at a higher `record_version`, or is reactivated, makes its slot due
immediately; a row in `needs_reauth` is not fetched at all.

`select_due_round_robin` picks due slots starting after the provider admitted
last, so no provider starves, and at most `CONCURRENCY_CAP` (8) fetches run at
once. Between turns the loop sleeps until the next slot is due, at most
`MAX_TICK_SLEEP` (5 s), which also bounds how long a new credential waits to be
seen.

### 4. Fetch

Each admitted slot runs `fetch_handle()` in its own task under
`FETCH_DEADLINE` (35 s). A timeout becomes an upstream failure and a panic
becomes `internal_error`; neither affects any other slot. The adapter resolves
its credential (reading a vault credential through `CredentialSource`, which
returns it already refreshed), calls the provider, and normalizes the answer
into rate windows and `spend` pools, or returns a `FetchError`.

### 5. Store

`refresh.rs` turns the attempt into the slot's next state:

- success replaces the served entry and records `fetchedAt`;
- a transient failure keeps serving the last healthy entry and marks it
  `stale`, or degrades the slot if it never succeeded;
- a non-transient failure replaces the entry with a degraded one carrying its
  `errorClass`.

`quota_drop::detect` compares the new reading with the previous one and
records drops and not-comparable reasons. The slot is then published with
`publish_if_current`, which discards the result if the slot was removed or
replaced while the fetch was running.

### 6. Serve

`usage.get` calls `Registry::usage_snapshot`, which reads the store under its
mutex and never waits on a fetch. It applies the identity rules (one unlabeled
entry at most per provider; entries withheld while their account is
unverified), applies the read-time banked-reset relaxation, computes
`completeProviders`, and returns the envelope. `usage.drops` pages the drop
ring. `health.check` combines `Registry::health()` with the vault client's and
the serving counters into the status, detail and metrics described in
[the consumer contract](consumer-contract.md#health-is-a-separate-axis).

## State

All serving state is in memory and is rebuilt after a restart: slots, the drop
ring (the newest 100 records, identified by an `epoch` that changes on
restart), and every counter. The one file insula writes is the Codex
redemption journal, `$XDG_STATE_HOME/cortexkit/insula/redemptions.json`
(default `~/.local/state/cortexkit/insula/redemptions.json`, directory
overridable with `CK_QUOTA_STATE_DIR`). It is written to a temporary file in
the same directory and renamed into place.

## Rules that bite

- **Timing constants are linked.** `FRESH_HORIZON` (120 s) must exceed
  `BASE_INTERVAL + FETCH_DEADLINE`, or healthy providers read stale between
  their own fetches (`a_healthy_slot_stays_fresh_across_its_whole_refresh_cycle`).
  `STALL_HORIZON` (300 s) must exceed `FETCH_DEADLINE + MAX_TICK_SLEEP`
  (`a_healthy_refresher_is_not_reported_stalled_between_its_own_turns`).
  `FETCH_BLACKOUT_HORIZON` (1800 s) must exceed the longest gap one failing
  provider can legitimately produce
  (`the_blackout_horizon_exceeds_the_longest_legitimate_success_gap`).
- **`handles()` is local.** Enumeration runs for every provider on every turn,
  outside any fetch deadline; network I/O there would stall the whole loop.
- **Never clear the store's view on a failed enumeration.** A vault list that
  fails keeps the installed rows; a provider that fails to enumerate keeps its
  slots. Removing slots is how a consumer would be told an account is gone.
- **A failure's `FetchError` variant is its contract.** The variant decides the
  published `errorClass`, whether the failure is transient, and whether the
  slot keeps serving its last window. `classify` in `refresh.rs` is the one
  place that mapping lives.
- **Identity before labels.** An account id is published only when the fetch
  that produced the usage also verified it. Two lanes reading one account must
  publish the same `account`; `source` may differ.
- **Absent is not zero.** An adapter publishes only what the provider stated.
  A window with no reset, a pool with no remainder, or a count the provider did
  not report stays absent.
- **A redemption is journaled before it is sent.** Anything that spends Codex
  credit goes through `codex_resets.rs`, which reserves a request id and
  persists it before the request.
