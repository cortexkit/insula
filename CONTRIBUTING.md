# Contributing

## The gate

Every change passes `scripts/gates.sh` before it lands:

```sh
scripts/gates.sh            # repository checks, fmt, clippy, unit tests, doctests
scripts/gates.sh --e2e      # also builds ck-insula and runs skeleton_e2e
```

It stops at the first failure and exits `0` when everything passed, `1` when a
gate failed and `2` when it could not check. It runs, in order:

1. repository path dependencies (`scripts/path-dependencies.py`, Python 3.11+);
2. planted violations for every source scanner (`scripts/mutation-scans.sh`);
3. a compile of every Python instrument and a parse of every shell script;
4. the endpoint host manifest (`scripts/endpoint-hosts.py`);
5. the CI preconditions a train depends on (`scripts/check-train-preconditions.sh`);
6. the public `.cortexkit/` layout (`scripts/check-cortexkit-layout.sh`);
7. `cargo fmt --check`;
8. `cargo clippy --workspace --all-targets -- -D warnings`, and again on the
   newest stable toolchain when the local one is older, because CI lints with
   the newest;
9. `cargo test --workspace --lib --bins` with `TMPDIR` pointed at an empty
   folder that must still be empty afterwards;
10. `cargo test --workspace --doc`;
11. with `--e2e`, `cargo build -p quota-module --bins` and
    `cargo test -p quota-module --test it skeleton_e2e::`;
12. an orphan-module check: every `src/*.rs` must be declared by a `mod` in its
    crate root.

If the gates rewrite `Cargo.lock` they say so; commit that as its own
dependency change or restore it. [`docs/verifying.md`](docs/verifying.md)
covers what the gate does not run.

## Committing

Write commit messages with a quoted heredoc so backticked identifiers survive:

```sh
git commit -F - <<'MSG'
provider: one-line summary

What changed and why, in plain sentences. `identifiers` stay intact because
the quoted delimiter stops the shell from expanding them.
MSG
```

With `git commit -m` and a double-quoted message, the shell runs each
backtick-quoted word as a command and leaves a gap where the identifier was;
git exits 0 and nothing else notices.

## Landing

`master` is protected: it accepts a push only when the commit being pushed
has passing `Test (ubuntu)` and `Test (windows)` checks. Maintainers land with
`scripts/train.sh`:

```sh
scripts/train.sh <name>
```

It refuses a dirty tree, refuses while an earlier train has not landed (a
second train would move master's tip off the commit the first one is waiting
on), and refuses a commit message that looks like it lost a backticked
identifier. It then pushes `HEAD` to `train/<name>`, waits for the CI run on
that commit, confirms every check branch protection requires has passed, and
pushes the same commit to `master`. On failure it leaves the train branch in
place for inspection and exits non-zero.

Outside contributors open a pull request; CI runs the same jobs on it.

## Where code goes

### Adding a provider

1. Create `crates/quota-core/src/<provider>.rs` (hyphens become underscores)
   implementing `UsageProvider` from `provider.rs`: `name()`, `handles()` (local
   enumeration only, never network I/O) and `fetch_handle()`. Map every failure
   to a `FetchError` variant; the variant decides the published `errorClass`
   and whether the refresher treats it as transient.
2. Declare it with `pub mod <provider>;` in `crates/quota-core/src/lib.rs` and
   add it to `Registry::with_defaults`.
3. If it reads a vault credential, add its id family to `CREDENTIAL_FAMILIES`
   in `vault_handles.rs`. A cookie provider returns `true` from
   `is_cookie_based()`, reads its deposit through `cookie_vault.rs`, and needs a
   row in the cookie-capture table in
   [`docs/consumer-contract.md`](docs/consumer-contract.md);
   `cookie_capture_urls_match_the_providers_fetch_urls` checks that row against
   the source.
4. If the provider has a canonical models.dev slug, add it to
   `api_provider_name` in `lib.rs`.
5. Record each endpoint constant's host in `scripts/endpoint-hosts.py`.
6. Add the provider to [`docs/provider-matrix.md`](docs/provider-matrix.md) and
   update its `N providers registered.` line;
   `the_documented_provider_count_matches_the_registry` compares it with the
   registry.
7. Test the normalizer against fixtures in the module's own `#[cfg(test)]`
   block. A test that needs a real session goes in
   `crates/quota-core/tests/it/<provider>_live.rs` and is `#[ignore]`d.

### Adding a helper

Shared code lives in `quota-core` beside its callers' concerns: HTTP requests,
error excerpts and URL stripping in `http.rs`; environment and credential-file
reads in `env.rs`; money amounts in `money.rs`; text and JSON scanning in
`text.rs` and `json_scan.rs`; credential-vault access in `credential_source.rs`
and `vault_handles.rs`. A helper used by one provider stays in that provider's
file. Code that only the subc module needs (frame loop, vault client, health
report) goes in `crates/quota-module/src/`.

### Adding a test

- Registry and cross-provider behaviour: `crates/quota-core/src/tests.rs`.
- One provider's parsing: that provider's `#[cfg(test)]` block.
- The subc wire, health report and config loading: the `tests` module in
  `crates/quota-module/src/main.rs`.
- A spawned `ck-insula` against an in-process daemon:
  `crates/quota-module/tests/it/skeleton_e2e.rs`.
- A test that creates files uses `crate::tests::ResetTempDir`, which removes
  its folder on drop; the gate fails when a unit test leaves anything in
  `TMPDIR`.

A test that guards a costly property (money spent, exactly-once, credential
trust, a wire field, data loss) should also get a row in `mutations.toml`; see
[`docs/mutations.md`](docs/mutations.md).
