use super::*;

#[tokio::test]
async fn cache_hit_bodies_use_independent_readers_for_same_object() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let key = SegmentCacheKey::new(ProxySessionId("proxy-session".to_string()), 12, "ts");
    segment_cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");
    let access = Arc::new(CacheAccessState::new());
    let object = CacheObject {
        is_media: true,
        key,
        access: Arc::clone(&access),
        content_type: "video/mp2t".to_string(),
        log_context: CacheObjectLogContext {
            lease: "lease-a".to_string(),
            identity: test_log_identity(),
            resource_id: "000012".to_string(),
            object_kind: "Segment",
            body_source: "normal",
        },
        repair_context: None,
    };

    let first = serve_cache_object(
        Arc::clone(&segment_cache),
        object.clone(),
        Some(header("bytes=0-")),
        CacheObjectServeContext {
            cache_duration_seconds: 300,
            metrics: None,
            segment_repair: test_segment_repair_manager(),
            qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<tuliprox_session::StreamMeterHandle>>)),
            media_activity_marker: None,
            playback_cursor_tracking: HlsPlaybackCursorTracking::Disabled,
            now_ms: 1,
        },
    )
    .await
    .expect("first cache object response");
    let second = serve_cache_object(
        segment_cache,
        CacheObject {
            is_media: true,
            log_context: CacheObjectLogContext {
                lease: "lease-b".to_string(),
                identity: test_log_identity(),
                resource_id: "000012".to_string(),
                object_kind: "Segment",
                body_source: "normal",
            },
            ..object
        },
        Some(header("bytes=0-")),
        CacheObjectServeContext {
            cache_duration_seconds: 300,
            metrics: None,
            segment_repair: test_segment_repair_manager(),
            qos_meter: Arc::new(ArcSwapOption::from(None::<Arc<tuliprox_session::StreamMeterHandle>>)),
            media_activity_marker: None,
            playback_cursor_tracking: HlsPlaybackCursorTracking::Disabled,
            now_ms: 2,
        },
    )
    .await
    .expect("second cache object response");

    assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(access.active_readers(), 2);

    let (first_body, second_body) = tokio::join!(first.into_body().collect(), second.into_body().collect(),);

    assert_eq!(first_body.expect("first body").to_bytes(), Bytes::from_static(b"0123456789"));
    assert_eq!(second_body.expect("second body").to_bytes(), Bytes::from_static(b"0123456789"));
    assert_eq!(access.active_readers(), 0);
    assert_eq!(access.last_accessed_at_ms(), 2);
}
