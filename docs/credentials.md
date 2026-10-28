# Making an account show up

insula never asks for a password and never issues a credential. An account
appears when insula can find a credential for it on the host, through one of
three lanes:

| Lane | Published `source` | Where the credential comes from |
|---|---|---|
| vault | `vault` | the CortexKit credential vault, via the daemon |
| local login | `oauth` | a token or session file a provider's own tool wrote on this machine |
| API key | `api` | an environment variable or a provider tool's config file |

Which lanes a provider supports is listed per provider in
[`provider-matrix.md`](provider-matrix.md). A provider with no credential in
any lane is reported as `unconfigured` in health and is either absent from
`usage.get` or present as a `credential_absent` entry; neither is a fault.

## Vault logins

The vault is the preferred lane: it refreshes OAuth tokens itself, holds one
record per account, and is the only lane that can serve several accounts of
one provider with their identities.

insula reads the vault records granted to it in two categories:

- `llm-provider`: OAuth logins and API keys;
- `browser-session`: web session cookies.

### Log in with `ck auth`

```sh
ck auth login            # follow the prompts for the provider
ck quota                 # the account appears once insula has fetched it
```

insula lists the vault's grants at the start of every refresher turn, so a new
record gets a slot within a few seconds and is fetched on that turn or the
next. Records are matched to providers by their id prefix (the "family"):

| Vault id family | Provider |
|---|---|
| `chatgpt:openai` | `codex` |
| `oauth:anthropic` | `claude` |
| `cookie:claude.ai` | `claude` (subscription dates only, never a usage lane) |
| `oauth:xai` | `grok` |
| `antigravity:google` | `antigravity` |
| `kimi-for-coding`, `apikey:kimi-for-coding` | `kimi-for-coding` |
| `cookie:kimi.com` | `kimi-for-coding` (subscription extras only) |
| `oauth:cursor`, `cookie:cursor.com` | `cursor` |
| `cookie:ampcode.com` | `amp` |
| `cookie:qwencloud.com` | `qwen-cloud` |
| `cookie:qoder.com` | `qoder` |
| `cookie:factory.ai` | `factory` |
| `cookie:langdock.com` | `langdock` |
| `cookie:trae.cn` | `traecn` |
| `cookie:xiaomimimo.com` | `mimo` |
| `cookie:ollama.com`, `apikey:ollama-cloud` | `ollama` |
| `cookie:opencode.ai` | `opencode` and `opencodego` |
| `apikey:opencode-go` | `opencodego` |
| `apikey:deepseek` | `deepseek` |
| `apikey:synthetic` | `synthetic` |
| `apikey:openrouter` | `openrouter` |
| `apikey:minimax-coding-plan` | `minimax` |
| `apikey:muse` | `muse` |
| `cookie:dev.meta.ai` | `muse` (idle-window enrichment only; `llama_dev_sess`) |
| `apikey:vercel` | `vercel` (the AI Gateway key) |
| `apikey:vercel-budget` | `vercel-budget` (an access token and team id) |
| `apikey:soniox` | `soniox` (an organization API key) |
| `apikey:abliteration` | `abliteration` (an abliteration.ai API key) |
| `apikey:chatbox` | `chatbox` (a Chatbox AI license key) |

The authoritative list is `CREDENTIAL_FAMILIES` in
`crates/quota-core/src/vault_handles.rs`. A record id may carry an account
suffix after the family (`oauth:anthropic:<account>`).

### Web sessions captured by Cerebellum

Some providers publish quota only on a logged-in web page. insula reads their
session cookie only from a vault deposit and never from a browser's cookie
store. Cerebellum, the CortexKit browser module, opens a throwaway browser,
lets the user sign in, and deposits the session as `cookie:<domain>:<account>`
(or `cookie:<domain>`) in the `browser-session` category.

The deposit must be the request `Cookie:` header for the URL insula fetches;
the capture URL for each domain is in the
[cookie-capture table](consumer-contract.md#depositing-a-session-cookie-which-url-to-capture-against).
Rules that follow from how those lanes read identity:

- a domain that supplies usage windows takes one deposit; a second deposit for
  the same domain is refused when the vault listing is installed. Re-capturing
  replaces the record (`ck auth put --replace`) rather than adding a suffix;
- a `cookie:claude.ai` deposit uses the same account suffix as its
  `oauth:anthropic:<account>` login, and is used only after its owner is
  verified against that login;
- a `cookie:kimi.com` deposit only adds subscription extras to
  `kimi-for-coding`; usage comes from the API key.

When a captured session expires, the entry degrades with
`credential_rejected` and the provider is listed in the `cookieLoginsStale`
health metric. Capture the login again.

### Depositing a key by hand

A credential with no `ck auth login` flow can be stored with `ck auth put`
under the family the provider reads. The Muse CLI may keep its device token in the
operating system's credential store (the Keychain on macOS, Credential Manager on
Windows, a secret service on Linux), which insula does not read. So `muse` is
served from a hand deposit under `apikey:muse`, or from the environment variable
or file below.

The Vercel personal budget needs an access token and the team whose budget it
reads, so its deposit under `apikey:vercel-budget` is a JSON object,
`{"token": "<access token>", "team_id": "<team id or slug>"}`. The AI Gateway
key for the team's credit balance is a separate deposit under `apikey:vercel`.
They are separate families, never one id with an account suffix: each
credential is sent only to its own host.

## Local logins and API keys

Without a vault record, most providers fall back to what is already on the
machine. How a provider with both lanes combines them (the vault record
replacing the local one, or both serving the same account) is
provider-specific; [`provider-matrix.md`](provider-matrix.md) and the
provider's source state it.

| Provider | Local file | Environment |
|---|---|---|
| `abliteration` | | `ABLITERATION_API_KEY` |
| `alibaba` | | `ALIBABA_CODING_PLAN_API_KEY`, `ALIBABA_QWEN_API_KEY`, `DASHSCOPE_API_KEY` |
| `antigravity` | the running editor or `agy` CLI (loopback); the opencode antigravity-auth plugin's sidebar file | `ANTIGRAVITY_AUTH_SIDEBAR_STATE_FILE` |
| `chatbox` | | `CHATBOX_LICENSE_KEY` |
| `claude` | OpenCode's `anthropic` OAuth entry | |
| `clinepass` | | `CLINE_API_KEY`, `CLINEPASS_API_KEY` |
| `codebuff` | `~/.config/manicode/credentials.json` | `CODEBUFF_API_KEY` |
| `codex` | `~/.codex/auth.json` | |
| `copilot` | | `COPILOT_API_TOKEN`, `GITHUB_TOKEN`, `GH_TOKEN` |
| `cursor` | Cursor's `state.vscdb` | |
| `deepseek` | OpenCode's `deepseek` entry | `DEEPSEEK_API_KEY`, `DEEPSEEK_TOKEN` |
| `doubao` | | Volcengine access key settings; `ARK_API_KEY`, `VOLCENGINE_API_KEY`, `DOUBAO_API_KEY` |
| `elevenlabs` | | `ELEVENLABS_API_KEY`, `XI_API_KEY` |
| `grok` | OpenCode's `xai` OAuth entry | |
| `jetbrains` | the IDE's `AIAssistantQuotaManager2.xml` and `idea.log` | |
| `kilo` | `~/.local/share/kilo/auth.json` | `KILO_API_KEY` |
| `kimi` | | `KIMI_AUTH_TOKEN` |
| `kimi-for-coding` | | `KIMI_CODE_API_KEY` |
| `llmproxy` | | `LLM_PROXY_API_KEY`, `LLM_PROXY_BASE_URL` |
| `manus` | | `MANUS_SESSION_TOKEN`, `MANUS_SESSION_ID`, `MANUS_COOKIE` |
| `minimax` | | `MINIMAX_CODING_API_KEY`, `MINIMAX_API_KEY` |
| `muse` | `MUSE_AUTH_PATH` or `~/.config/muse/auth.json` | `MUSE_DEVICE_TOKEN` |
| `neuralwatt` | | `NEURALWATT_API_KEY` |
| `opencodego` | | `OPENCODE_API_KEY` |
| `openrouter` | OpenCode's `openrouter` entry | |
| `sakana` | | `SAKANA_COOKIE` |
| `soniox` | | `SONIOX_ORG_API_KEY` (an organization key; `SONIOX_API_KEY`, a project key, is not read) |
| `stepfun` | | `STEPFUN_TOKEN` |
| `sub2api` | | `SUB2API_API_KEY`, `SUB2API_BASE_URL` |
| `synthetic` | OpenCode's `synthetic` entry | `SYNTHETIC_API_KEY` |
| `vercel` | | `AI_GATEWAY_API_KEY` |
| `vercel-budget` | | `VERCEL_ACCESS_TOKEN` with `VERCEL_TEAM_ID`; the token alone is `credential_unusable` |
| `warp` | | `WARP_API_KEY`, `WARP_TOKEN` |
| `zai` | | `Z_AI_API_KEY` |
| `zenmux` | | `ZENMUX_MANAGEMENT_API_KEY` |

OpenCode entries are read from the `credential` table in OpenCode 2's
`~/.local/share/opencode/opencode.db` and from OpenCode 1's `auth.json` beside it
(`$XDG_DATA_HOME/opencode/` when set). The database is opened read-only, in place;
it is never copied or written. Rows explicitly marked inactive (`active = 0`)
are excluded; NULL is live. For each provider's integration ID, the newest
`time_updated` row is considered. When both stores have OAuth credentials, the
later `expires` wins. If either expiry is missing, or for API keys, the database
row's `time_updated` is compared with `auth.json`'s modification time. An
unavailable database is reported as a local source problem, not an absent
credential; a valid `auth.json` entry can still be used, with a warning logged
once. Tokens are never refreshed, because refresh-token rotation would disrupt
OpenCode's session. An expired-looking access token is still tried: its recorded
expiry need not match the server's actual validity window.

The cookie providers `amp`, `factory`, `langdock`, `mimo`, `opencode`, `qoder`,
`qwen-cloud` and `traecn` have no local lane: without a vault deposit they are not
configured. Endpoint and region overrides are listed in
[`provider-matrix.md`](provider-matrix.md).

Environment variables are read from the module's own environment, which is the
daemon's environment plus the module entry's `env` in `subc.jsonc`. A variable
exported in an interactive shell does not reach a module the daemon started
earlier; restart the module after changing it (`ck module restart insula`).

## Checking that an account is served

```sh
ck quota                                             # what consumers see
python3 scripts/health.py scopedCredentialIds vaultHandleState vaultEnumerationFailure
cargo run -p quota-module --example vault-lanes      # every vault credential against what is served
```

- `scopedCredentialIds` lists the vault records insula installed on its last
  successful listing. A record missing here was not granted to insula.
- `vaultHandleState` is `answered` once a listing has succeeded. `awaiting`
  that persists means the vault keeps failing to list; read
  `vaultEnumerationFailure`.
- A provider listed under `degraded` in health has a credential that is
  failing; its entry's `errorClass` says why
  (see [the error classes](consumer-contract.md#not-every-degraded-entry-is-a-problem)).

## Removing an account

Delete the vault record, or remove the local file or environment variable.
On the next successful vault listing, insula removes the slots for records that
are no longer granted. A consumer that reconciles stored accounts on
`completeProviders` sees the account gone on the provider's next complete
answer.
