use super::*;

#[test]
fn test_provider_stream_factory_options_propagate_buffer_max_bytes() {
    let addr = "127.0.0.1:8080".parse().unwrap();
    let stream_url = Url::parse("http://example.com/stream").unwrap();
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: true,
        buffer_size: 1024,
        buffer_max_bytes: 4096,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::Stream,
    };
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
        disabled_headers: None,
        default_user_agent: None,
        username: None,
        client_ip: None,
        stream_channel: None,
        connect_failure_stage: None,
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });
    assert_eq!(options.get_buffer_max_bytes(), 4096);
    assert_eq!(options.get_buffer_size(), 1024);
}

#[test]
fn test_shared_streams_do_not_use_provider_buffer_wrapper() {
    let addr = "127.0.0.1:8080".parse().unwrap();
    let stream_url = Url::parse("http://example.com/shared.ts").unwrap();
    let req_headers = HeaderMap::new();
    let stream_options = StreamOptions {
        stream_retry: true,
        buffer_enabled: true,
        buffer_size: 1024,
        buffer_max_bytes: 0,
        pipe_provider_stream: false,
        response_mode: StreamResponseMode::Stream,
    };

    let shared_options = ProviderStreamFactoryOptions::new(&ProviderStreamFactoryParams {
        addr,
        item_type: PlaylistItemType::Live,
        share_stream: true,
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

    assert!(
        !should_wrap_provider_stream_in_buffer(&shared_options),
        "shared streams must bypass provider-side BufferedStream"
    );
}

#[test]
fn test_provider_stream_factory_options_builds_connect_failed_stream_info_from_history_context() {
    let addr = "127.0.0.1:8080".parse().unwrap();
    let stream_url = Url::parse("http://example.com/stream").unwrap();
    let req_headers = HeaderMap::new();
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
        item_type: PlaylistItemType::Live,
        share_stream: false,
        stream_options: &stream_options,
        stream_url: &stream_url,
        req_headers: &req_headers,
        input_headers: None,
        session_headers: None,
        disabled_headers: None,
        default_user_agent: None,
        username: Some("alice"),
        client_ip: Some("203.0.113.9"),
        stream_channel: Some(&StreamChannel {
            target_id: 1,
            virtual_id: 77,
            provider_id: 3,
            input_name: "input-a".intern(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "News".intern(),
            title: "Example".intern(),
            url: "http://provider.example/live/77".intern(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: Some("Channel-UA".intern()),
        }),
        connect_failure_stage: Some(FailureStage::ProviderOpen),
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
    });

    let info = options.build_connect_failed_stream_info("provider-a".intern()).expect("history context");

    assert_eq!(info.username, "alice");
    assert_eq!(info.client_ip, "203.0.113.9");
    assert_eq!(info.provider.as_ref(), "provider-a");
    assert_eq!(info.channel.input_name.as_ref(), "input-a");
    assert_eq!(info.channel.virtual_id, 77);
    assert_eq!(
        options.headers.get(reqwest::header::USER_AGENT).and_then(|value| value.to_str().ok()),
        Some("Channel-UA")
    );
}

#[test]
fn html_content_type_is_rejected_for_catchup_streams() {
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::CONTENT_TYPE, "text/html; charset=UTF-8".parse().unwrap());

    assert!(should_reject_success_response_content_type(PlaylistItemType::Catchup, &headers));
    assert!(should_reject_success_response_content_type(PlaylistItemType::Video, &headers));
    assert!(should_reject_success_response_content_type(PlaylistItemType::Live, &headers));
}

#[test]
fn html_content_type_is_allowed_for_live_adaptive_streams() {
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::CONTENT_TYPE, "text/html; charset=UTF-8".parse().unwrap());

    assert!(!should_reject_success_response_content_type(PlaylistItemType::LiveHls, &headers));
    assert!(!should_reject_success_response_content_type(PlaylistItemType::LiveDash, &headers));
}

#[tokio::test]
async fn provider_stream_factory_captures_session_cookies_separately() {
    let (response, _) = local_response(
        StatusCode::OK,
        &[("content-type", "application/vnd.apple.mpegurl"), ("set-cookie", "sid=abc; Path=/")],
        b"#EXTM3U\nsegment.ts\n".to_vec(),
    )
    .await;

    let result = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
    )
    .await;

    assert!(result.is_ok());
    if let Ok(response) = result {
        assert_eq!(response.provider_session_headers.headers.get("cookie").map(String::as_str), Some("sid=abc"));
        assert!(response.info.as_ref().is_some_and(|(headers, _, _, _)| headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("set-cookie"))));
    }
}

#[test]
fn preserve_origin_does_not_override_accept_encoding() {
    let stream_url = Url::parse("http://provider.example/movie").unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip, br".parse().unwrap());
    let options =
        test_options(ProviderContentRepresentationMode::PreserveOrigin, &stream_url, &req_headers, None, None);

    let request =
        prepare_client(&reqwest::Client::new(), &options, None, ProviderRequestCredentialState::OriginalOrigin)
            .0
            .build()
            .unwrap();

    assert_eq!(request.headers()[reqwest::header::ACCEPT_ENCODING], "gzip, br");
}
