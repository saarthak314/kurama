use std::time::Duration;

use kurama_adapters::{HttpClient, OpenAiCompatBackend};
use kurama_sdk::Agent;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinSet,
};

const SUCCESS: &str = concat!(
    "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"retry recovered\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

async fn run_provider(
    first_status: u16,
    first_body: &'static str,
) -> (
    Result<kurama_sdk::TurnOutcome, kurama_protocol::KuramaError>,
    Vec<Value>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (received, mut requests) = tokio::sync::mpsc::channel::<Value>(2);
    let mut server = JoinSet::new();
    server.spawn(async move {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0, "request headers ended early");
                bytes.extend_from_slice(&chunk[..count]);
                assert!(bytes.len() <= 4 * 1024 * 1024);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") { break end + 4; }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let length: usize = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
            }).expect("request length");
            assert!(length <= 4 * 1024 * 1024);
            while bytes.len() < header_end + length {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0, "request body ended early");
                bytes.extend_from_slice(&chunk[..count]);
            }
            received.try_send(serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()).unwrap();
            let (status, body, content_type) = if attempt == 0 {
                (first_status, first_body, "text/plain")
            } else {
                (200, SUCCESS, "text/event-stream")
            };
            let response = format!("HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let backend = OpenAiCompatBackend::from_endpoint(HttpClient::new(), &endpoint, None).unwrap();
    let mut agent = Agent::new().backend(backend).build().unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt("inspect this fixture"),
    )
    .await
    .expect("bounded provider turn");
    server.abort_all();
    while let Some(result) = server.join_next().await {
        if let Err(error) = result {
            assert!(error.is_cancelled(), "fixture failed: {error}");
        }
    }
    let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect();
    (outcome, requests)
}

#[tokio::test]
async fn transient_http_status_retries_the_unchanged_model_request() {
    let (outcome, requests) = run_provider(503, "overloaded").await;
    assert_eq!(outcome.unwrap().text, "retry recovered");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
}

#[tokio::test]
async fn authentication_status_does_not_retry_based_on_error_body_words() {
    let (outcome, requests) =
        run_provider(401, "authentication failed; timeout is not the cause").await;
    assert!(
        outcome.is_err(),
        "authentication failure must not become a successful retry"
    );
    assert_eq!(requests.len(), 1);
}
