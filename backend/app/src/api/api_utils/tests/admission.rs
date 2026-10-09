use crate::{
    api::model::{AppState, UserSession},
    auth::Fingerprint,
};
use shared::model::{PlaylistItemType, UserConnectionPermission};
use std::{collections::HashMap, sync::Arc};

pub(in crate::api::api_utils::tests) fn create_test_fingerprint_with_user_agent(
    addr: std::net::SocketAddr,
    user_agent: &str,
) -> Fingerprint {
    Fingerprint::new(format!("{}|{user_agent}", addr.ip()), addr.ip().to_string(), addr)
}

pub(in crate::api::api_utils::tests) fn create_test_session(
    token: &str,
    item_type: PlaylistItemType,
    lifecycle: crate::api::model::PlaybackLifecycle,
) -> UserSession {
    UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::<str>::from("provider-a"),
        stream_url: Arc::<str>::from(match item_type {
            PlaylistItemType::LiveHls => "http://provider-1.example/live/42.m3u8",
            _ => "http://provider-1.example/live/42.ts",
        }),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        user_agent_stream_index: None,
        addr: "127.0.0.1:55555".parse().unwrap_or_else(|_| unreachable!()),
        socket_bound: item_type.uses_socket_bound_session(),
        active_addrs: Vec::new(),
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle,
        ..Default::default()
    }
}

pub(in crate::api::api_utils::tests) fn connection_denied_count(app_state: &Arc<AppState>) -> u64 {
    app_state
        .active_users
        .events()
        .stats()
        .emitted()
        .into_iter()
        .find_map(|(kind, count)| (kind == shared::model::EventKind::ConnectionDenied).then_some(count))
        .unwrap_or(0)
}
