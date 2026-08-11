#!/usr/bin/env python3
"""Find providers whose credential is on this host but which never look for it.

THE DEFECT THIS CATCHES is a provider reporting `credential_absent` while the
key it needs sits in the shared opencode auth store. That reads to an operator
as "this provider was never set up", so nobody investigates, and the account
stays dark indefinitely. It is the quietest failure this module has: the
provider is honest, the wire is well-formed, and the answer is wrong.

It happens because a provider adapter is written against the credential source
its upstream documents -- usually an environment variable -- while a user who
signed in through another tool already has the same key stored under the
provider's models.dev slug. Both sides are reasonable; the gap is only visible
by crossing them.

HOW THE CROSS IS BUILT, and why each half is derived rather than listed:

  env-only providers   source files that call `first_env` and never mention
                       `opencode_auth`. Derived from the source, so a provider
                       that gains a store fallback leaves this set by itself.
                       COARSE ON PURPOSE: a provider may call `first_env` for a
                       region or project override while reading its actual
                       credential from a file, so this set over-includes. The
                       live filter below is what removes those.

  currently absent     read from the deployed module: only a provider REPORTING
                       `credential_absent` right now can be missing a key it
                       could read. A serving provider has no gap by definition,
                       whatever its source looks like. This is the discriminator
                       that turns a static over-approximation into a finding.

  slug per provider    parsed out of `api_provider_name` in lib.rs, which is
                       the map this module already maintains for the wire. The
                       store is keyed by models.dev slug and this module by its
                       own provider names, so nothing can be matched without it.

  keys on this host    read from the auth store, never assumed.

A hand-kept list of "providers that should check the store" would answer the
same question and rot silently, because nothing fails when it falls behind.

WHAT A CLEAN RESULT MEANS, precisely: no provider on THIS host is missing a key
it could read. It is a statement about this machine's credentials, not a
property of the code -- a different host with different logins can produce
findings from the identical tree, which is why this prints the population it
examined rather than only the verdict.

Exit codes: 0 clean, 1 findings, 2 refused to run (nothing examined).
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SRC = REPO / "crates/quota-core/src"

# Not providers: shared helpers that legitimately read the environment.
NOT_PROVIDERS = {"env", "lib", "model", "money", "http", "text", "config", "tests"}


def auth_store_path() -> Path:
    base = os.environ.get("XDG_DATA_HOME")
    root = Path(base) if base else Path.home() / ".local/share"
    return root / "opencode/auth.json"


def absent_providers() -> set[str] | None:
    """Providers the deployed module currently reports as credential_absent.

    Returns None when the wire cannot be read, in which case the sweep runs
    without this filter and says so. A finding without it is a CANDIDATE: the
    static half cannot tell a provider that reads its key from the environment
    from one that merely consults the environment for a region override.
    """
    ck = Path.home() / "Work/Projects/CortexKit/subconscious/target/release/ck"
    if not ck.exists():
        return None
    try:
        out = subprocess.run(
            [str(ck), "quota", "--json"],
            capture_output=True, text=True, timeout=45, check=False,
        )
        if out.returncode != 0:
            return None
        rows = json.loads(out.stdout)
        if isinstance(rows, dict):
            rows = rows.get("entries") or rows.get("result") or []
    except (subprocess.SubprocessError, json.JSONDecodeError, OSError):
        return None
    return {
        r["provider"]
        for r in rows
        if isinstance(r, dict) and r.get("errorClass") == "credential_absent"
    }


def code_only(source: str) -> str:
    """Strip line comments, so a MENTION is not mistaken for a call.

    Both halves of the detection below are substring tests, and a comment
    naming either one moves a provider between sets without anything saying so.
    The over-inclusion direction is caught later by the live filter; this
    direction is not caught by anything -- a provider drops out of the examined
    population and the run reports a smaller number that still reads as clean.

    Verified rather than assumed: adding the line "this does not read
    opencode_auth" to a provider that genuinely reads only its environment
    removed it from the crossable set, silently.
    """
    return "\n".join(line.split("//")[0] for line in source.splitlines())


def env_only_providers() -> tuple[list[str], list[str]]:
    """Providers reading an env var for a key and never consulting the store.

    Returns the set and the providers excluded for having a store fallback, so
    the run can report the exclusion rather than only its result. A population
    that shrinks silently is the failure this whole check exists to avoid one
    layer down.
    """
    out, has_fallback = [], []
    for f in sorted(SRC.glob("*.rs")):
        if f.stem in NOT_PROVIDERS:
            continue
        text = code_only(f.read_text(encoding="utf-8", errors="replace"))
        if "first_env" not in text:
            continue
        if "opencode_auth" in text:
            has_fallback.append(f.stem)
        else:
            out.append(f.stem)
    return out, has_fallback


def slug_map() -> dict[str, str]:
    """The provider -> models.dev slug map this module publishes as apiProvider."""
    text = (SRC / "lib.rs").read_text(encoding="utf-8", errors="replace")
    start = text.find("fn api_provider_name")
    if start < 0:
        sys.exit("refusing: api_provider_name not found in lib.rs -- has it been renamed?")
    body = text[start:]
    end = body.find("\n}")
    return dict(re.findall(r'"([^"]+)" => Some\("([^"]+)"\)', body[:end]))


def main() -> int:
    store_path = auth_store_path()
    if not store_path.exists():
        print(f"refusing: no auth store at {store_path}")
        print("nothing to cross against; this is not a clean result")
        return 2
    try:
        store = json.loads(store_path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, OSError) as exc:
        print(f"refusing: auth store unreadable ({exc})")
        return 2

    env_only, has_fallback = env_only_providers()
    slugs = slug_map()

    if not env_only:
        print("refusing: no env-only provider found -- the detector is broken,")
        print("not the codebase; a zero here would look identical to a clean run")
        return 2
    if not slugs:
        print("refusing: the provider -> slug map parsed empty")
        return 2

    # Only providers with a slug can be crossed at all: without one there is no
    # key to look for, and saying nothing about them is honest.
    crossable = [(p, slugs[p]) for p in env_only if p in slugs]
    unmapped = [p for p in env_only if p not in slugs]

    absent = absent_providers()
    findings = []
    print(f"  auth store: {store_path} ({len(store)} entries)")
    if absent is None:
        print("  live filter: UNAVAILABLE -- findings below are candidates, not")
        print("               confirmed gaps (a provider may read its credential")
        print("               from a file while consulting the environment for")
        print("               something else entirely)")
    else:
        print(f"  live filter: {len(absent)} providers currently report credential_absent")
    print(f"  env-only providers: {len(env_only)}  of which mapped to a slug: {len(crossable)}")
    print(
        f"  already read the store, so nothing to cross: {len(has_fallback)}"
        + (f" ({', '.join(has_fallback)})" if has_fallback else "")
    )
    print()
    print("  PROVIDER              SLUG                       KEY ON THIS HOST")
    for provider, slug in crossable:
        entry = store.get(slug)
        if not entry:
            print(f"  {provider:21} {slug:26} no")
            continue
        kind = entry.get("type", "?") if isinstance(entry, dict) else "?"
        if absent is not None and provider not in absent:
            # Serving, or failing for some other reason. Either way it is not
            # missing a credential it could have read.
            print(f"  {provider:21} {slug:26} YES ({kind}) -- but not absent, so no gap")
            continue
        print(f"  {provider:21} {slug:26} YES ({kind})")
        findings.append((provider, slug, kind))

    if unmapped:
        print()
        print(f"  not crossable ({len(unmapped)}, no canonical slug, so no store key to look for):")
        print("   ", ", ".join(unmapped))

    print()
    if findings:
        print(f"findings: {len(findings)}")
        for provider, slug, kind in findings:
            print(f"  {provider} reads only its environment, but a {kind} credential for")
            print(f"    '{slug}' is present on this host. If that key would authenticate")
            print(f"    {provider}, it is reporting credential_absent while its credential exists.")
        print()
        print("Not automatically a defect: a slug can hold a credential for a")
        print("DIFFERENT product on the same API. Check what the endpoint accepts")
        print("before wiring it -- sending the wrong token earns a 401, which is")
        print("recorded as a rejected credential and is worse than absent.")
        return 1

    print("findings: none")
    print(f"({len(crossable)} providers crossed against {len(store)} stored credentials;")
    print(" a clean result here is about THIS host's logins, not about the code)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
