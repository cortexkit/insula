# Quota wire v2: explicit Codex saved reset

Synthetic identities only: account `synthetic-codex-id`, email `phone@example.com`,
principal `reserved:callosum`, harness `fed-phone:synthetic-fp`. No credentials.
The fixed clock is 2026-07-14T12:00:00Z; the blocking reservation is 12 minutes older.

`use-saved-reset-request.json` is the routed request envelope; its `params` is the
operation request body. Success files are Response frame bodies, error files are
Error frame bodies. `recently_reset` includes RFC 3339 `detail.last_reset_at`.
`provider_refused` includes the bounded (200-character) provider excerpt in `detail.excerpt`.
The before/after files are individual rows from `usage.get`, with availableCount
changing from 2 to 1 after the due slot is refreshed. The operation reply does not
wait for that refresh. A repeated request_id replays its durable outcome; a new id
is not a retry. Only callosum's attested fed-phone: routes may spend a credit.

Generated in process by the Rust wire tests, including SHA256SUMS. Regenerate with:
`INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_v2_vectors_match`

v1 remains unchanged. Consumers may verify SHA256SUMS with `shasum -a 256 -c SHA256SUMS`.
