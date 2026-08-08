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

# Each rule is an `if <cond> {` whose body pushes a finding. Find the guard line
# for every findings.push by walking back to the nearest enclosing `if`.
push_lines = [i for i, l in enumerate(lines) if "findings.push" in l]
guards = []
for p in push_lines:
    for j in range(p, max(p - 12, -1), -1):
        stripped = lines[j].strip()
        if re.match(r"^if .*\{$", stripped) or re.match(r"^\} else if .*\{$", stripped):
            if j not in guards:
                guards.append(j)
            break

print(f"  rules with a mutable guard: {len(guards)} (of {len(push_lines)} findings sites)")
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
    compiled = "error[E" not in run.stderr

    subprocess.run(["git", "checkout", "--", REL], cwd=REPO, check=True)

    label = guard.strip()[:64]
    if not compiled:
        print(f"  line {idx + 1}: SKIPPED (mutation did not compile) {label}")
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
