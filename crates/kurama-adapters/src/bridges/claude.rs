use std::path::{Path, PathBuf};

use kurama_protocol::{
    KuramaError,
    model::{BackendCapabilities, BackendCursor, FinishReason, ModelEvent, ModelRequest, Usage},
    traits::{BoxFuture, CancelSignal, ModelBackend, ModelStream},
};
use serde_json::Value;

use super::{
    BridgeCommand,
    control::{bridge_prompt, control_schema, parse_control},
};

const BACKEND: &str = "claude_cli";

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
            "--include-partial-messages".into(),
            "--verbose".into(),
            "--safe-mode".into(),
            "--tools".into(),
            "".into(),
            "--strict-mcp-config".into(),
            "--permission-mode".into(),
            "manual".into(),
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
            stdin: bridge_prompt(request),
            cwd: None,
        }
    }

    pub fn parse_fixture(fixture: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        parse_lines(fixture.lines(), false)
    }
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
            let lines = command.run(cancel, &self.secrets).await?;
            let events = parse_lines(
                lines.iter().map(String::as_str),
                request.delegation.is_some(),
            )
            .map_err(|error| super::control::bounded_kurama_error(error, &self.secrets))?;
            Ok(super::event_stream(events.into_iter().map(Ok).collect()))
        })
    }
}

fn parse_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    delegation_enabled: bool,
) -> Result<Vec<ModelEvent>, KuramaError> {
    let mut events = Vec::new();
    let mut session_id = None;
    let mut control = None;
    let mut partial = String::new();
    let mut completed = false;
    for line in lines {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| KuramaError::Protocol(format!("invalid Claude JSONL: {error}")))?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "system" if value.get("subtype").and_then(Value::as_str) == Some("init") => {
                if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                    session_id = Some(id.to_owned());
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
                    partial.push_str(text);
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
                        control = Some(text);
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
                    session_id = Some(id.to_owned());
                }
                let control_value = value
                    .get("structured_output")
                    .map(Value::to_string)
                    .or_else(|| {
                        value
                            .get("result")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .or_else(|| control.take())
                    .filter(|text| !text.is_empty())
                    .unwrap_or(partial.clone());
                let normalized = parse_control(&control_value, delegation_enabled)?;
                let tool_calls = normalized.iter().any(|event| {
                    matches!(
                        event,
                        ModelEvent::ToolCall { .. } | ModelEvent::Delegation { .. }
                    )
                });
                events.extend(normalized);
                events.push(ModelEvent::ResponseCompleted {
                    cursor: session_id.clone().map(|value| BackendCursor {
                        backend: BACKEND.into(),
                        value,
                    }),
                    finish_reason: if tool_calls {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    },
                });
                completed = true;
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
    }
    if !completed {
        return Err(KuramaError::Protocol(
            "Claude JSONL ended before result".into(),
        ));
    }
    Ok(events)
}
