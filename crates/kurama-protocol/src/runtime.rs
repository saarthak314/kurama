use crate::{
    agent::AgentSnapshot,
    id::{AgentId, CallId, OperationId},
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    tool::ToolResult,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineCommand {
    SubmitTurn {
        text: String,
        explicit_delegation: bool,
    },
    ResolveApproval {
        operation_id: OperationId,
        response: ApprovalResponse,
    },
    CancelTurn,
    Compact,
    SetMode(ExecutionMode),
    Agent(AgentCommand),
    Shutdown,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentCommand {
    Inspect { agent_id: AgentId },
    Message { agent_id: AgentId, text: String },
    Cancel { agent_id: AgentId },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Status {
        message: String,
    },
    AssistantDelta {
        text: String,
    },
    ApprovalRequired {
        request: ApprovalRequest,
    },
    ToolStarted {
        operation_id: OperationId,
        name: String,
        context: String,
    },
    ToolOutputDelta {
        call_id: CallId,
        stream: String,
        chunk: String,
    },
    ToolCompleted {
        operation_id: OperationId,
        result: ToolResult,
    },
    AgentUpdated {
        snapshot: AgentSnapshot,
    },
    AgentInspection {
        snapshot: AgentSnapshot,
        transcript: Vec<String>,
    },
    TurnCompleted,
    Error {
        message: String,
    },
    Shutdown,
}
