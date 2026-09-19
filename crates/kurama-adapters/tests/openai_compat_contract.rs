#![cfg(feature = "openai-compatible")]

use futures_util::{StreamExt, TryStreamExt};
use kurama_adapters::{HttpClient, OpenAiCompatBackend};
use kurama_protocol::{
    KuramaError,
    id::SessionId,
    model::{DelegationSchema, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    tool::ToolDescriptor,
    traits::ModelBackend,
};

#[path = "support/provider_http.rs"]
mod provider_http;

use provider_http::{NeverCancel, serve_sse_once, serve_sse_until_released, split_first_sse_event};

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

#[tokio::test]
async fn maps_chat_completions_stream_to_normalized_events() {
    let events = events_from_wire(include_str!(
        "../../../tests/fixtures/openai_compat/tool_turn.jsonl"
    ))
    .await
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

#[tokio::test]
async fn openai_compat_backend_rejects_transport_eof_before_done() {
    let (endpoint, captured) = serve_sse_once(include_str!(
        "../../../tests/fixtures/openai_compat/truncated_text.jsonl"
    ))
    .await;
    let backend = OpenAiCompatBackend::from_endpoint(HttpClient::default(), &endpoint, None)
        .expect("backend");

    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");
    let mut error = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(ModelEvent::ResponseCompleted { .. }) => panic!("truncated stream completed"),
            Ok(_) => {}
            Err(stream_error) => {
                error = Some(stream_error);
                break;
            }
        }
    }
    let _ = captured.await.expect("captured request");

    assert!(matches!(
        error.expect("truncated stream error"),
        KuramaError::Model(_)
    ));
}

#[tokio::test]
async fn openai_compat_backend_yields_before_response_eof() {
    let fixture = include_str!("../../../tests/fixtures/openai_compat/tool_turn.jsonl");
    let (first, tail) = split_first_sse_event(fixture);
    let server = serve_sse_until_released(first, tail).await;
    let backend = OpenAiCompatBackend::from_endpoint(HttpClient::default(), &server.endpoint, None)
        .expect("backend");

    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        backend.stream(request(), &NeverCancel),
    )
    .await
    .expect("OpenAI-compatible stream waited for response EOF")
    .expect("stream");
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect("first event timed out")
        .expect("first event")
        .expect("first event error");

    assert!(matches!(first, ModelEvent::ResponseStarted { .. }));
    server.release.send(()).expect("release response tail");
    while let Some(event) = stream.next().await {
        event.expect("remaining event");
    }
    let _ = server.captured.await.expect("captured request");
}

#[tokio::test]
async fn openai_compat_normalization_bounds_cumulative_tool_argument_bytes() {
    let mut body = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id":"chat_bounded",
            "choices":[{
                "delta":{"tool_calls":[{
                    "index":0,
                    "id":"call_bounded_a",
                    "function":{"name":"write","arguments":"{\"content\":\""}
                },{
                    "index":1,
                    "id":"call_bounded_b",
                    "function":{"name":"write","arguments":"{\"content\":\""}
                }]}
            }]
        })
    );
    let chunk = "x".repeat(4 * 1024);
    for _ in 0..128 {
        body.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "id":"chat_bounded",
                "choices":[{
                    "delta":{"tool_calls":[{
                        "index":0,
                        "function":{"arguments":chunk}
                    },{
                        "index":1,
                        "function":{"arguments":chunk}
                    }]}
                }]
            })
        ));
    }

    let (endpoint, captured) = serve_sse_once(body).await;
    let backend = OpenAiCompatBackend::from_endpoint(HttpClient::default(), &endpoint, None)
        .expect("backend");
    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");
    let error = loop {
        match stream.next().await.expect("bounded stream item") {
            Ok(_) => {}
            Err(error) => break error,
        }
    };
    let _ = captured.await.expect("captured request");

    assert!(matches!(error, KuramaError::Protocol(_)));
}

#[tokio::test]
async fn openai_compat_normalization_bounds_tool_call_count() {
    let mut body = String::new();
    for index in 0..65_u64 {
        body.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "id":"chat_bounded",
                "choices":[{
                    "delta":{"tool_calls":[{
                        "index":index,
                        "id":format!("call_{index}"),
                        "function":{"name":"read","arguments":"{}"}
                    }]}
                }]
            })
        ));
    }

    let error = events_from_wire(body)
        .await
        .expect_err("tool call count must be bounded");

    assert!(matches!(error, KuramaError::Protocol(_)));
}

async fn events_from_wire(body: impl Into<String>) -> Result<Vec<ModelEvent>, KuramaError> {
    let (endpoint, _) = serve_sse_once(body).await;
    let backend = OpenAiCompatBackend::from_endpoint(HttpClient::default(), &endpoint, None)?;
    backend
        .stream(request(), &NeverCancel)
        .await?
        .try_collect()
        .await
}

#[tokio::test]
async fn openai_compat_backend_cancels_after_first_event() {
    let (first, tail) = split_first_sse_event(include_str!(
        "../../../tests/fixtures/openai_compat/tool_turn.jsonl"
    ));
    let server = serve_sse_until_released(first, tail).await;
    let backend = OpenAiCompatBackend::from_endpoint(HttpClient::default(), &server.endpoint, None)
        .expect("backend");
    provider_http::assert_stream_cancels(&backend, request(), server).await;
}

#[tokio::test]
async fn openai_compat_backend_accepts_bom_and_cr_only_sse() {
    let wire = format!(
        "\u{feff}{}",
        include_str!("../../../tests/fixtures/openai_compat/tool_turn.jsonl").replace('\n', "\r")
    );
    let events = events_from_wire(wire).await.expect("CR-only SSE");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::ToolCall { name, .. } if name == "read"))
    );
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted { .. })
    ));
}

#[tokio::test]
async fn openai_compat_backend_rejects_empty_http_body() {
    assert!(matches!(
        events_from_wire("").await,
        Err(KuramaError::Model(_))
    ));
}
