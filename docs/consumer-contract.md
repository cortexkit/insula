# The `usage.get` consumer contract

What a consumer of this module can rely on, what it must not infer, and where
each answer comes from in the source. Every rule here was settled by checking a
real consumer against the code from both ends, and four of them exist because
doing that found a defect.

## The core rule

**A well-formed response is always a successful poll.** Per-provider failure is
in-band by design: a single provider's failure never fails the array. So *did
the poll succeed* and *is this data fresh* are different questions with
different sources, and conflating them is the mistake every consumer audited so
far had made in some form.

Response assembly is `crates/quota-module/src/main.rs`, array construction is
`Registry::get_usage` in `crates/quota-core/src/lib.rs`, and the entry type
comes from the published `cortexkit-provider-usage` crate.

## The five response shapes

| shape | meaning | consumer action |
|---|---|---|
| entries, some usable | normal, including mid-sweep | use them; never infer from the count |
| entries, all degraded | reached every credential path, none usable | price as known-bad, not as no-signal |
| empty array | no capacity information at all | see below — this is *not* "nothing configured" |
| malformed body | not producible on the success path | transport corruption; report it |
| error frame | the only unavailable-poll case | retry per your own policy |

Results publish incrementally as each provider completes, so a partial array is
normal rather than a symptom. A provider missing from one poll and present in
the next has not necessarily changed state — it may simply have finished.
**Nothing in the array is a delta.**

## An empty array is not "nothing configured"

Degraded entries *are* entries, so a host with zero usable credentials returns a
**full array of degraded entries**. An empty array therefore means one of three
much narrower things:

- **cold** — the refresher has not published a first result yet, so slots exist
  but hold no entry,
- **structural** — providers resolved no credential handles at all, so there are
  no slots, or
- **withheld** — entries exist but every one is suppressed because its account
  identity is unconfirmed. This is deliberate: serving usage under an identity
  that may have changed is the one error this module refuses to risk, so it
  emits nothing until the next fetch settles it.

The third resolves on its own within a refresh cycle and is not a fault.

Which causes are plausible depends on the request. An **unfiltered** query goes
empty only if *every* provider is simultaneously cold, handle-less, or withheld
— so in practice that means cold or structural. A query **filtered to one
provider** goes empty whenever that single provider is in any of the three
states, and withheld is entirely ordinary there. Do not carry an inference from
one shape of request to the other.

Distinguish cold from structural using the health check's metrics —
`withoutHandles` names providers that resolved no handle, and `lastTickAgeSecs`
says whether the refresher has ticked at all. Never infer the difference from
the array itself, and do not flip a staleness flag on elapsed time: a
duration-based guess is the weakest available signal for a question that has a
precise answer one call away.

The failure this prevents is specific. A renderer that treats zero rows as
all-quiet shows its calmest possible screen in the one case where something is
genuinely broken.

## Freshness comes from the producer, never from the poll

Each entry carries its own `fetchedAt`: the wall-clock time of that slot's last
**successful** fetch, unchanged by failed attempts. Anchor per-entry freshness
on it.

Do not stamp poll time. On a transient failure this module keeps serving the
last healthy window, so an entry can legitimately be much older than the
response containing it. Stamping poll time does not merely under-report that
staleness, it **actively resets** it, and the reset is driven by the consumer's
poll cadence rather than the producer's failure duration. Polling more often
makes it strictly worse.

**There is no upper bound on how stale a served entry can be.** The retry
interval is capped — a nominal 60s doubling to 15 minutes — but that caps how
often this module *retries*, not how long it will keep serving. Nothing expires
an entry on age: while failures stay transient, a provider down for six hours
serves a six-hour-old window, retried every fifteen minutes throughout. Do not
size a staleness threshold against the retry cap, and do not treat any duration
as "too old to still be served" — `fetchedAt` is the only thing that says how
old an entry is, and it is authoritative without limit.

The mirror of that mistake is easy to make while fixing it. If you preserve
windows across a response that did not mention them — an empty array, or a
provider absent mid-sweep — **keep aging them on `fetchedAt` anyway**. Freezing
their age because nothing arrived produces data that never grows stale, since
the only thing that could have aged it is the thing that stopped coming.

Both halves of one rule: never reset age on your own activity, and never pause
it on the producer's silence. The decay clock belongs to whoever knew when the
data was true, and it keeps running through the quiet.

`fetchedAt` is per-entry and **never a common instant**. Two accounts of the
same provider routinely differ, because they are separate slots on separate
backoff schedules. Build no snapshot-moment invariant on one response.

**A degraded entry can carry a `fetchedAt` too, and it means something
different.** The timestamp survives the failure that degraded the entry, so it
reports when that provider last succeeded — not when the error was observed, and
not when anything now in the entry was true. On a usable entry `fetchedAt` dates
the content you are holding; on a degraded one it dates content that is no
longer there. Its presence therefore says nothing about whether an entry is
usable: test `usage` for that. It is absent only where a slot has never
succeeded, which is the common case on a host lacking that credential.

## Degraded means "we cannot read it", not "the provider is down"

The set that produces a degraded entry is the non-transient failures, defined by
`classify` in `crates/quota-core/src/refresh.rs` — read it there rather than
trusting this list to stay current:

- HTTP 401 and 403
- no session, unauthorized
- decode failure
- a caught panic in a provider fetch

**429 and every 5xx are transient** and never arrive degraded; they arrive as an
unchanged, aging healthy window. The honest phrasing is *the credential or the
payload is unusable in a way retrying soon will not fix*, which is exactly why
rate limits and server errors are excluded.

A consequence worth stating: only decode failures self-recover. The auth family
needs a human, and a panic recurs on every fetch until the code is fixed. So
"degraded implies a dead credential" is wrong, and so is "most degraded entries
recover on their own".

The error string is prose, not a stable taxonomy. Use it for observability, and
if you need to branch on the class, ask for a machine-readable field rather than
parsing it — a retention policy keyed on prose is how the next seam defect gets
built.

## 100% used does not mean requests are being refused

`usedPercent` is a **capacity reading**, not an enforcement state. A window at
100 says the accounting reached the limit; it does not say the next request is
rejected, and the two genuinely diverge:

- A provider can serve past a reported 100% — soft limits, grace, or an
  allowance the reported figure does not cover.
- A provider can refuse *below* 100%, when the enforced cap differs from the one
  the usage figures are computed against. This is not hypothetical: a plan
  change can leave the console reporting against the new cap while the edge
  still enforces the previous one until the window resets.

Only one upstream in this module reports enforcement directly, and it is
consumed internally rather than published. Everywhere else, a 100% window is
inferred from counts or percentages, so **no field on this wire states that a
provider is currently refusing requests**. Do not read one into `usedPercent`.

### An exhausted window may carry no reset at all

`resetsAt` is optional and is never fabricated. The percent is the load-bearing
field: a window is emitted from the percent alone, and a reset is carried
through only when the upstream reports one. **A window at 100% with no
`resetsAt` is a legal, intended shape**, not a parse failure.

It is also reachable rather than theoretical, and it clusters exactly where it
hurts most. At least one upstream *moves* its reset timestamp out of the
exhausted window's block once that window is spent, so the depleted window
honestly reports no recovery time. Others simply omit it, and dropping such a
window once made a fully-exhausted account read as no signal at all — which is
why they are emitted.

The consequence for any logic that sorts or selects on reset time: decide
explicitly what an absent reset means there, because the answer that falls out
of a comparator by default is rarely the one you want. Treating absent as
"soonest" makes a provider with no evidence of recovery win every retry;
treating it as "never" excludes it from recovery paths permanently rather than
until it resets. Neither is wrong by construction, but it should be a decision.

A live sample will not reveal this. At any given moment most exhausted windows
do carry a reset, so a check against today's wire agrees with the assumption
that one is always present. That agreement is not evidence — the case is
structurally absent from the sample rather than impossible.

What follows for a consumer: treat 100% as *expect no headroom* rather than
*calls will fail*, and let the actual API error be what proves refusal. If you
need enforcement as a fact rather than an inference, ask — it would be an
explicit optional field with three states (refusing / not refusing / not
reported), and absence would have to mean unknown rather than false, since for
most providers here it is genuinely unknown.

## Verdict versus unfinished

A degraded entry is a **verdict**: this module concluded the credential or
payload is unusable. An absent entry is **unfinished**: a sweep is still
running.

Retaining a last-known-good value through an absent entry is correct. Retaining
it through a degraded entry overrides a conclusion already reached. Hard-age
rather than drop if you keep anything, since only some of the class
self-recovers.

One exception, and it matters because it is the *first* thing a consumer sees
from a provider: a transient failure serves the last healthy window stale only
when there **is** one. A slot that has never yet succeeded has nothing to serve,
so a transient failure there degrades like any other — a provider whose very
first fetch times out reports a degraded entry carrying a timeout error, not an
absent one. Such an entry is a *verdict about this attempt*, not about the
credential, and it flips to a window on the first success without any state
change on your side. Concretely: a degraded entry with **no** `fetchedAt` has
never produced data, so there is nothing to retain and nothing to hard-age, and
its error text may name a transient cause. The retention question only arises
for a degraded entry that *does* carry a `fetchedAt`.

That split is exhaustive: on the wire, a degraded entry without `fetchedAt`
means the slot has never succeeded, with no third case. The only thing that
clears the timestamp is an account change, and an account change also puts the
label in flux — which suppresses the entry entirely rather than publishing it
without a timestamp. So there is no "succeeded once, but the timestamp is gone"
entry to account for.

## Health is a separate axis

Module health comes from the health check, not from the array: `failing` only
when the serving store is poisoned, `degraded` only when the refresher heartbeat
is stale, otherwise `ok`. Per-provider degradation deliberately does not flip
module status — most providers lack credentials on any given host, so it would
sit permanently degraded and mean nothing.

Aggregating across a provider's accounts is a consumer decision, and it is not
the same decision for every question. "Can I use this provider at all" takes the
best account. "Is this account exhausted" must stay per-account, or one dead
account walls a provider that has a live sibling.

If you collapse entries at all, the scope of each field decides what is safe to
carry across. Only two are properties of the provider:

| Field | Scope | Why |
|---|---|---|
| `provider` | provider | the module's own name for the upstream |
| `apiProvider` | provider | derived from `provider` at read time, identical on every entry |
| `account` | account | the credential's account id |
| `accountInfo` | account | that account's email, org, plan |
| `source` | account | set per credential lane, so two entries of one provider can differ |
| `fetchedAt` | account | that slot's own last success |
| `savedResets` | account | reset credits are granted to **one** account |
| `usage` (incl. `rawUsedPercent`) | account | measured against that account's own limits |
| `error` | account | that lane's failure, not the provider's |

So a merged per-provider view is safe only if it **selects a whole entry** and
uses its fields together. Taking fields from more than one entry produces a
coherent-looking record describing no actual account — one account's effective
percent beside another's `rawUsedPercent`, or a `savedResets` count belonging to
a third. Nothing on the wire marks the difference, which is why it is stated
here.

**Health counts providers; the array carries accounts.** A provider with several
credentialed accounts contributes several entries to `usage.get` and exactly one
to these buckets, so the array is normally longer than `providersTotal` and the
two are not reconcilable by counting. On this host today: 39 entries against 35
providers. When a provider's accounts disagree, the buckets take the best one —
any fresh account makes it fresh, any stale one makes it stale, and it counts as
degraded only when *every* account is. So a provider showing `fresh` here can
still have a dead account visible in the array, which is the intended reading:
this axis answers whether the module is serving a provider at all, not whether
every account under it is healthy.

The metrics carry a conservation identity that must balance:

```
fresh + stale + len(degraded) + len(withoutHandles) == providersTotal
```

**The four terms are not the same type on the wire.** `fresh` and `stale` are
numbers; `degraded` and `withoutHandles` are arrays of provider names, because
knowing *which* providers are degraded is worth more than knowing how many.
Take their lengths. A language that coerces rather than complaining will happily
evaluate the wrong expression here — in JavaScript, `7 + 0 + [..28 names..] +
[]` is a string, and the comparison is simply false forever, which reads as a
standing imbalance rather than as a bug in the assertion.

Assert it and alert on imbalance. It cannot be tuned into silence and cannot
pass by sampling healthy members. It is **not** a liveness signal, though: it
holds perfectly when every provider is degraded.

**It holds only once the refresher has ticked.** Before the first tick no
provider is in any bucket — slots do not exist yet and nothing is reported as
handle-less, because naming every provider during the seconds after a start
would make the field meaningless. So the sum is zero while `providersTotal` is
not. Gate the assertion on `lastTickAgeSecs` being non-null, or it fires on
every module start and restart, and an alert that cries wolf at boot gets
trained into noise within a week — which costs more than the identity was worth.

`buildCommit` reports the commit the running module was built from. It is
generated by the module and merely relayed by whatever asks, so a stale client
can fail to ask but cannot report a stale answer as current.

## Three kinds of quiet

Where this document is silent, the silence has a category, and they are
different promises:

- **specified** — guaranteed behaviour, above.
- **deliberately unspecified** — a non-guarantee you should rely on. `fetchedAt`
  entries not sharing an instant is the example: build nothing on cross-entry
  alignment.
- **undocumented** — unstable ground, such as error-string prose. Reliance is
  unsafe precisely because nothing marks it.

A field's category changes only by an explicit decision recorded here, never by
observation and never by appearing in a document. **Silence is not permission**:
a consumer's reasonable inference from an underspecified field is
indistinguishable from a guarantee until it breaks, which is exactly how the
retention defect above was built.

## Why this file exists

Every defect found at these seams was invisible from inside either codebase. A
consumer stamping its own clock is locally correct and cannot be falsified
without knowing this module's backoff semantics; this module's own summary of
its degraded set was wrong about a function its author had read many times.

**A contract between two correct modules is not derivable from either one.** It
has to be written down and checked from both ends, or the seam accumulates
exactly the defects neither codebase looks wrong for having. Silence from a
consumer is not confirmation — every defect found this way had been live for
weeks with nobody reporting it.
