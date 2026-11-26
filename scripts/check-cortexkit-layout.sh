#!/usr/bin/env bash
# Nothing under .cortexkit/ may be tracked except the allowed folders and files.
#
# The fleet layout keeps internal material (design notes, investigations, tool state)
# under .cortexkit/, ignored by git. Git tracks only release-notes/, skills/, rules/,
# WORKER.md and top-level project settings files (.cortexkit/<module>.jsonc, which
# the daemon reads).
#
# The check lists every tracked path under .cortexkit/ and refuses any outside that
# allowlist. Listing only tracked files that are also ignored (`ls-files -ci`) is not
# enough: a stray `!` rule in any .gitignore un-ignores a private file, which is then
# tracked but not ignored, and that check passes.
#
# It also confirms the ignore block works (an internal path is ignored, an allowed one
# isn't), and self-tests the allowlist on planted paths. If a control fails or
# `git ls-files` fails, it reports itself BLIND and exits 2 rather than passing.
#
# Exit 0 clean, 1 a disallowed file is tracked, 2 the check can't see.

set -u
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 2

# Prints every input line that the layout does not allow under .cortexkit/.
disallowed() {
  grep -vE '^\.cortexkit/(release-notes/|skills/|rules/|WORKER\.md$|[a-z0-9-]+\.jsonc$)' | grep -v '^$'
}

planted_ok=$'.cortexkit/WORKER.md\n.cortexkit/release-notes/v1.md\n.cortexkit/skills/a/SKILL.md\n.cortexkit/rules/x.md\n.cortexkit/insula.jsonc'
planted_bad=$'.cortexkit/notes.md\n.cortexkit/internal-docs/designs/d.md\n.cortexkit/sub/insula.jsonc\n.cortexkit/WORKER.md.bak'
if [ -n "$(printf '%s\n' "$planted_ok" | disallowed)" ]; then
  echo "BLIND: the allowlist refuses an allowed path" >&2
  exit 2
fi
if [ "$(printf '%s\n' "$planted_bad" | disallowed | wc -l | tr -d ' ')" != 4 ]; then
  echo "BLIND: the allowlist admits a planted disallowed path" >&2
  exit 2
fi

if ! git check-ignore -q --no-index .cortexkit/internal-docs/designs/control.md; then
  echo "BLIND: .cortexkit/internal-docs/ is not ignored; the .cortexkit block is missing from .gitignore" >&2
  exit 2
fi
if git check-ignore -q --no-index .cortexkit/release-notes/control.md; then
  echo "BLIND: .cortexkit/release-notes/ is ignored; the block's allow lines are missing" >&2
  exit 2
fi

if ! all="$(git ls-files -- .cortexkit)"; then
  echo "BLIND: git ls-files failed" >&2
  exit 2
fi
bad="$(printf '%s\n' "$all" | disallowed)"
if [ -n "$bad" ]; then
  echo "tracked under .cortexkit/ outside the allowlist:" >&2
  printf '  %s\n' $bad >&2
  echo "move them to .cortexkit/internal-docs/ (or remove them) and 'git rm --cached' them" >&2
  exit 1
fi
echo "controls hold; $(printf '%s\n' "$all" | grep -c .) tracked paths under .cortexkit/, all allowed"
