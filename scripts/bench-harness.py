#!/usr/bin/env python3
"""Run the release control-plane and TUI benchmarks sequentially on Linux/macOS."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import signal
import subprocess
import sys
import threading
import time


BENCHMARKS = (
    ("control_plane_bench", "kurama-control-plane"),
    ("tui_bench", "kurama-tui"),
)


def write_json(path: Path, value: object) -> None:
    path.write_text(
        json.dumps(value, indent=2, allow_nan=False) + "\n", encoding="utf-8"
    )


def command_version(command: list[str]) -> dict[str, object]:
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=5)
        return {
            "command": command,
            "returncode": result.returncode,
            "stdout": result.stdout.strip(),
            "stderr": result.stderr.strip(),
        }
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"command": command, "error": str(error)}


def provenance() -> dict[str, object]:
    return {
        "started_at_utc": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "system": platform.system(),
        "machine": platform.machine(),
        "python": sys.version,
        "python_executable": sys.executable,
        "cwd": str(Path.cwd()),
        "argv": sys.argv,
        "revision": command_version(["git", "rev-parse", "HEAD"]),
        "rustc": command_version(["rustc", "--version", "--verbose"]),
        "ci": {
            key: os.environ[key]
            for key in (
                "GITHUB_SHA",
                "GITHUB_REF",
                "GITHUB_RUN_ID",
                "GITHUB_RUN_ATTEMPT",
                "GITHUB_REPOSITORY",
                "RUNNER_OS",
                "RUNNER_ARCH",
                "ImageOS",
                "ImageVersion",
            )
            if key in os.environ
        },
        "resource_method": "os.wait4(exact_child_pid, 0); not cumulative RUSAGE_CHILDREN",
        "peak_rss_native_unit": "bytes" if sys.platform == "darwin" else "KiB",
        "resource_scope": (
            "Per benchmark process, with descendant accounting as supplied by the OS. "
            "Peak RSS is not the sum of concurrent process-tree RSS. Wall time and CPU "
            "include process startup, warmups, setup, validation, and all measured samples; "
            "benchmark JSON retains its narrower internal latency measurements."
        ),
    }


def reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number: {value}")


def nonnegative_number(value: object) -> bool:
    return (type(value) is int and value >= 0) or (
        type(value) is float and math.isfinite(value) and value >= 0
    )


def validate_output(path: Path, benchmark: str) -> list[str]:
    try:
        with path.open(encoding="utf-8") as output:
            value = json.load(output, parse_constant=reject_constant)
    except (OSError, UnicodeError, ValueError, RecursionError) as error:
        return [f"stdout is not one valid JSON document: {error}"]
    if not isinstance(value, dict):
        return ["benchmark output must be a JSON object"]
    errors = []
    if value.get("benchmark") != benchmark:
        errors.append(f"expected benchmark {benchmark!r}")
    if value.get("release_build") is not True:
        errors.append("benchmark must report release_build=true")
    if "fatal_error" in value:
        errors.append(f"fatal_error: {value['fatal_error']}")
    if benchmark == "kurama-control-plane":
        if value.get("ok") is not True:
            errors.append("control-plane benchmark did not report ok=true")
        parameters = value.get("parameters")
        repetitions = (
            parameters.get("repetitions") if isinstance(parameters, dict) else None
        )
        scenarios = value.get("scenarios")
        if type(repetitions) is not int or repetitions <= 0:
            errors.append("missing or invalid control-plane repetitions")
        if not isinstance(scenarios, list) or not scenarios:
            return errors + ["missing control-plane scenarios"]
        for index, scenario in enumerate(scenarios):
            label = f"scenarios[{index}]"
            if not isinstance(scenario, dict):
                errors.append(f"{label} must be an object")
                continue
            if "fatal_error" in scenario:
                errors.append(f"{label} fatal_error: {scenario['fatal_error']}")
            if scenario.get("failed_samples_including_warmups") != 0:
                errors.append(f"{label} has failures or missing failure accounting")
            if scenario.get("successful_samples") != repetitions:
                errors.append(f"{label} did not complete all repetitions successfully")
            samples = scenario.get("samples")
            if not isinstance(samples, list) or not samples:
                errors.append(f"{label} has no raw samples")
                continue
            measured = 0
            for sample_index, sample in enumerate(samples):
                if not isinstance(sample, dict):
                    errors.append(f"{label}.samples[{sample_index}] must be an object")
                    continue
                measured += sample.get("warmup") is False
                if (
                    sample.get("errors") != []
                    or sample.get("turn_completed") is not True
                    or type(sample.get("warmup")) is not bool
                    or not nonnegative_number(sample.get("elapsed_ms"))
                ):
                    errors.append(
                        f"{label}.samples[{sample_index}] failed or is incomplete"
                    )
            if measured != repetitions:
                errors.append(
                    f"{label} raw measured sample count differs from repetitions"
                )
    else:
        repetitions = value.get("repetitions")
        if type(repetitions) is not int or repetitions <= 0:
            errors.append("missing or invalid TUI repetitions")
        for name in ("expanded_scroll", "ignored_events", "compact_tool_preview"):
            group = value.get(name)
            if not isinstance(group, dict):
                errors.append(f"missing TUI group {name}")
                continue
            if not all(
                nonnegative_number(group.get(key)) for key in ("median_ms", "p95_ms")
            ):
                errors.append(f"{name} has invalid latency statistics")
            samples = group.get("samples")
            if not isinstance(samples, list) or not samples:
                errors.append(f"{name} has no raw samples")
                continue
            if len(samples) != repetitions:
                errors.append(f"{name} raw sample count differs from repetitions")
            for index, sample in enumerate(samples):
                if not isinstance(sample, dict) or not nonnegative_number(
                    sample.get("elapsed_ms")
                ):
                    errors.append(f"{name}.samples[{index}] has invalid elapsed_ms")
    return errors


def measure(
    binary: Path, benchmark: str, output_dir: Path, timeout: float
) -> dict[str, object]:
    stdout_path = output_dir / f"{binary.name}.stdout.json"
    stderr_path = output_dir / f"{binary.name}.stderr.txt"
    resources_path = output_dir / f"{binary.name}.resources.json"
    result = {
        "command": [str(binary)],
        "benchmark": benchmark,
        "provenance": "summary.json",
        "stdout": stdout_path.name,
        "stderr": stderr_path.name,
        "timeout_seconds": timeout,
        "returncode": None,
        "wall_seconds": None,
        "user_cpu_seconds": None,
        "system_cpu_seconds": None,
        "peak_rss_bytes": None,
        "timed_out": False,
        "status": "starting",
    }
    write_json(resources_path, result)
    timed_out = threading.Event()
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        try:
            with binary.open("rb") as executable:
                result["binary_sha256"] = hashlib.file_digest(
                    executable, "sha256"
                ).hexdigest()
            started = time.monotonic()
            process = subprocess.Popen(
                [str(binary)],
                stdin=subprocess.DEVNULL,
                stdout=stdout,
                stderr=stderr,
                start_new_session=True,
            )
        except OSError as error:
            result["spawn_error"] = str(error)
            result["exit_status"] = 127 if isinstance(error, FileNotFoundError) else 126
            stderr.write((str(error) + "\n").encode())
        else:

            def kill_group() -> None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    return
                timed_out.set()

            # A one-shot watchdog also covers a benchmark stalled in synchronous I/O.
            # Only wait4 reaps this child: Popen.wait/poll would discard its rusage.
            watchdog = threading.Timer(timeout, kill_group)
            watchdog.daemon = True
            watchdog.start()
            try:
                _, status, usage = os.wait4(process.pid, 0)
                process.returncode = os.waitstatus_to_exitcode(status)
                result["wall_seconds"] = time.monotonic() - started
            finally:
                watchdog.cancel()
                watchdog.join()
                if process.returncode is None:
                    kill_group()
                    _, status, _ = os.wait4(process.pid, 0)
                    process.returncode = os.waitstatus_to_exitcode(status)
            result.update(
                {
                    "returncode": process.returncode,
                    "user_cpu_seconds": usage.ru_utime,
                    "system_cpu_seconds": usage.ru_stime,
                    "peak_rss_bytes": int(usage.ru_maxrss)
                    * (1 if sys.platform == "darwin" else 1024),
                    "timed_out": timed_out.is_set(),
                    "exit_status": (
                        124
                        if timed_out.is_set()
                        else process.returncode
                        if process.returncode >= 0
                        else 128 - process.returncode
                    ),
                }
            )
    result["validation_errors"] = validate_output(stdout_path, benchmark)
    if result["exit_status"] == 0 and result["validation_errors"]:
        result["exit_status"] = 1
    result["status"] = "passed" if result["exit_status"] == 0 else "failed"
    write_json(resources_path, result)
    return result


def positive_seconds(value: str) -> float:
    seconds = float(value)
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("timeout must be finite and positive")
    return seconds


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog=(
            "Uses only the Python standard library (Python 3.11+). Runs "
            "control_plane_bench, then tui_bench with their default parameters, even if "
            "the first fails. Keeps exact stdout JSON and stderr, per-child resource JSON, "
            "and summary.json with binary hashes and host/toolchain/CI provenance. "
            "os.wait4 supplies per-child CPU and peak RSS, normalized to bytes on "
            "Linux/macOS, not a cumulative RUSAGE_CHILDREN high-water mark. Exit status "
            "is the first failed run's status (signals: 128+signal, watchdog: 124, "
            "invalid/debug/failure JSON with zero child exit: 1). No latency thresholds. "
            "Use a fresh output directory for each measurement to retain earlier runs."
        ),
    )
    parser.add_argument(
        "--bin-dir", required=True, type=Path, help="release examples directory"
    )
    parser.add_argument(
        "--output-dir", required=True, type=Path, help="directory for raw artifacts"
    )
    parser.add_argument(
        "--timeout-seconds",
        type=positive_seconds,
        default=300.0,
        help="wall-clock watchdog per benchmark, including setup (default: 300)",
    )
    args = parser.parse_args()
    if sys.platform not in ("darwin", "linux") or not hasattr(os, "wait4"):
        parser.error("per-child RSS measurement requires Linux or macOS with os.wait4")
    output_dir = args.output_dir.resolve()
    try:
        output_dir.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        parser.error(f"cannot create output directory: {error}")
    summary = {
        "schema_version": 1,
        "provenance": provenance(),
        "runs": [],
        "status": "running",
        "exit_status": 0,
    }
    summary_path = output_dir / "summary.json"
    write_json(summary_path, summary)
    for executable, benchmark in BENCHMARKS:
        result = measure(
            args.bin_dir.resolve() / executable,
            benchmark,
            output_dir,
            args.timeout_seconds,
        )
        summary["runs"].append(result)
        if summary["exit_status"] == 0:
            summary["exit_status"] = result["exit_status"]
        write_json(summary_path, summary)
        if result["exit_status"]:
            print(
                f"{executable} failed: see {output_dir / (executable + '.resources.json')}",
                file=sys.stderr,
            )
    summary["status"] = "passed" if summary["exit_status"] == 0 else "failed"
    write_json(summary_path, summary)
    print(json.dumps(summary, allow_nan=False))
    return summary["exit_status"]


if __name__ == "__main__":
    raise SystemExit(main())
