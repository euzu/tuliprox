use super::*;

#[test]
fn hls_mp4_mime_detection_preserves_specific_types_and_repairs_container_defaults(
) -> Result<(), Box<dyn std::error::Error>> {
    for (path, original, expected) in [
        ("tracks-v1/init-1.hls.mp4", Some("video/MP2T"), "video/mp4"),
        ("tracks-v1/init-1.hls.fmp4", Some("application/octet-stream"), "video/mp4"),
        ("tracks-a1/init-1.hls.fmp4", None, "audio/mp4"),
        ("tracks-a2/init-1.hls.mp4", Some("video/mp2t"), "audio/mp4"),
        ("audio/init.mp4", Some("audio/mp4"), "audio/mp4"),
        ("tracks-a1/init.mp4", Some("audio/mp4; codecs=mp4a.40.2"), "audio/mp4; codecs=mp4a.40.2"),
        ("tracks-v1/segment.fmp4", None, "video/mp4"),
        ("tracks-v1/segment.m4s", Some("video/mp2t; charset=binary"), "video/mp4"),
        ("audio/segment.cmfa", None, "audio/mp4"),
        ("audio/segment.m4a", None, "audio/mp4"),
        ("video/segment.CMFV", None, "video/mp4"),
        ("tracks-v1/init.mp4", Some("application/mp4"), "application/mp4"),
        ("tracks-v1/init.mp4", Some("text/html"), "text/html"),
        ("channel/dvr-1.ts", Some("video/MP2T"), "video/MP2T"),
        ("key.bin", Some("application/octet-stream"), "application/octet-stream"),
    ] {
        for flussonic_audio_tracks in [true, false] {
            let url = Url::parse(&format!("https://origin.example/{path}?token=signed&file=ignored.ts"))?;
            let mut headers = HeaderMap::new();
            if let Some(original) = original {
                headers.insert(reqwest::header::CONTENT_TYPE, HeaderValue::try_from(original)?);
            }
            let mut headers = headers.clone();
            normalize_hls_resource_content_type(&mut headers, &url, flussonic_audio_tracks);
            // Without the input option only the extension decides; tracks-a paths stay video.
            let expected = if !flussonic_audio_tracks
                && path.starts_with("tracks-a")
                && original.is_none_or(|o| !o.starts_with("audio"))
            {
                "video/mp4"
            } else {
                expected
            };
            assert_eq!(
                headers[reqwest::header::CONTENT_TYPE],
                expected,
                "{path} flussonic_audio_tracks={flussonic_audio_tracks}"
            );
        }
    }
    Ok(())
}

#[test]
fn auth_failure_selects_origin_and_allocated_account() -> Result<(), Box<dyn std::error::Error>> {
    let dto = shared::model::ConfigInputDto {
        name: Arc::from("root"),
        url: "http://main.example".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        aliases: Some(vec![shared::model::ConfigInputAliasDto {
            name: Arc::from("alias"),
            url: "http://alias.example".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            ..shared::model::ConfigInputAliasDto::default()
        }]),
        ..shared::model::ConfigInputDto::default()
    };
    let mut input = crate::model::ConfigInput::from(&dto);
    let mut options = redirect_test_options(&Url::parse("http://alias.example/live/user/pass/1.ts")?);
    assert_eq!(failed_stream_account(&input, &options).map(|account| account.name), Some(Arc::from("alias")));
    options.url = Url::parse("http://main.example/live/user/pass/1.ts")?;
    assert_eq!(failed_stream_account(&input, &options).map(|account| account.name), Some(Arc::from("root")));
    options.url = Url::parse("http://cdn.example/token")?;
    assert!(failed_stream_account(&input, &options).is_none());
    let alias = input.aliases.as_ref().and_then(|aliases| aliases.first()).ok_or("missing alias")?;
    options.account = Some(Arc::new(crate::model::ProviderConfig::new_alias(
        &input,
        alias,
        Arc::new(std::sync::RwLock::new(crate::model::ProviderConfigConnection::default())),
        Arc::new(|_, _| {}),
    )));
    assert_eq!(failed_stream_account(&input, &options).map(|account| account.name), Some(Arc::from("alias")));
    if let Some(alias) = input.aliases.as_mut().and_then(|aliases| aliases.first_mut()) {
        alias.password = Some("renewed".to_string());
    }
    assert!(failed_stream_account(&input, &options).is_none());
    Ok(())
}

#[test]
fn cross_origin_provider_requests_keep_only_redirect_safe_headers() {
    let stream_url = Url::parse("http://provider-a.example/live/segment.ts").unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    req_headers.insert(reqwest::header::RANGE, "bytes=17-31".parse().unwrap());
    req_headers.insert(reqwest::header::USER_AGENT, "test-agent".parse().unwrap());
    req_headers.insert(reqwest::header::AUTHORIZATION, "Bearer client".parse().unwrap());
    req_headers.insert(reqwest::header::COOKIE, "client=1".parse().unwrap());
    req_headers.insert(reqwest::header::PROXY_AUTHORIZATION, "Basic proxy-secret".parse().unwrap());
    let input_headers = HashMap::from([
        ("X-API-Key".to_string(), "api-secret".to_string()),
        ("X-Provider-Token".to_string(), "provider-secret".to_string()),
    ]);
    let options = test_options(
        ProviderContentRepresentationMode::PreserveOrigin,
        &stream_url,
        &req_headers,
        Some(&input_headers),
        None,
    );
    assert!(provider_headers_require_manual_redirects(options.get_headers()));

    let mut safe_headers = HeaderMap::new();
    safe_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    safe_headers.insert(reqwest::header::RANGE, "bytes=17-31".parse().unwrap());
    let safe_options =
        test_options(ProviderContentRepresentationMode::PreserveOrigin, &stream_url, &safe_headers, None, None);
    assert!(!provider_headers_require_manual_redirects(safe_options.get_headers()));

    let client = reqwest::Client::new();
    let sensitive_headers = ["authorization", "cookie", "proxy-authorization", "x-api-key", "x-provider-token"];

    let same_origin = Url::parse("http://provider-a.example/live/failover.ts").unwrap();
    let same_origin_request =
        prepare_client(&client, &options, Some(&same_origin), ProviderRequestCredentialState::OriginalOrigin)
            .0
            .build()
            .unwrap();
    for name in sensitive_headers {
        assert!(same_origin_request.headers().contains_key(name), "same-origin request lost {name}");
    }

    let cross_origin = Url::parse("http://provider-b.example/live/segment.ts").unwrap();
    for (target, credential_state) in [
        (Some(&cross_origin), ProviderRequestCredentialState::Scrubbed),
        (None, ProviderRequestCredentialState::Scrubbed),
    ] {
        let request = prepare_client(&client, &options, target, credential_state).0.build().unwrap();
        for name in sensitive_headers {
            assert!(!request.headers().contains_key(name), "cross-origin request retained {name}");
        }
        assert_eq!(request.headers()[reqwest::header::ACCEPT_ENCODING], "gzip");
        assert_eq!(request.headers()[reqwest::header::RANGE], "bytes=17-31");
        assert_eq!(request.headers()[reqwest::header::USER_AGENT], "test-agent");
    }
}

#[tokio::test]
async fn deferred_open_without_origin_head_accepts_only_plain_origin_200() {
    const IDENTITY_BODY: &[u8] = b"deferred identity body";

    let stream_url = Url::parse("http://provider.example/live/deferred.ts").unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert(reqwest::header::ACCEPT_ENCODING, "gzip".parse().unwrap());
    let deferred_options =
        test_options(ProviderContentRepresentationMode::PreserveOrigin, &stream_url, &req_headers, None, None)
            .for_deferred_open();
    let deferred_request = prepare_client(
        &reqwest::Client::new(),
        &deferred_options,
        None,
        ProviderRequestCredentialState::OriginalOrigin,
    )
    .0
    .build()
    .expect("deferred provider request");
    assert_eq!(deferred_options.content_representation(), ProviderContentRepresentationMode::Identity);
    assert!(!deferred_options.response_head_is_available());
    assert_eq!(deferred_options.hls_content_coding_object_kind(), None);
    assert_eq!(deferred_request.headers()[reqwest::header::ACCEPT_ENCODING], "identity");

    let encoded = encode(IDENTITY_BODY, TestEncoding::Gzip).await;
    let (response, _) = local_response(StatusCode::OK, &[("Content-Encoding", "gzip")], encoded).await;
    let ProviderStreamFactoryResponse { stream, info, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Unavailable,
    )
    .await
    .expect("plain 200 is safe for deferred identity streaming");
    assert_eq!(collect_stream(stream).await.expect("deferred body decodes"), IDENTITY_BODY);
    let (headers, status, _, _) = info.expect("normalized deferred response metadata");
    assert_eq!(status, StatusCode::OK);
    assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-encoding")));
    assert!(headers.iter().all(|(name, _)| !name.eq_ignore_ascii_case("content-length")));

    let encoded = encode(IDENTITY_BODY, TestEncoding::Gzip).await;
    let (response, _) =
        local_response(StatusCode::OK, &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 0-21/22")], encoded)
            .await;
    assert!(matches!(
        prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Unavailable,
        )
        .await,
        Err(ProviderStreamPreparationError::DeferredResponseHead { status: StatusCode::OK, has_content_range: true })
    ));

    let (response, _) = local_response(StatusCode::PARTIAL_CONTENT, &[], IDENTITY_BODY.to_vec()).await;
    assert!(matches!(
        prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::Identity,
            ProviderResponseHeadAvailability::Unavailable,
        )
        .await,
        Err(ProviderStreamPreparationError::DeferredResponseHead {
            status: StatusCode::PARTIAL_CONTENT,
            has_content_range: false,
        })
    ));
}

#[tokio::test]
async fn deferred_open_rejects_preserve_origin_without_origin_head() {
    let (response, _) = local_response(StatusCode::OK, &[], b"opaque bytes".to_vec()).await;

    assert!(matches!(
        prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::PreserveOrigin,
            ProviderResponseHeadAvailability::Unavailable,
        )
        .await,
        Err(ProviderStreamPreparationError::DeferredResponseHead { status: StatusCode::OK, has_content_range: false })
    ));
}

#[tokio::test]
async fn legacy_hls_rejects_encoded_partial_content_before_client_streaming() {
    let encoded = encode(b"partial identity bytes", TestEncoding::Gzip).await;
    let (response, _) = local_response(
        StatusCode::PARTIAL_CONTENT,
        &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 0-9/10")],
        encoded,
    )
    .await;

    let result = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
    )
    .await;

    assert!(matches!(
        result,
        Err(ProviderStreamPreparationError::ContentCoding(ContentCodingError::EncodedPartialContent))
    ));
}
