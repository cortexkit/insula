# Multi-account per-account fetch — design note

Status: MACHINERY LANDED on master (commit 177bcf8, 2026-07-10, CI-green). The
per-(provider, handle) machinery, all 7 design-Oracle corrections, and all
implementation-Oracle fixes are merged; every provider is migrated and codex emits
a real single-account label. What remains is the VAULT-CONSUMER WIRING (a vault
client + minted handles reading `GetResult.account_id` per handle) that lights up
Ufuk's actual second account — a follow-on build, now unblocked (CKCRED shipped
the field). The two-account live smoke waits on that wiring, not on any external
gate.

History: CKCRED enumeration contract folded, then a design-Oracle pass (3 CRITICAL
+ 3 HIGH, folded into "Oracle-mandated corrections" below), then the build, then an
implementation-Oracle pass that caught F1 (a CRITICAL stale-serve-old-account bug
on timeout) + 5 latent traps, all fixed with non-vacuous regression tests before
merge.

## CKCRED enumeration contract (the load-bearing input, now known)

Corrects the earlier `list-accounts` assumption:
- **No `list-accounts` endpoint.** The vault read model is anonymous capability
  handles. Enumeration is not a vault call — it is the set of handles the module
  already knows (minted offline into its config home, `vault-handles.json`
  pattern). "Enumerate the handles you know, `get` each."
- **Account identity = optional `GetResult.account_id`**, parsed live from the
  served token via a per-provider claim table (openai → `chatgpt_account_id`, the
  SAME claim `codex.rs` reads into `tokens.account_id`). serde-skipped when
  absent. Additive, being built now, lands lockstep with llm-runner.
- **Revision marker = existing `GetResult.record_version`.** Bumps on BOTH
  refresh and replace; replace ALWAYS bumps. Cache `account_id` against
  `record_version`; re-resolve the label on any bump.
- **Handles survive replace**, so a handle is NOT an account identity — never key
  the served account's identity on the handle alone. The handle is the fetch
  unit; the account_id label is versioned by `record_version`.

## Problem

Today the module serves **one usage entry per provider**. Each provider fetcher
reads a single credential from a fixed location (codex from `~/.codex/auth.json`,
grok/claude from the opencode auth store, etc.) and emits one `ProviderUsage`.

Ufuk runs **two OpenAI OAuth accounts** (one in `~/.codex/auth.json`, one in the
vault). The router's account-scoped overlay (ALF's S1, independently deployable)
needs a usage signal **per account** to pace each account's routes on that
account's own remaining quota. The module cannot supply that yet: it emits usage
for whichever single account sits in `~/.codex/auth.json` and nothing for the
other. This note is the module-side change that emits **one labeled entry per
(provider, account)**.

## What already holds (verified from source, do not rebuild)

- **Identity label.** `ProviderUsage.account: Option<String>` already exists
  (`model.rs:114`). codex already populates it with `tokens.account_id`
  (`codex.rs:72,254`) — the ChatGPT-Account-Id claim, a JSON field separate from
  `access_token`, so it survives token refresh and changes only on an account
  swap. This is the agreed identity contract; the join key is that same string.
- **Emission rule (ALF-confirmed).** Label present on every entry once a provider
  has multiple credentials; absent OK for single-credential providers; two
  unlabeled entries for one provider = contract violation. The 28 non-codex
  providers pass `account: None` today and stay single-credential.
- **Freshness (ALF-confirmed, unchanged).** `fresh: bool` per window,
  serde-default; the router owns discount policy. Not part of this change.
- **Refresher base (prod-proven).** The background refresher + cache-only read
  (`refresh.rs`, `store.rs`, `Registry::refresh_tick`) is the base this builds
  on. Its slot store, class-conditional stale-serving, heartbeat liveness, panic
  containment, and per-slot backoff all carry over unchanged in behavior — the
  only structural change is the **slot key**.

## The core change: key the slot by the fetch unit (provider, handle)

The refresher slot is keyed by provider name today
(`SlotStore.slots: HashMap<String, ProviderSlot>`, `store.rs:19`). It becomes
keyed by the **fetch unit = (provider, handle)**, where a handle is a vault
capability handle (or a single implicit local handle for machine-local
providers). The **account_id is a re-resolved label ON the slot, NOT part of the
key** — because handles survive `replace` while the account behind them can
change, so keying on account_id would churn the slot on every account swap and
lose the backoff/stale history that belongs to that fetch unit.

A provider is no longer a single fetch unit; it is a set of `(handle)` fetch
units. Concretely:

1. **Provider trait** gains handle-scoped fetch. Sketch (post-Oracle: `handles()`
   returns `Result` per H5; `fetch_handle` returns an envelope per C2):
   ```
   trait UsageProvider {
       fn name(&self) -> &str;
       // The credential handles this provider fetches under. Result so a config
       // read/parse FAILURE is distinct from an authoritative empty set (H5) —
       // a failed enumeration must NOT be read as "remove all handles".
       fn handles(&self) -> Result<Vec<CredentialHandle>, HandlesError>;  // NEW (config read, NOT a vault/network call)
       // Fetch ONE handle. Returns the credential OBSERVATION (account_id +
       // record_version, captured from the vault get) INDEPENDENTLY of whether
       // the usage call succeeded (C2), so a label change is detected even when
       // the usage fetch itself fails.
       async fn fetch_handle(&self, handle: &CredentialHandle) -> FetchAttempt;  // CHANGED from fetch(&self)
       fn is_cookie_based(&self) -> bool { false }
   }
   struct FetchAttempt {
       observed: Option<AccountObservation>,   // { account_id: Option<String>, record_version }
       usage: Result<Usage, FetchError>,
   }
   ```
   `handles()` is a cheap config read (the known handle set), enumerated for every
   provider on every scheduler turn (H5) — there is NO `list-accounts` vault RPC
   (CKCRED contract). Machine-local providers return one implicit handle and
   behave exactly as today.

2. **Slot key.** `SlotStore` keys on `SlotKey { provider: String, handle:
   HandleId }` — the shipped type is `CredentialHandle`; `HandleId` was a
   working name here and exists nowhere in the source. Plus an
   **incarnation token** per active key (H4) so a stale
   in-flight write for a removed-then-readded handle is fenced out. `ProviderSlot`
   caches `(account_id, record_version)`; the label re-resolves when
   `record_version` changes (any change, not just increase — monotonicity is not
   warranted under restore/re-mint). An OBSERVED account change (different
   account_id) clears the entry + freshness and RESTARTS backoff (C2) — a
   different account is not the same account stale.

3. **Scheduler turn** (replaces "refresh tick enumerates per due provider"):
   each turn (a) enumerates ALL providers' handles outside the store lock,
   retaining last-known-good on any `handles()` error (H5); (b) reconciles the
   active-key set under the lock, assigning incarnations to new keys and reaping
   removed ones (H4); (c) stamps the heartbeat (H6, independent of fetch-unit
   count); (d) selects due units round-robin ACROSS providers (H6, no
   provider-major starvation) up to the concurrency cap; (e) fetches each under
   the existing per-fetch deadline + panic containment; (f) publishes each result
   ONLY if its key is still active with the same incarnation (H4). Stale-serving
   and backoff apply per (provider, handle).

4. **get_usage** groups by provider at read time and enforces the emission rule
   (C1): if not all of a provider's active handles have a resolved `account_id`,
   emit exactly ONE deterministic unlabeled entry (never two unlabeled); only when
   all are resolved, emit one labeled entry per handle, deduplicated by
   `(provider, account_id)`. Cheap values cloned under the lock, sorted OUTSIDE it
   (registry index, then stable handle id). A single-implicit-handle provider with
   account_id absent emits exactly today's shape (label None).

5. **Credential source.** Two classes:
   - **Vault-sourced** (codex/claude/grok/... the OAuth + api-key set): one
     capability handle per credential, minted offline into the module's config
     home (`vault-handles.json` pattern). The module is a pure vault *consumer*;
     it holds the handle set (its own config), and the vault owns the
     account identity behind each handle (`GetResult.account_id`).
   - **Machine-local** (browser-cookie cohort, antigravity, jetbrains): one
     implicit local handle, the local desktop session. `handles()` returns that
     single handle; behavior identical to today. These do NOT gain a vault
     dependency.

## What deliberately does NOT change

- The wire model (`ProviderUsage`/`Usage`/`RateWindow`) — already carries
  `account`. No serde change beyond entries now being per-account.
- The subc module/transport, health path, and the cache-only read guarantee.
- The 28 machine-local / single-credential providers' behavior.
- Freshness, Retry-After (still deferred, still must be clamped when added).

## Oracle-mandated corrections (adversarial pass, folded)

The pass found 3 CRITICAL + 3 HIGH + 1 MEDIUM. All accepted; each reshapes the
sketch above. These are binding on the implementation.

**C1 — read-time emission gate (fixes multi-unlabeled-entry contract violation).**
The sharpest risk, confirmed real: during the interim before `GetResult.account_id`
ships (and any time two handles both resolve to an absent/None label), naive
per-slot assembly emits TWO unlabeled entries for one provider — which the router
drops loudly as a contract violation. Degraded entries also force `account=None`
(`model.rs`), the same trap. FIX: `get_usage` groups by provider at read time and
enforces the emission rule itself, not per-slot:
- If a provider has >1 active handle but NOT all of them have a resolved
  `account_id`, emit exactly ONE deterministic entry (the primary/legacy handle),
  unlabeled — never two unlabeled.
- Only when ALL active handles for a provider carry a resolved `account_id` do we
  emit one labeled entry per handle, deduplicated by `(provider, account_id)`.
- This makes the account_id-absent interim provably safe (collapses to today's
  single entry) and is the invariant the router relies on.

**C2 — fetch-attempt envelope (fixes non-atomic label update, esp. on failure).**
`fetch_handle -> Result<ProviderUsage>` cannot carry the observed
`(account_id, record_version)` when the usage call itself fails, so a
replace-then-timeout would keep serving the OLD account's window under a handle
that now serves a DIFFERENT account (transient-stale-serving of A against new B).
FIX: the fetch returns an internal envelope `{ observed: Option<AccountObservation
{ account_id, record_version }>, usage: Result<Usage, FetchError> }`. The
credential observation is captured from the vault `get` INDEPENDENTLY of whether
the usage fetch succeeds. On an observed account change (account_id differs from
the slot's cached label), the transition CLEARS the old entry + freshness and
RESTARTS backoff — a different account is not the same account stale, so it must
never be served as either the old or (unverified) new window.

**C3 — consistency guarantee weakened to bounded + fail-closed.** The original
"no window where old account_id is served against the new token" is impossible
under a polling fetch model: a replace can land after the vault `get` but before
publication, or while the slot is backed off (up to the 5–15m transient backoff),
so A stays visible until the next observation. FIX: the guarantee is BOUNDED
snapshot consistency (convergence within one observation interval), and routing
must fail CLOSED during convergence (treat a label-in-flux slot as unavailable,
not as its stale identity). Zero-window consistency would require
revision-coupled invalidation shared with the token-serving path — out of scope,
explicitly not promised.

**H4 — generation/incarnation fencing on GC + publication.** GC and the fetch
writeback race: a fetch clones its prev slot, GC removes the handle, then the
in-flight fetch re-inserts it (resurrection); remove+re-add is an ABA. FIX:
reconcile the active handle set BEFORE due-selection under a single scheduler
owner; stamp each slot with an incarnation/attempt token; publish a fetch result
ONLY if the key is still active AND carries the same incarnation. A re-added
handle gets a NEW incarnation, so a stale in-flight write for the old incarnation
is dropped.

**H5 — enumerate every provider every turn; `handles()` returns `Result`.**
"Enumerate per due provider" starves discovery (a zero-handle or fully-backed-off
provider is never due, so a newly added handle waits behind a 15m backoff), and a
bare `Vec` can't tell an authoritative empty set from a transient config
read/parse failure (which would look like "remove all handles" → mass GC). FIX:
`handles() -> Result<Vec<CredentialHandle>>`, enumerated for EVERY provider on
every scheduler turn (outside the store lock), canonicalized/deduplicated;
reconcile only SUCCESSFUL snapshots and retain the last-known-good set on a read
error (never GC on a failed enumeration).

**H6 — round-robin admission + fetch-count-independent heartbeat.** Provider-major
admission lets one provider's 100 slow handles delay the next provider ~7m under
`buffer_unordered(8)` at the 35s deadline, and a sweep-start-only heartbeat goes
stale (>300s) mid-sweep despite progress. FIX: admit due units round-robin ACROSS
providers in bounded scheduler turns; stamp the heartbeat per scheduler turn
(and reconcile handles) independently of the total fetch-unit count, so liveness
and discovery cadence don't degrade as handles grow.

**M7 — get_usage/sleep/health must stop assuming one slot per provider.** All
three do singular name lookups today. FIX: keep an atomically reconciled
active-key snapshot in the store; `get_usage` clones cheap values under the lock
and sorts OUTSIDE it (registry index, then stable handle id); `sleep_until_next_due`
takes the minimum across ALL active slots; `health` gets an explicitly defined
provider-level aggregation (a provider is degraded only if ALL its handles are —
one healthy account should not read as a provider-wide degrade). `handles()` is
NEVER called under the cache mutex.

Preserved base invariant (unchanged): enumeration, sorting, fetches, and
next-slot computation stay OUTSIDE the store guard; only reconciliation,
snapshotting, and fenced publication happen inside. The `std::sync::Mutex` +
`!Send`-guard-across-await-is-a-compile-error protection from the refresher spike
still holds.

## Verification plan

Each test must be non-vacuous (fail if the mechanism were wrong), and the Oracle
findings each get an explicit adversarial test:
- **C1 emission gate:** a provider with 2 handles both resolving account_id=None
  emits exactly ONE unlabeled entry (would FAIL — emit two — without the gate);
  once both resolve, emits two labeled entries deduped by (provider, account_id).
- **C2 label atomicity on failure:** replace handle A→B, then B's usage call
  times out; assert the slot does NOT serve A's old window (neither as A nor B) —
  it clears + restarts backoff. Fails if the envelope/observation path is missing.
- **C3 bounded consistency:** assert a label-in-flux slot reads as unavailable
  (fail-closed), not as its stale identity, during convergence.
- **H4 fencing:** a fetch in flight for handle H, H removed by GC, the fetch's
  late write is DROPPED (no resurrection); remove+re-add gives a new incarnation
  so the old write can't land on it (ABA).
- **H5 enumeration:** a `handles()` error retains the last-known-good set (no mass
  GC on a transient config read failure); a newly added handle is picked up on the
  next turn without waiting behind another handle's backoff.
- **H6 fairness + heartbeat:** one provider with many slow handles does not starve
  another provider's due unit past a bounded turn; the heartbeat stays fresh
  mid-sweep regardless of fetch-unit count.
- **M7 health aggregation:** a provider with one healthy + one degraded handle
  does NOT read as a provider-wide degrade.
- **Base regressions:** the refresher's existing invariants (cache-only read
  never blocks, panic containment, class-conditional stale-serving) still hold.
- Integration: real-daemon supervision still green; get_usage emits two labeled
  codex entries when two handles resolve, one when one does; account_id-absent
  (field not yet shipped) degrades to today's single unlabeled entry.
- Gates: fmt + forced clippy + full suite, per the standing rule.

## Sequencing

Design note (this doc — CKCRED contract + Oracle corrections folded) → implement
on a branch with the C1–M7 fixes → unit + integration green with account_id
absent (safe degrade, the router-contract-critical interim) → CKCRED ships
`GetResult.account_id` → live smoke via driver drain-restart proving two labeled
codex entries + a live `replace` flipping the label → merge. No rush; do not
preempt current lane priorities. The only hard external dependency remaining is
the `account_id` field shipping; everything up to live labeled smoke can proceed
now. The build is Medium (1–2d) per the Oracle estimate — the C1 emission gate,
C2 envelope, H4 fencing, and H6 round-robin scheduler are the substantive net-new
pieces beyond the mechanical slot-key widening.
