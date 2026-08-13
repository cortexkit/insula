"""Mutate each wire_sanity rule to a no-op, one at a time, and report which
tests redden.

A rule whose deletion leaves the suite green is undefended. The point of doing
this mechanically rather than by reading is that a rule beside a well-tested
neighbour reads as covered.

Assumes the working tree is clean and everything is committed: each round
restores with `git checkout --`, which is only correct when the index holds the
state to return to.

READ THE SKIPPED LINES. A mutation that does not compile is reported rather than
hidden, because a skip and a pass look identical in a summary and the skip is the
more interesting result: this sweep rewrites `if <cond> {` guards, so a rule
living in a `match` arm is untouched by construction. The first run here reported
one skip, and that rule turned out to be the only undefended one in the file --
the sweep could not test exactly the rule that no test covered. A tool that
reports "16 of 17 defended" without naming the seventeenth invites the reader to
round up.
"""

import os
import re
import subprocess
import sys
from pathlib import Path

REPO = Path("/Users/ufukaltinok/Work/Projects/CortexKit/insula")

# Seconds a single mutated run may take before it is treated as a hang. Sized
# well above a healthy run of this suite (about two seconds) so a slow machine
# is not misread as a hang; override to a small value to exercise the hang path.
MUTATION_TIMEOUT = int(os.environ.get("MUTATION_TIMEOUT", "120"))
TARGET = REPO / "crates/quota-core/src/wire_sanity.rs"
REL = "crates/quota-core/src/wire_sanity.rs"

# The restore below is `git checkout -- <path>`, which returns the file to what
# the INDEX holds. That is the state wanted only if the index was made
# authoritative BEFORE any mutation: staging a mutated file makes the mutation
# the reference, and every restore afterwards faithfully restores the break.
#
# So refuse to start unless the target is clean against the index. A dirty target
# means either uncommitted work the first restore would delete, or a staged
# mutation from an interrupted run that every restore would reinstate. Both are
# silent -- the file looks restored, the tree looks clean, and the sweep reports
# on code that is not the code.
# Checked before anything else, because every guard below silently agrees that a
# missing file is fine: `git status` reports nothing for a path that does not
# exist, so the cleanliness check passes, and the read then raises a bare
# FileNotFoundError with no cause and no remedy. The sweep does stop, which is
# the right outcome reached by the wrong route -- it reads as the script being
# broken rather than as its target having moved.
if not TARGET.exists():
    print(
        f"  REFUSING TO RUN: {REL} does not exist. This sweep audits the rules in "
        f"that one file; if it moved, point TARGET and REL at its new location.",
        file=sys.stderr,
    )
    sys.exit(2)

status = subprocess.run(
    ["git", "status", "--porcelain", "--", REL],
    cwd=REPO,
    capture_output=True,
    text=True,
    check=True,
# NOT .strip(): porcelain marks staged-versus-unstaged in the first TWO columns,
# and the unstaged case is a LEADING SPACE. Stripping it makes the two states
# identical -- which would leave this guard unable to tell apart the very
# distinction its message turns on.
).stdout.rstrip("\n")
if status:
    # The remedy differs by which kind of dirt this is, and telling the reader
    # to "stage it" is actively wrong for the staged case -- staging is what put
    # them here. Unstaged work would be destroyed by the first restore; staged
    # work becomes the version every restore returns to, so a mutation staged
    # mid-run is audited forever while the tree looks clean.
    staged = status[:1] not in (" ", "?")
    remedy = (
        "Commit it, or reset the staged copy if it is a mutation from an "
        "interrupted run -- staging is what makes the index the reference, so "
        "anything staged while mutated is the version every restore returns to."
        if staged
        else "Commit or stage it first -- this sweep restores from the index "
        "after every mutation, so uncommitted work is destroyed by the first "
        "round."
    )
    print(f"  REFUSING TO RUN: {REL} is not clean ({status!r}). {remedy}", file=sys.stderr)
    sys.exit(2)

original = TARGET.read_text()
lines = original.splitlines(keepends=True)

# Cross-check the population against an independent count before doing anything
# with it. This sweep enumerates by matching `findings.push`, and a rule that
# reaches the report another way -- pushed onto a differently-named binding, or
# extended from a helper -- is invisible to that match. The sweep would then
# audit a subset and report it as the whole, which is the exact failure it
# exists to detect, committed by the detector.
#
# The independent signal is the finding message formats, since every rule that
# can fire must have text to fire with. Counting a different artefact is the
# point: a miscount shared by both would defeat this.
#
# The message pattern accepts more than one leading placeholder because a
# cross-entry rule names the provider and account separately rather than through
# the single prefix an entry-level rule uses. Matching only the common form made
# the counts disagree by one and stopped the sweep -- which is the refusal
# working, on the sweep's own pattern rather than on the file.
#
# COMPARED BY LINE, NOT BY COUNT. Equal totals imply the same set only while
# every rule has exactly one message and no other string carries the prefix --
# a property of today's file, not of the check. A rule rewritten to push a bare
# string while some unrelated literal gains a placeholder keeps both totals
# identical and silently removes a rule from the population, which is the exact
# failure this guard exists to catch.
PUSH_PATTERN = re.compile(r"findings\.push")
MESSAGE_PATTERN = re.compile(r'"(?:\{[a-z_]+\}[:/])+')

# Paired inside each push's OWN statement, not merely on a nearby line. A line
# can hold a bare push and an unrelated placeholder string, which pairs by
# proximity while the rule itself has become unenumerable.
def push_statements(text):
    """Yield (line number, source) for each `findings.push(...)` statement."""
    for match in PUSH_PATTERN.finditer(text):
        depth, end = 0, None
        for i, ch in enumerate(text[match.start():], start=match.start()):
            if ch == "(":
                depth += 1
            elif ch == ")":
                depth -= 1
                if depth == 0:
                    end = i + 1
                    break
        if end is None:
            end = len(text)
        yield text[: match.start()].count("\n") + 1, text[match.start():end]


statements = list(push_statements(original))
silent_pushes = sorted(line for line, body in statements if not MESSAGE_PATTERN.search(body))

# The reverse direction: a placeholder-shaped string that belongs to no push is
# either a rule reaching the report another way, or a decoy the sweep would
# miscount.
claimed = "".join(body for _, body in statements)
stray_messages = len(MESSAGE_PATTERN.findall(original)) - len(MESSAGE_PATTERN.findall(claimed))

if silent_pushes or stray_messages:
    print(
        f"  REFUSING TO RUN: the two enumerations disagree. "
        f"findings.push carrying no finding message at lines {silent_pushes}; "
        f"{stray_messages} finding-shaped message(s) belonging to no push. "
        f"A rule this sweep cannot see would be reported as absent rather than "
        f"unchecked -- fix the patterns, not the file.",
        file=sys.stderr,
    )
    sys.exit(2)

# Agreement is not enough on its own, because two derivations can be emptied
# together by a change upstream of both -- renaming the vector rules push onto,
# say -- and they then agree perfectly at zero. That is the most reassuring
# possible way to report having checked nothing, so an empty population is
# refused outright rather than swept.
if not statements:
    print(
        "  REFUSING TO RUN: no rules found at all. Both readings agree, and they "
        "agree about nothing -- something upstream of both patterns changed, so "
        "a clean sweep below would describe an empty population.",
        file=sys.stderr,
    )
    sys.exit(2)
print(f"  population agrees: {len(statements)} rules, each carrying its own message")

# Each rule is an `if <cond> {` whose body pushes a finding. Find the guard line
# for every findings.push by walking back to the nearest enclosing `if`.
push_lines = [i for i, l in enumerate(lines) if "findings.push" in l]
guards = []
unmatched = []
for p in push_lines:
    for j in range(p, max(p - 12, -1), -1):
        stripped = lines[j].strip()
        if re.match(r"^if .*\{$", stripped) or re.match(r"^\} else if .*\{$", stripped):
            if j not in guards:
                guards.append(j)
            break
    else:
        # No single-line `if` above this finding: the rule is in a match arm, or
        # behind a guard whose condition spans several lines. Reported rather
        # than dropped -- a finding this sweep cannot reach is the one most
        # likely to be untested, since the same irregular shape defeats casual
        # reading too.
        unmatched.append(p + 1)

print(f"  rules with a mutable guard: {len(guards)} (of {len(push_lines)} findings sites)")
if unmatched:
    print(f"  NOT REACHABLE by this sweep, check by hand: lines {unmatched}")
print()

def neutralise_and_run(idx):
    """Disable the guard at `idx`, run the suite, restore, and report.

    Returns (compiled, failed_test_names). Restoring comes from the index, which
    is only correct when the tree is staged or committed -- see the module
    docstring.
    """
    guard = lines[idx]
    indent = guard[: len(guard) - len(guard.lstrip())]
    mutated = lines[:]
    mutated[idx] = f"{indent}if false {{\n"
    TARGET.write_text("".join(mutated))

    # A mutated run has THREE outcomes, not two. Beyond red and green, a
    # mutation can make the suite HANG -- a guard whose removal turns a bounded
    # retry into an unbounded one produces no verdict at all. Without a cap the
    # sweep blocks forever, reports nothing about any site after this one, and
    # leaves the file mutated if it is killed by hand.
    #
    # A timeout gets its own verdict rather than being folded into not-reached,
    # because the two mean opposite things: a hang says the condition changes
    # control flow, a compile failure says the rewrite was malformed.
    #
    # But HUNG is not purely a control-flow signal, and reading it as one
    # overstates what happened. The budget covers a rebuild as well as the run,
    # so a mutation landing on a cold cache can exhaust it while the suite would
    # have finished. Raise MUTATION_TIMEOUT and re-run before concluding a rule
    # changes control flow: a verdict that moves when only the budget moved was
    # never about the code.
    #
    # Do NOT kill this script mid-sweep. It restores after each mutation, so an
    # interrupted run leaves the file rewritten -- recoverable, because the next
    # run refuses a dirty tree, but only if the next run happens.
    try:
        run = subprocess.run(
            ["cargo", "test", "-p", "quota-core", "--lib", "wire_sanity"],
            cwd=REPO,
            capture_output=True,
            text=True,
            timeout=MUTATION_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        subprocess.run(["git", "checkout", "--", REL], cwd=REPO, check=True)
        return None, []
    failed = re.findall(r"^test (\S+) \.\.\. FAILED", run.stdout, re.M)
    # "Did the suite run at all" is the unambiguous question, and it has to be
    # asked FIRST. Asking "did it compile" first means matching on error text,
    # and cargo prints `error: test failed` for a test that built and correctly
    # failed -- so every genuinely defended rule would be misreported as skipped.
    # A summary line is printed only when the binary ran.
    compiled = re.search(r"^test result:", run.stdout, re.M) is not None

    subprocess.run(["git", "checkout", "--", REL], cwd=REPO, check=True)
    return compiled, failed


# POSITIVE CONTROL, before any verdict below is trusted.
#
# "Every rule defended" is produced by a sweep that works AND by a sweep that has
# lost the ability to detect -- a rewrite that stops applying, a verdict pattern
# that stops matching after a refactor. One observable, two states, which is the
# fault this script hunts, arriving in the script.
#
# So neutralise a rule whose defence is known and require the run to notice. If
# it does not, the instrument cannot detect and every result below is
# meaningless, so it refuses rather than reports.
#
# It terminates the regress rather than adding another layer needing its own
# audit, because it FAILS CLOSED: a broken self-test does not pass its own check,
# and the script exits.
if not guards:
    # Distinct from the empty-population refusal above: rules WERE found, and
    # none of them is shaped so this sweep can neutralise it. The remedy is the
    # rewrite pattern, not the enumeration.
    print(
        f"  REFUSING TO RUN: {len(statements)} rules found, but none has a guard "
        f"this sweep can neutralise -- every one is a shape the `if <cond> {{` "
        f"rewrite cannot reach, so there is no liveness probe to run.",
        file=sys.stderr,
    )
    sys.exit(2)

probe = guards[0]
probe_compiled, probe_failed = neutralise_and_run(probe)
if probe_compiled is None:
    print(
        f"  REFUSING TO RUN: neutralising the rule at line {probe + 1} made the "
        f"suite hang. A sweep that cannot tell a hang from a verdict cannot be "
        f"trusted, and every result below would be one of the two.",
        file=sys.stderr,
    )
    sys.exit(2)
if not (probe_compiled and probe_failed):
    print(
        f"  REFUSING TO RUN: neutralising the rule at line {probe + 1} did not "
        f"redden any test. The sweep cannot detect, so a clean result below "
        f"would mean nothing.",
        file=sys.stderr,
    )
    sys.exit(2)
print(f"  instrument proven live: neutralising line {probe + 1} reddened {probe_failed[0]}")
print()

undefended = []
hung = []
for idx in guards:
    compiled, failed = neutralise_and_run(idx)
    label = lines[idx].strip()[:64]

    if compiled is None:
        # A fourth outcome, distinct from defended, undefended, and NOT
        # REACHED. Removing this guard changed control flow enough that the
        # suite never finished -- so the guard is load-bearing -- but no test
        # ever reported on it, which leaves open whether anything asserts what
        # it does.
        print(f"  line {idx + 1}: HUNG, inspect by hand: {label}")
        hung.append(idx + 1)
    elif not compiled:
        # The guard binds something its body uses (`if let Some(x) = …`), so
        # replacing it with `if false` leaves that name unbound. The rule is
        # NOT REACHED by this sweep and has to be mutated by hand -- and it is
        # the likeliest place for a gap, since the same irregular shape that
        # defeats this rewrite also defeats casual reading.
        print(f"  line {idx + 1}: NOT REACHED, mutate by hand: {label}")
    elif failed:
        print(f"  line {idx + 1}: defended by {len(failed)} test(s) -> {failed[0]}")
    else:
        print(f"  line {idx + 1}: *** UNDEFENDED *** {label}")
        undefended.append(idx + 1)

print()
if TARGET.read_text() != original:
    print("  !! file not restored cleanly", file=sys.stderr)
    sys.exit(2)
if hung:
    print(f"  rules whose removal hung the suite: {hung}")
print(f"  restored byte-identical; undefended rules: {undefended or 'none'}")
