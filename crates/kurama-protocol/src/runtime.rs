use crate::{
    agent::AgentSnapshot,
    id::{AgentId, CallId, OperationId},
    model::Usage,
    policy::{ApprovalRequest, ApprovalResponse, ExecutionMode},
    session::SessionGoal,
    tool::ToolResult,
    verification::{VerificationRecipe, VerificationReport},
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineCommand {
    SubmitTurn {
        text: String,
        explicit_delegation: bool,
    },
    Steer {
        text: String,
        explicit_delegation: bool,
    },
    InspectContext,
    Verify {
        name: String,
        recipe: VerificationRecipe,
    },
    InspectVerifications {
        recipes: std::collections::BTreeMap<String, VerificationRecipe>,
    },
    ResolveApproval {
        operation_id: OperationId,
        response: ApprovalResponse,
    },
    CancelTurn,
    Compact,
    SetMode(ExecutionMode),
    SetGoal {
        objective: String,
    },
    EditGoal {
        objective: String,
    },
    PauseGoal,
    ResumeGoal,
    ClearGoal,
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
pub struct ContextInspection {
    pub max_input_tokens: u64,
    pub reserved_output_tokens: u64,
    pub usable_tokens: u64,
    pub estimated_tokens: u64,
    pub categories: Vec<ContextCategory>,
    pub total_completed_turns: usize,
    pub included_recent_turns: usize,
    pub omitted_turns: usize,
    pub summary_covered_through_sequence: Option<u64>,
    pub compaction: Option<CompactionPreview>,
    pub assembly_error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextCategory {
    pub name: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactionPreview {
    pub covered_through_sequence: u64,
    pub event_count: usize,
    pub estimated_tokens: u64,
    pub fits_budget: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Ready,
    VerificationUpdated {
        report: VerificationReport,
    },
    VerificationsInspected {
        reports: Vec<VerificationReport>,
    },
    Status {
        message: String,
    },
    SteeringQueued {
        text: String,
    },
    SteeringApplied {
        text: String,
    },
    SteeringRejected {
        text: String,
        message: String,
    },
    ContextInspected {
        inspection: ContextInspection,
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
    Usage {
        usage: Usage,
    },
    TurnCompleted,
    GoalUpdated {
        goal: SessionGoal,
    },
    GoalCleared,
    Error {
        message: String,
    },
    Shutdown,
}
