use super::*;

#[tokio::test]
async fn progressive_http_drop_errors_without_content_length_or_cursor_completion() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"canonical-body".to_vec()).await;
    let owner = publish_revision_fixture(&fixture, true).await?;
    let response = match fixture.serve(None).await {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(failure) => return Err(std::io::Error::other(format!("{failure:?}"))),
    };
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    let mut body = response.into_body().into_data_stream();
    assert_eq!(body.next().await.transpose().map_err(std::io::Error::other)?.as_deref(), Some(b"prefix".as_slice()));
    owner.revision().fail();
    assert!(body.next().await.is_some_and(|chunk| chunk.is_err()));
    assert!(body.next().await.is_none());
    assert_eq!(fixture.cursor().await.highest_contiguous_completed_proxy_seq, None);
    Ok(())
}

#[tokio::test]
async fn finite_terminal_media_uses_qos_completion_and_drop_semantics() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let manager = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let session = manager.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 1_000).await;
    let lease_id = HlsAccessLeaseId("terminal-lease".to_string());
    let marker = live_media_marker(&manager, &session, lease_id.clone()).await;
    let meter = Arc::new(StreamMeterHandle::new(7, Weak::new()));
    let context = HlsCacheResponseContext::new(
        lease_id,
        test_log_identity(),
        300,
        Arc::clone(manager.metrics()),
        Arc::clone(manager.segment_repair()),
        Some(Arc::clone(&meter)),
        Some(marker),
        1_000,
    );
    let proxy_session_id = ProxySessionId("terminal-session".to_string());

    let dropped = finite_hls_media_response(
        Bytes::from_static(b"0123456789"),
        None,
        "video/mp2t",
        "private, immutable",
        &context,
        &proxy_session_id,
        "terminal/1/0".to_string(),
    );
    drop(dropped);
    tokio::task::yield_now().await;
    assert_eq!(meter.bytes_total(), 0);
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);

    let partially_consumed = finite_hls_media_response(
        Bytes::from(vec![7_u8; PREPARED_MEDIA_CHUNK_SIZE.saturating_mul(2)]),
        None,
        "video/mp2t",
        "private, immutable",
        &context,
        &proxy_session_id,
        "terminal/1/0".to_string(),
    );
    let mut partial_body = partially_consumed.into_body();
    let partial_chunk = partial_body
        .frame()
        .await
        .expect("first frame")
        .expect("first frame body")
        .into_data()
        .expect("first data frame");
    drop(partial_body);
    tokio::task::yield_now().await;
    assert_eq!(partial_chunk.len(), PREPARED_MEDIA_CHUNK_SIZE);
    assert_eq!(meter.bytes_total(), PREPARED_MEDIA_CHUNK_SIZE as u64);
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);

    context.mark_media_activity().await;
    let range = header("bytes=2-5");
    let completed = finite_hls_media_response(
        Bytes::from_static(b"0123456789"),
        Some(&range),
        "video/mp2t",
        "private, immutable",
        &context,
        &proxy_session_id,
        "terminal/1/0".to_string(),
    );
    assert_eq!(completed.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!tuliprox_core::utils::response_compression::should_compress_response(&completed));
    assert_eq!(completed.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"2345"));
    assert_eq!(meter.bytes_total(), PREPARED_MEDIA_CHUNK_SIZE as u64 + 4);
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(1_000));
}

#[tokio::test]
async fn dropped_full_range_segment_body_records_request_without_completion() {
    const FULL_SIZE: usize = 512 * 1_024;
    let fixture = CachedSegmentFixture::new(vec![9_u8; FULL_SIZE]).await;
    let response = ready_response(fixture.serve(Some("bytes=0-")).await);

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], format!("bytes 0-{}/{FULL_SIZE}", FULL_SIZE - 1));
    let requested = fixture.cursor().await;
    assert_eq!(requested.first_requested_proxy_seq, Some(12));
    assert_eq!(requested.last_requested_proxy_seq, Some(12));
    assert_eq!(requested.highest_contiguous_completed_proxy_seq, None);
    assert!(fixture.session.read().await.activity.last_authorized_media_at_ms.is_some());

    let mut body = response.into_body();
    let first_chunk = body
        .frame()
        .await
        .expect("first segment frame")
        .expect("first segment frame body")
        .into_data()
        .expect("first segment data frame");
    assert!(first_chunk.len() < FULL_SIZE);
    drop(body);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    let dropped = fixture.cursor().await;
    assert_eq!(dropped.first_requested_proxy_seq, Some(12));
    assert_eq!(dropped.last_requested_proxy_seq, Some(12));
    assert_eq!(dropped.highest_contiguous_completed_proxy_seq, None);
    assert_eq!(dropped.first_segment_completed_at_ms, None);
    assert_eq!(fixture.meter.bytes_total(), u64::try_from(first_chunk.len()).expect("first chunk size"));
    assert_eq!(fixture.access.active_readers(), 0);
}
