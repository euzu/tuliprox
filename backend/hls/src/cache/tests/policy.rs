use super::*;

#[tokio::test]
async fn write_temp_and_commit_creates_final_segment_cache_file_with_proxy_layout() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();

    let metadata = cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

    assert_eq!(metadata.size, 12);
    assert!(metadata.path.exists());
    assert_eq!(cache.metadata(&key).await.expect("metadata should read"), Some(metadata.clone()));
    assert!(metadata.path.ends_with("proxy_session/000123.ts"));
}

#[tokio::test]
async fn write_temp_and_commit_creates_final_map_cache_file_with_proxy_layout() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = MapCacheKey::new(ProxySessionId("proxy_session".to_string()), 0, "mp4");

    let metadata = cache.write_bytes_and_commit(&key, b"map-body").await.expect("commit should succeed");

    assert_eq!(metadata.size, 8);
    assert!(metadata.path.ends_with("proxy_session/map/000000.mp4"));
    assert_eq!(cache.metadata(&key).await.expect("metadata should read"), Some(metadata));
}

#[tokio::test]
async fn write_temp_and_commit_with_deadline_cleans_active_temp_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();
    let (_writer, reader) = tokio::io::duplex(64);

    let result = cache
        .write_temp_and_commit_with_deadline(&key, reader, tokio::time::Instant::now() + Duration::from_millis(1))
        .await;

    assert_eq!(result.expect_err("commit should time out").kind(), io::ErrorKind::TimedOut);
    assert!(!cache.has_active_temp_files());
    assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
}

#[tokio::test]
async fn temp_file_collision_does_not_overwrite_existing_temp_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();
    let parent = temp_dir.path().join("proxy_session");
    tokio::fs::create_dir_all(&parent).await.expect("parent should be created");
    let existing_temp = parent.join("000123.ts.tmp.0000000000000000");
    tokio::fs::write(&existing_temp, b"existing").await.expect("temp fixture should write");

    cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

    assert_eq!(tokio::fs::read(&existing_temp).await.expect("existing temp should remain"), b"existing");
}

#[tokio::test]
async fn invalidate_all_if_no_active_temp_files_deletes_only_when_no_temp_write_is_active() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let committed_key = cache_key();
    cache.write_bytes_and_commit(&committed_key, b"segment-body").await.expect("commit should succeed");

    let outcome = cache.invalidate_all_if_no_active_temp_files().await.expect("invalidation should succeed");

    assert_eq!(outcome, CacheInvalidationOutcome::Invalidated);
    assert_eq!(cache.metadata(&committed_key).await.expect("metadata should read"), None);

    cache.write_bytes_and_commit(&committed_key, b"segment-body").await.expect("second commit should succeed");
    let active_key = SegmentCacheKey::new(ProxySessionId("proxy_session".to_string()), 124, "ts");
    let staged = cache
        .stage_temp_with_deadline(&active_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
        .await
        .expect("active object stages");
    assert!(cache.has_active_temp_files());

    let outcome = cache.invalidate_all_if_no_active_temp_files().await.expect("deferred invalidation should succeed");

    assert_eq!(outcome, CacheInvalidationOutcome::DeferredActiveTempFiles);
    assert!(cache.metadata(&committed_key).await.expect("metadata should read").is_some());
    cache.commit_staged(&active_key, staged).await.expect("staged object commits");
}

#[tokio::test]
async fn invalidate_all_refuses_unmarked_cache_root() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let unrelated_file = temp_dir.path().join("unrelated");
    tokio::fs::write(&unrelated_file, b"keep").await.expect("fixture should write");

    let err = cache.invalidate_all().await.expect_err("unmarked root should be refused");

    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(unrelated_file.exists());
}
