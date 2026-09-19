#![cfg(feature = "anthropic")]

use futures_util::{StreamExt, TryStreamExt};
use kurama_adapters::{AnthropicBackend, HttpClient};
use kurama_protocol::{
    KuramaError,
    id::SessionId,
    model::{DelegationSchema, ModelEvent, ModelItem, ModelProfile, ModelRequest},
    tool::ToolDescriptor,
    traits::ModelBackend,
};

#[path = "support/provider_http.rs"]
mod provider_http;

use provider_http::{
    DelayedSseResponse, NeverCancel, serve_sse_once, serve_sse_until_released,
    split_first_sse_event,
};

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

#[tokio::test]
async fn maps_messages_stream_to_normalized_events() {
    let events = events_from_wire(include_str!(
        "../../../tests/fixtures/anthropic/tool_turn.jsonl"
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

#[tokio::test]
async fn anthropic_backend_rejects_transport_eof_before_message_stop() {
    let (endpoint, captured) = serve_sse_once(include_str!(
        "../../../tests/fixtures/anthropic/truncated_text.jsonl"
    ))
    .await;
    let backend =
        AnthropicBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
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
async fn anthropic_backend_yields_before_response_eof() {
    let fixture = include_str!("../../../tests/fixtures/anthropic/tool_turn.jsonl");
    let (first, tail) = split_first_sse_event(fixture);
    let server = serve_sse_until_released(first, tail).await;
    let backend = AnthropicBackend::from_endpoint(
        HttpClient::default(),
        &server.endpoint,
        "test-key".to_owned(),
    )
    .expect("backend");

    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        backend.stream(request(), &NeverCancel),
    )
    .await
    .expect("Anthropic stream waited for response EOF")
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
async fn anthropic_backend_bounds_candidate_non_text_events() {
    let mut body = format!(
        "event: message_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\n",
        serde_json::json!({"type":"message_start","message":{"id":"msg_buffer","usage":{"input_tokens":1}}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<kurama_delegate>{"}}),
    );
    for output_tokens in 0..65_u64 {
        body.push_str(&format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":null},"usage":{"output_tokens":output_tokens}})
        ));
    }
    let DelayedSseResponse {
        endpoint,
        captured,
        first_sent: _,
        release,
        disconnected,
    } = serve_sse_until_released(body, "x").await;
    let backend =
        AnthropicBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
            .expect("backend");
    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");

    let error = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            match stream.next().await.expect("bounded stream item") {
                Ok(_) => {}
                Err(error) => break error,
            }
        }
    })
    .await
    .expect("candidate event bound waited for EOF");
    assert!(matches!(error, KuramaError::Protocol(_)));
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(1), disconnected)
        .await
        .expect("bounded candidate response stayed open")
        .expect("disconnect signal");
    let _ = captured.await.expect("captured request");
    drop(release);
}

#[tokio::test]
async fn anthropic_normalization_bounds_cumulative_tool_argument_bytes() {
    let mut body = format!(
        "event: message_start\ndata: {}\n\nevent: content_block_start\ndata: {}\n\nevent: content_block_start\ndata: {}\n\n",
        serde_json::json!({"type":"message_start","message":{"id":"msg_bounded","usage":{"input_tokens":1}}}),
        serde_json::json!({
            "type":"content_block_start",
            "index":0,
            "content_block":{
                "type":"tool_use",
                "id":"tool_bounded",
                "name":"write",
                "input":{}
            }
        }),
        serde_json::json!({
            "type":"content_block_start",
            "index":1,
            "content_block":{
                "type":"tool_use",
                "id":"tool_bounded_b",
                "name":"write",
                "input":{}
            }
        }),
    );
    let chunk = "x".repeat(4 * 1024);
    for _ in 0..129 {
        for index in [0, 1] {
            body.push_str(&format!(
                "event: content_block_delta\ndata: {}\n\n",
                serde_json::json!({
                    "type":"content_block_delta",
                    "index":index,
                    "delta":{"type":"input_json_delta","partial_json":chunk}
                })
            ));
        }
    }

    let (endpoint, captured) = serve_sse_once(body).await;
    let backend =
        AnthropicBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
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
async fn anthropic_normalization_bounds_tool_call_count() {
    let mut body = format!(
        "event: message_start\ndata: {}\n\n",
        serde_json::json!({"type":"message_start","message":{"id":"msg_bounded","usage":{"input_tokens":1}}})
    );
    for index in 0..65_u64 {
        body.push_str(&format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::json!({
                "type":"content_block_start",
                "index":index,
                "content_block":{
                    "type":"tool_use",
                    "id":format!("tool_{index}"),
                    "name":"read",
                    "input":{}
                }
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
    let backend =
        AnthropicBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())?;
    backend
        .stream(request(), &NeverCancel)
        .await?
        .try_collect()
        .await
}

#[tokio::test]
async fn anthropic_backend_cancels_after_first_event() {
    let (first, tail) = split_first_sse_event(include_str!(
        "../../../tests/fixtures/anthropic/tool_turn.jsonl"
    ));
    let server = serve_sse_until_released(first, tail).await;
    let backend = AnthropicBackend::from_endpoint(
        HttpClient::default(),
        &server.endpoint,
        "test-key".to_owned(),
    )
    .expect("backend");
    provider_http::assert_stream_cancels(&backend, request(), server).await;
}

#[tokio::test]
async fn anthropic_backend_rejects_unfinished_or_malformed_tool_calls() {
    let added = serde_json::json!({"type":"content_block_start","index":0,"content_block":{
        "type":"tool_use","id":"call-1","name":"read","input":{}
    }});
    for fault in [
        "unfinished",
        "missing-id",
        "empty-id",
        "missing-name",
        "empty-name",
        "arguments",
    ] {
        let mut start = added.clone();
        match fault {
            "missing-id" => {
                start["content_block"].as_object_mut().unwrap().remove("id");
            }
            "empty-id" => start["content_block"]["id"] = "".into(),
            "missing-name" => {
                start["content_block"]
                    .as_object_mut()
                    .unwrap()
                    .remove("name");
            }
            "empty-name" => start["content_block"]["name"] = "".into(),
            _ => {}
        }
        let mut body = format!("data: {start}\n\n");
        if fault == "arguments" {
            body.push_str("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\"}}\n\n");
        }
        if fault != "unfinished" {
            body.push_str("data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
        }
        body.push_str("data: {\"type\":\"message_stop\"}\n\n");
        assert!(
            matches!(events_from_wire(body).await, Err(KuramaError::Protocol(_))),
            "{fault}"
        );
    }
}

#[tokio::test]
async fn anthropic_backend_accepts_bom_and_cr_only_sse() {
    let wire = format!(
        "\u{feff}{}",
        include_str!("../../../tests/fixtures/anthropic/tool_turn.jsonl").replace('\n', "\r")
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
async fn anthropic_backend_rejects_empty_http_body() {
    assert!(matches!(
        events_from_wire("").await,
        Err(KuramaError::Model(_))
    ));
}
