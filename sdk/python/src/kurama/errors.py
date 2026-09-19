"""Errors raised by the client, distinct from exceptions in user callbacks."""

from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .types import ApprovalRequest, Reply


class KuramaError(Exception):
    """Base class for SDK and server errors."""

    def __init__(self, message: str, *, code: str) -> None:
        super().__init__(message)
        self.code = code


class ProtocolError(KuramaError):
    """The binary sent invalid or incompatible protocol data."""

    def __init__(self, message: str, *, code: str = "invalid_protocol") -> None:
        super().__init__(message, code=code)


class ProcessError(KuramaError):
    """The owned Kurama process could not start or exited unexpectedly."""

    def __init__(self, message: str, *, returncode: int | None = None) -> None:
        super().__init__(message, code="process_error")
        self.returncode = returncode


class ClosedError(KuramaError):
    def __init__(self) -> None:
        super().__init__("The Kurama agent is closed.", code="closed")


class BusyError(KuramaError):
    def __init__(self) -> None:
        super().__init__(
            "An execution is already active; finish or close its stream first.",
            code="busy",
        )


class BufferOverflowError(KuramaError):
    def __init__(self) -> None:
        super().__init__(
            "The event consumer fell behind the bounded buffer; the agent was closed. "
            "Consume events promptly or increase max_buffered_events/max_buffered_bytes.",
            code="event_buffer_overflow",
        )


class ServerError(KuramaError):
    """A structured error returned by the Rust process."""


class TurnFailed(ServerError):
    """An accepted execution failed; partial text remains available in reply."""

    def __init__(self, message: str, *, code: str, reply: Reply) -> None:
        super().__init__(message, code=code)
        self.reply = reply


class ApprovalRequired(KuramaError):
    """A convenience call needed approval and safely cancelled its execution."""

    def __init__(self, request: ApprovalRequest) -> None:
        super().__init__(
            "This operation needs approval. Supply on_approval, or use "
            "agent.stream() and agent.approve() for manual approval.",
            code="approval_required",
        )
        self.request = request
