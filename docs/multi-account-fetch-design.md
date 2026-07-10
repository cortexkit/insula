# Multi-account per-account fetch — design note

Status: DRAFT. Greenlit by Ufuk (via ALF) to proceed at own pace; acute pain is
gone (both OpenAI accounts replenished), so this is correctness/completeness, not
urgent. **Oracle pass and implementation are GATED on CKCRED's `list-accounts`
answer** (see Open Questions) — the enumeration shape is a load-bearing design
input and this note carries a placeholder for it until then.

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

## The core change: widen the slot key

Everything follows from one change: the refresher slot is keyed by provider name
today (`SlotStore.slots: HashMap<String, ProviderSlot>`, `store.rs:19`). It
becomes keyed by **(provider, account_id)**.

A provider is no longer a single fetch unit; it is a set of `(account)` fetch
units. Concretely:

1. **Provider trait** gains account enumeration + per-account fetch. Sketch:
   ```
   trait UsageProvider {
       fn name(&self) -> &str;
       // Which accounts this provider currently has credentials for. Machine-
       // local single-credential providers return one anonymous account.
       async fn list_accounts(&self) -> Vec<AccountRef>;   // NEW
       // Fetch ONE account's usage.
       async fn fetch_account(&self, account: &AccountRef) -> Result<ProviderUsage, FetchError>; // CHANGED from fetch(&self)
       fn is_cookie_based(&self) -> bool { false }
   }
   ```
   where `AccountRef { account_id: Option<String>, handle: CredentialHandle }`.
   `account_id` is the label string (None for anonymous single-credential
   providers); `handle` is what the fetcher uses to obtain the token.

2. **Slot key.** `SlotStore` keys on `SlotKey { provider: String, account_id:
   Option<String> }`. `due_now` seeding, `insert`, `get`, backoff, and heartbeat
   are otherwise unchanged. `ProviderSlot` is unchanged.

3. **Refresh tick** enumerates accounts per due provider (cheap, from the vault
   handle list / local file presence — NOT a network call; see Open Q on cost),
   then fetches each `(provider, account)` as its own unit under the existing
   concurrency cap + per-fetch deadline + panic containment. Each result is a
   whole-slot insert at its `SlotKey`. Class-conditional stale-serving and
   backoff apply per (provider, account), so one depleted/failing account never
   affects the other's slot.

4. **get_usage** assembles entries in a stable order (registry provider order,
   then a deterministic account order — e.g. account_id sort) and emits one
   `ProviderUsage` per resolved slot, each carrying its `account` label. A
   provider with one anonymous account emits exactly today's shape (label None).

5. **Credential source.** Two classes, already understood:
   - **Vault-sourced** (codex/claude/grok/... the OAuth + api-key set): accounts
     come from the vault. One capability handle per `credential_id`, minted
     offline into the module's config home (llm-runner `vault-handles.json`
     pattern). The module is a pure vault *consumer*; it does not mint or hold
     account-set truth — the vault is the source of the account set.
   - **Machine-local** (browser-cookie cohort, antigravity, jetbrains): one
     account, the local desktop session. `list_accounts` returns a single
     anonymous account; behavior identical to today. These do NOT gain a vault
     dependency.

## What deliberately does NOT change

- The wire model (`ProviderUsage`/`Usage`/`RateWindow`) — already carries
  `account`. No serde change beyond entries now being per-account.
- The subc module/transport, health path, and the cache-only read guarantee.
- The 28 machine-local / single-credential providers' behavior.
- Freshness, Retry-After (still deferred, still must be clamped when added).

## Open questions — GATE the Oracle pass on these

1. **[CKCRED, load-bearing] `list-accounts` shape.** What exactly does vault
   `list-accounts` return per credential? The design needs: (a) it exposes the
   same `ChatGPT-Account-Id` string codex labels with, as the join key; (b) the
   per-account credential handle to pass to `fetch_account`; (c) whether
   enumeration is a local handle-file read (cheap, callable each tick) or a
   vault RPC (then the account list must be cached and refreshed on its own
   cadence, not per tick, to preserve the cheap-lock/no-await-under-lock
   invariant). This determines `AccountRef` and the enumeration placement.
   Relayed thread: ses_100a028aaffeVG0zdK3qwcEXf8.
2. **Anonymous-account slot key.** For machine-local providers, `account_id` is
   None. Confirm `SlotKey { provider, None }` is unambiguous (one such slot per
   provider) — trivially yes, but stated so the Oracle checks it.
3. **Account set changes at runtime** (login --replace, add/remove account):
   slots for vanished accounts must be reaped (not served stale forever). Design
   a slot-GC pass keyed on the current enumerated set. This is new state the
   refresher must own; it is the main net-new concurrency surface and the reason
   an Oracle pass is warranted beyond the slot-key widening.

## Verification plan (once unblocked)

- Unit: multi-account slot store (two accounts, one fails non-transiently → only
  that slot degrades, the other keeps serving), slot-key uniqueness, anonymous
  key, slot GC on account removal.
- Non-vacuous: a test that would FAIL if a depleted account's window pressured
  the healthy account's slot (the exact routing bug this fixes).
- Integration: real-daemon supervision still green; get_usage emits two labeled
  codex entries when two handles are present, one when one is.
- Gates: fmt + forced clippy + full suite, per the standing rule.

## Sequencing

Design note (this doc) now → CKCRED answers `list-accounts` → fold the answer +
resolve the open questions → adversarial Oracle pass on the concurrency model
(slot-key widening + slot GC + enumeration placement) → implement on a branch →
live smoke via driver drain-restart → merge. No rush; do not preempt current
lane priorities.
