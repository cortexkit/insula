# Release notes

The public changelog is [GitHub Releases](https://github.com/cortexkit/insula/releases).
Each release body comes from `.cortexkit/release-notes/vX.Y.Z.md` at the tagged
commit. Write and commit that file before running `scripts/release.sh`.

Start with the commits since the last release tag (`git log vLAST..HEAD`) and
describe what changes for someone installing or upgrading insula. Use
[v0.2.0.md](v0.2.0.md) as a worked example: it explains the core requirement,
vault setup, provider changes and consumer-facing fields in user terms.

Use the sections below in this order; omit a section with nothing to say.
An opening paragraph should name the version and minimum CortexKit core version
from `release-manifest.json`. Keep each paragraph on one source line: GitHub
release bodies render single newlines as line breaks.

## Before you upgrade

Actions the user must take, removed features and compatibility breaks. Put the
upgrade consequence before the implementation detail. A breaking wire change
before 1.0 requires a minor version bump and an explicit warning here.

## New providers

Newly supported services, what usage they report and any setup needed.

## Provider updates

Changes to readings, limits, subscriptions, polling or authentication for
existing providers. Use the provider's public name.

## For apps and other modules

New or changed consumer behaviour: operations, fields, events and compatibility
requirements. Link to the consumer contract when readers need exact semantics.

## Fixes

Problems users could observe and how they now behave. Test-only changes and
refactors with no user-visible effect do not belong in a release body.

## Platforms

List the architectures actually shipped, including signing/notarization status
and any platform-specific limitation. Do not promise an asset that was not built.

---

Write for users, not for a development log. No internal ids, task labels, names
used only by development automation, audit text, private paths or personal identities. Public operation
and field names are useful when they explain an integration change. Review the
notes for clarity before tagging. Patch releases are fixes only; use a minor
release for new providers, new wire fields or behaviour changes.
