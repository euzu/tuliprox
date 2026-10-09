use super::{
    test_adaptive_channel, test_channel, test_user_credentials, ActiveUserConnectionParams, ActiveUserManager,
    CreateUserSessionParams, PendingProviderReason, PendingProviderWakeSource, PlaybackLifecycle,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::UserConnectionPermission,
    utils::{current_time_secs, Internable},
};
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

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

#[tokio::test]
async fn eviction_candidates_include_charged_stream_with_uncounted_session() {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    let manager = ActiveUserManager::new(&config, &geoip, &event_manager);

    let addr: SocketAddr = "127.0.0.1:55034".parse().unwrap();
    let fingerprint = Fingerprint::new("fp-switch".to_string(), "10.41.41.170".to_string(), addr);
    let username = "switch-user";
    manager.add_connection(&addr).await;
    manager
        .update_connection(ActiveUserConnectionParams {
            uid: 34,
            meter_uid: 0,
            username,
            max_connections: 1,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider-a".intern(),
            stream_channel: &test_channel(1034),
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("missing-session"),
        })
        .await;

    assert_eq!(manager.user_connections(username).await, 1);
    assert_eq!(manager.connection_permission(username, 1, 0).await, UserConnectionPermission::Exhausted);

    let candidates = manager.get_eviction_candidates(username, &fingerprint.client_ip).await;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].addr, addr);
    assert_eq!(candidates[0].client_ip, fingerprint.client_ip);

    manager.release_connection_as_kicked(&addr).await;
    assert_eq!(manager.connection_permission(username, 1, 0).await, UserConnectionPermission::Allowed);
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
    assert_eq!(normal_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(normal_admission.kind(), Some(ConnectionKind::Normal));
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
    assert_eq!(soft_admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(soft_admission.kind(), Some(ConnectionKind::Soft));
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
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);

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
        admission.permission(),
        UserConnectionPermission::Exhausted,
        "uncounted session should not bypass admission when user is at limit; \
             bug: session exists -> Allowed -> strategy evaluation skipped"
    );
}
