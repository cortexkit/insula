# Quota wire v3: Claude saved resets by kind

Synthetic identities only: account `synthetic-claude-account`, principal
`reserved:callosum`, harness `fed-phone:synthetic-fp`. No credentials. Anthropic is a
local stand-in serving three grants: a session reset, a weekly reset and a weekly-model
(`model_weekly`) grant holding two resets.

`usage-get-row-saved-resets.json` is the `savedResets` member of the Claude row from
`usage.get`: `kinds` groups the counted resets by what they restore, and the kinds'
`availableCount` values add up to the row's. Grant ids and labels never appear.

`use-saved-reset-request.json` is the routed request envelope; its `params` is the
operation request body. A Claude spend names `kind` (one of session, weekly, full,
model_weekly); `error-invalid_request.json` is the reply to the same request without
it. The remaining files answer the same request id in turn while Anthropic answers
`cooldown` (`recently_reset` with `detail.cooldown_until`), then `ineligible` with
reason `tier` (`provider_refused` with `detail.result` and `detail.reason`), then
`reset`. A refused attempt leaves the request id pending, so its retry is sent again;
once resolved, a repeated request_id replays the stored outcome.

Generated in process by the Rust wire tests, including SHA256SUMS. Regenerate with:
`INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_v3_vectors_match`

v1 and v2 remain as they are. Consumers may verify SHA256SUMS with `shasum -a 256 -c SHA256SUMS`.
