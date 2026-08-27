#!/usr/bin/env python3
"""Whole-history exposure verifier for insula's public-flip precheck.

Scans EVERY blob in EVERY ref plus EVERY commit message against narrow pattern
classes. The runbook's rule, learned from a rehearsal where a generic ip:port
class ate loopback examples in surviving files: ENUMERATE NARROW CLASSES, NEVER
GENERIC SHAPES. A generic class produces hits nobody will read and hides the ones
that matter.

Contains no banned literals. Machine-local specimen seeds, if any are ever
needed, belong in a gitignored file and never here.

This module's likely exposure class is not source code -- it is PROVIDER FIXTURES:
payloads captured from live provider endpoints while building normalizers, which
can carry account ids, emails, org names, bearer tokens and cookie values.
"""

from __future__ import annotations

import re
import subprocess
import sys
from collections import defaultdict

# Each class is narrow enough that a hit demands a human read. Ordered by how
# badly a true positive would matter.
CLASSES: list[tuple[str, re.Pattern[str]]] = [
    # --- credentials: a hit here is disqualifying until proven a test vector ---
    ("openai-key",      re.compile(r"\bsk-[A-Za-z0-9]{20,}")),
    ("anthropic-key",   re.compile(r"\bsk-ant-[A-Za-z0-9_-]{20,}")),
    ("alibaba-key",     re.compile(r"\bsk-sp-[A-Za-z0-9]{16,}")),
    ("github-token",    re.compile(r"\bgh[pusor]_[A-Za-z0-9]{30,}")),
    ("slack-token",     re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{10,}")),
    ("aws-key-id",      re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("google-api-key",  re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b")),
    ("jwt",             re.compile(r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{5,}")),
    ("private-key-pem", re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")),

    # --- identity: real addresses and account ids captured from live accounts ---
    # Deliberately excludes example.com/test/localhost domains, which appear
    # throughout the fixtures by design.
    ("email",           re.compile(
        r"\b[A-Za-z0-9._%+-]+@(?!example\.|test\.|localhost|.*\.invalid)"
        r"[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")),

    # --- session material lifted from a browser during cookie-lane work ---
    ("cookie-session",  re.compile(
        r"(login_qwencloud_ticket|WorkosCursorSessionToken|wos-session)\s*[=:]\s*"
        r"[A-Za-z0-9%._-]{20,}")),
]

# Paths whose hits are structural rather than exposures. Kept tiny and explicit:
# an over-wide allow list is how a real hit gets filed as noise.
ALLOW_PATH = re.compile(r"(^|/)(Cargo\.lock)$")


def blobs() -> list[tuple[str, str]]:
    out = subprocess.run(
        ["git", "rev-list", "--objects", "--all"],
        capture_output=True, text=True, check=True,
    ).stdout.splitlines()
    seen = []
    for line in out:
        parts = line.split(maxsplit=1)
        if len(parts) == 2:
            seen.append((parts[0], parts[1]))
    return seen


def scan_text(text: str) -> list[tuple[str, str]]:
    hits = []
    for name, pat in CLASSES:
        for m in pat.finditer(text):
            hits.append((name, m.group(0)))
    return hits


def main() -> int:
    findings: dict[str, list[tuple[str, str]]] = defaultdict(list)
    examined_blobs = 0
    skipped_binary = 0

    all_blobs = blobs()
    print(f"  objects in history: {len(all_blobs)}", flush=True)

    for sha, path in all_blobs:
        kind = subprocess.run(
            ["git", "cat-file", "-t", sha],
            capture_output=True, text=True,
        ).stdout.strip()
        if kind != "blob":
            continue
        raw = subprocess.run(
            ["git", "cat-file", "blob", sha], capture_output=True
        ).stdout
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            skipped_binary += 1
            continue
        examined_blobs += 1
        if ALLOW_PATH.search(path):
            continue
        for name, specimen in scan_text(text):
            findings[name].append((f"blob {sha[:10]} {path}", specimen))

    msgs = subprocess.run(
        ["git", "log", "--all", "--format=%H%x00%B%x01"],
        capture_output=True, text=True,
    ).stdout.split("\x01")
    examined_msgs = 0
    for chunk in msgs:
        if "\x00" not in chunk:
            continue
        sha, body = chunk.split("\x00", 1)
        examined_msgs += 1
        for name, specimen in scan_text(body):
            findings[name].append((f"msg  {sha.strip()[:10]}", specimen))

    print(f"  text blobs examined: {examined_blobs}  "
          f"(binary skipped: {skipped_binary})")
    print(f"  commit messages examined: {examined_msgs}")
    print()

    if not findings:
        print("  RESULT: zero hits across every class.")
        return 0

    print("  RESULT: hits found — each needs a human read before any decision.\n")
    for name, hits in sorted(findings.items(), key=lambda kv: -len(kv[1])):
        uniq_specimens = sorted({h[1] for h in hits})
        uniq_sites = sorted({h[0] for h in hits})
        print(f"  [{name}] {len(hits)} hit(s), "
              f"{len(uniq_specimens)} distinct, {len(uniq_sites)} site(s)")
        for s in uniq_specimens[:12]:
            shown = s if len(s) <= 64 else s[:61] + "..."
            print(f"      specimen: {shown}")
        if len(uniq_specimens) > 12:
            print(f"      ... {len(uniq_specimens) - 12} more distinct")
        for site in uniq_sites[:8]:
            print(f"      at: {site}")
        if len(uniq_sites) > 8:
            print(f"      ... {len(uniq_sites) - 8} more sites")
        print()
    return 1


if __name__ == "__main__":
    sys.exit(main())
