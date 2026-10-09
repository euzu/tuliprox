use super::*;

#[tokio::test]
async fn direct_key_decodes_declared_gzip_brotli_and_zstd_to_exact_identity_bytes() {
    let identity = b"\x00\xffdirect-key-identity\x10\x80";
    let encoded_bodies = [
        ("gzip", gzip_encode(identity).await),
        ("br", brotli_encode(identity).await),
        ("zstd", zstd_encode(identity).await),
    ];

    for (encoding, encoded) in encoded_bodies {
        let origin = spawn_test_origin(
            "200 OK",
            vec![("Content-Encoding", encoding), ("Content-Type", "application/octet-stream")],
            encoded,
        )
        .await;
        let decoded = fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
            .await
            .expect("declared direct key coding decodes");
        let response = direct_client_response(decoded, TransientResourceKind::Key);

        assert!(!should_compress_response(&response));
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.expect("decoded key body streams"),
            identity.as_slice()
        );

        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert!(requests[0].to_ascii_lowercase().contains("accept-encoding: identity"));
    }
}

#[tokio::test]
async fn direct_map_and_part_or_other_resources_decode_to_identity() {
    let identity = b"shared-direct-map-part-other";
    for (resource_kind, encoding, encoded) in [
        (TransientResourceKind::Map, "gzip", gzip_encode(identity).await),
        (TransientResourceKind::Part, "br", brotli_encode(identity).await),
        (TransientResourceKind::Other, "zstd", zstd_encode(identity).await),
    ] {
        let origin = spawn_test_origin(
            "200 OK",
            vec![("Content-Encoding", encoding), ("Content-Type", "application/octet-stream")],
            encoded,
        )
        .await;
        let decoded = fetch_direct_decoded(format!("{}/object.bin", origin.base_url), resource_kind, None)
            .await
            .expect("declared direct resource coding decodes");
        let response = direct_client_response(decoded, resource_kind);

        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.expect("decoded resource body streams"),
            identity.as_slice()
        );
        assert_eq!(origin.requests.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn direct_binary_declared_only_leaves_headerless_gzip_magic_unchanged() {
    let identity = b"headerless-gzip-representation";
    let headerless_encoded = gzip_encode(identity).await;
    let random_magic = vec![0x1f, 0x8b, 0x11, 0x00, 0xff, 0x42, 0x7e];

    for body in [headerless_encoded, random_magic] {
        let origin =
            spawn_test_origin("200 OK", vec![("Content-Type", "application/octet-stream")], body.clone()).await;
        let decoded = fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
            .await
            .expect("headerless binary is not inspected");
        let response = direct_client_response(decoded, TransientResourceKind::Key);

        assert_eq!(to_bytes(response.into_body(), usize::MAX).await.expect("headerless binary streams"), body);
        assert_eq!(origin.requests.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn decoded_direct_response_normalizes_headers_and_disables_tower_compression() {
    let identity = b"normalized-direct-response";
    let encoded = gzip_encode(identity).await;
    let origin = spawn_test_origin(
        "200 OK",
        vec![
            ("Content-Encoding", "gzip"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Range", "bytes 0-9/10"),
            ("Accept-Ranges", "bytes"),
            ("Cache-Control", "private, max-age=5"),
            ("ETag", "strong-origin-representation"),
            ("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
            ("Set-Cookie", "provider_session=secret"),
            ("X-Provider-Secret", "do-not-forward"),
        ],
        encoded,
    )
    .await;
    let decoded = fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
        .await
        .expect("direct response decodes");
    let response = direct_client_response(decoded, TransientResourceKind::Key);
    let headers = response.headers();

    assert!(!should_compress_response(&response));
    for removed in [
        header::CONTENT_ENCODING,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::ETAG,
        header::TRANSFER_ENCODING,
        header::SET_COOKIE,
    ] {
        assert!(headers.get(&removed).is_none(), "{removed} must not describe the decoded response");
    }
    assert!(headers.get("x-provider-secret").is_none());
    assert_eq!(headers.get(header::CONTENT_TYPE).expect("content type"), "application/octet-stream");
    assert_eq!(headers.get(header::CACHE_CONTROL).expect("cache control"), "private, max-age=5");
    assert_eq!(to_bytes(response.into_body(), usize::MAX).await.expect("normalized body streams"), identity.as_slice());
}

#[tokio::test]
async fn direct_encoded_partial_content_is_rejected_before_client_response() {
    let encoded = zstd_encode(b"encoded-range").await;
    let origin = spawn_test_origin(
        "206 Partial Content",
        vec![
            ("Content-Encoding", "zstd"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Range", "bytes 2-5/16"),
        ],
        encoded,
    )
    .await;

    let result = fetch_direct_decoded(
        format!("{}/key.bin", origin.base_url),
        TransientResourceKind::Key,
        Some(HeaderValue::from_static("bytes=2-5")),
    )
    .await;

    assert!(matches!(result, Err(HlsOriginResourceFetchError::ContentCoding(_))));
    let requests = origin.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let request = requests[0].to_ascii_lowercase();
    assert!(request.contains("accept-encoding: identity"));
    assert!(request.contains("range: bytes=2-5"));
}

#[tokio::test]
async fn direct_identity_partial_content_preserves_consistent_range_headers() {
    let identity = b"part";
    let origin = spawn_test_origin(
        "206 Partial Content",
        vec![
            ("Content-Type", "application/octet-stream"),
            ("Content-Range", "bytes 2-5/16"),
            ("Accept-Ranges", "bytes"),
        ],
        identity.to_vec(),
    )
    .await;
    let decoded = fetch_direct_decoded(
        format!("{}/key.bin", origin.base_url),
        TransientResourceKind::Key,
        Some(HeaderValue::from_static("bytes=2-5")),
    )
    .await
    .expect("identity partial response is allowed");
    let response = direct_client_response(decoded, TransientResourceKind::Key);

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers().get(header::CONTENT_LENGTH).expect("content length"), "4");
    assert_eq!(response.headers().get(header::CONTENT_RANGE).expect("content range"), "bytes 2-5/16");
    assert_eq!(response.headers().get(header::ACCEPT_RANGES).expect("accept ranges"), "bytes");
    assert!(!should_compress_response(&response));
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.expect("identity partial body streams"),
        identity.as_slice()
    );
    assert_eq!(origin.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn direct_fetch_propagates_retry_attempt_and_identity_to_client_response() {
    let identity = b"successful-second-attempt";
    let origin =
        spawn_retry_then_body_origin(vec![("Content-Type", "application/octet-stream")], identity.to_vec()).await;
    let origin_url = format!("{}/segment.ts", origin.base_url);
    let decoded = fetch_direct_decoded(origin_url.clone(), TransientResourceKind::Segment, None)
        .await
        .expect("second origin attempt succeeds");

    assert_eq!(decoded.attempt.attempt_index, 1);
    assert_eq!(decoded.attempt.attempts, 5);

    let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url);
    let response = fixture.response(decoded);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.expect("retried response body streams"),
        identity.as_slice()
    );

    let requests = origin.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| request.to_ascii_lowercase().contains("accept-encoding: identity")));
}
