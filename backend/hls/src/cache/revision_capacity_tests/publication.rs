use super::*;

#[tokio::test]
async fn revision_peak_and_legacy_cache_share_the_global_and_session_quota() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    cache.update_cache_limits(1000, 500);
    let first = SegmentCacheKey::new(ProxySessionId("one".into()), 0, "ts");
    let other = SegmentCacheKey::new(ProxySessionId("two".into()), 0, "ts");
    let reservation = cache.reserve_revision_peak(&first, 400).await?;
    assert!(cache.reserve_revision_peak(&first, 200).await.is_err());
    let other_reservation = cache.reserve_revision_peak(&other, 400).await?;
    assert!(cache.write_bytes_and_commit(&first, &[1; 300]).await.is_err());
    drop(other_reservation);
    drop(reservation);
    cache.write_bytes_and_commit(&first, &[1; 300]).await?;
    assert_eq!(cache.capacity_usage(first.proxy_session_id()).await?.global_bytes, 300);
    Ok(())
}

#[tokio::test]
async fn processed_revision_retries_and_ranges_preserve_bytes_across_cache_replacement() -> io::Result<()> {
    use futures::StreamExt;
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    let legacy = SegmentCacheKey::new(ProxySessionId("range".into()), 0, "ts");
    cache.write_bytes_and_commit(&legacy, b"original-segment").await?;
    let store = crate::SegmentRevisionStore::default();
    let owner = store.create(legacy.proxy_session_id().clone(), 0, crate::SegmentRevisionKind::Processed)?;
    let pin = cache.pin_revision_file(&cache.object_path(&owner.revision().key))?;
    *owner.revision().file_pin.lock().map_err(|_| io::Error::other("file pin"))? = Some(pin);
    let metadata = cache
        .publish_processed_revision(
            &owner.revision().key,
            &cache.object_path(&legacy),
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .await?;
    owner.revision().complete(metadata);
    cache.write_bytes_and_commit(&legacy, b"longer-repaired-replacement").await?;
    for (start, end, expected) in [
        (0, None, b"original-segment".as_slice()),
        (3, Some(11), b"ginal-se".as_slice()),
        (0, None, b"original-segment".as_slice()),
    ] {
        let mut body =
            crate::progressive_startup::revision_body(owner.clone(), std::time::Duration::from_secs(5), start, end);
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        assert_eq!(bytes, expected);
    }
    assert_eq!(cache.delete_orphan_revision_files(SystemTime::now()).await?, 0);
    Ok(())
}
