# Codex banked resets: auto-consume + relaxed reporting

Status: SHIPPED and live in production since 2026-07-15, including real credit
redemptions against live accounts. The line below describes the state this note
was written in, not the state of the system.

> **This is a record of a decision, not a description of what exists.** Where it
> and the source disagree, the source is right; the value here is the reasoning,
> which the code does not carry. `crates/quota-core/src/codex_resets.rs` and the
> relaxation transform in `lib.rs` are authoritative for behaviour, and
> `docs/consumer-contract.md` for what reaches the wire.

Original status: DESIGN v2 — adversarial Oracle pass folded (8 findings, 4 critical).
Date: 2026-07-14. Ufuk-approved shape (one knob, module-local, no the router change;
exhaustion backstop confirmed). v1 of this note was ruled UNSAFE by the Oracle
pass; every mechanism below that differs from v1 exists to close a named
finding (F1..F8).

## Objective

OpenAI grants Codex accounts banked rate-limit reset credits ("Full reset",
30-day expiry from grant). Unused credits vanish at `expires_at`. Two wants:

1. Auto-consume a banked reset instead of letting it expire, and instead of
   letting the account sit hard-blocked at the rate-limit wall while credits
   exist.
2. While the feature is armed and credits are verifiably available, report
   codex windows to the consumer as unused (`usedPercent: 0`) so the router's pace
   model relaxes. No the router-side change; the relaxation is expressed entirely
   through the existing wire shape.

The v1 claim "the account can never sit at the wall" is NOT achievable with a
polling refresher; the honest guarantee is BOUNDED: while armed with credits,
a wall is detected within one refresh interval and a consume is attempted on
that same tick, so the account is walled for at most ~2 refresh cycles per
logical exhaustion event, not for days.

## Verified contract (live-probed 2026-07-14 + CodexBar v0.43.0 source)

All three endpoints share auth: `Authorization: Bearer <tokens.access_token>`,
`ChatGPT-Account-Id: <account_id>` (from `~/.codex/auth.json`, same handle
resolution the codex provider already does).

1. `GET {base}/wham/usage` — already fetched today. Relevant extras:
   `rate_limit.limit_reached: bool`, `rate_limit_reached_type`, and
   `rate_limit_reset_credits: { available_count }` (count only, no expiries).
   Live topology note: the 5h window is gone server-side; `primary_window` is
   now the weekly (604800s) and `secondary_window` is null. Our normalizer is
   already dynamic; no change needed for that.
2. `GET {base}/wham/rate-limit-reset-credits` — per-credit detail
   (CodexOAuthUsageFetcher.swift:347,420-470,562-608; live HTTP 200):
   `{ credits: [ { id, reset_type: "codex_rate_limits", status, granted_at,
   expires_at, redeem_started_at, redeemed_at, title, description } ],
   available_count }`. ISO8601 dates. CodexBar sends `OpenAI-Beta: codex-1`
   and `originator: Codex Desktop`; we match. Usable credit = status ==
   "available".
3. `POST {base}/wham/rate-limit-reset-credits/consume` with JSON body
   `{ "redeem_request_id": "<uuid4>" }`. Response `{ code, windows_reset }`,
   `code ∈ { reset, nothing_to_reset, no_credit, already_redeemed }`.
   `nothing_to_reset` / `no_credit` do NOT burn a credit. Retrying the SAME
   `redeem_request_id` is idempotent (`already_redeemed`); requests with
   DIFFERENT ids are independent redemptions — which is exactly why the
   journal below reuses ids across crashes (F3).

CodexBar only displays credits (no auto-consume in its source); auto-consume
is our behavior.

## Config

New file, fleet convention: `$XDG_CONFIG_HOME/cortexkit/ck-quota.jsonc`
(default `~/.config/cortexkit/ck-quota.jsonc`).

```jsonc
{
  "codex": {
    // Seconds. 0 (or absent, or missing/malformed file) = feature OFF.
    // When > 0: auto-consume a banked reset when the earliest available
    // credit is within this many seconds of its expires_at, or when the
    // account is at its rate limit (exhaustion backstop).
    "auto_use_resets": 86400
  }
}
```

Plumbing: quota-module reads + parses the file at startup with the shared
`subc-jsonc` crate (`jsonc_to_json`, the same parser subc-core uses) and
injects a plain `QuotaConfig` struct into `Registry::with_defaults(config)`;
quota-core never touches subc crates. STARTUP-ONLY by design (F8): no
hot-reload races; a supervised restart applies changes; the effective armed
state is logged prominently at startup. Malformed file = off + one stderr
warning. Unknown fields ignored. Value validation: negative or non-numeric =
off; values are clamped to sane arithmetic (max 30 days).

MULTI-HOST CAVEAT (documented, not solved): the redemption journal is
per-host. Arming the same OpenAI account on two machines can double-spend
(each host mints its own request ids). One armed host per account.

## Arming requirements (F2, F5, F7)

The feature ARMS for a given fetch only when ALL hold this tick:
- `auto_use_resets > 0` (config),
- the credential is OAuth with a non-empty `account_id` (an API-key fallback
  has no account identity and cannot be mutation-keyed — never arms),
- the credits GET succeeded THIS tick,
- at least one credit has `status == "available"` and its `expires_at` is
  beyond a safety margin (60s) from now — `now` taken from the credits
  response `Date` header when present, else local clock (clock-skew hedge;
  at the credits' 30-day scale only gross skew matters).

Not armed ⇒ no consume and no relaxed reporting; raw truth flows exactly as
today.

## Redemption journal (F3) — crash-safe idempotency

A tiny durable state file: `$XDG_STATE_HOME/cortexkit/ck-quota/redemptions.json`
(default `~/.local/state/cortexkit/ck-quota/redemptions.json`), written
atomically (temp + rename).

**`CK_QUOTA_STATE_DIR` overrides that directory outright**, ahead of both
`XDG_STATE_HOME` and the default. Recorded here because the journal is what
stops a credit being spent twice, and moving it is indistinguishable from having
none: the module finds an empty directory, writes a fresh journal, and every
pending record from the previous location is unfenced. Nothing on the wire or in
health reports says the file moved.

The variable exists for test isolation — the end-to-end harnesses set it so a
test run cannot spend a real credit — and for a deployment that keeps state off
the default path. Both are legitimate; the hazard is setting it once and
forgetting, or setting it for the module and not for a tool that reads the same
journal.

**The resolved path is printed at startup** (`[ck-quota] codex reset journal:
<path> (N record(s))`), which is the only signal that distinguishes a relocated
journal from a genuinely new one. It is announced rather than checked because
there is nothing to check against: a first run on a new host is legitimately
empty, and any stored marker proving otherwise would itself live in the
directory whose location is in question. An operator comparing the printed path
against the one they expect is the only comparison available from outside.

One record per logical redemption:

```json
{ "account_id": "...", "redeem_request_id": "<uuid4>",
  "created_at": "...", "status": "pending" | "resolved",
  "outcome": "reset" | "nothing_to_reset" | "no_credit" | "already_redeemed" | null }
```

Rules:
- The record is written with `status: pending` BEFORE the POST is sent
  (reserve-then-act). If the process dies between reserve and response, the
  next attempt for that account REUSES the pending record's id — the server's
  `already_redeemed` resolves it without a second spend. A NEW id is never
  minted while a pending record exists for the account.
- A pending record is **never abandoned**. There are two statuses, `pending` and
  `resolved`, and only a server outcome (`reset`, `nothing_to_reset`,
  `no_credit`, `already_redeemed`) moves a record to `resolved`. Past 24h the
  record is logged as pending-old on each inspection and nothing else changes:
  the id stays the only id for its logical redemption, because the alternative
  — retiring it locally — would allow a second id for a redemption that may
  already have landed, which is the double-spend this journal exists to prevent.
  Recovery is by retry, not by expiry: while a pending record exists the account
  retries that same id about once a minute, and the server resolves it.
- Consequence worth stating, because it is the operational failure mode: an
  account whose pending record never resolves is **fenced indefinitely** — it
  will not consume another credit, and it reports raw usage rather than relaxed,
  which is the safe direction but is silent. The only signal is the pending-old
  line on stderr; the journal file is the authority. If an account stops
  relaxing while credits remain, read `redemptions.json` before anything else.
- Resolved records double as the durable spend-rate bound (F3/F4): a new
  redemption for an account is not opened within 30 minutes of the previous
  record's `created_at` (survives restart, unlike the v1 in-memory cooldown).
  EXCEPTION: the 30-min bound applies to consume attempts, and deliberately
  does NOT suppress truth-reporting (see reporting gate) — it can only delay
  the next spend, never keep a lie alive.
- Records for an account are pruned once resolved AND older than 7 days
  (bounded file).
- Journal I/O failures (unwritable dir, corrupt file) ⇒ the feature DISARMS
  for that tick (fail-closed: no reserve ⇒ no POST; no arm ⇒ no zeroing) with
  a stderr warning.

In-process, a per-account mutex map (std Mutex, never held across await;
entries pruned with the journal) makes reserve-check-act atomic against any
overlapping fetch for the same account (F2 — the production scheduler is
sequential per refresh_loop, but nothing structurally prevents overlap and
`SlotStore::admit` explicitly tolerates it; the reserve is the mutation
fence, publication fencing alone cannot un-send a POST).

## Trigger (pure function, unit-tested)

Evaluated per handle per tick, with FRESH same-tick usage + credits:

- `expiry_trigger` = earliest available credit's `expires_at - now <=
  auto_use_resets` AND at least one served window has `used_percent >= 1.0`
  (a credit cannot be harvested into an unused window — the server would
  return `nothing_to_reset` and keep the credit while it marches to expiry).
- `exhaustion_trigger` = `rate_limit.limit_reached == true` OR any served
  window `used_percent >= 99.0`.
- Fire iff `armed && (expiry_trigger || exhaustion_trigger)` AND no pending
  journal record for the account AND the 30-min spend bound allows it AND the
  pre-POST time cutoff allows it (below).

At most one consume attempt per account per tick, enforced by the journal +
account mutex, not by scheduler assumptions.

## Fetch sequencing per tick (F6)

1. Resolve credentials (spawn_blocking, as today).
2. GET usage and GET credits IN PARALLEL (join), timeouts 12s / 8s.
3. Evaluate trigger.
4. If firing AND elapsed-since-attempt-start < 20s (pre-POST cutoff):
   journal-reserve, POST consume (timeout 8s), journal-resolve with the
   response code. No re-GET after the POST — the next tick (60s) is the
   verifier. Worst case ≈ 12s + 8s + slack, comfortably inside the 35s
   FETCH_DEADLINE; a deadline overrun after a landed POST can no longer
   discard an unresolved redemption, because the journal record survives and
   the next tick resolves it with the same id.
5. Return the attempt with RAW usage (never transformed) + the relaxation
   eligibility flag (below).

## Reporting: relaxation as a READ-TIME transform (F1, F4, F5)

THE SLOT ALWAYS STORES RAW USAGE. v1's provider-side zeroing is the critical
flaw the Oracle caught: a transformed 0% entry retained by StaleTransient
stale-serving would be served indefinitely through outages with no truth left
to fall back to.

Mechanism:
- `FetchAttempt` gains `relax_eligible: bool` (default false — no other
  provider sets it). It rides into `ProviderSlot`.
- `Registry::get_usage` zeroes every window's `used_percent` (primary /
  secondary / tertiary / extra) of an entry ONLY when the slot's
  `relax_eligible` is set AND the slot is fresh (`is_fresh(now)`, the
  existing read-time freshness the health path already uses). `resets_at` and
  `window_minutes` stay real.
- A stale or degraded slot therefore serves RAW truth (or degrades) — a
  relaxed 0% can never outlive the freshness horizon of the tick that
  earned it.

`relax_eligible` is set by the codex fetch iff ALL hold:
- armed this tick (all arming requirements above), AND
- fresh usage is BELOW the wall (`!limit_reached` and every window
  `used_percent < 99.0`), AND
- no consume was attempted this tick, AND no pending/unresolved journal
  record exists for the account.

Consequences (deliberate, the honest ones):
- Mutation ticks report TRUE numbers; relaxation resumes only on the next
  fresh below-wall observation (~60-120s). The consumer sees reality during
  the one interval where reality is uncertain.
- A walled account with a credit reports TRUE (100%) numbers while the
  consume + verification cycle runs — never 0% at the wall. `nothing_to_reset`
  loops (server disagreement), `no_credit` surprises, consume timeouts, and
  the last-credit + instant-re-burn race all collapse into this same rule:
  truth until a fresh below-wall pair proves otherwise (F4, F5).
- The v1 "zero while walled, backstop will save us" is gone; the relaxation
  now NEVER asserts more than the machinery has verified.

Fail-closed enumeration (any ⇒ raw truth this read): feature off; not armed
(incl. API-key credential, credits GET failed, zero available credits,
expiring-now credits); walled or ≥99% this tick; consume attempted this tick;
pending journal record; journal I/O failure; slot stale or degraded.

## Observability

stderr per tick when the feature is on: raw percents, credit count, earliest
expiry, armed state, relax_eligible, and every journal transition
(reserve/resolve) with outcome codes. The truth is always in the log even when
the wire is relaxed.

That is the whole of it, and it is worth being blunt about the limit: none of
this reaches the health check or the wire, and the supervisor does not persist a
module's stderr, so on a running host the log is effectively unreadable after
the fact. **The journal file is the only durable record of what this feature
did.** Read it directly — it is small, it is JSON, and it answers "did this
account spend a credit, when, and did the server confirm it" exactly.

## What we deliberately do NOT do

- No spark / `additional_rate_limits` parsing here (Ufuk ruling).
- No `X-OpenAI-Fedramp` header (CodexBar's credits fetcher doesn't send it).
- No opportunistic consume outside the two triggers.
- No hot config reload; no cross-host coordination (documented caveat).
- No new wire field; no model.rs change.

## Verification plan

Unit (all non-vacuous — each asserts the unsafe behavior is ABSENT):
- Trigger truth table: armed/unarmed × expiry/exhaustion/floor × pending ×
  spend-bound × cutoff.
- Journal: reserve-before-POST ordering; crash-simulation (pending record
  present at startup ⇒ same id reused, no new id); abandoned handling;
  prune; corrupt-file ⇒ disarm; 30-min bound enforced across a simulated
  restart.
- Double-spend: overlapping fetches for one account (two tasks racing the
  account mutex) ⇒ exactly one POST; two handles resolving the same
  account_id ⇒ one POST.
- Read-time transform: relax_eligible + fresh ⇒ zeroed; relax_eligible +
  STALE slot ⇒ RAW (the F1 regression); degraded ⇒ raw/error; other
  providers unaffected.
- Reporting gate: walled ⇒ raw even with credits; mutation tick ⇒ raw;
  nothing_to_reset storm ⇒ raw + no POST within spend bound (the F4
  regression); credits-GET failure ⇒ raw.
- Consume response handling: all four codes + HTTP error + timeout.
- Config: valid/absent/malformed/negative/clamp/unknown-fields.
- codex.rs credits normalizer against the live-captured JSON shape.
Live (gated #[ignore]): credits GET read-only against the real endpoint.
NO live auto-consume test — a real credit is not CI ammunition; the consume
path was manually live-verified 2026-07-12 (4→3, weekly 33→0%, code=reset).
e2e: unchanged (feature off in harness); the skeleton continues to prove the
wire shape.

## Build shape

- `crates/quota-core/src/config.rs` (QuotaConfig struct only, subc-free) +
  quota-module startup read via subc-jsonc.
- `crates/quota-core/src/codex_resets.rs`: credits normalizer, trigger pure
  functions, journal (reserve/resolve/prune), consume client.
- `codex.rs`: parallel GETs, trigger evaluation, relax_eligible.
- `provider.rs` + `refresh.rs` + `lib.rs`: thread `relax_eligible`
  (attempt → slot → read-time transform in get_usage).
- Oracle impl-pass after build, before merge (same pipeline as the
  multi-account machinery — its impl pass caught a critical both reviews
  missed).
