# The `usage.get` consumer contract

What a consumer of this module can rely on, what it must not infer, and the test
that holds each rule in place. It covers the `usage.get` envelope, the
`usage.watch` subscription, the `usage.drops` and `usage.observe` operations and
the `health.check` metrics.

**This document owns the wire.** Field meanings, error classes, freshness and
staleness semantics, and what a consumer may infer are settled here. Where any
other document in this repository states one of those facts differently, this
one wins and the other is wrong. How the module produces these answers is in
[`architecture.md`](architecture.md); the provider inventory is in
[`provider-matrix.md`](provider-matrix.md); the gate procedure is in
`verifying.md`.

Each section ends with **Proven by**: the tests that fail if the rule stops
holding, named by function. Registry-wide tests are in
`crates/quota-core/src/tests.rs`, a provider's parsing tests are in that
provider's module, scheduling and drop tests are in `refresh.rs`, `store.rs`
and `quota_drop.rs`, and tests marked `main.rs` are in
`crates/quota-module/src/main.rs`. `cargo test -p <crate> <name>` runs one.

## Contents

1. [Scope and the kinds of silence](#1-scope-and-the-kinds-of-silence)
2. [The core rule](#the-core-rule)
3. [Response shapes](#the-six-response-shapes)
   - [`usage.watch`: route-level change notices](#34-usagewatch-route-level-change-notices)
4. [Freshness](#freshness-comes-from-the-producer-never-from-the-poll)
5. [Error classes](#degraded-means-we-cannot-read-it-not-the-provider-is-down)
6. [Windows](#the-window-slots-are-positions-not-a-ranking)
7. [Spend pools](#spend-is-money-and-it-fails-in-the-opposite-direction-from-a-window)
8. [Account fields and keys](#fields-on-the-account-not-the-window)
9. [Accounts and completeness](#an-absent-account-is-never-a-statement-that-the-account-is-gone)
10. [`usage.drops`](#usagedrops--recently-observed-decreases)
11. [Health](#health-is-a-separate-axis)
12. [Observed provider behaviour](#measured-behaviour-is-recorded-here-not-stamped-on-the-wire)
13. [Quota sample notices on ck-bus](#13-quota-sample-notices-on-ck-bus)

## 1. Scope and the kinds of silence

<a id="three-kinds-of-quiet"></a>
<a id="why-this-file-exists"></a>

A producer and a consumer can each be locally correct and still disagree about
what a response means, because the meaning lives at the boundary. The rules
live here so a change on either side is reviewed against the same text.

Where this document is silent, the silence has a category:

| Category | Meaning |
|---|---|
| specified | guaranteed behaviour, stated in a section below |
| deliberately unspecified | a non-guarantee you should rely on: for example, `fetchedAt` values in one response do not share an instant, so build nothing on cross-entry alignment |
| undocumented | unstable ground, such as the prose of an `error` string; nothing marks it, so relying on it is unsafe |

A field changes category only by an explicit change to this document, never by
being observed on the wire. A consumer's reasonable inference from an
underspecified field is not a guarantee.

**Decode permissively.** This producer adds envelope fields, entry fields,
error classes, `source` values, `funding` and `basis` values and window kinds
without notice. A decoder that rejects unknown fields
(`serde(deny_unknown_fields)` and equivalents) or unknown enum values turns
each addition into an outage on the consumer side.

<a id="the-core-rule"></a>

## 2. The core rule

**A well-formed response is always a successful poll.** One provider's failure
never fails the array: each failure is in-band, in that provider's entry. *Did
the poll succeed* and *is this data fresh* are different questions with
different sources.

Response assembly is in `crates/quota-module/src/main.rs`, the array is built by
`Registry::usage_snapshot` in `crates/quota-core/src/lib.rs`, and the entry type
comes from the published `cortexkit-provider-usage` crate.

**Proven by:** `panicking_provider_is_contained_and_other_provider_resolves`,
`cancellation_contains_a_provider_future_drop_panic`.

<a id="the-six-response-shapes"></a>

## 3. Response shapes

| Shape | Meaning | Consumer action |
|---|---|---|
| entries, some usable | normal, including mid-sweep | use them; never infer anything from the count |
| entries, all degraded | every credential path was reached and none is usable | price as known-bad, not as no signal |
| empty array | no capacity information at all | see [3.3](#an-empty-array-is-not-nothing-configured); this is not "nothing configured" |
| malformed body | not producible on the success path | transport corruption; report it |
| error frame | this module answered and refused | retry per your own policy |
| no answer at all | the daemon refused before reaching this module | see [3.2](#a-refused-call-is-not-a-poll-at-all); carries no information about capacity |

Results publish as each provider completes, so a partial array is normal. A
provider missing from one poll and present in the next has not necessarily
changed state; it may simply have finished. **Nothing in the array is a
delta.**

<a id="a-partial-array-can-look-settled-for-seconds"></a>

### 3.1 A partial array can look settled for seconds

The array does not fill smoothly. Providers with no credential fail in
microseconds and arrive at once; providers doing network work arrive seconds
later, and the array can stay unchanged between the two. Two identical polls
seconds apart do not mean the sweep has finished.

The `pending` health metric answers it: it counts providers whose first fetch
has not completed, and reaches 0 when the last one lands. Never infer
completeness from a count that is stable across polls.

<a id="a-refused-call-is-not-a-poll-at-all"></a>

### 3.2 A refused call is not a poll at all

If the daemon has no module registered under the id being dialled,
`route.open` fails with `unknown_module` and nothing reaches this module. That
is distinct from both entries and error frames:

- a **degraded entry** is a verdict: this module reached a credential path and
  concluded it was unusable;
- an **absent entry** is unfinished: the sweep has not got there yet;
- a **refused call** is neither. No verdict was reached and no work is pending.
  Nothing appears in this module's output, metrics or logs, and its
  `health.check` reads normal.

**A refused call carries no information about capacity.** Retain what you last
knew and retry; do not age, degrade or prune anything because of it. It
authorises nothing about account sets: `completeProviders` cannot arrive on a
refused call. Handle it at transport level, before any body is interpreted.

The usual cause is a module id changed on one side only.

<a id="render-it-differently-from-an-empty-result"></a>

#### Render it differently from an empty result

A UI that hides providers without credentials is right to, and that is what
makes a refused call dangerous: a display with no rows looks the same whether
there is nothing to show or the route is dead. Render a refused call as its own
state ("limits unavailable"), never as an empty list.

<a id="the-module-id-is-the-contract-and-insula-is-the-durable-one"></a>

#### The module id is the contract

The subc module id is `insula`. Pin it. Do not resolve the module by
capability (the `ManagementSurface` with a `usage.get` operation in
`catalog.list`): that describes a shape, not an identity, and selects the wrong
module as soon as a second one declares the same operation. Treat a failure to
open the route as the distinct condition above.

<a id="an-empty-array-is-not-nothing-configured"></a>

### 3.3 An empty array is not "nothing configured"

Degraded entries are entries, so a host with no usable credential returns a
full array of degraded entries. An empty array means one of three narrower
states:

| State | Meaning | Resolves |
|---|---|---|
| cold | the refresher has not published a first result; slots exist but hold no entry | within the first turns after a start |
| structural | providers resolved no credential handles, so there are no slots | when a credential appears |
| withheld | entries exist but each is suppressed because its account identity is unverified | within a refresh cycle |

Withheld is deliberate: serving usage under an identity that may have changed
is the one error this module refuses to risk.

Which states are plausible depends on the request. An **unfiltered** query is
empty only if every provider is cold, handle-less or withheld at once, which in
practice means cold or structural. A query **filtered to one provider** is
empty whenever that provider is in any of the three, and withheld is ordinary
there.

Tell cold from structural with the health metrics, never from the array:
`withoutHandles` names providers that resolved no handle, `pending` counts
those whose first fetch has not completed, and `lastTickAgeSecs` says whether
the refresher has ticked at all. Cold is not "before the first tick": the
refresher admits a bounded number of fetches per turn, so a provider can stay
cold for several turns. Do not decide this on elapsed time.

**Proven by:** `no_provider_is_complete_before_the_first_turn`,
`a_usage_bearing_entry_is_withheld_while_identity_is_unverified`,
`a_labeled_entry_is_withheld_while_its_identity_is_in_flux`,
`cancelled_tick_leaves_slots_pending_without_hanging`.

<a id="freshness-comes-from-the-producer-never-from-the-poll"></a>

### 3.4 `usage.watch`: route-level change notices

Host plugins subscribe on their existing insula route rather than reading
ck-bus. Send a `REQUEST` with body `{"method":"usage.watch"}`. The method makes
it a subscription: the `SUBSCRIPTION` flag (bit 7) is accepted but not
required, because the TypeScript subc client doesn't send it yet. No parameters
or provider filter are required; each stream covers all entries. At most 64
watches may be open in one insula process, with additional requests refused as
`usage_watch_limit`.

The reply is a held-open stream on the request's correlation, channel and
binding epoch, not a `RESPONSE`. Every notice is one JSON `STREAM_DATA` frame
with `ADMISSION=2` (sheddable):

```json
{"kind":"changed","epoch":"process-drops-epoch","seq":1}
{"kind":"changed","epoch":"process-drops-epoch","seq":2,"provider":"codex","account":"work"}
```

Notice 1 is queued immediately, with no `provider` or `account`: read
`usage.get` now. Later notices name one changed `(provider, account)` entry
from the same per-refresher-turn change detector as `quota_sampled` (section
13). They omit `account` when the entry has none, and carry no quota data or
account metadata. An unchanged turn emits nothing. Account metadata alone
does not change a sample. These are invalidations, not account-deletion
events; all completeness and absence rules for `usage.get` still apply.

`seq` starts at 1 **per watch**, not per account. `epoch` is the same opaque
process-lifetime epoch returned by `usage.drops`, not the frame's route-binding
epoch. Re-read `usage.get` on every notice. On a sequence gap (including a
first sequence other than 1), or a different epoch, re-read the entire
snapshot instead of just the named account. Reconnect by opening a new watch;
there is no replay cursor.

Each watch retains at most 16 notices. A full queue drops the **oldest**;
sequence numbers still advance so loss remains visible to readers. Serving
and refreshing never await a watcher. Ordinary replies take precedence over
queued notices. Neither this stream nor ck-bus is a durable history; a
consumer needing periodic backstop snapshots may continue taking them.

Send a pure-header `CANCEL` on the same channel, binding epoch and correlation
to stop a watch and free its queue. Frames already in transport may still be
in flight; ignore them after local cancellation. Route `GOODBYE` frees all
watches on that binding. On module shutdown, active watches receive
`STREAM_END` with `LAST=1` and normal admission (terminal frames are never
sheddable). An insula predating this method replies `unknown_method` with an
"unknown method" message: fall back to polling `usage.get`.

**Proven by:** `usage_watch_immediate_notice_has_no_ids`,
`usage_watch_changed_entry_names_its_identity`,
`usage_watch_unchanged_turn_is_quiet`,
`usage_watch_cancel_stops_the_correlation`,
`usage_watch_cancel_discards_queued_notices_in_receive_order`,
`usage_watch_shutdown_sends_a_reliable_terminal`,
`usage_watch_overflow_keeps_newest_and_prioritizes_usage_get`,
`usage_watch_stalled_transport_does_not_stall_serving`,
`usage_watch_eof_ends_watches_and_reaps_writer` and
`usage_watch_limit_and_route_goodbye_release_state`
(`crates/quota-module/src/usage_watch_tests/mod.rs`, through the real frame loop).

## 4. Freshness

### `usage.observe`: routing's writer contract

Only the daemon-attested principal `reserved:prefrontal-routing` may call
`usage.observe`. Every other principal (including an absent attestation) receives
a route error with code `forbidden`, without applying or counting any rows.
Client-supplied identities and scope owners cannot grant this permission.

```json
{"method":"usage.observe","params":{"observations":[
  {"provider":"claude","account":"vault-account-id","observedAtMs":1790394000123,
   "headers":{"anthropic-ratelimit-unified-5h-utilization":"0.21",
              "anthropic-ratelimit-unified-5h-reset":"1790394000"}}
]}}
```

Routing forwards the newest reading per account, with every
`anthropic-ratelimit-unified-*` header verbatim as a string; insula owns parsing.
The account is the id published as `account` by `usage.get`, not a credential id.
`observedAtMs` is the reading's Unix-millisecond value time, not its delivery time.
Reply: `{"result":{"accepted":1,"ignored":[]}}`. Each ignored row names its
`account` and `reason`; a bad row never refuses the rest of the batch:

| Reason | Meaning |
|---|---|
| `unsupported_provider` | provider is not `claude` |
| `unknown_account` | no current verified Claude slot names that account |
| `no_poll_reading_yet` | the account has no usable successful poll inventory yet |
| `older_than_held` | value time is older than or equal to the held reading |
| `no_usable_window` | neither utilization header is a finite number |
| `invalid_observation` | row shape or timestamp cannot be decoded |

Only usable `5h-utilization` and `7d-utilization` update their matching windows,
as fractions converted to percent and clamped to 0–100. Missing or malformed
utilization leaves that window unchanged. Matching reset headers are Unix
seconds, rendered as RFC 3339 Z; missing or malformed resets are absent. Overall
status, representative claim and overage headers are ignored. Applied readings
advance canonical `fetchedAt` to `observedAtMs` and clear `stale`. They preserve
`source`, account metadata, tertiary and Fable windows, saved resets, breakdowns
and the Extra-usage pool from the poll. They use the same drop comparison,
quota-sample detector and `usage.watch` invalidations as polled readings.

**Proven by:** `usage_observe_requires_attested_routing_and_changes_get_samples_and_watch`,
`captured_call_headers_parse_and_missing_values_do_not_invent_windows`,
`call_observation_preserves_poll_inventory_and_publishes_drop_and_value_time`,
`older_call_observations_are_ignored_without_changing_the_held_reading`,
`ignored_calls_name_unknown_cold_unsupported_and_unusable_rows_independently`,
`an_applied_call_reading_enters_the_existing_quota_sample_queue`.

### 4.1 `fetchedAt` dates the reading

Each entry carries `fetchedAt`: the wall-clock time at which the served reading
was **obtained from its source**. Failed attempts do not change it. Anchor
per-entry freshness on it, never on your poll time.

Usually that is when this module fetched. Where a provider's figure comes from
a source this module reads rather than polls, `fetchedAt` is when that source
produced the value. For `antigravity`, the plugin lane reads the non-secret
sidebar v2 state file (`ANTIGRAVITY_AUTH_SIDEBAR_STATE_FILE`, else
`$XDG_STATE_HOME/cortexkit/antigravity-auth/sidebar-state.json`, else
`~/.local/state/cortexkit/antigravity-auth/sidebar-state.json`) and publishes
the observation time recorded there; a matching live local editor still wins.
That lane identifies accounts only by genuine non-empty emails, never by
positional ids or display labels, and never reads the plugin's account store or
exchanges its refresh tokens.

Guarantees:

- **No upper bound on age.** On a transient failure this module keeps serving
  the last healthy window. Retries back off from 60 s to at most 15 minutes,
  but that caps how often it retries, not how long it serves: a provider down
  for six hours serves a six-hour-old window. `fetchedAt` is the only age, and
  it is authoritative without limit.
- **Per entry, never a common instant.** Two accounts of one provider are
  separate slots on separate schedules.
- **Deterministic within a process.** The value is recorded once with the
  reading, so the same reading renders the same string on every read.
- **Nanosecond precision**, so two distinct successes cannot collide.
- **On a degraded entry it dates the last success**, not the failure and not
  anything now in the entry. It is absent only where the slot has never
  succeeded, or where a credential verdict means nothing may be attributed to
  the current credential ([5.4](#verdict-versus-unfinished)). Test `usage` for
  usability, never the presence of `fetchedAt`.

Two mistakes to avoid, which are the same rule: never reset an entry's age on
your own activity (stamping poll time makes data look younger the more often
you poll), and never pause it on the producer's silence (a window you preserve
across a response that did not mention it keeps aging on its `fetchedAt`).

**Parsing.** A timestamp carries nine fractional digits:

```
2026-09-18T22:43:31.664189000+00:00
```

Python's `datetime.fromisoformat` rejects more than six; JavaScript's `Date`
truncates; Go's RFC 3339 parser accepts it. An ad-hoc reader can truncate the
fraction first:

```python
re.sub(r"\.(\d{6})\d*", r".\1", stamp)   # keep the dot: r"\1" alone drops it
```

Never treat an unparseable timestamp as an absent one: an absent `fetchedAt`
means the entry never succeeded.

**Restarts.** A restart discards every slot, so the same underlying window is
fetched again and gets a new `fetchedAt`: a genuine new observation, with
`stale` unset. On a cache-backed lane a restart can also step `fetchedAt`
**backwards**: within one process, a fetched reading older than the one already
served is discarded and the entry is disclosed `stale`, but a new process has
nothing to compare against. A consumer differencing a value series must treat a
pair whose later `fetchedAt` is earlier than the former as not comparable,
which is what this module's own drop detector does. The `usage.drops` `epoch`
changing is the signal that a restart happened.

**Proven by:** `fetched_at_is_stable_for_fresh_and_stale_slot_entries`,
`a_degraded_entry_keeps_the_timestamp_of_its_last_success`,
`entries_for_a_never_successful_slot_have_no_fetched_at`,
`transient_failure_keeps_serving_last_good_window`,
`a_blip_on_the_fresher_lane_does_not_republish_an_older_reading`,
`a_backward_wall_step_does_not_manufacture_continuity`.

<a id="sizing-a-staleness-threshold"></a>

### 4.2 Sizing a staleness threshold

A slot is normally refetched 60 s after its last success, and a fetch may run up to 35 s
before it is abandoned, so a healthy entry's age reaches up to about 95 s. A
threshold must clear that sum, not the interval alone; below it, every healthy
provider reads stale for part of every cycle, all at once, which looks like a
producer-wide outage. This module itself counts an entry as fresh for 120 s
(`FRESH_HORIZON`). Claude polls every 180 s instead, with a health freshness
horizon of 215 s. A Claude account with a call reading applied in the last three
minutes defers polling until fifteen minutes after its last completed poll.
Once calls stop, the ordinary three-minute schedule resumes; cold accounts are
never deferred. Failure backoff and vault-repair acceleration take precedence
over this deferral: an account backing off after a 429 polls on the backoff
schedule even while call readings arrive.

**Proven by:** `claude_call_cadence_defers_only_recent_observations_and_keeps_backoff`.

**A Claude usage-endpoint 429 uses Anthropic's `retry-after`, within bounds.** Anthropic
rate-limits `GET /api/oauth/usage` per account, and asking again early only
extends the refusal. When a 429 carries a `retry-after` header (delta-seconds
or an HTTP-date) between 0 s and one day, the account's next attempt is that
many seconds after the failed attempt completed, but never sooner than the
180 s poll interval and never later than one hour. Without a believable
`retry-after`, consecutive 429s wait 5, 10, 20, then 30 minutes (capped at
30). A negative value, a date a second or more in the past, or a value over
a day counts as absent. The last good reading keeps serving through the wait with `stale`
set and `class: "upstream_failed"`, as for any transient failure. Only Claude
429s are scheduled this way. Other providers' 429s keep the generic transient
backoff (60 s doubling to 900 s), and every other failure keeps its usual
transient or non-transient delay.

When a 429 carries `retry-after` or any `anthropic-ratelimit-*` response
headers, the failure's message starts with them, for example `provider returned HTTP
429: [retry-after: 600; anthropic-ratelimit-unified-status: rejected] {...}`,
so `lastStaleEpisode` and a degraded entry's `error` show what Anthropic said;
with neither present the message is unchanged. Each value is cut to 80
characters and the header text to 512 bytes (plus a truncation marker when
cut), and no other response header or any request header is ever included. The bracket is
evidence, not an API; don't parse it. The module logs these headers when an
account starts being rate limited, when the set of headers changes, and once
when the endpoint answers again. A host whose only previously-working lanes
are Claude accounts can pass the 1800 s `fetchBlackout` horizon
([11.4](#114-health-metrics)) while it honours a long `retry-after` or sits at
the 30-minute step.

**Proven by:** `a_429_with_retry_after_600_schedules_the_next_attempt_600_seconds_out`,
`a_429_with_an_http_date_retry_after_schedules_from_that_date`,
`a_claude_429_without_retry_after_backs_off_from_five_minutes_to_thirty`,
`an_absurd_retry_after_falls_back_to_the_claude_429_ladder`,
`a_codex_429_keeps_the_generic_backoff`,
`a_429_records_its_rate_limit_headers_in_the_error_message`.

The threshold is a floor on time since the last *success*, so a provider
retrying through failures legitimately ages past it. An entry aging past your
bound is a reason to discount the reading, never evidence that the producer is
failing; whether the producer is failing is answered by health
([11](#health-is-a-separate-axis)).

**Sidebar-backed readings age by design.** The Antigravity plugin polls quota
and publishes a sidebar snapshot. A quota group is served only while its
observation `checkedAt` is at most 3600 s old (`CACHED_QUOTA_MAX_AGE`); an
undated group means no reading yet. Groups age independently, and `fetchedAt`
carries the oldest served group's `checkedAt`, never the file's publication
time or the time this module read it. A sidebar read error is transient, never
an empty roster. Sidebar versions other than v2 (including v1) need a plugin
update. Changing `CACHED_QUOTA_MAX_AGE` is a contract change: consumers
(astrocyte's `window_sample` insert) rely on over-age readings being withheld.

For the vault lane, `tier.paidId` present and not `free-tier` is positive
paid-tier evidence, so a matching fresh sidebar outranks the cloud's standard
pool. An absent tier is unknown, not paid.

**Proven by:** `a_healthy_slot_stays_fresh_across_its_whole_refresh_cycle`,
`freshness_is_computed_from_last_success`.

### 4.3 `stale`: a reading preserved through failure

Prefer the `stale` field to any age threshold, because it says directly that a
reading is being preserved through failure:

```json
"fetchedAt": "2026-08-13T10:00:00Z",
"stale": { "since": "2026-08-13T10:02:00Z", "class": "upstream_failed" }
```

`fetchedAt` is when the served reading was obtained; `since` is when the
producer first failed to refresh it. The gap between them is how long the
producer has been unable to look, which is the quantity a staleness policy
wants. `since` is always later than `fetchedAt`. `class` uses the `errorClass`
vocabulary and is optional: render "stale, cause unstated" rather than
suppressing it.

`stale` absent means the entry is a fresh success or a cache-served entry that
is healthy and older than a poll. It is not a fault signal: preserving a
reading through a brief upstream failure is correct behaviour.

**Proven by:** `a_verdict_discloses_no_staleness_while_a_stale_served_window_does`,
`an_unchanged_unlabeled_record_still_stale_serves_through_a_flap`.

<a id="degraded-means-we-cannot-read-it-not-the-provider-is-down"></a>

## 5. Error classes

### 5.1 Degraded means "we cannot read it", not "the provider is down"

A degraded entry carries `error` (prose) and `errorClass` (machine-readable),
and no `usage`. Which failures degrade is decided by `classify` in
`crates/quota-core/src/refresh.rs`; read it there rather than trusting a copy:

| Failure | Classified | On a slot that has served before |
|---|---|---|
| HTTP 401 or 403 | non-transient | degrades |
| no session, credential unusable, unauthorized | non-transient | degrades |
| no quota reported | non-transient | degrades |
| decode failure | non-transient | degrades |
| a contained panic in a provider fetch | non-transient | degrades |
| HTTP 429, any 5xx, network error, fetch deadline | transient | keeps serving the last window, marked `stale` |
| a local source (editor, app) not reachable | transient | keeps serving the last window, marked `stale` |

A transient failure on a slot that has **never** succeeded has nothing to
serve, so it degrades like any other: a provider whose first fetch times out
publishes a degraded entry carrying a timeout error.

A non-transient failure is retried after a fixed 300 s
(`NON_TRANSIENT_BACKOFF`); a transient one after an exponential backoff from
60 s capped at 900 s (`MAX_TRANSIENT_BACKOFF`).

**Proven by:** `classify_splits_transient_from_non_transient`,
`a_transient_failure_with_no_prior_success_degrades`,
`non_transient_failure_replaces_a_prior_healthy_entry`,
`transient_failure_keeps_a_prior_healthy_entry`,
`non_transient_backoff_is_fixed`, `transient_backoff_is_exponential_and_capped`,
`a_deadline_overrun_is_attributed_to_the_upstream_and_stays_transient`.

<a id="not-every-degraded-entry-is-a-problem"></a>

### 5.2 The `errorClass` vocabulary

`errorClass` is on the wire on every degraded entry and absent on every healthy
one: branch on `error` for health, never on this field's presence. The
self-recovery column decides between two messages to a person, *wait* or
*act*; `no_quota_reported` is the one row where "no" means nothing is wrong.

| class | meaning | self-recovers? |
| --- | --- | --- |
| `credential_absent` | no credential is configured for this provider on this host | no — steady state |
| `credential_unusable` | a credential is present but cannot be used as one | no |
| `credential_rejected` | the credential was sent and refused (401/403) | no — needs re-authentication |
| `no_quota_reported` | the credential works; the account reports no quota | no, and nothing is wrong |
| `local_source_unavailable` | a local source this provider reads usage from cannot be reached — a program that is not running, or data this process is not permitted to read | yes — when the program runs again, or the permission is granted |
| `upstream_failed` | the provider's endpoint failed (429, 5xx, network) | yes |
| `decode_failed` | a response arrived in an unexpected shape | yes |
| `internal_error` | a fault in this module, contained rather than crashing | sometimes |

Notes on single rows:

- `credential_unusable` is also what a vault credential reports once the vault
  has marked it as needing re-authentication; this module does not refresh
  OAuth tokens itself.
- `local_source_unavailable` exists so that `credential_absent` keeps one
  meaning. A provider that reads a running application would otherwise appear
  and disappear as someone opens and closes it, while `credential_absent` is a
  stable fact about the host that a consumer may act on.
- `upstream_failed` and `local_source_unavailable` are rare on a degraded entry,
  because both are transient: seeing either means the slot never had a healthy
  window to fall back on.
- `internal_error` means the provider may be healthy and this module failed
  reading it. It is *sometimes* because a panic on one response clears on the
  next fetch, while a deterministic bug repeats until fixed here; nothing on the
  wire separates them.

**The vocabulary grows.** Decode `errorClass` as an open string. Render an
unrecognised class as a degraded entry with an unknown reason; never drop the
entry and never fold it into a known class.

**No class authorises removing an account.** A degraded entry names an account
that exists and is currently unusable, `credential_absent` included. Account
removal is authorised only by `completeProviders`
([9.3](#completeproviders-is-how-an-account-may-be-removed)).

**The `error` string is prose with no stability promise**, and its wording has
changed between releases. Use it for display and logs; branch on `errorClass`.

**Proven by:** `every_wire_error_class_has_a_documented_row` (this table against
`FetchError::error_class`, both directions), `a_degraded_entry_publishes_its_error_class`,
`a_healthy_entry_publishes_no_error_class`,
`a_contained_panic_is_attributed_to_this_module_not_the_upstream`.

### 5.3 Which degraded entries need attention

`errorClass` says how a fetch failed, not whether anything is wrong:

- a provider nobody configured on this host fails every fetch forever, and that
  is the correct steady state;
- a credential that worked and was then refused fails identically on the wire,
  and needs attention;
- a working credential whose account has no quota is neither absent nor broken.

On a typical host most degraded entries are the first kind, so an
undifferentiated count of degraded entries mostly measures how many providers
are unconfigured. The health check applies the split at the provider level
(`degraded` versus `unconfigured`, [11.2](#health-is-a-separate-axis)); a
surface rendering accounts should group on `errorClass`.

<a id="verdict-versus-unfinished"></a>

### 5.4 Verdict versus unfinished

A degraded entry is a **verdict**: this module concluded the credential or
payload is unusable. An absent entry is **unfinished**. Retaining a
last-known-good value through an absent entry is correct; retaining it through
a degraded entry overrides a conclusion already reached. If you keep anything,
age it hard rather than dropping it, since some classes self-recover.

Read `errorClass` and `fetchedAt` together:

| `errorClass` | `fetchedAt` | What happened |
| --- | --- | --- |
| absent | present | served |
| present | present | a provider call failed; the timestamp is the last success |
| present | **absent** | the slot never succeeded, or a credential verdict was reached and nothing about earlier readings may be attributed to the credential now in place |

The last row covers a slot whose identity is unverified: a credential that was
replaced, or one the vault has latched. One failure can pass through both
degraded rows: a vault credential going bad is first refused by the provider
(`credential_rejected` beside a frozen `fetchedAt`), and once the vault
reaches its own verdict this module stops calling the provider
(`credential_unusable`, no `fetchedAt`). So a class change on one account is not
necessarily a state change, and a disappearing `fetchedAt` does not mean the
account never worked; it means nothing here may be attributed to the current
credential.

**Proven by:** `a_vault_latch_on_an_identity_less_credential_publishes_a_verdict`,
`entries_for_a_never_successful_slot_have_no_fetched_at`,
`transient_failure_after_degradation_stays_degraded`.

<a id="recovery-after-a-latch-is-backoff-gated-not-immediate"></a>

### 5.5 Recovery after a credential is repaired

A slot in non-transient backoff is retried after 300 s at the latest. A vault
credential recovers sooner:

| Repair | When the slot is fetched again |
|---|---|
| the vault record is re-sealed (its `record_version` increases) | on the first refresher turn whose vault listing shows the new version |
| the vault record is reactivated (`needs_reauth` cleared) | on the first turn whose listing shows it reactivated |
| a vault-backed slot is in non-transient backoff | it also asks the vault for the credential's status once per 60 s, and a replaced record ends the backoff |
| a cookie or API-key lane, which records no version | after the 300 s backoff |

A record in `needs_reauth` is not fetched at all until it is repaired, so
continued `credential_unusable` until the vault shows the repair is expected,
not evidence that the repair failed.

**Proven by:** `replaced_record_acceleration_needs_a_version_basis_and_reports_only_real_backoff`,
`enumeration_version_and_reactivation_accelerate_while_needs_reauth_suppresses_fetch`,
`refresh_loop_reactivated_handle_is_admitted_within_one_discovery_interval`,
`scoped_non_transient_backoff_polls_status_once_per_base_interval`,
`an_advanced_record_version_clears_non_transient_backoff`.

<a id="the-window-slots-are-positions-not-a-ranking"></a>

## 6. Windows

### 6.1 The window slots are positions, not a ranking

A `usage` object carries up to three slots, `primary`, `secondary` and
`tertiary`, plus `extraRateWindows`, a list of named windows for anything that
does not fit three.

**No field identifies the binding limit**, and the producer does not compute
one. To know how much headroom an account has, take the maximum `usedPercent`
across every slot and every `extraRateWindows` entry, or apply your own policy
deliberately.

- **`primary` is usually the shortest window, not the most constrained one.**
  Most adapters fill slots in cadence order (session or five-hour in `primary`,
  weekly in `secondary`). A short window refills quickly and often reads low
  while the weekly window is well used.
- **Slots can have holes.** Each comes from its own optional upstream field, so
  `secondary` can be absent while `tertiary` is present. An absent slot means
  that window was not reported; never that it exists at zero, and never that
  the slots after it are empty.
- **Slots and named extras are disjoint.** A limit is published once, either in
  a slot or as an extra, never both.
- **A provider may publish only extras.** `antigravity` publishes one named
  window per pool and cadence and no slot, so a consumer reading `primary`
  alone gets nothing for it.
- **Slots are not comparable across providers.** One provider's `primary` may be
  five hours and another's a month. Read `windowKind`, falling back to
  `windowMinutes`.

<a id="a-maximum-over-an-incomplete-set-is-biased-downward"></a>

### 6.2 A maximum over an incomplete set is biased downward

A window missing from the set can only lower a maximum, and the result looks
exactly like a genuine reading. Two kinds of absence exist:

- **not reported by the upstream**: the account has no such limit, or the
  provider does not expose it. Nothing is lost;
- **reported but unusable**: a window whose percent cannot be represented is
  dropped rather than published, because a non-finite number serializes as
  `null` and would fail a typed decode of the whole array. This requires an
  unguarded division upstream of the check and is not reachable today.

The same applies to a consumer's own decoder: one that discards a window it
cannot parse turns *unreadable* into *absent* and reports a smaller number.
Prefer marking a reading as built from an incomplete set.

<a id="a-window-set-carries-no-completeness-claim"></a>

### 6.3 A clean entry carries every window

There is no per-entry flag saying a window set is complete, because the
producer guarantees it instead:

- a window set is complete unless the entry carries an error; publishing a
  subset silently is a defect in this module;
- a lane that cannot enumerate an account's windows reports that through the
  error classes (`no_quota_reported` if the upstream states there is nothing,
  `decode_failed` if its answer could not be read) rather than publishing what
  it managed to read;
- so a consumer may treat the windows on an entry without `error` as all of
  them.

<a id="the-other-window-fields"></a>

### 6.4 Window fields

| Field | Meaning | Absent means |
|---|---|---|
| `usedPercent` | share of the window used, 0 to 100; the effective figure after banked-reset relaxation ([6.6](#usedpercent-can-be-lower-than-what-the-provider-reported)) | (always present) |
| `rawUsedPercent` | what the upstream reported, present only when it differs from `usedPercent` | the two agree |
| `resetsAt` | when the window resets, as the upstream stated it | no reset was stated ([6.7](#an-exhausted-window-may-carry-no-reset-at-all)) |
| `windowMinutes` | the window's length, when the upstream states a cadence or its field name implies one | no stated length; never "unlimited" |
| `windowKind` | the period named by the upstream's own label, key or field: `hourly`, `five_hour`, `daily`, `weekly` or `monthly` | not named; never "some other kind" |
| `usedCount`, `totalCount` | the absolute figures behind the percentage, where the upstream reports them | not reported |
| `breakdown` | who consumed the window ([6.5](#breakdown-who-consumed-a-window)) | no split published |

`windowKind` is set only from the upstream's own naming. A numeric duration
alone does not name a period: Codex's `limit_window_seconds` leaves
`windowKind` absent even when `windowMinutes` is 300. The wire type is an open
string; this producer emits only the five values above. A calendar month has no
fixed length, so a monthly window carries `windowKind: "monthly"` and no
`windowMinutes`.

`usedCount` and `totalCount` are whole numbers; the wire type is `f64` for
range only. `usedCount` is published only where the upstream reports a count,
never reconstructed from the percentage and the cap, because an upstream can
compute its percentage against a different cap from the one it reports. Do not
compute one from the other. `totalCount` alone is still useful: a change in it
is a cap change.

#### Named extra windows

Each `extraRateWindows` entry has an optional `id`, `title` and `window`. Match
on `id`, render `title`, and handle an entry whose `window` is absent: that is a
limit the provider named without a readable figure. A consumer that ignores
`extraRateWindows` ignores real limits.

- **Key on `(provider, id)`, never `id` alone.** Ids are not drawn from a
  shared vocabulary, and providers sharing an upstream can share ids for
  different quota pools.
- **Ids can differ between lanes of one provider.** `antigravity` reads one
  account through the live editor, the vault cloud summary and the plugin's
  sidebar snapshot; all three publish `gemini-5h`, `gemini-weekly`, `3p-5h`
  and `3p-weekly`, with titles derived from cadence. When the summary endpoint
  refuses, a per-model fallback that states no cadence publishes one window per
  pool named for the pool (`Gemini Models`) with no `windowMinutes`. A consumer
  matching ids must accept every lane's form.
- **Never match on `title`.** It is display text and may carry mutable detail
  such as a model count. List the ids you want rather than the ones to skip; a
  skip list passes every form nobody anticipated.
- Ids this module constructs are stable and change only with notice; ids passed
  through from an upstream change when the upstream does.

#### Provider-specific window shapes

- **`claude`** five-hour `primary` and weekly `secondary` may come from model-call
  readings and then be fresher than Fable (`extraRateWindows`), tertiary, saved
  resets, breakdowns and Extra usage. Those poll-only fields are at most about
  fifteen minutes old during healthy operation; failed polls can leave them
  older. `fetchedAt` dates the newest applied reading, not every retained field.
- **`qwen-cloud`** tries the Team Token Plan before the personal plan. An active
  Team plan publishes exactly one named extra, `id: "team"`, `title: "Team"`,
  with all three slots empty. It is the shared subscription's `credit_value`
  allowance, with `usedPercent` derived as `(total − surplus) / total × 100`
  only when both figures are present and the total is positive. It carries the
  stated cycle reset when available, and no invented duration, kind, counts or
  duplicate spend pool; seat allocation has no field. With no active Team plan,
  a failed Team discovery, or a Team refusal (401/403, sign-in redirects, login
  codes), it publishes personal windows: five-hour `primary`, weekly
  `secondary`, and monthly as `primary` when alone or as the named `monthly`
  extra otherwise. Only the personal path reports an expired session as
  `credential_rejected`. Team coverage is fixture-verified only.
- **`ollama`** page lane: `Monthly usage` publishes `windowKind: "monthly"` with
  no `windowMinutes`; a legacy `Session usage` block in the same slot names the
  five-hour allowance (`windowKind: "five_hour"`). Use the kind, not the slot,
  to tell them apart. The page's `Free usage` label and the API-key lane
  (`apikey:ollama-cloud`, `GET /api/balance`) name no period, so they set
  neither `windowKind` nor `windowMinutes`. The API lane publishes the included
  allowance in `primary` with
  `usedPercent = min(100, max(0, allowance_usd - balance_usd) / allowance_usd * 100)`
  only when both amounts are stated and the allowance is positive, and takes
  `resetsAt` from `included.period.until` when parseable. A key replaces the
  cookie lane for that account. `Monthly credits used` and relative
  `Next refill` prose have no denominator or absolute time and are not
  published as a window or reset.
- **`vercel-budget`** publishes one `primary` window for the user's personal AI
  Gateway budget, including a cumulative budget with no reset, length or kind;
  see [7.12](#vercel-and-vercel-budget-the-team-balance-and-a-personal-budget).
- **`soniox`** publishes one `primary` window for an organization's monthly
  budget, only when the budget is positive; see
  [7.14](#soniox-an-organizations-prepaid-balance-and-monthly-budget).

**Proven by:** `a_monthly_usage_block_publishes_its_percent_without_a_fabricated_cadence`,
`a_page_with_monthly_and_session_blocks_publishes_both`,
`active_team_plan_publishes_one_distinct_credit_window`,
`a_monthly_window_beside_the_rolling_windows_is_an_extra`.

<a id="breakdown-who-consumed-a-window"></a>

### 6.5 `breakdown`: who consumed a window

`breakdown` is optional on every window. Today only `claude`'s weekly
all-models window (`secondary`, `windowMinutes: 10080`) carries it, read from
`seven_day_breakdown` on Anthropic's `/api/oauth/usage`.

```json
"breakdown": {
  "asOf": "2026-09-27T08:37:37.083733+00:00",
  "windowStartedAt": "2026-09-23T14:00:00.060894+00:00",
  "rows": [
    { "key": "claude_code", "sharePercent": 100 },
    { "key": "chat", "sharePercent": 0 },
    { "key": "cowork", "sharePercent": 0 },
    { "key": "other", "sharePercent": 0 }
  ]
}
```

- **`sharePercent` is a share of consumption, not a used percent.** Rows sum to
  about 100 whatever the window's `usedPercent`; the part of `usedPercent` a
  surface accounts for is `usedPercent * sharePercent / 100`. Every published
  share is within 0..=100.
- **`key` is the upstream's category key, verbatim** and open-ended. Handle
  keys you do not know. The upstream's `display_name` is not carried; a row
  without a key is dropped.
- **Absence means unpublished, never zero.** No `breakdown` means the upstream
  sent none, sent one this module could not read, or sent no usable rows. A row
  without `sharePercent` had no usable figure. A malformed split never fails
  the fetch. `asOf` (when the split was computed) and `windowStartedAt` (when
  its window began, which places a split relative to a reset) are each omitted
  when unstated.
- Which key traffic from third-party OAuth clients is attributed to is not
  documented by Anthropic; do not build on a mapping without confirming it for
  your own traffic.

**Proven by:** `a_breakdown_row_without_a_percent_publishes_no_share_not_zero`,
`a_malformed_breakdown_publishes_no_split_and_leaves_the_windows_alone`.

<a id="100-used-does-not-mean-requests-are-being-refused"></a>

### 6.6 100% used does not mean requests are being refused

`usedPercent` is a capacity reading, not an enforcement state. A provider can
serve past a reported 100% (soft limits, grace, an allowance the figure does
not cover), and can refuse below it (an enforced cap that differs from the one
the figure is computed against, for example after a plan change). **No field on
this wire states that a provider is refusing requests.** Treat 100% as *expect
no headroom* and let the provider's own error prove refusal.

<a id="usedpercent-can-be-lower-than-what-the-provider-reported"></a>

#### `usedPercent` can be lower than what the provider reported

Codex grants banked quota-reset credits (`savedResets`), and when
`codex.auto_use_resets` is configured this module may spend one. It spends one
in exactly two cases:

- **the account is at its wall**: the upstream itself reports the limit as
  reached. A `usedPercent` near 100 is not the wall, and a response that does
  not say whether the limit is reached is treated as not at the wall. Even then
  the credit is kept while any other account of the provider may have room: one
  whose fresh reading does not report its limit reached, or one not read
  recently. At most one account redeems at a time; among several walled
  accounts it is the one whose own reset is furthest away;
- **a credit is about to expire**, within the configured window, whether or not
  the account is at its wall, because an expired credit is lost either way.

While an account holds an unspent credit and is clear of its wall by a margin
(the upstream affirms the limit is not reached and every window is below 99%),
its published `usedPercent` is **zero**, and the upstream's figure moves to
`rawUsedPercent`. A window at 99% or more is always published as reported. The
relaxation is applied at read time and only to a fresh, healthy entry.

- `usedPercent` is the effective number: pace, route and price on it.
- `rawUsedPercent` is what the upstream said: show it where a zero beside real
  consumption would look like a bug. Do not pace on it; routing away from an
  account with a credit to spend wastes the credit.
- An account can jump from 0% to a high number in one poll when its credits run
  out or the conditions stop holding. That is a correct reading.

Claude also publishes a `savedResets` inventory, but it is display-only: no
redemption path is known, and Claude windows are never relaxed.

**Proven by:** `relaxation_is_read_time_only_and_expires_at_the_freshness_horizon`,
`degraded_slots_and_other_providers_never_relax`,
`real_codex_account_spends_its_reset_only_once_its_limit_is_reported`,
`an_absent_limit_reached_is_not_the_wall`,
`a_walled_account_withholds_when_sibling_is_stale_or_unaffirmed`,
`the_expiry_trigger_still_spends_on_an_account_not_at_its_wall`,
`redemption_journal_reuses_pending_id_after_restart`.

<a id="an-exhausted-window-may-carry-no-reset-at-all"></a>

### 6.7 An exhausted window may carry no reset at all

`resetsAt` is never fabricated. A window is published from its percent alone,
and a reset only when the upstream reports one, so **a window at 100% with no
`resetsAt` is a legal shape**. At least one upstream moves the reset out of an
exhausted window's block, and others omit it. A live sample rarely shows this
case, which is not evidence that it cannot occur.

Any logic that sorts or selects on reset time must decide what an absent reset
means. Treating it as "soonest" makes a provider with no evidence of recovery
win every retry; treating it as "never" excludes it from recovery permanently.

<a id="spend-is-money-and-it-fails-in-the-opposite-direction-from-a-window"></a>

## 7. Spend pools

Some providers sell credit beside a plan, or instead of one. Credit is an
amount with no period (no percentage, no reset, no window length), so it is
published separately as `spend`, never as a window:

```json
"spend": [
  { "id": "granted_balance",   "label": "Granted credit",
    "funding": "granted",   "basis": "reported",
    "remaining": { "minor": 0,    "exponent": 2, "unit": "CNY" } },
  { "id": "topped_up_balance", "label": "Purchased credit",
    "funding": "purchased", "basis": "reported",
    "remaining": { "minor": 2402, "exponent": 2, "unit": "CNY" } }
]
```

Over-consuming a window gets an account throttled until it resets.
Over-consuming a balance gets it **billed**, so nothing in a pool is a
percentage and nothing carries a window's semantics.

`claude-console` publishes each non-expired, positive-sized credit grant as a
reported pool, in subscription / purchased / other promotional order. Subscription
API credit uses `funding: subscription`, purchased tranches use `purchased`, and
other promotional tranches use `granted`. Exhausted remainders stay at zero; a
negative remainder is clamped to zero. The aggregate balance appears only when
the response lists no grants, never beside its constituent grants. `usage` has
no rate windows. Grant expiry is not a reset and the current pool type has no
expiry field, so expiry is used for filtering but not published. Monthly spend
is not fetched because the wire has no field for cumulative spending this month.

<a id="amounts-are-integer-minor-units"></a>

### 7.1 Amounts are integer minor units

`{ minor: 2402, exponent: 2, unit: "CNY" }` is 24.02 CNY: `minor / 10^exponent`.
Compare in minor units; binary floats cannot hold decimal amounts exactly.
`unit` is a free string: a currency code where the provider states one,
otherwise the provider's own word for its credits. **Two pools are not
comparable because both carry an amount.**

<a id="funding-says-what-a-pool-costs-to-spend"></a>

### 7.2 `funding` says what a pool costs to spend

| value | meaning |
| --- | --- |
| `granted` | given by the provider; spending it costs nothing |
| `purchased` | bought; spending it costs money |
| `subscription` | included in a plan already paid for |
| `unknown` | the provider separates this pool but does not say what funds it |

`unknown` is a correct answer, not a failure, and it is also how an
unrecognised value from a newer producer should decode. "Spend only free
credit" must mean *only pools stated as `granted`*, never *everything not
stated as `purchased`*.

<a id="basis-says-whether-remaining-is-exact"></a>

### 7.3 `basis` says whether `remaining` is exact

| value | meaning |
| --- | --- |
| `reported` | the provider states this pool's remainder directly — exact |
| `derived` | computed against a consumption figure covering several pools — a **ceiling** |
| `unstated` | not stated, or a value this consumer does not recognise — treat as a **ceiling** |

Providers often report grants per pool and consumption against their sum,
which makes a per-pool remainder underivable.

<a id="spendable-and-what-absence-means"></a>

### 7.4 `spendable`, and what absence means

`spendable` is read from the provider, never inferred from `remaining > 0`: a
pool can be non-empty and closed. When it is absent the provider did not say:

| Reader | Treat absent as | Why |
| --- | --- | --- |
| about to spend | **not spendable** | spending from a closed pool costs money and cannot be undone |
| rendering to a person | **unknown** | hiding a live pool misleads someone who could act on it |

<a id="decode-funding-and-basis-permissively"></a>

### 7.5 Decode `funding` and `basis` permissively

Both sets will gain values. Decode them as strings or as an enum with a
catch-all. A strict decode of one pool's `funding` fails the whole entry and
takes the account's rate windows with it. An unrecognised `funding` means the
same as `unknown`, and an unrecognised `basis` the same as `unstated`; both
land on the conservative side without special handling.

<a id="finding-out-what-your-build-cannot-decode"></a>

### 7.6 Finding out what your build cannot decode

The wire types live in the `cortexkit-provider-usage` crate, tagged per release
in `cortexkit/commons`. To see what changed since the version you decode
against:

```
git diff cortexkit-provider-usage-v<yours>..cortexkit-provider-usage-v<latest> \
  -- crates/cortexkit-provider-usage/src/lib.rs
```

<a id="absent-empty-and-present"></a>

### 7.7 Absent, empty, and present

- `spend` **absent**: nothing to report about pools, either because the
  provider has none or because the pool fetch failed. Silence.
- `spend: []`: the provider was asked and reports none. A statement.
- **A pool with no `remaining`**: the pool exists and the provider did not say
  how much is in it; its `basis` is `unstated`. Read it as "credits exist,
  amount unknown", never as zero. `codex` publishes this shape for an account
  whose plan has credits but whose balance is `null`.
- **Pools and no windows**: a provider that sells only credit publishes
  `usage: {}` beside a non-empty `spend`, and the entry is healthy.
- **A pool without `resetsAt`** means the period was not stated, never that the
  pool does not renew.
- **`expiresAt`** is when unused credit in the pool lapses, as the provider
  stated it. It is not a reset: what is left is lost and the pool does not
  refill. A later grant (next month's plan credit, say) arrives as a separate
  pool. Spend expiring credit first. Absent means not stated, never "does not
  expire". `claude-console` publishes it on every grant that states one.

**Proven by:** `a_null_balance_publishes_a_pool_with_no_amount_and_no_basis`,
`an_absent_has_credits_publishes_no_pool`, `deepseek_pools_are_reported_not_derived`,
`each_grant_publishes_its_expiry_and_no_reset`.

<a id="claude-paid-overage-as-the-extra_usage-pool"></a>

### 7.8 `claude`: paid overage as the `extra_usage` pool

A Claude account with extra usage enabled keeps serving past 100% of its
windows and bills the overage, published as one pool beside the windows:

```json
"spend": [
  { "id": "extra_usage", "label": "Extra usage",
    "funding": "unknown", "basis": "derived", "spendable": true,
    "total":     { "minor": 10000, "exponent": 2, "unit": "USD" },
    "remaining": { "minor": 8723,  "exponent": 2, "unit": "USD" } }
]
```

- `total` is the stated overage limit; `remaining` is that limit minus the
  overage used, never below zero. `basis: derived`, because the provider states
  a limit and a usage figure, not a remainder.
- `spendable: true` means overage is switched on **and** has not reached its
  limit; `false` means one of the two stopped it, which can happen while
  `remaining` is above zero. Absent when the provider did not state both.
- `funding: unknown` here means post-paid: billed after use. Spending it costs
  money, which is what the conservative reading of `unknown` already assumes.
- No reset: the provider states no period.
- No pool, and `spend` absent, when the provider sends no overage figures, a
  shape this producer cannot read, or a limit and usage in different currencies
  or scales. The windows are published as usual.

**Proven by:** `captured_overage_account_publishes_one_extra_usage_pool`,
`disabled_overage_is_not_spendable`.

<a id="codex-the-workspace-credit-limit-as-the-individual_limit-pool"></a>

### 7.9 `codex`: the workspace credit limit as the `individual_limit` pool

Codex team and enterprise workspaces can cap each member's monthly credit
spend. When the account reports that cap, it is published beside the windows
and beside any `credits` pool:

```json
"spend": [
  { "id": "individual_limit", "label": "Workspace credit limit",
    "funding": "unknown", "basis": "reported",
    "total":     { "minor": 10000, "exponent": 1, "unit": "credit" },
    "remaining": { "minor": 7495,  "exponent": 1, "unit": "credit" },
    "resetsAt": "2026-10-01T00:00:00Z" }
]
```

- `total` is the workspace's limit and `remaining` what the provider says is
  left, at one exponent. `unit` is the provider's own; no pool is published if
  it names none.
- `funding: unknown`: an admin-set cap is not a grant, a purchase or a
  subscription allowance.
- `spendable: false` when the provider says the limit is reached; otherwise
  absent, which is not a promise that the pool can be drawn on.
- `resetsAt` is the provider's period end for the limit. The pool is still
  published, without `resetsAt`, when the reset is missing or unreadable.
- Amounts keep at most nine decimal places. The provider renders them from
  floats with up to 14 places of noise, so a longer fraction is cut toward a
  floor: `remaining` is never overstated.
- **No pool means no limit**: the account has none (`individual_limit: null`)
  or sends no `spend_control`. Clear a remembered budget only then.
- **A limit that is stated but can't be read** (a field missing or not a plain
  decimal) never drops the pool. It republishes the account's last readable
  pool, or, with nothing remembered (a first reading, or after a restart), a
  pool with `basis: unstated` and no amounts: a budget exists, amount unknown.

**Proven by:** `a_populated_individual_limit_publishes_a_pool_with_its_reset`,
`a_null_individual_limit_publishes_no_pool_and_keeps_the_windows`,
`a_reached_limit_marks_the_pool_unspendable`,
`an_unreadable_reset_publishes_the_pool_without_one`,
`an_over_long_wham_fraction_publishes_the_limit_floored`,
`an_unreadable_first_budget_publishes_an_unstated_pool`.

<a id="jetbrains-the-purchased-top-up-as-the-topupquota-pool"></a>

### 7.10 `jetbrains`: the purchased top-up as the `topUpQuota` pool

JetBrains splits AI quota into a tariff that refills at `nextRefill` and a
purchased top-up that does not renew and is drawn only after the tariff is
spent. The primary window is the **tariff alone** (its `usedPercent`,
`usedCount`, `totalCount`, and `windowMinutes` when the payload states a
period: `PT720H` is 43200). The top-up is a pool:

```json
"spend": [
  { "id": "topUpQuota", "label": "Top-up quota",
    "funding": "purchased", "basis": "reported",
    "total":     { "minor": 207000000, "exponent": 3, "unit": "jetbrains-ai-quota" },
    "remaining": { "minor": 207000000, "exponent": 3, "unit": "jetbrains-ai-quota" } }
]
```

- A window at 100% beside a non-empty pool means the refilling allowance is
  spent and the account is drawing purchased quota.
- `unit` is a label, not a currency.
- No pool, and `spend` absent, when the payload has no `topUpQuota` or states
  it in a form that does not parse exactly. Without a readable `tariffQuota`
  the window is the summed balance, with no `windowMinutes` and no pool.

**Proven by:** `a_spent_tariff_reads_full_while_the_top_up_stays_untouched`,
`an_unreadable_top_up_publishes_no_pool_and_leaves_the_window`.

### 7.11 `ollama`: the credit balance as the `purchased` pool

The page's `Credit balance` and the API's `purchased.balance_usd` publish a
pool `id: "purchased"`, `label: "Credit balance"`, funding `purchased`, in exact
USD minor units. A stated remainder has basis `reported`; an API `purchased`
section with a missing or null amount has no remainder and basis `unstated`,
never a zero balance. No total, `spendable` flag or refill time is inferred.
A page with credits and no rate window is valid.

<a id="cursor-monthly-on-demand-spend-as-the-on_demand-pool"></a>

### 7.12 `cursor`: monthly on-demand spend as the `on_demand` pool

A Cursor account on a unified personal plan (`membershipType` starting with
`unified_`, `limitType: user`) is billed for on-demand use past its plan.
For those accounts only, insula makes one extra request to Cursor's dashboard
(`/api/dashboard/v2/usage-status?scope=user`, same session, 5-second timeout)
and publishes the month's on-demand spend beside the plan window:

```json
"spend": [
  { "id": "on_demand", "label": "On-demand spend (monthly)",
    "funding": "unknown", "basis": "reported",
    "total":     { "minor": 1000, "exponent": 2, "unit": "USD" },
    "remaining": { "minor": 907,  "exponent": 2, "unit": "USD" },
    "resetsAt": "2026-11-01T00:00:00Z" }
]
```

- `total` is the account's monthly on-demand limit and `remaining` that limit
  minus the month's on-demand spend, never below zero, both from Cursor's
  whole-cent figures. `basis: reported`: the spend figure covers this pool
  alone, so the remainder is exact rather than a ceiling.
- `funding: unknown` here means post-paid: on-demand use is billed after it
  happens. Spending it costs money.
- `resetsAt` is Cursor's own end of the monthly period. `spendable` is absent:
  Cursor states no on/off switch in this reading.
- The pool is published only when Cursor marks the reading `available`, both
  cents figures are finite, non-negative whole numbers no greater than 2^53,
  both period dates parse as RFC 3339 times, and the present falls inside the
  period (`periodStart <= now < resetsAt`). A reading for another month is
  never published as this month's.
- **No pool, and `spend` absent**, when the account is on any other plan
  (including a unified team budget, `limitType: team`; no extra request is
  made), or when the extra request fails, times out or returns a reading
  that fails a check above. The plan window and the entry's error class are
  exactly what they would be without the extra request: its failure is never
  the entry's failure.

**Proven by:** `a_unified_account_publishes_the_on_demand_pool_beside_the_window`,
`a_non_unified_account_makes_no_spend_request`,
`a_reading_outside_its_period_publishes_no_pool_and_keeps_the_window`,
`an_unusable_spend_reading_publishes_no_pool_and_keeps_the_window`.

<a id="vercel-and-vercel-budget-the-team-balance-and-a-personal-budget"></a>

### 7.13 `vercel` and `vercel-budget`: the team balance and a personal budget

Vercel's AI Gateway is read by two providers with two credentials, and neither
credential is ever sent to the other's host. They are separate providers
because the balance carries no account identity: as two credentials of one
provider, the identity-less one would collapse both into one unlabelled entry
([9.1](#an-absent-account-is-never-a-statement-that-the-account-is-gone)).
Each entry fails, goes stale and recovers on its own.

**`vercel`**, from the AI Gateway key (`apikey:vercel` or
`AI_GATEWAY_API_KEY`), sent only to `ai-gateway.vercel.sh`:

```json
"usage": {},
"spend": [
  { "id": "balance", "label": "Team credit balance",
    "funding": "unknown", "basis": "reported",
    "remaining": { "minor": 9550, "exponent": 2, "unit": "USD" } }
]
```

- `remaining` is the team's balance exactly as stated, at the provider's own
  precision. Zero and negative balances are published as they are.
- `basis: reported`: the endpoint states the remainder itself.
  `funding: unknown`: one figure covers whatever credit the team holds, bought
  or given.
- No `total`, `spendable` or `resetsAt`. The endpoint's lifetime spend is not
  the pool's size, a positive balance is not an enable flag, and no period is
  stated.
- There is no account: the key's answer names none.
- A missing or malformed amount is `decode_failed`, never a zero balance.

**`vercel-budget`**, from an access token and team id (`apikey:vercel-budget`
holding `{"token", "team_id"}`, or `VERCEL_ACCESS_TOKEN` with `VERCEL_TEAM_ID`),
sent only to `api.vercel.com`. It publishes the user's personal budget in that
team as `primary`:

| `refreshPeriod` | `resetsAt` | `windowMinutes` | `windowKind` |
| --- | --- | --- | --- |
| `daily` | next 00:00 UTC | `1440` | `daily` |
| `weekly` | next Monday 00:00 UTC | `10080` | `weekly` |
| `monthly` | the 1st of next month, 00:00 UTC | absent | `monthly` |
| `none` | absent | absent | absent |

- `usedPercent` is `currentSpend / limitAmount × 100`, capped at 100.
- `none` is a cumulative budget that never resets. All three fields are absent,
  and absent here means the allowance does not come back by waiting.
- `account` is the Vercel user id; `accountInfo.orgName` is the team id and
  `accountInfo.email` the user's email when Vercel sends one.
- No budget for this user in the team is `no_quota_reported`, never a 0%
  window.
- The remaining allowance is not what can be spent. Team, project and key
  budgets, or the team's balance, can stop requests first.
- **`vercel-budget` carries no `apiProvider`.** `vercel` publishes the
  models.dev slug `vercel`, and slugs are unique per provider
  ([8.3](#which-field-to-key-on)). A consumer that joins on `apiProvider`
  reaches only the balance. To use the budget for the same gateway, join it by
  `provider`, so the two entries are never double-joined as one.

Both providers' failures take the shared classes: no credential is
`credential_absent`; a deposit that is not the documented JSON, a token without
a team, or a team id outside 1-128 letters, digits, `_` or `-` is
`credential_unusable` and sends nothing; 401 and 403 are `credential_rejected`,
and only a 401 on a vault credential is reported to the vault (a 403 is
Vercel refusing permission, for example a Contributor reading budgets); 429 and
5xx are `upstream_failed` and keep the last reading served; an unexpected
answer, ambiguous budgets, non-Boolean flags, repeated cursors and a 21st page
are `decode_failed`. Both are tested against fixtures shaped from upstream
CodexBar v0.79.0, not against a live Vercel account.

**Proven by:** `the_documented_balance_is_one_reported_usd_pool_of_unknown_funding`,
`zero_and_negative_balances_are_published_as_stated`,
`each_period_resets_at_its_documented_utc_boundary`,
`a_cumulative_none_budget_has_no_reset_length_or_period`,
`a_budget_names_the_user_by_bare_or_usr_prefixed_id_only`,
`no_budget_for_this_user_is_no_quota_reported_not_zero`,
`neither_credential_ever_reaches_the_other_lanes_host`,
`a_failure_on_one_lane_never_costs_the_other_its_reading`.

<a id="soniox-an-organizations-prepaid-balance-and-monthly-budget"></a>

### 7.14 `soniox`: an organization's prepaid balance and monthly budget

`soniox` reads two answers with one organization API key (`apikey:soniox` or
`SONIOX_ORG_API_KEY`), sent only to `api.soniox.com`:
`GET /v1/organization/balance`, then `GET /v1/organization/usage/monthly`. A
prepaid organization with a budget publishes:

```json
"usage": {
  "primary": { "usedPercent": 6.0, "resetsAt": "2026-11-01T00:00:00Z",
               "windowKind": "monthly" }
},
"spend": [
  { "id": "balance", "label": "Prepaid balance",
    "funding": "unknown", "basis": "reported",
    "remaining": { "minor": 1850, "exponent": 2, "unit": "USD" } }
]
```

- `primary` is the month's spend against the organization's monthly budget,
  `usage_usd / monthly_budget_usd × 100`, clamped to 0-100. It resets at 00:00
  UTC on the 1st of the month after the month Soniox reports, and carries no
  `windowMinutes` because a month has no fixed length. It is published only
  when the budget is positive: a zero budget has no denominator, so there is no
  window rather than a 0% or 100% one.
- The budget is a spending cap, not money held, so it is never a pool.
- The `balance` pool is the prepaid balance exactly as stated, including zero,
  sub-cent and negative amounts. `basis: reported`; `funding: unknown`,
  because Soniox does not say whether the credit was bought or given. No
  `total`, `spendable` or `resetsAt` is stated.
- An organization on invoice billing publishes `spend: []`: its balance is not
  money it holds, so a zero there is never read as exhausted credit. With no
  budget either, the entry is `no_quota_reported`.
- There is no account: the key's answers name none.
- No key is `credential_absent`; a blank deposit is `credential_unusable` and
  sends nothing. 401 and 403 are `credential_rejected`. A 403 means a project
  key or an organization key without the Balance and spend and Usage and
  limits permissions. Only a 401 on a vault key is reported to the vault. 429
  and 5xx are `upstream_failed` and keep the last reading served. Any field
  Soniox's documented shape does not allow (an amount that is not a decimal
  string, a negative spend or budget, a non-Boolean `invoice_billing_enabled`,
  a `month` that is not `YYYY-MM`) is `decode_failed`, never a zero. A balance
  with more than nine decimal places is also `decode_failed`, because amounts
  are published at most at exponent 9.
- Tested against fixtures shaped from upstream CodexBar v0.80.0, not against a
  live Soniox account.

**Proven by:** `a_prepaid_organization_publishes_its_budget_window_and_balance_pool`,
`invoice_billing_publishes_the_window_and_no_balance_pool`,
`a_zero_budget_has_no_window_and_an_exceeded_one_reads_full`,
`invoice_billing_with_no_budget_is_no_quota_reported`,
`malformed_billing_is_decode_failed_rather_than_zero_usage`,
`each_status_maps_to_its_class_and_only_a_vault_401_is_reported`,
`a_vault_deposit_routes_to_soniox_and_replaces_the_environment_lane`.

<a id="fields-on-the-account-not-the-window"></a>

## 8. Account fields and keys

### 8.1 Fields on the account, not the window

| Field | Meaning |
|---|---|
| `account` | the account id this module **verified** for the fetch; absent when unverified or when the provider exposes no identity |
| `accountInfo` | optional `email`, `orgName`, `planType`, and for Claude `subscriptionRenewsAt` / `subscriptionEndsAt`; descriptive only |
| `source` | the lane that produced the reading ([8.2](#82-source-says-how-a-reading-was-obtained)) |
| `savedResets` | banked quota-reset credits held by this account |

**`accountInfo` is not an identity.** Nothing in this module derives behaviour
from it, and `planType` is the upstream's own label, not comparable across
providers. It can be present while `account` is absent: `account` is withheld
from every account of a provider when a handle that is serving usage resolves
no identity, because that handle may be one of the labelled accounts reached
through a second credential. The single unlabelled entry then published may
carry an email, which names whichever account's numbers were selected, not the
set the entry stands for. Render it; never key, join or resolve identity on it.

**Claude subscription dates.** `subscriptionRenewsAt` is the next charge of an
active or trialing subscription with no scheduled end; `subscriptionEndsAt` is
the scheduled end of an active, trialing or cancelled one, and takes precedence.
Each is an RFC 3339 date-time, or a date `YYYY-MM-DD` when the upstream gives
only a day; preserve that meaning rather than converting it to midnight. They
are billing metadata, not quota resets. They are added to the OAuth entry only
after the deposited claude.ai session's owner and organization match the owner
of that entry's OAuth bearer; the deposit's id suffix selects a candidate but
proves nothing. They are fetched in the background at most hourly per account,
so a cold poll may lack them; a failed fetch leaves them absent or keeps the
last good answer for the same credential, and never changes windows or
`errorClass`. A changed or unverifiable owner withholds them. This path is
fixture-verified only.

**`savedResets`** carries `availableCount` (credits held), `soonestExpiresAt`
(the next expiry) and `credits` (each with its own `expiresAt`). Credits are
granted to one account, never to a provider. Codex credits can be redeemed by
this module ([6.6](#usedpercent-can-be-lower-than-what-the-provider-reported));
Claude's inventory is display-only and counts only grants usable now (paused,
future, expired and ineligible grants are excluded). A credit without a stated
expiry counts toward `availableCount` with no expiry row. **Absent
`savedResets` does not mean zero credits**: the inventory lookup is separate
from the usage fetch and may fail without degrading the entry.
`soonestExpiresAt` absent means no credit has a stated expiry.

### 8.2 `source` says how a reading was obtained

| value | how the credential was obtained | what fixes it when it stops working |
| --- | --- | --- |
| `vault` | served by the credential vault | re-authenticate that vault record |
| `oauth` | an OAuth token or session found on this machine | log in with the tool that owns it |
| `api` | an API key from the environment or a config file | supply or replace the key |

The set is open. `source` is observability only: one account read through two
lanes (a local file and a vault credential) can publish `vault` on one poll and
`oauth` on the next with nothing changed. Never key on it or alert on a change.
`account` must not change with the lane; `source` may.

**Cookie providers publish `vault`.** Providers that read quota from a
logged-in web page take their session cookie only from a vault deposit
([11.5](#depositing-a-session-cookie-which-url-to-capture-against)). A host with
no deposit has no lane for such a provider: it is unconfigured, not a
`credential_absent` entry. `cursor` can also serve from Cursor's own sign-in on
the machine and publishes `oauth` then. The value `cookie`, published by
earlier releases that read browser stores, is retired.

Identity for a provider with two lanes is resolved independently of the lane
that fetched, and only on proof: `cursor`'s deposited session and its local
store each name a user id, and the email is attached only when the two ids are
equal.

**Proven by:** `every_cookie_provider_publishes_the_vault_source_label`,
`identical_readings_publish_a_stable_source_label`,
`adapter_distinguishes_absent_label_from_unavailable_observation`.

<a id="which-field-to-key-on"></a>

### 8.3 Which field to key on

Two fields name the provider, in different namespaces:

- `provider` is this module's own name for the upstream;
- `apiProvider` is the canonical models.dev slug, present only where one
  exists.

Key on the field whose namespace your lookup uses. A key from one namespace
matched against the other matches nothing silently, or worse, lands on a
different real entry:

| `provider` | `apiProvider` | what it is |
|---|---|---|
| `alibaba` | `alibaba-coding-plan` | the coding-plan subscription |
| `qwen-cloud` | `alibaba-token-plan` | the token-plan console |

`alibaba` and `qwen-cloud` are separate upstreams with separate credentials.
Prefer the published
`apiProvider` over a local mapping table.

**Proven by:** `every_api_provider_key_names_a_registered_provider`.

<a id="sibling-keys-on-the-envelope-may-be-invisible-to-you"></a>

### 8.4 Sibling keys on the envelope may be invisible to you

`completeProviders` sits beside `result` on the envelope, and this module adds
envelope keys rather than new operations. A consumer that narrows the response
to the array (a request helper that unwraps `result`, or a mapping step that
passes only `envelope["result"]` inward) loses every sibling key from that
point on.

That is harmless for a display-only consumer and not for one that reconciles
stored accounts, because `completeProviders` is the only thing that authorises
deleting one. Trace one response from the socket to the code that would act on
it and find where the object first becomes an array. If nothing narrows it,
nothing to do; if code you own narrows it, adopting a key is local; if a shared
library narrows it, adoption means changing code other consumers depend on.

<a id="an-absent-account-is-never-a-statement-that-the-account-is-gone"></a>

## 9. Accounts and completeness

### 9.1 An absent account is never a statement that the account is gone

An entry is per account, so a provider can publish some accounts and omit
others in one response, with nothing marking the response as partial:

- **a labelled sibling is withheld**: its identity became unverified, so its
  entry is cleared while the others stay labelled;
- **a new account briefly collapses the provider**: a credential added for a
  second account has no identity until its first fetch completes; if that fetch
  succeeds before the identity resolves, the provider publishes one unlabelled
  entry until it settles;
- **labelled entries beside one unlabelled entry**: one credential of a
  multi-account provider fails while the others serve. The failing one is a
  single unlabelled entry carrying its error and no usage, and its siblings
  keep their names. A handle whose credential was deleted while the handle
  remains configured reaches the same shape and does not clear on its own.

There is never more than one unlabelled entry per provider. Summing capacity is
unaffected (the unlabelled failing entry carries no usage); counting accounts
from entries overcounts, so count entries that carry an `account`. The health
metric `handlesWithoutAccount` names providers in the third shape.

**Do not reconcile by provider.** Key retention on `(provider, account)`,
update the accounts a response names, and leave the rest to age on their own
`fetchedAt`.

**Proven by:** `a_withheld_account_leaves_its_labeled_siblings_published`,
`unresolved_multi_handle_provider_emits_one_unlabeled_entry_then_deduplicates`,
`a_failing_identity_less_handle_does_not_unlabel_its_siblings`,
`one_identity_less_handle_suppresses_the_labels_of_every_other_handle`.

<a id="a-provider-can-leave-the-array-entirely-and-that-is-not-a-fault"></a>

### 9.2 A provider can leave the array entirely

Entries are emitted per credential handle, so a provider that resolves zero
handles emits nothing. The ordinary case is a vault that is wired but has not
answered a listing yet (`vaultHandleState: "awaiting"`, normally one turn
after a restart): every vault-aware provider is held back as unfinished and
publishes no entries, not even from a local lane, because a local credential
usually resolves no identity and would replace every labelled account with one
unlabelled row. Such providers are listed in `withoutHandles`. **Retain, do not
prune**: the accounts are not gone, and a provider that emitted nothing cannot
be named in `completeProviders`.

**Proven by:** `an_error_before_any_install_preserves_an_existing_vault_slot`,
`known_handles_from_a_failed_enumeration_serve_without_claiming_completeness`.

<a id="completeproviders-is-how-an-account-may-be-removed"></a>

### 9.3 `completeProviders` is how an account may be removed

```json
{ "result": [ ... ], "completeProviders": ["codex", "claude"] }
```

A provider named there has had its **whole account set** published in the same
response, so a consumer holding stored accounts for it may replace that set,
including removing accounts the entries do not name. A provider is named only
when all of these hold:

- its latest handle enumeration succeeded;
- every handle resolved an account identity;
- no account was withheld or still pending;
- the accounts emitted equal that inventory as a set (two credentials resolving
  one account are collapsed deliberately, so it is a set, not a count).

A successful enumeration that finds no credentials names the provider with no
entries; that is the only way a provider is cleared outright.

A provider **not** named is **unknown, never empty**. Its entries are as usable
as any other. A cold start names nobody.

- **The survivor set is every labelled entry, including degraded ones.**
  Usability decides which windows you store, never which accounts you keep.
- **Only a consumer performing a destructive action should read it.** A
  display-only consumer that gated on it would hide live data. A consumer that
  cannot see the key has no deletion authority, which is safe.
- **Providers that expose no account identity are never named.** They publish
  one unlabelled entry and never enter an account-keyed store. Expect them to
  be most providers.
- Health cannot substitute: its counts are per provider and a provider with one
  healthy and one withheld account reads as serving.

Reference envelopes for each case come from
`cargo run -p quota-core --example completeness-envelopes`. Pin the pair
genuine-removal and withheld-account: their `result` arrays are identical and
only the claim differs.

**Proven by:** `no_provider_is_complete_before_the_first_turn`,
`a_withheld_account_forfeits_the_claim`, `a_pending_account_forfeits_the_claim`,
`an_identity_less_provider_is_never_complete`,
`a_provider_with_no_credentials_left_is_complete_with_no_entries`,
`a_removed_account_leaves_a_complete_claim_naming_only_the_survivor`,
`an_enumeration_failure_withdraws_completeness_but_keeps_the_entries`,
`duplicate_handles_resolving_one_account_stay_complete`,
`a_degraded_account_is_named_and_keeps_the_provider_complete`; CI also runs the
`completeness-envelopes` example.

<a id="what-whole-is-measured-against-and-where-it-can-fall-short"></a>

#### What "whole" is measured against

The claim is over **the credentials this module could enumerate**: the vault
records granted to it plus its local lanes. A credential the vault holds but
has not granted to this module is invisible here, so the claim can name a
provider while an account you know about is missing. Where a consumer can keep
a tombstone rather than delete outright, the asymmetry favours it: a wrongly
pruned account comes back only when someone notices it is gone.

When a vault listing fails after one has succeeded, the last installed listing
is kept and completeness is computed against it. `vaultEnumerationFailure` is
then non-null and `retainedVaultSnapshotAgeSecs` gives the retained listing's
age. A consumer that prunes accounts can read both to judge how current the
claim is.

<a id="telling-an-identity-transition-from-a-broken-handle"></a>

### 9.4 Telling an identity transition from a broken handle

```
you hold labelled rows for P
AND this response has an unlabelled entry for P
AND that entry is USABLE (no `error`, carries `usage`)
AND this response has no labelled entries for P
=> P currently resolves no identity
```

Your labelled rows are then from an earlier state: not wrong, but undated.
Nothing replaces them, because an unlabelled entry never stands for an account
and a provider serving one is never in `completeProviders`. The third clause
matters: an unlabelled entry carrying an error can sit beside labelled serving
siblings ([9.1](#an-absent-account-is-never-a-statement-that-the-account-is-gone)).
**Mark, do not delete.** If the provider resolves identities again it becomes
labelled and completable, and ordinary reconciliation applies.

<a id="usagedrops--recently-observed-decreases"></a>

## 10. `usage.drops`

A second query operation on the same route. `usage.get` states current state;
a drop is an event, with its own retention rules.

```jsonc
{
  "epoch": "20260825T081500.123456789",
  "oldestRetained": 41,       // absent when the ring is empty
  "next": 57,
  "drops": [
    { "seq": 56, "at": "…", "provider": "codex",
      "account": "291f5165-…",   // absent when the credential resolves no identity
      "observedContinuously": true }
  ]
}
```

Poll with `{"method": "usage.drops", "params": {"since": 56}}`; omit `since` to
get everything retained.

| Field | Guarantee |
|---|---|
| `epoch` | identifies the ring, which lives in memory and dies with the process. A changed `epoch` means resynchronize; a held cursor is meaningless across it. It is formatted from the wall clock when the ring was created, so it is an identifier, not a start time: compare it for change only |
| `oldestRetained` | the oldest `seq` still held. A cursor older than it means records were evicted between your polls; without it an empty page and a lost cursor would look the same |
| `next` | the `seq` the next record will get |
| `seq`, `at` | every record carries both, so a consumer that lost its cursor can re-derive its position from its own log |
| `account` | the same key `usage.get` entries carry; absent when the credential resolves no identity |
| `observedContinuously` | whether the two readings either side were taken across a continuous poll interval |

**It is a gap-closer, not an archive.** The ring holds the newest 100 records;
its job is that a drop between two consecutive polls is not missed. Keep the
durable record yourself. The supervisor, not this module, reports when the
module started (`ck module status insula`).

<a id="what-a-drop-is-and-is-not"></a>

### 10.1 What a drop is, and is not

A drop is an account's used percent going **down** between two readings. It
names an observation, not a cause: a window rollover, a redeemed reset credit,
a goodwill grant, a plan change and an upstream correction look the same.

These do not count as drops:

- a decrease across an account change (two accounts are not one series);
- a decrease on a window that states a continuous `drip` regeneration;
- a decrease under one percentage point;
- a reading whose `fetchedAt` is earlier than the previous one.

**`observedContinuously: false`** means the readings either side were taken
across a gap: host suspend, a fetch blackout, a long backoff, or a switch
between lanes whose readings describe different moments (`antigravity` reads
one account through a live editor, the plugin's sidebar snapshot and a cloud
endpoint). A gap hides magnitude: a drop followed by consumption reads smaller,
and a drop followed by a refill reads as nothing. **Filter on
`observedContinuously` before counting.**

**Collapse by `(provider, account)` before counting events.** Detection runs
per credential, and one account reached through two credentials can emit two
records for one reset. A second decrease for the same `(provider, account)`
within 60 seconds is folded into the first, in both the ring and the
`quotaDropsByProvider` count. Records without an `account` are never folded,
because two identity-less credentials cannot be shown to be one account.

**Proven by:** `a_cursor_that_fell_off_the_ring_is_distinguishable_from_a_quiet_interval`,
`a_new_store_gets_a_new_epoch`, `a_decrease_across_one_interval_is_an_observed_drop`,
`the_same_decrease_across_a_gap_is_not_observed_continuously`,
`a_suspend_shaped_clock_disagreement_is_not_observed_continuously`,
`a_switch_to_an_older_source_is_a_drop_without_confidence`,
`a_changed_account_is_never_a_drop`, `ordinary_consumption_is_not_a_drop`,
`relaxation_engaging_is_not_a_drop`,
`the_fold_window_folds_siblings_and_spares_a_later_reset`,
`the_refresher_attributes_a_drop_to_the_account_it_observed`.

<a id="health-is-a-separate-axis"></a>

## 11. Health

### 11.1 Status

Module health comes from `health.check`, not from the array:

| Status | When |
|---|---|
| `failing` | the slot store is poisoned (a serving or refresher task panicked while holding it) |
| `degraded` | `refresherStalled` or `fetchBlackout` is true |
| `ok` | otherwise |

Per-provider failures never change the module status: most providers lack
credentials on any given host, so the module would be permanently degraded.
`fetchBlackout` reports `degraded`, not `failing`: `failing` asks the
supervisor to restart the module, and the signal states what is true without
acting on a guess about why.

**Proven by:** `stalled_refresher_reports_degraded`,
`a_lane_that_stops_succeeding_for_hours_reports_degraded`,
`a_host_that_never_fetched_anything_is_not_a_blackout`,
`health_check_control_request_returns_domain_report` (`main.rs`).

### 11.2 Provider buckets and the conservation identity

**Health counts providers; the array carries accounts.** A provider with
several accounts contributes several entries to `usage.get` and one to the
buckets. When its accounts disagree, the best one decides: any fresh account
makes it `fresh`, otherwise any stale one makes it `stale`, and it is
`degraded` or `unconfigured` only when every account failed.

```
fresh + stale + pending + len(degraded) + len(unconfigured) + len(withoutHandles)
  == providersTotal
```

`fresh`, `stale` and `pending` are numbers; `degraded`, `unconfigured` and
`withoutHandles` are arrays of provider names, so take their lengths. A
language that coerces (`7 + 0 + [..names..]` in JavaScript is a string) makes
the identity false forever.

- **Alert on `degraded`, not on `unconfigured`.** `unconfigured` names
  providers whose every account is an expected absence, the normal state for
  most providers on most hosts. `degraded` names providers where a credential
  exists and something is wrong with it, the upstream refused it, or the
  response could not be read. A provider with one broken credential and one
  never set up is `degraded`.
- **The identity holds only after the first tick.** Before it no provider is in
  any bucket, so gate the assertion on `lastTickAgeSecs` being non-null.
- **`pending` is ordinary after a start.** The refresher admits a bounded
  number of fetches per turn, so providers wait several turns for their first
  fetch. For "is this provider usable now", read `fresh + stale`, not the
  absence of `pending`.
- The identity holds when every provider is degraded, so it is not a liveness
  signal.
- `handlesWithoutAccount` names providers holding a credential that reaches no
  account while their other accounts serve. It sits outside the identity: such
  a provider is still in exactly one bucket.

**Proven by:** `every_provider_lands_in_exactly_one_health_bucket`,
`conservation_identity_holds_with_stale_episodes_nonzero`,
`a_broken_account_beside_an_unconfigured_one_stays_actionable`,
`a_provider_in_the_fresh_bucket_can_still_publish_a_failed_account`,
`the_health_detail_does_not_conflate_its_three_counts`.

<a id="folding-several-accounts-into-one-answer"></a>

### 11.3 Folding several accounts into one answer

Aggregating across a provider's accounts is a consumer decision, and only two
fields are provider-scoped:

| Field | Scope | Why |
|---|---|---|
| `provider` | provider | the module's own name for the upstream |
| `apiProvider` | provider | derived from `provider` at read time, identical on every entry |
| `account` | account | the credential's account id |
| `accountInfo` | account | that account's email, org, plan, optional subscription dates |
| `source` | lane | set per credential lane, and the lane serving one account can change between polls |
| `fetchedAt` | account | that slot's own last success |
| `savedResets` | account | reset credits are granted to **one** account |
| `usage` (incl. `rawUsedPercent`) | account | measured against that account's own limits |
| `spend` | account | credit is bought per account |
| `error` | account | that lane's failure, not the provider's |

So a merged view must **select a whole entry** and use its fields together.
"Can I use this provider now?" takes the account with the most headroom. "How
pressured is this account?" takes that account's entry and nothing else.

1. **Never average.** Two accounts at 50% are two accounts either of which can
   serve.
2. **Degraded is not zero and not full.** An entry with `error` and no `usage`
   folds as *unknown*.
3. **Never combine account-scoped fields** from different entries. Summing
   `spend` across siblings states a total no single credential can draw on.

This producer publishes no provider-level rollup entry: every field but two is
account-scoped, so a rollup would omit most of the record or state one
account's figures under a provider's name.

<a id="consumption-metrics"></a>

### 11.4 Health metrics

`health.check` publishes these metrics. Ages and counts are since the module
process started unless stated. A metric with value `null` is unknown or has
not happened, never zero.

| Metric | Type | Meaning |
|---|---|---|
| `buildCommit` | string | the commit the running binary was built from |
| `quotaConfig` | object or `null` | the `ck-quota.jsonc` settings in force now, which the module re-reads while it runs: `codexAutoUseResetsSecs` (number), `codexResetsArmed` (boolean), `loadedAt` (RFC 3339 time the config was last taken from the file or from the file's absence, `null` if never) and `lastError` (the latest stat, read or parse failure seen on the file, `null` once a usable file or the file's absence has been applied; the last good config stays in force meanwhile) |
| `providersTotal` | number | providers registered |
| `fresh` | number | providers serving a reading inside the freshness horizon |
| `stale` | number | providers serving a reading preserved through transient failure |
| `pending` | number | providers whose first fetch has not completed |
| `degraded` | array | providers with a credential that is failing ([11.2](#health-is-a-separate-axis)) |
| `unconfigured` | array | providers with no credential on this host |
| `withoutHandles` | array | providers that resolved no credential handle |
| `handlesWithoutAccount` | array | providers with a credential that reaches no account while siblings serve |
| `lastTickAgeSecs` | number or null | seconds since the refresher's last turn; null before the first |
| `refresherStalled` | bool | the refresher's heartbeat is older than its stall horizon (300 s) |
| `lastFetchSuccessAgeSecs` | number or null | seconds since any slot last fetched successfully; null when nothing ever has |
| `fetchBlackout` | bool | `lastFetchSuccessAgeSecs` has passed the blackout horizon (1800 s) |
| `staleEpisodes` | number | times any slot entered stale-serving |
| `staleEpisodesByProvider` | object | `staleEpisodes` per provider |
| `fetchAttemptsByProvider` | object | upstream fetches dispatched per provider since process start, regardless of outcome; excludes `usage.observe` call readings |
| `lastStaleEpisode` | string or null | the most recent episode |
| `quotaDropsByProvider` | object | drops recorded per provider |
| `quotaDropsObservedContinuously` | object | of those, the ones observed across a continuous interval |
| `quotaComparisonsNoDrop` | number | paired readings that were compared and showed no drop |
| `callObservationsApplied` | number | applied model-call observations since process start |
| `callObservationsIgnored` | object | ignored observations counted by the `usage.observe` reasons above; forbidden callers do not enter these counters |
| `quotaNotComparable` | object | readings that could not be compared, by reason |
| `cookieCohortTotal` | number | providers whose credential is a web session cookie |
| `cookieLoginsStale` | array | cookie providers whose deposited login stopped working |
| `usageRequestsServed` | number | requests on this route answered |
| `usageRequestsRefused` | number | requests refused (undecodable body, unknown method) |
| `lastServedAgeSecs` | number or null | seconds since the last answered request |
| `lastRefusedAgeSecs` | number or null | seconds since the last refused request |
| `usageWatchers` | number | currently open `usage.watch` streams (at most 64); cancellation, route close and shutdown release them |
| `usageWatchNoticesDropped` | number | oldest notices evicted from full per-watch queues since process start; sequence gaps tell each reader to reload |
| `vaultHandleState` | string or null | `no_vault`, `awaiting`, `answered`, or null without a vault loader |
| `scopedCredentialIds` | array | vault credential ids in the installed listing |
| `vaultEnumerationFailure` | string or null | the latest vault listing failure |
| `retainedVaultSnapshotAgeSecs` | number or null | age of the retained listing; null when none was ever installed |
| `vaultMappingWarning` | string or null | a warning from mapping the listing to providers |
| `vaultConnectionsEstablished` | number | vault connections opened |
| `vaultAuthFailuresReported` | number | upstream `401`s on vault credentials reported back to the vault |
| `vaultRouteWarmingRetries` | number | vault route opens refused while the vault was starting, then retried |
| `vaultUnmatchedDrops` | number | vault replies discarded because their caller had gone |
| `vaultListWatch` | string | `watching` after an accepted inventory notice; `polling` while disconnected or re-subscribing; `refused` after a refused or invalid watch; `unsupported` after an older vault's `unknown_method`, asked again once after the vault's route reopens (a vault restart or upgrade) and otherwise every 30 minutes. Only `watching` suppresses routine list reads. |
| `vaultListReads` | number | cumulative `credential.list_scoped` calls, including connect, change/discontinuity, confirming-refusal and fallback reads; never reset on reconnect |
| `vaultStaleGenerationDrops` | number | vault replies discarded because they belonged to a previous connection |
| `busPublishState` | object | quota publisher state and short structural reason; see [13](#13-quota-sample-notices-on-ck-bus) |
| `quotaSamplesPublished` | number | acknowledged quota notices since process start |
| `quotaSamplesDropped` | number | quota samples dropped from the bounded queue or failed publish attempts |

**Event counters** (`staleEpisodes`, `staleEpisodesByProvider`,
`fetchAttemptsByProvider`, the
`quotaDrops*` and `quotaComparisonsNoDrop` / `quotaNotComparable` counts) are
not part of the conservation identity: they count events over time, not
members of the current population. They reset on restart.

`fetchAttemptsByProvider` increments once when the refresher dispatches an
upstream fetch attempt, before its outcome is known. It measures upstream polling
cadence by provider; `usage.observe` call readings do not increment it.

**Stale episodes.** `stale` is a gauge; `staleEpisodes` counts transitions into
stale-serving, so a slot stale across many turns counts once, and a transient
failure that resolved between two polls still leaves a trace. A non-transient
failure is not an episode. `lastStaleEpisode` reads
`<provider> <account> <class> [<message>] at <time>`, where the account is
`<unattributed>` when the credential resolves no identity, the class is
`<no class>` when unclassified, and the message is the first 160 characters of
the failure. Only the most recent episode is kept.
`staleEpisodesByProvider` separates one marginal upstream (episodes
concentrated on one provider) from an environmental problem (spread across
many). A provider with a graceful fallback accrues episodes faster than one
with a single lane: `antigravity`'s local editor probe is
`local_source_unavailable` (transient) whenever the editor is closed, while
its cloud lane keeps serving. Episodes beside a healthy served entry are a
fallback working; episodes beside a degraded entry are a lane in trouble.

<a id="quotadropsbyprovider-and-quotadropsobservedcontinuously"></a>
<a id="quotacomparisonsnodrop-and-quotanotcomparable"></a>

**Drop counters.** `quotaDropsByProvider` and `quotaDropsObservedContinuously`
follow the rules in [10.1](#what-a-drop-is-and-is-not), including the 60-second
fold. `quotaComparisonsNoDrop` is the denominator: comparisons that ran and
found no drop. `quotaNotComparable` counts readings that could not be compared:

| reason | what it means |
| --- | --- |
| `account_changed` | the credential now serves a different account, so the two readings are not one series |
| `no_prior_reading` | a cold slot, or an entry carrying no usage |
| `prior_reading_was_an_error` | the previous reading was degraded |

A sustained run of `prior_reading_was_an_error` is a credential problem that
otherwise looks like a quiet account.

**Consumption metrics.** `usageRequestsServed`, `usageRequestsRefused`,
`lastServedAgeSecs` and `lastRefusedAgeSecs` answer whether anyone is reading
this module, which no production metric can. They are not alarmed, and
consumers should not alarm on them: a host with nobody polling is a legitimate
configuration. Read as a pair they separate quiet (neither advancing),
consuming (served advancing) and failing to consume (refused advancing while
served does not).

**Cookie logins.** Read `cookieLoginsStale` against `cookieCohortTotal` as "N of
C logins stale". It names providers whose deposited cookie was sent and
rejected, or whose page no longer parses; a cookie provider with no deposit is
unconfigured and is not counted. It is not the set of degraded cookie
providers.

**Vault client metrics.**

- `vaultHandleState`: `no_vault` means no credential vault serves this module
  (no vault module, or a vault that granted it nothing before ever granting
  anything) and providers use their local lanes; `awaiting` means a vault is
  wired and has not answered a listing yet, so vault-aware providers are held
  back ([9.2](#a-provider-can-leave-the-array-entirely-and-that-is-not-a-fault));
  `answered` means at least one listing was installed, and later failures keep
  it. A host that stays `awaiting` has a vault that keeps failing to list, and
  its bucket counts look like a host with no credentials.
- `scopedCredentialIds` is a readback of the listing the refresher used, not a
  fresh vault query, and carries no credential bytes. A standalone checker
  reads it here because its own daemon connection cannot list this module's
  grant. On a listing failure it keeps the last installed set, and
  `retainedVaultSnapshotAgeSecs` gives its age in whole seconds.
- `vaultMappingWarning` is null when every installed row mapped cleanly.
- `vaultConnectionsEstablished` is `1` while the first connection is in use;
  each increment is a reconnect. There is no idle timeout, so `1` for weeks is
  normal. Do not alert on it.
- `vaultAuthFailuresReported` counts reports that latch a credential as
  needing re-authentication, which only a human login clears. Only an upstream
  `401` on a bearer this module got from the vault is reported; a `403` is not,
  because suspensions, entitlement changes and network challenges all arrive as
  `403`. A zero rules this module out when an account is found latched.
- `vaultRouteWarmingRetries` rising during a daemon restart is expected; rising
  while nothing restarts is worth asking about.
- `vaultUnmatchedDrops` and `vaultStaleGenerationDrops` in small numbers around
  a daemon restart are ordinary; a rising unmatched count while nothing is
  restarting is not.

**Proven by:** `every_published_health_metric_is_documented` (`main.rs`; every
published key against this document),
`a_slot_stale_across_three_turns_counts_one_episode`,
`a_transient_failure_entering_stale_serving_counts_one_episode`,
`a_non_transient_failure_does_not_count_an_episode`,
`a_stale_episode_records_the_account_and_the_class`,
`a_degraded_previous_reading_is_named_as_its_own_reason`,
`only_a_failed_cookie_counts_as_a_stale_login`,
`a_cookie_login_leaves_the_stale_list_once_it_works_again`,
`scoped_inventory_health_keeps_rows_failure_and_retained_age_distinct`.

<a id="depositing-a-session-cookie-which-url-to-capture-against"></a>

### 11.5 Depositing a session cookie: which URL to capture against

A cookie-backed provider reads its web session only from the vault. Cerebellum
captures a login in a throwaway browser and deposits it as
`cookie:<domain>:<account>` (or a bare `cookie:<domain>`). Without a deposit the
cookie lane is unconfigured. A cookie usage lane without in-band account identity
takes **one** deposit; a second deposit for the same domain is refused when the
listing is installed. `claude-console` is the exception: each session resolves an
organization UUID, so several deposits can serve distinct organizations. Two
deposits resolving the same organization produce one row. Account-suffixed
deposits take precedence over a bare deposit.
Re-capturing replaces the same id (`ck auth put --replace`).

The captured value must be the **request** `Cookie:` header
(`name=value; ...`) for the exact URL insula fetches, so host and path scoping
match. Capture tools take the URL from this table:

| Deposit id | Provider(s) | Fetch URL |
|---|---|---|
| `cookie:claude.ai:<account>` | claude | `https://claude.ai/api/account` |
| `cookie:dev.meta.ai:<account>` | muse | `https://dev.meta.ai/api/auth/me` |
| `cookie:platform.claude.com:<account>` | claude-console | `https://platform.claude.com/api/organizations` |
| `cookie:ollama.com:<account>` | ollama | `https://ollama.com/settings` |
| `cookie:opencode.ai:<account>` | opencode, opencodego | `https://opencode.ai/console/api/go/status` |
| `cookie:ampcode.com:<account>` | amp | `https://ampcode.com/settings` |
| `cookie:qoder.com:<account>` | qoder | `https://qoder.com/api/v2/me/usages/big_model_credits` |
| `cookie:qwencloud.com:<account>` | qwen-cloud | `https://cs-data.qwencloud.com/data/api.json` |
| `cookie:xiaomimimo.com:<account>` | mimo | `https://platform.xiaomimimo.com/api/v1/tokenPlan/usage` |
| `cookie:cursor.com:<account>` | cursor | `https://cursor.com/api/usage-summary` |
| `cookie:factory.ai:<account>` | factory | `https://api.factory.ai/api/billing/limits` |
| `cookie:langdock.com:<account>` | langdock | `https://app.langdock.com/api/trpc/usageSettings.getPersonalUsage` |
| `cookie:kimi.com:<account>` | kimi-for-coding | `https://www.kimi.com/apiv2/kimi.gateway.membership.v2.MembershipService/GetSubscriptionStats` |

- **claude**: enrichment only, never a usage lane. Use the same account suffix
  as `oauth:anthropic:<account>`; a bare deposit is not used. Several suffixed
  deposits are allowed because each owner is verified against the OAuth bearer
  through `/api/oauth/profile` and `/api/account` before
  `/api/organizations/{org_id}/subscription_details` is read. Capture a
  `sessionKey` cookie scoped to `/api` so it covers both URLs.
- **kimi.com**: not a lane. `kimi-for-coding` reads usage with its API key and
  uses the session's `kimi-auth` cookie only for two optional subscription
  extras.
- **muse**: enrichment only, never another handle or row. Deposit `llama_dev_sess`
  scoped to `/api` as `cookie:dev.meta.ai` or `cookie:dev.meta.ai:<account>`.
  Insula reads no browser. See the Muse idle-window rules below.
- **claude-console**: sign in at `https://platform.claude.com/login`, then
  capture the request header containing `sessionKey` against the organizations
  check URL above. The first API organization with readable billing is selected;
  `account` is its UUID and `accountInfo.orgName` its name. A billing 403 means
  this login lacks billing access, not that its session needs re-capturing.
- The login page can be any page where the user signs in; for factory it is
  `app.factory.ai` while the fetch is on `api.factory.ai`.
- Langdock and the claude.ai capture URL are fixture-verified only.

insula logs a deposit's first rejection after it has served, with its age
(`deposited cookie ... first rejected after working <h>h<mm>m`): the site's
session lifetime for that capture.

**Proven by:** `cookie_capture_urls_match_the_providers_fetch_urls` (every row
against the provider's source, and every cookie family against the table),
`langdock_replacement_changes_the_single_deposit_and_two_deposits_are_refused`.

<a id="measured-behaviour-is-recorded-here-not-stamped-on-the-wire"></a>

## 12. Observed provider behaviour

Some facts a consumer needs are not stated by any upstream, so they are recorded
here as observations rather than published as wire fields. A wire field would
assert an upstream claim indefinitely; an observation here carries its method.

### Muse: idle subscription windows

Muse's `POST https://api.meta.ai/muse-code/key` omits `subs_usage` while the
5-hour window is idle, even when weekly usage is nonzero. A remembered full
reading for the mint response's `user_email` wins: 5h becomes 0% without a reset,
and the weekly figure stays unchanged until its reset passes (then 0% without
a known next reset). This memory is per process, not durable.

Without a remembered reading, a deposited dev.meta.ai session can supply the
quota of the team that holds the login's subscription. With no team configured,
insula reads each visible team's subscription quota and uses the team whose
`tier` matches the login's `subs_tier_name`, when exactly one does. The session
is already proven to belong to the login's account, so a single plan match is
that subscription. Two or more teams on the same plan are ambiguous: nothing is
served, and their IDs and names are logged once (and again only when that list
changes). Teams beyond the request budget are not guessed at. To choose
explicitly, set the optional string `muse.team_id` in the quota config, for
example `{"muse":{"team_id":"424242424242"}}`; it reloads live on the same
schedule as `codex.auto_use_resets`, and then only that team is read. A web
reading that serves is remembered like a full one, so later idle answers cost
no further requests.

The web lane follows CodexBar v0.79.0:

1. `GET https://dev.meta.ai/api/auth/me`, with only `llama_dev_sess` as the
   credential. Its email must match the mint email case-insensitively.
2. `GET /api/portal/teams`; a configured team's ID must be visible.
3. Only for a blank session email, use `userId` to find the member with the same
   `user_id` in `/api/portal/teams/{team_id}/members` and match that member's email
   to the mint email. When no selected team is visible, the first team may prove
   identity for listing purposes, but is never chosen for quota. A members 403
   ignores the session without expiring it.
4. `GET /api/portal/teams/{team_id}/subscription-quota` for the configured team,
   or for each visible team when none is configured; its `tier` must match
   `subs_tier_name` from the mint response.

Percentages are weighted used / limit, including decimal-string counts, capped
at 100%. A missing 5h reset with zero usage, or a valid reset already passed,
means 0% with no reset. As in the pinned source, zero usage with an explicitly
future reset retains that reset. Malformed resets, nonzero usage without a
reset, a weekly reset not in the future, zero/missing limits, or an invalid
window duration withhold the entire web reading.

The web lane is attempted at most once per Muse fetch, only when remembered
usage cannot serve. It makes at most five requests across deposits, with an
8-second timeout per request; it has no independent poll. Direct email matches
are tried before blank-email sessions. Device tokens go only to api.meta.ai;
cookies go only to dev.meta.ai, and redirects are not followed. A 401, a login
redirect, or a non-members 403 expires the session locally and is logged on
change (including the deposited-cookie lifetime after a successful reading).
Cookie failures are never reported to the vault.

If no lane can supply usage, the Muse row keeps the mint account identity and
reports transient `upstream_failed`, rather than inventing weekly usage. The
web lane also keeps the mint account identity; cookie deposits create no Muse
rows of their own.

**Proven by:** `web_email_team_and_tier_match_publish_windows_with_mint_identity`,
`blank_email_is_bound_by_the_session_users_membership`,
`email_mismatch_withholds_web_reading`, `tier_mismatch_withholds_web_reading`,
`remembered_reading_wins_without_any_web_requests`,
`web_never_receives_device_token`, `mint_never_receives_cookie`,
`team_selection_reloads_on_the_existing_provider` (`muse/web_tests.rs`).

**Anthropic five-hour windows reset at a boundary; they do not slide.** Polling
one account repeatedly within an active window shows its `resetsAt` constant
(sub-second jitter only), where a rolling window would move it forward. Two
accounts on one plan carry different reset instants, which is a per-account
anchor. An account nothing has used carries no `resetsAt`, consistent with a
window that starts at first use. Whether the next window anchors at the next
first use or on a fixed grid has not been observed. Reproduce by polling one
account and watching its own `resetsAt`; never compare `resetsAt` across
accounts, or across arrays from different hosts, as if it were one series.

The boundary reset is deliberately not published as a `regeneration`
mechanic: that object
means the upstream stated how quota returns, and Anthropic states only a reset
instant, which `resetsAt` already carries.

A measurement taken on a host that is itself using a provider is shaped by that
use. State what was driving the lane beside the method, and where nothing was
known to be, say what was checked.

## 13. Quota sample notices on ck-bus

The subject is `ck.{acct}.event.insula.quota_sampled.v1`, built by
`cortexkit-bus-naming`'s `AccountNames::event_subject("insula", "quota_sampled", 1)`.
`{acct}` is the bus account, **not** the quota account. After a complete refresher
turn, insula compares the serving entries per `(provider, account)` and enqueues
at most one changed sample per key. An absent account label is a valid key (for
example, DeepSeek). Entries absent from `usage.get`, including vault lanes held
in `awaiting`, emit nothing. A changed `fetchedAt` counts as a new sample; an
identical turn does not. Notices are a history aid, not an account deletion feed.

### 13.1 Notice and body

The broker carries **no JSON body**. Insula uses `Stream::publish` and waits for
a JetStream `PublishReceipt` in its own drain task. A notice contains:

- message id: lowercase-hex SHA-256 of the canonical identity object
  `{provider, account, epoch, seq}`, omitting `account` when absent. Keys are
  sorted, strings use JSON escaping, and the unsigned integer sequence is exact
  (including values above 2^53);
- digest: SHA-256 of the exact compact JSON body bytes;
- small headers: `kind=quota_sampled`, `provider`, optional `account`, `epoch`,
  and decimal `seq`. The SDK supplies `Nats-Msg-Id` and `Ck-Content-Digest`.

Fetch the body on insula's management route:

```json
{"method":"events_get","params":{"event_key":"<64 lowercase hex characters>","event_name":"quota_sampled"}}
```

The response is `{"result":{"body":{...},"digest":"<sha256>",
"event_key":"<key>","event_name":"quota_sampled","published_at":1234567890000}}`.
`published_at` is the producer's Unix millisecond timestamp prepared before
publishing, so a consumer receiving a notice before the producer receives its
ack can already fetch the immutable body. Verify the notice digest against the
compact JSON serialization of `result.body` (the exact body substring in the
response); do not pretty-print or reorder its fields before hashing.

`body` is the same account entry `Registry::get_usage` serves, with the same
optional-field/absence semantics: `provider`, `apiProvider`, `account`, `source`,
`fetchedAt`, `stale`, `usage`, the `spend` pools, `savedResets`, `errorClass`, and
`error`, **minus `accountInfo`**, plus `epoch` and `seq`. No email, organization
display name, token, or credential id is added to the body or hints. This keeps
the existing `usage.get` wire spelling (`spend`, not a second `pools` field).

Bodies live only in process memory: the newest 16 per quota account key, with
a global cap of 1024 bodies. `event_not_found` means unknown, evicted, or from
an earlier process: take a fresh `usage.get` snapshot. A malformed key gets
`invalid_request`; another event name gets `event_name_mismatch`. This cache is
not a durable history or a replay API.

### 13.2 Sequences, outages, and backstop reads

`epoch` is the **same opaque process run id returned by `usage.drops`**. Compare
it for equality; do not parse or order it. `seq` is a u64, starting at 1 for each
`(provider, account)` per process. Allocation happens at enqueue, so evicted or
failed samples can leave gaps. A skipped sequence within one epoch means a
missed sample: recover with `usage.get`. A new epoch means a restart: discard
old sequence cursors and take a `usage.get` snapshot. Deduplicate repeated ids.

Publishing never gates serving, provider refresh, or credential lookup. The
bounded queue holds 256 samples; overflow evicts the oldest rather than waiting
for capacity. Withdrawn entries are removed from the queue. A missing ckbus
module, refused credential, or NATS outage is normal: insula reports the state,
retries with exponential backoff capped at 5 minutes, and keeps serving. After
reconnection it publishes the current entries again with the next sequences,
including entries unchanged during the outage. Credential renewal rebuilds the
authenticated connection before expiry. Consumers should keep a slow
`usage.get` backstop (10 minutes) as well as snapshots on gaps, restarts and
not-found bodies: the bus is not an exhaustive inventory.

Health metrics (all process-lifetime):

| Metric | Meaning |
|---|---|
| `busPublishState` | `{state, reason}`; `disabled` before startup/after shutdown, `connecting` on startup, `publishing` when connected, or `unavailable` with a short structural reason. State changes are logged once, without credential text. Bus unavailability does not degrade the provider health status. |
| `quotaSamplesPublished` | acknowledged publishes, including idempotent duplicate acknowledgements |
| `quotaSamplesDropped` | samples evicted by overflow, withdrawal, replacement with the current reconnect snapshot, or failed publish attempts (which can be ambiguous after a timeout) |
