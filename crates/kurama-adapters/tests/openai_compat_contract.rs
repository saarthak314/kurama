#![cfg(feature = "openai-compatible")]
#![allow(dead_code)]

#[path = "../src/http.rs"]
mod http;
#[path = "../src/providers/mod.rs"]
mod providers;

use http::HttpClient;
use kurama_protocol::{
    KuramaError,
    id::SessionId,
    model::{DelegationSchema, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    tool::ToolDescriptor,
    traits::ModelBackend,
};
use providers::openai_compat::OpenAiCompatBackend;

#[path = "support/provider_http.rs"]
mod provider_http;

use provider_http::{NeverCancel, serve_sse_once};

fn request() -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from("session-1"),
        agent_id: None,
        workspace_root: "/workspace/project".into(),
        profile: ModelProfile::new("local", "local-test", 32_000, 4_000),
        system: "Be exact.".into(),
        items: vec![ModelItem::User {
            text: "Read it".into(),
        }],
        tools: vec![ToolDescriptor {
            name: "read".into(),
            description: "Read files".into(),
            parameters: serde_json::json!({"type":"object","additionalProperties":false}),
        }],
        delegation: Some(DelegationSchema {
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"agents": {"type": "array"}},
                "required": ["agents"],
                "additionalProperties": false
            }),
        }),
        continuation: None,
    }
}

#[test]
fn chat_request_supports_optional_parallel_tool_calls() {
    let body = OpenAiCompatBackend::request_body(&request(), Some(false));

    assert_eq!(body["model"], "local-test");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["max_tokens"], 4_000);
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert_eq!(body["tools"].as_array().expect("tools").len(), 1);
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .expect("system")
            .contains("<kurama_delegate>")
    );
}

#[test]
fn maps_chat_completions_stream_to_normalized_events() {
    let events = OpenAiCompatBackend::parse_fixture(include_str!(
        "../../../tests/fixtures/openai_compat/tool_turn.jsonl"
    ))
    .expect("fixture");

    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::TextDelta { text } if text == "Checking"))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::ToolCall { call_id, name, arguments }
            if call_id.as_ref() == "call_1"
                && name == "read"
                && arguments["files"][0]["path"] == "Cargo.toml"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::Usage { usage } if usage.input_tokens == 60 && usage.output_tokens == 12
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ModelEvent::ResponseCompleted { .. }))
            .count(),
        1
    );
}

#[test]
fn openai_compat_normalization_rejects_transport_eof_before_done() {
    let error = OpenAiCompatBackend::parse_fixture(include_str!(
        "../../../tests/fixtures/openai_compat/truncated_text.jsonl"
    ))
    .expect_err("truncated stream must fail");

    assert!(matches!(
        error,
        KuramaError::Model(message)
            if message == "OpenAI-compatible stream ended before [DONE]"
    ));
}

#[test]
fn openai_compat_normalization_rejects_empty_transport_eof() {
    let error = OpenAiCompatBackend::parse_fixture("").expect_err("empty stream must fail");

    assert!(matches!(
        error,
        KuramaError::Model(message)
            if message == "OpenAI-compatible stream ended before [DONE]"
    ));
}

#[tokio::test]
async fn openai_compat_backend_rejects_transport_eof_before_done() {
    let (endpoint, captured) = serve_sse_once(include_str!(
        "../../../tests/fixtures/openai_compat/truncated_text.jsonl"
    ))
    .await;
    let backend = OpenAiCompatBackend::from_endpoint(HttpClient::default(), &endpoint, None)
        .expect("backend");

    let error = match backend.stream(request(), &NeverCancel).await {
        Ok(_) => panic!("truncated stream must fail"),
        Err(error) => error,
    };
    let _ = captured.await.expect("captured request");

    assert!(matches!(
        error,
        KuramaError::Model(message)
            if message == "OpenAI-compatible stream ended before [DONE]"
    ));
}
