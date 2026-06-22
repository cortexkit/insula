# Provider matrix — the 46 CodexBar providers, by auth archetype

Reverse-engineered from CodexBar source (`/Users/ufukaltinok/Work/OSS/CodexBar/Sources/CodexBarCore/Providers/*`),
every row cited to `file:line` in the study transcripts. This is the map that
makes parallel fan-out safe: the **auth archetype** is the unit of effort (shared
auth+fetch scaffolding within a group), so one worker owns one archetype group.

Two providers are already built and proven live end-to-end:
**codex** (oauth-local-file) and **claude** (oauth-bearer via opencode store).

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

### Group 3 — web-session / browser-cookie, HAS WINDOW
Session from browser cookies (macOS-only import) or a login flow → web backend.
These are the HARDEST: cookie import is macOS-only and fragile, some scrape HTML
or speak protobuf. Lower priority unless the provider matters to Alfonso.
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| ollama | ollama | browser cookie OR opencode `ollama-cloud` | GET ollama.com/settings (HTML) | session + weekly |
| opencode | opencode | browser cookie `auth`/`__Host-auth` | POST/GET opencode.ai/_server | rolling + weekly |
| opencodego | opencodego | browser cookie (same) | GET opencode.ai/workspace/{id}/go (HTML) | rolling + weekly + monthly |
| cursor | cursor | browser cookie (cursor.com) | GET cursor.com/api/usage-summary | billing cycle |
| factory | factory | cookie OR bearer (WorkOS oauth) | GET api.factory.ai/api/billing/limits | 5h + weekly + monthly |
| minimax | minimax | cookie OR `MINIMAX_API_KEY` | GET {host}/v1/api/openplatform/coding_plan/remains | interval + weekly |
| mimo | mimo | cookie (api-platform_serviceToken + userId) | GET {api}/tokenPlan/usage | month |
| manus | manus | cookie OR `MANUS_SESSION_TOKEN` | POST api.manus.im/.../GetAvailableCredits | monthly + daily refresh |
| kimi | kimi | cookie OR `KIMI_AUTH_TOKEN` | POST kimi.com/apiv2/.../GetUsages | weekly + 5h rate limit |
| stepfun | stepfun | login flow (user/pass) → Oasis-Token | POST platform.stepfun.com/.../QueryStepPlanRateLimit | 5h + weekly |
| windsurf | windsurf | browser local-storage / SQLite `state.vscdb` | POST windsurf.com/_backend/.../GetPlanStatus (protobuf) | daily + weekly |
| doubao | doubao | `ARK_API_KEY`/`DOUBAO_API_KEY` | POST ark...volces.com/.../chat/completions (probe) | from response rate-limit headers |
| amp | amp | browser cookie (ampcode.com) | GET ampcode.com/settings (HTML scrape) | freeTierUsage quota + windowHours |
Notes: opencode/opencodego — for OUR ecosystem these likely become opencode-store
bearer reads (Group 1-style), NOT cookie scrapes; flag for design. windsurf is
protobuf-over-Connect. stepfun's login flow is the heaviest (device register +
password sign-in).

### Group 4 — cloud-vendor signed  (NO WINDOW)
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| bedrock | bedrock | AWS keys (`AWS_ACCESS_KEY_ID`...) SigV4 | POST ce.amazonaws.com GetCostAndUsage | NO WINDOW (cost) |
| vertexai | vertexai | gcloud ADC / `GOOGLE_APPLICATION_CREDENTIALS` | GET monitoring.googleapis.com/.../timeSeries | NO WINDOW (not mapped) |

### Group 5 — auth-file-token / CLI-probe  (mixed)
Token from a provider CLI's own on-disk file, or shell out to its CLI.
| provider | cb_id | session source | endpoint | window |
|---|---|---|---|---|
| grok | grok | `~/.grok/auth.json` or cookie | POST grok.com (protobuf) OR `grok agent` JSON-RPC | monthly (billingPeriodEnd) — HAS WINDOW |
| jetbrains | jetbrains | local XML `AIAssistantQuotaManager2.xml` | local file read | quota + nextRefill — HAS WINDOW |
| kiro | kiro | `kiro-cli` CLI probe | CLI stdout parse | credits + reset — PARTIAL |
| codebuff | codebuff | `CODEBUFF_API_KEY` or `~/.config/manicode/credentials.json` | POST codebuff.com/api/v1/usage | weekly (subscription) — PARTIAL |

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
