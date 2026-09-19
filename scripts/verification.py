"""Small POSIX lifecycle and provenance helpers for the verification runners."""

from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import os
from pathlib import Path
import platform
import pty
import signal
import subprocess
import sys
import time


def signal_group(pgid: int, sig: int) -> bool:
    try:
        os.killpg(pgid, sig)
        return True
    except ProcessLookupError:
        return False


def cleanup_group(pgid: int, grace: float = 0.2) -> bool:
    """Clean the owned session even if its leader has already been reaped.

    Returns whether group members remained at cleanup. Zombies may keep a group
    visible until their parent reaps them; SIGKILL cannot remove zombie records.
    Descendants that deliberately create another session are outside this scope.
    """
    found = signal_group(pgid, signal.SIGTERM)
    if found:
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline:
            if not signal_group(pgid, 0):
                return True
            time.sleep(0.01)
        signal_group(pgid, signal.SIGKILL)
    return found


def terminate_process(process: subprocess.Popen) -> bool:
    """Reap an ordinary Popen child; wait4 users must call cleanup_group instead."""
    try:
        found = signal_group(process.pid, signal.SIGTERM)
    except PermissionError:
        # A child already exiting can reject even the first signal on Darwin.
        # Waiting and the final group cleanup still have to succeed.
        found = True
    try:
        process.wait(timeout=0.2)
    except subprocess.TimeoutExpired:
        pass
    finally:
        try:
            if process.returncode is None:
                try:
                    signal_group(process.pid, signal.SIGKILL)
                except PermissionError:
                    # Darwin may reject signals once exit begins. A bounded wait
                    # must still prove the child exited; group cleanup stays strict.
                    pass
                process.wait(timeout=5)
        finally:
            cleanup_group(process.pid)
    return found


@contextmanager
def opened_pty():
    master, slave = pty.openpty()
    # File objects make an early slave close idempotent, without risking a reused FD.
    try:
        master_file = os.fdopen(master, "rb", buffering=0)
    except BaseException:
        os.close(master)
        os.close(slave)
        raise
    with master_file:
        try:
            slave_file = os.fdopen(slave, "wb", buffering=0)
        except BaseException:
            os.close(slave)
            raise
        with slave_file:
            yield master_file, slave_file


def run_command(command, *, timeout=5.0, check=False, input=None, **kwargs):
    """communicate with bounded runtime and group cleanup, including spawn failures."""
    if input is not None:
        if "stdin" in kwargs:
            raise ValueError("input and stdin cannot both be supplied")
        kwargs["stdin"] = subprocess.PIPE
    process = subprocess.Popen(command, start_new_session=True, **kwargs)
    try:
        stdout, stderr = process.communicate(input=input, timeout=timeout)
        result = subprocess.CompletedProcess(
            command, process.returncode, stdout, stderr
        )
        if check:
            result.check_returncode()
        return result
    finally:
        terminate_process(process)


def command_version(command):
    try:
        result = run_command(
            command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        return {
            "command": command,
            "returncode": result.returncode,
            "stdout": result.stdout.strip(),
            "stderr": result.stderr.strip(),
        }
    except (OSError, subprocess.SubprocessError) as error:
        return {"command": command, "error": str(error)}


def provenance(binary: Path | None = None):
    result = {
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
    }
    if binary is not None:
        result["binary"] = str(binary)
        try:
            with binary.open("rb") as executable:
                result["binary_sha256"] = hashlib.file_digest(
                    executable, "sha256"
                ).hexdigest()
        except OSError as error:
            result["binary_error"] = str(error)
    return result


def empty_output_dir(path: Path):
    path.mkdir(parents=True, exist_ok=True)
    if next(path.iterdir(), None) is not None:
        raise ValueError(f"output directory must be empty: {path}")


def prepare_home(root: Path) -> dict[str, str]:
    """Configured cold-state fixture; config paths remain identical across restores."""
    import json

    bridge = root / "bin" / "claude"
    bridge.parent.mkdir(parents=True)
    bridge.write_text(
        "#!/usr/bin/env bash\n"
        "if [[ ${1:-} == --version ]]; then echo kurama-bench-bridge; exit 0; fi\n"
        "cat >/dev/null\n"
        'printf \'%s\\n\' \'{"type":"result","result":"bench","session_id":"bench"}\'\n',
        encoding="utf-8",
    )
    bridge.chmod(0o755)
    config_dir = root / ".kurama"
    config_dir.mkdir(mode=0o700)
    (config_dir / "config.toml").write_text(
        "version = 1\n"
        'default_profile = "bench"\n'
        'default_mode = "supervised"\n\n'
        "[profiles.bench]\n"
        'kind = "claude_cli"\n'
        'model = "bench"\n'
        "max_input_tokens = 128000\n"
        "max_output_tokens = 16000\n"
        f"command = {json.dumps(str(bridge))}\n\n"
        "[search]\n"
        'kind = "json"\n'
        'endpoint = "http://127.0.0.1:9/search"\n',
        encoding="utf-8",
    )
    (root / "project").mkdir()
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


def fixture_digest(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        digest.update(str(path.relative_to(root)).encode() + b"\0")
        digest.update(str(path.stat().st_mode).encode() + b"\0")
        if path.is_file():
            digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()
