#![cfg(feature = "openai")]

use futures_util::{StreamExt, TryStreamExt};
use kurama_adapters::{HttpClient, HttpErrorClass, OpenAiBackend, SseDecoder};
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
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
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

#[tokio::test]
async fn openai_backend_yields_before_response_eof() {
    let fixture = include_str!("../../../tests/fixtures/openai/tool_turn.jsonl");
    let (first, tail) = split_first_sse_event(fixture);
    let server = serve_sse_until_released(first, tail).await;
    let backend = OpenAiBackend::from_endpoint(
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
    .expect("OpenAI stream waited for response EOF")
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
async fn openai_backend_streams_delegation_without_exposing_control_text() {
    let control = r#"<kurama_delegate>{"agents":[{"objective":"Review the patch","context_refs":[],"write_scope":{"roots":[],"files":[]},"budget":{"max_input_tokens":8000,"max_output_tokens":1000,"max_turns":2,"max_seconds":120},"depends_on":[]}]}</kurama_delegate>"#;
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        serde_json::json!({"type":"response.created","response":{"id":"resp_delegate"}}),
        serde_json::json!({"type":"response.output_text.delta","delta":control}),
        serde_json::json!({"type":"response.completed","response":{"id":"resp_delegate"}}),
    );
    let (endpoint, captured) = serve_sse_once(body).await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
            .expect("backend");

    let events = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream")
        .collect::<Vec<_>>()
        .await;
    let events = events
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("events");
    let _ = captured.await.expect("captured request");

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, ModelEvent::TextDelta { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::Delegation { request } if request.agents[0].objective == "Review the patch"
            && request.agents[0].role.is_empty() && request.agents[0].profile.is_none()
    )));
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted {
            finish_reason: kurama_protocol::model::FinishReason::ToolCalls,
            ..
        })
    ));
}

#[tokio::test]
async fn openai_backend_rejects_split_delegation_open_after_text() {
    let body = openai_text_stream(&["ordinary text", "<kurama_", "delegate>"]);
    let (endpoint, captured) = serve_sse_once(body).await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
            .expect("backend");

    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");
    let mut emitted_text = String::new();
    let mut error = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(ModelEvent::TextDelta { text }) => emitted_text.push_str(&text),
            Ok(_) => {}
            Err(stream_error) => {
                error = Some(stream_error);
                break;
            }
        }
    }
    let _ = captured.await.expect("captured request");

    assert_eq!(emitted_text, "ordinary text");
    assert!(matches!(
        error.expect("delegation marker error"),
        KuramaError::Protocol(_)
    ));
}

#[tokio::test]
async fn openai_backend_rejects_split_delegation_close_after_text() {
    let body = openai_text_stream(&["ordinary text", "</kurama_", "delegate>"]);
    let (endpoint, captured) = serve_sse_once(body).await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
            .expect("backend");

    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");
    let mut emitted_text = String::new();
    let mut error = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(ModelEvent::TextDelta { text }) => emitted_text.push_str(&text),
            Ok(_) => {}
            Err(stream_error) => {
                error = Some(stream_error);
                break;
            }
        }
    }
    let _ = captured.await.expect("captured request");

    assert_eq!(emitted_text, "ordinary text");
    assert!(matches!(
        error.expect("delegation marker error"),
        KuramaError::Protocol(_)
    ));
}

#[tokio::test]
async fn openai_backend_bounds_candidate_non_text_bytes() {
    let arguments = serde_json::json!({"blob": "x".repeat(70 * 1024)}).to_string();
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n",
        serde_json::json!({"type":"response.created","response":{"id":"resp_buffer"}}),
        serde_json::json!({"type":"response.output_text.delta","delta":"<kurama_delegate>{"}),
        serde_json::json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_buffer","call_id":"call_buffer","name":"read","arguments":""}}),
        serde_json::json!({"type":"response.function_call_arguments.done","item_id":"fc_buffer","arguments":arguments}),
    );
    let DelayedSseResponse {
        endpoint,
        captured,
        first_sent: _,
        release,
        disconnected,
    } = serve_sse_until_released(body, "x").await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
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
    .expect("candidate byte bound waited for EOF");
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
async fn openai_backend_bounds_and_redacts_stream_errors() {
    let secret = "secret-token";
    let provider_message = format!("{secret} {}", "x".repeat(20_000));
    let body = format!(
        "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_error\"}}}}\n\ndata: {}\n\n",
        serde_json::json!({
            "type": "error",
            "error": {"message": provider_message}
        })
    );
    let (endpoint, captured) = serve_sse_once(body).await;
    let backend = OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, secret.to_owned())
        .expect("backend");

    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");
    assert!(matches!(
        stream
            .next()
            .await
            .expect("started event")
            .expect("started"),
        ModelEvent::ResponseStarted { .. }
    ));
    let error = stream
        .next()
        .await
        .expect("provider error event")
        .expect_err("provider error must fail");
    let _ = captured.await.expect("captured request");

    let KuramaError::Model(message) = error else {
        panic!("expected model error");
    };
    assert!(!message.contains(secret));
    assert!(message.len() <= 16 * 1024 + 128);
}

#[tokio::test]
async fn openai_backend_rejects_transport_eof_before_response_completed() {
    let (endpoint, captured) = serve_sse_once(include_str!(
        "../../../tests/fixtures/openai/truncated_text.jsonl"
    ))
    .await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
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
async fn dropping_openai_stream_after_first_event_closes_in_flight_response() {
    let fixture = include_str!("../../../tests/fixtures/openai/tool_turn.jsonl");
    let (first, tail) = split_first_sse_event(fixture);
    let DelayedSseResponse {
        endpoint,
        captured,
        first_sent: _,
        release,
        disconnected,
    } = serve_sse_until_released(first, tail).await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
            .expect("backend");
    let mut stream = backend
        .stream(request(), &NeverCancel)
        .await
        .expect("stream");
    assert!(matches!(
        stream.next().await.expect("first event").expect("event"),
        ModelEvent::ResponseStarted { .. }
    ));

    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(1), disconnected)
        .await
        .expect("response stayed open after stream drop")
        .expect("disconnect signal");
    let _ = captured.await.expect("captured request");
    drop(release);
}

#[tokio::test]
async fn openai_backend_rejects_oversized_sse_record_before_eof() {
    let secret = "secret-token";
    let oversized = format!("data: {secret}{}", "x".repeat(1024 * 1024));
    let DelayedSseResponse {
        endpoint,
        captured,
        first_sent: _,
        release,
        disconnected,
    } = serve_sse_until_released(oversized, "").await;
    let backend = OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, secret.to_owned())
        .expect("backend");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        backend.stream(request(), &NeverCancel),
    )
    .await
    .expect("oversized SSE record waited for EOF");
    let error = match result {
        Ok(_) => panic!("oversized SSE record must fail"),
        Err(error) => error,
    };
    assert!(matches!(&error, KuramaError::Protocol(_)));
    let message = error.to_string();
    assert!(!message.contains(secret));
    assert!(message.len() <= 16 * 1024 + 128);
    tokio::time::timeout(std::time::Duration::from_secs(1), disconnected)
        .await
        .expect("oversized response stayed open")
        .expect("disconnect signal");
    let _ = captured.await.expect("captured request");
    drop(release);
}

#[tokio::test]
async fn openai_normalization_bounds_cumulative_tool_argument_bytes() {
    let mut body = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        serde_json::json!({"type":"response.created","response":{"id":"resp_bounded"}}),
        serde_json::json!({
            "type":"response.output_item.added",
            "item":{
                "type":"function_call",
                "id":"fc_bounded_a",
                "call_id":"call_bounded_a",
                "name":"write",
                "arguments":"{\"content\":\""
            }
        }),
        serde_json::json!({
            "type":"response.output_item.added",
            "item":{
                "type":"function_call",
                "id":"fc_bounded_b",
                "call_id":"call_bounded_b",
                "name":"write",
                "arguments":"{\"content\":\""
            }
        }),
    );
    let chunk = "x".repeat(4 * 1024);
    for _ in 0..128 {
        for item_id in ["fc_bounded_a", "fc_bounded_b"] {
            body.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":item_id,
                    "delta":chunk
                })
            ));
        }
    }

    let (endpoint, captured) = serve_sse_once(body).await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
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
async fn openai_normalization_bounds_tool_call_count() {
    let mut body = format!(
        "data: {}\n\n",
        serde_json::json!({"type":"response.created","response":{"id":"resp_bounded"}})
    );
    for index in 0..65 {
        body.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({
                "type":"response.output_item.added",
                "item":{
                    "type":"function_call",
                    "id":format!("fc_{index}"),
                    "call_id":format!("call_{index}"),
                    "name":"read",
                    "arguments":"{}"
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
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())?;
    backend
        .stream(request(), &NeverCancel)
        .await?
        .try_collect()
        .await
}

fn openai_text_stream(deltas: &[&str]) -> String {
    let mut body = format!(
        "data: {}\n\n",
        serde_json::json!({"type":"response.created","response":{"id":"resp_text"}})
    );
    for delta in deltas {
        body.push_str(&format!(
            "data: {}\n\n",
            serde_json::json!({"type":"response.output_text.delta","delta":delta})
        ));
    }
    body.push_str(&format!(
        "data: {}\n\n",
        serde_json::json!({"type":"response.completed","response":{"id":"resp_text"}})
    ));
    body
}

#[tokio::test]
async fn openai_backend_cancels_after_first_event() {
    let (first, tail) = split_first_sse_event(include_str!(
        "../../../tests/fixtures/openai/tool_turn.jsonl"
    ));
    let server = serve_sse_until_released(first, tail).await;
    let backend = OpenAiBackend::from_endpoint(
        HttpClient::default(),
        &server.endpoint,
        "test-key".to_owned(),
    )
    .expect("backend");
    provider_http::assert_stream_cancels(&backend, request(), server).await;
}

#[tokio::test]
async fn openai_backend_rejects_unfinished_or_malformed_tool_calls() {
    let added = serde_json::json!({"type":"response.output_item.added","item":{
        "type":"function_call","id":"item-1","call_id":"call-1","name":"read","arguments":"{}"
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
                start["item"].as_object_mut().unwrap().remove("call_id");
            }
            "empty-id" => start["item"]["call_id"] = "".into(),
            "missing-name" => {
                start["item"].as_object_mut().unwrap().remove("name");
            }
            "empty-name" => start["item"]["name"] = "".into(),
            "arguments" => start["item"]["arguments"] = "{".into(),
            _ => {}
        }
        let mut body = format!("data: {start}\n\n");
        if fault != "unfinished" {
            body.push_str("data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"item-1\"}\n\n");
        }
        body.push_str(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"response-1\"}}\n\n",
        );
        assert!(
            matches!(events_from_wire(body).await, Err(KuramaError::Protocol(_))),
            "{fault}"
        );
    }
}

#[tokio::test]
async fn openai_backend_accepts_bom_and_cr_only_sse() {
    let wire = format!(
        "\u{feff}{}",
        openai_text_stream(&["hello"]).replace('\n', "\r")
    );
    let events = events_from_wire(wire).await.expect("CR-only SSE");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ModelEvent::TextDelta { text } if text == "hello"))
    );
    assert!(matches!(
        events.last(),
        Some(ModelEvent::ResponseCompleted { .. })
    ));
}

#[tokio::test]
async fn openai_backend_rejects_empty_http_body() {
    assert!(matches!(
        events_from_wire("").await,
        Err(KuramaError::Model(_))
    ));
}

#[test]
fn http_errors_redact_secret_crossing_size_boundary() {
    let secret = "unique-secret-value";
    let body = format!("{}{secret}", "x".repeat(16 * 1024 - 7));
    let failure = HttpClient::provider_error("openai", 503, &body, &[secret]);
    assert!(!failure.message().contains("unique-"));
    assert!(failure.message().len() <= 16 * 1024 + 128);
}

#[tokio::test]
async fn http_response_redacts_secret_crossing_size_boundary() {
    for secret in ["unique-secret-value", "unique-é-value"] {
        let body = format!("{}{secret}", "x".repeat(16 * 1024 - 7));
        let (endpoint, _) = provider_http::serve_response_once(format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len(),
        ))
        .await;
        let response = HttpClient::default()
            .get(endpoint)
            .send()
            .await
            .expect("response");
        let failure = HttpClient::response_error("openai", response, &[secret]).await;
        assert!(!failure.message().contains("unique-"));
        assert_eq!(failure.status(), Some(401));
        assert_eq!(failure.class(), HttpErrorClass::Authentication);
        assert!(failure.message().len() <= 16 * 1024 + 128);
    }
}

#[tokio::test]
async fn error_body_read_failure_preserves_status_and_class() {
    for (status, class) in [
        (503, HttpErrorClass::Transient),
        (401, HttpErrorClass::Authentication),
    ] {
        let (endpoint, _) = provider_http::serve_response_once(format!(
            "HTTP/1.1 {status} Error\r\ncontent-length: 100\r\nconnection: close\r\n\r\npartial unique-",
        )).await;
        let response = HttpClient::default()
            .get(endpoint)
            .send()
            .await
            .expect("response");
        let failure =
            HttpClient::response_error("openai", response, &["unique-secret-value"]).await;
        assert_eq!(failure.status(), Some(status));
        assert_eq!(failure.class(), class);
        assert!(!failure.message().contains("unique-"));
        assert!(failure.message().contains("body read failed"));
    }
}

#[tokio::test]
async fn http_client_refuses_redirect_without_forwarding_credentials() {
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("destination");
    let (endpoint, captured) = provider_http::serve_response_once(format!(
        "HTTP/1.1 302 Found\r\nlocation: http://{}/stolen\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        destination.local_addr().expect("address"),
    )).await;
    let response = HttpClient::with_timeout(std::time::Duration::from_secs(1))
        .expect("client")
        .get(endpoint)
        .bearer_auth("test-key")
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    assert!(
        captured
            .await
            .expect("request")
            .contains("authorization: Bearer test-key")
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), destination.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn openai_backend_surfaces_provider_error_before_first_event() {
    let (endpoint, _) =
        serve_sse_once(include_str!("../../../tests/fixtures/openai/error.jsonl")).await;
    let backend =
        OpenAiBackend::from_endpoint(HttpClient::default(), &endpoint, "test-key".to_owned())
            .expect("backend");
    assert!(matches!(
        backend.stream(request(), &NeverCancel).await,
        Err(KuramaError::Model(_))
    ));
}
