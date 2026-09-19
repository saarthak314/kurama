"""Async process ownership, bounded event delivery, and cancellation-safe turns."""

from __future__ import annotations

import asyncio
import inspect
import json
import os
import signal
from collections import deque
from collections.abc import AsyncIterator, Mapping
from dataclasses import dataclass, field
from typing import Any, TypeVar, cast

from ._protocol import (
    CAPABILITIES,
    MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
    array,
    boolean,
    decode_error,
    decode_event,
    decode_frame,
    decode_report,
    enum,
    integer,
    object_value,
    request_id,
    string,
)
from .errors import (
    ApprovalRequired,
    BufferOverflowError,
    BusyError,
    ClosedError,
    KuramaError,
    ProcessError,
    ProtocolError,
    ServerError,
    TurnFailed,
)
from .types import (
    ApprovalCallback,
    ApprovalEvent,
    ApprovalRequest,
    ApprovalResponse,
    DoneEvent,
    Event,
    Mode,
    Reply,
    TextEvent,
    VerificationEvent,
    VerificationReport,
)

_T = TypeVar("_T")


def _future() -> asyncio.Future[Any]:
    future = asyncio.get_running_loop().create_future()
    # A cancelled caller can leave a response pending until the process answers.
    # Retrieving here avoids an unobserved-exception warning, not propagation.
    future.add_done_callback(
        lambda item: None if item.cancelled() else item.exception()
    )
    return future


async def _cleanup_wait(task: asyncio.Task[_T]) -> _T:
    """Finish bounded cleanup even if the owner is cancelled again mid-cleanup."""
    cancelled = False
    while not task.done():
        try:
            await asyncio.shield(task)
        except asyncio.CancelledError:
            cancelled = True
    result = task.result()
    if cancelled:
        raise asyncio.CancelledError
    return result


class _Inbox:
    def __init__(self, count_limit: int, byte_limit: int) -> None:
        self.items: deque[tuple[Event, int]] = deque()
        self.bytes = 0
        self.count_limit = count_limit
        self.byte_limit = byte_limit
        self.wake = asyncio.Event()
        self.ended = False
        self.error: KuramaError | None = None
        self.discard = False

    def offer(self, event: Event, size: int) -> None:
        if self.discard:
            return
        if self.ended:
            raise ProtocolError("Received an event after its terminal event.")
        if len(self.items) >= self.count_limit or self.bytes + size > self.byte_limit:
            raise BufferOverflowError()
        self.items.append((event, size))
        self.bytes += size
        self.wake.set()

    def finish(self, error: KuramaError | None = None) -> None:
        self.ended = True
        self.error = error
        self.wake.set()

    def drain(self) -> None:
        # Only explicit stream/agent cleanup discards events. Normal overflow fails.
        self.discard = True
        self.items.clear()
        self.bytes = 0
        self.wake.set()

    async def get(self) -> Event:
        event, _ = await self.take()
        return event

    async def take(self) -> tuple[Event, int]:
        while True:
            if self.error is not None:
                raise self.error
            if self.items:
                event, size = self.items.popleft()
                self.bytes -= size
                return event, size
            if self.ended:
                raise StopAsyncIteration
            self.wake.clear()
            await self.wake.wait()


@dataclass(slots=True)
class _Execution:
    inbox: _Inbox
    initializing: bool = False
    id: str | None = None
    accepted: bool = False
    finished: asyncio.Event = field(default_factory=asyncio.Event)


class Agent:
    """One owned Kurama process and resumable session.

    Install a Kurama binary supporting ``--stdio`` once; no binary is downloaded.
    The default path is ``async with Agent(profile="work") as agent`` followed by
    ``await agent.prompt(text)``. Existing Kurama configuration supplies providers
    and credentials. ``env`` overrides only the child environment.

    Only one prompt/verification can execute at a time. Event buffering is bounded:
    a stalled consumer fails explicitly and closes the process rather than dropping
    events. ``events()`` consumes unsolicited session events separately from turns.
    """

    def __init__(
        self,
        *,
        workspace: str | os.PathLike[str] | None = None,
        profile: str | None = None,
        mode: Mode = "supervised",
        binary: str | os.PathLike[str] | None = None,
        state_dir: str | os.PathLike[str] | None = None,
        session_id: str | None = None,
        env: Mapping[str, str] | None = None,
        on_approval: ApprovalCallback | None = None,
        startup_timeout: float = 30.0,
        shutdown_timeout: float = 5.0,
        request_timeout: float = 30.0,
        max_buffered_events: int = 64,
        max_buffered_bytes: int = 8 * MAX_FRAME_BYTES,
    ) -> None:
        if mode not in {"supervised", "auto", "yolo"}:
            raise ValueError("mode must be supervised, auto, or yolo")
        if min(startup_timeout, shutdown_timeout, request_timeout) <= 0:
            raise ValueError("timeouts must be positive")
        if max_buffered_events < 1 or max_buffered_bytes < 1:
            raise ValueError("event buffer limits must be positive")
        self.workspace = os.fspath(workspace) if workspace is not None else os.getcwd()
        self.profile = profile
        self.mode = mode
        self.session_id = session_id
        self.on_approval = on_approval
        self._binary = os.fspath(binary) if binary is not None else None
        self._state_dir = os.fspath(state_dir) if state_dir is not None else None
        self._env = dict(env) if env is not None else {}
        self._startup_timeout = startup_timeout
        self._shutdown_timeout = shutdown_timeout
        self._request_timeout = request_timeout
        self._buffer_count = max_buffered_events
        self._buffer_bytes = max_buffered_bytes
        self._process: asyncio.subprocess.Process | None = None
        self._spawn_task: asyncio.Task[asyncio.subprocess.Process] | None = None
        self._reader_task: asyncio.Task[None] | None = None
        self._watcher_task: asyncio.Task[None] | None = None
        self._close_task: asyncio.Task[None] | None = None
        self._open_lock = asyncio.Lock()
        self._write_lock = asyncio.Lock()
        self._active: _Execution | None = None
        self._hello: asyncio.Future[dict[str, Any]] | None = None
        self._pending: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._next_id = 1
        self._opened = False
        self._closing = False
        self._closed = False
        self._closing_signal = asyncio.Event()
        self._error: KuramaError | None = None
        self._wire_session: str | None = None
        self._unsolicited = self._inbox()

    def _inbox(self) -> _Inbox:
        return _Inbox(self._buffer_count, self._buffer_bytes)

    async def __aenter__(self) -> Agent:
        return await self.open()

    async def __aexit__(self, *exc: object) -> None:
        await self.close()

    async def open(self) -> Agent:
        """Start and negotiate once; prefer the async context manager."""
        async with self._open_lock:
            if self._opened and not self._closing:
                self._check_open()
                return self
            if self._closing or self._closed:
                raise ClosedError()
            try:
                await self._open()
            except BaseException:
                await self.close()
                raise
            self._opened = True
            return self

    async def _open(self) -> None:
        if os.name != "posix":
            raise ProcessError("The headless SDK currently supports Linux and macOS.")
        environment = os.environ.copy()
        environment.update(self._env)
        binary = self._binary or environment.get("KURAMA_BIN") or "kurama"
        args = [binary, "--stdio"]
        if self.mode == "yolo":
            args.append("--yolo")
        deadline = asyncio.timeout(self._startup_timeout)
        try:
            async with deadline:
                try:
                    self._spawn_task = asyncio.create_task(
                        asyncio.create_subprocess_exec(
                            *args,
                            stdin=asyncio.subprocess.PIPE,
                            stdout=asyncio.subprocess.PIPE,
                            stderr=asyncio.subprocess.DEVNULL,
                            env=environment,
                            start_new_session=True,
                            limit=MAX_FRAME_BYTES + 1,
                        ),
                        name="kurama-spawn",
                    )
                    self._process = await asyncio.shield(self._spawn_task)
                except (OSError, ValueError):
                    raise ProcessError(
                        "Could not start Kurama. Install a binary supporting --stdio, or set "
                        "binary= or KURAMA_BIN to its executable."
                    ) from None
                if self._closing:
                    raise ClosedError()
                self._hello = _future()
                self._reader_task = asyncio.create_task(
                    self._read(), name="kurama-stdout"
                )
                self._watcher_task = asyncio.create_task(
                    self._watch(), name="kurama-process"
                )
                await asyncio.shield(self._hello)
        except TimeoutError:
            if not deadline.expired():
                raise
            raise ProcessError(
                "Kurama protocol startup timed out. Install a binary supporting --stdio."
            ) from None
        # Recovery can legitimately await a user decision or model continuation.
        state = _Execution(self._inbox(), initializing=True)
        params: dict[str, Any] = {
            "protocol_version": PROTOCOL_VERSION,
            "workspace": self.workspace,
            "mode": self.mode,
        }
        for key, value in (
            ("profile", self.profile),
            ("state_dir", self._state_dir),
            ("session_id", self.session_id),
        ):
            if value is not None:
                params[key] = value
        response = await self._send("initialize", params, execution=state)
        while True:
            try:
                event, size = await state.inbox.take()
            except StopAsyncIteration:
                break
            if isinstance(event, ApprovalEvent):
                await self._approval(event.request)
            else:
                # Recovery progress is not attributed to the first submitted prompt.
                self._unsolicited.offer(event, size)
        result = await asyncio.shield(response)
        session = string(result.get("session_id"))
        if self._wire_session is not None and session != self._wire_session:
            raise ProtocolError("Initialization returned a different recovery session.")
        self.session_id = self._wire_session = session
        self.workspace = string(result.get("workspace"))
        self.profile = string(result.get("profile"))
        self.mode = enum(result.get("mode"), {"supervised", "auto", "yolo"})
        self._active = None

    def _check_open(self) -> None:
        if self._error is not None:
            raise self._error
        if not self._opened or self._closing or self._closed:
            raise ClosedError()

    async def _send(
        self,
        method: str,
        params: dict[str, Any],
        *,
        execution: _Execution | None = None,
        target: _Execution | None = None,
    ) -> asyncio.Future[dict[str, Any]]:
        async with self._write_lock:
            if self._error is not None:
                raise self._error
            if self._closed or (self._closing and method != "shutdown"):
                raise ClosedError()
            if self._process is None or self._process.stdin is None:
                raise ClosedError()
            if execution is not None and self._active is not None:
                raise BusyError()
            if execution is not None and execution.finished.is_set():
                raise ClosedError()
            if target is not None and (
                self._active is not target or target.finished.is_set()
            ):
                response = _future()
                response.set_result({"cancelled": False})
                return response
            if len(self._pending) >= 64:
                raise KuramaError(
                    "Too many outstanding requests.", code="too_many_requests"
                )
            identifier = str(self._next_id)
            try:
                frame = json.dumps(
                    {"id": identifier, "method": method, "params": params},
                    ensure_ascii=False,
                    separators=(",", ":"),
                    allow_nan=False,
                ).encode("utf-8")
            except (TypeError, ValueError, UnicodeError, RecursionError):
                raise ValueError(
                    "Request parameters must be finite UTF-8 JSON values."
                ) from None
            if len(frame) > MAX_FRAME_BYTES:
                raise ValueError(
                    "Request exceeds the 1048576-byte protocol frame limit."
                )
            self._next_id += 1
            response = _future()
            self._pending[identifier] = response
            if execution is not None:
                execution.id = identifier
                self._active = execution
            try:
                self._process.stdin.write(frame + b"\n")
                async with asyncio.timeout(self._request_timeout):
                    await self._process.stdin.drain()
            except (OSError, TimeoutError):
                error = ProcessError("Writing to Kurama failed or timed out.")
                self._fail(error)
                raise error from None
            return response

    async def _request(
        self, method: str, params: dict[str, Any], *, target: _Execution | None = None
    ) -> dict[str, Any]:
        try:
            response = await self._send(method, params, target=target)
            async with asyncio.timeout(self._request_timeout):
                return await asyncio.shield(response)
        except TimeoutError:
            error = ProcessError("Kurama did not acknowledge a request in time.")
            self._fail(error)
            raise error from None
        finally:
            if (
                self._error is not None
                and asyncio.current_task() is not self._close_task
            ):
                await self.close()

    async def _read(self) -> None:
        assert self._process is not None and self._process.stdout is not None
        try:
            while True:
                try:
                    line = await self._process.stdout.readuntil(b"\n")
                except asyncio.IncompleteReadError as exc:
                    if exc.partial:
                        raise ProtocolError(
                            "Kurama ended with a truncated protocol frame."
                        ) from None
                    if not self._closing:
                        raise ProcessError(
                            "Kurama closed stdout. Install a compatible binary supporting --stdio "
                            "and check your configured profile.",
                            returncode=self._process.returncode,
                        ) from None
                    return
                except asyncio.LimitOverrunError:
                    raise ProtocolError(
                        "Protocol frame exceeds the 1048576-byte limit."
                    ) from None
                self._receive(decode_frame(line), len(line) - 1)
        except asyncio.CancelledError:
            raise
        except KuramaError as error:
            self._fail(error)
        except Exception:
            self._fail(ProtocolError("Could not read the Kurama protocol stream."))

    def _receive(self, frame: dict[str, Any], size: int) -> None:
        assert self._hello is not None
        if not self._hello.done():
            if frame.get("type") != "hello":
                raise ProtocolError("Kurama did not send the required hello handshake.")
            if integer(frame.get("protocol_version")) != PROTOCOL_VERSION:
                raise ProtocolError(
                    "Incompatible Kurama protocol. Install compatible SDK and Kurama versions.",
                    code="incompatible_protocol",
                )
            if integer(frame.get("max_frame_bytes")) != MAX_FRAME_BYTES:
                raise ProtocolError("Kurama advertises an incompatible frame limit.")
            capabilities = {string(value) for value in array(frame.get("capabilities"))}
            if not CAPABILITIES.issubset(capabilities):
                raise ProtocolError(
                    "Kurama is missing required SDK capabilities.",
                    code="incompatible_protocol",
                )
            string(frame.get("server_version"))
            self._hello.set_result(frame)
            return
        if frame.get("type") == "response":
            if ("error" in frame) == ("result" in frame):
                raise ProtocolError(
                    "Response must contain exactly one result or error."
                )
            if frame.get("id") is None:
                info = decode_error(frame.get("error"))
                raise ProtocolError(info.message, code=info.code)
            identifier = request_id(frame.get("id"))
            response = self._pending.get(identifier)
            if response is None:
                raise ProtocolError("Response has no matching pending request.")
            state = self._active
            if "error" in frame:
                info = decode_error(frame["error"])
                error = (
                    ProtocolError(info.message, code=info.code)
                    if info.code == "incompatible_protocol"
                    else ServerError(info.message, code=info.code)
                )
                if state is not None and state.id == identifier:
                    state.inbox.finish(error)
                    state.finished.set()
                response.set_exception(error)
            else:
                result = object_value(frame["result"])
                if state is not None and state.id == identifier:
                    if state.initializing:
                        state.inbox.finish()
                        state.finished.set()
                    else:
                        if result.get("accepted") is not True:
                            raise ProtocolError(
                                "Execution response did not acknowledge acceptance."
                            )
                        state.accepted = True
                response.set_result(result)
            del self._pending[identifier]
            return
        if frame.get("type") != "event":
            raise ProtocolError("Unrecognized protocol frame type.")
        session = string(frame.get("session_id"))
        if self._wire_session is not None and self._wire_session != session:
            raise ProtocolError("Received an event belonging to a different session.")
        self._wire_session = session
        event = decode_event(frame.get("event"))
        correlation = frame.get("request_id")
        if correlation is None:
            if isinstance(event, DoneEvent):
                raise ProtocolError("A terminal event must identify its execution.")
            self._unsolicited.offer(event, size)
            return
        identifier = request_id(correlation)
        state = self._active
        if state is None or state.id != identifier or state.finished.is_set():
            raise ProtocolError("Received an event outside its correlated execution.")
        if not state.initializing and not state.accepted:
            raise ProtocolError("Received an execution event before acceptance.")
        if isinstance(event, DoneEvent) and state.initializing:
            raise ProtocolError(
                "Initialization recovery cannot emit an execution terminal."
            )
        state.inbox.offer(event, size)
        if isinstance(event, DoneEvent):
            state.finished.set()
            state.inbox.finish()

    async def _watch(self) -> None:
        assert self._process is not None
        # Process.wait() can wait for inherited pipes after the leader exits.
        # Observe the child-watcher's returncode independently of pipe closure.
        while self._process.returncode is None:
            await asyncio.sleep(0.05)
        if not self._closing:
            if self._reader_task is not None:
                await asyncio.wait(
                    {self._reader_task}, timeout=min(self._shutdown_timeout, 0.1)
                )
            if self._error is None:
                self._fail(
                    ProcessError(
                        "Kurama exited unexpectedly. Install a compatible binary supporting --stdio "
                        "and check your configured profile.",
                        returncode=self._process.returncode,
                    )
                )

    def _fail(self, error: KuramaError) -> None:
        if self._error is not None:
            return
        self._error = error
        if self._hello is not None and not self._hello.done():
            self._hello.set_exception(error)
        for response in self._pending.values():
            if not response.done():
                response.set_exception(error)
        self._pending.clear()
        self._unsolicited.finish(error)
        if self._active is not None:
            self._active.inbox.finish(error)
            self._active.finished.set()
        if self._close_task is None:
            self._close_task = asyncio.create_task(self._close(), name="kurama-close")

    def stream(self, text: str, *, explicit_delegation: bool = False) -> EventStream:
        """Stream typed events, including manual approvals and the terminal event.

        Python's ``async for`` does not close custom iterators on ``break``. Prefer
        ``async with agent.stream(text) as events`` when iteration may end early,
        or call ``await events.aclose()``. Agent context exit always owns cleanup.
        """
        self._check_open()
        return EventStream(
            self, "prompt", {"text": text, "explicit_delegation": explicit_delegation}
        )

    async def prompt(self, text: str, *, explicit_delegation: bool = False) -> Reply:
        """Collect text. Missing approval callbacks cancel safely and raise ApprovalRequired."""
        chunks: list[str] = []
        async with self.stream(text, explicit_delegation=explicit_delegation) as events:
            async for event in events:
                if isinstance(event, TextEvent):
                    chunks.append(event.text)
                elif isinstance(event, ApprovalEvent) and self.on_approval is None:
                    raise ApprovalRequired(event.request)
                elif isinstance(event, DoneEvent):
                    reply = Reply(
                        "".join(chunks), cast(str, self.session_id), event.status
                    )
                    self._raise_failed(event, reply)
                    return reply
        raise ProtocolError("Execution ended without a terminal event.")

    async def verify(self, name: str) -> VerificationReport:
        """Explicitly run a named project recipe through Rust's policy and Bash tool."""
        self._check_open()
        report: VerificationReport | None = None
        async with EventStream(self, "verify", {"name": name}) as events:
            async for event in events:
                if isinstance(event, ApprovalEvent) and self.on_approval is None:
                    raise ApprovalRequired(event.request)
                elif isinstance(event, VerificationEvent):
                    report = event.report
                elif isinstance(event, DoneEvent):
                    self._raise_failed(
                        event, Reply("", cast(str, self.session_id), event.status)
                    )
        if report is None:
            raise ProtocolError("Verification ended without a verification report.")
        return report

    async def verification_status(self) -> list[VerificationReport]:
        """Inspect configured recipes without executing any command or model call."""
        self._check_open()
        result = await self._request("verification_status", {})
        return [decode_report(value) for value in array(result.get("recipes"))]

    async def events(self) -> AsyncIterator[Event]:
        """Consume unsolicited/recovery events, never events from a submitted turn."""
        while True:
            try:
                yield await self._unsolicited.get()
            except StopAsyncIteration:
                return

    async def approve(
        self, operation_id: str, response: ApprovalResponse = "approve_once"
    ) -> None:
        """Resolve a pending operation; Rust enforces session and operation ownership."""
        valid = isinstance(response, str) and response in {
            "approve_once",
            "approve_session",
            "deny",
        }
        if isinstance(response, dict):
            edit = response.get("edit")
            valid = (
                set(response) == {"edit"}
                and isinstance(edit, dict)
                and set(edit) == {"arguments"}
            )
        if not valid:
            raise ValueError(
                "Approval must be approve_once, approve_session, deny, or {'edit': {'arguments': JSON}}."
            )
        result = await self._request(
            "approve", {"operation_id": operation_id, "response": response}
        )
        if result.get("accepted") is not True:
            raise ProtocolError("Approval response did not acknowledge acceptance.")

    async def _approval(self, request: ApprovalRequest) -> None:
        if self.on_approval is None:
            raise ApprovalRequired(request)
        state = self._active
        response = self.on_approval(request)
        if inspect.isawaitable(response):
            callback = asyncio.ensure_future(response)
            stopped = asyncio.create_task(
                state.finished.wait()
                if state is not None
                else self._closing_signal.wait()
            )
            closing = asyncio.create_task(self._closing_signal.wait())
            try:
                await asyncio.wait(
                    {callback, stopped, closing}, return_when=asyncio.FIRST_COMPLETED
                )
                if self._error is not None:
                    raise self._error
                if self._closing:
                    raise ClosedError()
                if not callback.done():
                    return
                response = callback.result()
            finally:
                cleanup = asyncio.create_task(
                    self._finish_callback(callback, stopped, closing)
                )
                await _cleanup_wait(cleanup)
        if state is not None and self._active is state and not state.finished.is_set():
            await self.approve(request.operation_id, response)

    async def _finish_callback(self, *tasks: asyncio.Future[Any]) -> None:
        for task in tasks:
            if not task.done():
                task.cancel()
            task.add_done_callback(
                lambda item: None if item.cancelled() else item.exception()
            )
        _, pending = await asyncio.wait(tasks, timeout=self._shutdown_timeout)
        if pending:
            # Python cannot forcibly stop cancellation-resistant application code;
            # it must not retain the Rust process or permit further executions.
            self._fail(
                KuramaError(
                    "An approval callback ignored cancellation; the agent was closed.",
                    code="approval_callback_timeout",
                )
            )

    async def cancel(self) -> bool:
        """Request cancellation of the current execution, without claiming rollback."""
        self._check_open()
        state = self._active
        if state is None or state.finished.is_set():
            return False
        result = await self._request("cancel", {}, target=state)
        return boolean(result.get("cancelled"))

    @staticmethod
    def _raise_failed(event: DoneEvent, reply: Reply) -> None:
        if event.status == "failed":
            raise TurnFailed(
                event.error.message if event.error else "Kurama execution failed.",
                code=event.error.code if event.error else "runtime_error",
                reply=reply,
            )

    async def close(self) -> None:
        """Bounded graceful shutdown, then terminate only this owned process group."""
        if self._close_task is None:
            self._close_task = asyncio.create_task(self._close(), name="kurama-close")
        await _cleanup_wait(self._close_task)

    async def _close(self) -> None:
        self._closing = True
        self._closing_signal.set()
        if self._active is not None:
            self._active.inbox.drain()
        self._unsolicited.drain()
        process = self._process
        try:
            if process is None and self._spawn_task is not None:
                try:
                    process = self._process = await self._spawn_task
                except (OSError, ValueError):
                    pass
            if process is not None:
                try:
                    if self._error is None:
                        async with asyncio.timeout(self._shutdown_timeout):
                            if process.returncode is None:
                                await self._request("shutdown", {})
                            if process.stdin is not None:
                                process.stdin.close()
                            await process.wait()
                except (KuramaError, OSError, TimeoutError):
                    pass
                # The group may still contain children even after its leader exits.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                try:
                    async with asyncio.timeout(self._shutdown_timeout):
                        await process.wait()
                except TimeoutError:
                    # asyncio has no public subprocess transport close. A descendant
                    # that detached and retained pipes must not hold our loop open.
                    process._transport.close()  # type: ignore[attr-defined]
        finally:
            current = asyncio.current_task()
            tasks = [
                task
                for task in (self._reader_task, self._watcher_task)
                if task is not None and task is not current
            ]
            for task in tasks:
                task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)
            self._closed = True
            self._opened = False
            error = self._error or ClosedError()
            for response in self._pending.values():
                if not response.done():
                    response.set_exception(error)
            self._pending.clear()
            if self._hello is not None and not self._hello.done():
                self._hello.set_exception(error)
            if self._active is not None:
                self._active.inbox.finish(error)
                self._active.finished.set()
            self._unsolicited.finish(self._error)


class EventStream(AsyncIterator[Event]):
    """Single-consumer stream; ``aclose`` cancels and drains before reuse.

    Use this object's async context manager around loops that might ``break``.
    Cancelling ``__anext__`` also performs bounded cleanup automatically.
    """

    def __init__(self, agent: Agent, method: str, params: dict[str, Any]) -> None:
        self._agent = agent
        self._method = method
        self._params = params
        self._state = _Execution(agent._inbox())
        self._started = False
        self._closed = False
        self._iterating = False
        self._close_task: asyncio.Task[None] | None = None

    def __aiter__(self) -> EventStream:
        return self

    async def __aenter__(self) -> EventStream:
        await self._start()
        return self

    async def __aexit__(self, *exc: object) -> None:
        await self.aclose()

    async def _start(self) -> None:
        if self._closed:
            raise ClosedError()
        if self._started:
            return
        self._started = True
        try:
            self._agent._check_open()
            response = await self._agent._send(
                self._method, self._params, execution=self._state
            )
            async with asyncio.timeout(self._agent._request_timeout):
                await asyncio.shield(response)
        except BaseException:
            await self.aclose()
            raise

    async def __anext__(self) -> Event:
        if self._closed:
            raise StopAsyncIteration
        if self._iterating:
            raise RuntimeError(
                "An EventStream has one consumer; concurrent iteration is not supported."
            )
        self._iterating = True
        try:
            await self._start()
            event = await self._state.inbox.get()
            if isinstance(event, ApprovalEvent) and self._agent.on_approval is not None:
                await self._agent._approval(event.request)
            if isinstance(event, DoneEvent):
                self._release()
            return event
        except StopAsyncIteration:
            self._release()
            raise
        except BaseException:
            await self.aclose()
            raise
        finally:
            self._iterating = False

    def _release(self) -> None:
        self._closed = True
        if self._agent._active is self._state:
            self._agent._active = None

    async def aclose(self) -> None:
        if self._closed:
            return
        if self._close_task is None:
            self._close_task = asyncio.create_task(
                self._close(), name="kurama-stream-close"
            )
        await _cleanup_wait(self._close_task)

    async def _close(self) -> None:
        state = self._state
        state.inbox.drain()
        try:
            if self._agent._error is not None:
                await self._agent.close()
            if self._agent._active is state and not state.finished.is_set():
                try:
                    async with asyncio.timeout(self._agent._shutdown_timeout):
                        await self._agent._request("cancel", {}, target=state)
                        await state.finished.wait()
                except (KuramaError, OSError, TimeoutError):
                    await self._agent.close()
        finally:
            state.inbox.finish()
            state.finished.set()
            self._release()
