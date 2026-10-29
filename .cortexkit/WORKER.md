# Working in insula

Standing rules for every worker in this repository. A brief can tighten them but never loosen
them. Start with `CONTRIBUTING.md`, `docs/architecture.md` and `docs/consumer-contract.md`.
This repository is public: no personal identities, account ids or internal module names in
fixtures, comments or commit messages.

## Build and test remotely

- Run builds, tests and lint on the build server: `runon: "linux,8c"`, or `linux,16c` for
  whole-workspace jobs. Use `runon: "windows"` when you touch paths, file I/O, process spawning
  or `cfg(windows)` code; Windows has been insula's most common CI-only failure.
- On this Mac, run only what needs macOS, at `nice -n 10`.
- Never run `real_daemon_e2e` against the installed daemon yourself; the integrating agent runs
  it once per merged batch. Say in your delivery whether your change needs it.

## Gate before you deliver

- Run `scripts/prepush.sh`: fast local checks, then the Linux and Windows CI legs in parallel
  on the build servers. `--fast` alone is not a delivery gate; `--linux-only` omits Windows.
  The optional `--e2e --sibling ../subconscious` uses a built sibling daemon, never the installed
  daemon. Filtered tests alone are not a gate.
- A bug fix adds a test and shows it failing once without the fix (`scripts/probe.py`).
- `cargo test -p quota-core --lib -- --list`: the total must equal the unique count. A doc comment
  inserted between a neighbour's doc comment and its `#[test]` silently unregisters that test.
- Report each command, its pass and fail counts, and every failing test name. If a failure isn't
  yours, show it fails the same way on your base commit.
- Commit only after `git config user.email` prints `94532+ualtinok@users.noreply.github.com`, with
  `git commit -F - <<'MSG'`. Backticks in `-m` are eaten by the shell.

## Pre-approved, so don't ask

- Updating a test your intended change breaks, keeping what the test checks.
- Mechanical compile fixes outside your fence: a new field initialised to `None` in other
  struct literals, a new match arm handled explicitly and covered by a test.
- Updating `docs/consumer-contract.md`, `docs/provider-matrix.md` and `docs/credentials.md` for
  behaviour your change adds.
- Adding the one upstream host your provider calls to `scripts/endpoint-hosts.py`, with the
  constant name that holds it. Never rename or derive a URL to get past that check.
- Fixtures taken from CodexBar's own test bodies at a tagged release (`git show <tag>:<path>`
  in `~/Work/OSS/CodexBar`), with the tag and path cited.

## Never

- Deploy, restart the module, or touch `~/.config/cortexkit/ck-quota.jsonc` or the live Codex
  redemption journal. The integrating agent deploys.
- Read the macOS Keychain, a browser's cookie store, or another program's credential store.
  insula reads the vault, provider-owned local files, and environment variables only.
- Map "couldn't look" to `credential_absent`. An unreadable source is `local_source_unavailable`
  or transient; `credential_absent` lets consumers prune accounts.
- Loosen an assertion, raise a timeout or add a retry to get green. Find the cause.
- Change `test-vectors/` without saying so in your delivery: the phone app pins those bytes by
  tag.
- Force-add ignored `.cortexkit/` files, or put task or slice labels in code comments.

## Ask when

- A field on the wire changes, appears or disappears, or an error class mapping changes.
- Anything could spend money or credit (Codex saved resets).
- A credential family is added to or removed from the vault routing table.
- The brief and the upstream source disagree. Say which and what you'd do.
