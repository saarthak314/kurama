#!/usr/bin/env python3
"""Measure live core-process RSS and process-CPU deltas during drained idle windows."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import traceback

from verification import (
    fixture_digest,
    opened_pty,
    prepare_home,
    provenance,
    run_command,
    terminate_process,
)


IDLE_SECONDS = 5.0
SETTLE_SECONDS = 1.0
RSS_LIMIT_KIB = 25_600
CPU_LIMIT_PERCENT = 0.5
READ_TIMEOUT_SECONDS = 5.0
RUNS = 3


def ps(*arguments) -> str:
    return run_command(
        ["ps", *arguments],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=dict(os.environ, LC_ALL="C"),
    ).stdout


def cpu_seconds(pid: int) -> float:
    if sys.platform == "linux":
        # comm may contain spaces or parentheses; fields after its final ')' start at 3.
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    if sys.platform == "darwin":
        value = ps("-o", "time=", "-p", str(pid)).strip()
        days, separator, rest = value.partition("-")
        total = int(days) * 86400.0 if separator else 0.0
        components = (rest if separator else days).split(":")
        if not 2 <= len(components) <= 3:
            raise RuntimeError(f"unexpected ps CPU time: {value!r}")
        elapsed = 0.0
        for component in components:
            elapsed = elapsed * 60 + float(component)
        return total + elapsed
    raise RuntimeError("idle process CPU measurement requires Linux or macOS")


def drain(master: int, process: subprocess.Popen, seconds: float) -> int:
    deadline = time.monotonic() + seconds
    count = 0
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                f"Kurama exited during idle window: {process.returncode}"
            )
        readable, _, _ = select.select(
            [master], [], [], min(0.05, max(0, deadline - time.monotonic()))
        )
        if readable:
            data = os.read(master, 65536)
            if not data:
                raise RuntimeError("PTY closed during idle window")
            count += len(data)
    return count


def child_processes(pid: int) -> list[dict[str, object]]:
    children = []
    for line in ps("-axo", "pid=,ppid=,rss=,comm=").splitlines():
        fields = line.strip().split(maxsplit=3)
        if len(fields) == 4 and int(fields[1]) == pid:
            children.append(
                {"pid": int(fields[0]), "rss_kib": int(fields[2]), "command": fields[3]}
            )
    return children


def measure_once(binary: Path, env: dict[str, str], cwd: Path, sample: dict):
    with opened_pty() as (master, slave):
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
                raise RuntimeError("Kurama did not render before the idle sample")
            data = os.read(master.fileno(), 4096)
            if not data:
                raise RuntimeError("Kurama exited before rendering")
            sample["first_bytes_hex"] = data.hex()
            sample["settle_bytes_drained"] = drain(
                master.fileno(), process, SETTLE_SECONDS
            )
            cpu_start = cpu_seconds(process.pid)
            started = time.monotonic()
            sample["cpu_start_seconds"] = cpu_start
            sample["idle_bytes_drained"] = drain(master.fileno(), process, IDLE_SECONDS)
            cpu_end = cpu_seconds(process.pid)
            wall_seconds = time.monotonic() - started
            delta = cpu_end - cpu_start
            if delta < 0:
                raise RuntimeError("process CPU time decreased during idle sample")
            rss_kib = int(ps("-o", "rss=", "-p", str(process.pid)).strip())
            children = child_processes(process.pid)
            if rss_kib <= 0 or process.poll() is not None:
                raise RuntimeError("idle sample did not observe a live Kurama process")
            sample.update(
                {
                    "cpu_end_seconds": cpu_end,
                    "cpu_delta_seconds": delta,
                    "wall_seconds": wall_seconds,
                    "cpu_percent": delta / wall_seconds * 100,
                    "rss_kib": rss_kib,
                    "external_child_processes": children,
                    "alive_at_endpoint": True,
                }
            )
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
        "metric": "idle",
        "provenance": provenance(binary),
        "parameters": {
            "runs": args.runs,
            "idle_seconds": IDLE_SECONDS,
            "settle_seconds": SETTLE_SECONDS,
            "read_timeout_seconds": READ_TIMEOUT_SECONDS,
        },
        "cpu_method": "process CPU delta / drained wall interval; Linux /proc stat ticks, macOS ps time (0.01s resolution)",
        "initial_state": "configured cold state restored before every sample; OS caches not flushed",
        "samples": [],
        "kurama": {
            "rss_kib": None,
            "rss_limit_kib": RSS_LIMIT_KIB,
            "cpu_percent": None,
            "cpu_limit_percent": CPU_LIMIT_PERCENT,
        },
        "note": "Core-process medians only. Direct external children and their endpoint RSS are reported per sample, not included in core CPU/RSS.",
        "exit_status": 1,
    }
    try:
        with tempfile.TemporaryDirectory(prefix="kurama-idle-") as temp:
            root = Path(temp) / "home"
            template = Path(temp) / "template"
            env = prepare_home(root)
            shutil.copytree(root, template)
            expected = fixture_digest(template)
            for index in range(args.runs):
                sample = {"index": index, "initial_state_sha256": None}
                result["samples"].append(sample)
                shutil.rmtree(root)
                shutil.copytree(template, root)
                sample["initial_state_sha256"] = fixture_digest(root)
                if sample["initial_state_sha256"] != expected:
                    raise RuntimeError("idle fixture restore changed initial state")
                measure_once(binary, env, root / "project", sample)
        metrics = result["kurama"]
        metrics["rss_kib"] = statistics.median(
            sample["rss_kib"] for sample in result["samples"]
        )
        metrics["cpu_percent"] = statistics.median(
            sample["cpu_percent"] for sample in result["samples"]
        )
        result["exit_status"] = (
            0
            if metrics["rss_kib"] <= RSS_LIMIT_KIB
            and metrics["cpu_percent"] <= CPU_LIMIT_PERCENT
            else 1
        )
    except Exception as error:
        result["fatal_error"] = str(error)
        result["traceback"] = traceback.format_exc()
    print(json.dumps(result, allow_nan=False))
    return result["exit_status"]


if __name__ == "__main__":
    raise SystemExit(main())
