use std::{
    future::{poll_fn, Future},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
};

pub(crate) fn http_response(status: u16, body: &str) -> String {
    format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

pub(crate) struct TestServer {
    pub(crate) url: String,
    pub(crate) requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl TestServer {
    pub(crate) async fn new(response: String) -> Self { Self::new_bytes(response.into_bytes()).await }

    pub(crate) async fn new_bytes(response: Vec<u8>) -> Self {
        Self::delayed_bytes(response, Duration::ZERO, false).await
    }

    pub(crate) async fn delayed_bytes(response: Vec<u8>, delay: Duration, send_headers: bool) -> Self {
        Self::serve(vec![response], delay, send_headers).await
    }

    pub(crate) async fn sequence(responses: Vec<String>) -> Self {
        Self::serve(responses.into_iter().map(String::into_bytes).collect(), Duration::ZERO, false).await
    }

    pub(crate) async fn serve(responses: Vec<Vec<u8>>, delay: Duration, send_headers: bool) -> Self {
        assert!(!responses.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let request = read_request(&mut stream).await;
                let index = {
                    let mut requests = recorded.lock().unwrap();
                    requests.push(request);
                    requests.len() - 1
                };
                let response = &responses[index.min(responses.len() - 1)];
                let split = if send_headers {
                    response.windows(4).position(|bytes| bytes == b"\r\n\r\n").unwrap() + 4
                } else {
                    0
                };
                let _ = stream.write_all(&response[..split]).await;
                tokio::time::sleep(delay).await;
                let _ = stream.write_all(&response[split..]).await;
            }
        });
        Self { url, requests, task }
    }

    /// Each received stream is a request-observed milestone. The test owns when
    /// headers/body/chunks are released, and keeps stalled streams open until timeout.
    pub(crate) async fn controlled() -> (Self, mpsc::UnboundedReceiver<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let (send, receive) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let request = read_request(&mut stream).await;
                recorded.lock().unwrap().push(request);
                if send.send(stream).is_err() {
                    break;
                }
            }
        });
        (Self { url, requests, task }, receive)
    }
}

async fn read_request(stream: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let count = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await.unwrap().unwrap();
        assert!(count > 0, "connection closed before request headers");
        request.extend_from_slice(&buffer[..count]);
        assert!(request.len() <= 16384);
    }
    String::from_utf8(request).unwrap()
}

/// Keep the paused runtime ready while real I/O progresses, preventing automatic
/// clock advance. This real-time watchdog also bounds missing milestones. All
/// acquisition/controller futures are inline, so panic drops them, not detached tasks.
pub(crate) async fn with_paused_io<T>(future: impl Future<Output = T>) -> T {
    let started = std::time::Instant::now();
    let mut future = std::pin::pin!(future);
    poll_fn(|cx| {
        assert!(started.elapsed() < Duration::from_secs(5), "paused-I/O test stalled (real-time watchdog)");
        let result = future.as_mut().poll(cx);
        if result.is_pending() {
            cx.waker().wake_by_ref();
        }
        result
    })
    .await
}

impl Drop for TestServer {
    fn drop(&mut self) { self.task.abort(); }
}
