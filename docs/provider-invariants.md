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

## A clean result must report how much it examined

The same trap one level up: a checker that examined *nothing* reports exactly
what a checker that examined everything and found nothing reports. "No problems"
and "no inputs" are the same sentence, and the second is far more common,
because a checker silently stops finding its inputs whenever the thing it reads
changes shape.

Both live examples in `crates/quota-core/examples/` had this. They call
`get_usage`, which is cache-only by design and serves an empty array until the
background refresher publishes — so a freshly built registry gives them nothing,
and they reported success. One of them describes itself as the live-verification
step before trusting this module as the daily quota source; it had been printing
an empty array. Nothing was broken *in* them, and nothing said so.

So: print the denominator next to the verdict, and exit non-zero when the
denominator is zero. An empty run is a failure to check, not a check that
passed. Enumerate what to examine by destructuring the type rather than by
naming fields, so a slot added to the wire fails to compile instead of being
quietly skipped — a hand-written field list is the usual reason the denominator
shrinks without anyone noticing.

And prove the checks fire: loosen a threshold until real data must trip it, and
read *which* findings appear. A check whose inputs are always absent is
indistinguishable from one that passes.

The same reasoning applies to production code that walks all of an account's
windows, and there it is not only a checker's blind spot: a decision that misses
a slot sees *less* usage than the account has. `model::windows` and
`windows_mut` exist so those sites enumerate in one place, destructured, and
their callers include the read-time relaxation and the banked-reset wall test —
two places where under-counting publishes a walled account as having room. A
provider's own parser is deliberately not a caller: it knows which slots it
fills, and a compile error there would be noise.

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

Local paths are the other thing to keep out. A credential file lives under the
account's home directory, so interpolating its path into a read failure puts the
operating-system username in a published string. Nothing is gained by it:
`std::io::Error` does not name the path it failed on, so the path in such a
message was contributed entirely by our own formatting. Read credential files
through the helper that takes a description instead of a path, so the unsafe
version cannot be written.

One provider drives `reqwest` directly rather than through the shared request
builder, because it needs the final URL after redirects. A provider that steps
outside a shared helper steps outside every rule the helper enforces, which is
worth checking whenever one appears — the reason for going direct is usually
unrelated to the reason the helper exists.

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

The two **labeled emission** rows were added after deleting their conditions and
finding nothing reddened — the guards existed, and the tests that appeared to
cover them were named for the unlabeled branch. The table is the defence: a
warning asks you to feel differently, a list asks you to check something.

They are named rather than counted deliberately. A count here is unverifiable
prose sitting beside a derived table — a reader cannot tell *which* two, and the
sentence turns false the moment a third row is added, with nothing to catch it.
Prose next to a correct table is the least-checked text in a document, because
the table's correctness makes the paragraph beside it look checked too.

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

## An insertion can move a comment off the thing it describes

A doc block belongs to whatever follows it. Insert a function between a block
and the item it documented and the block silently becomes the new item's
documentation, while the original is left bare — no warning, and the diff shows
only an addition.

The result is worse than a missing comment. The orphaned text still reads as
authoritative, so it can attach a constraint to code that does not have it while
removing it from the code that does. In this crate a rule requiring an ASCII
needle — the scan advances one byte past a rejected match, and a multibyte
needle would land inside a character and panic — ended up on a numeric
conversion, leaving the function it constrains undocumented.

Neither end looks wrong afterwards. The inserted function reads correctly with
the block above it, and nothing about the newly bare function looks changed, so
reviewing the commit that caused it would not surface it.

When inserting between existing items, read the block *above* the insertion
point and confirm it describes what still follows it. To find existing cases,
look for a doc block naming something other than what it is attached to, or one
whose paragraphs open two different subjects.

## A fixture records whether its values were observed or invented

Parser tests need input, and the fastest way to get it is to write some. That is
fine for exercising a branch, but the values in a hand-written fixture are
*inventions* — plausible-looking strings chosen to make a case reachable — while
the values in a captured one are *evidence* of what an upstream actually sends.
A reader cannot tell the two apart afterwards, because both are string literals
in the same file, and the invented one often looks tidier.

That matters when someone treats a fixture as a record. This crate's local
Antigravity fixtures carry bucket identifiers of two origins: `gemini-5h`,
`gemini-weekly`, `3p-5h` and `3p-weekly` appear in captures from a real local
server, while `g-5h`, `g-weekly` and `gemini-session` were written by hand to
exercise reset and cadence handling. Asked which identifiers this provider
publishes, a reader scanning the tests would produce all seven with equal
confidence — and a consumer building an extractor on the invented ones would be
pinning shapes no upstream has ever sent.

So say where a fixture's values came from, and say it where the values are:
captured from a live response, copied from a reference implementation, or
invented for the case. "Live-verified" on the test *function* does not answer it,
because a test can drive a real code path with made-up input.

The same distinction governs what a producer can promise a consumer about a
published string. Values this module composes are stable because it controls
them, and a change can be announced before it ships. Values passed through from
an upstream are observed rather than planned: a change arrives on the wire first
and is noticed afterwards. Both kinds sit side by side in the same field —
`extraRateWindows[].id` is composed on one Antigravity lane and passed through on
the other — so the guarantee is a property of the lane, not of the field.

## Testing these

A regression for any of the above must fail *for the reason it names*. Eight ways
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
7. it hands a gate an input the test built, so the **derivation** that produces
   that input in production is never exercised
8. its fixture models a state the producer cannot emit, so a later check that
   would catch a real defect fails against the fixture and reads as too strict

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

The seventh is the one that survives the most scrutiny, because **both halves
look tested**. The gate has tests proving it reacts correctly to each value, and
the derivation runs somewhere in the suite; what nothing covers is that the
derivation produces the value the gate expects. A test that constructs its own
input cannot cover a derivation, however thoroughly it exercises what happens
afterwards. Two decisions here sat undefended that way — the auth-failure report
that tells the credential store a credential died, and the threshold that spends
a banked reset credit — because every test handed the gate a pre-built value. Ask
which tests pass an input that production *computed*, not one the test chose.

The eighth does no damage on the day it is written: the fixture is legal and its
test is honest. The damage is to the **next** check. When someone adds a check
for a field the fixture never sets, the fixture goes red, and the natural reading
is that the new check is too strict — weaken it, and it is permanently disabled
for every test built on that fixture while still reading as coverage. A fixture
is a claim about what the producer can emit, so a fixture modelling an
unreachable state pre-emptively neuters checks on that field. When a new check
reddens an old fixture, establish which one is wrong about production before
touching either.

When auditing coverage, enumerate the **input shapes** a guard must handle and
check each has a test, rather than checking that the guard has tests. And when
sweeping a property across providers, take the population count from the
registry rather than from whoever asked: a sweep that adopts the requester's
count cannot find a missing member.

An assertion over a **growing** set must enumerate it rather than sample it.
Checking that each key you thought of is present will not notice the one added
later, nor the one that silently stopped being published — compare the whole
sorted set, so both directions fail.

One more trap specific to this codebase. A module doc and a function doc in the
same file have disagreed about the intended behaviour, and each looked
authoritative alone. When they conflict, the conflict itself is the finding, and
neither half is citable until the upstream contract settles it.

### A sweep for what is missing fails by finding more of it

Several rules here were found by sweeping every provider for something that
should be present. Those sweeps have a failure mode worth stating on its own:
**every bug in a detector that hunts absence produces more apparent absence.** A
pattern that is too narrow, an input that was truncated, a file extension left
out of a list — each removes evidence rather than adding it, so the detector's
errors are shaped exactly like its findings and cannot be told apart by reading
the output.

Three have happened here. One cut each file at the first `#[cfg(test)]`
attribute and reported a provider as missing a call it makes twenty-four lines
further down. One searched for filenames with an extension list that omitted
`md`, and reported every document as unmentioned. One required a `docs/` prefix
when the index it was checking lists bare filenames, and reported five of nine
documents as unlinked when all nine are.

A detector hunting absence has no failure mode in the opposite direction, so
"it found something" carries no information about whether it works.

All three share a narrower cause worth naming separately, because a denominator
does not fix it: **the detector encoded one spelling of the thing it was looking
for, and a correct implementation written differently read as absence.** An
extension list, a path prefix, an attribute where a module was meant — each is a
guess about how the target is written, and the guess is invisible in the output.
When a sweep reports something missing, read that file before believing it.

The cheap defence is to **run the sweep under two or three spellings a different
author would plausibly have chosen, and treat any disagreement as the population
the pattern was blind to.** A control written by the same person who wrote the
pattern is drawn from the same vocabulary, so it passes for exactly the reason
the pattern was wrong. Asking which files call a particular function here gives
9, 8 or 10 depending on whether the needle is the bare name, the name with `fn`,
or the trait that declares it — one number looks equally confident at any of
those values, and the spread is the only thing that reveals the assumption.

It narrows the class rather than closing it: spellings that share a premise
share its blind spot, and all three of those assume the target appears as a
literal substring at the call site.

And it helps least exactly where the stakes are highest. **When the expected
answer is zero, a broken pattern returns the answer you wanted.** Every security
sweep here has that shape — does any provider interpolate a credential path into
a published error, does any lane send a bearer over plain http — and a detector
that matches nothing because its pattern is wrong is indistinguishable from a
clean result. The denominator is right, and several equally broken spellings
agree with each other at zero.

So a zero-expected sweep has to **borrow a non-zero answer from somewhere**, and
the cheapest source is history: run the detector against the commit before the
fix that removed the last instance. `ff845ec^` still interpolates three
credential paths into wire errors; `d65444f^` still publishes an unstripped
request URL. **Every defect ever fixed in this repository is a positive control
lying around for free**, and the commit that fixed it is the pointer to its own
control.

The control has to be read as a **difference, not a presence**. A detector that
fires before the fix has proved nothing if it also fires after — the pattern may
be matching something incidental that the fix never touched, and a count that is
the same on both sides looks like evidence while discriminating nothing. Compare
the two counts:

| control | pre-fix | post-fix | at HEAD |
|---|---|---|---|
| a credential path interpolated into a wire error | 3 | 0 | 0 |
| a `reqwest` error stringified into a published message | 1 | 0 | 0 |

And the pattern must name the property rather than a word that happens to appear
in it. A string can already be in use elsewhere for an unrelated reason, in which
case the sweep is counting the string and not the thing.

And the same sweep has a **mirror failure that is quieter**. Cutting a file at
its test module leaves behind `#[cfg(test)]` applied to individual items — a
test-only constructor beside the real one, an injection hook, a helper accessor.
Six of those live in this crate, across four files. A sweep asking whether every
provider does X can then find X in a test-only helper and call the file fine.

The two directions are not equally visible. Under-including reports something
ABSENT that is present, which reads as a finding and gets investigated.
Over-including reports something PRESENT that exists only under `cfg(test)`,
which reads as a null and gets nothing. **A sweep hunting a missing call stops
looking as soon as it finds one**, so this direction is never revisited.

`scripts/prod_body.py` names the files carrying such items rather than trying to
remove them, because removing them needs the span each one covers and a
brace-matched span is a second guess stacked on the first. The warning narrows
where to look; it does not answer. Re-checking a sweep against it flagged a call
twenty-four lines below a test-only constructor, and reading the source showed
the constructor had already closed — the call was production and the original
result stood.

**Print the positive count beside the negative.** "9 of 9 indexed" is checkable
at a glance; "9 missing" is not, because the denominator is invisible and a
broken detector reports the whole population. Then prove the detector can be
satisfied at all: run it against an input where the thing genuinely is present,
or against a commit from before the fix that introduced it.

### The mutation proof needs its own vacuity check

Deleting a guard to watch a test go red is the strongest evidence in this
document, and it has the same failure mode as everything it is used to check: it
can pass while proving nothing. The mutation may land somewhere other than
intended — a replacement anchored on a string that appears in several places
edits the wrong one — and the target test stays green because the code under it
never changed. That reads exactly like a defended guard.

So the mutation must redden the test it was aimed at, by name. A mutation that
reddens *something else*, or nothing, has told you about your edit rather than
about your coverage. Confirm which test failed, not that the suite did — and
after restoring, confirm the file is byte-identical to the original, because a
partially reverted mutation is a silent behaviour change wearing a clean
`git status`.

The same doubt applies to a **control that has never fired**. The live checkers
in `examples/` have reported `findings: none` on every run they have ever made,
which is the expected result and also exactly what a checker that cannot report
would print. A rule's unit tests do not settle it: they prove the rule
classifies, not that a finding survives assembly, printing and the exit path.

Prove it the same way — invert one rule so live data must violate it, and check
both halves of the output contract. Doing that here surfaced something the
unit tests could not: the finding text and the count printed correctly, but the
first attempt reported success because the exit status was read from a pipeline
rather than the process. The rule was fine, the reporting was fine, and the
measurement was wrong — which is the failure this whole section is about, one
level further out.

One level of checking-the-checker is enough in practice. The regress is real but
every defect found this way has been at the first level, and a control proven
once to fire does not need re-proving unless its reporting path changes.

### A hand-rolled scan of this crate needs its boundary checked first

Sweeps across every provider are how several of the rules above were found, and
the scan that performs one has its own way of lying. Twice now a sweep here has
reported a provider missing something it had, because the extractor cut the file
at the first `#[cfg(test)]` attribute rather than at the test module.

That attribute appears on production items in this crate — a test-only
constructor beside the real one, a `thread_local!` used for injection — so the
cut can land a third of the way into a file and silently discard everything
after it. `codex.rs` truncates at 32%, which is enough to hide most of what a
provider does.

The failure is one-directional and that is what makes it convincing: the scan
reports things ABSENT that are present, never the reverse. A false "missing"
looks exactly like a finding, so it is investigated and then explained away as a
false positive — while a real gap in the same run is indistinguishable from the
noise.

So anchor on the module boundary (`#[cfg(test)]` immediately followed by
`mod tests`), and before believing any sweep, print how much of each file it
actually read. A scan reporting on 32% of a file is not a sweep with a caveat;
it is a different question with the same name.

Writing that down twice did not stop it happening twice, so the boundary now
lives in `scripts/prod_body.py` and sweeps should go through it:

```sh
./scripts/prod_body.py --grep-missing report_auth_failure crates/quota-core/src/*.rs
```

It prints the production body with the test module removed, and warns on stderr
for any file where the naive cut would have differed — which is the half that
matters. Fixing the anchor is silent, and the next person to write an extractor
gets no signal that theirs is wrong; a scan that says it read 5% of `lib.rs`
cannot be mistaken for one that read all of it.

Five files here truncate under the naive cut, and the worst is not a provider:
`lib.rs` cuts at 5% (line 78 of 977), on a `#[cfg(test)] thread_local!` used to
inject a hook. That file holds the emission gate, the completeness claim and the
whole read path — so any sweep over the module's most consequential logic read
essentially none of it.
