use std::{collections::BTreeMap, path::PathBuf};

use crate::{id::AgentId, model::ModelProfile, session::BlobRef};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriteScope {
    pub roots: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
}

impl WriteScope {
    pub fn is_read_only(&self) -> bool {
        self.roots.is_empty() && self.files.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentBudget {
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_turns: u32,
    pub max_seconds: u64,
}

impl Default for AgentBudget {
    fn default() -> Self {
        Self {
            max_input_tokens: 80_000,
            max_output_tokens: 8_000,
            max_turns: 12,
            max_seconds: 1_800,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentLimits {
    pub default_concurrency: usize,
    pub max_concurrency: usize,
    pub max_depth: u8,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            default_concurrency: 4,
            max_concurrency: 8,
            max_depth: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentSpec {
    pub role: String,
    pub objective: String,
    pub profile: Option<String>,
    pub context_refs: Vec<String>,
    pub write_scope: WriteScope,
    pub budget: AgentBudget,
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResolvedAgentSpec {
    pub id: AgentId,
    pub parent_id: Option<AgentId>,
    pub depth: u8,
    pub role: String,
    pub objective: String,
    pub profile: ModelProfile,
    pub context_refs: Vec<String>,
    pub write_scope: WriteScope,
    pub budget: AgentBudget,
    pub depends_on: Vec<String>,
    pub escalation_profiles: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DelegationRequest {
    pub agents: Vec<AgentSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentSnapshot {
    pub id: AgentId,
    pub role: String,
    pub objective: String,
    pub profile: String,
    pub state: AgentState,
    pub phase: Option<String>,
    pub active_operation: Option<String>,
    pub changed_files: Vec<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestrationContext {
    pub parent_profile: ModelProfile,
    pub profiles: BTreeMap<String, ModelProfile>,
    pub role_routes: BTreeMap<String, String>,
    pub role_escalations: BTreeMap<String, Vec<String>>,
    pub profile_escalations: BTreeMap<String, Vec<String>>,
    pub parent_write_scope: WriteScope,
    pub max_concurrency: usize,
    pub depth: u8,
    pub yolo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchedulePlan {
    pub ready: Vec<ResolvedAgentSpec>,
    pub queued: Vec<ResolvedAgentSpec>,
    pub blocked: Vec<ResolvedAgentSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildBrief {
    pub role: String,
    pub objective: String,
    pub project_summary: String,
    pub context_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLaunch {
    pub parent_session_id: crate::id::SessionId,
    pub parent_agent_id: Option<AgentId>,
    pub depth: u8,
    pub brief: ChildBrief,
    pub profile: ModelProfile,
    pub budget: AgentBudget,
    pub write_scope: WriteScope,
    pub delegation_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentResult {
    pub agent_id: AgentId,
    pub summary: String,
    pub changed_files: Vec<String>,
    pub evidence_refs: Vec<BlobRef>,
}
