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

# 2. back up what is running, then replace it BY RENAME, never by overwrite.
#    `cp` onto the live path writes new bytes into the EXISTING inode, and macOS
#    caches code-signature validation per vnode -- so the next exec finds bytes
#    that do not match the cached verdict and the kernel SIGKILLs it. Silently:
#    no output, no error, exit 137, and `ck module status` keeps reporting `ok`
#    from the process that is still running the OLD image. That is a deploy that
#    did not happen while every surface says it did; the only tell is
#    `last_exit: sig9` in the supervisor row.
cp ~/.local/share/cortexkit/bin/ck-insula ~/.local/share/cortexkit/bin/ck-insula.bak
cp target/release/ck-insula ~/.local/share/cortexkit/bin/ck-insula.new
mv -f ~/.local/share/cortexkit/bin/ck-insula.new ~/.local/share/cortexkit/bin/ck-insula

# 2b. prove the file you just placed can EXECUTE before restarting anything.
#     One second, and it is the difference between finding this now and finding
#     it in step 4 with a supervisor that has already fallen back.
~/.local/share/cortexkit/bin/ck-insula --version    # must print, must exit 0

# 3. restart under supervision
ck module restart insula

# 4. verify the running build is the one you just installed
ck module status insula --json    # health.metrics.buildCommit
git rev-parse --short=12 HEAD                # must match

# 5. confirm every configured credential is actually being served
cargo run -p quota-module --example vault-lanes

# 6. check what it now publishes is internally coherent
cargo run -p quota-module --example deployed-sanity
```

**If the whole browser-cookie cohort fails at once, restart before diagnosing.**
Nine providers failing in one tick is a machine-level fact, not nine upstreams.
On macOS the usual cause is a lost disk-access grant — an OS upgrade resets them —
and the module reports it as `local_source_unavailable` naming the remedy.

The trap is that granting access does **not** fix a running module: macOS binds
the decision to a process at launch, so a fresh shell can read the profile while
the module still cannot. `ck module restart insula` is what applies it, and until
that restart the wire is not evidence the grant failed. The grant is per process
tree too, so a shell reading the file proves nothing about this binary — the
daemon spawns it under a different subject.

**Run the checkers through `cargo run`, never from `target/release/examples/`.**
`cargo build --release` does **not** build examples, so that directory can hold a
checker from an earlier commit — and running it after a deploy silently validates
the *old* contract against the *new* module. The failure is self-confirming: a
stale checker that predates a wire field simply never mentions it, and a missing
line reads as a quiet host rather than as a check that is not in the binary.
A consumer hit exactly this while verifying `usage.drops` (insula#5): their
checker was a day older than the module and printed `findings: none`.

`cargo run` rebuilds the example first, which is why every command here uses it.
If you want the binaries on disk, `cargo build --release --examples` is the form
that actually produces them.

The binary, the module id and this repository are all `insula` — but three paths
on disk are still named `ck-quota` and are **not** leftovers: the redemption
journal, the quota config, and the vault handle file. Each comes from its own
hardcoded literal, none is derived from the binary or the module id, and each is
load-bearing. Renaming any of them is a migration, not a tidy: see the note in
`crates/quota-module/Cargo.toml` for what the journal one costs.

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

## Before claiming a gate result

`cargo clippy --workspace --all-targets -- -D warnings` reporting `0` means
cargo found nothing to redo, which is not the same as nothing being wrong. This
workspace path-depends on `../subconscious`, so that repo can change while
nothing in this tree does — and then a cached clean answer is reported as a gate
result while CI, which always builds cold, fails on a compile error. That
happened on 2026-08-16 and put a compile break on master.

Run `python3 scripts/sibling-freshness.py` first. It exits 1 when a sibling
repository's HEAD is newer than this workspace's newest build artifact, and
prints the forced-recompile command. It does not check whether the sibling
change breaks anything — it cannot, and the honest answer is "your cache may
predate a change", not "you are broken".

Related trap, same family: `cargo test --test <name>` does NOT rebuild the
binaries an integration test spawns. A stale `ck-insula` fails registration with
no error output at all, which reads as a hang rather than a build problem. Build
the bins before running the e2e suites.

### Absorbing a lock wave: do NOT reach for a hash comparison here

This is the step where the retired pinned-stamp check gets reinvented, and it was
reinvented HERE on 2026-09-04 by the author of the section that retires it -- see
"The pinned-stamp hash comparison is BROKEN" below, which already names every
reason it cannot work AND already says not to substitute the lock digest.

The pull is specific to this step. A wave notice says "no code change", the
question "then does my binary change?" is exactly the right one to ask, and a
hash comparison looks like the way to answer it. It is not: two embedded stamps
move on their own. `CK_QUOTA_PROVENANCE_SHA` differs between a dirty tree and a
clean one -- and swapping `Cargo.lock` to compare makes one build dirty and the
other clean, so the two arms differ by construction before any dependency is
considered. `build_lock_digest` is sha256 of `Cargo.lock`, so it differs whenever
the lock differs, which is the premise of the comparison.

SO A LOCK BUMP ALWAYS CHANGES THE BINARY, trivially and by construction, and that
fact carries no information about whether behaviour changed.

The question a lock wave actually poses is whether anything that SHIPS moved:

    cargo tree -p quota-module -e normal | grep <crate>   # zero edges = it does not ship
    git diff <deployed>..HEAD -- crates/                  # did our own runtime code move

A dev-only bump with zero normal edges warrants a deploy for stamp currency, not
for behaviour -- and saying which is the whole point.


## Do not kill the process

## Use the `ck` on PATH, not a repo build

Every command here says plain `ck`, which resolves through
`~/.local/bin/ck` to the DEPLOYED CLI. That is deliberate and worth stating,
because a freshly built `subconscious/target/release/ck` sits right there and
looks like the better choice.

It is not. The house rule pairs the deployed CLI with the RUNNING DAEMON'S
commit, and a repo build tracks the repo instead. Both directions hurt:

| repo build is | what you see | what it looks like |
| --- | --- | --- |
| ahead of the daemon | skew warning on every call; new verbs refused | a broken daemon |
| behind the daemon | verb missing entirely | a daemon that lacks the feature |

The second one bit on 2026-08-26. `./target/release/ck provenance insula` returned
`unknown domain`, which reads as "the daemon does not serve this" and was actually
"this binary predates the verb". The discrimination is a one-line ancestor check
against the repo the CLI is built from:

```sh
git -C ../subconscious merge-base --is-ancestor <commit-that-added-it> HEAD
```

Ancestor means the source is present and only the artifact is stale: a REBUILD.
Not an ancestor means the source is genuinely absent: a RELEASE. Those have very
different costs and the error message cannot tell them apart.

Confirming which binary you are on:

```sh
readlink $(which ck)          # -> ~/.local/share/cortexkit/bin/ck when correct
shasum -a256 $(which ck) ../subconscious/target/release/ck   # differ = not paired
```

Use `ck module restart`. A bare `kill` has left the module in supervision state
`failed` with **no respawn**, and because a supervised module dying looks much
like a supervised module restarting, it stayed down unnoticed. The daemon's
drain-restart path stops and starts it as a unit; killing it only performs the
first half and relies on the daemon choosing to do the second.

`ck module status insula` shows the supervision state. If the entry
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
ck module status insula --json    # module.last_probe_ms, epoch ms
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

**DO NOT USE THE HASH COMPARISON THAT WAS HERE.** It compared two builds made
with `CK_QUOTA_BUILD_COMMIT_OVERRIDE` pinned, and it cannot work: there are TWO
git-derived stamps and the override covers one. `CK_QUOTA_PROVENANCE_SHA` embeds
the real HEAD sha and deliberately resists override, so two builds at different
commits always differ whether or not any runtime code changed. Full measurement
and the tell that exposed it are at the end of this file.

Ask the SOURCE instead, which answers the question directly:

```sh
git diff --name-only <deployed-commit>..HEAD -- crates/
```

Read the FILE LIST first. A change confined to `tests/` or `examples/` is not
linked into `ck-insula` and cannot reach the deployed binary, so no deploy is
needed however many lines moved.

If runtime files did change, count the non-comment added lines — with an
instrument you have checked. A failed `grep` prints `0`, and a zero from a
failed command is not a measurement.


**Unequal hashes prove nothing on their own.** Panic messages embed their own
file and line, so adding a comment above a function in a runtime file shifts
every line below it and changes the binary without changing behaviour. On a
difference, read the diff.

**Record the finding with what would falsify it, not the conclusion alone.** The
result of this comparison is almost always reported to someone who cannot re-run
it — a summary, a commit message, a peer. "Byte-identical" and "no deploy
needed" carry nothing a reader can check, and the second is an inference rather
than a measurement. Write what was observed and why it is consistent with no
behaviour change: *same size, differing hash, diff is test-only*. That form
carries its own refutation — a differing SIZE, or a diff touching a runtime
line, contradicts it on sight.

The reason to insist on it is that the measurement is usually right and the
report is what drifts. This exact comparison was run correctly here and then
summarised an hour later as "byte-identical to HEAD's", which was the opposite
of the recorded numbers. Nothing in that sentence looked uncertain, and the
next reader's cost is skipping a check on the strength of it.

Two conditions are easy to get wrong. Both builds must run from the same
directory, because absolute paths are embedded and a worktree elsewhere yields
different bytes for identical source. That is not a small effect and it is
silent: the same commit built in a sibling worktree and in the repository
produced two different hashes here, which reads exactly like a real difference
and would send someone to deploy a binary identical to the running one. And both commits must contain the override
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

The subc module id is `insula`, and every other repository that calls this module
names it by that id in a constant. Those are *target* references: they do not
block a rename, they break when one lands. So callers ship **with** the change,
and a caller missed is a dead quota route.

Inside this repository the id has one definition, in
`crates/quota-module/src/ids.rs`, which the integration tests include by path
rather than restating. The local edit is a single line.

**The running process does not read that constant.** The daemon injects
`SUBC_MODULE_ID` when it spawns a module and that wins, so the id a supervised
process announces comes from the daemon's config, not from this binary. Two
consequences, and the second is the one that bites:

- A daemon-side rename needs **no rebuild and no new binary**. Verify which id a
  live process is actually using with `ps eww <pid> | grep SUBC_MODULE_ID`
  rather than reading it out of the source.
- The constant should **follow** that flip, never lead it, so the fallback cannot
  disagree with a live config if the flip is delayed.

And the flip is a **restart**, even though it needs no rebuild: the daemon
respawns the module rather than editing a running process's environment. Compare
the pid before and after to see it. That matters because the banked-resets config
is read once at startup (see `Registry::with_defaults`), so a restart is exactly
when it can silently stop being read — an unreadable config leaves the feature
off and the module healthy. Check it from the wire afterwards, per the step-5
note above.

### The checkers go dark during the window

`vault-lanes` and `deployed-sanity` dial this same constant to reach the daemon,
so between a daemon-side flip and this constant catching up, both fail with
`module <old id> did not appear in catalog` and exit 101. They are not only
defined by this id, they are *addressed* by it.

That is the acceptance check going down with the thing it checks. Survivable
because `ck module status <id>` and the daemon's own catalog answer the same
questions by an independent route — use those during the window, and treat the
examples as available again only once the constant matches.

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

## A new wire field is not shipped until every consumer knows

Deploying a field and documenting it is not the same as telling the people who
read it. The contract file is authoritative and nobody re-reads it on a
schedule, so a consumer learns about a field either because someone told them or
because they trip over it.

The failure is asymmetric in a way that hides it: an additive field breaks
nothing, so no consumer reports a problem and the producer sees a clean deploy.
What actually happens is that the field sits unread for months while the
consumer keeps doing the thing it was added to replace.

Enumerate the consumers rather than the ones involved in the conversation that
produced the change. There are four surfaces reading `usage.get` today: the
router that paces on it, the metering store that persists it, the CLI renderer,
and whoever filed the request. The first two are the easiest to skip, because
neither is in the room when a rendering or contract question is being settled —
and they are the two that act on the data programmatically.

Say what changes for each of them specifically. A consumer told "there is a new
field" checks whether it breaks them and moves on; one told "this is the
decision it puts in front of you" makes the decision. The metering store's
question here was whether a preserved reading is a second observation of the
same value or not a new observation at all, which is a modelling choice only
they can make and which the field exists to let them make.

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

## A pinned-hash comparison needs its own control

Building two commits with the stamp overridden and comparing hashes answers "did
the code change", but only if the build is reproducible — and nothing about the
comparison tells you whether it is. Build the SAME commit twice, with a forced
recompile between, before reading anything into a difference. Without that
control a nondeterministic build reports every pair as different and the
conclusion is "always redeploy", which is indistinguishable from a correct
answer.

**And a differing hash is not by itself a code change.** Two commits here
differed only by two `cfg` attributes that both evaluate TRUE on the building
platform, so the compiled behaviour provably could not differ — and the binaries
had identical SIZE, each reproducible, with different hashes. Source structure
reaches the output through symbol mangling and metadata without reaching
behaviour.

So read the pair together: identical size with a differing hash and no
behavioural delta in the diff is a metadata difference. A real code change moves
the size. When the diff is small enough to read, read it — it settles the
question faster than the hash does, and the hash cannot distinguish these two
cases at all.

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

**A cancellation between two failures is not evidence of anything, and it is
where this reading goes wrong.** The guidance above trains the eye to treat
`cancelled` as noise, which is correct in isolation and dangerous in a column: a
list reading failure, cancelled, cancelled, failure looks like one bad run
surrounded by churn, and is actually a red that has never gone green. Cancelled
means the run was killed before it could answer, so it removes evidence rather
than supplying it — it can never be the reason a neighbouring failure is stale.

This is not hypothetical. A Windows compile break here survived three commits
because the failures were separated by cancellations and the whole column read
as burst noise. **Read the most recent CONCLUSIVE run, and treat every
cancellation as a gap rather than a data point.** A single failure with no later
success beneath it is a red branch however many cancellations sit between.

**Ask for a run that contains the change, not the run at it.** Cancellation loses
verification *at a commit*, never verification *of the content* — the next
completed run covers everything beneath it. In that same sample, four cancelled
runs carried production Rust and all four were ancestors of a later green run.
So walk forward to the first completed run whose head contains the change:
`git merge-base --is-ancestor <change> <run-head>`.

The stricter reading — "the run at the commit that introduced it" — is the
natural one and would report a third of this repository's history as unverified.

## `deployed-sanity` was RED on purpose, and CLEARED ITSELF on 2026-08-30

RESOLVED. The expected finding count is ZERO again. If the checker reports
anything, it is real — do not read the section below as licence to expect one.

For roughly a day it reported one finding and exited 1:

    the daemon holds no self_signals for this module

That was TRUE and never a defect in this module. The running daemon predated
`subc-protocol` 0.14.0, which introduced the manifest field, so it had no such
field to store and serde discarded the block at HELLO. Registration succeeded,
health read ok, `buildCommit` matched — the declaration was genuinely in our
binary and genuinely on the wire, and the daemon kept none of it. The daemon
carrying it was placed on 2026-08-30 and the checker went green with no edit
here.

KEPT AS A WORKED EXAMPLE, because two things about it generalise.

A CHECKER LEFT DELIBERATELY RED NEEDS ITS EXPECTED COUNT WRITTEN DOWN, and then
needs that number RETIRED the moment the condition clears. The note that said
"expect one finding, a second is real" was correct for a day and would have been
actively harmful the day after: a reader following it would see one finding, tick
the box, and miss the real one. A stale expected-count is worse than no note,
because it converts a working alarm into a silenced one and looks like diligence
while doing it. If you ever write "expect N findings" here again, write the
condition that retires it in the same edit.

AND THE CLEAN CLEAR IS THE PROOF THE CHECK WAS SOUND. It fired while the
condition held, and stopped when the condition lifted, with no change to the
checker in between. That is the discrimination property this repo demands of
every rule, obtained for free from a real state transition rather than from a
synthetic fixture — which is the strongest form of it available.

## The pinned-stamp hash comparison is BROKEN and must not be used

This runbook has prescribed proving a binary unchanged by building both commits
with `CK_QUOTA_BUILD_COMMIT_OVERRIDE` set to a fixed value and comparing hashes.
That check cannot work any more, and has not since the provenance stamps landed.

THERE ARE TWO GIT-DERIVED STAMPS AND THE OVERRIDE COVERS ONE:

    CK_QUOTA_BUILD_COMMIT     overridable  -- what the procedure pins
    CK_QUOTA_PROVENANCE_SHA   NOT overridable, deliberately -- it embeds the real
                              HEAD sha when the tree is clean, and refuses to
                              state one otherwise

So two builds at different commits ALWAYS differ, whether or not a line of
runtime code changed. Measured 2026-08-30:

    build at deployed commit, clean tree   69cdaa72bdd84638
    build at HEAD, clean tree              15db8a59ebd08eaa
    two bisect builds, dirty tree          f93469fbe20d308f  -- BOTH of them

The two dirty builds agreeing is the tell: they carried DIFFERENT source and
produced identical binaries, because a dirty tree makes the provenance stamp
refuse to state a commit, so both embedded the same non-answer. Different source,
same hash; same source, different hash. The instrument reports the stamp, not the
code.

That the provenance stamp resists override is CORRECT — its whole value is that a
build cannot claim a commit it was not built from. The defect is in the procedure
that assumed one override covered every embedded git value, which was true when
it was written and stopped being true without anything failing.

USE THIS INSTEAD. The question is "did any runtime code change", and source
answers it directly:

    git diff <deployed>..HEAD -- crates/

Read the changed FILE LIST first: a change confined to `tests/` or `examples/` is
not linked into `ck-insula` and cannot reach the deployed binary. If runtime
files did change, count the non-comment added lines — and count them with an
instrument you have checked, since a failed `grep` prints `0` and a zero from a
failed command is not a measurement.

If a hash comparison is ever wanted again, it needs BOTH stamps neutralised, and
neutralising the provenance one would defeat the reason it exists.

AND DO NOT SUBSTITUTE THE LOCK DIGEST FOR IT. `build_lock_digest` hashes
`Cargo.lock`, so it moves whenever ANY dependency version moves — including a
DEV-only one that never links into `ck-insula`. Measured 2026-08-30: a
`subc-core` bump changed the digest while `cargo tree -p quota-module -e normal`
showed zero normal-dependency edges to it, so the runtime binary was unaffected.
A reader treating a digest difference as "this binary is stale" would redeploy
for a test-only dependency.

The digest answers "did the lock change", which is the question it was added for
(spotting a missed wave). It does not answer "did the binary change", and the two
diverge on exactly the dependencies that never ship.
