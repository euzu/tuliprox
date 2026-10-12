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

#[tokio::test]
async fn known_length_revision_body_completes_when_dropped_after_final_byte() -> std::io::Result<()> {
    let fixture = CachedSegmentFixture::new(b"original-segment".to_vec()).await;
    let _owner = publish_revision_fixture(&fixture, false).await?;
    let response = match fixture.serve(None).await {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(failure) => return Err(std::io::Error::other(format!("{failure:?}"))),
    };
    assert_eq!(response.headers()[header::CONTENT_LENGTH], fixture.full_size.to_string());
    let mut body = response.into_body();
    let mut received = 0_u64;
    while received < fixture.full_size {
        let frame = body.frame().await.ok_or_else(|| std::io::Error::other("body ended early"))?;
        let data = frame.map_err(std::io::Error::other)?.into_data().map_err(|_| std::io::Error::other("frame"))?;
        received += u64::try_from(data.len()).map_err(std::io::Error::other)?;
    }
    // HTTP/1 stops polling a known-length body after its final byte and drops it.
    drop(body);

    let completed = fixture.wait_for_completion().await;
    assert_eq!(completed.highest_contiguous_completed_proxy_seq, Some(fixture.segment_file.proxy_seq));
    Ok(())
}
