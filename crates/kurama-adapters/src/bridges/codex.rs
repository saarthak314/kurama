use std::{
    fs,
    path::{Path, PathBuf},
};

use kurama_protocol::{
    KuramaError,
    model::{BackendCapabilities, BackendCursor, FinishReason, ModelEvent, ModelRequest, Usage},
    traits::{BoxFuture, CancelSignal, ModelBackend, ModelStream},
};
use serde_json::Value;

use super::{
    BridgeCommand,
    control::{bridge_prompt, parse_control, write_control_schema},
};

const BACKEND: &str = "codex_cli";
const DISABLED_NATIVE_FEATURES: [&str; 8] = [
    "apps",
    "browser_use",
    "computer_use",
    "image_generation",
    "multi_agent",
    "shell_tool",
    "unified_exec",
    "view_image",
];

#[derive(Clone)]
pub struct CodexBridge {
    program: String,
    bridge_dir: PathBuf,
    schema_path: PathBuf,
    secrets: Vec<String>,
}

impl CodexBridge {
    pub fn new(bridge_dir: impl Into<PathBuf>, schema_path: impl Into<PathBuf>) -> Self {
        Self {
            program: "codex".into(),
            bridge_dir: bridge_dir.into(),
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
        bridge_dir: &Path,
        schema_path: &Path,
    ) -> BridgeCommand {
        Self::command_for_program("codex", request, cursor, bridge_dir, schema_path)
    }

    fn command_for_program(
        program: &str,
        request: &ModelRequest,
        cursor: Option<&BackendCursor>,
        bridge_dir: &Path,
        schema_path: &Path,
    ) -> BridgeCommand {
        let cursor = cursor.filter(|cursor| cursor.backend == BACKEND);
        let mut args = if cursor.is_some() {
            vec![
                "exec".into(),
                "resume".into(),
                "--json".into(),
                "--ignore-user-config".into(),
                "--ignore-rules".into(),
                "--skip-git-repo-check".into(),
                "--output-schema".into(),
                schema_path.display().to_string(),
                "-m".into(),
                request.profile.model.clone(),
            ]
        } else {
            vec![
                "exec".into(),
                "--json".into(),
                "--color".into(),
                "never".into(),
                "--sandbox".into(),
                "read-only".into(),
                "--ignore-user-config".into(),
                "--ignore-rules".into(),
                "--skip-git-repo-check".into(),
                "-c".into(),
                "web_search=\"disabled\"".into(),
                "-C".into(),
                bridge_dir.display().to_string(),
                "--output-schema".into(),
                schema_path.display().to_string(),
                "-m".into(),
                request.profile.model.clone(),
            ]
        };
        for feature in DISABLED_NATIVE_FEATURES {
            args.push("--disable".into());
            args.push(feature.into());
        }
        if let Some(cursor) = cursor {
            args.push(cursor.value.clone());
        }
        args.push("-".into());
        args.shrink_to_fit();
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

impl ModelBackend for CodexBridge {
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
            fs::create_dir_all(&self.bridge_dir)?;
            write_control_schema(&self.schema_path, request.delegation.is_some())?;
            let command = Self::command_for_program(
                &self.program,
                &request,
                request.continuation.as_ref(),
                &self.bridge_dir,
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
    let mut thread_id = None;
    let mut control = None;
    let mut completed = false;
    for line in lines {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| KuramaError::Protocol(format!("invalid Codex JSONL: {error}")))?;
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "thread.started" => {
                let id = value
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        KuramaError::Protocol("Codex thread.started omitted thread_id".into())
                    })?;
                thread_id = Some(id.to_owned());
                events.push(ModelEvent::ResponseStarted {
                    provider_id: id.to_owned(),
                });
            }
            "item.completed" => match value.pointer("/item/type").and_then(Value::as_str) {
                Some("agent_message") => {
                    control = value
                        .pointer("/item/text")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                Some(
                    item_type @ ("command_execution" | "file_change" | "mcp_tool_call"
                    | "web_search"),
                ) => {
                    return Err(KuramaError::Protocol(format!(
                        "native Codex tool event is forbidden in bridge mode: {item_type}"
                    )));
                }
                _ => {}
            },
            "turn.completed" => {
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
                                .get("cached_input_tokens")
                                .and_then(Value::as_u64)
                                .unwrap_or_default(),
                        },
                    });
                }
                let control = control.take().ok_or_else(|| {
                    KuramaError::Protocol("Codex completed without a control object".into())
                })?;
                let normalized = parse_control(&control, delegation_enabled)?;
                let tool_calls = normalized.iter().any(|event| {
                    matches!(
                        event,
                        ModelEvent::ToolCall { .. } | ModelEvent::Delegation { .. }
                    )
                });
                events.extend(normalized);
                events.push(ModelEvent::ResponseCompleted {
                    cursor: thread_id.clone().map(|value| BackendCursor {
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
            "turn.failed" | "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .unwrap_or("Codex CLI failed");
                return Err(KuramaError::Model(message.to_owned()));
            }
            _ => {}
        }
    }
    if !completed {
        return Err(KuramaError::Protocol(
            "Codex JSONL ended before turn.completed".into(),
        ));
    }
    Ok(events)
}
