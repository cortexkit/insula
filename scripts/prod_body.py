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

# `#[cfg(test)]` on something that is not a module: a test-only constructor
# beside the real one, an injection hook, a helper accessor. These sit ABOVE the
# test module, so cutting at the module leaves them inside what this calls the
# production body.
TEST_ONLY_ITEM = re.compile(r"#\[cfg\(test\)\]\s*(?://[^\n]*\n\s*)*(?!mod\b)")


def boundary_description() -> str:
    """Describe the cut rule, derived from the pattern that performs it.

    Read out of `TEST_MODULE` rather than written beside it. A hand-written
    description is prose sitting next to a derived artifact: it agrees with the
    code on the day it is written and silently stops agreeing when someone edits
    the pattern, while still carrying the authority of an explicit statement.
    Changing the anchor changes this line.
    """
    readable = (
        TEST_MODULE.pattern.replace(r"\[", "[")
        .replace(r"\]", "]")
        .replace(r"\(", "(")
        .replace(r"\)", ")")
        .replace(r"\s*(?://[^\n]*\n\s*)*", " ")
        .replace(r"\s+", " ")
        .replace(r"\b", "")
    )
    return f'"production" is everything above `{readable}`'


def whole_file_is_test_only(path: pathlib.Path) -> str | None:
    """Return the declaring file when this whole file is a `#[cfg(test)]` module.

    A module can be gated where it is DECLARED rather than inside itself:
    `#[cfg(test)] mod tests;` in lib.rs makes every line of tests.rs test-only,
    and nothing in tests.rs says so. Cutting at an in-file marker then keeps the
    entire file as production code and reports its contents as findings.

    That is not hypothetical -- sweeping for aborting constructs returned 16
    hits, every one of them in such a file, and the correct answer was zero.
    The failure is quiet in the dangerous direction: the sweep produced a
    plausible non-zero count with real line numbers, so the natural reaction is
    to start triaging rather than to doubt the population.
    """
    stem = path.stem
    for sibling in path.parent.glob("*.rs"):
        if sibling == path:
            continue
        text = sibling.read_text(encoding="utf-8", errors="replace")
        if re.search(r"#\[cfg\(test\)\]\s*(?://[^\n]*\n\s*)*mod\s+" + re.escape(stem) + r"\s*;", text):
            return sibling.name
    return None


def production_body(source: str) -> tuple[str, float]:
    """Return the source up to the test module, and the fraction that is."""
    match = TEST_MODULE.search(source)
    body = source[: match.start()] if match else source
    fraction = len(body) / len(source) if source else 1.0
    return body, fraction


def test_only_items(body: str) -> list[int]:
    """Line numbers of test-only items left inside a production body.

    Cutting at the test module is right for the common case and wrong for
    `#[cfg(test)]` applied to an individual item, which sits above that module
    and stays in the body. A sweep asking "does every provider do X" will then
    count a test-only helper as production code.

    This is the mirror of the failure this tool was written to fix. That one
    truncated a file and reported things ABSENT that were present; this one
    over-includes and reports things PRESENT that exist only under `cfg(test)`.
    The second is the quieter direction: a sweep looking for a missing call
    finds the one in the test-only helper and concludes the file is fine.
    """
    return [body[: m.start()].count("\n") + 1 for m in TEST_ONLY_ITEM.finditer(body)]


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
    impure: list[tuple[str, int]] = []
    hits: list[str] = []
    externally_gated: list[tuple[str, str]] = []
    read_bytes = 0
    total_bytes = 0

    for path in args.files:
        source = path.read_text()
        declared_by = whole_file_is_test_only(path)
        if declared_by is not None:
            # Every line is test-only, gated where the module is declared. Skip
            # it entirely rather than cut it: there is nothing to cut at, and
            # keeping it would report its whole contents as production findings.
            externally_gated.append((path.name, declared_by))
            total_bytes += len(source)
            continue
        body, fraction = production_body(source)
        read_bytes += len(body)
        total_bytes += len(source)

        naive = source.find("#[cfg(test)]")
        if naive >= 0 and naive < len(body) - 200:
            misleading.append((path.name, round(100 * naive / max(len(body), 1))))

        leftover = test_only_items(body)
        if leftover:
            impure.append((path.name, len(leftover)))

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
        # The premise is printed beside the numbers because it is the thing a
        # reader would otherwise have to reconstruct from the source. Every
        # result here rests on one definition of "production", and a reader who
        # disagrees with that definition cannot tell it was used -- the counts
        # look identical under any boundary. Stating it lets someone reject the
        # reasoning without re-deriving it, which is how the boundary's own
        # incompleteness was found.
        # The share of the corpus the boundary rule actually kept. This is the
        # rule's own output rather than a description of it, so it cannot agree
        # with the code while the code does something else: an anchor that fails
        # to match keeps whole files and drives this to 100%, and one that
        # matches too early collapses it. A reader who expects roughly two
        # thirds and sees 5% knows the answer is about a different question,
        # without knowing anything about the pattern.
        share = round(100 * read_bytes / total_bytes) if total_bytes else 100
        # "as given" rather than a bare count, because this tool never chooses
        # its own corpus -- the caller's shell glob does. A sweep reporting
        # "examined: 40" reads as exhaustive whether the glob covered the whole
        # crate or one directory, and the number cannot distinguish them. The
        # denominator this prints is the one it was handed, not the one that
        # exists.
        #
        # The caveat counts appear on every run, including when they are zero.
        # Reported only when non-zero, a caveat DISAPPEARS if the rule that
        # detects it stops matching -- and every other number stays identical,
        # because those rules describe the corpus rather than select from it. A
        # reader cannot notice a line that is not there, so the failure of a
        # caveat is invisible in exactly the way the caveat exists to prevent.
        print(
            f"premise: {boundary_description()}\n"
            f"examined: {len(args.files)} file(s) as given, {share}% of their "
            f"bytes   "
            f"matched: {len(hits)}\n"
            f"caveats: {len(misleading)} file(s) truncate under a "
            f"first-attribute cut, {len(impure)} carry test-only items in the "
            f"production body",
            file=sys.stderr,
        )

    if misleading:
        print(
            f"note: {len(misleading)} file(s) would truncate under a "
            f"first-attribute cut: "
            + ", ".join(f"{name} at {pct}%" for name, pct in sorted(misleading)),
            file=sys.stderr,
        )

    # Reported rather than excised. Removing these items would need the spans
    # they cover, and a brace-matched span is its own guessing game -- while
    # naming them lets a reader check whether a result rests on one.
    if externally_gated:
        for name, declared_by in externally_gated:
            print(
                f"  skipped {name}: every line is test-only, gated at "
                f"`#[cfg(test)] mod` in {declared_by}"
            )
    else:
        print("  files gated at their declaration: 0")

    if impure:
        print(
            f"note: {len(impure)} file(s) carry test-only items inside the "
            f"production body, so a match there may be test code: "
            + ", ".join(f"{name} ({n})" for name, n in sorted(impure)),
            file=sys.stderr,
        )

    for hit in hits:
        print(hit)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
