use super::*;

#[test]
fn transient_body_log_kind_uses_resource_kind() {
    assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Key), "bin"), "Key");
    assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Map), "bin"), "Map");
    assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Segment), "key"), "Segment");
    assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Part), "m4s"), "Segment");
    assert_eq!(transient_body_object_kind(Some(TransientResourceKind::Other), "bin"), "Segment");
}

#[test]
fn transient_body_log_kind_falls_back_to_key_extension() {
    assert_eq!(transient_body_object_kind(None, "key"), "Key");
    assert_eq!(transient_body_object_kind(None, "KEY"), "Key");
    assert_eq!(transient_body_object_kind(None, "ts"), "Segment");
}

#[tokio::test]
async fn full_object_segment_ranges_record_request_and_completion_without_changing_http_range_status() {
    const FULL_SIZE: usize = 512;
    for (range, expected_status) in [
        (None, StatusCode::OK),
        (Some("bytes=0-"), StatusCode::PARTIAL_CONTENT),
        (Some("bytes=0-511"), StatusCode::PARTIAL_CONTENT),
        (Some("bytes=-512"), StatusCode::PARTIAL_CONTENT),
    ] {
        let expected_body = vec![7_u8; FULL_SIZE];
        let fixture = CachedSegmentFixture::new(expected_body.clone()).await;
        let response = ready_response(fixture.serve(range).await);

        assert_eq!(response.status(), expected_status, "unexpected status for {range:?}");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&response));
        assert_eq!(response.headers()[header::CONTENT_LENGTH], FULL_SIZE.to_string());
        if range.is_some() {
            assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-511/512");
        } else {
            assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        }
        assert_eq!(fixture.access.active_readers(), 1);
        let requested = fixture.cursor().await;
        assert_eq!(requested.first_requested_proxy_seq, Some(12), "missing request for {range:?}");
        assert_eq!(requested.last_requested_proxy_seq, Some(12), "missing request for {range:?}");
        assert_eq!(requested.highest_contiguous_completed_proxy_seq, None);
        assert!(fixture.session.read().await.activity.last_authorized_media_at_ms.is_some());

        assert_eq!(
            response.into_body().collect().await.expect("full range body").to_bytes(),
            Bytes::from(expected_body)
        );
        let completed = fixture.wait_for_completion().await;
        assert_eq!(completed.highest_contiguous_completed_proxy_seq, Some(12));
        assert!(completed.first_segment_completed_at_ms.is_some());
        assert_eq!(fixture.meter.bytes_total(), fixture.full_size);
        assert_eq!(fixture.access.active_readers(), 0);
    }
}

#[tokio::test]
async fn partial_segment_ranges_mark_activity_without_mutating_playback_cursor() {
    const FULL_SIZE: usize = 512;
    for (range, expected_start, expected_end) in [("bytes=1-", 1, 511), ("bytes=0-187", 0, 187)] {
        let bytes = (0..FULL_SIZE).map(|value| u8::try_from(value % 251).expect("test byte")).collect::<Vec<_>>();
        let fixture = CachedSegmentFixture::new(bytes.clone()).await;
        let response = ready_response(fixture.serve(Some(range)).await);
        let expected_body = &bytes[expected_start..=expected_end];

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert!(!tuliprox_core::utils::response_compression::should_compress_response(&response));
        assert_eq!(
            response.headers()[header::CONTENT_RANGE],
            format!("bytes {expected_start}-{expected_end}/{FULL_SIZE}")
        );
        assert_eq!(response.headers()[header::CONTENT_LENGTH], expected_body.len().to_string());
        assert!(fixture.session.read().await.activity.last_authorized_media_at_ms.is_some());
        assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());

        assert_eq!(
            response.into_body().collect().await.expect("partial range body").to_bytes(),
            Bytes::copy_from_slice(expected_body)
        );
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());
        assert_eq!(fixture.meter.bytes_total(), u64::try_from(expected_body.len()).expect("partial body size"));
        assert_eq!(fixture.access.active_readers(), 0);
    }
}

#[tokio::test]
async fn unsatisfiable_or_missing_segment_records_neither_activity_nor_cursor() {
    const FULL_SIZE: usize = 512;
    let fixture = CachedSegmentFixture::new(vec![5_u8; FULL_SIZE]).await;
    let unsatisfiable = ready_response(fixture.serve(Some("bytes=512-")).await);

    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(unsatisfiable.headers()[header::CONTENT_RANGE], "bytes */512");
    assert_eq!(fixture.session.read().await.activity.last_authorized_media_at_ms, None);
    assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());
    assert_eq!(fixture.meter.bytes_total(), 0);
    assert_eq!(fixture.access.active_readers(), 0);

    let missing = serve_hls_segment_cache_outcome(
        Arc::clone(&fixture.segment_cache),
        Arc::clone(&fixture.session),
        HlsSegmentFile { proxy_seq: 99, extension: "ts".to_string() },
        Some(header("bytes=0-")),
        &fixture.context,
    )
    .await;
    assert!(matches!(missing, HlsResourceServeOutcome::Failure(HlsResourceServeFailure::Missing)));
    assert_eq!(fixture.session.read().await.activity.last_authorized_media_at_ms, None);
    assert_eq!(fixture.cursor().await, HlsLeasePlaybackCursor::default());
    assert_eq!(fixture.meter.bytes_total(), 0);
}

#[tokio::test]
async fn cache_body_marks_media_activity_at_start_and_body_end() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 12, "ts");
    segment_cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");
    let access = Arc::new(CacheAccessState::new());
    let session = hls_proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 1_000).await;
    let marker = live_media_marker(&hls_proxy, &session, HlsAccessLeaseId("lease-a".to_string())).await;

    let response = serve_cache_object(
        segment_cache,
        CacheObject {
            is_media: true,
            key,
            access,
            content_type: "video/mp2t".to_string(),
            log_context: CacheObjectLogContext {
                lease: "lease-a".to_string(),
                identity: test_log_identity(),
                resource_id: "000012".to_string(),
                object_kind: "Segment",
                body_source: "normal",
            },
            repair_context: None,
        },
        Some(header("bytes=0-")),
        CacheObjectServeContext {
            cache_duration_seconds: 300,
            metrics: None,
            segment_repair: test_segment_repair_manager(),
            qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<tuliprox_session::StreamMeterHandle>>)),
            media_activity_marker: Some(marker),
            playback_cursor_tracking: HlsPlaybackCursorTracking::Disabled,
            now_ms: 1_000,
        },
    )
    .await
    .expect("cache object response");

    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(1_000));

    assert_eq!(response.into_body().collect().await.expect("body").to_bytes(), Bytes::from_static(b"0123456789"));
    tokio::time::sleep(Duration::from_millis(10)).await;

    assert!(
        session.read().await.activity.last_authorized_media_at_ms.expect("media activity") >= 1_000,
        "body completion should not move media activity backwards"
    );
}
