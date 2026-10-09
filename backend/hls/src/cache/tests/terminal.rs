use super::*;

#[tokio::test]
async fn exhausted_write_deadline_rejects_even_an_immediately_available_body() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();

    let result = cache.write_temp_and_commit_with_deadline(&key, &b"ready"[..], tokio::time::Instant::now()).await;

    assert_eq!(result.expect_err("expired deadline must time out").kind(), io::ErrorKind::TimedOut);
    assert!(!cache.has_active_temp_files());
    assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
    assert_eq!(std::fs::read_dir(temp_dir.path()).expect("cache root reads").count(), 0);
}
