use std::{borrow::Cow, fs, path::Path};

use kurama_protocol::{
    KuramaError,
    agent::{AgentBudget, AgentSpec, DelegationRequest, WriteScope},
    id::CallId,
    model::{ModelEvent, ModelRequest},
};
use serde::{
    Deserialize, Deserializer,
    de::{DeserializeOwned, Error as _},
};
use serde_json::{Value, json};

const MAX_ERROR_BYTES: usize = 16 * 1024;

pub fn control_schema(delegation_enabled: bool) -> Value {
    let mut kinds = vec!["final", "tool_calls"];
    if delegation_enabled {
        kinds.push("delegate");
    }
    json!({
        "type": "object",
        "properties": {
            "kind": {"type": "string", "enum": kinds},
            "text": {"type": "string"},
            "calls": {
                "type": "array",
                "maxItems": 8,
                "items": {
                    "type": "object",
                    "properties": {
                        "call_id": {"type": "string"},
                        "name": {"type": "string", "enum": ["read", "write", "bash", "web-search"]},
                        "arguments": {
                            "type": "string",
                            "description": "A JSON-encoded object containing the tool arguments."
                        }
                    },
                    "required": ["call_id", "name", "arguments"],
                    "additionalProperties": false
                }
            },
            "agents": {
                "type": "array",
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
        "required": ["kind", "text", "calls", "agents"],
        "additionalProperties": false
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

pub fn bridge_system_prompt(request: &ModelRequest) -> String {
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
    let workspace_root =
        serde_json::to_string(&request.workspace_root).unwrap_or_else(|_| "\".\"".into());
    let mut prompt = format!(
        concat!(
            "{}\n\n",
            "You are a model bridge and stateless protocol adapter. Return exactly one JSON control object matching the supplied schema. Set fields unused by the selected kind to empty values; encode every tool arguments object as a JSON string. ",
            "Claude/Codex CLI tools are deliberately disabled and irrelevant. Never try to invoke them. The names listed below are Kurama protocol operations, not CLI tools. Every listed Kurama tool is available through this control protocol. Request one by returning kind=tool_calls; Kurama executes it after this response and sends its result in a later Active context. Do not claim any operation ran, failed, or was unavailable unless Active context contains its explicit result. ",
            "When the latest user request asks to use a listed Kurama tool and the active context does not already contain its result, return kind=tool_calls instead of kind=final. A tool process returning a nonzero exit status or a tool result with is_error=true is not evidence that the tool is missing or unavailable. Infer tool availability or unavailability only from explicit tool-result content or an engine error that states it. Never invent an unavailable-tool failure.\n\n",
            "Kurama workspace root: {}\nResolve every relative tool path against that root. For bash calls without a user-specified working directory, set cwd to that exact root; never use the bridge process working directory.\n\n",
            "Available Kurama tools:\n{}"
        ),
        request.system,
        workspace_root,
        serde_json::to_string(&tools).unwrap_or_else(|_| "[]".into())
    );
    if request.delegation.is_none() {
        prompt.push_str("\n\nDelegation is disabled for this turn.");
    } else {
        prompt.push_str("\n\nKurama assigns child roles and profiles. Delegation dependencies must name the exact objective text of prerequisite agents.");
    }
    prompt
}

pub fn bridge_context_prompt(request: &ModelRequest) -> String {
    let context = serde_json::to_string(&request.items).unwrap_or_else(|_| "[]".into());
    format!("Active context:\n{context}")
}

pub fn bridge_prompt(request: &ModelRequest) -> String {
    format!(
        "{}\n\n{}",
        bridge_system_prompt(request),
        bridge_context_prompt(request)
    )
}

pub fn parse_control(text: &str, delegation_enabled: bool) -> Result<Vec<ModelEvent>, KuramaError> {
    let control: Control = parse_json_with_escape_recovery(text).map_err(|error| {
        KuramaError::Protocol(format!("invalid bridge control output: {error}"))
    })?;
    match control.kind {
        ControlKind::Final => {
            if control.text.trim().is_empty()
                || !control.calls.is_empty()
                || !control.agents.is_empty()
            {
                return Err(KuramaError::Protocol(
                    "bridge returned an invalid final control object".into(),
                ));
            }
            Ok(vec![ModelEvent::TextDelta { text: control.text }])
        }
        ControlKind::ToolCalls => {
            if !control.text.is_empty() || !control.agents.is_empty() {
                return Err(KuramaError::Protocol(
                    "bridge returned contaminated tool-call control".into(),
                ));
            }
            let calls = control.calls;
            if calls.is_empty() || calls.len() > 8 {
                return Err(KuramaError::Protocol(
                    "bridge returned an invalid tool-call count".into(),
                ));
            }
            calls.into_iter().map(ControlCall::event).collect()
        }
        ControlKind::Delegate => {
            if !delegation_enabled {
                return Err(KuramaError::Protocol(
                    "bridge returned delegation while disabled".into(),
                ));
            }
            if !control.text.is_empty() || !control.calls.is_empty() {
                return Err(KuramaError::Protocol(
                    "bridge returned contaminated delegation control".into(),
                ));
            }
            let agents = control.agents;
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
#[serde(deny_unknown_fields)]
struct Control {
    kind: ControlKind,
    #[serde(default)]
    text: String,
    #[serde(default)]
    calls: Vec<ControlCall>,
    #[serde(default)]
    agents: Vec<ControlAgent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlKind {
    Final,
    ToolCalls,
    Delegate,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlCall {
    call_id: String,
    name: String,
    #[serde(deserialize_with = "deserialize_arguments")]
    arguments: Value,
}

fn deserialize_arguments<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(encoded) => {
            parse_json_with_escape_recovery(&encoded).map_err(D::Error::custom)
        }
        value => Ok(value),
    }
}

fn parse_json_with_escape_recovery<T: DeserializeOwned>(
    text: &str,
) -> Result<T, serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(value) => Ok(value),
        Err(error) => match repair_invalid_json_escapes(text) {
            Cow::Borrowed(_) => Err(error),
            Cow::Owned(repaired) => serde_json::from_str(&repaired),
        },
    }
}

fn repair_invalid_json_escapes(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut repaired = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut changed = false;
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        if !in_string {
            repaired.push(byte);
            if byte == b'"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        match byte {
            b'"' => {
                repaired.push(byte);
                in_string = false;
                index += 1;
            }
            b'\\' => {
                let valid_escape_len = match bytes.get(index + 1).copied() {
                    Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => 2,
                    Some(b'u')
                        if bytes
                            .get(index + 2..index + 6)
                            .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit)) =>
                    {
                        6
                    }
                    _ => 0,
                };
                if valid_escape_len == 0 {
                    repaired.extend_from_slice(b"\\\\");
                    changed = true;
                    index += 1;
                } else {
                    repaired.extend_from_slice(&bytes[index..index + valid_escape_len]);
                    index += valid_escape_len;
                }
            }
            b'\n' => {
                repaired.extend_from_slice(b"\\n");
                changed = true;
                index += 1;
            }
            b'\r' => {
                repaired.extend_from_slice(b"\\r");
                changed = true;
                index += 1;
            }
            b'\t' => {
                repaired.extend_from_slice(b"\\t");
                changed = true;
                index += 1;
            }
            0x08 => {
                repaired.extend_from_slice(b"\\b");
                changed = true;
                index += 1;
            }
            0x0c => {
                repaired.extend_from_slice(b"\\f");
                changed = true;
                index += 1;
            }
            control if control < 0x20 => {
                repaired.extend_from_slice(format!("\\u{control:04x}").as_bytes());
                changed = true;
                index += 1;
            }
            _ => {
                repaired.push(byte);
                index += 1;
            }
        }
    }

    if changed {
        Cow::Owned(String::from_utf8(repaired).expect("repair preserves UTF-8"))
    } else {
        Cow::Borrowed(text)
    }
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
