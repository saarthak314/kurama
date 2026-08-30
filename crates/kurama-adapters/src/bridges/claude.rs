use std::path::{Path, PathBuf};

use kurama_protocol::{
    KuramaError,
    model::{BackendCapabilities, BackendCursor, FinishReason, ModelEvent, ModelRequest, Usage},
    traits::{BoxFuture, CancelSignal, ModelBackend, ModelStream},
};
use serde_json::{Value, json};

use super::{
    BridgeCommand, BridgeDecoder, MAX_JSONL_LINE_BYTES,
    control::{bridge_context_prompt, bridge_system_prompt, control_schema, parse_control},
};

const BACKEND: &str = "claude_cli";
const CORE_TOOL_INSTRUCTION: &str = "Operate only through the supplied tools.";

#[derive(Clone)]
pub struct ClaudeBridge {
    program: String,
    schema_path: PathBuf,
    secrets: Vec<String>,
}

impl ClaudeBridge {
    pub fn new(schema_path: impl Into<PathBuf>) -> Self {
        Self {
            program: "claude".into(),
            schema_path: schema_path.into(),
            secrets: Vec::new(),
        }
    }

    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    pub fn with_redactions(mut self, secrets: Vec<String>) -> Self {
        self.secrets = secrets;
        self
    }

    pub fn command_for(
        request: &ModelRequest,
        cursor: Option<&BackendCursor>,
        schema_path: &Path,
    ) -> BridgeCommand {
        Self::command_for_program("claude", request, cursor, schema_path)
    }

    fn command_for_program(
        program: &str,
        request: &ModelRequest,
        cursor: Option<&BackendCursor>,
        _schema_path: &Path,
    ) -> BridgeCommand {
        let mut args = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--safe-mode".into(),
            "--tools".into(),
            "".into(),
            "--strict-mcp-config".into(),
            "--permission-mode".into(),
            "manual".into(),
            "--system-prompt".into(),
            claude_system_prompt(request),
            "--model".into(),
            request.profile.model.clone(),
            "--json-schema".into(),
            control_schema(request.delegation.is_some()).to_string(),
        ];
        if let Some(cursor) = cursor.filter(|cursor| cursor.backend == BACKEND) {
            args.push("--resume".into());
            args.push(cursor.value.clone());
        }
        BridgeCommand {
            program: program.into(),
            args,
            stdin: bridge_context_prompt(request),
            cwd: None,
        }
    }

    pub fn parse_fixture(fixture: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        parse_lines(fixture.lines(), false)
    }
}

fn claude_system_prompt(request: &ModelRequest) -> String {
    let mut prompt = bridge_system_prompt(request).replace(
        CORE_TOOL_INSTRUCTION,
        "Request external actions only through the returned control object.",
    );
    prompt.push_str(
        "\n\nThe only Claude tool you may invoke is StructuredOutput. Never invoke read, write, bash, or web-search as Claude tools. Encode those Kurama operations only inside the control object's calls array.",
    );
    prompt
}

impl ModelBackend for ClaudeBridge {
    fn backend_name(&self) -> &'static str {
        BACKEND
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            streaming: true,
            tool_calls: true,
            native_web_search: false,
            resumable: true,
        }
    }

    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<ModelStream, KuramaError>> {
        Box::pin(async move {
            super::control::write_control_schema(&self.schema_path, request.delegation.is_some())?;
            let command = Self::command_for_program(
                &self.program,
                &request,
                request.continuation.as_ref(),
                &self.schema_path,
            );
            super::event_stream(
                command,
                ClaudeDecoder::new(request.delegation.is_some()),
                cancel,
                self.secrets.clone(),
            )
            .await
        })
    }
}

fn parse_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    delegation_enabled: bool,
) -> Result<Vec<ModelEvent>, KuramaError> {
    let mut decoder = ClaudeDecoder::new(delegation_enabled);
    let mut events = Vec::new();
    for line in lines {
        events.extend(decoder.push_line(line)?);
    }
    decoder.finish()?;
    Ok(events)
}

struct ClaudeDecoder {
    delegation_enabled: bool,
    session_id: Option<String>,
    control: Option<String>,
    structured_control: Option<String>,
    protocol_calls: Vec<ModelEvent>,
    recovered_native_calls: bool,
    partial: String,
    completed: bool,
}

impl ClaudeDecoder {
    fn new(delegation_enabled: bool) -> Self {
        Self {
            delegation_enabled,
            session_id: None,
            control: None,
            structured_control: None,
            protocol_calls: Vec::new(),
            recovered_native_calls: false,
            partial: String::new(),
            completed: false,
        }
    }
}

impl BridgeDecoder for ClaudeDecoder {
    fn push_line(&mut self, line: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        let mut events = Vec::new();
        let value: Value = serde_json::from_str(line)
            .map_err(|error| KuramaError::Protocol(format!("invalid Claude JSONL: {error}")))?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "system" if value.get("subtype").and_then(Value::as_str) == Some("init") => {
                if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(id.to_owned());
                    events.push(ModelEvent::ResponseStarted {
                        provider_id: id.to_owned(),
                    });
                }
            }
            "stream_event" => {
                if value.pointer("/event/type").and_then(Value::as_str)
                    == Some("content_block_delta")
                    && value.pointer("/event/delta/type").and_then(Value::as_str)
                        == Some("text_delta")
                    && let Some(text) = value.pointer("/event/delta/text").and_then(Value::as_str)
                {
                    if self.partial.len().saturating_add(text.len()) > MAX_JSONL_LINE_BYTES {
                        return Err(KuramaError::Protocol(
                            "Claude streamed an oversized control response".into(),
                        ));
                    }
                    self.partial.push_str(text);
                }
            }
            "assistant" => {
                if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
                    let text = content
                        .iter()
                        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|block| block.get("text").and_then(Value::as_str))
                        .collect::<String>();
                    if !text.is_empty() {
                        self.control = Some(text);
                    }
                    for block in content {
                        let Some(name) = block.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(input) = block.get("input") else {
                            continue;
                        };
                        if name == "StructuredOutput" {
                            if input.is_object() {
                                self.structured_control = Some(input.to_string());
                            }
                            continue;
                        }
                        if !matches!(name, "read" | "write" | "bash" | "web-search") {
                            continue;
                        }
                        let Some(input) = input.as_object() else {
                            continue;
                        };
                        let Some(call_id) = input
                            .get("call_id")
                            .or_else(|| block.get("id"))
                            .and_then(Value::as_str)
                        else {
                            continue;
                        };
                        let arguments = match input.get("arguments") {
                            Some(arguments) => arguments.clone(),
                            None => {
                                let mut arguments = input.clone();
                                arguments.remove("call_id");
                                Value::Object(arguments)
                            }
                        };
                        let encoded = json!({
                            "kind": "tool_calls",
                            "text": "",
                            "calls": [{
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments
                            }],
                            "agents": []
                        })
                        .to_string();
                        if self.protocol_calls.len() >= 8 {
                            return Err(KuramaError::Protocol(
                                "Claude emitted too many protocol tool calls".into(),
                            ));
                        }
                        self.protocol_calls
                            .extend(parse_control(&encoded, self.delegation_enabled)?);
                        self.recovered_native_calls = true;
                    }
                }
            }
            "result" => {
                if value
                    .get("subtype")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "success")
                {
                    let error = value
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or("Claude CLI failed");
                    return Err(KuramaError::Model(error.to_owned()));
                }
                if let Some(usage) = value.get("usage") {
                    events.push(ModelEvent::Usage {
                        usage: Usage {
                            input_tokens: usage
                                .get("input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or_default(),
                            output_tokens: usage
                                .get("output_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or_default(),
                            cached_input_tokens: usage
                                .get("cache_read_input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or_default(),
                        },
                    });
                }
                if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                    self.session_id = Some(id.to_owned());
                }
                let control_value = value
                    .get("structured_output")
                    .map(Value::to_string)
                    .or_else(|| self.structured_control.take())
                    .or_else(|| {
                        value
                            .get("result")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .or_else(|| self.control.take())
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| self.partial.clone());
                let normalized = if self.protocol_calls.is_empty() {
                    parse_control(&control_value, self.delegation_enabled)?
                } else {
                    std::mem::take(&mut self.protocol_calls)
                };
                let tool_calls = normalized.iter().any(|event| {
                    matches!(
                        event,
                        ModelEvent::ToolCall { .. } | ModelEvent::Delegation { .. }
                    )
                });
                events.extend(normalized);
                events.push(ModelEvent::ResponseCompleted {
                    cursor: if self.recovered_native_calls {
                        None
                    } else {
                        self.session_id.clone().map(|value| BackendCursor {
                            backend: BACKEND.into(),
                            value,
                        })
                    },
                    finish_reason: if tool_calls {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    },
                });
                self.completed = true;
            }
            "error" => {
                let message = value
                    .get("error")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .unwrap_or("Claude CLI failed");
                return Err(KuramaError::Model(message.to_owned()));
            }
            _ => {}
        }
        Ok(events)
    }

    fn finish(&self) -> Result<(), KuramaError> {
        if self.completed {
            Ok(())
        } else {
            Err(KuramaError::Protocol(
                "Claude JSONL ended before result".into(),
            ))
        }
    }
}
