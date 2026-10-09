use super::{
    test_adaptive_channel, test_channel, ActiveUserConnectionParams, ActiveUserManager, AdaptiveExpiryKey,
    CreateUserSessionParams, DEFAULT_ACTIVE_SOCKET_TTL_SECS, USER_CON_TTL, USER_GC_TTL,
};
use crate::{
    active_provider_manager::ConnectionKind, connection_manager::CleanupEvent, stream::DIRECT_BODY_IDLE_TIMEOUT_SECS,
    EventManager,
};
use arc_swap::ArcSwapOption;
use shared::{
    defaults::default_hls_session_ttl_secs,
    model::{PlaylistItemType, StreamChannel, UserConnectionPermission, XtreamCluster},
    utils::{current_time_secs, Internable},
};
use std::{
    borrow::Cow,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

#[tokio::test]
async fn recently_evicted_session_guard_survives_ttl_while_protected_addr_is_still_active() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let evicted_addr: SocketAddr = "127.0.0.1:55111".parse().unwrap();
    let protected_addr: SocketAddr = "127.0.0.1:55112".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-guard-session".to_string(), "127.0.0.1".to_string(), evicted_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("guard-user");
    user.max_connections = 1;

    manager.add_connection(&evicted_addr).await;
    manager.add_connection(&protected_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-guard-session",
            virtual_id: 2018,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &evicted_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 18,
            meter_uid: 0,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(2018),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-guard-session"),
        })
        .await;

    manager.mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, Duration::from_secs(1)).await;
    {
        let mut connections = manager.connections.write().await;
        if let Some(registration) = connections.key_by_addr.get_mut(&protected_addr) {
            registration.add_user(&user.username, current_time_secs());
        }
        let protection = connections
            .recently_evicted_sessions
            .get_mut("tok-guard-session")
            .expect("recent eviction guard should exist");
        protection.expires_at = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
    }

    assert_eq!(manager.recently_evicted_session_protected_addr("tok-guard-session").await, Some(protected_addr));
}

#[tokio::test]
async fn test_preserved_adaptive_stream_is_pruned_after_session_ttl() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55061".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-7".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-expire",
            virtual_id: 6001,
            provider: "provider-a",
            stream_url: "http://localhost/hls.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 77,
            meter_uid: 177,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(6001) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-expire"),
        })
        .await;
    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);

    {
        let mut connections = manager.connections.write().await;
        let connection_data = connections.by_key.get_mut("user1").unwrap();
        let session = connection_data.sessions.iter_mut().find(|session| session.token == "tok-expire").unwrap();
        session.ts = session.ts.saturating_sub(default_hls_session_ttl_secs() + 1);
    }
    if let Some(gc_ts) = &manager.gc_ts {
        gc_ts.store(current_time_secs().saturating_sub(USER_GC_TTL + 1), Ordering::Release);
    }

    manager
        .process_due_adaptive_expiry_entries(current_time_secs().saturating_add(default_hls_session_ttl_secs() + 1))
        .await;
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn test_due_adaptive_expiry_removal_promotes_soft_stream() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let normal_addr: SocketAddr = "127.0.0.1:55062".parse().unwrap();
    let soft_addr: SocketAddr = "127.0.0.1:55063".parse().unwrap();
    let normal_fp = Fingerprint::new("fp-key-7a".to_string(), "127.0.0.1".to_string(), normal_addr);
    let soft_fp = Fingerprint::new("fp-key-7b".to_string(), "127.0.0.1".to_string(), soft_addr);

    manager.add_connection(&normal_addr).await;
    manager.add_connection(&soft_addr).await;

    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;
    user.soft_connections = 1;

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-expire-normal",
            virtual_id: 6002,
            provider: "provider-a",
            stream_url: "http://localhost/hls-normal.m3u8",
            addr: &normal_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 78,
            meter_uid: 178,
            username: "user1",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: ConnectionKind::Normal,
            priority: -1,
            soft_priority: 9,
            fingerprint: &normal_fp,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(6002) },
            user_agent: Cow::Borrowed("ua-normal"),
            session_token: Some("tok-expire-normal"),
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 79,
            meter_uid: 179,
            username: "user1",
            max_connections: 1,
            soft_connections: 1,
            connection_kind: ConnectionKind::Soft,
            priority: -5,
            soft_priority: 9,
            fingerprint: &soft_fp,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(6003),
            user_agent: Cow::Borrowed("ua-soft"),
            session_token: None,
        })
        .await;

    let released = manager.release_connection(&normal_addr).await;
    assert!(released.addr_removed);

    {
        let mut connections = manager.connections.write().await;
        let connection_data = connections.by_key.get_mut("user1").unwrap();
        let session = connection_data.sessions.iter_mut().find(|session| session.token == "tok-expire-normal").unwrap();
        session.ts = session.ts.saturating_sub(default_hls_session_ttl_secs() + 1);
    }

    manager
        .process_due_adaptive_expiry_entries(current_time_secs().saturating_add(default_hls_session_ttl_secs() + 1))
        .await;

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get("user1").unwrap();
    assert_eq!(connection_data.stream_kinds.get(&79), Some(&ConnectionKind::Soft));
    assert!(!connection_data.stream_normal_priorities.contains_key(&78));
}

#[tokio::test]
async fn test_repeated_preserve_for_same_adaptive_session_keeps_single_current_expiry_index() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr_a: SocketAddr = "127.0.0.1:55071".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:55072".parse().unwrap();
    let fp_a = Fingerprint::new("fp-key-a".to_string(), "127.0.0.1".to_string(), addr_a);
    let fp_b = Fingerprint::new("fp-key-b".to_string(), "127.0.0.1".to_string(), addr_b);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr_a).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-reuse",
            virtual_id: 7001,
            provider: "provider-a",
            stream_url: "http://localhost/live-a.m3u8",
            addr: &addr_a,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 88,
            meter_uid: 188,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fp_a,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(7001) },
            user_agent: Cow::Borrowed("ua-a"),
            session_token: Some("tok-reuse"),
        })
        .await;
    let released = manager.release_connection(&addr_a).await;
    assert!(released.addr_removed);

    manager.add_connection(&addr_b).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 99,
            meter_uid: 199,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fp_b,
            provider: "provider-b".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveDash, ..test_channel(7002) },
            user_agent: Cow::Borrowed("ua-b"),
            session_token: Some("tok-reuse"),
        })
        .await;
    let released = manager.release_connection(&addr_b).await;
    assert!(released.addr_removed);

    let expiry_index = manager.adaptive_expiry_index.lock().await;
    assert_eq!(expiry_index.len(), 1);
    assert!(expiry_index.contains_key(&AdaptiveExpiryKey {
        username: String::from("user1"),
        session_token: String::from("tok-reuse"),
        uid: 88,
    }));
}

#[tokio::test]
async fn test_due_adaptive_expiry_reschedules_when_session_timestamp_changes() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55083".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-10".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-reschedule",
            virtual_id: 8003,
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
            uid: 133,
            meter_uid: 233,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(8003) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-reschedule"),
        })
        .await;
    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);

    let key =
        AdaptiveExpiryKey { username: String::from("user1"), session_token: String::from("tok-reschedule"), uid: 133 };
    let old_expires_at = {
        let expiry_index = manager.adaptive_expiry_index.lock().await;
        *expiry_index.get(&key).unwrap()
    };

    {
        let mut connections = manager.connections.write().await;
        let session = connections
            .by_key
            .get_mut("user1")
            .unwrap()
            .sessions
            .iter_mut()
            .find(|session| session.token == "tok-reschedule")
            .unwrap();
        session.ts = session.ts.saturating_add(30);
    }

    manager.process_due_adaptive_expiry_entries(old_expires_at).await;

    let new_expires_at = {
        let expiry_index = manager.adaptive_expiry_index.lock().await;
        *expiry_index.get(&key).unwrap()
    };
    assert!(new_expires_at > old_expires_at);
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn test_due_adaptive_expiry_removes_stale_index_when_preserved_stream_missing() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55085".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-11a".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-stale",
            virtual_id: 8004,
            provider: "provider-a",
            stream_url: "http://localhost/stale.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 134,
            meter_uid: 234,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(8004) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-stale"),
        })
        .await;
    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);

    let key = AdaptiveExpiryKey { username: String::from("user1"), session_token: String::from("tok-stale"), uid: 134 };
    let old_expires_at = {
        let expiry_index = manager.adaptive_expiry_index.lock().await;
        *expiry_index.get(&key).unwrap()
    };

    {
        let mut connections = manager.connections.write().await;
        let connection_data = connections.by_key.get_mut("user1").unwrap();
        connection_data.streams.clear();
    }

    manager.process_due_adaptive_expiry_entries(old_expires_at).await;

    let expiry_index = manager.adaptive_expiry_index.lock().await;
    assert!(!expiry_index.contains_key(&key));
}

#[tokio::test]
async fn test_due_adaptive_expiry_does_not_block_on_full_cleanup_channel() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55084".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-11".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-full-channel",
            virtual_id: 8004,
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
            uid: 144,
            meter_uid: 244,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(8004) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-full-channel"),
        })
        .await;
    let released = manager.release_connection(&addr).await;
    assert!(released.addr_removed);

    {
        let mut connections = manager.connections.write().await;
        let session = connections
            .by_key
            .get_mut("user1")
            .unwrap()
            .sessions
            .iter_mut()
            .find(|session| session.token == "tok-full-channel")
            .unwrap();
        session.ts = session.ts.saturating_sub(default_hls_session_ttl_secs() + 1);
    }

    let (cleanup_tx, mut cleanup_rx) = mpsc::channel(1);
    cleanup_tx.send(CleanupEvent::ReleaseConnection { addr }).await.expect("prefill cleanup channel");
    manager.set_cleanup_sender(cleanup_tx);

    let process_result = tokio::time::timeout(
        Duration::from_millis(100),
        manager.process_due_adaptive_expiry_entries(
            current_time_secs().saturating_add(default_hls_session_ttl_secs() + 1),
        ),
    )
    .await;

    assert!(process_result.is_ok(), "adaptive expiry processing must not await while holding locks");

    let queued_event = cleanup_rx.try_recv().expect("prefilled cleanup event should remain queued");
    assert!(matches!(queued_event, CleanupEvent::ReleaseConnection { .. }));
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn named_socket_registration_exposes_expiry_deadline() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let stale_addr: SocketAddr = "127.0.0.1:55021".parse().unwrap();
    let fresh_addr: SocketAddr = "127.0.0.1:55022".parse().unwrap();
    let stale_fp = Fingerprint::new("fp-stale".to_string(), "127.0.0.1".to_string(), stale_addr);
    let fresh_fp = Fingerprint::new("fp-fresh".to_string(), "127.0.0.1".to_string(), fresh_addr);
    let mut stale_user = ProxyUserCredentials::default();
    stale_user.username = "user1".to_string();
    stale_user.max_connections = 1;
    let mut fresh_user = ProxyUserCredentials::default();
    fresh_user.username = "user2".to_string();
    fresh_user.max_connections = 1;

    manager.add_connection(&stale_addr).await;
    manager.add_connection(&fresh_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &stale_user,
            session_token: "tok-stale-deadline",
            virtual_id: 9201,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &stale_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &fresh_user,
            session_token: "tok-fresh-deadline",
            virtual_id: 9202,
            provider: "provider-b",
            stream_url: "http://localhost/live.m3u8",
            addr: &fresh_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 201,
            meter_uid: 301,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &stale_fp,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(9201),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-stale-deadline"),
        })
        .await
        .expect("stale stream should register");
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 202,
            meter_uid: 302,
            username: "user2",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fresh_fp,
            provider: "provider-b".intern(),
            stream_channel: &test_adaptive_channel(9202),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-fresh-deadline"),
        })
        .await
        .expect("fresh stream should register");

    {
        let mut connections = manager.connections.write().await;
        let stale_registration = connections.key_by_addr.get_mut(&stale_addr).expect("stale registration should exist");
        stale_registration.ts = stale_registration.ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 1);
    }

    let stale_deadline =
        manager.socket_expiry_deadline(&stale_addr).await.expect("stale named socket should have an expiry deadline");
    let fresh_deadline =
        manager.socket_expiry_deadline(&fresh_addr).await.expect("fresh named socket should have an expiry deadline");
    assert!(stale_deadline < fresh_deadline);
}

#[tokio::test]
async fn socket_expiry_deadline_extends_direct_body_streams_to_the_idle_timeout() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55040".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-vod".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-vod-expiry".to_string();
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-vod-expiry",
            virtual_id: 8888,
            provider: "provider-a",
            stream_url: "http://localhost/movie.mkv",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let mut channel = test_channel(8888);
    channel.item_type = PlaylistItemType::Video;
    channel.cluster = XtreamCluster::Video;
    channel.url = "http://localhost/movie.mkv".intern();

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 602,
            meter_uid: 702,
            username: "user-vod-expiry",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("player/1.0"),
            session_token: Some("tok-vod-expiry"),
        })
        .await
        .expect("vod stream should be created");

    // The player stops reading while draining its buffer. The pause already exceeds the
    // short HLS session TTL but stays well within the direct-body idle timeout, so the
    // socket must not be treated as expired yet.
    let previous_registration_ts = {
        let mut connections = manager.connections.write().await;
        let registration = connections.key_by_addr.get_mut(&addr).expect("registration should exist");
        registration.ts = registration.ts.saturating_sub(default_hls_session_ttl_secs() + 5);
        registration.ts
    };

    let deadline =
        manager.socket_expiry_deadline(&addr).await.expect("VOD streams should stay scheduled for expiry tracking");

    let unchanged_registration_ts = {
        let connections = manager.connections.read().await;
        connections.key_by_addr.get(&addr).expect("registration should still exist").ts
    };

    assert_eq!(unchanged_registration_ts, previous_registration_ts);
    assert!(
        deadline > current_time_secs(),
        "a buffering direct-body stream must not be treated as expired after only the HLS session TTL"
    );
    assert_eq!(
        deadline,
        previous_registration_ts.saturating_add(DIRECT_BODY_IDLE_TIMEOUT_SECS),
        "direct-body sockets must use the direct-body idle timeout as their expiry allowance"
    );
}

#[tokio::test]
async fn socket_expiry_deadline_keeps_hls_session_ttl_for_adaptive_streams() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55043".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-hls".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-hls-expiry".to_string();
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-hls-expiry",
            virtual_id: 8891,
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
            uid: 605,
            meter_uid: 705,
            username: "user-hls-expiry",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(8891),
            user_agent: Cow::Borrowed("player/1.0"),
            session_token: Some("tok-hls-expiry"),
        })
        .await
        .expect("hls stream should be created");

    let registration_ts = {
        let mut connections = manager.connections.write().await;
        let registration = connections.key_by_addr.get_mut(&addr).expect("registration should exist");
        registration.ts = registration.ts.saturating_sub(default_hls_session_ttl_secs() + 5);
        registration.ts
    };

    let deadline = manager.socket_expiry_deadline(&addr).await.expect("HLS streams should stay scheduled for expiry");

    assert_eq!(
        deadline,
        registration_ts.saturating_add(manager.active_socket_ttl_secs()),
        "adaptive sockets keep the short HLS session TTL"
    );
}

#[tokio::test]
async fn gc_keeps_active_ts_streams_even_when_user_timestamp_is_stale() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55013".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-ts".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 144,
            meter_uid: 244,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(9001),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("ts stream should register");

    {
        let mut connections = manager.connections.write().await;
        let connection_data = connections.by_key.get_mut("user1").expect("user entry should exist");
        connection_data.ts = connection_data.ts.saturating_sub(USER_CON_TTL + 1);
    }

    if let Some(gc_ts) = &manager.gc_ts {
        gc_ts.store(current_time_secs().saturating_sub(USER_GC_TTL + 1), Ordering::Release);
    }

    manager.active_streams().await;

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get("user1").expect("active user entry must survive gc");
    assert_eq!(connection_data.connections, 1);
    assert_eq!(connection_data.streams.len(), 1);
}
