use super::{test_adaptive_channel, ActiveUserConnectionParams, ActiveUserManager};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::utils::Internable;
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tuliprox_core::model::{Config, Fingerprint};
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
