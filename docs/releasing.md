# Releasing insula

The public changelog is [GitHub Releases](https://github.com/cortexkit/insula/releases).
A release is an annotated `vX.Y.Z` tag on a landed, CI-tested commit, six binary
ZIPs with SHA256 sidecars, and `release-manifest.json`. The release index at
`https://cortexkit.io/releases/v1/index.json` advertises the version and its
`requires_core` floor to installers.

## Prepare and release

1. Choose a version greater than every previous release: patches for fixes,
   minors for new providers, new wire fields or behaviour changes. Before 1.0,
   call out a breaking wire change in a minor release's upgrade notes.
2. Review the commits since the last tag. Write and commit the notes for the
   tag in `.cortexkit/release-notes/`, named after the tag, following the
   [format](../.cortexkit/release-notes/FORMAT.md). The
   [0.2.0 notes](../.cortexkit/release-notes/v0.2.0.md) are a worked example.
   Review the prose before tagging; commit titles are not release notes.
3. Check `release-manifest.json` names the minimum public core this release
   supports. The command tests that exact version, not today's latest core.
4. Land all preparation. From clean `master`, equal to freshly fetched
   `origin/master`, run:

   ```sh
   scripts/release.sh X.Y.Z --dry-run
   scripts/release.sh X.Y.Z
   ```

The command requires Bash 3.2+, Git, Python 3.11+, Cargo, curl, and an authenticated
GitHub CLI (`gh`) with permission to push and read Actions. The command selects an
installed Python with `tomllib` support, including versioned Python commands.
Cargo's local cache
must support an offline workspace lock refresh. Run host compilation at reduced
priority on macOS (`nice -n 10 scripts/release.sh X.Y.Z`). No installed daemon is
used, restarted or deployed by the release command.

The dry run makes every preflight check and runs the public-core floor test,
but changes no versions, commits, branches, tags or releases. Notes must already
be committed: an untracked notes file is a dirty checkout, not a dry-run waiver.

## What the command does

- Fetches `origin/master` and tags; refuses a dirty checkout, a branch other than
  `master`, any difference from `origin/master`, a non-`X.Y.Z` version (including
  leading zeroes), an existing tag, a version not greater than the latest release,
  or missing/empty release notes. Every refusal exits 2 and names its reason.
- Downloads core from GitHub release `subc-core-v<requires_core>` in
  `cortexkit/subconscious`, asset `ck-subc-<os>-<arch>.zip` and its `.sha256`
  sidecar. The index lists only the latest core, so it cannot locate an older
  floor. The command verifies the exact ZIP bytes before extracting the binary.
- In a real run, bumps the two crate versions and runs
  `cargo update --workspace --offline`. It refuses any lock change beyond those
  workspace versions. It then runs
  `CK_SUBC_BIN=<downloaded-path> cargo test -p quota-module --test it real_daemon_e2e -- --ignored`.
  The test harness executes disposable development-named copies of both binaries
  with isolated configuration, never the installed daemon.
- Commits only the two crate manifests and `Cargo.lock`, then invokes
  `scripts/train.sh release-X.Y.Z`. The train runs its fast prepush gates, waits
  for CI and lands the tested SHA. The release command verifies that exact SHA
  is `origin/master` before creating and pushing the annotated tag.
- Waits for the tag's `release.yml` push run on that SHA, then polls the release
  index every 30 seconds for up to 15 minutes. Success means both
  `components.insula.version` and `components.insula.requires_core` match.

The workflow's first job independently refuses publication without a successful
`ci.yml` push run on the exact commit, from `master` or a `train/` branch, with
every required check green in that same run. Scheduled, manual and pull-request
runs do not authorize a release. `.github/required-checks.txt` is the fallback
when the Actions token cannot read branch protection; keep it aligned with the
protection rule. When protection is readable the gate enforces both sets.
Build and publish jobs depend on this gate.

## Platforms and failure recovery

Assets are named `ck-insula-<os>-<arch>.zip` with `.zip.sha256` sidecars:
darwin-arm64, darwin-x64, linux-x64, linux-arm64, windows-x64 and windows-arm64.
Both macOS builds use the same Developer ID signing and notarization script
before checksumming. Native targets run `--version` on their runner; Linux ARM64
uses `cross` on x64 Linux, so it cannot run its binary natively there.

A pre-tag failure publishes no tag or release. A failure after a bump may leave
the bump dirty or committed locally, and a failed train leaves its branch for
inspection. Inspect that state before deciding how to recover; the command does
not reset changes or resume an existing bump automatically.

After tag creation, a failure reports the landed SHA, tag push state, workflow
state, index confirmation and the GitHub release/assets it can observe. An API
failure is reported as unknown, never as proof that nothing was published.
Workflow success with index timeout means GitHub publication succeeded but
installer discoverability has not been confirmed. Inspect the workflow and
index before any further action. Never move, force-push or replace a published
tag; fix forward with a new version if the published artifacts are wrong.
