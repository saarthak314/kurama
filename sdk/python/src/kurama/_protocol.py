"""Protocol v1 decoding. Invalid input never appears in diagnostic messages."""

from __future__ import annotations

import json
from typing import Any, cast

from .errors import ProtocolError
from .types import (
    AgentSnapshot,
    AgentUpdatedEvent,
    ApprovalEvent,
    ApprovalRequest,
    BlobRef,
    DoneEvent,
    ErrorInfo,
    Event,
    Operation,
    RuntimeEvent,
    StatusEvent,
    TextEvent,
    ToolCompletedEvent,
    ToolOutputEvent,
    ToolResult,
    ToolStartedEvent,
    Usage,
    UsageEvent,
    VerificationEvent,
    VerificationReport,
)

PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 1_048_576
CAPABILITIES = {"prompt", "stream", "approval", "cancel", "resume", "verify"}


def object_value(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ProtocolError("Expected a protocol object.")
    return value


def string(value: Any) -> str:
    if not isinstance(value, str):
        raise ProtocolError("Expected a protocol string.")
    return value


def integer(value: Any) -> int:
    if type(value) is not int:
        raise ProtocolError("Expected a protocol integer.")
    return value


def boolean(value: Any) -> bool:
    if type(value) is not bool:
        raise ProtocolError("Expected a protocol boolean.")
    return value


def array(value: Any) -> list[Any]:
    if not isinstance(value, list):
        raise ProtocolError("Expected a protocol array.")
    return value


def optional_string(value: Any) -> str | None:
    return None if value is None else string(value)


def optional_integer(value: Any) -> int | None:
    return None if value is None else integer(value)


def enum(value: Any, choices: set[str]) -> Any:
    if not isinstance(value, str) or value not in choices:
        raise ProtocolError("Unrecognized protocol variant.")
    return value


def request_id(value: Any) -> str:
    identifier = string(value)
    if (
        not identifier
        or not identifier.isascii()
        or not identifier.isdecimal()
        or identifier[0] == "0"
    ):
        raise ProtocolError("Invalid response/request correlation ID.")
    return identifier


def _reject_constant(value: str) -> None:
    raise ValueError("Non-finite JSON number")


def decode_frame(frame: bytes) -> dict[str, Any]:
    if not frame.endswith(b"\n"):
        raise ProtocolError("The process ended with a truncated protocol frame.")
    if len(frame) - 1 > MAX_FRAME_BYTES:
        raise ProtocolError("Protocol frame exceeds the 1048576-byte limit.")
    try:
        value = json.loads(frame[:-1].decode("utf-8"), parse_constant=_reject_constant)
    except (UnicodeDecodeError, ValueError, RecursionError):
        raise ProtocolError("The process sent invalid UTF-8 JSON.") from None
    return object_value(value)


def decode_error(value: Any) -> ErrorInfo:
    value = object_value(value)
    return ErrorInfo(string(value.get("code")), string(value.get("message")))


def decode_blob(value: Any) -> BlobRef:
    value = object_value(value)
    return BlobRef(string(value.get("sha256")), integer(value.get("bytes")))


def decode_report(value: Any) -> VerificationReport:
    value = object_value(value)
    return VerificationReport(
        name=string(value.get("name")),
        command=string(value.get("command")),
        cwd=string(value.get("cwd")),
        timeout_ms=integer(value.get("timeout_ms")),
        status=enum(
            value.get("status"),
            {
                "not_run",
                "running",
                "passed",
                "failed",
                "cancelled",
                "denied",
                "interrupted",
            },
        ),
        operation_id=optional_string(value.get("operation_id")),
        started_at_ms=optional_integer(value.get("started_at_ms")),
        finished_at_ms=optional_integer(value.get("finished_at_ms")),
        exit_code=optional_integer(value.get("exit_code")),
        output_refs=[decode_blob(item) for item in array(value.get("output_refs"))],
        message=optional_string(value.get("message")),
    )


def decode_operation(value: Any) -> Operation:
    value = object_value(value)
    kind = enum(value.get("type"), {"read", "write", "bash", "web_search", "web_open"})
    if kind in {"read", "write"}:
        for path in array(value.get("paths")):
            string(path)
        boolean(value.get("external"))
        if kind == "write":
            boolean(value.get("destructive"))
    elif kind == "bash":
        string(value.get("command"))
        string(value.get("cwd"))
        enum(value.get("class"), {"read_only", "mutating", "unknown"})
        integer(value.get("timeout_ms"))
    elif kind == "web_search":
        string(value.get("query"))
        boolean(value.get("contains_workspace_data"))
    elif kind == "web_open":
        string(value.get("url"))
        boolean(value.get("private_target"))
    return cast(Operation, value)


def decode_event(value: Any) -> Event:
    value = object_value(value)
    kind = value.get("type")
    if kind == "text":
        return TextEvent(string(value.get("text")))
    if kind == "approval":
        request = object_value(value.get("request"))
        return ApprovalEvent(
            ApprovalRequest(
                string(request.get("operation_id")),
                decode_operation(request.get("operation")),
                string(request.get("summary")),
                request.get("arguments", {}),
            )
        )
    if kind == "tool_started":
        return ToolStartedEvent(
            string(value.get("operation_id")),
            string(value.get("name")),
            string(value.get("context")),
        )
    if kind == "tool_output":
        return ToolOutputEvent(
            string(value.get("call_id")),
            string(value.get("stream")),
            string(value.get("chunk")),
        )
    if kind == "tool_completed":
        result = object_value(value.get("result"))
        return ToolCompletedEvent(
            string(value.get("operation_id")),
            ToolResult(
                string(result.get("call_id")),
                string(result.get("output")),
                boolean(result.get("is_error")),
                result.get("metadata"),
                boolean(result.get("truncated")),
                [decode_blob(item) for item in array(result.get("blob_refs"))],
            ),
        )
    if kind == "agent_updated":
        snapshot = object_value(value.get("snapshot"))
        return AgentUpdatedEvent(
            AgentSnapshot(
                string(snapshot.get("id")),
                string(snapshot.get("role")),
                string(snapshot.get("objective")),
                string(snapshot.get("profile")),
                enum(
                    snapshot.get("state"),
                    {"queued", "running", "completed", "failed", "cancelled"},
                ),
                optional_string(snapshot.get("phase")),
                optional_string(snapshot.get("active_operation")),
                [string(item) for item in array(snapshot.get("changed_files"))],
                optional_string(snapshot.get("last_error")),
            )
        )
    if kind == "usage":
        usage = object_value(value.get("usage"))
        return UsageEvent(
            Usage(
                integer(usage.get("input_tokens")),
                integer(usage.get("output_tokens")),
                integer(usage.get("cached_input_tokens")),
            )
        )
    if kind == "status":
        return StatusEvent(string(value.get("message")))
    if kind == "verification":
        return VerificationEvent(decode_report(value.get("report")))
    if kind == "runtime":
        return RuntimeEvent(object_value(value.get("event")))
    if kind == "done":
        status = enum(value.get("status"), {"completed", "cancelled", "failed"})
        return DoneEvent(
            status, decode_error(value["error"]) if "error" in value else None
        )
    raise ProtocolError(
        "Unrecognized friendly event type; install compatible SDK and Kurama versions."
    )
