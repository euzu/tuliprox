use super::*;

#[tokio::test]
async fn open_range_reads_from_requested_offset() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();
    cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");

    let file = cache.open_range(&key, 4).await.expect("range should open");
    let mut body = Vec::new();
    file.take(3).read_to_end(&mut body).await.expect("range should read");

    assert_eq!(body, b"456");
}
