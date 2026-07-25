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
**full array of degraded entries**. An empty array therefore means one of two
much narrower things:

- the module is cold, and its first sweep has not published yet, or
- every provider resolved zero credential handles, which is structural.

Distinguish them from the health check's metrics — `withoutHandles` names
providers that resolved no handle, and `lastTickAgeSecs` says whether the
refresher has ticked at all. Never infer the difference from the array itself,
and do not flip a staleness flag on elapsed time: a duration-based guess is the
weakest available signal for a question that has a precise answer one call away.

The failure this prevents is specific. A renderer that treats zero rows as
all-quiet shows its calmest possible screen in the one case where something is
genuinely broken.

## Freshness comes from the producer, never from the poll

Each entry carries its own `fetchedAt`: the wall-clock time of that slot's last
**successful** fetch, unchanged by failed attempts. Anchor per-entry freshness
on it.

Do not stamp poll time. On a transient failure this module keeps serving the
last healthy window for the whole exponential backoff — a nominal 60s doubling
to a 15-minute cap — so an entry can legitimately be much older than the
response containing it. Stamping poll time does not merely under-report that
staleness, it **actively resets** it, and the reset is driven by the consumer's
poll cadence rather than the producer's failure duration. Polling more often
makes it strictly worse.

`fetchedAt` is per-entry and **never a common instant**. Two accounts of the
same provider routinely differ, because they are separate slots on separate
backoff schedules. Build no snapshot-moment invariant on one response.

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

## Verdict versus unfinished

A degraded entry is a **verdict**: this module concluded the credential or
payload is unusable. An absent entry is **unfinished**: a sweep is still
running.

Retaining a last-known-good value through an absent entry is correct. Retaining
it through a degraded entry overrides a conclusion already reached — and since
the transient path never emits a degraded entry, every degraded entry a consumer
retains through is one this module has already judged. Hard-age rather than drop
if you keep anything, since only some of the class self-recovers.

## Health is a separate axis

Module health comes from the health check, not from the array: `failing` only
when the serving store is poisoned, `degraded` only when the refresher heartbeat
is stale, otherwise `ok`. Per-provider degradation deliberately does not flip
module status — most providers lack credentials on any given host, so it would
sit permanently degraded and mean nothing.

The metrics carry a conservation identity that must balance:

```
fresh + stale + degraded + withoutHandles == providersTotal
```

Assert it and alert on imbalance. It cannot be tuned into silence and cannot
pass by sampling healthy members. It is **not** a liveness signal, though: it
holds perfectly when every provider is degraded.

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
