#!/usr/bin/env python3
"""Check every provider endpoint against the host it is supposed to reach.

THE DEFECT THIS CATCHES is a request URL that changed host without anyone
noticing. Every one of these endpoints receives a credential -- a bearer token
in a header, or for the OAuth token endpoints a live REFRESH TOKEN in the
request body -- so a wrong host does not merely fail. It receives a working
credential, and the symptom afterwards is indistinguishable from an expired
login: the call fails, the account reads as dead, and an operator
re-authenticating does not fix it because the credential was never the problem.

WHY A SCRIPT RATHER THAN A TEST PER CONSTANT. There are 27 of these. Written as
27 unit tests each asserting one constant equals one literal, the population is
invisible: nothing says how many exist, nothing fails when a new provider adds
the 28th unchecked, and the obvious sweep -- "does a test mention this URL" --
is satisfied by a test that compares the constant to itself.

The manifest below is that population, enumerated. A new endpoint constant fails
this check until someone records its host, which is the point: the decision gets
made once, deliberately, instead of defaulting to whatever was typed.

WHAT IS AND IS NOT CHECKED. Only the HOST is pinned, not the path. A path is
routine to change with an upstream API and says nothing about where a credential
travels; the host is the thing that must not drift. Comparing full URLs would
make this fail on every ordinary path revision and it would be relaxed away.

Hosts were DERIVED from the source, not typed. That is not fastidiousness: the
first version of this manifest was written by hand and recorded
`mimo.xiaomi.com`, where the source says `platform.xiaomimimo.com`. The check
caught it on its first run -- but a pin written from recollection can encode the
very mistake it exists to prevent, and it presents as a correct test failing
against correct code, which is the most expensive way to be wrong here.

Regenerate after a deliberate endpoint change rather than editing by hand:
extract every `const *_URL/_BASE` from the production half of each provider
module and take `urlparse(...).hostname`. Then read the DIFF, which is the
review step -- a host that moved without a reason is the finding.

Exit codes: 0 clean, 1 findings, 2 refused to run (nothing examined).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path
from urllib.parse import urlparse

REPO = Path(__file__).resolve().parent.parent
SRC = REPO / "crates/quota-core/src"

# provider module -> constant -> expected host.
EXPECTED: dict[str, dict[str, str]] = {
    "amp": {"SETTINGS_URL": "ampcode.com"},
    "anthropic": {"USAGE_URL": "api.anthropic.com"},
    "antigravity": {
        "REMOTE_QUOTA_URL": "cloudcode-pa.googleapis.com",
        "TOKEN_URL": "oauth2.googleapis.com",
    },
    "codebuff": {"BASE_URL": "www.codebuff.com"},
    "codex": {"DEFAULT_BASE": "chatgpt.com"},
    "copilot": {"USAGE_URL": "api.github.com"},
    "cursor": {"USAGE_URL": "cursor.com"},
    "deepseek": {"BALANCE_URL": "api.deepseek.com"},
    "doubao": {"API_URL": "ark.cn-beijing.volces.com"},
    "elevenlabs": {"DEFAULT_BASE": "api.elevenlabs.io"},
    "factory": {"BILLING_LIMITS_URL": "api.factory.ai"},
    "gemini": {
        "LOAD_CODE_ASSIST_URL": "cloudcode-pa.googleapis.com",
        "PROJECTS_URL": "cloudresourcemanager.googleapis.com",
        "QUOTA_URL": "cloudcode-pa.googleapis.com",
        "TOKEN_URL": "oauth2.googleapis.com",
    },
    "grok": {"USAGE_URL": "grok.com"},
    "kilo": {"DEFAULT_TRPC_BASE": "app.kilo.ai"},
    "kimi_for_coding": {"USAGE_URL": "api.kimi.com"},
    "manus": {"CREDITS_URL": "api.manus.im"},
    "mimo": {"DETAIL_URL": "platform.xiaomimimo.com", "USAGE_URL": "platform.xiaomimimo.com"},
    "minimax": {"CHINA_API_BASE": "api.minimaxi.com", "GLOBAL_API_BASE": "api.minimax.io"},
    "neuralwatt": {"DEFAULT_BASE": "api.neuralwatt.com"},
    "ollama": {"SETTINGS_URL": "ollama.com"},
    "opencode": {"SERVER_BASE": "opencode.ai"},
    "qoder": {"USAGE_URL": "qoder.com"},
    "qwen_cloud": {
        "QUOTA_CONFIG_URL": "cs-data.qwencloud.com",
        "SUBSCRIPTION_URL": "cs-data.qwencloud.com",
        "USAGE_URL": "cs-data.qwencloud.com",
    },
    "sakana": {"BILLING_URL": "console.sakana.ai"},
    "synthetic": {"DEFAULT_BASE": "api.synthetic.new"},
    "warp": {"API_URL": "app.warp.dev"},
    "zai": {"DEFAULT_BASE": "api.z.ai"},
    "zenmux": {"SUBSCRIPTION_DETAIL_URL": "zenmux.ai"},
}

CONST = re.compile(r'const ([A-Z_]*(?:URL|BASE)[A-Z_]*): &str = "(https://[^"]+)"')

# Cut at the test MODULE, never at the first `#[cfg(test)]`.
#
# That attribute also sits on production items in this crate -- a test-only
# constructor beside the real one, a `pub mod` used only by tests -- and four
# provider modules have one BEFORE their test module today. Splitting on the
# first occurrence would silently stop reading those files partway, so an
# endpoint constant declared after that point would never be checked and the
# run would still report success.
#
# The mirror failure is over-inclusion: reading the whole file pulls in fixture
# URLs from the test module, which no credential ever reaches, and pins them as
# though they were real endpoints. Both directions report a clean, plausible
# number, which is why the boundary is stated here rather than left to a split.
#
# Same rule as scripts/prod_body.py, whose docstring records what a truncated
# scan costs.
TEST_MODULE = re.compile(r"#\[cfg\(test\)\]\s*(?://[^\n]*\n\s*)*mod\s+tests\b")


def production_body(source: str) -> str:
    """Return the source up to the test module."""
    match = TEST_MODULE.search(source)
    return source[: match.start()] if match else source


def main() -> int:
    if not SRC.is_dir():
        print(f"refusing: no source directory at {SRC}")
        return 2

    findings: list[str] = []
    checked = 0
    seen_modules = 0

    for path in sorted(SRC.glob("*.rs")):
        source = path.read_text(encoding="utf-8", errors="replace")
        # Production only: a fixture URL in a test module is not a request this
        # ever makes, and pinning one would fail on every harness change.
        body = production_body(source)
        found = CONST.findall(body)
        if not found:
            continue
        seen_modules += 1
        expected = EXPECTED.get(path.stem)
        if expected is None:
            findings.append(
                f"{path.stem}: has endpoint constants and no entry in this manifest -- "
                f"record the host it should reach"
            )
            continue
        for name, url in found:
            host = urlparse(url).hostname or ""
            want = expected.get(name)
            if want is None:
                findings.append(
                    f"{path.stem}::{name} is not in the manifest (currently {host})"
                )
                continue
            checked += 1
            if host != want:
                findings.append(
                    f"{path.stem}::{name} points at {host}, manifest says {want}"
                )

    if checked == 0:
        print("refusing: examined no endpoint constant at all -- the extractor is")
        print("broken, not the codebase; a zero here reads exactly like a clean run")
        return 2

    print(f"  modules with endpoint constants: {seen_modules}")
    print(f"  constants checked against a recorded host: {checked}")
    if findings:
        print(f"\nfindings: {len(findings)}")
        for finding in findings:
            print(f"  {finding}")
        return 1
    print("\nfindings: none")
    return 0


if __name__ == "__main__":
    sys.exit(main())
