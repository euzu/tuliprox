use super::*;

#[test]
fn identity_is_enforced_after_header_merges_for_every_request_target() {
    let stream_url = Url::parse("http://provider-a.example/live/segment.ts").unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert(reqwest::header::ACCEPT_ENCODING, "br".parse().unwrap());
    req_headers.insert(reqwest::header::RANGE, "bytes=17-31".parse().unwrap());
    req_headers.insert(reqwest::header::AUTHORIZATION, "Bearer client".parse().unwrap());
    req_headers.insert(reqwest::header::COOKIE, "client=1".parse().unwrap());
    let input_headers = HashMap::from([("Accept-Encoding".to_string(), "gzip".to_string())]);
    let session_headers = HashMap::from([("Accept-Encoding".to_string(), "zstd".to_string())]);
    let options = test_options(
        ProviderContentRepresentationMode::Identity,
        &stream_url,
        &req_headers,
        Some(&input_headers),
        Some(&session_headers),
    );
    let client = reqwest::Client::new();

    let same_origin = Url::parse("http://provider-a.example/live/failover.ts").unwrap();
    let cross_origin = Url::parse("http://provider-b.example/live/segment.ts").unwrap();
    for target in [None, Some(&same_origin), Some(&cross_origin)] {
        let credential_state = if target == Some(&cross_origin) {
            ProviderRequestCredentialState::Scrubbed
        } else {
            ProviderRequestCredentialState::OriginalOrigin
        };
        let request = prepare_client(&client, &options, target, credential_state).0.build().unwrap();
        assert_eq!(request.headers()[reqwest::header::ACCEPT_ENCODING], "identity");
        assert_eq!(request.headers()[reqwest::header::RANGE], "bytes=17-31");
        if target == Some(&cross_origin) {
            assert!(!request.headers().contains_key(reqwest::header::AUTHORIZATION));
            assert!(!request.headers().contains_key(reqwest::header::COOKIE));
        }
    }

    let reconnect_options = options.clone();
    assert_eq!(reconnect_options.content_representation(), ProviderContentRepresentationMode::Identity);
    assert_eq!(reconnect_options.hls_content_coding_object_kind(), Some(HlsOriginContentCodingObjectKind::Other));
    assert_eq!(
        options.clone().for_deferred_open().hls_content_coding_object_kind(),
        Some(HlsOriginContentCodingObjectKind::Other)
    );
}

#[tokio::test]
async fn automatic_redirect_round_trip_keeps_identity_and_never_restores_credentials() {
    let encoded = encode(b"decoded redirect body", TestEncoding::Gzip).await;
    let RedirectRoundTrip { entry_url, first_origin_task, second_origin_task } =
        spawn_redirect_round_trip(encoded).await;
    let options = redirect_test_options(&entry_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .expect("automatic redirect client");

    let response = prepare_client(&client, &options, None, ProviderRequestCredentialState::OriginalOrigin)
        .0
        .send()
        .await
        .expect("automatic redirect request succeeds");

    assert_identity_redirect_result(response, first_origin_task, second_origin_task).await;
}

#[tokio::test]
async fn manual_redirect_round_trip_keeps_identity_and_never_restores_credentials() {
    let encoded = encode(b"decoded redirect body", TestEncoding::Gzip).await;
    let RedirectRoundTrip { entry_url, first_origin_task, second_origin_task } =
        spawn_redirect_round_trip(encoded).await;
    let options = redirect_test_options(&entry_url);
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("manual redirect client");

    let response = send_with_manual_redirects(&client, &options, &test_app_config())
        .await
        .expect("manual redirect request succeeds");

    assert_identity_redirect_result(response, first_origin_task, second_origin_task).await;
}

#[tokio::test]
async fn legacy_hls_segment_key_map_part_and_other_stream_as_identity() {
    const IDENTITY: &[u8] = b"identity hls resource bytes";
    let cases = [
        ("segment", "gzip", TestEncoding::Gzip),
        ("segment-zlib", "deflate", TestEncoding::Zlib),
        ("segment-raw-deflate", "deflate", TestEncoding::RawDeflate),
        ("key", "br", TestEncoding::Brotli),
        ("map", "zstd", TestEncoding::Zstd),
        ("part", "gzip", TestEncoding::Gzip),
        ("other", "br", TestEncoding::Brotli),
    ];

    for (resource_kind, content_encoding, encoding) in cases {
        let encoded = encode(IDENTITY, encoding).await;
        let (response, _) = local_response(StatusCode::OK, &[("Content-Encoding", content_encoding)], encoded).await;

        let ProviderStreamFactoryResponse { stream, info, .. } = prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Available,
        )
        .await
        .unwrap();

        assert_eq!(collect_stream(stream).await.unwrap(), IDENTITY, "failed for {resource_kind}");
        let (headers, status, _, _) = info.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-encoding")));
        assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));
    }
}

#[tokio::test]
async fn legacy_hls_identity_keeps_unencoded_partial_content_headers() {
    const PARTIAL_BODY: &[u8] = b"abc";
    let (response, _) =
        local_response(StatusCode::PARTIAL_CONTENT, &[("Content-Range", "bytes 0-2/10")], PARTIAL_BODY.to_vec()).await;

    let ProviderStreamFactoryResponse { stream, info, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .expect("unencoded partial identity response is safe");
    let (headers, status, _, _) = info.expect("partial response metadata");

    assert_eq!(collect_stream(stream).await.expect("partial body streams"), PARTIAL_BODY);
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(headers.iter().any(|(name, value)| name == "content-length" && value == "3"));
    assert!(headers.iter().any(|(name, value)| name == "content-range" && value == "bytes 0-2/10"));
    assert!(headers.iter().all(|(name, _)| name != "content-encoding"));
}
