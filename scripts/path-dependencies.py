#!/usr/bin/env python3
"""Refuse Cargo path dependencies outside the repository, including unused patches."""

import json
from pathlib import Path
import subprocess
import sys
import tomllib


def main():
    root = Path(__file__).resolve().parent.parent
    # Metadata describes resolved dependencies, but omits unused patches and
    # manifests outside the workspace. Parse every versioned/unignored manifest
    # too, so those declarations cannot silently escape the repository boundary.
    files = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root,
    ).decode().split("\0")
    manifests = sorted({root / f for f in files if Path(f).name == "Cargo.toml"})
    examined = set()

    def record(manifest, name, path):
        resolved = (manifest.parent / path).resolve()
        examined.add((str(manifest.relative_to(root)), name, str(resolved)))

    def declarations(manifest, table):
        for section, values in table.items():
            if not isinstance(values, dict):
                continue
            if section in ("dependencies", "dev-dependencies", "build-dependencies", "replace"):
                for name, spec in values.items():
                    if isinstance(spec, dict) and "path" in spec:
                        record(manifest, name, spec["path"])
            elif section == "patch":
                for dependencies in values.values():
                    for name, spec in dependencies.items():
                        if isinstance(spec, dict) and "path" in spec:
                            record(manifest, name, spec["path"])
            elif section == "workspace":
                declarations(manifest, values)
            elif section == "target":
                for target in values.values():
                    declarations(manifest, target)

    for manifest in manifests:
        with manifest.open("rb") as source:
            declarations(manifest, tomllib.load(source))

    def report():
        outside = [entry for entry in sorted(examined) if not Path(entry[2]).is_relative_to(root)]
        print(f"path dependencies: {len(examined)} examined, {len(examined) - len(outside)} inside the repo, {len(outside)} outside ({len(manifests)} manifests)", flush=True)
        for manifest, name, path in outside:
            print(f"  REFUSED: {manifest}: {name} resolves outside the repository: {path}", flush=True)
        return bool(outside)

    # Refuse invalid declarations before Cargo tries to load an absent sibling.
    if report():
        return 1
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked"], cwd=root,
    ))
    for package in metadata["packages"]:
        manifest = Path(package["manifest_path"]).resolve()
        if manifest.is_relative_to(root):
            for dependency in package["dependencies"]:
                if dependency.get("path"):
                    record(manifest, dependency["name"], dependency["path"])
    if report():
        return 1
    if not examined:
        print("  REFUSED: no path dependencies examined; expected quota-module -> quota-core")
        return 1
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"path dependencies: could not check: {error}", file=sys.stderr)
        sys.exit(2)
