# Vault-consumer wiring: multi-account credentials from cortexkit-credentials

Status: DESIGN v2 — adversarial Oracle pass folded (10 findings: 2 CRITICAL,
6 HIGH, 1 MEDIUM, 1 LOW). Date: 2026-07-16. Scope ruling (Ufuk): codex
two-account is the priority pair; anthropic + grok + gemini follow the same
shape; antigravity OAuth fallback DEFERRED (Oracle-confirmed: its current
implementation is a loopback probe with no OAuth request path to reuse).
Banked resets knob stays GLOBAL — every armed-eligible codex account
auto-consumes its own credits.

> **This is a record of a decision, not a description of the system.** The
> antigravity deferral above has since been reversed and built — see the note in
> the per-provider section. Where this document and the source disagree, the
> source is right; the value here is the reasoning, which the source does not
> carry.

MERGE PHASING (Oracle-sized): merge 1 = vault client + core seam + machinery
fixes + codex two-account (the priority pair, everything below unless marked
later). Merge 2 = anthropic + grok (thin bearer-swap lanes). Merge 3 = gemini
vault lane (split dispatch). Antigravity OAuth: out entirely for now.

This is the deferred tail of docs/multi-account-fetch-design.md (read first).
The fetch machinery (slot per (provider, handle), FetchAttempt envelope,
emission gate C1, incarnation fencing, F1 unverified-failure path) is SHIPPED;
this note adds the credential plumbing AND four machinery corrections the
Oracle found necessary (V1, V2, V4, V6 below).

## Vault state AS OF THE DESIGN (CKCRED-confirmed 2026-07-16, receipt-verified)

**This inventory is a snapshot, not a live reading, and it has already moved.**
The heading used to say "live vault state", which invites the opposite. On
2026-08-12 the file held NINE handles against the five below — four Anthropic
accounts rather than one, plus `kimi-for-coding`, none of which existed when
this was written. Nothing is wrong: handles are minted by the credential vault
whenever an account is added, so this list ages by design.

Read the real inventory instead of this paragraph:
`python3 -c "import json;print(sorted(json.load(open('$HOME/.config/cortexkit/ck-quota/vault-handles.json'))['handles']))"`,
or run `cargo run -q -p quota-module --example vault-lanes`, which enumerates the
configured lanes and fails when one reaches no provider.

The banner at the top of this file says the source wins where the two disagree.
That covers the reasoning; a dated INVENTORY is the case where a reader is most
likely to treat prose as data, because it looks like a fact rather than an
argument.

At design time the file existed (0600) with five ck-quota-dedicated handles: `chatgpt:openai` (account 291f5165, v3),
`chatgpt:openai:gmail` (account 7b66addd, v1 — the second OpenAI account,
vault-native login), `oauth:anthropic` (v22), `oauth:xai` (v10),
`antigravity:google` (v116 — the ONLY google credential; no separate
gemini-cli entry; its get also serves `project_id`; account_id None by
design, claim table maps openai-family only).

## Upstream contract (verified live + CKCRED-confirmed)

- `credential.get { handle, min_ttl_ms? }` → two-level decode (broca-proven):
  Response `{ "result": ... }` where success = `{ payload: [u8],
  expires_at_ms: i64|null, record_version: u64, account_id?, project_id? }`
  and app error = `{ error: { code, class } }`. `needs_reauth` arrives as a
  RESPONSE outcome, never an Error frame — decode the result envelope FIRST.
- Error branching on the fleet error CLASS tag only (transient / permanent /
  auth_required / context_overflow); code strings are diagnostics.
- `credential.report_auth_failure { handle, provider_status, record_version }`
  — version-CAS: stale version = silent no-op; always report the version you
  were SERVED.
- Handles survive `login --replace`; re-point to a different account ALWAYS
  bumps `record_version`. Identity binds to record_version, never the handle.
- Consumer transport: TCP connect + HMAC client auth + route.open — NO
  consumer HELLO (matches broca and our own test driver; the v1 draft's
  "HELLO as a consumer" was wrong and is dropped).
- min_ttl_ms: request 120_000 — ample vs the 35s FETCH_DEADLINE, <20s
  pre-POST cutoff, and 8s POST timeout (Oracle-verified), assuming the vault
  honors the requested TTL. The served token is reused for the WHOLE attempt.

## Handle file

`~/.config/cortexkit/ck-quota/vault-handles.json` (0600, dedicated
secret-bearing file), env override `CK_QUOTA_VAULT_HANDLES_PATH` for tests.
Format: `{ "handles": { "<credential_id>": "ckh_..." } }`.

Loader rules (V7 — descriptor-based, race-free):
- Open ONCE with O_NOFOLLOW (unix), then fstat the OPEN descriptor for
  type/mode checks and read that same descriptor — no check-then-reopen
  TOCTOU. Refuse symlinks, non-regular files, group/other-readable modes.
- REJECT duplicate JSON keys (serde will collapse them silently — parse with
  a duplicate-detecting map). Deduplicate identical raw capabilities listed
  under different ids into ONE fetch unit, warning with ids only.
- The deserialized shape carries bearer secrets: no derived Debug/Display
  anywhere; log ids + presence only.

Enumeration outcomes (V8 — explicit, resolves the v1 contradiction):
- File ABSENT, MALFORMED, SYMLINKED, or INSECURE-MODE → an AUTHORITATIVE
  implicit-only snapshot: vault slots are reconciled AWAY (reaped) this tick,
  implicit-local units continue. (A misconfigured secrets file must not
  leave stale vault units fetching forever.) One stderr warning per change.
- Genuine transient inspection/read failure (EIO etc.) → HandlesError (H5
  path): last-known-good retained, old vault units continue this tick.

## Capability snapshot identity (V2 — CRITICAL fix)

`CredentialHandle::Vault` carries an owned, redacted CAPABILITY SNAPSHOT —
the exact raw `ckh_` value read at enumeration — not just the credential id:

- Slot EQUALITY/HASH includes the raw capability identity (a changed raw
  value under the same file key is a REMOVE + ADD: the old unit gets a new
  incarnation fence, in-flight publishes for the old capability are fenced
  out). Sorting/logging/display use only `credential_id`.
- Any identity cache is keyed by (raw capability identity, record_version) —
  never (credential_id, record_version), which can alias two capabilities
  (record_version is per-credential-record, not globally unique).
- The fetch uses the snapshot it was enumerated with — never re-reads the
  file mid-attempt.

## CredentialSource seam (quota-core, subc-free)

```rust
pub trait CredentialSource: Send + Sync {
    async fn get(&self, capability: &VaultCapability, min_ttl_ms: u64)
        -> Result<VaultCredential, VaultGetError>;
    /// CAS-guarded; fire-and-forget for the caller (errors logged only).
    async fn report_auth_failure(&self, capability: &VaultCapability,
        provider_status: u16, record_version: u64);
}

pub struct VaultCredential {
    pub payload: Vec<u8>,            // token bytes; NEVER stored in FetchAttempt
    pub expires_at_ms: Option<i64>,
    pub record_version: u64,
    pub account_id: Option<String>,  // canonicalized: trimmed, empty→None
    pub project_id: Option<String>,
    // Two more fields shipped later and are absent from this sketch:
    // `email` and `org_name`, both Option<String>, both canonicalized the same
    // way. They carry the account labels the wire's `accountInfo` is built from.
    // `ServedCodexContext` below likewise gained `email`, `org_name`,
    // `is_oauth` and `source`.
}

/// Fixed, secret-free variants (V7): NO upstream text rides these — the
/// class tag decides behavior; sanitized diagnostics go to stderr only,
/// never into FetchError strings that reach the usage wire.
pub enum VaultGetError {
    Transient,        // fleet class: transient
    AuthRequired,     // fleet class: auth_required (incl. needs_reauth)
    Permanent,        // fleet class: permanent (incl. not_found/revoked)
    FailClosed,       // context_overflow or UNKNOWN class → non-transient
}
```

Registry holds `Option<Arc<dyn CredentialSource>>` (None = unwired, all
implicit-local — tests and default).

## Vault-get failure fencing (V1 — CRITICAL fix, machinery change)

A FAILED vault get means the fetch unit's identity is UNVERIFIED this tick —
the handle may have been re-pointed since the last success. Every
`VaultGetError` (including Transient) routes the attempt through the F1
unverified-failure path (`next_slot_after_unverified_failure`): with a prior
observation, fail closed (clear entry, label in flux, restart backoff);
NEVER stale-serve the previous account's window on a vault-get failure.

Mechanism: `FetchAttempt` resolution gains the distinction
`CredentialResolution::Verified | Unverified` (naming per implementation).
Only a fetch whose SAME-TICK credential.get succeeded may take the normal
transient stale-serving path for downstream (provider-endpoint) failures.
Implicit-local units are Verified by construction (local file read is the
identity source). This is a small, targeted refresh.rs/lib.rs change; the
F1 machinery itself already exists.

## Immutable mutation context (V3 — HIGH fix, codex)

One successful `credential.get` per attempt constructs one immutable
`ServedCodexContext { bearer, canonical_account_id, record_version,
capability }`. The observation, usage GET, credits GET, journal account key,
consume POST, and any report_auth_failure are built EXCLUSIVELY from this
context. Never re-get mid-attempt; never mix the context with fallback
account resolution (if account_id is absent from the vault result, fall back
to parsing the token claim — but INTO the context, once, before any request).

Mutation authorization policy (explicit): a successful credential.get IS the
authorization point for that attempt's ≤20s consume window. A `login
--replace` DURING that window does not cancel the POST (nothing in the
contract can revoke an admitted spend); the blast radius is one reset
against the account the token actually belongs to — the journal key and the
bearer come from the same served result, so the journal cannot record a
different account than the one the POST executes under (canonical_account_id
is trimmed/validated once at context construction).

## Emission-gate selection fix (V4 — HIGH fix, machinery change)

`get_usage`'s UNRESOLVED branch (not all handles carry an account label)
currently emits `slots[0]` deterministically — which can surface a cold
degraded vault unit while a Fresh implicit-local unit holds healthy truth.
Fix: in the unresolved branch, select the representative by read-time
service rank — fresh-healthy (is_fresh(read_now)) > stale-healthy >
degraded — tie-broken by explicit local-before-vault priority, then stable
id. Regression: cold NotFound vault handle + Fresh local ⇒ the local entry
is served.

## The vault client (quota-module) — MULTIPLEXED (Oracle verdict)

One shared connection, corr-id multiplexing (serialization loses: 8 admitted
units × 10s vault timeout = 80s head-of-line, blowing the 35s deadline).

- Writer queue + pending map keyed by (route_epoch, corr_id); responses
  matched and dispatched to waiters; cancelled callers DEREGISTER their
  pending entry (drop guard).
- Single-flight state machines, SEPARATE for connection and route:
  `unknown_channel` / route-gone invalidates the ROUTE only (re-open on the
  live connection, fresh epoch); transport death invalidates the CONNECTION.
  Generation counters on both; a late failure from an old generation never
  evicts the replacement (compare-generation before invalidate).
- Locks: internal state behind async-aware primitives, never a std guard
  across await; connect storms bounded by single-flight + a short
  failed-connect cooldown (a few seconds). No deeper client backoff — the
  refresher's per-slot backoff already paces retries (Oracle: cut).
- No client-side token caching, no JWT parsing fallback in the client
  (Oracle: cut — the provider parses claims if it needs them).
- credential.get timeout 10s (inside FETCH_DEADLINE with margin).

## Class → slot behavior mapping (V6)

| fleet class      | VaultGetError | slot path |
|------------------|---------------|-----------|
| transient        | Transient     | F1 unverified (V1) — fail closed w/ prior observation, else degraded-transient backoff |
| auth_required    | AuthRequired  | non-transient degrade (NoSession-class), F1 fencing |
| permanent        | Permanent     | non-transient degrade, F1 fencing |
| context_overflow / unknown | FailClosed | non-transient degrade, F1 fencing |

Provider-endpoint failures AFTER a verified get keep today's classification
(transient stale-serve allowed — identity was verified this tick).
Numeric HTTP status is preserved through the request layers (not stringly
collapsed): every provider 401/403 — usage, credits, AND consume — fires
`report_auth_failure(capability, status, served_record_version)`
fire-and-forget alongside the normal degrade.

## Secrets hygiene (V7)

- Fixed secret-free error variants end-to-end: nothing derived from vault
  payloads or upstream error text reaches `FetchError` strings (which are
  wire-visible in degraded entries). Sanitized diagnostics → stderr only.
- Manual (redacting) Debug for every secret-bearing type, including the
  PRE-EXISTING derives the Oracle flagged: codex `CodexCredentials` /
  `AuthFile` / `AuthTokens`, `ResetRequest`, gemini `OauthCreds`, plus the
  new capability/credential types. Raw handles: no revealing Display.
- Payload bytes never stored in FetchAttempt/slots — used within the fetch,
  dropped.

## Provider integration

`handles()`: implicit-local handle(s) as today PLUS one Vault capability
snapshot per mapped entry. Prefix table: `chatgpt:openai*` → codex;
`oauth:anthropic*` → anthropic; `oauth:xai*` → grok; `antigravity:google` /
`oauth:google*` → gemini (merge 3). Unknown prefixes ignored with warning.

- codex (merge 1): vault lane builds ServedCodexContext; banked resets run
  per account under the existing account-keyed journal + in-process fence
  (Oracle V10 confirmed sufficient — first reserver wins, the other unit
  sees in-flight/pending/spend-bound; a REGISTRY-level two-units-one-account
  one-POST test is added). DOCUMENTED exception to the "walled ≤ ~2 refresh
  cycles" guarantee: after a consume, a genuine second exhaustion within 30
  minutes waits out the account-level spend bound (correct safety posture —
  credits are account-global; never make the bound handle-scoped).
- anthropic / grok (merge 2): vault payload → bearer for the existing
  endpoints; account_id absent upstream today → C1 keeps one unlabeled entry
  (narrowed since: the collapse now needs that handle to be SERVING USAGE — see
  docs/consumer-contract.md, which is authoritative for the emitted shape)
  per provider (ALF-confirmed interim).
- gemini (merge 3, V9): STRICT dispatch by handle source. Implicit-local
  keeps today's file/cache/refresh path. The vault lane parses the served
  credential JSON, uses ONLY the served access token, NEVER consults or
  populates the local token cache, never refreshes locally (vault owns
  refresh). Note: two valid tokens for one Google account share server-side
  rate limits — correlated 429s are expected and classified transient.
- antigravity OAuth fallback: DEFERRED at the time of writing, for want of an
  OAuth request path. **This has since shipped** — `antigravity.rs` now has a
  cloud lane calling `cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota`
  with a vault-served Google credential, used when no local editor process is
  running. The local probe wins when both are healthy. Read that file rather than
  this line for the current shape.

## What this deliberately does NOT change

- Slot machinery beyond the two targeted fixes (V1 fencing hook, V4
  selection): key shape, backoff, incarnation fencing, heartbeat, relax
  transform all unchanged.
- No wire/model changes.
- Implicit-local handles stay (coexistence, dedup at read; the dedup-two-
  handles-one-account topology is live on this host today and becomes a
  regression case). Decommissioning local reads is a later decision.
- Banked resets: no knob change.

## Verification plan

Unit: loader (O_NOFOLLOW/fstat path, duplicate-key rejection, identical-
capability dedup, absent/malformed/insecure → authoritative-empty vs
transient-IO → H5 retained), capability-identity slot equality (same id +
changed raw = new incarnation), V1 fencing (vault-get Transient with prior
observation ⇒ entry cleared + flux, NOT stale-served — the F1-class
regression), class mapping table incl. unknown-class fail-closed,
ServedCodexContext single-source construction (mock asserts journal key ==
context account == bearer's account), report_auth_failure carries served
version on usage AND credits AND consume 401s, V4 selection (cold NotFound
vault + Fresh local ⇒ local served), redacted Debug on secret types
(compile-time trait test or format! assertion).
Client: multiplex under 8 concurrent gets (mock daemon), route-gone
recovery re-opens route on live connection, connection-death recovery,
generation fencing (late old-generation failure doesn't evict), cancelled-
caller deregistration, single-flight connect storm.
Registry-level: two codex units (mock vault + mock local) one account ⇒
exactly one consume POST through the REAL scheduler admission path.
e2e (skeleton): stub vault module on the in-process daemon serving two
openai credentials with distinct account_ids ⇒ TWO labeled codex entries on
the wire; stub killed mid-run ⇒ codex fails closed (no stale old-account
labels), others unaffected.
Live smoke (the end-goal): both real accounts through the real vault ⇒ two
labeled codex entries, per-account percents, per-account banked-resets log
lines; then kill -9 ck-credentials and verify fail-closed labels.
