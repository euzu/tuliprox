use super::{
    get_response, hls_manifest_terminal_preflight, hls_proxy_uri, hls_temporary_resource_unavailable_response,
    hls_terminal_endpoint_action, hls_terminal_failed_closed_response, map_ready_segment, test_app_state,
    test_app_state_with_hls_proxy, HlsManifestTerminalPreflight, HlsTerminalEndpointAction,
};
use crate::{
    api::model::{
        HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot,
        HlsLeasePlaybackMode, HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsMediaContainer,
        HlsPlaybackFamilyKey, HlsProxyManager, HlsSessionKey, HlsTerminalFailedClosedReason, HlsTerminalResolution,
        SegmentCacheStatus, SegmentFetchPriority,
    },
    processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
};
use axum::http::{header, StatusCode};
use std::sync::Arc;

#[test]
fn hls_terminal_commit_endpoint_resolution_mapping_is_exhaustive() {
    assert_eq!(hls_terminal_endpoint_action(HlsTerminalResolution::LiveAllowed), HlsTerminalEndpointAction::ServeLive);
    assert_eq!(
        hls_terminal_endpoint_action(HlsTerminalResolution::Committed),
        HlsTerminalEndpointAction::ReloadTerminal
    );
    assert_eq!(hls_terminal_endpoint_action(HlsTerminalResolution::Reevaluate), HlsTerminalEndpointAction::Reevaluate);
    assert_eq!(
        hls_terminal_endpoint_action(HlsTerminalResolution::Pending { retry_after_ms: 250 }),
        HlsTerminalEndpointAction::RetryAfter { retry_after_ms: 250 }
    );

    for reason in [
        HlsTerminalFailedClosedReason::LeaseStateUnavailable,
        HlsTerminalFailedClosedReason::BundleNotReadyWithoutOwner,
        HlsTerminalFailedClosedReason::BundleIncompatible,
        HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed,
        HlsTerminalFailedClosedReason::RetryCapacityExceeded,
        HlsTerminalFailedClosedReason::RetryAttemptsExhausted,
        HlsTerminalFailedClosedReason::RuntimeUnavailable,
    ] {
        assert_eq!(
            hls_terminal_endpoint_action(HlsTerminalResolution::FailedClosed { reason }),
            HlsTerminalEndpointAction::FailClosed { reason }
        );
    }
}

#[tokio::test]
async fn hls_manifest_terminal_preflight_distinguishes_bootstrap_refresh_and_invalid_missing_snapshot() {
    let app_state = test_app_state();
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "preflight-stream"), &app_state.get_encrypt_secret(), 1_000)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let mut lease = HlsAccessLease::pending(
        HlsAccessLeaseId("preflight-lease".to_string()),
        HlsPlaybackFamilyKey::new("hls-user", "preflight-client"),
        proxy_session_id,
        "hls-user".to_string(),
        "preflight-user-session".to_string(),
        1,
        "preflight-stream".to_string(),
        12345,
        1_000,
        120_000,
    );

    assert_eq!(
        hls_manifest_terminal_preflight(&session, &lease, 2_000).await,
        HlsManifestTerminalPreflight::BootstrapPendingLease
    );

    lease.state = HlsAccessLeaseState::Activated;
    assert_eq!(
        hls_manifest_terminal_preflight(&session, &lease, 2_000).await,
        HlsManifestTerminalPreflight::FailClosed { reason: HlsTerminalFailedClosedReason::LeaseStateUnavailable }
    );

    lease.last_manifest_snapshot = Some(HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1_500),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 1_500,
        first_proxy_seq: 0,
        last_proxy_seq: 0,
        visible_segments: Arc::from([]),
        discontinuity_sequence: 0,
        target_duration_ms: 4_000,
        playlist_duration_ms: 0,
        last_visible_media_end_ms: 0,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    });
    {
        let mut session = session.write().await;
        session.origin_control.target_duration_snapshot_ms = Some(4_000);
        session.origin_control.last_media_progress_at_ms = Some(2_000);
    }
    assert_eq!(
        hls_manifest_terminal_preflight(&session, &lease, 8_000).await,
        HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation
    );
    session.write().await.origin_control.last_media_progress_at_ms = Some(7_999);
    assert_eq!(
        hls_manifest_terminal_preflight(&session, &lease, 8_000).await,
        HlsManifestTerminalPreflight::EvaluateTerminal
    );

    lease.playback_mode = HlsLeasePlaybackMode::Ended;
    assert_eq!(
        hls_manifest_terminal_preflight(&session, &lease, 8_000).await,
        HlsManifestTerminalPreflight::ServeCommittedPlayback
    );
}

#[tokio::test]
async fn hls_manifest_terminal_preflight_keeps_capacity_recovery_out_of_sync_terminal_wait() {
    let app_state = test_app_state();
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "capacity-preflight"), &app_state.get_encrypt_secret(), 1_000)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    {
        let mut session = session.write().await;
        let OriginManifestParseOutcome::Normal(manifest) = parse_origin_media_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n\
                 #EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
            "http://origin.example/live/index.m3u8",
        ) else {
            panic!("capacity preflight manifest parses");
        };
        session.apply_origin_manifest(&manifest).expect("capacity preflight timeline applies");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1_000 };
        }
        session.segments.get_mut(&1).expect("deferred segment").status =
            SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Prefetch, deferred_at_ms: 2_000 };
        session.origin_control.target_duration_snapshot_ms = Some(8_000);
        session.origin_control.last_media_progress_at_ms = Some(1_000);
    }
    let mut lease = HlsAccessLease::pending(
        HlsAccessLeaseId("capacity-preflight-lease".to_string()),
        HlsPlaybackFamilyKey::new("capacity-user", "capacity-client"),
        proxy_session_id,
        "capacity-user".to_string(),
        "capacity-user-session".to_string(),
        1,
        "capacity-preflight".to_string(),
        1,
        1_000,
        60_000,
    );
    lease.state = HlsAccessLeaseState::Activated;
    lease.last_manifest_snapshot = Some(HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 1_000,
        first_proxy_seq: 0,
        last_proxy_seq: 0,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: 0,
            duration_ms: 4_000,
            uri: "000000.ts".to_string().into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: 8_000,
        playlist_duration_ms: 4_000,
        last_visible_media_end_ms: 4_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    });

    assert_eq!(
        hls_manifest_terminal_preflight(&session, &lease, 20_000).await,
        HlsManifestTerminalPreflight::EvaluateTerminal,
    );

    session.write().await.segments.get_mut(&1).expect("recovered segment").status =
        SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 20_001 };
    assert_eq!(
        hls_manifest_terminal_preflight(&session, &lease, 20_001).await,
        HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation,
    );
}

#[test]
fn hls_terminal_commit_endpoint_pending_and_failed_closed_headers_are_distinct() {
    let pending = hls_temporary_resource_unavailable_response(250);
    assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(pending.headers().get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()), Some("1"));
    assert!(pending.headers().get(header::LOCATION).is_none());

    let failed_closed = hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::RetryAttemptsExhausted);
    assert_eq!(failed_closed.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(failed_closed.headers().get(header::RETRY_AFTER).is_none());
    assert!(failed_closed.headers().get(header::LOCATION).is_none());
}

#[tokio::test]
async fn invalid_normal_segment_uri_never_redirects_or_serves_terminal_media() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "broken.ts").await;

    let response = get_response(app_state, &uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key(header::LOCATION));
}
