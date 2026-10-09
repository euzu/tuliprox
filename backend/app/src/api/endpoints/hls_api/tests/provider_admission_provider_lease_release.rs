use super::{
    activate_test_hls_access_lease, create_bound_hls_test_session, get_response, hls_proxy_uri, map_transient_resource,
    overlap_provider_input, provider_admission::register_test_hls_stream_for_lease_release, response_body,
    single_hls_provider_input, spawn_test_transient_origin, test_addr_with_port, test_app_state_with_inputs,
    test_segment_entry, wait_for_provider_connection_count,
};
use crate::api::model::{
    ConnectionKind, HlsOriginAccountBinding, HlsOriginAccountBindingMode, MapCacheStatus, MapEntry, OriginMapKey,
    ProxyMapId, ProxySessionId, SegmentCacheStatus, SegmentFetchPriority,
};
use axum::http::StatusCode;
use std::sync::Arc;

#[tokio::test]
async fn hls_access_lease_idle_releases_user_but_keeps_origin_binding_and_queues() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let binding = session.read().await.origin_account_binding.clone().expect("binding exists");
    let account_name = Arc::from("account-a");
    app_state.active_provider.refresh_provider_reservation(&account_name, &binding.session_owner, 60);
    app_state.active_provider.confirm_playback_activity(&binding.session_owner);
    assert!(app_state.active_provider.is_provider_reserved_for_other_session(&account_name, Some("other-owner")));
    activate_test_hls_access_lease(&app_state, &proxy_session_id, "detach-lease", 1_000, 1_000).await;
    register_test_hls_stream_for_lease_release(&app_state, &session, &proxy_session_id, input.name.as_ref()).await;
    assert_eq!(app_state.active_users.active_streams().await.len(), 1);
    app_state
        .hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            &proxy_session_id,
            1_000,
        )
        .await;
    {
        let mut session = session.write().await;
        assert_eq!(session.activity.active_access_lease_count, 1);
        session.segments.insert(1, test_segment_entry(&proxy_session_id, 1, SegmentCacheStatus::Discovered));
        session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Prefetch, 1_100);
        session.segments.insert(
            2,
            test_segment_entry(
                &proxy_session_id,
                2,
                SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 1_000 },
            ),
        );
        let map_id = ProxyMapId(1);
        let mut map = MapEntry::new(
            &proxy_session_id,
            map_id,
            OriginMapKey {
                origin_epoch: 0,
                resolved_origin_uri: "http://origin.example.com/init.mp4".to_string(),
                byte_range: None,
            },
            "mp4".to_string(),
        );
        map.status = MapCacheStatus::Queued { queued_at_ms: 1_100 };
        session.maps.insert(map_id, map);
    }
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

    {
        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 0);
        let binding = session.origin_account_binding.as_ref().expect("binding is retained");
        assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
        assert_eq!(session.activity.origin_work_generation, old_generation);
        assert!(!session.segment_prefetch_queue.is_empty());
        assert!(matches!(
            session.segments.get(&1).expect("queued segment remains").status,
            SegmentCacheStatus::Queued { .. }
        ));
        assert!(matches!(
            session.segments.get(&2).expect("ready segment remains").status,
            SegmentCacheStatus::Ready { .. }
        ));
        assert!(matches!(session.maps.get(&ProxyMapId(1)).expect("map remains").status, MapCacheStatus::Queued { .. }));
    }
    assert!(app_state.active_provider.is_provider_reserved_for_other_session(&account_name, Some("other-owner")));
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
async fn hls_access_lease_sync_keeps_binding_and_queue_when_active_count_remains_positive() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    activate_test_hls_access_lease(&app_state, &proxy_session_id, "still-active-lease", 1_000, 10_000).await;
    {
        let mut session = session.write().await;
        session.activity.active_access_lease_count = 1;
        session.segments.insert(1, test_segment_entry(&proxy_session_id, 1, SegmentCacheStatus::Discovered));
        session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Prefetch, 1_100);
    }
    let old_generation = session.read().await.activity.origin_work_generation;

    app_state
        .hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            &proxy_session_id,
            1_500,
        )
        .await;

    let session = session.read().await;
    assert_eq!(session.activity.active_access_lease_count, 1);
    assert!(matches!(
        session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
        HlsOriginAccountBindingMode::Active
    ));
    assert_eq!(session.activity.origin_work_generation, old_generation);
    assert!(!session.segment_prefetch_queue.is_empty());
    assert!(matches!(
        session.segments.get(&1).expect("segment remains queued").status,
        SegmentCacheStatus::Queued { .. }
    ));
}

#[tokio::test]
async fn hls_access_lease_gc_prepass_releases_user_without_detaching_origin_binding() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    activate_test_hls_access_lease(&app_state, &proxy_session_id, "gc-expired-lease", 1_000, 1_000).await;
    app_state
        .hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            &proxy_session_id,
            1_000,
        )
        .await;
    let old_generation = session.read().await.activity.origin_work_generation;

    app_state
        .hls
        .proxy
        .sync_all_session_access_leases_and_detach_if_needed(&app_state.active_users, &app_state.active_provider, 3_000)
        .await;
    let _ = app_state.hls.proxy.run_garbage_collection_once(3_000).await.expect("gc should run");

    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_id)
        .await
        .expect("detach keeps shared hls session");
    let session = session.read().await;
    assert_eq!(session.activity.active_access_lease_count, 0);
    assert!(matches!(
        session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
        HlsOriginAccountBindingMode::Active
    ));
    assert_eq!(session.activity.origin_work_generation, old_generation);
}

#[tokio::test]
async fn transient_resource_holds_provider_handle_until_origin_body_is_finished() {
    let input = overlap_provider_input();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let origin = spawn_test_transient_origin().await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let proxy_session_id_value = ProxySessionId(proxy_session_id.clone());
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&proxy_session_id_value)
        .await
        .expect("session should exist");
    {
        let mut session = session.write().await;
        session.origin_account_binding = Some(HlsOriginAccountBinding::new(
            Arc::clone(&input.name),
            Arc::from("account-a"),
            &proxy_session_id_value,
            super::super::current_time_millis(),
        ));
    }
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

    let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    wait_for_provider_connection_count(&app_state, 1).await;
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"transient-body"));
    wait_for_provider_connection_count(&app_state, 0).await;
}

#[tokio::test]
async fn terminate_failed_hls_manifest_session_releases_identified_provider_reservation() {
    let input = single_hls_provider_input("failed-hls-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let username = "testuser";
    let session_token = "testuser|stream-1|hls|0123456789abcdef";
    let provider_name = Arc::clone(&input.name);
    let addr = test_addr_with_port(55301);

    let handle = app_state
        .active_provider
        .acquire_connection_with_lease_for_session(
            &provider_name,
            &addr,
            false,
            0,
            ConnectionKind::Normal,
            Some(tuliprox_session::PlaybackLeaseRef::new(session_token, crate::model::PlaybackKind::LiveHls)),
        )
        .expect("handle should be acquired");

    let binding_tag = handle.binding_tag;
    let request_id = handle.playback_request_id;
    assert!(binding_tag.is_some(), "handle should carry a binding tag");

    app_state.connection_manager.release_provider_handle(Some(handle));

    assert!(
        app_state.active_provider.binding_tag_for_owner(session_token).is_some(),
        "lease should still be active after releasing connection handle"
    );

    app_state.active_provider.clear_provider_reservation(session_token);
    assert!(
        app_state.active_provider.binding_tag_for_owner(session_token).is_some(),
        "clear_provider_reservation must return early for public HLS token"
    );

    super::super::segment::terminate_failed_hls_manifest_session(
        &app_state,
        username,
        session_token,
        Some(&provider_name),
        binding_tag,
        request_id,
    )
    .await;

    assert!(
        app_state.active_provider.binding_tag_for_owner(session_token).is_none(),
        "lease should be cleared after terminate_failed_hls_manifest_session with binding identity"
    );
}

#[tokio::test]
async fn delayed_manifest_failure_of_older_binding_keeps_provider_affinity() {
    let input = single_hls_provider_input("affinity-hls-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let provider_name = Arc::clone(&input.name);
    let old_token = "testuser|stream-1|hls|0123456789abcdef";
    let new_token = "testuser|stream-1|hls|fedcba9876543210";
    let acquire = |token: &'static str, port: u16| {
        app_state
            .active_provider
            .acquire_connection_with_lease_for_session(
                &provider_name,
                &test_addr_with_port(port),
                false,
                0,
                ConnectionKind::Normal,
                Some(tuliprox_session::PlaybackLeaseRef::new(token, crate::model::PlaybackKind::LiveHls)),
            )
            .expect("handle should be acquired")
    };

    // The older binding never produced media and ended.
    let old = acquire(old_token, 55311);
    let (old_tag, old_request) = (old.binding_tag, old.playback_request_id);
    let old_request_id = old_request.expect("old request id");
    app_state.connection_manager.release_provider_handle(Some(old));
    app_state.active_provider.finish_identified_playback_request(
        old_token,
        old_request_id,
        tuliprox_core::model::PlaybackRequestOutcome::FailedBeforeMedia,
    );

    // The successor binding confirms media and owns the provider preference.
    let current = acquire(new_token, 55312);
    let current_request = current.playback_request_id.expect("current request id");
    app_state.active_provider.refresh_adaptive_playback_lease(
        &provider_name,
        new_token,
        crate::model::PlaybackKind::LiveHls,
        15,
    );
    app_state.active_provider.confirm_identified_playback_activity(new_token, current_request);
    app_state.connection_manager.release_provider_handle(Some(current));
    assert_eq!(app_state.active_provider.provider_affinity_for_owner(new_token), Some(Arc::clone(&provider_name)));

    for (tag, request) in [(old_tag, old_request), (old_tag, None)] {
        super::super::segment::terminate_failed_hls_manifest_session(
            &app_state,
            "testuser",
            old_token,
            Some(&provider_name),
            tag,
            request,
        )
        .await;
    }

    assert_eq!(
        app_state.active_provider.provider_affinity_for_owner(new_token),
        Some(Arc::clone(&provider_name)),
        "a delayed failure of the older binding must not end the successor's provider preference"
    );
}

#[tokio::test]
async fn terminating_unknown_old_hls_session_keeps_newer_provider_lease() {
    use axum::response::IntoResponse;
    let input = single_hls_provider_input("terminate-hls-input");
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    let provider_name = Arc::clone(&input.name);
    let old_token = "testuser|stream-1|hls|0123456789abcdef";
    let new_token = "testuser|stream-1|hls|fedcba9876543210";
    let current = app_state
        .active_provider
        .acquire_connection_with_lease_for_session(
            &provider_name,
            &test_addr_with_port(55321),
            false,
            0,
            ConnectionKind::Normal,
            Some(tuliprox_session::PlaybackLeaseRef::new(new_token, crate::model::PlaybackKind::LiveHls)),
        )
        .expect("handle should be acquired");
    let request_id = current.playback_request_id.expect("request id");
    app_state.active_provider.refresh_adaptive_playback_lease(
        &provider_name,
        new_token,
        crate::model::PlaybackKind::LiveHls,
        15,
    );
    app_state.active_provider.confirm_identified_playback_activity(new_token, request_id);
    app_state.connection_manager.release_provider_handle(Some(current));
    app_state.active_provider.finish_identified_playback_request(
        new_token,
        request_id,
        tuliprox_core::model::PlaybackRequestOutcome::Completed,
    );
    let binding_tag = app_state.active_provider.binding_tag_for_owner(new_token);
    assert!(binding_tag.is_some(), "the newer playback keeps an idle lease");

    let response = crate::api::endpoints::v1_api_user::terminate_user_session(
        axum::extract::State(Arc::clone(&app_state)),
        axum::extract::Path(("testuser".to_string(), old_token.to_string())),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(app_state.active_provider.binding_tag_for_owner(new_token), binding_tag);
    assert_eq!(app_state.active_provider.provider_affinity_for_owner(new_token), Some(provider_name));
}
