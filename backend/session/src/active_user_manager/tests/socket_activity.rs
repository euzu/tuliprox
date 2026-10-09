use super::{
    test_adaptive_channel, test_channel, test_series_channel, test_user_credentials, ActiveUserConnectionParams,
    ActiveUserManager, CreateUserSessionParams, PlaybackLifecycle, SocketRegistration, ANON_SOCKET_TTL,
    DEFAULT_ACTIVE_SOCKET_TTL_SECS, USER_GC_TTL,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::{
        ActiveUserConnectionChange, EventMessage, PlaylistItemType, StreamChannel, UserConnectionPermission,
        XtreamCluster,
    },
    utils::{current_time_secs, Internable},
};
use std::{
    borrow::Cow,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

#[tokio::test]
async fn test_multi_session_same_addr_counts_and_releases_individually() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55001".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key".to_string(), "127.0.0.1".to_string(), addr);
    let username = "user1";

    manager.add_connection(&addr).await;

    let first = manager
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
            stream_channel: &test_channel(1001),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-1"),
        })
        .await;
    assert!(first.is_some());
    assert_eq!(manager.user_connections(username).await, 1);
    assert_eq!(manager.connection_permission(username, 1, 0).await, UserConnectionPermission::Exhausted);

    let second = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 2,
            meter_uid: 0,
            username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-b".intern(),
            stream_channel: &test_channel(1002),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-2"),
        })
        .await;
    assert!(second.is_some());
    assert_eq!(manager.user_connections(username).await, 2);

    assert!(manager.release_stream(&addr).await.is_some());
    assert_eq!(manager.user_connections(username).await, 1);

    assert!(manager.release_stream(&addr).await.is_some());
    assert_eq!(manager.user_connections(username).await, 0);
}

/// `terminate_sessions_for_addr` expires all sessions at a given addr and releases counted leases.
#[tokio::test]
async fn terminate_sessions_for_addr_expires_all_sessions_at_addr() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr_kick: SocketAddr = "127.0.0.1:55420".parse().unwrap();
    let addr_keep: SocketAddr = "127.0.0.1:55421".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "user-kick-addr".to_string();
    user.max_connections = 4;

    // Create session at kicked addr.
    let tok_kick = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-kick",
            virtual_id: 1,
            provider: "provider-a",
            stream_url: "http://provider/live/1.m3u8",
            addr: &addr_kick,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Create session at kept addr.
    let tok_keep = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-keep",
            virtual_id: 2,
            provider: "provider-b",
            stream_url: "http://provider/live/2.m3u8",
            addr: &addr_keep,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Mark both sessions as counted and active.
    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.get_mut(&user.username).unwrap();
        for session in &mut data.sessions {
            // Simulate counted state by setting lifecycle to Active.
            session.lifecycle = PlaybackLifecycle::Active;
        }
        data.increment_kind(ConnectionKind::Normal);
        data.increment_kind(ConnectionKind::Normal);
    }

    assert_eq!(manager.user_connections(&user.username).await, 2);

    // Kick the addr — should terminate only the sessions at that addr.
    manager.terminate_sessions_for_addr(&user.username, &addr_kick).await;

    // Session at kicked addr should be gone.
    assert!(
        manager.get_and_update_user_session(&user.username, &tok_kick).await.is_none(),
        "kicked session should be removed"
    );

    // Session at kept addr should remain.
    let kept =
        manager.get_and_update_user_session(&user.username, &tok_keep).await.expect("kept session should still exist");
    assert_eq!(kept.token, tok_keep);
    assert_eq!(kept.addr, addr_keep);

    // Connection count should drop by 1.
    assert_eq!(manager.user_connections(&user.username).await, 1);
}

#[tokio::test]
async fn eviction_candidates_ignore_ambiguous_socket_addrs() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let shared_addr: SocketAddr = "127.0.0.1:55031".parse().unwrap();
    let unique_addr: SocketAddr = "127.0.0.1:55032".parse().unwrap();
    let shared_fp = Fingerprint::new("fp-shared".to_string(), "127.0.0.1".to_string(), shared_addr);
    let unique_fp = Fingerprint::new("fp-unique".to_string(), "127.0.0.1".to_string(), unique_addr);

    manager.add_connection(&shared_addr).await;
    manager.add_connection(&unique_addr).await;

    // Create sessions first so update_connection can mark them as counted.
    let user = test_user_credentials("same-user", 3, 0);
    for (token, addr, channel_id) in
        [("tok-31", shared_addr, 1031u32), ("tok-32", shared_addr, 1032), ("tok-33", unique_addr, 1033)]
    {
        manager
            .create_user_session(crate::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: channel_id,
                provider: "provider-a",
                stream_url: "",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
    }

    // update_connection marks the session as counted.
    for (uid, token, fp, channel_id) in [(31, "tok-31", &shared_fp, 1031u32), (32, "tok-32", &shared_fp, 1032)] {
        manager
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: 0,
                username: "same-user",
                max_connections: 3,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: fp,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(channel_id),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some(token),
            })
            .await;
    }

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 33,
            meter_uid: 0,
            username: "same-user",
            max_connections: 3,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &unique_fp,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1033),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-33"),
        })
        .await;

    let candidates = manager.get_eviction_candidates("same-user", "127.0.0.1").await;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].addr, unique_addr);
}

#[tokio::test]
async fn kicked_release_removes_preserved_adaptive_stream_without_socket_registration() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55017".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-preserved-kick".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-preserved-kick");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preserved-kick",
            virtual_id: 2017,
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
            uid: 17,
            meter_uid: 0,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(2017),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-preserved-kick"),
        })
        .await;

    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty());
    assert!(manager.active_streams().await.is_empty());

    let kicked = manager.release_connection_as_kicked(&addr).await;
    assert!(kicked.addr_removed);
    assert_eq!(kicked.removed_streams.len(), 1);
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn test_same_session_token_on_new_addr_reuses_logical_connection() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let first_addr: SocketAddr = "127.0.0.1:55021".parse().unwrap();
    let second_addr: SocketAddr = "127.0.0.1:55022".parse().unwrap();
    let first = Fingerprint::new("fp-key-1".to_string(), "127.0.0.1".to_string(), first_addr);
    let second = Fingerprint::new("fp-key-2".to_string(), "127.0.0.1".to_string(), second_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&first_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls",
            virtual_id: 2001,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 0,
            meter_uid: 0,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &first,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(2001),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-hls"),
        })
        .await;

    assert_eq!(
        manager.connection_permission_for_session("user1", 1, 0, "tok-hls").await,
        UserConnectionPermission::Allowed
    );

    manager.add_connection(&second_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 0,
            meter_uid: 0,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &second,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(2001),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-hls"),
        })
        .await;

    assert_eq!(manager.user_connections("user1").await, 1);

    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].addr, second_addr);
    assert_eq!(streams[0].session_token.as_deref(), Some("tok-hls"));
}

#[tokio::test]
async fn adaptive_session_stream_cleanup_addrs_excludes_manifest_addr_and_current_addr() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let manifest_addr: SocketAddr = "127.0.0.1:55091".parse().unwrap();
    let first_segment_addr: SocketAddr = "10.41.41.89:55092".parse().unwrap();
    let next_segment_addr: SocketAddr = "10.41.41.89:55093".parse().unwrap();
    let first_segment = Fingerprint::new("fp-segment-1".to_string(), "10.41.41.89".to_string(), first_segment_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&manifest_addr).await;
    manager.add_connection(&first_segment_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-cleanup",
            virtual_id: 2002,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &manifest_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 0,
            meter_uid: 0,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &first_segment,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(2002) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-hls-cleanup"),
        })
        .await;

    assert_eq!(
        manager.adaptive_session_stream_cleanup_addrs("user1", "tok-hls-cleanup", &next_segment_addr).await,
        vec![first_segment_addr]
    );
}

#[tokio::test]
async fn adaptive_session_stream_cleanup_addrs_falls_back_to_same_ip_session_addrs() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let manifest_addr: SocketAddr = "127.0.0.1:55101".parse().unwrap();
    let first_segment_addr: SocketAddr = "10.41.41.89:55102".parse().unwrap();
    let next_segment_addr: SocketAddr = "10.41.41.89:55103".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user2");
    user.max_connections = 1;

    manager.add_connection(&manifest_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-cleanup-fallback",
            virtual_id: 2003,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &manifest_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager.update_session_addr("user2", "tok-hls-cleanup-fallback", &first_segment_addr).await;
    manager.update_session_addr("user2", "tok-hls-cleanup-fallback", &next_segment_addr).await;

    assert_eq!(
        manager.adaptive_session_stream_cleanup_addrs("user2", "tok-hls-cleanup-fallback", &next_segment_addr).await,
        vec![first_segment_addr]
    );
}

#[tokio::test]
async fn socket_bound_live_streams_with_colliding_token_are_tracked_separately() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let Some(addr) = "127.0.0.1:55032".parse::<SocketAddr>().ok() else {
        return;
    };
    let fingerprint = Fingerprint::new("fp-key-colliding".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    let first = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 31,
            meter_uid: 301,
            username: "user1",
            max_connections: 0,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(3003),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-live-colliding"),
        })
        .await;
    let second = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 32,
            meter_uid: 302,
            username: "user1",
            max_connections: 0,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-b".intern(),
            stream_channel: &test_channel(3003),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-live-colliding"),
        })
        .await;

    assert!(first.is_some());
    assert!(second.is_some());

    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 2);
    assert!(streams.iter().any(|stream| stream.uid == 31));
    assert!(streams.iter().any(|stream| stream.uid == 32));
}

#[tokio::test]
async fn release_stream_by_uid_finds_original_user_after_shared_addr_owner_changes() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55034".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-cross-user-stream".to_string(), "127.0.0.1".to_string(), addr);
    manager.add_connection(&addr).await;

    for (uid, username) in [(43, "user-a"), (44, "user-b")] {
        manager
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: 0,
                username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_series_channel(3005),
                user_agent: Cow::Borrowed("ua"),
                session_token: None,
            })
            .await
            .expect("direct Series stream should register");
    }

    assert_eq!(manager.active_users_and_connections().await, (2, 2));
    assert_eq!(manager.active_streams().await.len(), 2);

    let removed = manager.release_stream_by_uid(&addr, 43).await;
    assert!(removed.as_ref().is_some_and(|stream| stream.uid == 43));
    assert_eq!(manager.active_users_and_connections().await, (1, 1));
    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].uid, 44);

    assert!(manager.release_stream_by_uid(&addr, 44).await.is_some());
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn release_connection_cleans_every_user_stream_for_reused_addr_only() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let reused_addr: SocketAddr = "127.0.0.1:55035".parse().unwrap();
    let unrelated_addr: SocketAddr = "127.0.0.1:55036".parse().unwrap();
    let reused_fingerprint = Fingerprint::new("fp-cross-user-socket".to_string(), "127.0.0.1".to_string(), reused_addr);
    let unrelated_fingerprint =
        Fingerprint::new("fp-unrelated-socket".to_string(), "127.0.0.1".to_string(), unrelated_addr);
    manager.add_connection(&reused_addr).await;
    manager.add_connection(&unrelated_addr).await;

    for (uid, username, fingerprint) in [
        (45, "user-a", &reused_fingerprint),
        (46, "user-b", &reused_fingerprint),
        (47, "user-a", &unrelated_fingerprint),
    ] {
        manager
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: 0,
                username,
                max_connections: 2,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_series_channel(3006 + uid),
                user_agent: Cow::Borrowed("ua"),
                session_token: None,
            })
            .await
            .expect("direct Series stream should register");
    }

    let released = manager.release_connection(&reused_addr).await;
    let mut removed_uids = released.removed_streams.iter().map(|stream| stream.uid).collect::<Vec<_>>();
    removed_uids.sort_unstable();
    assert!(released.addr_removed);
    assert_eq!(removed_uids, vec![45, 46]);
    assert_eq!(manager.active_users_and_connections().await, (1, 1));
    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].uid, 47);

    manager.release_connection(&unrelated_addr).await;
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn stale_anonymous_socket_registration_is_pruned_by_gc() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let stale_addr: SocketAddr = "127.0.0.1:55011".parse().unwrap();
    let fresh_addr: SocketAddr = "127.0.0.1:55012".parse().unwrap();

    manager.add_connection(&stale_addr).await;
    {
        let mut connections = manager.connections.write().await;
        let registration = connections.key_by_addr.get_mut(&stale_addr).expect("socket registration should exist");
        registration.ts = registration.ts.saturating_sub(ANON_SOCKET_TTL + 1);
    }

    if let Some(gc_ts) = &manager.gc_ts {
        gc_ts.store(current_time_secs().saturating_sub(USER_GC_TTL + 1), Ordering::Release);
    }

    manager.add_connection(&fresh_addr).await;

    let connections = manager.connections.read().await;
    assert!(!connections.key_by_addr.contains_key(&stale_addr));
    assert!(connections.key_by_addr.contains_key(&fresh_addr));
}

#[tokio::test]
async fn touch_http_activity_refreshes_session_and_registration_without_stream() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55024".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-http-touch",
            virtual_id: 9302,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let previous_ts = {
        let mut connections = manager.connections.write().await;
        let previous_ts = {
            let registration = connections.key_by_addr.get_mut(&addr).expect("registration should exist");
            registration.ts = registration.ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 5);
            registration.ts
        };
        let connection_data = connections.by_key.get_mut("user1").expect("user should exist");
        connection_data.sessions[0].ts =
            connection_data.sessions[0].ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 5);
        previous_ts
    };

    manager.touch_http_activity("user1", "tok-http-touch", &addr).await;

    let connections = manager.connections.read().await;
    let registration = connections.key_by_addr.get(&addr).expect("registration should still exist");
    let connection_data = connections.by_key.get("user1").expect("user should still exist");
    assert!(registration.ts > previous_ts);
    assert!(connection_data.sessions[0].ts >= registration.ts);
}

#[tokio::test]
async fn touch_http_activity_does_not_reset_stream_started_at_ts() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr1: SocketAddr = "127.0.0.1:55030".parse().unwrap();
    let addr2: SocketAddr = "127.0.0.1:55031".parse().unwrap();
    let fingerprint = Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), addr1);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-touch-ts".to_string();

    manager.add_connection(&addr1).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-ts",
            virtual_id: 7777,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr1,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Simulate first HLS segment: creates the stream entry with ts = now
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 601,
            meter_uid: 701,
            username: "user-touch-ts",
            max_connections: 0,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(7777),
            user_agent: Cow::Borrowed("player/1.0"),
            session_token: Some("tok-hls-ts"),
        })
        .await
        .expect("stream should be created");

    // Record the original stream start timestamp
    let original_ts = {
        let connections = manager.connections.read().await;
        connections
            .by_key
            .get("user-touch-ts")
            .and_then(|data| data.streams.iter().find(|s| s.session_token.as_deref() == Some("tok-hls-ts")))
            .map(|s| s.ts)
            .expect("stream should exist")
    };

    // Simulate manifest re-fetch (touch_http_activity called with a new addr)
    manager.touch_http_activity("user-touch-ts", "tok-hls-ts", &addr2).await;

    // stream.ts must NOT have been reset — it represents session start time shown as Duration
    let connections = manager.connections.read().await;
    let stream = connections
        .by_key
        .get("user-touch-ts")
        .and_then(|data| data.streams.iter().find(|s| s.session_token.as_deref() == Some("tok-hls-ts")))
        .expect("stream should still exist");
    assert_eq!(stream.ts, original_ts, "touch_http_activity must not reset the stream start timestamp");
    // Lightweight manifest activity must not move the active stream socket.
    assert_eq!(stream.addr, addr1, "touch_http_activity must not replace the active stream addr");
}

#[tokio::test]
async fn touch_http_activity_does_not_migrate_adaptive_stream_to_manifest_addr_on_close() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let mut events = event_manager.get_event_channel();

    let segment_addr: SocketAddr = "127.0.0.1:55032".parse().unwrap();
    let manifest_addr: SocketAddr = "127.0.0.1:55033".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-hls-segment".to_string(), "127.0.0.1".to_string(), segment_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-hls-manifest-touch".to_string();
    user.max_connections = 1;

    manager.add_connection(&segment_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-manifest-touch",
            virtual_id: 7788,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &segment_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 602,
            meter_uid: 702,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(7788),
            user_agent: Cow::Borrowed("player/1.0"),
            session_token: Some("tok-hls-manifest-touch"),
        })
        .await
        .expect("stream should be created");

    manager.touch_http_activity(&user.username, "tok-hls-manifest-touch", &manifest_addr).await;

    let released = manager.release_connection(&segment_addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty(), "adaptive close should preserve without history removal");
    assert_eq!(manager.user_connections(&user.username).await, 0);
    assert!(
        manager.active_streams().await.is_empty(),
        "preserved rows stay out of active_streams (use panel_streams for StatusCheck)"
    );
    let panel = manager.panel_streams().await;
    assert_eq!(panel.len(), 1, "preserved adaptive/catchup session rows stay in panel snapshots");
    assert!(panel[0].preserved);
    assert_eq!(panel[0].session_token.as_deref(), Some("tok-hls-manifest-touch"));

    let connections = manager.connections.read().await;
    let data = connections.by_key.get(&user.username).expect("user should remain for preserved session");
    let stream = data
        .streams
        .iter()
        .find(|stream| stream.session_token.as_deref() == Some("tok-hls-manifest-touch"))
        .expect("preserved stream should remain internally tracked");
    assert!(stream.preserved);
    assert_eq!(stream.addr, segment_addr, "closed segment must not migrate to manifest addr");
    assert!(!data.sessions[0].active_addrs.contains(&manifest_addr));
    drop(connections);

    let mut saw_preserved_update = false;
    while let Ok(event) = events.try_recv() {
        if matches!(event, EventMessage::ActiveUser(ActiveUserConnectionChange::Updated(stream)) if stream.addr == segment_addr && stream.preserved)
        {
            saw_preserved_update = true;
        }
    }
    assert!(saw_preserved_update, "preserving a stream must notify the frontend so adaptive TTL cleanup can hide it");
}

#[tokio::test]
async fn clear_unbound_session_addr_prunes_manifest_addr_while_stream_is_active_elsewhere() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let segment_addr: SocketAddr = "127.0.0.1:55034".parse().unwrap();
    let manifest_addr: SocketAddr = "127.0.0.1:55035".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-hls-segment-2".to_string(), "127.0.0.1".to_string(), segment_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-hls-manifest-clear".to_string();
    user.max_connections = 1;

    manager.add_connection(&segment_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-manifest-clear",
            virtual_id: 7789,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &segment_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 603,
            meter_uid: 703,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(7789),
            user_agent: Cow::Borrowed("player/1.0"),
            session_token: Some("tok-hls-manifest-clear"),
        })
        .await
        .expect("stream should be created");

    manager.add_connection(&manifest_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-manifest-clear",
            virtual_id: 7789,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &manifest_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    manager.clear_unbound_session_addr(&user.username, "tok-hls-manifest-clear", &manifest_addr).await;

    let connections = manager.connections.read().await;
    assert!(!connections.key_by_addr.contains_key(&manifest_addr));
    let data = connections.by_key.get(&user.username).expect("user should exist");
    assert_eq!(data.streams[0].addr, segment_addr);
    assert!(!data.sessions[0].active_addrs.contains(&manifest_addr));
}

#[tokio::test]
async fn clear_unbound_session_addr_prunes_touch_only_manifest_addr() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let segment_addr: SocketAddr = "127.0.0.1:55036".parse().unwrap();
    let manifest_addr: SocketAddr = "127.0.0.1:55037".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-hls-segment-3".to_string(), "127.0.0.1".to_string(), segment_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-hls-manifest-touch-clear".to_string();
    user.max_connections = 1;

    manager.add_connection(&segment_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-manifest-touch-clear",
            virtual_id: 7790,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &segment_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 604,
            meter_uid: 704,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(7790),
            user_agent: Cow::Borrowed("player/1.0"),
            session_token: Some("tok-hls-manifest-touch-clear"),
        })
        .await
        .expect("stream should be created");

    manager.touch_http_activity(&user.username, "tok-hls-manifest-touch-clear", &manifest_addr).await;
    manager.clear_unbound_session_addr(&user.username, "tok-hls-manifest-touch-clear", &manifest_addr).await;

    let connections = manager.connections.read().await;
    assert!(!connections.key_by_addr.contains_key(&manifest_addr));
    let data = connections.by_key.get(&user.username).expect("user should exist");
    assert_eq!(data.streams[0].addr, segment_addr);
    assert_eq!(data.sessions[0].addr, segment_addr);
    assert!(!data.sessions[0].active_addrs.contains(&manifest_addr));
}

#[tokio::test]
async fn touch_socket_activity_refreshes_registration_without_resetting_stream_start() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55041".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-vod-touch".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-vod-touch".to_string();
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    let mut channel = test_channel(8889);
    channel.item_type = PlaylistItemType::Video;
    channel.cluster = XtreamCluster::Video;
    channel.url = "http://localhost/movie-2.mkv".intern();

    let stream = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 603,
            meter_uid: 703,
            username: "user-vod-touch",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("player/1.0"),
            session_token: None,
        })
        .await
        .expect("vod stream should be created");

    let stale_registration_ts = {
        let mut connections = manager.connections.write().await;
        let registration = connections.key_by_addr.get_mut(&addr).expect("registration should exist");
        registration.ts = registration.ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 5);
        registration.ts
    };

    manager.touch_socket_activity(&addr).await;

    let (refreshed_registration_ts, stream_started_at) = {
        let connections = manager.connections.read().await;
        let registration_ts = connections.key_by_addr.get(&addr).expect("registration should still exist").ts;
        let stream_started_at = connections
            .by_key
            .get("user-vod-touch")
            .and_then(|data| data.streams.iter().find(|active| active.uid == stream.uid))
            .expect("stream should still exist")
            .ts;
        (registration_ts, stream_started_at)
    };

    assert!(refreshed_registration_ts > stale_registration_ts);
    assert_eq!(stream_started_at, stream.ts, "body activity must not reset visible stream duration");
}

#[tokio::test]
async fn update_session_addr_prunes_previous_registration_for_socket_bound_session() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let old_addr: SocketAddr = "127.0.0.1:55121".parse().unwrap();
    let new_addr: SocketAddr = "127.0.0.1:55122".parse().unwrap();
    let old_fingerprint = Fingerprint::new("fp-old".to_string(), "127.0.0.1".to_string(), old_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&old_addr).await;
    manager.add_connection(&new_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-move",
            virtual_id: 9101,
            provider: "provider-a",
            stream_url: "http://localhost/live.ts",
            addr: &old_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 301,
            meter_uid: 401,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &old_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::Live, ..test_channel(9101) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-move"),
        })
        .await
        .expect("initial live stream should register");

    manager.update_session_addr("user1", "tok-move", &new_addr).await;

    let connections = manager.connections.read().await;
    assert!(
        !connections.key_by_addr.contains_key(&old_addr),
        "previous range-request socket registration should be pruned once the session moved"
    );
    assert!(connections.key_by_addr.contains_key(&new_addr));

    let connection_data = connections.by_key.get("user1").expect("user connection data");
    assert_eq!(connection_data.sessions.len(), 1);
    assert_eq!(connection_data.sessions[0].addr, new_addr);
    assert_eq!(connection_data.streams.len(), 1);
    assert_eq!(connection_data.streams[0].addr, new_addr);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn vod_session_survives_overlapping_and_seek_sockets() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let base_addr: SocketAddr = "127.0.0.1:55131".parse().unwrap();
    let range_addr: SocketAddr = "127.0.0.1:55132".parse().unwrap();
    let seek_addr: SocketAddr = "127.0.0.1:55133".parse().unwrap();
    let base_fingerprint = Fingerprint::new("fp-vod-base".to_string(), "127.0.0.1".to_string(), base_addr);
    let range_fingerprint = Fingerprint::new("fp-vod-range".to_string(), "127.0.0.1".to_string(), range_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&base_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-vod",
            virtual_id: 9102,
            provider: "provider-a",
            stream_url: "http://localhost/movie.mkv",
            addr: &base_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 302,
            meter_uid: 402,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &base_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::Video, ..test_channel(9102) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-vod"),
        })
        .await
        .expect("initial vod stream should register");

    manager.add_connection(&range_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 303,
            meter_uid: 403,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &range_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::Video, ..test_channel(9102) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-vod"),
        })
        .await
        .expect("overlapping range request should reuse the same vod session");

    assert_eq!(manager.user_connections("user1").await, 1);
    assert!(manager.release_stream(&range_addr).await.is_none());
    let released = manager.release_connection(&range_addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty());

    {
        let connections = manager.connections.read().await;
        assert!(connections.key_by_addr.contains_key(&base_addr));
        let connection_data = connections.by_key.get("user1").expect("user connection data");
        assert_eq!(connection_data.sessions[0].addr, base_addr);
        assert_eq!(connection_data.streams[0].addr, base_addr);
    }

    manager.add_connection(&seek_addr).await;
    manager.update_session_addr("user1", "tok-vod", &seek_addr).await;

    {
        let connections = manager.connections.read().await;
        assert!(
            connections.key_by_addr.contains_key(&base_addr),
            "existing vod socket must remain registered while the session spans multiple requests"
        );
        assert!(connections.key_by_addr.contains_key(&seek_addr));

        let connection_data = connections.by_key.get("user1").expect("user connection data");
        assert_eq!(connection_data.sessions[0].addr, seek_addr);
        assert_eq!(connection_data.streams[0].addr, seek_addr);
    }

    assert!(manager.release_stream(&seek_addr).await.is_none());
    let released = manager.release_connection(&seek_addr).await;
    assert!(released.addr_removed);
    assert!(released.removed_streams.is_empty());

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get("user1").expect("user connection data");
    assert_eq!(connection_data.sessions[0].addr, base_addr);
    assert_eq!(connection_data.streams[0].addr, base_addr);
}

#[tokio::test]
async fn clear_unbound_session_addr_prunes_manifest_addr_without_stream() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let first_addr: SocketAddr = "127.0.0.1:55185".parse().unwrap();
    let second_addr: SocketAddr = "127.0.0.1:55186".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-clear-addr");
    user.max_connections = 1;

    manager.add_connection(&first_addr).await;
    manager.add_connection(&second_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-clear-addr",
            virtual_id: 9206,
            provider: "provider-a",
            stream_url: "http://localhost/live-clear.m3u8",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-clear-addr",
            virtual_id: 9206,
            provider: "provider-a",
            stream_url: "http://localhost/live-clear.m3u8",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    manager.clear_unbound_session_addr(&user.username, "tok-clear-addr", &second_addr).await;

    let connections = manager.connections.read().await;
    let session = connections
        .by_key
        .get(&user.username)
        .and_then(|connection_data| connection_data.sessions.iter().find(|session| session.token == "tok-clear-addr"))
        .expect("session should remain");
    assert_eq!(session.addr, first_addr);
    assert_eq!(session.active_addrs, vec![first_addr]);
}

#[tokio::test]
async fn get_eviction_candidates_does_not_count_preserved_streams_in_addr_counts() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55801".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-preserved-no-count".to_string(), "10.0.0.5".to_string(), addr);
    let username = "user-preserved-addr-count";
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-preserved-addr-count",
            virtual_id: 7000,
            provider: "provider-preserved",
            stream_url: "http://localhost/preserved.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 7000,
            meter_uid: 0,
            username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-preserved".intern(),
            stream_channel: &test_adaptive_channel(7000),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-preserved-addr-count"),
        })
        .await
        .expect("stream should be created");

    // Release -> stream becomes preserved, session becomes uncounted
    manager.release_stream(&addr).await;

    // Preserved streams do not consume a counted slot — user_connections should be 0
    assert_eq!(
        manager.user_connections(username).await,
        0,
        "preserved stream should not count toward active connections"
    );

    // But the preserved stream is still a valid eviction candidate (valid victim)
    let candidates = manager.get_eviction_candidates(username, "10.0.0.5").await;
    assert!(candidates.iter().any(|c| c.addr == addr), "preserved stream should be an eviction candidate");
}

#[test]
fn socket_registration_primary_username_is_deterministic() {
    let mut reg = SocketRegistration::anonymous();
    reg.add_user("user_z", 100);
    reg.add_user("user_a", 100);
    reg.add_user("user_m", 100);
    assert_eq!(reg.primary_username(), Some("user_a"));
}

#[tokio::test]
async fn release_stream_without_uid_rejects_ambiguous_multiple_users_on_same_addr() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "192.168.1.100:12345".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key".to_string(), "192.168.1.100".to_string(), addr);

    manager.add_connection(&addr).await;

    let stream_a = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 1,
            meter_uid: 0,
            username: "user_a",
            max_connections: 5,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1001),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("register user_a");

    let stream_b = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 2,
            meter_uid: 0,
            username: "user_b",
            max_connections: 5,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-b".intern(),
            stream_channel: &test_channel(1002),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("register user_b");

    // Releasing without stream_uid when multiple users are on the same addr must return None (ambiguous)
    let released = manager.release_stream(&addr).await;
    assert!(
        released.is_none(),
        "releasing without uid must not arbitrarily pick a user when multiple are active on the same addr"
    );

    // But releasing with explicit stream_uid works cleanly:
    let released_a = manager.release_stream_by_uid(&addr, stream_a.uid).await;
    assert!(released_a.is_some(), "releasing with stream_uid succeeds");

    // Now only user_b remains on addr. Releasing without stream_uid should now succeed:
    let released_b = manager.release_stream(&addr).await;
    assert!(released_b.is_some(), "releasing without stream_uid succeeds when only a single user remains on addr");
    assert_eq!(released_b.unwrap().uid, stream_b.uid);
}
