use super::{
    assert_no_real_connection_slots, assert_preserved_session_is_uncounted, commit_and_preserve_adaptive_session,
    divergence_key, test_channel, test_user_credentials, ActiveUserConnectionParams, ActiveUserManager,
    CreateUserSessionParams, DivergenceKind, PendingProviderReason, PendingProviderState, PlaybackLifecycle,
    UserConnectionData, UserSession,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::{EventMessage, PlaylistItemType, StreamChannel, StreamInfo, UserConnectionPermission, XtreamCluster},
    utils::{current_time_secs, Internable},
};
use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

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
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(manager.active_users_and_connections().await, (0, 0));

    let connections = manager.connections.read().await;
    let connection_data = connections.by_key.get(&user.username).expect("user connection data");
    assert_preserved_session_is_uncounted(connection_data, session_token, stream_uid);
    assert_no_real_connection_slots(connection_data);
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
            provider_session_headers_host: None,
            media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: None,
            lifecycle: PlaybackLifecycle::Active,
            ..Default::default()
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
            provider_session_headers_host: None,
            media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            ..Default::default()
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
            provider_session_headers_host: None,
            media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            user_agent_stream_index: None,
            addr,
            socket_bound: false,
            active_addrs: vec![addr],
            ts: current_time_secs(),
            started_at: current_time_secs(),
            permission: UserConnectionPermission::Allowed,
            connection_kind: None,
            lifecycle: PlaybackLifecycle::Prepared,
            ..Default::default()
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
