#!/usr/bin/env python3
"""Exercise the real CLI through a local SSE provider and a PTY, without credentials.

Run: uv run --with pyte --with pillow scripts/check-tui.py target/release/kurama .lavish/tui-check
Use --no-images to require only pyte. The VT model handles alternate screens and
cursor-anchored height shrink; screenshots are decoded PTY output, not a native
terminal-emulator window capture. This is a POSIX-only development gate.
"""

import argparse
import copy
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
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

    def log_message(self, *args):
        pass

    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
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

        if call < 3:
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
        "--no-images", action="store_true", help="Skip optional Pillow screenshots"
    )
    parser.add_argument(
        "--baseline",
        action="store_true",
        help="Record old history behavior without requiring preservation",
    )
    args = parser.parse_args()
    output_dir = Path(args.output).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    result = {"frames": [], "findings": {}}
    with tempfile.TemporaryDirectory(prefix="kurama-tui-") as temp:
        root = Path(temp)
        state = root / ".kurama"
        state.mkdir()
        (root / "input.txt").write_text("fixture input\n")
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
        if args.seed_todos:
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
            items = [
                {"id": str(index), "content": f"task {index}", "status": "pending"}
                for index in range(20)
            ]
            initial = [
                {"type": "session_started", "metadata": metadata},
                {"type": "todo_updated", "items": items},
                {"type": "turn_completed"},
            ]
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
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 100, 0, 0))
        command = [
            "/bin/sh",
            "-c",
            'printf "SHELL_HISTORY_MARKER\\r\\n$ kurama\\r\\n"; exec "$@"',
            "fixture-shell",
            str(Path(args.binary).resolve()),
        ]
        if args.seed_todos:
            command.extend(["--resume", "ui-session"])
        environment = dict(
            os.environ, HOME=str(root), TERM="xterm-256color", COLORTERM="truecolor"
        )
        environment.pop("NO_COLOR", None)
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
                if time.monotonic() > deadline or process.poll() is not None:
                    raise RuntimeError(
                        "terminal condition failed:\n" + "\n".join(screen.display)
                    )

        def send(text):
            os.write(master, text if isinstance(text, bytes) else text.encode())
            pump(0.15)

        def capture(name):
            pump(0.15)
            if not args.no_images:
                image(screen, output_dir / (name + ".png"))
            snapshot = {
                "name": name,
                "columns": screen.columns,
                "lines": screen.lines,
                "cursor": [screen.cursor.x, screen.cursor.y],
                "cursor_visible": not screen.cursor.hidden,
                "screen": screen.display,
                "history_rows": len(screen.history.top),
            }
            snapshot["raw_bytes"] = len(raw)
            snapshot["shell_history_present"] = (
                "SHELL_HISTORY_MARKER" in screen.all_text()
            )
            snapshot["history_head"] = [
                "".join(cell.data for _, cell in sorted(line.items()))
                for line in list(screen.history.top)[:3]
            ]
            result["frames"].append(snapshot)
            (output_dir / (name + ".txt")).write_text("\n".join(screen.display))

        def resize(columns, lines):
            screen.resize(lines=lines, columns=columns)
            fcntl.ioctl(
                master, termios.TIOCSWINSZ, struct.pack("HHHH", lines, columns, 0, 0)
            )
            os.killpg(process.pid, signal.SIGWINCH)
            pump(0.25)

        try:
            wait_for(lambda: "Ask Kurama" in "\n".join(screen.display))
            capture("startup")
            result["findings"]["shell_history_preserved_at_start"] = (
                "SHELL_HISTORY_MARKER" in screen.all_text()
            )
            if args.seed_todos:
                send(b"\x14")
                send(b"\x1b[F")
                resize(24, 6)
                capture("todo-narrow-last")
                assert "task 19" in "\n".join(screen.display)
                send(b"\x1b[H")
                capture("todo-narrow-first")
                assert "task 0" in "\n".join(screen.display)
                send(b"\x1b")
                resize(100, 36)
                result["findings"]["todo_first_and_last_accessible"] = True
            send("/")
            capture("command-menu")
            send(b"\x1b")
            send(b"\x03")
            send(b"\x1b[200~first line\nsecond line with editable text\x1b[201~")
            capture("multiline-composer")
            send(b"\x03")
            send("inspect fixture\r")
            wait_for(lambda: "approve once" in "\n".join(screen.display).lower())
            capture("approval")
            resize(36, 14)
            capture("approval-narrow")
            send("a")
            wait_for(lambda: (root / "result.txt").exists())
            wait_for(lambda: "RENDER_COMPLETE" in screen.all_text())
            capture("markdown-narrow")
            resize(100, 36)
            capture("markdown-wide")
            send(b"\x0f")
            capture("transcript-overlay")
            resize(44, 16)
            capture("overlay-resized")
            send(b"\x1b")
            capture("overlay-dismissed")
            resize(100, 36)
            send("long answer\r")
            wait_for(lambda: "LONG_COMPLETE" in screen.all_text())
            capture("long-answer")
            send(b"\x0f")
            send(b"\x1b[H")
            capture("transcript-home")
            send(b"\x1b")
            capture("transcript-restored")
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
            ) == 2 and all(not result["is_error"] for result in completions)
            assert result["findings"]["tools_completed_without_error"], completions
            result["findings"]["scrollback_purges"] = bytes(raw).count(b"\x1b[3J")
            result["findings"]["shell_history_preserved_at_end"] = (
                "SHELL_HISTORY_MARKER" in screen.all_text()
            )
            if not args.baseline:
                assert result["findings"]["shell_history_preserved_at_start"]
                assert result["findings"]["shell_history_preserved_at_end"]
                assert result["findings"]["scrollback_purges"] == 0
            send("/exit\r")
            wait_for(lambda: process.poll() is not None)
            result["exit_code"] = process.returncode
            assert process.returncode == 0, process.returncode
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
            }
        )
    )


if __name__ == "__main__":
    main()
