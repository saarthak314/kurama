"""Typed, friendly events. Extensible Rust runtime payloads remain JSON objects."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Awaitable, Callable, Literal, TypeAlias, TypedDict

JSON: TypeAlias = "None | bool | int | float | str | list[JSON] | dict[str, JSON]"
Mode: TypeAlias = Literal["supervised", "auto", "yolo"]
TurnStatus: TypeAlias = Literal["completed", "cancelled", "failed"]
VerificationStatus: TypeAlias = Literal[
    "not_run", "running", "passed", "failed", "cancelled", "denied", "interrupted"
]


class ReadOperation(TypedDict):
    type: Literal["read"]
    paths: list[str]
    external: bool


class WriteOperation(TypedDict):
    type: Literal["write"]
    paths: list[str]
    destructive: bool
    external: bool


# The wire key is the Python keyword "class"; dictionary access preserves it.
BashOperation = TypedDict(
    "BashOperation",
    {
        "type": Literal["bash"],
        "command": str,
        "cwd": str,
        "class": Literal["read_only", "mutating", "unknown"],
        "timeout_ms": int,
    },
)


class WebSearchOperation(TypedDict):
    type: Literal["web_search"]
    query: str
    contains_workspace_data: bool


class WebOpenOperation(TypedDict):
    type: Literal["web_open"]
    url: str
    private_target: bool


Operation: TypeAlias = (
    ReadOperation
    | WriteOperation
    | BashOperation
    | WebSearchOperation
    | WebOpenOperation
)


class EditedArguments(TypedDict):
    arguments: JSON


class EditApproval(TypedDict):
    edit: EditedArguments


ApprovalResponse: TypeAlias = (
    Literal["approve_once", "approve_session", "deny"] | EditApproval
)


@dataclass(frozen=True, slots=True)
class ApprovalRequest:
    operation_id: str
    operation: Operation
    summary: str
    arguments: JSON


ApprovalCallback: TypeAlias = Callable[
    [ApprovalRequest], ApprovalResponse | Awaitable[ApprovalResponse]
]


@dataclass(frozen=True, slots=True)
class ErrorInfo:
    code: str
    message: str


@dataclass(frozen=True, slots=True)
class Reply:
    text: str
    session_id: str
    status: TurnStatus


@dataclass(frozen=True, slots=True)
class BlobRef:
    sha256: str
    bytes: int


@dataclass(frozen=True, slots=True)
class VerificationReport:
    name: str
    command: str
    cwd: str
    timeout_ms: int
    status: VerificationStatus
    operation_id: str | None
    started_at_ms: int | None
    finished_at_ms: int | None
    exit_code: int | None
    output_refs: list[BlobRef]
    message: str | None


@dataclass(frozen=True, slots=True)
class ToolResult:
    call_id: str
    output: str
    is_error: bool
    metadata: JSON
    truncated: bool
    blob_refs: list[BlobRef]


@dataclass(frozen=True, slots=True)
class Usage:
    input_tokens: int
    output_tokens: int
    cached_input_tokens: int


@dataclass(frozen=True, slots=True)
class AgentSnapshot:
    id: str
    role: str
    objective: str
    profile: str
    state: Literal["queued", "running", "completed", "failed", "cancelled"]
    phase: str | None
    active_operation: str | None
    changed_files: list[str]
    last_error: str | None


@dataclass(frozen=True, slots=True)
class TextEvent:
    text: str
    type: Literal["text"] = field(default="text", init=False)


@dataclass(frozen=True, slots=True)
class ApprovalEvent:
    request: ApprovalRequest
    type: Literal["approval"] = field(default="approval", init=False)


@dataclass(frozen=True, slots=True)
class ToolStartedEvent:
    operation_id: str
    name: str
    context: str
    type: Literal["tool_started"] = field(default="tool_started", init=False)


@dataclass(frozen=True, slots=True)
class ToolOutputEvent:
    call_id: str
    stream: str
    chunk: str
    type: Literal["tool_output"] = field(default="tool_output", init=False)


@dataclass(frozen=True, slots=True)
class ToolCompletedEvent:
    operation_id: str
    result: ToolResult
    type: Literal["tool_completed"] = field(default="tool_completed", init=False)


@dataclass(frozen=True, slots=True)
class AgentUpdatedEvent:
    snapshot: AgentSnapshot
    type: Literal["agent_updated"] = field(default="agent_updated", init=False)


@dataclass(frozen=True, slots=True)
class UsageEvent:
    usage: Usage
    type: Literal["usage"] = field(default="usage", init=False)


@dataclass(frozen=True, slots=True)
class StatusEvent:
    message: str
    type: Literal["status"] = field(default="status", init=False)


@dataclass(frozen=True, slots=True)
class VerificationEvent:
    report: VerificationReport
    type: Literal["verification"] = field(default="verification", init=False)


@dataclass(frozen=True, slots=True)
class RuntimeEvent:
    event: dict[str, JSON]
    type: Literal["runtime"] = field(default="runtime", init=False)


@dataclass(frozen=True, slots=True)
class DoneEvent:
    status: TurnStatus
    error: ErrorInfo | None = None
    type: Literal["done"] = field(default="done", init=False)


Event: TypeAlias = (
    TextEvent
    | ApprovalEvent
    | ToolStartedEvent
    | ToolOutputEvent
    | ToolCompletedEvent
    | AgentUpdatedEvent
    | UsageEvent
    | StatusEvent
    | VerificationEvent
    | RuntimeEvent
    | DoneEvent
)
