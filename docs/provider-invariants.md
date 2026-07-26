# Provider invariants

Properties every provider normalizer must uphold, each recovered from a defect
that shipped. Check them when adding a provider or changing how one maps its
response to windows.

## A successful fetch must carry a window

`Ok(Usage { primary: None, .. })` is the worst outcome a fetch can produce. A
success is stored fresh, so it **replaces** whatever good windows the provider
had, resets its retry state, and reports the provider healthy — while consumers
see no quota signal at all. A degraded entry carries a reason a consumer can
price; a transient failure keeps serving the last good window; an empty success
does neither, and looks fine.

Ask of every path that can return `Ok`: what is the weakest input that still
reaches it? Shapes that have produced this defect here:

- an error envelope alongside HTTP 200 (a `errors` array the response type never
  deserialized, so serde silently discarded it)
- a missing required link in a nested chain
- a filter loop with no post-filter emptiness check: the input collection is
  non-empty, every element is skipped, and the function returns success anyway

Rejecting a response that produced no windows cannot break a working provider,
because there is nothing to lose. This is the one place a new rejection path is
free — but reject **only** the zero-output case. A response carrying one
recognized window beside several unrecognized ones is still usable, and
validating the upstream vocabulary would turn a new limit type into an outage.

Swept 2026-07-25 across all `Ok`-producing normalization paths. Re-run the
enumeration when a provider is added.

## Porting from a reference implementation

Fidelity is to observable meaning, not to expressions. Where our type asserts
more than the reference's does, copying the arithmetic exactly is how you assert
something false.

The reference keeps a minimum remaining percent and a minimum reset time as two
independent scalars, and never claims they describe the same pool. A
`RateWindow` claims exactly that — *this much of this window is used, and this
window resets then*. Reducing both minima faithfully and then pairing them
inside one manufactured a claim the source data did not support: the defect was
in the container, not the maths, which is why it survived a correct port and
passing tests.

The same applies wherever we hold information the reference cannot represent.
When its type cannot distinguish two cases and ours can, matching it exactly
means deliberately discarding what we know.

## An independent pool must stay visible without headlining

Providers that bill more than one pool have two opposite failure modes, and the
obvious fix for one produces the other:

- **Dropped**: an independent allowance never reaches the wire. The account
  reads comfortable while that pool is walled.
- **Wrongly headlining**: an independent pool claims an unnamed
  primary/secondary/tertiary slot. Those slots are read as *the provider's*
  pressure, so a walled secondary pool reports the whole provider as exhausted
  while its main pool still serves.

Named extra windows say both things at once, and are the correct shape whenever
a pool is genuinely independent. `antigravity.rs` is the reference: the native
pool claims primary, external pools are named-extras-only, and secondary is
never emitted.

The over-report is the more expensive error. An under-report is latent — it
costs nothing until the hidden pool is reached. An over-report withdraws
capacity that exists on every routing decision from the moment it ships.

## The percent is load-bearing; the reset is optional

Emit a window from its percent alone. Carry `resets_at` through when the
provider reports it, omit it when absent, and never fabricate one. Dropping a
window because it lacks a reset makes an exhausted provider read as *no signal*,
which is strictly worse than reporting it exhausted with no horizon.

Distinguish these three, which are different facts about the world:

| state | meaning | handling |
|---|---|---|
| absent | the provider never sent it | emit the window, omit the reset |
| present, unreadable | we failed to parse it | keep the real percent, omit the reset |
| present, in the past | the window genuinely rolled | percent may be reported as 0 |

The middle case is the dangerous one: it is a decode failure wearing the costume
of a provider answer. Reporting a number derived from a field we could not read
would announce an exhausted provider as fully available.

## An empty response body is transient, not a decode failure

"The endpoint returned nothing" is a transport or edge condition — classify it
`Upstream` so the refresher keeps serving the last healthy window through the
flap. "The endpoint returned malformed data" or "a required field is missing" is
a genuine `Decode` failure, which is non-transient and replaces the cached entry
with a degraded one.

Getting this backwards makes a healthy provider read as dead on every
intermittent flap. A rejection that names itself with a code is a real provider
answer and still degrades; only a content-free response is transient.

## Never panic on input we do not control

A fetch panic is contained, but classified non-transient — so it clears the
cached window and suppresses the provider for the backoff. A working provider
then reads as *absent* rather than degraded, and a parse bug becomes a lasting
outage.

Two recurring sources here:

- **Byte cursors into `&str`.** Slicing at an arbitrary byte offset panics when
  the offset lands inside a multibyte character, and any provider response can
  carry user-chosen UTF-8. A cursor advancing by bytes must compare and slice by
  bytes; a fixed byte budget must be rounded down with
  `text::floor_char_boundary` before slicing. A match position is already a
  boundary and needs nothing.
- **Wire-supplied lengths.** A length read off the wire can be any `u64`.
  Convert with `usize::try_from` and advance with `checked_add`; treat a length
  that cannot be honoured as malformed input.

## Identity must fail closed

A handle whose account cannot be confirmed must not serve the previous account's
usage. Two traps:

- A fence comparing observed identifiers is a **no-op** wherever the identifier
  is absent by contract, and it fails toward "these are the same" — the
  permissive answer. Some records carry no account id by design, so ask what
  happens when the compared value is missing on *both* sides.
- One identity-less handle collapses a provider to a single unlabeled entry,
  because the read path emits labeled entries only when *every* handle resolves
  an account. A lane that cannot resolve identity therefore suppresses the
  accounts that can.

## A grant needs a guard at every site that makes it

Most checks in this codebase *refuse* something: a malformed payload, an
unusable credential, an unverifiable identity. A refusal is an outcome someone
writes a test for, because it is visibly an outcome.

The read-time banked-reset relaxation does the opposite — it **grants** a claim.
It reports `usedPercent: 0` while the provider reported far more, so a consumer
treats the account as having room. The read still succeeds and nothing errors,
so it reads as a happy path and attracts no rejection vector. When the grant is
unearned, the wire asserts spare capacity that does not exist, in the permissive
direction, and nothing downstream can tell.

Two habits follow, both of which this codebase needed:

- **Read what the nearest guard asserts, rather than noting that a guard
  exists.** A test asserting `!slot.relax_eligible` checks that the flag is not
  *set*; it never checks that an unset flag is not *honoured*. Those look like
  the same check and only one of them is one.
- **Ask whether the condition is tested *at this site*, not whether it is
  tested.** Where a guard exists twice, a search for "is there a test for this"
  returns yes, correctly, and the answer is useless — the named test exists and
  is attached to the other branch. The first question has a misleading true
  answer whenever a twin exists.
- **Guard every site that applies the grant.** The relaxation transform runs at
  two separate emission sites, unlabeled and labeled. A test covering one says
  nothing about the other, and the temptation to guard once and call the class
  closed is strongest right after finding the first gap.

The read path has two emission branches — one for entries carrying an account
label, one for entries without — so **every guard in it exists twice**. Both
grants found so far were guarded at one branch and tested only there: the
relaxation, and the account label itself, which asserts that this usage belongs
to that account and is the most expensive claim in the module to get wrong.

In both cases the tested branch was the simpler one. That is not chance: the
simple path is easier to write a test for, so it attracts the test, while the
richer path carries more information and therefore more consequence. When
reading either branch, diff it against its twin rather than reading it alone —
any condition present in one and absent in the other is either a deliberate
difference worth a comment or a gap.

The cheap way to check either: delete the condition and see which tests redden.
If the only failures are in tests named for something else, the condition is
defended by accident and one refactor from being unguarded.

### The grants this module makes

This list exists so the check does not depend on remembering to run it. Every
row is something the wire asserts that a consumer acts on. **If you add a field
or a transform that asserts anything, add a row** — and if a row has no guard or
no test naming that site, that is the finding.

| the claim | where it is granted | what withholds it | test |
|---|---|---|---|
| relaxed `usedPercent` (0% while the provider reported more) | unlabeled emission | slot opted in **and** is fresh | yes, per site |
| relaxed `usedPercent` | labeled emission | same | yes, per site |
| `account` label (this usage belongs to that account) | unlabeled emission | identity not in flux | yes, per site |
| `account` label | labeled emission | identity not in flux | yes, per site |
| `fetchedAt` (the data was true at this time) | both | set only from a successful fetch, by construction | structural |
| a healthy entry (`error` absent, `usage` present) | both | a fetch that returned windows | structural |

Two of these rows were added *after* deleting the condition and finding nothing
reddened. The table is the defence: a warning asks you to feel differently, a
list asks you to check something.

## Do not add a guard you cannot calibrate

Most rules above say *reject*, *withhold*, or *fail closed*, so the obvious way
to comply is to guard more. That direction has its own cost, and it is the
larger one: **wrongly rejecting a good response turns a working provider into a
broken one, while wrongly accepting a questionable one costs at most one stale
read.**

So weigh the two sides before adding a check:

- When **both** costs are bounded, decline the guard.
- When one side is **unbounded and silent** — an error that persists
  undetectably, like serving one account's usage under another's credential —
  take it, and accept a bounded cost to avoid an unbounded one.

The rule cuts both ways. One that only ever says "do not guard" is a preference
wearing a rule's clothes.

The specific trap this exists to stop: **a guard whose correctness depends on a
value that cannot be measured from this machine must not ship.** Most providers
have no live credential here, so a size cap, a timeout, or a vocabulary of
valid status values "measured with margin" is a guess wearing measurement's
clothes — and one set too low converts a working provider into a failing one
against a payload nobody could observe. Two proposals were rejected on exactly
this ground.

Where a bound is genuinely needed with nothing to calibrate against, make it
absurd rather than tight — orders of magnitude beyond any plausible value — and
say in a comment that it is a safety bound rather than a tuned limit, so nobody
later "optimises" it toward observed sizes.

Rejecting a response that produced **no** windows is the one free case: there is
nothing to lose, so the asymmetry does not apply.

## Testing these

A regression for any of the above must fail *for the reason it names*. Five ways
a suite can be uninformative while looking decisive, only one of which has a
colour that suggests a problem:

1. it passes while unable to fail (a constant function satisfies every vector)
2. it passes while covering only the easy input shape, so the hard shape reads
   as covered
3. it **fails** for a reason unrelated to the defect (a fixture using field
   names the model does not have)
4. it never runs at all
5. it **encodes the defect as the specification** — a passing test asserting the
   wrong behaviour is correct
6. it is **true of its fixture and false in general**, under a name that states
   the general claim

The fifth is the most hostile, because the test is not merely silent about the
broken case: it actively certifies it. Two of the defects behind this document
were pinned that way, so fixing them meant deleting a green test. If a change
you believe in breaks an existing test, establish which of the two encodes the
intended behaviour before assuming it is your change — and when a rule here and
a test disagree, say in the commit message why the test moved.

The sixth is the only one where the suite becomes the **source** of a wrong
belief rather than merely failing to catch one. The assertion is what CI checks;
the **name** is what a human reads, and a name that generalises beyond its
inputs will be believed by the next reader — including its author. Nothing can
ever go red, so the belief survives indefinitely and gets repeated as a
guarantee. Name a test after the case it constructs, and put the general rule in
a comment where it cannot be mistaken for something CI verifies.

A corollary worth holding onto: **when a document and a test name agree, that is
not two sources.** If the document was written by someone reading the suite, it
is one source counted twice, and it feels exactly like corroboration. The only
independent check is the code path.

Watching a test fail proves only that it can change colour. Read the failure
message and confirm it names the mechanism. For byte-level or encoding
regressions, assert the input's own length so a future miscount fails loudly
instead of silently testing nothing.

When auditing coverage, enumerate the **input shapes** a guard must handle and
check each has a test, rather than checking that the guard has tests. And when
sweeping a property across providers, take the population count from the
registry rather than from whoever asked: a sweep that adopts the requester's
count cannot find a missing member.

One more trap specific to this codebase. A module doc and a function doc in the
same file have disagreed about the intended behaviour, and each looked
authoritative alone. When they conflict, the conflict itself is the finding, and
neither half is citable until the upstream contract settles it.
