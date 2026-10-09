use super::*;

#[tokio::test]
async fn cache_root_handoff_invalidates_armed_switch_cleanup_reservations() {
    let old_root = tempfile::tempdir().expect("old cache root");
    let new_root = tempfile::tempdir().expect("new cache root");
    let (gc, session) = gc_with_session(&old_root).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let key = crate::SegmentCacheKey::new(proxy_session_id, 1, "ts");
    let cleanup = gc.reserve_switch_segment_cleanup(key.clone()).expect("switch cleanup reservation");
    assert_eq!(gc.cache_deletion_queue_usage(), (0, 1));

    assert!(gc.update_cache_path(new_root.path()).await);
    assert_eq!(gc.cache_deletion_queue_usage(), (0, 0));
    drop(cleanup);
    assert_eq!(gc.cache_deletion_queue_usage(), (0, 0));

    gc.cache.write_bytes_and_commit(&key, b"new-root-sentinel").await.expect("new-root sentinel writes");
    let report = gc.run_once(0).await.expect("new-root gc runs");

    assert_eq!(report.cache_object_deletions_succeeded, 0);
    let metadata = gc.cache.metadata(&key).await.expect("new-root metadata reads").expect("sentinel remains");
    assert_eq!(tokio::fs::read(metadata.path).await.expect("sentinel reads"), b"new-root-sentinel");
}

#[tokio::test]
async fn concurrent_projected_pressure_is_serialized_without_over_deletion() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
    }
    cache_selected_ready_segments(&gc, &session, &[1, 2], b"0123456789").await;
    gc.cache.update_cache_limits(100, 25);
    let (third, fourth, proxy_session_id) = {
        let session = session.read().await;
        (
            session.segments.get(&3).expect("third").cache_key.clone(),
            session.segments.get(&4).expect("fourth").cache_key.clone(),
            session.proxy_session_id.clone(),
        )
    };
    let first_staged = gc
        .cache
        .stage_temp_with_deadline(&third, &b"abcdefghij"[..], tokio::time::Instant::now() + Duration::from_mins(1))
        .await
        .expect("first object stages");
    let second_staged = gc
        .cache
        .stage_temp_with_deadline(&fourth, &b"klmnopqrst"[..], tokio::time::Instant::now() + Duration::from_mins(1))
        .await
        .expect("second object stages");
    let commit_barrier = Arc::new(Barrier::new(2));
    let first_cache = Arc::clone(&gc.cache);
    let first_barrier = Arc::clone(&commit_barrier);
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_cache.commit_staged(&third, first_staged).await
    });
    let second_cache = Arc::clone(&gc.cache);
    let second = tokio::spawn(async move {
        commit_barrier.wait().await;
        second_cache.commit_staged(&fourth, second_staged).await
    });

    let (first, second) = tokio::join!(first, second);

    let first = first.expect("first task");
    let second = second.expect("second task");

    assert!(first.is_ok(), "first cache write failed: {first:?}");
    assert!(second.is_ok(), "second cache write failed: {second:?}");
    let session = session.read().await;
    assert!(!session.segments.contains_key(&1));
    assert!(!session.segments.contains_key(&2));
    assert!(session.segments.contains_key(&3));
    assert!(session.segments.contains_key(&4));
    drop(session);
    let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
    assert_eq!(usage.session_bytes, 20);
    assert!(usage.global_bytes <= 25);
}
