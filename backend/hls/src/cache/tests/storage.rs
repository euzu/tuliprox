use super::*;

#[tokio::test]
async fn gc_style_delete_defers_while_the_object_has_an_active_mutation() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();
    cache.write_bytes_and_commit(&key, b"segment-body").await.expect("fixture commit");
    let cache_path = cache.cache_path_snapshot();
    let path = cache.object_path(&key);
    let reservation = cache
        .try_begin_capacity_mutation(&cache_path.path, &path, key.session_path_component())
        .expect("test mutation reserves");

    let error = cache.delete_if_inactive(&key).await.expect_err("active mutation protects object");

    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(cache.metadata(&key).await.expect("metadata").is_some());
    drop(reservation);
    cache.delete_if_inactive(&key).await.expect("delete resumes after mutation");
    assert!(cache.metadata(&key).await.expect("metadata").is_none());
}
