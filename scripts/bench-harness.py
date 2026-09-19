#!/usr/bin/env python3
"""Run the release control-plane and TUI benchmarks sequentially on Linux/macOS."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import traceback

from verification import (
    cleanup_group,
    empty_output_dir,
    provenance as run_provenance,
    signal_group,
)


BENCHMARKS = (
    ("control_plane_bench", "kurama-control-plane"),
    ("tui_bench", "kurama-tui"),
)


def write_json(path: Path, value: object) -> None:
    path.write_text(
        json.dumps(value, indent=2, allow_nan=False) + "\n", encoding="utf-8"
    )


def provenance() -> dict[str, object]:
    return {
        **run_provenance(),
        "resource_method": "os.wait4(exact_child_pid, 0); not cumulative RUSAGE_CHILDREN",
        "peak_rss_native_unit": "bytes" if sys.platform == "darwin" else "KiB",
        "resource_scope": (
            "Exact-child wait4 accounting, including only descendants accounted by the OS. "
            "Unwaited or surviving descendants are not full process-tree accounting; residual "
            "group members are terminated after measurement. Peak RSS is not a tree sum. "
            "Wall/CPU include startup, warmups, setup, validation and all measured samples; "
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
    if benchmark == "kurama-adapters":
        return validate_adapters(value)
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
        names = {"expanded_scroll", "ignored_events", "compact_tool_preview"}
        names.update(
            name
            for name, group in value.items()
            if isinstance(group, dict) and "samples" in group
        )
        for name in sorted(names):
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


def validate_adapters(value: dict) -> list[str]:
    errors = []
    if value.get("schema_version") != 1:
        errors.append("adapter benchmark requires schema_version=1")
    if "fatal_error" in value:
        errors.append(f"fatal_error: {value['fatal_error']}")
    groups = value.get("benchmarks")
    if not isinstance(groups, list):
        return errors + ["missing adapter benchmarks"]
    expected = {
        "sse_byte_fragmented_32k",
        "sse_burst_10k_records",
        "html_tag_dense_2mb",
        "compatible_large_request",
        "small_staged_output",
        "bounded_output_1mb",
        "read_first_line_16mb",
    }
    seen = set()
    for index, group in enumerate(groups):
        if not isinstance(group, dict):
            errors.append(f"benchmarks[{index}] must be an object")
            continue
        name = group.get("name")
        if not isinstance(name, str):
            errors.append(f"benchmarks[{index}] has invalid name")
            continue
        if name in seen or name not in expected:
            errors.append(f"unexpected or duplicate adapter benchmark {name!r}")
        seen.add(name)
        for key in ("iterations_per_sample", "observable_size"):
            if type(group.get(key)) is not int or group[key] <= 0:
                errors.append(f"{name} has invalid {key}")
        samples = group.get("samples_ms")
        if (
            not isinstance(samples, list)
            or len(samples) != 5
            or not all(map(nonnegative_number, samples))
        ):
            errors.append(f"{name} requires five finite nonnegative raw samples")
        elif (
            not nonnegative_number(group.get("median_ms"))
            or group["median_ms"] != sorted(samples)[2]
        ):
            errors.append(f"{name} median disagrees with raw samples")
        if "fatal_error" in group:
            errors.append(f"{name} fatal_error: {group['fatal_error']}")
    if seen != expected:
        errors.append(f"missing adapter benchmarks: {sorted(expected - seen)}")
    return errors


def measure(
    binary: Path, benchmark: str, output_dir: Path, timeout: float, arguments: tuple[str, ...] = ()
) -> dict[str, object]:
    stdout_path = output_dir / f"{binary.name}.stdout.json"
    stderr_path = output_dir / f"{binary.name}.stderr.txt"
    resources_path = output_dir / f"{binary.name}.resources.json"
    result = {
        "command": [str(binary), *arguments],
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
        "residual_group_cleanup": False,
        "status": "starting",
        "exit_status": 1,
    }
    timed_out = threading.Event()
    try:
        write_json(resources_path, result)
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            with binary.open("rb") as executable:
                result["binary_sha256"] = hashlib.file_digest(
                    executable, "sha256"
                ).hexdigest()
            started = time.monotonic()
            process = subprocess.Popen(
                result["command"],
                stdin=subprocess.DEVNULL,
                stdout=stdout,
                stderr=stderr,
                start_new_session=True,
            )
            watchdog = None
            try:

                def timeout_group():
                    if signal_group(process.pid, signal.SIGKILL):
                        timed_out.set()

                # Only wait4 may reap this exact child; Popen.wait/poll loses rusage.
                watchdog = threading.Timer(timeout, timeout_group)
                watchdog.daemon = True
                watchdog.start()
                _, status, usage = os.wait4(process.pid, 0)
                process.returncode = os.waitstatus_to_exitcode(status)
                result.update(
                    {
                        "wall_seconds": time.monotonic() - started,
                        "returncode": process.returncode,
                        "user_cpu_seconds": usage.ru_utime,
                        "system_cpu_seconds": usage.ru_stime,
                        "peak_rss_bytes": int(usage.ru_maxrss)
                        * (1 if sys.platform == "darwin" else 1024),
                    }
                )
            finally:
                if watchdog is not None:
                    watchdog.cancel()
                    if watchdog.ident is not None:
                        watchdog.join()
                try:
                    if process.returncode is None:
                        signal_group(process.pid, signal.SIGKILL)
                        _, status, _ = os.wait4(process.pid, 0)
                        process.returncode = os.waitstatus_to_exitcode(status)
                finally:
                    result["residual_group_cleanup"] = cleanup_group(process.pid)
                    result["timed_out"] = timed_out.is_set()
            result["exit_status"] = (
                124
                if timed_out.is_set()
                else process.returncode
                if process.returncode >= 0
                else 128 - process.returncode
            )
    except Exception as error:
        result["fatal_error"] = str(error)
        result["traceback"] = traceback.format_exc()
        result["exit_status"] = 127 if isinstance(error, FileNotFoundError) else 1
    result["validation_errors"] = validate_output(stdout_path, benchmark)
    if result["exit_status"] == 0 and result["validation_errors"]:
        result["exit_status"] = 1
    result["status"] = "passed" if result["exit_status"] == 0 else "failed"
    try:
        write_json(resources_path, result)
    except OSError as error:
        result["artifact_error"] = str(error)
        result["exit_status"] = 1
        result["status"] = "failed"
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
        "--include-adapters",
        action="store_true",
        help="also require adapter_bench; omit for older two-binary baselines",
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
        empty_output_dir(output_dir)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    summary = {
        "schema_version": 2,
        "provenance": provenance(),
        "runs": [],
        "status": "running",
        "exit_status": 0,
    }
    summary_path = output_dir / "summary.json"

    def save_summary():
        try:
            write_json(summary_path, summary)
        except OSError as error:
            summary["artifact_error"] = str(error)
            summary["exit_status"] = summary["exit_status"] or 1
            summary["status"] = "failed"

    save_summary()
    benchmarks = BENCHMARKS + (
        (("adapter_bench", "kurama-adapters"),) if args.include_adapters else ()
    )
    for executable, benchmark in benchmarks:
        result = measure(
            args.bin_dir.resolve() / executable,
            benchmark,
            output_dir,
            args.timeout_seconds,
        )
        summary["runs"].append(result)
        if summary["exit_status"] == 0:
            summary["exit_status"] = result["exit_status"]
        save_summary()
        if result["exit_status"]:
            print(
                f"{executable} failed: see {output_dir / (executable + '.resources.json')}",
                file=sys.stderr,
            )
    summary["status"] = "passed" if summary["exit_status"] == 0 else "failed"
    save_summary()
    print(json.dumps(summary, allow_nan=False))
    return summary["exit_status"]


if __name__ == "__main__":
    raise SystemExit(main())
