use super::*;

#[tokio::test]
async fn preserve_origin_rejects_invalid_representation_header_before_stream_release() {
    let (response, requests) =
        local_response(StatusCode::OK, &[("Content-Encoding", "gzip,,br")], b"body must not be released".to_vec())
            .await;

    assert!(matches!(
        prepare_provider_stream_response(
            response,
            ProviderContentRepresentationMode::PreserveOrigin,
            ProviderResponseHeadAvailability::Available,
        )
        .await,
        Err(ProviderStreamPreparationError::ResponseHeader(ProviderResponseHeaderError::InvalidContentEncoding))
    ));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn legacy_hls_identity_decoding_removes_stale_representation_headers() {
    const IDENTITY_BODY: &[u8] = b"decoded identity response";
    let encoded = encode(IDENTITY_BODY, TestEncoding::Gzip).await;
    let (response, _) =
        local_response(StatusCode::OK, &[("Content-Encoding", "gzip"), ("Content-Range", "bytes 0-9/10")], encoded)
            .await;

    let ProviderStreamFactoryResponse { stream, info, .. } = prepare_provider_stream_response(
        response,
        ProviderContentRepresentationMode::Identity,
        ProviderResponseHeadAvailability::Available,
    )
    .await
    .expect("encoded full response decodes");
    let (headers, status, _, _) = info.expect("decoded response metadata");

    assert_eq!(collect_stream(stream).await.expect("decoded body streams"), IDENTITY_BODY);
    assert_eq!(status, StatusCode::OK);
    for name in ["content-encoding", "content-length", "content-range"] {
        assert!(headers.iter().all(|(key, _)| key != name), "stale header {name}");
    }
}
