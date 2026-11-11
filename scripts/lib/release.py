#!/usr/bin/env python3
"""Small, offline validators used by the release driver."""

import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
import zipfile


def semver(value):
    if not isinstance(value, str) or not re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value
    ):
        raise ValueError(f"expected X.Y.Z without leading zeroes, got {value!r}")
    return tuple(map(int, value.split(".")))


def check_version(version, tags):
    proposed = semver(version)
    released = []
    for tag in tags.splitlines():
        if tag == "v" + version:
            raise ValueError(f"tag {tag} already exists")
        try:
            released.append((semver(tag.removeprefix("v")), tag))
        except ValueError:
            continue
    if released and proposed <= max(released)[0]:
        raise ValueError(f"{version} must be greater than latest release {max(released)[1]}")


def unpack_core(archive, destination):
    archive = Path(archive)
    sidecar = Path(str(archive) + ".sha256").read_text().strip()
    match = re.fullmatch(r"([a-fA-F0-9]{64}) [ *](.+)", sidecar)
    if not match or match[2] != archive.name:
        raise ValueError("core checksum sidecar must name the downloaded ZIP")
    if hashlib.sha256(archive.read_bytes()).hexdigest() != match[1].lower():
        raise ValueError("core ZIP checksum mismatch")
    # Read only the root executable, never extract paths supplied by an archive.
    name = "ck-subc.exe" if "-windows-" in archive.name else "ck-subc"
    with zipfile.ZipFile(archive) as bundle:
        if bundle.namelist().count(name) != 1:
            raise ValueError(f"core ZIP must contain one root {name}")
        binary = Path(destination) / name
        binary.write_bytes(bundle.read(name))
        binary.chmod(0o755)
    return str(binary.resolve())


def crate_updates(version):
    semver(version)
    paths = [Path(f"crates/{name}/Cargo.toml") for name in ("quota-core", "quota-module")]
    replacements = []
    for path in paths:
        text = path.read_text()
        old = tomllib.loads(text)["package"]["version"]
        semver(old)
        if semver(version) <= semver(old):
            raise ValueError(f"{path}: requested version must exceed {old}")
        updated, count = re.subn(
            r'(?m)^version = "' + re.escape(old) + r'"$', f'version = "{version}"', text
        )
        if count != 1:
            raise ValueError(f"{path}: expected one explicit package version")
        replacements.append((path, updated))
    return replacements


def bump(version):
    for path, updated in crate_updates(version):
        path.write_text(updated)


def check_lock(before, after, version):
    old = tomllib.loads(Path(before).read_text())
    new = tomllib.loads(Path(after).read_text())
    for lock in (old, new):
        for name in ("quota-core", "quota-module"):
            packages = [p for p in lock["package"] if p["name"] == name and "source" not in p]
            if len(packages) != 1:
                raise ValueError(f"expected one workspace {name} in Cargo.lock")
            if lock is new and packages[0]["version"] != version:
                raise ValueError(f"Cargo.lock {name} does not match {version}")
            packages[0]["version"] = "WORKSPACE"
    if old != new:
        raise ValueError("Cargo.lock refresh changed more than the two workspace versions")


def index_matches(payload, version, floor):
    component = payload.get("components", {}).get("insula", {})
    return component.get("version") == version and component.get("requires_core") == floor


def publication_status(repo, tag):
    result = subprocess.run(
        ["gh", "api", f"repos/{repo}/releases/tags/{tag}"], text=True, capture_output=True
    )
    try:
        payload = json.loads(result.stdout)
    except ValueError:
        payload = {}
    if result.returncode:
        if payload.get("status") == "404":
            print(f"GitHub release/assets for {tag}: absent (API returned HTTP 404)")
        else:
            print(f"GitHub release/assets for {tag}: unknown; API query failed: {result.stderr.strip()}")
        return
    print(f"GitHub release: {payload['html_url']}")
    assets = payload["assets"]
    if not assets:
        print("published assets: none")
    for asset in assets:
        print(f"published asset: {asset['name']}")


def main():
    command, *args = sys.argv[1:]
    if command == "check-version":
        check_version(args[0], sys.stdin.read())
    elif command == "floor":
        floor = json.loads(Path(args[0]).read_text())["requires_core"]
        semver(floor)
        print(floor)
    elif command == "unpack-core":
        print(unpack_core(*args))
    elif command == "bump":
        bump(*args)
    elif command == "check-crates":
        crate_updates(*args)
    elif command == "check-lock":
        check_lock(*args)
    elif command == "publication":
        publication_status(*args)
    elif command == "index":
        if not index_matches(json.loads(Path(args[0]).read_text()), *args[1:]):
            return 1
    else:
        raise ValueError(f"unknown command {command}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, KeyError, OSError, zipfile.BadZipFile) as error:
        print(f"release: {error}", file=sys.stderr)
        sys.exit(2)
