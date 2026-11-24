use super::{
    create_api_proxy_user, create_test_app_state, create_test_fingerprint, create_test_live_channel,
    create_test_provider_app_config, create_test_provider_app_state, create_test_shared_target,
    evaluate_network_access, is_hop_by_hop_response_header, is_media_server_playback_url,
    is_media_server_stream_ref_url, is_seek_request, is_seekable_media_request, mark_response_as_uncompressed,
    resolve_request_url_for_logging, resolve_stream_config_u64, resource_proxy_response, spawn_legacy_hls_test_origin,
    stream_response, stream_responses::forced_legacy_hls_test_response, user_with_network_access,
    NetworkAccessDecision, PlaybackSource,
};
use crate::{
    api::{
        api_utils::streaming::session_headers_belong_to_allocation,
        model::{SharedStreamCtx, SharedStreamManager},
    },
    model::{ConfigInput, NetworkAccess, ProxyUserCredentials},
    repository::GeoIp,
};
use arc_swap::ArcSwapOption;
use axum::{
    http::{header, HeaderMap, HeaderName, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use bytes::Bytes;
use futures::stream;
use http_body_util::BodyExt;
use shared::{
    defaults::{default_catchup_session_ttl_secs, default_hls_session_ttl_secs, HLS_EXT},
    model::{
        ClusterFlags, GeoIpUnavailablePolicy, InputType, PlaylistItemType, StreamChannel, UserConnectionPermission,
        VirtualId, XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::{borrow::Cow, collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tower::ServiceExt;
use tuliprox_core::utils::response_compression::should_compress_response;

#[test]
fn test_is_seek_request() {
    let mut headers = HeaderMap::new();

    // No range header
    assert!(!is_seek_request(XtreamCluster::Video, &headers));

    // Range: bytes=0- (Should be true now to allow session takeover on restart)
    headers.insert("range", "bytes=0-".parse().unwrap());
    assert!(is_seek_request(XtreamCluster::Video, &headers));

    // Range: bytes=100- (Should be true)
    headers.insert("range", "bytes=100-".parse().unwrap());
    assert!(is_seek_request(XtreamCluster::Video, &headers));

    // Range: bytes=100-200 (Should be true)
    headers.insert("range", "bytes=100-200".parse().unwrap());
    assert!(is_seek_request(XtreamCluster::Video, &headers));

    // Live cluster should always return false
    headers.insert("range", "bytes=100-".parse().unwrap());
    assert!(!is_seek_request(XtreamCluster::Live, &headers));
}

#[test]
fn hls_manifests_are_not_forced_as_seek_responses() {
    let mut headers = HeaderMap::new();
    headers.insert("range", HeaderValue::from_static("bytes=0-"));

    assert!(!is_seekable_media_request(XtreamCluster::Video, &headers, Some(HLS_EXT)));
    assert!(is_seekable_media_request(XtreamCluster::Video, &headers, Some(".ts")));
}

#[test]
fn media_server_proxy_response_header_filter_drops_hop_by_hop_headers() {
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "trailers",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(is_hop_by_hop_response_header(&HeaderName::from_static(name)));
    }
    assert!(!is_hop_by_hop_response_header(&header::CONTENT_TYPE));
}

/// Session headers follow the account they were captured for: a fallback away from the forced provider or from the
/// stream URL owner drops them.
#[test]
fn session_headers_are_dropped_when_allocation_leaves_owner_or_forced_provider() {
    let main = "main".intern();
    let alias = "alias".intern();
    assert!(session_headers_belong_to_allocation(Some(&main), None, None));
    assert!(session_headers_belong_to_allocation(None, Some(&main), Some(&main)));
    assert!(session_headers_belong_to_allocation(Some(&main), Some(&main), Some(&main)));
    assert!(!session_headers_belong_to_allocation(Some(&alias), Some(&main), None));
    assert!(!session_headers_belong_to_allocation(Some(&alias), None, Some(&main)));
    assert!(!session_headers_belong_to_allocation(Some(&alias), Some(&alias), Some(&main)));
}

#[test]
fn media_server_playback_urls_are_proxy_only_redirect_guard_candidates() {
    let plex_input = ConfigInput { input_type: InputType::Plex, ..ConfigInput::default() };
    let emby_input = ConfigInput { input_type: InputType::Emby, ..ConfigInput::default() };
    let jellyfin_input = ConfigInput { input_type: InputType::Jellyfin, ..ConfigInput::default() };
    let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };

    assert!(is_media_server_playback_url(
        &plex_input,
        "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
    ));
    assert!(is_media_server_playback_url(
        &m3u_input,
        "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
    ));
    assert!(is_media_server_playback_url(&plex_input, "https://plex.example/stream.mkv"));
    assert!(is_media_server_playback_url(&emby_input, "https://emby.example/stream.mkv"));
    assert!(is_media_server_playback_url(&jellyfin_input, "https://jellyfin.example/stream.mkv"));
    assert!(is_media_server_playback_url(&emby_input, "media-server://emby/server/item?source_id=redacted"));
    assert!(is_media_server_playback_url(&jellyfin_input, "media-server://jellyfin/server/item?source_id=redacted"));
    assert!(is_media_server_stream_ref_url("media-server://emby/server/item?source_id=redacted"));
    assert!(is_media_server_stream_ref_url("media-server://jellyfin/server/item?source_id=redacted"));
    assert!(!is_media_server_playback_url(&m3u_input, "https://provider.example/stream.mkv"));
    assert!(!is_media_server_stream_ref_url("https://provider.example/stream.mkv"));
    assert!(is_media_server_stream_ref_url("media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"));
    assert_eq!(
        resolve_request_url_for_logging(
            &plex_input,
            "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted"
        )
        .as_ref(),
        "media-server://<redacted>"
    );
}

#[test]
fn test_resolve_request_url_for_logging_respects_sanitization_setting() {
    let _sanitize = crate::api::model::lock_sanitize_flag(true);

    let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };
    let media_ref = "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fsecret_key";
    let standard_url = "http://provider.example/live/stream.m3u8";
    let transport_stream_url = "http://provider.example/session/auth-path/stream.ts?opaque=synthetic-opaque";

    // 1. When sanitization is enabled:
    shared::utils::set_sanitize_sensitive_info(true);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, transport_stream_url).as_ref(), transport_stream_url);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, media_ref).as_ref(), "media-server://<redacted>");
    assert_eq!(resolve_request_url_for_logging(&m3u_input, standard_url).as_ref(), standard_url);

    // 2. When sanitization is disabled:
    shared::utils::set_sanitize_sensitive_info(false);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, transport_stream_url).as_ref(), transport_stream_url);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, media_ref).as_ref(), media_ref);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, standard_url).as_ref(), standard_url);
}

#[test]
fn test_streaming_response_extension_disables_compression() {
    let mut response = Response::new(());
    mark_response_as_uncompressed(&mut response);

    assert!(!should_compress_response(&response));
}

#[tokio::test]
async fn forced_hls_provider_response_disables_compression_and_streams_identity_bytes() {
    const IDENTITY_BODY: &[u8] = b"legacy hls identity segment";

    let mut encoder = async_compression::tokio::write::GzipEncoder::new(Vec::new());
    encoder.write_all(IDENTITY_BODY).await.expect("gzip test body encodes");
    encoder.shutdown().await.expect("gzip test encoder finishes");
    let encoded_body = encoder.into_inner();

    let response_head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            encoded_body.len()
        );
    let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, encoded_body).await;
    let mut request_headers = HeaderMap::new();
    request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    let response = forced_legacy_hls_test_response(origin_addr, &request_headers, 55_310).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert!(!should_compress_response(&response));
    assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    let body = response.into_body().collect().await.expect("legacy HLS response body").to_bytes();
    assert_eq!(body.as_ref(), IDENTITY_BODY);

    let request = origin_task.await.expect("test origin task completes").to_ascii_lowercase();
    assert!(request.contains("\r\naccept-encoding: identity\r\n"));
}

#[tokio::test]
async fn forced_hls_unencoded_partial_response_preserves_range_and_disables_compression() {
    const PARTIAL_BODY: &[u8] = b"cdef";
    let response_head = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp2t\r\nContent-Range: bytes 2-5/10\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            PARTIAL_BODY.len()
        );
    let (origin_addr, origin_task) = spawn_legacy_hls_test_origin(response_head, PARTIAL_BODY.to_vec()).await;
    let mut request_headers = HeaderMap::new();
    request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-"));

    let response = forced_legacy_hls_test_response(origin_addr, &request_headers, 55_311).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert!(!should_compress_response(&response));
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
    assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
    let body = response.into_body().collect().await.expect("legacy HLS partial body").to_bytes();
    assert_eq!(body.as_ref(), PARTIAL_BODY);

    let request = origin_task.await.expect("test origin task completes").to_ascii_lowercase();
    assert!(request.contains("\r\naccept-encoding: identity\r\n"));
    assert!(request.contains("\r\nrange: bytes=2-\r\n"));
}

#[test]
fn test_regular_response_keeps_compression_enabled() {
    let response = Response::new(());

    assert!(should_compress_response(&response));
}

#[test]
fn test_get_stream_config_u64_uses_default_when_stream_config_missing() {
    assert_eq!(
        resolve_stream_config_u64(None, |stream| stream.hls_session_ttl_secs, default_hls_session_ttl_secs()),
        default_hls_session_ttl_secs()
    );
    assert_eq!(
        resolve_stream_config_u64(None, |stream| stream.catchup_session_ttl_secs, default_catchup_session_ttl_secs()),
        default_catchup_session_ttl_secs()
    );
}

#[tokio::test]
async fn create_api_proxy_user_defaults_output_clusters_to_all() {
    let app_state = create_test_app_state();
    let user = create_api_proxy_user(&app_state);
    assert_eq!(user.output_clusters, ClusterFlags::all());
}

#[tokio::test]
async fn proxied_m3u_logo_follows_private_redirect_without_forwarding_credentials() {
    let app_state = create_test_app_state();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock proxy binds");
    let proxy_addr = listener.local_addr().expect("mock proxy address");
    let proxy_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: http://10.0.0.2/icon.png\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\npng",
        ] {
            let (mut socket, _) = listener.accept().await.expect("mock proxy accepts request");
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.expect("mock proxy reads request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            socket.write_all(response.as_bytes()).await.expect("mock proxy writes response");
            requests.push(String::from_utf8_lossy(&request).into_owned());
        }
        requests
    });
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_addr}")).expect("mock proxy URL"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("mock resource client");
    app_state.http_clients.resource_no_redirect.store(Arc::new(client));
    let input = ConfigInput {
        url: "http://10.0.0.1/playlist.m3u".to_string(),
        headers: HashMap::from([("Authorization".to_string(), "Bearer secret".to_string())]),
        ..ConfigInput::default()
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer player-secret"));

    let response = resource_proxy_response(&app_state, "http://10.0.0.1/logo.png", &headers, Some(&input)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.into_body().collect().await.expect("image body").to_bytes(), Bytes::from_static(b"png"));

    let requests = proxy_task.await.expect("mock proxy task");
    assert!(requests[0].starts_with("GET http://10.0.0.1/logo.png HTTP/1.1\r\n"));
    assert!(requests[0].to_ascii_lowercase().contains("authorization: bearer secret"));
    assert!(!requests[0].contains("player-secret"));
    assert!(requests[1].starts_with("GET http://10.0.0.2/icon.png HTTP/1.1\r\n"));
    assert!(!requests[1].to_ascii_lowercase().contains("authorization:"));
}

#[tokio::test]
async fn stream_response_preserves_soft_kind_for_shared_reuse() {
    let app_state = create_test_provider_app_state();
    let stream_url = "http://provider-1.example/live/shared.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr = "127.0.0.1:55140".parse().unwrap_or_else(|_| unreachable!());
    let owner_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &owner_addr, 0, crate::api::model::ConnectionKind::Normal)
        .expect("owner allocation should exist");
    let subscriber_id =
        tuliprox_core::model::SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, owner_addr)
            .await
            .expect("shared cleanup admission should succeed");
    let shared_stream = stream::pending::<Result<Bytes, std::io::Error>>();
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &owner_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), owner_handle)),
        pending_cleanup,
        0,
        crate::api::model::ConnectionKind::Normal,
        "test_user",
        "test_token",
    )
    .await;
    assert!(registered.is_some(), "shared stream should register");

    let mut user = ProxyUserCredentials::default();
    user.username = "soft-user".to_string();
    user.max_connections = 1;
    user.soft_connections = 1;
    user.priority = 0;
    user.soft_priority = 9;

    let normal_addr = "127.0.0.1:55141".parse().unwrap_or_else(|_| unreachable!());
    let normal_fingerprint = create_test_fingerprint(normal_addr);
    let normal_channel = create_test_live_channel("http://provider-1.example/live/normal.ts");
    app_state.active_users.add_connection(&normal_addr).await;
    app_state
        .active_users
        .update_connection(crate::api::model::ActiveUserConnectionParams {
            uid: 1001,
            meter_uid: 0,
            username: &user.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint: &normal_fingerprint,
            provider: input.name.clone(),
            stream_channel: &normal_channel,
            user_agent: Cow::Borrowed("ua"),
            session_token: Some("normal-session"),
        })
        .await
        .expect("normal stream should register");

    let admission =
        app_state.active_users.connection_admission(&user.username, user.max_connections, user.soft_connections).await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);
    assert_eq!(admission.kind(), Some(crate::api::model::ConnectionKind::Soft));

    let soft_addr = "127.0.0.1:55142".parse().unwrap_or_else(|_| unreachable!());
    let soft_fingerprint = create_test_fingerprint(soft_addr);
    let response = stream_response(
        &soft_fingerprint,
        &app_state,
        "soft-session",
        None,
        create_test_live_channel(stream_url),
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::default(),
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    let session_admission = app_state
        .active_users
        .connection_admission_for_session(&user.username, user.max_connections, user.soft_connections, "soft-session")
        .await;
    assert_eq!(session_admission.kind(), Some(crate::api::model::ConnectionKind::Soft));
}

#[tokio::test]
async fn requesting_joinable_virtual_id_at_provider_limit_joins_shared_stream() {
    let app_state = create_test_provider_app_state();
    let stream_url = "http://provider-1.example/live/shared.ts";
    let input_name = "provider_1".intern();
    let input = app_state.app_config.get_input_by_name(&input_name).expect("provider input should exist");
    let target = Arc::new(create_test_shared_target());

    let owner_addr = "127.0.0.1:55150".parse().unwrap_or_else(|_| unreachable!());
    let owner_handle = app_state
        .active_provider
        .acquire_connection(&input.name, &owner_addr, 0, crate::api::model::ConnectionKind::Normal)
        .expect("owner allocation should exist");
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1, "provider at limit 1");

    let subscriber_id =
        tuliprox_core::model::SharedSubscriberId::from_stream_uid(app_state.connection_manager.next_stream_uid());
    let pending_cleanup =
        SharedStreamManager::reserve_subscriber_cleanup(&app_state.connection_manager, subscriber_id, owner_addr)
            .await
            .expect("shared cleanup admission should succeed");
    let shared_stream = stream::pending::<Result<Bytes, std::io::Error>>();
    let registered = SharedStreamManager::register_shared_stream(
        SharedStreamCtx {
            app_config: &app_state.app_config,
            shared_stream_manager: &app_state.shared_stream_manager,
            active_provider: &app_state.active_provider,
            connection_manager: &app_state.connection_manager,
        },
        stream_url,
        shared_stream,
        &owner_addr,
        subscriber_id,
        Vec::new(),
        1,
        Some(tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), owner_handle)),
        pending_cleanup,
        0,
        crate::api::model::ConnectionKind::Normal,
        "test_user",
        "test_token",
    )
    .await;
    assert!(registered.is_some(), "shared stream should register");

    // Second user joins the listed channel
    let mut joiner = ProxyUserCredentials::default();
    joiner.username = "joiner".to_string();
    joiner.max_connections = 1;

    let joiner_addr = "127.0.0.1:55151".parse().unwrap_or_else(|_| unreachable!());
    let joiner_fingerprint = create_test_fingerprint(joiner_addr);
    let joiner_channel = create_test_live_channel(stream_url); // virtual_id 42

    let admission = app_state
        .active_users
        .connection_admission(&joiner.username, joiner.max_connections, joiner.soft_connections)
        .await;
    assert_eq!(admission.permission(), UserConnectionPermission::Allowed);

    let response = stream_response(
        &joiner_fingerprint,
        &app_state,
        "joiner-session",
        None,
        joiner_channel,
        PlaybackSource::Single { input: &input, stream_url },
        None,
        &HeaderMap::default(),
        &target,
        &joiner,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::OK);

    // Provider count remains 1 because joiner attached to the shared stream
    assert_eq!(app_state.active_provider.get_provider_connections_count(), 1);
}

#[test]
fn network_denied_reason_none_when_allowed() {
    let user = user_with_network_access(Some(NetworkAccess {
        allowed_countries: vec!["DE".to_string()],
        allowed_networks: vec![],
    }));
    let mock_geoip = Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new("DE")))));
    assert_eq!(
        evaluate_network_access(&user, "8.8.8.8", &mock_geoip, GeoIpUnavailablePolicy::Deny),
        NetworkAccessDecision::Allowed
    );
}

struct PlayerApiStreamTestEnv {
    app_state: Arc<crate::api::model::AppState>,
    input: Arc<ConfigInput>,
    target: Arc<crate::model::ConfigTarget>,
    alice_token: String,
    bob_token: String,
    alice_credentials: ProxyUserCredentials,
    bob_credentials: ProxyUserCredentials,
    carol_credentials: ProxyUserCredentials,
    stream_url: String,
}

fn mock_peer() -> axum::extract::connect_info::MockConnectInfo<std::net::SocketAddr> {
    axum::extract::connect_info::MockConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345)))
}

#[allow(clippy::too_many_lines)]
async fn setup_player_api_stream_test_env() -> PlayerApiStreamTestEnv {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("origin binds");
    let origin_addr = listener.local_addr().expect("origin addr");
    let app = axum::Router::new().fallback(|| async {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
        tokio::spawn(async move {
            for i in 0..20u8 {
                let _ = tx.send(Ok(Bytes::from(vec![i; 188]))).await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        axum::response::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "video/mp2t")
            .body(axum::body::Body::from_stream(body_stream))
            .unwrap()
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let stream_url = format!("http://{origin_addr}/live/user1/pass1/test.ts");

    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: format!("http://{origin_addr}"),
        username: Some("user1".to_string()),
        password: Some("pass1".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 10,
        ..ConfigInput::default()
    });

    let mut target = create_test_shared_target();
    target.use_memory_cache = true;
    target.options.as_mut().unwrap().share_live_streams.mpeg_ts = true;
    target.output = vec![tuliprox_core::model::TargetOutput::Xtream(tuliprox_core::model::XtreamTargetOutput::from(
        &shared::model::XtreamTargetOutputDto::default(),
    ))];
    let target = Arc::new(target);

    let sources = crate::model::SourcesConfig {
        inputs: vec![Arc::clone(&input)],
        sources: vec![crate::model::ConfigSource {
            inputs: vec![Arc::clone(&input.name)],
            targets: vec![Arc::clone(&target)],
        }],
        ..crate::model::SourcesConfig::default()
    };

    let web_auth = shared::model::WebAuthConfigDto {
        enabled: true,
        issuer: "test".to_string(),
        secret: "test-secret".to_string(),
        ..Default::default()
    };
    let web_ui = shared::model::WebUiConfigDto {
        enabled: true,
        user_ui_enabled: true,
        player_api_enabled: Some(true),
        auth: Some(web_auth.clone()),
        ..Default::default()
    };
    let main_cfg = crate::model::Config { web_ui: Some((&web_ui).into()), ..crate::model::Config::default() };

    let mut app_cfg = create_test_provider_app_config();
    app_cfg.config = Arc::new(arc_swap::ArcSwap::from_pointee(main_cfg));
    app_cfg.sources = Arc::new(arc_swap::ArcSwap::from_pointee(sources));

    let mut alice_credentials = ProxyUserCredentials::default();
    alice_credentials.username = "alice".to_string();
    alice_credentials.password = "pass".to_string();
    alice_credentials.ui_enabled = true;
    alice_credentials.max_connections = 5;
    alice_credentials.proxy = shared::model::ProxyType::Reverse(None);

    let mut bob_credentials = ProxyUserCredentials::default();
    bob_credentials.username = "bob".to_string();
    bob_credentials.password = "pass".to_string();
    bob_credentials.ui_enabled = true;
    bob_credentials.max_connections = 5;
    bob_credentials.proxy = shared::model::ProxyType::Reverse(None);

    let mut carol_credentials = ProxyUserCredentials::default();
    carol_credentials.username = "carol".to_string();
    carol_credentials.password = "pass".to_string();
    carol_credentials.ui_enabled = false;
    carol_credentials.max_connections = 5;
    carol_credentials.proxy = shared::model::ProxyType::Reverse(None);
    carol_credentials.player_features.api_enabled = Some(false);

    let api_proxy_dto = shared::model::ApiProxyConfigDto {
        user: vec![shared::model::TargetUserDto {
            target: target.name.clone(),
            credentials: vec![
                shared::model::ProxyUserCredentialsDto {
                    username: "alice".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    max_connections: 5,
                    proxy: shared::model::ProxyType::Reverse(None),
                    ..Default::default()
                },
                shared::model::ProxyUserCredentialsDto {
                    username: "bob".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    max_connections: 5,
                    proxy: shared::model::ProxyType::Reverse(None),
                    ..Default::default()
                },
                shared::model::ProxyUserCredentialsDto {
                    username: "carol".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: false,
                    max_connections: 5,
                    proxy: shared::model::ProxyType::Reverse(None),
                    player_features: Some(shared::model::PlayerFeaturesDto {
                        api_enabled: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
        }],
        ..Default::default()
    };

    let mut api_proxy = crate::model::ApiProxyConfig::from(&api_proxy_dto);
    for tu in &mut api_proxy.user {
        for cred in &mut tu.credentials {
            Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
        }
    }
    app_cfg.set_api_proxy(api_proxy).unwrap();

    let app_state = super::create_test_app_state_for_config(Arc::new(app_cfg));

    let web_auth_core = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
    let alice_token =
        crate::auth::create_jwt_api_user(&web_auth_core, "alice", shared::model::UserId::api("alice")).unwrap();
    let bob_token = crate::auth::create_jwt_api_user(&web_auth_core, "bob", shared::model::UserId::api("bob")).unwrap();

    PlayerApiStreamTestEnv {
        app_state,
        input,
        target,
        alice_token,
        bob_token,
        alice_credentials,
        bob_credentials,
        carol_credentials,
        stream_url,
    }
}

impl PlayerApiStreamTestEnv {
    fn create_channel(&self, virtual_id: u32, item_type: PlaylistItemType) -> StreamChannel {
        let mut channel = create_test_live_channel(&self.stream_url);
        channel.virtual_id = virtual_id;
        channel.provider_id = u32::from(self.input.id);
        channel.input_name = Arc::clone(&self.input.name);
        channel.target_id = self.target.id;
        channel.item_type = item_type;
        channel
    }
}

#[tokio::test]
async fn test_live_ts_player_api_user_carries_matching_stream_uid_header() {
    let env = setup_player_api_stream_test_env().await;
    let addr = "127.0.0.1:55160".parse().unwrap();
    let fp = create_test_fingerprint(addr);
    let channel = env.create_channel(42, PlaylistItemType::Live);

    let response = stream_response(
        &fp,
        &env.app_state,
        "session-alice-1",
        None,
        channel,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &HeaderMap::default(),
        &env.target,
        &env.alice_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();

    eprintln!("RESPONSE HEADERS: {:?}", response.headers());
    assert_eq!(response.status(), StatusCode::OK);
    let header_val = response
        .headers()
        .get("x-tuliprox-stream-uid")
        .expect("X-Tuliprox-Stream-Uid header must be present on live TS response");
    let header_uid: u32 = header_val.to_str().unwrap().parse().expect("uid must be decimal integer");

    let router = crate::api::endpoints::me_player_api::me_player_api_register("")
        .layer(mock_peer())
        .with_state(Arc::clone(&env.app_state));
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/me/streams")
        .header("Authorization", format!("Bearer {}", env.alice_token))
        .body(axum::body::Body::empty())
        .unwrap();
    let res = router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
    let streams: Vec<shared::model::UserStreamDto> = serde_json::from_slice(&body).unwrap();

    assert_eq!(streams.len(), 1);
    assert_eq!(header_uid, streams[0].uid);
}

#[tokio::test]
async fn test_two_concurrent_live_connections_same_user_different_user_agents_carry_own_uids() {
    let env = setup_player_api_stream_test_env().await;
    let addr1 = "127.0.0.1:55161".parse().unwrap();
    let fp1 = create_test_fingerprint(addr1);
    let mut headers1 = HeaderMap::new();
    headers1.insert(axum::http::header::USER_AGENT, "TV-LivingRoom".parse().unwrap());
    let channel1 = env.create_channel(42, PlaylistItemType::Live);

    let response1 = stream_response(
        &fp1,
        &env.app_state,
        "session-alice-tv1",
        None,
        channel1,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &headers1,
        &env.target,
        &env.alice_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(response1.status(), StatusCode::OK);

    let addr2 = "127.0.0.1:55162".parse().unwrap();
    let fp2 = create_test_fingerprint(addr2);
    let mut headers2 = HeaderMap::new();
    headers2.insert(axum::http::header::USER_AGENT, "TV-Bedroom".parse().unwrap());
    let channel2 = env.create_channel(43, PlaylistItemType::Live);

    let response2 = stream_response(
        &fp2,
        &env.app_state,
        "session-alice-tv2",
        None,
        channel2,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &headers2,
        &env.target,
        &env.alice_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(response2.status(), StatusCode::OK);

    let uid1: u32 = response1
        .headers()
        .get("x-tuliprox-stream-uid")
        .expect("response1 must have X-Tuliprox-Stream-Uid")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();

    let uid2: u32 = response2
        .headers()
        .get("x-tuliprox-stream-uid")
        .expect("response2 must have X-Tuliprox-Stream-Uid")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();

    assert_ne!(uid1, uid2, "concurrent connections must have different UIDs");

    let router = crate::api::endpoints::me_player_api::me_player_api_register("")
        .layer(mock_peer())
        .with_state(Arc::clone(&env.app_state));
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/me/streams")
        .header("Authorization", format!("Bearer {}", env.alice_token))
        .body(axum::body::Body::empty())
        .unwrap();
    let res = router.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
    let streams: Vec<shared::model::UserStreamDto> = serde_json::from_slice(&body).unwrap();

    assert_eq!(streams.len(), 2);
    let s1 = streams.iter().find(|s| s.user_agent == "TV-LivingRoom").expect("TV-LivingRoom stream found");
    assert_eq!(s1.uid, uid1);
    let s2 = streams.iter().find(|s| s.user_agent == "TV-Bedroom").expect("TV-Bedroom stream found");
    assert_eq!(s2.uid, uid2);
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn test_shared_joined_live_stream_header_equals_own_me_streams_uid() {
    let env = setup_player_api_stream_test_env().await;

    // Bob opens the shared stream first (owner)
    let addr_bob = "127.0.0.1:55163".parse().unwrap();
    let fp_bob = create_test_fingerprint(addr_bob);
    let channel_bob = env.create_channel(42, PlaylistItemType::Live);

    let bob_stream_resp = stream_response(
        &fp_bob,
        &env.app_state,
        "session-bob-owner",
        None,
        channel_bob,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &HeaderMap::default(),
        &env.target,
        &env.bob_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(bob_stream_resp.status(), StatusCode::OK);

    let header_val_bob = bob_stream_resp
        .headers()
        .get("x-tuliprox-stream-uid")
        .expect("producer connection must have X-Tuliprox-Stream-Uid");
    let header_uid_bob: u32 = header_val_bob.to_str().unwrap().parse().expect("uid must be decimal integer");

    // Alice (Player API user) joins the shared stream
    let addr_alice = "127.0.0.1:55164".parse().unwrap();
    let fp_alice = create_test_fingerprint(addr_alice);
    let channel_alice = env.create_channel(42, PlaylistItemType::Live);

    let alice_stream_resp = stream_response(
        &fp_alice,
        &env.app_state,
        "session-alice-joiner",
        None,
        channel_alice,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &HeaderMap::default(),
        &env.target,
        &env.alice_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(alice_stream_resp.status(), StatusCode::OK);

    let header_val_alice = alice_stream_resp
        .headers()
        .get("x-tuliprox-stream-uid")
        .expect("joining shared connection must have X-Tuliprox-Stream-Uid");
    let header_uid_alice: u32 = header_val_alice.to_str().unwrap().parse().expect("uid must be decimal integer");

    // Producer and joiner responses carry DIFFERENT uids
    assert_ne!(header_uid_bob, header_uid_alice, "producer and joiner must carry different stream uids");

    // Assert the join really happened (exactly one provider connection, joiner counted as shared join)
    assert_eq!(
        env.app_state.active_provider.get_provider_connections_count(),
        1,
        "joiner must not open its own provider connection"
    );
    assert_eq!(
        env.app_state.active_users.user_shared_joins(&env.alice_credentials.username).await,
        1,
        "joiner must count as shared join in active user manager"
    );
    assert_eq!(
        env.app_state.active_users.user_shared_joins(&env.bob_credentials.username).await,
        0,
        "producer must not count as shared join in active user manager"
    );

    let router = crate::api::endpoints::me_player_api::me_player_api_register("")
        .layer(mock_peer())
        .with_state(Arc::clone(&env.app_state));

    // Alice /me/streams
    let alice_me_req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/me/streams")
        .header("Authorization", format!("Bearer {}", env.alice_token))
        .body(axum::body::Body::empty())
        .unwrap();
    let alice_me_resp = router.clone().oneshot(alice_me_req).await.unwrap();
    assert_eq!(alice_me_resp.status(), StatusCode::OK);
    let body_alice = axum::body::to_bytes(alice_me_resp.into_body(), 4096).await.unwrap();
    let streams_alice: Vec<shared::model::UserStreamDto> = serde_json::from_slice(&body_alice).unwrap();

    assert_eq!(streams_alice.len(), 1);
    assert_eq!(streams_alice[0].uid, header_uid_alice);
    assert!(streams_alice[0].shared);

    // Bob /me/streams
    let bob_me_req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/me/streams")
        .header("Authorization", format!("Bearer {}", env.bob_token))
        .body(axum::body::Body::empty())
        .unwrap();
    let bob_me_resp = router.oneshot(bob_me_req).await.unwrap();
    assert_eq!(bob_me_resp.status(), StatusCode::OK);
    let body_bob = axum::body::to_bytes(bob_me_resp.into_body(), 4096).await.unwrap();
    let streams_bob: Vec<shared::model::UserStreamDto> = serde_json::from_slice(&body_bob).unwrap();

    assert_eq!(streams_bob.len(), 1);
    assert_eq!(streams_bob[0].uid, header_uid_bob);
    assert!(streams_bob[0].shared);
}

#[tokio::test]
async fn test_stream_uid_header_absent_for_user_without_player_api_and_for_vod() {
    let env = setup_player_api_stream_test_env().await;

    // 1. Carol has Player API disabled -> header absent on live TS
    let addr_carol = "127.0.0.1:55165".parse().unwrap();
    let fp_carol = create_test_fingerprint(addr_carol);
    let channel_carol = env.create_channel(42, PlaylistItemType::Live);

    let resp_carol = stream_response(
        &fp_carol,
        &env.app_state,
        "session-carol",
        None,
        channel_carol,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &HeaderMap::default(),
        &env.target,
        &env.carol_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(resp_carol.status(), StatusCode::OK);
    assert!(
        !resp_carol.headers().contains_key("x-tuliprox-stream-uid"),
        "user without player api must not get X-Tuliprox-Stream-Uid header"
    );

    // 2. Alice has Player API enabled, but requests VOD -> header absent
    let addr_alice = "127.0.0.1:55166".parse().unwrap();
    let fp_alice = create_test_fingerprint(addr_alice);
    let channel_vod = env.create_channel(42, PlaylistItemType::Video);

    let resp_vod = stream_response(
        &fp_alice,
        &env.app_state,
        "session-alice-vod",
        None,
        channel_vod,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &HeaderMap::default(),
        &env.target,
        &env.alice_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(resp_vod.status(), StatusCode::OK);
    assert!(
        !resp_vod.headers().contains_key("x-tuliprox-stream-uid"),
        "VOD response must not get X-Tuliprox-Stream-Uid header"
    );
}

#[tokio::test]
async fn test_stream_uid_header_absent_when_web_auth_disabled() {
    let env = setup_player_api_stream_test_env().await;

    // Disable web auth in config
    {
        let mut config = (*env.app_state.app_config.config.load_full()).clone();
        if let Some(web_ui) = config.web_ui.as_mut() {
            if let Some(auth) = web_ui.auth.as_mut() {
                auth.enabled = false;
            }
        }
        env.app_state.app_config.config.store(Arc::new(config));
    }

    let addr_alice = "127.0.0.1:55167".parse().unwrap();
    let fp_alice = create_test_fingerprint(addr_alice);
    let channel_alice = env.create_channel(42, PlaylistItemType::Live);

    let resp_alice = stream_response(
        &fp_alice,
        &env.app_state,
        "session-alice-no-web-auth",
        None,
        channel_alice,
        PlaybackSource::Single { input: &env.input, stream_url: &env.stream_url },
        None,
        &HeaderMap::default(),
        &env.target,
        &env.alice_credentials,
        UserConnectionPermission::Allowed,
        crate::api::model::ConnectionKind::Normal,
        false,
        None,
        false,
        false,
    )
    .await
    .into_response();
    assert_eq!(resp_alice.status(), StatusCode::OK);
    assert!(
        !resp_alice.headers().contains_key("x-tuliprox-stream-uid"),
        "live TS response must not have stream uid when web auth is disabled"
    );
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn test_xtream_live_route_attaches_stream_uid_and_vod_omits_header() {
    let env = setup_player_api_stream_test_env().await;

    let live_item = XtreamPlaylistItem {
        virtual_id: VirtualId::new(42),
        provider_id: u32::from(env.input.id),
        name: "Test Live".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "".intern(),
        title: "Test Live".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: env.stream_url.clone().intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Live,
        additional_properties: None,
        item_type: PlaylistItemType::Live,
        category_id: 1,
        input_name: Arc::clone(&env.input.name),
        channel_no: 1,
        source_ordinal: 0,
        input_stream_id: "42".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    };

    let vod_item = XtreamPlaylistItem {
        virtual_id: VirtualId::new(43),
        provider_id: u32::from(env.input.id),
        name: "Test Movie".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "".intern(),
        title: "Test Movie".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: env.stream_url.clone().intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Video,
        additional_properties: Some(shared::model::StreamProperties::Video(Box::new(
            shared::model::VideoStreamProperties {
                name: "Test Movie".intern(),
                plugin_fingerprint: None,
                imdb: None,
                imdb_rating: None,
                imdb_votes: None,
                effective_rating: None,
                category_id: 2,
                stream_id: 43,
                stream_icon: "".intern(),
                direct_source: "".intern(),
                custom_sid: None,
                added: "".intern(),
                container_extension: "mp4".intern(),
                rating: None,
                rating_5based: None,
                stream_type: Some("movie".intern()),
                trailer: None,
                tmdb: None,
                is_adult: 0,
                details: None,
            },
        ))),
        item_type: PlaylistItemType::Video,
        category_id: 2,
        input_name: Arc::clone(&env.input.name),
        channel_no: 2,
        source_ordinal: 0,
        input_stream_id: "43".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    };

    let mut live_tree = crate::repository::BPlusTree::new();
    live_tree.insert(live_item.virtual_id.get(), live_item.clone());
    let mut vod_tree = crate::repository::BPlusTree::new();
    vod_tree.insert(vod_item.virtual_id.get(), vod_item.clone());

    env.app_state
        .playlists
        .cache_playlist(
            &env.target.name,
            crate::api::model::PlaylistStorage::XtreamPlaylist(Box::new(
                crate::api::model::PlaylistXtreamStorage::from_trees(
                    live_tree,
                    vod_tree,
                    crate::repository::BPlusTree::new(),
                ),
            )),
        )
        .await;

    let mut id_mapping = crate::repository::BPlusTree::new();
    id_mapping.insert(
        live_item.virtual_id,
        crate::repository::VirtualIdRecord::new(
            live_item.provider_id,
            live_item.virtual_id,
            PlaylistItemType::Live,
            VirtualId::new(0),
            shared::model::UUIDType::default(),
        ),
    );
    id_mapping.insert(
        vod_item.virtual_id,
        crate::repository::VirtualIdRecord::new(
            vod_item.provider_id,
            vod_item.virtual_id,
            PlaylistItemType::Video,
            VirtualId::new(0),
            shared::model::UUIDType::default(),
        ),
    );
    env.app_state.playlists.cache_id_mapping(&env.target.name, id_mapping).await;

    let xtream_router = crate::api::endpoints::xtream_api::xtream_api_register()
        .layer(mock_peer())
        .with_state(Arc::clone(&env.app_state));

    // 1. Real Xtream live route (/live/{user}/{pass}/{vid}.ts)
    let live_uri = format!(
        "/live/{}/{}/{}.ts",
        env.alice_credentials.username,
        env.alice_credentials.password,
        live_item.virtual_id.get()
    );
    let req_live = axum::http::Request::builder().method("GET").uri(live_uri).body(axum::body::Body::empty()).unwrap();
    let resp_live = xtream_router.clone().oneshot(req_live).await.unwrap();
    assert_eq!(resp_live.status(), StatusCode::OK);
    let header_val = resp_live
        .headers()
        .get("x-tuliprox-stream-uid")
        .expect("real Xtream live route must carry X-Tuliprox-Stream-Uid header");
    let header_uid: u32 = header_val.to_str().unwrap().parse().expect("uid must be decimal integer");

    // Assert the header equals the /me/streams uid
    let me_router = crate::api::endpoints::me_player_api::me_player_api_register("")
        .layer(mock_peer())
        .with_state(Arc::clone(&env.app_state));
    let me_request = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/me/streams")
        .header("Authorization", format!("Bearer {}", env.alice_token))
        .body(axum::body::Body::empty())
        .unwrap();
    let me_response = me_router.oneshot(me_request).await.unwrap();
    assert_eq!(me_response.status(), StatusCode::OK);
    let body_me = axum::body::to_bytes(me_response.into_body(), 4096).await.unwrap();
    let streams_me: Vec<shared::model::UserStreamDto> = serde_json::from_slice(&body_me).unwrap();
    assert_eq!(streams_me.len(), 1);
    assert_eq!(streams_me[0].uid, header_uid);

    // 2. Real Xtream VOD route (/movie/{user}/{pass}/{vid}.mp4) carries no header
    let vod_uri = format!(
        "/movie/{}/{}/{}.mp4",
        env.alice_credentials.username,
        env.alice_credentials.password,
        vod_item.virtual_id.get()
    );
    let req_vod = axum::http::Request::builder().method("GET").uri(vod_uri).body(axum::body::Body::empty()).unwrap();
    let resp_vod = xtream_router.oneshot(req_vod).await.unwrap();
    assert_eq!(resp_vod.status(), StatusCode::OK);
    assert!(
        !resp_vod.headers().contains_key("x-tuliprox-stream-uid"),
        "real Xtream VOD route must not carry X-Tuliprox-Stream-Uid header"
    );
}
