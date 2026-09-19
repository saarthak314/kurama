#!/usr/bin/env python3
"""Exercise the real CLI through a local SSE provider and a PTY, without credentials.

Run: uv run --with pyte --with pillow scripts/check-tui.py target/release/kurama target/verification/tui-check
Use --no-images to require only pyte. The VT model tracks separate primary and
alternate buffers; screenshots are decoded PTY output, not a native terminal
window capture. This is a POSIX-only development gate.
"""

import argparse
import copy
from contextlib import ExitStack
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import traceback
import zlib

from verification import (
    empty_output_dir,
    opened_pty,
    provenance,
    run_command,
    terminate_process,
)

import pyte

MARKDOWN = """## Rendering check

A **clear answer**, `inline_code`, and a [reference](https://example.com/reference).

- First item with enough words to wrap naturally in a narrow terminal.
- Second item
  - Nested detail

> A quoted observation that should keep its indentation when the window changes.

```rust
fn main() {
    println!("rendered exactly");
}
```

| Case | Result |
| --- | --- |
| Read | preserved |
| Write | approved |

RENDER_COMPLETE
"""


class Screen(pyte.HistoryScreen):
    def __init__(self, columns, lines):
        super().__init__(columns, lines, history=20000)
        self.primary = None
        self.restored_primary = None
        self.reply = lambda value: None

    def resize(self, lines=None, columns=None):
        lines, columns = lines or self.lines, columns or self.columns
        used = max(
            [self.cursor.y + 1]
            + [
                row + 1
                for row, cells in self.buffer.items()
                if any(cell.data.strip() for cell in cells.values())
            ]
        )
        shifted = max(0, min(used, self.lines) - lines)
        # pyte's default resize deletes top rows without archiving them. Model
        # cursor-anchored height shrink and preserve primary-screen scrollback.
        if shifted and self.primary is None:
            self.history.top.extend(
                copy.deepcopy(self.buffer[row]) for row in range(shifted)
            )
        old = self.buffer
        self.buffer = type(old)(
            old.default_factory,
            {
                row - shifted: cells
                for row, cells in old.items()
                if shifted <= row < shifted + lines
            },
        )
        if columns < self.columns:
            for cells in self.buffer.values():
                for column in list(cells):
                    if column >= columns:
                        del cells[column]
        self.lines, self.columns = lines, columns
        self.cursor.y = min(max(0, self.cursor.y - shifted), lines - 1)
        self.cursor.x = min(self.cursor.x, columns - 1)
        self.margins = None
        self.dirty.update(range(lines))

    def write_process_input(self, value):
        self.reply(value)

    def set_mode(self, *modes, **kwargs):
        if kwargs.get("private") and 1049 in modes:
            if self.primary is None:
                # Keep the saved primary buffer/history independent of every
                # subsequent alternate-buffer reset, clear, scroll, and resize.
                self.primary = copy.deepcopy(
                    {
                        k: v
                        for k, v in self.__dict__.items()
                        if k not in ("primary", "reply")
                    }
                )
                self.reset()
            modes = tuple(m for m in modes if m != 1049)
        super().set_mode(*modes, **kwargs)

    def reset_mode(self, *modes, **kwargs):
        if kwargs.get("private") and 1049 in modes:
            if self.primary is not None:
                columns, lines = self.columns, self.lines
                saved, self.primary = self.primary, None
                self.__dict__.update(saved)
                self.resize(lines=lines, columns=columns)
                self.restored_primary = (self.display, (self.cursor.x, self.cursor.y))
            modes = tuple(m for m in modes if m != 1049)
        super().reset_mode(*modes, **kwargs)

    def erase_in_display(self, how=0, *args, **kwargs):
        if how == 3:
            self.history.top.clear()
            self.history.bottom.clear()
            return
        super().erase_in_display(how, *args, **kwargs)

    def all_text(self):
        history = [
            "".join(cell.data for _, cell in sorted(line.items()))
            for line in self.history.top
        ]
        return "\n".join(history + self.display)

    def primary_text(self):
        if self.primary is None:
            return self.all_text()
        rows = list(self.primary["history"].top) + [
            self.primary["buffer"][row] for row in range(self.primary["lines"])
        ]
        return "\n".join(
            "".join(cell.data for _, cell in sorted(row.items())) for row in rows
        )


COLORS = {
    "default": "#d5dae3",
    "black": "#15191f",
    "red": "#e66b73",
    "green": "#98c379",
    "brown": "#e5c07b",
    "blue": "#61afef",
    "magenta": "#c678dd",
    "cyan": "#56b6c2",
    "white": "#d5dae3",
    "brightblack": "#7f8794",
    "brightwhite": "#ffffff",
}


def color(value, background=False):
    if value == "default" and background:
        return "#15191f"
    if value in COLORS:
        return COLORS[value]
    return "#" + value if len(value) == 6 else "#d5dae3"


def image(screen, path):
    from PIL import Image, ImageDraw, ImageFont

    try:
        fonts = [
            ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 16, index=index)
            for index in range(4)
        ]
    except OSError:
        names = [
            "DejaVuSansMono.ttf",
            "DejaVuSansMono-Bold.ttf",
            "DejaVuSansMono-Oblique.ttf",
            "DejaVuSansMono-BoldOblique.ttf",
        ]
        try:
            fonts = [ImageFont.truetype(name, 16) for name in names]
        except OSError:
            fonts = [ImageFont.load_default()] * 4
    cell_width, cell_height = 10, 23
    result = Image.new(
        "RGB",
        (screen.columns * cell_width + 32, screen.lines * cell_height + 32),
        "#15191f",
    )
    draw = ImageDraw.Draw(result)
    for row in range(screen.lines):
        for column in range(screen.columns):
            cell = screen.buffer[row][column]
            x, y = 16 + column * cell_width, 16 + row * cell_height
            fg, bg = color(cell.fg), color(cell.bg, True)
            if cell.reverse:
                fg, bg = bg, fg
            draw.rectangle((x, y, x + cell_width - 1, y + cell_height - 1), fill=bg)
            if cell.data.strip():
                draw.text(
                    (x, y),
                    cell.data,
                    font=fonts[int(cell.bold) + 2 * int(cell.italics)],
                    fill=fg,
                )
            if cell.underscore:
                draw.line((x, y + 20, x + cell_width - 1, y + 20), fill=fg)
    if not screen.cursor.hidden:
        x, y = 16 + screen.cursor.x * cell_width, 16 + screen.cursor.y * cell_height
        draw.line((x, y + 20, x + 9, y + 20), fill="#ffffff", width=2)
    result.save(path)


class Fixture(http.server.BaseHTTPRequestHandler):
    calls = 0
    compaction_inputs = []
    requests = []
    control_release = threading.Event()
    control_cancel_release = threading.Event()
    control_tool_proposed = False
    agents_issued = False

    def setup(self):
        self.request.settimeout(3)
        super().setup()

    def log_message(self, *args):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        messages = request.get("messages", [])
        system = next(
            (
                message.get("content", "")
                for message in messages
                if message.get("role") == "system"
            ),
            "",
        )
        is_compaction = str(system).startswith("Compact the supplied")
        latest_user = next(
            (
                message.get("content", "")
                for message in reversed(messages)
                if message.get("role") == "user"
            ),
            "",
        )
        # Context also includes synthetic user messages (for example session
        # todos). Route this fixture by its authored prompts, not their position.
        for message in reversed(messages):
            content = message.get("content", "")
            if (
                message.get("role") == "user"
                and isinstance(content, str)
                and (
                    content.startswith("CONTROL_")
                    or "FEEDBACK_E2E" in content
                    or content
                    in ("advance compaction fixture", "inspect fixture", "long answer")
                )
            ):
                latest_user = content
                break
        type(self).requests.append(request)
        advancing = latest_user == "advance compaction fixture"
        if not is_compaction and not advancing:
            type(self).calls += 1
        call = type(self).calls
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()

        def event(delta, finish=None):
            data = {
                "id": "fixture",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            }
            self.wfile.write(("data: " + json.dumps(data) + "\n\n").encode())
            self.wfile.flush()

        if latest_user == "CONTROL_BEGIN":
            event({"content": "CONTROL_MODEL_WAITING"})
            if not type(self).control_release.wait(30):
                raise RuntimeError("control fixture was not released")
            event({}, "stop")
        elif str(latest_user).startswith("CONTROL_AGENTS"):
            if not type(self).agents_issued:
                type(self).agents_issued = True
                delegation = {
                    "agents": [
                        {
                            "objective": f"review CHILD_E2E_{index} output",
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
                        for index in range(8)
                    ]
                }
                event(
                    {
                        "content": "<kurama_delegate>"
                        + json.dumps(delegation)
                        + "</kurama_delegate>"
                    }
                )
            else:
                child_results = [
                    message
                    for message in messages
                    if str(message.get("content", "")).startswith("Child agent ")
                ]
                assert len(child_results) == 8, "missing delegated results"
                assert all("NEW29" in message["content"] for message in child_results)
                event({"content": "CONTROL_AGENTS_DONE"})
            event({}, "stop")
        elif "CHILD_E2E_" in str(latest_user):
            body = (
                "OLD00\n"
                + "\n".join(
                    f"history {index}: " + "verified child output " * 3
                    for index in range(1, 29)
                )
                + "\nNEW29"
            )
            event({"content": body})
            event({}, "stop")
        elif latest_user == "CONTROL_CANCEL":
            event({"content": "CONTROL_CANCEL_WAITING"})
            type(self).control_cancel_release.wait(30)
            return
        elif latest_user == "CONTROL_STEER":
            assert not type(self).control_tool_proposed, "control tool repeated"
            type(self).control_tool_proposed = True
            event(
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "control_bash",
                            "type": "function",
                            "function": {
                                "name": "bash",
                                "arguments": json.dumps(
                                    {
                                        "command": "while [ ! -f release-tool ]; do sleep 0.02; done; printf CONTROL_TOOL_DONE",
                                        "cwd": ".",
                                        "timeout_ms": 30000,
                                    }
                                ),
                            },
                        }
                    ]
                }
            )
            event({}, "tool_calls")
        elif latest_user == "CONTROL_STEER_TOOL":
            assert "CONTROL_TOOL_DONE" in json.dumps(messages), "tool result lost"
            assert "CONTROL_BEGIN" in json.dumps(messages), "original user turn lost"
            event({"content": "CONTROL_STEERING_DONE"})
            event({}, "stop")
        elif latest_user == "CONTROL_SCROLL":
            event({"content": "CONTROL_SCROLL_DONE"})
            event({}, "stop")
        elif latest_user == "CONTROL_FOLLOWUP_EDITED":
            event({"content": "CONTROL_FOLLOWUP_DONE"})
            event({}, "stop")
        elif "FEEDBACK_E2E" in str(latest_user):
            assert "DIFF_FIRST_NEW" in latest_user, "selected hunk missing"
            assert "DIFF_FIRST_OLD" in latest_user, "old hunk missing"
            assert "DIFF_SECOND_NEW" not in latest_user, "unselected hunk leaked"
            assert "diff-control.txt" in latest_user, "selected path missing"
            event({"content": "CONTROL_FEEDBACK_DONE"})
            event({}, "stop")
        elif is_compaction:
            retained = "COMP_KEEP_ALPHA" in json.dumps(request)
            type(self).compaction_inputs.append(retained)
            summary = "COMP_KEEP_ALPHA" if retained else "PRIOR_DECISION_MISSING"
            event(
                {
                    "content": json.dumps(
                        {
                            "summary": summary,
                            "decisions": [summary],
                            "open_tasks": [],
                            "files": [],
                            "operation_ids": [],
                        }
                    )
                }
            )
            event({}, "stop")
        elif advancing:
            event({"content": "COMPACTION_ADVANCED"})
            event({}, "stop")
        elif call < 3:
            name = "read" if call == 1 else "write"
            args = (
                {"files": [{"path": "input.txt", "start_line": 1, "end_line": 10}]}
                if call == 1
                else {"path": "result.txt", "content": "approved result\n"}
            )
            event(
                {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": f"call_{call}",
                            "type": "function",
                            "function": {"name": name, "arguments": json.dumps(args)},
                        }
                    ]
                }
            )
            event({}, "tool_calls")
        else:
            content = (
                MARKDOWN
                if call == 3
                else "\n".join(f"History row {n:03}: preserved text" for n in range(90))
                + "\nLONG_COMPLETE"
            )
            for start in range(0, len(content), 35):
                event({"content": content[start : start + 35]})
                time.sleep(0.005)
            event({}, "stop")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("output")
    parser.add_argument("--no-cpr", action="store_true")
    parser.add_argument("--seed-todos", action="store_true")
    parser.add_argument(
        "--term", default="xterm-256color", help="TERM for the actual PTY child"
    )
    parser.add_argument(
        "--torn-tail",
        action="store_true",
        help="Resume a session with an incomplete final log record",
    )
    parser.add_argument(
        "--seed-large-output-mib",
        type=int,
        default=0,
        help="Resume a tool display blob of this size and exercise full-output expansion",
    )
    parser.add_argument(
        "--check-compaction",
        action="store_true",
        help="Compact twice and require the first decision in both model requests",
    )
    parser.add_argument(
        "--no-images", action="store_true", help="Skip optional Pillow screenshots"
    )
    parser.add_argument(
        "--check-controls",
        action="store_true",
        help="Exercise diff feedback, live steering, editable queue and context inspection",
    )
    args = parser.parse_args()
    if args.seed_large_output_mib < 0:
        parser.error("--seed-large-output-mib cannot be negative")
    output_dir = Path(args.output).resolve()
    try:
        empty_output_dir(output_dir)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    result = {
        "frames": [],
        "findings": {},
        "resources": {},
        "provenance": provenance(Path(args.binary).resolve()),
        "parameters": vars(args),
    }
    raw = bytearray()
    artifacts = {}
    status = 0
    try:
        run_check(args, output_dir, result, raw, artifacts)
    except Exception as error:
        result["fatal_error"] = str(error)
        result["traceback"] = traceback.format_exc()
        status = 1
    finally:
        result["exit_status"] = status
        artifacts.update(
            {
                "terminal.raw": bytes(raw),
                "provider-requests.json": json.dumps(
                    Fixture.requests, indent=2
                ).encode(),
            }
        )
        for name, content in artifacts.items():
            try:
                (output_dir / name).write_bytes(content)
            except OSError as error:
                result.setdefault("artifact_errors", []).append(f"{name}: {error}")
                result["exit_status"] = status = 1
        try:
            (output_dir / "audit.json").write_text(json.dumps(result, indent=2))
        except OSError as error:
            result.setdefault("artifact_errors", []).append(f"audit.json: {error}")
            result["exit_status"] = status = 1
    print(
        json.dumps(
            {
                "output": str(output_dir),
                "audit": str(output_dir / "audit.json"),
                "frames": len(result["frames"]),
                **result["findings"],
                "exit": result.get("exit_code"),
                "exit_status": status,
                "resources": result["resources"],
                **{
                    key: result[key]
                    for key in ("fatal_error", "traceback", "artifact_errors")
                    if key in result
                },
            }
        )
    )
    return status


def run_check(args, output_dir, result, raw, artifacts):
    seed_session = (
        args.seed_todos
        or args.torn_tail
        or args.seed_large_output_mib > 0
        or args.check_compaction
    )
    process = None
    thread = None
    with ExitStack() as cleanup:

        def closed():
            result["findings"]["pty_process_and_server_closed"] = (
                process is None or process.returncode is not None
            ) and (thread is None or not thread.is_alive())

        cleanup.callback(closed)
        root = Path(
            cleanup.enter_context(tempfile.TemporaryDirectory(prefix="kurama-tui-"))
        )
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
        server.daemon_threads = False
        cleanup.callback(server.server_close)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        cleanup.callback(thread.join)
        cleanup.callback(server.shutdown)
        cleanup.callback(Fixture.control_release.set)

        def collect_session_events():
            if args.check_controls:
                for log in (root / ".kurama" / "sessions").glob("*/events.jsonl"):
                    artifacts["session-events.jsonl"] = log.read_bytes()

        cleanup.callback(collect_session_events)
        cleanup.callback(Fixture.control_cancel_release.set)
        state = root / ".kurama"
        state.mkdir()
        (root / "input.txt").write_text("fixture input\n")
        if args.check_controls:

            def git(*arguments):
                return run_command(
                    ["git", *arguments],
                    cwd=root,
                    check=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )

            git("init", "-q")
            (root / ".gitignore").write_text(
                ".kurama/\nfixture-bin/\npointer-actions.jsonl\nrelease-tool\n"
            )
            diff_lines = [f"unchanged line {index}" for index in range(40)]
            diff_lines[2] = "DIFF_FIRST_OLD"
            diff_lines[32] = "DIFF_SECOND_OLD"
            (root / "diff-control.txt").write_text("\n".join(diff_lines) + "\n")
            (root / "staged.txt").write_text("STAGED_OLD\n")
            git("add", ".gitignore", "input.txt", "diff-control.txt", "staged.txt")
            git(
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture baseline",
            )
            diff_lines[2] = "DIFF_FIRST_NEW"
            diff_lines[32] = "DIFF_SECOND_NEW"
            (root / "diff-control.txt").write_text("\n".join(diff_lines) + "\n")
            (root / "staged.txt").write_text("STAGED_NEW\n")
            git("add", "staged.txt")
            (root / "untracked.txt").write_text("UNTRACKED_VISIBLE\n")
            (root / "binary.dat").write_bytes(b"\x00BINARY_VISIBLE\xff")
        pointer_log = root / "pointer-actions.jsonl"
        helper_directory = root / "fixture-bin"
        helper_directory.mkdir()
        helper_source = (
            f"#!{sys.executable}\n"
            + """import json
import os
from pathlib import Path
import sys

command = Path(sys.argv[0]).name
kind = "open" if command in ("open", "xdg-open") else "copy"
record = {"kind": kind, "command": command, "args": sys.argv[1:]}
if kind == "copy":
    record["text"] = sys.stdin.buffer.read().decode("utf-8")
payload = (json.dumps(record) + "\\n").encode("utf-8")
descriptor = os.open(
    os.environ["KURAMA_TUI_POINTER_LOG"], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600
)
try:
    os.write(descriptor, payload)
finally:
    os.close(descriptor)
"""
        )
        for name in ("open", "xdg-open", "pbcopy", "wl-copy", "xclip"):
            helper = helper_directory / name
            helper.write_text(helper_source)
            helper.chmod(0o700)
        (state / "config.toml").write_text(f"""version = 1
default_profile = "fixture"
default_mode = "supervised"
[profiles.fixture]
kind = "open_ai_compatible"
model = "fixture-model"
endpoint = "http://127.0.0.1:{server.server_port}/v1"
max_input_tokens = 32000
max_output_tokens = 4000
[search]
kind = "json"
endpoint = "http://127.0.0.1:9/search"
""")
        if seed_session:
            directory = state / "sessions" / "ui-session"
            directory.mkdir(parents=True)
            (directory / "agents").mkdir()
            metadata = {
                "id": "ui-session",
                "created_at_ms": 0,
                "project_root": str(root.resolve()),
                "profile": "fixture",
                "mode": "supervised",
                "redaction_best_effort": False,
            }
            (directory / "metadata.json").write_text(json.dumps(metadata))
            initial = [{"type": "session_started", "metadata": metadata}]
            if args.seed_todos:
                items = [
                    {"id": str(index), "content": f"task {index}", "status": "pending"}
                    for index in range(20)
                ]
                initial.append({"type": "todo_updated", "items": items})
            if args.seed_large_output_mib:
                size = args.seed_large_output_mib * 1024 * 1024
                line = b"large historical command output " + b"x" * 64 + b"\n"
                first = b"LARGE_HEAD_MARKER\n"
                last = b"\nLARGE_TAIL_MARKER\n"
                middle_size = size - len(first) - len(last)
                payload = (
                    first
                    + (line * ((middle_size + len(line) - 1) // len(line)))[
                        :middle_size
                    ]
                    + last
                )
                digest = hashlib.sha256(payload).hexdigest()
                blobs = state / "blobs"
                blobs.mkdir()
                (blobs / digest).write_bytes(payload)
                reference = {"sha256": digest, "bytes": len(payload)}
                result["resources"]["seed_blob_bytes"] = len(payload)
                initial.extend(
                    [
                        {"type": "user_message", "text": "Historical command output"},
                        {
                            "type": "tool_proposed",
                            "operation_id": "seed-operation",
                            "call_id": "seed-call",
                            "operation": {
                                "type": "bash",
                                "command": "printf historical",
                                "cwd": str(root.resolve()),
                                "class": "read_only",
                                "timeout_ms": 1000,
                            },
                        },
                        {
                            "type": "tool_invocation_recorded",
                            "operation_id": "seed-operation",
                            "invocation": {
                                "call_id": "seed-call",
                                "name": "bash",
                                "arguments": {
                                    "command": "printf historical",
                                    "timeout_ms": 1000,
                                },
                            },
                        },
                        {"type": "tool_prepared", "operation_id": "seed-operation"},
                        {"type": "tool_started", "operation_id": "seed-operation"},
                        {
                            "type": "tool_completed",
                            "operation_id": "seed-operation",
                            "result": {
                                "call_id": "seed-call",
                                "output": "Historical output is stored in a display blob.",
                                "is_error": False,
                                "truncated": True,
                                "blob_refs": [],
                                "metadata": {
                                    "tool_name": "bash",
                                    "display_blobs": {"output": reference},
                                },
                            },
                        },
                        {
                            "type": "assistant_message",
                            "text": "Historical command completed.",
                        },
                    ]
                )
                del payload
            initial.append({"type": "turn_completed"})
            if args.check_compaction:
                for index in range(5):
                    initial.extend(
                        [
                            {
                                "type": "user_message",
                                "text": "COMP_KEEP_ALPHA"
                                if index == 0
                                else f"Historical turn {index}",
                            },
                            {"type": "assistant_message", "text": "Recorded."},
                            {"type": "turn_completed"},
                        ]
                    )
            (directory / "events.jsonl").write_text(
                "".join(
                    json.dumps(
                        {
                            "schema_version": 1,
                            "sequence": sequence,
                            "timestamp_ms": sequence,
                            "session_id": "ui-session",
                            "agent_id": None,
                            "event": event,
                        }
                    )
                    + "\n"
                    for sequence, event in enumerate(initial)
                )
            )
            if args.torn_tail:
                with (directory / "events.jsonl").open("ab") as log:
                    log.write(b'{"schema_version":1')
        master_file, slave_file = cleanup.enter_context(opened_pty())
        master, slave = master_file.fileno(), slave_file.fileno()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 100, 0, 0))
        command = [
            "/bin/sh",
            "-c",
            'printf "SHELL_HISTORY_MARKER\\r\\n$ kurama\\r\\n"; exec "$@"',
            "fixture-shell",
            str(Path(args.binary).resolve()),
        ]
        if seed_session:
            command.extend(["--resume", "ui-session"])
        environment = dict(
            os.environ, HOME=str(root), TERM=args.term, COLORTERM="truecolor"
        )
        environment["PATH"] = (
            str(helper_directory) + os.pathsep + os.environ.get("PATH", os.defpath)
        )
        environment["KURAMA_TUI_POINTER_LOG"] = str(pointer_log)
        environment.pop("NO_COLOR", None)
        started = time.monotonic()
        process = subprocess.Popen(
            command,
            cwd=root,
            env=environment,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )

        def stop_process():
            result["resources"]["process_group_cleanup"] = terminate_process(process)

        cleanup.callback(stop_process)
        slave_file.close()
        screen = Screen(100, 36)
        screen.reply = lambda value: (
            None if args.no_cpr else os.write(master, value.encode())
        )
        stream = pyte.ByteStream(screen)

        def pump(seconds=0.1):
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                readable, _, _ = select.select(
                    [master], [], [], min(0.02, max(0, end - time.monotonic()))
                )
                if readable:
                    try:
                        data = os.read(master, 65536)
                    except OSError:
                        return
                    if not data:
                        return
                    raw.extend(data)
                    stream.feed(data)

        def wait_for(predicate, timeout=12):
            deadline = time.monotonic() + timeout
            while not predicate():
                pump(0.03)
                if predicate():
                    return
                exited = process.poll() is not None
                if exited and predicate():
                    return
                if time.monotonic() > deadline or exited:
                    raise RuntimeError(
                        "terminal condition failed:\n" + "\n".join(screen.display)
                    )

        def send(text):
            os.write(master, text if isinstance(text, bytes) else text.encode())
            pump(0.15)

        def mouse(button, point, release=False):
            column, row = point
            send(f"\x1b[<{button};{column + 1};{row + 1}{'m' if release else 'M'}")

        def visible_point(text):
            # These fixture tokens are ASCII, so character offsets are cells.
            for row, line in enumerate(screen.display):
                column = line.find(text)
                if column >= 0:
                    return column, row
            raise AssertionError(f"pointer target is not visible: {text}")

        def pointer_actions():
            if not pointer_log.exists():
                return []
            return [json.loads(line) for line in pointer_log.read_text().splitlines()]

        expected_opens = [["https://example.com/reference"]]

        def assert_pointer_actions(copies):
            actions = pointer_actions()
            assert [
                action["args"] for action in actions if action["kind"] == "open"
            ] == expected_opens, actions
            assert [
                action["text"] for action in actions if action["kind"] == "copy"
            ] == (copies), actions
            assert len(actions) == len(expected_opens) + len(copies), actions

        def contrast_ratio(foreground, background):
            def luminance(rgb):
                channels = [
                    int(rgb[index : index + 2], 16) / 255 for index in (0, 2, 4)
                ]
                channels = [
                    value / 12.92
                    if value <= 0.04045
                    else ((value + 0.055) / 1.055) ** 2.4
                    for value in channels
                ]
                return sum(
                    value * weight
                    for value, weight in zip(channels, (0.2126, 0.7152, 0.0722))
                )

            first, second = sorted([luminance(foreground), luminance(background)])
            return (second + 0.05) / (first + 0.05)

        def wait_for_copy(count):
            wait_for(
                lambda: (
                    sum(action["kind"] == "copy" for action in pointer_actions())
                    >= count
                )
            )
            text = [
                action["text"]
                for action in pointer_actions()
                if action["kind"] == "copy"
            ][-1]
            characters = len(text)
            label = f"Copied {characters} {'char' if characters == 1 else 'chars'}"
            wait_for(lambda: label in "\n".join(screen.display[-2:]))
            column, row = visible_point(label)
            badge = [
                screen.buffer[row][index]
                for index in range(column, column + len(label))
            ]
            assert all(
                cell.bold and contrast_ratio(cell.fg, cell.bg) >= 7 for cell in badge
            )
            result["findings"]["copied_count_and_high_contrast_badge"] = True

        def rss_kib():
            measured = run_command(
                ["ps", "-o", "rss=", "-p", str(process.pid)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                check=True,
            )
            return int(measured.stdout.strip())

        def composer_ready():
            if screen.cursor.hidden or not 0 <= screen.cursor.y < len(screen.display):
                return False
            before_cursor = screen.display[screen.cursor.y][: screen.cursor.x].rstrip()
            return before_cursor.endswith(">")

        def main_ready(composer_text):
            display = screen.display
            if screen.primary is None or "supervised" not in display[-1].lower():
                return False
            if set(display[-3].strip()) != {"─"}:
                return False
            if composer_text is None:
                return True
            if screen.cursor.hidden or screen.cursor.y != screen.lines - 4:
                return False
            if not composer_text:
                return composer_ready()
            expected = composer_text.split("\n")
            start = screen.cursor.y - len(expected) + 1
            actual = [line.strip() for line in display[start : screen.cursor.y + 1]]
            return actual == ["> " + expected[0], *expected[1:]] and (
                screen.cursor.x == 4 + len(expected[-1])
            )

        def capture(name, view="main", composer_text="", active=True):
            pump(0.15)
            if active:
                if view == "main":
                    wait_for(lambda: main_ready(composer_text))
                elif view == "transcript":
                    wait_for(
                        lambda: screen.cursor.hidden and any(
                            label in screen.display[-1].lower()
                            for label in ("esc", "release to copy", "copied")
                        )
                    )
                assert screen.primary is not None, (
                    "application left its alternate screen"
                )
                assert "SHELL_HISTORY_MARKER" not in screen.all_text()
                assert "SHELL_HISTORY_MARKER" in screen.primary_text()
                assert raw.count(b"\x1b[?1049h") == 1, "nested alternate-screen entry"
                assert raw.count(b"\x1b[?1049l") == 0, "overlay restored the shell"
            assert raw.count(b"\x1b[6n") + raw.count(b"\x1b[?6n") == 0, (
                "fullscreen startup must not request a cursor-position report"
            )
            assert raw.count(b"\x1b[3J") == 0, "application purged shell scrollback"
            if not args.no_images:
                image(screen, output_dir / (name + ".png"))
            has_input = active and view == "main" and composer_text is not None
            snapshot = {
                "name": name,
                "columns": screen.columns,
                "lines": screen.lines,
                "cursor": [screen.cursor.x, screen.cursor.y],
                "cursor_visible": not screen.cursor.hidden,
                "input_row": screen.cursor.y if has_input else None,
                "input_bottom_gap": screen.lines - 1 - screen.cursor.y
                if has_input
                else None,
                "footer_row": screen.lines - 1
                if active and view in ("main", "transcript")
                else None,
                "alternate_screen": screen.primary is not None,
                "observed_ms": (time.monotonic() - started) * 1000,
                "screen": screen.display,
                "history_rows": len(screen.history.top),
            }
            snapshot["raw_bytes"] = len(raw)
            snapshot["shell_history_visible"] = (
                "SHELL_HISTORY_MARKER" in screen.all_text()
            )
            snapshot["shell_history_saved"] = (
                "SHELL_HISTORY_MARKER" in screen.primary_text()
            )
            snapshot["history_head"] = [
                "".join(cell.data for _, cell in sorted(line.items()))
                for line in list(screen.history.top)[:3]
            ]
            result["frames"].append(snapshot)
            (output_dir / (name + ".txt")).write_text("\n".join(screen.display))
            return snapshot

        def resize(columns, lines):
            screen.resize(lines=lines, columns=columns)
            fcntl.ioctl(
                master, termios.TIOCSWINSZ, struct.pack("HHHH", lines, columns, 0, 0)
            )
            os.killpg(process.pid, signal.SIGWINCH)
            pump(0.25)

        def compacted_events():
            data = (state / "sessions" / "ui-session" / "events.jsonl").read_bytes()
            events = [
                json.loads(line)
                for line in data.splitlines(keepends=True)
                if line.endswith(b"\n")
            ]
            return [
                event["event"]
                for event in events
                if event["event"]["type"] == "context_compacted"
            ]

        try:
            wait_for(lambda: main_ready(""))
            initial_prompt = screen.display[screen.cursor.y][screen.cursor.x :].rstrip()
            result["resources"]["ready_ms"] = (time.monotonic() - started) * 1000
            result["resources"]["startup_rss_kib"] = rss_kib()
            capture("startup")
            result["findings"]["shell_history_hidden_while_active"] = True
            resize(44, 16)
            capture("startup-narrow")
            resize(100, 36)
            capture("startup-restored")
            if args.seed_large_output_mib:
                for _ in range(40):
                    if "LARGE_TAIL_MARKER" in screen.all_text():
                        break
                    send(b"\x1b[<64;5;10M")
                else:
                    raise AssertionError("large output preview was not reachable")
                capture("large-output-preview")
                send(b"\x0f")
                send(b"\x1b[H")
                wait_for(lambda: "LARGE_HEAD_MARKER" in "\n".join(screen.display))
                capture("large-output-expanded", view="transcript")
                result["resources"]["expanded_rss_kib"] = rss_kib()
                send(b"\x1b[F")
                for _ in range(40):
                    if "LARGE_TAIL_MARKER" in "\n".join(screen.display):
                        break
                    send(b"\x1b[<64;5;10M")
                else:
                    raise AssertionError("expanded large output tail was not reachable")
                send(b"\x1b")
                wait_for(composer_ready)
                capture("large-output-collapsed")
                result["resources"]["collapsed_rss_kib"] = rss_kib()
                result["findings"]["full_blob_accessible_on_expansion"] = True
            if args.seed_todos:
                send(b"\x14")
                send(b"\x1b[F")
                resize(24, 6)
                wait_for(lambda: "task 19" in "\n".join(screen.display))
                capture("todo-narrow-last", view="todos")
                assert "task 19" in "\n".join(screen.display)
                send(b"\x1b[H")
                wait_for(lambda: "task 0" in "\n".join(screen.display))
                capture("todo-narrow-first", view="todos")
                assert "task 0" in "\n".join(screen.display)
                send(b"\x1b")
                resize(100, 36)
                capture("todo-dismissed")
                result["findings"]["todo_first_and_last_accessible"] = True
            if args.check_compaction:
                send("/compact\r")
                wait_for(lambda: len(compacted_events()) == 1)
                send("advance compaction fixture\r")
                wait_for(lambda: "COMPACTION_ADVANCED" in screen.all_text())
                send("/compact\r")
                wait_for(lambda: len(compacted_events()) == 2)
                assert Fixture.compaction_inputs == [True, True], (
                    Fixture.compaction_inputs
                )
                assert "COMP_KEEP_ALPHA" in compacted_events()[-1]["summary"]
                capture("repeated-compaction")
                result["findings"]["prior_decision_survived_two_compactions"] = True
            send("/")
            capture("command-menu", composer_text="/")
            send(b"\x1b")
            send(b"\x03")
            send(b"\x1b[200~first line\nsecond line with editable text\x1b[201~")
            capture(
                "multiline-composer",
                composer_text="first line\nsecond line with editable text",
            )
            send(b"\x03")
            send("inspect fixture\r")
            wait_for(lambda: "approve once" in "\n".join(screen.display).lower())
            capture("approval", composer_text=None)
            responsive_sizes = [(48, 14), (72, 18), (120, 40)]
            for columns, lines in responsive_sizes:
                resize(columns, lines)
                capture(f"approval-{columns}x{lines}", composer_text=None)
                visible = "".join(line.strip() for line in screen.display)
                assert "result.txt" in visible
                assert "approve once" in visible
            resize(36, 14)
            capture("approval-narrow", composer_text=None)
            send("a")
            wait_for(lambda: (root / "result.txt").exists())
            wait_for(lambda: "RENDER_COMPLETE" in screen.all_text())
            capture("markdown-narrow")
            for columns, lines in responsive_sizes:
                resize(columns, lines)
                capture(f"markdown-{columns}x{lines}")
                assert "RENDER_COMPLETE" in "\n".join(screen.display)
                send(b"\x0f")
                send(b"\x1b[F")
                for label, content in [
                    (
                        "markdown-wrapped",
                        "First item with enough words to wrap naturally in a narrow terminal.",
                    ),
                    ("tool-output", "fixture input"),
                ]:
                    # Move by rows, not pages: a wrapped phrase must be visible
                    # in full, including when it would straddle two pages.
                    for _ in range(80):
                        visible = " ".join(" ".join(screen.display).split())
                        if content in visible:
                            break
                        send(b"\x1b[A")
                    else:
                        raise AssertionError(
                            f"{label} lost content at {columns}x{lines}: {content}"
                        )
                    capture(f"{label}-{columns}x{lines}", view="transcript")
                send(b"\x1b")
                capture(f"content-restored-{columns}x{lines}")
                assert "RENDER_COMPLETE" in "\n".join(screen.display)
            result["findings"]["responsive_content_sizes"] = responsive_sizes
            resize(100, 36)
            capture("markdown-wide")
            visible = "\n".join(screen.display)
            assert "fn main()" in visible
            assert "```" not in visible
            assert not any(line.strip() == "rust" for line in screen.display)
            result["findings"]["code_fences_and_language_hidden"] = True
            wait_for(lambda: "? shortcuts" in screen.display[-2])
            draft = "OVERLAY_DRAFT_MARKER"
            send(draft)
            capture("pointer-before", composer_text=draft)
            reference = visible_point("reference")
            link_cell = screen.buffer[reference[1]][reference[0]]
            url_column, url_row = visible_point("https://example.com/reference")
            url_cell = screen.buffer[url_row][url_column]
            assert (
                link_cell.fg == url_cell.fg
                and link_cell.underscore
                and url_cell.underscore
            )
            assert link_cell.fg != screen.buffer[reference[1]][0].fg
            result["findings"]["clickable_links_have_distinct_color"] = True
            assert pointer_actions() == []
            mouse(0, reference)
            assert pointer_actions() == [], "link opened before mouse release"
            mouse(0, reference, release=True)
            wait_for(lambda: len(pointer_actions()) >= 1)
            assert_pointer_actions([])
            capture("link-click", composer_text=draft)
            result["findings"]["plain_link_click_opens_exact_url"] = True

            code_text = 'println!("rendered exactly");'
            code_start = visible_point(code_text)
            code_end = (code_start[0] + len(code_text) - 1, code_start[1])
            original_backgrounds = [
                screen.buffer[code_start[1]][column].bg
                for column in range(code_start[0] - 1, code_end[0] + 2)
            ]
            mouse(0, code_start)
            mouse(32, code_end)
            capture("selection-dragging", composer_text=draft)
            selected_cells = [
                screen.buffer[code_start[1]][column]
                for column in range(code_start[0], code_end[0] + 1)
            ]
            assert len({cell.bg for cell in selected_cells}) == 1
            assert all(
                cell.fg != cell.bg and cell.bg != previous
                for cell, previous in zip(selected_cells, original_backgrounds[1:-1])
            )
            assert (
                screen.buffer[code_start[1]][code_start[0] - 1].bg
                == original_backgrounds[0]
            )
            assert (
                screen.buffer[code_end[1]][code_end[0] + 1].bg
                == original_backgrounds[-1]
            )
            assert_pointer_actions([])
            result["findings"]["drag_selection_highlighted"] = True
            mouse(0, code_end, release=True)
            copied_texts = [code_text]
            wait_for_copy(len(copied_texts))
            assert_pointer_actions(copied_texts)
            capture("selection-copied", composer_text=draft)
            result["findings"]["drag_selection_copies_exact_text"] = True
            result["findings"]["copied_selection_feedback_visible"] = True

            reference_end = (reference[0] + len("reference") - 1, reference[1])
            mouse(0, reference)
            assert_pointer_actions(copied_texts)
            mouse(32, reference_end)
            capture("link-selection-dragging", composer_text=draft)
            assert_pointer_actions(copied_texts)
            mouse(0, reference_end, release=True)
            copied_texts.append("reference")
            wait_for_copy(len(copied_texts))
            assert_pointer_actions(copied_texts)
            capture("link-selection-copied", composer_text=draft)
            result["findings"]["link_drag_copies_text_without_opening"] = True

            block_start = visible_point("fn main() {")
            block_end = (block_start[0], block_start[1] + 2)
            assert screen.display[block_end[1]][block_end[0]] == "}"
            mouse(0, block_end)
            mouse(32, block_start)
            capture("selection-backward-multiline-dragging", composer_text=draft)
            assert_pointer_actions(copied_texts)
            mouse(0, block_start, release=True)
            copied_texts.append("\n".join(("fn main() {", "    " + code_text, "}")))
            wait_for_copy(len(copied_texts))
            assert_pointer_actions(copied_texts)
            capture("selection-backward-multiline-copied", composer_text=draft)
            result["findings"][
                "backward_multiline_selection_copies_in_display_order"
            ] = True

            send(b"\x1b")
            capture("selection-dismissed", composer_text=draft)
            assert "Copied" not in "\n".join(screen.display[-2:])
            assert all(
                screen.buffer[row][column].bg != "cyan"
                for row in range(block_start[1], block_end[1] + 1)
                for column in range(screen.columns)
            )
            result["findings"]["selection_cleared_with_escape"] = True
            result["findings"]["composer_draft_survived_pointer_interactions"] = True
            send(b"\x0f")
            capture("expanded-copy-before", view="transcript")
            expanded_start = visible_point(code_text)
            expanded_end = (expanded_start[0] + len(code_text) - 1, expanded_start[1])
            mouse(0, expanded_start)
            mouse(32, expanded_end)
            mouse(0, expanded_end, release=True)
            copied_texts.append(code_text)
            wait_for_copy(len(copied_texts))
            assert_pointer_actions(copied_texts)
            capture("expanded-copy-count", view="transcript")
            send(b"\x1b")
            send(b"\x1b")
            capture("expanded-copy-restored", composer_text=draft)
            before_overlay = capture("draft-before-overlay", composer_text=draft)
            send(b"\x0f")
            capture("transcript-overlay", view="transcript")
            resize(44, 16)
            capture("overlay-resized", view="transcript")
            send(b"\x1b")
            capture("overlay-dismissed", composer_text=draft)
            resize(100, 36)
            after_overlay = capture("draft-after-resize", composer_text=draft)
            assert after_overlay["screen"] == before_overlay["screen"], (
                "overlay dismissal and resize left stale text or changed the draft"
            )
            result["findings"]["composer_draft_survived_overlay_and_resize"] = True
            send(b"\x03")
            send("long answer\r")
            wait_for(lambda: "LONG_COMPLETE" in screen.all_text())
            capture("long-answer")
            assert not any(
                line.strip() and set(line.strip()) == {"─"}
                for line in screen.display[: screen.cursor.y - 1]
            )
            result["findings"]["latest_output_has_no_separator"] = True
            wheel_draft = "WHEEL_DRAFT_MARKER"
            send(wheel_draft)
            wheel_before = capture("wheel-before", composer_text=wheel_draft)
            send(b"\x1b[<64;5;10M" * 3)
            wait_for(lambda: screen.display[0] != wheel_before["screen"][0])
            capture("wheel-older", composer_text=wheel_draft)
            assert "LONG_COMPLETE" not in "\n".join(screen.display)
            resize(48, 14)
            capture("wheel-older-resized", composer_text=wheel_draft)
            resize(100, 36)
            send(b"\x1b[<65;5;10M" * 100)
            wait_for(lambda: "LONG_COMPLETE" in "\n".join(screen.display))
            wheel_after = capture("wheel-latest", composer_text=wheel_draft)
            assert wheel_after["screen"] == wheel_before["screen"]
            result["findings"]["mouse_wheel_browses_output_without_changing_draft"] = (
                True
            )
            send(b"\x03")
            send(b"\x0f")
            for _ in range(100):
                if "RENDER_COMPLETE" in "\n".join(screen.display):
                    break
                send(b"\x1b[<64;5;10M")
            else:
                raise AssertionError(
                    "older output was not reachable with the mouse wheel"
                )
            previous_end = next(
                index
                for index, line in enumerate(screen.display)
                if "RENDER_COMPLETE" in line
            )
            assert any(
                line.strip() and set(line.strip()) == {"─"}
                for line in screen.display[previous_end + 1 : previous_end + 5]
            )
            capture("older-output-separator", view="transcript")
            result["findings"]["older_output_has_separator"] = True
            send(b"\x1b[H")
            capture("transcript-home", view="transcript")
            send(b"\x1b[F")
            wait_for(lambda: "LONG_COMPLETE" in "\n".join(screen.display))
            capture("transcript-end", view="transcript")
            send(b"\x1b")
            capture("transcript-restored")
            assert "LONG_COMPLETE" in "\n".join(screen.display)
            wait_for(composer_ready)
            current_prompt = screen.display[screen.cursor.y][screen.cursor.x :].rstrip()
            assert current_prompt == initial_prompt, (initial_prompt, current_prompt)
            result["findings"]["composer_prompt_stayed_stable"] = True
            result["findings"]["result_exact"] = (
                root / "result.txt"
            ).read_text() == "approved result\n"
            assert result["findings"]["result_exact"]
            logs = list((state / "sessions").glob("*/events.jsonl"))
            events = [json.loads(line) for line in logs[0].read_text().splitlines()]
            completions = [
                event["event"]["result"]
                for event in events
                if event["event"]["type"] == "tool_completed"
            ]
            result["findings"]["tools_completed_without_error"] = len(
                completions
            ) == 2 + int(args.seed_large_output_mib > 0) and all(
                not result["is_error"] for result in completions
            )
            assert result["findings"]["tools_completed_without_error"], completions
            if args.torn_tail:
                repairs = [
                    event
                    for event in events
                    if event["event"]["type"] == "recovery_repair"
                ]
                assert len(repairs) == 1, repairs
                assert [event["sequence"] for event in events] == list(
                    range(len(events))
                )
                result["findings"]["repaired_session_continued_once"] = True
            result["findings"]["main_footer_and_input_bottom_anchored"] = True
            result["findings"]["overlays_kept_alternate_screen"] = True
            result["findings"]["overlay_dismissal_cleared_controls"] = True
            if args.check_controls:

                def display_text():
                    return "\n".join(screen.display)

                def session_events():
                    return [
                        json.loads(line)["event"]
                        for line in logs[0].read_text().splitlines()
                    ]

                def inspect_context(name):
                    before = len(Fixture.requests)
                    send("/context\r")
                    wait_for(
                        lambda: (
                            "compaction" in display_text().lower()
                            and "estimated" in display_text().lower()
                        )
                    )
                    capture(name, view="context")
                    resize(48, 14)
                    capture(name + "-narrow", view="context")
                    send(b"\x1b[6~")
                    capture(name + "-scrolled", view="context")
                    send(b"\x1b")
                    resize(100, 36)
                    assert len(Fixture.requests) == before, "inspection called provider"

                inspect_context("context-idle")
                send("CONTROL_BEGIN\r")
                wait_for(lambda: "CONTROL_MODEL_WAITING" in display_text())
                send("CONTROL_STEER\r")
                send("CONTROL_FOLLOWUP_OLD\x1b\r")
                send("CONTROL_DELETE\x1b\r")
                send("/queue\r")
                wait_for(lambda: "CONTROL_DELETE" in display_text())
                capture("queue-two", view="queue")
                send(b"\x1b[B\x1b[3~")
                wait_for(lambda: "CONTROL_DELETE" not in display_text())
                send(b"\x1b[A\r")
                send(b"\x15CONTROL_FOLLOWUP_EDITED\r")
                wait_for(lambda: "CONTROL_FOLLOWUP_EDITED" in display_text())
                capture("queue-edited", view="queue")
                send(b"\x1b")
                inspect_context("context-streaming")
                assert not any(
                    "CONTROL_FOLLOWUP_EDITED" in json.dumps(request)
                    for request in Fixture.requests
                ), "follow-up ran while busy"
                Fixture.control_release.set()
                wait_for(lambda: "approve once" in display_text().lower())
                capture("steering-tool-approval", composer_text=None)
                send("a")
                wait_for(
                    lambda: (
                        any(
                            event.get("type") == "tool_started"
                            and event.get("operation_id") is not None
                            for event in session_events()
                        )
                        and "bash" in display_text().lower()
                    )
                )
                send("CONTROL_STEER_TOOL\r")
                inspect_context("context-tool-running")
                (root / "release-tool").touch()
                wait_for(lambda: "CONTROL_FOLLOWUP_DONE" in display_text())
                capture("steering-followup-complete")
                control_events = session_events()
                steering = [
                    event["text"]
                    for event in control_events
                    if event["type"] == "user_steered"
                ]
                assert steering == ["CONTROL_STEER", "CONTROL_STEER_TOOL"], steering
                control_users = [
                    event["text"]
                    for event in control_events
                    if event["type"] == "user_message"
                    and event["text"].startswith("CONTROL_")
                ]
                assert control_users == ["CONTROL_BEGIN", "CONTROL_FOLLOWUP_EDITED"], (
                    control_users
                )
                control_results = [
                    event["result"]
                    for event in control_events
                    if event["type"] == "tool_completed"
                    and event["result"].get("call_id") == "control_bash"
                ]
                assert (
                    len(control_results) == 1 and not control_results[0]["is_error"]
                ), control_results
                all_requests = json.dumps(Fixture.requests)
                assert "CONTROL_DELETE" not in all_requests
                assert "CONTROL_FOLLOWUP_OLD" not in all_requests
                result["findings"][
                    "steering_preserves_current_turn_and_tool_results"
                ] = True
                result["findings"]["edited_followup_runs_only_after_steered_work"] = (
                    True
                )
                result["findings"]["deleted_followup_never_reaches_provider"] = True
                result["findings"][
                    "context_inspection_idle_streaming_tool_no_provider_call"
                ] = True

                before_diff = (root / "diff-control.txt").read_bytes()
                send("/diff\r")
                wait_for(lambda: "hunk" in display_text().lower())
                seen = set()
                for _ in range(20):
                    text = display_text()
                    for marker in (
                        "DIFF_FIRST_NEW",
                        "DIFF_SECOND_NEW",
                        "STAGED_NEW",
                        "UNTRACKED_VISIBLE",
                        "binary.dat",
                    ):
                        if marker in text:
                            seen.add(marker)
                    if len(seen) == 5:
                        break
                    send("n")
                assert len(seen) == 5, seen
                capture("diff-review", view="diff")
                for _ in range(20):
                    if "DIFF_FIRST_NEW" in display_text():
                        break
                    send("n")
                else:
                    raise AssertionError("first diff hunk not reachable")
                capture("diff-first-hunk", view="diff")
                resize(48, 14)
                capture("diff-narrow", view="diff")
                send(b"\x1b[6~\x1b[5~")
                resize(100, 36)
                send("\r")
                wait_for(lambda: "Feedback:" in display_text())
                capture("diff-feedback-draft", composer_text=None)
                send("FEEDBACK_E2E\r")
                wait_for(lambda: "CONTROL_FEEDBACK_DONE" in display_text())
                capture("diff-feedback-complete")
                assert (root / "diff-control.txt").read_bytes() == before_diff
                result["findings"]["diff_reviews_staged_unstaged_untracked_binary"] = (
                    True
                )
                result["findings"][
                    "selected_hunk_feedback_reaches_provider_exactly"
                ] = True
                result["findings"]["diff_review_does_not_mutate_files"] = True
                inspect_context("context-after-controls")
                send(b"\x1b[<64;5;10M" * 30)
                send("CONTROL_SCROLL\r")
                wait_for(lambda: "CONTROL_SCROLL_DONE" in display_text())
                capture("new-turn-returns-to-latest")
                result["findings"]["new_turn_restores_live_transcript_position"] = True
                send("CONTROL_CANCEL\r")
                wait_for(lambda: "CONTROL_CANCEL_WAITING" in display_text())
                send("CONTROL_RETURNED\r")
                send(b"\x1b")
                wait_for(
                    lambda: (
                        any(
                            event["type"] == "turn_failed" for event in session_events()
                        )
                        and "CONTROL_RETURNED" in display_text()
                    )
                )
                capture("cancel-restores-steering", composer_text="CONTROL_RETURNED")
                Fixture.control_cancel_release.set()
                assert not any(
                    "CONTROL_RETURNED" in json.dumps(request)
                    for request in Fixture.requests
                )
                assert not any(
                    event["type"] == "user_steered"
                    and event["text"] == "CONTROL_RETURNED"
                    for event in session_events()
                )
                send(b"\x03")
                capture("cancel-draft-cleared")
                result["findings"][
                    "cancel_returns_unapplied_steering_without_autorun"
                ] = True

                resize(24, 8)
                send("?")
                send(b"\x1b[F")
                wait_for(lambda: "shift+tab" in display_text().lower())
                capture("shortcuts-last-narrow", view="shortcuts")
                send(b"\x1b[H")
                wait_for(lambda: "ctrl+c" in display_text().lower())
                capture("shortcuts-first-narrow", view="shortcuts")
                resize(100, 36)
                capture("shortcuts-wide", view="shortcuts")
                send(b"\x1b")
                result["findings"]["shortcuts_reachable_at_short_heights"] = True

                send(b"\x0f")
                for _ in range(32):
                    if "reference" in display_text():
                        break
                    send(b"\x1b[5~")
                else:
                    raise AssertionError("keyboard link fixture not reachable")
                send(b"\t")
                column, row = visible_point("reference")
                assert any(
                    screen.buffer[row][x].reverse for x in range(column, column + 9)
                )
                capture("keyboard-link-focused", view="transcript")
                expected_opens.append(["https://example.com/reference"])
                send("\r")
                wait_for(
                    lambda: (
                        sum(action["kind"] == "open" for action in pointer_actions())
                        == len(expected_opens)
                    )
                )
                send("y")
                copied_texts.append("https://example.com/reference")
                wait_for_copy(len(copied_texts))
                assert_pointer_actions(copied_texts)
                capture("keyboard-link-copied", view="transcript")
                send(b"\x1b")
                send(b"\x1b")
                capture("keyboard-links-restored")
                result["findings"]["keyboard_links_open_and_copy_exact_targets"] = True

                workflow = root / ".github" / "workflows"
                workflow.mkdir(parents=True)
                (workflow / "audit.yml").write_text("name: fixture\n")
                send("@.github/")
                wait_for(lambda: ".github/workflows/audit.yml" in display_text())
                capture("mention-dot-directory", composer_text="@.github/")
                send(b"\x03")
                (root / "new_late.rs").write_text("fn refreshed() {}\n")
                send("@new_late")
                wait_for(lambda: "new_late.rs" in display_text())
                capture("mention-refreshed", composer_text="@new_late")
                send(b"\x03")
                result["findings"][
                    "mention_index_refreshes_and_includes_dot_directories"
                ] = True

                def png_chunk(kind, payload):
                    return (
                        struct.pack(">I", len(payload))
                        + kind
                        + payload
                        + struct.pack(">I", zlib.crc32(kind + payload))
                    )

                png = (
                    b"\x89PNG\r\n\x1a\n"
                    + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
                    + png_chunk(b"IDAT", zlib.compress(b"\x00" * 5))
                    + png_chunk(b"IEND", b"")
                )
                (root / "R0lGOD-screenshot.png").write_bytes(png)
                paste_dir = root / ".kurama" / "paste"
                before_paste = set(paste_dir.glob("*"))
                send(b"\x1b[200~R0lGOD-screenshot.png\x1b[201~")
                wait_for(lambda: len(set(paste_dir.glob("*")) - before_paste) == 1)
                attached = (set(paste_dir.glob("*")) - before_paste).pop()
                assert attached.read_bytes() == png
                capture("relative-image-attached", composer_text=None)
                send(b"\x03")
                with (root / "oversized.png").open("wb") as image_file:
                    image_file.write(png)
                    image_file.truncate(8 * 1024 * 1024 + 1)
                send("KEEP_IMAGE_DRAFT")
                send(b"\x1b[200~oversized.png\x1b[201~")
                wait_for(lambda: "8 MiB limit" in display_text())
                capture("oversized-image-rejected", composer_text="KEEP_IMAGE_DRAFT")
                assert set(paste_dir.glob("*")) == before_paste | {attached}
                send(b"\x03")
                result["findings"][
                    "bounded_image_paste_preserves_draft_and_relative_paths"
                ] = True

                send(b"\x1b[200~alpha\nbeta\x1b[201~")
                send(b"\x1b[HX\x01Y\x05")
                capture("composer-line-boundaries", composer_text="Yalpha\nXbeta")
                send(b"\x03")
                result["findings"][
                    "composer_line_and_buffer_boundaries_are_distinct"
                ] = True

                config_before = (state / "config.toml").read_bytes()
                send("/connect\r")
                send(b"\x1b[B\x1b[B\r")
                send("\r")
                send("fixture-model\r")
                wait_for(lambda: "API key" in display_text())
                secret_fixture = "A\U0001f469\u200d\U0001f4bbe\u0301Z"
                send(secret_fixture)
                wait_for(lambda: display_text().count("•") == 4)
                send(b"\x1b[H\x1b[C\x1b[3~")
                wait_for(lambda: display_text().count("•") == 3)
                send("Ω")
                wait_for(lambda: display_text().count("•") == 4)
                resize(24, 8)
                capture("onboarding-secret-middle-narrow", view="onboarding")
                assert secret_fixture not in display_text()
                assert secret_fixture.encode() not in raw
                resize(100, 36)
                send(b"\x03")
                capture("onboarding-cancel-restores-session")
                assert (state / "config.toml").read_bytes() == config_before
                result["findings"][
                    "onboarding_grapheme_editing_stays_masked_and_cancels_cleanly"
                ] = True

                send("CONTROL_AGENTS use sub-agents to review\r")
                wait_for(lambda: "CONTROL_AGENTS_DONE" in display_text(), timeout=30)
                completed_agents = [
                    event["snapshot"]
                    for event in session_events()
                    if event["type"] == "agent_completed"
                ]
                assert len(completed_agents) == 8
                send("/agents\r")
                resize(24, 8)
                send(b"\x1b[H")
                first_agents = capture("agents-first-narrow", view="agents")
                send(b"\x1b[F")
                last_agents = capture("agents-last-narrow", view="agents")
                assert first_agents["screen"] != last_agents["screen"]
                send("\r")
                wait_for(lambda: "NEW29" in display_text())
                capture("agent-inspection-tail", view="agents")
                send(b"\x1b[H")
                for _ in range(20):
                    if "OLD00" in display_text():
                        break
                    send(b"\x1b[B")
                else:
                    raise AssertionError("older child transcript is unreachable")
                capture("agent-inspection-first", view="agents")
                send(b"\x1b[F")
                wait_for(lambda: "NEW29" in display_text())
                resize(100, 36)
                capture("agent-inspection-wide", view="agents")
                send(b"\x1b")
                send(b"\x1b")
                capture("agents-restored")
                result["findings"][
                    "live_agent_list_and_transcript_navigation_are_reachable"
                ] = True
            send("/exit\r")
            wait_for(lambda: process.poll() is not None)
            result["exit_code"] = process.returncode
            assert process.returncode == 0, process.returncode
            pump(0.1)
            assert screen.primary is None, "exit did not restore the primary screen"
            restored_rows, restored_cursor = screen.restored_primary
            assert [line.rstrip() for line in restored_rows] == [
                "SHELL_HISTORY_MARKER",
                "$ kurama",
                *([""] * (screen.lines - 2)),
            ], "exit left application text on the restored primary screen"
            assert not screen.cursor.hidden
            assert restored_cursor == (0, 2)
            assert f"kurama resume {logs[0].parent.name}" in screen.all_text()
            assert pyte.modes.DECAWM in screen.mode, "line wrapping was not restored"
            assert raw.rfind(b"\x1b[?2004l") > raw.rfind(b"\x1b[?2004h")
            for mode in (1002, 1006):
                enabled = f"\x1b[?{mode}h".encode()
                disabled = f"\x1b[?{mode}l".encode()
                assert raw.count(enabled) == raw.count(disabled) == 1
                assert raw.rfind(disabled) > raw.rfind(enabled)
            assert b"\x1b[?1003h" not in raw, "all-motion mouse reporting was enabled"
            result["findings"]["drag_mouse_modes_restored"] = True
            assert_pointer_actions(copied_texts)
            assert raw.count(b"\x1b[?1049h") == raw.count(b"\x1b[?1049l") == 1
            capture("exit-restored", active=False)
            result["findings"]["shell_history_restored_on_exit"] = True
            result["findings"]["terminal_modes_restored_on_exit"] = True
            result["findings"]["alternate_screen_entries"] = raw.count(b"\x1b[?1049h")
            result["findings"]["alternate_screen_exits"] = raw.count(b"\x1b[?1049l")
            result["findings"]["scrollback_purges"] = raw.count(b"\x1b[3J")
            result["findings"]["cursor_position_queries"] = raw.count(
                b"\x1b[6n"
            ) + raw.count(b"\x1b[?6n")
        finally:
            if args.check_controls:
                Fixture.control_release.set()
                Fixture.control_cancel_release.set()
                (root / "release-tool").touch()


if __name__ == "__main__":
    raise SystemExit(main())
