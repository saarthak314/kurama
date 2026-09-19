#!/usr/bin/env python3
"""Verify packaged stdio compatibility and publish a checksummed platform manifest."""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tarfile

from verification import run_command

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "aarch64-apple-darwin": (b"\xcf\xfa\xed\xfe", 0x0100000C),
    "x86_64-apple-darwin": (b"\xcf\xfa\xed\xfe", 0x01000007),
    "aarch64-unknown-linux-gnu": (b"\x7fELF", 183),
    "x86_64-unknown-linux-gnu": (b"\x7fELF", 62),
}


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def checked_archive(directory, version, target):
    name = f"kurama-{version}-{target}.tar.gz"
    archive = directory / name
    expected, recorded_name = (directory / (name + ".sha256")).read_text().split()
    actual = digest(archive)
    if recorded_name != name or expected != actual:
        raise ValueError(f"archive checksum mismatch: {name}")
    with tarfile.open(archive, "r:gz") as bundle:
        binaries = [
            member
            for member in bundle.getmembers()
            if member.name.removeprefix("./") == "kurama"
        ]
        if (
            len(binaries) != 1
            or not binaries[0].isfile()
            or not 0 < binaries[0].size <= 10 * 1024 * 1024
        ):
            raise ValueError(
                f"archive needs one bounded regular kurama executable: {name}"
            )
        with bundle.extractfile(binaries[0]) as source:
            header = source.read(20)
        magic, cpu = TARGETS[target]
        if magic == b"\x7fELF":
            valid = (
                header[:6] == b"\x7fELF\x02\x01"
                and int.from_bytes(header[18:20], "little") == cpu
            )
        else:
            valid = header[:4] == magic and int.from_bytes(header[4:8], "little") == cpu
        if not valid:
            raise ValueError(f"archive architecture does not match {target}")
        with bundle.extractfile(binaries[0]) as source:
            binary_hash = hashlib.file_digest(source, "sha256").hexdigest()
    return {"archive": name, "sha256": actual, "binary_sha256": binary_hash}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--binary", type=Path)
    mode.add_argument("--combine", type=Path)
    parser.add_argument("--target", choices=sorted(TARGETS))
    parser.add_argument("--directory", type=Path, default=Path("dist"))
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location(
        "release_version", ROOT / "scripts/check-release-version.py"
    )
    release_version = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(release_version)
    expected = release_version.validate(ROOT, args.tag, args.binary)
    version = expected[1:]
    protocol_version = json.loads((ROOT / "protocol/sdk.fixtures.json").read_text())[
        "protocol_version"
    ]
    if args.binary is not None:
        if args.target is None:
            parser.error("--target is required with --binary")
        result = run_command(
            [str(args.binary.resolve()), "--stdio"],
            input='{"id":"1","method":"shutdown","params":{}}\n',
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=True,
            timeout=15,
        )
        frames = [json.loads(line) for line in result.stdout.splitlines()]
        if len(frames) != 2:
            raise ValueError("stdio probe must emit hello then shutdown response only")
        hello, closed = frames
        if (
            hello.get("type") != "hello"
            or hello.get("protocol_version") != protocol_version
            or hello.get("server_version") != version
            or not {
                "prompt",
                "stream",
                "approval",
                "cancel",
                "resume",
                "verify",
            }.issubset(hello.get("capabilities", []))
        ):
            raise ValueError(
                "packaged binary does not implement the expected SDK protocol"
            )
        if closed != {"type": "response", "id": "1", "result": {"closed": True}}:
            raise ValueError("stdio shutdown did not acknowledge clean closure")
        record = checked_archive(args.directory, version, args.target)
        if record["binary_sha256"] != digest(args.binary):
            raise ValueError("archive executable differs from the verified binary")
        metadata = {
            "schema_version": 1,
            "version": version,
            "protocol_version": protocol_version,
            "target": args.target,
            **record,
        }
        path = args.directory / f"kurama-{version}-{args.target}.metadata.json"
    else:
        platforms = {}
        metadata_paths = []
        for target in TARGETS:
            path = args.combine / f"kurama-{version}-{target}.metadata.json"
            record = json.loads(path.read_text())
            if (
                record.get("schema_version") != 1
                or record.get("version") != version
                or record.get("protocol_version") != protocol_version
                or record.get("target") != target
            ):
                raise ValueError(f"incompatible platform metadata: {target}")
            checked = checked_archive(args.combine, version, target)
            if any(record.get(key) != value for key, value in checked.items()):
                raise ValueError(
                    f"platform metadata differs from packaged bytes: {target}"
                )
            platforms[target] = checked
            metadata_paths.append(path)
        metadata = {
            "schema_version": 1,
            "version": version,
            "protocol_version": protocol_version,
            "platforms": platforms,
        }
        path = args.combine / f"kurama-{version}-platforms.json"
    path.write_text(json.dumps(metadata, indent=2) + "\n")
    if args.combine is not None:
        for fragment in metadata_paths:
            fragment.unlink()
    print(f"SDK release metadata OK: {path}")


if __name__ == "__main__":
    main()
