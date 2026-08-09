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

import re
import subprocess
import sys
from pathlib import Path

REPO = Path("/Users/ufukaltinok/Work/Projects/CortexKit/insula")
TARGET = REPO / "crates/quota-core/src/wire_sanity.rs"
REL = "crates/quota-core/src/wire_sanity.rs"

original = TARGET.read_text()
lines = original.splitlines(keepends=True)

# Cross-check the population against an independent count before doing anything
# with it. This sweep enumerates by matching `findings.push`, and a rule that
# reaches the report another way -- pushed onto a differently-named binding, or
# extended from a helper -- is invisible to that match. The sweep would then
# audit a subset and report it as the whole, which is the exact failure it
# exists to detect, committed by the detector.
#
# The independent signal is the number of finding message formats, since every
# rule that can fire must have text to fire with. Counting a different artefact
# is the point: a miscount shared by both would defeat this.
#
# The message pattern accepts more than one leading placeholder because a
# cross-entry rule names the provider and account separately rather than through
# the single prefix an entry-level rule uses. Matching only the common form made
# the counts disagree by one and stopped the sweep -- which is the refusal
# working, on the sweep's own pattern rather than on the file.
PUSH_PATTERN = re.compile(r"findings\.push")
MESSAGE_PATTERN = re.compile(r'"(?:\{[a-z_]+\}[:/])+')

pushes = len(PUSH_PATTERN.findall(original))
message_sites = len(MESSAGE_PATTERN.findall(original))

if message_sites != pushes:
    print(
        f"  REFUSING TO RUN: {pushes} findings.push sites but {message_sites} "
        f"finding messages. The enumeration is incomplete, so a clean result "
        f"would describe a subset. Fix the sweep, not the count.",
        file=sys.stderr,
    )
    sys.exit(2)
print(f"  population agrees: {pushes} rules by two independent counts")

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

undefended = []
for idx in guards:
    guard = lines[idx]
    indent = guard[: len(guard) - len(guard.lstrip())]
    mutated = lines[:]
    mutated[idx] = f"{indent}if false {{\n"
    TARGET.write_text("".join(mutated))

    run = subprocess.run(
        ["cargo", "test", "-p", "quota-core", "--lib", "wire_sanity"],
        cwd=REPO,
        capture_output=True,
        text=True,
    )
    failed = re.findall(r"^test (\S+) \.\.\. FAILED", run.stdout, re.M)
    # "Did the suite run at all" is the unambiguous question, and it has to be
    # asked FIRST. Asking "did it compile" first means matching on error text,
    # and cargo prints `error: test failed` for a test that built and correctly
    # failed -- so every genuinely defended rule would be misreported as skipped.
    # A summary line is printed only when the binary ran.
    compiled = re.search(r"^test result:", run.stdout, re.M) is not None

    subprocess.run(["git", "checkout", "--", REL], cwd=REPO, check=True)

    label = guard.strip()[:64]
    if not compiled:
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
print(f"  restored byte-identical; undefended rules: {undefended or 'none'}")
