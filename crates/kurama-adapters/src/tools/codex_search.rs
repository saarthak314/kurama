use std::fs;

use kurama_protocol::{
    KuramaError,
    model::{FinishReason, ModelEvent},
    traits::{BoxFuture, CancelSignal},
};
use serde_json::Value;

use crate::{SearchBackend, SearchResult, tools::web_search::search_result_schema};

use super::native_search::{run_search, search_prompt};
use crate::bridges::{BridgeCommand, BridgeDecoder, codex::DISABLED_NATIVE_FEATURES};

pub struct CodexNativeSearch {
    program: String,
    model: String,
}

impl CodexNativeSearch {
    pub fn new(program: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            model: model.into(),
        }
    }
}

impl SearchBackend for CodexNativeSearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, KuramaError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(KuramaError::Cancelled);
            }
            let directory = tempfile::Builder::new()
                .prefix("kurama-codex-search-")
                .tempdir()?;
            let schema_path = directory.path().join("search-schema.json");
            let schema = serde_json::to_vec(&search_result_schema(limit)).map_err(|error| {
                KuramaError::Configuration(format!("Codex search schema: {error}"))
            })?;
            fs::write(&schema_path, schema)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&schema_path, fs::Permissions::from_mode(0o600))?;
            }
            let mut args = vec![
                "exec".into(),
                "--json".into(),
                "--color".into(),
                "never".into(),
                "--sandbox".into(),
                "read-only".into(),
                "--ignore-user-config".into(),
                "--ignore-rules".into(),
                "--skip-git-repo-check".into(),
                "--ephemeral".into(),
                "-c".into(),
                "web_search=\"live\"".into(),
                "-C".into(),
                directory.path().display().to_string(),
                "--output-schema".into(),
                schema_path.display().to_string(),
                "-m".into(),
                self.model.clone(),
            ];
            for feature in DISABLED_NATIVE_FEATURES {
                args.push("--disable".into());
                args.push(feature.into());
            }
            args.push("-".into());
            let command = BridgeCommand {
                program: self.program.clone(),
                args,
                stdin: search_prompt(query, limit),
                cwd: Some(directory.path().to_path_buf()),
            };
            let result = run_search(command, CodexSearchDecoder::default(), limit, cancel).await;
            drop(directory);
            result
        })
    }
}

#[derive(Default)]
struct CodexSearchDecoder {
    searched: bool,
    output: Option<String>,
    completed: bool,
}

impl BridgeDecoder for CodexSearchDecoder {
    fn push_line(&mut self, line: &str) -> Result<Vec<ModelEvent>, KuramaError> {
        let value: Value = serde_json::from_str(line).map_err(|error| {
            KuramaError::Protocol(format!("invalid Codex search JSONL: {error}"))
        })?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| KuramaError::Protocol("Codex search event omitted its type".into()))?;
        match event_type {
            "thread.started" | "turn.started" => {}
            "item.started" | "item.updated" | "item.completed" => {
                let item = value.get("item").ok_or_else(|| {
                    KuramaError::Protocol("Codex search event omitted its item".into())
                })?;
                let item_type = item.get("type").and_then(Value::as_str).ok_or_else(|| {
                    KuramaError::Protocol("Codex search item omitted its type".into())
                })?;
                match item_type {
                    "web_search" => {
                        if item.get("error").is_some_and(|error| !error.is_null())
                            || matches!(
                                item.get("status").and_then(Value::as_str),
                                Some("failed" | "declined" | "cancelled")
                            )
                        {
                            return Err(provider_error(item, "Codex native web search failed"));
                        }
                        if event_type == "item.completed" {
                            // Codex's web_search item has no status field; completion is
                            // represented by item.completed, not by item.started.
                            if item
                                .get("status")
                                .is_some_and(|status| status.as_str() != Some("completed"))
                            {
                                return Err(KuramaError::Model(
                                    "Codex native web search did not complete successfully".into(),
                                ));
                            }
                            self.searched = true;
                        }
                    }
                    "agent_message" if event_type == "item.completed" => {
                        let text = item.get("text").and_then(Value::as_str).ok_or_else(|| {
                            KuramaError::Protocol("Codex search agent_message omitted text".into())
                        })?;
                        if self.searched {
                            self.output = Some(text.to_owned());
                        }
                    }
                    "agent_message" | "reasoning" => {}
                    "error" => {
                        return Err(provider_error(item, "Codex native web search failed"));
                    }
                    other => {
                        return Err(KuramaError::Protocol(format!(
                            "native Codex tool is forbidden during web search: {other}"
                        )));
                    }
                }
            }
            "turn.completed" => {
                if !self.searched {
                    return Err(KuramaError::Model(
                        "Codex completed without performing native web search".into(),
                    ));
                }
                let text = self.output.take().ok_or_else(|| {
                    KuramaError::Protocol(
                        "Codex completed without search output after native web search".into(),
                    )
                })?;
                self.completed = true;
                return Ok(vec![
                    ModelEvent::TextDelta { text },
                    ModelEvent::ResponseCompleted {
                        cursor: None,
                        finish_reason: FinishReason::Stop,
                    },
                ]);
            }
            "turn.failed" | "error" => {
                return Err(provider_error(&value, "Codex native web search failed"));
            }
            other => {
                return Err(KuramaError::Protocol(format!(
                    "unknown Codex search event: {other}"
                )));
            }
        }
        Ok(Vec::new())
    }

    fn finish(&self) -> Result<(), KuramaError> {
        if self.completed {
            Ok(())
        } else {
            Err(KuramaError::Protocol(
                "Codex search JSONL ended before successful turn.completed".into(),
            ))
        }
    }
}

fn provider_error(value: &Value, fallback: &str) -> KuramaError {
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or(fallback);
    KuramaError::Model(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn final_message() -> String {
        json!({
            "type": "item.completed",
            "item": {
                "type": "agent_message",
                "text": json!({"results": [{
                    "title": "Rust releases",
                    "url": "https://www.rust-lang.org/",
                    "snippet": "Official Rust release information."
                }]}).to_string()
            }
        })
        .to_string()
    }

    #[test]
    fn requires_completed_native_search_instead_of_model_memory() {
        let mut decoder = CodexSearchDecoder::default();
        decoder
            .push_line(r#"{"type":"item.started","item":{"type":"web_search","query":"Rust"}}"#)
            .unwrap();
        decoder.push_line(&final_message()).unwrap();

        assert!(matches!(
            decoder.push_line(r#"{"type":"turn.completed"}"#),
            Err(KuramaError::Model(_))
        ));
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn rejects_pre_search_placeholder_as_final_output() {
        let mut decoder = CodexSearchDecoder::default();
        decoder
            .push_line(r#"{"type":"item.completed","item":{"type":"agent_message","text":"{\"results\":[]}"}}"#)
            .unwrap();
        decoder
            .push_line(r#"{"type":"item.completed","item":{"type":"web_search","query":"Rust"}}"#)
            .unwrap();

        assert!(matches!(
            decoder.push_line(r#"{"type":"turn.completed"}"#),
            Err(KuramaError::Protocol(_))
        ));
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn extracts_final_results_only_after_successful_turn() {
        let mut decoder = CodexSearchDecoder::default();
        decoder
            .push_line(r#"{"type":"item.completed","item":{"type":"agent_message","text":"Searching now."}}"#)
            .unwrap();
        decoder
            .push_line(r#"{"type":"item.completed","item":{"type":"web_search","query":"Rust"}}"#)
            .unwrap();
        assert!(decoder.push_line(&final_message()).unwrap().is_empty());
        assert!(decoder.finish().is_err());

        let events = decoder.push_line(r#"{"type":"turn.completed"}"#).unwrap();
        let [
            ModelEvent::TextDelta { text },
            ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: FinishReason::Stop,
            },
        ] = events.as_slice()
        else {
            panic!("search did not complete with final structured output: {events:?}");
        };
        let payload: Value = serde_json::from_str(text).unwrap();
        let results: Vec<SearchResult> =
            serde_json::from_value(payload["results"].clone()).unwrap();
        assert_eq!(
            results,
            vec![SearchResult {
                title: "Rust releases".into(),
                url: "https://www.rust-lang.org/".into(),
                snippet: "Official Rust release information.".into(),
            }]
        );
        decoder.finish().unwrap();
    }

    #[test]
    fn rejects_unrelated_native_tools_before_they_complete() {
        for tool in [
            "command_execution",
            "file_change",
            "mcp_tool_call",
            "collab_tool_call",
        ] {
            let mut decoder = CodexSearchDecoder::default();
            let event = json!({"type": "item.started", "item": {"type": tool}});
            assert!(matches!(
                decoder.push_line(&event.to_string()),
                Err(KuramaError::Protocol(_))
            ));
            assert!(decoder.finish().is_err());
        }
    }

    #[test]
    fn failed_search_cannot_be_replaced_by_model_output() {
        let mut decoder = CodexSearchDecoder::default();
        let error = decoder
            .push_line(r#"{"type":"item.completed","item":{"type":"web_search","status":"failed","error":{"message":"Search quota exhausted"}}}"#)
            .unwrap_err();
        assert!(
            matches!(error, KuramaError::Model(message) if message == "Search quota exhausted")
        );
        assert!(decoder.finish().is_err());
    }

    #[test]
    fn propagates_provider_failure_after_search_and_output() {
        for failure in [
            r#"{"type":"turn.failed","error":{"message":"Provider disconnected"}}"#,
            r#"{"type":"error","message":"Provider disconnected"}"#,
            r#"{"type":"item.completed","item":{"type":"error","message":"Provider disconnected"}}"#,
        ] {
            let mut decoder = CodexSearchDecoder::default();
            decoder
                .push_line(
                    r#"{"type":"item.completed","item":{"type":"web_search","query":"Rust"}}"#,
                )
                .unwrap();
            decoder.push_line(&final_message()).unwrap();
            let error = decoder.push_line(failure).unwrap_err();
            assert!(
                matches!(error, KuramaError::Model(message) if message == "Provider disconnected")
            );
            assert!(decoder.finish().is_err());
        }
    }
}
