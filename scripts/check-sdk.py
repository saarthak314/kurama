#!/usr/bin/env python3
"""Exercise Python/TypeScript clients and the native SDK against one offline scenario."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import select
import shlex
import subprocess
import sys
import tempfile
import threading
import time
import traceback

from verification import cleanup_group, empty_output_dir, provenance, run_command

ROOT = Path(__file__).resolve().parents[1]
CANARY = "sdk-secret-must-not-appear-in-protocol"
PROMPTS = [
    "SDK_SIMPLE",
    "SDK_STREAM",
    "SDK_WRITE",
    "SDK_NEEDS_APPROVAL",
    "SDK_CANCEL",
    "SDK_AFTER_CANCEL",
    "SDK_AGENTS",
    "SDK_RESUME",
]
REPLIES = [
    "SDK_SIMPLE_OK",
    "stream 世界\nfinished",
    "SDK_WRITE_DONE",
    "SDK_AFTER_CANCEL_OK",
    "SDK_AGENTS_DONE",
    "SDK_RESUME_OK",
]


class Provider(ThreadingHTTPServer):
    daemon_threads = False
    block_on_close = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.requests = []
        self.failures = []
        self.cancel_disconnected = threading.Event()
        self.stopping = threading.Event()
        self.lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def setup(self):
        self.request.settimeout(5)
        super().setup()

    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            length = int(self.headers["Content-Length"])
            if not 0 < length <= 4 * 1024 * 1024:
                raise ValueError("fixture request exceeded its bound")
            body = json.loads(self.rfile.read(length))
            with self.server.lock:
                self.server.requests.append(body)
            messages = body.get("messages", [])
            users = [
                message.get("content", "")
                for message in messages
                if message.get("role") == "user"
            ]
            authored = [
                text for text in users if isinstance(text, str) and text in PROMPTS
            ]
            latest = authored[-1] if authored else None
            is_child = any(
                isinstance(text, str) and "Objective: review SDK_CHILD" in text
                for text in users
            )
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()

            def event(delta, finish=None):
                value = {
                    "id": "fixture",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                }
                self.wfile.write(
                    ("data: " + json.dumps(value, ensure_ascii=False) + "\n\n").encode()
                )
                self.wfile.flush()

            if is_child:
                event({"content": "SDK_CHILD_DONE"})
            elif latest == "SDK_SIMPLE":
                event({"content": "SDK_SIMPLE_OK"})
            elif latest == "SDK_STREAM":
                event({"content": "stream 世界\n"})
                time.sleep(0.12)
                event({"content": "finished"})
            elif latest in ("SDK_WRITE", "SDK_NEEDS_APPROVAL"):
                last_user = max(
                    index
                    for index, message in enumerate(messages)
                    if message.get("role") == "user"
                    and message.get("content") == latest
                )
                results = [
                    message
                    for message in messages[last_user + 1 :]
                    if message.get("role") == "tool"
                ]
                if results:
                    if latest != "SDK_WRITE":
                        raise AssertionError("unhandled approval executed a write")
                    event({"content": "SDK_WRITE_DONE"})
                else:
                    filename = (
                        "sdk-result.txt"
                        if latest == "SDK_WRITE"
                        else "must-not-exist.txt"
                    )
                    arguments = {"path": filename, "content": "written\n"}
                    event(
                        {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "fixture_write",
                                    "type": "function",
                                    "function": {
                                        "name": "write",
                                        "arguments": json.dumps(arguments),
                                    },
                                }
                            ]
                        }
                    )
                    event({}, "tool_calls")
                    self.wfile.write(b"data: [DONE]\n\n")
                    self.wfile.flush()
                    return
            elif latest == "SDK_CANCEL":
                event({"content": "SDK_CANCEL_BEGIN"})
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline and not self.server.stopping.is_set():
                    ready, _, _ = select.select([self.connection], [], [], 0.05)
                    if ready and not self.connection.recv(1):
                        self.server.cancel_disconnected.set()
                        return
                raise AssertionError("cancelled provider stream remained connected")
            elif latest == "SDK_AFTER_CANCEL":
                event({"content": "SDK_AFTER_CANCEL_OK"})
            elif latest == "SDK_AGENTS":
                child_results = [
                    message
                    for message in messages
                    if str(message.get("content", "")).startswith("Child agent ")
                ]
                if child_results:
                    if not any(
                        "SDK_CHILD_DONE" in str(message.get("content", ""))
                        for message in child_results
                    ):
                        raise AssertionError("parent request omitted child evidence")
                    event({"content": "SDK_AGENTS_DONE"})
                else:
                    delegation = {
                        "agents": [
                            {
                                "objective": "review SDK_CHILD output",
                                "context_refs": [],
                                "write_scope": {"roots": [], "files": []},
                                "budget": {
                                    "max_input_tokens": 16000,
                                    "max_output_tokens": 4000,
                                    "max_turns": 2,
                                    "max_seconds": 30,
                                },
                                "depends_on": [],
                            }
                        ]
                    }
                    event(
                        {
                            "content": "<kurama_delegate>"
                            + json.dumps(delegation)
                            + "</kurama_delegate>"
                        }
                    )
            elif latest == "SDK_RESUME":
                if "SDK_AFTER_CANCEL" not in users:
                    raise AssertionError("resume omitted prior conversation context")
                event({"content": "SDK_RESUME_OK"})
            else:
                raise AssertionError(
                    "unexpected model call (verification must be model-free)"
                )
            event({}, "stop")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            return
        except Exception as error:
            with self.server.lock:
                self.server.failures.append(str(error))


def prepare(root, endpoint, binary, python_path, typescript_module):
    home, workspace, state = root / "home", root / "workspace", root / "state"
    for directory in (home, workspace, state):
        directory.mkdir(mode=0o700)
    (state / "config.toml").write_text(f"""version = 1
default_profile = "fixture"
default_mode = "supervised"
[profiles.fixture]
kind = "open_ai_compatible"
model = "fixture-model"
endpoint = {json.dumps(endpoint)}
max_input_tokens = 32000
max_output_tokens = 4000
[search]
kind = "json"
endpoint = "http://127.0.0.1:9/search"
""")
    checks = workspace / ".kurama"
    checks.mkdir()
    command = (
        shlex.quote(sys.executable)
        + " -c "
        + shlex.quote(
            "from pathlib import Path; Path('verified.txt').write_text('verified\\n'); print('verification ok')"
        )
    )
    failure = shlex.quote(sys.executable) + " -c " + shlex.quote("raise SystemExit(7)")
    (checks / "verification.toml").write_text(
        f"version = 1\n[recipes.quick]\ncommand = {json.dumps(command)}\ntimeout_ms = 10000\n"
        f"[recipes.fail]\ncommand = {json.dumps(failure)}\ntimeout_ms = 10000\n"
    )
    pid_file = root / "servers.jsonl"
    wrapper = root / "kurama-wrapper"
    wrapper.write_text(f"""#!{sys.executable}
import json, os, sys
with open({str(pid_file)!r}, 'a') as output:
    output.write(json.dumps({{"pid":os.getpid()}})+'\\n')
os.execv({str(binary)!r}, [{str(binary)!r}, *sys.argv[1:]])
""")
    wrapper.chmod(0o755)
    environment = dict(
        os.environ,
        HOME=str(home),
        SDK_WORKSPACE=str(workspace),
        SDK_STATE=str(state),
        SDK_ENDPOINT=endpoint,
        KURAMA_BIN=str(wrapper),
        KURAMA_SDK_MODULE=str(typescript_module),
        PYTHONPATH=str(python_path),
    )
    return workspace, state, pid_file, environment


def session_projection(events):
    authored = []
    terminals = []
    replies = []
    current = None
    for envelope in events:
        event = envelope["event"]
        if event["type"] == "user_message":
            current = event["text"] if event["text"] in PROMPTS else None
            if current:
                authored.append([current, event.get("explicit_delegation", False)])
        elif event["type"] == "verification_started":
            # Explicit checks are separate from the preceding conversational turn.
            current = None
        elif event["type"] in ("turn_completed", "turn_failed") and current:
            terminals.append([current, event["type"]])
        elif event["type"] == "assistant_message" and event["text"] in REPLIES:
            replies.append(event["text"])
    return {
        "user_turns": authored,
        "terminals": terminals,
        "assistant_replies": replies,
    }


def run_case(language, command, binary, output, python_path, typescript_module):
    output.mkdir()
    with (
        tempfile.TemporaryDirectory(prefix=f"kurama-sdk-{language}-") as temporary,
        ExitStack() as resources,
    ):
        provider = Provider()
        resources.callback(provider.server_close)
        worker = threading.Thread(
            target=provider.serve_forever, name=f"sdk-{language}-provider"
        )
        worker.start()
        resources.callback(worker.join)
        resources.callback(provider.shutdown)
        resources.callback(provider.stopping.set)
        endpoint = f"http://127.0.0.1:{provider.server_port}/v1"
        workspace, state, pid_file, environment = prepare(
            Path(temporary), endpoint, binary, python_path, typescript_module
        )
        result = None
        try:
            result = run_command(
                command,
                env=environment,
                cwd=ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=120,
            )
            (output / "stdout.json").write_text(result.stdout)
            (output / "stderr.txt").write_text(result.stderr)
            if result.returncode:
                raise RuntimeError(
                    f"{language} driver exited {result.returncode}; see {output / 'stderr.txt'}"
                )
            summary = json.loads(result.stdout)
            assert summary["replies"] == REPLIES
            assert summary["manual_approvals"] == 1
            assert not (workspace / "must-not-exist.txt").exists(), (
                "unapproved write happened"
            )
            assert provider.cancel_disconnected.wait(3), (
                "provider did not observe cancellation"
            )
            assert not provider.failures, provider.failures
            assert len(provider.requests) == 11, (
                "extra/missing model calls, or verification contacted model"
            )
            journal = state / "sessions" / summary["session_id"] / "events.jsonl"
            raw_events = journal.read_text()
            (output / "events.jsonl").write_text(raw_events)
            events = [json.loads(line) for line in raw_events.splitlines()]
            assert [event["sequence"] for event in events] == list(range(len(events)))
            assert (
                sum(event["event"]["type"] == "session_started" for event in events)
                == 1
            )
            assert (
                sum(event["event"]["type"] == "agent_completed" for event in events)
                == 1
            )
            assert (
                CANARY
                not in raw_events
                + result.stdout
                + result.stderr
                + json.dumps(provider.requests)
            )
            summary["projection"] = session_projection(events)
            assert [
                entry[0] for entry in summary["projection"]["user_turns"]
            ] == PROMPTS
            return summary
        finally:
            provider.stopping.set()
            (output / "requests.json").write_text(
                json.dumps(provider.requests, indent=2)
            )
            (output / "provider-errors.json").write_text(
                json.dumps(provider.failures, indent=2)
            )
            if pid_file.exists():
                # PIDs are recorded by our exec wrapper at birth; SDKs own these
                # separate process groups. Check and clean them even if a driver fails.
                remaining = []
                for record in map(json.loads, pid_file.read_text().splitlines()):
                    pid = record["pid"]
                    if cleanup_group(pid):
                        remaining.append(pid)
                if remaining and result is not None and result.returncode == 0:
                    raise AssertionError(
                        f"SDK close left process groups running: {remaining}"
                    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument(
        "--rust-client", type=Path, default=Path("target/debug/examples/sdk_contract")
    )
    parser.add_argument("--python-path", type=Path, default=ROOT / "sdk/python/src")
    parser.add_argument(
        "--typescript-module", type=Path, default=ROOT / "sdk/typescript/dist/index.js"
    )
    args = parser.parse_args()
    empty_output_dir(args.output)
    summary = {
        "schema_version": 1,
        "provenance": provenance(args.binary.resolve()),
        "cases": [],
        "passed": False,
    }
    try:
        commands = [
            ("rust", [str(args.rust_client.resolve())]),
            ("typescript", ["node", str(ROOT / "tests/fixtures/sdk/client.mjs")]),
            ("python", [sys.executable, str(ROOT / "tests/fixtures/sdk/client.py")]),
        ]
        for language, command in commands:
            summary["cases"].append(
                run_case(
                    language,
                    command,
                    args.binary.resolve(),
                    args.output / language,
                    args.python_path.resolve(),
                    args.typescript_module.resolve(),
                )
            )
        expected = summary["cases"][0]["projection"]
        assert all(case["projection"] == expected for case in summary["cases"]), (
            "durable behavior differs across SDKs"
        )
        summary["passed"] = True
    except Exception as error:
        summary["error"] = str(error)
        summary["traceback"] = traceback.format_exc()
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(
        json.dumps(
            {
                "output": str(args.output.resolve()),
                "passed": summary["passed"],
                "error": summary.get("error"),
            }
        )
    )
    return 0 if summary["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
