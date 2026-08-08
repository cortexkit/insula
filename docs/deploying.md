# Deploying

This module is a long-lived binary supervised by the subc daemon. **Merging is
not deploying.** A fix that is on `master` and CI-green is still not running,
and nothing in the repository will tell you otherwise — the running process
keeps serving the binary it was started with until it is replaced deliberately.

That gap is the reason for the build stamp described below: it makes the
question "is the deployed build current?" answerable from the running process
rather than inferred.

## The procedure

```sh
# 1. build
cargo build --release

# 2. back up what is running, then replace it
cp ~/.local/share/cortexkit/bin/ck-insula ~/.local/share/cortexkit/bin/ck-insula.bak
cp target/release/ck-insula ~/.local/share/cortexkit/bin/ck-insula

# 3. restart under supervision
ck module restart ai-provider-quota

# 4. verify the running build is the one you just installed
ck module status ai-provider-quota --json    # health.metrics.buildCommit
git rev-parse --short=12 HEAD                # must match

# 5. confirm every configured credential is actually being served
cargo run -p quota-module --example vault-lanes

# 6. check what it now publishes is internally coherent
cargo run -p quota-module --example deployed-sanity
```

The binary is `ck-insula` and the module id is `ai-provider-quota`. They differ
because the repository was renamed and the module id was deliberately left
alone: every other repository that calls this module names it by that id, and
such a reference does not block a rename — it breaks when one lands. So the
restart and status commands keep the old spelling while the artifact does not.

Step 5 exists because steps 4 and 6 cannot see an absent lane. Both check that
what is published is self-consistent, and a set that shrinks stays consistent —
so a credential that stopped being served leaves them green. `vault-lanes`
compares what this host is configured for against what the wire is serving,
which is a relation neither side derives from the other.

Step 5 answers a different question from step 4. The stamp says the right *code*
is running; the sanity sweep says the *data* that code publishes does not
contradict itself — a percent outside 0..=100, a reset further out than its
window is long, counts that disagree with their own percent. Those are
well-formed values that parse cleanly, so nothing else catches them, and one such
defect reached production and was found by a person reading the output.

It exits non-zero both when a check fails and when it could not examine anything,
so it can gate the deploy rather than merely inform it.

Use the module-side example, not `quota-core`'s `wire-sanity`. The latter builds
its own registry with no credential-vault client, so it cannot see the lane that
serves most of the labelled accounts here — on this host it examines 16 windows
against the deployed module's 30. A clean result from it is not a statement about
what production publishes.

## Do not kill the process

Use `ck module restart`. A bare `kill` has left the module in supervision state
`failed` with **no respawn**, and because a supervised module dying looks much
like a supervised module restarting, it stayed down unnoticed. The daemon's
drain-restart path stops and starts it as a unit; killing it only performs the
first half and relies on the daemon choosing to do the second.

`ck module status ai-provider-quota` shows the supervision state. If the entry
is missing entirely, the daemon has not read the config that declares it — `ck
module rescan` re-reads `subc.jsonc`.

## Running the ignored tests

Several tests are `#[ignore]` because they need something the machine may not
have: real credentials, an IDE's configuration on disk, or the sibling daemon
binary. They are the only checks that exercise a live credential end to end, so
nothing else can tell you a lane has stopped working.

```sh
cargo test --workspace --all-targets --no-fail-fast -- --ignored
```

**`--no-fail-fast` is load-bearing.** Without it the run stops at the first
failing test binary and every later one is skipped, reported only as a count you
have to notice is short. A first failure hides the rest, which is how two
unrelated failures here were found one at a time rather than together.

Expect some of these to fail for reasons that are not defects: a machine with no
JetBrains subscription, a credential deliberately not configured. Read the
failure before treating it as one. Two things do make them worth running
regularly:

- **A live credential can die without any other check noticing**, particularly
  where the upstream answers a dead credential ambiguously. Nothing in the unit
  suite or the deployed checkers contacts an upstream with a real token.
- **An ignored test does not fail when the code around it changes**, so it drifts
  silently and the drift surfaces only when someone runs it. One here asserted a
  set of error classes that a later refactor had replaced, and it had been red
  from the moment of that commit.

## Verifying the deploy

`health.metrics.buildCommit` is stamped at compile time from the git HEAD of the
tree the binary was built in, and is reported by the running module. Comparing
it against `git rev-parse --short=12 HEAD` answers exactly whether the running
binary contains the commits you believe it does.

Timestamps and inodes are not substitutes. Neither says what a binary
*contains*: a stale build copied into place has a fresh mtime and a fresh inode,
and both look exactly like a successful deploy. The stamp is generated by the
thing being measured rather than by the thing doing the measuring, so a stale
reader can fail to ask, but cannot report a stale answer as current.

One limit of the reading path, as opposed to the stamp itself: `ck module
status` reports the health record the supervisor last collected, not a fresh
probe issued for your question. Immediately after a restart that record can
still be the one from before it, so the stamp you read is the *previous*
binary's — which looks exactly like a deploy that did not take, and invites a
second pointless deploy.

The reply dates itself, so this is checkable rather than a matter of waiting:

```sh
ck module status ai-provider-quota --json    # module.last_probe_ms, epoch ms
```

If that timestamp predates the restart, the health block beside it describes the
old process and the stamp in it proves nothing yet. Re-read once it advances.

Note the field sits under `module`, not under `health` — next to the stamp it
dates, but not inside it.

### Deciding whether a deploy is needed at all

Most commits here touch only tests, comments, or documentation, and rebuilding
for those is wasted work. Reading the diff to decide is a heuristic, and an
unreliable one: Rust arranges test code in several ways — an inline `mod tests`,
a whole file included by `#[cfg(test)] mod tests;`, a `tests/` directory — and a
rule that counts lines against the first `#[cfg(test)]` marker silently reports
whichever arrangement it does not model as a runtime change.

Ask the compiler instead. `CK_QUOTA_BUILD_COMMIT_OVERRIDE` pins the stamp, so
two commits can be built into byte-comparable binaries:

```sh
# From the SAME directory, so embedded absolute paths match.
git stash                       # or use a clean tree
git checkout <deployed-commit>
CK_QUOTA_BUILD_COMMIT_OVERRIDE=000000000000 cargo build --release -p quota-module
shasum -a 256 target/release/ck-insula

git checkout <head-commit>
CK_QUOTA_BUILD_COMMIT_OVERRIDE=000000000000 cargo build --release -p quota-module
shasum -a 256 target/release/ck-insula
```

**Equal hashes mean no deploy is needed** — the commits produce the same binary.

**Unequal hashes prove nothing on their own.** Panic messages embed their own
file and line, so adding a comment above a function in a runtime file shifts
every line below it and changes the binary without changing behaviour. On a
difference, read the diff.

Two conditions are easy to get wrong. Both builds must run from the same
directory, because absolute paths are embedded and a worktree elsewhere yields
different bytes for identical source. And both commits must contain the override
support, since an older `build.rs` ignores the variable and stamps its own sha —
making the stamp itself the difference being measured. Overlay the current
`build.rs` onto the older commit if needed.

This cannot be used against the *deployed* binary directly unless it was built
in the same directory, which is the usual case here but worth confirming.

**When it is worth the rebuild.** The comparison costs a full release build per
side, so it earns its keep only where the cheap signal is genuinely ambiguous:
a SMALL gap that also touches FEW runtime files. A gap of one or two commits
over a handful of files is exactly the case where the diff might be comments —
and exactly where a rebuild is cheaper than being wrong.

Elapsed time alone is the wrong thing to sort on. The smallest gaps often carry
the LARGEST file counts, because a small gap usually means a seat shipped work
recently. A large gap over few files is not ambiguous either: something real
landed and nobody rebuilt. Both extremes answer themselves, and the escalation
is for the middle.

### `unknown` is a third outcome, not a failed comparison

The stamp is `unknown` when the build could not resolve `HEAD` — most often
because the tree is not a git checkout at all. It never guesses, because a wrong
commit would make a stale deploy look current, which is the one failure the
stamp exists to prevent.

That safety has a cost worth naming: **`unknown` and a mismatched sha mean
opposite things and only one of them is about the deploy.** A mismatch says the
running binary is old. `unknown` says nothing about the binary at all — it says
the instrument is unavailable, and the deploy may be perfectly current. Reading
it as "not deployed" produces a redeploy that changes nothing and appears not to
work.

So treat it as its own branch: do not redeploy, find out why the stamp could not
resolve, and verify the deploy another way in the meantime — the module's start
time moving after a restart at least confirms the new binary was executed, even
though it cannot say what that binary contains.

This matters more than the mismatch case because it is quiet. A wrong sha is
noticed the moment anyone compares it; an instrument that answers "I don't know"
keeps answering, and nothing about the output says it has stopped measuring.

**The stamp reads `HEAD`, not the working tree, so it says which commit the
build started from and not what was compiled.** A build from a dirty tree
carries the clean `HEAD` sha verbatim, and the comparison above then passes for
a binary containing code that exists in no commit. Confirm the tree was clean
when the release binary was built:

```sh
git status --porcelain    # empty before you build the binary you intend to ship
```

The stamp answers *is this build missing commits*. It cannot answer *does this
build contain anything extra*, and a matching sha is not evidence that it does
not.

## Renaming the module id

The subc module id is `ai-provider-quota`, and every other repository that calls
this module names it by that id in a constant. Those are *target* references:
they do not block a rename, they break when one lands. So callers ship **with**
the change, and a caller missed is a dead quota route.

Inside this repository the id has one definition, in
`crates/quota-module/src/ids.rs`, which the integration tests include by path
rather than restating. The local edit is a single line.

What the suite can and cannot tell you about that line:

- **The unit tests cannot verify the value.** Both sides read the same
  definition, so any id passes — setting it to something else leaves the suite
  green. They check that the pair moves together, which is a different property.
- **`real_daemon_e2e` can.** It writes a `subc.jsonc` naming the id and has a
  real `ck-subc` binary spawn and supervise this module from it, so the id must
  round-trip through daemon config, process launch, and a routed `usage.get`.
  It is `#[ignore]` because it builds the sibling daemon, so it needs running
  explicitly:

  ```sh
  cargo test -p quota-module --test real_daemon_e2e -- --ignored
  ```

Rehearse by making the edit, running that test, and restoring the file. That
confirms supervision works under the new id before any caller is touched.

To enumerate the callers, run `scripts/module_id_callers.py`. It separates
routes from trees that cannot receive an edit, and refuses to report a clean
result when it finds no routes at all.

## When a deploy is not needed

Commits touching only tests or documentation change no runtime behaviour, and
`buildCommit` will legitimately lag `HEAD` after them, so a difference is not
by itself a pending deploy.

To decide, use the pinned-stamp hash comparison above. Do not filter the file
list by name: which paths hold test code is exactly the judgement that section
explains is unreliable here, and a filter that misses one arrangement reports a
runtime change as tests. Reading the diff is for understanding *what* changed
once the hashes already say *whether* it matters.

If you do glance at the file list first, read it — do not gate on it. A pipeline
like `git diff --name-only … | grep -v tests.rs` returns **grep's** exit status,
and grep exits non-zero when nothing matches, so "no files changed" and "the
command failed" are the same status. Any check whose exit code you intend to act
on must not be read through a pipe.

## Reading CI evidence for a specific commit

This workflow sets `cancel-in-progress`, and pushes here often come in bursts, so
a run being killed by the next push is routine — **30% of a recent 40-run sample
was cancelled**. A cancelled run reports `status: completed` with
`conclusion: cancelled`, which reads as closure to anything scanning for a
terminal state.

That matters when someone asks whether a particular change is verified, including
a downstream consumer pinning evidence to a producer commit. Two rules:

**Assert the step, not the run.** A run can be cancelled, and a run can conclude
green with a step skipped by a conditional. The step list is the evidence and the
run status is a summary of it:

```
gh run view <run-id> --json jobs \
  | python3 -c "import json,sys; [print(s['conclusion'], s['name']) for j in json.load(sys.stdin)['jobs'] for s in j['steps']]"
```

**Ask for a run that contains the change, not the run at it.** Cancellation loses
verification *at a commit*, never verification *of the content* — the next
completed run covers everything beneath it. In that same sample, four cancelled
runs carried production Rust and all four were ancestors of a later green run.
So walk forward to the first completed run whose head contains the change:
`git merge-base --is-ancestor <change> <run-head>`.

The stricter reading — "the run at the commit that introduced it" — is the
natural one and would report a third of this repository's history as unverified.
