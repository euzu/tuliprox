use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
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
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                loop {
                    let count =
                        tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await.unwrap().unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    assert!(request.len() <= 16384);
                    if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                        break;
                    }
                }
                let index = {
                    let mut requests = recorded.lock().unwrap();
                    requests.push(String::from_utf8(request).unwrap());
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
}

impl Drop for TestServer {
    fn drop(&mut self) { self.task.abort(); }
}
