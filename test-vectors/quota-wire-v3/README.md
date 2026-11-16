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
once resolved, a repeated request_id replays the stored outcome. Every success
outcome (reset, already_redeemed, nothing_to_reset, no_credit) includes redeemed_at:
the original decision time in canonical RFC 3339 UTC (nanoseconds and +00:00),
matching last_reset_at and cooldown_until. The resolution clock for success-reset.json
is fixed at 2026-07-14T12:00:00Z. success-replay.json replays an id resolved on
2026-07-01T12:00:05Z, almost two weeks before the fresh result, without contacting
Anthropic. Requests use the test process's current UTC clock; their send time is
not a request-body field.
Clients compare redeemed_at with their send time to distinguish an earlier result
from a reset decided for the current press. Old resolved records without resolved_at
use their last_attempt_at as the closest recorded decision time.

Generated in process by the Rust wire tests, including SHA256SUMS. Regenerate with:
`INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_v3_vectors_match`

v1 and v2 remain as they are. Consumers may verify SHA256SUMS with `shasum -a 256 -c SHA256SUMS`.
