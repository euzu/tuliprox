use super::{
    create_active_hls_user_session, enable_channel_unavailable_custom_response, enable_hls_cache, get_response,
    map_ready_segment, mark_hls_user_session_exhausted, normal_manifest, prepare_pending_test_hls_access_lease,
    response_body, runtime_policy_endpoint_fixture, spawn_test_status_origin,
    terminal_tail::{
        assert_conflicted_standalone_fallback, assert_publication_late_terminal_result, assert_terminal_plan_unchanged,
        prepare_other_live_lease, prepare_publication_late_terminal_pressure, publication_late_fixture,
        refresh_publication_late_fixture, terminal_generation_for_lease,
    },
    terminalize_existing_test_lease, test_app_state_with_hls_proxy, test_app_state_with_inputs, test_fingerprint,
    test_hls_access_context, HlsCanonicalOwnerRegistrationKind,
};
use crate::{
    api::model::{
        build_proxy_session_id, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsAccessLeaseTiming,
        HlsAvailabilityReevaluationFinishReason, HlsAvailabilityReevaluationMode,
        HlsAvailabilityReevaluationRegistration, HlsLeasePlaybackMode, HlsManifestCommitRequirement,
        HlsOriginPathCondition, HlsPlaybackFamilyKey, HlsProxyManager, HlsRuntimeCustomTailReason,
        HlsSessionStoreOutcome, HlsTerminalSegmentPath, ProxySessionId,
    },
    model::{ConfigInput, StripConfig},
};
use axum::http::{header, HeaderMap, StatusCode};
use shared::model::{HlsStripMode, InputType};
use std::sync::Arc;

#[tokio::test]
async fn completed_owner_resolves_new_lease_standalone_without_reusing_old_terminal() {
    let temp_dir = tempfile::tempdir().expect("owner handoff cache tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    enable_channel_unavailable_custom_response(&app_state);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"old-terminal-base").await;
    let old_lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &old_lease_id.0, 123).await;
    let proxy_session_id = ProxySessionId(proxy_session_id);
    let now_ms = super::super::current_time_millis();
    let old_generation = terminal_generation_for_lease(&app_state, &proxy_session_id, &old_lease_id).await;
    let new_lease_id = HlsAccessLeaseId("new-owner-handoff-lease".to_string());
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            new_lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "new-owner-handoff-session".to_string(),
            1,
            "12345".to_string(),
            12345,
            now_ms,
            60_000,
        ))
        .await;
    let session =
        app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.expect("shared session");
    let owner_key =
        app_state.hls.proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await.expect("owner key");
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let task_owner_key = owner_key.clone();
    let coordinator = app_state.hls.proxy.availability_reevaluations();
    assert_eq!(
        coordinator.register(
            owner_key,
            HlsAvailabilityReevaluationMode::RecoveryPressure,
            move |ownership| async move {
                task_release.notified().await;
                let _ = ownership.finish_cycle(&task_owner_key, HlsAvailabilityReevaluationFinishReason::Evaluated);
            },
        ),
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
    let safe_session = {
        let session = session.read().await;
        super::super::safe_session_key(&session.key)
    };
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 0 };
    let mut response = Box::pin(super::super::join_hls_canonical_manifest_owner(
        super::super::HlsCanonicalOwnerHandoffContext {
            app_state: &app_state,
            proxy_session_id: &proxy_session_id,
            access_lease_id: &new_lease_id,
            expected_lease_issued_at_ms: Some(now_ms),
            strip: &strip,
            server_path: None,
            manifest_commit_requirement: HlsManifestCommitRequirement::CommittedManifestAllowed,
            manifest_boundary_rendered_at_ms: 0,
            bandwidth_learning: super::super::HlsRuntimeBandwidthLearningContext::Disabled,
            request_deadline_ms: now_ms.saturating_add(60_000),
            safe_session,
        },
        HlsCanonicalOwnerRegistrationKind::Scheduled,
    ));
    assert!(matches!(futures::poll!(response.as_mut()), std::task::Poll::Pending));

    release.notify_one();
    let response = response.await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    assert!(!response.headers().contains_key(header::RETRY_AFTER));
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("standalone manifest utf8");
    assert!(body.contains("#EXT-X-ENDLIST"));
    assert!(!body.contains("/hls/shared/live/"));
    assert_eq!(
        terminal_generation_for_lease(&app_state, &proxy_session_id, &old_lease_id).await,
        old_generation,
        "new lease fallback cannot reactivate old terminal lease"
    );
}

#[tokio::test]
async fn cold_user_denial_uses_standalone_finite_response() {
    let fixture = runtime_policy_endpoint_fixture(false).await;
    mark_hls_user_session_exhausted(&fixture.app_state).await;

    let denial = get_response(Arc::clone(&fixture.app_state), &fixture.live_segment_uri, None).await;
    assert_eq!(denial.status(), StatusCode::FORBIDDEN);
    let denied = fixture
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            super::super::current_time_millis(),
        )
        .await
        .expect("cold denied lease");
    assert_eq!(denied.state, HlsAccessLeaseState::Denied);
    assert_eq!(denied.playback_mode, HlsLeasePlaybackMode::Ended);
    assert!(denied.runtime_policy_revocation.is_none());
    assert_eq!(denied.runtime_policy_denial_reason(), Some(HlsRuntimeCustomTailReason::UserConnectionsExhausted));

    let response = get_response(Arc::clone(&fixture.app_state), &fixture.manifest_uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("standalone policy manifest utf8");
    assert!(body.contains(&format!("/cvs/hls/{}/", fixture.lease_id.0)));
    assert!(!body.contains("/hls-user/"));
    assert!(!body.contains("/hls-pass/"));
    assert!(!body.contains("/user_connections_exhausted/"));
    assert!(
        !body.contains(&format!("/hls/shared/live/{}/{}/terminal/", fixture.proxy_session_id.0, fixture.lease_id.0))
    );
    assert!(body.ends_with("#EXT-X-ENDLIST\n"));
}

#[tokio::test]
async fn reused_conflicted_session_standalone_fallback_preserves_terminal_lease_during_recovery() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300)));
    enable_channel_unavailable_custom_response(&app_state);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"original-live-tail").await;
    let terminal_lease_id = format!("test-access-lease-{proxy_session_id}");
    terminalize_existing_test_lease(&app_state, &proxy_session_id, &terminal_lease_id, 123).await;
    let proxy_session_key = ProxySessionId(proxy_session_id.clone());
    let now_ms = super::super::current_time_millis();
    let terminal_before = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&HlsAccessLeaseId(terminal_lease_id.clone()), &proxy_session_key, now_ms)
        .await
        .expect("terminal lease exists before the other lease");
    let HlsLeasePlaybackMode::TerminalTail(terminal_plan_before) = terminal_before.playback_mode else {
        panic!("original lease is terminal");
    };
    let terminal_path = HlsTerminalSegmentPath { generation: terminal_plan_before.generation, index: 0 };
    let terminal_bytes_before =
        terminal_plan_before.segment_bytes(terminal_path).expect("terminal segment zero is immutable");
    let other_lease_id = prepare_other_live_lease(&app_state, &proxy_session_key, now_ms).await;
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_key)
        .await
        .expect("shared session exists");
    session.write().await.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;

    assert_conflicted_standalone_fallback(&app_state, &session, &proxy_session_key, &other_lease_id).await;

    let terminal_after_fallback = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &HlsAccessLeaseId(terminal_lease_id.clone()),
            &proxy_session_key,
            now_ms.saturating_add(1),
        )
        .await
        .expect("terminal lease remains stored after standalone fallback");
    assert_terminal_plan_unchanged(
        &terminal_after_fallback.playback_mode,
        terminal_plan_before.generation,
        terminal_path,
        &terminal_bytes_before,
    );

    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &other_lease_id,
            &proxy_session_key,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 5_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());

    let recovered_manifest =
        normal_manifest("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:124\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n124.ts\n");
    {
        let mut session = session.write().await;
        session.apply_origin_manifest(&recovered_manifest).expect("recovery manifest commits");
        session.origin_control.record_media_progress(now_ms.saturating_add(1), 4_000);
    }

    let terminal = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &HlsAccessLeaseId(terminal_lease_id),
            &proxy_session_key,
            now_ms.saturating_add(2),
        )
        .await
        .expect("terminal lease remains stored");
    let other = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&other_lease_id, &proxy_session_key, now_ms.saturating_add(2))
        .await
        .expect("other live lease remains stored");
    assert_terminal_plan_unchanged(
        &terminal.playback_mode,
        terminal_plan_before.generation,
        terminal_path,
        &terminal_bytes_before,
    );
    assert_eq!(other.playback_mode, HlsLeasePlaybackMode::Live);
}

#[tokio::test]
async fn hls_cache_manifest_unpublished_lease_uses_same_finite_fallback_for_created_and_reused_session() {
    let mut rendered_bodies = Vec::new();
    for expected_outcome in [HlsSessionStoreOutcome::Created, HlsSessionStoreOutcome::Reused] {
        let input_name = Arc::<str>::from("test-input");
        let origin = spawn_test_status_origin(StatusCode::NOT_FOUND, b"missing").await;
        let input = ConfigInput {
            id: 1,
            name: Arc::clone(&input_name),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        enable_channel_unavailable_custom_response(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        if expected_outcome == HlsSessionStoreOutcome::Reused {
            let (session, outcome) = app_state
                .hls
                .proxy
                .get_or_create_session_with_source_and_outcome(
                    session_key,
                    origin_source.clone(),
                    &app_state.get_encrypt_secret(),
                    super::super::current_time_millis(),
                )
                .await;
            assert_eq!(outcome, HlsSessionStoreOutcome::Created);
            session.write().await.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
        }
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let response = super::super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::super::HlsCacheManifestOrigin {
                raw_request_url: request_url.as_str(),
                session_entry_url: super::super::HlsOriginEntryUrl::direct_http(request_url.as_str()),
                input: &input,
                origin_source,
            },
            HeaderMap::new(),
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
            super::super::HlsManifestRefreshOrdering::Background,
        )
        .await
        .expect("hls cache should handle valid live hls entrypoint");

        assert_eq!(response.status(), StatusCode::OK, "session outcome: {expected_outcome:?}");
        assert!(response.headers().get(header::RETRY_AFTER).is_none(), "custom response must not expose retry-after");
        assert!(!response.headers().contains_key(header::LOCATION));
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");
        assert!(body.contains("#EXT-X-ENDLIST"));
        assert!(!body.contains("/hls/shared/live/"), "standalone response must not expose an unready normal URI");
        rendered_bodies.push(body);

        let snapshot = app_state
            .hls
            .proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
            .await
            .expect("lease remains available for strict cold-start handling");
        assert_eq!(snapshot.playback_mode, HlsLeasePlaybackMode::Live);
        assert!(snapshot.last_manifest_snapshot.is_none());
    }
    assert_eq!(rendered_bodies[0], rendered_bodies[1]);
}

#[tokio::test]
async fn publication_late_live_manifest_request_refreshes_before_terminal_evaluation() {
    let fixture = publication_late_fixture().await;
    let lease = refresh_publication_late_fixture(&fixture).await;
    let base_manifest = lease.last_manifest_snapshot.as_ref().expect("live manifest snapshot");
    prepare_publication_late_terminal_pressure(&fixture, base_manifest).await;
    assert_publication_late_terminal_result(&fixture).await;
}
