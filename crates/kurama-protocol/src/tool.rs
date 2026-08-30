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

impl ToolLimits {
    pub fn from_model_input_budget(max_input_tokens: u64, reserved_output_tokens: u64) -> Self {
        let max_bytes = max_input_tokens
            .saturating_sub(reserved_output_tokens)
            .saturating_mul(3)
            .try_into()
            .unwrap_or(usize::MAX);
        Self {
            max_bytes,
            max_lines: usize::MAX,
        }
    }
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

pub fn split_shell_commands(command: &str) -> Option<Vec<&str>> {
    let mut single_quote = false;
    let mut double_quote = false;
    let mut escaped = false;
    let mut start = 0;
    let mut segments = Vec::new();
    let bytes = command.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let character = command[index..].chars().next()?;
        let character_len = character.len_utf8();
        if escaped {
            escaped = false;
            index += character_len;
            continue;
        }
        if character == '\\' && !single_quote {
            escaped = true;
            index += character_len;
            continue;
        }
        match character {
            '\'' if !double_quote => single_quote = !single_quote,
            '"' if !single_quote => double_quote = !double_quote,
            '>' | '<' | '`' | '$' | '(' | ')' if !single_quote && !double_quote => return None,
            '|' if !single_quote && !double_quote => {
                if bytes.get(index + 1) == Some(&b'|') {
                    return None;
                }
                push_shell_segment(command, start, index, &mut segments)?;
                start = index + 1;
            }
            '&' if !single_quote && !double_quote => {
                if bytes.get(index + 1) != Some(&b'&') {
                    return None;
                }
                push_shell_segment(command, start, index, &mut segments)?;
                index += 1;
                start = index + 1;
            }
            ';' | '\n' | '\r' if !single_quote && !double_quote => {
                push_shell_segment(command, start, index, &mut segments)?;
                start = index + character_len;
            }
            _ => {}
        }
        index += character_len;
    }
    if single_quote || double_quote || escaped {
        return None;
    }
    push_shell_segment(command, start, command.len(), &mut segments)?;
    Some(segments)
}

fn push_shell_segment<'a>(
    command: &'a str,
    start: usize,
    end: usize,
    segments: &mut Vec<&'a str>,
) -> Option<()> {
    let segment = command.get(start..end)?.trim();
    if segment.is_empty() {
        return None;
    }
    segments.push(segment);
    Some(())
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
