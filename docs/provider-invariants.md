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

**The rule assumes the percent is identified.** It holds wherever a percent
arrives in a named field, which is everywhere but one: `grok.rs` decodes an
opaque protobuf with no field names, so its percent is recognised by shape alone
— the shallowest 32-bit float that happens to fall in 0..=100 — and any
unrelated in-range ratio can match it. There the reset is not decoration but the
*evidence* that the scan found the right message, since it must appear at an
exact field path a coincidental value will not occupy. Without it, a percent has
unknown provenance and must not reach the wire.

So the precondition is worth stating on its own: **a rule about what to do with
a value assumes you know what the value is.** Where a field is identified
positionally or by shape rather than by name, the corroborating field is load
bearing too, and dropping it silently converts a guess into a published fact.
That divergence is pinned by a test at its own site, so a future sweep of this
rule cannot quietly undo it.

## An empty response body is transient, not a decode failure

"The endpoint returned nothing" is a transport or edge condition — classify it
`Upstream` so the refresher keeps serving the last healthy window through the
flap. "The endpoint returned malformed data" or "a required field is missing" is
a genuine `Decode` failure, which is non-transient and replaces the cached entry
with a degraded one.

Getting this backwards makes a healthy provider read as dead on every
intermittent flap. A rejection that names itself with a code is a real provider
answer and still degrades; only a content-free response is transient.

The empty-body case is handled **centrally**, in `http.rs` `send`: a 2xx whose
body is empty never reaches a normalizer, because every normalizer would fail to
parse it and report `Decode`. Providers do not need their own guard for it, and
adding one is duplicated logic rather than defence in depth. The exception is
`send_raw`, which deliberately applies no classification for callers whose
status and body policy is bespoke — those callers own this decision themselves.

This rule was ratified and then applied to three providers one at a time, over
three separate incidents, while every other provider kept the defect. That is
the general failure and it is worth stating on its own: **fixing instances is
not closing a class.** If a rule here has been applied more than twice without
anyone enumerating where else it applies, the enumeration is the outstanding
work, not the next instance. Where the class has a single choke point, put the
rule there — a rule enforced in one place cannot be half-applied.

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
- **A delimiter that is its own pair.** Stripping surrounding quotes from a
  credential looks total — until the value is a single `"`, which satisfies both
  `starts_with` and `ends_with` because it is the same character answering both,
  so the strip slices `[1..0]` and panics. Use `text::strip_wrapping_quotes`
  rather than writing this again; it requires two distinct characters before
  stripping anything. The general shape is worth recognising: **a test for a
  matched pair must establish that there are two of them.**

Every remaining slice site in the HTML and text scrapers is boundary-safe today,
and they are all safe for the *same* two reasons. Preserve both when writing a
new one, because neither is visible at the slice itself:

- **The needles are ASCII literals.** A `find` result is a boundary, and adding
  the length of an ASCII needle to it lands on another one. A needle containing
  any non-ASCII character breaks that silently, and the panic surfaces at the
  slice rather than at the literal that caused it.
- **Cursor walks stop on ASCII bytes.** A scan that advances while a byte is an
  ASCII digit stops on a byte that is not one, and a UTF-8 continuation byte is
  never an ASCII digit — so the cursor cannot come to rest inside a character.
  A walk whose stop condition admits non-ASCII bytes loses that property.

So the class is currently closed, and it is closed by circumstance rather than
by construction. The one form to avoid outright is advancing a cursor by a fixed
small number of bytes (`pos + 1`) to continue a search: it is correct only while
the byte at `pos` is single-byte, which is a fact about the needle rather than
about the loop. Advance by the needle's length instead.

## Validate what a served value is *used as*, not what it looks like

A reply from another module gets checked field by field, and the fields that
invite checking are the ones a human reads — an identifier, an email, an
organisation name. Those are labels. The field that decides whether anything
works is usually the opaque one, and it attracts no scrutiny precisely because
there is nothing to eyeball.

Credential bytes served by the vault are the case here: they are converted
straight into a bearer, and empty bytes convert *cleanly* into an empty bearer.
Nothing fails until the upstream answers 401 — which is a non-transient class,
so it clears the cached window and reports the account as auth-dead. A momentary
wrong answer from the credential module would be recorded as a dead credential.

So ask what a field is *used as* rather than what it is named. A value converted
into a request header, a URL, or a token needs to be valid **for that use**, and
emptiness is the case that survives every type-level check: it parses, it
converts, it serialises, and it is silently meaningless. Reject it where the
value enters, so the failure is attributed to the source that sent it rather
than to the upstream that rejected it.

## A negative assertion needs a witness that the value was reachable

`assert!(!output.contains(secret))` passes for two reasons: the secret was
redacted, or it was never there. Only one of them is the property under test,
and the difference is invisible in a green run — a formatter that writes nothing
at all, a capture that read no bytes, or a fixture that never carried the value
all satisfy it.

So pair every negative assertion with a positive one on the *same* output, plus
evidence the value really was present in the input. A redaction test should
assert that the safe fields still appear, and that the value is genuinely
retrievable through whatever accessor exposes it.

Redaction has a second trap of its own: a type that *contains* a secret usually
redacts with its own literal rather than delegating to the secret type's
formatter. Those are independent sites, and a test covering one says nothing
about the other. Mutate each site separately — a leak at the innermost type may
be caught only by a test named for something else entirely, which is one
refactor away from covering nothing.

## A wire error string is published, so treat it as output

The `error` on a degraded entry is not a log line: consumers read it, and at
least one stores it. So the question for anything interpolated into one is not
whether it looks sensitive but whether we *chose* it.

The risk is in text we did not write. A dependency's error `Display` may append
context of its own — `reqwest` appends the request URL, which prints a query
parameter verbatim — so a value that never appears in our own `format!` still
reaches the wire. Convert third-party errors at a single site and strip what
the consumer cannot act on; the entry already names the provider.

Upstream response bodies are the other source, and they are deliberately kept:
an excerpt is what distinguishes one upstream rejection from another. That is a
considered trade, not an oversight — but it means a body excerpt must never be
assumed safe to widen.

And where upstream text is admitted, it must be **bounded**. The deliberate
excerpt is capped, but a decode failure carries upstream text too, without
anyone choosing to include it: `serde_json` quotes the value it rejected
verbatim and does not truncate, so a response with a one-megabyte string where
a number belonged yields a one-megabyte published error. The bound belongs in
the type's `Display`, the one point every variant becomes wire text, rather
than at the sites that construct the error — those are the ones that will be
added later without the guard.

The general form: **the cap that matters is the one nearest the wire, not the
one nearest the reader**. A caller-side limit only bounds the text that caller
knows about, and the dangerous text is the text nobody wrote.

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

That second trap is **latent rather than absent** wherever a provider keeps an
implicit local lane alongside vault lanes, and it is worth understanding before
adding a lane to any provider.

It needs two things to fire: the vault lane starts resolving an account, and the
provider has more than one vault lane. Neither is a change to this repository —
the first happens in the credential store, the second when someone adds a second
account — so the defect arrives without any commit here, in a provider that has
worked for months. It has fired once already, and the fix was to make vault
handles *replace* the local lane once any exist.

Which providers this applies to is not uniform, and the answer is a property of
the lanes rather than of the provider:

- A local lane that resolves an account is safe. It participates in labeling
  like any other lane, so nothing is suppressed.
- A provider whose vault records **cannot** carry an account by contract is
  also safe, because no lane will ever resolve one and the provider stays
  legitimately unlabeled.
- The exposed shape is a local lane that can never resolve identity beside a
  vault lane that could start to.

Do not apply the replace-the-local-lane fix pre-emptively to all of them. It is
right where both lanes are the same account and the vault copy strictly
dominates — it owns refresh, so the local token is only ever a staler duplicate.
It is **wrong** where the local lane is a genuinely different account, because
then dropping it removes real usage from the wire rather than removing a
duplicate. Establish which case a provider is in before changing its handles.

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

A grant can also fail by **going silent** rather than by being unearned, and
that direction has no guard to delete — there is nothing to mutate, because the
claim is simply absent. `apiProvider` is keyed by this module's own provider
name, so renaming a provider leaves a stale key that stops matching; the lookup
then returns the same "no counterpart exists" answer it gives for a provider
that genuinely has none, and the canonical name quietly disappears from the
wire. When a claim is optional by design, its absence cannot be distinguished
from its correct absence, so the check has to be that **the thing it is keyed to
still exists**.

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
| `fetchedAt` (this slot last succeeded then — **not** that the entry beside it is usable) | both | set only from a successful fetch, by construction | yes |
| a healthy entry (`error` absent, `usage` present) | both | a fetch that returned windows | structural |
| `apiProvider` (this usage is that upstream's, in a shared vocabulary consumers join spend and pricing on) | both | a mapping entry exists for this provider | key‑drift only |

Two of these rows were added *after* deleting the condition and finding nothing
reddened. The table is the defence: a warning asks you to feel differently, a
list asks you to check something.

The `fetchedAt` row is worded carefully because the natural reading is wrong.
The timestamp survives the failure that degrades an entry, so a degraded entry
carries the time of the success *before* it — dating content that is no longer
there. It therefore grants a fact about the slot, never about the entry's
usability, and reading it as the latter is a claim this module does not make.

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

A regression for any of the above must fail *for the reason it names*. Six ways
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

The check that finds it is narrower than "does this name generalise", which
catches too much to be useful — plenty of names quantify over cases the fixture
really does enumerate. Ask instead: **could this fixture have produced the
counterexample?** A name is safe over a fixture that could have failed it. The
dangerous shape is a name asserting an ABSENCE over inputs that cannot generate
the presence, which is unfalsifiable by construction rather than merely
untested. Sweeping every test name here on that question found exactly one
instance, so it is rare — but it produced a false guarantee in a published
contract, which is as far as a defect in a test name can travel.

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
