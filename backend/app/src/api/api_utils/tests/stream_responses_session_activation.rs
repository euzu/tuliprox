use super::{
    activate_session_before_stream_open, create_test_app_config, create_test_app_state,
    create_test_app_state_for_config, create_test_app_state_with_stream_config, create_test_fingerprint,
    create_test_live_channel, create_test_provider_app_config, resolve_admission_with_strategies, stream_response,
    AdmissionRequest, EvictionReentryGuard, PlaybackRequestClass, SessionActivationRequest,
};
use crate::model::{Config, ConfigTarget, ProxyUserCredentials};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use shared::{
    foundation::Filter,
    model::{AdmissionStrategy, PlaylistItemType, ProcessingOrder, UserConnectionPermission, VirtualId},
    utils::Internable,
};
use std::{borrow::Cow, net::SocketAddr, sync::Arc};

#[tokio::test]
async fn activate_session_before_stream_open_skips_placeholder_for_follow_up_session() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55220".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "follow-up-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55220.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55220;

    app_state.connection_manager.add_connection(&addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-follow-up",
            virtual_id: channel.virtual_id,
            provider: input.name.as_ref(),
            stream_url: channel.url.as_ref(),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 10,
            fingerprint: &fingerprint,
            provider: input.name.clone(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-follow-up"),
        })
        .await;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-follow-up",
            request_class: None,
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: None,
            socket_bound: true,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.admission.kind(), Some(crate::api::model::ConnectionKind::Normal));
    assert_eq!(activation.grace_mode, None);
    assert!(
        activation.placeholder_transition_version.is_none(),
        "follow-up activation must not create a placeholder session"
    );
}

#[tokio::test]
async fn activate_session_before_stream_open_revalidates_precomputed_follow_up_request_class() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55221".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "precomputed-follow-up-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55221.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55221;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-precomputed-follow-up",
            virtual_id: channel.virtual_id,
            provider: input.name.as_ref(),
            stream_url: channel.url.as_ref(),
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-precomputed-follow-up",
            request_class: Some(PlaybackRequestClass::FollowUp),
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: None,
            socket_bound: true,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.admission.kind(), Some(crate::api::model::ConnectionKind::Normal));
    assert_eq!(activation.grace_mode, None);
    assert!(
        activation.placeholder_transition_version.is_some(),
        "precomputed FollowUp must be revalidated against the current uncounted lifecycle"
    );
}

// pre-resolved Grace materialization
#[tokio::test]
async fn activate_session_before_stream_open_pre_resolved_grace_period_materializes_pending_provider() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
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
    let addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "pre-resolved-grace-user".to_string();
    user.max_connections = 1;
    let mut channel = create_test_live_channel("http://provider-1.example/live/55231.m3u8");
    channel.item_type = PlaylistItemType::LiveHls;
    channel.virtual_id = 55231;

    // A pre-resolved grace grant reaches activation before a session exists.
    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-pre-resolved-grace",
            request_class: None,
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: Some(crate::api::model::GraceMode::Hold),
            socket_bound: true,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::GracePeriod);
    assert_eq!(activation.grace_mode, Some(crate::api::model::GraceMode::Hold));

    let session = app_state.active_users.get_and_update_user_session(&user.username, "tok-pre-resolved-grace").await;
    assert!(
        session.is_some_and(|s| matches!(s.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. })),
        "pre-resolved GracePeriod must materialize as PendingProvider lifecycle"
    );
}

#[tokio::test]
async fn pre_resolved_instant_grace_creates_counted_live_session() {
    let app_state = create_test_app_state();
    let addr: SocketAddr = "127.0.0.1:55233".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "instant-grace-live-user".to_string();
    user.max_connections = 1;
    let channel = create_test_live_channel("http://provider-1.example/live/55233.ts");

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-instant-grace",
            request_class: Some(PlaybackRequestClass::Activate),
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: PlaylistItemType::Live,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: Some(crate::api::model::GraceMode::Instant),
            socket_bound: true,
        },
    )
    .await;

    assert_eq!(activation.grace_mode, Some(crate::api::model::GraceMode::Instant));
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    let session = app_state.active_users.get_and_update_user_session(&user.username, "tok-instant-grace").await;
    assert!(session.is_some_and(|session| session.lifecycle == crate::api::model::PlaybackLifecycle::GraceActive));
}

/// `activate_session_before_stream_open` skips placeholder for Prepare class.
#[tokio::test]
async fn activate_session_before_stream_open_skips_placeholder_for_prepare() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest]),
    });
    let addr: SocketAddr = "127.0.0.1:55222".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "prepare-user".to_string();
    user.max_connections = 1;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-prepare",
            // Explicitly pass Prepare class — placeholder and admission should be skipped.
            request_class: Some(PlaybackRequestClass::Prepare),
            virtual_id: VirtualId::new(55222),
            item_type: PlaylistItemType::LiveHls,
            stream_url: "http://provider.example/live/test.ts",
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: None,
            socket_bound: true,
        },
    )
    .await;

    // Prepare returns Allowed without running admission strategies.
    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.grace_mode, None);
    assert!(
        activation.placeholder_transition_version.is_none(),
        "Prepare activation must not create a placeholder session"
    );
}

#[tokio::test]
async fn activate_session_before_stream_open_does_not_commit_user_lease_before_provider_success() {
    let stream_cfg = crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 0,
        grace_period_timeout_secs: 8,
        grace_period_hold_stream: false,
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
        admission_strategies: None,
    };
    let mut app_cfg = create_test_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
        user_access_control: true,
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(stream_cfg),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let addr: SocketAddr = "127.0.0.1:55232".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "atomic-commit-user".to_string();
    user.max_connections = 1;
    let channel = create_test_live_channel("http://provider-1.example/live/3.ts");

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-atomic-commit",
            request_class: None,
            virtual_id: VirtualId::new(channel.virtual_id),
            item_type: channel.item_type,
            stream_url: channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: None,
            socket_bound: false,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(activation.grace_mode, None);

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "tok-atomic-commit")
        .await
        .expect("placeholder session should exist");
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "allowed activation should stay provisional until provider acquisition and stream commit succeed"
    );
    assert!(
        !session.lifecycle.is_counted(),
        "placeholder session must stay uncounted until the provider side has been committed"
    );
    assert!(!matches!(session.lifecycle, crate::api::model::PlaybackLifecycle::PendingProvider { .. }));
}

#[tokio::test]
async fn grace_context_is_populated_when_grace_strategy_is_actually_granted() {
    // Use a DIFFERENT session token than the pre-existing counted session.
    // Otherwise session-admission may treat it as a valid reopen and skip the exhausted path.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
        retry: true,
        metrics_enabled: true,
        buffer: None,
        grace_period_millis: 2_000,
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
        admission_strategies: Some(vec![
            AdmissionStrategy::EvictUserSameIpOldest,
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserOldest,
        ]),
    });

    let addr1: SocketAddr = "127.0.0.1:55401".parse().unwrap_or_else(|_| unreachable!());
    let addr2: SocketAddr = "10.0.0.5:55402".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint1 = create_test_fingerprint(addr1);
    let fingerprint2 = create_test_fingerprint(addr2);
    // addr1 and addr2 have DIFFERENT IPs.
    // EvictUserSameIpOldest will NOT match (different IP), so GraceHoldStream is evaluated.
    let mut user = ProxyUserCredentials::default();
    user.username = "user-grace-ctx".to_string();
    user.max_connections = 1;

    // Register the connection first so update_connection succeeds
    app_state.connection_manager.add_connection(&addr1).await;

    // Create the session — lifecycle starts as Prepared (uncounted)
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-existing-counted",
            virtual_id: 55401,
            provider: "provider_1",
            stream_url: "http://provider-1.example/live/55401.m3u8",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    // update_connection promotes the session to Active (counted) and creates a stream.
    // This exhausts the user's single slot (max_connections = 1).
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 55401,
            meter_uid: 55401,
            username: "user-grace-ctx",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint1,
            provider: "provider_1".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/live/55401.m3u8"),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-existing-counted"),
        })
        .await
        .expect("stream should be created");

    // Now the new request finds the slot exhausted and the grace strategy kicks in.
    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint2.client_ip,
            request_addr: &fingerprint2.addr,
            use_session_admission: true,
            session_token: Some("tok-new-request"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-new-request"),
        },
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::GracePeriod, "grace should be granted");
    assert!(matches!(result.grace_mode, Some(crate::api::model::GraceMode::Hold)));
    let ctx = result.grace_context.expect("grace_context must be present when grace is granted");
    assert_eq!(ctx.strategy_index, 1, "GraceHoldStream is at index 1");
    assert_eq!(ctx.strategies.len(), 3);
    assert!(matches!(ctx.strategies[ctx.strategy_index], AdmissionStrategy::GraceHoldStream));
}

#[tokio::test]
async fn stream_response_rolls_back_provisional_user_activation_when_provider_open_fails() {
    let mut app_cfg = create_test_provider_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(Config {
        user_access_control: true,
        custom_stream_response_enabled: true,
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let addr = "127.0.0.1:55143".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: "test".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    });
    let mut user = ProxyUserCredentials::default();
    user.username = "rollback-user".to_string();
    user.max_connections = 1;
    let stream_url = "provider://bad-url";
    let channel = create_test_live_channel(stream_url);

    let response = stream_response(
        &fingerprint,
        &app_state,
        "rollback-session",
        None,
        channel,
        stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
    )
    .await
    .into_response();

    // Custom-video stream is enabled (`custom_stream_response_enabled: true`
    // in this fixture), so a missing resource must return 400 — the
    // Nginx `proxy_intercept_errors on;` contract requires 4xx so the
    // socket is severed instead of looping on a 200 OK fallback body.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "failed provider open must rollback provisional user activation"
    );
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, "rollback-session").await.is_none(),
        "failed provider open must remove the provisional placeholder session"
    );
}
