use super::{
    assert_hls_cache_stream_registered, assert_no_hls_cache_stream_registered,
    enable_channel_unavailable_custom_response, get_response, hls_session_last_media_at_ms, map_hls_map,
    map_ready_segment, map_transient_resource, request_response, response_body, terminal_test_plan_shape,
    terminalize_existing_test_lease, test_app_state_with_hls_proxy,
};
use crate::api::model::{
    AppState, HlsAccessLeaseId, HlsLifecycleEvent, HlsLifecycleEventKey, HlsProxyManager, ProxySessionId,
    HLS_TERMINAL_TAIL_SEGMENT_COUNT,
};
use axum::http::{header, Method, StatusCode};
use std::sync::Arc;

pub(in crate::api::endpoints::hls_api::tests) async fn terminal_head_content_length(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
    segment_uri: &str,
) -> usize {
    let head_response = request_response(Arc::clone(app_state), Method::HEAD, segment_uri, None).await;
    assert_eq!(head_response.status(), StatusCode::OK);
    assert_eq!(head_response.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(head_response.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(head_response.headers()[header::CACHE_CONTROL].to_str().is_ok_and(|value| value.contains("immutable")));
    let content_length = head_response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .expect("HEAD content length")
        .parse::<usize>()
        .expect("HEAD content length value");
    assert!(content_length > 0);
    assert!(response_body(head_response).await.is_empty());
    assert_eq!(hls_session_last_media_at_ms(app_state, proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(app_state).await;

    let head_range = request_response(Arc::clone(app_state), Method::HEAD, segment_uri, Some("bytes=0-187")).await;
    assert_eq!(head_range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(head_range.headers()[header::CONTENT_LENGTH], "188");
    let expected_content_range = format!("bytes 0-187/{content_length}");
    assert_eq!(head_range.headers()[header::CONTENT_RANGE].to_str().ok(), Some(expected_content_range.as_str()));
    assert!(response_body(head_range).await.is_empty());
    assert_eq!(hls_session_last_media_at_ms(app_state, proxy_session_id).await, None);
    assert_no_hls_cache_stream_registered(app_state).await;
    content_length
}

#[tokio::test]
async fn hls_terminal_response_serves_prepared_finite_full_and_range_bytes_per_index() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
    let lease_id = format!("test-access-lease-{proxy_session_id}");
    let terminal_renderer = terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
    let renders_before_requests = terminal_renderer.finite_hls_render_count();
    let finalizations_before_requests = terminal_renderer.finite_hls_finalize_count();
    assert_eq!(renders_before_requests, usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
    assert_eq!(finalizations_before_requests, usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
    let (generation, _) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
    let segment_zero_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.ts");
    let segment_one_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/1.ts");
    let repair_before = app_state.hls.proxy.segment_repair().stats().await;
    let provider_connections_before = app_state.active_provider.get_provider_connections_count();

    let head_content_length = terminal_head_content_length(&app_state, &proxy_session_id, &segment_zero_uri).await;

    let segment_zero_response = get_response(Arc::clone(&app_state), &segment_zero_uri, None).await;
    assert_eq!(segment_zero_response.status(), StatusCode::OK);
    assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
    assert!(!tuliprox_core::utils::response_compression::should_compress_response(&segment_zero_response));
    assert_eq!(segment_zero_response.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(segment_zero_response.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(segment_zero_response.headers()[header::CACHE_CONTROL]
        .to_str()
        .is_ok_and(|value| value.contains("immutable")));
    let declared_length = segment_zero_response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .expect("finite content length header")
        .parse::<usize>()
        .expect("finite content length value");
    assert_eq!(declared_length, head_content_length);
    let segment_zero = response_body(segment_zero_response).await;
    assert_eq!(segment_zero.len(), declared_length);
    assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
    assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
    assert_eq!(app_state.hls.proxy.segment_repair().stats().await, repair_before);
    assert_eq!(app_state.active_provider.get_provider_connections_count(), provider_connections_before);

    let segment_zero_again = response_body(get_response(Arc::clone(&app_state), &segment_zero_uri, None).await).await;
    let segment_one = response_body(get_response(Arc::clone(&app_state), &segment_one_uri, None).await).await;
    assert_eq!(segment_zero, segment_zero_again, "same terminal index is immutable");
    assert_ne!(segment_zero, segment_one, "successive terminal indices advance timestamps and continuity");

    let range_response = get_response(Arc::clone(&app_state), &segment_zero_uri, Some("bytes=0-187")).await;
    assert_eq!(range_response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!tuliprox_core::utils::response_compression::should_compress_response(&range_response));
    assert_eq!(range_response.headers()[header::CONTENT_LENGTH], "188");
    let expected_content_range = format!("bytes 0-187/{declared_length}");
    assert_eq!(range_response.headers()[header::CONTENT_RANGE].to_str().ok(), Some(expected_content_range.as_str()));
    assert_eq!(range_response.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(range_response.headers()[header::CACHE_CONTROL].to_str().is_ok_and(|value| value.contains("immutable")));
    assert_eq!(response_body(range_response).await, segment_zero.slice(..188));

    let unsatisfiable_range = format!("bytes={declared_length}-");
    let unsatisfiable_response = get_response(app_state, &segment_zero_uri, Some(&unsatisfiable_range)).await;
    assert_eq!(unsatisfiable_response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(!tuliprox_core::utils::response_compression::should_compress_response(&unsatisfiable_response));
    let expected_unsatisfied_content_range = format!("bytes */{declared_length}");
    assert_eq!(
        unsatisfiable_response.headers()[header::CONTENT_RANGE].to_str().ok(),
        Some(expected_unsatisfied_content_range.as_str())
    );
    assert!(response_body(unsatisfiable_response).await.is_empty());
    assert_eq!(
        terminal_renderer.finite_hls_render_count(),
        renders_before_requests,
        "terminal HTTP serving must not invoke the TS writer again"
    );
    assert_eq!(
        terminal_renderer.finite_hls_finalize_count(),
        finalizations_before_requests,
        "terminal HTTP serving must not invoke lease-specific TS finalization again"
    );
}

#[tokio::test]
async fn hls_terminal_response_body_after_lease_denial_does_not_extend_shared_session_activity() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
    let lease_id = format!("test-access-lease-{proxy_session_id}");
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
    let (generation, _) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("terminal session");
    let segment_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.ts");
    let response = get_response(Arc::clone(&app_state), &segment_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let activity_after_authorized_response =
        session.read().await.activity.last_authorized_media_at_ms.expect("terminal GET marks media activity");

    let _ = app_state
        .hls
        .proxy
        .deny_access_lease(
            &HlsAccessLeaseId(lease_id),
            tuliprox_hls::HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,
        )
        .await;
    assert!(!response_body(response).await.is_empty());

    assert_eq!(
        session.read().await.activity.last_authorized_media_at_ms,
        Some(activity_after_authorized_response),
        "body completion after denial must not extend activity"
    );
}

#[tokio::test]
async fn hls_terminal_response_rejects_stale_malformed_and_out_of_bounds_paths() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
    let lease_id = format!("test-access-lease-{proxy_session_id}");
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
    let (generation, segment_count) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
    let stale_generation = generation.saturating_add(1);
    let stale_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{stale_generation}/0.ts");
    let out_of_bounds_uri =
        format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/{segment_count}.ts");

    let malformed_generation = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/01/0.ts");
    let non_numeric_generation =
        format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/not-a-generation/0.ts");
    let overflowing_generation =
        format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/18446744073709551616/0.ts");
    let malformed_file = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/00.ts");
    let wrong_extension = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.m4s");

    for uri in [
        stale_uri,
        out_of_bounds_uri,
        malformed_generation,
        non_numeric_generation,
        overflowing_generation,
        malformed_file,
        wrong_extension,
    ] {
        let response = get_response(Arc::clone(&app_state), &uri, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.headers().contains_key(header::LOCATION));
    }
}

#[tokio::test]
async fn expired_route_replays_already_committed_custom_tail() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
    let lease_id = format!("test-access-lease-{proxy_session_id}");
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
    let (generation, _) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
    let proxy_session_key = ProxySessionId(proxy_session_id.clone());
    let lease_key = HlsAccessLeaseId(lease_id.clone());
    let expired_at_ms = super::super::current_time_millis().saturating_sub(1);
    {
        let mut leases = app_state.hls.proxy.access_leases().write().await;
        let mut lease = leases.remove_access_lease(&lease_key).expect("terminal lease exists");
        lease.valid_until_ms = expired_at_ms;
        leases.prepare_access_lease(lease);
    }
    let manifest_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/manifest.m3u8");
    let segment_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/terminal/{generation}/0.ts");

    let manifest_response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;
    let segment_response = get_response(Arc::clone(&app_state), &segment_uri, None).await;

    assert_eq!(manifest_response.status(), StatusCode::OK);
    assert!(!manifest_response.headers().contains_key(header::LOCATION));
    assert!(String::from_utf8(response_body(manifest_response).await.to_vec())
        .expect("expired committed manifest utf8")
        .ends_with("#EXT-X-ENDLIST\n"));
    assert_eq!(segment_response.status(), StatusCode::OK);
    assert!(!response_body(segment_response).await.is_empty());
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_key)
        .await
        .expect("terminal session exists");
    assert!(session.read().await.terminal_tail_protection(&lease_key).is_some());

    let cleanup_at_ms = super::super::current_time_millis();
    app_state
        .hls
        .proxy
        .handle_lifecycle_event(
            &app_state.active_users,
            &app_state.active_provider,
            HlsLifecycleEvent {
                key: HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: lease_key.clone(),
                    proxy_session_id: proxy_session_key,
                },
                due_at_ms: cleanup_at_ms,
            },
            cleanup_at_ms,
        )
        .await;

    assert!(!session.read().await.has_terminal_tail_protections());
    assert!(app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&lease_key, &ProxySessionId(proxy_session_id), cleanup_at_ms)
        .await
        .is_none());
    assert_eq!(get_response(Arc::clone(&app_state), &manifest_uri, None).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(get_response(app_state, &segment_uri, None).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expired_route_without_session_evidence_returns_not_found_instead_of_unanchored_manifest() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"live-without-lease").await;
    enable_channel_unavailable_custom_response(&app_state);
    let missing_lease = "expired-without-base-evidence";
    let manifest_uri = format!("/hls/shared/live/{proxy_session_id}/{missing_lease}/manifest.m3u8");

    let response = get_response(app_state, &manifest_uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert!(response_body(response).await.is_empty());
}

#[tokio::test]
async fn hls_terminal_response_normal_segment_map_and_resource_routes_never_serve_terminal_fallbacks() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
    let lease_id = format!("test-access-lease-{proxy_session_id}");
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
    let normal_segment_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/000123.ts");

    let normal_segment_response = get_response(Arc::clone(&app_state), &normal_segment_uri, None).await;

    assert_eq!(normal_segment_response.status(), StatusCode::OK);
    assert!(!normal_segment_response.headers().contains_key(header::LOCATION));
    assert_eq!(response_body(normal_segment_response).await, bytes::Bytes::from_static(b"original-live-tail"));

    let map_temp_dir = tempfile::tempdir().expect("map tempdir");
    let map_app_state =
        test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(map_temp_dir.path(), 300)));
    let map_proxy_session_id = map_hls_map(&map_app_state, b"original-map", true).await;
    let map_lease_id = format!("test-access-lease-{map_proxy_session_id}");
    let map_session = map_app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(map_proxy_session_id.clone()))
        .await
        .expect("map session exists");
    let map_base_proxy_seq = *map_session.read().await.segments.keys().next().expect("map manifest has media");
    terminalize_existing_test_lease(&map_app_state, &map_proxy_session_id, &map_lease_id, map_base_proxy_seq).await;
    let map_uri = format!("/hls/shared/live/{map_proxy_session_id}/{map_lease_id}/map/000000.mp4");
    let map_response = get_response(map_app_state, &map_uri, None).await;
    assert_eq!(map_response.status(), StatusCode::NOT_FOUND);
    assert!(!map_response.headers().contains_key(header::LOCATION));
    assert!(response_body(map_response).await.is_empty());

    let resource_temp_dir = tempfile::tempdir().expect("resource tempdir");
    let resource_app_state =
        test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(resource_temp_dir.path(), 300)));
    let (resource_proxy_session_id, resource_id) =
        map_transient_resource(&resource_app_state, "http://origin.example.com/old.ts", "ts", true).await;
    let resource_lease_id = format!("test-access-lease-{resource_proxy_session_id}");
    terminalize_existing_test_lease(&resource_app_state, &resource_proxy_session_id, &resource_lease_id, 0).await;
    let resource_uri = format!("/hls/shared/live/{resource_proxy_session_id}/{resource_lease_id}/r/{resource_id}.ts");
    let resource_response = get_response(resource_app_state, &resource_uri, None).await;
    assert_eq!(resource_response.status(), StatusCode::NOT_FOUND);
    assert!(!resource_response.headers().contains_key(header::LOCATION));
    assert!(response_body(resource_response).await.is_empty());
}
