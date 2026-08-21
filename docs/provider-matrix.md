# Provider matrix — the 46 CodexBar providers, by auth archetype

> **This document maps the UPSTREAM CodexBar inventory, not this module's
> registry.** It was written to plan the porting effort and its per-provider
> statuses — BUILD, DEFER, v1 — record what was decided at the time each row was
> written. Many have since been built, so **a status here is not evidence of what
> is registered today**.
>
> The registry is the only current inventory:
> `Registry::with_defaults` in `crates/quota-core/src/lib.rs`, and the provider
> list in the README is generated from it. Six registered providers have no row
> here at all, because they were ported after this document was last revised.
>
> What stays useful is the per-provider research: endpoints, auth archetypes,
> response shapes, and the reasoning behind each deferral. Read it as a study of
> upstream, and check the registry for what we actually serve.

Reverse-engineered from CodexBar source (`/Users/ufukaltinok/Work/OSS/CodexBar/Sources/CodexBarCore/Providers/*`),
every row cited to `file:line` in the study transcripts. This is the map that
makes parallel fan-out safe: the **auth archetype** is the unit of effort (shared
auth+fetch scaffolding within a group), so one worker owns one archetype group.

At the time of writing, two providers were built and proven live end-to-end:
**codex** (oauth-local-file) and **claude** (oauth-bearer via opencode store).

---

## Parity status

**Current parity: CodexBar v0.53.0** (37 providers registered; verified
2026-08-20, a null round). The v0.49.3 round is a NULL: the entire provider delta from v0.49.2
is one line in `AzureOpenAIUsageFetcher`, raising a validation probe's
`max_completion_tokens` from 1 to 64 and naming the constant. AzureOpenAI is
excluded here as a validation probe with no usage payload, so nothing to port. CodexBar is a moving upstream; parity is re-checked whenever it
publishes a newer GitHub release. Read that release's content with `git show
<tag>:<path>` or `git grep <tag>` — the checkout usually sits at an older tag, so
plain `grep` silently reads a different version and reports a symbol added in the
new release as absent. On a new release, do BOTH of these — the second is the one
that gets skipped, because the first produces a satisfying result and the diff is
where the eye goes:

1. Re-check every row of **Opaque constants** below against the new tag. A
   provider whose logic did not change can still have a rotated constant, so a
   diff-driven read never reaches it. This step was missed for five consecutive
   releases while the anchor line kept being updated, which is exactly what it
   looks like when a check lives after the thing that feels like the whole job.
2. Diff `git -C ~/Work/OSS/CodexBar
diff v0.49.3..<new-tag> -- Sources/CodexBarCore/Providers/` and triage into: window
drift on providers we already serve (highest risk — no live creds to catch a
silent degradation), new window-bearing providers to port, and credit-pool
sources (now portable: the Balance axis ships, so a prepaid balance is publishable
rather than deferred — see docs/balance-axis-design.md).

Read the diff for CREDENTIAL AND GATING changes as well as mapping drift. The one
real port of the v0.48.0 round came from upstream adding a gate, which made
visible that WE were sending the wrong credential to a second endpoint — a
best-effort enrichment that has never once succeeded looks exactly like one that
succeeds and finds nothing, since both publish no extra window and neither
degrades the entry. Any provider with a second optional endpoint has that shape. When a diff touches a provider we serve,
confirm it changes the endpoint WE parse (e.g. Claude's live anchor reads
`/api/oauth/usage`, not CodexBar's CLI/web fetcher) and that the field actually
changed within the range (`git show <old-tag>:<file>`), not a pre-existing value.


### v0.49.4 - v0.49.6

Upstream added a **pay-as-you-go fallback for OpenCode**: a workspace on that
plan has no subscription object, so the subscription server function answers
null or fails outright, and spend lives in a separate billing payload reachable
with the same cookie.

That is a second reading of a symptom this repo has carried for weeks. Our
opencode entry publishes `HTTP 500` from the subscription call, and the standing
diagnosis was an upstream outage -- reached partly because the published error
named no stage, so it was equally compatible with every explanation and the
cheapest one won.

**Probed on this host rather than argued** (`crates/quota-core/examples/opencode-stage.rs`):
workspaces answers, subscription returns 500, billing answers. The failure is
specific to one server function -- not a session, a cookie, or a site-wide
outage, all of which the old error was consistent with.

Narrowed once more after deploying: the stage name did NOT appear on the wire,
because each server function retries as a POST when its GET parses to nothing,
and those retries bypassed the helper carrying the stage. So the live 500 comes
from the subscription RETRY while its GET answers -- a materially different fact
from "the subscription call fails", and one no amount of reading found. Every
send is now staged, fenced by a test comparing send sites to staged sites so a
new one cannot publish an anonymous error.

**The root cause was ours, found by continuing past the stage names.** The retry
only fires when the GET body does not parse, so the GET answers -- and it answers
`null`, in the value slot of the server-function envelope:

```text
;0x41;((self.$R=self.$R||{})["server-fn:18cb..."]=[],null)
```

`is_explicit_null` recognised a bare `null` and a JSON null, and this is neither:
it is a JavaScript statement. So the caller retried as a POST, the retry answered
500, and a workspace with no subscription was published as an upstream failure
for weeks. It is now recognised, and reported as `no_quota_reported` rather than
`decode_failed` -- the class matters, because Decode counts toward the stale
browser login metric and would send an operator to re-authenticate a session that
works.

This is upstream's pay-as-you-go case after all, reached from the other side:
their fallback exists because that call "answers null or fails outright", and
ours was answering null the whole time where we could not see it.

**The spend fallback is still NOT taken, on evidence.** The live billing payload here reads
`monthlyUsage:null, monthlyLimit:null, balance:0, subscription:null`, and
upstream's parser requires `monthlyUsage` before it builds anything. On this
account their fallback returns nothing and rethrows the original error, so
porting it would add a call, a constant to rotate, and change nothing
observable. The constant and the billing call are in place for whenever an
account on that plan appears; the fallback wiring is not, because nothing here
can exercise it and an untested fallback is the shape that quietly stops
working.

**What was taken** is the diagnostic half: every opencode server-function error
now names its stage, so the next reader can tell which call produced a failure
without building a probe.

Two things worth remembering from this round:

- The probe's first verdict was wrong in my favour. It keyed "answered" on the
  call returning 200, and billing returns 200 with nothing in it -- so it
  printed a conclusion supporting the hypothesis under test. A second defect
  stacked on it: the payload is a JS object literal, not JSON, so a quoted-key
  search reported every field absent, which looks identical to an empty
  response. Both errors pointed the same way.
- A parity round can pay even when nothing is portable. The finding was not
  upstream's code; it was that our own standing diagnosis of a live provider was
  unsupported.


### v0.50.0

**The tripwire did not fire for this one.** A watch-gap notice arrived saying
stored data was behind the vendor, and checking the tag list directly found
v0.50.0 already shipped. The lesson is not about that watch: a release-tracking
mechanism fails silently by definition, because a release that never arrives
looks exactly like no release. Treat a gap notice from ANY watch as a reason to
check the thing it watches, not as information about the watch.

Constants re-checked first per the procedure: all four present at v0.50.0.

**`cursor` gains a headless credential lane, and this one is worth having.** The
provider is dark on any host with no browser session — the jar holds only
anonymous analytics ids. Upstream added `CursorAppAuth`, which reads the Cursor
editor's own SQLite store rather than a browser:

```text
state.vscdb  ->  cursorAuth/accessToken (JWT)
                 sub claim, last `|` segment  ->  user id
WorkosCursorSessionToken = <user id>%3A%3A<access token>
```

Verified end to end on this host before any code was written: the synthesized
cookie returns HTTP 200 with a real usage summary from the endpoint this module
already calls. `cursorAuth/cachedEmail` sits beside it, so the entry can carry an
account identity it has never had.

Shipped and verified on the wire: `cursor` moved from `credential_absent` to
serving, labelled with a real account email it has never carried before, and the
host's unconfigured count fell from 27 to 26.

Cleared the rotation gate first, which is what killed the Claude and Codex plugin
lanes: this READS an access token and uses it directly. No refresh exchange, so
nothing rotates and no editor sign-in is disturbed. The JWT carries an exp in
2107, so no refresh is needed at all. `cursorAuth/refreshToken` sits in the same
table and must never be touched.

**`gemini`: Google shut down Gemini CLI OAuth for individual, AI Pro and Ultra
accounts in June 2026.** Upstream now ships user-facing copy directing those
accounts to Antigravity instead; workspace and education accounts keep working.

Nothing to port — but recording it here because of what it will look like when it
reaches an account. The lane will start failing with an auth rejection, and this
module will publish `credential_rejected`, whose whole meaning is *the credential
was refused, re-authenticate*. Re-authenticating cannot work, because the
mechanism is gone rather than the session. A future reader seeing gemini fail on
a consumer account should check this line before diagnosing a credential problem,
and the remedy is the Antigravity provider, which already serves on this host.

This host's gemini account is unaffected today: it returns four per-model windows
on a live fetch, so its 0% is real unused quota rather than a hollow reading.

**Declined: `CodexCLIBackendConfiguration`.** It classifies rate-limit errors by
which backend the Codex CLI's `config.toml` selects. This module never reads that
file — it reads `auth.json` and queries OpenAI's usage endpoint directly — so
there is nothing here to classify.

### v0.50.1 — a null round, and the null that mattered

Constants first, per the procedure: all four present at v0.50.1.

25 provider files changed and **nothing is portable.** Most of the delta is
presentation — accent colours, branding, descriptors. The rest:

- `ollama` fetcher: parses a pasted `curl`/`Cookie:` capture as an override. A
  CodexBar UI affordance; this module's cookie comes from the browser store and
  never from a pasted string.
- `CodexTokenRefresher`: threads two existing fields through a struct. No parse
  or wire change.
- Codex error copy: "Run `codex`" → "Run `codex login`". Their string, not ours.

**The one worth the round was a question rather than a port.** Upstream added a
`keychainAccessRevoked` credential error whose message names a live mechanism:
*"Claude Keychain access was revoked by Claude Code's token rotation."*

That is a rotation event on the credential family this repo already treats as
dangerous — anthropic and openai refresh tokens are single-use, which is why the
plugin-store lanes were declined. So the question is whether **this module** could
be a party to revoking a user's Claude Code session.

It cannot, verified at source rather than recalled: `anthropic.rs` never
exchanges a refresh token. The local lane reads an access token from the opencode
store and sends it; the vault lane receives a token from `claustrum`, which owns
refresh. Neither path performs the exchange that rotates.

Recording the null because the question is the kind that recurs, and next time
the answer should be checkable in one read rather than re-derived. If an
anthropic refresh is ever added here, this is the paragraph that says why it
would be a defect rather than a feature.

### openrouter, added 2026-08-16 outside a parity round

Not from CodexBar. Found by listing the opencode auth store against the
registry: **five credentials on this host are read by no provider at all**
(openrouter, cerebras, fireworks-ai, inception, amazon-bedrock). The
unread-credentials detector crosses REGISTERED providers against the store, so
it structurally cannot see a credential with no provider behind it — a blind
spot nothing here covered.

`openrouter` publishes `GET /api/v1/credits` -> `{total_credits, total_usage}`,
proved live before any code was written. One derived USD pool, `funding:
unknown` because the endpoint never says whether credits were bought or comped.

**`GET /api/v1/auth/key` is deliberately unused.** It carries `usage`,
`usage_daily/weekly/monthly` and `limit`, and on this account both `limit` and
`limit_remaining` are null — spend-to-date with no cap. That fits no shape on
this wire: a rate window needs a utilisation, a pool needs a balance, and
publishing consumption with no denominator invites a consumer to invent one. If
an account ever sets a spend limit, that is a separate decision with its own
evidence.

The other four were probed on 2026-08-16 and **none is portable**. Recorded with
what was tried, so the next reader inherits the evidence rather than the
curiosity — all four are plain API keys, so the rotation gate is clear for every
one of them and only the quota surface decides.

| credential | probe | disposition |
|---|---|---|
| `cerebras` | `/v1/models` -> 403 code 1010 | Cloudflare bot block, not an auth answer. The key is neither proved nor disproved, and a provider built on a surface that refuses a plain request would be guessing at headers. |
| `fireworks-ai` | `/v1/accounts` -> 412 | The account IS suspended for monthly spend — a genuine quota fact. But that string is the ONLY surface: `/v1/account`, `/v1/models` and `/verel/v1/accounts` all 404. Parsing prose from an error body for the word "suspended" is the fragile inference this repo refuses elsewhere; error text carries no stability promise, ours or theirs. |
| `inception` | `/v1/models` -> 200; `/v1/usage`, `/v1/credits`, `/v1/account`, `/v1/billing` -> 404 | The key works and there is nothing to read. A live credential with no quota surface is not a gap. |
| `amazon-bedrock` | not probed | AWS SigV4, not a bearer token, so the shape does not match any adaptor here. Usage would come from Cost Explorer or CloudWatch, which is an AWS integration rather than a provider port. |

The `fireworks-ai` one is worth remembering if that vendor ever ships a billing
endpoint: the account state is real and currently invisible, and only the
*surface* is missing.

### v0.52.0 -> v0.53.0 (checked 2026-08-20)

NULL. Constants all present at the tag. Fifteen provider files changed, eight of
them Grok, and none of it reaches what we parse.

**Grok** is the one that looked substantive and is not. The delta adds a
credential-routing type that picks between an OAuth token and a manually pasted
cookie header, which is a settings-UI concern with no counterpart here: our two
lanes are the local opencode auth store and the vault, and neither is
user-pasted. The only change inside the credits path we DO parse is a line
wrapped across three lines -- `resetsAt` still resolves `currentPeriod.end` then
falls back to `billingPeriodEnd`, unchanged. Read the functions whole in both
tags before concluding; the fragment reads like a reset-resolution change.

**OpenCodeGo** is the interesting null. Upstream now refuses to fall back to a
local estimate when a manually configured credential fails authentication --
"do not hide its authentication failure behind an unrelated local estimate".
That is the invariant `classify_go_page` already enforces, for the reason its own
doc comment gives: a signed-out page can carry an unsubscribed-looking record, so
the signed-out check must come first. Same conclusion, reached independently, and
their local-estimate lane still has no counterpart here.

**OpenAI** adds `costProvenance: .vendorMetered` to its API-key usage snapshot,
which is a spend concern on a lane we do not serve -- codex reads `wham/usage`.

### v0.50.1 -> v0.52.0 (checked 2026-08-17)

Nine provider files changed; two touch providers we serve.

**Constants: all five present, nothing rotated.** This round is also why the
table above lists five rather than four -- `BILLING_SERVER_ID` had never been in
it, so no previous round re-checked the function `opencode` falls back to.

**OpenCodeGo -- no port, structurally.** Upstream added a `quotaIsAuthoritative`
flag and marks non-authoritative readings `.estimated`. It distinguishes their
LOCAL-FILE lane, whose monthly window is "anchored at the earliest local row",
from the web overlay that knows the real billing anchors. We have no local-file
lane -- our `opencodego` is cookie-and-web only, zero local-data references -- so
everything we publish is already the path they call authoritative. Nothing to
port, and the reason is structural rather than a judgement.

**Grok -- no port.** Six files, and none of it moves a window. The credits-proxy
diff looks like a reset-handling change (`guard resetsAt != nil else { throw }`
became `if resetsAt != nil { return }` then `throw`) but the percent branches
precede it in BOTH versions, so the two forms are behaviourally identical; a
percent-bearing payload with no reset returned its percent before and returns it
now. The real addition is `subscriptionTier`, a display label, sourced from a new
endpoint (`cli-chat-proxy.grok.com/v1/settings`, field
`subscription_tier_display`).

Declined deliberately: our grok lane resolves no account identity, so the label
would decorate an unlabelled row, and it costs an extra HTTP call per tick on a
provider whose windows already serve. Reconsider if grok ever gains identity --
the endpoint is recorded here so that is a lookup rather than a rediscovery.

**Kiro and Zed:** not implemented here.

### qwen-cloud request fidelity, verified against a capture

Checked 2026-08-21 against a capture of the working console, by enumerating
every gateway call the browser made rather than sampling the ones we already
knew about. Four distinct APIs, and the capture is now fully accounted for:

| call | ours | verdict |
|---|---|---|
| `usage` | `GATEWAY_PARAMS` | `cornerstoneParam` identical |
| `quota-config` | `QUOTA_CONFIG_PARAMS` | `cornerstoneParam` identical |
| `subscription` | `SUBSCRIPTION_PARAMS` | identical, plus they send `commodityCode` — see below |
| `reset-card/list` | not implemented | deferred, shape unobserved |

**The one divergence is deliberate.** The console filters its subscription call
by `commodityCode` and we do not, because filtering is the rejecting direction:
an account on a different token-plan product would get no record at all where
today it gets its own. The cost of staying unfiltered — another product's record
reaching a cap lookup keyed by a bare tier name — is closed at the enrichment
instead, which refuses counts it cannot attribute while leaving the percentage
alone.

**Method note, because the first run of this was wrong.** The comparison script
named the constants it expected and reported `(not found)` for the usage call —
whose constant is `GATEWAY_PARAMS`, not the `USAGE_PARAMS` it guessed. A
not-found row must never read as a verified one. Deriving the constants from the
source instead found all three, and is what makes a re-run trustworthy after a
rename: **a comparison keyed on names you supply can only check the ones you
thought of.**

### qwen-cloud reset cards, deferred on an unobserved shape

The 2026-08-20 capture revealed a fourth gateway call this console makes:
`zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/reset-card/list`, alongside the
`usage`, `quota-config` and `subscription` calls we already make. The name says
it holds something like the Codex banked resets — credits that reset a window on
demand — and if so it belongs on the wire the same way `savedResets` does.

**Not built, because the response was empty.** The captured call returned
`success: true` with `data: []`: this account holds zero cards. So the endpoint
is confirmed to exist and confirmed to answer, and the SHAPE OF A CARD is
unobserved — no field names, no expiry, no count. Building a parser against that
means inventing the field names and pinning them in a fixture that looks exactly
like a real one, which is the provenance failure this repo has already paid for
once (a transcribed JetBrains fixture asserted fields no payload had shown).

What is known and worth keeping: an empty list here is a STATED absence
(`success: true`), not a failure, so whoever builds this maps it to zero cards
rather than to an error — the same distinction `savedResets` draws between an
inventory of none and an inventory that could not be fetched.

Unblocks when an account with at least one card is captured. The call shape is
already known: same host, same `sec_token` form field, `Data` carrying only the
standard `cornerstoneParam` block.

### Opaque constants, re-checked every round

Five values are copied from the upstream rather than derived from anything we
can compute. They carry no meaning we can validate, so a stale one is invisible
here and surfaces only as a request the upstream rejects — which reads exactly
like an outage, and the confusion is expensive: the provider looks broken
upstream while the defect is a constant in this repo.

They are also the part of a port most likely to be missed. A parity diff draws
the eye to logic, and a rotated constant is a one-line change indistinguishable
from formatting. Check every row against the new tag, not just the providers
whose logic changed:

| Constant | Where | What it is |
|---|---|---|
| `WORKSPACES_SERVER_ID` | `crates/quota-core/src/opencode.rs` | Hash naming the upstream server function that lists workspaces. Rotates when they rebundle. |
| `SUBSCRIPTION_SERVER_ID` | `crates/quota-core/src/opencode.rs` | Same, for the subscription call. |
| `BILLING_SERVER_ID` | `crates/quota-core/src/opencode.rs` | Same, for the customer/billing call — the function a pay-as-you-go workspace is read from when it has no subscription object. Missing from this table until 2026-08-17. |
| `BETA_HEADER` | `crates/quota-core/src/anthropic.rs` | Dated opt-in header (`oauth-2025-04-20`). Dated values get superseded. |
| `OASIS_WEB_ID` | `crates/quota-core/src/stepfun.rs` | Fallback device identifier, used only when the token carries no `device_id` claim. |

All five matched CodexBar v0.53.0, re-verified 2026-08-20 by locating each
value in the tagged tree (`git grep -l <value> v0.53.0`) rather than in a
checkout, since a working tree can be on any commit.

File paths here carry no line numbers on purpose. The two that had them were
both stale by the time anyone read them -- a line number drifts on every edit
above the constant, and a citation that is wrong in a detail teaches a reader to
stop trusting the row.

The membership of this table is fenced by
`every_opaque_upstream_constant_is_in_the_parity_table`: each constant carries an
`OPAQUE-UPSTREAM-CONSTANT` marker at its definition, and the test compares that
set against these rows in both directions. It exists because this table said
"four values" while the source had five -- `BILLING_SERVER_ID` was absent, so no
round re-checked the one function opencode falls back to. Declaring the
population where a new constant is written beats a table an author must remember
to update from another directory.

The re-check was overdue by five releases: this line read v0.47.0 while v0.48.0,
v0.48.1, v0.49.0, v0.49.2 and v0.49.3 had all shipped. The values had not
drifted, so nothing was broken — but the SENTENCE had, and it is the only thing
here that says when a reader last looked. A currency claim goes stale silently
while the thing it describes stays correct, which is why this note carries the
tag it was checked against and not a bare "verified".

A mismatch is not automatically a port: confirm the value changed **within** the
diff range rather than always having differed, and that we call the same endpoint
it belongs to. But an unchecked mismatch is the failure mode this table exists
for.

v0.45.2 → v0.46.0 carried one behaviour fix for a provider we serve: **zai** now
clamps the percentage it reports directly, not just the one it computes. Our port
had the same split — computed branch clamped, fallback branch verbatim — and it is
fixed. The rest is deliberately not ported. **ZoomMate** is a new prepaid-credits
provider with no rate window (Balance axis). **QwenCloud** was restructured onto a
new shared OneConsole layer with a cookie importer and SEC-token resolver; our
`qwen_cloud.rs` reaches live 5h and weekly windows with absolute counts by a
different route, so this is an upstream refactor rather than a behaviour gap —
revisit if ours starts failing. `UsagePercent.swift` is a new shared clamp helper
whose equivalent here is per-provider. `switcherWeeklyWindow` and the OpenCodeGo
`pace:` attribute are CodexBar display surfaces with no counterpart on this wire.

v0.41.0 → v0.42.0 delta was all additive/non-window on served providers: Codex
credits (balance axis), Gemini `paidTierName` label, Antigravity port-detection
mechanics, Claude CLI/web-path refactor (not our OAuth endpoint). New providers
were balance-only: **KimiK2** (prepaid credits, distinct from our coding-plan
`kimi`) and **Wayfinder** (dollar savings meter) — both deferred to the Balance
axis.

Providers added at v0.41.0: **sakana** (env `SAKANA_COOKIE` + billing HTML
scrape) and **qoder** (browser-cookie, base+shared quota merge). Window updates:
doubao (Volcengine-signed Coding Plan session/weekly/monthly), kimi (monthly +
Code-7d subscription windows), zai (optional `msg` for CN + team scope), minimax
(Token Plan percent lanes). Deferred to the Balance axis: **CrossModel**,
**Wayfinder**, **ClawRouter**, **KimiK2** (dollar-budget/prepaid credits, no rate
window). Qoder's CN
endpoint (`qoder.com.cn`) is deferred — unverifiable without a CN session.

At v0.47.0 two window mappings were ported. **zai**: a `TIME_LIMIT` entry states
its own window in the same `unit`/`number` pair token limits use, which we were
discarding — with one exception, `unit: minutes, number: 1`, which is a marker
rather than a duration (payloads carrying it pair it with a reset weeks away).
The upstream substitutes a thirty-day constant where nothing is stated; we emit
nothing, because a fabricated cadence is worse than an absent one. **stepfun**:
plan classification now reads the payload's shape before its `plan_family`
label — a live rolling window means windowed, a credit pool with no window means
credit-metered, and the label breaks ties only when neither is present. The
label-first order discarded live windows on an account labelled credit-metered,
and published nothing at all for a credit pool on an account labelled windowed.

Declined at v0.47.0, with reasons: **xAI** and **Notion** are new providers whose
windows are dollar balances and workspace credit pools (Balance axis); Notion is
also browser-cookie-only. Doubao gained a second endpoint (`GetAFPUsage`) whose
windows are additive to the `GetCodingPlanUsage` ones we already serve — real,
but unverifiable from this host, which has no Doubao credential. The Claude
changes are OAuth refresh coordination and probe plumbing, with no window-mapping
change; Alibaba's are credential scoping; the ~100 remaining descriptor edits are
a mechanical refactor of two to four lines each.

At v0.48.0 one port landed, and it exposed a defect of ours rather than upstream
drift. **kimi-for-coding**: the upstream began gating its subscription-stats call
on a separate web session token, which made visible that the two surfaces take
different credentials — the usage endpoint takes the coding API key, the web
console takes a browser session. We had been sending the API key to the console,
so every fetch made a request the console rejected, and the failure was swallowed
as best-effort. The monthly and code-7d extras had therefore never reached the
wire. Probing the console with a browser cookie returned exactly the shape the
parser already expected, so nothing about the parsing was wrong; only the
credential was. The enrichment is now skipped when no browser session exists
rather than attempted with the wrong one.

The general lesson is worth more than the fix: **a best-effort call that has
never once succeeded is indistinguishable from one that succeeds and finds
nothing.** Both publish no extra window and neither degrades the entry. The
upstream diff is what surfaced it, which argues for reading parity rounds for
credential and gating changes, not only for window-mapping drift.

At v0.49.0 through v0.49.2 nothing was portable, and what was SEARCHED matters
more than the verdict. 129 provider-core files changed across the three
releases; the sweep read them for credential and gating changes as well as
window-mapping drift, since the previous round's one real port came from a
gating change rather than a mapping one.

Three findings, none of them ports:

- **Codex gained a third credit-limit source.** `spend_control.individual_limit`,
  for team and enterprise workspaces that report a monthly credit pool there
  instead of at the response root, behind an established precedence of root then
  `rate_limit` then `spend_control`. Verified against our own live `wham/usage`
  payload: `spend_control` is present and its `individual_limit` is null on an
  individual Pro account, which is the expected shape for a non-team plan. Not
  ported because we consume no codex credit limit at all today — the field would
  land on the Balance axis, and porting a precedence chain we cannot exercise on
  any account here would ship an untested selection rule.

  Recorded as the concrete next step for codex on that axis: the same payload
  also carries `credits` (`has_credits`, `balance`, `unlimited`,
  `overage_limit_reached`), which is a pool this module already fetches and
  discards.

- **A binding-quota projection**, capping a session row's displayed percentage
  and reset by any exhausted longer lane. It is a display-layer transform in
  their menu card, and the equivalent decision on our side is already made in the
  opposite direction and documented: this producer publishes every window
  untransformed and states that no field identifies the binding limit, because
  the ranking policy belongs to whoever acts on it. Nothing to port; the
  divergence is deliberate and already written down.

- **ClinePass moved to a TypeScript plugin** (-223 lines), which is a delivery
  change on their side with no effect on the endpoint or the mapping.

The Claude fetcher change in range is an error-construction refactor, and the
remaining volume is settings storage, plugin engine, and menu presentation.

At v0.48.1 nothing was portable, and the round is recorded because a null is
only worth anything when the search behind it is stated. The release is a CLI
dashboard and web-UI feature — serve command, HTML, snapshot payloads, and their
tests. Two files under `CodexBarCore` are touched, both replacing `Bundle.module`
with a resource lookup that also resolves inside an `.app`, so plugin JavaScript
loads when the CLI runs from the app bundle. No provider fetcher, no credential
handling, no window mapping, and no gating changed: reading the whole delta for
credential and gating drift, rather than window mapping alone, returned those two
resource lines and nothing else.

Declined at v0.48.0: the bulk of a 758-file release is a settings-storage
refactor (`settings[providerConfig:field:]`) touching ~30 provider files by two
to four lines each, plus a logging-category rename, neither with any wire
counterpart. **Copilot** gained a `creditsUsed` counter that keeps an otherwise
placeholder snapshot accessible — a credits axis, and upstream is explicit that
it must never become a percentage window, which is exactly what our placeholder
guard already refuses. **MiniMax** and several others gained detail sections for
the upstream's own UI. **Sub2API** moved to a bundled plugin on JavaScriptCore
platforms, leaving the Swift fetcher as a Linux fallback. **OpenCodeGo** gained a
zen-balance join timeout (Balance axis) and no window change. **Claude** is
refresh-coordination detail; **Doubao** is a struct-field cleanup.

---

## The load-bearing finding: only ~24 of 46 actually have a RATE WINDOW

Alfonso consumes **RateWindows** — `{ utilization, resetsAt, windowMinutes }` —
and its pace/pressure projection is built on them. But a large fraction of
CodexBar's providers expose **no window at all** — they report a credit balance, a
cumulative cost, or a remaining count with no reset time or window length. Those
do not fit the RateWindow model and Alfonso's pace projection cannot use them.

- **HAS WINDOW (~24)** — session/weekly/monthly utilization + reset. These are the
  providers worth porting for Alfonso's actual need.
- **NO WINDOW (~17)** — cost/credits/balance/count only. Porting them produces an
  entry with no usable window (silent-degrade territory, or a different "credits"
  signal Alfonso doesn't model today).
- **PARTIAL (~5)** — a window on one sub-signal (e.g. codebuff's weekly
  subscription limit) but credits on the primary.

> **RULINGS (Ufuk + ALF, locked):** v1 = the window-bearing set only; the
> "all 46" lock is revised. The former NO-WINDOW set is re-categorized in Group 6
> below into IMPLICIT-RESET (faithful reset → promotable into v1, e.g. **copilot**),
> TRULY-RESETLESS (prepaid balance → the reserved `Balance` seam, deferred), and
> REPORT (ambiguous → held for confirmation). A balance signal is NEVER expressed
> as a fabricated window. crof + Group 4 (cloud-cost) stay deferred.

**Q-matrix-1 — RESOLVED:** v1 is window-bearing only; NO-WINDOW deferred as an
additive fan-out. (Kept below for history.) The original question was: port all 46
(NO-WINDOW emit a degraded/credits entry), or scope the fan-out to
the ~24 window-bearing providers that Alfonso's pace model can actually use? My
lean: port the ~24 window-bearing first (real value), defer NO-WINDOW until we
decide whether Alfonso should model a credits/balance signal at all. This is a
behavior/scope decision, so I'm surfacing it before dispatching workers.

---

## Ecosystem shortcut: opencode's unified auth store collapses several archetypes

We run inside CortexKit/opencode. `~/.local/share/opencode/auth.json` already holds
OAuth tokens + API keys for **10 providers** in one cross-platform file:
`anthropic, openai, openrouter, deepseek, google, ollama-cloud, xai, cerebras,
fireworks-ai, inception`. Where opencode carries a provider, we can use a simple
bearer token (the `opencode_auth.rs` reader, already built for claude) INSTEAD of
CodexBar's macOS-only Keychain / browser-cookie / CLI scraping. This is a strictly
simpler, cross-platform path for any provider opencode holds — and it's how claude
is already proven. Per-provider we still hit each provider's OWN usage endpoint;
only the credential SOURCE is unified.

---

## Archetype groups (the fan-out partition)

### Group 1 — oauth-local-file / oauth-bearer  (HAS WINDOW)  ✅ pattern proven
Read an OAuth bearer from a local file (or opencode store) → one GET → decode.
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| **codex** ✅ | codex | `~/.codex/auth.json` (oauth access_token) | GET chatgpt.com/backend-api/wham/usage | 5h + weekly |
| **claude** ✅ | claude | opencode `anthropic` / Keychain | GET api.anthropic.com/api/oauth/usage | 5h + weekly + sonnet/opus |
| **antigravity** ✅ two lanes | antigravity | vault `antigravity:google`, or the RUNNING `agy` CLI / app language server via `ps` + `lsof` | POST cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota, or loopback `…/RetrieveUserQuotaSummary` | per-pool (resetTime) |
| gemini | gemini | `~/.gemini/oauth_creds.json` | POST cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota | per-model 24h |
Notes — the Google-OAuth sub-archetype (gemini/antigravity, cloudcode-pa quota):
the fetch is a clean POST (loadCodeAssist→retrieveUserQuota, per-model windows),
but the access token in oauth_creds.json expires (~1h), so a real fetch needs an
oauth2.googleapis.com/token refresh with the CLI's OAuth client_id/secret.
- **gemini = IN v1, Option 2 (refresh-only).** `~/.gemini/oauth_creds.json` is
  created by gemini-cli ITSELF (a real native, headless path) and carries the Code
  Assist scope (cloud-platform). We do NOT replicate CodexBar's macOS-only package
  archaeology to discover the client_id; gemini-cli is open-source, so we hardcode
  its public installed-app client (RFC 8252 native-app client — secret not
  confidential), cited to gemini-cli source. Refresh in-memory, cache in cache.rs,
  NEVER write back to oauth_creds.json (read-only consumer).
- **antigravity = TWO LANES, cloud and local probe.** The cloud lane uses the
  vault's `antigravity:google` credential and needs no local process. The local
  lane finds a running `agy` CLI or app language server with `ps`, discovers its
  loopback port with `lsof`, and asks that process.

  Both are offered, and the local one is **not** replaced when the vault handle
  exists — unlike the vault-served providers, where a vault handle replaces the
  implicit local lane to avoid two identity-less entries. Here both lanes
  describe the same account, so there is no ambiguity to avoid, and keeping both
  means a cloud outage or a dead credential still leaves the local answer.

  **Why the cloud lane was needed:** the local probe reports usage only while
  that process happens to be running, and nothing in this repository starts it.
  On a desktop the process usually exists because something else started it —
  CodexBar spawns and supervises its own `agy` instance, with an ownership record
  at `~/.codexbar/antigravity/agy-session.json`. So the provider's quota tracked
  an unrelated application's lifecycle, and its disappearance was
  indistinguishable from a credential nobody configured.

  **The credential is Antigravity's, not Gemini's, and the distinction is
  load-bearing.** Both are Google logins reaching the same Code Assist API, so a
  request made with either succeeds and the numbers look plausible. They answer
  for different products: an Antigravity login's quota response carries
  Antigravity's own model pool — Claude and GPT alongside Gemini — which a
  Gemini CLI login has no access to. `vault_handles.rs` routes
  `antigravity:google` to its own provider for exactly this reason.

  **The cloud response states no window length**, unlike the local server which
  labels each bucket `5h` or `weekly`. Its buckets carry only a model id, a
  fraction, a reset and a token type. The reset cannot supply the cadence either:
  the local server meters each pool on both a five-hour and a weekly window while
  the cloud returns a single reset per pool, so which meter a given reset belongs
  to is not knowable from the response. The cloud lane therefore publishes no
  `windowMinutes` rather than guessing one.

  Granularity differs between the lanes — named pool groups locally, one bucket
  per model from the cloud — so the cloud lane folds models back into their pools
  (by model-id prefix, the only pool evidence it carries) and publishes the same
  shape either way. Buckets with no reset are the always-available internal
  models and are excluded: folding a permanently-idle bucket into a metered pool
  would drag its worst-case reading toward zero.

### Group 2 — api-key-env, bearer, HAS WINDOW
Simplest HTTP archetype: API key from env (or opencode store) → GET → decode window.
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| crof | crof | `CROF_API_KEY` | GET crof.ai/usage_api/ | 24h (reset computed locally, America/Chicago) |
| elevenlabs | elevenlabs | `ELEVENLABS_API_KEY`/`XI_API_KEY` | GET /v1/user/subscription (xi-api-key) | char reset (next_character_count_reset_unix) |
| llmproxy | llmproxy | `LLM_PROXY_API_KEY`+`LLM_PROXY_BASE_URL` | GET {base}/v1/quota-stats | quota groups (resetTime) |
| synthetic | synthetic | `SYNTHETIC_API_KEY` | GET api.synthetic.new/v2/quotas | 5h + weekly + hourly |
| warp | warp | `WARP_API_KEY`/`WARP_TOKEN` | POST app.warp.dev/graphql/v2 | request limit (nextRefreshTime) |
| zai | zai | `Z_AI_API_KEY` | GET {base}/api/monitor/usage/quota/limit | token limits (nextResetTime + unit→minutes) |
| kilo | kilo | `KILO_API_KEY` or `~/.local/share/kilo/auth.json` | GET app.kilo.ai/api/trpc/... | credits + pass reset |
| alibaba | alibaba | `ALIBABA_CODING_PLAN_API_KEY` (or cookie) | POST {host}/data/api.json | 5h + weekly + monthly |
Notes: warp needs `x-warp-client-id` + UA spoof; zai is multi-region; crof
fabricates resetsAt locally; kilo straddles Group 5 (env OR auth-file). alibaba
has an api-key path (Group 2) and a console-cookie path (Group 3) — port the
api-key path; its console mode needs a `sec_token` scraped from HTML.

### Group 3 — web-session, HAS WINDOW — re-partitioned by HEADLESS SOURCEABILITY

Audited each provider's credential ORIGIN (3 explore workers vs CodexBar +
OmniRoute, + a live ollama probe) because for G3 the gating question is the
antigravity one: can a headless server even OBTAIN the credential, or does it only
exist after an interactive desktop/browser login? Three buckets:

**WINDSURF — DEFERRED (Ufuk's "no stale-window-as-live" bar).** It is
headless-SOURCEABLE (reads the editor's own native SQLite `state.vscdb` →
`windsurf.settings.cachedPlanInfo`, with real `dailyResetAtUnix`/`weeklyResetAtUnix`),
but it is fundamentally a DIFFERENT KIND of signal: an editor CACHE, not a live
fetch. The deciding problem is "consumed AS-LIVE" — any window we emit is used by
Alfonso's router identically to a live-fetched one (source is observability-only;
the router does not discount on it), yet windsurf's value can be hours-to-days stale
and we CANNOT measure how stale:
- No per-key write-timestamp exists in the cache; `state.vscdb` mtime is the
  shared-DB last-write of ANY VSCode key, reliable only as "file ancient ⇒ drop",
  never as freshness proof.
**Re-examined 2026-08-15, and the deferral's condition has changed shape.** The
`cursor` app-auth lane shipped that day reads a CREDENTIAL out of an editor's
`state.vscdb` and performs a live fetch with it. That is a different thing from
reading cached usage, and it sidesteps the staleness objection below entirely:
a token is not stale, it is either accepted or refused by the upstream, and the
usage figures come from the live call.

So the question for windsurf is no longer "can we discount a cache" but "does
its store hold a credential and does an endpoint accept it" — the shape cursor
just proved. It stays deferred because that is unmeasurable here: no Windsurf
install exists on this host, so there is no store to inspect and no way to test
an endpoint. A lane whose correctness cannot be measured from this machine must
not ship.

THE CONDITION TO RE-EXAMINE IS NOW "a host with Windsurf installed", not a
staleness field on the wire. Recording that because the old condition is the one
a future reader would check, and it would keep looking unmet while pointing at
the wrong question.

- A designed staleness guard (KEEP for a future revisit — it's correct and reusable):
  (a) drop a window whose STORED `*ResetAtUnix` is already in the past (period rolled
  over since cache write — exact, no mtime needed); (b) bound cache-age to the window
  length (daily ≤24h, weekly ≤7d) via mtime. This is the tightest bound the data
  supports — but ≤24h on a daily window still means reporting this-morning's 0% as
  current after the user burned 90%, indistinguishable-to-the-consumer from a live
  ±60s value. Every other v1 provider is refreshed on the background refresher's
  cadence (a nominal 60s interval — not a TTL, and reads never fetch inline);
  windsurf would be
  the lone silently-stale-as-live outlier = the misleading-metric case the user
  rules out, for one provider.
UNBLOCK CONDITIONS (revisit precisely when any lands): (1) windsurf exposes a real
headless USAGE FETCH endpoint (live value) → builds like any live provider; (2) the
cache grows a per-key write-timestamp → staleness becomes measurable; (3) we add a
staleness/confidence field to `ProviderUsage` that the router discounts on (ALF's-
call model+consumer change, same shape as the balance seam) → the reset-guarded
design above becomes shippable as-is.

**Condition (3) is now PARTLY met, and the gap is smaller than this section
assumes.** `ProviderUsage` gained `fetchedAt` — a producer-stamped per-entry
timestamp of that slot's last successful fetch — and consumers were told to age
entries on it. So the wire can already say *when a value was true*, which is the
half this section says is missing.

What is still missing is narrower: `fetchedAt` records when **this module** read
the source, and for a cache-backed provider that is not when the value became
true. Reading a three-hour-old cache entry stamps a fresh `fetchedAt`, so the
field would assert freshness this module cannot support — the deferral's own
objection, one layer in.

That makes the remaining ask concrete rather than open-ended: either the entry
carries a second timestamp meaning *when the underlying source last updated*, or
`fetchedAt` is documented as read-time and a separate field carries value-time.
Worth re-deriving from the published crate before acting on this paragraph — the
wire has gained fields twice since the deferral was written, and a deferral that
is not re-read against the current wire outlives its reason. Note for a future build: SET
windowMinutes=daily 1440/weekly 10080 (derivable-and-correct from the field names,
and the consumer needs the burn-rate denominator) — confirmed not a faithfulness
violation.

**3A — HEADLESS-SOURCEABLE (api-key-env / native token) → BUILD (these are really
api-key providers mis-filed as cookie):**
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| minimax | minimax | `MINIMAX_CODING_API_KEY`/`MINIMAX_API_KEY` bearer | GET {host}/v1/api/openplatform/coding_plan/remains | remains_time/end_time (epoch) |
| doubao | doubao | `ARK_API_KEY`/`VOLCENGINE_API_KEY`/`DOUBAO_API_KEY` bearer | POST ark...volces.com (probe) | `x-ratelimit-reset-requests` header (ISO/duration/sec) |
| kimi | kimi | `KIMI_AUTH_TOKEN` env (else cookie) | POST kimi.com/apiv2/.../GetUsages | weekly + 5h (`resetTime`) |
| stepfun | stepfun | `STEPFUN_TOKEN` env (else user/pass login flow) | POST platform.stepfun.com/.../QueryStepPlanRateLimit | 5h + weekly (Unix-sec) |
| ~~windsurf~~ DEFERRED | windsurf | native SQLite `state.vscdb` (editor cache) | local read of `windsurf.settings.cachedPlanInfo` | daily + weekly (`*ResetAtUnix`) — see defer below |
Notes: minimax/doubao/kimi belong with Group 2 (api-key-env bearer) — minimax has
real epoch windows, doubao reuses synthetic's duration-string parser, kimi-official
has a real resetTime (NOT KimiK2, which is the deferred credits-only one). stepfun
is headless via env token; its user/pass login flow (device-register + sign-in) is
heavier and a fallback, not the primary. windsurf is headless-WITH-CAVEAT: a real
native file like gemini's, but needs the Windsurf desktop editor installed (absent
on a pure server → degrade) AND a SQLite read dep; the web path is protobuf-over-
Connect behind browser local-storage (desktop-only) — so port the SQLite read, not
the protobuf web fetch.

**3B — DESKTOP-ONLY (browser-cookie scrape / short-lived JWT, NO native headless
origin) → DEFER (same antigravity wall, report-don't-force):**
| provider | cb_id | why deferred |
|---|---|---|
| cursor | cursor | browser cookie (cursor.com), short-lived JWT, NO CLI file. Real window `billingCycleEnd`. |
| factory | factory | WorkOS/next-auth browser session (cookie + local-storage scrape), NO CLI file. Real window `windowEnd`/`secondsRemaining`. |
| mimo | mimo | browser cookie (`api-platform_serviceToken`+`userId`), desktop-only. Real window `currentPeriodEnd`. |
| ollama | ollama | browser session cookie → `ollama.com/settings` HTML scrape. LIVE-PROVEN dead-end: the opencode-store `ollama-cloud` key is an INFERENCE key — it 404s on /api/user, /api/usage, /api/account (no usage endpoint accepts it). No headless origin. |
| opencode | opencode | browser cookie `auth`/`__Host-auth`. The `~/.local/share/opencode/auth.json` store holds creds for OTHER providers, NOT an opencode-own usage credential — charter assumption corrected. Real window (rolling 5h + weekly). |
| opencodego | opencodego | same opencode browser cookie; HTML scrape of opencode.ai/workspace/{id}/go. Real window (rolling+weekly+monthly). |
| amp | amp | browser cookie (ampcode.com) → settings HTML scrape, desktop-only. |

**3C — manus → BUILD (copilot test PASSED against source).** `MANUS_SESSION_TOKEN`
env → Bearer (headless-sourceable). Applied the refilling-vs-prepaid test to
CodexBar `ManusUsageFetcher.swift:164-206 toUsageSnapshot`: the refresh window is
`usedPercent = (maxRefreshCredits − refreshCredits)/maxRefreshCredits` with
`resetsAt: nextRefreshTime` — a genuine REFILLING ALLOTMENT (per-period cap
`maxRefreshCredits`, current refill `refreshCredits`) with a REAL provider reset.
That is a faithful implicit-reset window (the copilot case), NOT crof's fabricated
reset. So manus maps secondary←refresh-allotment legitimately. Caveat: the
`proMonthlyCredits` primary has `resetsAt: nil` (no reset) so it can't be a
RateWindow — emit only the refresh window; `totalCredits` is a prepaid balance →
the future Balance seam, not forced into a window.

CHARTER CORRECTION (important): the "prefer opencode-store bearer over cookie-scrape
for ollama/opencode/opencodego" rule rested on a false premise. The opencode store
does NOT carry an opencode-own or ollama-usage credential — it holds bearer tokens
for the inference providers the user logged into (anthropic/openai/ollama-cloud-
inference/etc.). ollama's usage endpoint rejects the inference key (proven live);
opencode/opencodego usage is browser-cookie-only. So none of the three collapse to
a headless bearer — all three DEFER.

### Group 4 — cloud-vendor signed  (NO WINDOW)
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| bedrock | bedrock | AWS keys (`AWS_ACCESS_KEY_ID`...) SigV4 | POST ce.amazonaws.com GetCostAndUsage | NO WINDOW (cost) |
| vertexai | vertexai | gcloud ADC / `GOOGLE_APPLICATION_CREDENTIALS` | GET monitoring.googleapis.com/.../timeSeries | NO WINDOW (not mapped) |

### Group 5 — auth-file-token / CLI-probe  (mixed)
Token from a provider CLI's own on-disk file, or shell out to its CLI.
GROK LIVE-PROVEN: the opencode store `xai` entry is an OAUTH token (type/refresh/
access/expires — like claude, NOT an inference api-key like ollama-cloud). Probed
grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig live with it (POST,
grpc-web+proto, empty 5-byte frame, Bearer) → HTTP 200 grpc-status:0 with real
protobuf Timestamp fields (billing-period start ≈2026-05-31 / end ≈2026-06-30). So
grok IS live-anchorable via the opencode xai OAuth token — the store shortcut holds
here (unlike ollama). The response is protobuf (grpc-web), so grok needs minimal
protobuf decoding of the billing Timestamps, not JSON.
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| **grok** ✅ | grok | opencode `xai` OAuth (LIVE-VERIFIED) | POST grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig (grpc-web+proto) | monthly (billingPeriodEnd) — DONE, commit ed302a5 |
| **jetbrains** ✅ | jetbrains | local XML `AIAssistantQuotaManager2.xml` | local file read | quota + nextRefill — DONE, commit cb31db2 (hybrid-verified) |
| kiro | kiro | `kiro-cli` CLI probe | CLI stdout parse | credits + reset — PARTIAL (not in v1) |
| **codebuff** ✅ | codebuff | `CODEBUFF_API_KEY` or `~/.config/manicode/credentials.json` | POST codebuff.com/api/v1/usage + GET /api/user/subscription | weekly (subscription) — DONE, commit 7a3f4c9 |

### Group 6 — the former "NO WINDOW" set, re-categorized (ALF ruling)

"No window" conflated two different things. Each provider below is now tagged:
- **IMPLICIT-RESET** — quota-like with a FAITHFUL reset the provider itself
  reports/uses → folds into the existing RateWindow, no new axis → PROMOTABLE to
  v1. A reset counts only if it is real (a field in the response or a genuine
  cycle the provider uses), never a fabricated convenience period.
- **TRULY-RESETLESS** — prepaid balance / cumulative cost / rate gauge with no
  real period → needs the future Balance axis (the reserved seam) → stays deferred.
- **REPORT** — carries a billing/renewal date, but whether the signal is a
  *refilling quota* (implicit-reset) or a *prepaid balance with a charge date*
  (resetless) cannot be determined from the response shape alone. Held for driver
  confirmation rather than promoted on a guess (the "report, don't force" rule).

| provider | cb_id | category | reset evidence |
|---|---|---|---|
| copilot | copilot | **IMPLICIT-RESET → PROMOTE** | real top-level `quota_reset_date` field + refilling monthly `percent_remaining`; CodexBar parses the date (`Sources/CodexBarCore/CopilotUsageModels.swift:217,223,257`) and only drops it in its *simplified* per-quota window (`Sources/CodexBarCore/Providers/Copilot/CopilotUsageFetcher.swift:141`). Faithful: `percent_remaining`→usedPercent, `quota_reset_date`→resetsAt, windowMinutes=43200 (monthly). |
| deepseek | deepseek | TRULY-RESETLESS | USD balance, no period (DeepSeekUsageSnapshot resetsAt nil). Balance axis. |
| moonshot | moonshot | TRULY-RESETLESS | account balance, no period. Balance axis. |
| venice | venice | TRULY-RESETLESS | USD/DIEM balance; DIEM epoch-allocation is not a reset. Balance axis. |
| kimik2 | kimik2 | TRULY-RESETLESS | prepaid credits, no period. Balance axis. |
| deepgram | deepgram | TRULY-RESETLESS | usage-breakdown count, no period. Balance/count axis. |
| mistral | mistral | TRULY-RESETLESS | cumulative monthly COST aggregate (admin billing), not a quota. Balance axis. |
| groq | groq | TRULY-RESETLESS | Prometheus rate gauge; usedPercent is statically 0 (GroqUsageSnapshot:65) — a throughput metric, not a quota window. Exclude. |
| azureopenai | azureopenai | TRULY-RESETLESS | validation probe only (no usage payload at all). Exclude. |
| abacus | abacus | **REPORT** | `nextBillingDate`→resetsAt exists, but compute-points are likely prepaid (depleting), not a refilling allotment. Refill-vs-prepaid unverified. (cookie auth.) |
| augment | augment | **REPORT** | `billingPeriodEnd`→resetsAt exists; credits = consumed/available. Refill-vs-prepaid unverified. (cookie/CLI auth.) |
| commandcode | commandcode | **REPORT** | `billingPeriodEnd` present; monthlyCredits signal. Refill-vs-prepaid unverified. (cookie auth.) |
| perplexity | perplexity | **REPORT** | "recurring" credit grants carry `renewalDateTs`→resetsAt; "recurring" implies refill (→ implicit-reset), but waterfall-attributed across recurring/purchased/promotional. (cookie auth.) |
| openrouter | openrouter | EXCLUDED (Ufuk) | not categorized. |

**Promotion proposal to driver:** copilot → v1 (verified faithful). The 4 REPORT
providers need a refill-semantics check before promotion; all 4 are also cookie-auth
(a G3 concern), so they're naturally sequenced with the G3 cookie/web work, not now.

---

## Shared scaffolding to build ONCE (before fan-out), owned by me

These are the contended shared surfaces; per the fan-out rule they go through me,
not parallel workers:
1. `model.rs` — RateWindow/Usage/ProviderUsage (DONE).
2. `opencode_auth.rs` — unified opencode store reader (DONE; claude uses it).
3. `provider.rs` `UsageProvider` trait + `FetchError` (DONE).
4. **`http.rs` (TO BUILD)** — a shared bearer-GET-JSON helper (timeout, 401/403→
   Unauthorized, status→Upstream, body decode) so Group 1/2 providers are ~40 lines
   each. codex/anthropic currently inline this; extract before fan-out.
5. **env/settings reader helper** — the `api-key-env` pattern (env var list → first
   non-empty), shared by all of Group 2/6.
6. `lib.rs` Registry — additive one-line registration per provider (I merge).

## Proposed fan-out (AFTER driver signs off scope + shared scaffolding)
- 1 worker per archetype GROUP (disjoint files: `<provider>.rs` each), starting
  with Group 2 (api-key-env + window — simplest, highest parity-per-effort).
- Group 1 (antigravity/gemini) — 1 worker; Google oauth refresh is the shared cost.
- Group 3 (cookie/web) — deferred or per-provider, decide which matter to Alfonso.
- Workers touch ONLY their `<provider>.rs` + one additive Registry line; any
  model.rs/lib.rs change routes through me.

## Open questions for the driver
- **Q-matrix-1 (scope):** all 46 (NO-WINDOW → degraded), or just the ~24
  window-bearing providers? (my lean: ~24 first.)
- **Q-matrix-2 (opencode store):** for providers opencode holds (openrouter,
  deepseek, google, xai, ollama-cloud, ...), prefer the opencode-store bearer over
  CodexBar's cookie/keychain path? (my lean: yes — simpler, cross-platform, already
  proven for claude.)
- **Q-matrix-3 (credits signal):** should Alfonso model a credits/balance signal at
  all, or do NO-WINDOW providers simply stay "no signal"? (affects whether Group 6
  is worth any effort.)
