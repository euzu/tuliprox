use super::{test_addr, test_fingerprint};
use crate::{
    api::model::{
        AppState, ConnectionKind, CreateUserSessionParams, HlsOriginIoContext, HlsSessionHandle, PlaybackLifecycle,
        ProxySessionId, UserSession,
    },
    model::ProxyUserCredentials,
};
use shared::model::UserConnectionPermission;
use std::{collections::HashMap, sync::Arc};

pub(in crate::api::endpoints::hls_api::tests) fn test_hls_origin_io_context(
    app_state: &Arc<AppState>,
) -> HlsOriginIoContext {
    HlsOriginIoContext {
        ctx: app_state.hls_ctx(),
        client_addr: test_fingerprint().addr,
        allow_grace: false,
        priority: 0,
        connection_kind: ConnectionKind::Normal,
        reservation_ttl_secs: 60,
        preacquired_provider_handle: None,
        started_generation: None,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn register_test_hls_stream_for_lease_release(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    provider: &str,
) {
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.max_connections = 1;
    app_state
        .active_users
        .create_user_session(CreateUserSessionParams {
            user: &user,
            session_token: "hls-session-token",
            virtual_id: 12345,
            provider,
            stream_url: "http://origin.example.com/live/user/pass/12345.m3u8",
            addr: &test_addr(),
            connection_permission: UserConnectionPermission::Allowed,
            connection_kind: Some(ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let mut stream_channel = super::super::fallback_hls_cache_stream_channel(
        0,
        12345,
        &session.read().await.origin_source,
        proxy_session_id,
    );
    stream_channel.shared = true;
    stream_channel.shared_stream_id = Some(super::super::hls_cache_shared_stream_id(proxy_session_id));
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &test_fingerprint(),
            provider: Arc::from(provider),
            stream_channel: &stream_channel,
            user_agent: std::borrow::Cow::Borrowed("test"),
            session_token: Some("hls-session-token"),
        })
        .await;
}

pub(in crate::api::endpoints::hls_api::tests) fn stats_provider_test_user_session(provider: &str) -> UserSession {
    UserSession {
        token: "stats-session-token".to_string(),
        transition_version: 0,
        virtual_id: 12345,
        provider: Arc::from(provider),
        stream_url: Arc::from("http://origin.example.com/live/12345.m3u8"),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        user_agent_stream_index: None,
        addr: test_addr(),
        socket_bound: false,
        active_addrs: Vec::new(),
        ts: 100,
        started_at: 100,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(ConnectionKind::Normal),
        lifecycle: PlaybackLifecycle::Active,
        ..Default::default()
    }
}
