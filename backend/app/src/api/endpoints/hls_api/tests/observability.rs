use super::{
    create_active_hls_user_session_with, map_ready_segment_without_lease, media_uri_count, normal_manifest_body,
    test_addr_with_port, test_app_state_with_hls_proxy, test_fingerprint, test_fingerprint_with_addr,
    test_hls_access_context_with,
};
use crate::{
    api::model::{AppState, HlsAccessLeaseId, HlsAccessLeaseState, HlsProxyManager, HlsSessionHandle, ProxySessionId},
    auth::Fingerprint,
    model::StripConfig,
};
use axum::http::HeaderMap;
use shared::model::HlsStripMode;
use std::sync::Arc;

#[test]
fn repeated_speculative_strip_candidates_yield_one_committed_applied_diagnostic() {
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let body = normal_manifest_body("proxy-session");
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    let candidates = (0..8)
        .map(|_| {
            super::super::materialize_shared_hls_access_manifest(
                &body,
                &access_lease_id,
                HlsAccessLeaseState::Pending,
                &strip,
                super::super::HlsManifestWindowPolicy::ApplyLiveWindow,
                "normal",
                None,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(candidates.len(), 8);
    let candidate_count = candidates.len();
    let diagnostics = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            super::super::hls_initial_strip_publication_diagnostic(
                if index.saturating_add(1) == candidate_count {
                    super::super::HlsInitialStripPublicationStatus::Committed
                } else {
                    super::super::HlsInitialStripPublicationStatus::NotCommitted
                },
                HlsAccessLeaseState::Pending,
                candidate,
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(
        diagnostics,
        vec![super::super::HlsInitialStripPublicationDiagnostic::Applied {
            mode: "normal",
            strip_mode: "segments",
            configured: 3,
            effective: 3,
            visible_segments: 3,
        }]
    );
}

#[test]
fn committed_pending_strip_disabled_yields_one_skipped_diagnostic() {
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let materialized = super::super::materialize_shared_hls_access_manifest(
        &normal_manifest_body("proxy-session"),
        &access_lease_id,
        HlsAccessLeaseState::Pending,
        &StripConfig { mode: HlsStripMode::Segments, value: 0 },
        super::super::HlsManifestWindowPolicy::ApplyLiveWindow,
        "normal",
        None,
    );

    let diagnostics = [super::super::hls_initial_strip_publication_diagnostic(
        super::super::HlsInitialStripPublicationStatus::Committed,
        HlsAccessLeaseState::Pending,
        &materialized,
    )
    .expect("committed strip diagnostic")];

    assert_eq!(
        diagnostics,
        [super::super::HlsInitialStripPublicationDiagnostic::Skipped {
            mode: "normal",
            reason: crate::api::model::hls_cache::initial_strip::HlsInitialStripSkipReason::StripDisabled,
            visible_segments: 6,
        }]
    );
    assert_eq!(media_uri_count(&materialized.body), 6);
}

#[test]
fn committed_activated_manifest_yields_one_lease_state_skip_diagnostic() {
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let materialized = super::super::materialize_shared_hls_access_manifest(
        &normal_manifest_body("proxy-session"),
        &access_lease_id,
        HlsAccessLeaseState::Activated,
        &StripConfig { mode: HlsStripMode::Segments, value: 3 },
        super::super::HlsManifestWindowPolicy::ApplyLiveWindow,
        "normal",
        None,
    );

    let diagnostics = [super::super::hls_initial_strip_publication_diagnostic(
        super::super::HlsInitialStripPublicationStatus::Committed,
        HlsAccessLeaseState::Activated,
        &materialized,
    )
    .expect("committed strip diagnostic")];

    assert_eq!(
        diagnostics,
        [super::super::HlsInitialStripPublicationDiagnostic::SkippedForLeaseState {
            mode: "normal",
            reason: super::super::HlsInitialStripLeaseSkipReason::LeaseActivated,
        }]
    );
    assert_eq!(media_uri_count(&materialized.body), 6);
}

pub(in crate::api::endpoints::hls_api::tests) async fn register_hls_cache_stream_for_stats_test(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    session_token: &str,
    fingerprint: &Fingerprint,
    lease_id: &str,
) {
    create_active_hls_user_session_with(
        app_state,
        session_token,
        "origin-provider",
        "http://origin.example.com/live/12345.m3u8",
        fingerprint.addr,
    )
    .await;
    let context = test_hls_access_context_with(
        proxy_session_id.clone(),
        HlsAccessLeaseId(lease_id.to_string()),
        session_token,
        fingerprint.key.clone(),
    );
    super::super::ensure_hls_cache_stream_registered(app_state, fingerprint, &HeaderMap::new(), &context, session)
        .await
        .expect("HLS stream registers");
}

pub(in crate::api::endpoints::hls_api::tests) fn find_stream_by_session_token(
    streams: &[shared::model::StreamInfo],
    session_token: &str,
) -> shared::model::StreamInfo {
    streams
        .iter()
        .find(|stream| stream.session_token.as_deref() == Some(session_token))
        .unwrap_or_else(|| panic!("{session_token} stream should exist"))
        .clone()
}

#[tokio::test]
async fn hls_cache_stream_stats_mark_additional_viewers_as_joined_existing() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;
    let proxy_session_id = ProxySessionId(proxy_session_id);
    let shared_stream_id = super::super::hls_cache_shared_stream_id(&proxy_session_id);
    let session =
        app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.expect("session should exist");
    let first_fingerprint = test_fingerprint();
    let second_fingerprint = test_fingerprint_with_addr(test_addr_with_port(55124));
    register_hls_cache_stream_for_stats_test(
        &app_state,
        &session,
        &proxy_session_id,
        "hls-session-token",
        &first_fingerprint,
        "first-access-lease",
    )
    .await;

    let streams = app_state.active_users.active_streams().await;
    let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
    let first_meter_uid = first_stream.meter_uid;
    assert!(first_stream.channel.shared);
    assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
    assert_eq!(first_stream.channel.shared_joined_existing, Some(false));

    register_hls_cache_stream_for_stats_test(
        &app_state,
        &session,
        &proxy_session_id,
        "hls-second-session-token",
        &second_fingerprint,
        "second-access-lease",
    )
    .await;

    let streams = app_state.active_users.active_streams().await;
    let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
    let second_stream = find_stream_by_session_token(&streams, "hls-second-session-token");
    assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
    assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
    assert_eq!(second_stream.channel.shared_stream_id, Some(shared_stream_id));
    assert_eq!(second_stream.channel.shared_joined_existing, Some(true));

    register_hls_cache_stream_for_stats_test(
        &app_state,
        &session,
        &proxy_session_id,
        "hls-session-token",
        &first_fingerprint,
        "first-access-lease",
    )
    .await;

    let streams = app_state.active_users.active_streams().await;
    let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
    assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
    assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
    assert_eq!(first_stream.meter_uid, first_meter_uid);
}
