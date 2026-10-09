use super::{
    enable_channel_unavailable_custom_response, get_response, map_hls_map, map_ready_segment, response_body,
    terminal_tail::{
        assert_prepared_terminal_cutover_manifest, assert_terminal_cutover_sticky_after_recovery,
        commit_prepared_terminal_cutover, exhaust_terminal_cutover_recovery, prepared_terminal_cutover_fixture,
        publish_test_manifest_and_exhaust_configured_acceptance, terminal_cutover_acceptance_directive,
    },
    terminal_test_asset, terminal_test_plan_shape, terminalize_existing_test_lease, test_app_state_with_hls_proxy,
    test_hls_access_context,
};
use crate::api::model::{
    HlsAccessLeaseId, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsLeasePlaybackMode,
    HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsMapSignature, HlsMediaContainer, HlsProxyManager,
    ProxySessionId,
};
use axum::http::{header, StatusCode};
use std::sync::Arc;

#[tokio::test]
async fn terminal_lease_manifest_is_inline_immutable_endlist_on_canonical_path() {
    const LIVE_TAIL_BYTES: &[u8] = b"original-live-tail";
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", LIVE_TAIL_BYTES).await;
    let lease_id = format!("test-access-lease-{proxy_session_id}");
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id, 123).await;
    let cursor_before = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &HlsAccessLeaseId(lease_id.clone()),
            &ProxySessionId(proxy_session_id.clone()),
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal lease before media read")
        .playback_cursor;
    let (generation, segment_count) = terminal_test_plan_shape(&app_state, &proxy_session_id, &lease_id).await;
    let manifest_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/manifest.m3u8");
    let mut reloaded_api_proxy =
        app_state.app_config.api_proxy.load_full().as_deref().cloned().expect("test API proxy config");
    reloaded_api_proxy.server[0].path = Some("reloaded".to_string());
    app_state.app_config.api_proxy.store(Some(Arc::new(reloaded_api_proxy)));

    let response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("terminal manifest is utf8");
    let repeated = get_response(Arc::clone(&app_state), &manifest_uri, None).await;
    assert_eq!(repeated.status(), StatusCode::OK);
    assert!(!repeated.headers().contains_key(header::LOCATION));
    assert_eq!(response_body(repeated).await, body.as_bytes());
    let live_tail = format!("/{proxy_session_id}/{lease_id}/000123.ts");
    let terminal_prefix = format!("/{proxy_session_id}/{lease_id}/terminal/{generation}/");
    assert!(body.contains(&live_tail));
    assert!(body.contains("/iptv/hls/shared/live/"));
    assert!(!body.contains("/reloaded/hls/shared/live/"));
    assert_eq!(body.matches(&terminal_prefix).count(), usize::from(segment_count));
    assert_eq!(body.matches("#EXT-X-DISCONTINUITY\n").count(), 1);
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
    assert!(body.find(&live_tail) < body.find("#EXT-X-DISCONTINUITY\n"));
    assert!(body.find("#EXT-X-DISCONTINUITY\n") < body.find(&terminal_prefix));

    let live_tail_uri = format!("/hls/shared/live/{proxy_session_id}/{lease_id}/000123.ts");
    let live_tail_response = get_response(Arc::clone(&app_state), &live_tail_uri, Some("bytes=0-")).await;
    assert_eq!(live_tail_response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(live_tail_response.headers()[header::CONTENT_LENGTH], LIVE_TAIL_BYTES.len().to_string());
    assert_eq!(
        live_tail_response.headers()[header::CONTENT_RANGE],
        format!("bytes 0-{}/{}", LIVE_TAIL_BYTES.len() - 1, LIVE_TAIL_BYTES.len())
    );
    assert_eq!(response_body(live_tail_response).await, bytes::Bytes::from_static(LIVE_TAIL_BYTES));
    let cursor_after = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &HlsAccessLeaseId(lease_id),
            &ProxySessionId(proxy_session_id),
            super::super::current_time_millis(),
        )
        .await
        .expect("terminal lease after media read")
        .playback_cursor;
    assert_eq!(cursor_after, cursor_before);
}

#[tokio::test]
async fn commits_prepared_terminal_tail_once_when_recovery_misses_deadline() {
    let fixture = prepared_terminal_cutover_fixture().await;
    let directive = terminal_cutover_acceptance_directive(&fixture).await;
    let pressured_lease = exhaust_terminal_cutover_recovery(&fixture, directive).await;
    let (generation, cutover_now_ms) = commit_prepared_terminal_cutover(&fixture, &pressured_lease).await;
    assert_prepared_terminal_cutover_manifest(&fixture, generation).await;
    assert_terminal_cutover_sticky_after_recovery(&fixture, generation, cutover_now_ms).await;
}

#[tokio::test]
async fn warm_fmp4_map_cutover_without_ready_reserve_fails_closed_without_a_ts_splice() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    enable_channel_unavailable_custom_response(&app_state);
    let proxy_session_id = ProxySessionId(map_hls_map(&app_state, b"fmp4-init", true).await);
    let lease_id = HlsAccessLeaseId(format!("test-access-lease-{}", proxy_session_id.0));
    let now_ms = super::super::current_time_millis();
    let (base_proxy_seq, duration_ms) = {
        let session = app_state
            .hls
            .proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("fMP4 shared session");
        let session = session.read().await;
        let entry = session.segments.values().next().expect("fMP4 media entry");
        (entry.proxy_seq, entry.duration_ms)
    };
    let snapshot = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(now_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: now_ms,
        first_proxy_seq: base_proxy_seq,
        last_proxy_seq: base_proxy_seq,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: base_proxy_seq,
            duration_ms,
            uri: format!("/iptv/hls/shared/live/{}/{}/{base_proxy_seq:06}.m4s", proxy_session_id.0, lease_id.0).into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: terminal_test_asset().duration_ms().saturating_add(1_000),
        playlist_duration_ms: duration_ms,
        last_visible_media_end_ms: duration_ms,
        active_map: Some(HlsMapSignature { fingerprint: [7; 32], container: HlsMediaContainer::FragmentedMp4 }),
        active_encryption: None,
        container: HlsMediaContainer::FragmentedMp4,
    };
    publish_test_manifest_and_exhaust_configured_acceptance(&app_state, &proxy_session_id, &lease_id, snapshot, now_ms)
        .await;
    let manifest_uri = format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, lease_id.0);

    let response = get_response(Arc::clone(&app_state), &manifest_uri, None).await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert!(response_body(response).await.is_empty());
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, super::super::current_time_millis())
        .await
        .expect("failed-closed lease snapshot");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    let terminal_uri = format!("/hls/shared/live/{}/{}/terminal/1/0.ts", proxy_session_id.0, lease_id.0);
    let terminal_response = get_response(app_state, &terminal_uri, None).await;
    assert_eq!(terminal_response.status(), StatusCode::NOT_FOUND);
    assert!(!terminal_response.headers().contains_key(header::LOCATION));
    assert!(response_body(terminal_response).await.is_empty());
}

#[tokio::test]
async fn delayed_live_resource_completion_revalidates_after_terminal_cutover_without_a_sleep() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"ready-before-cutover").await;
    let lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
    let proxy_session_key = ProxySessionId(proxy_session_id.clone());
    let access_context = test_hls_access_context(proxy_session_key.clone(), lease_id.clone());
    let live_identity = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_key, super::super::current_time_millis())
        .await
        .and_then(|lease| lease.media_identity())
        .expect("live lease identity");
    let (release_sender, release_receiver) = tokio::sync::oneshot::channel();
    let app_state_for_completion = Arc::clone(&app_state);
    let access_context_for_completion = access_context.clone();
    let completion = tokio::spawn(async move {
        let _ = release_receiver.await;
        super::super::hls_live_lease_identity_is_current(
            &app_state_for_completion,
            &access_context_for_completion,
            live_identity,
        )
        .await
    });

    terminalize_existing_test_lease(&app_state, &proxy_session_id, &lease_id.0, 123).await;
    release_sender.send(()).expect("release delayed completion after cutover");

    assert!(!completion.await.expect("controlled completion task"));
}
