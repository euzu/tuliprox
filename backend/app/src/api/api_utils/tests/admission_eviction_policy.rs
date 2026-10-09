use super::{
    admission::{connection_denied_count, create_test_fingerprint_with_user_agent},
    create_session_fingerprint, create_test_app_config, create_test_app_state_for_config,
    create_test_app_state_with_stream_config, create_test_live_channel, resolve_admission_with_strategies,
    resolve_playback_request_admission, AdmissionRequest, EvictionReentryGuard, PlaybackRequestClass,
};
use crate::model::{Config, ProxyUserCredentials};
use arc_swap::ArcSwap;
use axum::http::StatusCode;
use shared::{
    model::{AdmissionStrategy, PlaylistItemType, StreamChannel, UserConnectionPermission, VirtualId},
    utils::Internable,
};
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tuliprox_session::AdmissionRejectionReason;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_prevents_recently_evicted_playback_ping_pong() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55181".parse().unwrap_or_else(|_| unreachable!());
    let reconnect_addr: std::net::SocketAddr = "127.0.0.1:55182".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55183".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let reconnect_fingerprint = create_test_fingerprint_with_user_agent(reconnect_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9001.ts");
    victim_channel.virtual_id = 9001;
    let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9002.ts");
    winner_channel.virtual_id = 9002;

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "loop-user".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9001.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9002,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9002.ts",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &reconnect_fingerprint.client_ip,
            request_addr: &reconnect_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("socket-reconnect"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9001) },
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(admission.rejection_reason(), Some(AdmissionRejectionReason::RecentEvictionReentry));
    assert!(admission.is_reentry_suppressed());
    assert_eq!(grace_mode, None);
    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams[0].channel.virtual_id, 9002);

    // A player may retry the winning plain-TS stream on another socket before the
    // previous HTTP body has drained. That is a replacement of the same playback,
    // not the evicted channel trying to reclaim its slot.
    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&winner_addr, reconnect_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&winner_addr).await;
    app_state.connection_manager.add_connection(&reconnect_addr).await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 3,
            username: "loop-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &reconnect_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: None,
        })
        .await;

    let retry_addr: std::net::SocketAddr = "127.0.0.1:55184".parse().unwrap_or_else(|_| unreachable!());
    let retry = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &reconnect_fingerprint.client_ip,
            request_addr: &retry_addr,
            use_session_admission: true,
            session_token: Some("socket-retry"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9002) },
        },
    )
    .await;

    assert_eq!(retry.admission.permission(), UserConnectionPermission::Allowed);
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_allows_other_channel_after_recent_eviction() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55184".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55185".parse().unwrap_or_else(|_| unreachable!());
    let new_addr: std::net::SocketAddr = "127.0.0.1:55186".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let new_fingerprint = create_test_fingerprint_with_user_agent(new_addr, "player/1.0");
    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9101.ts");
    victim_channel.virtual_id = 9101;
    let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9102.ts");
    winner_channel.virtual_id = 9102;
    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "loop-user-2".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    // Create sessions before update_connection so streams are linked to counted sessions
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9101,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9101.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9102,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9102.ts",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user-2",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user-2",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user-2",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &new_fingerprint.client_ip,
            request_addr: &new_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-new"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9103) },
        },
    )
    .await;
    let admission = result.admission;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_does_not_suppress_different_session_on_same_channel() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55190".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55191".parse().unwrap_or_else(|_| unreachable!());
    let new_addr: std::net::SocketAddr = "127.0.0.1:55192".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "player/1.0");
    let new_fingerprint = create_test_fingerprint_with_user_agent(new_addr, "player/1.0");
    let mut channel = create_test_live_channel("http://provider-1.example/live/9301.m3u8");
    channel.virtual_id = 9301;
    channel.item_type = PlaylistItemType::LiveHls;
    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "loop-user-4".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    // Create sessions before update_connection so streams are linked to counted sessions
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9301,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9301.m3u8",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9301,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9301.m3u8",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user-4",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user-4",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user-4",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &new_fingerprint.client_ip,
            request_addr: &new_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-other"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::Session("session-other"),
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(grace_mode, None);
    assert!(app_state.active_users.active_streams().await.is_empty());
}

#[tokio::test]
async fn resolve_admission_with_strategies_allows_recently_evicted_playback_when_soft_slot_is_free() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: std::net::SocketAddr = "127.0.0.1:55187".parse().unwrap_or_else(|_| unreachable!());
    let reconnect_addr: std::net::SocketAddr = "127.0.0.1:55188".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: std::net::SocketAddr = "127.0.0.1:55189".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let reconnect_fingerprint = create_test_fingerprint_with_user_agent(reconnect_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9201.ts");
    victim_channel.virtual_id = 9201;
    let mut winner_channel = create_test_live_channel("http://provider-1.example/live/9202.ts");
    winner_channel.virtual_id = 9202;

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "loop-user-3",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "loop-user-3",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "loop-user-3",
            max_connections: 1,
            soft_connections: 1,
            client_ip: &reconnect_fingerprint.client_ip,
            request_addr: &reconnect_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("socket-reconnect"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9201) },
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(crate::api::model::ConnectionKind::Soft));
    assert_eq!(grace_mode, None);

    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams[0].channel.virtual_id, 9202);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolve_admission_with_strategies_evicts_preserved_hls_session_for_same_user_ts_request() {
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
            AdmissionStrategy::EvictUserSameIpLatest,
            AdmissionStrategy::GraceHoldStream,
            AdmissionStrategy::EvictUserOldest,
            AdmissionStrategy::EvictUserLatest,
        ]),
    });

    let hls_addr: std::net::SocketAddr = "127.0.0.1:55176".parse().unwrap_or_else(|_| unreachable!());
    let ts_addr: std::net::SocketAddr = "127.0.0.1:55177".parse().unwrap_or_else(|_| unreachable!());
    let hls_fingerprint = create_test_fingerprint_with_user_agent(hls_addr, "player/1.0");
    let ts_fingerprint = create_test_fingerprint_with_user_agent(ts_addr, "player/1.0");
    let mut user = ProxyUserCredentials::default();
    user.username = "same-user".to_string();
    user.max_connections = 1;

    app_state.connection_manager.add_connection(&hls_addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-preserved",
            virtual_id: 5001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/5001.m3u8",
            addr: &hls_addr,
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
            soft_priority: 0,
            fingerprint: &hls_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel {
                item_type: PlaylistItemType::LiveHls,
                virtual_id: 5001,
                ..create_test_live_channel("http://provider-1.example/live/5001.m3u8")
            },
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("tok-hls-preserved"),
        })
        .await;

    app_state.connection_manager.release_connection(&hls_addr).await;
    assert_eq!(app_state.active_users.user_connections(&user.username).await, 0);
    assert!(app_state.active_users.active_streams().await.is_empty());

    let mut close_rx = app_state.connection_manager.get_close_connection_channel();
    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            client_ip: &ts_fingerprint.client_ip,
            request_addr: &ts_fingerprint.addr,
            use_session_admission: false,
            session_token: None,
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(5001) },
        },
    )
    .await;
    let admission = result.admission;
    let grace_mode = result.grace_mode;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(grace_mode, None);
    assert!(app_state.active_users.active_streams().await.is_empty());
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_millis(100), close_rx.recv()).await.ok().and_then(Result::ok),
        Some(crate::api::model::CloseConnectionSignal::WithReason(
            hls_addr,
            shared::model::DisconnectReason::ClientKicked,
        ))
    );
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, "tok-hls-preserved").await.is_none(),
        "preserved session should be removed once the TS request evicts it"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn xtream_hls_then_ts_uses_distinct_tokens_and_evicts_old_hls_session() {
    let mut app_cfg = create_test_app_config();
    let config = Config {
        user_access_control: true,
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(crate::model::StreamConfig {
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
                    AdmissionStrategy::EvictUserSameIpLatest,
                    AdmissionStrategy::GraceHoldStream,
                    AdmissionStrategy::EvictUserOldest,
                    AdmissionStrategy::EvictUserLatest,
                ]),
            }),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        ..Config::default()
    };
    app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
    let app_state = create_test_app_state_for_config(Arc::new(app_cfg));
    let hls_addr: SocketAddr = "127.0.0.1:55186".parse().unwrap_or_else(|_| unreachable!());
    let ts_addr: SocketAddr = "127.0.0.1:55187".parse().unwrap_or_else(|_| unreachable!());
    let hls_fingerprint = create_test_fingerprint_with_user_agent(hls_addr, "libmpv");
    let ts_fingerprint = create_test_fingerprint_with_user_agent(ts_addr, "libmpv");
    let mut user = ProxyUserCredentials::default();
    user.username = "xtream-hls-ts".to_string();
    user.max_connections = 1;

    let virtual_id = 7811;
    let hls_token = create_session_fingerprint(&hls_fingerprint, &user.username, virtual_id, false);
    let ts_token = create_session_fingerprint(&ts_fingerprint, &user.username, virtual_id, true);
    assert_ne!(hls_token, ts_token, "Xtream .m3u8 and .ts must not share the same playback token");

    let mut hls_channel = create_test_live_channel("http://provider-1.example/live/7811.m3u8");
    hls_channel.virtual_id = virtual_id;
    hls_channel.item_type = PlaylistItemType::LiveHls;
    let mut ts_channel = create_test_live_channel("http://provider-1.example/live/7811.ts");
    ts_channel.virtual_id = virtual_id;

    app_state.connection_manager.add_connection(&hls_addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &hls_token,
            virtual_id,
            provider: "provider_1",
            stream_url: hls_channel.url.as_ref(),
            addr: &hls_addr,
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
            soft_priority: 0,
            fingerprint: &hls_fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &hls_channel,
            user_agent: Cow::Borrowed("libmpv"),
            session_token: Some(&hls_token),
        })
        .await;

    app_state.connection_manager.release_connection(&hls_addr).await;
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "the preserved HLS playback must reserve capacity only virtually"
    );
    assert_eq!(app_state.active_users.active_users_and_connections().await, (0, 0));
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, &hls_token).await.is_some(),
        "preserved HLS session should still exist before the competing TS request"
    );
    assert_eq!(
        app_state
            .active_users
            .connection_admission(&user.username, user.max_connections, user.soft_connections)
            .await
            .permission(),
        UserConnectionPermission::Exhausted,
        "the preserved HLS playback must still reserve the user's only slot before the TS request is evaluated"
    );
    assert_eq!(
        app_state.active_users.get_eviction_candidates(&user.username, &ts_fingerprint.client_ip).await.len(),
        1,
        "the preserved HLS playback should be the single eviction candidate for the competing TS request"
    );

    let (ts_admission, ts_grace_mode, request_class) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &ts_fingerprint,
        None,
        &ts_token,
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(virtual_id) },
        false,
        false,
    )
    .await;
    assert_eq!(request_class, PlaybackRequestClass::Activate);
    assert_eq!(ts_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(ts_grace_mode, None);
    assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_none(),
            "the competing TS activation must remove the old preserved HLS session even though there is no live socket left to kick"
        );
    assert_eq!(
        app_state.active_users.user_connections(&user.username).await,
        0,
        "eviction must not leave a real slot before the TS stream commits"
    );
    assert_eq!(app_state.active_users.active_users_and_connections().await, (0, 0));

    app_state.connection_manager.add_connection(&ts_addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &ts_token,
            virtual_id,
            provider: "provider_1",
            stream_url: ts_channel.url.as_ref(),
            addr: &ts_addr,
            connection_permission: ts_admission.permission(),
            connection_kind: ts_admission.kind(),
            socket_bound: true,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &ts_fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &ts_channel,
            user_agent: Cow::Borrowed("libmpv"),
            session_token: Some(&ts_token),
        })
        .await;

    assert_eq!(app_state.active_users.user_connections(&user.username).await, 1);
    assert_eq!(app_state.active_users.active_users_and_connections().await, (1, 1));
    let active_streams = app_state.active_users.active_streams().await;
    assert_eq!(active_streams.len(), 1);
    assert_eq!(active_streams.first().and_then(|stream| stream.session_token.as_deref()), Some(ts_token.as_str()));
    assert!(
            app_state
                .active_users
                .get_and_update_user_session(&user.username, &hls_token)
                .await
                .is_none(),
            "after the competing TS request, the old Xtream HLS session must be gone so later /hls segment fetches cannot revive it"
        );
    assert!(
        app_state.active_users.get_and_update_user_session(&user.username, &ts_token).await.is_some(),
        "the winning TS playback should remain tracked under its socket-bound Xtream token"
    );
}

#[tokio::test]
async fn test_reentry_suppression_response_behavior() {
    let response = crate::api::api_utils::reentry_suppressed_response();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(!response.headers().contains_key("x-tuliprox-rejection"));
    assert!(!response.headers().contains_key("content-type"));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_eviction_strategy_falls_back_when_candidate_protected_by_reentry() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest, AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: SocketAddr = "127.0.0.1:55301".parse().unwrap_or_else(|_| unreachable!());
    let winner_addr: SocketAddr = "127.0.0.1:55302".parse().unwrap_or_else(|_| unreachable!());
    let other_ip_addr: SocketAddr = "10.0.0.1:55303".parse().unwrap_or_else(|_| unreachable!());
    let retry_addr: SocketAddr = "127.0.0.1:55304".parse().unwrap_or_else(|_| unreachable!());

    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let winner_fingerprint = create_test_fingerprint_with_user_agent(winner_addr, "winner/1.0");
    let other_ip_fingerprint = create_test_fingerprint_with_user_agent(other_ip_addr, "other/1.0");
    let retry_fingerprint = create_test_fingerprint_with_user_agent(retry_addr, "player/1.0");

    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9501.ts");
    victim_channel.virtual_id = 9501;
    let winner_channel = create_test_live_channel("http://provider-1.example/live/9502.ts");
    let other_channel = create_test_live_channel("http://provider-1.example/live/9503.ts");

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "fallback-user".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&winner_addr).await;
    app_state.connection_manager.add_connection(&other_ip_addr).await;

    // Create user sessions so streams are linked to counted sessions
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9501,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9501.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-winner",
            virtual_id: 9502,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9502.ts",
            addr: &winner_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-other",
            virtual_id: 9503,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9503.ts",
            addr: &other_ip_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Stream 0: victim on same IP (127.0.0.1), channel 9501
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 99,
            username: "fallback-user",
            max_connections: 3,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;

    // Stream 1: winner on same IP (127.0.0.1), channel 9502
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 1,
            username: "fallback-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &winner_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &winner_channel,
            user_agent: std::borrow::Cow::Borrowed("winner/1.0"),
            session_token: Some("session-winner"),
        })
        .await;

    // Stream 2: stream on different IP (10.0.0.1), channel 9503
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 2,
            username: "fallback-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &other_ip_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &other_channel,
            user_agent: std::borrow::Cow::Borrowed("other/1.0"),
            session_token: Some("session-other"),
        })
        .await;

    // Mark winner_addr as protected against reentry of victim_addr (channel 9501)
    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, winner_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    // Retry for channel 9501 arrives from same IP (127.0.0.1).
    // EvictUserSameIpOldest will target winner_addr (same IP), but winner_addr is protected!
    // It should be suppressed, filtered out, and Fall B will continue to EvictUserOldest.
    // EvictUserOldest will then target other_ip_addr (which is NOT protected).
    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "fallback-user",
            max_connections: 2,
            soft_connections: 0,
            client_ip: &retry_fingerprint.client_ip,
            request_addr: &retry_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-retry"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9501) },
        },
    )
    .await;

    // Request must be Admitted because the fallback strategy evicted the unprotected stream!
    assert_eq!(result.admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(result.admission.rejection_reason(), None);

    // Now test rejection when NO other candidate exists (only the protected stream).
    // EvictUserSameIpOldest suppresses winner_addr; EvictUserOldest finds no candidates left.
    let result_exhausted = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "fallback-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &retry_fingerprint.client_ip,
            request_addr: &retry_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-retry-2"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9501) },
        },
    )
    .await;

    assert_eq!(result_exhausted.admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(result_exhausted.admission.rejection_reason(), Some(AdmissionRejectionReason::RecentEvictionReentry));
    assert!(result_exhausted.admission.is_reentry_suppressed());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_mixed_suppressed_and_legitimate_eviction_reports_exhaustion() {
    // A protected candidate is skipped, then a legitimate (but uncounted) candidate is
    // evicted and admission still fails. That residual failure is real exhaustion and
    // must not be mislabelled as `RecentEvictionReentry`.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest, AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: SocketAddr = "127.0.0.1:55321".parse().unwrap_or_else(|_| unreachable!());
    let protected_addr: SocketAddr = "127.0.0.1:55322".parse().unwrap_or_else(|_| unreachable!());
    let orphan_addr: SocketAddr = "10.0.0.1:55323".parse().unwrap_or_else(|_| unreachable!());
    let request_addr: SocketAddr = "127.0.0.1:55324".parse().unwrap_or_else(|_| unreachable!());

    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let protected_fingerprint = create_test_fingerprint_with_user_agent(protected_addr, "protected/1.0");
    let orphan_fingerprint = create_test_fingerprint_with_user_agent(orphan_addr, "orphan/1.0");
    let request_fingerprint = create_test_fingerprint_with_user_agent(request_addr, "player/1.0");

    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9701.ts");
    victim_channel.virtual_id = 9701;
    let mut protected_channel = create_test_live_channel("http://provider-1.example/live/9702.ts");
    protected_channel.virtual_id = 9702;
    let mut orphan_channel = create_test_live_channel("http://provider-1.example/live/9703.ts");
    orphan_channel.virtual_id = 9703;

    let mut session_user = ProxyUserCredentials::default();
    session_user.username = "mixed-user".to_string();

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&protected_addr).await;
    app_state.connection_manager.add_connection(&orphan_addr).await;

    // The guard source: a counted stream on the requested channel (9701). It supplies the
    // socket reentry key and is released before the retry is resolved.
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-victim",
            virtual_id: 9701,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9701.ts",
            addr: &victim_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 91,
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &victim_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &victim_channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-victim"),
        })
        .await;

    // The protected candidate: counted, same IP as the retry, different channel.
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &session_user,
            session_token: "session-protected",
            virtual_id: 9702,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9702.ts",
            addr: &protected_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 92,
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &protected_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &protected_channel,
            user_agent: std::borrow::Cow::Borrowed("protected/1.0"),
            session_token: Some("session-protected"),
        })
        .await;

    // The legitimate fallback candidate: an orphan stream (no session), so evicting it
    // frees no counted connection and admission stays exhausted.
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 93,
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &orphan_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &orphan_channel,
            user_agent: std::borrow::Cow::Borrowed("orphan/1.0"),
            session_token: None,
        })
        .await;

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, protected_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let result = resolve_admission_with_strategies(
        &app_state.admission_ctx(),
        AdmissionRequest {
            username: "mixed-user",
            max_connections: 1,
            soft_connections: 0,
            client_ip: &request_fingerprint.client_ip,
            request_addr: &request_fingerprint.addr,
            use_session_admission: true,
            session_token: Some("session-retry"),
            activate_unbound_session: false,
            eviction_reentry_guard: EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9701) },
        },
    )
    .await;

    assert_eq!(result.admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(result.admission.rejection_reason(), Some(AdmissionRejectionReason::UserConnectionsExhausted));
    assert!(!result.admission.is_reentry_suppressed());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_reentry_suppression_does_not_emit_connection_denied() {
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![AdmissionStrategy::EvictUserSameIpOldest, AdmissionStrategy::EvictUserOldest]),
    });

    let victim_addr: SocketAddr = "127.0.0.1:55331".parse().unwrap_or_else(|_| unreachable!());
    let protected_addr: SocketAddr = "127.0.0.1:55332".parse().unwrap_or_else(|_| unreachable!());
    let request_addr: SocketAddr = "127.0.0.1:55333".parse().unwrap_or_else(|_| unreachable!());
    let victim_fingerprint = create_test_fingerprint_with_user_agent(victim_addr, "player/1.0");
    let protected_fingerprint = create_test_fingerprint_with_user_agent(protected_addr, "protected/1.0");
    let request_fingerprint = create_test_fingerprint_with_user_agent(request_addr, "player/1.0");

    let mut victim_channel = create_test_live_channel("http://provider-1.example/live/9711.ts");
    victim_channel.virtual_id = 9711;
    let mut protected_channel = create_test_live_channel("http://provider-1.example/live/9712.ts");
    protected_channel.virtual_id = 9712;

    let mut user = ProxyUserCredentials::default();
    user.username = "event-reentry-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 0;

    app_state.connection_manager.add_connection(&victim_addr).await;
    app_state.connection_manager.add_connection(&protected_addr).await;

    for (token, virtual_id, url, addr, fingerprint, channel, meter_uid) in [
        (
            "session-victim",
            9711,
            "http://provider-1.example/live/9711.ts",
            &victim_addr,
            &victim_fingerprint,
            &victim_channel,
            201_u32,
        ),
        (
            "session-protected",
            9712,
            "http://provider-1.example/live/9712.ts",
            &protected_addr,
            &protected_fingerprint,
            &protected_channel,
            202_u32,
        ),
    ] {
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id,
                provider: "provider-a",
                stream_url: url,
                addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid,
                username: "event-reentry-user",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint,
                provider: "provider-a".intern(),
                stream_channel: channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some(token),
            })
            .await;
    }

    app_state
        .active_users
        .mark_recent_eviction_guard_for_addr(&victim_addr, protected_addr, std::time::Duration::from_secs(3))
        .await;
    app_state.connection_manager.release_connection_as_kicked(&victim_addr).await;

    let (admission, _, _) = crate::api::api_utils::resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &request_fingerprint,
        None,
        "session-event",
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(9711) },
        false,
        false,
    )
    .await;

    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
    assert!(admission.is_reentry_suppressed());
    assert_eq!(connection_denied_count(&app_state), 0, "a suppressed reentry must not emit ConnectionDenied");
    assert_eq!(
        app_state.active_users.reentry_suppressed_total(),
        1,
        "a suppressed reentry must be counted once as a diagnostic"
    );
}

#[tokio::test]
async fn test_real_exhaustion_emits_connection_denied() {
    // Negative control: a genuine connection-limit denial must still emit the event.
    let app_state = create_test_app_state_with_stream_config(crate::model::StreamConfig {
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
        admission_strategies: Some(vec![]),
    });

    let addr: SocketAddr = "127.0.0.1:55341".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = create_test_fingerprint_with_user_agent(addr, "player/1.0");
    let mut user = ProxyUserCredentials::default();
    user.username = "event-limit-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 0;

    app_state.connection_manager.add_connection(&addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: "session-limit",
            virtual_id: 9721,
            provider: "provider-a",
            stream_url: "http://provider-1.example/live/9721.ts",
            addr: &addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    // A stream row is what makes the session count towards `max_connections`.
    let channel = create_test_live_channel("http://provider-1.example/live/9721.ts");
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 211,
            username: "event-limit-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("session-limit"),
        })
        .await;

    let (admission, _, _) = crate::api::api_utils::resolve_playback_request_admission(
        &app_state.admission_ctx(),
        &user,
        &fingerprint,
        None,
        "session-new",
        false,
        EvictionReentryGuard::Session("session-new"),
        false,
        false,
    )
    .await;

    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
    assert_eq!(admission.rejection_reason(), Some(AdmissionRejectionReason::UserConnectionsExhausted));
    assert_eq!(connection_denied_count(&app_state), 1, "a real limit denial must emit ConnectionDenied");
    assert_eq!(
        app_state.active_users.reentry_suppressed_total(),
        0,
        "a real limit denial must not be counted as reentry suppression"
    );
}
