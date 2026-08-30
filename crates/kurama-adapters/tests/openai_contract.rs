#![cfg(feature = "openai")]
#![allow(dead_code)]

#[path = "../src/http.rs"]
mod http;
#[path = "../src/providers/mod.rs"]
mod providers;

use futures_util::StreamExt;
use http::{HttpClient, HttpErrorClass};
use kurama_protocol::{
    KuramaError,
    id::SessionId,
    model::{DelegationSchema, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    tool::ToolDescriptor,
    traits::ModelBackend,
};
use providers::{normalize_delegation_events, openai::OpenAiBackend, sse::SseDecoder};

#[path = "support/provider_http.rs"]
mod provider_http;

use provider_http::{NeverCancel, serve_sse_once};

fn request() -> ModelRequest {
    ModelRequest {
        session_id: SessionId::from("session-1"),
        agent_id: None,
        workspace_root: "/workspace/project".into(),
        profile: ModelProfile::new("main", "gpt-test", 32_000, 4_000),
        system: "Be exact.".into(),
        items: vec![ModelItem::User {
            text: "Read Cargo.toml".into(),
        }],
        tools: vec![ToolDescriptor {
            name: "read".into(),
            description: "Read files".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"files": {"type": "array"}},
                "required": ["files"],
                "additionalProperties": false
            }),
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
fn http_client_is_cloneable_and_disables_redirects() {
    let client = HttpClient::default();
    let cloned = client.clone();

    assert_eq!(client.user_agent(), "kurama/0.1.0");
    let _ = cloned.client();
    assert_eq!(HttpClient::classify_status(429), HttpErrorClass::Transient);
    assert_eq!(
        HttpClient::classify_status(401),
        HttpErrorClass::Authentication
    );
    assert_eq!(HttpClient::classify_status(422), HttpErrorClass::Permanent);
}

#[test]
fn responses_request_uses_strict_tools_and_no_storage() {
    let mut request = request();
    request.continuation = Some(kurama_protocol::model::BackendCursor {
        backend: "openai".into(),
        value: "resp_previous".into(),
    });
    let body = OpenAiBackend::request_body(&request);

    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["max_output_tokens"], 4_000);
    assert_eq!(body["previous_response_id"], "resp_previous");
    assert_eq!(body["tools"][0]["name"], "read");
    assert_eq!(body["tools"][0]["strict"], true);
    assert_eq!(body["tools"].as_array().expect("tools").len(), 1);
    assert!(
        body["instructions"]
            .as_str()
            .expect("instructions")
            .contains("<kurama_delegate>")
    );
}

#[test]
fn text_control_block_normalizes_to_delegation() {
    let events = normalize_delegation_events(
        vec![
            ModelEvent::TextDelta {
                text: r#"<kurama_delegate>{"agents":[{"objective":"Review the patch","context_refs":[],"write_scope":{"roots":[],"files":[]},"budget":{"max_input_tokens":8000,"max_output_tokens":1000,"max_turns":2,"max_seconds":120},"depends_on":[]}]}</kurama_delegate>"#.into(),
            },
            ModelEvent::ResponseCompleted {
                cursor: None,
                finish_reason: kurama_protocol::model::FinishReason::Stop,
            },
        ],
        true,
    )
    .expect("delegation block");

    assert!(matches!(
        events.as_slice(),
        [ModelEvent::Delegation { request }, ModelEvent::ResponseCompleted { .. }]
            if request.agents[0].role.is_empty() && request.agents[0].profile.is_none()
    ));
    assert!(
        normalize_delegation_events(
            vec![ModelEvent::TextDelta {
                text: "prose <kurama_delegate>{}</kurama_delegate>".into(),
            }],
            true,
        )
        .is_err()
    );
}

#[test]
fn sse_decoder_handles_byte_fragmentation_and_multiline_data() {
    let mut decoder = SseDecoder::default();
    let wire = b": keepalive\r\nevent: update\r\ndata: {\"a\":1,\r\ndata: \"b\":2}\r\n\r\n";
    let mut events = Vec::new();

    for byte in wire {
        events.extend(decoder.push(&[*byte]).expect("fragment"));
    }

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("update"));
    assert_eq!(events[0].data, "{\"a\":1,\n\"b\":2}");
}

#[test]
fn maps_responses_stream_to_normalized_events() {
    let events = OpenAiBackend::parse_fixture(include_str!(
        "../../../tests/fixtures/openai/tool_turn.jsonl"
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
        ModelEvent::Usage { usage } if usage.input_tokens == 120 && usage.output_tokens == 18
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
fn openai_normalization_rejects_transport_eof_before_response_completed() {
    let error = OpenAiBackend::parse_fixture(include_str!(
        "../../../tests/fixtures/openai/truncated_text.jsonl"
    ))
    .expect_err("truncated stream must fail");

    assert!(matches!(
        error,
        KuramaError::Model(message)
            if message == "OpenAI stream ended before response.completed"
    ));
}

#[test]
fn openai_normalization_rejects_empty_transport_eof() {
    let error = OpenAiBackend::parse_fixture("").expect_err("empty stream must fail");

    assert!(matches!(
        error,
        KuramaError::Model(message)
            if message == "OpenAI stream ended before response.completed"
    ));
}

#[test]
fn bounds_and_redacts_http_errors() {
    let error = HttpClient::provider_error(
        "openai",
        429,
        &format!("Bearer secret-token {}", "x".repeat(20_000)),
        &["secret-token"],
    );

    assert!(error.is_transient());
    assert!(!error.message().contains("secret-token"));
    assert!(error.message().len() <= 16 * 1024 + 128);
}

#[tokio::test]
async fn openai_backend_posts_responses_request_and_streams_fixture() {
    let (endpoint, captured) = serve_sse_once(include_str!(
        "../../../tests/fixtures/openai/tool_turn.jsonl"
    ))
    .await;
    let backend = OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key")
        .expect("backend");

    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("event"));
    }
    let request = captured.await.expect("captured request");

    assert!(request.starts_with("POST /v1/responses HTTP/1.1\r\n"));
    assert!(request.contains("authorization: Bearer test-key\r\n"));
    assert!(request.contains("\"store\":false"));
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted { .. })
    ));
}

#[tokio::test]
async fn openai_backend_bounds_and_redacts_stream_errors() {
    let secret = "secret-token";
    let provider_message = format!("{secret} {}", "x".repeat(20_000));
    let body = format!(
        "data: {}\n\n",
        serde_json::json!({
            "type": "error",
            "error": {"message": provider_message}
        })
    );
    let (endpoint, captured) = serve_sse_once(body).await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, secret).expect("backend");

    let error = match backend.stream(request(), &NeverCancel).await {
        Ok(_) => panic!("provider error must fail"),
        Err(error) => error,
    };
    let _ = captured.await.expect("captured request");

    let KuramaError::Model(message) = error else {
        panic!("expected model error");
    };
    assert!(message.starts_with("OpenAI stream: [REDACTED]"));
    assert!(!message.contains(secret));
    assert!(message.len() <= 16 * 1024 + 128);
}

#[tokio::test]
async fn openai_backend_rejects_transport_eof_before_response_completed() {
    let (endpoint, captured) = serve_sse_once(include_str!(
        "../../../tests/fixtures/openai/truncated_text.jsonl"
    ))
    .await;
    let backend = OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key")
        .expect("backend");

    let error = match backend.stream(request(), &NeverCancel).await {
        Ok(_) => panic!("truncated stream must fail"),
        Err(error) => error,
    };
    let _ = captured.await.expect("captured request");

    assert!(matches!(
        error,
        KuramaError::Model(message)
            if message == "OpenAI stream ended before response.completed"
    ));
}
