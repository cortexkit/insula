# Background Refresher + Cache-Only Read (Q4 spike)

Status: **LANDED on master and in production**, and has been since the spike
graduated; the `spike/refresher` branch no longer exists. This document is the
design as it was proposed and reviewed, kept for the reasoning behind the
shape — it is not a description of the current code, and several details below
shipped differently (each is flagged where it appears). For what the code does
now, read `crates/quota-core/src/refresh.rs`, `store.rs`, and the read path in
`lib.rs`; for what the wire promises, `docs/consumer-contract.md`.

Both Oracle passes were folded before it landed. 2nd-pass implementation fixes
folded: (1) CRITICAL panic containment — a panicking
provider is `catch_unwind`-contained into its own non-transient failure, never
crashing the refresher (would silently drop Q4); (2) transient-after-degraded
stays degraded (a prior degraded entry is not relabelled stale-transient); (3)
per-provider completion timestamps + incremental writeback (each result
published as it completes, computed OUTSIDE the lock) — a fast provider is no
longer held behind a slow one, which also means `usage.get` can return a PARTIAL
array mid-sweep (a real consumer polls for the provider it needs). Scope:
single-account, per-provider. The account-aware retrofit
(per-`(provider, account)`) is deliberately clean: every store key and refresh
unit below is already per-provider and becomes per-`(provider, account)` by
widening the key. A SECOND adversarial Oracle pass on the IMPLEMENTATION is
reserved before prod graduation (this pass reviewed the design).

## Crate boundary

All concurrency-critical logic (the slot store, the read path, one sweep, the
scheduling/backoff math, health) lives in `quota-core` so it is unit-tested in
one place — tokio is promoted from a dev-dep to a real dep there (the crate is
already async-native via reqwest/async_trait; this adds only `tokio::time` +
`sync::Semaphore` + `time::timeout`, still subc-free). `quota-module` owns ONLY
the lifecycle: spawn the loop task, drive the cadence, cancel it when the frame
loop ends.

## Problem

Today `Registry::get_usage` is lazy pull-through: on a cold/expired cache it
fetches ALL providers inline and blocks the caller (verified: `lib.rs:114-145`).
The multi-account contract's Q4 requires `route.select` to NEVER block on a live
sweep. So the serving read must become cache-only, and a background task must
own all fetching.

## Invariants (the load-bearing ones)

1. **Reader never touches the network.** `get_usage` only ever does
   lock → clone → unlock. No `.await` on a fetch inside it. This is the Q4
   guarantee, and the test asserts it holds even while the refresher is mid-sweep.
2. **The cache lock is NEVER held across `provider.fetch().await`.** The
   refresher does: lock → take a snapshot of what is due (cheap) → unlock →
   fetch outside the lock → lock → write results (cheap) → unlock. A held lock
   only ever guards in-memory map ops, so reader latency is bounded and
   independent of network latency.
3. **Poison-tolerant serving.** The store is just a cache; a torn write is at
   worst a stale/missing entry, never memory-unsafety. Both the reader and
   `health()` recover a poisoned guard (`lock().unwrap_or_else(|e| e.into_inner())`)
   rather than panicking the module. (Panic-across-write is already near-
   impossible since the write path holds the lock only over a map insert.)

## Store shape (Oracle fix #1: no ambiguous `entry`)

Replace the filter-keyed single-slot cache with a per-provider map. `entry` is
`Option` — `None` means never-successfully-fetched (honest cold/absent), so the
"absent vs stale vs degraded" ambiguity Oracle flagged is gone:

```
provider_name -> ProviderSlot {
    entry: Option<ProviderUsage>,      // last SERVED value; None until first resolve
    last_success_at: Option<Instant>,  // last OK fetch — drives read-time freshness
    last_attempt_at: Option<Instant>,  // set at each attempt START (liveness, not data)
    last_status: SlotStatus,           // Fresh | StaleTransient | Degraded(FetchClass)
    next_due_at: Instant,              // when the refresher should try again
    retry_count: u32,
}
```

This sketch is what was proposed; four details shipped differently, and the
source is `crates/quota-core/src/refresh.rs` and `store.rs`:

| Sketched here | Shipped as |
|---|---|
| `last_status` | the field is `status` — `last_status` exists nowhere |
| `Degraded(FetchClass)` | `Degraded` is a **unit** variant, carrying no class |
| keyed by `provider_name` | keyed by `SlotKey { provider, handle }`, once multi-account landed |
| `last_tick_at: Instant` (below) | `Option<Instant>`, `None` until the first tick |

The concurrency cap below is also described as a semaphore; the implementation
uses `buffer_unordered(CONCURRENCY_CAP)`. The cap value of 8 is accurate.

- **Whole-slot atomic write (Oracle fix #3):** the refresher computes the entire
  next `ProviderSlot` OUTSIDE the lock, then does a single `insert` under it.
  No multi-field mutation under the lock that a panic could tear.
- **Stable order (Oracle fix, medium):** `get_usage(None)` assembles the array
  by iterating `self.providers` (registry order) and looking up each slot — NOT
  raw `HashMap` iteration (which is nondeterministic and would churn tests /
  consumer order).
- **Read-time freshness (Oracle fix #5):** the served window's freshness is
  computed AT READ from `last_success_at` (`now - t <= FRESH_HORIZON`), never a
  stored boolean — a wedged refresher can therefore never serve `fresh=true`
  forever. (`fresh` on the WIRE stays deferred until the ALF-coordinated serde
  change; the spike computes+tests it internally.)

## Failure policy by class (Oracle fix #2: stale-healthy must not mislead)

The dangerous case: serving a stale HEALTHY window after the local session died
would route to a dead provider, and `fresh:bool` is not on the wire yet for the
router to discount. So stale-serving is **class-conditional**, which is also
more correct (degrade-never-wrong):

- **Transient** (`Upstream`/timeout/5xx/429): keep the last good `entry`, set
  `last_status = StaleTransient`. A blip does not mean the session is gone —
  this is the "never blank on a 429" the contract wants. `fresh=false` at read.
- **Non-transient** (`NoSession`/`Unauthorized`/`Decode`): REPLACE `entry` with
  a degraded `ProviderUsage` (drop the stale healthy window), `last_status =
  Degraded`. An auth/session failure is exactly when a stale healthy window is
  unsafe; the consumer already skips entries carrying `error`.
- **Cold first failure:** `entry` was `None` → a degraded entry (named provider,
  error) so a known-bad provider is visible, distinct from never-attempted.

## Refresher loop

A single background task, spawned by the module, owns the fetch loop:

- **Cold start:** immediate first pass on spawn. Until it completes, `get_usage`
  returns an empty/partial array (correct: non-blocking; the router treats an
  absent provider as no-data). The module does NOT block HELLO/readiness on warm.
- **Tick:** wake on `min(next_due_at)` (or a bounded max sleep, e.g. 5s, so a
  newly-due provider is not starved). Collect providers with
  `next_due_at <= now`, fetch them **concurrently within the tick** with a
  concurrency cap (semaphore, default 8) so one slow provider does not delay the
  others and we do not thunder-herd ~27 upstreams.
- **Whole-fetch deadline (Oracle fix #2/must-fix):** each `provider.fetch()` is
  wrapped in a refresher-level `tokio::time::timeout` (e.g. 35s — just over the
  30s HTTP timeout). "One fetch is 30s-bounded" is NOT guaranteed by `http.rs`
  alone: several providers make MULTIPLE awaited HTTP calls plus sync local
  reads, so only a refresher-level deadline actually bounds a sweep. A timeout
  is a transient failure; a panicked/`JoinError` fetch is a non-transient
  `Decode`-class failure (its own bug), never a refresher crash.
- **Schedule from COMPLETION, not sweep-start (Oracle fix #4):** `next_due_at`
  is computed from when THIS provider's attempt finished, not the tick start.
  Otherwise a fast provider that finished at t+1s while a slow one ran to t+35s
  would be instantly re-due → back-to-back churn.
- **On success:** `entry = Some(usage)`, `last_success_at = now`,
  `retry_count = 0`, `next_due_at = completed_at + BASE_INTERVAL` (60s),
  `last_status = Fresh`.
- **On failure:** apply the class policy above, bump `retry_count`, schedule per
  backoff below (from completion time).
- **Refresher heartbeat (Oracle fix #3):** the loop stamps a
  `last_tick_at: Instant` (shared, own field) at the TOP of every tick,
  unconditionally — this is the liveness signal health() reads, decoupled from
  whether any provider succeeded.
- **Shutdown:** the task holds a `CancellationToken`; the module cancels it when
  the frame loop ends. Cancellation aborts the sleep, the semaphore wait, AND
  in-flight fetch tasks (select on the token). No leaked task, no lingering
  fetch after shutdown.

## Backoff (contract reference semantics)

Classify `FetchError`:
- **transient** (`Upstream`, timeouts, 5xx, 429): `next_due = completed_at +
  min(60s · 2^min(retry_count-1, 6), 15m)`. `retry_count` is incremented BEFORE
  the formula and the exponent uses `saturating_sub(1)` (Oracle fix #4:
  n=1 → 60s, n=2 → 120s, ... capped 15m).
- **non-transient** (`NoSession`, `Unauthorized`, `Decode`): fixed
  `completed_at + 5m`. (No creds / bad shape will not fix itself on a fast
  retry; slow re-probe so a fresh login is picked up within ~5m.)

Thundering-herd on cold start: all ~27 providers are due at t0. The semaphore
cap (8) bounds the first sweep to ceil(27/8)·deadline concurrency, not 27
simultaneous upstream hits.

**Retry-After — DEFERRED to graduation, not in the spike (Oracle fix #4 +
contract note).** `FetchError::Upstream(String)` cannot carry a `Retry-After`
value or reliably distinguish 429 from generic 5xx, and threading structured
retry metadata through every provider's error mapping is full-scope work, not
spike. The spike implements CLASS-BASED backoff only (which is safe and bounded);
an unbounded `Retry-After` override could otherwise suppress refresh for hours.
Retry-After lands with the structured-error change at graduation. Flagged to the
driver since the contract named it. GRADUATION REQUIREMENT (driver-ruled): when
Retry-After is implemented it MUST be CLAMPED — `min(retry_after,
MAX_TRANSIENT_BACKOFF)` — never honored raw, so an upstream cannot dictate an
unbounded refresh-suppression window.

Re-derived against the shipped code: **the stated blocker is gone, and the
feature is still not wanted.** `FetchError` gained `ProviderStatus(u16)`, so 429
is distinguishable from a generic 5xx today and the "cannot tell them apart"
reason no longer holds. What has not changed is the value: class-based backoff
already caps transient retries at 15 minutes, no provider here has been observed
sending a `Retry-After` this module would obey differently, and honouring one
could only lengthen a wait that is already bounded. The clamp requirement above
remains the condition on any future implementation.

## Freshness

`fresh = last_success_at.map(|t| now - t <= FRESH_HORIZON).unwrap_or(false)`,
`FRESH_HORIZON = 2 · BASE_INTERVAL` (120s) — i.e. fresh while the refresher is
keeping the entry current; false once it falls a cycle behind (backoff / wedge).
Tracked internally + in tests for the spike. `fresh: bool` on the WIRE is a
shared-shape serde change (two consumers: ALF pace extractor + router) and is
NOT shipped until coordinated with ALF.

## Health integration (Q4 observable) — Oracle fix #3

The wedge signal is the refresher's own HEARTBEAT (`last_tick_at`), NOT
"newest last_success_at across slots" — the latter is unsound both ways:
- one provider succeeding every minute would hide 26 stale providers (false Ok);
- an all-`NoSession` box (every provider legitimately unauthorized) would look
  "wedged" though the refresher is ticking perfectly (false Degraded).

So the status ladder reads the loop's liveness, and provider staleness is
detail:
- **cache poison → `Failing`** (unchanged; and health surfaces poison, never
  silently clears it — Oracle fix #6).
- **refresher stalled → `Degraded`:** `last_tick_at` older than
  `STALL_HORIZON` (5 · BASE_INTERVAL = 300s) ⇒ the loop is wedged/dead. First
  real use of the `Degraded` variant.
- **else `Ok`**, with metrics carrying the useful detail: counts of
  fresh / stale-transient / degraded providers, oldest `last_success_at`, and
  `last_tick_age`. Per-provider staleness is observable WITHOUT flipping status
  (a provider legitimately lacking creds is normal, per the existing posture).

## Explicitly OUT of the spike (full-scope, later)

- Multi-account per-`(provider, account)` fetch (needs vault list-accounts + own
  handles) — gated on ALF bandwidth + Ufuk sequencing.
- Shipping `fresh: bool` on the wire — coordinate with ALF's extractor first.
- Prod graduation — reserved behind the pre-ship Oracle pass + a live smoke.

## Oracle verdicts (bg_f9972eaf) — resolved

Architecture confirmed sound (cache-only read + single background refresher);
"not safe as-is" must-fixes are all folded above:
1. Reader/writer atomicity — mixed-sweep array is fine for a per-provider
   consumer; whole-slot atomic write-back enforced (store shape).
2. Whole-`fetch()` deadline added (http.rs 30s is not a per-fetch guarantee);
   stale-serving made class-conditional (transient keeps, auth/no-session
   degrades).
3. Health liveness switched to a refresher heartbeat, not newest-success.
4. Backoff from completion time, `retry_count` pre-incremented, Retry-After
   deferred (structured-error work) — class-based only in the spike.
5. Freshness computed at read; stall horizon is heartbeat-based.
6. Poison → `Failing` in health (surfaced, not silently cleared); read path
   recovers the guard only to keep serving the cache.

## Cross-module / graduation gates (unchanged)

- Ship `fresh: bool` on the wire only after coordinating the serde change with
  ALF's extractor (two consumers).
- A SECOND Oracle pass on the implementation + a live prod smoke before the
  refresher graduates from spike to prod.
- Cold-start "absent provider = no-data (not a signal)" is a router-contract
  assumption — confirm ALF's reader treats an absent provider as unusable-for-
  ranking, not as zero/healthy/negative.
