# The Balance axis

Every signal this module publishes today has the same shape: how much of a
period has been consumed, and when it resets. That is `RateWindow`, and it is
what makes pacing possible — a percentage plus a horizon answers "will I run out
before this refills".

A **balance** is a different fact: an amount with no period. NeuralWatt reports
`credits_remaining_usd`. Sub2API reports `balance` with a currency. StepFun
reports a credit pool. There is no window, no reset, and no percentage — only a
quantity that shrinks when spent and grows when paid for.

This document records what was decided about publishing those, and the findings
that decided it. It is a record of a DECISION, not a description of shipped
behaviour: nothing here is implemented yet.

## Why it was deferred, and why that changed

A reserved `Balance` seam shipped in June 2026 and was never populated by any
provider. It was removed during the shared-crate extraction as dead weight.

The deferral was correct at the time and its stated reason has since expired.
The only consumer then was a router that paces on windows, and a balance has no
pace, so publishing it would have handed the only consumer something it could
not use. There are now consumers that can use it, and the request has arrived
three separate times from one reporter (issue #2 F-D, issue #1 FEAT-3 for
MiniMax's credit pool, issue #1 FEAT-4 for DeepSeek where a balance is the only
signal that exists).

The concrete defect it fixes: MiniMax debits a prepaid pool once its plan
windows exhaust, so a depleted window plus a live pool is a **working provider**.
We publish 100% used, a consumer correctly reads "unusable", and both of us are
wrong about an account that would have served the request.

## The rule that survives everything else

**A balance is never expressed as a window, and never as a percentage.**

Giving `remaining: 42.5` a synthetic `resetsAt` would poison every consumer's
pace arithmetic. The subtler version matters as much: a balance must never reach
the field a pace calculation reads, because a consumer that finds "$42
remaining" where it expects headroom will pace into a bill.

Quota and money fail in opposite directions, which is the whole reason they are
kept apart:

| | over-consuming quota | over-consuming a balance |
|---|---|---|
| result | throttled | billed |
| recovery | wait | pay |

Nothing in a routing loop can undo the second.

## What the vendors already do

Anthropic answered the structural question before we asked it. Its
`/api/oauth/usage` response keeps three kinds of thing apart:

- rate windows (`five_hour`, `seven_day`, …) — utilization and `resets_at`
- `spend` — `{ used: {amount_minor, currency, exponent}, limit, balance, cap,
  auto_reload, can_purchase_credits, enabled }`
- `extra_usage` — `{ is_enabled, monthly_limit, used_credits, utilization,
  currency, user_disabled, credits_ever_enabled }`

Money lives in its own structure with its own enable flag, its own limit, and
its own balance. That is strong evidence for a separate structure rather than a
funding tag on `RateWindow`, and it comes from the vendor whose product raised
the question.

It also supplies something better than an inferred plan type: `enabled`,
`user_disabled` and `can_purchase_credits` state directly whether an account
*may* spend money. A consumer choosing between "never spend" and "spend when
quota is gone" can read that rather than guess from a plan label.

**The counter-example, recorded because it will be met eventually.** The same
response carries `nimbus_quill`, shaped `{utilization, resets_at, limit_dollars,
used_dollars, remaining_dollars}` — a window denominated in money, with both a
reset and a remaining balance. Every field was null on the account inspected, so
its semantics are unknown and nothing here is built on it. But a design assuming
"windows are percentages, balances are money" will meet a hybrid.

## Pools are plural

A single `{remaining, total, unit}` cannot express "$9.50 granted and $40
purchased", and that distinction is the point for a user who wants to spend
granted credits without spending money.

Providers state it. Kilo splits `currentPeriodBaseCreditsUsd` from
`currentPeriodBonusCreditsUsd`. Manus carries `freeCredits`, `periodicCredits`,
`addonCredits`, `eventCredits` and `proMonthlyCredits`. Both are currently
flattened into one figure by our own normalizers — kilo's adds base and bonus
before publishing.

So the shape is a **list of pools**, each carrying its own funding kind, not one
balance per account.

## The asymmetry that limits what a consumer can promise

Providers report grants per pool and consumption **against the combined total**.
Kilo again:

```
currentPeriodBaseCreditsUsd    40.0   granted, per pool
currentPeriodBonusCreditsUsd    9.5   granted, per pool
currentPeriodUsageUsd          12.0   spent, against the SUM
```

After $12 of spend, whether the granted $9.50 is gone or untouched is not
derivable. The provider does not say.

Consequences, in descending order of how much a consumer can rely on them:

1. **"Never spend money on this provider"** — reliable. Read the provider's own
   enable flags, or route only to accounts with no purchasable pool.
2. **"Stop when the pool is empty"** — reliable. `remaining <= 0` is a hard
   fact, and for a balance-only provider it is a genuine *enforcement* signal
   rather than a capacity reading. This is the one place this wire can state
   that a provider will refuse.
3. **"Prefer free credits"** — best effort only. Expressible as a spend ceiling
   at the granted amount, and sound only if the provider debits granted pools
   first. That ordering is undocumented for every provider examined.
4. **"Spend from this pool and not that one"** — not expressible at any layer.
   The provider chooses which pool it debits.

Because of (1) through (4), each published pool states whether its `remaining`
is **known** or **derived from a combined total**, so a consumer can tell
"granted 9.5, remaining unknown" from "granted 9.5, remaining 4.5" instead of
assuming the second.

## Where the policy lives

Publishing a balance is not spending one. The decision of *when* money may be
spent belongs beside the thing that spends it, which is the router.

Three routing policies were described, defaulting to the first:

- only quota, never credits
- credits allowed when no other provider has quota for the request
- credits merged with quota, so pressure is computed across both

The third is the interesting one, because it needs a merged pressure figure and
somebody has to compute it. That belongs to the router too: it needs `remaining`
and `total`, both published, and the merge policy then sits beside the spend
decision rather than being frozen for every consumer by the producer.

**The precedent that argues the other way, and why it does not apply.** Codex
banked resets already merge at this module: a relaxed `usedPercent: 0` is
published beside a real `rawUsedPercent`, gated by `auto_use_resets` in this
module's own config. That is exactly the third policy's shape. It is honest
*only because this module performs the consume that justifies it* — the same
process that claims the headroom is the one that redeems the credit. With a
balance, this module spends nothing. A producer that merges a resource it cannot
spend asserts headroom it has no authority over.

Verifying debit order is likewise the router's, handled best-effort from the
data published here.

## Scope

Land the shape with two providers rather than seventeen:

- **MiniMax** — the hybrid case: rate windows plus a backstop pool, and the one
  with a user waiting. Note its rate-limit endpoint carries no pool at all, so
  this is a fetch to add rather than a parse to fix.
- **DeepSeek** — the pure case: no windows exist, the balance is the signal.

Those two cover both structural shapes. If the shape is wrong, it is discovered
with two fetchers written instead of seventeen. Roughly a dozen other providers
already parse balance-shaped data and deliberately discard it — `neuralwatt`,
`zenmux` and `sub2api` carry explicit skip notes — and they follow once the
shape is proven.

## Rendering

A balance is shown as a separate line rather than a bar. A bar implies a period,
and the absence of one is the defining property.
