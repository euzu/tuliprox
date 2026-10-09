use super::{
    build_segment_origin_headers, normal_manifest, test_segment_repair_manager, SegmentFetchContext,
    SegmentFetchPolicy, TestSegmentServer,
};
use crate::{
    HlsSegmentCache, HlsSegmentFile, HlsSegmentWorkerPool, HlsSessionKey, HlsSessionStore, SegmentCacheStatus,
};
use axum::http::{header, HeaderMap, HeaderValue};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

#[test]
fn segment_origin_headers_remove_client_range_and_force_identity() {
    let mut headers = HeaderMap::new();
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
    headers.insert(header::HOST, HeaderValue::from_static("origin.example.com"));
    let headers = build_segment_origin_headers(&headers, &HeaderMap::new(), None).expect("headers should build");

    assert!(!headers.contains_key(header::RANGE));
    assert!(!headers.contains_key(header::AUTHORIZATION));
    assert!(!headers.contains_key(header::COOKIE));
    assert!(!headers.contains_key(header::HOST));
    assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("encoding"), "identity");
}

#[test]
fn segment_origin_headers_apply_byterange() {
    let headers = build_segment_origin_headers(
        &HeaderMap::new(),
        &HeaderMap::new(),
        Some(tuliprox_parser::hls::origin_manifest::ParsedByteRange { offset: 10, length: 5 }),
    )
    .expect("headers should build");

    assert_eq!(headers.get(header::RANGE).expect("range"), "bytes=10-14");
}

pub(in crate::segment_fetcher::tests) async fn spawn_range_segment_server() -> TestSegmentServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0_u8; 2048];
        let Ok(read) = socket.read(&mut buf).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let request = String::from_utf8_lossy(&buf[..read]).to_string();
        task_requests.lock().await.push(request.clone());
        let body = if request.to_ascii_lowercase().contains("range: bytes=10-14") { "seg!!" } else { "bad" };
        let status = if body == "seg!!" { "206 Partial Content" } else { "200 OK" };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes 10-14/20\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });

    TestSegmentServer { base_url: format!("http://{addr}"), requests, task }
}

#[tokio::test]
async fn origin_byterange_segment_fetch_uses_http_range_and_stores_logical_segment() {
    let server = spawn_range_segment_server().await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        origin_segment_timeout_ms: 1_000,
        ..SegmentFetchPolicy::default()
    };
    let store = HlsSessionStore::new();
    let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    {
        let mut session = session.write().await;
        session
            .apply_origin_manifest(&normal_manifest(&format!(
                "#EXTM3U\n#EXT-X-BYTERANGE:5@10\n#EXTINF:4.0,\n{}/big.m4s\n",
                server.base_url
            )))
            .expect("manifest maps");
    }
    let context = SegmentFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
        segment_repair: test_segment_repair_manager(),
        repair_access_lease_id: None,
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: true,
        origin_io: None,
    };
    let worker = Arc::new(HlsSegmentWorkerPool::new(policy));
    let segment_file = HlsSegmentFile { proxy_seq: 0, extension: "m4s".to_string() };

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].to_ascii_lowercase().contains("range: bytes=10-14"));
    let session = context.session.read().await;
    let segment = session.segments.get(&0).expect("segment");
    assert!(matches!(segment.status, SegmentCacheStatus::Ready { content_length: 5, .. }));
    assert!(context.segment_cache.metadata(&segment.cache_key).await.expect("metadata").is_some());
}
