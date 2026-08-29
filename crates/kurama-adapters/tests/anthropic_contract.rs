#![cfg(feature = "anthropic")]
#![allow(dead_code)]

#[path = "../src/http.rs"]
mod http;
#[path = "../src/providers/mod.rs"]
mod providers;

use kurama_protocol::{
    id::SessionId,
    model::{DelegationSchema, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    tool::ToolDescriptor,
};
use providers::anthropic::AnthropicBackend;

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
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted { .. })
    ));
}
