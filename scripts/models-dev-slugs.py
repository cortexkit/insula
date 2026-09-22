#!/usr/bin/env python3
"""Check every `apiProvider` slug insula publishes still exists on models.dev.

WHY THIS EXISTS: consumers join our usage entries to pricing and routing on the
`apiProvider` slug, and models.dev renames providers. On 2026-09-22 it retired
`kimi-for-coding` in favour of `kimi-code-plan-global` / `kimi-code-plan-cn`, and
we kept publishing the retired id. Nothing failed: the join simply matched
nothing, routing treated the account as having neutral abundance, and it took a
consumer tracing a tie-break to find it. A slug that joins to nothing is silent
by construction, so the only reader is one that goes and looks.

It reads the slug table straight out of `api_provider_name` in
crates/quota-core/src/lib.rs rather than from a copied list, so it cannot
disagree with what the module publishes.

Exit codes follow the repo's tri-state convention:
  0  every published slug exists on models.dev
  1  at least one slug is absent (the finding)
  2  could not check (network, parse failure, or an empty table)

Needs the network, so it is not part of scripts/gates.sh. Run it on a parity
round, and whenever a consumer reports a join that matches nothing.
"""

import json
import pathlib
import re
import sys
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
LIB = ROOT / "crates" / "quota-core" / "src" / "lib.rs"
API = "https://models.dev/api.json"

# A slug that must be in the table. If the extractor ever returns a table
# without it, the extractor broke rather than the table emptying.
POSITIVE_CONTROL = "anthropic"


def published_slugs() -> dict[str, str]:
    source = LIB.read_text()
    start = source.find("fn api_provider_name(")
    if start < 0:
        return {}
    end = source.find("\n}\n", start)
    body = source[start:end]
    return dict(re.findall(r'"([a-z0-9-]+)"\s*=>\s*Some\("([^"]+)"\)', body))


def main() -> int:
    table = published_slugs()
    if not table or POSITIVE_CONTROL not in table.values():
        print(f"could not check: extracted {len(table)} slug(s) from {LIB.name}, "
              f"and the table must contain {POSITIVE_CONTROL!r}")
        return 2
    try:
        # models.dev answers 403 to urllib's default User-Agent while serving
        # curl's, measured 2026-09-22; name ourselves rather than impersonate.
        request = urllib.request.Request(API, headers={"User-Agent": "insula-slug-check/1"})
        with urllib.request.urlopen(request, timeout=30) as response:
            catalog = json.load(response)
    except Exception as error:  # noqa: BLE001 -- any failure is "could not check"
        print(f"could not check: fetching {API} failed: {error}")
        return 2
    if POSITIVE_CONTROL not in catalog:
        print(f"could not check: {API} has no {POSITIVE_CONTROL!r}, so it is not "
              f"the catalog this script expects")
        return 2

    missing = {provider: slug for provider, slug in table.items() if slug not in catalog}
    print(f"published slugs: {len(table)}   on models.dev: {len(table) - len(missing)}   "
          f"catalog providers: {len(catalog)}")
    for provider, slug in sorted(missing.items()):
        near = sorted(key for key in catalog if slug.split("-")[0] in key)
        hint = f" (catalog has: {', '.join(near)})" if near else ""
        print(f"  finding: {provider} publishes apiProvider {slug!r}, which models.dev "
              f"does not carry{hint}")
    print(f"findings: {len(missing) or 'none'}")
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
