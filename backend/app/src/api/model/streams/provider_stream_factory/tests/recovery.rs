use super::*;

#[test]
fn test_provider_stream_factory_options_keeps_initial_retry_for_live_adaptive_streams() {
    let addr = "127.0.0.1:8080".parse().unwrap();
    let stream_url = Url::parse("http://example.com/segment.ts").unwrap();
    let req_headers = HeaderMap::new();
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: true,
        buffer_size: 1024,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::Stream,
    };

    let hls_options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::LiveHls,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });

    let dash_options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::LiveDash,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });

    assert!(hls_options.should_retry_provider_request());
    assert!(dash_options.should_retry_provider_request());
    assert!(!hls_options.should_retry_initial_open_loop());
    assert!(!dash_options.should_retry_initial_open_loop());
}

#[tokio::test]
async fn legacy_hls_stream_decoder_failure_aborts_without_retry() {
    let mut truncated = encode(b"decoder failure after response headers", TestEncoding::Gzip).await;
    truncated.truncate(truncated.len().saturating_sub(5));
    let (response, requests) = local_response(StatusCode::OK, &[("Content-Encoding", "gzip")], truncated).await;

    let ProviderStreamFactoryResponse { stream, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .unwrap();
    let error = collect_stream(stream).await.expect_err("truncated gzip must terminate the body stream");

    assert!(matches!(error, StreamError::ContentDecoding(_)));
    assert_eq!(requests.load(Ordering::SeqCst), 1, "a body-stream failure cannot trigger a new request");
}
