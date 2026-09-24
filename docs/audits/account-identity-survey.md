# Account identity survey: where each provider says who a credential belongs to

Run 2026-09-24 on the operator's Mac. insula sets `account` on an entry only
when the credential resolves to an identity. This survey asks, for every
provider without one today, whether an identity can be had and from where.

Sources checked for each provider:

1. CodexBar at its newest tag, `v0.65.0` (`~/Work/OSS/CodexBar`, read with
   `git show v0.65.0:<path>`). Swift paths below are relative to
   `Sources/CodexBarCore/Providers/`; plugin paths to
   `Sources/CodexBarCore/Resources/Plugins/`.
2. The provider's own API docs or official client source.
3. For cookie providers, the page or JSON endpoint the logged-in site uses.

Credentials on this host: the opencode auth store holds `openrouter`, `deepseek`,
`kimi-for-coding`, `ollama-cloud` (API keys) and `xai` (OAuth access token), plus
entries for other providers. `~/.grok/auth.json` exists. Chrome has cookies for
ollama.com, opencode.ai, qwencloud.com, kimi.com, grok.com and cursor.com. It has
none for ampcode.com, factory.ai, xiaomimimo.com or deepseek.com (locale only),
and only tracking cookies for qoder.com. No provider API keys are set in the
environment. `~/.gemini/oauth_creds.json` does not exist. Nothing was read from
the credential vault.

Terms used below:

- **stable id**: a user or account id that does not change (a UUID, a
  `user_…` id or a numeric id). This is the value insula would use as `account`.
- **email**: an email address or display name. It can be shown to a person, but
  on its own it does not count as identity.
- **extra request**: the identity needs a call insula does not make today.
  **free** means the value is already in a response or token insula reads.

## Summary

| provider | identity source | kind | extra request? | verified? | verdict |
|---|---|---|---|---|---|
| **grok** | access-token JWT claim `sub` (also `team_id`) | stable id (UUID) | free (claims of the token insula already sends) | live | **obtainable** |
| **openrouter** | `GET /api/v1/key` → `data.creator_user_id` (also `workspace_id`, `organization_id`) | stable id (`user_…`, 32 chars) | extra request | live | **obtainable** |
| **ollama** | API key: `POST https://ollama.com/api/me` → `ID`, `Email`. Cookie: settings page `#header-email` | stable id (UUID) with the API key; email only with the cookie | API key: extra request, and a credential the ollama lane does not use today. Cookie: free | live (both) | **obtainable** (API key) / **email-only** (cookie lane as built) |
| **deepseek** | none. `/user/balance` has no identity and no user endpoint exists | — | — | live (balance fields), docs | **not obtainable** with the API key |
| **kimi-for-coding** | none. `/coding/v1/usages` has no identity; key is an opaque `sk-…` | — | — | live | **not obtainable** with the API key |
| kimi | `kimi-auth` JWT claims `sub`, `abstract_user_id` | stable id (20 chars) | free (insula already decodes this JWT's payload) | live (JWT from the Chrome cookie) | **obtainable** |
| opencode / opencodego | `/console/api/orgs` → `id` (`wrk_…`) | stable **workspace** id, not a user id | free for opencodego (already called); opencode resolves a workspace id already | live | **obtainable as a workspace id only** |
| qwen-cloud | `ALIYUN_CONSOLE_CONFIG.CURRENT_PK` on the token-plan page insula already loads; `aui` cookie | stable numeric id (likely) | free | not verified (page served without a session) | **unknown**, likely obtainable |
| factory | `GET api.factory.ai/api/app/auth/me` → `userProfile.id`, `organization.id`; or bearer JWT `sub` | stable id | JWT: free. Endpoint: extra request | CodexBar only | **obtainable** (not verified) |
| amp | settings page "Signed in as `<email>` (`<org>`)" | email (+ org name) | free (page insula already fetches) | CodexBar only | **obtainable but email-only** |
| gemini | `id_token` in `~/.gemini/oauth_creds.json`, claims `sub`, `email` | stable id (Google `sub`) | free (file insula already reads) | CodexBar only | **obtainable** (not verified) |
| elevenlabs | `GET /v1/user` → `user_id` | stable id | extra request | documented, not verified | **obtainable** (not verified) |
| copilot | `GET https://api.github.com/user` → `id`, `login` | stable numeric id | extra request; depends on the token type | documented, not verified | **unknown** (depends on token) |
| codebuff | `GET /api/user/subscription` → `email` | email | extra request | CodexBar only | **obtainable but email-only** |
| jetbrains | none in `AIAssistantQuotaManager2.xml` | — | — | live (file fields) | **not obtainable** from the file insula reads |
| cursor | (already labelled) | — | — | — | — |
| mimo, qoder | none in CodexBar; no session on this host | — | — | — | **unknown** |
| alibaba, clinepass, doubao, kilo, llmproxy, manus, minimax, neuralwatt, sakana, stepfun, sub2api, synthetic, warp, zai, zenmux | CodexBar shows plan or balance only, never an account; no credential on this host | — | — | — | **unknown** |

## Probes and how they were made

All probes were read-only. They used credentials already on this host: the
opencode auth store, `~/.grok/auth.json`, and Chrome's cookie store, decrypted
with the Chrome Safe Storage key the same way insula's cookie layer does. The
output recorded only status codes, field names and value shapes. No
credential, cookie or identity value was printed or written anywhere. No refresh
token was exchanged for any provider, and the Anthropic and OpenAI tokens were
not sent anywhere.

Some calls go beyond what insula's fetch path makes today:

- OpenRouter `GET /api/v1/key`. This is the documented "get current API key"
  endpoint, and `docs/provider-matrix.md` records it as considered.
- Ollama `POST /api/me`. This is the official client's `Whoami`.
- Three GETs that returned 404: DeepSeek `/user` and `/user/info`, and Kimi
  `/coding/v1/users/me`.

All of these read and change nothing.

## grok

- **Source:** the OAuth access token insula already sends is a JWT from
  `https://auth.x.ai`. Its payload carries `sub` (UUID), `principal_id` (UUID,
  equal to `sub` on this host), `principal_type` (`User`) and `team_id` (UUID).
  The scopes include `openid profile email`.
- **Kind:** stable id. `sub` is the xAI user. `team_id` is the team.
- **Cross-checks (live):** `sub` equals `user_id` in `~/.grok/auth.json`, and
  also equals Chrome's `grok.com` cookie `x-userid`. `team_id` equals the
  `team_id` in `~/.grok/auth.json`.
- **Cost:** free. The claims are read from the token already in hand, with no
  request.
- **Email:** not in the access token. It is in `~/.grok/auth.json` (`email`).
  It is also available from the OIDC `userinfo_endpoint`
  `https://auth.x.ai/oauth2/userinfo`, listed in
  `https://auth.x.ai/.well-known/openid-configuration` with `email` in
  `claims_supported`. That endpoint was not called.
- **CodexBar:** `Grok/GrokAuth.swift:188-189` reads `user_id` and `email` from
  the grok CLI's `auth.json`. `Grok/GrokStatusProbe.swift:57-60` shows the email
  as the account and `teamId` as the organization.
- **Verdict:** obtainable, stable id, live-verified, no extra request.

## openrouter

- **Source:** `GET https://openrouter.ai/api/v1/key` (same body at
  `/api/v1/auth/key`), with the bearer API key insula already holds. It returned
  200 with `data.creator_user_id` (`user_…`, 32 chars), `data.workspace_id`
  (UUID) and `data.organization_id` (null for this key). Docs:
  `https://openrouter.ai/docs/api/api-reference/api-keys/get-current-api-key.md`
  lists both `creator_user_id` and `workspace_id` in the response.
- **Kind:** stable id. `creator_user_id` is the user who created the key.
  Credits are billed to the user, or to the organization when
  `organization_id` is set. So an org key's account is its organization, not
  its creator.
- **Cost:** one extra request per fetch. `/api/v1/credits`, the only call
  insula makes today, returns just `total_credits` and `total_usage`.
- **CodexBar:** `openrouter.js` shows only a balance as `loginMethod` (line
  435). It has no account identity.
- **Verdict:** obtainable, stable id, live-verified, needs one extra request.

## ollama

insula's ollama lane reads Chrome cookies and scrapes `ollama.com/settings`.

- **Cookie lane (what insula uses):** the settings page insula already fetches
  has `id="header-email"` holding the signed-in email (live: `…@gmail.com`).
  The only UUID on the page is an avatar image path, not a user id. The same
  cookie jar gets 401 from `POST /api/me`, so the cookie cannot reach the id
  endpoint. CodexBar reads the same email:
  `Ollama/OllamaUsageParser.swift:83` (`id="header-email"`) feeds
  `accountEmail` at `OllamaUsageSnapshot.swift:53-57`.
- **API-key route:** the opencode store's `ollama-cloud` key gets 200 from
  `POST https://ollama.com/api/me`, returning `ID` (UUID), `Email`, `Name` and
  `Plan`. This is the official Go client's `Whoami`
  (`github.com/ollama/ollama` `api/client.go:519-521`, `POST /api/me`; response
  type `UserResponse` in `api/types.go:964-973`, `ID uuid.UUID`). GET on the
  same path returns 405. The earlier dead-end note in
  `docs/provider-matrix.md` (the key 404s on `/api/user`, `/api/usage` and
  `/api/account`) does not cover `/api/me`.
- **Link between the two:** on this host the cookie page's email equals the
  API key's `Email`. That is the only thing tying the key to the cookie
  session. Nothing on the page names the account id.
- **Kind:** stable id (UUID) with the API key. Email only with the cookie.
- **Cost:** the email is free. The id needs one extra request, made with a
  credential the ollama lane does not read today.
- **Verdict:** obtainable with the API key (live-verified). The cookie lane as
  built is email-only.

## deepseek

- **Source checked:** `GET https://api.deepseek.com/user/balance` (what insula
  calls) returns `is_available` and `balance_infos[{currency, total_balance,
  granted_balance, topped_up_balance}]`. There are no identity fields (live).
  The API docs (`https://api-docs.deepseek.com`) cover chat and FIM completion,
  list models, and get user balance. None of these returns an account. Probing
  `/user` and `/user/info` gave 404. The API key is an opaque `sk-…` (35 chars),
  not a JWT.
- **CodexBar:** `DeepSeek/DeepSeekUsageFetcher.swift:186-190` sets
  `accountEmail: nil, accountOrganization: nil`. Its optional platform lane uses
  a browser-localStorage `userToken` against
  `platform.deepseek.com/api/v0/users/get_user_summary`. That lane decodes only
  wallets (`:72-80`), and its "profiles" are browser profiles
  (`DeepSeekPlatformBalanceOwner.swift`), not accounts. This host's Chrome has
  no deepseek.com session.
- **Verdict:** not obtainable with the credential insula holds. A web-session
  identity was not surveyed, because no session exists here.

## kimi-for-coding

- **Source checked:** `GET https://api.kimi.com/coding/v1/usages` (what insula
  calls) returns `usage`, `limits` and `usages` (`limit_5h`, `limit_7d`). There
  are no identity fields (live). `/coding/v1/models` returns only models.
  `/coding/v1/users/me` returns 404. The key is an opaque `sk-…` (72 chars),
  not a JWT.
- **CodexBar:** `Kimi/KimiUsageSnapshot.swift:188-192` sets
  `accountEmail: nil, accountOrganization: nil` and shows only the plan name.
- **Verdict:** not obtainable with the API key.

## kimi (web lane)

- **Source:** the `KIMI_AUTH_TOKEN` insula reads is the `kimi-auth` cookie
  (`crates/quota-core/src/kimi.rs:3,374`). It is a JWT whose payload carries
  `sub` (20 chars), `abstract_user_id` (20 chars) and `space_id`. Verified on
  the Chrome `kimi-auth` cookie on this host. The env var itself is not set
  here.
- **Kind:** stable id. `sub` is the user. Whether `abstract_user_id` always
  equals `sub` was not checked.
- **Cost:** free. `kimi.rs:100-123` already base64-decodes this payload to read
  `device_id` and `ssid`.
- **CodexBar:** `Kimi/KimiUsageSnapshot.swift:188-192` shows no account.
- **Verdict:** obtainable, stable id, verified on the browser copy of the token.

## opencode and opencodego

- **Source:** `GET https://opencode.ai/console/api/orgs` with the Chrome cookie
  jar returned 200 with `[{id: "wrk_…" (30 chars), name}]` (live). opencodego
  already calls this path (`opencodego.rs:49`). opencode's `_server` flow
  already resolves a workspace id.
- **Kind:** a stable **workspace** id, not a user id. A workspace can be shared
  and one user can have several. No user id or email was found. The `auth`
  cookie is a sealed opaque value (`Fe26…`), and the `__Host-console_session`
  cookie is an opaque `st_…`.
- **CodexBar:** `OpenCode/` and `OpenCodeGo/` set no identity.
- **Verdict:** obtainable as a workspace id only. That is free and
  live-verified, but it is not an account in the user sense.

## qwen-cloud

- **Source:** the token-plan page insula loads for `SEC_TOKEN`
  (`home.qwencloud.com/billing/subscription/token-plan-individual`) defines a
  `var ALIYUN_CONSOLE_CONFIG = {…}` object. The page's own analytics code reads
  `uid: ALIYUN_CONSOLE_CONFIG.CURRENT_PK`. Chrome also holds `aui` and `cnaui`
  cookies for qwencloud.com, equal to each other, each a 16-digit number.
- **Live result:** the page returned 200, but the config object carried only
  `APP_ID`, `PRODUCT`, `LANG`, `LOCALE`, `portalType` and `MAIN_RESOURCE_CDN`.
  There was no `SEC_TOKEN` and no `CURRENT_PK`, so this host's Qwen session
  appears to be logged out. The `aui` value could not be tied to any response.
- **Kind:** likely a stable numeric account id (`CURRENT_PK`). Not confirmed.
- **Cost:** free, since it comes from the same page insula already parses.
- **CodexBar:** has no Qwen Cloud provider.
- **Verdict:** unknown, likely obtainable. Re-probe with a live session.

## factory

- **Source (CodexBar only; no factory.ai cookies here):**
  `Factory/FactoryStatusProbe.swift:1107` calls
  `GET <base>/api/app/auth/me`, decoding `userProfile.id`, `userProfile.email`
  and `organization.id`/`name` (`:142-165`). The user id comes from
  `userProfile.id`, falling back to the bearer JWT's `sub`
  (`:1037-1038`, `:1464-1476`). Organization name and email are shown as the
  account (`:461-462`).
- **insula today:** calls only `api.factory.ai/api/billing/limits`, with a
  bearer resolved from the `access-token` or `session` cookie
  (`factory.rs:3-5,50,72`).
- **Kind:** stable id (`userProfile.id` or JWT `sub`), plus an org id.
- **Cost:** the JWT `sub` is free when the bearer is a JWT. `auth/me` is one
  extra request.
- **Verdict:** obtainable (CodexBar only, not verified).

## amp

- **Source (CodexBar only; no ampcode.com cookies here):**
  `Amp/AmpUsageParser.swift:22-23,158-159` matches
  `Signed in as <email> (<org>)` in the settings page text. insula already
  fetches that page (`amp.rs:28`).
- **Kind:** email plus an organization name. No stable id was found.
- **Verdict:** obtainable but email-only (CodexBar only, not verified).

## gemini

- **Source:** gemini-cli's `~/.gemini/oauth_creds.json` (the file insula
  reads, `gemini.rs:19`) stores an `id_token`. CodexBar decodes its claims for
  the email: `Gemini/GeminiStatusProbe.swift:192,287,318,930`. A Google ID
  token carries `sub` (a stable Google account id) and `email`.
- **Cost:** free when `id_token` is present in the file. insula refreshes via
  `oauth2.googleapis.com/token`, and CodexBar copies the refreshed `id_token`
  back (`:889-890`).
- **Verified:** no. The file does not exist on this host. `gemini.rs` also
  notes that this lane degrades for free-tier accounts.
- **Verdict:** obtainable, stable id (CodexBar plus the OIDC standard, not
  verified).

## elevenlabs

- **Source:** `GET https://api.elevenlabs.io/v1/user` with `xi-api-key`. The
  docs page `https://elevenlabs.io/docs/api-reference/user/get.mdx` lists
  `user_id` (with `first_name` and `xi_api_key`). insula calls only
  `/v1/user/subscription`.
- **Kind:** stable id.
- **Caveat:** ElevenLabs keys can be permission-scoped. Whether a key limited
  to subscription reads can also read `/v1/user` was not checked.
- **CodexBar:** `elevenlabs.js:119` shows only `loginMethod`.
- **Verdict:** obtainable (documented, not verified), one extra request.

## copilot

- **Source:** GitHub's documented `GET https://api.github.com/user`
  ("get the authenticated user") returns a numeric `id` and a `login`. insula
  reads `COPILOT_API_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN` and calls
  `copilot_internal/user`.
- **Caveat:** whether `/user` accepts the token depends on its kind. A
  `gh`-style OAuth token or PAT can read it; a Copilot-only token may not.
  Whether `copilot_internal/user` itself names the user was not checked.
  CodexBar shows only the plan (`Copilot/CopilotUsageFetcher.swift:120`).
- **Verdict:** unknown. It depends on the token type, and nothing was verified.

## codebuff

- **Source (CodexBar only):** `Codebuff/CodebuffUsageFetcher.swift:163,208`
  calls `GET /api/user/subscription` and reads `email` (or `user.email`). The
  email is shown as the account (`:79`).
- **Verdict:** obtainable but email-only, one extra request (CodexBar only).

## jetbrains

- **Source checked:** insula reads `options/AIAssistantQuotaManager2.xml`
  (`jetbrains.rs:4`). On this host the file has two options, `quotaInfo` (keys:
  `type`) and `nextRefill` (keys: `exception`, `previous`, `type`). Neither has
  an account. No other `options/*.xml` under the installed IDEs names an email,
  user id or account id. CodexBar puts the IDE name in `accountOrganization`
  (`JetBrains/JetBrainsStatusProbe.swift:73`), not an account.
- **Verdict:** not obtainable from the file insula reads. The JetBrains
  Account login lives elsewhere and was not surveyed.

## mimo, qoder

- **mimo:** CodexBar shows only a plan label
  (`MiMo/MiMoUsageSnapshot.swift:86`). No xiaomimimo.com cookies here.
  **Unknown.**
- **qoder:** CodexBar's `qoder.js` sets no identity. insula's endpoint lives
  under `/api/v2/me/…`, which suggests a `me` resource exists, but no call to
  it was checked. Chrome holds only the tracking cookies insula already ignores
  (`qoder.rs:275`), so no probe was possible. **Unknown.**

## The rest

alibaba, clinepass, doubao, kilo, llmproxy, manus, minimax, neuralwatt, sakana,
stepfun, sub2api, synthetic, warp, zai and zenmux have no credential on this
host. For each, CodexBar v0.65.0 shows only a plan, balance or key count as
identity, never an account:

- `Alibaba/AlibabaCodingPlanUsageSnapshot.swift:86`
- `clinepass.js:127`
- `Doubao/DoubaoUsageFetcher.swift:155`
- `Kilo/KiloUsageFetcher.swift:87`
- `llmproxy.js:150`
- `manus.js:63`
- `MiniMax/MiniMaxUsageSnapshot.swift:129,158`
- `neuralwatt.js:148`
- `Sakana/SakanaUsageFetcher.swift:63`
- `StepFun/StepFunUsageFetcher.swift:257`
- `sub2api.js:217`
- `synthetic.js:350`
- `zai.js:176-207`
- `ZenMux/ZenMuxUsageFetcher.swift:110`
- Warp sets nothing.

Notes:

- sub2api and llmproxy are self-hosted relays. Their key identifies a group or
  proxy, not a provider account.
- synthetic: Chrome holds a synthetic.new Clerk `__session` JWT with `sub`
  (`user_…`). That is the website session, not the API key insula uses, so it
  does not answer the question.
- The official API docs for these providers were not checked in this run (no
  doc retrieval beyond the providers above).

All are **unknown**.

## Where adding identity is cheap and verified (ranked)

1. **grok.** Read `sub` (and `team_id`) from the access token already in hand.
   No request. Live-verified, and it matches the grok CLI's `user_id`.
2. **kimi.** Read `sub` from the `kimi-auth` JWT payload that `kimi.rs` already
   decodes. No request. Verified on the browser copy of the same token.
3. **openrouter.** One extra `GET /api/v1/key` for `creator_user_id` (or
   `organization_id` when set). Live-verified and documented.
4. **ollama.** One extra `POST /api/me` with the opencode-store `ollama-cloud`
   key gives a UUID. Live-verified. It depends on a credential the cookie lane
   does not use today. The free alternative, the page email, is email-only.
5. **opencode / opencodego.** A free, live-verified `wrk_…` id, but it names a
   workspace, not a user. Only worth using if a workspace is an acceptable
   meaning of `account` for these two.

Not cheap-and-verified today: factory and gemini (likely free, but no
credential here to verify); qwen-cloud (likely free, session logged out
here); elevenlabs (documented, needs an extra request and possibly a key
permission); amp and codebuff (email only); deepseek, kimi-for-coding and
jetbrains (nothing to read with the credential insula holds).
