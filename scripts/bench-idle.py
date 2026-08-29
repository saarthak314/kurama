#!/usr/bin/env python3
"""Measure Kurama core-process RSS and CPU after five idle seconds."""

from __future__ import annotations

import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import tempfile
import time


IDLE_SECONDS = 5.0
RSS_LIMIT_KIB = 25_600
CPU_LIMIT_PERCENT = 0.5
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


def sample_process(pid: int) -> tuple[int, float]:
    result = subprocess.run(
        ["ps", "-o", "rss=,%cpu=", "-p", str(pid)],
        check=True,
        capture_output=True,
        text=True,
    )
    fields = result.stdout.split()
    if len(fields) != 2:
        raise RuntimeError(f"unexpected ps output: {result.stdout!r}")
    return int(fields[0]), float(fields[1])


def child_processes(pid: int) -> list[dict[str, object]]:
    result = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss=,%cpu=,comm="],
        check=True,
        capture_output=True,
        text=True,
    )
    children: list[dict[str, object]] = []
    for line in result.stdout.splitlines():
        fields = line.strip().split(maxsplit=4)
        if len(fields) == 5 and int(fields[1]) == pid:
            children.append(
                {
                    "pid": int(fields[0]),
                    "rss_kib": int(fields[2]),
                    "cpu_percent": float(fields[3]),
                    "command": fields[4],
                }
            )
    return children


def main() -> int:
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/kurama").resolve()
    if not binary.is_file():
        print(f"missing binary: {binary}", file=sys.stderr)
        return 2

    with tempfile.TemporaryDirectory(prefix="kurama-idle-") as temp:
        root = Path(temp)
        env = prepare_home(root)
        project = root / "project"
        project.mkdir()
        master, slave = pty.openpty()
        process = subprocess.Popen(
            [str(binary)],
            cwd=project,
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
            if not readable or not os.read(master, 4096):
                raise RuntimeError("Kurama did not render before the idle sample")
            time.sleep(IDLE_SECONDS)
            rss_kib, cpu_percent = sample_process(process.pid)
            children = child_processes(process.pid)
        finally:
            terminate_group(process)
            os.close(master)

    print(
        json.dumps(
            {
                "metric": "idle",
                "kurama": {
                    "rss_kib": rss_kib,
                    "rss_limit_kib": RSS_LIMIT_KIB,
                    "cpu_percent": cpu_percent,
                    "cpu_limit_percent": CPU_LIMIT_PERCENT,
                },
                "external_child_processes": children,
                "note": "Provider CLI and local-server processes are reported separately from Kurama.",
            }
        )
    )
    return 0 if rss_kib <= RSS_LIMIT_KIB and cpu_percent <= CPU_LIMIT_PERCENT else 1


if __name__ == "__main__":
    raise SystemExit(main())
