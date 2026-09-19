#!/usr/bin/env python3
"""Check release metadata without fetching dependencies (Python 3.11+)."""

import argparse
from pathlib import Path
import re
import subprocess
import sys
import tomllib


# SemVer 2.0: numeric core/prerelease identifiers cannot have leading zeroes.
NUMBER = r"(?:0|[1-9][0-9]*)"
PRERELEASE = rf"(?:{NUMBER}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
SEMVER = re.compile(
    rf"{NUMBER}\.{NUMBER}\.{NUMBER}"
    rf"(?:-{PRERELEASE}(?:\.{PRERELEASE})*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def load_toml(path):
    with path.open("rb") as source:
        return tomllib.load(source)


def validate(root, tag=None, binary=None):
    workspace = load_toml(root / "Cargo.toml")["workspace"]
    version = workspace["package"]["version"]
    if not isinstance(version, str) or not SEMVER.fullmatch(version):
        raise ValueError(f"workspace version must be valid SemVer, got {version!r}")
    expected = f"v{version}"
    if tag is not None and tag != expected:
        raise ValueError(f"release tag must be exactly {expected!r}, got {tag!r}")
    packages = load_toml(root / "Cargo.lock")["package"]
    # Workspace members are explicit paths in Cargo.toml, not a name prefix.
    for member in workspace["members"]:
        package = load_toml(root / member / "Cargo.toml")["package"]
        name = package["name"]
        if package["version"] != {"workspace": True}:
            raise ValueError(f"{name} must inherit workspace.package.version")
        locked = [p["version"] for p in packages if p["name"] == name and "source" not in p]
        if locked != [version]:
            raise ValueError(f"{name} lockfile version must be {version!r}, got {locked!r}")
    if binary is not None:
        result = subprocess.run(
            [str(binary.resolve()), "--version"],
            check=True, capture_output=True, text=True, timeout=30,
        )
        if result.stdout != f"kurama {version}\n":
            raise ValueError(f"binary --version must match {expected!r}, got {result.stdout!r}")
    return expected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", nargs="?", help="release tag; omit for source-only checks")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--binary", type=Path, help="also run this binary's --version")
    args = parser.parse_args()
    try:
        expected = validate(args.root, args.tag, args.binary)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"release version check failed: {error}", file=sys.stderr)
        return 1
    print(f"release version OK: {expected}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
