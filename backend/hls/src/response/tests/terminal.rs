use super::*;

#[tokio::test]
async fn finite_terminal_media_supports_full_range_and_unsatisfiable_responses() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let manager = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let context = HlsCacheResponseContext::new(
        HlsAccessLeaseId("terminal-lease".to_string()),
        test_log_identity(),
        300,
        Arc::clone(manager.metrics()),
        Arc::clone(manager.segment_repair()),
        None,
        None,
        1_000,
    );
    let proxy_session_id = ProxySessionId("terminal-session".to_string());
    let full = finite_hls_media_response(
        Bytes::from_static(b"0123456789"),
        None,
        "video/mp2t",
        "private, immutable",
        &context,
        &proxy_session_id,
        "terminal/1/0".to_string(),
    );
    assert_eq!(full.status(), StatusCode::OK);
    assert!(!tuliprox_core::utils::response_compression::should_compress_response(&full));
    assert_eq!(full.headers()[header::CONTENT_LENGTH], "10");
    assert_eq!(full.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"0123456789"));

    let range_header = header("bytes=2-5");
    let partial = finite_hls_media_response(
        Bytes::from_static(b"0123456789"),
        Some(&range_header),
        "video/mp2t",
        "private, immutable",
        &context,
        &proxy_session_id,
        "terminal/1/0".to_string(),
    );
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!tuliprox_core::utils::response_compression::should_compress_response(&partial));
    assert_eq!(partial.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(partial.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"2345"));

    let invalid_header = header("bytes=20-");
    let invalid = finite_hls_media_response(
        Bytes::from_static(b"0123456789"),
        Some(&invalid_header),
        "video/mp2t",
        "private, immutable",
        &context,
        &proxy_session_id,
        "terminal/1/0".to_string(),
    );
    assert_eq!(invalid.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(!tuliprox_core::utils::response_compression::should_compress_response(&invalid));
    assert_eq!(invalid.headers()[header::CONTENT_RANGE], "bytes */10");
}

#[tokio::test(start_paused = true)]
async fn finite_terminal_media_stream_honors_shared_send_deadline() {
    let stream = ActiveReaderStream::new(
        Box::pin(futures::stream::pending()),
        None,
        CacheBodyLogContext {
            body_id: "00000001".to_string(),
            identity: test_log_identity(),
            resource_id: "terminal/1/0".to_string(),
            object_kind: "TerminalSegment",
            source: "prepared",
            content_length: 1,
        },
        Arc::new(ArcSwapOption::from(None::<Arc<StreamMeterHandle>>)),
        None,
        None,
    );
    let task = tokio::spawn(async move {
        let mut stream = stream;
        stream.next().await
    });
    tokio::task::yield_now().await;
    advance(super::super::hls_client_body_send_deadline().saturating_add(Duration::from_millis(1))).await;

    let result = task.await.expect("deadline task").expect("deadline result");
    assert_eq!(result.expect_err("deadline error").kind(), std::io::ErrorKind::TimedOut);
}
