use super::{
    assert_hls_cache_stream_registered, assert_no_hls_cache_stream_registered, get_response, get_status, hls_proxy_uri,
    hls_session_last_media_at_ms, map_hls_map, map_ready_segment, map_ready_segment_without_lease, map_segment,
    map_segment_with_origin_url, response_body, spawn_test_segment_origin, test_app_state,
    test_app_state_with_hls_proxy,
};
use crate::api::model::{HlsProxyManager, ProxySessionId, SegmentCacheStatus};
use axum::http::{header, StatusCode};
use std::sync::Arc;

#[tokio::test]
async fn valid_hls_proxy_segment_without_session_returns_not_found() {
    let status = get_status(test_app_state(), "/hls/shared/live/a8f31c9eQ7sLk92pV0mTaw/000123.ts").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn valid_hls_proxy_segment_with_not_ready_session_returns_not_found() {
    let app_state = test_app_state();
    let proxy_session_id = map_segment(&app_state, 123, "ts").await;

    let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/000123.ts")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn valid_hls_proxy_segment_with_not_ready_and_valid_lease_returns_service_unavailable() {
    let app_state = test_app_state();
    let proxy_session_id = map_segment(&app_state, 123, "ts").await;
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("session should exist");
    session.write().await.segments.get_mut(&123).expect("segment should exist").origin_fetch_ref = None;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn not_ready_hls_proxy_segment_with_fetch_ref_demand_fetches_and_returns_ok() {
    let origin = spawn_test_segment_origin(b"0123456789").await;
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id =
        map_segment_with_origin_url(&app_state, 123, "ts", &format!("{}/seg.ts", origin.base_url)).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
}

#[tokio::test]
async fn not_ready_hls_proxy_segment_with_range_waits_for_demand_fetch_then_returns_partial() {
    let origin = spawn_test_segment_origin(b"0123456789").await;
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id =
        map_segment_with_origin_url(&app_state, 123, "ts", &format!("{}/seg.ts", origin.base_url)).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
}

#[tokio::test]
async fn not_ready_hls_proxy_segment_without_fetch_ref_returns_service_unavailable() {
    let app_state = test_app_state();
    let proxy_session_id = map_segment(&app_state, 123, "ts").await;
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("session should exist");
    session.write().await.segments.get_mut(&123).expect("segment should exist").origin_fetch_ref = None;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn ready_hls_proxy_segment_without_lease_returns_not_found() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;

    let status = get_status(app_state, &format!("/hls/shared/live/{proxy_session_id}/000123.ts")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ready_hls_proxy_segment_without_range_returns_ok() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=300, immutable");
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("session should exist");
    {
        let session = session.read().await;
        let segment = session.segments.get(&123).expect("segment should exist");
        assert_eq!(segment.access.active_readers(), 1);
        assert!(matches!(segment.status, SegmentCacheStatus::Ready { content_length: 10, .. }));
    }

    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));

    {
        let session = session.read().await;
        let segment = session.segments.get(&123).expect("segment should exist");
        assert_eq!(segment.access.active_readers(), 0);
        assert!(segment.access.last_accessed_at_ms() > 0);
        assert!(session.activity.last_authorized_media_at_ms.is_some());
    }
    assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
}

#[tokio::test]
async fn ready_hls_proxy_segment_range_zero_open_returns_partial_content() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "m4s", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.m4s").await;

    let response = get_response(app_state, &uri, Some("bytes=0-")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-9/10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
}

#[tokio::test]
async fn ready_hls_proxy_segment_range_start_open_returns_partial_content_from_offset() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "m4v", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.m4v").await;

    let response = get_response(app_state, &uri, Some("bytes=4-")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 4-9/10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "6");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"456789"));
}

#[tokio::test]
async fn ready_hls_proxy_segment_range_start_end_returns_partial_content() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
}

#[tokio::test]
async fn ready_hls_proxy_segment_suffix_range_returns_partial_content() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(app_state, &uri, Some("bytes=-3")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 7-9/10");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"789"));
}

#[tokio::test]
async fn ready_hls_proxy_segment_unsatisfiable_range_returns_416() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=99-")).await;

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn ready_hls_proxy_segment_multi_range_returns_416() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-1,4-5")).await;

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn ready_hls_proxy_map_without_range_returns_ok() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

    let response = get_response(Arc::clone(&app_state), &uri, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
    assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
    assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
}

#[tokio::test]
async fn ready_hls_proxy_map_range_returns_partial_content() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

    let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
}

#[tokio::test]
async fn ready_hls_proxy_map_multi_range_returns_416_with_zero_content_length() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

    let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-1,4-5")).await;

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
    assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(&app_state).await;
}

#[tokio::test]
async fn ready_hls_proxy_segment_unknown_range_unit_is_ignored() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

    let response = get_response(app_state, &uri, Some("items=0-1")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
}
