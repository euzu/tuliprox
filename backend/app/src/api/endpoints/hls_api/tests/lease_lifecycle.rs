use super::{
    access_lease_id_from_variant_uri, access_lease_session_token, create_bound_hls_test_session, enable_hls_cache,
    get_response, hls_proxy_uri, map_ready_segment, media_uri_count, normal_manifest_body, overlap_provider_input,
    proxy_session_id_from_variant_uri, response_body, single_variant_uri, store_normal_manifest_body, test_app_state,
    test_app_state_with_hls_proxy, test_app_state_with_inputs, test_fingerprint, transient_manifest_body,
    try_test_hls_cached_manifest_response,
};
use crate::{
    api::model::{
        ConnectionKind, HlsAccessAdmissionMode, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState,
        HlsAccessLeaseTiming, HlsAccessLeaseValidationError, HlsLifecycleEvent, HlsLifecycleEventKey,
        HlsOriginAccountBindingMode, HlsPlaybackFamilyKey, HlsProxyManager, HlsSessionKey, HlsSessionMode,
        ProxySessionId,
    },
    model::{ConfigInput, HlsCacheConfig, ProxyUserCredentials, StripConfig},
};
use axum::http::{header, StatusCode};
use shared::model::{HlsCacheConfigDto, HlsSegmentRepairMode, HlsStripMode, UserConnectionPermission};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn hls_access_lease_validity_uses_session_idle_timeout_not_cache_duration() {
    let hls_dto = HlsCacheConfigDto {
        cache_duration: shared::model::Secs::new(900),
        session_idle_timeout: shared::model::Secs::new(42),
        ..Default::default()
    };
    let hls_config = HlsCacheConfig::from(&hls_dto);
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));

    assert_eq!(super::super::hls_access_lease_ttl_ms(&app_state), 42_000);
}

#[tokio::test]
async fn hls_access_lease_active_window_uses_two_target_durations() {
    let app_state = test_app_state();
    let key = HlsSessionKey::new(1, "access-window-stream");
    let (session, _) = app_state
        .hls
        .proxy
        .get_or_create_session_with_outcome(key, b"secret", super::super::current_time_millis())
        .await;
    session.write().await.target_duration = Some(11);

    let timing = super::super::hls_access_lease_timing_for_session(&app_state, &session).await;

    assert_eq!(timing.active_window_ms, 22_000);
    assert_eq!(timing.valid_window_ms, super::super::hls_access_lease_ttl_ms(&app_state));
}

#[tokio::test]
async fn hls_lifecycle_active_timer_moves_access_lease_to_idle() {
    let app_state = test_app_state();
    let now_ms = super::super::current_time_millis();
    let lease_id = HlsAccessLeaseId("lifecycle-lease".to_string());
    let key = HlsSessionKey::new(1, "lifecycle-stream");
    let (session, _) = app_state.hls.proxy.get_or_create_session_with_outcome(key, b"secret", now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();

    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", "client"),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "lifecycle-stream".to_string(),
            123,
            now_ms,
            60_000,
        ))
        .await;
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 1, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    session.write().await.activity.active_access_lease_count = 1;

    app_state
        .hls
        .proxy
        .handle_lifecycle_event(
            &app_state.active_users,
            &app_state.active_provider,
            HlsLifecycleEvent {
                key: HlsLifecycleEventKey::AccessLeaseActive {
                    lease_id: lease_id.clone(),
                    proxy_session_id: proxy_session_id.clone(),
                },
                due_at_ms: now_ms.saturating_add(1),
            },
            now_ms.saturating_add(2),
        )
        .await;

    assert_eq!(
        app_state.hls.proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
        Some(HlsAccessLeaseState::Idle)
    );
    assert_eq!(session.read().await.activity.active_access_lease_count, 0);
}

#[tokio::test]
async fn hls_lifecycle_validity_timer_removes_expired_access_lease() {
    let mut hls_cache = HlsCacheConfigDto::default();
    hls_cache.segment_repair.max_level = HlsSegmentRepairMode::Low;
    let hls_config = HlsCacheConfig::from(&hls_cache);
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
    let now_ms = super::super::current_time_millis();
    let proxy_session_id = ProxySessionId("lifecycle-validity-proxy".to_string());
    let lease_id = HlsAccessLeaseId("lifecycle-validity-lease".to_string());
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", "client"),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "lifecycle-stream".to_string(),
            123,
            now_ms,
            1,
        ))
        .await;
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 1, valid_window_ms: 1 },
        )
        .await
        .is_activated());
    let repair_before = app_state.hls.proxy.segment_repair().stats().await;
    assert_eq!(repair_before.windows, 1);
    assert_eq!(repair_before.generations, 1);

    app_state
        .hls
        .proxy
        .handle_lifecycle_event(
            &app_state.active_users,
            &app_state.active_provider,
            HlsLifecycleEvent {
                key: HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: lease_id.clone(),
                    proxy_session_id: proxy_session_id.clone(),
                },
                due_at_ms: now_ms.saturating_add(1),
            },
            now_ms.saturating_add(2),
        )
        .await;

    assert_eq!(
        app_state.hls.proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
        None
    );
    let repair_after = app_state.hls.proxy.segment_repair().stats().await;
    assert_eq!(repair_after.windows, 0);
    assert_eq!(repair_after.generations, 0);
}

#[tokio::test]
async fn hls_lifecycle_validity_timer_removes_expired_pending_access_lease() {
    let app_state = test_app_state();
    let now_ms = super::super::current_time_millis();
    let key = HlsSessionKey::new(1, "pending-expiry-stream");
    let (session, _) = app_state.hls.proxy.get_or_create_session_with_outcome(key, b"secret", now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("pending-expiry-lease".to_string());
    let active_lease_id = HlsAccessLeaseId("active-soft-lease".to_string());
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", "client"),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "pending-expiry-stream".to_string(),
            123,
            now_ms,
            1,
        ))
        .await;
    app_state
        .hls
        .proxy
        .prepare_access_lease(
            HlsAccessLease::pending(
                active_lease_id.clone(),
                HlsPlaybackFamilyKey::new("soft-user", "client"),
                proxy_session_id.clone(),
                "soft-user".to_string(),
                "soft-session-token".to_string(),
                1,
                "pending-expiry-stream".to_string(),
                123,
                now_ms,
                60_000,
            )
            .with_origin_acquire_policy(ConnectionKind::Soft, 20),
        )
        .await;
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &active_lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());

    app_state
        .hls
        .proxy
        .handle_lifecycle_event(
            &app_state.active_users,
            &app_state.active_provider,
            HlsLifecycleEvent {
                key: HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: lease_id.clone(),
                    proxy_session_id: proxy_session_id.clone(),
                },
                due_at_ms: now_ms.saturating_add(1),
            },
            now_ms.saturating_add(2),
        )
        .await;

    assert_eq!(
        app_state.hls.proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
        None
    );
    let session = session.read().await;
    assert_eq!(session.activity.active_access_lease_count, 1);
    let effective_policy = session.effective_origin_acquire_policy_or_default();
    assert_eq!(effective_policy.connection_kind, ConnectionKind::Soft);
    assert_eq!(effective_policy.priority, 20);
}

#[tokio::test]
async fn hls_lifecycle_session_idle_timer_removes_idle_session() {
    let mut hls_dto = HlsCacheConfigDto { session_idle_timeout: shared::model::Secs::new(1), ..Default::default() };
    hls_dto.segment_repair.max_level = HlsSegmentRepairMode::Low;
    let hls_config = HlsCacheConfig::from(&hls_dto);
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
    let now_ms = super::super::current_time_millis();
    let key = HlsSessionKey::new(1, "expired-session");
    let (session, _) =
        app_state.hls.proxy.get_or_create_session_with_outcome(key, b"secret", now_ms.saturating_sub(2_000)).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("session-idle-cleanup-lease".to_string());
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", "client"),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "expired-session".to_string(),
            123,
            now_ms,
            60_000,
        ))
        .await;
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    assert_eq!(app_state.hls.proxy.access_leases().read().await.len(), 1);
    assert_eq!(app_state.hls.proxy.segment_repair().stats().await.windows, 1);

    app_state
        .hls
        .proxy
        .handle_lifecycle_event(
            &app_state.active_users,
            &app_state.active_provider,
            HlsLifecycleEvent {
                key: HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                due_at_ms: now_ms.saturating_sub(1_000),
            },
            now_ms,
        )
        .await;

    assert!(app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.is_none());
    assert_eq!(app_state.hls.proxy.access_leases().read().await.len(), 0);
    let repair_after = app_state.hls.proxy.segment_repair().stats().await;
    assert_eq!(repair_after.windows, 0);
    assert_eq!(repair_after.generations, 0);
}

#[tokio::test]
async fn hls_gc_session_removal_cleans_access_leases_and_repair_state() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let mut hls_dto = HlsCacheConfigDto {
        cache_path: Some(temp_dir.path().to_string_lossy().into_owned()),
        session_idle_timeout: shared::model::Secs::new(1),
        ..Default::default()
    };
    hls_dto.segment_repair.max_level = HlsSegmentRepairMode::Low;
    let hls_config = HlsCacheConfig::from(&hls_dto);
    let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
    let now_ms = super::super::current_time_millis();
    let key = HlsSessionKey::new(1, "gc-cleanup-session");
    let (session, _) =
        app_state.hls.proxy.get_or_create_session_with_outcome(key, b"secret", now_ms.saturating_sub(2_000)).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("gc-cleanup-lease".to_string());
    app_state
        .hls
        .proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("hls-user", "client"),
            proxy_session_id.clone(),
            "hls-user".to_string(),
            "hls-session-token".to_string(),
            1,
            "gc-cleanup-session".to_string(),
            123,
            now_ms,
            60_000,
        ))
        .await;
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    assert_eq!(app_state.hls.proxy.access_leases().read().await.len(), 1);
    assert_eq!(app_state.hls.proxy.segment_repair().stats().await.windows, 1);

    let report = app_state.hls.proxy.run_garbage_collection_once(now_ms).await.expect("gc should run");

    assert_eq!(report.sessions_deleted, 1);
    assert!(app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.is_none());
    assert_eq!(app_state.hls.proxy.access_leases().read().await.len(), 0);
    let repair_after = app_state.hls.proxy.segment_repair().stats().await;
    assert_eq!(repair_after.windows, 0);
    assert_eq!(repair_after.generations, 0);
}

#[tokio::test]
async fn hls_access_lease_sync_zero_to_zero_is_idempotent() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let old_generation = session.read().await.activity.origin_work_generation;

    app_state
        .hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            &proxy_session_id,
            3_000,
        )
        .await;

    let session = session.read().await;
    assert_eq!(session.activity.active_access_lease_count, 0);
    assert!(matches!(
        session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
        HlsOriginAccountBindingMode::Active
    ));
    assert_eq!(session.activity.origin_work_generation, old_generation);
}

#[tokio::test]
async fn hls_cache_entry_denies_access_lease_for_grace_without_slot_and_exhausted() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let user = app_state.app_config.get_user_credentials("hls-user").expect("test user should exist");
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");

    for (connection_permission, connection_kind) in [
        (UserConnectionPermission::GracePeriod, None),
        (UserConnectionPermission::Exhausted, Some(ConnectionKind::Normal)),
    ] {
        let response = super::super::create_hls_cache_entry_master_playlist_response(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            None,
            None,
            request_url,
            &input,
            connection_permission,
            connection_kind,
            Some("/iptv"),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let variant_uri = single_variant_uri(response).await;
        let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&variant_uri).to_string());
        let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&variant_uri).to_string());
        let now_ms = super::super::current_time_millis();
        let snapshot = app_state
            .hls
            .proxy
            .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms)
            .await
            .expect("denied lease should stay available for response rendering");
        assert_eq!(snapshot.state, HlsAccessLeaseState::Denied);
        assert!(app_state
            .hls
            .proxy
            .access_lease_session_snapshot(&proxy_session_id, now_ms)
            .await
            .effective_origin_policy
            .is_none());

        let err = super::super::validate_hls_proxy_access_context(
            &app_state,
            &test_fingerprint(),
            &proxy_session_id,
            &access_lease_id.0,
            now_ms,
            HlsAccessAdmissionMode::ManifestPrepare,
        )
        .await
        .expect_err("denied lease must surface as admission denied");
        assert!(matches!(err, HlsAccessLeaseValidationError::AdmissionDenied { runtime_tail: None, .. }));
    }
}

#[tokio::test]
async fn hls_cache_entry_does_not_reuse_activated_access_lease() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
    let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
    let origin_source = super::super::build_hls_origin_source(&input, "12345");

    let first_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source.clone(),
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let first_variant_uri = single_variant_uri(first_response).await;
    let first_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&first_variant_uri).to_string());
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&first_variant_uri).to_string());
    let now_ms = super::super::current_time_millis();
    assert!(app_state
        .hls
        .proxy
        .activate_access_lease(
            &first_lease_id,
            &proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming {
                active_window_ms: 5_000,
                valid_window_ms: super::super::hls_access_lease_ttl_ms(&app_state),
            },
        )
        .await
        .is_activated());

    let second_response = super::super::create_hls_cache_entry_master_playlist_response(
        &app_state,
        &test_fingerprint(),
        &user,
        origin_source,
        12345,
        None,
        None,
        None,
        request_url,
        &input,
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        None,
    )
    .await;
    let second_variant_uri = single_variant_uri(second_response).await;
    let second_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&second_variant_uri).to_string());

    assert_ne!(first_lease_id, second_lease_id);
    let first_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_lease_id).await;
    let second_session_token = access_lease_session_token(&app_state, &proxy_session_id, &second_lease_id).await;
    assert_ne!(first_session_token, second_session_token);
    assert!(
        app_state
            .hls
            .proxy
            .touch_access_lease(
                &first_lease_id,
                super::super::current_time_millis(),
                HlsAccessLeaseTiming {
                    active_window_ms: 5_000,
                    valid_window_ms: super::super::hls_access_lease_ttl_ms(&app_state),
                },
            )
            .await
    );
}

#[tokio::test]
async fn hls_cache_idle_normal_manifest_applies_initial_strip_without_mutating_shared_body() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        let proxy_session_id = session.proxy_session_id.0.clone();
        let rendered_at_ms = super::super::current_time_millis();
        store_normal_manifest_body(&mut session, normal_manifest_body(&proxy_session_id), rendered_at_ms);
        session.mark_authorized_media_access(rendered_at_ms);
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    let stored_before = session.read().await.last_rendered_manifest.as_ref().expect("normal manifest").body.clone();

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Idle,
        &strip,
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("idle normal manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
    assert!(body.contains("/000000.ts"));
    assert!(body.contains("/000001.ts"));
    assert!(body.contains("/000002.ts"));
    assert!(!body.contains("/000003.ts"));
    assert!(body.contains(&access_lease_id.0));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    assert_eq!(session.read().await.last_rendered_manifest.as_ref().expect("stored manifest").body, stored_before);
    assert_eq!(media_uri_count(&stored_before), 6);
}

#[tokio::test]
async fn hls_cache_idle_transient_manifest_applies_initial_strip_without_mutating_shared_body() {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let session = app_state
        .hls
        .proxy
        .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
        .await;
    let _proxy_session_id = {
        let mut session = session.write().await;
        session.mode =
            HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
        let proxy_session_id = session.proxy_session_id.0.clone();
        session.transient.replace_manifest_with_semantics(transient_manifest_body(&proxy_session_id), 100, None);
        session.mark_authorized_media_access(super::super::current_time_millis());
        proxy_session_id
    };
    let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
    let strip = StripConfig { mode: HlsStripMode::Segments, value: 3 };
    let stored_before = session.read().await.transient.last_manifest_body.clone().expect("transient manifest body");

    let response = try_test_hls_cached_manifest_response(
        &app_state,
        &session,
        &access_lease_id,
        HlsAccessLeaseState::Idle,
        &strip,
        None,
        super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
    )
    .await
    .expect("idle transient manifest response");
    let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

    assert_eq!(media_uri_count(&body), 3);
    assert!(body.contains(&access_lease_id.0));
    assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    assert_eq!(session.read().await.transient.last_manifest_body.as_ref().expect("stored manifest"), &stored_before);
    assert_eq!(media_uri_count(&stored_before), 6);
}

#[tokio::test]
async fn ready_hls_proxy_segment_marked_for_gc_returns_not_found_without_redirect() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
    let app_state = test_app_state_with_hls_proxy(hls_proxy);
    let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("session should exist");
    session.write().await.mark_for_gc_removal();

    let response = get_response(app_state, &uri, None).await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key(header::LOCATION));
}
