#!/usr/bin/env python3
"""Check that every credential the vault holds routes to a provider that reads it.

WHY THIS EXISTS. `CREDENTIAL_FAMILIES` maps credential-id prefixes to providers.
The vault holds credential ids. Both lists are internally coherent, so reading
either one finds nothing wrong -- only the JOIN between them can be broken, and a
broken join is silent: the credential is present, granted, and simply unrouted.

Found by hand on 2026-09-19: the vault canonicalises static keys under `apikey:`
and holds `apikey:kimi-for-coding`, while the family table had only the bare
`kimi-for-coding`. That lane was SERVING. It worked because the handle map is
written by hand and its author matched the table rather than the vault. Under
scoped grants the ids arrive FROM the vault, so at cutover the lane would have
gone dark with no error anywhere.

WHAT IT CANNOT DO, stated so a clean run is not over-read: it compares the
CURRENT vault against the CURRENT table. It cannot know which unclaimed ids
SHOULD be claimed -- that is a judgement about what each provider reads -- so it
flags ids that NAME a family without matching it and leaves the adjudication to a
person. Ids that resemble nothing are reported as a count, not as findings.

Exit 0 clean, 1 findings, 2 could not check.
"""

import pathlib
import re
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
FAMILIES_SRC = REPO / "crates" / "quota-core" / "src" / "vault_handles.rs"

# Ids that name a family and are DELIBERATELY unclaimed, with the reason.
#
# Each entry is a claim that can go stale in one direction only: if a family ever
# starts claiming one of these, the entry is wrong and this script says so. That
# check is why the exemption is safe to keep -- an exemption nobody re-reads is
# how a stale one survives, which this repo learned from a DUAL_LANE entry that
# outlived its reason by hours.
DELIBERATELY_UNCLAIMED = {
    "apikey:openai": "a platform API key is a different plane from a ChatGPT "
    "subscription; codex reads the subscription OAuth credential and must never "
    "be handed this",
    "apikey:openai:astro": "another seat's platform key, same reason",
    "apikey:cerebras": "declined: the endpoint is Cloudflare-blocked and exposes "
    "no usage API",
    "apikey:fireworks-ai": "declined: no structured usage endpoint",
}


def families():
    """Parse the (prefix, provider) table out of the source."""
    src = FAMILIES_SRC.read_text()
    start = src.index("pub const CREDENTIAL_FAMILIES")
    block = src[start : src.index("];", start)]
    found = re.findall(r'\("([^"]+)",\s*"([^"]+)"\)', block)
    if not found:
        print("could not parse CREDENTIAL_FAMILIES: the table shape changed", file=sys.stderr)
        print("exit 2: no verdict is possible about routing", file=sys.stderr)
        sys.exit(2)
    return found


def vault_ids():
    """Credential ids the vault currently holds."""
    try:
        out = subprocess.run(
            ["ck", "auth", "list"], capture_output=True, text=True, timeout=30
        )
    except (FileNotFoundError, subprocess.TimeoutExpired) as error:
        print(f"could not read the vault inventory: {error}", file=sys.stderr)
        print("exit 2: this needs a running daemon and the ck CLI", file=sys.stderr)
        sys.exit(2)
    if out.returncode != 0:
        print(f"`ck auth list` exited {out.returncode}", file=sys.stderr)
        print("exit 2: no inventory to compare against", file=sys.stderr)
        sys.exit(2)
    ids = []
    for line in out.stdout.splitlines()[1:]:
        parts = line.split()
        # STATE VER CREDENTIAL CATEGORIES
        if len(parts) >= 3 and parts[1].startswith("v"):
            ids.append(parts[2])
    return ids


def claims(cid, fams):
    """Which providers claim this id. Mirrors `handle_id_names_family`."""
    return [n for p, n in fams if cid == p or cid.startswith(p + ":")]


def main():
    fams = families()
    ids = vault_ids()

    # REFUSE ON AN EMPTY POPULATION rather than reporting a clean run over
    # nothing. A zero here means the inventory could not be read, and a clean
    # verdict over zero credentials is indistinguishable from a clean verdict over
    # all of them.
    if not ids:
        print("no credential ids parsed from `ck auth list`", file=sys.stderr)
        print("exit 2: the inventory is empty or its format changed", file=sys.stderr)
        sys.exit(2)

    print(f"  vault credentials: {len(ids)}   family prefixes: {len(fams)}")

    routed = [c for c in ids if claims(c, fams)]
    unclaimed = [c for c in ids if not claims(c, fams)]
    print(f"  routed to a provider: {len(routed)}   unclaimed: {len(unclaimed)}")

    findings = []

    # A STALE EXEMPTION IS A FINDING. If a family now claims something this table
    # says is deliberately unrouted, the reason above is wrong and somebody should
    # know which way.
    for cid, reason in DELIBERATELY_UNCLAIMED.items():
        if cid in ids and claims(cid, fams):
            findings.append(
                f"{cid} is now claimed by {claims(cid, fams)}, but this checker "
                f"records it as deliberately unclaimed ({reason}) -- one of the two is wrong"
            )

    # THE DEFECT SHAPE: an id that NAMES a family without matching it. That is a
    # spelling divergence between two coherent lists, which is exactly what no
    # amount of reading either list reveals.
    namelike = []
    for cid in unclaimed:
        if cid in DELIBERATELY_UNCLAIMED:
            continue
        for prefix, provider in fams:
            tail = prefix.split(":")[-1]
            if tail and tail in cid:
                namelike.append((cid, prefix, provider))
                break

    for cid, prefix, provider in namelike:
        findings.append(
            f"{cid} names the {provider} family (prefix {prefix!r}) but does not match it: "
            f"a granted credential that no provider would read"
        )

    # Ids resembling nothing are ordinary -- the vault holds credentials for tools
    # that are not providers here. A COUNT, not findings, because listing them
    # every run trains the reader to skim.
    unrelated = len(unclaimed) - len(namelike) - len(
        [c for c in DELIBERATELY_UNCLAIMED if c in ids]
    )
    print(f"  unclaimed and unrelated to any provider here: {unrelated}")
    print(f"  deliberately unclaimed and still unclaimed: "
          f"{len([c for c in DELIBERATELY_UNCLAIMED if c in ids])}")

    if not findings:
        print("  findings: none")
        return 0

    print(f"  findings: {len(findings)}")
    for finding in findings:
        print(f"    {finding}")
    return 1


if __name__ == "__main__":
    sys.exit(main())
