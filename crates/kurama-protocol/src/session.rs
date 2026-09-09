use std::path::PathBuf;

use crate::{
    agent::AgentSnapshot,
    id::{AgentId, CallId, OperationId, SessionId},
    model::{BackendCursor, Usage},
    policy::{ApprovalResponse, ExecutionMode},
    tool::{Operation, ToolInvocation, ToolResult},
};

pub const MAX_TODO_ITEMS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

impl TodoItem {
    pub fn validate_list(items: &[Self]) -> Result<(), crate::KuramaError> {
        if items.len() > MAX_TODO_ITEMS {
            return Err(crate::KuramaError::Protocol(format!(
                "todo list cannot exceed {MAX_TODO_ITEMS} items"
            )));
        }
        let in_progress = items
            .iter()
            .filter(|item| item.status == TodoStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err(crate::KuramaError::Protocol(
                "todo list can have at most one in_progress item".into(),
            ));
        }
        let mut ids = std::collections::BTreeSet::new();
        for item in items {
            if item.id.trim().is_empty() || item.content.trim().is_empty() {
                return Err(crate::KuramaError::Protocol(
                    "todo items need a non-empty id and content".into(),
                ));
            }
            if !ids.insert(&item.id) {
                return Err(crate::KuramaError::Protocol(format!(
                    "duplicate todo id {}",
                    item.id
                )));
            }
        }
        Ok(())
    }
}

pub fn latest_todos<'a, I>(events: I) -> Vec<TodoItem>
where
    I: IntoIterator<Item = &'a EventEnvelope>,
{
    let mut todos = Vec::new();
    for event in events {
        if let SessionEvent::TodoUpdated { items } = &event.event {
            todos.clone_from(items);
        }
    }
    todos
}

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BlobRef {
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileCheckpoint {
    pub path: PathBuf,
    pub content: Option<BlobRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionMetadata {
    pub id: SessionId,
    pub created_at_ms: u64,
    pub project_root: String,
    pub profile: String,
    pub mode: ExecutionMode,
    pub redaction_best_effort: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub project_root: String,
    pub profile: String,
    pub mode: ExecutionMode,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub event: SessionEvent,
}

impl EventEnvelope {
    pub fn new(
        sequence: u64,
        timestamp_ms: u64,
        session_id: SessionId,
        agent_id: Option<AgentId>,
        event: SessionEvent,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            sequence,
            timestamp_ms,
            session_id,
            agent_id,
            event,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionStarted {
        metadata: SessionMetadata,
    },
    ModeSelected {
        mode: ExecutionMode,
    },
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ModelCursor {
        cursor: BackendCursor,
    },
    ModelUsage {
        usage: Usage,
    },
    ToolProposed {
        operation_id: OperationId,
        call_id: CallId,
        operation: Operation,
    },
    ToolInvocationRecorded {
        operation_id: OperationId,
        invocation: ToolInvocation,
    },
    WritePrepared {
        operation_id: OperationId,
        files: Vec<FileCheckpoint>,
    },
    ToolPrepared {
        operation_id: OperationId,
    },
    ToolStarted {
        operation_id: OperationId,
    },
    ToolCompleted {
        operation_id: OperationId,
        result: ToolResult,
    },
    WriteApplied {
        operation_id: OperationId,
        files: Vec<FileCheckpoint>,
        result: ToolResult,
    },
    ToolUnknown {
        operation_id: OperationId,
        reason: String,
    },
    ApprovalRequested {
        operation_id: OperationId,
        summary: String,
    },
    ApprovalResolved {
        operation_id: OperationId,
        response: ApprovalResponse,
    },
    ContextCompacted {
        covered_through_sequence: u64,
        summary: String,
        tokens: u64,
    },
    AgentQueued {
        snapshot: AgentSnapshot,
    },
    AgentStarted {
        snapshot: AgentSnapshot,
    },
    AgentProgress {
        snapshot: AgentSnapshot,
    },
    AgentCompleted {
        snapshot: AgentSnapshot,
        summary: String,
    },
    AgentFailed {
        snapshot: AgentSnapshot,
        error: String,
    },
    AgentCancelled {
        snapshot: AgentSnapshot,
    },
    AgentMessage {
        agent_id: AgentId,
        text: String,
    },
    TodoUpdated {
        items: Vec<TodoItem>,
    },
    TurnCompleted,
    TurnFailed {
        error: String,
    },
    RecoveryRepair {
        removed_bytes: u64,
    },
    RecoveryDecision {
        operation_id: OperationId,
        action: String,
    },
}
