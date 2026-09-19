#!/usr/bin/env python3
"""Measure spawn-to-first-terminal-byte latency from identical configured cold state."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import statistics
import subprocess
import tempfile
import time
import traceback

from verification import (
    fixture_digest,
    opened_pty,
    prepare_home,
    provenance,
    terminate_process,
)


RUNS = 30
LIMIT_MS = 100.0
READ_TIMEOUT_SECONDS = 5.0


def measure_once(binary: Path, env: dict[str, str], cwd: Path, sample: dict) -> float:
    with opened_pty() as (master, slave):
        started = time.monotonic_ns()
        process = subprocess.Popen(
            [str(binary)],
            cwd=cwd,
            env=env,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
            close_fds=True,
        )
        try:
            slave.close()
            readable, _, _ = select.select([master], [], [], READ_TIMEOUT_SECONDS)
            if not readable:
                raise RuntimeError("timed out waiting for first terminal output")
            data = os.read(master.fileno(), 4096)
            if not data:
                raise RuntimeError("Kurama exited before rendering terminal output")
            elapsed_ms = (time.monotonic_ns() - started) / 1_000_000
            sample["first_bytes_hex"] = data.hex()
            if process.poll() is not None:
                raise RuntimeError(
                    f"Kurama exited during startup: {process.returncode}"
                )
            sample["elapsed_ms"] = elapsed_ms
            return elapsed_ms
        finally:
            sample["process_group_cleanup"] = terminate_process(process)
            sample["returncode_after_cleanup"] = process.returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", nargs="?", default="target/release/kurama", type=Path)
    parser.add_argument("--runs", type=int, default=RUNS)
    args = parser.parse_args()
    if args.runs <= 0:
        parser.error("--runs must be positive")
    binary = args.binary.resolve()
    result = {
        "schema_version": 2,
        "metric": "startup_ms",
        "provenance": provenance(binary),
        "parameters": {"runs": args.runs, "read_timeout_seconds": READ_TIMEOUT_SECONDS},
        "initial_state": "configured cold state restored before every sample; OS caches not flushed",
        "samples": [],
        "raw_samples": [],
        "median": None,
        "limit": LIMIT_MS,
        "exit_status": 1,
    }
    try:
        with tempfile.TemporaryDirectory(prefix="kurama-startup-") as temp:
            root = Path(temp) / "home"
            template = Path(temp) / "template"
            env = prepare_home(root)
            shutil.copytree(root, template)
            expected = fixture_digest(template)
            for index in range(args.runs):
                sample = {"index": index, "initial_state_sha256": None}
                result["raw_samples"].append(sample)
                shutil.rmtree(root)
                shutil.copytree(template, root)
                sample["initial_state_sha256"] = fixture_digest(root)
                if sample["initial_state_sha256"] != expected:
                    raise RuntimeError("startup fixture restore changed initial state")
                result["samples"].append(
                    measure_once(binary, env, root / "project", sample)
                )
        result["median"] = statistics.median(result["samples"])
        result["exit_status"] = 0 if result["median"] <= LIMIT_MS else 1
    except Exception as error:
        result["fatal_error"] = str(error)
        result["traceback"] = traceback.format_exc()
    print(json.dumps(result, allow_nan=False))
    return result["exit_status"]


if __name__ == "__main__":
    raise SystemExit(main())
