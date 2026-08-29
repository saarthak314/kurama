#!/usr/bin/env python3
"""Measure time from process spawn to Kurama's first terminal bytes."""

from __future__ import annotations

import json
import os
from pathlib import Path
import pty
import select
import signal
import statistics
import subprocess
import sys
import tempfile
import time


RUNS = 30
LIMIT_MS = 100.0
READ_TIMEOUT_SECONDS = 5.0


def prepare_home(root: Path) -> dict[str, str]:
    bridge = root / "bin" / "claude"
    bridge.parent.mkdir(parents=True)
    bridge.write_text(
        "#!/usr/bin/env bash\n"
        "if [[ ${1:-} == --version ]]; then echo kurama-bench-bridge; exit 0; fi\n"
        "cat >/dev/null\n"
        "printf '%s\\n' '{\"type\":\"result\",\"result\":\"bench\",\"session_id\":\"bench\"}'\n",
        encoding="utf-8",
    )
    bridge.chmod(0o755)

    config_dir = root / ".kurama"
    config_dir.mkdir(mode=0o700)
    config = (
        "version = 1\n"
        'default_profile = "bench"\n'
        'default_mode = "supervised"\n\n'
        "[profiles.bench]\n"
        'kind = "claude_cli"\n'
        'model = "bench"\n'
        f"command = {json.dumps(str(bridge))}\n\n"
        "[search]\n"
        'kind = "json"\n'
        'endpoint = "http://127.0.0.1:9/search"\n'
    )
    (config_dir / "config.toml").write_text(config, encoding="utf-8")

    env = os.environ.copy()
    env.update(
        {
            "HOME": str(root),
            "PATH": f"{bridge.parent}{os.pathsep}{env.get('PATH', '')}",
            "TERM": "xterm-256color",
            "NO_COLOR": "1",
        }
    )
    return env


def terminate_group(process: subprocess.Popen[bytes]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=1.0)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=1.0)


def measure_once(binary: Path, env: dict[str, str], cwd: Path) -> float:
    master, slave = pty.openpty()
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
    os.close(slave)
    try:
        readable, _, _ = select.select([master], [], [], READ_TIMEOUT_SECONDS)
        if not readable:
            raise RuntimeError("timed out waiting for first terminal output")
        data = os.read(master, 4096)
        if not data:
            raise RuntimeError("Kurama exited before rendering terminal output")
        return (time.monotonic_ns() - started) / 1_000_000
    finally:
        terminate_group(process)
        os.close(master)


def main() -> int:
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/kurama").resolve()
    if not binary.is_file():
        print(f"missing binary: {binary}", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="kurama-startup-") as temp:
        root = Path(temp)
        env = prepare_home(root)
        project = root / "project"
        project.mkdir()
        samples = [measure_once(binary, env, project) for _ in range(RUNS)]

    median_ms = statistics.median(samples)
    print(json.dumps({"metric": "startup_ms", "samples": samples, "median": median_ms, "limit": LIMIT_MS}))
    return 0 if median_ms <= LIMIT_MS else 1


if __name__ == "__main__":
    raise SystemExit(main())
