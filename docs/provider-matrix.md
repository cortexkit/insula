# Provider matrix

38 providers registered.

This is the public inventory of `Registry::with_defaults` in
`crates/quota-core/src/lib.rs`, not a porting plan or a parity-review ledger.
Endpoints and signals below describe the current implementation; they do not
promise that every account has a quota product or that every lane is available
on every operating system. The public wire semantics are in
[`consumer-contract.md`](consumer-contract.md).

## Reading the inventory

- Vault families are the credential-id prefixes in
  `crates/quota-core/src/vault_handles.rs`; an account suffix may follow the prefix.
  Families marked enrichment do not enumerate an independent usage lane.
- A deposited cookie is a request `Cookie:` header captured by Cerebellum, not a
  browser store read by insula. The capture URL table is in
  [`consumer-contract.md`](consumer-contract.md#depositing-a-session-cookie-which-url-to-capture-against).
- Periods name supported windows, not guaranteed response fields. Provider-reported
  durations and resets can vary; absent metadata stays absent. Balances are separate
  `spend` pools, never invented rate windows.
- The implementation source is `crates/quota-core/src/<provider>.rs`, replacing
  hyphens with underscores. `claude` uses `anthropic.rs`; its cookie enrichment
  is in `claude_subscription.rs`. Code and public contract take precedence over
  this inventory if an endpoint changes.
- How to make an account appear through each lane is in
  [`credentials.md`](credentials.md); how the module reaches these endpoints is
  in [`architecture.md`](architecture.md).

## OAuth, local-session and vault-token providers

| Provider | Credential sources / vault families | Main endpoints or local source | Published windows / signals |
|---|---|---|---|
| `codex` | Codex CLI `~/.codex/auth.json`; vault `chatgpt:openai` | `GET https://chatgpt.com/backend-api/wham/usage` (base configurable); `/wham/rate-limit-reset-credits` and optional consume | Provider-reported windows (including 5h / weekly), banked resets, credit balance |
| `claude` | OpenCode `anthropic` OAuth; vault `oauth:anthropic`; `cookie:claude.ai` enrichment | `GET https://api.anthropic.com/api/oauth/usage`; profile and optional web subscription enrichment | 5h / weekly, named model windows, optional extra usage and subscription dates |
| `antigravity` | Running editor/CLI, plugin sidebar cache; vault `antigravity:google` | Loopback `LanguageServerService/RetrieveUserQuotaSummary`; `https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary`, refusal fallback `retrieveUserQuota` | Named Gemini and Claude/GPT pool windows (5h / weekly when stated); the vault login reads a paid account's own pool, so neither the editor nor the plugin needs to run |
| `grok` | OpenCode `xai` OAuth; vault `oauth:xai` | gRPC-web `POST https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig` | Billing-period utilization and reset |
| `cursor` | Cursor app `state.vscdb`; vault `oauth:cursor` or `cookie:cursor.com` | `https://cursor.com/api/usage-summary` | Plan and on-demand billing-cycle utilization; period/reset metadata from response |
| `jetbrains` | IDE quota XML and `idea.log` | Local `options/AIAssistantQuotaManager2.xml` or newer same-installation quota log record (legacy AI Assistant, 262.x `JbaiQuota`/`JbaiQuotaRefill`, Junie/Air `quota_update`); no network call | Tariff quota window, stated refill duration; purchased top-up pool |
| `kimi` | `KIMI_AUTH_TOKEN` / `kimi_auth_token`; JWT-derived headers and account identity when available | `POST https://www.kimi.com/apiv2/kimi.gateway.billing.v1.BillingService/GetUsages`; membership `GetSubscriptionStats` | Weekly / 5h plus optional monthly and Code 7-day extras |
| `manus` | `MANUS_SESSION_TOKEN`, `MANUS_SESSION_ID`, `MANUS_COOKIE` | `POST https://api.manus.im/user.v1.UserService/GetAvailableCredits` | Refilling refresh allotment; resetless credit fields are not published as windows |
| `muse` | `MUSE_DEVICE_TOKEN`, JSON at `MUSE_AUTH_PATH` or `~/.config/muse/auth.json`; vault `apikey:muse` | `POST https://api.meta.ai/muse-code/key` | 5h / weekly; Meta omits both while the 5h window is idle, and this module then serves 5h at 0% and the last weekly figure (0% once its reset passes) |

## API-key and signed-request providers

| Provider | Credential sources / vault families | Main endpoints | Published windows / signals |
|---|---|---|---|
| `alibaba` | `ALIBABA_CODING_PLAN_API_KEY`, `ALIBABA_QWEN_API_KEY`, `DASHSCOPE_API_KEY`; region/URL overrides | `POST https://modelstudio.console.alibabacloud.com/data/api.json` or `https://bailian.console.aliyun.com/data/api.json`, `queryCodingPlanInstanceInfoV2` | 5h / weekly / monthly |
| `codebuff` | `CODEBUFF_API_KEY` or `~/.config/manicode/credentials.json` | `POST https://www.codebuff.com/api/v1/usage`; `GET /api/user/subscription` | Credit quota utilization and weekly subscription window |
| `copilot` | `COPILOT_API_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN` | `GET https://api.github.com/copilot_internal/user` | Monthly premium-request quota |
| `deepseek` | `DEEPSEEK_API_KEY`, `DEEPSEEK_TOKEN`, OpenCode `deepseek`; vault `apikey:deepseek` | `GET https://api.deepseek.com/user/balance` | Granted and purchased balances in reported currencies; no rate window |
| `doubao` | Volcengine access-key id / secret / region env settings; Ark fallback `ARK_API_KEY`, `VOLCENGINE_API_KEY`, `DOUBAO_API_KEY` | Signed `POST https://open.volcengineapi.com/?Action=GetCodingPlanUsage&Version=2024-01-01`; fallback `https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions` | Coding Plan windows; fallback request-rate quota from headers with unknown duration |
| `elevenlabs` | `ELEVENLABS_API_KEY`, `XI_API_KEY` | `GET https://api.elevenlabs.io/v1/user/subscription` | Character allowance and reported next reset |
| `kimi-for-coding` | `KIMI_CODE_API_KEY`; vault `kimi-for-coding` / `apikey:kimi-for-coding`; `cookie:kimi.com` enrichment | `GET https://api.kimi.com/coding/v1/usages`; optional membership `GetSubscriptionStats` | Weekly / 5h, optional monthly ratio pool and web subscription extras |
| `clinepass` | `CLINE_API_KEY`, `CLINEPASS_API_KEY` | `GET https://api.cline.bot/api/v1/users/me/plan/usage-limits` | 5h / weekly / monthly |
| `llmproxy` | `LLM_PROXY_API_KEY`, configured `LLM_PROXY_BASE_URL` | `GET {base}/v1/quota-stats` | Named quota groups with provider-reported reset metadata |
| `minimax` | `MINIMAX_CODING_API_KEY`, `MINIMAX_API_KEY`; vault `apikey:minimax-coding-plan`; `MINIMAX_API_REGION` | `GET https://api.minimax.io/v1/api/openplatform/coding_plan/remains` (China: `api.minimaxi.com`); optional `/account/query_balance` | General interval / weekly, non-text model window; wallet pools |
| `neuralwatt` | `NEURALWATT_API_KEY`; `NEURALWATT_API_URL` override | `GET https://api.neuralwatt.com/v1/quota` | Subscription utilization and named extra quotas; resetless balance fields are not published |
| `openrouter` | OpenCode `openrouter` API key; vault `apikey:openrouter` | `GET https://openrouter.ai/api/v1/credits`; `/api/v1/key` for owner | Derived USD balance; no rate window |
| `stepfun` | `STEPFUN_TOKEN` | `POST https://platform.stepfun.com/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit` | 5h / weekly quota |
| `sub2api` | `SUB2API_API_KEY`, configured `SUB2API_BASE_URL` | `GET {base}/v1/usage` | Named rate-limit windows (5h / 1d / 7d recognized); money fields omitted |
| `warp` | `WARP_API_KEY`, `WARP_TOKEN` | `POST https://app.warp.dev/graphql/v2?op=GetRequestLimitInfo` | Request-limit utilization and next refresh |
| `synthetic` | `SYNTHETIC_API_KEY`; vault `apikey:synthetic`; `SYNTHETIC_API_URL` override | `GET https://api.synthetic.new/v2/quotas` | Subscription / weekly and additional reported quota windows |
| `zai` | `Z_AI_API_KEY`; configurable base/region and optional organization/project scope | `GET https://api.z.ai/api/monitor/usage/quota/limit` (base configurable) | Token quota and named limits; durations from upstream units |
| `zenmux` | `ZENMUX_MANAGEMENT_API_KEY` | `GET https://zenmux.ai/api/v1/management/subscription/detail` | 5h / 7-day quota and request counts; PAYG balance not fetched |
| `kilo` | `KILO_API_KEY` or `~/.local/share/kilo/auth.json`; optional `KILO_ORGANIZATION_ID` | tRPC batch at `https://app.kilo.ai/api/trpc/`, including `user.getCreditBlocks` and `kiloPass.getState` | Kilo Pass subscription window only; resetless credits omitted |

## Deposited-cookie and environment-cookie providers

| Provider | Credential sources / vault families | Main endpoints | Published windows / signals |
|---|---|---|---|
| `amp` | Vault `cookie:ampcode.com` | `GET https://ampcode.com/settings` | Free-tier utilization with reported `windowHours`; full-recovery reset projected from continuous replenishment |
| `factory` | Vault `cookie:factory.ai` | `GET https://api.factory.ai/api/billing/limits` | 5h / weekly / monthly pools |
| `langdock` | Vault `cookie:langdock.com` | `GET https://app.langdock.com/api/trpc/usageSettings.getPersonalUsage` | Session (5h) / weekly |
| `mimo` | Vault `cookie:xiaomimimo.com` | `https://platform.xiaomimimo.com/api/v1/tokenPlan/detail` and `/usage` | Token-plan billing-period window |
| `ollama` | Vault `apikey:ollama-cloud` (takes precedence) or `cookie:ollama.com` | Key: `GET https://ollama.com/api/balance`; cookie: `GET https://ollama.com/settings` | Key balance; page included-credit / session (5h) / weekly windows and wallet pools |
| `opencode` | Vault `cookie:opencode.ai` (shared with Go) | Workspace, subscription and billing actions at `https://opencode.ai/_server`; console workspace check | Rolling 5h / weekly subscription windows; migrated Zen accounts without a readable usage endpoint degrade |
| `opencodego` | `OPENCODE_API_KEY` or vault `apikey:opencode-go` (key takes precedence); otherwise `cookie:opencode.ai` | Key: `GET https://opencode.ai/zen/go/v1/usage`; cookie: `/console/api/orgs`, `/console/api/go/status`, legacy `/workspace/<id>/go` fallback | Rolling 5h / weekly / monthly |
| `qoder` | Vault `cookie:qoder.com` | `GET https://qoder.com/api/v2/me/usages/big_model_credits` | Combined base/shared primary quota; duration unstated |
| `qwen-cloud` | Vault `cookie:qwencloud.com` | Personal plan gateway at `https://cs-data.qwencloud.com/data/api.json`; Team inventory/seat calls at `https://home.qwencloud.com` | Personal 5h / weekly / monthly; Team seat quota |
| `sakana` | `SAKANA_COOKIE` environment request header | `GET https://console.sakana.ai/billing` | 5h / weekly subscription cards |
