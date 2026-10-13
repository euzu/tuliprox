use super::{test_adaptive_channel, ActiveUserConnectionParams, ActiveUserManager, CreateUserSessionParams};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::utils::Internable;
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

#[tokio::test]
async fn affine_request_claims_keep_playback_until_oldest_body_finishes() {
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
                stream_channel: &test_adaptive_channel(3004),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-live-shared-addr"),
            })
            .await;
    }

    assert_eq!(manager.active_streams().await.len(), 1);
    assert!(manager.release_stream_by_uid(&addr, 42).await.is_none());

    let streams = manager.active_streams().await;
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].uid, 41);

    let removed = manager.release_stream_by_uid(&addr, 41).await;
    assert!(removed.as_ref().is_some_and(|stream| stream.uid == 41));
    assert!(manager.active_streams().await.is_empty());
}

#[tokio::test]
async fn affine_request_claims_keep_playback_until_newest_body_finishes() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let addr: SocketAddr = "127.0.0.1:55037".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-affine-cleanup-order".to_string(), "127.0.0.1".to_string(), addr);

    manager.add_connection(&addr).await;
    for uid in [45, 46] {
        manager
            .update_connection(ActiveUserConnectionParams {
                uid,
                meter_uid: 0,
                username: "user1",
                max_connections: 1,
                soft_connections: 0,
                connection_kind: ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-a".intern(),
                stream_channel: &test_adaptive_channel(3005),
                user_agent: Cow::Borrowed("ua"),
                session_token: Some("tok-affine-cleanup-order"),
            })
            .await
            .expect("affine request should register");
    }

    assert!(manager.release_stream_by_uid(&addr, 45).await.is_none());
    assert_eq!(manager.active_users_and_connections().await, (1, 1));
    assert_eq!(manager.active_streams().await.len(), 1);

    let removed = manager.release_stream_by_uid(&addr, 46).await;
    assert!(removed.as_ref().is_some_and(|stream| stream.uid == 45));
    assert_eq!(manager.active_users_and_connections().await, (0, 0));
    assert!(manager.active_streams().await.is_empty());
}

fn shared_hls_channel(virtual_id: u32, shared_stream_id: u64) -> shared::model::StreamChannel {
    let mut channel = test_adaptive_channel(virtual_id);
    channel.shared = true;
    channel.shared_stream_id = Some(shared_stream_id);
    channel
}

async fn create_device_session(manager: &ActiveUserManager, session_token: &str, port: u16) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut user = ProxyUserCredentials::default();
    user.username = String::from("user1");
    user.max_connections = 2;
    manager.add_connection(&addr).await;
    manager
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token,
            virtual_id: 3010,
            provider: "provider-a",
            stream_url: "http://localhost/live.m3u8",
            addr: &addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
}

async fn register_shared_segment(
    manager: &ActiveUserManager,
    uid: u32,
    port: u16,
    channel: &shared::model::StreamChannel,
    session_token: &str,
) -> SocketAddr {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let fingerprint = Fingerprint::new(format!("fp-{port}"), "127.0.0.1".to_string(), addr);
    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid,
            meter_uid: uid,
            username: "user1",
            max_connections: 2,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some(session_token),
        })
        .await
        .expect("shared segment request should register");
    addr
}

#[tokio::test]
async fn shared_hls_segment_requests_of_one_playback_share_one_panel_row() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);
    let channel = shared_hls_channel(3010, 77);
    create_device_session(&manager, "tok-shared-device", 55050).await;

    // Each segment arrives on its own short socket and finishes before the next one.
    for (uid, port) in [(51, 55051), (52, 55052), (53, 55053)] {
        let addr = register_shared_segment(&manager, uid, port, &channel, "tok-shared-device").await;
        manager.release_stream_by_uid(&addr, uid).await;
    }
    // Overlapping segment bodies of the same playback.
    let first = register_shared_segment(&manager, 54, 55054, &channel, "tok-shared-device").await;
    let second = register_shared_segment(&manager, 55, 55055, &channel, "tok-shared-device").await;

    assert_eq!(manager.panel_streams().await.len(), 1);
    assert_eq!(manager.active_users_and_connections().await, (1, 1));
    manager.release_stream_by_uid(&first, 54).await;
    assert_eq!(manager.active_streams().await.len(), 1);
    manager.release_stream_by_uid(&second, 55).await;
    assert_eq!(manager.panel_streams().await.len(), 1);
}

#[tokio::test]
async fn shared_hls_rows_stay_separate_per_device_and_per_shared_stream() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    create_device_session(&manager, "tok-device-a", 55060).await;
    create_device_session(&manager, "tok-device-b", 55064).await;
    register_shared_segment(&manager, 61, 55061, &shared_hls_channel(3011, 81), "tok-device-a").await;
    register_shared_segment(&manager, 62, 55062, &shared_hls_channel(3011, 81), "tok-device-b").await;
    register_shared_segment(&manager, 63, 55063, &shared_hls_channel(3012, 82), "tok-device-a").await;

    assert_eq!(manager.panel_streams().await.len(), 3);
}
