use crate::{
    agent::DelegationRequest,
    id::{AgentId, CallId, SessionId},
    session::{BlobRef, TodoItem},
    tool::ToolDescriptor,
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelProfile {
    pub name: String,
    pub model: String,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

impl ModelProfile {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        max_input_tokens: u64,
        max_output_tokens: u64,
    ) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
            max_input_tokens,
            max_output_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackendCursor {
    pub backend: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackendCapabilities {
    pub streaming: bool,
    pub tool_calls: bool,
    pub native_web_search: bool,
    pub resumable: bool,
}

impl BackendCapabilities {
    pub const fn remote_default() -> Self {
        Self {
            streaming: true,
            tool_calls: true,
            native_web_search: false,
            resumable: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    ToolResult {
        call_id: CallId,
        name: String,
        content: String,
        is_error: bool,
        blob_refs: Vec<BlobRef>,
    },
    Summary {
        text: String,
        covered_through_sequence: u64,
        tokens: u64,
    },
    Evidence {
        path: String,
        content: String,
        blob: Option<BlobRef>,
    },
    AgentResult {
        agent_id: AgentId,
        summary: String,
        changed_files: Vec<String>,
        evidence_refs: Vec<BlobRef>,
    },
    TodoList {
        items: Vec<TodoItem>,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DelegationSchema {
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelRequest {
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub workspace_root: String,
    pub profile: ModelProfile,
    pub system: String,
    pub items: Vec<ModelItem>,
    pub tools: Vec<ToolDescriptor>,
    pub delegation: Option<DelegationSchema>,
    pub continuation: Option<BackendCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    ResponseStarted {
        provider_id: String,
    },
    TextDelta {
        text: String,
    },
    ToolCall {
        call_id: CallId,
        name: String,
        arguments: serde_json::Value,
    },
    Delegation {
        request: DelegationRequest,
    },
    Usage {
        usage: Usage,
    },
    ResponseCompleted {
        cursor: Option<BackendCursor>,
        finish_reason: FinishReason,
    },
}
