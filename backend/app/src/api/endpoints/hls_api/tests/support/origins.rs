use super::{
    super::{Arc, AsyncReadExt, AsyncWriteExt, AtomicUsize, Duration, Ordering, RwLock, StatusCode, TcpListener},
    path_has_extension,
};
use std::fmt::Write as _;

pub(in crate::api::endpoints::hls_api::tests) struct TestSegmentOrigin {
    pub(in crate::api::endpoints::hls_api::tests) base_url: String,
    pub(in crate::api::endpoints::hls_api::tests) key_requests: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) manifest_requests: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) segment_requests: Arc<AtomicUsize>,
    pub(in crate::api::endpoints::hls_api::tests) key_bytes: Option<Arc<RwLock<Arc<[u8]>>>>,
    pub(in crate::api::endpoints::hls_api::tests) task: tokio::task::JoinHandle<()>,
}

impl Drop for TestSegmentOrigin {
    fn drop(&mut self) { self.task.abort(); }
}

impl TestSegmentOrigin {
    pub(in crate::api::endpoints::hls_api::tests) fn key_request_count(&self) -> usize {
        self.key_requests.load(Ordering::SeqCst)
    }

    pub(in crate::api::endpoints::hls_api::tests) fn manifest_request_count(&self) -> usize {
        self.manifest_requests.load(Ordering::SeqCst)
    }

    pub(in crate::api::endpoints::hls_api::tests) fn segment_request_count(&self) -> usize {
        self.segment_requests.load(Ordering::SeqCst)
    }

    pub(in crate::api::endpoints::hls_api::tests) async fn set_key_bytes(&self, bytes: Arc<[u8]>) {
        if let Some(key_bytes) = &self.key_bytes {
            *key_bytes.write().await = bytes;
        }
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_segment_origin(
    body: &'static [u8],
) -> TestSegmentOrigin {
    spawn_test_status_origin(StatusCode::OK, body).await
}

pub(in crate::api::endpoints::hls_api::tests) struct TestBinaryOriginResponse {
    pub(in crate::api::endpoints::hls_api::tests) status: StatusCode,
    pub(in crate::api::endpoints::hls_api::tests) location: Option<String>,
    pub(in crate::api::endpoints::hls_api::tests) body: Arc<[u8]>,
}

impl TestBinaryOriginResponse {
    pub(in crate::api::endpoints::hls_api::tests) fn new(status: StatusCode, body: Arc<[u8]>) -> Self {
        Self { status, location: None, body }
    }

    pub(in crate::api::endpoints::hls_api::tests) fn redirect(location: String) -> Self {
        Self { status: StatusCode::FOUND, location: Some(location), body: Arc::from(&b""[..]) }
    }
}

pub(in crate::api::endpoints::hls_api::tests) type TestBinaryOriginHandler =
    Arc<dyn Fn(&str) -> TestBinaryOriginResponse + Send + Sync>;

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_binary_origin(
    handler: TestBinaryOriginHandler,
) -> TestSegmentOrigin {
    let key_requests = Arc::new(AtomicUsize::new(0));
    let manifest_requests = Arc::new(AtomicUsize::new(0));
    let segment_requests = Arc::new(AtomicUsize::new(0));
    let manifest_requests_for_task = Arc::clone(&manifest_requests);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            let manifest_requests = Arc::clone(&manifest_requests_for_task);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 2048];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buf[..read]);
                let path = request.lines().next().and_then(|line| line.split_whitespace().nth(1)).unwrap_or("/");
                if path_has_extension(path, "m3u8") {
                    manifest_requests.fetch_add(1, Ordering::SeqCst);
                }
                let TestBinaryOriginResponse { status, location, body } = handler(path);
                let reason = status.canonical_reason().unwrap_or("Status");
                let location_header = location.map_or_else(String::new, |location| format!("Location: {location}\r\n"));
                let response = format!(
                    "HTTP/1.1 {} {reason}\r\n{location_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    status.as_u16(),
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });
    TestSegmentOrigin {
        base_url: format!("http://{addr}"),
        key_requests,
        manifest_requests,
        segment_requests,
        key_bytes: None,
        task,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_status_origin(
    status: StatusCode,
    body: &'static [u8],
) -> TestSegmentOrigin {
    let body = Arc::<[u8]>::from(body);
    spawn_test_binary_origin(Arc::new(move |_path| TestBinaryOriginResponse::new(status, Arc::clone(&body)))).await
}

pub(in crate::api::endpoints::hls_api::tests) struct TestEncodedManifestOrigin {
    pub(in crate::api::endpoints::hls_api::tests) base_url: String,
    pub(in crate::api::endpoints::hls_api::tests) requests: Arc<tokio::sync::Mutex<Vec<String>>>,
    pub(in crate::api::endpoints::hls_api::tests) task: tokio::task::JoinHandle<()>,
}

impl Drop for TestEncodedManifestOrigin {
    fn drop(&mut self) { self.task.abort(); }
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_encoded_manifest_origin(
    content_encoding: Option<&'static str>,
    body: Vec<u8>,
    body_delay: Duration,
) -> TestEncodedManifestOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let body = Arc::new(body);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut chunk = [0_u8; 2048];
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                }
                requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
                let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len());
                if let Some(content_encoding) = content_encoding {
                    let _ = writeln!(&mut response, "Content-Encoding: {content_encoding}\r");
                }
                response.push_str("Connection: close\r\n\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                if body_delay.is_zero() {
                    let _ = socket.write_all(body.as_slice()).await;
                } else {
                    let split_at = body.len().min(4);
                    let _ = socket.write_all(&body[..split_at]).await;
                    tokio::time::sleep(body_delay).await;
                    let _ = socket.write_all(&body[split_at..]).await;
                }
            });
        }
    });
    TestEncodedManifestOrigin { base_url: format!("http://{addr}"), requests, task }
}

pub(in crate::api::endpoints::hls_api::tests) async fn encode_test_manifest(
    content_encoding: &str,
    body: &[u8],
) -> Vec<u8> {
    match content_encoding {
        "gzip" => {
            let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
            encoder.write_all(body).await.expect("gzip test body encodes");
            encoder.shutdown().await.expect("gzip test encoder finishes");
            encoder.into_inner()
        }
        "deflate" => {
            let mut encoder = async_compression::tokio::write::DeflateEncoder::new(Vec::new());
            encoder.write_all(body).await.expect("deflate test body encodes");
            encoder.shutdown().await.expect("deflate test encoder finishes");
            encoder.into_inner()
        }
        "br" => {
            let mut encoder = async_compression::tokio::write::BrotliEncoder::new(Vec::new());
            encoder.write_all(body).await.expect("brotli test body encodes");
            encoder.shutdown().await.expect("brotli test encoder finishes");
            encoder.into_inner()
        }
        "zstd" => {
            let mut encoder = async_compression::tokio::write::ZstdEncoder::new(Vec::new());
            encoder.write_all(body).await.expect("zstd test body encodes");
            encoder.shutdown().await.expect("zstd test encoder finishes");
            encoder.into_inner()
        }
        _ => panic!("unsupported test Content-Encoding: {content_encoding}"),
    }
}

pub(in crate::api::endpoints::hls_api::tests) struct TestTransientOrigin {
    pub(in crate::api::endpoints::hls_api::tests) base_url: String,
    pub(in crate::api::endpoints::hls_api::tests) requests: Arc<tokio::sync::Mutex<Vec<String>>>,
    pub(in crate::api::endpoints::hls_api::tests) task: tokio::task::JoinHandle<()>,
}

impl Drop for TestTransientOrigin {
    fn drop(&mut self) { self.task.abort(); }
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_transient_origin() -> TestTransientOrigin {
    spawn_test_transient_origin_with_response(
        "206 Partial Content",
        &[
            ("Content-Type", "video/mp2t"),
            ("Content-Range", "bytes 2-15/16"),
            ("Accept-Ranges", "bytes"),
            ("Cache-Control", "no-store"),
            ("ETag", "\"abc\""),
            ("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
        ],
        "transient-body",
    )
    .await
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_transient_origin_with_response(
    status_line: &'static str,
    response_headers: &'static [(&'static str, &'static str)],
    body: &'static str,
) -> TestTransientOrigin {
    spawn_test_transient_origin_with_delayed_binary_response(
        status_line,
        response_headers,
        body.as_bytes().to_vec(),
        Duration::ZERO,
    )
    .await
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_transient_origin_with_delayed_response(
    status_line: &'static str,
    response_headers: &'static [(&'static str, &'static str)],
    body: &'static str,
    response_delay: Duration,
) -> TestTransientOrigin {
    spawn_test_transient_origin_with_delayed_binary_response(
        status_line,
        response_headers,
        body.as_bytes().to_vec(),
        response_delay,
    )
    .await
}

pub(in crate::api::endpoints::hls_api::tests) async fn spawn_test_transient_origin_with_delayed_binary_response(
    status_line: &'static str,
    response_headers: &'static [(&'static str, &'static str)],
    body: Vec<u8>,
    response_delay: Duration,
) -> TestTransientOrigin {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let body = Arc::new(body);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let requests = Arc::clone(&task_requests);
            let body = Arc::clone(&body);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                requests.lock().await.push(request);
                if !response_delay.is_zero() {
                    tokio::time::sleep(response_delay).await;
                }
                let mut response_head = format!("HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n", body.len());
                for (name, value) in response_headers {
                    let _ = writeln!(&mut response_head, "{name}: {value}\r");
                }
                response_head.push_str("Connection: close\r\n\r\n");
                let _ = socket.write_all(response_head.as_bytes()).await;
                let _ = socket.write_all(body.as_slice()).await;
            });
        }
    });
    TestTransientOrigin { base_url: format!("http://{addr}"), requests, task }
}
