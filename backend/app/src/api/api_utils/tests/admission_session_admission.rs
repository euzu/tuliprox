use super::{
    activate_session_before_stream_open, admission_failure_video_type, create_test_app_config, create_test_app_state,
    create_test_app_state_for_config, create_test_fingerprint, create_test_live_channel,
    create_test_provider_app_config, create_test_shared_target, load_test_channel, load_test_user,
    resolve_admission_with_strategies, should_allow_exhausted_shared_reconnect,
    should_defer_provider_open_for_grace_hold, spawn_load_test_origin, stream_admission_rejected_response,
    stream_response, AdmissionRequest, EvictionReentryGuard, SessionActivationRequest,
};
use crate::{
    api::model::{CustomVideoStreamType, UserSession},
    model::{Config, ConfigInput, ProxyUserCredentials, SourcesConfig},
};
use arc_swap::ArcSwap;
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use shared::{
    model::{
        AdmissionStrategy, ConnectFailureReason, InputType, PlaylistItemType, UserConnectionPermission, VirtualId,
    },
    utils::Internable,
};
use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc};

#[test]
fn stream_admission_rejection_is_503_without_upstream_headers() {
    use crate::api::model::StreamAdmissionError;

    for error in [
        StreamAdmissionError::CleanupReceiverClosed,
        StreamAdmissionError::CleanupAdmissionTimeout,
        StreamAdmissionError::RegistrationRejected,
    ] {
        let response = stream_admission_rejected_response(error, "test-user");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().get("content-length").is_none(), "must not leak upstream content-length");
        assert!(response.headers().get("content-range").is_none(), "must not leak upstream content-range");
    }
}

#[tokio::test]
async fn shared_admission_rejection_does_not_leak_meter_registration() {
    let mut target = create_test_shared_target();
    target.options.as_mut().unwrap().share_live_streams.mpeg_ts = true;
    let target = Arc::new(target);

    let origin_addr = spawn_load_test_origin(b"adm-test-ts-chunk").await;
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_shared_adm".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 5,
        ..ConfigInput::default()
    });
    let mut config = create_test_provider_app_config();
    config.sources =
        Arc::new(ArcSwap::from_pointee(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    config.config = Arc::new(ArcSwap::from_pointee(Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(crate::model::StreamConfig { metrics_enabled: true, ..crate::model::StreamConfig::default() }),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    }));
    let app_state = create_test_app_state_for_config(Arc::new(config));

    let cleanup_tx = app_state.connection_manager.cleanup_tx();
    let mut permits = Vec::new();
    while let Ok(permit) = cleanup_tx.clone().try_reserve_owned() {
        permits.push(permit);
    }

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 55_195));
    let fp = create_test_fingerprint(client_addr);
    let user = load_test_user("user_adm_fail");
    let stream_url = format!("http://{origin_addr}/live/user/pass/adm.ts");
    let mut channel = load_test_channel(&input, origin_addr);
    channel.url = stream_url.as_str().intern();

    let resp = stream_response(
        &fp,
        &app_state,
        "shared-session-adm",
        None,
        channel,
        &stream_url,
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

    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "shared admission failure must yield 503 Service Unavailable"
    );
    assert_eq!(
        app_state.active_provider.get_provider_connections_count(),
        0,
        "admission failure must NOT fall through to opening a provider connection"
    );
    assert_eq!(app_state.shared_stream_manager.meter_count(), 0, "rejected admission must not retain a meter ID");

    drop(permits);
}

#[test]
fn test_should_allow_exhausted_shared_reconnect_only_for_matching_shared_session() {
    let session = UserSession {
        transition_version: 1,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        token: "tok".to_string(),
        virtual_id: 282,
        provider: Arc::<str>::from("provider"),
        stream_url: Arc::<str>::from("http://provider/live/449924.ts"),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        user_agent_stream_index: None,
        addr: "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!()),
        socket_bound: false,
        active_addrs: vec!["127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!())],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };

    assert!(should_allow_exhausted_shared_reconnect(true, Some(&session), 282, "http://provider/live/449924.ts"));
    assert!(!should_allow_exhausted_shared_reconnect(false, Some(&session), 282, "http://provider/live/449924.ts"));
    assert!(!should_allow_exhausted_shared_reconnect(true, Some(&session), 999, "http://provider/live/449924.ts"));
    assert!(!should_allow_exhausted_shared_reconnect(true, Some(&session), 282, "http://provider/live/other.ts"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn activate_session_before_stream_open_marks_pending_provider_for_grace_hold() {
    let stream_cfg = crate::model::StreamConfig {
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
    let first_addr: SocketAddr = "127.0.0.1:55230".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: SocketAddr = "127.0.0.1:55231".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let input = app_state.app_config.sources.load().inputs[0].clone();
    let mut user = ProxyUserCredentials::default();
    user.username = "grace-hold-user".to_string();
    user.max_connections = 1;
    let first_channel = create_test_live_channel("http://provider-1.example/live/1.ts");
    let mut second_channel = create_test_live_channel("http://provider-1.example/live/2.m3u8");
    second_channel.item_type = PlaylistItemType::LiveHls;
    second_channel.virtual_id = 55231;

    app_state.connection_manager.add_connection(&first_addr).await;
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
            fingerprint: &first_fingerprint,
            provider: input.name.clone(),
            stream_channel: &first_channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-first"),
        })
        .await;

    let activation = activate_session_before_stream_open(
        &app_state,
        SessionActivationRequest {
            fingerprint: &second_fingerprint,
            input: input.as_ref(),
            user: &user,
            session_token: "tok-grace-hold",
            request_class: None,
            virtual_id: VirtualId::new(second_channel.virtual_id),
            item_type: PlaylistItemType::LiveHls,
            stream_url: second_channel.url.as_ref(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            granted_grace_mode: None,
            socket_bound: false,
        },
    )
    .await;

    assert_eq!(activation.admission.permission(), UserConnectionPermission::GracePeriod);
    assert_eq!(activation.grace_mode, Some(crate::api::model::GraceMode::Hold));

    let session = app_state
        .active_users
        .get_and_update_user_session(&user.username, "tok-grace-hold")
        .await
        .expect("placeholder session should exist");
    let crate::api::model::PlaybackLifecycle::PendingProvider { data: pending } = &session.lifecycle else {
        panic!("grace hold should mark pending provider state")
    };
    assert!(matches!(pending.reason_code, crate::api::model::PendingProviderReason::GraceHold));
    assert!(pending.deadline >= pending.created_at);
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    assert!(
        !session.lifecycle.is_counted(),
        "pending provider placeholder must not consume an active user lease before commit"
    );
}

#[test]
fn admission_failure_reason_maps_to_custom_video_type() {
    assert!(matches!(
        admission_failure_video_type(ConnectFailureReason::UserAccountExpired),
        Some(CustomVideoStreamType::UserAccountExpired)
    ));
    assert!(matches!(
        admission_failure_video_type(ConnectFailureReason::UserConnectionsExhausted),
        Some(CustomVideoStreamType::UserConnectionsExhausted)
    ));
    assert!(matches!(
        admission_failure_video_type(ConnectFailureReason::ProviderConnectionsExhausted),
        Some(CustomVideoStreamType::ProviderConnectionsExhausted)
    ));
    assert!(admission_failure_video_type(ConnectFailureReason::ProviderError).is_none());
}

#[tokio::test]
async fn resolve_admission_with_strategies_allows_existing_session_even_when_user_is_at_limit() {
    let app_state = create_test_app_state();
    let addr = "127.0.0.1:55154".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint(addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "session-admission".to_string();
    user.max_connections = 1;

    app_state.connection_manager.add_connection(&addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "vod-session",
            virtual_id: 1,
            provider: "provider-a",
            stream_url: "http://provider-1.example/movie/1.mkv",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
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
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/movie/1.mkv"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some("vod-session"),
        })
        .await;

    let session_based = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: true,
            session_token: Some("vod-session"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("vod-session"),
        },
    )
    .await;
    assert_eq!(session_based.admission.permission(), UserConnectionPermission::Allowed);

    let connection_based = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &fingerprint.client_ip,
            request_addr: &fingerprint.addr,
            use_session_admission: false,
            session_token: Some("vod-session"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("vod-session"),
        },
    )
    .await;
    assert_eq!(connection_based.admission.permission(), UserConnectionPermission::Exhausted);
}

#[tokio::test]
async fn activated_session_admission_keeps_hls_placeholders_uncounted_via_api_utils() {
    let app_state = create_test_app_state();
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.max_connections = 1;

    let first_addr: std::net::SocketAddr = "127.0.0.1:55177".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55178".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-first",
            virtual_id: 7101,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/7101.m3u8",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-second",
            virtual_id: 7102,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/7102.m3u8",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let first_admission = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &first_fingerprint.client_ip,
            request_addr: &first_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-hls-first"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-hls-first"),
        },
    )
    .await;
    let second_admission = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &second_fingerprint.client_ip,
            request_addr: &second_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("tok-hls-second"),
            activate_unbound_session: true,
            eviction_reentry_guard: EvictionReentryGuard::Session("tok-hls-second"),
        },
    )
    .await;

    assert_eq!(first_admission.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(second_admission.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);
}

#[test]
fn grace_hold_defers_live_and_fresh_video_but_not_catchup_or_affine_reopens() {
    assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::LiveHls, false));
    assert!(should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Video, false));
    assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Catchup, false));
    assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Catchup, true));
    assert!(!should_defer_provider_open_for_grace_hold(true, true, PlaylistItemType::Video, true));
    assert!(!should_defer_provider_open_for_grace_hold(true, false, PlaylistItemType::Video, true));
}
