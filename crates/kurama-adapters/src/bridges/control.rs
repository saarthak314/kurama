use std::{fs, path::Path};

use kurama_protocol::{
    KuramaError,
    agent::{AgentBudget, AgentSpec, DelegationRequest, WriteScope},
    id::CallId,
    model::{ModelEvent, ModelRequest},
};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_PROMPT_BYTES: usize = 256 * 1024;
const MAX_ERROR_BYTES: usize = 16 * 1024;

pub fn control_schema(delegation_enabled: bool) -> Value {
    let mut branches = vec![
        json!({
            "type": "object",
            "properties": {"kind": {"const": "final"}, "text": {"type": "string"}},
            "required": ["kind", "text"],
            "additionalProperties": false
        }),
        json!({
            "type": "object",
            "properties": {
                "kind": {"const": "tool_calls"},
                "calls": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": {
                        "type": "object",
                        "properties": {
                            "call_id": {"type": "string"},
                            "name": {"enum": ["read", "write", "bash", "web-search"]},
                            "arguments": {"type": "object"}
                        },
                        "required": ["call_id", "name", "arguments"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["kind", "calls"],
            "additionalProperties": false
        }),
    ];
    if delegation_enabled {
        branches.push(json!({
            "type": "object",
            "properties": {
                "kind": {"const": "delegate"},
                "agents": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 8,
                    "items": {
                        "type": "object",
                        "properties": {
                            "objective": {"type": "string"},
                            "write_roots": {"type": "array", "items": {"type": "string"}},
                            "write_files": {"type": "array", "items": {"type": "string"}},
                            "depends_on": {
                                "type": "array",
                                "description": "Exact objective strings of prerequisite agents.",
                                "items": {"type": "string"}
                            }
                        },
                        "required": ["objective", "write_roots", "write_files", "depends_on"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["kind", "agents"],
            "additionalProperties": false
        }));
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "oneOf": branches
    })
}

pub fn write_control_schema(path: &Path, delegation_enabled: bool) -> Result<(), KuramaError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let encoded = serde_json::to_vec(&control_schema(delegation_enabled))
        .map_err(|error| KuramaError::Configuration(format!("bridge control schema: {error}")))?;
    if fs::read(path).ok().as_deref() == Some(encoded.as_slice()) {
        return Ok(());
    }
    fs::write(path, encoded)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn bridge_prompt(request: &ModelRequest) -> String {
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters
            })
        })
        .collect::<Vec<_>>();
    let context = serde_json::to_string(&request.items).unwrap_or_else(|_| "[]".into());
    let mut prompt = format!(
        "{}\n\nYou are a model bridge. Do not use any CLI-provided tools, filesystem access, shell access, web access, plugins, skills, agents, or custom instructions. Return exactly one JSON control object matching the supplied schema.\n\nAvailable Kurama tools:\n{}\n\nActive context:\n{}",
        request.system,
        serde_json::to_string(&tools).unwrap_or_else(|_| "[]".into()),
        context
    );
    if request.delegation.is_none() {
        prompt.push_str("\n\nDelegation is disabled for this turn.");
    } else {
        prompt.push_str("\n\nKurama assigns child roles and profiles. Delegation dependencies must name the exact objective text of prerequisite agents.");
    }
    truncate_utf8(&mut prompt, MAX_PROMPT_BYTES);
    prompt
}

pub fn parse_control(text: &str, delegation_enabled: bool) -> Result<Vec<ModelEvent>, KuramaError> {
    let control: Control = serde_json::from_str(text).map_err(|error| {
        KuramaError::Protocol(format!("invalid bridge control output: {error}"))
    })?;
    match control {
        Control::Final { text } => Ok(vec![ModelEvent::TextDelta { text }]),
        Control::ToolCalls { calls } => {
            if calls.is_empty() || calls.len() > 8 {
                return Err(KuramaError::Protocol(
                    "bridge returned an invalid tool-call count".into(),
                ));
            }
            calls.into_iter().map(ControlCall::event).collect()
        }
        Control::Delegate { agents } => {
            if !delegation_enabled {
                return Err(KuramaError::Protocol(
                    "bridge returned delegation while disabled".into(),
                ));
            }
            if agents.is_empty() || agents.len() > 8 {
                return Err(KuramaError::Protocol(
                    "bridge returned an invalid agent count".into(),
                ));
            }
            Ok(vec![ModelEvent::Delegation {
                request: DelegationRequest {
                    agents: agents.into_iter().map(ControlAgent::spec).collect(),
                },
            }])
        }
    }
}

pub fn bounded_error(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret, "[REDACTED]");
        }
    }
    if text.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

pub fn bounded_kurama_error(error: KuramaError, secrets: &[String]) -> KuramaError {
    match error {
        KuramaError::Configuration(message) => {
            KuramaError::Configuration(bounded_error(&message, secrets))
        }
        KuramaError::Model(message) => KuramaError::Model(bounded_error(&message, secrets)),
        KuramaError::Tool(message) => KuramaError::Tool(bounded_error(&message, secrets)),
        KuramaError::Protocol(message) => KuramaError::Protocol(bounded_error(&message, secrets)),
        other => other,
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Final { text: String },
    ToolCalls { calls: Vec<ControlCall> },
    Delegate { agents: Vec<ControlAgent> },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlCall {
    call_id: String,
    name: String,
    arguments: Value,
}

impl ControlCall {
    fn event(self) -> Result<ModelEvent, KuramaError> {
        if !matches!(self.name.as_str(), "read" | "write" | "bash" | "web-search") {
            return Err(KuramaError::Protocol(format!(
                "bridge returned unknown tool {}",
                self.name
            )));
        }
        if !self.arguments.is_object() {
            return Err(KuramaError::Protocol(
                "bridge tool arguments must be an object".into(),
            ));
        }
        Ok(ModelEvent::ToolCall {
            call_id: CallId::from(self.call_id),
            name: self.name,
            arguments: self.arguments,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlAgent {
    objective: String,
    write_roots: Vec<String>,
    write_files: Vec<String>,
    depends_on: Vec<String>,
}

impl ControlAgent {
    fn spec(self) -> AgentSpec {
        AgentSpec {
            role: String::new(),
            objective: self.objective,
            profile: None,
            context_refs: Vec::new(),
            write_scope: WriteScope {
                roots: self.write_roots.into_iter().map(Into::into).collect(),
                files: self.write_files.into_iter().map(Into::into).collect(),
            },
            budget: AgentBudget::default(),
            depends_on: self.depends_on,
        }
    }
}

fn truncate_utf8(text: &mut String, max_bytes: usize) {
    if text.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push('…');
}
