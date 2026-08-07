#!/usr/bin/env python3
"""Read the production half of a Rust source file, for sweeps across providers.

A sweep that asks "does every provider do X" needs each file with its test
module removed, or every assertion in a test counts as production code. The
obvious way to cut -- stop at the first `#[cfg(test)]` -- is wrong in this
crate, because that attribute also sits on production items: a test-only
constructor beside the real one, a `thread_local!` used for injection. The cut
then lands wherever the first such item happens to be, and `lib.rs` truncates at
5%.

The resulting failure is one-directional, which is what makes it convincing: a
truncated scan reports things ABSENT that are present, never the reverse. A
false "missing" reads as a finding, so it gets investigated and then explained
away -- and a real gap in the same run is indistinguishable from that noise. The
scan does not fail loudly; it produces exactly the sort of output a sweep is
supposed to produce.

So this cuts at the test MODULE (`#[cfg(test)] mod tests`), and reports how much
of each file it read. The coverage line is the load-bearing half: an anchor fix
alone is silent, and the next person to write an extractor gets no warning that
theirs is wrong. A scan that says it read 5% of a file cannot be mistaken for
one that read all of it.

    # every provider missing a call, with coverage printed to stderr
    ./scripts/prod_body.py --grep-missing report_auth_failure crates/quota-core/src/*.rs

    # the production body of one file, for piping
    ./scripts/prod_body.py crates/quota-core/src/codex.rs
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# `#[cfg(test)]`, any whitespace or comments, then `mod tests`. Anchoring on the
# module rather than the attribute is the whole point: the attribute alone
# appears on production items.
TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*(?://[^\n]*\n\s*)*mod\s+tests\b")


def production_body(source: str) -> tuple[str, float]:
    """Return the source up to the test module, and the fraction that is."""
    match = TEST_MODULE.search(source)
    body = source[: match.start()] if match else source
    fraction = len(body) / len(source) if source else 1.0
    return body, fraction


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("files", nargs="+", type=Path)
    parser.add_argument(
        "--grep-missing",
        metavar="NEEDLE",
        help="list files whose production body does NOT contain NEEDLE",
    )
    parser.add_argument(
        "--grep",
        metavar="NEEDLE",
        help="list files whose production body DOES contain NEEDLE",
    )
    args = parser.parse_args()

    # Refuse a sweep with nothing to sweep, rather than reporting it clean.
    #
    # Argument parsing already rejects an empty invocation, so this looks
    # redundant -- but that protection is a side effect of `files` being
    # positional, and it disappears the first time someone gives it a default or
    # accepts a glob that matches nothing. A refusal that exists on purpose
    # survives a change to how arguments are handled; one that exists by
    # accident dies silently with it, and the failure it was preventing looks
    # exactly like success.
    if not args.files:
        print(
            "refusing: no files to examine, which would report clean without "
            "having looked at anything",
            file=sys.stderr,
        )
        return 2

    # Files where the naive cut would have differed, so a reader can see whether
    # this sweep would have been wrong without it.
    misleading: list[tuple[str, int]] = []
    hits: list[str] = []

    for path in args.files:
        source = path.read_text()
        body, fraction = production_body(source)

        naive = source.find("#[cfg(test)]")
        if naive >= 0 and naive < len(body) - 200:
            misleading.append((path.name, round(100 * naive / max(len(body), 1))))

        if args.grep_missing is not None:
            if args.grep_missing not in body:
                hits.append(str(path))
        elif args.grep is not None:
            if args.grep in body:
                hits.append(str(path))
        else:
            sys.stdout.write(body)

    # The denominator, printed whenever this is used as a search rather than as
    # a filter. Without it a run that examined forty files and found nothing is
    # indistinguishable from one that examined a single file, or from a pattern
    # that could never match -- and this is a tool for hunting ABSENCE, where
    # every bug produces more apparent absence. Its errors would otherwise be
    # shaped exactly like its findings.
    if args.grep_missing is not None or args.grep is not None:
        print(
            f"examined: {len(args.files)} file(s)   matched: {len(hits)}",
            file=sys.stderr,
        )

    if misleading:
        print(
            f"note: {len(misleading)} file(s) would truncate under a "
            f"first-attribute cut: "
            + ", ".join(f"{name} at {pct}%" for name, pct in sorted(misleading)),
            file=sys.stderr,
        )

    for hit in hits:
        print(hit)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
