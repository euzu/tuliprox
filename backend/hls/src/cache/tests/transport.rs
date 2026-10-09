use super::*;

#[test]
fn cache_key_debug_redacts_proxy_session_id() {
    let key = SegmentCacheKey::new(ProxySessionId("secretToken".to_string()), 123, "ts");
    let debug = format!("{key:?}");

    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("secretToken"));
    assert!(!debug.contains(&key.stable_value()));
}

#[test]
fn transient_object_cache_key_keeps_redirect_hosts_distinct_without_leaking_urls() {
    let proxy_session_id = ProxySessionId("proxy_session".to_string());
    let first_resource = build_transient_resource_id("https://cdn-a.example.net/live/redirected/seg001.ts", b"secret");
    let second_resource = build_transient_resource_id("https://cdn-b.example.net/live/redirected/seg001.ts", b"secret");

    let first = TransientObjectCacheKey::new(proxy_session_id.clone(), first_resource, "ts");
    let second = TransientObjectCacheKey::new(proxy_session_id, second_resource, "ts");

    assert_ne!(first, second);
    assert_ne!(first.stable_value(), second.stable_value());
    for value in [first.stable_value(), second.stable_value()] {
        assert!(!value.contains("provider://"));
        assert!(!value.contains("cdn-a.example.net"));
        assert!(!value.contains("cdn-b.example.net"));
        assert!(!value.contains("/live/redirected/seg001.ts"));
    }
}

#[tokio::test]
async fn cache_object_write_rejects_bytes_above_the_configured_limit() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    cache.update_cache_limits(3, 3);
    let key = SegmentCacheKey::new(ProxySessionId("session".to_string()), 1, "ts");

    let result = cache.write_bytes_and_commit(&key, b"four").await;

    let error = result.expect_err("decoded object above the configured limit must fail");
    let limit_error = hls_cache_object_limit_from_io(&error).expect("typed object-limit source");
    assert_eq!(limit_error.limit(), 3);
    assert!(matches!(
        HlsOriginResourceFetchError::cache_body(&error),
        HlsOriginResourceFetchError::CacheObjectLimit { limit: 3 }
    ));
    assert!(cache.metadata(&key).await.expect("metadata").is_none());
    assert!(!cache.has_active_temp_files());
}

#[tokio::test]
async fn cache_commits_enforce_the_global_budget_across_sessions() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    cache.update_cache_limits(5, 5);
    let first = SegmentCacheKey::new(ProxySessionId("first".to_string()), 1, "ts");
    let second = SegmentCacheKey::new(ProxySessionId("second".to_string()), 1, "ts");

    assert!(cache.write_bytes_and_commit(&first, b"123").await.is_ok());
    let error = cache.write_bytes_and_commit(&second, b"456").await.expect_err("global limit rejects commit");
    let capacity = hls_cache_capacity_from_io(&error).expect("typed local capacity error");
    assert_eq!(capacity.required_session_bytes(), 0);
    assert_eq!(capacity.required_global_bytes(), 1);
    assert!(cache.metadata(&second).await.expect("metadata").is_none());
    assert!(!cache.has_active_temp_files());
    let first_usage = cache.capacity_usage(first.proxy_session_id()).await.expect("usage");
    assert_eq!(first_usage.global_bytes, 3);
    assert_eq!(first_usage.session_bytes, 3);
}
