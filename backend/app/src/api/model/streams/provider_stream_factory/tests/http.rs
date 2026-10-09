use super::*;

#[test]
fn provider_requests_preserve_complete_client_range_header() {
    let stream_url = Url::parse("http://provider.example/movie").unwrap();
    let client = reqwest::Client::new();

    for requested_range in ["bytes=100-199", "bytes=100-", "bytes=-100", "bytes=0-0", "bytes=0-0,200-299"] {
        let mut req_headers = HeaderMap::new();
        req_headers.insert(reqwest::header::RANGE, requested_range.parse().unwrap());
        let options =
            test_options(ProviderContentRepresentationMode::PreserveOrigin, &stream_url, &req_headers, None, None);

        let (request, partial) =
            prepare_client(&client, &options, None, ProviderRequestCredentialState::OriginalOrigin);
        let request = request.build().unwrap();

        assert!(partial, "Range request was not recognized: {requested_range}");
        assert_eq!(
            request.headers().get(reqwest::header::RANGE).and_then(|value| value.to_str().ok()),
            Some(requested_range)
        );
    }
}

#[test]
fn client_range_takes_precedence_over_configured_provider_range() {
    let stream_url = Url::parse("http://provider.example/movie").unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert(reqwest::header::RANGE, "bytes=100-199".parse().unwrap());
    let input_headers = HashMap::from([("Range".to_string(), "bytes=0-".to_string())]);
    let options = test_options(
        ProviderContentRepresentationMode::PreserveOrigin,
        &stream_url,
        &req_headers,
        Some(&input_headers),
        None,
    );

    let request =
        prepare_client(&reqwest::Client::new(), &options, None, ProviderRequestCredentialState::OriginalOrigin)
            .0
            .build()
            .unwrap();

    assert_eq!(request.headers()[reqwest::header::RANGE], "bytes=100-199");
}

#[test]
fn test_provider_stream_factory_options_range_logic() {
    let addr = "127.0.0.1:8080".parse().unwrap();
    let stream_url = Url::parse("http://example.com/stream").unwrap();
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: true,
        buffer_size: 1024,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::Stream,
    };
    let disabled_headers = None;

    // Case 1: VOD, no initial range requested
    let mut req_headers = HeaderMap::new();
    let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::Video,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });
    assert!(!options.was_range_requested());
    assert!(options.get_requested_range().is_none());

    // Case 2: VOD, range requested
    req_headers.insert("Range", "bytes=100-".parse().unwrap());
    let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::Video,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });
    assert!(options.was_range_requested());
    assert_eq!(options.get_requested_range().and_then(|value| value.to_str().ok()), Some("bytes=100-"));

    // Case 3: Live, no initial range requested
    let req_headers = HeaderMap::new();
    let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::Live,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });
    assert!(!options.was_range_requested());
    assert!(options.get_requested_range().is_none());

    // Case 4: Live, range requested (should be stripped)
    let mut req_headers = HeaderMap::new();
    req_headers.insert("Range", "bytes=100-".parse().unwrap());
    let options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::Live,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });
    assert!(!options.was_range_requested()); // Stripped by filter
    assert!(options.get_requested_range().is_none());
}

#[tokio::test]
async fn legacy_hls_decoder_setup_obeys_provider_body_idle_timeout() {
    let (response, origin_task) = local_stalled_deflate_response().await;

    let result = prepare_provider_stream_response_with_idle_timeout(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
        Duration::from_millis(25),
    )
    .await;
    origin_task.abort();

    assert!(matches!(
        result,
        Err(ProviderStreamPreparationError::ContentCoding(ContentCodingError::PrefixRead(error)))
            if error.kind() == io::ErrorKind::TimedOut
    ));
}

#[tokio::test]
async fn preserve_origin_keeps_encoded_body_and_representation_headers() {
    let encoded = encode(b"preserved representation", TestEncoding::Gzip).await;
    let expected = encoded.clone();
    let expected_len = encoded.len().to_string();
    let (response, _) = local_response(
        StatusCode::PARTIAL_CONTENT,
        &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 10-20/100")],
        encoded,
    )
    .await;

    let ProviderStreamFactoryResponse { stream, info, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::PreserveOrigin,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .unwrap();
    let (headers, status, _, _) = info.unwrap();

    assert_eq!(collect_stream(stream).await.unwrap(), expected);
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(headers.iter().any(|(name, value)| name == "content-encoding" && value == "gzip"));
    assert!(headers.iter().any(|(name, value)| name == "content-length" && value == &expected_len));
    assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 10-20/100"));
    assert!(headers.iter().all(|(name, _)| name != "transfer-encoding"));
}

#[tokio::test]
async fn preserve_origin_keeps_unknown_content_coding_and_body_unchanged() {
    let body = b"opaque provider representation".to_vec();
    let expected = body.clone();
    let expected_len = body.len().to_string();
    let (response, _) = local_response(
        StatusCode::PARTIAL_CONTENT,
        &[("Content-Encoding", "x-provider-coding"), ("Content-Range", "bytes 0-29/100")],
        body,
    )
    .await;

    let ProviderStreamFactoryResponse { stream, info, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::PreserveOrigin,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .expect("valid unknown coding is preserved");
    let (headers, status, _, _) = info.expect("preserved response metadata");

    assert_eq!(collect_stream(stream).await.expect("opaque body streams"), expected);
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(headers.iter().any(|(name, value)| name == "content-encoding" && value == "x-provider-coding"));
    assert!(headers.iter().any(|(name, value)| name == "content-length" && value == &expected_len));
    assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 0-29/100"));
}

#[tokio::test]
async fn preserve_origin_never_magic_sniffs_headerless_body() {
    let body = Bytes::from_static(b"\x1f\x8bopaque non-hls bytes");
    let (response, _) = local_response(StatusCode::OK, &[], body.to_vec()).await;

    let ProviderStreamFactoryResponse { stream, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::PreserveOrigin,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .unwrap();

    assert_eq!(collect_stream(stream).await.unwrap(), body);
}
