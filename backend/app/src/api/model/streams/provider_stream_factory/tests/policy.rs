use super::*;

#[test]
fn session_headers_are_forwarded_to_provider_requests() {
    let addr = "127.0.0.1:8080".parse().unwrap();
    let stream_url = Url::parse("http://example.com/live/segment.ts").unwrap();
    let req_headers = HeaderMap::new();
    let mut session_headers = HashMap::new();
    session_headers.insert(String::from("cookie"), String::from("sid=abc; pref=1"));
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: true,
        buffer_size: 1024,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::Stream,
    };

    let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::LiveHls,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: Some(&session_headers),
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });

    assert_eq!(
        options.get_headers().get(axum::http::header::COOKIE).and_then(|value| value.to_str().ok()),
        Some("sid=abc; pref=1")
    );
}

#[test]
fn provider_scheme_resolution_keeps_configured_headers_before_redirect() {
    let stream_url = Url::parse("provider://mirrors/live/segment.ts").unwrap();
    let req_headers = HeaderMap::new();
    let input_headers = HashMap::from([("X-API-Key".to_string(), "api-secret".to_string())]);
    let options = test_options(
        ProviderContentRepresentationMode::PreserveOrigin,
        &stream_url,
        &req_headers,
        Some(&input_headers),
        None,
    );
    let resolved_url = Url::parse("https://mirror-a.example/live/segment.ts").unwrap();

    let request = prepare_client(
        &reqwest::Client::new(),
        &options,
        Some(&resolved_url),
        ProviderRequestCredentialState::OriginalOrigin,
    )
    .0
    .build()
    .unwrap();

    assert_eq!(request.headers()["x-api-key"], "api-secret");
}

#[tokio::test]
async fn legacy_hls_declared_only_preserves_headerless_gzip_magic() {
    let body = Bytes::from_static(b"\x1f\x8bnot-an-encoded-key");
    let (response, _) = local_response(StatusCode::OK, &[], body.to_vec()).await;

    let ProviderStreamFactoryResponse { stream, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .unwrap();

    assert_eq!(collect_stream(stream).await.unwrap(), body);
}
