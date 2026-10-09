use super::{
    assert_connection_ownership_invariants, assert_no_real_connection_slots, assert_preserved_session_is_uncounted,
    commit_and_preserve_adaptive_session, test_adaptive_channel, test_channel, test_user_credentials,
    ActiveUserConnectionParams, ActiveUserManager, CreateUserSessionParams, PlaybackLifecycle, UserConnectionData,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::{PlaylistItemType, StreamChannel, UserConnectionPermission},
    utils::{current_time_secs, Internable},
};
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tuliprox_core::{
    model::{Config, Fingerprint, ProxyUserCredentials},
    utils::utc_day_from_secs,
};
use tuliprox_repository::GeoIp;

#[tokio::test]
async fn terminate_session_removes_preserved_adaptive_stream() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55412".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-terminate-preserved".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-terminate-preserved".to_string();
    user.max_connections = 1;

    let token = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-terminate-preserved",
            virtual_id: 8003,
            provider: "provider-terminate-preserved",
            stream_url: "http://localhost/test.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 8003,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-terminate-preserved".intern(),
            stream_channel: &test_adaptive_channel(8003),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some(&token),
        })
        .await
        .expect("adaptive stream should be registered");

    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty(), "adaptive stream should be preserved first");

    manager.terminate_session(&user.username, &token).await;

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user data should remain inspectable");
    assert!(connection_data.streams.is_empty(), "terminating a session must remove its preserved adaptive stream");
    assert!(connection_data.sessions.iter().all(|session| session.token != token));
}

pub(in crate::active_user_manager::tests) fn assert_active_stream_kind(
    connection_data: &UserConnectionData,
    stream_uid: u32,
    expected_kind: ConnectionKind,
) {
    assert!(
        connection_data.streams.iter().any(|stream| stream.uid == stream_uid && !stream.preserved),
        "stream {stream_uid} must remain active"
    );
    assert_eq!(connection_data.stream_kinds.get(&stream_uid), Some(&expected_kind));
}

pub(in crate::active_user_manager::tests) fn assert_single_normal_stream_slot(
    connection_data: &UserConnectionData,
    stream_uid: u32,
) {
    assert_eq!(connection_data.connections, 1);
    assert_eq!(connection_data.counts.normal, 1);
    assert_eq!(connection_data.counts.soft, 0);
    assert_active_stream_kind(connection_data, stream_uid, ConnectionKind::Normal);
    assert_connection_ownership_invariants(connection_data);
}

#[tokio::test]
async fn eviction_candidates_include_preserved_adaptive_streams() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55043".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-preserved".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("same-user");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preserved",
            virtual_id: 1043,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 43,
            meter_uid: 0,
            username: "same-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(1043) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-preserved"),
        })
        .await;

    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty(), "adaptive stream should stay logically active");

    let candidates = manager.get_eviction_candidates("same-user", "127.0.0.1").await;
    assert_eq!(candidates.len(), 1, "preserved adaptive streams must remain evictable");
    assert_eq!(candidates[0].addr, addr);
}

#[tokio::test]
async fn test_kicked_release_does_not_preserve_adaptive_stream() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55014".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-adaptive".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-adaptive");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-adaptive",
            virtual_id: 2014,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 14,
            meter_uid: 0,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(2014),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-adaptive"),
        })
        .await;

    let removed = manager.release_connection_as_kicked(&addr).await;
    assert!(removed.addr_removed);
    assert_eq!(removed.removed_streams.len(), 1);
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn test_reused_logical_stream_refreshes_normal_priority() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55023".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-2a".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-prio",
            virtual_id: 2002,
            provider: "provider-a",
            stream_url: "http://localhost/live-prio.ts",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Soft),
            socket_bound: true,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 201,
            meter_uid: 0,
            username: "user1",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: ConnectionKind::Soft,
            priority: 8,
            soft_priority: 8,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(2002),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-prio"),
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 201,
            meter_uid: 0,
            username: "user1",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: ConnectionKind::Soft,
            priority: -7,
            soft_priority: 8,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(2002),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-prio"),
        })
        .await;

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get("user1").unwrap();
    assert_eq!(connection_data.stream_normal_priorities.get(&201), Some(&-7));
}

#[tokio::test]
async fn test_adaptive_session_release_connection_preserves_logical_stream_and_start_time() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55041".parse().unwrap();
    let next_addr: SocketAddr = "127.0.0.1:55042".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-4".to_string(), "127.0.0.1".to_string(), addr);
    let next_fingerprint = Fingerprint::new("fp-key-5".to_string(), "127.0.0.1".to_string(), next_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls",
            virtual_id: 4001,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let first = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 44,
            meter_uid: 144,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(4001) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-hls"),
        })
        .await
        .expect("initial adaptive session should register");

    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty(), "adaptive session should remain logically active");
    assert_eq!(manager.user_connections("user1").await, 0);
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
    assert!(manager.active_streams().await.is_empty());

    let connections = manager.connections.read().await;
    let preserved_stream = connections
        .by_key
        .get("user1")
        .and_then(|data| data.streams.iter().find(|stream| stream.uid == 44))
        .expect("preserved adaptive stream should stay internally tracked");
    assert_eq!(preserved_stream.ts, first.ts);
    assert!(preserved_stream.preserved);
    drop(connections);

    manager.add_connection(&next_addr).await;
    let second = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 55,
            meter_uid: 155,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &next_fingerprint,
            provider: "provider-b".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveDash, ..test_channel(4002) },
            user_agent: Cow::Borrowed("ua-2"),
            session_token: Some("tok-hls"),
        })
        .await
        .expect("adaptive session should reuse logical stream");

    assert_eq!(second.uid, 44);
    assert_eq!(second.ts, first.ts, "adaptive session duration must stay session-based");
    assert_eq!(second.addr, next_addr);
    assert_eq!(second.meter_uid, 155);
    assert_eq!(manager.user_connections("user1").await, 1);

    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 1);
    assert!(!streams[0].preserved);
}

#[tokio::test]
async fn test_release_stream_ignores_preserved_adaptive_entry() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55051".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-6".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls",
            virtual_id: 5001,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 66,
            meter_uid: 166,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(5001) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-hls"),
        })
        .await;

    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty());
    assert!(manager.release_stream(&addr).await.is_none());
}

#[tokio::test]
async fn test_release_stream_without_session_removes_adaptive_stream_instead_of_preserving() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55082".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-9".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 122,
            meter_uid: 222,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(8002) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("missing-session"),
        })
        .await;

    let released = manager.release_stream(&addr).await;
    assert!(released.is_some(), "stream without schedulable expiry must be removed");
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn test_preserved_adaptive_stream_reconnect_across_day_sets_previous_session_id() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55085".parse().unwrap();
    let next_addr: SocketAddr = "127.0.0.1:55086".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-rollover-a".to_string(), "127.0.0.1".to_string(), addr);
    let next_fingerprint = Fingerprint::new("fp-rollover-b".to_string(), "127.0.0.1".to_string(), next_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-rollover",
            virtual_id: 8005,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let first = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 145,
            meter_uid: 245,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(8005) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-rollover"),
        })
        .await
        .expect("initial adaptive session should register");

    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);

    let forced_old_ts = {
        let mut connections = manager.connections.write().await;
        let stream = connections
            .by_key
            .get_mut("user1")
            .unwrap()
            .streams
            .iter_mut()
            .find(|stream| stream.session_token.as_deref() == Some("tok-rollover"))
            .unwrap();
        stream.ts = stream.ts.saturating_sub(86_400);
        stream.ts
    };

    manager.add_connection(&next_addr).await;
    let second = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 146,
            meter_uid: 246,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &next_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveDash, ..test_channel(8005) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-rollover"),
        })
        .await
        .expect("adaptive session should reconnect");

    assert_eq!(second.previous_session_id, Some((forced_old_ts << 32) | u64::from(first.uid)));
    assert!(second.ts > forced_old_ts);
    assert_eq!(utc_day_from_secs(second.ts), utc_day_from_secs(current_time_secs()));
}

#[tokio::test]
async fn catchup_release_connection_preserves_logical_stream_until_session_expires() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55141".parse().unwrap();
    let next_addr: SocketAddr = "127.0.0.1:55142".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-catchup-1".to_string(), "127.0.0.1".to_string(), addr);
    let next_fingerprint = Fingerprint::new("fp-catchup-2".to_string(), "127.0.0.1".to_string(), next_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-catchup");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-catchup",
            virtual_id: 9103,
            provider: "provider-a",
            stream_url: "http://localhost/archive.ts",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let first = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 304,
            meter_uid: 404,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::Catchup, ..test_channel(9103) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-catchup"),
        })
        .await
        .expect("initial catchup stream should register");

    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);
    assert!(
        released.removed_streams.is_empty(),
        "catchup stream should remain logically active between range requests"
    );

    assert_eq!(manager.user_connections(&user.username).await, 0);
    assert!(manager.active_streams().await.is_empty());

    let connections = manager.connections.read().await;
    let preserved_stream = connections
        .by_key
        .get(&user.username)
        .and_then(|data| data.streams.iter().find(|stream| stream.uid == first.uid))
        .expect("preserved catchup stream should stay internally tracked");
    assert!(preserved_stream.preserved);
    drop(connections);

    manager.add_connection(&next_addr).await;
    let second = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 305,
            meter_uid: 405,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &next_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::Catchup, ..test_channel(9103) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-catchup"),
        })
        .await
        .expect("catchup stream should reconnect");

    assert_eq!(second.uid, first.uid);
    assert_eq!(second.started_at, first.started_at);
    assert!(!second.preserved);
}

#[tokio::test]
async fn preserved_reactivation_admission_does_not_create_ownerless_counted_slot() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let user = test_user_credentials("user-preserved-activation", 1, 0);
    let addr: SocketAddr = "127.0.0.1:55195".parse().unwrap();
    let session_token = "tok-preserved-activation";
    let stream_uid = 501;

    commit_and_preserve_adaptive_session(&manager, &user, session_token, stream_uid, addr, ConnectionKind::Normal)
        .await;

    let virtual_admission =
        manager.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(virtual_admission.permission(), UserConnectionPermission::Exhausted);

    let admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            session_token,
        )
        .await;

    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(ConnectionKind::Normal));
    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_preserved_session_is_uncounted(connection_data, session_token, stream_uid);
    assert_no_real_connection_slots(connection_data);
}

#[tokio::test]
async fn preserved_reactivation_admission_then_kicked_release_removes_state_without_ghost_counter() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let user = test_user_credentials("user-preserved-eviction", 1, 0);
    let addr: SocketAddr = "127.0.0.1:55201".parse().unwrap();
    let session_token = "tok-preserved-eviction";
    let stream_uid = 601;

    commit_and_preserve_adaptive_session(&manager, &user, session_token, stream_uid, addr, ConnectionKind::Normal)
        .await;
    let admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            session_token,
        )
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_preserved_session_is_uncounted(connection_data, session_token, stream_uid);
        assert_no_real_connection_slots(connection_data);
    }

    let released = manager.release_connection_as_kicked(&addr).await;
    assert!(released.addr_removed);
    assert_eq!(released.removed_streams.len(), 1);
    let removed_stream = released.removed_streams.first().expect("kicked release must remove the preserved stream");
    assert_eq!(removed_stream.uid, stream_uid);
    assert!(removed_stream.preserved);
    assert_eq!(removed_stream.session_token.as_deref(), Some(session_token));

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert!(connection_data.streams.iter().all(|stream| stream.uid != stream_uid));
    assert!(connection_data.sessions.iter().all(|session| session.token != session_token));
    assert!(!connection_data.stream_kinds.contains_key(&stream_uid));
    assert_no_real_connection_slots(connection_data);
    drop(connections);
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
}

#[tokio::test]
async fn preserved_reactivation_admission_then_lease_idle_cleanup_leaves_counters_at_zero() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let user = test_user_credentials("user-preserved-idle-cleanup", 1, 0);
    let addr: SocketAddr = "127.0.0.1:55202".parse().unwrap();
    let session_token = "tok-preserved-idle-cleanup";
    let stream_uid = 602;

    commit_and_preserve_adaptive_session(&manager, &user, session_token, stream_uid, addr, ConnectionKind::Normal)
        .await;
    let admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            session_token,
        )
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_preserved_session_is_uncounted(connection_data, session_token, stream_uid);
        assert_no_real_connection_slots(connection_data);
    }

    // Shared-HLS lease-idle cleanup delegates to this manager operation.
    let counter_changed = manager.release_session_streams_and_counted_reservation(&user.username, session_token).await;
    assert!(!counter_changed, "removing an uncounted preserved stream must not change real counters");

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert!(connection_data.streams.iter().all(|stream| stream.uid != stream_uid));
    assert!(connection_data
        .sessions
        .iter()
        .find(|session| session.token == session_token)
        .is_some_and(|session| session.lifecycle == PlaybackLifecycle::Preserved));
    assert!(!connection_data.stream_kinds.contains_key(&stream_uid));
    assert_no_real_connection_slots(connection_data);
    drop(connections);
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
}

#[tokio::test]
async fn repeated_preserved_reactivation_cleanup_does_not_accumulate_connections() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let user = test_user_credentials("user-preserved-repeat", 4, 0);
    for (session_token, stream_uid, addr) in
        [("tok-preserved-repeat-one", 603, "127.0.0.1:55203"), ("tok-preserved-repeat-two", 604, "127.0.0.1:55204")]
    {
        let addr = addr.parse().unwrap();
        commit_and_preserve_adaptive_session(&manager, &user, session_token, stream_uid, addr, ConnectionKind::Normal)
            .await;

        let admission = manager
            .connection_admission_for_session_activation(
                &user.username,
                user.max_connections,
                user.soft_connections,
                session_token,
            )
            .await;
        assert_eq!(admission.permission(), UserConnectionPermission::Allowed);

        let counter_changed =
            manager.release_session_streams_and_counted_reservation(&user.username, session_token).await;
        assert!(!counter_changed, "cleanup must not release a slot that was never committed");

        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_no_real_connection_slots(connection_data);
        drop(connections);
        assert_eq!(manager.active_users_and_connections().await, (0, 0));
    }
}

#[tokio::test]
async fn preserved_soft_reactivation_and_cleanup_leave_normal_slot_unchanged() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let user = test_user_credentials("user-preserved-soft", 1, 1);
    let normal_addr: SocketAddr = "127.0.0.1:55206".parse().unwrap();
    let normal_fingerprint =
        Fingerprint::new("fp-preserved-soft-normal".to_string(), normal_addr.ip().to_string(), normal_addr);
    let soft_addr: SocketAddr = "127.0.0.1:55207".parse().unwrap();
    let normal_stream_uid = 606;
    let session_token = "tok-preserved-soft";
    let soft_stream_uid = 607;

    manager.add_connection(&normal_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: normal_stream_uid,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &normal_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(normal_stream_uid),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("normal stream should bind");
    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_single_normal_stream_slot(connection_data, normal_stream_uid);
    }

    commit_and_preserve_adaptive_session(
        &manager,
        &user,
        session_token,
        soft_stream_uid,
        soft_addr,
        ConnectionKind::Soft,
    )
    .await;
    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_preserved_session_is_uncounted(connection_data, session_token, soft_stream_uid);
        assert_single_normal_stream_slot(connection_data, normal_stream_uid);
    }

    let admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            session_token,
        )
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(ConnectionKind::Soft));

    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_preserved_session_is_uncounted(connection_data, session_token, soft_stream_uid);
        assert_single_normal_stream_slot(connection_data, normal_stream_uid);
    }

    let counter_changed = manager.release_session_streams_and_counted_reservation(&user.username, session_token).await;
    assert!(!counter_changed, "preserved soft cleanup must not release an uncommitted slot");
    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert!(connection_data.streams.iter().all(|stream| stream.uid != soft_stream_uid));
        assert!(!connection_data.stream_kinds.contains_key(&soft_stream_uid));
        assert!(connection_data
            .sessions
            .iter()
            .find(|session| session.token == session_token)
            .is_some_and(|session| session.lifecycle == PlaybackLifecycle::Preserved));
        assert_single_normal_stream_slot(connection_data, normal_stream_uid);
    }

    manager.release_stream_by_uid(&normal_addr, normal_stream_uid).await.expect("normal stream should release");
    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert!(connection_data.streams.is_empty());
    assert!(connection_data.stream_kinds.is_empty());
    assert_no_real_connection_slots(connection_data);
    drop(connections);
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
}

#[tokio::test]
async fn get_eviction_candidates_keeps_preserved_streams_evictable() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55300".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key".to_string(), "192.168.1.100".to_string(), addr);
    let username = "user-eviction-addr";
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user.max_connections = 1;
    user.soft_connections = 0;

    // Create session first (HLS type = preserved after release)
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preserved-1",
            virtual_id: 5001,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Create stream + register connection
    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 1,
            meter_uid: 0,
            username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(5001),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-preserved-1"),
        })
        .await
        .expect("first stream");
    assert_eq!(manager.user_connections(username).await, 1);

    // Release -> stream becomes preserved, session becomes uncounted
    manager.release_stream(&addr).await;
    assert_eq!(manager.user_connections(username).await, 0, "preserved stream should not count");

    let candidates = manager.get_eviction_candidates(username, "192.168.1.100").await;
    assert!(
        candidates.iter().any(|candidate| candidate.addr == addr),
        "preserved stream should remain a direct eviction candidate"
    );
}

#[tokio::test]
async fn connection_admission_treats_preserved_stream_as_reserved_capacity() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55305".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-preserved-admission".to_string(), "192.168.1.100".to_string(), addr);
    let username = "user-preserved-admission";
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preserved-admission",
            virtual_id: 6000,
            provider: "provider-a",
            stream_url: "http://localhost/live-preserved.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 6000,
            meter_uid: 0,
            username,
            max_connections: user.max_connections,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(6000),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-preserved-admission"),
        })
        .await
        .expect("preserved stream should be created");

    manager.release_connection(&addr).await;
    assert_eq!(manager.user_connections(username).await, 0, "preserved stream stays uncounted for active snapshots");

    let admission = manager.connection_admission(username, user.max_connections, 0).await;
    assert_eq!(
        admission.permission(),
        UserConnectionPermission::Exhausted,
        "a preserved stream should still reserve capacity against unrelated playback admissions"
    );
}
