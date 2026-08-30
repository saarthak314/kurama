#![cfg(feature = "anthropic")]
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
use providers::anthropic::AnthropicBackend;

#[path = "support/provider_http.rs"]
mod provider_http;

use provider_http::{NeverCancel, serve_sse_once};

fn request() -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from("session-1"),
        agent_id: None,
        workspace_root: "/workspace/project".into(),
        profile: ModelProfile::new("main", "claude-test", 32_000, 4_000),
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
fn messages_request_uses_anthropic_tool_shape() {
    let body = AnthropicBackend::request_body(&request());

    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 4_000);
    assert_eq!(body["tools"][0]["name"], "read");
    assert!(body["tools"][0].get("input_schema").is_some());
    assert_eq!(body["tools"].as_array().expect("tools").len(), 1);
    assert!(
        body["system"]
            .as_str()
            .expect("system")
            .contains("<kurama_delegate>")
    );
}

#[test]
fn maps_messages_stream_to_normalized_events() {
    let events = AnthropicBackend::parse_fixture(include_str!(
        "../../../tests/fixtures/anthropic/tool_turn.jsonl"
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
            if call_id.as_ref() == "toolu_1"
                && name == "read"
                && arguments["files"][0]["path"] == "Cargo.toml"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::Usage { usage } if usage.input_tokens == 80 && usage.output_tokens == 14
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
fn anthropic_normalization_rejects_transport_eof_before_message_stop() {
    let error = AnthropicBackend::parse_fixture(include_str!(
        "../../../tests/fixtures/anthropic/truncated_text.jsonl"
    ))
    .expect_err("truncated stream must fail");

    assert!(matches!(
        error,
        KuramaError::Model(message) if message == "Anthropic stream ended before message_stop"
    ));
}

#[test]
fn anthropic_normalization_rejects_empty_transport_eof() {
    let error = AnthropicBackend::parse_fixture("").expect_err("empty stream must fail");

    assert!(matches!(
        error,
        KuramaError::Model(message) if message == "Anthropic stream ended before message_stop"
    ));
}

#[tokio::test]
async fn anthropic_backend_rejects_transport_eof_before_message_stop() {
    let (endpoint, captured) = serve_sse_once(include_str!(
        "../../../tests/fixtures/anthropic/truncated_text.jsonl"
    ))
    .await;
    let backend = AnthropicBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key")
        .expect("backend");

    let error = match backend.stream(request(), &NeverCancel).await {
        Ok(_) => panic!("truncated stream must fail"),
        Err(error) => error,
    };
    let _ = captured.await.expect("captured request");

    assert!(matches!(
        error,
        KuramaError::Model(message) if message == "Anthropic stream ended before message_stop"
    ));
}
