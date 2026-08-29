use std::path::PathBuf;

use crate::{
    agent::WriteScope,
    id::{AgentId, CallId, SessionId},
    policy::ExecutionMode,
    session::BlobRef,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolLimits {
    pub max_bytes: usize,
    pub max_lines: usize,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            max_bytes: 65_536,
            max_lines: 2_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolInvocation {
    pub call_id: CallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    ReadOnly,
    Mutating,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Operation {
    Read {
        path: PathBuf,
        external: bool,
    },
    Write {
        paths: Vec<PathBuf>,
        destructive: bool,
        external: bool,
    },
    Bash {
        command: String,
        cwd: PathBuf,
        class: CommandClass,
        timeout_ms: u64,
    },
    WebSearch {
        query: String,
        contains_workspace_data: bool,
    },
    WebOpen {
        url: String,
        private_target: bool,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub call_id: CallId,
    pub output: String,
    pub is_error: bool,
    pub metadata: serde_json::Value,
    pub truncated: bool,
    pub blob_refs: Vec<BlobRef>,
}

impl ToolResult {
    pub fn success(call_id: CallId, output: impl Into<String>) -> Self {
        Self {
            call_id,
            output: output.into(),
            is_error: false,
            metadata: serde_json::Value::Null,
            truncated: false,
            blob_refs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub agent_id: Option<AgentId>,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub mode: ExecutionMode,
    pub limits: ToolLimits,
    pub write_scope: WriteScope,
}
