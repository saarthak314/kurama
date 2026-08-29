#![cfg(all(feature = "tools", feature = "http"))]

use std::future::pending;

use kurama_adapters::{JsonSearchBackend, WebSearchTool, html_to_text};
use kurama_protocol::{
    agent::WriteScope,
    id::{CallId, SessionId},
    policy::ExecutionMode,
    tool::{ToolContext, ToolInvocation, ToolLimits},
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

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(pending())
    }
}

#[test]
fn html_extraction_drops_active_content_and_decodes_entities() {
    let source = "<main><h1>Hello &amp; bye</h1><script>secret()</script><pre>a\n b</pre></main>";
    assert_eq!(html_to_text(source), "Hello & bye\na\n b");
}

#[tokio::test]
async fn supervised_open_rejects_private_targets() {
    let root = tempfile::tempdir().expect("tempdir");
    let context = ToolContext {
        session_id: SessionId::from("session"),
        agent_id: None,
        cwd: root.path().to_owned(),
        workspace_root: root.path().to_owned(),
        mode: ExecutionMode::Supervised,
        limits: ToolLimits::default(),
        write_scope: WriteScope::default(),
    };
    let invocation = ToolInvocation {
        call_id: CallId::from("call"),
        name: "web-search".into(),
        arguments: serde_json::json!({"operation":"open", "url":"http://127.0.0.1/private"}),
    };

    let error = WebSearchTool::default()
        .execute(context, invocation, &NeverCancel)
        .await
        .expect_err("private target");
    assert!(matches!(error, kurama_protocol::KuramaError::Policy(_)));
}

#[tokio::test]
async fn json_search_normalizes_only_public_result_fields() {
    let endpoint = serve_once(
        "application/json",
        r#"{"results":[{"title":"Rust async guide","url":"https://example.com/rust","snippet":"A compact guide."}]}"#,
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
        result.metadata["results"]
            .as_array()
            .expect("results")
            .len(),
        1
    );
    assert!(result.output.contains("Rust async guide"));
}

#[tokio::test]
async fn yolo_open_keeps_limits_but_allows_private_http() {
    let endpoint = serve_once(
        "text/html",
        "<main>Hello &amp; bye<script>secret()</script></main>",
    )
    .await;
    let invocation = ToolInvocation {
        call_id: CallId::from("open-call"),
        name: "web-search".into(),
        arguments: serde_json::json!({"operation": "open", "url": endpoint}),
    };

    let result = WebSearchTool::default()
        .execute(context(ExecutionMode::Yolo), invocation, &NeverCancel)
        .await
        .expect("open");
    assert_eq!(result.output, "Hello & bye");
    assert!(!result.output.contains("secret"));
}

#[test]
fn web_search_schema_exposes_only_search_and_open() {
    let descriptor = WebSearchTool::default().descriptor();
    assert_eq!(descriptor.name, "web-search");
    let variants = descriptor.parameters["oneOf"].as_array().expect("oneOf");
    assert_eq!(variants.len(), 2);
}

fn context(mode: ExecutionMode) -> ToolContext {
    let root = tempfile::tempdir().expect("tempdir").keep();
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

async fn serve_once(content_type: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).await.expect("read request");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write response");
    });
    format!("http://{address}/")
}
