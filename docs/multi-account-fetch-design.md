# Multi-account per-account fetch — design note

Status: DRAFT, CKCRED enumeration contract folded in (2026-07-10). Greenlit by
Ufuk (via ALF) to proceed at own pace; acute pain is gone (both OpenAI accounts
replenished), so this is correctness/completeness, not urgent. The design-input
gate is lifted; the remaining integration gate is that `GetResult.account_id` is
still being built vault-side (CKCRED pings when the field lands) — so the
handle-fetch machinery can be built and Oracle-reviewed against the known
contract, but end-to-end labeled emission is only live-verifiable once the field
ships.

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

1. **Provider trait** gains handle-scoped fetch. Sketch:
   ```
   trait UsageProvider {
       fn name(&self) -> &str;
       // The credential handles this provider fetches under. Vault-sourced
       // providers return the handles minted into the module's config home;
       // machine-local providers return one implicit local handle.
       fn handles(&self) -> Vec<CredentialHandle>;          // NEW (config read, NOT a vault/network call)
       // Fetch ONE handle's usage; the returned ProviderUsage.account is set
       // from GetResult.account_id resolved during this fetch.
       async fn fetch_handle(&self, handle: &CredentialHandle) -> Result<ProviderUsage, FetchError>; // CHANGED from fetch(&self)
       fn is_cookie_based(&self) -> bool { false }
   }
   ```
   `handles()` is a cheap config read (the known handle set), safe to call each
   tick — there is NO `list-accounts` vault RPC (CKCRED contract). Machine-local
   providers return a single implicit handle and behave exactly as today.

2. **Slot key.** `SlotStore` keys on `SlotKey { provider: String, handle:
   HandleId }`. `ProviderSlot` gains a cached `(account_id, record_version)` so
   the label is only re-resolved when `record_version` bumps (refresh or
   replace). `due_now` seeding, `insert`, `get`, backoff, and heartbeat are
   otherwise unchanged.

3. **Refresh tick** enumerates handles per due provider (cheap config read), then
   fetches each `(provider, handle)` as its own unit under the existing
   concurrency cap + per-fetch deadline + panic containment. Each `get` returns
   the token plus optional `GetResult.account_id` + `record_version`; the fetcher
   labels `ProviderUsage.account` with `account_id` (falling back to the cached
   label when the field is absent — see the field-not-yet-shipped gate). Each
   result is a whole-slot insert at its `SlotKey`. Stale-serving and backoff
   apply per (provider, handle), so one depleted/failing account never affects the
   other's slot.

4. **get_usage** assembles entries in a stable order (registry provider order,
   then a deterministic handle order) and emits one `ProviderUsage` per resolved
   slot, each carrying its re-resolved `account` label. A provider with one
   implicit handle whose `account_id` is absent emits exactly today's shape
   (label None).

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

## Open questions for the Oracle pass

1. **[RESOLVED — CKCRED]** Enumeration shape. No `list-accounts`; enumerate known
   handles + `get` each; account_id is optional `GetResult.account_id` versioned
   by `record_version`; handles survive replace. Folded into the core change
   above. Remaining dependency is timing only: `GetResult.account_id` is still
   being built vault-side (CKCRED pings on landing) — until then every vault get
   returns account_id absent, so multi-account emission degrades to today's
   single-implicit-handle behavior (safe, label None). The machinery is built and
   Oracle-reviewed against the contract now; labeled emission is smoke-verified
   once the field ships.
2. **Label re-resolution on `record_version` bump.** The slot caches
   `(account_id, record_version)`. On any bump (refresh OR replace) the label is
   re-resolved from the fresh `get`. The Oracle should check: a `replace` that
   swaps the account behind a handle must surface as a NEW account_id on the SAME
   slot key (handle unchanged) — the slot's backoff/stale history correctly
   carries across the swap (it is the same fetch unit), but the emitted label
   flips. Confirm no window where the old account_id is served against the new
   token.
3. **Handle-set changes at runtime** (a handle added/removed from config): slots
   for removed handles must be reaped (not served stale forever). A slot-GC pass
   keyed on the current `handles()` set. This is the main net-new concurrency
   surface. Note it is HANDLE churn, not account churn — narrower than the
   original framing, because an account swap under a stable handle is just a
   label re-resolve, not a slot add/remove.
4. **Anonymous/implicit handle key.** Machine-local providers key on their single
   implicit handle; `account_id` absent → label None, exactly today's shape.
   Trivial, stated so the Oracle checks the degenerate case.

## Verification plan

- Unit: multi-handle slot store (two handles, one fails non-transiently → only
  that slot degrades, the other keeps serving), slot-key uniqueness per handle,
  implicit-handle key, slot GC on handle removal, label re-resolution on
  `record_version` bump (account swap under a stable handle flips the label but
  keeps the slot's backoff history).
- Non-vacuous: a test that would FAIL if a depleted account's window pressured
  the healthy account's slot (the exact routing bug this fixes); a test that
  would FAIL if a `replace`-bumped `record_version` served the stale old
  account_id against the new token.
- Integration: real-daemon supervision still green; get_usage emits two labeled
  codex entries when two handles are present, one when one is; account_id-absent
  (field not yet shipped) degrades to today's single unlabeled entry.
- Gates: fmt + forced clippy + full suite, per the standing rule.

## Sequencing

Design note (this doc, CKCRED contract folded) → adversarial Oracle pass on the
concurrency model (slot-key = (provider, handle) + label re-resolution on
record_version + handle-set GC) → implement on a branch → unit + integration
green with account_id absent (safe degrade) → CKCRED ships `GetResult.account_id`
→ live smoke via driver drain-restart proving two labeled codex entries → merge.
No rush; do not preempt current lane priorities. The only hard external
dependency remaining is the `account_id` field shipping; everything up to live
labeled smoke can proceed now.
