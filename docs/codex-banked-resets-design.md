# Codex banked resets: auto-consume + relaxed reporting

Status: DRAFT — pending adversarial Oracle pass on the consume-safety path.
Date: 2026-07-14. Ufuk-approved design shape (one knob, module-local, no ALF
change); exhaustion backstop explicitly confirmed.

## Objective

OpenAI grants Codex accounts banked rate-limit reset credits ("Full reset",
30-day expiry from grant). Unused credits vanish at `expires_at`. Two wants:

1. Auto-consume a banked reset instead of letting it expire, and instead of
   ever sitting hard-blocked at the rate limit wall while credits exist.
2. While the feature is armed and credits are available, report codex windows
   to the consumer as unused (`usedPercent: 0`) so ALF's pace model relaxes.
   No ALF-side change: the relaxation is expressed entirely through the
   existing wire shape.

## Verified contract (live-probed 2026-07-14 + CodexBar v0.43.0 source)

All three endpoints share auth: `Authorization: Bearer <tokens.access_token>`,
`ChatGPT-Account-Id: <account_id>` (from `~/.codex/auth.json`, same handle
resolution the codex provider already does).

1. `GET {base}/wham/usage` — already fetched today. Relevant extras:
   `rate_limit.limit_reached: bool`, `rate_limit_reached_type`, and
   `rate_limit_reset_credits: { available_count }` (count only, no expiries).
   Live topology note: the 5h window is gone server-side; `primary_window` is
   now the weekly (604800s) and `secondary_window` is null. Our normalizer is
   already dynamic; no change needed.
2. `GET {base}/wham/rate-limit-reset-credits` — per-credit detail
   (CodexOAuthUsageFetcher.swift:347,420-470,562-608; live HTTP 200):
   `{ credits: [ { id, reset_type: "codex_rate_limits", status: "available" |
   (redeemed/expired states), granted_at, expires_at, redeem_started_at,
   redeemed_at, title, description } ], available_count }`. ISO8601 dates.
   CodexBar sends extra headers `OpenAI-Beta: codex-1`, `originator: Codex
   Desktop`; live probe with them returns 200. We match CodexBar.
3. `POST {base}/wham/rate-limit-reset-credits/consume` with JSON body
   `{ "redeem_request_id": "<uuid4>" }` (gist reference; live-verified by the
   manual consume on 2026-07-12). Response `{ code, windows_reset }` where
   `code ∈ { reset, nothing_to_reset, no_credit, already_redeemed }`.
   `nothing_to_reset` / `no_credit` do NOT burn a credit. `redeem_request_id`
   is a retry-idempotency key for one logical redemption.

CodexBar itself only displays credits (no auto-consume anywhere in its
source); the auto-consume behavior is ours.

## Config

New file, fleet convention: `$XDG_CONFIG_HOME/cortexkit/ck-quota.jsonc`
(default `~/.config/cortexkit/ck-quota.jsonc`).

```jsonc
{
  "codex": {
    // Seconds. 0 (or absent, or missing file) = feature OFF.
    // When > 0: auto-consume a banked reset when the earliest available
    // credit is within this many seconds of its expires_at, or when the
    // account is at its rate limit (exhaustion backstop).
    "auto_use_resets": 86400
  }
}
```

Plumbing: quota-module reads + parses the file at startup using the shared
`subc-jsonc` crate (`jsonc_to_json`, same parser subc-core uses for
subc.jsonc) and injects a plain `QuotaConfig` struct into
`Registry::with_defaults(config)`. quota-core defines the struct but never
touches subc crates (preserves the subc-free rule and standalone build).
Config changes require a module restart (supervised restart is a standard
fleet op); documented in the file template. Unknown fields ignored
(forward-compatible). Malformed file = feature off + one stderr warning,
never a crash (silent-degrade posture).

## Trigger (pure function, unit-tested)

Definitions, evaluated per handle on each refresher fetch with FRESH data
from the same tick:

- `armed` = `auto_use_resets > 0` AND credits fetch succeeded this tick AND
  at least one credit has `status == "available"`.
- `expiry_trigger` = earliest available credit's `expires_at - now <=
  auto_use_resets` AND at least one served window has `used_percent >= 1.0`
  (the floor stops pointless consumes into a fresh window, which the server
  would refuse as `nothing_to_reset` while the credit marches to expiry
  anyway — a credit cannot be harvested into an unused window).
- `exhaustion_trigger` = `rate_limit.limit_reached == true` OR any served
  window `used_percent >= 99.0` (belt for lag between the bool and the
  percents).
- Fire consume iff `armed && (expiry_trigger || exhaustion_trigger)` AND the
  handle is not in consume cooldown.

At most ONE consume attempt per handle per tick. No in-tick retry: the next
tick re-evaluates from fresh state.

## Consume flow (inside the codex provider's fetch_handle)

Sequencing per tick when the feature is configured on:

1. GET usage (as today).
2. GET credits.
3. Evaluate trigger. If it does not fire → step 6.
4. POST consume with a fresh uuid4 `redeem_request_id`. Stamp the handle's
   consume-cooldown timestamp regardless of outcome.
5. Re-GET usage (publish ground truth after a mutation, never guess).
6. Reporting transform (below), return the FetchAttempt.

Timeout budget: the refresher's outer FETCH_DEADLINE is 35s and an overrun
fail-closes the slot (F1 machinery: identity_unverified). Per-request
timeouts are tightened to fit worst case under the deadline: usage GET 12s,
credits GET 5s, consume POST 8s, re-GET 5s = 30s worst case + slack. (The
usage GET's current 30s timeout shrinks to 12s only when the feature is on;
unarmed behavior is unchanged at 30s.)

Consume outcome handling:
- `reset` → success; log windows_reset; re-fetch shows the fresh window.
- `already_redeemed` → treat as success (a previous attempt with this id won).
- `nothing_to_reset` / `no_credit` → credit state was stale; no burn; log;
  cooldown prevents hammering; next tick re-syncs.
- HTTP error / timeout → log; cooldown; this tick reports TRUE numbers
  (fail-closed); next tick re-evaluates. If the POST actually landed
  server-side despite the timeout, the next tick's usage GET shows the fresh
  window and the trigger is naturally false — the credit is spent and used,
  not double-spent (a second attempt would use a NEW uuid, but the trigger
  no longer fires; and if it raced, the server's own `nothing_to_reset`
  refuses a pointless second reset without burning the credit).

Consume cooldown: 30 minutes per handle, held as provider-internal state
(`Mutex<HashMap<handle_id, Instant>>` inside CodexProvider — no slot-machinery
change). Restart loses the cooldown, which is safe: post-restart the first
tick re-fetches real usage + credits, and a just-consumed reset means a fresh
window + one fewer credit, so the trigger does not re-fire.

## Reporting transform (the deliberate relaxation)

When `armed` (fresh successful credits read, >= 1 available) — after any
consume step — the returned `Usage` has every window's `used_percent` set to
`0.0`, keeping the REAL `resets_at` and `window_minutes`. Rationale: with the
exhaustion backstop armed, the account can never sit at the wall while a
credit exists, so "effectively unused" is the honest pressure signal for the
pace model. The raw observed percents are logged to stderr each tick
(`codex[<handle>]: raw primary=49% → reported 0% (resets available: 3)`), so
observability keeps the truth.

Fail-closed enumeration (any of these ⇒ report TRUE percents this tick):
- feature off (knob 0/absent/file missing or malformed)
- credits GET failed this tick (no relaxing on stale credit knowledge)
- zero available credits
- usage GET itself failed (normal degraded path, unchanged)
- consume attempted and errored (report the truth; retry next tick)

The zeroing is provider-local: no model/wire change, no store or scheduler
change, other providers untouched. Multi-account: credits, trigger, cooldown,
and transform are all per-handle (each account has its own credits).

## What we deliberately do NOT do

- No spark / `additional_rate_limits` parsing in this feature (Ufuk ruling;
  parity lane may add it separately).
- No `X-OpenAI-Fedramp` header: CodexBar's credits fetcher does not send it;
  we match CodexBar. (The gist sends it for fedramp accounts; ours are not.)
- No consume when the trigger does not fire — never "top up" opportunistically.
- No persistence of consume history beyond the in-memory cooldown.
- No new wire field (resetCredits count stays module-internal for now).

## Verification plan

- Unit: trigger pure-function truth table (armed/expiry/exhaustion/floor/
  cooldown), credits JSON normalizer against the live-captured shape,
  consume-response code handling (all four codes), reporting transform
  (zeroing + fail-closed cases), config parse (valid/absent/malformed/
  unknown-fields).
- Non-vacuous bar: each fail-closed case asserts TRUE percents are served;
  the double-spend test asserts exactly one POST per tick and cooldown
  suppression; a `nothing_to_reset` storm test asserts no repeated POSTs
  within cooldown.
- Live (gated #[ignore]): credits GET against the real endpoint (read-only).
  NO live auto-consume test — spending a real credit in CI is not acceptable;
  the consume path was manually live-verified 2026-07-12 (4→3, weekly 33→0%,
  code=reset) and the response codes are locked in unit fixtures.
- e2e: config file with knob on + a mock? No — the e2e rides real providers;
  keep e2e as-is (feature off in harness), rely on unit coverage for the
  transform. The skeleton e2e continues to prove the unchanged wire shape.

## Open questions for the Oracle pass

1. Double-spend containment across ticks/restarts — is the
   trigger-naturally-false argument airtight, or does a race exist where two
   ticks both see stale "credit available + limit_reached" and fire twice?
   (Cooldown covers a tick; does anything defeat the cooldown?)
2. The 100%-report + exhaustion-backstop interaction: is there a reachable
   state where we report 0% used but the backstop cannot fire (e.g. credits
   endpoint down while usage fine) for long enough to strand the account at
   the wall while ALF keeps routing? (Design says credits-GET failure ⇒ true
   numbers; verify no gap.)
3. Deadline interaction: consume POST at 8s + slow usage GETs — can the
   4-request worst case overrun FETCH_DEADLINE and fail-close the slot in a
   way that discards the consume outcome? Is the tightened budget sufficient?
4. Multi-handle: two accounts both armed in one tick — any shared-state
   hazard in the provider-internal cooldown map?
