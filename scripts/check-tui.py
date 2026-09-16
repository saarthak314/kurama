#!/usr/bin/env python3
"""Exercise the real CLI through a local SSE provider and a PTY, without credentials.

Run: uv run --with pyte --with pillow scripts/check-tui.py target/release/kurama .lavish/tui-check
Use --no-images to require only pyte. The VT model tracks separate primary and
alternate buffers; screenshots are decoded PTY output, not a native terminal
window capture. This is a POSIX-only development gate.
"""

import argparse
import copy
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

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

        if is_compaction:
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
    args = parser.parse_args()
    if args.seed_large_output_mib < 0:
        parser.error("--seed-large-output-mib cannot be negative")
    seed_session = (
        args.seed_todos
        or args.torn_tail
        or args.seed_large_output_mib > 0
        or args.check_compaction
    )
    output_dir = Path(args.output).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    result = {"frames": [], "findings": {}, "resources": {}}
    with tempfile.TemporaryDirectory(prefix="kurama-tui-") as temp:
        root = Path(temp)
        state = root / ".kurama"
        state.mkdir()
        (root / "input.txt").write_text("fixture input\n")
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
        master, slave = pty.openpty()
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
            os.environ, HOME=str(root), TERM="xterm-256color", COLORTERM="truecolor"
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
        os.close(slave)
        screen = Screen(100, 36)
        screen.reply = lambda value: (
            None if args.no_cpr else os.write(master, value.encode())
        )
        stream = pyte.ByteStream(screen)
        raw = bytearray()

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

        def assert_pointer_actions(copies):
            actions = pointer_actions()
            assert [
                action["args"] for action in actions if action["kind"] == "open"
            ] == [["https://example.com/reference"]], actions
            assert [
                action["text"] for action in actions if action["kind"] == "copy"
            ] == (copies), actions
            assert len(actions) == 1 + len(copies), actions

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
            wait_for(lambda: len(pointer_actions()) >= count + 1)
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
            measured = subprocess.run(
                ["ps", "-o", "rss=", "-p", str(process.pid)],
                capture_output=True,
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
                        lambda: any(
                            label in screen.display[-1].lower()
                            for label in ("esc close", "release to copy", "copied")
                        )
                    )
                assert screen.primary is not None, (
                    "application left its alternate screen"
                )
                assert "SHELL_HISTORY_MARKER" not in screen.all_text()
                assert "SHELL_HISTORY_MARKER" in screen.primary_text()
                assert raw.count(b"\x1b[?1049h") == 1, "nested alternate-screen entry"
                assert raw.count(b"\x1b[?1049l") == 0, "overlay restored the shell"
                if view == "main":
                    assert "esc close · ↑↓ scroll" not in "\n".join(screen.display)
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
            (output_dir / "terminal.raw").write_bytes(raw)
            (output_dir / "audit.json").write_text(json.dumps(result, indent=2))
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=3)
            os.close(master)
            server.shutdown()
            server.server_close()
    print(
        json.dumps(
            {
                "output": str(output_dir),
                "frames": len(result["frames"]),
                **result.get("findings", {}),
                "exit": result.get("exit_code"),
                "resources": result["resources"],
            }
        )
    )


if __name__ == "__main__":
    main()
