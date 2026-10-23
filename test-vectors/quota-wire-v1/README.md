# Quota wire v1

`usage-get.json` is the exact compact response body from the module's `usage.get` handler, driven by stub providers through the real Registry. It includes fresh 5h and weekly windows, a retained stale reading and fetchedAt, an exhausted (100%) window, subscription-funded and bought pools with expiry, saved reset expiry, a degraded account with no reading, and named extra rate windows. Amount edges include an overdrawn DeepSeek remaining balance (-2 minor, exponent 2, USD) and a large Codex balance (55598241861500 minor, exponent 9). All identities are synthetic example.com addresses; all clocks are fixed.

`usage-watch-read-now.json` is the exact first StreamData body produced by the real watch handler: sequence 1 tells the consumer to read now, with no row identity. The epoch is a made-up UUID fixed for reproducibility.

`usage-watch-row-change.json` is the next body from the same watch, after the real sample observer sees fresh@example.com's primary usage change. Sequence 2 names that row; notices carry invalidations, not quota snapshots.

`README.md` is generated alongside the bodies. String fields are open vocabularies, not closed enums; consumers must tolerate unfamiliar values. See [the consumer contract](../../docs/consumer-contract.md) for semantics and reconciliation rules.

`SHA256SUMS` covers every other file in this directory in `shasum -a 256` format. JSON files contain the frame body bytes without a trailing newline. Verify with `shasum -a 256 -c SHA256SUMS` from this directory. Regenerate all files with `INSULA_WRITE_VECTORS=1 cargo test -p quota-module --bin ck-insula committed_quota_wire_vectors_match`. The named drift test checks every file and the checksums byte-for-byte.
