use super::{
    apply_hls_user_agent_stream_index, assert_hls_cache_stream_registered, build_hls_manifest_request_headers,
    cache_recording_test_item, configure_recording_test_listener, enable_hls_cache, encode_test_manifest, get_response,
    hls_proxy_uri, hls_session_last_media_at_ms, legacy_manifest_test_client_headers, legacy_manifest_test_input,
    map_transient_resource, recording_test_response, recording_test_router, recording_test_url, response_body,
    spawn_recording_header_origin, spawn_test_encoded_manifest_origin, spawn_test_transient_origin,
    store_test_sources_with_target, test_app_state, test_app_state_with_inputs, test_hls_share_target,
    test_m3u_hls_item,
};
use crate::{
    api::model::{HlsSession, HlsSessionKey, ProxySessionId},
    model::{ConfigInput, ReverseProxyDisabledHeaderConfig},
};
use axum::{
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
};
use http_body_util::BodyExt;
use shared::model::{InputType, M3uTargetOutputDto, XtreamCluster};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tuliprox_hls::api::{extract_hls_provider_session_headers, MAX_HLS_MANIFEST_BYTES};

#[test]
fn extract_hls_provider_session_headers_converts_set_cookie_to_cookie_header() {
    let mut headers = HeaderMap::new();
    headers.append("set-cookie", "sid=abc; Path=/; HttpOnly".parse().expect("valid cookie"));
    headers.append("set-cookie", "pref=1; Secure".parse().expect("valid cookie"));

    let session_headers = extract_hls_provider_session_headers(&headers);

    assert_eq!(session_headers.headers.get("cookie").map(String::as_str), Some("sid=abc; pref=1"));
}

#[test]
fn hls_response_uses_rfc8216_content_type_and_remains_tower_compressible() {
    let response = super::super::hls_response("#EXTM3U\n".to_string()).into_response();

    assert_eq!(response.headers().get(header::CONTENT_TYPE).unwrap(), "application/vnd.apple.mpegurl");
    assert!(tuliprox_core::utils::response_compression::should_compress_response(&response));
}

#[test]
fn hls_manifest_headers_apply_disabled_headers_and_default_user_agent_policy() {
    let mut input_headers = HashMap::new();
    input_headers.insert("User-Agent".to_string(), "Input-UA".to_string());
    input_headers.insert("Accept-Language".to_string(), "de".to_string());
    input_headers.insert("Accept-Encoding".to_string(), "gzip".to_string());
    input_headers.insert("Authorization".to_string(), "Bearer input-secret".to_string());
    input_headers.insert("X-Origin-Secret".to_string(), "input-secret".to_string());

    let mut request_headers = HeaderMap::new();
    request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
    request_headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("br"));
    request_headers.insert(header::USER_AGENT, HeaderValue::from_static("Client-UA"));
    request_headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer client-secret"));
    request_headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
    request_headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
    request_headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
    request_headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("client"));
    request_headers.insert(HeaderName::from_static("cf-ray"), HeaderValue::from_static("cf"));

    let disabled = ReverseProxyDisabledHeaderConfig {
        referer_header: false,
        x_header: true,
        cloudflare_header: true,
        custom_header: vec!["X-Origin-Secret".to_string()],
    };
    let headers = build_hls_manifest_request_headers(
        &input_headers,
        &request_headers,
        Some(&disabled),
        Some("Default-UA"),
        Some("Channel-UA"),
    );

    assert_eq!(headers.get(header::USER_AGENT).expect("user agent"), "Channel-UA");
    assert_eq!(headers.get("accept-language").expect("accept language"), "de");
    assert_eq!(headers.get(header::ACCEPT_ENCODING).expect("accept encoding"), "identity");
    assert!(!headers.contains_key(header::RANGE));
    assert!(!headers.contains_key(header::AUTHORIZATION));
    assert!(!headers.contains_key(header::COOKIE));
    assert!(!headers.contains_key("proxy-authorization"));
    assert!(!headers.contains_key(header::HOST));
    assert!(!headers.contains_key("x-origin-secret"));
    assert!(!headers.contains_key("x-blocked"));
    assert!(!headers.contains_key("cf-ray"));
}

#[tokio::test]
async fn hls_user_agent_stream_index_is_stable_for_the_session() {
    let app_state = test_app_state();
    let session = Arc::new(tokio::sync::RwLock::new(HlsSession::new(HlsSessionKey::new(1, "stream-1"), b"secret", 0)));
    let mut manifest_headers = HeaderMap::new();
    manifest_headers.insert(header::USER_AGENT, HeaderValue::from_static("VLC/3.0"));

    apply_hls_user_agent_stream_index(&session, &mut manifest_headers, true, &app_state.active_users).await;
    let stream_index = session.read().await.user_agent_stream_index.unwrap_or_default();
    assert_ne!(stream_index, 0);
    assert_eq!(
        manifest_headers.get(header::USER_AGENT).and_then(|value| value.to_str().ok()),
        Some(format!("VLC/3.0 {stream_index}").as_str())
    );

    let mut segment_headers = HeaderMap::new();
    segment_headers.insert(header::USER_AGENT, HeaderValue::from_static("VLC/3.0"));
    apply_hls_user_agent_stream_index(&session, &mut segment_headers, true, &app_state.active_users).await;

    assert_eq!(
        segment_headers.get(header::USER_AGENT).and_then(|value| value.to_str().ok()),
        Some(format!("VLC/3.0 {stream_index}").as_str())
    );
    assert_eq!(session.read().await.user_agent_stream_index, Some(stream_index));
}

#[tokio::test]
async fn legacy_hls_manifest_decodes_supported_origin_codings_and_enforces_identity() {
    const MANIFEST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nsegment.ts\n";

    for coding in ["gzip", "deflate", "br", "zstd"] {
        let encoded = encode_test_manifest(coding, MANIFEST.as_bytes()).await;
        let origin = spawn_test_encoded_manifest_origin(Some(coding), encoded, Duration::ZERO).await;
        let input = legacy_manifest_test_input(&origin);
        let client_headers = legacy_manifest_test_client_headers();

        let (manifest, final_url, _) =
            super::super::download_legacy_hls_manifest(&test_app_state(), &input, &client_headers)
                .await
                .unwrap_or_else(|error| panic!("{coding} manifest should decode: {error}"));

        assert_eq!(manifest, MANIFEST, "coding={coding}");
        assert_eq!(final_url, input.url, "coding={coding}");
        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 1, "coding={coding}");
        assert!(
            requests[0].to_ascii_lowercase().contains("\r\naccept-encoding: identity\r\n"),
            "coding={coding}, request={}",
            requests[0]
        );
    }
}

#[tokio::test]
async fn legacy_hls_manifest_handles_identity_and_headerless_gzip_magic() {
    const MANIFEST: &[u8] = b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n";
    let cases = [("identity", MANIFEST.to_vec()), ("gzip-magic", encode_test_manifest("gzip", MANIFEST).await)];

    for (case, body) in cases {
        let origin = spawn_test_encoded_manifest_origin(None, body, Duration::ZERO).await;
        let input = legacy_manifest_test_input(&origin);

        let (manifest, _, _) = super::super::download_legacy_hls_manifest(
            &test_app_state(),
            &input,
            &legacy_manifest_test_client_headers(),
        )
        .await
        .unwrap_or_else(|error| panic!("{case} manifest should decode: {error}"));

        assert_eq!(manifest.as_bytes(), MANIFEST, "case={case}");
    }
}

#[tokio::test]
async fn legacy_hls_manifest_limit_applies_after_decompression() {
    let decoded = vec![b'x'; MAX_HLS_MANIFEST_BYTES + 1];
    let origin =
        spawn_test_encoded_manifest_origin(Some("gzip"), encode_test_manifest("gzip", &decoded).await, Duration::ZERO)
            .await;
    let input = legacy_manifest_test_input(&origin);

    let error =
        super::super::download_legacy_hls_manifest(&test_app_state(), &input, &legacy_manifest_test_client_headers())
            .await
            .expect_err("decoded manifest above limit must fail");

    assert!(matches!(
        error.get_ref().and_then(|source| source.downcast_ref()),
        Some(crate::utils::content_coding::ContentBodyReadError::LimitExceeded { limit })
            if *limit == MAX_HLS_MANIFEST_BYTES
    ));
}

#[test]
fn hls_proxy_public_path_prefix_rewrites_only_proxy_hls_uri_surfaces() {
    let body = concat!(
        "#EXTM3U\n",
        "#EXT-X-KEY:METHOD=AES-128,URI=\"/hls/shared/live/proxy-id/r/key.key\",IV=0x1\n",
        "#EXT-X-MAP:URI=\"/hls/shared/live/proxy-id/map/000000.mp4\",BYTERANGE=\"10@0\"\n",
        "#EXT-X-PART:DURATION=1.0,URI=\"/hls/shared/live/proxy-id/r/part.m4s\"\n",
        "#EXT-X-MEDIA-SEQUENCE:7\n",
        "#EXTINF:4.0,\n",
        "/hls/shared/live/proxy-id/000007.ts\n",
        "https://origin.example.com/not-proxy.ts\n",
    );

    let prefixed = super::super::apply_hls_proxy_public_path_prefix(body.to_string(), Some("/iptv/"));

    assert!(prefixed.contains("URI=\"/iptv/hls/shared/live/proxy-id/r/key.key\""));
    assert!(prefixed.contains("URI=\"/iptv/hls/shared/live/proxy-id/map/000000.mp4\""));
    assert!(prefixed.contains("URI=\"/iptv/hls/shared/live/proxy-id/r/part.m4s\""));
    assert!(prefixed.contains("\n/iptv/hls/shared/live/proxy-id/000007.ts\n"));
    assert!(prefixed.contains("#EXT-X-MEDIA-SEQUENCE:7"));
    assert!(prefixed.contains("https://origin.example.com/not-proxy.ts"));
}

#[test]
fn hls_proxy_public_path_prefix_keeps_body_unchanged_without_server_path() {
    let body = "#EXTM3U\n#EXTINF:4.0,\n/hls/shared/live/proxy-id/000007.ts\n".to_string();

    assert_eq!(super::super::apply_hls_proxy_public_path_prefix(body.clone(), None), body);
    assert_eq!(super::super::apply_hls_proxy_public_path_prefix(body.clone(), Some("/")), body);
}

pub(in crate::api::endpoints::hls_api::tests) fn local_recording_hls_url(
    manifest: &str,
) -> Result<url::Url, Box<dyn std::error::Error>> {
    let resource =
        manifest.lines().find(|line| !line.is_empty() && !line.starts_with('#')).ok_or("HLS resource URI missing")?;
    let url = url::Url::parse(resource)?;
    assert_eq!(url.scheme(), "http");
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.port(), Some(8901));
    assert!(url.path().starts_with("/hls/recording_user/"), "{url}");
    Ok(url)
}

#[tokio::test]
async fn recording_hls_routes_keep_local_urls_and_headers_after_session_loss() -> Result<(), Box<dyn std::error::Error>>
{
    for (output, share) in [
        (shared::model::TargetType::Xtream, false),
        (shared::model::TargetType::M3u, false),
        (shared::model::TargetType::Xtream, true),
        (shared::model::TargetType::M3u, true),
    ] {
        let (origin_url, requests, origin_task) = spawn_recording_header_origin().await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("recording-input"),
            input_type: if output == shared::model::TargetType::Xtream { InputType::Xtream } else { InputType::M3u },
            url: origin_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let mut target = test_hls_share_target(share);
        target.use_memory_cache = true;
        if output == shared::model::TargetType::M3u {
            target.output = vec![crate::model::TargetOutput::M3u(crate::model::M3uTargetOutput::from(
                &M3uTargetOutputDto::default(),
            ))];
        }
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        store_test_sources_with_target(&app_state, input.clone(), target.clone());
        app_state.app_config.api_proxy.store(None);
        if share {
            enable_hls_cache(&app_state);
        }
        let storage = tempfile::tempdir()?;
        configure_recording_test_listener(&app_state, storage.path());
        let manifest_url = format!("{origin_url}/live/user/pass/12345.m3u8");
        cache_recording_test_item(&app_state, &target, test_m3u_hls_item(&input, 12345, "12345", &manifest_url))
            .await?;
        let router = recording_test_router(&app_state);
        let entry =
            recording_test_response(&router, &recording_test_url(&app_state, &target, &input, XtreamCluster::Live)?)
                .await?;
        assert_eq!(entry.status(), StatusCode::OK, "{output:?}");
        let master = String::from_utf8(entry.into_body().collect().await?.to_bytes().to_vec())?;
        let variant_url = local_recording_hls_url(&master)?;
        let mut public_credentials = variant_url.clone();
        public_credentials.set_path(&variant_url.path().replacen(
            &crate::api::api_utils::create_recording_proxy_user(&app_state).password,
            crate::model::RECORDING_PROXY_USERNAME,
            1,
        ));
        let denied = recording_test_response(&router, public_credentials.as_str()).await?;
        assert!(!denied.status().is_success(), "the public recording_user password must not authorize HLS");
        let token = variant_url.path_segments().and_then(Iterator::last).ok_or("HLS token missing")?;
        let decoded = super::get_hls_session_token_and_url_from_token(&app_state.get_encrypt_secret(), token)
            .ok_or("HLS token cannot be decoded")?;
        let session_token = decoded.session_token.ok_or("HLS session token missing")?;
        let variant = recording_test_response(&router, variant_url.as_str()).await?;
        assert_eq!(variant.status(), StatusCode::OK, "{output:?}");
        let media = String::from_utf8(variant.into_body().collect().await?.to_bytes().to_vec())?;
        let segment_url = local_recording_hls_url(&media)?;
        let segment = recording_test_response(&router, segment_url.as_str()).await?;
        assert_eq!(segment.status(), StatusCode::OK, "{output:?}");
        assert_eq!(segment.into_body().collect().await?.to_bytes().as_ref(), b"segment-bytes");
        // The hand-off served the first variant; a refresh must fetch the origin again.
        let refresh = recording_test_response(&router, variant_url.as_str()).await?;
        assert_eq!(refresh.status(), StatusCode::OK, "{output:?}");
        let refreshed = String::from_utf8(refresh.into_body().collect().await?.to_bytes().to_vec())?;
        local_recording_hls_url(&refreshed)?;
        assert!(app_state.active_users.terminate_session(crate::model::RECORDING_PROXY_USERNAME, &session_token).await);
        let recreated = recording_test_response(&router, variant_url.as_str()).await?;
        assert_eq!(recreated.status(), StatusCode::OK, "{output:?}");
        let recreated = String::from_utf8(recreated.into_body().collect().await?.to_bytes().to_vec())?;
        local_recording_hls_url(&recreated)?;
        assert!(app_state
            .active_users
            .get_and_update_user_session(crate::model::RECORDING_PROXY_USERNAME, &session_token)
            .await
            .is_some());
        let requests = requests.lock().map_err(|error| error.to_string())?.clone();
        assert!(requests.iter().any(|request| request.contains("seg1.ts")), "{requests:?}");
        assert_eq!(requests.iter().filter(|request| request.contains("12345.m3u8")).count(), 3);
        assert!(requests.iter().all(|request| request.contains("x-recording-test: custom")), "{requests:?}");
        origin_task.abort();
    }
    Ok(())
}

#[tokio::test]
async fn transient_resource_with_valid_lease_streams_origin_response_and_headers() {
    let app_state = test_app_state();
    let origin = spawn_test_transient_origin().await;
    let (proxy_session_id, resource_id) =
        map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
    let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;
    let session = app_state
        .hls
        .proxy
        .sessions()
        .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.clone()))
        .await
        .expect("session should exist");
    {
        let mut session = session.write().await;
        session.origin_request_headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        session.origin_request_headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
        session
            .origin_request_headers
            .insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        session.origin_request_headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
        session
            .origin_request_headers
            .insert(HeaderName::from_static("x-tuliprox-main-revision"), HeaderValue::from_static("secret"));
        session.origin_request_headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));
    }

    let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp2t");
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-15/16");
    assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::ETAG], "\"abc\"");
    assert_eq!(response.headers()[header::LAST_MODIFIED], "Wed, 21 Oct 2015 07:28:00 GMT");
    assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"transient-body"));
    assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
    let origin_requests = origin.requests.lock().await;
    let origin_request = origin_requests.first().expect("origin request").to_ascii_lowercase();
    assert!(origin_request.contains("range: bytes=2-15"));
    assert!(origin_request.contains("accept-language: de"));
    assert!(!origin_request.contains("authorization: bearer secret"));
    assert!(!origin_request.contains("cookie: sid=secret"));
    assert!(!origin_request.contains("proxy-authorization: basic secret"));
    assert!(!origin_request.contains("host: proxy.example.com"));
    assert!(!origin_request.contains("x-tuliprox-main-revision"));
    assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
}

#[test]
fn transient_cross_origin_redirect_strips_sensitive_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
    headers.insert(header::COOKIE, HeaderValue::from_static("session=secret"));
    headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
    headers.insert(header::HOST, HeaderValue::from_static("origin.example.com"));
    headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1"));

    crate::api::model::scrub_hls_origin_headers(&mut headers, None);

    assert!(!headers.contains_key(header::AUTHORIZATION));
    assert!(!headers.contains_key(header::COOKIE));
    assert!(!headers.contains_key("proxy-authorization"));
    assert!(!headers.contains_key(header::HOST));
    assert_eq!(headers[header::RANGE], "bytes=0-1");
}
