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

**Current parity: CodexBar v0.47.0** (35 providers registered; verified
2026-08-05). CodexBar is a moving upstream; parity is re-checked whenever it
publishes a newer GitHub release. Read that release's content with `git show
<tag>:<path>` or `git grep <tag>` — the checkout usually sits at an older tag, so
plain `grep` silently reads a different version and reports a symbol added in the
new release as absent. On a new release, diff `git -C ~/Work/OSS/CodexBar
diff v0.47.0..<new-tag> -- Sources/CodexBarCore/Providers/` and triage into: window
drift on providers we already serve (highest risk — no live creds to catch a
silent degradation), new window-bearing providers to port, and balance/credits
providers (deferred to the Balance axis). When a diff touches a provider we serve,
confirm it changes the endpoint WE parse (e.g. Claude's live anchor reads
`/api/oauth/usage`, not CodexBar's CLI/web fetcher) and that the field actually
changed within the range (`git show <old-tag>:<file>`), not a pre-existing value.

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
| ~~antigravity~~ DEFERRED | antigravity | NO native headless creds source — see below | POST cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota | per-model (resetTime) |
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
- **antigravity = DEFERRED (report-don't-force, same as crof).** It has NO native
  headless creds source: the token lives ONLY at `~/.codexbar/antigravity/
  oauth_creds.json` — a CodexBar-CREATED path (CodexBar runs its own OAuth login
  and writes there; no native antigravity token file to import,
  AntigravityOAuthCredentialsStore.swift:242-251). The OAuth client is discoverable
  ONLY from env vars or by parsing the installed `Antigravity.app/.../main.js`
  bundle (macOS-desktop archaeology, AntigravityOAuthCredentialsStore.swift:155-196).
  On a headless machine that never ran CodexBar, antigravity has no usable origin.
  Revisit only if a native antigravity token path appears.

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
design above becomes shippable as-is. Note for a future build: SET
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
