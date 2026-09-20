#!/usr/bin/env python3
"""Check that every installed scoped credential routes to a provider that reads it.

WHY THIS EXISTS. `CREDENTIAL_FAMILIES` maps credential-id prefixes to providers.
The module's installed grant snapshot holds credential ids. Both lists are
internally coherent, so reading either one finds nothing wrong -- only the JOIN
between them can be broken, and a broken join is silent: the credential is
present, granted, and simply unrouted.

Found by hand on 2026-09-19: the vault canonicalises static keys under `apikey:`
and holds `apikey:kimi-for-coding`, while the family table had only the bare
`kimi-for-coding`. That lane was SERVING. It worked because the handle map is
written by hand and its author matched the table rather than the vault. Under
scoped grants the ids arrive FROM the vault, so at cutover the lane would have
gone dark with no error anywhere.

The inventory comes from `ck module status insula --json`, the same daemon
health readback used by `vault-lanes`. This process never calls the vault: a
standalone process has no reserved principal and cannot enumerate the grant.

WHAT IT CANNOT DO, stated so a clean run is not over-read: it compares the
module's CURRENT installed snapshot against the CURRENT table. It cannot know
which unclaimed ids SHOULD be claimed -- that is a judgement about what each
provider reads -- so it
flags ids that NAME a family without matching it and leaves the adjudication to a
person. Ids that resemble nothing are reported as a count, not as findings.

Exit 0 clean, 1 findings, 2 could not check.
"""

import json
import pathlib
import re
import shutil
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
FAMILIES_SRC = REPO / "crates" / "quota-core" / "src" / "vault_handles.rs"
MODULE_ID = "insula"
CALL_TIMEOUT_SECS = 45
DAEMON_CLI_CANDIDATES = (
    pathlib.Path.home() / "Work/Projects/CortexKit/subconscious/target/release/ck",
    pathlib.Path.home() / ".local/share/cortexkit/bin/ck",
)

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


def locate_cli():
    """Find the daemon CLI without assuming the caller's PATH."""
    for candidate in DAEMON_CLI_CANDIDATES:
        if candidate.is_file():
            return candidate
    found = shutil.which("ck")
    return pathlib.Path(found) if found else None


def ids_from_status(payload):
    """Read the exact inventory installed by the supervised module."""
    health = payload.get("health")
    if not isinstance(health, dict):
        raise ValueError("status carried no health object")
    metrics = health.get("metrics")
    if not isinstance(metrics, dict):
        raise ValueError("health carried no metrics object")

    if "vaultEnumerationFailure" not in metrics:
        raise ValueError("health omitted vaultEnumerationFailure")
    failure = metrics["vaultEnumerationFailure"]
    if failure is not None:
        age = metrics.get("retainedVaultSnapshotAgeSecs")
        retained = f"; retained snapshot age {age}s" if age is not None else ""
        raise ValueError(f"scoped credential enumeration failed: {failure}{retained}")

    ids = metrics.get("scopedCredentialIds")
    if not isinstance(ids, list) or not all(isinstance(cid, str) for cid in ids):
        raise ValueError("health carried no scopedCredentialIds string array")
    return ids


def installed_snapshot_ids():
    """Credential ids from the module health record retained by the daemon."""
    cli = locate_cli()
    if cli is None:
        print("could not read the installed snapshot: no `ck` binary found", file=sys.stderr)
        print("exit 2: this needs a running daemon and the ck CLI", file=sys.stderr)
        sys.exit(2)
    try:
        out = subprocess.run(
            [str(cli), "module", "status", MODULE_ID, "--json"],
            capture_output=True,
            text=True,
            timeout=CALL_TIMEOUT_SECS,
        )
    except subprocess.TimeoutExpired:
        print(
            f"could not read the installed snapshot: `ck module status` did not "
            f"answer in {CALL_TIMEOUT_SECS}s",
            file=sys.stderr,
        )
        sys.exit(2)
    if out.returncode != 0:
        detail = (out.stderr or out.stdout).strip().splitlines()
        first = detail[0] if detail else f"exit {out.returncode}"
        print(f"could not read the installed snapshot: {first}", file=sys.stderr)
        sys.exit(2)
    try:
        payload = json.loads(out.stdout)
        return ids_from_status(payload)
    except (json.JSONDecodeError, ValueError) as error:
        print(f"could not read the installed snapshot: {error}", file=sys.stderr)
        sys.exit(2)


def claims(cid, fams):
    """Which providers claim this id. Mirrors `handle_id_names_family`."""
    return [n for p, n in fams if cid == p or cid.startswith(p + ":")]


def main():
    fams = families()
    ids = installed_snapshot_ids()

    # REFUSE ON AN EMPTY POPULATION rather than reporting a clean run over
    # nothing. The readback field is present, so zero is not a parse default; it
    # means the supervised module installed an empty grant snapshot, which leaves
    # no routing claim for this checker to verify.
    if not ids:
        print("installed scoped snapshot contains zero credential ids", file=sys.stderr)
        print("exit 2: no routing population to compare", file=sys.stderr)
        sys.exit(2)

    # This is the row-id set read from the module, not a second vault query. A
    # caller can compare it byte-for-byte with scopedCredentialIds from the same
    # health turn and know both checkers used the same population.
    print(f"  installed snapshot row ids: {json.dumps(sorted(set(ids)))}")
    print(f"  enumerated rows: {len(ids)}   family prefixes: {len(fams)}")

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

    # THE DEFECT SHAPE: an id naming something this module serves, that routes
    # nowhere. Two discriminators, because one of them missed a real row.
    #
    # FIRST, the prefix-tail match: an id that names a family without matching it,
    # which is the spelling divergence that cost a serving lane.
    #
    # SECOND, and added after the first one missed `oauth:cursor`: an id whose
    # PROVIDER SEGMENT is a provider this module serves. `cookie:cursor.com` has
    # tail `cursor.com`, which shares no substring with `oauth:cursor`, so the
    # tail test classified a credential for a provider we serve as "unrelated to
    # any provider here" and buried it in a count. A heuristic keyed on the
    # FAMILY's spelling cannot see an id that spells the same provider a different
    # way -- which is the whole class this checker exists for.
    providers = {name for _, name in fams}
    namelike = []
    for cid in unclaimed:
        if cid in DELIBERATELY_UNCLAIMED:
            continue
        matched = None
        for prefix, provider in fams:
            tail = prefix.split(":")[-1]
            if tail and tail in cid:
                matched = (cid, prefix, provider)
                break
        if matched is None:
            # `<method>:<provider>[:<account>]` -- the provider is segment two.
            parts = cid.split(":")
            if len(parts) >= 2 and parts[1] in providers:
                matched = (cid, None, parts[1])
        if matched:
            namelike.append(matched)

    for cid, prefix, provider in namelike:
        if prefix is None:
            # No family covers this credential METHOD for a provider we serve.
            findings.append(
                f"{cid} is a credential for {provider}, which this module serves, but no "
                f"family covers the {cid.split(':')[0]!r} method: it would route nowhere. "
                f"Either add the family, or record it in DELIBERATELY_UNCLAIMED with the "
                f"reason no lane can consume it"
            )
        else:
            findings.append(
                f"{cid} names the {provider} family (prefix {prefix!r}) but does not match "
                f"it: a spelling divergence, so a granted credential routes nowhere"
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
