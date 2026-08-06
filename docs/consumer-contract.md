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

### A partial array can look settled for seconds

The array does not fill smoothly. Providers whose credential is simply absent
fail in microseconds, so they arrive almost immediately; the ones that reach a
network settle much later. Sampling a real start every 5ms, the array reached 31
of 35 entries and **stayed there, unchanged, for about six seconds** before the
last four arrived.

Two polls seconds apart therefore return identical results while the sweep is
still running, and a consumer treating stability as completeness would take that
for a finished answer. The four stragglers were the providers holding live
credentials — the slowest to arrive are the ones doing real network work, which
is exactly the data worth waiting for.

`pending` is the signal that resolves it: it held at 4 for the whole plateau and
dropped to 0 as the last entries landed. Read it before concluding an array is
complete, and never infer completeness from the count being stable across polls.

## An empty array is not "nothing configured"

Degraded entries *are* entries, so a host with zero usable credentials returns a
**full array of degraded entries**. An empty array therefore means one of three
much narrower things:

- **cold** — the refresher has not published a first result yet, so slots exist
  but hold no entry. On an unfiltered query this is briefer than it sounds:
  measured on a real start, the array was empty for **8 milliseconds** before the
  first credential-absent providers landed. Treat it as a real state to handle,
  not one to design around,
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
`withoutHandles` names providers that resolved no handle, `pending` counts those
whose first fetch has not completed, and `lastTickAgeSecs` says whether the
refresher has ticked at all. Never infer the difference from the array itself,
and do not flip a staleness flag on elapsed time: a duration-based guess is the
weakest available signal for a question that has a precise answer one call away.

Note that cold is not the same as "before the first tick". The refresher admits
a bounded number of fetch units per turn, so a provider can still be cold after
several ticks have happened — `lastTickAgeSecs` being non-null says the refresher
is alive, not that every provider has been tried. `pending` is the count that
answers the per-provider question.

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
- no session, credential unusable, unauthorized
- no quota reported
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

## Not every degraded entry is a problem

The list above answers *how* a fetch failed. It does not answer the question a
reader of your output actually has, which is **whether anything is wrong**, and
those come apart sharply:

- A provider nobody configured on this host fails every fetch, forever, and that
  is the correct steady state. Nothing is broken and nothing is fixable.
- A provider whose credential worked yesterday and was rejected this morning
  fails identically on the wire, and is worth someone's attention.
- A provider whose credential is fine but whose account genuinely has no quota
  to report is a third thing again: not absent, not broken.

On the host these docs are written from, that split is 24 / 3 / 1 — so an
undifferentiated count of degraded entries is dominated by the case that can
never mean anything, and a real breakage moves it by one.

`errorClass` is the machine-readable answer (`credential_absent`,
`credential_unusable`, `credential_rejected`, `no_quota_reported`,
`upstream_failed`, `decode_failed`). It is merged into
`cortexkit-provider-usage` and **not yet on the wire** — this section describes
where it will appear so that consumers stop deriving the distinction from prose
in the meantime. When it arrives it is additive and absent on healthy entries,
and an unrecognised class must render as a degraded entry with an unknown
reason: never dropped, never folded into an existing bucket.

Until then, the distinction exists in the taxonomy but not on the wire. If you
need it now, ask rather than parsing the message.

The error string is prose, not a stable taxonomy. Use it for observability, and
if you need to branch on the class, ask for a machine-readable field rather than
parsing it — a retention policy keyed on prose is how the next seam defect gets
built. **The prose is not stable across releases and has already changed**:
failures that once read `no session: …` now read `credential unusable: …` or
`no quota reported: …` where that is what actually happened. Anything storing
these strings will see a discontinuity at that release, and anything matching on
them was relying on a promise this contract never made.

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

## The window slots are positions, not a ranking

A `usage` object carries up to three named slots — `primary`, `secondary`,
`tertiary` — plus `extraRateWindows`, a list of named windows for anything that
does not fit three. It is tempting to read `primary` as *the* number for a
provider. It is not.

**`primary` is the provider's shortest window, not its most constrained one.**
For most upstreams here the slots are filled in cadence order: the session or
five-hour window lands in `primary`, the weekly in `secondary`. Nothing checks
that `primary` is the one closest to its limit, and usually it is not — a short
window refills quickly and reads low most of the time, while the long window
accumulates.

On this host as this was written, three of the four entries carrying a `primary`
had a **more used** secondary: one showed `primary 0%` over five hours beside a
weekly at 36.5%. A consumer reading only `primary` would have seen an idle
provider that was in fact a third of the way through its week. This is not a
rare alignment; it is the normal state of a short window.

So for "how much headroom does this account have", take the **maximum**
`usedPercent` across every slot and every entry in `extraRateWindows`, or apply
whatever policy you want deliberately. Do not let slot position stand in for a
judgement about which limit binds.

The slots are also not stable across providers. One provider's `primary` may be
a five-hour window and another's a monthly one, so `primary` is not comparable
between providers. Read `windowMinutes` — the window's length — when the cadence
matters, rather than inferring it from the slot.

### The other window fields

`windowMinutes` is the window's length, and it is optional. It is set when the
upstream states a cadence or the field name implies one; it is absent when the
upstream reports usage without saying over what period. Absent means unknown —
not "unlimited", and not a default worth inventing.

`usedCount` and `totalCount` are the absolute figures behind the percentage,
carried only where the upstream supplies them, which is a small minority of
providers. They exist because a percentage alone cannot distinguish a plan whose
cap changed from one whose usage did. Do not compute one from the other and the
percent: the percent can be reported directly by the upstream against a cap that
differs from the one you would divide by.

**The two are not equally independent, and the difference matters if you plan to
cross-check them.** `totalCount` is the plan's cap, which the upstream states
directly. `usedCount` may be *recovered* from the percentage and that cap where
the upstream publishes no absolute figure of its own — a faithful recovery of
the number the upstream divided, to the precision at which it transmitted the
percentage, but not an independent measurement. So `usedCount / totalCount`
returns the reported percentage by construction: agreement between them confirms
nothing, and disagreement would indicate a producer defect rather than anything
about the account. The independent signal is `totalCount` alone: watching it
change is how a cap change becomes visible, which is what these fields were
added for.

Each entry in `extraRateWindows` carries an `id` (a stable identifier), a `title`
(human-facing text), and a `window` — all three optional. Match on `id`; render
`title`; and handle an entry whose `window` is absent, which is how a provider
names a limit it could not read a figure for. A window whose meaning has no slot
— a per-model pool, a scoped weekly — lives here rather than being forced into
one, so a consumer ignoring `extraRateWindows` silently ignores real limits.

`id` is not unique across providers and is not drawn from any shared vocabulary:
one provider's ids are model names, another's are its own scope labels. Key on
`(provider, id)`, never on `id` alone.

**Nor is it guaranteed stable within a provider.** Where a provider reads one
account through more than one lane, the lanes can publish different ids for the
same limit, and which lane answers is decided per fetch. `antigravity` does this
today: its cloud lane names pools (`Gemini Models`), while its local lane passes
through the upstream's own bucket identifiers (`gemini-weekly`, `3p-5h`). The
same account switches between those shapes depending on whether a local process
happens to be running.

So a consumer matching ids for a provider with more than one lane must enumerate
every lane's form. Enumerate the ids you positively want rather than the ones you
mean to skip: a skip-list fails **open** on every form nobody thought of, and
those are exactly the forms that share no vocabulary with the ones that were.

Provenance differs by lane and decides what a producer can promise. Ids this
module constructs are stable because it controls them, and a change is
announced. Ids passed through from an upstream are not: a change there is
observed, not planned, so it arrives on the wire before anyone can warn you.
`title` is weaker than both — render it, never match on it, and note that a title
can legitimately carry mutable detail such as a count of the models in a pool.

### Fields on the account, not the window

`accountInfo` carries `email`, `orgName` and `planType`, each optional and each
present only when the upstream identifies the account that way. They are
display- and grouping-only: nothing in this module derives behaviour from them,
and `planType` in particular is the upstream's own label rather than a
normalised vocabulary, so it is not comparable across providers.

`savedResets` describes banked quota-reset credits, which exactly one upstream
grants. `availableCount` is how many are held, `soonestExpiresAt` when the next
one lapses, and `credits` lists each with its own `expiresAt`. They are granted
to **one account**, never to a provider — see the field-scope table below.

## Which field to key on

Two fields name the provider and they are **different namespaces**:

- `provider` is this module's own name for the upstream, inherited from the
  reference implementation the adapters were ported from.
- `apiProvider` is the canonical slug, and is present only where a canonical
  counterpart exists — it is `null` for roughly a third of entries.

Decide which namespace your lookup key is in and key on that field. A key from
one namespace matched against the other does not fail loudly; it simply matches
nothing, and an entry that never matches is indistinguishable from a provider
that published no signal.

**A wrong guess can land on a real entry rather than on nothing**, which is the
case worth guarding against, because it produces a confident wrong answer. Two
entries here carry names close enough to be mapped into each other by hand:

| `provider` | `apiProvider` | what it is |
|---|---|---|
| `alibaba` | `alibaba-coding-plan` | the coding-plan subscription |
| `qwen-cloud` | `alibaba-token-plan` | the token-plan console |

They are separate upstreams with separate credentials, and on a host holding one
and not the other, a lookup landing on the wrong one reads as a permanently
degraded provider — a steady "no signal" for a provider that is in fact serving
full windows under the other name. Prefer the published `apiProvider` over any
local table: a hand-rolled map is a copy of this one that nothing keeps in step.

## Sizing a staleness threshold

A threshold below the refresh cadence reports a healthy provider as stale for
most of every cycle, and does it to **every** provider at once, so it looks like
a producer-wide outage rather than a threshold that is too tight.

A slot is refetched every 60 seconds after its last success, and a fetch may run
up to 35 seconds before it is abandoned. So the oldest a `fetchedAt` gets while
everything is working is those two added together, and a threshold has to clear
the sum, not the interval alone.

Measured on a healthy host, sampling the deployed module: entry ages span 1–62
seconds and average 32. A consumer calling anything older than 30 seconds stale
sees a live signal on **45%** of reads; at 60 seconds, 97%; at 90 seconds and
beyond, 100%. Under 120 seconds — the same figure this module uses internally,
for this reason — a healthy provider never reads stale.

This is a floor rather than a period: it is time since the last *success*, so a
provider retrying through failures legitimately ages past it. That is the
signal working, not the threshold being wrong, and
[the freshness section](#freshness-comes-from-the-producer-never-from-the-poll)
covers what to do with it.

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
| `source` | lane | set per credential lane, and the lane serving one account can change between polls |
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

**`source` is narrower than account scope, and it moves.** A provider can read
one account through more than one lane — a local file or process, and a
credential from the vault. Both describe the same account, and which one answers
is decided per fetch by whichever is healthy and fresh. So the same account can
publish `source: "vault"` on one poll and something else on the next, with no
change to the account, the credential, or anything a consumer did.

It is observability only: read it to know how a figure was obtained, never as an
identity or a state. Keying on it treats one account as two, and alerting on a
change reports a lane fallback — which is the system working — as an incident.
The values in use are `oauth`, `api` and `vault`; treat the set as open, since a
new lane adds a value without warning.

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
fresh + stale + pending + len(degraded) + len(withoutHandles) == providersTotal
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

**`pending` is why one tick is not enough on its own.** The refresher admits a
bounded number of fetch units per turn, so a provider registered here can wait
several turns for its first fetch — with 35 providers that is the ordinary state
for the first turns after a start, not an exceptional one. Those providers are
serving nothing and have failed at nothing, so they belong to no other bucket;
`pending` exists to hold them and keep the sum whole. It is not a fault, and
alerting on a non-zero `pending` would alert on a module that is simply starting
up. If you want "is this provider usable right now", read `fresh + stale`
(published in the health detail as *serving*), not the absence of `pending`.

**The remaining metrics are diagnostics, not a second capacity axis.**

`cookieCohortTotal` counts the providers whose credential is a browser cookie —
the ones coupled to a desktop login rather than a stored token.
`cookieLoginsStale` names the subset whose login **stopped working**: a cookie
was found and the upstream rejected it, or the page it scrapes no longer parses.

Read them together as "N of C logins stale". Do **not** read
`cookieLoginsStale` as the cookie providers that are degraded — it is a
deliberately narrower set, excluding every service this host never logged into,
because not being logged into something you do not use is the correct state and
counting it would pin the number at the cohort size on every machine. The two
diverge sharply in practice: this host currently has eight degraded cookie
providers and two stale logins.

`refresherStalled` is the boolean behind a `degraded` module status: the
refresher's heartbeat is older than its stall horizon. Windows already fetched
keep serving while it is set — only their freshness decays — which is why it
degrades rather than fails.

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
