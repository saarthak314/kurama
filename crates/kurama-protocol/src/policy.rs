use std::path::PathBuf;

use crate::{agent::WriteScope, id::OperationId, tool::Operation};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Supervised,
    Auto,
    Yolo,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AutoBoundaries {
    pub write_roots: Vec<PathBuf>,
    pub allowed_commands: Vec<String>,
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContext {
    pub mode: ExecutionMode,
    pub workspace_root: PathBuf,
    pub write_scope: WriteScope,
    pub auto: AutoBoundaries,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Deny { reason: String },
    Ask { reason: String },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApprovalRequest {
    pub operation_id: OperationId,
    pub operation: Operation,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResponse {
    ApproveOnce,
    ApproveSession,
    Deny,
}
