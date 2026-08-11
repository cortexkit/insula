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
that decided it.

It is a record of REASONING, not the current contract. The shape shipped in
`cortexkit-provider-usage` 0.5.0 and is served by `deepseek` and `minimax`;
`docs/consumer-contract.md` describes what is actually on the wire and wins
wherever the two disagree. What is kept here is why the shape is what it is,
which the source and the contract do not carry.

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

## Money is not an `f64`

The removed seam declared `remaining: f64`. Every real payload examined since
says that is wrong, and they disagree with it in three different ways:

| provider | how an amount arrives |
| --- | --- |
| DeepSeek | `"110.00"` — a decimal **string** |
| MiniMax | `"98.00"` — a decimal **string** |
| Anthropic | `{amount_minor: 0, currency: "USD", exponent: 2}` — integer minor units |

Two of the three deliberately avoid a binary float, and the third is the
standard representation for money precisely because floats cannot hold it: the
nearest `f64` to `0.1` is not `0.1`, so sums drift and a threshold comparison
near zero can go either way. A balance is compared against zero on every routing
decision that reads it.

So amounts are carried as **integer minor units plus an exponent**, matching
Anthropic's shape, and a provider's decimal string is parsed once at the edge
where its own precision is still known. Parsing `"110.00"` into a float and
re-rendering it is the same defect with an extra step.

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

That flag is not defensive bookkeeping for one provider's quirk — the two
providers this ships with answer it differently. DeepSeek reports a **remaining
balance per kind**:

```json
{ "is_available": true,
  "balance_infos": [ { "currency": "CNY", "total_balance": "110.00",
                       "granted_balance": "10.00", "topped_up_balance": "100.00" } ] }
```

There, `granted` and `topped_up` are live remainders and a "spend only granted
credits" policy is exactly expressible. Kilo reports grants per pool and
consumption against their sum, so the same policy is only a ceiling. One field
tells a consumer which of the two it is holding.

## The shape

Derived from the six payloads examined rather than from first principles, and
checked back against each one.

```
spend: [ Pool ]          // per entry, alongside `usage`; absent when unknown

Pool {
  id:        String      // the provider's own name for it, never ours
  label:     String      // display text
  funding:   "granted" | "purchased" | "subscription" | "unknown"
  remaining: Option<Amount>
  total:     Option<Amount>
  basis:     "reported" | "derived" | "unstated"  // how `remaining` was obtained
  spendable: Option<bool>             // provider says this may be drawn on
}

Amount {
  minor:    i64          // integer minor units, never a float
  exponent: u8           // 2 for cents; 0 for whole credits or points
  unit:     String       // "USD", "CNY", or a provider's credit label
}
```

Five decisions in there are load-bearing, and each was forced by a payload:

**`id` is the provider's own name.** MiniMax's wallet separates
`voucher_balance`, `cash_balance` and `credit_balance` without publicly defining
which is a gift. Publishing `voucher` and letting a consumer decide is honest;
publishing `granted` is us inventing the label a spend policy keys on.

**`funding` therefore admits `"unknown"`, and it is not a failure value.** It is
the correct answer for every MiniMax pool today. A three-value enum would push
callers to guess.

**`basis` distinguishes reported from derived**, because DeepSeek reports
`granted_balance` as a live remainder while kilo's is a grant with consumption
tracked against the sum. Same field, different meaning, and only the producer
knows which.

**`unit` is a free string, not a currency enum.** DeepSeek and Anthropic are
denominated in currency; Manus is denominated in points that convert to nothing.
An enum would force points into a currency slot or drop them.

**Both enums tolerate a value they do not recognise**, and this is the one
correction the shape needed after review. The payload crosses a repository
boundary, so producer and consumer versions move independently — and a closed
enum makes the first new variant fail deserialization of the *whole entry*, not
one field. Measured before fixing: an entry with a healthy 42% window and one
pool of an unrecognised funding loses everything, so a new credit-pool kind
would delete an account's quota signal and read as the provider being
unavailable.

`funding` falls back to `unknown`, where the fallback and the meaning already
agree — a kind this consumer cannot name is one it must not spend from. `basis`
falls back to a distinct `unstated` rather than to `derived`, because both are
read conservatively but `derived` is a *claim about how a number was obtained*,
and answering "I do not know" with it would assert a computation nobody
performed. Keeping `unstated` separate also leaves the existence of a new basis
visible instead of absorbed into a real one.

**`spendable` is tri-state and comes from the provider**, not from
`remaining > 0`. Anthropic publishes `is_enabled`, `user_disabled` and
`can_purchase_credits` directly, so a pool can be non-empty and closed. Inferring
it from the amount would report a disabled pool as available headroom.

What it deliberately does not carry: a percentage, a reset, a window length, or
any field a pace calculation reads. A pool with a period is a `RateWindow` and
belongs in `usage`.

## What a consumer does when the provider says nothing

Three fields can be absent or unrecognised, and each has a direction that is
safe. All three are consumer policy rather than wire facts, so they belong in
`consumer-contract.md` once pools ship; they are recorded here because deciding
them late means every consumer has already defaulted them silently.

**`spendable` absent** — the provider does not say whether this pool may be
drawn on. The safe reading differs by what the reader is about to do, and both
readings are correct at once:

| reader | treat absent as | why |
|---|---|---|
| a router about to spend | **not spendable** | spending from a closed pool costs money and cannot be undone |
| a UI showing the account | **unknown** | hiding a real pool misleads a person who could act on it |

The asymmetry is the whole argument: a display that omits a live pool is a
smaller harm than a router that spends from a closed one. A single global
default would have to pick one and be wrong for the other reader.

**`funding` is `unknown`** — either the provider named a pool without defining
it, or the producer used a kind this consumer predates. Do not spend from it
under a policy that names a specific funding. "Only granted credits" must mean
*only pools stated as granted*, never *everything not stated as purchased*.

**`basis` is `unstated`** — treat `remaining` as a ceiling. Never as an exact
figure, and never as a reason to drop the pool.

The shape of all three: **absence is not a value**, and the correct reading of
absence depends on what the reader is about to do with it.

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
  `GET /user/balance`, documented, and the only examined provider that reports
  granted and purchased remainders separately.

**A caveat that belongs with MiniMax specifically, because the motivating user
wants free credits.** Its wallet lives at `GET /account/query_balance` and is
**not in the public documentation** — it is used by MiniMax's own CLI, which is
where the shape below comes from:

```json
{ "available_amount": "98.00", "cash_balance": "0.00",
  "voucher_balance": "98.00", "credit_balance": "0.00", "owed_amount": "0.00" }
```

`voucher_balance` is the plausible home for granted credits, and MiniMax does
not publicly define it that way. So reading "voucher means free" is an
inference, and acting on it spends real money if it is wrong. Publish the pools
MiniMax names, rather than relabelling one of them `granted` on a guess — the
naming is the part we would be inventing, and it is the part a spend policy
would key on.

Those two cover both structural shapes. If the shape is wrong, it is discovered
with two fetchers written instead of seventeen. Roughly a dozen other providers
already parse balance-shaped data and deliberately discard it — `neuralwatt`,
`zenmux` and `sub2api` carry explicit skip notes — and they follow once the
shape is proven.

## Rendering

A balance is shown as a separate line rather than a bar. A bar implies a period,
and the absence of one is the defining property.
