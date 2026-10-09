use super::*;

#[tokio::test]
async fn processed_http_retry_and_range_use_the_published_revision() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"original-segment".to_vec()).await;
    let _owner = publish_revision_fixture(&fixture, false).await?;
    let key = SegmentCacheKey::new(fixture.proxy_session_id.clone(), fixture.segment_file.proxy_seq, "ts");
    fixture.segment_cache.write_bytes_and_commit(&key, b"longer-repaired-replacement").await?;
    for (range, status, bytes) in [
        (None, StatusCode::OK, b"original-segment".as_slice()),
        (Some("bytes=3-10"), StatusCode::PARTIAL_CONTENT, b"ginal-se".as_slice()),
        (None, StatusCode::OK, b"original-segment".as_slice()),
    ] {
        let response = match fixture.serve(range).await {
            HlsResourceServeOutcome::Ready(response) => response,
            HlsResourceServeOutcome::Failure(failure) => return Err(std::io::Error::other(format!("{failure:?}"))),
        };
        assert_eq!(response.status(), status);
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).and_then(|value| value.to_str().ok()),
            Some(bytes.len().to_string().as_str())
        );
        assert_eq!(response.into_body().collect().await.map_err(std::io::Error::other)?.to_bytes().as_ref(), bytes);
    }
    Ok(())
}

#[test]
fn temporary_unavailable_response_uses_concrete_retry_after() {
    let response = super::super::hls_resource_failure_default_response(
        super::super::HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms: 2_500 },
    );

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry-after"), "3");
}
