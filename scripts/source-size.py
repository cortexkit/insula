#!/usr/bin/env python3
"""Refuse a hand-written Rust source file over 5,000 lines.

Hand-written files stay under 5,000 lines so that a one-line edit
rebuilds quickly and a reader can hold a file in mind. Test code is exempt:
whole test files (`tests.rs`, `*_tests.rs`, anything under a `tests/` or
`*_tests/` directory) are skipped, and an in-file `#[cfg(test)] mod ... { }`
block is not counted. A file between 4,500 and 5,000 counted lines is listed
as a warning so it can be split before it reaches the limit.

A file that must stay larger is named in EXCEPTIONS with the reason. Its
counted lines may not grow past the recorded size.

Run with --self-test to check the counter against planted sources first; the
gate does that before trusting a clean result.
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

LIMIT = 5_000
WARN = 4_500
# path -> (recorded counted lines, reason). Empty: every file is under the limit.
EXCEPTIONS: dict[str, tuple[int, str]] = {}


def is_test_file(path: str) -> bool:
    parts = path.split("/")
    name = parts[-1]
    if name == "tests.rs" or name.endswith("_tests.rs"):
        return True
    return any(p == "tests" or p.endswith("_tests") for p in parts[:-1])


def counted_lines(text: str) -> int:
    """Lines outside `#[cfg(test)]`-gated `mod` blocks."""
    lines = text.splitlines()
    count = 0
    i = 0
    while i < len(lines):
        stripped = lines[i].strip()
        if stripped == "#[cfg(test)]":
            # Skip attributes and blank lines up to the item it gates.
            j = i + 1
            while j < len(lines) and (
                lines[j].strip().startswith("#[") or lines[j].strip().startswith("//")
                or not lines[j].strip()
            ):
                j += 1
            item = lines[j].strip() if j < len(lines) else ""
            declaration = item.removeprefix("pub(crate) ").removeprefix("pub ")
            if declaration.startswith("mod ") and "{" in item:
                depth = 0
                k = j
                while k < len(lines):
                    depth += lines[k].count("{") - lines[k].count("}")
                    k += 1
                    if depth <= 0:
                        break
                i = k
                continue
        count += 1
        i += 1
    return count


def tracked_sources() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "--", "crates/*.rs", "crates/**/*.rs"],
        check=True, capture_output=True, text=True,
    ).stdout
    return sorted(set(p for p in out.splitlines() if p.endswith(".rs")))


def self_test() -> bool:
    body = "fn a() {}\n" * 10
    tests = "#[cfg(test)]\nmod tests {\n" + "    fn t() {}\n" * 50 + "}\n"
    gated_item = "#[cfg(test)]\nfn helper() {}\n"
    after = "fn z() {}\n"
    planted = body + tests + gated_item + after
    # 10 body lines + 2 for the gated fn (only mod blocks are skipped) + 1 after.
    ok = counted_lines(planted) == 13
    ok &= is_test_file("crates/x/src/tests.rs") and is_test_file("crates/x/tests/it/a.rs")
    ok &= is_test_file("crates/x/src/usage_watch_tests/mod.rs")
    ok &= not is_test_file("crates/x/src/lib.rs") and not is_test_file("crates/x/src/testing.rs")
    return ok


def main() -> int:
    if not self_test():
        print("  source-size self-test failed: the counter is wrong, so a clean result would mean nothing")
        return 2
    if "--self-test" in sys.argv:
        print("  self-test passed")
        return 0
    sources = [p for p in tracked_sources() if not is_test_file(p)]
    if not sources:
        print("  no tracked Rust sources found -- refusing rather than passing")
        return 2
    failures, warnings, largest = [], [], (0, "")
    for path in sources:
        n = counted_lines(Path(path).read_text(encoding="utf-8"))
        largest = max(largest, (n, path))
        if path in EXCEPTIONS:
            recorded, reason = EXCEPTIONS[path]
            if n > recorded:
                failures.append(f"{path}: {n} lines, grew past its recorded {recorded} ({reason})")
        elif n > LIMIT:
            failures.append(f"{path}: {n} lines, over the {LIMIT}-line limit; split it by area")
        elif n > WARN:
            warnings.append(f"{path}: {n} lines (warning above {WARN})")
    for line in warnings:
        print(f"  warning: {line}")
    for line in failures:
        print(f"  {line}")
    print(f"  {len(sources)} hand-written files checked, largest {largest[1]} at {largest[0]} lines, "
          f"{len(failures)} over the limit")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
