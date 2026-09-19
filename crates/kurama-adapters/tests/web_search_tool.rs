#![cfg(all(feature = "tools", feature = "http"))]

use std::future::pending;

use kurama_adapters::{
    HttpClient, JsonSearchBackend, OpenAiNativeSearch, SearchBackend, SearchResult, SecretValue,
    WebSearchTool, html_to_text,
};
use kurama_protocol::{
    agent::WriteScope,
    id::{CallId, SessionId},
    policy::ExecutionMode,
    tool::{Operation, ToolContext, ToolInvocation, ToolLimits},
    traits::{BoxFuture, CancelSignal, Tool},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct NeverCancel;

impl CancelSignal for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'static, ()> {
        Box::pin(pending())
    }
}

#[test]
fn html_extraction_drops_active_content_and_decodes_entities() {
    let source = "<main><h1>Hello &amp; bye</h1><script>secret()</script><pre>a\n b</pre></main>";
    assert_eq!(html_to_text(source), "Hello & bye\na\n b");
}

#[test]
fn dropped_elements_require_a_complete_closing_tag_name() {
    for source in [
        "<script>before </scripture> hidden </ScRiPt \t><p>shown</p>",
        "<style>before </stylesheet> hidden </STYLE/><p>shown</p>",
        "<script>before </script-extra> hidden </script><p>shown</p>",
        "<SCRIPT>before </scripture> hidden",
    ] {
        let expected = if source.ends_with("hidden") {
            ""
        } else {
            "shown"
        };
        assert_eq!(html_to_text(source), expected, "{source}");
    }
}

#[tokio::test]
async fn json_search_normalizes_only_public_result_fields() {
    let endpoint = serve_once(
        "application/json",
        r#"{"results":[{"title":"Rust async guide","url":"https://example.com/rust","snippet":"A compact guide.","internal":"not public"}]}"#,
    )
    .await;
    let tool = WebSearchTool::with_backend(JsonSearchBackend::new(endpoint, None));
    let context = context(ExecutionMode::Supervised);
    let invocation = ToolInvocation {
        call_id: CallId::from("search-call"),
        name: "web-search".into(),
        arguments: serde_json::json!({
            "operation": "search",
            "query": "rust async",
            "limit": 3,
            "contains_workspace_data": false
        }),
    };

    let result = tool
        .execute(context, invocation, &NeverCancel)
        .await
        .expect("search");
    assert_eq!(
        result.metadata["results"],
        serde_json::json!([{
            "title": "Rust async guide",
            "url": "https://example.com/rust",
            "snippet": "A compact guide."
        }])
    );
    assert_eq!(
        result.output,
        "1. Rust async guide\nhttps://example.com/rust\nA compact guide."
    );
    assert!(!result.truncated);
}

#[tokio::test]
async fn yolo_open_keeps_limits_but_allows_private_http() {
    let endpoint = serve_once(
        "text/html",
        "<main><p>alpha</p><p>middle content</p><p>omega</p><script>secret()</script></main>",
    )
    .await;
    let invocation = ToolInvocation {
        call_id: CallId::from("open-call"),
        name: "web-search".into(),
        arguments: serde_json::json!({"operation": "open", "url": endpoint}),
    };

    let mut context = context(ExecutionMode::Yolo);
    context.limits = ToolLimits {
        max_bytes: 24,
        max_lines: 2,
    };
    let result = WebSearchTool::default()
        .execute(context, invocation, &NeverCancel)
        .await
        .expect("open");
    assert!(result.truncated);
    assert!(result.output.len() <= 24);
    assert!(!result.output.contains("secret"));
    let staged_path = std::path::PathBuf::from(
        result.metadata["_display_staging"]["output"]
            .as_str()
            .expect("staged web output"),
    );
    assert_eq!(
        std::fs::read_to_string(&staged_path).expect("complete staged web output"),
        "alpha\nmiddle content\nomega"
    );
    std::fs::remove_file(staged_path).expect("remove staged web output");
}

#[test]
fn web_search_ignores_unknown_fields_and_defaults_limit() {
    let context = context(ExecutionMode::Supervised);
    let extra = ToolInvocation {
        call_id: CallId::from("search-extra"),
        name: "web-search".into(),
        arguments: serde_json::json!({
            "operation": "search",
            "query": "ratatui",
            "items": [{"id": "track", "status": "pending"}]
        }),
    };
    let operation = WebSearchTool::default()
        .classify(&context, &extra)
        .expect("ignore extra fields and default limit");
    assert!(matches!(
        operation,
        Operation::WebSearch {
            query,
            contains_workspace_data: false
        } if query == "ratatui"
    ));
}

fn context(mode: ExecutionMode) -> ToolContext {
    let root = std::env::temp_dir();
    ToolContext {
        session_id: SessionId::from("session"),
        agent_id: None,
        cwd: root.clone(),
        workspace_root: root,
        mode,
        limits: ToolLimits::default(),
        write_scope: WriteScope::default(),
    }
}

async fn serve_once(content_type: &str, body: impl AsRef<[u8]>) -> String {
    let body = body.as_ref();
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ).into_bytes();
    response.extend_from_slice(body);
    serve_raw_once(response).await
}

async fn serve_raw_once(response: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await.expect("read request");
        stream.write_all(&response).await.expect("write response");
    });
    format!("http://{address}/")
}

fn native_backend(endpoint: String) -> OpenAiNativeSearch {
    OpenAiNativeSearch::new(
        HttpClient::new(),
        endpoint,
        SecretValue::new("fixture-secret".into()),
        "fixture-model",
    )
}

#[tokio::test]
async fn openai_search_requires_successful_completed_native_evidence() {
    let results = serde_json::json!({"results": [{
        "title": "Fresh result", "url": "https://example.com/", "snippet": "Found on the web."
    }]})
    .to_string();
    let message = serde_json::json!({
        "type": "message", "content": [{"type": "output_text", "text": results}]
    });
    let completed = serde_json::json!({"type": "web_search_call", "status": "completed"});
    for (status, calls, expected_success) in [
        ("completed", vec![], false),
        ("completed", vec![completed.clone()], true),
        (
            "completed",
            vec![serde_json::json!({"type": "web_search_call", "status": "in_progress"})],
            false,
        ),
        ("incomplete", vec![completed.clone()], false),
        (
            "completed",
            vec![
                completed,
                serde_json::json!({
                    "type": "web_search_call", "status": "failed",
                    "error": {"message": "provider failure fixture-secret"}
                }),
            ],
            false,
        ),
    ] {
        let mut output = calls;
        output.push(message.clone());
        let endpoint = serve_once(
            "application/json",
            serde_json::json!({
                "status": status, "output": output
            })
            .to_string(),
        )
        .await;
        let result = native_backend(endpoint)
            .search("query", 1, &NeverCancel)
            .await;
        if expected_success {
            assert_eq!(
                result.expect("completed native search"),
                vec![SearchResult {
                    title: "Fresh result".into(),
                    url: "https://example.com/".into(),
                    snippet: "Found on the web.".into()
                }]
            );
        } else {
            let error = result.expect_err("no successful native search");
            assert!(matches!(error, kurama_protocol::KuramaError::Tool(_)));
            assert!(!error.to_string().contains("fixture-secret"));
            if output.iter().any(|item| item["status"] == "failed") {
                assert!(error.to_string().contains("provider failure"));
            }
        }
    }
}

#[tokio::test]
async fn native_search_redacts_nested_escaped_and_short_credentials() {
    for secret in [r#"sk-"quote\backslash"#, r#"x"\y"#, "\"", "\\", "q7"] {
        let endpoint = serve_once(
            "application/json",
            serde_json::json!({
                "status": "failed",
                "error": {
                    "code": "search_unavailable",
                    "message": "cannot reach upstream",
                    "details": [{
                        (format!("credential={secret}")): [format!("rejected {secret}")]
                    }]
                }
            })
            .to_string(),
        )
        .await;
        let error = OpenAiNativeSearch::new(
            HttpClient::new(),
            endpoint,
            SecretValue::new(secret.into()),
            "fixture-model",
        )
        .search("query", 1, &NeverCancel)
        .await
        .expect_err("native search failed");
        let kurama_protocol::KuramaError::Tool(message) = error else {
            panic!("expected a search failure");
        };
        let diagnostic: serde_json::Value = serde_json::from_str(
            &message[message.find('{').expect("structured provider diagnostic")..],
        )
        .expect("redaction preserves JSON escaping");
        assert_eq!(diagnostic["code"], "search_unavailable");
        assert_eq!(diagnostic["message"], "cannot reach upstream");
        let details = diagnostic["details"][0].as_object().unwrap();
        for (key, values) in details {
            assert!(
                !key.contains(secret),
                "credential leaked in a diagnostic key"
            );
            for value in values.as_array().unwrap() {
                assert!(
                    !value.as_str().unwrap().contains(secret),
                    "credential leaked in a nested value"
                );
            }
        }
    }
}

#[tokio::test]
async fn native_search_redacts_escaped_credentials_crossing_the_diagnostic_limit() {
    let secret = r#"credential-"private\tail"#;
    let encoded = serde_json::to_string(secret).unwrap();
    let encoded = &encoded[1..encoded.len() - 1];
    let header_len = serde_json::json!({
        "code": "search_unavailable", "message": ""
    })
    .to_string()
    .len()
        - 2;
    // Cut before an escape, inside its two-byte representation, and near the end.
    for visible_bytes in [5, encoded.find('\\').unwrap() + 1, encoded.len() - 1] {
        let padding = "x".repeat(16 * 1024 - header_len - visible_bytes);
        let endpoint = serve_once(
            "application/json",
            serde_json::json!({
                "status": "completed",
                "output": [{
                    "type": "web_search_call", "status": "incomplete",
                    "incomplete_details": {
                        "code": "search_unavailable",
                        "message": format!("{padding}{secret} more detail")
                    }
                }]
            })
            .to_string(),
        )
        .await;
        let error = OpenAiNativeSearch::new(
            HttpClient::new(),
            endpoint,
            SecretValue::new(secret.into()),
            "fixture-model",
        )
        .search("query", 1, &NeverCancel)
        .await
        .expect_err("incomplete native search")
        .to_string();
        assert!(error.contains("search_unavailable"));
        assert!(
            !error.contains(&encoded[..visible_bytes]),
            "truncated credential prefix leaked"
        );
        assert!(
            error.len() <= 16 * 1024 + 128,
            "diagnostic must remain bounded"
        );
    }
}

#[tokio::test]
async fn public_http_backends_reject_invalid_limits_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let endpoint = format!("http://{}/", listener.local_addr().expect("address"));
    let backends: Vec<Box<dyn SearchBackend>> = vec![
        Box::new(JsonSearchBackend::new(&endpoint, None)),
        Box::new(native_backend(endpoint)),
    ];
    for backend in backends {
        for limit in [0, 9] {
            let result = tokio::select! {
                result = backend.search("query", limit, &NeverCancel) => result,
                _ = listener.accept() => panic!("invalid limit reached the network"),
            };
            assert!(matches!(result, Err(kurama_protocol::KuramaError::Tool(_))));
        }
    }
}

struct LargeSearch;

impl SearchBackend for LargeSearch {
    fn search<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        _cancel: &'a dyn CancelSignal,
    ) -> BoxFuture<'a, Result<Vec<SearchResult>, kurama_protocol::KuramaError>> {
        Box::pin(async {
            Ok(vec![SearchResult {
                title: "Title".into(),
                url: "https://example.com/".into(),
                snippet: "α long snippet\n".repeat(8192),
            }])
        })
    }
}

#[tokio::test]
async fn search_limits_bound_visible_text_and_metadata_but_stage_full_results() {
    let tool = WebSearchTool::with_backend(LargeSearch);
    let mut context = context(ExecutionMode::Supervised);
    context.limits = ToolLimits {
        max_bytes: 32,
        max_lines: 2,
    };
    let result = tool
        .execute(
            context,
            ToolInvocation {
                call_id: CallId::from("bounded-search"),
                name: "web-search".into(),
                arguments: serde_json::json!({"operation": "search", "query": "query", "limit": 1}),
            },
            &NeverCancel,
        )
        .await
        .expect("bounded search");
    assert!(result.truncated);
    assert!(result.output.len() <= 32);
    assert_eq!(result.metadata["result_count"], 1);
    assert!(result.metadata.get("results").is_none());
    assert!(result.metadata.to_string().len() < 2048);
    assert!(
        result.metadata["omitted_bytes"]
            .as_u64()
            .expect("omitted bytes")
            > 0
    );
    assert!(
        result.metadata["omitted_lines"]
            .as_u64()
            .expect("omitted lines")
            > 0
    );
    let staged_path = result.metadata["_display_staging"]["output"]
        .as_str()
        .expect("staged output");
    let full = std::fs::read_to_string(staged_path).expect("full output");
    assert_eq!(
        full,
        format!(
            "1. Title\nhttps://example.com/\n{}",
            "α long snippet\n".repeat(8192)
        )
    );
    assert_eq!(result.metadata["readable_bytes"], full.len());
    std::fs::remove_file(staged_path).expect("remove staging");
}

#[tokio::test]
async fn private_open_is_denied_in_auto_as_well_as_supervised() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    for mode in [ExecutionMode::Auto, ExecutionMode::Supervised] {
        let tool = WebSearchTool::default();
        let result = tokio::select! {
            result = tool.execute(context(mode), ToolInvocation {
                call_id: CallId::from("private-open"), name: "web-search".into(),
                arguments: serde_json::json!({"operation":"open", "url":url}),
            }, &NeverCancel) => result,
            _ = listener.accept() => panic!("private target reached the network"),
        };
        assert!(matches!(
            result,
            Err(kurama_protocol::KuramaError::Policy(_))
        ));
    }
}

#[tokio::test]
async fn redirects_cannot_introduce_embedded_credentials() {
    let target = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let location = format!(
        "http://user:password@{}/",
        target.local_addr().expect("address")
    );
    let url = serve_raw_once(format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    ).into_bytes()).await;
    let tool = WebSearchTool::default();
    let result = tokio::select! {
        result = tool.execute(context(ExecutionMode::Yolo), ToolInvocation {
            call_id: CallId::from("redirect-open"), name: "web-search".into(),
            arguments: serde_json::json!({"operation":"open", "url":url}),
        }, &NeverCancel) => result,
        _ = target.accept() => panic!("credential-bearing redirect was followed"),
    };
    assert!(matches!(result, Err(kurama_protocol::KuramaError::Tool(_))));
}

#[tokio::test]
async fn open_preserves_valid_utf8_and_replaces_invalid_bytes() {
    for (bytes, expected) in [("α\nβ".as_bytes(), "α\nβ"), (&b"a\xffb"[..], "a�b")] {
        let url = serve_once("text/plain", bytes).await;
        let result = WebSearchTool::default()
            .execute(
                context(ExecutionMode::Yolo),
                ToolInvocation {
                    call_id: CallId::from("utf8-open"),
                    name: "web-search".into(),
                    arguments: serde_json::json!({"operation":"open", "url":url}),
                },
                &NeverCancel,
            )
            .await
            .expect("open text");
        assert_eq!(result.output, expected);
        assert_eq!(result.metadata["bytes"], bytes.len());
        assert_eq!(result.metadata["readable_bytes"], expected.len());
    }
}
