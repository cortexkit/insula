#!/usr/bin/env python3
"""Refuse Cargo path dependencies outside the repository, including unused patches."""

import json
from pathlib import Path
import subprocess
import sys
import tomllib


def outside_repository(root, path):
    return not path.resolve().is_relative_to(root.resolve())


def declarations(table):
    for section, values in table.items():
        if not isinstance(values, dict):
            continue
        if section in ("dependencies", "dev-dependencies", "build-dependencies", "replace"):
            for name, spec in values.items():
                if isinstance(spec, dict) and "path" in spec:
                    yield name, spec["path"]
        elif section == "patch":
            for dependencies in values.values():
                for name, spec in dependencies.items():
                    if isinstance(spec, dict) and "path" in spec:
                        yield name, spec["path"]
        elif section == "workspace":
            yield from declarations(values)
        elif section == "target":
            for target in values.values():
                yield from declarations(target)


def self_check():
    # No real sibling is needed: containment is a property of resolved paths,
    # including paths that do not exist. Test both directions on every scan.
    root = Path(__file__).resolve().parent.parent
    planted = tomllib.loads('''
[dependencies]
inside = { path = "crates/quota-core" }
outside = { path = "../planted-sibling" }
[patch.crates-io]
unused = { path = "../planted-unused" }
[workspace.dependencies]
shared = { path = "../planted-shared" }
[target.'cfg(unix)'.build-dependencies]
build = { path = "../planted-build" }
''')
    refused = {name for name, path in declarations(planted) if outside_repository(root, root / path)}
    assert refused == {"outside", "unused", "shared", "build"}, "planted outside dependencies must be refused, inside must pass"


def main():
    self_check()
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

    for manifest in manifests:
        with manifest.open("rb") as source:
            for name, path in declarations(tomllib.load(source)):
                record(manifest, name, path)

    def report():
        outside = [entry for entry in sorted(examined) if outside_repository(root, Path(entry[2]))]
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
    except (AssertionError, OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"path dependencies: could not check: {error}", file=sys.stderr)
        sys.exit(2)
