use futures_util::StreamExt;
use kurama_protocol::{
    KuramaError,
    model::{ModelEvent, ModelRequest},
    traits::{BoxFuture, CancelSignal, ModelBackend},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

pub struct DelayedSseResponse {
    pub endpoint: String,
    pub captured: oneshot::Receiver<String>,
    pub first_sent: oneshot::Receiver<()>,
    pub release: oneshot::Sender<()>,
    pub disconnected: oneshot::Receiver<()>,
}

pub async fn serve_sse_once(body: impl Into<String>) -> (String, oneshot::Receiver<String>) {
    let body = body.into();
    serve_response_once(format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(), body,
    )).await
}

pub async fn serve_response_once(response: String) -> (String, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("server address");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let request = read_request(&mut socket).await;
        let _ = sender.send(String::from_utf8(request).expect("HTTP request UTF-8"));
        // Rejection of oversized/error responses may close the connection early.
        let _ = socket.write_all(response.as_bytes()).await;
    });
    (format!("http://{address}/v1"), receiver)
}

pub async fn serve_sse_until_released(
    first: impl Into<String>,
    tail: impl Into<String>,
) -> DelayedSseResponse {
    let first = first.into();
    let tail = tail.into();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("server address");
    let (captured_sender, captured) = oneshot::channel();
    let (first_sender, first_sent) = oneshot::channel();
    let (release, release_receiver) = oneshot::channel();
    let (disconnected_sender, disconnected) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
        let request = read_request(&mut socket).await;
        let _ = captured_sender.send(String::from_utf8(request).expect("HTTP request UTF-8"));
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            first.len() + tail.len()
        );
        if socket.write_all(headers.as_bytes()).await.is_err()
            || socket.write_all(first.as_bytes()).await.is_err()
            || socket.flush().await.is_err()
        {
            let _ = disconnected_sender.send(());
            return;
        }
        let _ = first_sender.send(());

        let mut probe = [0_u8; 1];
        tokio::select! {
            _ = release_receiver => {
                let _ = socket.write_all(tail.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
            _ = socket.read(&mut probe) => {
                let _ = disconnected_sender.send(());
            }
        }
    });
    DelayedSseResponse {
        endpoint: format!("http://{address}/v1"),
        captured,
        first_sent,
        release,
        disconnected,
    }
}

pub fn split_first_sse_event(body: &str) -> (String, String) {
    let boundary = body.find("\n\n").expect("fixture contains one SSE event") + 2;
    (body[..boundary].to_owned(), body[boundary..].to_owned())
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.expect("read request");
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if request_complete(&request) {
            break;
        }
    }
    request
}

fn request_complete(request: &[u8]) -> bool {
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or_default();
    request.len() >= header_end + 4 + content_length
}

pub struct NeverCancel;

impl CancelSignal for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled(&self) -> BoxFuture<'static, ()> {
        Box::pin(std::future::pending())
    }
}

pub struct TestCancel(tokio::sync::watch::Sender<bool>);

impl TestCancel {
    pub fn new() -> Self {
        Self(tokio::sync::watch::channel(false).0)
    }

    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}

impl CancelSignal for TestCancel {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    fn cancelled(&self) -> BoxFuture<'static, ()> {
        let mut receiver = self.0.subscribe();
        Box::pin(async move {
            let _ = receiver.wait_for(|cancelled| *cancelled).await;
        })
    }
}

pub async fn assert_stream_cancels(
    backend: &dyn ModelBackend,
    request: ModelRequest,
    server: DelayedSseResponse,
) {
    let cancel = TestCancel::new();
    let mut stream = backend.stream(request, &cancel).await.expect("stream");
    server.first_sent.await.expect("first response bytes");
    assert!(matches!(
        stream.next().await,
        Some(Ok(ModelEvent::ResponseStarted { .. }))
    ));
    loop {
        let mut next = Box::pin(stream.next());
        match std::future::poll_fn(|context| std::task::Poll::Ready(next.as_mut().poll(context)))
            .await
        {
            std::task::Poll::Ready(Some(Ok(event))) => {
                assert!(!matches!(event, ModelEvent::ResponseCompleted { .. }));
                continue;
            }
            std::task::Poll::Ready(other) => panic!("stream ended before release: {other:?}"),
            std::task::Poll::Pending => {}
        }
        cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), next)
                .await
                .expect("cancellation did not wake stream"),
            Some(Err(KuramaError::Cancelled))
        ));
        break;
    }
    tokio::time::timeout(std::time::Duration::from_secs(1), server.disconnected)
        .await
        .expect("cancelled response stayed open")
        .expect("disconnect signal");
    assert!(stream.next().await.is_none());
    server.captured.await.expect("captured request");
    drop(server.release);
}
