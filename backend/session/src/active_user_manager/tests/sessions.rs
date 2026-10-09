use super::{
    create_provider_header_session, provider_header_test_manager, session_identity, test_adaptive_channel,
    test_channel, test_series_channel, ActiveUserConnectionParams, ActiveUserManager, CreateUserSessionParams,
    PendingProviderReason, PendingProviderState, PendingProviderWakeSource, PlaybackLifecycle,
    PlaybackSessionRegistration, UserConnectionData, UserSession,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::{ActiveUserConnectionChange, EventMessage, StreamInfo, UserConnectionPermission},
    utils::{current_time_secs, Internable},
};
use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

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
            provider_session_headers_host: None,
            media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            lifecycle: PlaybackLifecycle::Expired,
            ..Default::default()
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
            provider_session_headers_host: None,
            media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            ..Default::default()
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

/// PR1 regression: two bodies with distinct request UIDs that map to the same
/// display stream must both clean up correctly. The current body path passes the
/// *display* UID (`stream_info.uid`) to cleanup, not the request UID. This test
/// asserts that the cleanup is request-affine — releasing the display UID twice
/// must drain both claims, and the second release must find the stream.
#[tokio::test]
async fn pr1_two_bodies_same_display_uid_both_cleanups_must_succeed() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55035".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-pr1-display-vs-request".to_string(), "127.0.0.1".to_string(), addr);
    manager.add_connection(&addr).await;

    // Request 41 creates the display stream.
    let first = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 41,
            meter_uid: 301,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(3006),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-pr1"),
        })
        .await
        .expect("first body should register");

    // Request 42 reuses the same playback (same session token, same channel).
    let second = manager
        .update_connection(ActiveUserConnectionParams {
            uid: 42,
            meter_uid: 302,
            username: "user1",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_adaptive_channel(3006),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("tok-pr1"),
        })
        .await
        .expect("second body should register");

    // Both return the same display UID.
    assert_eq!(first.uid, second.uid, "display UID must be stable across requests");
    let display_uid = first.uid;

    // Fixed behavior: the body passes its own request UID to cleanup, not the display UID.
    // First body (request 41) finishes → should NOT remove the stream (request 42 still active).
    let removed_first = manager.release_stream_by_uid(&addr, 41).await;
    assert!(removed_first.is_none(), "first body cleanup must keep stream alive for second body");

    assert_eq!(manager.active_streams().await.len(), 1, "stream must survive first body cleanup");

    // Second body (request 42) finishes → should remove the stream (no more claims).
    let removed_second = manager.release_stream_by_uid(&addr, 42).await;
    assert!(removed_second.is_some(), "second body cleanup must find and remove the stream");
    assert_eq!(removed_second.unwrap().uid, display_uid);

    assert!(manager.active_streams().await.is_empty(), "no streams left after all bodies finish");
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
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
async fn update_session_provider_binding_updates_session_and_streams() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55140".parse().unwrap();
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 1;

    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "tok-binding",
            virtual_id: 100,
            provider: "account-b",
            stream_url: "http://example.com/vod/movie.mkv?token=account-b",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;

    {
        let mut connections = manager.connections.write().await;
        let data = connections.by_key.get_mut("user1").expect("user connection data");
        data.streams.push(StreamInfo::new(shared::model::StreamInfoParams {
            uid: 1,
            meter_uid: 1,
            username: &user.username,
            addr: &addr,
            client_ip: "127.0.0.1",
            provider: "account-b".intern(),
            stream_channel: test_channel(100),
            user_agent: "ua".to_string(),
            country_code: None,
            session_token: Some("tok-binding"),
        }));
    }

    manager
        .update_session_provider_binding(
            "user1",
            "tok-binding",
            "account-a".intern(),
            "http://example.com/vod/movie.mkv?token=account-a".into(),
        )
        .await;

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get("user1").expect("user connection data");
    assert_eq!(connection_data.sessions[0].provider.as_ref(), "account-a");
    assert_eq!(connection_data.sessions[0].stream_url.as_ref(), "http://example.com/vod/movie.mkv?token=account-a");
    assert_eq!(connection_data.streams[0].provider.as_ref(), "account-a");
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

    assert_eq!(first_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(first_admission.kind(), Some(ConnectionKind::Normal));
    assert_eq!(second_admission.permission(), UserConnectionPermission::Allowed);

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
    assert_eq!(first_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(first_admission.kind(), Some(ConnectionKind::Normal));

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
    assert_eq!(second_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(second_admission.kind(), Some(ConnectionKind::Soft));

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

/// Kicked and evicted sessions are marked ended, also with the reentry guard disabled.
#[tokio::test]
async fn kicked_and_evicted_sessions_are_marked_ended() {
    let manager = provider_header_test_manager();
    let mut user = ProxyUserCredentials::default();
    user.username = "user-ended".to_string();
    create_provider_header_session(&manager, &user, "http://cdn.example/a/video.m3u8").await;
    let addr: SocketAddr = "127.0.0.1:55420".parse().unwrap_or_else(|_| unreachable!());

    assert!(!manager.is_session_ended("tok-host").await);
    assert!(manager.terminate_session(&user.username, "tok-host").await);
    assert!(!manager.is_session_ended("tok-host").await, "a plain terminate (e.g. manifest failure) is not an end");

    create_provider_header_session(&manager, &user, "http://cdn.example/a/video.m3u8").await;
    manager.terminate_sessions_for_addr(&user.username, &addr).await;
    assert!(manager.is_session_ended("tok-host").await);

    manager.mark_session_ended("tok-explicit").await;
    assert!(manager.is_session_ended("tok-explicit").await);
}

pub(in crate::active_user_manager::tests) async fn register_guarded_resource(
    manager: &ActiveUserManager,
    user: &ProxyUserCredentials,
    token: &str,
    uid: u32,
    requirement: &PlaybackSessionRegistration,
) -> Option<StreamInfo> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 55500 + u16::try_from(uid).ok()?));
    let fingerprint = Fingerprint::new("guarded-player".to_string(), "127.0.0.1".to_string(), addr);
    manager
        .update_connection_with_session_registration(
            ActiveUserConnectionParams {
                uid,
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: ConnectionKind::Normal,
                priority: user.priority,
                soft_priority: user.soft_priority,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(7010),
                user_agent: Cow::Borrowed("guarded-player"),
                session_token: Some(token),
            },
            Some(requirement),
        )
        .await
}

#[tokio::test]
async fn guarded_resource_registration_rejects_removed_and_replaced_sessions() -> Result<(), Box<dyn std::error::Error>>
{
    let manager = provider_header_test_manager();
    let mut user = ProxyUserCredentials::default();
    user.username = "registration-identity".to_string();
    user.max_connections = 1;
    create_provider_header_session(&manager, &user, "http://provider.example/index.m3u8").await;
    let identity = session_identity(&manager, &user.username, "tok-host").await.ok_or("session missing")?;
    let requirement = PlaybackSessionRegistration { identity, enforce_limits: true, grace_admitted: false };
    assert!(manager.terminate_session(&user.username, "tok-host").await);
    assert!(register_guarded_resource(&manager, &user, "tok-host", 1, &requirement).await.is_none());
    assert_eq!(manager.user_connections(&user.username).await, 0);
    assert!(manager.connections.read().await.key_by_addr.is_empty(), "rejection must not mutate socket registrations");
    create_provider_header_session(&manager, &user, "http://provider.example/index.m3u8").await;
    assert!(register_guarded_resource(&manager, &user, "tok-host", 2, &requirement).await.is_none());
    let current = PlaybackSessionRegistration {
        identity: session_identity(&manager, &user.username, "tok-host").await.ok_or("replacement missing")?,
        ..requirement
    };
    assert!(register_guarded_resource(&manager, &user, "tok-host", 3, &current).await.is_some());
    assert!(register_guarded_resource(&manager, &user, "tok-host", 4, &current).await.is_some());
    assert_eq!(manager.user_connections(&user.username).await, 1, "parallel resources share one user admission");
    Ok(())
}

#[tokio::test]
async fn session_end_wakes_waiters_without_polling() -> Result<(), Box<dyn std::error::Error>> {
    let manager = Arc::new(provider_header_test_manager());
    let mut user = ProxyUserCredentials::default();
    user.username = "session-end-waiter".to_string();
    create_provider_header_session(&manager, &user, "http://provider.example/index.m3u8").await;
    let identity = session_identity(&manager, &user.username, "tok-host").await.ok_or("session missing")?;
    let waiter = {
        let manager = Arc::clone(&manager);
        let username = user.username.clone();
        tokio::spawn(async move { manager.wait_for_playback_session_end(&username, "tok-host", identity).await })
    };
    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "a current session must keep the waiter pending");
    assert!(manager.terminate_session(&user.username, "tok-host").await);
    tokio::time::timeout(Duration::from_secs(1), waiter).await??;
    Ok(())
}

#[tokio::test]
async fn guarded_resource_registration_rechecks_user_capacity_at_commit() -> Result<(), Box<dyn std::error::Error>> {
    let manager = provider_header_test_manager();
    let mut user = ProxyUserCredentials::default();
    user.username = "registration-capacity".to_string();
    user.max_connections = 1;
    create_provider_header_session(&manager, &user, "http://provider.example/index.m3u8").await;
    let first = PlaybackSessionRegistration {
        identity: session_identity(&manager, &user.username, "tok-host").await.ok_or("session missing")?,
        enforce_limits: true,
        grace_admitted: false,
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], 55421));
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "second-session",
            virtual_id: 7011,
            provider: "provider-a",
            stream_url: "http://provider.example/other.m3u8",
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let second = PlaybackSessionRegistration {
        identity: session_identity(&manager, &user.username, "second-session").await.ok_or("second session missing")?,
        enforce_limits: true,
        grace_admitted: false,
    };
    assert!(register_guarded_resource(&manager, &user, "tok-host", 5, &first).await.is_some());
    assert!(register_guarded_resource(&manager, &user, "second-session", 6, &second).await.is_none());
    assert_eq!(manager.user_connections(&user.username).await, 1);
    assert!(
        manager.playback_session_is_current(&user.username, "second-session", second.identity).await,
        "capacity rejection does not terminate an otherwise valid session"
    );
    Ok(())
}
