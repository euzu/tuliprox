use super::{
    create_socket_reentry_guard_key, test_channel, ActiveUserConnectionParams, ActiveUserManager,
    CreateUserSessionParams,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::{PlaylistItemType, UserConnectionPermission, XtreamCluster},
    utils::Internable,
};
use std::{
    borrow::Cow,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

#[tokio::test]
async fn recently_evicted_vod_uses_session_reentry_guard() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let evicted_addr: SocketAddr = "127.0.0.1:55113".parse().unwrap();
    let protected_addr: SocketAddr = "127.0.0.1:55114".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-vod-guard".to_string(), "127.0.0.1".to_string(), evicted_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("vod-guard-user");
    user.max_connections = 1;
    let mut channel = test_channel(2019);
    channel.item_type = PlaylistItemType::Video;
    channel.cluster = XtreamCluster::Video;
    channel.url = "http://localhost/movie.mkv".intern();

    manager.add_connection(&evicted_addr).await;
    manager.add_connection(&protected_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-guard-vod",
            virtual_id: channel.virtual_id,
            provider: "provider-a",
            stream_url: channel.url.as_ref(),
            addr: &evicted_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 19,
            meter_uid: 0,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-guard-vod"),
        })
        .await;

    manager.mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, Duration::from_secs(10)).await;

    assert_eq!(manager.recently_evicted_session_protected_addr("tok-guard-vod").await, Some(protected_addr));
    let connections = manager.connections.read().await;
    assert!(
        connections.recent_socket_reentry_guards.is_empty(),
        "provider-affine VOD must not be guarded by transient socket identity"
    );
}

#[tokio::test]
async fn provider_affine_stream_without_session_token_uses_socket_reentry_fallback() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let evicted_addr: SocketAddr = "127.0.0.1:55115".parse().unwrap();
    let protected_addr: SocketAddr = "127.0.0.1:55116".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-vod-no-token".to_string(), "127.0.0.1".to_string(), evicted_addr);
    let mut channel = test_channel(2020);
    channel.item_type = PlaylistItemType::Video;
    channel.cluster = XtreamCluster::Video;
    channel.url = "http://localhost/movie-no-token.mkv".intern();

    manager.add_connection(&evicted_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 20,
            meter_uid: 0,
            username: "vod-no-token-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await;

    manager.mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, Duration::from_secs(10)).await;

    assert_eq!(
        manager
            .recent_socket_reentry_protected_addr(
                "vod-no-token-user",
                "127.0.0.1",
                shared::model::VirtualId::new(channel.virtual_id)
            )
            .await,
        Some(protected_addr)
    );
}

#[tokio::test]
async fn socket_reentry_guard_expires_at_ttl() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let evicted_addr: SocketAddr = "127.0.0.1:55117".parse().unwrap();
    let protected_addr: SocketAddr = "127.0.0.1:55118".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-socket-ttl".to_string(), "127.0.0.1".to_string(), evicted_addr);
    let channel = test_channel(2021);

    manager.add_connection(&evicted_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 21,
            meter_uid: 0,
            username: "socket-ttl-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await;

    manager.mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, Duration::from_secs(10)).await;
    let virtual_id = shared::model::VirtualId::new(channel.virtual_id);
    assert_eq!(
        manager.recent_socket_reentry_protected_addr("socket-ttl-user", "127.0.0.1", virtual_id).await,
        Some(protected_addr)
    );

    // Expire only the guard while the protected address stays registered: the TTL is
    // authoritative and must not fall back to the still-present socket registration.
    {
        let mut connections = manager.connections.write().await;
        let key = create_socket_reentry_guard_key("socket-ttl-user", "127.0.0.1", virtual_id);
        let protection =
            connections.recent_socket_reentry_guards.get_mut(&key).expect("socket reentry guard should exist");
        protection.expires_at = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
    }
    assert_eq!(manager.recent_socket_reentry_protected_addr("socket-ttl-user", "127.0.0.1", virtual_id).await, None);
}

#[tokio::test]
async fn arm_eviction_protection_protects_all_users_on_addr() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "192.168.1.100:12345".parse().unwrap();
    let protected_addr: SocketAddr = "192.168.1.101:54321".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key".to_string(), "192.168.1.100".to_string(), addr);

    manager.add_connection(&addr).await;

    manager
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
            stream_channel: &test_channel(2001),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("register user_a");

    manager
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
            stream_channel: &test_channel(2002),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("register user_b");

    manager.mark_recent_eviction_guard_for_addr(&addr, protected_addr, Duration::from_secs(60)).await;

    let connections = manager.connections.read().await;
    let key_a = create_socket_reentry_guard_key("user_a", "192.168.1.100", shared::model::VirtualId::new(2001));
    let key_b = create_socket_reentry_guard_key("user_b", "192.168.1.100", shared::model::VirtualId::new(2002));

    assert!(connections.recent_socket_reentry_guards.contains_key(&key_a), "user_a guard must be armed");
    assert!(connections.recent_socket_reentry_guards.contains_key(&key_b), "user_b guard must be armed");
}
