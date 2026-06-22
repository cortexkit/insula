# ai-provider-quota

A subc-supervised module that knows every AI provider's usage limits and reset
windows — the headless engine that replaces the external **CodexBar** dependency.

Alfonso's model router needs to know how much quota each provider has left and when
each window resets, so it can route around exhaustion (the "provider is in a quota
cooldown" decisions). Today Alfonso polls a running `codexbar serve --port 8087`
binary for this. This module replicates that capability natively and serves it
**through subc** — so Alfonso connects to subc (not an external binary) for its
quota signal.

See `docs/charter.md` for the full mission, the reverse-engineered contracts, the
locked decisions, the multi-repo split, and the Phase 0 charter.

## Status

Greenfield. Owning agent makes the first commit (this scaffold is uncommitted so the
module's history is yours from line one).
