use super::*;
use crate::EventManager;
use arc_swap::ArcSwapOption;
use shared::{
    model::{PlaylistItemType, ProxyType, StreamChannel, StreamInfo, XtreamCluster},
    utils::Internable,
};
use std::{borrow::Cow, collections::HashMap, sync::Arc};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};

fn test_channel(virtual_id: u32) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id,
        provider_id: 1,
        input_name: "input".intern(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        group: "group".intern(),
        title: "title".intern(),
        url: "http://localhost/stream.ts".intern(),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

fn test_adaptive_channel(virtual_id: u32) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id,
        provider_id: 1,
        input_name: "input".intern(),
        item_type: PlaylistItemType::LiveHls,
        cluster: XtreamCluster::Live,
        group: "group".intern(),
        title: "title".intern(),
        url: "http://localhost/stream.ts".intern(),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

fn test_series_channel(virtual_id: u32) -> StreamChannel {
    StreamChannel {
        item_type: PlaylistItemType::Series,
        cluster: XtreamCluster::Series,
        url: "http://localhost/series/episode.mkv".intern(),
        ..test_channel(virtual_id)
    }
}

#[tokio::test]
async fn target_scoped_session_lookup_does_not_use_same_virtual_id_from_other_target() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55499".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "target-scoped-user".to_string();
    let mut target_one = test_channel(42);
    target_one.target_id = 1;
    let mut target_two = test_channel(42);
    target_two.target_id = 2;

    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-target-one",
            virtual_id: 42,
            provider: "provider",
            stream_url: "http://localhost/target-one.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-target-two",
            virtual_id: 42,
            provider: "provider",
            stream_url: "http://localhost/target-two.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    {
        let mut connections = manager.connections.write().await;
        assert!(connections.by_key.contains_key(&user.username), "user connection data should exist");
        let Some(data) = connections.by_key.get_mut(&user.username) else {
            return;
        };
        data.streams.push(StreamInfo::new(shared::model::StreamInfoParams {
            uid: 1,
            meter_uid: 1,
            username: &user.username,
            addr: &addr,
            client_ip: "127.0.0.1",
            provider: "provider".intern(),
            stream_channel: target_one,
            user_agent: "ua".to_string(),
            country_code: None,
            session_token: Some("tok-target-one"),
        }));
        data.streams.push(StreamInfo::new(shared::model::StreamInfoParams {
            uid: 2,
            meter_uid: 2,
            username: &user.username,
            addr: &addr,
            client_ip: "127.0.0.1",
            provider: "provider".intern(),
            stream_channel: target_two,
            user_agent: "ua".to_string(),
            country_code: None,
            session_token: Some("tok-target-two"),
        }));
    }

    let session = manager.find_latest_session_for_target_stream(&user.username, 2, "input", 42, "tok-target-two").await;
    assert!(session.is_some(), "target-scoped session should resolve");
    let Some(session) = session else {
        return;
    };
    assert_eq!(session.token, "tok-target-two");
    assert!(manager
        .find_latest_session_for_target_stream(&user.username, 3, "input", 42, "tok-target-two")
        .await
        .is_none());
    assert!(manager
        .find_latest_session_for_target_stream(&user.username, 1, "input", 42, "tok-target-two")
        .await
        .is_none());
}

#[tokio::test]
async fn user_agent_stream_index_is_stable_per_session_and_unique_between_sessions() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let addr = SocketAddr::from(([127, 0, 0, 1], 55_500));
    let mut user = ProxyUserCredentials::default();
    user.username = "indexed-user".to_string();

    for token in ["indexed-session-a", "indexed-session-b"] {
        manager
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: 42,
                provider: "provider-a",
                stream_url: "http://localhost/live.m3u8",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
    }

    let first = manager.get_or_assign_user_agent_stream_index(&user.username, "indexed-session-a").await;
    let repeated = manager.get_or_assign_user_agent_stream_index(&user.username, "indexed-session-a").await;
    let second = manager.get_or_assign_user_agent_stream_index(&user.username, "indexed-session-b").await;

    assert!(first.is_some());
    assert_eq!(repeated, first);
    assert!(second.is_some());
    assert_ne!(second, first);
}

/// Session refresh normalizes Expired -> Prepared.
/// When a new request arrives on an expired session, the lifecycle should be
/// reset to Prepared so that full activation evaluation happens.
#[tokio::test]
async fn create_user_session_normalizes_expired_lifecycle() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55400".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "user-lifecycle-refresh".to_string();

    // Create a session in Expired state directly via session manipulation
    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.entry(user.username.clone()).or_insert_with(|| UserConnectionData::new(0, 1, 0));
        data.add_session(UserSession {
            token: "tok-refresh-expired".to_string(),
            transition_version: 1,
            virtual_id: 7001,
            provider: "provider-a".intern(),
            stream_url: "http://localhost/live.m3u8".intern(),
            provider_session_headers: HashMap::new(),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            lifecycle: PlaybackLifecycle::Expired,
        });
    }

    // Refresh the session via create_user_session
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-refresh-expired",
            virtual_id: 7001,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let sessions = manager.connections.read().await;
    let data = sessions.by_key.get(&user.username).expect("user data should exist");
    let session = data.sessions.iter().find(|s| s.token == "tok-refresh-expired").expect("session");
    assert_eq!(
        session.lifecycle,
        PlaybackLifecycle::Prepared,
        "Expired session should normalize to Prepared on refresh"
    );
}

/// Session refresh does NOT normalize `PendingProvider`.
/// A `PendingProvider` session must not be reset — pending state must continue
/// until explicitly resolved via `activate_pending_provider` or `expire_pending_provider`.
#[tokio::test]
async fn create_user_session_does_not_normalize_pending_provider_lifecycle() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55401".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "user-pending-lifecycle".to_string();

    // Create a session in PendingProvider state
    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.entry(user.username.clone()).or_insert_with(|| UserConnectionData::new(0, 1, 0));
        data.add_session(UserSession {
            token: "tok-refresh-pending".to_string(),
            transition_version: 1,
            virtual_id: 7002,
            provider: "provider-a".intern(),
            stream_url: "http://localhost/live.m3u8".intern(),
            provider_session_headers: HashMap::new(),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            lifecycle: PlaybackLifecycle::PendingProvider {
                data: PendingProviderState {
                    reason_code: PendingProviderReason::GraceHold,
                    created_at: current_time_secs(),
                    deadline: current_time_secs() + 30,
                    version: 1,
                    wake_source: None,
                },
            },
        });
    }

    // Refresh the session via create_user_session
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-refresh-pending",
            virtual_id: 7002,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let sessions = manager.connections.read().await;
    let data = sessions.by_key.get(&user.username).expect("user data should exist");
    let session = data.sessions.iter().find(|s| s.token == "tok-refresh-pending").expect("session");
    assert!(
        matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }),
        "PendingProvider session should NOT be normalized on refresh - pending wait must continue"
    );
}

#[tokio::test]
async fn update_session_provider_headers_updates_existing_session_and_timestamp() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55402".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "user-provider-headers".to_string();

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-provider-headers",
            virtual_id: 7003,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let before = manager
        .get_and_update_user_session(&user.username, "tok-provider-headers")
        .await
        .expect("session should exist");
    let previous_ts = before.ts;
    let headers = HashMap::from([(String::from("cookie"), String::from("sid=abc"))]);

    assert!(manager.update_session_provider_headers(&user.username, "tok-provider-headers", &headers).await);

    let after = manager
        .get_and_update_user_session(&user.username, "tok-provider-headers")
        .await
        .expect("session should exist");
    assert_eq!(after.provider_session_headers, headers);
    assert!(after.ts >= previous_ts);
}

#[tokio::test]
async fn update_session_provider_headers_returns_false_for_missing_user_or_token() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let headers = HashMap::from([(String::from("cookie"), String::from("sid=abc"))]);

    assert!(!manager.update_session_provider_headers("missing-user", "missing-token", &headers).await);

    let addr: SocketAddr = "127.0.0.1:55403".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "user-missing-token".to_string();
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-existing",
            virtual_id: 7004,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    assert!(!manager.update_session_provider_headers(&user.username, "tok-missing", &headers).await);
}

#[tokio::test]
async fn create_user_session_clears_provider_headers_when_provider_or_stream_url_changes() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55404".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "user-provider-header-reset".to_string();
    let headers = HashMap::from([(String::from("cookie"), String::from("sid=abc"))]);

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-reset",
            virtual_id: 7005,
            provider: "provider-a",
            stream_url: "http://localhost/live-a.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    assert!(manager.update_session_provider_headers(&user.username, "tok-reset", &headers).await);

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-reset",
            virtual_id: 7005,
            provider: "provider-b",
            stream_url: "http://localhost/live-b.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let session = manager.get_and_update_user_session(&user.username, "tok-reset").await.expect("session should exist");
    assert!(session.provider_session_headers.is_empty());
}

/// `terminate_session` expires a session and removes it.
#[tokio::test]
async fn terminate_session_expires_and_removes_session() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55410".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "user-terminate".to_string();
    user.max_connections = 2;

    let token = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-terminate-test",
            virtual_id: 8001,
            provider: "provider-terminate",
            stream_url: "http://localhost/test.ts",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Verify session exists.
    let before = manager.get_and_update_user_session(&user.username, &token).await;
    assert!(before.is_some(), "session should exist before terminate");
    assert_eq!(before.as_ref().unwrap().lifecycle, PlaybackLifecycle::Prepared);

    // Terminate the session.
    manager.terminate_session(&user.username, &token).await;

    // Session should be gone.
    let after = manager.get_and_update_user_session(&user.username, &token).await;
    assert!(after.is_none(), "session should be removed after terminate");
}

/// `terminate_session` releases counted lease.
#[tokio::test]
async fn terminate_session_releases_counted_lease() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55411".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "user-terminate-counted".to_string();
    user.max_connections = 2;

    let token = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-terminate-counted",
            virtual_id: 8002,
            provider: "provider-terminate-counted",
            stream_url: "http://localhost/test.ts",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    // Mark the session as counted and active (simulating post-admission state).
    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.get_mut(&user.username).unwrap();
        let session = data.sessions.iter_mut().find(|s| s.token == token).unwrap();
        // Simulate counted state by setting lifecycle to Active.
        session.lifecycle = PlaybackLifecycle::Active;
        data.increment_kind(ConnectionKind::Normal);
    }

    // Verify counted before terminate.
    {
        let before = manager.get_and_update_user_session(&user.username, &token).await.unwrap();
        assert!(before.lifecycle.is_counted(), "session should be counted before terminate");
    }

    // Terminate.
    manager.terminate_session(&user.username, &token).await;

    // Session should be gone.
    let after = manager.get_and_update_user_session(&user.username, &token).await;
    assert!(after.is_none(), "session should be removed after terminate");
}

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

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn terminate_session_promotes_soft_stream_after_releasing_capacity() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let normal_addr: SocketAddr = "127.0.0.1:55413".parse().unwrap();
    let soft_addr: SocketAddr = "127.0.0.1:55414".parse().unwrap();
    let soft_addr_two: SocketAddr = "127.0.0.1:55415".parse().unwrap();
    let normal_fp = Fingerprint::new("fp-terminate-normal".to_string(), "127.0.0.1".to_string(), normal_addr);
    let soft_fp = Fingerprint::new("fp-terminate-soft".to_string(), "127.0.0.1".to_string(), soft_addr);
    let soft_fp_two = Fingerprint::new("fp-terminate-soft-2".to_string(), "127.0.0.1".to_string(), soft_addr_two);
    let mut user = ProxyUserCredentials::default();
    user.username = "user-terminate-promote".to_string();
    user.max_connections = 1;
    user.soft_connections = 2;

    manager.add_connection(&normal_addr).await;
    manager.add_connection(&soft_addr).await;
    manager.add_connection(&soft_addr_two).await;

    let normal_token = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-terminate-normal",
            virtual_id: 8101,
            provider: "provider-normal",
            stream_url: "http://localhost/normal.ts",
            addr: &normal_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let soft_token = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-terminate-soft",
            virtual_id: 8102,
            provider: "provider-soft",
            stream_url: "http://localhost/soft.ts",
            addr: &soft_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Soft),
            socket_bound: false,
        })
        .await;
    let soft_token_two = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-terminate-soft-2",
            virtual_id: 8103,
            provider: "provider-soft-2",
            stream_url: "http://localhost/soft-2.ts",
            addr: &soft_addr_two,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Soft),
            socket_bound: false,
        })
        .await;

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 8101,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &normal_fp,
            provider: "provider-normal".intern(),
            stream_channel: &test_channel(8101),
            user_agent: Cow::Borrowed("ua-normal"),
            session_token: Some(&normal_token),
        })
        .await
        .expect("normal stream should be registered");
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 8102,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Soft,
            priority: -5,
            soft_priority: 9,
            fingerprint: &soft_fp,
            provider: "provider-soft".intern(),
            stream_channel: &test_channel(8102),
            user_agent: Cow::Borrowed("ua-soft"),
            session_token: Some(&soft_token),
        })
        .await
        .expect("soft stream should be registered");
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 8103,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Soft,
            priority: -3,
            soft_priority: 9,
            fingerprint: &soft_fp_two,
            provider: "provider-soft-2".intern(),
            stream_channel: &test_channel(8103),
            user_agent: Cow::Borrowed("ua-soft-2"),
            session_token: Some(&soft_token_two),
        })
        .await
        .expect("second soft stream should be registered");

    {
        let mut connections = manager.connections.write().await;
        let connection_data = connections.by_key.get_mut(&user.username).expect("user data should exist");
        connection_data.soft_connections = 1;
    }

    manager.terminate_session(&user.username, &normal_token).await;

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user data should remain inspectable");
    assert_eq!(connection_data.counts.normal, 1);
    assert_eq!(connection_data.counts.soft, 1);
    let promoted_uid = [8102_u32, 8103_u32]
        .into_iter()
        .find(|uid| connection_data.stream_kinds.get(uid) == Some(&ConnectionKind::Normal))
        .expect("one soft stream should be promoted to normal");
    let promoted_token = if promoted_uid == 8102 { soft_token.as_str() } else { soft_token_two.as_str() };
    let promoted_session = connection_data
        .sessions
        .iter()
        .find(|session| session.token == promoted_token)
        .expect("promoted soft session should remain");
    assert_eq!(promoted_session.connection_kind, Some(ConnectionKind::Normal));
    assert!(matches!(promoted_session.lifecycle, PlaybackLifecycle::Active));
}

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

#[tokio::test]
async fn mark_pending_provider_tracks_metadata_on_session() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55021".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "pending-user".to_string();

    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-pending",
            virtual_id: 1001,
            provider: "provider-a",
            stream_url: "http://provider/live/1001.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let _ =
        manager.mark_pending_provider(&user.username, "tok-pending", PendingProviderReason::GraceHold, 12_345).await;

    let session =
        manager.get_and_update_user_session(&user.username, "tok-pending").await.expect("session should exist");
    let PlaybackLifecycle::PendingProvider { data: pending } = &session.lifecycle else {
        panic!("pending provider should be tracked")
    };
    assert!(matches!(pending.reason_code, PendingProviderReason::GraceHold));
    assert_eq!(pending.deadline, 12_345);
    assert!(pending.created_at > 0);
    assert_eq!(pending.version, 1);
    assert!(pending.wake_source.is_none());
}

#[tokio::test]
async fn activate_pending_provider_clears_pending_metadata() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55022".parse().unwrap_or_else(|_| unreachable!());
    let fingerprint = Fingerprint::new("fp-pending".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "pending-activate".to_string();

    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-pending-activate",
            virtual_id: 1002,
            provider: "provider-a",
            stream_url: "http://provider/live/1002.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let _ = manager
        .mark_pending_provider(
            &user.username,
            "tok-pending-activate",
            PendingProviderReason::GraceHold,
            current_time_secs().saturating_add(30),
        )
        .await;

    let _ = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 12,
            meter_uid: 0,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(1002),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-pending-activate"),
        })
        .await;
    manager
        .activate_pending_provider(&user.username, "tok-pending-activate", 1, PendingProviderWakeSource::Activated)
        .await;

    let session = manager
        .get_and_update_user_session(&user.username, "tok-pending-activate")
        .await
        .expect("session should exist");
    assert!(session.lifecycle.is_counted());
    assert!(
        !matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }),
        "explicit pending resolution must clear pending provider state"
    );
}

#[tokio::test]
async fn activate_pending_provider_ignores_stale_version_after_replacement() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55023".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "pending-stale".to_string();

    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-pending-stale",
            virtual_id: 1003,
            provider: "provider-a",
            stream_url: "http://provider/live/1003.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let first_version = manager
        .mark_pending_provider(&user.username, "tok-pending-stale", PendingProviderReason::GraceHold, 5_000)
        .await
        .expect("first pending version should be created");
    let second_version = manager
        .mark_pending_provider(&user.username, "tok-pending-stale", PendingProviderReason::GraceHold, 6_000)
        .await
        .expect("second pending version should replace the first");
    assert!(second_version > first_version);

    manager
        .activate_pending_provider(
            &user.username,
            "tok-pending-stale",
            first_version,
            PendingProviderWakeSource::CapacityNotify,
        )
        .await;

    let session = manager
        .get_and_update_user_session(&user.username, "tok-pending-stale")
        .await
        .expect("session should still exist");
    let PlaybackLifecycle::PendingProvider { data: pending_data } = &session.lifecycle else {
        panic!("session should still be in PendingProvider after stale wakeup")
    };
    assert_eq!(pending_data.version, second_version);
    assert!(pending_data.wake_source.is_none());
    assert_eq!(session.permission, UserConnectionPermission::GracePeriod);
}

#[tokio::test]
async fn expire_pending_provider_marks_session_exhausted() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55024".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "pending-expire".to_string();

    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-pending-expire",
            virtual_id: 1004,
            provider: "provider-a",
            stream_url: "http://provider/live/1004.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let version = manager
        .mark_pending_provider(&user.username, "tok-pending-expire", PendingProviderReason::GraceHold, 6_000)
        .await
        .expect("pending version should be created");

    manager
        .expire_pending_provider(&user.username, "tok-pending-expire", version, PendingProviderWakeSource::Timeout)
        .await;

    let session = manager
        .get_and_update_user_session(&user.username, "tok-pending-expire")
        .await
        .expect("session should still exist");
    assert_eq!(session.permission, UserConnectionPermission::Exhausted);
    assert!(!matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }));
    assert!(!session.lifecycle.is_counted());
}

#[tokio::test]
async fn expire_pending_provider_releases_counted_slot_for_pending_session() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55025".parse().unwrap_or_else(|_| unreachable!());
    let mut user = ProxyUserCredentials::default();
    user.username = "pending-expire-counted".to_string();
    user.max_connections = 1;

    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-pending-expire-counted",
            virtual_id: 1005,
            provider: "provider-a",
            stream_url: "http://provider/live/1005.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    {
        let mut connections = manager.connections.write().await;
        let connection_data =
            connections.by_key.get_mut(&user.username).expect("session should have created connection data");
        connection_data.increment_kind(ConnectionKind::Normal);
        let session = connection_data
            .sessions
            .iter_mut()
            .find(|session| session.token == "tok-pending-expire-counted")
            .expect("session should exist");
        // Simulate a previously-counted session transitioning to PendingProvider.
        // Set lifecycle to Active (is_counted() = true). The kind count is already
        // incremented above via connection_data.increment_kind().
        session.lifecycle = PlaybackLifecycle::Active;
    }

    assert_eq!(manager.user_connections(&user.username).await, 1);

    let version = manager
        .mark_pending_provider(&user.username, "tok-pending-expire-counted", PendingProviderReason::GraceHold, 6_500)
        .await
        .expect("pending version should be created");

    manager
        .expire_pending_provider(
            &user.username,
            "tok-pending-expire-counted",
            version,
            PendingProviderWakeSource::Timeout,
        )
        .await;

    let session = manager
        .get_and_update_user_session(&user.username, "tok-pending-expire-counted")
        .await
        .expect("session should still exist");
    assert_eq!(session.permission, UserConnectionPermission::Exhausted);
    assert!(!matches!(session.lifecycle, PlaybackLifecycle::PendingProvider { .. }));
    assert!(!session.lifecycle.is_counted());
    assert_eq!(manager.user_connections(&user.username).await, 0);
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
async fn test_grant_grace_succeeds_at_and_above_limit_without_prior_grace() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let at_limit_addr: SocketAddr = "127.0.0.1:55011".parse().unwrap();
    let at_limit_fingerprint = Fingerprint::new("fp-limit".to_string(), "127.0.0.1".to_string(), at_limit_addr);
    let over_limit_addr: SocketAddr = "127.0.0.1:55012".parse().unwrap();
    let over_limit_fingerprint = Fingerprint::new("fp-over".to_string(), "127.0.0.1".to_string(), over_limit_addr);

    manager.add_connection(&at_limit_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 10,
            meter_uid: 0,
            username: "at-limit",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &at_limit_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1010),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-limit"),
        })
        .await;

    assert!(manager.grant_grace("at-limit").await);

    manager.add_connection(&over_limit_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 11,
            meter_uid: 0,
            username: "over-limit",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &over_limit_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1011),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-over-1"),
        })
        .await;
    manager.add_connection(&"127.0.0.1:55013".parse().unwrap()).await;
    let second_fingerprint =
        Fingerprint::new("fp-over-2".to_string(), "127.0.0.1".to_string(), "127.0.0.1:55013".parse().unwrap());
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 12,
            meter_uid: 0,
            username: "over-limit",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &second_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1012),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-over-2"),
        })
        .await;

    assert!(manager.grant_grace("over-limit").await);
}

fn test_user_credentials(username: &str, max_connections: u32, soft_connections: u16) -> ProxyUserCredentials {
    ProxyUserCredentials {
        username: username.to_string(),
        password: "test".to_string(),
        token: None,
        proxy: ProxyType::default(),
        server: None,
        epg_timeshift: None,
        epg_request_timeshift: None,
        created_at: None,
        exp_date: None,
        max_connections,
        status: None,
        output_clusters: shared::model::ClusterFlags::all(),
        ui_enabled: true,
        comment: None,
        priority: 0,
        soft_connections,
        soft_priority: 0,
        t_is_api_user: false,
        network_access: None,
        plan: None,
        filter: None,
        raw_output_clusters: None,
        raw_max_connections: 0,
        raw_soft_connections: 0,
        raw_proxy: Some(ProxyType::default()),
        t_filter: None,
        t_has_unresolved_plan: false,
        t_has_invalid_filter: false,
    }
}

fn record_owned_slot(counts: &mut UserConnectionCounts, kind: ConnectionKind) {
    match kind {
        ConnectionKind::Normal => counts.normal += 1,
        ConnectionKind::Soft => counts.soft += 1,
    }
}

fn assert_connection_ownership_invariants(connection_data: &UserConnectionData) {
    let mut owned_slots = UserConnectionCounts::default();

    // A counted session owns one logical slot. Active streams tied to that
    // session validate its kind below, but do not add another slot.
    for session in connection_data.sessions.iter().filter(|session| session.lifecycle.is_counted()) {
        record_owned_slot(&mut owned_slots, session.connection_kind.unwrap_or(ConnectionKind::Normal));
    }

    for stream in &connection_data.streams {
        let stream_kind = connection_data.stream_kinds.get(&stream.uid);
        if stream.preserved {
            assert!(stream_kind.is_none(), "preserved stream {} must not own a real connection slot", stream.uid);
            continue;
        }

        let stream_kind = stream_kind.expect("every active stream must have a connection kind");
        let counted_session = stream.session_token.as_deref().and_then(|session_token| {
            connection_data
                .sessions
                .iter()
                .find(|session| session.token == session_token && session.lifecycle.is_counted())
        });
        if let Some(session) = counted_session {
            assert_eq!(
                *stream_kind,
                session.connection_kind.unwrap_or(ConnectionKind::Normal),
                "a counted session and its active stream must use the same slot kind"
            );
        } else {
            record_owned_slot(&mut owned_slots, *stream_kind);
        }
    }

    for uid in connection_data.stream_kinds.keys() {
        assert!(
            connection_data.streams.iter().any(|stream| stream.uid == *uid && !stream.preserved),
            "stream kind for uid {uid} must belong to an active stream"
        );
    }

    assert_eq!(connection_data.counts.normal, owned_slots.normal, "normal slots must match their owners");
    assert_eq!(connection_data.counts.soft, owned_slots.soft, "soft slots must match their owners");
    assert_eq!(
        connection_data.connections,
        connection_data.counts.normal + u32::from(connection_data.counts.soft),
        "aggregate connection count must equal the normal and soft counters"
    );
}

fn assert_no_real_connection_slots(connection_data: &UserConnectionData) {
    assert_eq!(connection_data.connections, 0);
    assert_eq!(connection_data.counts.normal, 0);
    assert_eq!(connection_data.counts.soft, 0);
    assert_connection_ownership_invariants(connection_data);
}

fn assert_active_stream_kind(connection_data: &UserConnectionData, stream_uid: u32, expected_kind: ConnectionKind) {
    assert!(
        connection_data.streams.iter().any(|stream| stream.uid == stream_uid && !stream.preserved),
        "stream {stream_uid} must remain active"
    );
    assert_eq!(connection_data.stream_kinds.get(&stream_uid), Some(&expected_kind));
}

fn assert_single_normal_stream_slot(connection_data: &UserConnectionData, stream_uid: u32) {
    assert_eq!(connection_data.connections, 1);
    assert_eq!(connection_data.counts.normal, 1);
    assert_eq!(connection_data.counts.soft, 0);
    assert_active_stream_kind(connection_data, stream_uid, ConnectionKind::Normal);
    assert_connection_ownership_invariants(connection_data);
}

fn assert_preserved_session_is_uncounted(connection_data: &UserConnectionData, session_token: &str, stream_uid: u32) {
    let session = connection_data
        .sessions
        .iter()
        .find(|session| session.token == session_token)
        .expect("preserved session must exist");
    assert_eq!(session.lifecycle, PlaybackLifecycle::Preserved);

    let stream =
        connection_data.streams.iter().find(|stream| stream.uid == stream_uid).expect("preserved stream must exist");
    assert!(stream.preserved);
    assert_eq!(stream.session_token.as_deref(), Some(session_token));
    assert!(!connection_data.stream_kinds.contains_key(&stream_uid));
}

async fn commit_and_preserve_adaptive_session(
    manager: &ActiveUserManager,
    user: &ProxyUserCredentials,
    session_token: &str,
    stream_uid: u32,
    addr: SocketAddr,
    connection_kind: ConnectionKind,
) {
    let fingerprint = Fingerprint::new(format!("fp-preserved-{stream_uid}"), addr.ip().to_string(), addr);

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user,
            session_token,
            virtual_id: stream_uid,
            provider: "provider-a",
            stream_url: "http://localhost/live-preserved.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(connection_kind),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: stream_uid,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(stream_uid),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some(session_token),
        })
        .await
        .expect("adaptive session stream should bind");

    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        let session = connection_data
            .sessions
            .iter()
            .find(|session| session.token == session_token)
            .expect("committed session must exist");
        assert_eq!(session.lifecycle, PlaybackLifecycle::Active);
        assert_eq!(connection_data.stream_kinds.get(&stream_uid), Some(&connection_kind));
        assert_connection_ownership_invariants(connection_data);
    }

    assert!(manager.release_stream(&addr).await.is_none(), "adaptive stream should be preserved");

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_preserved_session_is_uncounted(connection_data, session_token, stream_uid);
    assert_connection_ownership_invariants(connection_data);
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
async fn eviction_candidates_include_other_ips_for_user_wide_rules() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let first_addr: SocketAddr = "127.0.0.1:55041".parse().unwrap();
    let second_addr: SocketAddr = "127.0.0.1:55042".parse().unwrap();
    let first_fp = Fingerprint::new("fp-user-wide-1".to_string(), "10.0.0.1".to_string(), first_addr);
    let second_fp = Fingerprint::new("fp-user-wide-2".to_string(), "10.0.0.2".to_string(), second_addr);

    manager.add_connection(&first_addr).await;
    manager.add_connection(&second_addr).await;

    let user = test_user_credentials("same-user", 2, 0);
    for (token, addr, channel_id) in [("tok-41", first_addr, 1041u32), ("tok-42", second_addr, 1042)] {
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

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 41,
            meter_uid: 0,
            username: "same-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &first_fp,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1041),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-41"),
        })
        .await;

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 42,
            meter_uid: 0,
            username: "same-user",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &second_fp,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1042),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-42"),
        })
        .await;

    let candidates = manager.get_eviction_candidates("same-user", "10.0.0.1").await;
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().any(|candidate| candidate.addr == first_addr));
    assert!(candidates.iter().any(|candidate| candidate.addr == second_addr));
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
async fn test_kicked_release_invalidates_removed_session_tokens() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let kicked_addr: SocketAddr = "127.0.0.1:55015".parse().unwrap();
    let survivor_addr: SocketAddr = "127.0.0.1:55016".parse().unwrap();
    let kicked_fingerprint = Fingerprint::new("fp-kicked".to_string(), "127.0.0.1".to_string(), kicked_addr);
    let survivor_fingerprint = Fingerprint::new("fp-survivor".to_string(), "127.0.0.1".to_string(), survivor_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("kicked-user");
    user.max_connections = 1;

    manager.add_connection(&kicked_addr).await;
    manager.add_connection(&survivor_addr).await;

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-kicked",
            virtual_id: 2015,
            provider: "provider-a",
            stream_url: "http://localhost/live-1.ts",
            addr: &kicked_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-survivor",
            virtual_id: 2016,
            provider: "provider-a",
            stream_url: "http://localhost/live-2.ts",
            addr: &survivor_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: true,
        })
        .await;

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 15,
            meter_uid: 0,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &kicked_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(2015),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-kicked"),
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 16,
            meter_uid: 0,
            username: &user.username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &survivor_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(2016),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-survivor"),
        })
        .await;

    let removed = manager.release_connection_as_kicked(&kicked_addr).await;
    assert!(removed.addr_removed);
    assert_eq!(removed.removed_streams.len(), 1);
    assert_eq!(
        manager.connection_admission_for_session(&user.username, 1, 0, "tok-kicked").await.permission,
        UserConnectionPermission::Exhausted
    );
    assert_eq!(
        manager.connection_admission_for_session(&user.username, 1, 0, "tok-survivor").await.permission,
        UserConnectionPermission::Allowed
    );
}

#[tokio::test]
async fn test_grace_at_limit_remains_active_until_connections_drop_below_limit() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55017".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-grace".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 17,
            meter_uid: 0,
            username: "grace-at-limit",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(2017),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-grace"),
        })
        .await;

    assert!(manager.grant_grace("grace-at-limit").await);
    assert_eq!(
        manager.connection_admission("grace-at-limit", 1, 0).await.permission,
        UserConnectionPermission::Exhausted
    );
    assert!(!manager.grant_grace("grace-at-limit").await);
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

    manager.mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, 1).await;
    {
        let mut connections = manager.connections.write().await;
        if let Some(registration) = connections.key_by_addr.get_mut(&protected_addr) {
            registration.username = user.username.clone();
        }
        let protection = connections
            .recently_evicted_sessions
            .get_mut("tok-guard-session")
            .expect("recent eviction guard should exist");
        protection.expires_at = current_time_secs().saturating_sub(1);
    }

    assert_eq!(manager.recently_evicted_session_protected_addr("tok-guard-session").await, Some(protected_addr));
}

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

    manager.mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, 10).await;

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

    manager.mark_recent_eviction_guard_for_addr(&evicted_addr, protected_addr, 10).await;

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
async fn test_same_session_token_refreshes_meter_metadata_on_reuse() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55031".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-3".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    let first = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 11,
            meter_uid: 101,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(3001),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-meter"),
        })
        .await
        .expect("initial stream should register");
    assert_eq!(first.uid, 11);
    assert_eq!(first.meter_uid, 101);

    let second = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 22,
            meter_uid: 202,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-b".intern(),
            stream_channel: &test_adaptive_channel(3002),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-meter"),
        })
        .await
        .expect("reused stream should register");

    assert_eq!(second.uid, 11, "logical stream identity should stay stable on session reuse");
    assert_eq!(second.meter_uid, 202, "reused stream must refresh its meter mapping");

    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].uid, 11);
    assert_eq!(streams[0].meter_uid, 202);
    assert_eq!(streams[0].provider.as_ref(), "provider-b");
    assert_eq!(streams[0].channel.virtual_id, 3002);
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
async fn unlimited_user_can_open_same_and_different_live_streams_from_same_ip() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let username = "unlimited-same-ip";
    let client_ip = "10.9.0.1";
    let addrs = [
        "10.9.0.1:55101".parse::<SocketAddr>().unwrap(),
        "10.9.0.1:55102".parse::<SocketAddr>().unwrap(),
        "10.9.0.1:55103".parse::<SocketAddr>().unwrap(),
    ];
    let fingerprints = [
        Fingerprint::new("fp-unlimited-1".to_string(), client_ip.to_string(), addrs[0]),
        Fingerprint::new("fp-unlimited-2".to_string(), client_ip.to_string(), addrs[1]),
        Fingerprint::new("fp-unlimited-3".to_string(), client_ip.to_string(), addrs[2]),
    ];

    for addr in addrs {
        manager.add_connection(&addr).await;
    }

    for (idx, (fingerprint, virtual_id)) in fingerprints.iter().zip([4100, 4100, 4101]).enumerate() {
        let token = format!("tok-unlimited-{idx}");
        manager
            .update_connection(ActiveUserConnectionParams {
                uid: 410 + u32::try_from(idx).unwrap_or_default(),
                meter_uid: 0,
                username,
                max_connections: 0,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(virtual_id),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some(&token),
            })
            .await
            .expect("unlimited stream should register");
    }

    assert_eq!(manager.user_connections(username).await, 3);
    assert_eq!(manager.active_streams().await.len(), 3);
    assert_eq!(manager.connection_admission(username, 0, 0).await.permission, UserConnectionPermission::Allowed);
    assert_eq!(
        manager.connection_admission_for_session(username, 0, 0, "tok-unlimited-new").await.permission,
        UserConnectionPermission::Allowed
    );
}

#[tokio::test]
async fn release_stream_by_uid_removes_only_matching_stream_on_shared_addr() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let Some(addr) = "127.0.0.1:55033".parse::<SocketAddr>().ok() else {
        return;
    };
    let fingerprint = Fingerprint::new("fp-key-shared-addr".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    for uid in [41, 42] {
        manager
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: uid + 300,
                username: "user1",
                max_connections: 0,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_channel(3004),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-live-shared-addr"),
            })
            .await;
    }

    let removed = manager.release_stream_by_uid(&addr, 42).await;
    assert!(removed.as_ref().is_some_and(|stream| stream.uid == 42));

    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].uid, 41);
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
async fn connection_counts_are_broadcast_when_active_user_logging_is_disabled() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let mut events = event_manager.get_event_channel();
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55037".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-count-events".to_string(), "127.0.0.1".to_string(), addr);
    manager.add_connection(&addr).await;
    manager.release_connection(&addr).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv()).await.is_err(),
        "closing an unowned socket must not broadcast unchanged connection counts"
    );
    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 48,
            meter_uid: 0,
            username: "event-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_series_channel(3048),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("direct Series stream should register");

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("connection count update should be broadcast")
            .expect("event channel should remain open"),
        EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(1, 1))
    );

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 49,
            meter_uid: 0,
            username: "event-user",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_series_channel(3048),
            user_agent: Cow::Borrowed("ua"),
            session_token: None,
        })
        .await
        .expect("same direct Series stream should be reused");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv()).await.is_err(),
        "unchanged connection counts must not broadcast another full snapshot"
    );

    manager.release_stream_by_uid(&addr, 48).await.expect("direct Series stream should release");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("released count update should be broadcast")
            .expect("event channel should remain open"),
        EventMessage::ActiveUser(ActiveUserConnectionChange::Connections(0, 0))
    );
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
async fn test_release_stream_preserved_path_emits_connection_update_event() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let mut events = event_manager.get_event_channel();

    let addr: SocketAddr = "127.0.0.1:55081".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-key-8".to_string(), "127.0.0.1".to_string(), addr);
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-event",
            virtual_id: 8001,
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
            uid: 111,
            meter_uid: 211,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &StreamChannel { item_type: PlaylistItemType::LiveHls, ..test_channel(8001) },
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-event"),
        })
        .await;
    let _ = events.try_recv();

    let released = manager.release_stream(&addr).await;
    assert!(released.is_none(), "adaptive stream should remain logically preserved");

    let event = events.try_recv().expect("preserved release should emit an ActiveUser event");
    assert!(matches!(event, EventMessage::ActiveUser(_)));
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
async fn socket_expiry_deadline_does_not_refresh_active_vod_streams_without_activity() {
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

    let previous_registration_ts = {
        let mut connections = manager.connections.write().await;
        let registration = connections.key_by_addr.get_mut(&addr).expect("registration should exist");
        registration.ts = registration.ts.saturating_sub(DEFAULT_ACTIVE_SOCKET_TTL_SECS + 5);
        registration.ts
    };

    let deadline =
        manager.socket_expiry_deadline(&addr).await.expect("VOD streams should stay scheduled for expiry tracking");

    let unchanged_registration_ts = {
        let connections = manager.connections.read().await;
        connections.key_by_addr.get(&addr).expect("registration should still exist").ts
    };

    assert_eq!(unchanged_registration_ts, previous_registration_ts);
    assert_eq!(
        deadline,
        previous_registration_ts.saturating_add(manager.active_socket_ttl_secs()),
        "deadline checks must not refresh VOD sockets without real body activity"
    );
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

#[tokio::test]
async fn session_activation_keeps_first_hls_slot_uncommitted_before_stream_registration() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-hls-reserve");
    user.max_connections = 1;

    let first_addr: SocketAddr = "127.0.0.1:55180".parse().unwrap();
    let second_addr: SocketAddr = "127.0.0.1:55181".parse().unwrap();

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-first",
            virtual_id: 9201,
            provider: "provider-a",
            stream_url: "http://localhost/live-a.m3u8",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-second",
            virtual_id: 9202,
            provider: "provider-a",
            stream_url: "http://localhost/live-b.m3u8",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let first_admission =
        manager.connection_admission_for_session_activation(&user.username, user.max_connections, 0, "tok-first").await;
    let second_admission = manager
        .connection_admission_for_session_activation(&user.username, user.max_connections, 0, "tok-second")
        .await;

    assert_eq!(first_admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(first_admission.kind, Some(ConnectionKind::Normal));
    assert_eq!(second_admission.permission, UserConnectionPermission::Allowed);

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_eq!(connection_data.connections, 0);
    assert_eq!(connection_data.counts.normal, 0);
    assert_eq!(connection_data.streams.len(), 0);
    assert!(connection_data
        .sessions
        .iter()
        .find(|session| session.token == "tok-first")
        .is_some_and(|session| !session.lifecycle.is_counted()));
    assert!(connection_data
        .sessions
        .iter()
        .find(|session| session.token == "tok-second")
        .is_some_and(|session| !session.lifecycle.is_counted()));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn binding_reserved_sessions_keeps_hard_and_soft_counts_stable() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-hls-soft");
    user.max_connections = 1;
    user.soft_connections = 1;

    let first_addr: SocketAddr = "127.0.0.1:55182".parse().unwrap();
    let second_addr: SocketAddr = "127.0.0.1:55183".parse().unwrap();
    let first_fingerprint = Fingerprint::new("fp-hls-1".to_string(), "127.0.0.1".to_string(), first_addr);
    let second_fingerprint = Fingerprint::new("fp-hls-2".to_string(), "127.0.0.1".to_string(), second_addr);

    manager.add_connection(&first_addr).await;
    manager.add_connection(&second_addr).await;

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-normal",
            virtual_id: 9203,
            provider: "provider-a",
            stream_url: "http://localhost/live-normal.m3u8",
            addr: &first_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-soft",
            virtual_id: 9204,
            provider: "provider-a",
            stream_url: "http://localhost/live-soft.m3u8",
            addr: &second_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let first_admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            "tok-normal",
        )
        .await;
    assert_eq!(first_admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(first_admission.kind, Some(ConnectionKind::Normal));

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 401,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &first_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(9203),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-normal"),
        })
        .await
        .expect("reserved normal session should bind");

    let second_admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            "tok-soft",
        )
        .await;
    assert_eq!(second_admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(second_admission.kind, Some(ConnectionKind::Soft));

    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 402,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Soft,
            priority: 0,
            soft_priority: 0,
            fingerprint: &second_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(9204),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-soft"),
        })
        .await
        .expect("reserved soft session should bind");

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_eq!(connection_data.connections, 2);
    assert_eq!(connection_data.counts.normal, 1);
    assert_eq!(connection_data.counts.soft, 1);
    assert_eq!(connection_data.streams.len(), 2);
    assert_eq!(
        connection_data.stream_kinds.get(&401),
        Some(&ConnectionKind::Normal),
        "binding a reserved normal session must not increment counts twice"
    );
    assert_eq!(
        connection_data.stream_kinds.get(&402),
        Some(&ConnectionKind::Soft),
        "binding a reserved soft session must keep the soft classification"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn origin_policy_refresh_promotes_counted_soft_session_when_hard_slot_is_available() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-hls-policy-refresh");
    user.max_connections = 1;
    user.soft_connections = 1;

    let normal_addr: SocketAddr = "127.0.0.1:55185".parse().unwrap();
    let soft_addr: SocketAddr = "127.0.0.1:55186".parse().unwrap();
    let normal_fingerprint = Fingerprint::new("fp-hls-policy-1".to_string(), "127.0.0.1".to_string(), normal_addr);
    let soft_fingerprint = Fingerprint::new("fp-hls-policy-2".to_string(), "127.0.0.1".to_string(), soft_addr);

    manager.add_connection(&normal_addr).await;
    manager.add_connection(&soft_addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-normal",
            virtual_id: 9210,
            provider: "provider-a",
            stream_url: "http://localhost/live-normal.m3u8",
            addr: &normal_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-soft",
            virtual_id: 9211,
            provider: "provider-a",
            stream_url: "http://localhost/live-soft.m3u8",
            addr: &soft_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let normal_admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            "tok-normal",
        )
        .await;
    assert_eq!(normal_admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(normal_admission.kind, Some(ConnectionKind::Normal));
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 411,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &normal_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(9210),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-normal"),
        })
        .await
        .expect("normal stream should bind");

    let soft_admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            "tok-soft",
        )
        .await;
    assert_eq!(soft_admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(soft_admission.kind, Some(ConnectionKind::Soft));
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 412,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Soft,
            priority: 0,
            soft_priority: 0,
            fingerprint: &soft_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(9211),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-soft"),
        })
        .await
        .expect("soft stream should bind");

    assert!(manager.release_session_streams_and_counted_reservation(&user.username, "tok-normal").await);
    {
        let connections = manager.connections.read().await;
        let connection_data = connections.by_key.get(&user.username).expect("user connection data");
        assert_eq!(connection_data.connections, 1);
        assert_eq!(connection_data.counts.normal, 0);
        assert_eq!(connection_data.counts.soft, 1);
        assert_eq!(
            connection_data
                .sessions
                .iter()
                .find(|session| session.token == "tok-soft")
                .and_then(|session| session.connection_kind),
            Some(ConnectionKind::Soft)
        );
    }

    let refreshed_kind = manager
        .refresh_session_connection_kind_for_origin_policy(
            &user.username,
            user.max_connections,
            user.soft_connections,
            "tok-soft",
        )
        .await;
    assert_eq!(refreshed_kind, Some(ConnectionKind::Normal));

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_eq!(connection_data.connections, 1);
    assert_eq!(connection_data.counts.normal, 1);
    assert_eq!(connection_data.counts.soft, 0);
    assert_eq!(
        connection_data
            .sessions
            .iter()
            .find(|session| session.token == "tok-soft")
            .and_then(|session| session.connection_kind),
        Some(ConnectionKind::Normal)
    );
    assert_eq!(connection_data.stream_kinds.get(&412), Some(&ConnectionKind::Normal));
}

#[tokio::test]
async fn origin_policy_refresh_returns_none_for_pending_grace_without_available_slot() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-pending-grace-origin-policy");
    user.max_connections = 1;

    let active_addr: SocketAddr = "127.0.0.1:55195".parse().unwrap();
    let pending_addr: SocketAddr = "127.0.0.1:55196".parse().unwrap();
    let active_fingerprint = Fingerprint::new("active".to_string(), "127.0.0.1".to_string(), active_addr);

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-active",
            virtual_id: 9301,
            provider: "provider-a",
            stream_url: "http://localhost/live-active.m3u8",
            addr: &active_addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 9301,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &active_fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(9301),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-active"),
        })
        .await
        .expect("active stream should bind the only normal slot");

    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-pending",
            virtual_id: 9302,
            provider: "provider-a",
            stream_url: "http://localhost/live-pending.m3u8",
            addr: &pending_addr,
            connection_permission: UserConnectionPermission::GracePeriod,
            connection_kind: None,
            socket_bound: false,
        })
        .await;
    manager
        .mark_pending_provider(
            &user.username,
            "tok-pending",
            PendingProviderReason::GraceHold,
            current_time_secs() + 30,
        )
        .await
        .expect("pending session should be marked");

    let refreshed_kind = manager
        .refresh_session_connection_kind_for_origin_policy(
            &user.username,
            user.max_connections,
            user.soft_connections,
            "tok-pending",
        )
        .await;

    assert_eq!(refreshed_kind, None);
}

#[tokio::test]
async fn release_unbound_session_reservation_frees_reserved_slot() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-release-reservation");
    user.max_connections = 1;

    let addr: SocketAddr = "127.0.0.1:55184".parse().unwrap();
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-release",
            virtual_id: 9205,
            provider: "provider-a",
            stream_url: "http://localhost/live-release.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let admission = manager
        .connection_admission_for_session_activation(&user.username, user.max_connections, 0, "tok-release")
        .await;
    assert_eq!(admission.permission, UserConnectionPermission::Allowed);

    manager.release_unbound_session_reservation(&user.username, "tok-release", None, false).await;

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_eq!(connection_data.connections, 0);
    assert_eq!(connection_data.counts.normal, 0);
    assert_eq!(connection_data.streams.len(), 0);
    assert!(connection_data
        .sessions
        .iter()
        .find(|session| session.token == "tok-release")
        .is_some_and(|session| !session.lifecycle.is_counted()));
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
    assert_eq!(virtual_admission.permission, UserConnectionPermission::Exhausted);

    let admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            session_token,
        )
        .await;

    assert_eq!(admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(admission.kind, Some(ConnectionKind::Normal));
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
    assert_eq!(admission.permission, UserConnectionPermission::Allowed);
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
    assert_eq!(admission.permission, UserConnectionPermission::Allowed);
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
        assert_eq!(admission.permission, UserConnectionPermission::Allowed);

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
async fn dashboard_counts_only_real_slots_during_preserved_reactivation_admission() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let user = test_user_credentials("user-preserved-dashboard", 1, 0);
    let addr: SocketAddr = "127.0.0.1:55205".parse().unwrap();
    let session_token = "tok-preserved-dashboard";
    let stream_uid = 605;

    commit_and_preserve_adaptive_session(&manager, &user, session_token, stream_uid, addr, ConnectionKind::Normal)
        .await;
    assert_eq!(manager.active_users_and_connections().await, (0, 0));

    let admission = manager
        .connection_admission_for_session_activation(
            &user.username,
            user.max_connections,
            user.soft_connections,
            session_token,
        )
        .await;
    assert_eq!(admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(manager.active_users_and_connections().await, (0, 0));

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_preserved_session_is_uncounted(connection_data, session_token, stream_uid);
    assert_no_real_connection_slots(connection_data);
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
    assert_eq!(admission.permission, UserConnectionPermission::Allowed);
    assert_eq!(admission.kind, Some(ConnectionKind::Soft));

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
async fn release_unbound_session_reservation_ignores_stale_transition_version() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user-stale-release");

    let addr: SocketAddr = "127.0.0.1:55194".parse().unwrap();
    let stale_version = manager
        .ensure_user_session_placeholder(CreateUserSessionParams {
            user: &user,
            session_token: "tok-stale-release",
            virtual_id: 9206,
            provider: "provider-a",
            stream_url: "http://localhost/live-stale.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    let _ = manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-stale-release",
            virtual_id: 9206,
            provider: "provider-b",
            stream_url: "http://localhost/live-stale-updated.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    manager.release_unbound_session_reservation(&user.username, "tok-stale-release", Some(stale_version), true).await;

    let session = manager
        .get_and_update_user_session(&user.username, "tok-stale-release")
        .await
        .expect("stale rollback must not remove the newer session");
    assert!(session.transition_version > stale_version);
    assert_eq!(session.provider.as_ref(), "provider-b");
    assert_eq!(session.stream_url.as_ref(), "http://localhost/live-stale-updated.m3u8");
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
        admission.permission,
        UserConnectionPermission::Exhausted,
        "a preserved stream should still reserve capacity against unrelated playback admissions"
    );
}

#[tokio::test]
async fn connection_admission_for_session_evaluates_admission_for_uncounted_session() {
    // Bug: connection_admission_for_session returns Allowed for any existing session,
    // even if it's uncounted (preserved). This causes strategy evaluation to be skipped.
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55310".parse().unwrap();
    let username = "user-uncounted-admission";
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user.max_connections = 1;
    user.soft_connections = 0;

    // Create session + counted stream (HLS type = preserved after release)
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
            fingerprint: &Fingerprint::new("fp".to_string(), "192.168.1.50".to_string(), addr),
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(6001),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-uncounted"),
        })
        .await
        .expect("first stream");

    // Release to preserve (uncounted session, but counts.normal still = 1 from the stream)
    manager.release_stream(&addr).await;
    // After preserve: session is uncounted, stream is preserved, connections=0
    // BUT the stream was removed, so counts.normal is decremented -> counts=0
    assert_eq!(manager.user_connections(username).await, 0);

    // Add a second stream first - this uses a different session token and consumes the slot
    let second_addr: SocketAddr = "192.168.1.100:55311".parse().unwrap();
    manager.add_connection(&second_addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 2,
            meter_uid: 0,
            username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &Fingerprint::new("fp2".to_string(), "192.168.1.100".to_string(), second_addr),
            provider: "provider-b".intern(),
            stream_channel: &test_channel(6002),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-second"),
        })
        .await
        .expect("second stream");
    // Now user is at limit: connections=1, counts.normal=1, max_connections=1
    assert_eq!(manager.user_connections(username).await, 1);

    // connection_admission_for_session for the PRESERVED session token should return
    // Exhausted so that eviction strategies can run and evict the preserved stream,
    // freeing a slot for the uncounted session to reactivate
    let admission = manager.connection_admission_for_session(username, 1, 0, "tok-uncounted").await;
    assert_eq!(
        admission.permission,
        UserConnectionPermission::Exhausted,
        "uncounted session should not bypass admission when user is at limit; \
             bug: session exists -> Allowed -> strategy evaluation skipped"
    );
}

#[tokio::test]
async fn playback_transition_gate_serializes_same_session() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

    let first_guard = manager.acquire_playback_transition("user-gated", "tok-gated").await;
    let second_manager = Arc::clone(&manager);
    let waiting = tokio::spawn(async move {
        let _second_guard = second_manager.acquire_playback_transition("user-gated", "tok-gated").await;
    });

    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        !waiting.is_finished(),
        "same-session transition gate should block a concurrent transition until the first completes"
    );

    drop(first_guard);
    tokio::time::timeout(Duration::from_millis(100), waiting)
        .await
        .expect("second transition should proceed once the first guard is released")
        .expect("second transition task should complete");
}

#[tokio::test]
async fn playback_transition_gate_cleanup_removes_idle_gates_on_next_acquire() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let first_guard = manager.acquire_playback_transition("user-gated-cleanup", "tok-first").await;
    assert_eq!(manager.transition_gates.lock().await.len(), 1);
    drop(first_guard);

    let second_guard = manager.acquire_playback_transition("user-gated-cleanup", "tok-second").await;
    assert_eq!(manager.transition_gates.lock().await.len(), 1);
    drop(second_guard);
}

#[tokio::test]
async fn check_divergence_detects_connection_count_mismatch() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

    let addr: SocketAddr = "127.0.0.1:55902".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "div-user-2".to_string();

    // Create a counted session without a stream or matching legacy counter.
    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.entry(user.username.clone()).or_insert_with(|| UserConnectionData::new(0, 1, 0));
        data.add_session(UserSession {
            token: "tok-div-2".to_string(),
            transition_version: 1,
            virtual_id: 9002,
            provider: "provider-a".intern(),
            stream_url: "http://localhost/stream.ts".intern(),
            provider_session_headers: HashMap::new(),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: None,
            lifecycle: PlaybackLifecycle::Active,
        });
    }

    let connections = manager.connections.read().await;
    let data = connections.by_key.get(&user.username).expect("user connection data");
    let snapshot = ActiveUserManager::build_divergence_snapshot(data, &user.username);
    assert!(snapshot.kinds.contains(&DivergenceKind::CountedSessionWithoutStream));
    assert!(snapshot.kinds.contains(&DivergenceKind::ConnectionCountMismatch { legacy: 0, counted: 1 }));
    drop(connections);
    manager.log_divergence_snapshot(Some(snapshot)).await;
}

#[tokio::test]
async fn check_divergence_detects_stream_without_counted_session() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

    let addr: SocketAddr = "127.0.0.1:55903".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "div-user-3".to_string();

    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.entry(user.username.clone()).or_insert_with(|| UserConnectionData::new(0, 1, 0));

        // Add a session with GraceHold pending — exempt from Invariant 1
        data.add_session(UserSession {
            token: "tok-div-3".to_string(),
            transition_version: 1,
            virtual_id: 9003,
            provider: "provider-a".intern(),
            stream_url: "http://localhost/stream.ts".intern(),
            provider_session_headers: HashMap::new(),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: None,
            lifecycle: PlaybackLifecycle::PendingProvider {
                data: PendingProviderState {
                    reason_code: PendingProviderReason::GraceHold,
                    created_at: current_time_secs(),
                    deadline: current_time_secs() + 30,
                    version: 1,
                    wake_source: None,
                },
            },
        });
        data.increment_kind(ConnectionKind::Normal);

        // Add a stream whose session_token doesn't match any counted session
        let orphan_stream = StreamInfo::new(shared::model::StreamInfoParams {
            uid: 903,
            meter_uid: 0,
            username: &user.username,
            addr: &addr,
            client_ip: "127.0.0.1",
            provider: "provider-a".intern(),
            stream_channel: StreamChannel {
                target_id: 1,
                virtual_id: 9003,
                provider_id: 1,
                input_name: "provider-a".intern(),
                item_type: PlaylistItemType::Live,
                cluster: XtreamCluster::Live,
                group: "g".intern(),
                title: "t".intern(),
                url: "http://localhost/stream.ts".intern(),
                shared: false,
                shared_joined_existing: None,
                shared_stream_id: None,
                technical: None,
                epg_channel_id: None,
                epg_reference_ts: None,
                upstream_user_agent: None,
            },
            user_agent: "ua".to_string(),
            country_code: None,
            session_token: Some("tok-orphan"),
        });
        data.streams.push(orphan_stream);
        data.stream_kinds.insert(903, ConnectionKind::Normal);
    }

    let connections = manager.connections.read().await;
    let data = connections.by_key.get(&user.username).expect("user connection data");
    let snapshot = ActiveUserManager::build_divergence_snapshot(data, &user.username);
    assert!(snapshot.kinds.contains(&DivergenceKind::StreamWithoutCountedSession));
    drop(connections);
    manager.log_divergence_snapshot(Some(snapshot)).await;
}

#[tokio::test]
async fn divergence_log_rate_limited_within_cooldown_window() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));

    let addr: SocketAddr = "127.0.0.1:55904".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = "div-user-4".to_string();

    // Create mismatch
    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.entry(user.username.clone()).or_insert_with(|| UserConnectionData::new(0, 1, 0));
        data.increment_kind(ConnectionKind::Normal);
        data.add_session(UserSession {
            token: "tok-div-4".to_string(),
            transition_version: 1,
            virtual_id: 9004,
            provider: "provider-a".intern(),
            stream_url: "http://localhost/stream.ts".intern(),
            provider_session_headers: HashMap::new(),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: None,
            lifecycle: PlaybackLifecycle::Prepared,
        });
    }

    let connections = manager.connections.read().await;
    let data = connections.by_key.get(&user.username).expect("user connection data");
    let snapshot = ActiveUserManager::build_divergence_snapshot(data, &user.username);
    drop(connections);
    manager.log_divergence_snapshot(Some(snapshot)).await;
    let key = divergence_key(&user.username, &DivergenceKind::ConnectionCountMismatch { legacy: 1, counted: 0 });
    let first_logged = {
        let cache = manager.divergence_cache.lock().await;
        let entry = cache.peek(&key).expect("first divergence should populate the cache");
        assert_eq!(entry.count_since_last_log, 0);
        entry.last_logged
    };

    let connections = manager.connections.read().await;
    let data = connections.by_key.get(&user.username).expect("user connection data");
    let snapshot = ActiveUserManager::build_divergence_snapshot(data, &user.username);
    drop(connections);
    manager.log_divergence_snapshot(Some(snapshot)).await;
    let connections = manager.connections.read().await;
    let data = connections.by_key.get(&user.username).expect("user connection data");
    let snapshot = ActiveUserManager::build_divergence_snapshot(data, &user.username);
    drop(connections);
    manager.log_divergence_snapshot(Some(snapshot)).await;
    let cache = manager.divergence_cache.lock().await;
    let entry = cache.peek(&key).expect("repeated divergence should remain cached");
    assert_eq!(entry.count_since_last_log, 2);
    assert_eq!(entry.last_logged, first_logged);
}
