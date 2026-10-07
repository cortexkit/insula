#!/usr/bin/env bash
# Pick an installed TOML-capable Python, just as gates.sh does; do not fetch one.
set -euo pipefail
cd "$(dirname "$0")/.."
for candidate in python3 python3.14 python3.13 python3.12 python3.11; do
  if "$candidate" -c 'import tomllib' >/dev/null 2>&1; then
    exec "$candidate" scripts/mutation-scan-tests.py "$@"
  fi
done
echo 'scan controls require Python 3.11 or newer' >&2
exit 2
