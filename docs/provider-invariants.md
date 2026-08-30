# Provider invariants

Properties every provider normalizer must uphold, each recovered from a defect
that shipped. Check them when adding a provider or changing how one maps its
response to windows.

**This document OWNS producer behaviour and engineering method** — what a
normalizer must do, how failures are classified at their construction sites, and
how work here is tested and proved. It does NOT own the wire: where it states what
a field MEANS to a consumer, `consumer-contract.md` is authoritative and this one
is quoting. Declared because two documents that disagree have no oracle between
them, and naming which loses converts a contradiction from a question into a
verdict.

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

## The enumeration of what we receive and discard

Five defects in one day shared one shape: **the upstream told us something and
the normalizer dropped it.** amp's replenishment rate flattened into a fake
cutoff, openrouter's grant left off the pool, cursor's identity lost to lane
precedence, opencode's stated null unread, codex's email in a claim nobody
looked at. Fixing five instances of a rule without enumerating where else it
applies is the outstanding work, not the sixth instance.

So: every field this crate deserializes and never reads. 32 of them.

- **Live providers are clean, with reasons already at the site**: codex's
  `unlimited` (the pool shape already states an uncapped pool by omitting
  `total`), deepseek's `total_balance` (publishing it beside its two components
  would double-count).
- **One false positive**: qwen-cloud's `msg` IS surfaced, through
  `v.get("msg")` on a raw `serde_json::Value` rather than the typed field. The
  sweep keys on `.field` access, so **every read through an untyped Value is
  invisible to it** — worth knowing before trusting its next run.
- **One real finding**: zenmux states `used_flows` and `max_flows` beside the
  percentage, and both were discarded. Those are exactly `usedCount`/`totalCount`
  on the wire; without them a consumer asking "how many requests are left" must
  multiply a percentage by a total it was never given. Now mapped, integral-only.
- **The remaining 29** are in providers with no credential on this host
  (neuralwatt 17, zenmux's siblings, minimax, doubao). Unverifiable here, which
  is the same gate that blocks everything else about them.

The asymmetry worth keeping: codex and deepseek carry their reasoning at the
field, so a reader can tell a decision from an oversight. zenmux carried
nothing, which is why it took a sweep to find. **An unread field with no note is
indistinguishable from a forgotten one** — if you decide not to map something,
say so where the field is declared.


## A browser session is a fallback, not a requirement — check the app first

Nine providers here read a browser cookie, and that was treated as their nature
rather than as one available surface. It is not: `cursor` was dark on this host
with only anonymous analytics cookies, and it now serves a labelled account from
a token in Cursor.app's own `state.vscdb`, with no browser involved.

The shape generalises to any provider whose vendor ships a desktop app or CLI:

1. Does the app keep a credential locally? Editor-family apps commonly use an
   SQLite `state.vscdb` under their global storage; CLIs commonly use a dotfile.
2. If so, read it **immutably** (`file:...?immutable=1`) so a running app is
   never disturbed and no lock is taken.
3. Clear the rotation gate BEFORE building anything: an ACCESS token may be read
   and used, a REFRESH token must never be exchanged. Anthropic and OpenAI rotate
   refresh tokens single-use, so a second reader breaks the user's sign-in to
   collect a usage figure. Google does not rotate. This is what killed the Claude
   and Codex plugin lanes and it is the first question, not the last.
4. Prove the whole path live before writing code — synthesize the credential the
   app would send and call the endpoint the provider already uses.

**This cannot be swept from one machine, and the distinction matters.** Checking
this host found Cursor.app installed and nothing else from that cohort: no Amp,
Qoder, Factory, Ollama, Windsurf or Trae, as apps or CLIs. That is "no install to
inspect", NOT "no local store exists" — the two look identical in a sweep and
mean opposite things.

So the trigger is per-host rather than per-round: **when a provider's app turns
up installed on a machine this module runs on, inspect its store before accepting
that a browser session is required.** A provider marked cookie-only is marked
that way because of where it was last looked at.

## An absent field is not the same as a stated absence

A payload can fail to give you a value two ways, and they need different error
classes because they send a reader to different repositories.

**The upstream STATES the absence.** An explicit `null` in a value slot, a
present-but-empty list, a flag saying there is no plan. Nothing is broken and
nothing is actionable: it is a fact about the account. `no_quota_reported`.

**We LOOK FOR a key and do not find it.** Absence here is indistinguishable
from a rename on their side or a misspelling on ours, and `decode_failed` is
precisely the class that says *our parser and their payload disagree, look at
this repo*. Reclassifying it to an account fact files our own defect as a true
statement about the user, where nobody will look for it.

The distinction is worth stating because the error MESSAGES read the same. A
sweep of this repo found 25 `Decode` sites whose message names an absence, and
the wording separates none of them; the code does. Ask whether serde already
succeeded and a field was then found missing, or whether the payload
affirmatively said there was nothing.

Three fixed instances, all where the upstream stated it:

- `opencode` subscription: an explicit `null` in the server-function envelope,
  which is a JavaScript statement rather than JSON and parsed as neither. The
  caller retried as a POST, that retry answered 500, and a workspace with no
  subscription was published as an upstream failure for weeks.
- `opencode` workspaces: the same shape one call up, yielding no ids and
  published as our missing-workspace-id parse failure.
- `gemini` and `antigravity` remote quota: `buckets` absent and `buckets: []`
  shared one branch. The empty list is the upstream stating the account has no
  buckets; the absent field could be a rename. Now separate, and this one needed
  no knowledge of what any account receives -- only of what the JSON says.

And one deliberately NOT changed, which is the boundary: `kimi` looks for a
`FEATURE_CODING` scope in a list and reports `Decode` when it is not there.
That reads like the same defect and is not. No account on this host carries that
credential, so nothing here can establish what an account without the scope
actually receives -- and if the scope were ever renamed, `Decode` is the answer
that sends someone to fix it while `no_quota_reported` would say there is
nothing to fix. A guard whose correctness cannot be measured from this machine
must not ship.

The class also matters beyond the wire: `decode_failed` counts toward the stale
browser login metric, so misfiling an account fact as a parse failure tells an
operator to re-authenticate a session that works.

### Running a mutation proof: use the tool, do not hand-roll the restore

`scripts/probe.py` applies a mutation, runs the tests, and restores — staging
first so the restore lands on your work rather than on HEAD, in a `finally` and
from signal handlers so no exit path leaves the tree mutated, and classifying the
outcome into reddened / undefended / NOT REACHED / HUNG.

**Do not hand-roll this with `git checkout --`.** That command restores from the
index, so with uncommitted work in the tree it reverts the whole file, not the
mutation. A worker here hit exactly that on 2026-08-16: its implementation
vanished at the first restore, it re-applied the change by hand after each of
five proofs, and the delivery's "verified byte-identical" became a claim rather
than a checked fact. Every one of those proofs then ran against a hand-rebuilt
copy of the code under test.

Note where the failure actually came from: this repo had the hazard written down
twice, and the three task prompts issued that day *instructed the unsafe method
by name* — "confirm the restore was byte-identical via `git checkout --`". The
worker followed its instructions correctly. **When a tool exists to make a
procedure safe, name the tool at the point of use; describing the manual dance
teaches the hazard to whoever reads it next**, and a prompt is read far more
carefully than a doc.

If you must mutate by hand — a file the tool cannot target, a multi-file change —
then `git add` first, and verify the restore with `git diff` against the staged
state rather than assuming.

### A mutation tool must survive the signal, not just the exception

Both tools here neutralise a piece of source, run the suite, and put it back.
The restore has to survive every way the process can end, and `finally` does
not do that: Ctrl-C arrives as an exception and unwinds, while SIGTERM
terminates without unwinding. A `timeout` around either script -- or any
supervisor killing it -- leaves the mutation applied.

That is how a dead checker rule reached master: `audit-checker-rules.py` was run
under `timeout`, killed between the mutation and the restore, and committed by a
`git add -A`. **A neutralised rule is silent.** The suite stays green, the
checker prints `findings: none`, and nothing distinguishes a rule that found
nothing from one that cannot fire. Only a test pinning that specific rule caught
it.

Both scripts now restore from signal handlers as well, and both proofs were run
non-vacuously -- on a clean tree, confirming a mutation was actually applied
mid-run before the kill. The first attempt proved nothing: the script refused on
a dirty tree and never mutated, so the clean result after the kill was the
result of nothing having happened.

Check when adding any tool that edits tracked files: does the restore run on
SIGTERM, and has that been demonstrated with the mutation verified present
first?

### The enumeration, so nobody runs it twice

Every optional collection this crate deserializes -- 13 of them -- checked for
whether an empty one reaches an error path:

- **Fixed**: `gemini` and `antigravity` `buckets`.
- **Correct already, nine of them**: an empty collection is treated as nothing
  to add rather than as a failure.
- **`warp` `errors`**: inverted semantics, an empty list means no errors, and it
  is handled that way.
- **`deepseek` `balance_infos`**: folded on purpose, arguing the OPPOSITE
  asymmetry, and the reasoning is at the site. For money, wrongly reporting "no
  balance" makes a consumer stop routing, while wrongly reporting a decode
  failure costs a retry. The direction of the cost is what decides this, not the
  shape -- so the same conflation can be right in one provider and wrong in
  another.
- **`sub2api` `rate_limits`**: folded, and refused rather than fixed. Three
  causes share one message and no credential on any machine here can say which
  is live. Noted at the site with what to do if the provider is ever configured.

The instrument that produced this list was wrong on its first run: it scoped to
LINES containing the field name, and error arms sit on the lines after. It
reported clean for the two providers fixed an hour earlier. A verdict from a
check that misses its known cases says nothing about the unknown ones -- so put
a case you have already established into any sweep before believing its nulls.


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
- An identity-less handle **that is serving usage** collapses a provider to a
  single unlabeled entry, because it may be one of the labeled accounts reached
  by another credential and publishing both would state that account's capacity
  twice. A lane that cannot resolve identity therefore suppresses the accounts
  that can — but only while it has usage to contribute. One that carries an
  error publishes as a single unlabeled entry beside its labeled siblings, since
  an entry with no usage cannot be double-counted.

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

## A best-effort call that has never succeeded looks like one that found nothing

Several providers enrich a resolved usage snapshot from a second, optional
endpoint. The pattern is deliberate: the windows are already in hand, so a
console that is unreachable or logged out must not turn a good fetch into a
degraded entry. `if let Ok(body) = second_call().await` is the shape.

It has no failure signal at all. A call that is rejected on **every** fetch
produces exactly what a call that succeeds and finds nothing produces: no extra
window, no degraded entry, no failed test, and a provider that looks healthy
because it is. `kimi-for-coding` sent its coding API key to a web console that
authenticates with a browser session, so the enrichment had been rejected on
every fetch since it was written and two windows had never once reached the wire.

**The tell is that the two endpoints take different credentials.** One surface
with one credential is fine; the moment a provider reaches a second host, or a
second product surface on the same host, ask which credential that surface
accepts rather than reusing the one already in hand. Reusing it is the natural
thing to write, because it is right there and the call compiles.

Sweeping for the shape is cheap — look for a discarded `Result` from an awaited
request — but the sweep only lists candidates. Deciding each one means comparing
the origin against the credential. Of the three sites in this crate, `kimi.rs`
sends its web token to two paths on the same web host and is correct; the one
that crossed a credential boundary was the defect.

And the fix is not to make the call loud. It is to **skip it when its credential
is absent**, so a host with no browser session makes no request at all, and a
request that does go out is one that could have worked.

### The general question, of which the credential boundary is one predictor

A discarded result is safe exactly when **something else would notice its
absence**. That is the question to ask at each site, and it is wider than the
credential tell: a call can be permanently broken for reasons that have nothing
to do with credentials, and the same silence follows.

Applied across this crate, the discarded results fall into three groups and only
the first was ever at risk:

- **Optional enrichment** (`kimi_for_coding`, `kimi`) — nothing notices. The
  windows it would add are the only evidence it ran, so permanent failure and
  nothing-to-report are the same observation. This is where the defect was.
- **Parse fallbacks** (~40 sites, `if let Ok(dt) = parse_rfc3339(..)`) — the
  fallback chain notices. Each is followed by another attempt and ultimately by
  a `None` that the caller acts on, so a permanently failing branch shows up as
  an absent reset or an absent percent, which the wire already reports.
- **Fire-and-forget reports** (`report_auth_failure`, six providers) — an
  independent path notices. The report tells the credential store a login is
  dead, and it is spawned with no return value, so a permanently failing report
  is invisible at the call site. But the same 401 that triggers it also produces
  a `credential_rejected` entry on the wire through a separate path, so the
  operator-visible signal does not depend on the report landing. The report is
  an optimisation on top of a signal that stands alone.

The third group is the interesting one, because it looks exactly like the first
at the call site — a spawned task, no result, no test. What makes it safe is not
visible from the code that discards it, which is the argument for asking the
question per site rather than pattern-matching on the shape.

**The noticer must not be downstream of the same break.** A backstop that fails
whenever the thing it backstops fails is not one. The auth reports pass because
the report and the wire entry both descend from a single 401 but travel
independently: the report goes to the credential store, the entry goes to the
next read, and neither is on the other's path.

Answering "what would notice" is easier against a list of shapes than from
scratch, and there are three here:

| shape | the noticer | example |
|---|---|---|
| control flow | the fallback chain itself — failure funnels into a value the caller acts on | a reset that stays `None` and is omitted from the window |
| independent signal | a second observation derived from the same event by another route | a 401 that lands as `credential_rejected` whether or not the report is delivered |
| counter or reclaim | an explicit metric, or a path that cleans up regardless | not used here; the shape a teardown call needs, since nothing downstream reads its result |

A site matching none of the three is where to look, and optional enrichment
matches none by construction: the data it adds is the only evidence it ran.

**The claim that a noticer exists is itself a discarded result until someone
reads it.** It is a statement about code that often lives somewhere else, and it
feels settled the moment it is written down — a deferral with a reason attached
reads as adjudicated whether or not the reason was checked.

The auth reports are the worked example, and reading the other side changed the
answer. For OAuth records the backstop is real and better than assumed: the
credential store's refresh path marks a record as needing re-authentication when
the provider rejects the refresh token, with no consumer involvement, so a dead
record is retired whether or not any report is delivered. **For static API-key
records there is no refresh adapter and therefore no refresh attempt**, so
nothing produces that verdict and the report is the only thing that retires the
record. One credential class is covered by an independent path and the other is
not, from call sites that look identical here.

Enumerating every writer rather than the one nearest to hand is what makes that
checkable. Retirement is derived from record invalidation, and every automatic
invalidation in that store sits on the refresh or rotation machinery: the
invalid-grant verdict, the interrupted-rotation corruption guard (which reads the
OAuth block and is skipped when there is none), and two crash-window
reconciliation paths that take a refresh intent as their argument. A static
API-key record never acquires a refresh intent, so it cannot reach any of them.
**For that class the complete set of retirement mechanisms is the report or a
human running an admin command.**

The report is not a single shot, which softens the exposure without closing it.
It is fired from inside the fetch path on every rejected fetch, and a rejected
credential is a non-transient failure, so the slot retries on a fixed five-minute
backoff and reports again each time. A transient delivery failure therefore
heals on the next attempt. What does not heal is a report that fails for a
*persistent* reason — the store unreachable for the life of the process, or a
rejection it will give every time — because then every retry reproduces it, and
the repetition is not evidence of anything.

That paragraph describes another repository, read at `cortexkit-credentials`
commit `f9f96c2` on 2026-08-07. It is a dependency with no compile-time edge:
nothing here fails if that store changes how it retires records, so the citation
can go stale silently and from this side it looks exactly as authoritative as the
day it was written. Recording where and when it was read does not prevent the
drift, but it turns an assertion into something the next reader can re-check
against a known starting point.

The wire stays honest either way — the rejection is published as
`credential_rejected` from a separate path — so the gap is in remediation rather
than diagnosis. Which is the other half of the question: **notice what?** A
backstop can cover one consequence of a failure and leave another uncovered, and
"something would notice" is satisfied by covering either.

## A guard's precondition belongs in its signature, not beside its call

A check written next to a call holds only as long as nobody adds another caller.
The code you read stays identical while the property stops being true, and
nothing fails to compile. A check the function takes as a parameter cannot be
skipped without changing the signature.

The banked-reset relaxation is the case that matters here, because it is the one
transform that publishes a number the provider did not report: it sets
`usedPercent` to zero and moves the real figure to `rawUsedPercent`. Consumers
pace on the zero, so an ungated application tells a router an exhausted account
is idle. Its eligibility test used to sit at both call sites while the transform
took only the entry — so the slot holding the evidence was not required to reach
it, and a third call site could have applied it with no gate at all. It now takes
the slot and decides for itself.

**This class of finding has no symptom.** Both call sites were correct, every
test passed, and the published wire was honest; the defect was reachable rather
than present. An audit asking "is this correct" returns yes. The question that
finds it is **could a future caller get this wrong without the compiler
objecting**, and that question has to be asked deliberately, because nothing
raises it. It also loses every prioritisation contest against a real defect,
which is the argument for doing it while the file is already open rather than
scheduling it.

### A guard has two sides and they need separate audits

Who may **act** on a privileged value, and who may **create** it. Fixing the
first feels like closing the question, because the consumption side is where the
consequence is visible and therefore where attention goes — but a value that can
be minted without the check is not protected by a gate on its use.

After moving the relaxation gate, the minting side was still a public setter
taking a bare `bool`. One production caller passes a value computed from all five
reset-tick conditions, so it is correct; but the check happens elsewhere and the
boolean arrives stripped of its provenance, so any caller can pass `true` and it
compiles. **Enforced and merely correct are different properties**, and a
call-site audit cannot tell them apart, since both look like one correct caller.

The strong remedy is a witness type: make the value constructible only by the
function that performs the check, so it cannot exist without the check having
run. That was not done here — the flag crosses three struct boundaries and each
would have to thread the witness — and the weaker remedy was used instead, which
is to say at the definition what entitles a caller to set it. The setter's
previous comment described only the mechanics, which reads as an invitation.
Where the cost of the witness is bearable, prefer it: a sentence asks the next
author to agree, a type does not.

### And a third side: where does the reader stand

The relaxation had three separate written explanations — the transform, the
setter, and the section above — and all three are **mint-side**. The consumer
contract, which is the file other teams actually read, did not contain the word
"relax" at all, and named `rawUsedPercent` only in a table about field scope.

That is the predictable direction. Mint-side documentation is written by the
person who understands the mechanism at the moment they understand it best, so
it feels complete while landing where no consumer looks. A published field needs
its explanation **where someone stands when they need it**, which for anything on
the wire is the contract rather than the code.

The stakes are set by which misreading is natural. Here a consumer meeting an
unexplained zero would reasonably treat `rawUsedPercent` as the truer figure and
pace on that — routing away from an account whose credit is about to be spent,
when the credit expires whether or not it is used. **The cautious-looking reading
is the lossy one**, which is exactly the case where leaving the reader to infer
is most expensive.

And for any optional field, **say what absence means**. A consumer will infer
something, and the inference that reads as unremarkable is usually the fail-open
one. `rawUsedPercent` absent means the effective and reported figures agree — not
that relaxation is switched off.

That is a warning until someone lists the fields. Enumerate them from the wire
type's `skip_serializing_if` attributes rather than from memory — there are 24 —
and check each one against the contract. Doing that found two whose absence had
never been defined: a window slot (the three can have holes, since each is filled
from its own optional upstream field) and `savedResets` (absent covers the credit
inventory lookup having failed, not zero credits held).

**A keyword sweep over prose fails differently from one over code.** A code sweep
fails loose when the pattern is too broad; a documentation sweep fails loose
*because the document is about the thing being searched for*, so the vocabulary
saturates and every mention matches by proximity. Searching this contract for
absence-language scored every field as documented, including a field name that
does not exist — the words appear every 213 characters. Tightening to
same-sentence matches then reported two fields as gaps whose absence is spelled
out in the neighbouring sentence.

Neither setting is the answer. The sweep produces **candidates**; reading
produces verdicts. What made the exercise work was the enumeration forcing a look
at every field once, rather than at whichever field prompted the question.

### A contract gap that looks theoretical here is often already live downstream

The slot-holes finding above was written up as a shape a consumer *could* get
wrong. Within hours a consumer reported the same defect one level shallower,
already shipped: a status bar reading `usage.primary` alone had been showing 25%
for an account whose binding constraint was a weekly window at 36%.

That is the argument against deferring a documentation gap because nothing has
gone wrong. **Nothing going wrong is what a producer observes either way.** A
consumer misreading the wire produces no error here, no failing test, and no
support request — the module is serving correct data and the misreading happens
in someone else's process. The absence of a symptom is evidence about the
producer's visibility, not about the consumer's behaviour.

The same asymmetry explains why these gaps are found by consumers rather than
found and then reported: a producer's sense of which readings are plausible is
built from the consumers they can imagine, and every consumer they can imagine is
one they have already imagined. There is no self-check for this. What closes it
is someone standing where the author is not — which is worth stating plainly
rather than filing the class as solved.

### A dense fixture cannot test tolerance of sparseness

Every fixture here was built from a fully-populated response — all three window
slots present, every optional field set — because that is what a healthy account
looks like and it is what anyone writes by default. A fixture like that passes
identically whether the code tolerates a missing slot or stops at the first one,
so it cannot distinguish the two.

The cost is visible only across consumers. The same gap appeared three times in
one day from three positions: one decoder read `usage.primary` alone and shipped
a status bar showing 25% for an account whose binding constraint was 36%; another
happened to be correct because its loop was written as a filter rather than a
search, with nothing in its code recording that holes were possible; and this
repository had documented the shape without an instance. **None of the three
would have found it alone** — one had the shape without a case, one had the case
without the shape, and one had code that was right for a reason nobody had
written down.

So where a structure has optional members, one fixture must **omit** some. The
test to apply is the usual one: replace the tolerant construct with the
intolerant version and confirm the sparse fixture reddens and the dense ones do
not.

### `--lib --bins` is not the suite

`cargo test --workspace --lib --bins` runs unit tests only. It does not run the
integration targets under `tests/`, which is where every test that spawns a
daemon, drives the wire, or exercises a stub lives — so the fast local gate is
blind to exactly the tests that check how this module talks to anything else.

That gap ran for four hours once: a constant restated in a `tests/` file drifted
from the one the module dialled, CI failed on every push, and the local gate
reported hundreds of passing tests throughout. **A high pass count from a
restricted target set reads like coverage**, and the number climbs while the
blind spot stays exactly as wide.

So `--lib --bins` is a fast inner-loop check, not evidence a change is sound. The
suite that answers that is `--all-targets`, and CI runs it. Reporting the
restricted count as if it were the full one is the part to avoid: state which
targets ran, or run all of them.

### An example's assertions do not run unless something runs the example

`cargo test --workspace --all-targets` **compiles** examples and never executes
them. An assertion inside one is checked by nothing until a step invokes it, so
an example carrying a real invariant is a test that has never been run.

That matters when a consumer pins against the example's output.
`completeness-envelopes` asserts that the withheld-account and genuine-removal
envelopes differ only in `completeProviders` — which is what lets a consumer
prove its reconciliation reads the claim rather than something incidental.
Unexecuted here, a break in that property leaves this repository green and
surfaces in the consumer's pipeline, where the cause is a producer commit nobody
there can see. **The failure lands where it cannot be diagnosed.**

So any example whose assertions are load-bearing needs its own CI step, and the
step needs proving: run the example under a mutation and confirm a non-zero exit.
The first two attempts here exited 0 without reaching the property — one perturbed
a withheld slot that is never emitted, the other landed before both emits and so
perturbed them equally. A green mutation run is more often a missed mutation than
a weak guard.

### Verify a change applied before reading the result it produced

When the thing you changed lives **outside the file under test** — a
`[patch.crates-io]` stanza, a lockfile, a mutation applied by a script, a
heredoc — confirm it took effect before believing the run. The result cannot
distinguish *applied and passed* from *never applied*, and the second is
indistinguishable from success in every output either produces.

A worked instance: patching this crate's dependency at a merged-but-unpublished
version and running the suite gave a full green pass. The patch had **not**
applied — cargo warns `patch ... was not used in the crate graph` and the lockfile
still named the registry version, because a patch does not take effect until the
lock is updated. The green run had tested the old dependency. The warning sat
above the test output; `cargo metadata` settled it in one line.

Four instances of this shape appeared in a single day, and none was visible in
the result: two mutations that missed their target, a shell-escaping failure that
left a file untouched, and this. **A change that silently fails to apply produces
exactly the output of a change that applied and worked.** So check the mechanism,
not the outcome — which is the same discipline as the mutation-proof rule above,
pointed at the setup instead of the assertion.

One remedy is weaker than it looks. Applying a mutation from a script and
asserting *inside that script* that the anchor was found proves the anchor
matched — not that the edit produced what was intended. Read the changed artefact
itself: `git diff` the mutated file, or query the resolved state
(`cargo metadata`) rather than the manifest you edited. **The check must consult
something the failing step does not produce**, or it inherits that step's blind
spot.

The available tell, before any of that, is that the run **felt too smooth** — a
suite passing against a dependency that was supposed to have changed, a perfect
score from a freshly written detector. The anomaly is in the ease, not in the
output, and it is worth acting on precisely when the result agrees with what you
expected.

### A timed-out send with an unknown outcome is cheap to make safe

When a call to another process times out with no terminal response, the request
may have executed and only the reply was lost. Both recoveries are wrong by
default: retrying can duplicate an effect, and not retrying can drop a message
the recipient never received.

For anything without an idempotency key, **say in the retry that it may be a
duplicate**. One sentence — "if this arrives twice, my first send timed out with
an unknown outcome" — turns a confusing second copy into a labelled one, and the
recipient can discard it without having to work out whether the two differ. It
costs nothing when the first send was genuinely lost.

And verify state before retrying rather than after: the timeout says the outcome
is unknown, not that it failed, so the first question is what actually landed.

### A health probe reports on the path it exercises, and nothing else

During the outage above, the credential vault's own health read `ok` while it was
fenced out of its own store by a competing writer. Not a broken probe — a true
answer to a narrower question than anyone was asking, because the probe performed
a read and reads were unfenced while writes were not.

The same shape appeared on this side: every caller of that vault reported healthy
throughout, because a health check that does not exercise a credential path
cannot distinguish *working* from *not asked yet*. This module's own
`health.check` is honest about its scope for exactly this reason — it reports
refresher liveness and slot buckets, not whether any particular upstream would
answer.

So when a subsystem reports healthy during an incident, ask **which operation the
probe performs** before treating it as evidence. A probe that exercises one path
is silent about every other, and its silence looks identical to a clean bill.

The same question applies to this repository's own checkers, and the answer is
uncomfortable. During that outage the module's health status, the conservation
identity, the deployed-sanity checker, the `buildCommit` verification and the
error-class distribution all passed — while seven credentialed accounts silently
stopped being served. Every one of them measures **internal agreement**, and
**a set that shrinks stays consistent**: the vanished providers moved from one
bucket to another and the buckets still summed to the total.

That is not an argument against those checks; the conservation identity caught a
real bucket defect when it was written. It is an argument for knowing what each
one is blind to. **A checker built to catch contradictions is silent about a
silent subtraction** — so if the question is "did something stop working", none
of them answers it, and their passing is not evidence either way.

### A narrowing step with no record of what it dropped produces a list that looks complete

A sweep that finds the full population, followed by a second pass that narrows it,
yields a list indistinguishable from a complete one — unless the narrowing records
what it removed. The evidence was correct and the summary was wrong, and the
summary is what gets acted on.

This reached here as an outage. A rename of the credential vault's daemon module
id was staged across its callers; the sweep that found them included this module,
a second pass over a subset of repositories was used to write the caller list,
and every caller the narrow pass had not covered fell out silently. Five were
told to stage and hold. This one was told nothing, and every vault-served
provider lost its credential when the rename landed.

Same shape as an absence sweep reporting a count with no denominator: the output
carries no trace of its own scope, so nothing about it invites the question.
**Print what the narrowing removed, or carry the wide result forward.**

### An explanation that fits one member may be its special case wearing the shape of the class

When one member of a set behaves differently and a mechanism explains it, the
mechanism is established for that member only. Letting it stand as the rule is
the cheap step, and it survives because it is *true* — just not general.

A worked case: during a credential-store outage, one provider vanished from the
response entirely while others merely degraded. The explanation was real — that
provider replaces its local credential lane with stored handles, so an
unreachable store leaves it zero handles and it emits nothing. Reading the other
five vault-aware providers showed **none of them share it**; each keeps an
implicit local handle and extends it. The variation was in the providers, not in
the outage, and the difference in symptom had been observed by two people without
either connecting it.

The check is one step and it is the step that gets skipped: **enumerate the other
members before letting the explanation stand as the rule.** A mechanism that
explains the case in front of you feels finished, which is precisely when it is
least examined.

### A review that answered about the wrong target is unaddressed, not wrong

An automated review can examine something other than what was asked — a whole
file rather than a diff, a neighbouring function, an older revision. Its findings
are then neither right nor wrong about the intended target; they are **about
something else**, and both obvious responses lose information.

Discarding it because it disagrees with the change throws away a finding that may
be correct where it landed. Accepting it as being about the change rewrites
something that was fine. This happened here: a review of an uncommitted section
returned specific, well-reasoned criticism of a passage forty lines away, which
had been wrong for weeks. The critique was inapplicable to the change and
accurate about its actual subject, so the finding was applied where it belonged.

So before judging whether a review is right, establish **what it examined**. Same
discipline as reading which test a mutation reddened rather than noting that
something went red.

### A check that returns the same answer for every member of a set is answering a question about itself

A uniform result across a population that cannot plausibly be uniform is an
instrument reading, not a measurement. Two instances in one day: a
documentation detector that scored every field as documented, including a field
name that does not exist; and a case-mismatched grep that reported zero for every
provider in a rendering, including the ones plainly present.

The useful part is that **uniformity is the tell, and a partial result would have
been worse.** One lucky match in either case yields a plausible partial answer
that gets reported and believed. A broken instrument does not usually produce an
obviously wrong answer — it produces a suspiciously tidy one.

### Merging a doc fix does not deliver it

Documentation on a published type reaches a reader through a **release**, not
through a merge. Doc comments merged to a shared crate's default branch are seen
by path-dependants immediately and by everyone else — registry consumers, docs.rs,
anyone hovering the type in an editor — only when a new version is published.

This was measured after exactly that merge: the published version carried sixteen
optional fields with no doc comment while the branch carried none, at the **same
version number**, differing by 83 lines. One consumer of four had the docs, and
this module — which authored them and pins the crate from the registry — did not.

So a doc change to a shared crate is not finished at merge. Either publish a
patch release or record that the fix is pending one, because "merged" and
"delivered" differ by exactly the audience the change was written for. And a
published version that differs in content from the branch claiming the same
number is its own hazard: it teaches readers to distrust the version rather than
the content.

### A comparison of two identical inputs says nothing about the transform

The completeness harness emits a pair of envelopes whose `result` arrays are
byte-identical, so that a consumer proving retain-versus-delete is isolating
`completeProviders` and nothing else.

It is tempting to extend that downstream: *decode both through your types and
assert the decoded arrays are still equal, to prove the decode loses nothing.*
**That check cannot fail.** The two inputs are already identical, so any
deterministic function of them produces identical outputs — including one that
drops every field. The assertion is satisfied by the inputs rather than by the
property it appears to test.

The general form: **a differential test over two equal inputs tests the equality
of the inputs, not the behaviour of what is applied to them.** To show a decode
preserves what matters, compare the decoded value against the *raw* payload field
by field, and prove that check can fail by dropping a field and watching it
redden. Keep that separate from the pair discrimination, or one assertion's
vacuity hides inside the other's success.

### Record the consequence, not just the fact

"`rawUsedPercent` is emitted only where the two figures diverge" is a fact, and a
reader agrees with it. "So absence means they agree, and rendering a placeholder
would be wrong on every unrelaxed window" is a consequence, and a reader
remembers it while editing.

The difference decides whether a correct behaviour survives a refactor. Code that
is correct for an unstated reason is indistinguishable from code that is correct
by accident, and the next author cannot tell which they are holding — so a
simplification that looks like an improvement removes the property silently.

A fact invites agreement; a consequence invites care. When documenting a field or
a guard, write the sentence that names what breaks.

## An asymmetric guard states its reason where the next caller looks

Some guards are deliberately applied on one path and skipped on another. The
empty-response rule is one: `send` and `send_full` refuse an empty 2xx body,
`send_raw` does not, because a caller reading rate-limit headers off a `429`
never looks at the body and would break if an empty one were refused.

An omission like that reads as an oversight unless it says otherwise, and the
cost of a reader believing it was one runs both ways. Applying a guard a path
deliberately skips can reintroduce exactly the behaviour that path exists to
avoid; skipping one a path needs reintroduces the defect it was written for —
here, an empty 2xx becoming a decode failure, which is non-transient, so a
flapping endpoint reads as dead. **The careful reader, doing what the code
appears to ask, is the one who breaks it.**

So the reason belongs at a definition rather than at the call sites, which are
read by people who already know. And it belongs at **the definition the next
caller opens first**: this rule's reasoning sat on `HttpResponse::body_for_parsing`,
which a reader reaches only if they already suspect the rule exists, while
`send_raw` — the door a new provider actually opens — said nothing at all.

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

The precondition is **two independent producers of the same data**, each doing
its own work, so a guard genuinely has to be written twice. One producer feeding
two renderers does not qualify: the value is computed once and then serialised or
formatted, so there is only ever one place for the guard to live and the
duplication this depends on is absent by construction. Applied to a render-fork
the check returns a null that tests nothing — the same defect as a control scoped
more narrowly than the query it guards, where the check ran, produced a value,
and answered a different question. Stated because a null from the wrong shape
invites two wrong conclusions: that the code is clean, or that the rule is
unfounded.

**Externally confirmed, which every rule here should be and most are not.** Until
2026-08-12 every instance of this was in this repository, which makes it an
observation about one codebase rather than a rule.

**And running it elsewhere is only half the confirmation.** A peer ran a rule of
theirs against this module's health surface on 2026-08-26 and got a NULL — the
surface passes. The null was a FACT and not a finding. The finding appeared when
they disagreed with what I had recorded the null to mean: I wrote that the rule
was "publish the population", and they corrected it to "partition by member
state", because a denominator beside an aggregate satisfies the first and is not
what saves this surface.

So the rule wants a third clause: THE OTHER PARTY MUST BE ABLE TO DISAGREE WITH
THE RESULT, which needs enough context to hold an opinion about it rather than
just enough to execute the check. A reader who can only run the check returns a
value; a reader who can contest the reading returns a finding.

**And a completion condition, which is the fourth clause and the cheapest.** A
rule is not finished being written until it has been RUN AGAINST THE REPOSITORY
THAT PRODUCED IT, with whatever that run returned recorded beside it — including a
null, because "no instance found" and "not looked" are the distinction this whole
document keeps insisting on, applied to its own contents.

In the same act as writing it, NOT as a follow-up, and the reason is mechanical
rather than motivational: AT THE MOMENT OF ARTICULATION THE PREDICATE IS AT ITS
SHARPEST, and every later encounter is with a remembered gist. A follow-up is
scheduled against the summary and runs something vaguer. Two rules in this
document found an instance in the repository that produced them when applied one
step after being written — the account-level bucketing case, and a peer's
ownership case found while they were still explaining the rule to me. Neither was
looked for.

**Stop adding clauses before the rule becomes a document nobody runs.** Each
addition above was a real improvement and the sum can still be worse than any
intermediate version — the same shape as everything else here, an artifact
reporting success about a subject that is no longer the one that matters, which is
whether anyone EXECUTES it.

**The measure is execution cost, not clause count.** A four-clause rule that runs
as one shell line is affordable; a two-clause rule needing judgement on both is
already too long. This one stops at four because three are mechanically checkable
and the fourth is a caveat about reading a result, which is the cheapest kind of
clause to carry.

**The reduction that survives, checked against every finding in that exchange:**
it is the DISAGREEMENT that produces the finding, not the second reading. Each one
came from one side asserting the other was wrong about something specific and
being right about half of it — three hypotheses about a frozen row, all false, the
falsification locating the real mechanism; a rejected suspend-bug explanation; two
corrected rule statements. None came from either side reading more carefully.

The cost of that, stated because it is uncomfortable: a second reading by a
CO-OPERATIVE reader is worth much less than it feels like FOR CONTRADICTIONS, and
most review is co-operative reading. A reviewer with no position of their own can
only find what the author already almost saw.

**That qualifier is load-bearing and the sentence was wrong without it.** It holds
for DEFECTS — a claim that contradicts the code or another claim — because
catching one requires holding a rival account. It does NOT hold for OMISSIONS: a
reader who does not know what you meant is excellent at noticing something is
UNSTATED, because their confusion is itself the signal and no position is
required. The two arrive differently — contradictions need a reader holding the
whole document, gaps need a reader trying to BUILD from it.

The evidence here is one-sided in exactly that pattern. The consumer defects were
found by readers holding a position: one reading `usage.primary` as the most
constrained window, one comparing two hosts' arrays as one series. The omissions
were all found by readers with no position at all, simply trying to consume — the
partial-account-set gap, refused-versus-empty rendering, and three issue filers
finding unstated cases by building against the contract and hitting a case it did
not cover. Run against a repository
written by someone else, it found a live divergence on the first surface it was
pointed at: two independent spend-report producers, one querying SQLite directly
and one reading an in-memory projection, sharing five guards — invalid window,
live-fact filtering, non-null timestamps, half-open window, subject/project/scope
filters — and diverging on the sixth. The projection scopes corrections to the
facts that survived filtering; the direct path applies no subject, project or
scope predicate to corrections at all. Unfiltered requests agree, so the
divergence appears only on a filtered one.

Two details from the confirmation are worth more than the hit itself.

The direct producer turned out to be production-dead, reached only from tests,
so the divergence is a WRONG ORACLE rather than a wrong report — which is the
worse outcome: it will either mask a real defect in the production path or
manufacture a phantom one, on exactly the queries an operator filters. A rule
that finds divergence between two producers should ask which one is on the wire
before scoring the severity, and the answer can invert it.

And the predicted untested twin was untested for two independent reasons, not
one. No test in that file inserts a correction at all, and no test passes a
non-null filter — every request built there sets subject, scope and project to
`None`. Two guards, two blind spots, intersecting exactly where the divergence
lives. The rule pointed at the right cell of a table whose rows and columns were
both empty, which is the case where reading either producer alone reveals
nothing.

**Which producer holds the gap is predictable, and the predictor is checkable
from outside.** The original form of this said the tested branch is the SIMPLER
one, because it is easier to test. That is a judgement about code, and it
requires reading both branches and forming an opinion about which is simpler.

The sharper predictor, from three cases across two repositories: **the gap is on
the producer FURTHEST FROM THE EXERCISED PATH.** A SQL report producer no
production caller reaches, an emitter nothing has ever sent, and — in this
module — the labeled emission branch, which serves 2 entries where the unlabeled
branch serves 35 on this host. In every case the guard that exists is the one on
the producer something actually runs.

That is better than "simpler" in two ways. It requires no judgement: rank the
producers by whether a production caller or a live receiver exercises them, which
is a search rather than an opinion. And it connects to the receiver-acceptance
criterion above — a shape no receiver has accepted is a shape whose correctness
is an opinion, and a producer nothing exercises is where its guards go missing.
The two rules are the same observation from different ends.

The uncomfortable corollary for this module: the labeled branch is the rare one
here, and it is also the one carrying the account identity — the most expensive
claim in the module to get wrong. Rarity in production and consequence on the
wire are not correlated, and where they are inverted the natural amount of
testing is backwards.

**A competing predictor was proposed and falsified, which is why this one is
stated rather than the obvious alternative.** After the first two cases the
divergences sat in read paths, so read-versus-write looked like the
discriminator — equally checkable from outside, and a tidier story. The third
case is a write pair that diverges: one producer guards against a negative
generation, its twin takes three signed coordinates and checks none. So across
three pairs the split is one read, one write, and one genuine null where both
producers are live.

Distance-from-the-exercised-path predicted all three; read-versus-write would
have been wrong once. Recorded because a rule that has beaten a plausible rival
on a case that separated them is worth more than one that merely fits its own
examples, and the losing candidate is the thing a future reader is most likely
to propose again.

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

### The asymmetry has a second, worse form

Both cases above were *one* producer of a claim, split into two emission
branches. There is a harder shape: two producers that are not peers at all,
where one **acts** on the answer and the other only **reports** it.

Run outside this codebase, the rule found exactly that: a value computed once by
the component that takes an action on it, and again by an inline scan that fills
a diagnostic field on an error. The acting producer scoped its scan correctly and
held five tests including one for that scoping. The reporting producer omitted
the scope and carried a single assertion, for the trivial case where the answer
is empty.

The prediction holds and sharpens: the simpler path is not merely easier to test,
it is the one whose output **nobody acts on**, so nothing fails when it is wrong.
A diagnostic that names a recovery time the caller cannot actually have is a
lying instrument, and it lies most convincingly precisely because the acting path
beside it is correct and well tested.

So when diffing twins, note which one has **consequences**. A guard missing from
the consequential path is a defect that surfaces; a guard missing from the
reporting path produces a plausible number that is never contradicted.

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

## A stored deadline is not a working deadline

A retry ladder has two properties and they are tested very differently. That the
durations are right is a question about a pure function, and it is the one that
gets asserted, because it is easy: call `backoff(class, n)` and compare. That the
scheduler HONOURS them is a question about behaviour, and it is the one that
matters, because a deadline nothing consults is decoration.

The values passing tells you nothing about the second. Every duration in this
crate's ladder is asserted, and making the scheduler ignore `next_due_at`
entirely — treating every slot as due on every tick — reddened exactly one test,
named for enumeration errors and handle admission. Defended by accident, by a
test about something else. A refresher hammering every provider on every tick
would have shipped with the whole backoff suite green.

**The proof is a negative and a positive, and neither alone is worth anything.**
Fetch once to arm the ladder, tick several more times and assert the call count
did NOT move — that is what shows the deadline suppresses work. Then advance
past it and assert the count DID move, because a suppression test alone is
satisfied by a scheduler that fetches nothing at all.

That second half is not hypothetical caution. Mutating the scheduler to fetch
nothing did not redden a test here: it HUNG, and the suite reported a timeout
rather than a failure. A permanently idle refresher is caught by the clock
rather than by an assertion, which is the weakest form of catching something —
so the positive half has to be proven by a mutation that suppresses only the
retry, not by one that suppresses everything.

**An assertion that nothing extra happened never shows anything would happen
again**, and that generalises past retry ladders. The refresher's idle sleep is
capped so that discovery keeps running: handles are enumerated at the top of a
tick, so the longest the loop may sleep is the longest a newly added credential
stays invisible. Replacing that cap with its opposite — sleeping the full
backoff instead of the bound — reddened nothing, and a user adding an account
while their providers were failing would have waited a quarter of an hour.

The liveness check could not catch it either, for a reason worth generalising:
`STALL_HORIZON` is DERIVED from `MAX_TICK_SLEEP`, so raising the sleep raises
what counts as stalled, and the heartbeat keeps reporting healthy because the
refresher is doing exactly what its own constants now permit. **A guard computed
from the thing it guards cannot fail while the two move together** — which is
why a bound like this needs a test naming the behaviour it protects, not merely
a documented relationship between the constants.

## Proven not-reached is worse than never-proven-reached

A rule nobody has measured MIGHT be running. A rule measured at zero has
definitively never run — and the only way to reach the worse state is to
measure, which is why the comfortable reading survives so long.

Both states look identical from outside: the checker reports `findings: none`
either way. The difference is entirely in whether a denominator exists, so the
work is not finding a defect but converting an unknown into a known bad.

The instance here: six spend-pool rules, one of which needs a pool to state BOTH
a remaining balance and a total. The only provider publishing pools states a
balance and no total, so that rule — the one catching a balance stated wrongly
against its own cap — has never evaluated on live data in its entire life. The
sweep said `findings: none` throughout, honestly.

**Count the population each RULE examined, not the population the container
walked.** A conditional guard inside a loop creates a narrower population than
the loop's, and the loop's count silently vouches for it. `pools checked: 2` was
an honest denominator for the two rules with no precondition and an overstatement
for the other four.

Where this stops is worth stating, because it is a real limit rather than a gap:
requiring a count per conditional rule cannot be ENFORCED without the checker
knowing which of its own branches are preconditions — and whatever decided that
would be a rule with no denominator, needing a count of the branches it examined.
So the requirement lives as a note at the tally each rule increments, beside the
live figures, because a zero sitting where the next author reads argues better
than a rule telling them to count.

## Drawing a seam and testing behind it are two different jobs

A decision fused to the I/O it depends on cannot be driven in a test, and the
usual repair is to take the I/O as an argument: `refresh_snapshot_if_stale`
receives its acquirer, `locate_under` receives a base directory. That separates
"when should we do this" from "how do we do it", and only the first is a
decision.

**The seam is not the test, and this repo has an example of each half arriving
without the other.** `locate_under` was extracted precisely so a test could
supply a directory rather than alter a real Chrome installation, with a comment
saying so — and the decision behind it, which cookie database to pick when a user
has several profiles, was untested. Switching "newest wins" to "oldest wins"
reddened nothing. Someone did the harder part, wrote down why, and then covered
only the branch they had in mind at the time.

So when a seam already exists, do not read it as coverage. It is an invitation
that may never have been accepted, and the reason it looks like coverage is that
the effort visible in the diff went into making the test possible.

A fixture note that generalises past this case: the test writes two cookie
databases and asserts the newest is chosen, which only means something if the
two mtimes actually differ. Files written back to back can land in the same
filesystem timestamp tick, and a fixture whose two values are EQUAL passes under
either rule. It therefore separates them deliberately and asserts the separation
before asserting the outcome — better to fail on the setup than to pass on a
coincidence.

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

**And provenance is not correctness.** A capture proves what the wire said, not
that what it said was intended — which is the opposite of how a captured fixture
is usually argued for. The two are indistinguishable from inside the fixture: a
value that is wrong and a value that is right look identical once they are string
literals, and the capture's authority attaches to both.

This is not theoretical here. A test built from a real captured payload asserted
`"2026-07-14T13:00:01+00:00"` for a credit expiry, and that assertion was pinning
a **defect** — the formatter dropped the fractional part because the instant
happened to land on a whole second, so one instant had several spellings on the
wire. The test was green, the value was genuine, and its realness was the reason
nobody questioned it. Fixing the producer reddened it.

The distinction to hold: a capture is authoritative about the SHAPE the upstream
sends and about what THIS module emitted at that moment. It is not authoritative
about whether emitting that was right. When a captured expectation blocks a fix,
the question is which side the defect is on — and "it came from real data" argues
for neither answer.

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

### A rule that finds nothing on first use is unexposed, not wrong

Every rule above was written after a defect it would have caught. Applied
somewhere new it will often find nothing, and that result is indistinguishable
from the rule being unfounded — so the natural response is to stop applying it,
which guarantees it never reaches the case it was written for. No bad judgement
is needed anywhere in that chain.

A null is only evidence if the code had the shape the rule needs. The
twin-branch rule requires two independent producers of the same data; applied to
one producer with two renderers it returns a clean null having tested nothing.
**Report what the null covered, not just that it was clean** — an unqualified
null is evidence for whatever the reader already believed.

Rules also arrive blunt and are sharpened by their own easy cases. The
asymmetric-guard rule was first written as "state the reason at the definition,
not at the call sites", which caught a crude instance; catching it is what forced
the sharper form — *the definition the next caller opens first* — and only that
form could catch the near-miss where the reason sat on a real definition nobody
opens cold. **The trivial first catch was not a lesser version of the good one;
it was its precondition.**

## A note explaining why a check cannot fire is a defect report

When a check cannot detect the thing it was written for, the honest instinct is
to write that down beside it. That instinct is right about the fact and wrong
about what to do next: the note documents a gap where a working check should
be, and it reads as diligence, so nobody returns to it. The interval between
writing the note and the incident is time the defect was known and shipped
anyway.

Concretely here. `wire_sanity` compares `usedCount` against `totalCount`, and a
comment beside it observed that where a provider *derives* the count from the
percentage and the cap, the comparison returns the reported percentage by
construction and cannot fire. True, carefully reasoned, and the end of the
thought. What it should have prompted is the question of which OTHER property
of a derived count is checkable — and one was, four lines long: a count is a
quantity of things, so a fractional value proves it was computed rather than
measured. That rule would have caught a live fractional count before it reached
a consumer, where instead it cost that consumer every provider's capacity on the
response until it was fixed.

**A harder class sits next to this one, and this rule does not catch it.** The
peer case that arrived with this exchange looked at first like the same shape
one level worse: a health check consulting only one freshness source, so a
configured axis with no successful poll was not represented at all and its
absence read as healthy. Four days dark, surfaced by someone inspecting a table
by hand after an unrelated deploy.

But that failure had NO NOTE. Nobody had reasoned about the gap and stopped —
the state simply was not in the enumeration, and nothing in the file drew
attention to its absence. A sweep for cannot-fire annotations would never have
found it, on any day, however carefully run. Filing it as an instance of the
rule above would credit that rule with a catch it cannot make.

So the two are different problems. This section's rule finds the gap someone
DOCUMENTED. The harder one is that an instrument's coverage of the state space
is not visible from the instrument's own source: absence of a state from the
enumeration reads as absence of a problem, and there is no text to grep for.

The mechanical form that does catch it is a **conservation identity** over the
partition — assert that the buckets SUM to the population, rather than that each
bucket looks right. An unrepresented state then cannot hide, because the sum
stops balancing, and no one has to have thought of the missing state in advance.
That is not theory here: `fresh + stale + pending + len(degraded) +
len(unconfigured) + len(withoutHandles) == providersTotal` is published for
exactly this reason, and it earned itself. Providers awaiting a first fetch
answered none of the bucket arms and landed nowhere while still counting in the
total; the identity broke, which is how the `pending` bucket came to exist. No
inspection of the health code would have raised the question, because every
bucket in it was individually correct.

The precondition is that the population be independently known. `providersTotal`
comes from the registry rather than from summing the buckets, which is the whole
reason the identity can fail — a total derived from its own parts always
balances and proves nothing.

**And an identity cannot see a state that reports in the WRONG place.** It
proves nothing reports nowhere; a state landing in a bucket it does not belong
in keeps the sum whole. The shape that produces this is bucketing on BOOLEANS
derived from a state rather than matching on the state itself: the health
buckets here ask `has_fresh`, `has_stale`, `all_degraded`, so a new `SlotStatus`
variant fails every test, falls into the last arm, and is counted as pending
while the identity stays balanced.

The companion instrument is an **exhaustiveness fence** — an `every()`
enumerator plus a match in a test, so a new variant fails to compile and its
bucket is named rather than inherited. Where the state is a `&str` the compiler
cannot help directly, and the fence goes at the ONE PRODUCER instead: walk every
variant of the enum that generates the string, and assert each generated value
appears in a hand-maintained judged list. That is what makes the `_ => false`
arms in `class_is_expected_absence` safe — not the wildcard, but that no
unjudged class can reach it.

That reasoning holds only while the string has a producer you own. A state read
from a database or another process is bounded by nothing your tests can reach: a
migration or a manual write can produce a value no code path ever emitted. There
the only available instrument is a validating boundary that REFUSES an
unrecognised value, and it must be structural rather than positional — a guard
that works because validation happens to run first stops working when someone
adds a second entry point.

**A dropped value is only detectable against an independently-known population.**
The third disposition for a wildcard — after fencing at the producer and failing
closed on foreign input — is dropping the value, which is right for parsing and
mapping where the alternative is refusing a payload over one unreadable member.
Its risk is different from mis-bucketing: the value is simply absent from the
output, and absence reads as nothing-to-report.

That is not fixed at the match site, and auditing wildcards forever is the wrong
response. `vault_handles` returns `None` for a credential family that maps to no
provider and drops it with a stderr warning; what makes that safe is elsewhere.
`vault-lanes` enumerates configured handles from the handle FILE — a population
known independently of what the mapper produced — collects every handle that
reached no provider, and exits non-zero. A warning printed beside `findings:
none` and an exit code of 0 is the shape that checker exists to refuse: the fact
was on screen and the summary said everything was fine.

So the test for a dropping wildcard is not what the arm returns. It is whether
anything downstream knows how many values SHOULD have arrived. Where the output
is built by iterating whatever a query returned, no such number exists and the
omission is undetectable however careful the arm is.

**Check where the fence actually bites, not that one exists.** Adding a variant
to `SlotStatus` already broke this build before the fence was added — at
`service_rank`, which decides which of two duplicate slots wins and asks nothing
about health. "The compiler catches it" was true and irrelevant. The general
form is worth more than the instance: a check that fires is evidence only about
the property it fires ABOUT, and accepting a real check as evidence for a claim
it was never making is harder to notice than having no check at all, because
something genuinely does go red.

So: **an unfalsifiable check is a reason to find a check that bites, not a
limitation to document.** Where you cannot verify the property you wanted,
verify a different one that the same defect would violate. And treat the note
itself as the alert — a comment saying a check cannot fire is a defect report
that has been filed and never triaged.

The corollary about who found what: neither of these was caught by a control.
One was found by chance, the other bounded by a peer's strictness at their own
boundary. The only mechanism that functioned as designed was a rule written to
refuse a shape, which worked on a day when nobody was watching — and that is the
whole property distinguishing a control from an anecdote. Every note either side
wrote, and every blind spot either side documented, failed on days when we were
paying attention. That asymmetry is the argument for rules over vigilance.

## The claims that rot are the ones no test could have failed

A day spent auditing this module and a peer's alongside it produced nine
defects, and the pattern in them is narrow enough to act on. None were found by
looking where either of us suspected. Every one was in reviewed, believed code,
and the check that found it was going and verifying something expected to be
fine.

But "check what you are confident about" is too broad to be a practice, and the
sharper version comes from what the nine had in common. Four were claims about
BEHAVIOUR and those were all well covered — the strongest claim here, that the
refresher cannot lose a healthy window to a transient failure, turns out to be
defended by six tests approaching it from different directions. Confidence and
coverage are not reliably inverse.

The ones that had rotted were claims about SCOPE:

- what a check covers (`vault-lanes` proves the deployed binary, not the working
  tree — and reported `findings: none` on a renamed wire field),
- what a note implies (a comment explaining why a rule cannot fire, standing in
  for the rule that would),
- what a measurement showed (a correct hash comparison summarised an hour later
  as its own opposite),
- which direction a shape belongs to (a peer's design document quoting the
  acknowledgment struct where the request struct belongs).

Behaviour gets tests because that is what tests are for. **A scope claim lives in
prose, and prose has no runner** — nothing goes red when it drifts, and it is
usually written by the person best placed to be wrong about it, at the moment
they are most confident.

So the practice: pick the strongest claim you would defend THAT NO TEST COULD
HAVE FAILED, and check that one. Every item above is a sentence rather than a
code path.

Two corollaries, both paid for:

**A verified fact does not stay verified by being repeated.** "Deployed at X and
serving" was checked once and restated in eight messages over several hours as
present tense. It held — but repetition is what made it feel checked, which is
the same freshness error this module warns consumers about on the wire.

**Report the nulls.** A practice that only ever produces hits stops being
believed, and a rule that has never returned clean has been exercised rather
than tested.

## Which surface a credential lives on is a per-provider fact

Before writing a fetcher, establish WHICH surface answers for usage, because the
obvious one is often wrong and the wrong one fails in a way that reads as the
user's fault.

Three shapes seen here, in rising cost order:

**The API key does not reach the billing endpoint.** `ollama-cloud`'s key
verifies access and returns no quota; `qwen-cloud`'s console billing answers a
browser session and not the API credential. The tell is a structurally valid
credential producing a refusal, and the trap is that the refusal reads as
"sign in again" when signing in cannot fix it. PROBE THE BILLING ENDPOINT WITH
THE API CREDENTIAL FIRST — one call, before any adapter exists, settles it.

**The credential lives somewhere other than where its name suggests.**
`synthetic`'s key sits in the shared harness auth store rather than an
environment variable. Cheap to get wrong, cheap to fix, and the symptom is a
provider reporting an absent credential on a host where the user believes they
configured one.

**The credential is rotating and reading it SPENDS it.** This is the expensive
one and it is not recoverable. Anthropic's and OpenAI's OAuth refresh returns a
REPLACEMENT refresh token and invalidates the old one, so a second reader
breaks the user's sign-in to the tool that owns the credential — collecting a
usage figure at the cost of the thing being measured. Google's does not rotate,
which is why the antigravity plugin lane is safe and is NOT a pattern to copy.
Establish which kind a provider's refresh is before adding any file-backed lane;
the failure is silent on our side and expensive on theirs.

The cookie cohort's mechanics are already shared — `browser_cookies` resolves a
jar in one call, one snapshot per refresh tick across the whole cohort, with a
single `SOURCE_LABEL`. What stays per-provider is the session-cookie predicate
and the response decoder, and both are genuinely per-provider: some upstreams
name a session cookie and some accept any jar for the domain, which is exactly
the distinction `cookieLoginsStale` depends on.

## Registering a provider has a step that fails the build

Adding an adapter means: the module under `crates/quota-core/src/`, the
`UsageProvider` impl, the registration in `Registry::with_defaults` — and then
the provider count stated in `docs/provider-matrix.md`, which a unit test
compares against the registry.

That last one needs no instructions and has none here on purpose. The test's
failure message names both numbers and says to update the sentence, delivered to
the person who needs it at the moment it is actionable — and a procedure written
in advance would be a second copy of a rule that already enforces itself, free to
drift from the check. What prose is for is the part a failure cannot say: it is
enforced because it had ALREADY drifted, and the wrong number came from counting
`Box::new` occurrences in the registry source, one of which wraps a fetch outcome
and registers nothing. A cheap proxy answering a nearby question, in a sentence
ending "verified".

The canonical slug map (`api_provider_name`) is the deliberate opposite, and it
is the half that genuinely needs writing down. A new provider does NOT have to
appear there, and no test requires it, because many providers have no models.dev
counterpart — a required entry would invite a guessed slug, and consumers join
pricing data on that field, so the enforcement would manufacture exactly the bad
data it appears to prevent. What IS enforced is the other direction: every key in
the map must name a registered provider, so a rename cannot silently strip the
slug from a provider's entries.

**An unenforced rule is the one that needs prose, and an enforced one mostly does
not.** A check announces itself at the moment of refusal and carries its own
remedy. A deliberate non-check announces nothing, ever — it looks identical to an
oversight, and the only thing standing between it and a well-meant "fix" is a
note saying the absence was chosen and why.

**Note where this is written.** `STRUCTURE.md` carries an add-a-provider
procedure and is the obvious place for it — but that file is gitignored AND
regenerated, so an edit there reaches nobody and is overwritten by the next
doc-gen run. A procedure has to live somewhere tracked to be a procedure at all.

## An enumeration with no escape clause is a closed-world claim

A comment listing the cases a value can take is the most authoritative shape a
comment has, and the most expensive one to get wrong. It does not mislead by
being vague — it misleads by being COMPLETE, because a reader who accepts the
list has no reason to look for a case outside it.

The instance: a match arm here carried "the vault holds a record for this handle
in both cases — it was minted deliberately — so neither is an absent
credential." True of both arms it described. False of the case that was missing,
which was a handle naming a credential that had been REMOVED — no record, no
login that fixes it. A reader hitting that bug would have been talked out of the
right answer by a comment that sounded settled.

Two properties made it durable, and both look like good writing:

- It enumerated, so the space appeared closed.
- It gave a RATIONALE ("it was minted deliberately"), so questioning it meant
  defeating an argument rather than noticing an omission.

The repair is one clause: "…unless the record itself is gone." Not a hedge — a
named case, which is the difference between inviting a check and foreclosing it.

**When a comment enumerates cases, the load-bearing sentence is the one naming
what would ADD a case, not the one listing the cases.** A comment that says what
it cannot see keeps a future reader looking; one that says what cannot happen
stops them. In a codebase with other authors and an upstream that changes, a
closed-world claim is a bet you are not positioned to cover.

The generalisation is not "hedge everything", which produces comments that
assert nothing. It is that an enumeration should state its own boundary
condition — what would make it incomplete — so the next reader can check that
condition instead of trusting the list.

## A record and a currency claim look identical and age oppositely

Both are a sentence with a date in it. One says something HAPPENED then — a
probe was run, a credit was consumed, an Oracle pass was folded — and stays true
forever. The other says something IS the case, checked then, and starts rotting
immediately while the thing it describes usually stays correct. That asymmetry
is what makes the second dangerous: nothing goes wrong, so nothing draws
attention to the sentence.

Two of these had rotted here, found by enumerating rather than by noticing:

- `provider-matrix` said the four opaque upstream constants "matched CodexBar
  v0.47.0" while five further releases had shipped. The constants had NOT
  drifted, so no behaviour was wrong — only the line recording when anyone last
  looked, which is the entire value of that line.
- `vault-consumer-design` had a section headed "Live vault state" listing five
  handles as of a date. The file holds nine today, because the credential vault
  mints one whenever an account is added. The document was not wrong when
  written and had a banner saying the source wins on disagreement — but a dated
  INVENTORY is where a reader is most likely to read prose as data, since it
  looks like a fact rather than an argument.

Two habits, both cheap:

**Say which kind you are writing.** A record can name its date and stop. A
currency claim should carry the tag or command that re-derives it, so checking
it is one line rather than an investigation — and should be phrased as a
snapshot, not as the present tense.

**Enumerate them rather than watching for them.** Grep for the verbs
(`verified`, `checked`, `matched`, `confirmed`, `as of`) beside a date or
version. Note the instrument trap: a naive sentence-splitter on `.` cannot
capture a claim ending in a VERSION NUMBER, because the version contains the
delimiter — the first run of this sweep silently could not have found the very
defect it was written from. Split on lines and join each with its successor
instead, since these claims wrap.

## Where a rule is written decides who ever reads it

Two places to record a durable constraint, and they trade the same resource in
opposite directions. A note at the SITE reaches exactly the person already
thinking about that code, costs nothing to everyone else, and is invisible to
anyone who never opens the file. A note in a document read at the start of work
reaches everyone, costs attention on every read, and pays out once.

Neither is better. **The choice follows from what triggers the hazard.** If it
fires when someone edits a known place — adding a provider, extending a tally,
touching a formatter — the site note reaches them at the moment it matters. If it
fires when someone writes something NEW that has no site yet — a first rule of a
kind, a lane for a provider nobody has integrated — only the document can reach
it, because the site does not exist to carry the warning.

Two placement failures in this repo, both of which looked like the obvious
choice at the time:

- The add-a-provider procedure lives in `STRUCTURE.md`, which is gitignored AND
  regenerated. An edit there reaches nobody and is overwritten by the next
  doc-gen run. It is the natural place to look, which is exactly why it is the
  wrong place to write.
- The rule about which upstreams rotate refresh tokens — the one that stops a
  future lane from spending a credential to measure it — sat in a `const` in one
  provider file. Correct, and unreachable by anyone who had not already decided
  to read that provider.

The test for a site note is whether the person who needs it has a reason to be
looking there. If they only find it AFTER making the mistake, it belongs in a
document instead.

**A derived file is a third case, and reachability is not what is wrong with
it.** A note in the wrong file is unreachable; a note in a REGENERATED file is
unreachable and then destroyed, with no event marking either — the next
generation reverts it silently and the author has no way to learn that it
happened. Whether a file is authored or derived is invisible from inside it,
because the text is identical either way, so the question has to be asked before
writing rather than noticed while reading. In this repo `ARCHITECTURE.md` and
`STRUCTURE.md` are generated; the only marker is a comment in `.gitignore`, which
nobody editing them will see.

The general constraint, which is why `.gitignore` is the right home rather than a
compromise: **a property of an artifact that is destroyed by the process
producing that artifact cannot be recorded inside it.** The marker has to live in
the thing that does the destroying, or in something that outlives it. That covers
generated docs, and equally anything a migration rewrites, a formatter
normalises, or a codegen step owns.

Enumerating this repo's untracked paths turns up one more with the same hazard:
`.cortexkit`, `.alfonso` and `.opencode` are machine-local agent state, and a
long-running audit ledger lived under `.cortexkit` once — reachable, never
destroyed, and invisible to every other checkout for its whole life. Both markers
now sit in `.gitignore` beside the entries.

## A bug report that arrives with a mechanism is the dangerous kind

A report with only symptoms gets investigated. A report that also names a cause
gets *implemented*, because the mechanism reads as work already done — and it is
the one part of the report nobody re-derives.

The instance (insula#8). A consumer filed a precise, well-evidenced timeline: a
credential lane stayed `credential_unusable` for an hour after the vault record
behind it was re-sealed, and recovered only on module restart. Attached was a
mechanism — *unusable lanes are excluded from the probe cycle with no clearing
edge* — and a fix: retry with backoff. Every symptom fitted.

The mechanism was wrong. `NON_TRANSIENT_BACKOFF` is a flat 300s with no terminal
state, so the lane was re-probed about twelve times during that hour, and a slot
that fails and then succeeds recovers with no restart. Building the filed fix
would have added a retry to a path that already retries, shipped as a fix, and
reproduced identically on the next re-seal — with the report closed.

**Reproduce the SYMPTOM, never the diagnosis.** Take the timeline as evidence and
the mechanism as one hypothesis among the ones you have not thought of yet. The
tell that you are implementing rather than investigating: you can describe the
patch before you can name the test that fails without it.

TWO THINGS THAT MADE THE ELIMINATION CHEAP, both worth reaching for early:

- **Ask what the observation rules OUT.** Here, that a restart cured it. A
  restart changes only in-process state, so every explanation living outside the
  process was excluded in one step, and the remaining candidates were a short
  list that could each be tested.
- **Read the published string back to its single construction site.** The lane
  reported `credential requires authentication`, which exactly one path produces:
  a *successful* vault answer of `needs_reauth`. That distinguished "we latched a
  stale verdict" from "we were told this, repeatedly, and correctly reported it"
  — opposite investigations, identical appearance from outside.

AND THE GAP IT EXPOSED, which generalises past this bug: every assertion about
that failure class checked that a failure DEGRADES, and none checked that the
degradation LIFTS. A refusal is visibly an outcome, so someone writes a test for
it; its un-refusal produces no symptom to write a test about. **The recovery
direction of a well-tested failure path is systematically the untested one.**
Check it where the prior state differs — a lane recovering from stale-serving
starts somewhere else than one recovering from degraded, and only the first has
a disclosure that must be cleared.

## After a timeout with no socket error, what happens to the connection?

"Nothing" is the defect. Found in this module 2026-08-22 and, once stated, in
four other hand-rolled clients across the fleet.

Every ordinary way a client drops a connection needs the socket to SAY
something: the reader sees EOF, the reader sees an error, or a write fails. A
socket killed while the host slept says none of those — writes are accepted into
the void, no FIN arrives, and the reader stays blocked. **Silence is the only
symptom**, and the only place it surfaces is a request timeout.

So a timeout that reports failure to its caller and leaves the connection
installed makes every later call reuse it, spend the full budget, and fail. The
lane is dark until the process restarts, which is why this class always presents
as *sticky until restart, cured by restart, everything reporting healthy
throughout*.

**The fail-before recipe**, which needs a stub rather than a unit test: accept
the connection, complete the handshake and any route setup, then **never
answer**, and hold the socket open — closing it proves nothing, because that
exercises the EOF path instead of the one under test. Call twice. If the second
call does not establish a NEW connection, the dead one was retained. A
per-connection counter is the cleanest observable.

**The asymmetry that settles the fix**: discarding a merely SLOW connection costs
one handshake, and a retry usually already exists. Retaining a dead one costs an
outage that ends only with a restart.

### The population this belongs to

Ask it of every long-lived resource, not the one that prompted the question.
This module's inventory, and what notices when each dies silently:

| resource | who notices |
|---|---|
| vault client connection | this module, on a call timeout — fixed `d7f262f` |
| HTTP connection pools | evicted by an idle timeout below the refresh cadence — `564edde` |
| the daemon frame loop | **the supervisor**, and barely exposed anyway — see below |

The third row was written down rather than left blank because "somebody else's
probe covers it" is exactly the belief that made the vault connection's gap
invisible. Checking it produced a better answer than the assumption, in both
directions.

**The supervisor restarts on silence, not only on reported failure** (confirmed
by SUBC at source in `supervise.rs`, 2026-08-22). A probe that times out records
`NoAnswer` and increments a consecutive-failure streak; the restart escalation
fires at the module's `failure_threshold`. A late answer resets the streak, and a
dead socket never produces one, so the streak marches and the restart lands.
Exposure is `probe cadence × failure_threshold + drain` — minutes here.

**And this row is barely exposed at all, for a reason worth keeping.** The frame
loop rides LOOPBACK. Silent half-open needs a real network underneath it — some
path whose state a sleep or a NAT can lose. Over loopback the death modes are
process exit and daemon close, and both arrive as EOF or RST. The disease needs a
network; the two rows that had one are the two that were exposed, and both are
fixed. I had assumed this row carried the same shape as the vault's, and the
physics says otherwise.

**The caveat that does apply**: the lifetime restart budget. Repeated silent
deaths exhaust `max_restarts` and park the module `Failed`/disabled, and the gap
is then unbounded until somebody looks. Measured here — `restarts 0/3` after a
dozen deploys in a day, so an OPERATOR restart (`ck module restart`) does not
spend the failure budget. Only real failures do.

### When a resilience fix ships in a shared SDK, the hand-rolled clients did not get it

`subc-client-rs` 0.7.0 shipped liveness probes fleet-wide. This module does not
use it — it depends on `subc-protocol` and `subc-transport` directly and
hand-rolls its frame loop — so the prevention landed everywhere except here,
silently. Nothing in a green fleet check distinguishes a module a fix skipped
from one it covered.

## A migration belongs to the constructor that asked where to live

Anything reachable from a general-purpose constructor is reachable from a test,
and a test that runs a migration is a test with production side effects.

The instance. Moving the redemption journal out of the retired `ck-quota`
directory needed a real migration — a renamed segment does not fail, it finds no
file, creates an empty journal, and an empty journal is indistinguishable from a
correctly migrated one while every pending redemption it fenced becomes free to
spend again. The migration itself was written carefully: adopt only when the new
path is empty and the old one is not, verify by contents, delete the old file
only after reading the new one back, leave everything in place on any error.

It was then called from `probe_atomic_write`, which every coordinator runs —
**including the ones tests build over temporary directories**. The legacy path
resolves from the process environment regardless of where the journal itself
lives, so a scratch journal is "empty" and the developer's real journal is
"populated", which is exactly the adoption condition. A plain `cargo test`
carried this host's real redemption records into a scratch file and deleted the
original, eight minutes before the deploy meant to migrate them.

**The rule:** a migration may only be reachable from the constructor that
resolved its path from the environment. Only something that asked the
environment where to live has any business reading the environment's previous
answer. That makes test code safe by construction rather than by care — the
alternative is every future caller of a general constructor remembering a
hazard that is invisible at the call site.

**The tell, generalised:** ask what else calls the function you are adding an
effect to. A side effect added to a widely-called helper inherits every one of
its callers, and in a test binary some of those callers exist only to be cheap.

AND A TEST THAT CANNOT FAIL ON ITS OWN BUG. The first guard written for this
seeded a legacy-shaped file in a scratch directory and drove the real
constructor. It passed — and it passed with the bug reinstated, because the
buggy path reads the ENVIRONMENT's legacy location and never looked at the
scratch one. Reinstating the defect and watching the test stay green is the only
thing that revealed it; the test name and the assertion both read as correct.
Where a property is not observable without mutating process-global state, assert
it over the source instead, which is decidable — the same move as fencing HTTP
client construction sites when the library exposes no way to read the setting
back.

## The dangerous constant is the one you do not own

Two constants can be correct alone and wrong together — that is already recorded
above, and the version of it that bit this module hardest had one constant that
is not ours at all.

`BASE_INTERVAL` is 60 seconds: how often the refresher refetches a unit. reqwest
keeps an idle pooled connection for 90 seconds by default. Sixty is less than
ninety, so a pooled connection was always handed back out before it could age
out, and was therefore never evicted. Harmless for the life of the module —
until a connection died without saying so. The host slept at 11:07:58 and the
first fetch failure landed sixteen seconds later; every provider failed at once,
because every pool held sockets opened before that sleep. The refresher then
reused those dead sockets every 60 seconds for TEN HOURS, **each attempt
refreshing the very idle timer that had to expire for them to be discarded**.

WHY THIS IS HARDER THAN THE TWO-CONSTANTS-YOU-OWN CASE, which is the whole
point of writing it down separately:

- **The other number is invisible.** Ours is declared in a file with a comment
  explaining it. Theirs is a default, written nowhere in this repo, discoverable
  only by reading a dependency's source. Nothing in review will ever put the two
  on the same page.
- **It can change under you without a code change.** A dependency upgrade can
  move their side of the relationship, and the diff that does it is a lockfile
  line. Our constant did not move; the relationship can still break.
- **The failure is delayed and unrelated.** The two constants met years of ticks
  without incident. What surfaced it was an unrelated host event, which is why
  the symptom arrived looking like a network problem rather than a design one.

WHAT TO DO ABOUT IT:

1. When a dependency's behaviour participates in a relationship with one of our
   constants, **write their number down beside ours, with the version and the
   file you read it in.** Not to trust it — to make its change visible when
   someone next reads the line.
2. **Prefer a bound you own to a bound you inherit.** Setting the pool timeout
   explicitly costs one builder call and converts an invisible default into a
   declared constant with a fence.
3. **Fence the relationship where it is decidable.** The constants compare
   directly. Whether the client actually *carries* the constant does not — the
   library exposes no way to read it back — so that half is fenced by walking
   the construction sites instead. Mutating the setting reddened nothing until
   that second fence existed, which is the exact "coverage does not flow through
   a call" trap one layer out.

AND THE READING DISCIPLINE THE INCIDENT TAUGHT: the symptom was every lane
failing at once with a transport error, on a box where `curl` reached the same
endpoints fine. That combination says the fault is in *process state*, not the
network and not the code — a fresh process fetching everything in twenty
seconds proved it in one step. **Ask what an observation rules OUT before
hunting for what causes it**; "a restart cures it" excluded every explanation
living outside the process and left a list short enough to test.

## A tripwire that is not firing cannot be proved by deleting it

Mutation proof has a standing shape here: delete the guard, and the test named
for it must redden. That shape assumes the guard is DOING something on today's
inputs. A drift fence is not, and most fences are drift fences.

The health-metric fence checks that every published metrics key is explained in
`docs/consumer-contract.md`. Today every key is, so the assertion passes on every
run. Deleting the assertion changed nothing and reddened nothing — correctly.
Reading that as "undefended" would have been wrong, and the instinct to then
"strengthen" a fence that is already correct is how a working check gets rewritten
into a worse one.

**Prove a tripwire by CREATING the drift it exists to catch, not by removing the
check.** Rename a published key so it no longer matches the doc: the fence fires
by name. That is the only mutation that establishes the thing is wired to
anything.

**And prove the vacuity guard separately.** A fence that iterates a collection
passes trivially when the collection is empty, which is the failure mode that
makes it silent forever — a renamed heading, an emptied payload, a scan whose
anchor stopped matching. Empty the input and confirm the guard bites. Two
mutations, because a fence has two ways to be useless and they are not the same
way.

The general form, worth checking before writing any proof: **ask whether the
thing under test is currently ACTIVE or merely PRESENT.** An active guard is
proved by deletion. A dormant one is proved by making it fire. Applying the first
recipe to the second kind produces a confident `NOTHING REDDENED` about code that
is working exactly as intended — the same false negative as mutating a
behaviour-neutral edit, arriving from the opposite direction.


## A sweep for what is missing fails by finding more of it

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

A corollary worth stating because it inverts the obvious: **a repository with no
history of a defect has no control for it**, so its clean sweep is
indistinguishable from a broken detector. The first time anything is swept for is
when the answer can least be trusted, and a codebase that has already been wrong
in some direction is better equipped to check itself along it than one that never
has.

### Print the premise beside the result

Every sweep rests on a definition — what counts as production code, what counts
as a call site, which files were even considered — and the numbers look identical
under any of them. A reader who would disagree with the definition cannot tell it
was used, so the assumption is the one part of the reasoning that never reaches
the output.

`scripts/prod_body.py` therefore names its boundary rule on every run, above the
counts. This is not a debugging aid: it is what makes a result auditable by
someone who did not write the tool, and that is the person most likely to catch
it — not being the one who chose the pattern is the whole advantage. The
incompleteness of this tool's own boundary was found exactly that way, by a
reader who knew which anchor it used.

**Derive the printed premise from the thing that enforces it.** Written by hand
beside the pattern, it is prose next to a derived artifact and rots the same way:
it agrees on the day it is written and stops agreeing when someone edits the
pattern, while still carrying the authority of an explicit statement. A premise
that can disagree with the code is worse than none. The check is one mutation —
change the rule and confirm the printed line changes with it; here, anchoring on
`mod checks` left the output still claiming `mod tests`.

Better still, publish a number the rule itself produces. There is an ordering:

| form | can it disagree with the code? |
|---|---|
| implicit — the premise is only in the author's head | always |
| transcribed — written by hand beside the rule | after any edit |
| derived — read out of the rule | only if read from the wrong thing |
| structural — an output of applying the rule | not without the result being wrong too |

This tool prints the **share of bytes its boundary kept**, which was already
being computed per file and discarded. It moves under both ways the boundary can
fail: an anchor that never matches keeps whole files and drives it to 100%, and
cutting at the first `#[cfg(test)]` attribute rather than the module drops it to
51% and changes a sweep's answer. A reader expecting roughly two thirds and
seeing 5% knows the result answers a different question, without knowing
anything about the pattern.

One number is not enough when a tool applies more than one rule. The byte share
reports on the boundary; it says nothing about the separate rule that detects
test-only items left inside the body. Disabling that one produced output
identical to a clean run **minus a line** — same share, same counts, and a caveat
that simply was not there. **A reader cannot notice a line that is absent**,
which is the failure the caveat exists to prevent, pointed at the caveat itself.
So every rule reports on every run, zeros included: a caveat count of `0` is a
statement, an omitted caveat is nothing at all.

The direction matters when choosing what to publish. A rule that SELECTS from
the corpus moves the result when it breaks; a rule that DESCRIBES the corpus does
not, so its failure is invisible unless its own output is printed.

That makes the audit mechanical rather than a matter of noticing: **enumerate a
tool's rules, classify each one, and confirm every describer has an
unconditional number of its own.** `scripts/prod_body.py` has four, and each was
checked by breaking it and reading the output:

| rule | kind | what moves when it breaks |
|---|---|---|
| the test-module boundary | selector | byte share — 100% if the anchor never matches, 51% under a first-attribute cut |
| which files match the needle | selector | the match count — 10 becomes 43 when inverted |
| truncation detection | describer | its caveat count, 4 to 0 |
| test-only items in the body | describer | its caveat count, 4 to 0 |

And when only one number can be afforded, publish the count of what a rule
**affected** rather than what it **considered**. A pattern rots by matching less
rather than more, so the likelier failure of any filter is that it stops
filtering — which leaves the considered-count untouched while the affected-count
goes to zero.

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

### Why a check ends up narrower than what it checks

The failures below are all one shape: **a verification scoped to the example
rather than to the rule**. It runs, produces a value, and answers a question
adjacent to the one asked — which is why it reads as evidence.

The cause is worth stating because it defeats care. A check is written by
whoever best understands the thing being checked, at the moment of understanding
it, immediately after solving the specific case in front of them. So its scope
inherits the shape of that case. Being careful produces the narrow version,
because the narrow version is exactly what the example makes salient.

That is why the audit has to be a **separate pass, later**, asking one question
of each check: *what shape does this inspect, and does its control contain a
healthy instance of that shape?* Doing it in the moment does not work, and
passing the audit is the only way to distinguish a control that holds by design
from one that holds by luck — both look identical until something mutates them.

### A rule test needs three properties, not one

A test for one rule of a multi-rule checker passes for the reason in its name
only if all three hold:

- **Detection** — it fires on input the rule should reject.
- **Discrimination** — a paired test proves it stays silent on the neighbouring
  case. Without it, a rule that rejects *everything* passes review: the
  past-reset test would go green against a version reporting every reset at or
  before now, and the used-count test against one reporting every window that
  fills up.

  **The silent case must contain a healthy instance of the shape the rule
  inspects** — not a healthy entry, not a healthy neighbour. A rule checking
  extra windows was paired with a control holding only slots, so the over-wide
  version, firing on every extra window rather than only an empty one, passed
  the whole suite. The control was scoped narrower than the rule and proved the
  rule stays silent on data it never examines, which is no evidence at all. Same
  defect as a detector's blind spot, one layer up.
- **Isolation** — the fixture triggers no other rule, so the rule under test is
  the only explanation for the finding.

The third is the one that hides. A fixture whose used count exceeded its total
by 20% also tripped the counts-versus-percent rule beside it, and the assertion
searched the findings list — so the test would have passed on the neighbour
alone. Overrunning by one instead, with the percent agreeing, leaves a single
cause. Asserting the finding **count** rather than searching the list is what
makes that isolation load-bearing rather than incidental.

The same defect appears on the mutation side: a mutation must not move the input
into a neighbouring rule's domain. Replacing a `must be present` guard with a
default that yields an empty collection hands the input to the next guard, which
rejects empty — the suite stays green for a reason unrelated to coverage.

And on the assertion side, which is where it hides in this crate: **several rules
in one file report the same `FetchError` variant**, so a test asserting only the
variant is satisfied by any of them. `kimi_for_coding` has two failure paths that
both report `Decode` — the body not parsing, and the parsed body carrying no
usable window — with four tests named for specific causes and none of them saying
which path it expected.

The refactor that exploits this is ordinary rather than contrived: making a usage
field required moves a missing-value body from the window guard to the parse
error. Every test stays green while three of them silently begin exercising a
different rule. Pin the message, not the variant, wherever a file has more than
one rule producing the same one.

A fix to a test's quality needs its own fail-before, exactly like a fix to code:
apply the mutation with the *old* assertion and confirm it passes. Otherwise
there is no evidence the sharpening changed anything — and a sharpened assertion
feels like an improvement by definition, so nobody asks for the proof that any
code change would need.

Run it per assertion, not per batch. A refactor that moves one test's input into
a neighbouring rule's path leaves the other tests untouched, so one demonstration
says nothing about its siblings. All four assertions in `kimi_for_coding` turned
out to be genuinely load-bearing, but that took four separate refactors — a
required field, a non-positive check hoisted ahead of the guard, and lenient
parsing that lets an unparseable body reach the window guard instead of failing
at the parse.

A red result is not the end of it either. **Check that the pinned run fails at
the assertion rather than before reaching it.** A mutation can break the setup
instead of the behaviour — removing a guard so the input now parses cleanly makes
`unwrap_err()` panic on an `Ok` value, and the test dies before the assertion is
evaluated. That reddens without saying anything about the assertion's strength.

**Read the panic message, not the line number.** A stack trace from a mutated
build is indexed against the mutant, so every line below an inserted or deleted
block is displaced — and the displaced number lands on some other real statement,
which may be the `unwrap_err()` that would suggest exactly the wrong conclusion.
The message is unambiguous where the line is not: a panic from `unwrap_err()`
reports an `Ok` value, while a failed assertion prints its own text.

That only works if the assertion carries a message worth reading, so state the
expectation in it rather than only the value:

    expected the window guard, got: decode error: ... not decodable: missing used

A bare mismatch says a test broke. Naming the rule that was expected, beside the
one that answered instead, says *why* — which is the question a green mutation
leaves open.

When a sharpening cannot be shown to matter, keep it and label it **prophylactic
rather than proven**. Those are different claims, and recording the weaker one is
the difference between a documented margin and an imagined one.

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

**The edit landing is not the same as the state being reached.** The paragraph
above catches a mutation that edited the wrong text. There is a second cause of
a green result that survives it: an edit that lands exactly where intended, on
the line you meant, and still fails to produce the forbidden state — so the code
never behaves differently and the suite is right to stay green.

Found here gating a descriptive field on the *observation* existing, to prove
that field independent of the verified identity. The mutation applied cleanly to
the correct line. But an unresolved handle still HAS an observation — the null is
the `account_id` inside it — so the gate was already satisfied and nothing
changed. Gating on the identity itself reddened the test immediately. The first
result was a verdict on the mutation, not on the coverage, and it is
indistinguishable by exit code from a genuine gap.

So a mutation carries two obligations, and only the second makes the first
meaningful:

1. does the target test go red when the guard is removed, and
2. does this mutation actually produce the state the guard forbids?

Question 2 is answered by constructing the forbidden state directly and checking
the mutated code reaches it — usually by reading what the mutated expression
evaluates to for the fixture at hand, rather than trusting that a plausible edit
implies a changed behaviour. The trap is sharpest where a value is nested inside
an `Option` that is itself always present: the outer layer reads as the thing
being tested while the inner one carries the meaning.

**Stage or commit before mutating, then restore from the index.** The reference
copy has to be made authoritative *before* the edit, and both ways of skipping
that step fail silently in opposite directions.

A snapshot taken into a scratch directory goes **stale**: restoring one taken
before an earlier round reverts everything since, including committed work,
leaving a clean tree and a green suite. Of the snapshots left over from one day's
mutation rounds here, several had been overtaken by later commits to the same
file, and restoring any of them would have reverted between four and ninety
lines. The byte-identical check above catches a *partial* revert against the
snapshot and cannot catch a *complete* revert to the wrong version, because the
file then matches its snapshot exactly.

Restoring from the index instead is stale-proof and discards **uncommitted
work**: mutating a rule to prove a test you have just written, then running
`git checkout -- <path>`, deletes that test, because the index holds the version
from before it existed. The suite goes green again and looks like a successful
restore.

Staging first closes both. The index then holds exactly the state the restore
should return, rather than a copy that is merely old or merely clean.

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

## A verdict over a set needs its denominator, and homogeneity hides the gap

From a consumer whose capacity axis read healthy for months while one provider's
row had not moved in a week (2026-08-26). Their diagnosis generalises past that
incident and has now been tested here.

**The tell:** any health verdict computed over a SET rather than a single subject
needs its population PARTITIONED BY MEMBER STATE, and the test is whether ONE
ANOMALOUS MEMBER WOULD BE VISIBLE IN THE OUTPUT. If not, the verdict is a claim
about the healthiest member wearing the population's name.

**Partitioned, not merely counted** — that wording is the result of running this
test rather than the version that arrived. Publishing a denominator is only one
way to satisfy it, and not the way this module does: what makes an anomalous lane
visible here is that each provider lands in a bucket by its own state and the
buckets are forced to sum. A count beside an aggregate would not have done it.

**Why these survive review:** they are undetectable while the population is
homogeneous. Their axis was correct for months because every provider refreshed,
and it became wrong the moment one stopped — with no code change. Nothing in the
verdict's own history warns you, because its history is the period when it
happened to be right.

Four instances across three modules, all the same shape: a drop count that could
not separate a quiet host from one where nothing was comparable; a confidence flag
whose run of `true` could not separate always-continuous from a stuck arm; their
axis verdict; and their per-axis staleness reading. In every case the aggregate
was TRUE and the population UNSTATED.

**Tested against this module's health surface, and it passes — for a reason worth
stating rather than by luck.** `lastFetchSuccessAgeSecs` really is an aggregate
(`max` over slots, so the freshest wins) and would hide a stalled lane on its own.
It is not on its own: every provider is partitioned into `fresh` / `stale` /
`pending` / `degraded` / `unconfigured` / `withoutHandles` by ITS OWN state, so an
anomalous member moves buckets and shows up. The conservation identity is what
makes the population visible, which was not why it was built.

**Where the shape does exist here, and is documented rather than fixed:** health
buckets are per PROVIDER, and a multi-account provider is bucketed by its best
slot — one healthy account and one withheld reads as `serving`. At the account
level, one anomalous member is invisible in health.

That is THE SAME DEFECT ONE LEVEL DOWN, not a smaller one: a provider bucketed by
its best slot is precisely "the healthiest member wearing the population's name".
The shape survived the fix at the provider level and reappeared at the account
level, which is what to expect of it — fixing an instance does not exhaust the
class, and the next place to look is one level in.

The remedy is not a better
health number; it is that account-level questions are answered from the `usage.get`
array, which carries every account separately. Stated in the consumer contract as
"health cannot substitute for this".

## An instrument reports on its own subject, not on yours

From a peer exchange (2026-08-26) after a commit here reported success while
having changed nothing. Four instances across two modules, and the shape is not
about editing.

**`git commit` reports on the repository, not on your edit.** An edit that
silently did nothing plus a commit that finds a clean tree still prints a hash —
the PREVIOUS one — and any reasonable reader takes it as the commit just made. A
silent no-op fails quietly; this composition fails AFFIRMATIVELY, producing a
positive artifact that needs no further mistake to become permanent.

Same shape elsewhere: `lsof` reports the path recorded at open time rather than
the file on disk now, which is how a peer ran green for hours against an orphaned
inode while every path check passed. A deploy step reports what it SENT, not what
is serving. In each case the instrument is honest about its own subject and the
reader supplies a subject it never claimed.

**The remedy is one move: when a step reports success, check the THING YOU
CHANGED, not the thing that reported.** Grep the file for the text you believe you
wrote. Read the wire after deploying. Compare inode to path rather than path to
path.

**What actually failed here is worth separating from what looked like it failed.**
The edit script's guard DID fire — it asserted its patterns and raised. The defect
was that the surrounding shell ran `git add` and `git commit` regardless, because
the heredoc's exit status was never consulted. So the lesson is not "add
assertions to edit scripts" (`scripts/probe.py` already refuses on a missed
pattern and says so); it is that A GUARD THAT FIRES INTO AN IGNORED EXIT STATUS IS
NOT A GUARD. Same family as reading an exit code through a pipe, which this repo
already forbids in `docs/deploying.md`.

**And the trigger was punctuation:** two hyphens against an em-dash, invisible in
review and total in matching. A guard should print the pattern that missed rather
than a count — a count says something missed, the pattern says it was punctuation.

### Three layers, three ways to claim success about the wrong subject

Converged with the same peer, who reproduced the composition failure on their own
side and supplied the third:

| failure | what claims success | detectable by |
|---|---|---|
| **drift** — pattern missed | the edit | asserting `old in text` before replacing |
| **partial** — pattern matched fewer sites than intended | the match | counting sites, not just presence |
| **composition** — guard fired into a channel nobody read | the report | consulting the exit status |

**Partial is the one that defeats the obvious guard**, because the assertion is
SATISFIED: the pattern did match, just not everywhere it should have. Drift is
loud once guarded; partial is silent by construction.

**Checked here, and `scripts/probe.py` is immune by a stronger route than
counting: it REFUSES on a multi-site pattern** (`pattern occurs N times; make it
unique`) rather than applying to the first. Driven rather than read — a pattern
occurring 13 times was refused, and a single-site control still reddened two tests
by name, with the tree byte-clean after both. A guard nobody has seen fire is a
guard nobody has measured.

**Their reproduction found the shell detail that makes composition invisible in
review:** a raising heredoc followed by `&&` stops the chain, and the same heredoc
followed by a NEWLINE lets the next command run with the error text still on
screen. One character of punctuation, identical at a glance.

### The fourth is upstream of all three: the anchor means something else

An anchor that matches EXACTLY ONCE, satisfying every guard above, in a place that
is not the place you meant. Pattern found, one site, exit zero, tree dirty, commit
real — the artifact is wrong and nothing in the pipeline is.

**Not a fourth stage.** The three above are failures of EXECUTING an intent;
this is a failure of the intent-to-anchor step, which is MODELLING rather than
execution. It fails when your mental map of the file has drifted from the file —
the same root as mode one, one layer earlier, producing a false POSITIVE where
drift produces a false negative.

**Two instances here, both already recorded and neither previously connected to
the others.** Probing a corpus guard, the mutation hit the failure MESSAGE rather
than a map entry; probing an extraction floor, it appended a comment to a mapping
arm. Both were unique, both applied cleanly, both reported NOTHING REDDENED with
complete accuracy about the wrong thing.

**What catches it here is not a guard, it is the naming discipline.** A mutation
proof must redden the TARGET TEST BY NAME, so a wrong-site anchor shows up as the
wrong tests reddening or none at all. That rule was written to prove a mutation
was not vacuous; it also catches anchoring in the wrong place, which is the
stronger reason to keep it.

**And the uncomfortable cost of refusing multi-site patterns:** forced to make a
pattern unique, the natural move is to extend it with neighbouring text until it
matches once — and the extended pattern can be unique in the WRONG PLACE just as
easily. UNIQUENESS IS NOT CORRECTNESS. Worth knowing at the moment the refusal
fires, because that is exactly when the temptation appears.

### A diagnostic that defaults an absent field to zero fabricates a measurement

This repository's standing rule is that absence and a stated zero must not
collapse. It is enforced on the wire, in provider normalizers, and in the contract.
It was NOT enforced in the ad-hoc probes used to read this module's own health, and
the result was a fabricated datum handed to a peer as a measurement.

The probe read `(metrics.get('uptimeSecs') or 0) / 3600` and printed `0.0h`.
`uptimeSecs` IS NOT A PUBLISHED FIELD. The output was indistinguishable from a
module that had just restarted, so a stall was reported with a restart beside it,
and a peer spent a log pass disproving the restart against their retained daemon
log. `ps -o lstart=` settled it in one line: the process had been up three and a
half hours.

**Third instance this month, all in diagnostics rather than in shipped code.** A
provider read an omitted block as a stated emptiness; a probe read a renamed metric
as an unpopulated counter; this one read a field that never existed as a zero. The
shipped code has guards for exactly this and the throwaway scripts do not, which is
the whole pattern: THE RULE WAS APPLIED WHERE IT WAS WRITTEN DOWN AND NOT WHERE IT
WAS TYPED IN A HURRY.

**The first remedy written here was vigilance, and a peer caught it within the
hour:** "print the raw value before deriving anything" is a rule enforced only by
care, which this document has an entire section warning about. Written one turn
after recording that warning.

**The mechanical version is `scripts/health.py`**, which removes the site rather
than guarding it. It requires nobody to name a key or supply a default: it prints
EVERY published metric with its raw value, and a key asked for by name that is not
published renders `ABSENT (not a published metric)` and exits 1. Driven against
the exact call that fabricated the datum — `uptimeSecs` now reports ABSENT where
the hand-written probe reported `0.0h` — with a real key as the control.

`or 0` and `or []` remain the constructs to distrust wherever they survive: they
are load-bearing defaults wearing convenience syntax.

**And prefer an independent instrument for a claim about process identity.** The
health surface cannot answer "did this process restart" at all, and no amount of
careful reading of it would have; `ps -o lstart=` answers it directly and shares no
machinery with the thing under suspicion.

### A constant divided by your sampling rate is not a measurement

A consumer reported the same figure across nine independent episodes: the number
of samples between a credential's first refusal and the vault's verdict was ten,
every time. Nine consistent observations of a system under real load reads as one
of the strongest results on that thread. It was arithmetic.

Both endpoints are anchored to THE SAME EVENT. This module's report fires on the
provider's refusal; the vault's verdict is only observable on the next attempt,
which `NON_TRANSIENT_BACKOFF` puts 300 seconds later. The consumer polls at 30
seconds. Ten is `300 / 30`, and the repetition across episodes was the repetition
of two constants, one of which has a unit test asserting it directly.

**The tell is that both ends of the interval trace back to one event.** An interval
between two consequences of the same cause measures the machinery between them,
not the world. It cannot vary with the thing you are watching, so it cannot
falsify anything about it — and the consumer proved this by deriving what a
shorter prefix would require: an earlier provider refusal makes the report fire
earlier and the verdict land one backoff later still, leaving the count unchanged.

**The question that number was supposed to answer was mine, and it could not.** I
asked whether a short prefix would show the provider failing before the credential
did. Nothing about that count could have shown it. The observable that answers it
is TERMINAL rather than DURATIONAL: a refusal phase that ends in RECOVERY instead
of in a verdict — a refusal the vault declines to convict — which is visible in a
single sample and would mean this module reported an auth failure for a credential
that was fine.

**Where else this shape appears here:** any figure derived from `BASE_INTERVAL`,
`FETCH_DEADLINE` or the backoff ladder divided by an observer's cadence. Before
treating a repeated interval as evidence, ask which events bound it and whether a
change in the phenomenon could move either end independently. If not, the number
is a property of the instruments.

**The consumer then found the stronger form, and it is the one to remember.** A
third figure had been treated as corroboration: the credential store's own audit
log timed report-to-verdict at ~301s, from a different system with its own clock
and its own log file. It is the same constant again. This module holds no token
cache -- `fetch_vault` calls `credential_source.get` inline per attempt
(`anthropic.rs:294`), which the F1 identity fence requires -- so the store's
verdict is not merely OBSERVED on the next attempt, it is CAUSED by it: the
inline refresh that produces the verdict cannot happen until this module asks
again, one `NON_TRANSIENT_BACKOFF` later.

So the store's number is `300s + one refresh round-trip`, and three instruments
were reporting one constant.

**A number arriving through an independent instrument is not thereby an
independent measurement.** Independence is a property of what CAUSES the number,
not of what reports it -- and a figure from another team's system, in another
process, with its own timestamps, is the most convincing possible presentation of
a value your own constant produced. The falsifier is to break the causal link
rather than to compare the numbers again: if a verdict ever lands ~300s after a
report while this module is NOT attempting, the store has its own timer and the
two are genuinely independent.

**RUN AND SETTLED 2026-08-26** (insula#8), and by a better experiment than the
one proposed. Mine asked for an ABSENCE — stop the module for 400s and watch for
no verdict — which costs every provider lane going dark and is only as strong as
the enumeration of everything else that could suppress a verdict. The filer
inverted it to ask for a PRESENCE: issue a single off-schedule `credential.get`
well inside the window, and watch the verdict arrive early.

```text
ep13  one off-schedule get at report+60.2s   verdict +60.7s   prefix collapsed
ep14  unperturbed control, same everything    verdict ~+300s   prefix 9 or 10
```

One frame separates them. So the ~301s is this module's `NON_TRANSIENT_BACKOFF`
plus a round-trip, twelve prior episodes agreed because they shared a cause, and
the store's figure was never corroboration.

**A COUNT OBTAINED BY SAMPLING IS NOT AN INTEGER PROPERTY OF THE MECHANISM.** The
filer first published the prefix as "exactly 10", citing two independent recorders
agreeing, and then corrected it when a third episode had the same two recorders
disagree — 9 against 10, from a 15s phase offset between them. Both readings were
correct: a 300s interval sampled every 30s yields 9 or 10 observations depending
on where the observer's phase falls against the window boundaries.

The correction is worth more than the number. Two independent recorders agreeing
is the strongest evidence available for a sampled quantity, and it still did not
establish an integer — because both shared the cadence, so the agreement was
about their common phase rather than about the mechanism. The same shape as the
single-consumer confound below, one level down: agreement between instruments
that share a property carries no information about that property.

Stated correctly: the rejected phase lasts one `NON_TRANSIENT_BACKOFF`, and a
poller measures it to within its own sampling period. `docs/consumer-contract.md`
says "about ten samples" and did not need correcting; this file said "exactly"
and did.

**The general form is theirs and it is the part to keep: a single-consumer system
cannot distinguish a property of the server from a property of its only client.**
No amount of further observation of the normal loop would have caught it, because
the confound lives IN the loop — it took a stimulus the only client would never
produce. Watch for it wherever this module is the sole caller of something whose
timing we then characterise.

**A THIRD INSTANCE, and the only one where the shared property is PERMANENT.** A
consumer building a shadow reader against an external quota-header feed confirmed
at the producer's source that only unified `5h` and `7d` headers exist on that
lane. Every scoped window in that feed therefore arrived by POLLING THE SAME
ENDPOINT THIS MODULE POLLS — so their feed's `Fable` reading of 62 beside our
`extraRateWindows` 62.0 is not two instruments agreeing, it is one measurement
seen twice.

That matters for the feed design on issue #7, where the rule was heading toward
"a feed fact may UPDATE a window where sameness is established by paired
measurement". Paired measurement cannot establish it here: for scoped windows the
two readings share a source by construction, so the agreement table would be
filled with tautologies for exactly the windows where independent confirmation was
the point. **Divergence stays informative — two readings that disagree cannot both
be the same source — while agreement does not.** The asymmetry is the usable part.

The first two instances were accidents of timing that a different measurement could
dissolve. This one is structural, so it should be encoded as a precondition rather
than re-derived: before treating any second source as confirmation, establish that
it does not reach the same upstream by the same route.

**A related trap from the same reading, worth its own line: the same quantity on
two surfaces can carry two scales.** Raw wire headers are `0-1` floats; the feed
publishes them already multiplied and clamped to `0-100`, which is our scale too
(`anthropic.rs` clamps to `0.0..=100.0`). A consumer told "headers are 0-1, apply
100x" and pointed at the FEED would be 100x high on every window — and the error
would read as a plausible number rather than a fault. If a normalisation is ever
applied to an inbound feed here, it belongs behind an assertion that REFUSES on a
suspected scale mismatch, never behind a silent correction.

### Refusal is correct only where the walker knows the value is adjudicable

A rule written for TYPED VALUES, applied by a traversal that also reaches PROSE,
refuses on content that merely contains a value-shaped substring. Where a walk
might be traversing content rather than fields, the honest arms are pass-through
or report — never refuse.

**We shipped this bug and a reviewer caught it.** `PoolFunding` / `PoolBasis` on
the shared wire crate originally failed deserialization of the WHOLE
`ProviderUsage` entry on an unrecognised variant, so one unknown funding kind
would have discarded an entry carrying a live 42% window. The fix was fallback
variants (`Unknown`, `Unstated`), not a longer list of known ones — a longer list
defers the next unknown, a fallback ends the class.

**And we have the mirror case, which survived for a reason worth naming.**
`scripts/public-flip-verifier.py` applies credential patterns to every blob in
history, necessarily including test fixtures. It found three hits and all three
were legitimate: two are `user:pass@host` URLs that exist to prove the userinfo
guard REJECTS them, so deleting the specimens would delete the tests defending
the exposure class the scan is for.

It survived because it REPORTS FOR A HUMAN READ rather than refusing. The pattern
design was not better than the one that broke a peer's module — the same false
positives fired. **Report-versus-refuse was the load-bearing choice**, and that
was not obvious when the tool was written.

THE STRUCTURAL FIX BEATS THE BETTER PREDICATE: fix the traversal so it cannot
reach content, rather than sharpening the pattern so it matches less. A sharper
pattern is a patch; the next document with different prose finds it again.

### Exit codes name the mechanism, never the intent

An exit code reports what happened to a process. It does not report what the
result MEANS for the question you were asking, and the gap between those is where
false proofs live.

**Found in `scripts/probe.py`, the tool every mutation proof here depends on.** It
returned `1` for a hung suite, and `1` is documented as "something reddened (the
usual proof)". The printed text was honest — "load-bearing, but no test named it"
— but a caller reading only the exit code would have been handed a proof nobody
wrote. HUNG now exits 2, with load-bearing and defended kept as separate claims.

Three more instances from one day, all the same shape:

| observed | mechanism | what it was read as |
| --- | --- | --- |
| gate exit 1 after a mutation | formatting failed first | the doctest arm proving itself |
| `timeout` kill, exit 124 | SIGTERM skipped a `finally` | a clean failed run, tree left mutated |
| peer module exit 101 | deliberate refusal-before-write | a crash mid-migration |

The last was a peer's, and the correction is the reusable part: **right about the
signal, wrong about what it meant.** The rollback was byte-clean every attempt,
which is why their recovery was pure fix-forward rather than repair.

THE CHECK: before reading a non-zero exit as evidence for your question, name the
mechanism that produced it. If more than one mechanism produces the same code,
the code is not the evidence — the text is, and an automated caller reading only
the code is being told something nobody asserted.

### A median over repetitions reports the cached cost, not the first-time one

Measured 2026-08-27 while confirming a peer's hypothesis that macOS code-signature
validation cost scales with binary size. It does: fresh-path first exec runs about
300 ms fixed plus 5 ms/MB, from 363 ms at 9.8 MB to 1257 ms at 188 MB.

My first sweep reported 3.5-9.7 ms flat across that whole range -- a clean, tidy
refutation of a true hypothesis. The instrument copied each source to a FIXED path
three times and took the median. Repetition 1 pays the first-exec cost;
repetitions 2 and 3 hit the validation cache; the median of one expensive and two
cheap samples IS the cheap value. The statistic averaged away the only thing being
measured.

**The rule: when a measurement has a first-time cost and a cached cost, take the
first sample or none.** Any central statistic over repetitions is a measurement of
the cache.

WHY THIS IS THE MOST DANGEROUS MEMBER OF THE FAMILY SO FAR, in the peer's
formulation: the instrument did not fail silent, it failed CONCLUSIVE. A vacuous
test passes and tells you nothing, which at least leaves the question open. This
produced a specific, plausible, internally consistent number that contradicted a
correct hypothesis -- and had it not been checked against a differently-shaped
measurement (fresh unique paths), it would have closed the question in the wrong
direction with data attached.

Same shape as the guard-resolution blind spot on the checker side -- a check whose
own resolution hides exactly what it measures -- but on the instrument side, where
the output is a number rather than a pass.

THE CHECK: for any instrument reporting a summary statistic, ask what the FIRST
observation was and whether it differed from the rest. If it did, the summary is
describing the steady state and the transient is the thing you were after.

### Filing a defect as a skill problem terminates the search for a mechanism

The two instances above sat in these notes for weeks as "probe selection is itself
a skill". That framing is why no catcher was ever looked for: a lesson whose only
remedy is CARE is unfalsifiable and actionable only as try-harder, so it closes
the question before a mechanism can be found. The naming discipline had been
catching this class the whole time and nobody noticed, because nobody was looking.

**The tell is a recorded lesson whose remedy is care.** When a write-up ends in
"be more careful about X", the class has been named and the search abandoned in
the same sentence. Ask instead what would have caught it without anyone being
careful — and if the honest answer is nothing, say THAT, because "no mechanism
found" stays open while "a skill problem" does not.

### The layer above intent: is this the right change

One half is judgement — does the design serve the product — and no tool reaches
it. The other half is mechanical and worth separating: **does this contradict
something already settled?** A change can be locally correct and globally
contradictory, and detecting that needs no taste, only the settled set written
somewhere a checker can hold.

**Measured here, and the split is uneven.** Statement-versus-code contradictions
ARE fenced: the wire error classes against their contract table, the health
metrics against their documentation, the opaque parity constants against their
source markers, the provider count against the registry. Each of those has caught
a real drift.

**Statement-versus-statement is not fenced at all**, and this repo has hit it three
times — a `fetchedAt` fact stated differently in two documents, a prose count
beside its own five-row table, and an exhaustiveness claim that a later fix
falsified while the claim stayed. All three were found by hand, after the fact.
The standing practice ("when correcting a fact in one doc, grep every other doc
for it") is a HABIT, not a mechanism, and habits are what get optimised away when
a step feels slow.

## A guard that prevents and a guard that reveals are not substitutes

`ManifestProvenance.wire_crate_version` was populated from our own lockfile when
the field means the LINKED PROTOCOL crate — two different numbering spaces, so a
census gate reading it would have scored this module non-conformant while it
demonstrably spoke the current protocol. A confident wrong answer, which the
contract's own rule says is worse than an absence.

The fix was one line. What took longer was working out what still guards it.

For a day, nothing here could check that declaration at all: the manifest is sent
once at HELLO and had no consumer-visible readback, so the module's own inspection
could confirm what it INTENDED to send and nothing more. That is worth naming as a
class:

> A declaration with no readback path is unfalsifiable by its own producer.

The gap between "the binary computes X" and "the peer received X" is exactly where
serialisation bugs and version skew live, which is why an external census found
this and local inspection could not have.

When the readback shipped, the daemon-side maintainer offered that the identity
assertion in our test was now retired. It is not, and the readback's own output
says why:

```
RUNNING IMAGE: match (macos_spawn_inode)   <- a COMPARISON the daemon performs
COMMIT / LOCK DIGEST / WIRE CRATE VERSION  <- no annotation: RELAYED
```

Regress the field back to the lockfile read and `ck provenance` prints the wrong
version faithfully. Nothing fails. **The test PREVENTS the wrong value shipping;
the readback REVEALS it to whoever looks.** Retire the first because the second
exists and the regression becomes detectable only by someone who happens to check.

The pair generalises — it is the same split as write-ahead intent versus done-probe
(record what you meant, check what the world holds). But the halves are NOT equally
strong, and the asymmetry is the argument for keeping the preventing half: a
done-probe checks against a WORLD that can disagree with you, while a relayed
declaration checks against nothing. The weaker second half is the reason the first
one stays.

Practical form: when adding a field to any outbound manifest or handshake, ask
which of the two you have. If the value is relayed rather than compared, the test
asserting its REFERENT — not its shape — is the only guard, and a shape check is
not one. `wire_crate_version` had a test that checked "three dot-separated parts",
which passes just as happily on the wrong crate's version.

## An overstated rule manufactures its own counterexamples

A finding of ours went into a fleet recipe: a lock-only commit changes the binary,
because `build_lock_digest` hashes the on-disk `Cargo.lock` at build time, so a
module that absorbs a lock wave and skips its next deploy declares the PRE-wave
digest. Proved across a commit with zero source lines changed, read from the
provenance readback at both ends.

True here, and not true everywhere. Checked at source across the fleet: six modules
populate that field, three pass `None` — and those three have no `build.rs` at all,
so nothing reads the lock and their binaries are byte-identical across a lock-only
commit on a path-dep workspace.

The cost of shipping the rule unscoped is NOT the wasted rebuild. It is that a seat
in the second group follows it, rebuilds, gets an identical hash, and now holds
first-hand evidence AGAINST a rule that is correct for the six seats where it
bites. **An unscoped rule hands its own counterexample to the people it does not
apply to**, and they are the ones who will repeat it.

The roster is not the fix either, because a list of instances is dated the moment
one of the three adds a build script. State the PREDICATE and let each reader
evaluate it against their own tree — and prefer a predicate that needs no source
interpretation. Here the decisive one is empirical.

THE VERSION THIS PARAGRAPH ORIGINALLY GAVE — build both commits with
`CK_QUOTA_BUILD_COMMIT_OVERRIDE` set identically and compare hashes — IS WRONG
and is kept only as the worked example. It stopped working when a second
git-derived stamp landed: `CK_QUOTA_PROVENANCE_SHA` embeds the real HEAD sha and
deliberately resists override, so two builds at different commits always differ
whether or not any runtime code changed. Nothing failed when that landed; the
procedure kept running and started answering a different question.

Which makes it the best illustration of the rule it was written to support. A
STATED PREDICATE ALSO ROTS — it just rots quietly, because a reader evaluating it
gets an answer and has no reason to doubt one. The roster was dated the moment a
module added a build script; the predicate was dated the moment a second stamp
did.

So the surviving guidance is one level up: state the predicate AND what would
make it stop being decisive. Here: this comparison is decisive only while exactly
one embedded value varies with the commit. The determinism control (rebuild ONE
commit twice, confirm the hashes match) remains necessary and is not sufficient —
it proves the build is reproducible, not that the thing you are comparing is the
thing you care about. Mine passed cleanly while the comparison was meaningless.

The working check for this particular question is in `docs/deploying.md`: diff
the source and read the changed file list, since a change confined to `tests/` or
`examples/` cannot reach the binary.


## Text about a property clusters where the property is absent

A fleet census of ours said six modules populate `build_lock_digest` and three pass
`None`. Two of the six were wrong, caught by the maintainers of those modules
reading their own source. Re-derived properly, it is four and two.

The instrument was one line:

```sh
grep -h 'build_lock_digest' | grep -vc 'None'
```

It counted MENTIONS-WITHOUT-THE-WORD-None as ASSIGNMENTS. The two false positives
in `cerebellum` are worth reading in full, because of what they say:

```
manifest.rs:173  /// That distinction is why it is filled while `build_lock_digest` is not
manifest.rs:186          build_lock_digest: None,
manifest.rs:652              provenance.build_lock_digest.is_none(),
```

Line 173 is a doc comment stating the field is NOT filled, counted as evidence
that it IS. Line 652 is a test asserting the field is absent — `.is_none()`,
lowercase, which `grep -v 'None'` does not exclude — counted as a population.
**Both false positives were statements of the truth, read as their opposite.**

That is not bad luck, and it generalises past greps:

> When you search for evidence of a property, the text that DISCUSSES the property
> clusters exactly where the property is ABSENT — because absence is what needs
> explaining. A present thing is usually one unremarked line.

So a mention-counting instrument is biased toward false positives precisely in the
population it exists to exclude, and the more carefully a team documented their
decision NOT to do something, the more certainly it is miscounted as having done
it. `cerebellum` and `claustrum` were miscounted BECAUSE they wrote down their
reasoning; a module that silently omitted the field would have been classified
correctly.

The fix is to match the CONSTRUCT, not the NAME: `build_lock_digest:` followed by
something that is not `None`, excluding comment lines. Better still, prefer a
predicate needing no source reading at all — for this question, build two commits
and compare hashes.

**THE BIAS IS IN THE MEASUREMENT; THE HARM IS IN WHAT IT FEEDS.** Ran this rule
against another repo's `audit-markers.sh`, which counts `grep -c 'RunMarker'` and
subtracts match arms. Same bias exactly: planting one ordinary comment mentioning
the type moves its count from 10 to 11 against an enumerated 8, so the tool
refuses. Proved by planting it and restoring byte-identical. But their count feeds
a CROSS-CHECK THAT REFUSES ON MISMATCH, not an assertion — so the false positive
makes the tool decline to run and say why, where ours made it state a fact.

**MY FIRST NUMBERS FOR THAT WERE WRONG, AND WRONGLY IN A WAY THE TOOL HAD ALREADY
FIXED.** I published 2 and 3. The real figures are 10 and 11, and the gap is the
boundary: my ad-hoc probe grepped `#\[cfg(test)\]` unanchored and stopped at an
INDENTED one inside an impl block at line 754, while the script anchors
`^#\[cfg(test)\]` and finds the real module boundary at 2619. So I measured a
fragment. That exact unanchored-boundary bug had bitten their script months earlier
— it audited 2 of 7 sites before the anchor went in — and I reproduced it in my
probe OF the fixed tool.

The general form is the two-reader rule pointed at PROBING: **a probe that
re-derives the tool's own boundary is a second reader of that boundary, and it
drifts exactly where the tool has already been repaired.** The repair lives in the
tool, not in the idea, so a fresh re-derivation starts from the unrepaired
version. When probing an instrument, take its predicate from the instrument.

So the same defective measurement is a nuisance in one position and a lie in the
other:

```
count -> assert a property        a false fact, which travels
count -> cross-check, refuse      a false refusal, which announces itself
```

When a mention-count cannot be avoided, put it where a wrong answer STOPS the tool
rather than where it becomes the tool's output. That is a placement decision, not
a regex decision, and it survives the regex being wrong — which it eventually will
be.

Two second-order notes, both costly:

- A roster derived this way was handed to another seat, who relayed its rows into
  per-seat instructions. A wrong measurement travels further than a wrong
  conclusion, because it arrives as a fact rather than as an argument.
- When an instrument is disproved, EVERY row it produced is suspect, not the rows
  someone happened to check. Two were corrected by their owners; the other four
  were re-derived here, and could equally have been wrong.


## A measurement that never reached its subject still returns a number

Three wrong conclusions in one day, all from instruments that ran cleanly, returned
well-formed values, and never touched the thing being measured. None announced
itself; each was caught by a different accident.

```
strings | grep -oE '[0-9a-f]{16}'   returned a real 16-hex token from
                                     elsewhere in the binary, not the digest
median over three repetitions        reported the CACHED cost, refuting a
                                     hypothesis that was true
timing a copied system binary        ~1.5ms readings were SIGKILL refusals;
                                     the process never executed
```

The third one stood for two days as a recorded, actively-investigated *anomaly* —
"some binaries are cheap to validate and nobody has found why" — with size,
signature shape and architecture each tested and ruled out. There was nothing to
explain. The cheap set was not cheap; it was refused.

**The concrete check, which costs nothing: when a timing comes back unexpectedly
cheap, read the exit status before believing it.** `subprocess.run(...)` returns a
completed object whether the program ran or was killed, and `-9` versus `0` is the
whole difference between "fast" and "never happened". A shell reports the same
thing as exit 137.

The general form is worth more than the check, because it covers instruments with
no exit status at all:

> A measurement that never reached its subject still returns a number, and the
> number is usually plausible. Plausibility is what makes it survive review.

So the question to ask of any instrument is not "is this value reasonable" but
"what path did it take, and could that path have skipped the subject entirely".
For the three above the answers were: matched a different token, hit a cache,
and was refused at exec — none of which a reasonable-looking result would ever
reveal.

Related, and the reason this section sits beside the vacuity ones: a test that
cannot fail and a measurement that never happened are the same defect at different
scales. Both produce evidence of a thing not examined, and both read as
confirmation.


## A green suite can be green about the wrong thing

A consumer team replaced a hardcoded provider-name mapping with a read of the
`apiProvider` field we publish. All 239 of their existing tests passed the moment
the change landed — and none of them exercised the new code:

> Every fixture I had carried a bare `provider` and no slug, so they exercised
> the fallback and said nothing about the path I had just written. The green was
> real and about the wrong thing.

This is NOT the fixture-generalisation trap recorded above, where a test is true
of its fixture and false in general. Here the tests were correct and the code was
correct; the fixtures simply all took the OTHER BRANCH. Nothing was wrong, and
nothing was verified.

WHY IT IS HARD TO SEE FROM INSIDE: a full-suite pass is the signal everyone reads
as "this change is safe", and it is the same signal whether the suite covered the
change or never reached it. Coverage of the DIFF is a different question from
coverage of the codebase, and only the first one is being asked after an edit.

CHECK: after changing a branch, confirm at least one fixture takes it. The cheap
version is to break the new code deliberately and watch something redden — if the
suite stays green, the fixtures are all on the other side of the branch. That is
the same act as a mutation proof, applied to a code path rather than to a rule.

WE HAVE THE MIRROR AND IT HOLDS, checked rather than assumed: our
`StubProvider` fixtures set no slug, so the emission tests could have exercised
only the None arm. `a_mapped_provider_publishes_its_canonical_slug` asserts both
directions on real wire entries — `codex` publishes `openai`, `mock` publishes
nothing — so the populated arm is defended. A clean negative, but only because it
was measured; the fixture shape alone predicted the gap.

## A wrong fact with a citation outlives one without

The mapping that consumer deleted carried a comment asserting it was "the
complete, stable mapping" and attributing that to us. It had been wrong for
weeks, and their account of why it survived is the useful part:

> The comment made it look settled, which is why it survived every reading of
> that function.

An unsourced constant invites a reader to check it. A sourced one answers the
question before it is asked, so the check never happens — and the citation keeps
vouching long after the fact it cites has moved. The four names were complete
when written; ten differ now.

PRACTICE, in the sharpened form the same consumer sent back after sweeping their
own source with it: A COMMENT CITING ANOTHER SYSTEM NEEDS THE DERIVATION ONLY
WHEN THE CLAIM IS THE LOAD-BEARING PART. Where the reasoning travels with the
citation, the attribution is provenance rather than evidence and re-derivation
buys nothing -- a reader can agree or disagree by reading it, and if the argument
is wrong it is wrong on its face. Where the comment is a BARE CLAIM whose weight
rests on someone having checked, record the query, the file, or the command.

Their two examples separate the cases cleanly:

    carries its own argument   "a capability matches a SHAPE, not an identity, so
                                the day a second module declares the same
                                operation, resolution picks one silently"
    bare claim                 "ordinals are the intended cursor per the owner"

The first is checkable by reading it. The second cost them weeks.

SWEPT OUR OWN SOURCE WITH IT AND FOUND ONE TRUE HIT, on the most safety-critical
comment in the tree. `antigravity.rs` justified reading another tool's refresh
token with "Google does not rotate ... verified against the live endpoint rather
than assumed" -- load-bearing, because it guards a user's sign-in (a rotating
credential spent by a reader that does not own it breaks the plugin's actual
login), and unrepeatable as written. It now records the exact exchange and the
response field to look for. The other hit was already correct: `http.rs` cites
`reqwest 0.12.28 async_impl/client.rs`, which a reader can open.

So: record HOW TO RE-DERIVE IT rather than only the fact — the query,
the file, the command. A citation that cannot be re-run is a claim wearing the
costume of evidence. Our own version of this is the parity anchor in
`docs/provider-matrix.md`, which names the upstream tag it was checked against
precisely so the next reader can tell whether the check is current or stale.
