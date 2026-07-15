# Vault-consumer wiring: multi-account credentials from cortexkit-credentials

Status: DESIGN v1 — pending adversarial Oracle pass. Date: 2026-07-16.

VAULT STATE (live, CKCRED-confirmed 2026-07-16, receipt-verified): the handle
file at `~/.config/cortexkit/ck-quota/vault-handles.json` EXISTS (0600) with
five freshly minted ck-quota-dedicated handles: `chatgpt:openai` (account
291f5165, v3), `chatgpt:openai:gmail` (account 7b66addd, v1 — the second
OpenAI account, vault-native login), `oauth:anthropic` (v22), `oauth:xai`
(v10), `antigravity:google` (v116 — the ONLY google credential; there is no
separate gemini-cli entry, so the gemini vault lane resolves through the
antigravity-method credential, whose get also serves `project_id`).
`antigravity:google` serves `account_id: None` by design (claim table maps
openai-family only today) — its entries stay unlabeled per the C1 gate, which
is correct for a single-credential provider.
Scope ruling (Ufuk): design for codex (openai) + anthropic + grok (xai) +
gemini/antigravity-oauth in one coherent shape; codex two-account is the
priority pair; masons build per-provider. Banked resets knob stays GLOBAL —
every armed-eligible codex account auto-consumes its own credits.

This is the deferred tail of docs/multi-account-fetch-design.md (read it
first). The fetch machinery (slot per (provider, handle), FetchAttempt
envelope, emission gate, incarnation fencing) is SHIPPED and Oracle-proven;
this note adds ONLY the credential plumbing that lights up real vault handles.

## The upstream contract (verified live + CKCRED-confirmed 2026-07-16)

Vault module: `cortexkit-credentials` (`ck-credentials`, live in prod,
supervised by the same daemon as us). Read surface over a management route:

- `credential.get { handle, min_ttl_ms? }` → `{ "result": ... }` two-level
  decode (broca-proven): success `{ payload: [u8], expires_at_ms: i64|null,
  record_version: u64, account_id?: string, project_id?: string }`; app error
  `{ error: { code, class } }`. `needs_reauth` arrives as a RESPONSE, never an
  Error frame. Optional fields are additive-only.
- Error branching is on the fleet error CLASS tag (transient / permanent /
  auth_required / context_overflow), never on code strings.
- `credential.report_auth_failure { handle, provider_status, record_version }`
  — version-CAS: a stale record_version makes the report a silent no-op, so
  the consumer MUST report the version it was actually served.
- `account_id` is parsed live from the served token (openai →
  ChatGPT-Account-Id claim — the SAME claim codex.rs parses today). Handles
  survive `login --replace`; a re-point to a different account ALWAYS bumps
  `record_version`. RULE: bind (handle → account_id) on record_version and
  re-resolve on any bump — never cache identity against the handle alone.
  (This is exactly the ProviderSlot.observation re-resolution the shipped
  machinery already does; the vault client just supplies the inputs.)
- Handle delivery: CKCRED mints and writes our handle file directly (raw
  handles never transit chat). Admin ops (mint/status) run against the LIVE
  daemon since 2026-07-11 — no maintenance windows.

## Handle file

`~/.config/cortexkit/ck-quota/vault-handles.json` (0600, dedicated
secret-bearing file, broca's exact format and hygiene):

```json
{ "handles": { "chatgpt:openai": "ckh_...", "chatgpt:openai:ufuk": "ckh_...",
               "oauth:anthropic": "ckh_...", "oauth:xai": "ckh_..." } }
```

- Keyed by credential id `<method>:<provider>[:<account>]`. The id is a
  LABEL for operators; the account identity truth is GetResult.account_id.
- Loader (ported from broca's vault_config.rs discipline): refuse symlinks,
  refuse non-regular files, refuse group/other-readable modes (unix), never
  Debug-derive the deserialized shape, log only ids + presence. Env override
  `CK_QUOTA_VAULT_HANDLES_PATH` for tests.
- Absent file = no vault handles = today's behavior exactly (implicit-local
  handles only). Malformed/insecure file = SKIP vault handles for the tick +
  stderr warning, serve implicit-local (degrade-never-wrong); the handles()
  Result-error path (H5) already prevents mass-reaping on read errors.

## Credential id → provider mapping

Static prefix table, applied at enumeration:

| id prefix           | provider  | notes |
|---------------------|-----------|-------|
| `chatgpt:openai`    | codex     | any suffix = another account |
| `oauth:anthropic`   | anthropic | |
| `oauth:xai`         | grok      | |
| `oauth:google` / `gemini:` | gemini | gemini-cli Code-Assist scope |
| `antigravity:google`| antigravity (oauth fallback only) | local probe stays primary |

Unknown prefixes: ignored with a warning (forward-compatible with vault
holdings for other consumers).

## CredentialSource seam (the blessed interface, now concrete)

quota-core stays subc-free. New trait in quota-core:

```rust
pub trait CredentialSource: Send + Sync {
    /// Resolve a vault credential by handle. Returns the opaque payload plus
    /// the identity/version envelope. Implementations must be safe to call
    /// concurrently and must NOT block the executor (async).
    async fn get(&self, handle: &VaultHandleRef) -> Result<VaultCredential, VaultGetError>;
    /// CAS-guarded auth-failure report; fire-and-forget semantics for the
    /// caller (errors logged, never escalated into the fetch result).
    async fn report_auth_failure(&self, handle: &VaultHandleRef, provider_status: u16, record_version: u64);
}

pub struct VaultCredential {
    pub payload: Vec<u8>,          // token bytes, provider-interpreted
    pub expires_at_ms: Option<i64>,
    pub record_version: u64,
    pub account_id: Option<String>,
    pub project_id: Option<String>,
}

pub enum VaultGetError {
    Transient(String),    // → FetchError::Network class (StaleTransient path)
    AuthRequired,         // → FetchError::Unauthorized (degrade, non-transient)
    Permanent(String),    // → FetchError::Unauthorized-class degrade
    NotFound,             // revoked/unknown handle → non-transient degrade
}
```

- The registry holds `Option<Arc<dyn CredentialSource>>` (None = vault
  wiring absent, all providers implicit-local — tests and the unwired
  default). quota-module constructs the real client and passes it into
  `Registry::with_defaults`.
- Class mapping is the SAME transient/non-transient split the refresher
  already classifies on; the vault's class tag routes directly.

## The vault client (quota-module, the only subc-aware part)

A second outbound consumer connection from the ck-quota process to the
daemon, mirroring broca's vault.rs:

- Own TCP connect + HMAC auth + HELLO as a CONSUMER (not module identity),
  `route.open(management, "cortexkit-credentials")`, then `credential.get`
  request/response over the route channel with corr-id matching.
- Wire v2: stamps the route's (channel, epoch) from route.open; reconnects
  re-open the route (fresh epoch) — the stale-epoch drop is the signal.
- Lifecycle: lazy connect on first use; on transport error or route-gone,
  mark connection dead and reconnect on next call with backoff (the
  refresher's own per-slot backoff already paces retries — the client's
  internal backoff only guards connect storms within a tick).
- Concurrency: the refresher fetches up to CONCURRENCY_CAP units in
  parallel; the client multiplexes on one connection with corr-ids (broca
  pattern) OR serializes behind an async Mutex as v1 simplification —
  Oracle input requested. Either way no std lock across await.
- Timeouts: credential.get bounded well inside FETCH_DEADLINE (proposed 10s)
  so a wedged vault can never eat the whole fetch budget; a vault timeout is
  Transient.
- `min_ttl_ms`: request enough validity for the fetch that follows
  (proposed 120_000) so the vault refreshes proactively rather than serving
  a token that dies mid-fetch.

## Provider integration

`handles()` (per provider): implicit-local handle(s) as today PLUS one
`CredentialHandle::Vault { credential_id }` per mapped entry in the handle
file. The handle file is read per enumeration tick (cheap, already the
handles() cadence) — adding/removing vault entries needs no restart
(consistent with H5 last-known-good retention on read errors).

`fetch_handle(Vault{..})`: resolve via CredentialSource, then run the SAME
request path as the local-credential fetch with the served token:
- codex: payload → access token; ChatGPT-Account-Id header from
  GetResult.account_id (fallback: parse the claim from the token as today);
  observation = account_id + record_version. Banked resets: the SAME
  eligibility rules apply per account (OAuth + non-empty account_id);
  journal + spend bound are already account-keyed; the consume POST uses the
  vault-served token. On 401/403 from the provider: report_auth_failure with
  the served record_version, then degrade as today.
- anthropic / grok: payload → bearer token for the existing endpoints; no
  account claim table upstream yet → account_id likely absent → the C1
  emission gate keeps ONE unlabeled entry per provider until the vault
  serves identities for them (correct, ALF-confirmed interim).
- gemini / antigravity-oauth: payload is the full OAuth credential JSON
  (vault owns refresh; we stop refreshing ourselves for vault handles —
  strictly less bespoke OAuth in our repo, CKCRED's charter direction).

Dedup: two handles serving the SAME account_id (e.g. implicit-local
~/.codex/auth.json AND vault chatgpt:openai are both 291f5165 today) is
ALREADY handled: the read-time gate dedups by account preferring
Fresh > StaleTransient > Degraded, and the banked-resets journal fences
consumes per account, so two fetch units cannot double-spend one account's
credits. This exact topology becomes a live regression case.

## What this deliberately does NOT change

- No slot-machinery changes (key, backoff, fencing, emission all shipped).
- No wire/model changes; labels appear exactly as the emission gate already
  specifies.
- The local implicit handles stay (coexistence, dedup at read) — removing
  local reads is a separate decommission decision, not this build.
- Banked resets: no knob change; global arming per Ufuk ruling.

## Verification plan

- Unit: handle-file loader (permissions/symlink/malformed/absent), id→
  provider mapping, class→FetchError mapping, dedup-two-handles-one-account
  (mock CredentialSource), record_version bump → label re-resolution,
  report_auth_failure carries served version (mock captures it).
- e2e (skeleton): a stub vault module served by the in-process daemon
  (tests/common already drives the consumer wire) serving two openai
  credentials with distinct account_ids → assert TWO labeled codex entries
  on the wire; kill the stub → codex slots degrade transient, others
  unaffected.
- Live smoke (the deferred end-goal): both real accounts through the real
  vault + real daemon → two labeled codex entries with correct per-account
  percents; banked-resets log lines show per-account credit counts.
