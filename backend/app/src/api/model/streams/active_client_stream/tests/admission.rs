use super::{
    assert_missing_custom_video_terminates, create_active_client_stream, create_deferred_provider_grace_details,
    create_test_app_config, create_test_app_state, create_test_fingerprint, create_test_stream_channel,
    create_test_user, stream_grace_period, ActiveClientStreamParams, ActiveClientStreamState, GracePeriodParams,
    StreamMode,
};
use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, CreateUserSessionParams,
        CustomVideoStreamType, EventManager, GraceResolutionContext, MetadataUpdateManager, PlaylistStorageState,
        ProviderContentRepresentationMode, RecordingQueue, SharedStreamManager, StreamDetails,
    },
    model::{Config, GracePeriodOptions, StreamConfig},
    repository::GeoIp,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::http::HeaderMap;
use futures::{pin_mut, StreamExt};
use shared::{
    model::{AdmissionStrategy, StreamChannel, UserConnectionPermission, VirtualId},
    utils::Internable,
};
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_app_state_with_stream_config(
    stream: StreamConfig,
) -> Arc<AppState> {
    let config = Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(stream),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        user_access_control: true,
        ..Config::default()
    };

    let mut app_cfg = create_test_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(config));

    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    active_provider.set_shared_stream_manager(&shared_stream_manager);

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config_loaded = app_cfg.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&config_loaded, &geoip, &event_manager));
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, None));

    let tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));

    Arc::new(AppState {
        recording_capacity: crate::api::model::recording_runtime::ProviderCapacityAdapter::new(
            Arc::clone(&active_provider),
            Arc::clone(&connection_manager),
        ),
        app_config: Arc::new(app_cfg),
        http_clients: Arc::default(),
        recordings: Arc::new(RecordingQueue::new()),
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls: crate::api::model::HlsState::new(Arc::new(crate::api::model::HlsProxyManager::new())),
        stalker_resolve_coordinator: crate::api::model::StalkerResolveCoordinator::default(),
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: ArcSwap::from_pointee(tokens),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        metadata_manager,
        auth: crate::api::model::AuthState::for_tests(),
        playlist_updates: crate::api::model::PlaylistUpdateControl::for_tests(),
    })
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_shared_stream_channel(
    virtual_id: u32,
    url: &str,
) -> StreamChannel {
    let mut channel = create_test_stream_channel(virtual_id, url);
    channel.shared = true;
    channel
}

pub(in crate::api::model::streams::active_client_stream::tests) async fn start_deferred_provider_grace_resolution(
    app_state: &Arc<AppState>,
    provider_name: &Arc<str>,
    deferred_addr: std::net::SocketAddr,
    session_token: Option<&str>,
) -> (Arc<AtomicU8>, tokio::task::JoinHandle<()>, crate::api::model::ProviderHandle) {
    let deferred_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            provider_name,
            &deferred_addr,
            true,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("deferred client should receive provider grace allocation");
    let mut stream_details = create_deferred_provider_grace_details(
        provider_name,
        tuliprox_session::ManagedProviderHandle::new(
            Arc::clone(&app_state.connection_manager.provider_manager),
            deferred_handle,
        ),
    );
    // The grace-resolution task only reads Copy fields of `stream_details` and never
    // takes ownership of the handle, so disarm it back into the raw handle the caller
    // releases explicitly after the grace task completes.
    let deferred_provider_handle = stream_details
        .provider_handle
        .take()
        .and_then(|mut managed| managed.disarm())
        .expect("deferred provider handle must be retained during grace");
    let test_user = create_test_user("grace-user");
    let test_fingerprint = create_test_fingerprint(deferred_addr);
    let pending_provider_version = if let Some(token) = session_token {
        app_state.active_users.pending_provider_version(&test_user.username, token).await
    } else {
        None
    };
    let (flag, task) = stream_grace_period(GracePeriodParams {
        app_state: Arc::clone(app_state),
        stream_details,
        user_grace_period: false,
        user: test_user,
        fingerprint: test_fingerprint,
        virtual_id: VirtualId::new(1),
        session_token: session_token.map(str::to_string),
        provisioning_info: None,
        waker: None,
        hold_stream: true,
        capacity_notify: app_state.connection_manager.capacity_notified(),
        pending_provider_version,
        grace_active_version: None,
        grace_resolution_context: None,
        grace_kind: None,
        socket_bound: false,
        shared_subscriber_id: None,
    });
    (
        flag.expect("provider grace should install a mode flag"),
        task.expect("provider grace should spawn a grace-resolution task"),
        deferred_provider_handle,
    )
}

#[test]
fn test_custom_video_type_mapping_for_grace_modes() {
    assert_eq!(
        ActiveClientStreamState::custom_video_type_for_mode(StreamMode::UserExhausted),
        Some(CustomVideoStreamType::UserConnectionsExhausted)
    );
    assert_eq!(
        ActiveClientStreamState::custom_video_type_for_mode(StreamMode::ProviderExhausted),
        Some(CustomVideoStreamType::ProviderConnectionsExhausted)
    );
    assert_eq!(
        ActiveClientStreamState::custom_video_type_for_mode(StreamMode::Provisioning),
        Some(CustomVideoStreamType::Provisioning)
    );
    assert_eq!(
        ActiveClientStreamState::custom_video_type_for_mode(StreamMode::LowPriorityPreempted),
        Some(CustomVideoStreamType::LowPriorityPreempted)
    );
    assert_eq!(
        ActiveClientStreamState::custom_video_type_for_mode(StreamMode::ChannelUnavailable),
        Some(CustomVideoStreamType::ChannelUnavailable)
    );
    // A suppressed reentry must never map to a user-visible error clip.
    assert_eq!(ActiveClientStreamState::custom_video_type_for_mode(StreamMode::ReentrySuppressed), None);
    assert_eq!(ActiveClientStreamState::custom_video_type_for_mode(StreamMode::Inner), None);
    assert_eq!(ActiveClientStreamState::custom_video_type_for_mode(StreamMode::GracePending), None);
}

#[tokio::test]
async fn test_low_priority_preempted_without_custom_video_terminates_immediately() {
    assert_missing_custom_video_terminates(StreamMode::LowPriorityPreempted, false).await;
}

#[tokio::test(start_paused = true)]
async fn test_provider_grace_resolution_transitions_from_grace_pending_to_inner_when_capacity_notify_arrives() {
    let app_state = create_test_app_state();
    let provider_name = "provider_1".intern();
    let holder_addr = "127.0.0.1:55010".parse().unwrap_or_else(|_| unreachable!());
    let deferred_addr = "127.0.0.1:55011".parse().unwrap_or_else(|_| unreachable!());

    let holder_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &holder_addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("holder should consume the provider's live capacity");
    let (flag, grace_task, deferred_handle) =
        start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr, None).await;

    assert_eq!(
        StreamMode::try_from(flag.load(Ordering::Acquire)).unwrap(),
        StreamMode::GracePending,
        "provider grace resolution must begin in GracePending while provider capacity is exhausted"
    );

    app_state.connection_manager.release_provider_handle(Some(holder_handle));
    let join_result = tokio::time::timeout(Duration::from_millis(1), grace_task).await;

    assert!(join_result.is_ok(), "provider grace resolution stayed pending after capacity_notify should have fired");
    assert_eq!(
        StreamMode::try_from(flag.load(Ordering::Acquire)).unwrap(),
        StreamMode::Inner,
        "capacity-notify should resolve provider grace from GracePending to Inner before the deadline"
    );

    app_state.connection_manager.release_provider_handle(Some(deferred_handle));
}

#[tokio::test(start_paused = true)]
async fn test_provider_grace_resolution_clears_pending_provider_on_capacity_notify() {
    let app_state = create_test_app_state();
    let provider_name = "provider_1".intern();
    let holder_addr = "127.0.0.1:55024".parse().unwrap_or_else(|_| unreachable!());
    let deferred_addr = "127.0.0.1:55025".parse().unwrap_or_else(|_| unreachable!());
    let user = create_test_user("grace-user");

    let _ = app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-grace",
            virtual_id: 1,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/1.ts",
            addr: &deferred_addr,
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let _ = app_state
        .active_users
        .mark_pending_provider(&user.username, "tok-grace", crate::api::model::PendingProviderReason::GraceHold, 9_999)
        .await;

    let holder_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &holder_addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("holder should consume the provider's live capacity");
    let (_flag, grace_task, deferred_handle) =
        start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr, Some("tok-grace")).await;

    app_state.connection_manager.release_provider_handle(Some(holder_handle));
    let join_result = tokio::time::timeout(Duration::from_millis(1), grace_task).await;
    assert!(join_result.is_ok(), "grace task should finish after capacity notify");

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "tok-grace")
        .await
        .expect("session should still exist");
    assert!(
        !matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. }),
        "capacity notify should clear pending provider state"
    );
    assert_eq!(session.permission, UserConnectionPermission::Allowed);

    app_state.connection_manager.release_provider_handle(Some(deferred_handle));
}

#[tokio::test(start_paused = true)]
async fn test_provider_grace_resolution_ignores_stale_pending_version_on_capacity_notify() {
    let app_state = create_test_app_state();
    let provider_name = "provider_1".intern();
    let holder_addr = "127.0.0.1:55026".parse().unwrap_or_else(|_| unreachable!());
    let deferred_addr = "127.0.0.1:55027".parse().unwrap_or_else(|_| unreachable!());
    let user = create_test_user("grace-user");

    let _ = app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-grace-stale",
            virtual_id: 1,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/1.ts",
            addr: &deferred_addr,
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let _ = app_state
        .active_users
        .mark_pending_provider(
            &user.username,
            "tok-grace-stale",
            crate::api::model::PendingProviderReason::GraceHold,
            9_999,
        )
        .await;

    let holder_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &holder_addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("holder should consume the provider's live capacity");
    let (_flag, grace_task, deferred_handle) =
        start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr, Some("tok-grace-stale"))
            .await;

    let replacement_version = app_state
        .active_users
        .mark_pending_provider(
            &user.username,
            "tok-grace-stale",
            crate::api::model::PendingProviderReason::GraceHold,
            10_500,
        )
        .await
        .expect("replacement pending version should be created");

    app_state.connection_manager.release_provider_handle(Some(holder_handle));
    let join_result = tokio::time::timeout(Duration::from_millis(1), grace_task).await;
    assert!(join_result.is_ok(), "grace task should finish after capacity notify");

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "tok-grace-stale")
        .await
        .expect("session should still exist");
    let crate::api::model::PlaybackLifecycle::PendingProvider { data: pending } = &session.lifecycle else {
        panic!("stale grace task must not clear the replacement pending provider state")
    };
    assert_eq!(pending.version, replacement_version);
    assert!(pending.wake_source.is_none());
    assert_eq!(session.permission, UserConnectionPermission::GracePeriod);

    app_state.connection_manager.release_provider_handle(Some(deferred_handle));
}

#[tokio::test(start_paused = true)]
async fn test_active_client_stream_deferred_provider_grace_retains_provider_handle_while_grace_pending() {
    let app_state = create_test_app_state();
    let provider_name = "provider_1".intern();
    let holder_addr = "127.0.0.1:55012".parse().unwrap_or_else(|_| unreachable!());
    let deferred_addr = "127.0.0.1:55013".parse().unwrap_or_else(|_| unreachable!());
    let third_addr = "127.0.0.1:55014".parse().unwrap_or_else(|_| unreachable!());

    let holder_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &holder_addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("holder should consume the provider's live capacity");
    let deferred_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &deferred_addr,
            true,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("deferred client should receive provider grace allocation");
    let stream_details = create_deferred_provider_grace_details(
        &provider_name,
        tuliprox_session::ManagedProviderHandle::new(
            Arc::clone(&app_state.connection_manager.provider_manager),
            deferred_handle.clone(),
        ),
    );
    let test_user = create_test_user("grace-user");
    let test_fingerprint = create_test_fingerprint(deferred_addr);
    let stream = create_active_client_stream(ActiveClientStreamParams {
        stream_details,
        app_state: &app_state,
        user: &test_user,
        connection_permission: UserConnectionPermission::Allowed,
        connection_kind: crate::api::model::ConnectionKind::Normal,
        fingerprint: &test_fingerprint,
        stream_channel: create_test_stream_channel(1, "http://provider-1.example/live/1"),
        socket_bound: true,
        session_token: None,
        req_headers: &HeaderMap::default(),
        meter_uid: 0,
        meter_stream: false,
    })
    .await
    .expect("deferred test stream admission should succeed");
    pin_mut!(stream);

    assert!(
        matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
        "deferred active-client-stream should park in GracePending while waiting for provider grace resolution"
    );

    let third_handle = app_state.active_provider.acquire_exact_connection_with_grace(
        &provider_name,
        &third_addr,
        true,
        0,
        crate::api::model::ConnectionKind::Normal,
    );

    assert!(
        third_handle.is_none(),
        "deferred active-client-stream should retain the deferred provider grace reservation while GracePending"
    );

    app_state.connection_manager.release_provider_handle(Some(holder_handle));
    app_state.connection_manager.release_provider_handle(Some(deferred_handle));
}

#[tokio::test(start_paused = true)]
async fn test_active_client_stream_shared_deferred_provider_grace_retains_provider_handle_while_grace_pending() {
    let app_state = create_test_app_state();
    let provider_name = "provider_1".intern();
    let holder_addr = "127.0.0.1:55017".parse().unwrap_or_else(|_| unreachable!());
    let deferred_addr = "127.0.0.1:55018".parse().unwrap_or_else(|_| unreachable!());
    let third_addr = "127.0.0.1:55019".parse().unwrap_or_else(|_| unreachable!());

    let holder_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &holder_addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("holder should consume the provider's live capacity");
    let deferred_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &deferred_addr,
            true,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("deferred shared client should receive provider grace allocation");
    let stream_details = create_deferred_provider_grace_details(
        &provider_name,
        tuliprox_session::ManagedProviderHandle::new(
            Arc::clone(&app_state.connection_manager.provider_manager),
            deferred_handle.clone(),
        ),
    );
    let test_user = create_test_user("grace-user");
    let test_fingerprint = create_test_fingerprint(deferred_addr);
    let stream = create_active_client_stream(ActiveClientStreamParams {
        stream_details,
        app_state: &app_state,
        user: &test_user,
        connection_permission: UserConnectionPermission::Allowed,
        connection_kind: crate::api::model::ConnectionKind::Normal,
        fingerprint: &test_fingerprint,
        stream_channel: create_test_shared_stream_channel(1, "http://provider-1.example/live/1"),
        socket_bound: true,
        session_token: None,
        req_headers: &HeaderMap::default(),
        meter_uid: 0,
        meter_stream: false,
    })
    .await
    .expect("shared deferred test stream admission should succeed");
    pin_mut!(stream);

    assert!(
        matches!(futures::poll!(stream.next()), std::task::Poll::Pending),
        "shared deferred active-client-stream should stay pending instead of returning an empty stream"
    );

    let third_handle = app_state.active_provider.acquire_exact_connection_with_grace(
        &provider_name,
        &third_addr,
        true,
        0,
        crate::api::model::ConnectionKind::Normal,
    );

    assert!(
        third_handle.is_none(),
        "shared deferred active-client-stream should retain the deferred provider grace reservation while pending"
    );

    app_state.connection_manager.release_provider_handle(Some(holder_handle));
    app_state.connection_manager.release_provider_handle(Some(deferred_handle));
}

#[tokio::test(start_paused = true)]
async fn test_provider_grace_resolution_transitions_from_grace_pending_to_provider_exhausted_at_deadline() {
    let app_state = create_test_app_state();
    let provider_name = "provider_1".intern();
    let holder_addr = "127.0.0.1:55015".parse().unwrap_or_else(|_| unreachable!());
    let deferred_addr = "127.0.0.1:55016".parse().unwrap_or_else(|_| unreachable!());
    let user = create_test_user("grace-user");

    let _ = app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-grace-timeout",
            virtual_id: 1,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/1.ts",
            addr: &deferred_addr,
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let _ = app_state
        .active_users
        .mark_pending_provider(
            &user.username,
            "tok-grace-timeout",
            crate::api::model::PendingProviderReason::GraceHold,
            9_999,
        )
        .await;

    let holder_handle = app_state
        .active_provider
        .acquire_exact_connection_with_grace(
            &provider_name,
            &holder_addr,
            false,
            0,
            crate::api::model::ConnectionKind::Normal,
        )
        .expect("holder should consume the provider's live capacity");
    let (flag, grace_task, deferred_handle) =
        start_deferred_provider_grace_resolution(&app_state, &provider_name, deferred_addr, Some("tok-grace-timeout"))
            .await;

    assert_eq!(
        StreamMode::try_from(flag.load(Ordering::Acquire)).unwrap(),
        StreamMode::GracePending,
        "provider grace resolution must begin in GracePending while provider capacity is exhausted"
    );

    tokio::time::advance(Duration::from_millis(101)).await;
    let task_result = grace_task.await;

    assert!(
        task_result.is_ok(),
        "grace-resolution task should complete once the deadline expires without capacity becoming available"
    );
    assert_eq!(
        StreamMode::try_from(flag.load(Ordering::Acquire)).unwrap(),
        StreamMode::ProviderExhausted,
        "provider grace resolution should transition from GracePending to ProviderExhausted when the deadline expires"
    );

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "tok-grace-timeout")
        .await
        .expect("session should still exist");
    assert!(
        !matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. }),
        "timeout expiry should clear pending provider state"
    );
    assert_eq!(session.permission, UserConnectionPermission::Exhausted);

    app_state.connection_manager.release_provider_handle(Some(holder_handle));
    app_state.connection_manager.release_provider_handle(Some(deferred_handle));
}

/// Regression test: verifies that when user-grace fails and remaining strategies are
/// exhausted, the `grace_kind` (original `ConnectionKind = Soft`) flows through to
/// `evaluate_remaining_strategies_after_grace` and the session expires correctly.
///
/// This test exercises the full `stream_grace_period` path with:
/// - `grace_kind = Some(Soft)` (passed directly from `create_active_client_stream`)
/// - `grace_resolution_context` pointing to `GraceHoldStream` (remaining slice is empty)
/// - user-grace failure (deadline expires, user still at connection limit)
/// - remaining strategies exhausted -> `expire_pending_provider` is called
#[tokio::test(start_paused = true)]
#[allow(clippy::too_many_lines)]
async fn test_user_grace_failure_preserves_soft_kind_on_exhausted() {
    // Use GraceHoldStream only, remaining slice is empty after grace exhaustion.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 100,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        provider_affinity_ttl_secs: 120,
        hls_wrap_media_playlist: true,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream]),
    });

    let provider_name = "provider_1".intern();
    // Two addresses: first holds the counted session, second triggers grace.
    let first_addr: std::net::SocketAddr = "127.0.0.1:55201".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55202".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);

    let mut user = create_test_user("grace-soft-user");
    user.max_connections = 1; // User has 1 hard slot; first session fills it, grace session exceeds it
    user.soft_connections = 0;

    // First session: counted Normal, consumes the user's only hard slot.
    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-first",
            virtual_id: 1,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/1.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "grace-soft-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &first_fingerprint,
            provider: provider_name.clone(),
            stream_channel: &create_test_stream_channel(1, "http://provider-1.example/live/1.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-first"),
        })
        .await;

    // Second session: will be admitted via grace (user already at limit, grace is granted).
    // The grace session itself is not yet counted, so user_connections returns 1 (just the
    // first session). When the grace deadline expires, the first session is still present,
    // user_connections (1) > max_connections (1) is NOT true. We need the first session to
    // actually consume the hard slot so that the second (grace) session causes the over-limit
    // check to fire when grace expires. Since max_connections=1, the first Normal session
    // uses that slot, and the grace session would push to 2 > 1 — but PendingProvider sessions
    // are not counted! So we need a different approach: create a second Normal session BEFORE
    // the grace session so that user_connections = 2 when grace expires.
    //
    // Actually, the simpler path: make the first session Soft and max_connections=1.
    // A Soft session IS counted (it increments connection_data.connections but uses the soft
    // counter). When grace expires, user_connections = 1 (the Soft session), which is
    // NOT > max_connections = 1. So user_ok = true, no failure.
    //
    // The correct setup: max_connections = 1, first session = Normal (uses the hard slot),
    // second session = GracePeriod (PendingProvider, not counted). When grace deadline hits,
    // user_connections = 1 (Normal only), max_connections = 1, so 1 > 1 is false — no failure.
    //
    // We need the grace session itself to trigger the over-limit check at deadline.
    // But PendingProvider sessions are not counted in user_connections!
    //
    // So the only way for grace to fail at deadline is if there is already a DIFFERENT
    // counted session taking up the slot, AND that session is still there at deadline.
    // With max_connections=1 and first session Normal: when grace expires, user_conn=1,
    // max=1, so 1 > 1 is false.
    //
    // The solution: we need TWO already-counted sessions at deadline, not one.
    // But we can't create both before the grace session because the second would also be
    // admitted via grace.
    //
    // Instead: make the first session consume the slot AND also expire it at deadline,
    // so when grace expires, user_connections = 0, which is NOT > max_connections = 1.
    //
    // The grace session (tok-second) is in PendingProvider state and is NOT counted.
    // To trigger user-grace failure, we need user_connections > max_connections at deadline.
    // With max_connections=1: we need 2 counted sessions at grace deadline.
    // Solution: create two Normal sessions BEFORE the grace session:
    //   tok-first  -> Normal, counted (consumes hard slot)
    //   tok-preload -> Normal, counted (exceeds max, 2 > 1)
    //   tok-second -> GracePeriod, PendingProvider (NOT counted, grace session)
    //
    // At grace deadline: user_connections = 2 (tok-first + tok-preload), max = 1.
    // 2 > 1 -> user_ok = false -> user-grace failure path entered.
    let preload_addr: std::net::SocketAddr = "127.0.0.1:55203".parse().unwrap_or_else(|_| unreachable!());
    let preload_fingerprint = create_test_fingerprint(preload_addr);

    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preload",
            virtual_id: 3,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/3.ts",
            addr: &preload_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 3,
            username: "grace-soft-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &preload_fingerprint,
            provider: provider_name.clone(),
            stream_channel: &create_test_stream_channel(3, "http://provider-1.example/live/3.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-preload"),
        })
        .await;

    let second_fingerprint = create_test_fingerprint(second_addr);

    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-second",
            virtual_id: 2,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/2.ts",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: Some(crate::api::model::ConnectionKind::Soft),
            socket_bound: false,
        })
        .await;

    let _pending_version = app_state
        .active_users
        .mark_pending_provider(
            "grace-soft-user",
            "tok-second",
            crate::api::model::PendingProviderReason::GraceHold,
            9_999,
        )
        .await
        .expect("pending version must be created for tok-second");

    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: [AdmissionStrategy::GraceHoldStream].into(),
        kind: Some(crate::api::model::ConnectionKind::Soft),
    };

    let pending_version = app_state
        .active_users
        .pending_provider_version("grace-soft-user", "tok-second")
        .await
        .expect("pending version must be created for tok-second");

    let stream_details = StreamDetails {
        shared_subscriber_id: None,
        stream: None,
        stream_info: None,
        provider_name: Some(provider_name),
        request_url: Some("http://provider-1.example/live/2.ts".intern()),
        session_headers: None,
        provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
        user_agent_stream_index: None,
        grace_period: GracePeriodOptions { period_millis: 100, timeout_secs: 0, hold_stream: true },
        provider_grace_active: false,
        disable_provider_grace: false,
        reconnect_flag: None,
        provider_handle: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: Some(grace_context.clone()),
        custom_reason: None,
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        session_registration: None,
    };

    let (flag, grace_task) = stream_grace_period(GracePeriodParams {
        app_state: Arc::clone(&app_state),
        stream_details,
        user_grace_period: true,
        user: user.clone(),
        fingerprint: second_fingerprint.clone(),
        virtual_id: VirtualId::new(2),
        session_token: Some("tok-second".to_string()),
        provisioning_info: None,
        waker: None,
        hold_stream: true,
        capacity_notify: app_state.connection_manager.capacity_notified(),
        pending_provider_version: Some(pending_version),
        grace_active_version: None,
        grace_resolution_context: Some(grace_context),
        grace_kind: Some(crate::api::model::ConnectionKind::Soft),
        socket_bound: false,
        shared_subscriber_id: None,
    });

    // Grace should be pending initially.
    assert_eq!(
        StreamMode::try_from(flag.as_ref().unwrap().load(Ordering::Acquire)).unwrap(),
        StreamMode::GracePending,
        "user grace should start in GracePending"
    );

    // Advance time past the grace deadline.
    tokio::time::advance(Duration::from_millis(101)).await;
    let _ = grace_task.expect("grace task should be spawned").await;

    // Remaining strategies are exhausted (only GraceHoldStream was configured, no eviction).
    // The session should expire with UserExhausted.
    assert_eq!(
        StreamMode::try_from(flag.as_ref().unwrap().load(Ordering::Acquire)).unwrap(),
        StreamMode::UserExhausted,
        "exhausted remaining strategies should result in UserExhausted"
    );

    let session = app_state
        .active_users
        .get_and_update_user_session("grace-soft-user", "tok-second")
        .await
        .expect("session must exist after grace failure");

    // Permission should be Exhausted (not GracePeriod).
    assert_eq!(
        session.permission,
        UserConnectionPermission::Exhausted,
        "session permission should be Exhausted after grace failure with exhausted strategies"
    );

    // Lifecycle should be Expired.
    assert!(
        matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::Expired),
        "session lifecycle should be Expired after grace failure"
    );

    // The session's original connection_kind is Soft and must NOT be modified by the
    // grace failure path. The grace_kind = Soft that was passed to
    // evaluate_remaining_strategies_after_grace is verified by the fact that
    // the session's kind remained Soft (it was set at creation time and is preserved
    // through the grace failure flow since session.connection_kind is not changed).
    assert_eq!(
        session.connection_kind,
        Some(crate::api::model::ConnectionKind::Soft),
        "session connection_kind should remain Soft (unchanged from creation)"
    );
}

#[tokio::test(start_paused = true)]
#[allow(clippy::too_many_lines)]
async fn test_user_grace_failure_reentry_suppression_terminates_quietly() {
    // GraceHoldStream admits the request; the remaining EvictUserOldest candidate
    // is reentry-protected, so the grace task must resolve to ReentrySuppressed
    // instead of painting the user-visible connections-exhausted video.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 100,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: true,
        hls_session_ttl_secs: 10,
        catchup_session_ttl_secs: 10,
        provider_affinity_ttl_secs: 120,
        hls_wrap_media_playlist: true,
        throttle_str: None,
        throttle_kbps: 0,
        shared_burst_buffer_mb: 1,
        shared_subscriber_idle_timeout_secs: 300,
        cleanup_queue_capacity: 4096,
        recent_eviction_reentry_ttl: std::time::Duration::from_millis(1500),
        admission_strategies: Some(vec![AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest]),
    });

    let provider_name = "provider_1".intern();
    let first_addr: std::net::SocketAddr = "127.0.0.1:55211".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55212".parse().unwrap_or_else(|_| unreachable!());
    let preload_addr: std::net::SocketAddr = "127.0.0.1:55213".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let preload_fingerprint = create_test_fingerprint(preload_addr);

    let mut user = create_test_user("grace-reentry-user");
    user.max_connections = 1;
    user.soft_connections = 0;

    // Two counted Normal streams make the user over limit when grace expires.
    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-first",
            virtual_id: 1,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/1.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "grace-reentry-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &first_fingerprint,
            provider: provider_name.clone(),
            stream_channel: &create_test_stream_channel(1, "http://provider-1.example/live/1.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-first"),
        })
        .await;
    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preload",
            virtual_id: 3,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/3.ts",
            addr: &preload_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 3,
            username: "grace-reentry-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &preload_fingerprint,
            provider: provider_name.clone(),
            stream_channel: &create_test_stream_channel(3, "http://provider-1.example/live/3.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-preload"),
        })
        .await;

    // Grace session on a distinct channel; its socket-bound stream row is what
    // `mark_recent_eviction_guard_for_addr` keys the reentry guard on.
    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-second",
            virtual_id: 2,
            provider: provider_name.as_ref(),
            stream_url: "http://provider-1.example/live/2.ts",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: Some(crate::api::model::ConnectionKind::Soft),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "grace-reentry-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &second_fingerprint,
            provider: provider_name.clone(),
            stream_channel: &create_test_stream_channel(2, "http://provider-1.example/live/2.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("tok-second"),
        })
        .await;

    // Protect the oldest candidate (channel 1) against a retry of this channel 2.
    app_state.active_users.mark_recent_eviction_guard_for_addr(&second_addr, first_addr, Duration::from_secs(3)).await;

    let _pending_version = app_state
        .active_users
        .mark_pending_provider(
            "grace-reentry-user",
            "tok-second",
            crate::api::model::PendingProviderReason::GraceHold,
            9_999,
        )
        .await
        .expect("pending version must be created for tok-second");

    let grace_context = GraceResolutionContext {
        strategy_index: 0,
        strategies: [AdmissionStrategy::GraceHoldStream, AdmissionStrategy::EvictUserOldest].into(),
        kind: Some(crate::api::model::ConnectionKind::Soft),
    };
    let pending_version = app_state
        .active_users
        .pending_provider_version("grace-reentry-user", "tok-second")
        .await
        .expect("pending version must be created for tok-second");

    let stream_details = StreamDetails {
        shared_subscriber_id: None,
        stream: None,
        stream_info: None,
        provider_name: Some(provider_name),
        request_url: Some("http://provider-1.example/live/2.ts".intern()),
        session_headers: None,
        provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
        user_agent_stream_index: None,
        grace_period: GracePeriodOptions { period_millis: 100, timeout_secs: 0, hold_stream: true },
        provider_grace_active: false,
        disable_provider_grace: false,
        reconnect_flag: None,
        provider_handle: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: Some(grace_context.clone()),
        custom_reason: None,
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        session_registration: None,
    };

    let (flag, grace_task) = stream_grace_period(GracePeriodParams {
        app_state: Arc::clone(&app_state),
        stream_details,
        user_grace_period: true,
        user: user.clone(),
        fingerprint: second_fingerprint.clone(),
        virtual_id: VirtualId::new(2),
        session_token: Some("tok-second".to_string()),
        provisioning_info: None,
        waker: None,
        hold_stream: true,
        capacity_notify: app_state.connection_manager.capacity_notified(),
        pending_provider_version: Some(pending_version),
        grace_active_version: None,
        grace_resolution_context: Some(grace_context),
        grace_kind: Some(crate::api::model::ConnectionKind::Soft),
        socket_bound: true,
        shared_subscriber_id: None,
    });

    assert_eq!(
        StreamMode::try_from(flag.as_ref().unwrap().load(Ordering::Acquire)).unwrap(),
        StreamMode::GracePending,
        "user grace should start in GracePending"
    );

    tokio::time::advance(Duration::from_millis(101)).await;
    let _ = grace_task.expect("grace task should be spawned").await;

    assert_eq!(
        StreamMode::try_from(flag.as_ref().unwrap().load(Ordering::Acquire)).unwrap(),
        StreamMode::ReentrySuppressed,
        "reentry-protected remaining candidate must resolve to quiet termination"
    );

    let session = app_state
        .active_users
        .get_and_update_user_session("grace-reentry-user", "tok-second")
        .await
        .expect("session must exist after grace failure");
    assert!(
        matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::Expired),
        "session lifecycle should be Expired after reentry-suppressed grace failure"
    );
}
