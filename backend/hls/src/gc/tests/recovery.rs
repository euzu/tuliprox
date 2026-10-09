use super::*;

#[tokio::test]
async fn full_retry_queue_preserves_session_metadata_until_a_slot_is_reserved() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        session.segments.get_mut(&1).expect("segment").status =
            SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
    }
    {
        let mut queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for proxy_seq in 0..MAX_PENDING_CACHE_DELETIONS {
            queue.pending.push_back(PendingCacheObjectDeletion {
                deletion: CacheObjectDeletion::Segment {
                    key: crate::SegmentCacheKey::new(
                        crate::ProxySessionId("queued".to_string()),
                        u64::try_from(proxy_seq).unwrap_or_default(),
                        "ts",
                    ),
                    reason: SegmentCacheDeletionReason::Duration,
                },
                attempts: 0,
            });
        }
    }
    let mut report = GarbageCollectionReport::default();
    let policy = gc.policy();
    let mut batch = gc.reserve_cache_deletion_batch();
    assert_eq!(batch.remaining_capacity(), 0);

    {
        let mut session = session.write().await;
        HlsGarbageCollector::collect_session_deletions(
            &mut session,
            &gc.cache,
            10_000,
            &policy,
            &mut report,
            &mut batch,
        );
        assert!(session.segments.contains_key(&1));
    }
    batch.persist(&mut report);

    assert_eq!(report.cache_object_deletions_planned, 0);
    assert_eq!(gc.pending_cache_deletion_count(), MAX_PENDING_CACHE_DELETIONS);
}

#[tokio::test]
async fn dropping_unpersisted_deletion_batch_keeps_reserved_deletion_retryable() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, _) = gc_with_session(&temp_dir).await;
    let key = crate::SegmentCacheKey::new(crate::ProxySessionId("cancelled".to_string()), 1, "ts");
    {
        let mut batch = gc.reserve_cache_deletion_batch();
        batch.push(CacheObjectDeletion::Segment { key: key.clone(), reason: SegmentCacheDeletionReason::Duration });
    }

    let queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(queue.reserved_slots, 0);
    assert_eq!(queue.pending.len(), 1);
    assert_eq!(
        queue.pending.front().map(|pending| &pending.deletion),
        Some(&CacheObjectDeletion::Segment { key, reason: SegmentCacheDeletionReason::Duration })
    );
}

#[tokio::test]
async fn failed_switch_rollback_blocks_same_key_until_gc_retry_succeeds() {
    let cache_root = tempfile::tempdir().expect("cache root");
    let (gc, session) = gc_with_session(&cache_root).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let key = crate::SegmentCacheKey::new(proxy_session_id, 1, "ts");
    gc.cache.write_bytes_and_commit(&key, b"uncommitted-switch").await.expect("switch fixture writes");
    let object_path = gc.cache.object_path(&key);
    tokio::fs::remove_file(&object_path).await.expect("switch fixture file removes");
    tokio::fs::create_dir(&object_path).await.expect("directory forces remove-file failure");
    let cleanup = gc.reserve_switch_segment_cleanup(key.clone()).expect("switch cleanup reservation");
    drop(cleanup);

    assert!(gc.has_pending_switch_cleanup(&key, None));
    let deferred = gc.run_once(0).await.expect("deferred rollback GC runs");
    assert_eq!(deferred.cache_object_deletions_succeeded, 0);
    assert_eq!(deferred.cache_object_deletions_deferred, 1);
    assert_eq!(gc.cache_deletion_queue_usage(), (1, 0));
    assert!(gc.has_pending_switch_cleanup(&key, None));

    tokio::fs::remove_dir(&object_path).await.expect("failing directory removes");
    tokio::fs::write(&object_path, b"uncommitted-switch").await.expect("retry fixture writes");
    let retried = gc.run_once(1).await.expect("rollback retry GC runs");

    assert_eq!(retried.cache_object_deletions_succeeded, 1);
    assert_eq!(gc.cache_deletion_queue_usage(), (0, 0));
    assert!(!gc.has_pending_switch_cleanup(&key, None));
    assert!(gc.cache.metadata(&key).await.expect("rolled-back metadata reads").is_none());
}
