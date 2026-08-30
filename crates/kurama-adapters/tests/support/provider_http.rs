use kurama_protocol::traits::{BoxFuture, CancelSignal};
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
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind server");
    let address = listener.local_addr().expect("server address");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept request");
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
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        let _ = sender.send(String::from_utf8(request).expect("HTTP request UTF-8"));
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

    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
