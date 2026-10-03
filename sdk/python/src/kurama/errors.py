"""SDK and server errors; exceptions from application callbacks stay untouched."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from .types import ApprovalRequest, Reply

_DEFAULT_MESSAGES = {
    "closed": "The Kurama agent is closed.",
    "busy": "An execution is already active; finish or close its stream first.",
    "backpressure": (
        "The event consumer fell behind the bounded buffer; the agent was closed. "
        "Consume events promptly."
    ),
    "reply_too_large": "The reply exceeds the 16 MiB aggregation limit; use agent.stream().",
    "approval_required": (
        "This operation needs approval. Supply approve=True or an approval callback, "
        "or use agent.stream() and agent.approve() for manual approval."
    ),
}


class KuramaError(Exception):
    """SDK or server failure with a stable code and optional execution details."""

    def __init__(
        self,
        code: str,
        message: str | None = None,
        details: dict[str, Any] | None = None,
    ) -> None:
        super().__init__(
            message if message is not None else _DEFAULT_MESSAGES.get(code, code)
        )
        self.code = code
        self.details = details
        self.reply: Reply | None = details.get("reply") if details else None
        self.returncode: int | None = details.get("returncode") if details else None


class ApprovalRequired(KuramaError):
    """A convenience call needed approval and safely cancelled its execution."""

    def __init__(self, request: ApprovalRequest) -> None:
        super().__init__("approval_required")
        self.request = request
