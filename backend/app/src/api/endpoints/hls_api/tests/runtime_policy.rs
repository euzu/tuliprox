use super::{
    get_response, get_status, mark_hls_user_session_exhausted, response_body, runtime_policy_endpoint_fixture,
    wait_for_runtime_policy_terminal_plan, RuntimePolicyEndpointFixture,
};
use crate::api::model::{HlsAccessLeaseState, HlsAccessLeaseTouch, HlsLeasePlaybackMode, HlsRuntimeCustomTailReason};
use axum::http::{header, StatusCode};
use std::sync::Arc;

pub(in crate::api::endpoints::hls_api::tests) async fn assert_runtime_policy_terminal_segments(
    fixture: &RuntimePolicyEndpointFixture,
    terminal_prefix: &str,
    segment_count: u16,
) {
    for index in [0, 1, segment_count.saturating_sub(1)] {
        let uri = format!("{terminal_prefix}{index}.ts");
        let response = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(response.status(), StatusCode::OK, "terminal segment {index}");
        assert!(!response_body(response).await.is_empty());
    }
}

#[tokio::test]
async fn resource_access_denial_commits_and_preserves_user_exhausted_tail() {
    let fixture = runtime_policy_endpoint_fixture(true).await;
    let session = fixture
        .app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&fixture.proxy_session_id)
        .await
        .expect("runtime policy session");
    let origin_refresh_before = session.read().await.origin_refresh.clone();
    mark_hls_user_session_exhausted(&fixture.app_state).await;

    let denied_live = get_response(Arc::clone(&fixture.app_state), &fixture.live_segment_uri, None).await;
    assert_eq!(denied_live.status(), StatusCode::FORBIDDEN);
    let revoking = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("revoking lease snapshot");
    assert_eq!(revoking.state, HlsAccessLeaseState::PolicyRevoking);
    assert_eq!(revoking.playback_mode, HlsLeasePlaybackMode::Live);
    assert_eq!(
        revoking.runtime_policy_revocation.as_ref().map(|revocation| revocation.reason),
        Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted)
    );

    let pending_manifest = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
    assert_eq!(pending_manifest.status(), StatusCode::SERVICE_UNAVAILABLE);
    let committed_plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
    let manifest_response = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
    assert_eq!(manifest_response.status(), StatusCode::OK);
    assert!(!manifest_response.headers().contains_key(header::LOCATION));
    let manifest = String::from_utf8(response_body(manifest_response).await.to_vec())
        .expect("runtime policy terminal manifest utf8");
    assert!(manifest.ends_with("#EXT-X-ENDLIST\n"));
    assert!(!manifest.contains("/cvs/hls/"));

    let committed = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("committed runtime policy lease");
    assert_eq!(committed.state, HlsAccessLeaseState::Denied);
    let HlsLeasePlaybackMode::TerminalTail(plan) = committed.playback_mode else {
        panic!("resource denial must commit a finite terminal plan");
    };
    assert_eq!(plan.generation, committed_plan.generation);
    assert_eq!(plan.reason, HlsRuntimeCustomTailReason::UserConnectionsExhausted);
    assert!(plan.segment_count >= 2);
    let terminal_prefix = format!(
        "/hls/shared/live/{}/{}/terminal/{}/",
        fixture.proxy_session_id.0, fixture.lease_id.0, plan.generation.0
    );
    assert_eq!(manifest.matches(&terminal_prefix).count(), usize::from(plan.segment_count));

    assert_runtime_policy_terminal_segments(&fixture, &terminal_prefix, plan.segment_count).await;
    let replay = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(
        String::from_utf8(response_body(replay).await.to_vec()).expect("replayed runtime policy manifest utf8"),
        manifest
    );
    assert_eq!(get_status(Arc::clone(&fixture.app_state), &fixture.live_segment_uri).await, StatusCode::FORBIDDEN);
    let retained = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("retained runtime policy plan");
    assert!(matches!(
        retained.playback_mode,
        HlsLeasePlaybackMode::TerminalTail(ref current)
            if current.generation == plan.generation
                && current.reason == HlsRuntimeCustomTailReason::UserConnectionsExhausted
    ));
    assert_eq!(session.read().await.origin_refresh, origin_refresh_before);
}

#[tokio::test]
async fn manifest_touch_denied_replays_policy_tail_instead_of_standalone_clock() {
    let fixture = runtime_policy_endpoint_fixture(true).await;
    let _ = fixture
        .app_state
        .hls
        .proxy
        .begin_runtime_policy_revocation(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
            super::super::current_time_millis(),
        )
        .await;
    assert_eq!(
        fixture
            .app_state
            .hls
            .proxy
            .touch_manifest_access_lease(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                super::super::current_time_millis(),
                None,
                None,
                60_000,
            )
            .await,
        HlsAccessLeaseTouch::Denied
    );

    let pending = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
    assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE);
    let committed_plan = wait_for_runtime_policy_terminal_plan(&fixture).await;
    let response = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("policy touch manifest utf8");
    let lease = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("touch-denied lease");
    let HlsLeasePlaybackMode::TerminalTail(plan) = lease.playback_mode else {
        panic!("touch denial must retain a lease-bound terminal plan");
    };
    assert_eq!(plan.generation, committed_plan.generation);
    assert!(body.contains(&format!(
        "/hls/shared/live/{}/{}/terminal/{}/0.ts",
        fixture.proxy_session_id.0, fixture.lease_id.0, plan.generation.0
    )));
    assert!(!body.contains("/cvs/hls/"));
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
}
