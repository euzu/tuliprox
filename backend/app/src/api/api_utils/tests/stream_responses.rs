use super::{
    create_test_app_state_for_config, create_test_fingerprint, create_test_local_channel,
    create_test_provider_app_config, force_provider_stream_response, ForceStreamRequestContext,
};
use crate::{
    api::model::UserSession,
    model::{ConfigInput, ProxyUserCredentials, SourcesConfig},
};
use axum::{http::HeaderMap, response::IntoResponse};
use shared::{
    model::{InputFetchMethod, InputType, PlaylistItemType, UserConnectionPermission, XtreamCluster},
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};

pub(in crate::api::api_utils::tests) async fn forced_legacy_hls_test_response(
    origin_addr: SocketAddr,
    request_headers: &HeaderMap,
    client_port: u16,
) -> axum::response::Response {
    let origin_url = format!("http://{origin_addr}/segment.ts");
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::from([("Accept-Encoding".to_string(), "gzip".to_string())]),
        url: format!("http://{origin_addr}"),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        ..ConfigInput::default()
    });
    let app_config = create_test_provider_app_config();
    app_config.sources.store(Arc::new(SourcesConfig { inputs: vec![Arc::clone(&input)], ..SourcesConfig::default() }));
    let app_state = create_test_app_state_for_config(Arc::new(app_config));
    let client_addr = SocketAddr::from(([127, 0, 0, 1], client_port));
    let fingerprint = create_test_fingerprint(client_addr);
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();
    let session = UserSession {
        token: format!("legacy-hls-marker-{client_port}"),
        transition_version: 1,
        virtual_id: 41,
        provider: Arc::clone(&input.name),
        stream_url: origin_url.as_str().intern(),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        user_agent_stream_index: None,
        addr: client_addr,
        socket_bound: false,
        active_addrs: vec![client_addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    };
    let mut stream_channel = create_test_local_channel(&origin_url);
    stream_channel.provider_id = u32::from(input.id);
    stream_channel.input_name = Arc::clone(&input.name);
    stream_channel.item_type = PlaylistItemType::Catchup;
    stream_channel.cluster = XtreamCluster::Live;
    stream_channel.url = origin_url.as_str().intern();

    force_provider_stream_response(
        &fingerprint,
        &app_state,
        &session,
        stream_channel,
        ForceStreamRequestContext {
            req_headers: request_headers,
            input: &input,
            user: &user,
            session_reservation_ttl_secs: 0,
            content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
        },
        None,
    )
    .await
    .into_response()
}
