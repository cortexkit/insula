# quota module — charter & handoff

> **This is the founding brief, kept as a record of what was decided and why.**
> It describes the module as it was being planned, so its interface sketches and
> counts have been overtaken — most visibly, the consumer contract is now the
> subc `usage.get` route rather than an HTTP `/usage` endpoint, and the entry
> shape carries several fields this document does not list.
>
> For what the module does today, read `../README.md`,
> `provider-invariants.md` (properties a provider must uphold), and
> `consumer-contract.md` (what a consumer may rely on — the authoritative
> statement of the output shape). This file is retained for its rationale, not
> as a current description.

This spec was reverse-engineered from the real code on both sides — the provider
fetchers being replicated, and the consumer that would read the result — rather
than designed from a blank page.

## Mission

Serve per-provider quota from a native **subc module** rather than an external binary, exposing
per-provider usage/quota windows. The model router consumes this to route around
provider exhaustion. Today the router polls `codexbar serve --port 8087` over HTTP; after this
lands, **the router connects to subc** and reaches this module over the subc wire — no external
binary, no HTTP coexistence (clean cutover).

## The two contracts (reverse-engineered — verify against source yourself)

### A. What the reference implementation's `serve` exposes
Source: [CodexBar](https://github.com/steipete/CodexBar) (Swift). The serve entry is
`Sources/CodexBarCLI/CLIServeCommand.swift`; the per-provider fetchers are
`Sources/CodexBarCore/Providers/*`. The serve API (loopback `127.0.0.1`, 60s response cache):
- `GET /health` → `{ "status": "ok" }`
- `GET /usage?provider=<id>` → per-provider array (THE endpoint the router uses)
- `GET /cost?provider=<id>` → cost payloads (Claude/Codex only; the router does NOT use this)

The hard part, and what makes this more than an HTTP client: each provider reuses its OWN existing session
to fetch usage — OAuth tokens, browser cookies, API keys, or local app files — and normalizes
to uniform JSON. That bespoke per-provider auth+fetch+normalize is the real engineering.

### B. What the router actually consumes (your real requirement)
Source: `~/Work/Projects/CortexKit/alfonso/src/features/model-routing/quota/`
(`codexbar-quota-source.ts`, `codexbar-window-extractors.ts`). The router:
- calls **`/usage` only** (never `/cost`),
- expects an array of `{ provider, account, source, usage: { primary, secondary, tertiary } }`
  where each window is a **RateWindow**: `{ usedPercent/utilization, resetsAt, windowMinutes }`,
- maps CodexBar provider names → its own provider ids (today only 6:
  `codex→openai`, `claude→anthropic`, `grok→xai`, `antigravity→google`,
  `opencodego→opencode-go`, `ollama→ollama-cloud` — but see scope below),
- is **silent-degrade**: a failed/missing signal is simply "no signal", never an error.
So the load-bearing output is **RateWindows per provider/account** (utilization + resetsAt +
windowMinutes). Get that shape right and the router's existing pace/pressure projection just works.

## Scope: the full surveyed provider set, day 1

Not just the router's current 6. The authoritative provider list is the directories under
`Sources/CodexBarCore/Providers/` (~46: Abacus, Alibaba, Amp, Antigravity, Augment, AzureOpenAI,
Bedrock, Claude, Codebuff, Codex, CommandCode, Copilot, Crof, Cursor, Deepgram, DeepSeek, Doubao,
ElevenLabs, Factory, Gemini, Grok, Groq, JetBrains, Kilo, Kimi, KimiK2, Kiro, LLMProxy, Manus,
MiMo, MiniMax, Mistral, Moonshot, Ollama, OpenAI, OpenCode, OpenCodeGo, OpenRouter, Perplexity,
StepFun, Synthetic, Venice, VertexAI, Warp, Windsurf, Zai). Enumerate the real list yourself;
each is a separate fetcher port (auth source + endpoint + window normalization). This is large
and parallelizable — once the pattern is proven for one provider, the rest fan out cleanly.

## Locked decisions

1. **subc-native cutover, no HTTP coexistence.** The router connects to subc as a consumer, opens
   a route to this module, requests usage over the wire. CodexBar's HTTP server is fully retired
   for the router — do not keep a codexbar-compatible HTTP shim as a coexistence path.
2. **First user of the TS subc-client.** The router is TypeScript (bun); it reaches subc via
   `@cortexkit/subc-client` (the HMAC handshake + connection-file reader + frame codec in TS).
   **That client lives in the `subconscious` monorepo and is owned by the subc maintainer,
   NOT in this repo.** This build is the moment it finally gets built, with quota as its first
   real consumer. Coordinate its contract with the subc maintainer.
3. **This module is a Rust subc module** (manifest + route channel + control ops), same family as
   AFT / subc-mcp. It is NOT a ToolProvider — decide its provider role (likely an
   InternalService/ManagementSurface that answers a "get usage windows" request). Bodies stay
   opaque to subc; subc just routes.

## Multi-repo coordination (3 repos, you drive the quota one)

- **this repo (`insula`)** — the Rust subc module + the per-provider usage fetchers. YOU own it.
- **`subconscious`** (the subc maintainer) — provides `@cortexkit/subc-client` (TS). Coordinate the
  client API + the module's subc contract (route op, request/response shape) with the subc maintainer.
- **the router's repo** — repoints `codexbar-quota-source.ts` from HTTP to the subc-client.
  Coordinate the cutover with that peer; the consumer adapter was already designed for this
  ("Later, the same adapter repoints to our own desktop app" — its own comment).

## Phase 0 charter

1. **Study the fetchers**: enumerate the surveyed providers and, per provider, capture {auth source
   (OAuth/cookie/api-key/local-file), usage endpoint, response→RateWindow normalization}. Produce a
   provider matrix doc. Reuse existing sessions exactly as CodexBar does — e.g. OpenCode/Anthropic
   OAuth lives at `~/.local/share/opencode/auth.json`.
2. **Design the subc contract**: what route op does the router call to get usage, what's the
   request/response shape, the provider role, caching/refresh semantics (CodexBar uses a 60s TTL),
   silent-degrade behavior. Co-design with the subc side + the TS subc-client API.
3. **Walking skeleton (the exit gate)**: prove ONE provider end-to-end — a consumer →
   `@cortexkit/subc-client` → subc → this module → a REAL provider usage fetch → RateWindow back —
   from real provider state, not a stub. Pick a provider that's actually burned (anthropic/claude or
   openai/codex). Verify from the real returned window, not an exit code.

## Reference

- [CodexBar](https://github.com/steipete/CodexBar) (Swift) — the reference
  implementation these normalizers are checked against.
  - serve entry: `Sources/CodexBarCLI/CLIServeCommand.swift`
  - providers: `Sources/CodexBarCore/Providers/*`
- [subc](https://github.com/cortexkit/subconscious) — the supervisor this module
  runs under, and where the TypeScript client lives.
