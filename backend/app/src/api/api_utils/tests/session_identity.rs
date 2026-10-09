use super::{
    allows_provider_pool_failover, create_playback_session_fingerprint, create_session_fingerprint,
    create_test_app_state, create_test_fingerprint, create_test_live_channel, get_session_reservation_ttl_secs,
    is_session_based_playback, is_socket_bound_playback_session,
};
use crate::{auth::Fingerprint, model::ProxyUserCredentials};
use shared::{
    defaults::{default_hls_session_ttl_secs, DASH_EXT, HLS_EXT},
    model::{PlaylistItemType, UserConnectionPermission},
    utils::Internable,
};
use std::net::SocketAddr;

#[tokio::test]
async fn test_get_session_reservation_ttl_secs_uses_hls_ttl_for_live_dash() {
    let app_state = create_test_app_state();
    assert_eq!(
        get_session_reservation_ttl_secs(&app_state, PlaylistItemType::LiveDash),
        default_hls_session_ttl_secs()
    );
}

#[test]
fn provider_affinity_policy_matches_stream_types() {
    assert!(!PlaylistItemType::Live.requires_provider_affinity());
    assert!(!PlaylistItemType::LiveUnknown.requires_provider_affinity());
    assert!(PlaylistItemType::LiveHls.requires_provider_affinity());
    assert!(PlaylistItemType::LiveDash.requires_provider_affinity());
    assert!(PlaylistItemType::Video.requires_provider_affinity());
    assert!(PlaylistItemType::Series.requires_provider_affinity());
    assert!(PlaylistItemType::Catchup.requires_provider_affinity());

    assert!(!allows_provider_pool_failover(PlaylistItemType::LiveHls));
    assert!(!allows_provider_pool_failover(PlaylistItemType::LiveDash));
    assert!(allows_provider_pool_failover(PlaylistItemType::Video));
    assert!(allows_provider_pool_failover(PlaylistItemType::Series));
    assert!(!allows_provider_pool_failover(PlaylistItemType::Catchup));
}

#[tokio::test]
async fn socket_bound_playback_sessions_enforce_hard_limits_per_socket() {
    let app_state = create_test_app_state();
    let mut user = ProxyUserCredentials::default();
    user.username = "user1".to_string();
    user.max_connections = 1;

    let first_addr: std::net::SocketAddr = "127.0.0.1:55171".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55172".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let first_token = create_session_fingerprint(&first_fingerprint, &user.username, 5001, true);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let second_token = create_session_fingerprint(&second_fingerprint, &user.username, 5001, true);

    app_state.connection_manager.add_connection(&first_addr).await;
    app_state.connection_manager.add_connection(&second_addr).await;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &first_token,
            virtual_id: 5001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/vod/5001.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 5001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &first_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/vod/5001.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some(&first_token),
        })
        .await;

    let admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, &second_token)
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Exhausted);
}

#[tokio::test]
async fn socket_bound_playback_sessions_still_allow_soft_slots() {
    let app_state = create_test_app_state();
    let mut user = ProxyUserCredentials::default();
    user.username = "soft-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 1;
    user.priority = 0;
    user.soft_priority = 9;

    let first_addr: std::net::SocketAddr = "127.0.0.1:55173".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: std::net::SocketAddr = "127.0.0.1:55174".parse().unwrap_or_else(|_| unreachable!());
    let third_addr: std::net::SocketAddr = "127.0.0.1:55175".parse().unwrap_or_else(|_| unreachable!());
    let first_fingerprint = create_test_fingerprint(first_addr);
    let second_fingerprint = create_test_fingerprint(second_addr);
    let first_token = create_session_fingerprint(&first_fingerprint, &user.username, 6001, true);
    let second_token = create_session_fingerprint(&second_fingerprint, &user.username, 6001, true);
    let third_fingerprint = create_test_fingerprint(third_addr);
    let third_token = create_session_fingerprint(&third_fingerprint, &user.username, 6001, true);

    app_state.connection_manager.add_connection(&first_addr).await;
    app_state.connection_manager.add_connection(&second_addr).await;

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &first_token,
            virtual_id: 6001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/vod/6001.ts",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 6001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &first_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/vod/6001.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some(&first_token),
        })
        .await;

    let second_admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, &second_token)
        .await;
    assert_eq!(second_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(second_admission.kind(), Some(crate::api::model::ConnectionKind::Soft));

    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user: &user,
            session_token: &second_token,
            virtual_id: 6001,
            provider: "provider-a",
            stream_url: "http://provider-1.example/vod/6001.ts",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Soft),
            socket_bound: true,
        })
        .await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 6002,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Soft,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &second_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &create_test_live_channel("http://provider-1.example/vod/6002.ts"),
            user_agent: std::borrow::Cow::Borrowed("ua"),
            session_token: Some(&second_token),
        })
        .await;

    let third_admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, &third_token)
        .await;
    assert_eq!(third_admission.permission(), UserConnectionPermission::Exhausted);
}

#[test]
fn session_based_playback_matches_adaptive_types_and_extensions() {
    assert!(is_session_based_playback(PlaylistItemType::LiveHls, None));
    assert!(is_session_based_playback(PlaylistItemType::LiveDash, None));
    assert!(is_session_based_playback(PlaylistItemType::Live, Some(HLS_EXT)));
    assert!(is_session_based_playback(PlaylistItemType::Live, Some(DASH_EXT)));
    assert!(!is_session_based_playback(PlaylistItemType::Video, None));
}

#[test]
fn create_session_fingerprint_switches_between_logical_and_socket_bound_keys() {
    let fingerprint = create_test_fingerprint("127.0.0.1:55176".parse().unwrap_or_else(|_| unreachable!()));
    let logical = create_session_fingerprint(&fingerprint, "user1", 7001, false);
    let socket_bound = create_session_fingerprint(&fingerprint, "user1", 7001, true);

    assert_ne!(logical, socket_bound);
    assert!(logical.contains(&fingerprint.key));
    assert!(socket_bound.contains(&fingerprint.addr.to_string()));
}

#[test]
fn adaptive_playback_session_fingerprint_is_logical_across_initial_sockets() {
    let Some(first_addr) = "127.0.0.1:55177".parse().ok() else {
        return;
    };
    let Some(second_addr) = "127.0.0.1:55178".parse().ok() else {
        return;
    };
    let first = Fingerprint::new("10.0.0.6|player".to_string(), "10.0.0.6".to_string(), first_addr);
    let second = Fingerprint::new(first.key.clone(), first.client_ip.clone(), second_addr);

    let first_token = create_playback_session_fingerprint(&first, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));
    let second_token =
        create_playback_session_fingerprint(&second, "user1", 7002, PlaylistItemType::Live, Some(HLS_EXT));

    assert_eq!(first_token, second_token);
    assert!(first_token.contains(&first.key));
    assert!(!first_token.contains(&first.addr.to_string()));
    assert!(!second_token.contains(&second.addr.to_string()));
}

#[test]
fn playback_session_fingerprint_keeps_ts_socket_bound_but_vod_logical() {
    let first_addr: SocketAddr = "127.0.0.1:55179".parse().unwrap_or_else(|_| unreachable!());
    let second_addr: SocketAddr = "127.0.0.1:55180".parse().unwrap_or_else(|_| unreachable!());
    let first = Fingerprint::new("10.0.0.7|player".to_string(), "10.0.0.7".to_string(), first_addr);
    let second = Fingerprint::new(first.key.clone(), first.client_ip.clone(), second_addr);

    let first_ts = create_playback_session_fingerprint(&first, "user1", 7003, PlaylistItemType::Live, None);
    let second_ts = create_playback_session_fingerprint(&second, "user1", 7003, PlaylistItemType::Live, None);
    let first_vod = create_playback_session_fingerprint(&first, "user1", 7003, PlaylistItemType::Video, None);
    let second_vod = create_playback_session_fingerprint(&second, "user1", 7003, PlaylistItemType::Video, None);

    assert_ne!(first_ts, second_ts, "plain TS live remains socket-bound");
    assert_eq!(first_vod, second_vod, "VOD remains logical across reopen/seek sockets");
}

#[test]
fn socket_bound_playback_session_matches_only_plain_live_playback() {
    assert!(is_socket_bound_playback_session(PlaylistItemType::Live, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Live, Some(HLS_EXT)));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Live, Some(DASH_EXT)));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::LiveHls, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Video, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Series, None));
    assert!(!is_socket_bound_playback_session(PlaylistItemType::Catchup, None));
}
