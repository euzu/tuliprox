use super::{
    ActiveUserConnectionParams, ActiveUserManager, CreateUserSessionParams, PlaybackLifecycle, SessionIdentity,
    UserConnectionCounts, UserConnectionData, UserSession,
};
use crate::{active_provider_manager::ConnectionKind, EventManager};
use arc_swap::ArcSwapOption;
use shared::{
    model::{PlaylistItemType, ProxyType, StreamChannel, UserConnectionPermission, XtreamCluster},
    utils::Internable,
};
use std::{borrow::Cow, net::SocketAddr, sync::Arc};
use tuliprox_core::model::{Config, Fingerprint, ProxyUserCredentials};
use tuliprox_repository::GeoIp;

pub(in crate::active_user_manager::tests) fn test_channel(virtual_id: u32) -> StreamChannel {
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

pub(in crate::active_user_manager::tests) fn test_adaptive_channel(virtual_id: u32) -> StreamChannel {
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

pub(in crate::active_user_manager::tests) fn test_series_channel(virtual_id: u32) -> StreamChannel {
    StreamChannel {
        item_type: PlaylistItemType::Series,
        cluster: XtreamCluster::Series,
        url: "http://localhost/series/episode.mkv".intern(),
        ..test_channel(virtual_id)
    }
}

pub(in crate::active_user_manager::tests) fn test_user_credentials(
    username: &str,
    max_connections: u32,
    soft_connections: u16,
) -> ProxyUserCredentials {
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

pub(in crate::active_user_manager::tests) fn record_owned_slot(
    counts: &mut UserConnectionCounts,
    kind: ConnectionKind,
) {
    match kind {
        ConnectionKind::Normal => counts.normal += 1,
        ConnectionKind::Soft => counts.soft += 1,
    }
}

pub(in crate::active_user_manager::tests) fn assert_connection_ownership_invariants(
    connection_data: &UserConnectionData,
) {
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

pub(in crate::active_user_manager::tests) fn assert_no_real_connection_slots(connection_data: &UserConnectionData) {
    assert_eq!(connection_data.connections, 0);
    assert_eq!(connection_data.counts.normal, 0);
    assert_eq!(connection_data.counts.soft, 0);
    assert_connection_ownership_invariants(connection_data);
}

pub(in crate::active_user_manager::tests) fn assert_preserved_session_is_uncounted(
    connection_data: &UserConnectionData,
    session_token: &str,
    stream_uid: u32,
) {
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

pub(in crate::active_user_manager::tests) async fn commit_and_preserve_adaptive_session(
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

pub(in crate::active_user_manager::tests) fn provider_header_test_manager() -> ActiveUserManager {
    let config = Config::default();
    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let event_manager = Arc::new(EventManager::new());
    ActiveUserManager::new(&config, &geoip, &event_manager)
}

pub(in crate::active_user_manager::tests) async fn create_provider_header_session(
    manager: &ActiveUserManager,
    user: &ProxyUserCredentials,
    url: &str,
) {
    let addr: SocketAddr = "127.0.0.1:55420".parse().unwrap_or_else(|_| unreachable!());
    manager
        .create_user_session(CreateUserSessionParams {
            user,
            session_token: "tok-host",
            virtual_id: 7010,
            provider: "provider-a",
            stream_url: url,
            addr: &addr,
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
}

pub(in crate::active_user_manager::tests) async fn session_identity(
    manager: &ActiveUserManager,
    username: &str,
    token: &str,
) -> Option<SessionIdentity> {
    let users = manager.connections.read().await;
    users.by_key.get(username)?.sessions.iter().find(|session| session.token == token).map(UserSession::identity)
}
