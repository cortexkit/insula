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

**Open question Q-matrix-1 (for the driver):** do we port all 46 for parity
(NO-WINDOW providers emit a degraded/credits-only entry), or scope the fan-out to
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
| antigravity | antigravity | `~/.codexbar/antigravity/oauth_creds.json` + Google client | POST cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota | per-model (resetTime, no windowMinutes) |
| gemini | gemini | `~/.gemini/oauth_creds.json` | POST cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota | per-model 24h |
Notes: antigravity/gemini refresh via Google oauth2 token endpoint and discover
client secrets from the installed CLI's bundle — heavier than codex/claude.
antigravity is the per-model-windows case Alfonso's extractor already special-cases.

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
Notes: warp needs `x-warp-client-id` + UA spoof; zai is multi-region; crof
fabricates resetsAt locally; kilo straddles Group 5 (env OR auth-file).

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

### Group 6 — api-key-env, NO WINDOW (credits/balance/count)
Don't fit RateWindow. Lowest priority; emit degraded or a future credits signal.
| provider | cb_id | signal |
|---|---|---|
| openrouter | openrouter | credits (totalUsage/totalCredits) |
| deepseek | deepseek | USD balance |
| moonshot | moonshot | balance |
| venice | venice | USD/DIEM balance |
| kimik2 | kimik2 | credits |
| deepgram | deepgram | usage count |
| copilot | copilot | premium-interaction % remaining (no reset) |
| groq | groq | prometheus rate (no window) |
| azureopenai | azureopenai | validation probe only |
| abacus | abacus | compute points (cookie) |
| augment | augment | credits (cookie/CLI) |
| commandcode | commandcode | credits (cookie) |
| mistral | mistral | cost (cookie, admin.mistral.ai) |
| perplexity | perplexity | credits (cookie) |
| alibaba | alibaba | HAS WINDOW actually (5h/weekly/monthly) — move to Group 2/3; api-key OR cookie |

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
