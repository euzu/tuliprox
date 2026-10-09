use super::{
    create_api_proxy_user, create_test_app_state, create_test_fingerprint, create_test_live_channel,
    create_test_provider_app_state, create_test_shared_target, evaluate_network_access, is_hop_by_hop_response_header,
    is_media_server_playback_url, is_media_server_stream_ref_url, is_seek_request, is_seekable_media_request,
    mark_response_as_uncompressed, resolve_request_url_for_logging, resolve_stream_config_u64, resource_proxy_response,
    spawn_legacy_hls_test_origin, stream_response, stream_responses::forced_legacy_hls_test_response,
    user_with_network_access, NetworkAccessDecision,
};
use crate::{
    api::model::{SharedStreamCtx, SharedStreamManager},
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
    model::{ClusterFlags, GeoIpUnavailablePolicy, InputType, UserConnectionPermission, XtreamCluster},
    utils::Internable,
};
use std::{borrow::Cow, collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
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

#[test]
fn media_server_playback_urls_are_proxy_only_redirect_guard_candidates() {
    let plex_input = ConfigInput { input_type: InputType::Plex, ..ConfigInput::default() };
    let emby_input = ConfigInput { input_type: InputType::Emby, ..ConfigInput::default() };
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
    assert!(!is_media_server_playback_url(&emby_input, "https://emby.example/stream.mkv"));
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
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    struct SanitizeSettingGuard(bool);
    impl Drop for SanitizeSettingGuard {
        fn drop(&mut self) { shared::utils::set_sanitize_sensitive_info(self.0); }
    }
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let prev = shared::utils::is_sanitize_sensitive_info_enabled();
    let _restore = SanitizeSettingGuard(prev);

    let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };
    let media_ref = "media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fsecret_key";
    let standard_url = "http://provider.example/live/stream.m3u8";

    // 1. When sanitization is enabled:
    shared::utils::set_sanitize_sensitive_info(true);
    assert_eq!(resolve_request_url_for_logging(&m3u_input, media_ref).as_ref(), "media-server://<redacted>");
    assert_eq!(resolve_request_url_for_logging(&m3u_input, standard_url).as_ref(), standard_url);

    // 2. When sanitization is disabled:
    shared::utils::set_sanitize_sensitive_info(false);
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
        stream_url,
        None,
        &HeaderMap::default(),
        &input,
        &target,
        &user,
        admission.permission(),
        admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal),
        false,
        None,
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
