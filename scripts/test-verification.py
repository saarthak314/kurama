#!/usr/bin/env python3
"""Credential-free regression fixtures; run with Python 3.11+ on Linux or macOS."""

import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import verification

from verification import empty_output_dir, opened_pty, run_command, terminate_process


SCRIPTS = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(
        name.replace("-", "_"), SCRIPTS / f"{name}.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


harness = load("bench-harness")
idle = load("bench-idle")


class VerificationLifecycle(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="kurama-verification-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def executable(self, name, source):
        path = self.root / name
        path.write_text(f"#!{sys.executable}\n" + source)
        path.chmod(0o755)
        return path

    def orphan_fixture(self, name, payload):
        pid_path = self.root / f"{name}.pid"
        binary = self.executable(
            name,
            f"""
import os, signal, time
read_fd, write_fd = os.pipe()
child = os.fork()
if child == 0:
    os.close(read_fd)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    with open({str(pid_path)!r}, 'w') as record:
        record.write(str(os.getpid()))
    os.write(write_fd, b'ready')
    os.close(write_fd)
    while True:
        time.sleep(1)
os.close(write_fd)
os.read(read_fd, 5)
os.close(read_fd)
print({payload!r}, flush=True)
""",
        )

        # Independent emergency cleanup also runs when the tested runner fails.
        def cleanup_child():
            if pid_path.exists():
                try:
                    os.kill(int(pid_path.read_text()), 9)
                except ProcessLookupError:
                    pass

        self.addCleanup(cleanup_child)
        return binary, pid_path

    def assert_stopped(self, pid):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            state = run_command(
                ["ps", "-o", "stat=", "-p", str(pid)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            ).stdout.strip()
            # Orphans may briefly remain zombies under a slow container init.
            if not state or state.startswith("Z"):
                return
            time.sleep(0.02)
        self.fail(f"descendant {pid} is still running: {state}")

    def test_reaped_leader_does_not_hide_sigterm_ignoring_descendant(self):
        binary, pid_path = self.orphan_fixture("orphan", "done")
        process = subprocess.Popen(
            [sys.executable, str(binary)], stdout=subprocess.DEVNULL, start_new_session=True
        )
        self.addCleanup(terminate_process, process)
        process.wait(timeout=3)
        terminate_process(process)
        self.assert_stopped(int(pid_path.read_text()))

    def test_exiting_child_is_reaped_when_kernel_rejects_further_signals(self):
        release = self.root / "release-exit"
        binary = self.executable(
            "exiting",
            f"""
import pathlib, signal, time
def finish(signum, frame):
    while not pathlib.Path({str(release)!r}).exists():
        time.sleep(0.01)
    raise SystemExit(0)
signal.signal(signal.SIGTERM, finish)
print('ready', flush=True)
while True:
    time.sleep(1)
""",
        )
        process = subprocess.Popen(
            [sys.executable, str(binary)], stdout=subprocess.PIPE, start_new_session=True
        )
        self.addCleanup(terminate_process, process)
        self.addCleanup(process.stdout.close)
        self.assertEqual(process.stdout.readline(), b"ready\n")
        signal_group = verification.signal_group
        terminating = False

        def exiting_kernel(pgid, sig):
            nonlocal terminating
            if pgid == process.pid and sig in (signal.SIGTERM, signal.SIGKILL):
                if terminating and process.poll() is None:
                    release.touch()
                    raise PermissionError("kernel has begun process exit")
                terminating = True
            return signal_group(pgid, sig)

        with patch("verification.signal_group", side_effect=exiting_kernel):
            terminate_process(process)
        self.assertEqual(process.returncode, 0)

    def test_exact_child_benchmark_cleans_successful_orphan_and_keeps_resources(self):
        group = {"median_ms": 1, "p95_ms": 1, "samples": [{"elapsed_ms": 1}]}
        payload = json.dumps(
            {
                "benchmark": "kurama-tui",
                "release_build": True,
                "repetitions": 1,
                "expanded_scroll": group,
                "ignored_events": group,
                "compact_tool_preview": group,
            }
        )
        binary, pid_path = self.orphan_fixture("benchmark", payload)
        artifacts = self.root / "artifacts"
        artifacts.mkdir()
        result = harness.measure(Path(sys.executable), "kurama-tui", artifacts, 3, (str(binary),))
        self.assertEqual(result["exit_status"], 0, result)
        self.assertTrue(result["residual_group_cleanup"])
        self.assert_stopped(int(pid_path.read_text()))
        self.assertGreater(result["peak_rss_bytes"], 0)
        self.assertEqual(
            json.loads((artifacts / result["stdout"]).read_text())["benchmark"],
            "kurama-tui",
        )

    def test_timeout_keeps_parseable_failure_resources(self):
        binary = self.executable("stalled", "import time\ntime.sleep(60)\n")
        result = harness.measure(binary, "kurama-tui", self.root, 0.1)
        self.assertEqual(result["exit_status"], 124, result)
        saved = json.loads((self.root / "stalled.resources.json").read_text())
        self.assertTrue(saved["timed_out"])
        self.assertEqual(saved["returncode"], -9)

    def test_missing_binary_keeps_failure_json(self):
        result = harness.measure(self.root / "missing", "kurama-tui", self.root, 1)
        self.assertEqual(result["exit_status"], 127)
        saved = json.loads((self.root / "missing.resources.json").read_text())
        self.assertIn("fatal_error", saved)
        self.assertEqual(saved["status"], "failed")

    def test_nonempty_output_is_rejected_without_altering_previous_artifact(self):
        path = self.root / "artifacts"
        path.mkdir()
        previous = path / "previous.png"
        previous.write_bytes(b"previous run")
        with self.assertRaises(ValueError):
            empty_output_dir(path)
        self.assertEqual(previous.read_bytes(), b"previous run")

    def test_startup_restores_state_before_every_launch(self):
        binary = self.executable(
            "stateful",
            """
import os, pathlib, time
state = pathlib.Path(os.environ['HOME']) / '.kurama' / 'startup-witness'
previous = 'mutated' if state.exists() else 'absent'
state.write_text('mutated')
print('PREVIOUS_STATE=' + previous, flush=True)
time.sleep(60)
""",
        )
        command = run_command(
            [
                sys.executable,
                str(SCRIPTS / "bench-startup.py"),
                str(binary),
                "--runs",
                "2",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=20,
        )
        result = json.loads(command.stdout)
        self.assertNotIn("fatal_error", result)
        self.assertEqual(len(result["raw_samples"]), 2)
        for sample in result["raw_samples"]:
            self.assertIn(
                b"PREVIOUS_STATE=absent", bytes.fromhex(sample["first_bytes_hex"])
            )
        self.assertEqual(
            len({sample["initial_state_sha256"] for sample in result["raw_samples"]}), 1
        )

    def test_startup_and_idle_spawn_failure_emit_json(self):
        for script in ("bench-startup.py", "bench-idle.py"):
            with self.subTest(script=script):
                command = run_command(
                    [
                        sys.executable,
                        str(SCRIPTS / script),
                        str(self.root / "missing"),
                        "--runs",
                        "1",
                    ],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    timeout=20,
                )
                self.assertNotEqual(command.returncode, 0)
                result = json.loads(command.stdout)
                self.assertIn("fatal_error", result)
                self.assertIn("traceback", result)

    def test_spawn_failure_closes_both_pty_descriptors(self):
        descriptors = []
        with self.assertRaises(FileNotFoundError):
            with opened_pty() as (master, slave):
                descriptors = [master.fileno(), slave.fileno()]
                subprocess.Popen(
                    [str(self.root / "missing")], stdin=slave, stdout=slave
                )
        for descriptor in descriptors:
            with self.assertRaises(OSError):
                os.fstat(descriptor)

    def test_idle_window_drains_more_than_the_pty_capacity(self):
        marker = self.root / "writer-finished"
        binary = self.executable(
            "flood",
            f"""
import os, pathlib, time
print('ready', flush=True)
time.sleep(0.05)
payload = b'x' * 65536
for _ in range(16):
    offset = 0
    while offset < len(payload):
        offset += os.write(1, payload[offset:])
pathlib.Path({str(marker)!r}).write_text('finished')
time.sleep(60)
""",
        )
        old_idle, old_settle = idle.IDLE_SECONDS, idle.SETTLE_SECONDS
        idle.IDLE_SECONDS, idle.SETTLE_SECONDS = 1.0, 0.01
        try:
            sample = {}
            idle.measure_once(binary, dict(os.environ), self.root, sample)
        finally:
            idle.IDLE_SECONDS, idle.SETTLE_SECONDS = old_idle, old_settle
        self.assertEqual(marker.read_text(), "finished")


if __name__ == "__main__":
    unittest.main()
