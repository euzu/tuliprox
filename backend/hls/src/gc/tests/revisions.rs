use super::*;
use crate::{SegmentRevisionKind, SegmentRevisionStore};

#[tokio::test]
async fn diagnostic_inventory_errors_do_not_fail_capacity_reclamation() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let (gc, session) = gc_with_session(&directory).await;
    let id = session.read().await.proxy_session_id.clone();
    let key = crate::SegmentCacheKey::new(id.clone(), 1, "ts");
    assert_eq!(gc.cache.capacity_usage(&id).await?.global_bytes, 0);
    tokio::fs::write(gc.cache.object_path(&key).parent().unwrap(), b"not a directory").await?;
    assert!(gc.cache.session_revision_disk_bytes(&id).await.is_err());
    gc.log_capacity_inventory(&id, &GarbageCollectionReport::default()).await;
    let outcome = gc
        .reclaim_for_projected_write(crate::HlsCacheCapacityReclaimRequest {
            proxy_session_id: id.clone(),
            target_path: gc.cache.object_path(&key),
            required_session_bytes: 0,
            required_global_bytes: 0,
            staged_bytes: 1,
        })
        .await?;
    assert_eq!(outcome, crate::HlsCacheCapacityReclaimOutcome::default());
    let occupied = crate::SegmentCacheKey::new(ProxySessionId("occupied".into()), 1, "ts");
    gc.cache.write_bytes_and_commit(&occupied, &[1; 20]).await?;
    gc.cache.update_cache_limits(30, 30);
    let error = gc.cache.reserve_revision_peak_reclaiming(&key, 20).await.err().unwrap();
    assert!(
        crate::cache::hls_cache_capacity_from_io(&error).is_some(),
        "diagnostic I/O must preserve the functional capacity error"
    );
    assert_eq!(gc.cache.remaining_revision_bytes(&id), 0);
    Ok(())
}

#[tokio::test]
async fn global_reclamation_retires_revisions_unpinned_by_another_sessions_segment_deletion() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let (gc, old_session) = gc_with_session(&directory).await;
    let incoming = gc.sessions.get_or_create_session(HlsSessionKey::new(2, "incoming"), b"secret", 1).await;
    let old_id = old_session.read().await.proxy_session_id.clone();
    let incoming_id = incoming.read().await.proxy_session_id.clone();
    let store = Arc::new(SegmentRevisionStore::default());
    gc.install_revision_store(&store);
    let revision = store.create(old_id.clone(), 1, SegmentRevisionKind::Processed)?;
    let revision_key = revision.revision().key.clone();
    let metadata = gc.cache.write_bytes_and_commit(&revision_key, &[7; 20]).await?;
    revision.revision().complete(metadata);
    *revision.revision().file_pin.lock().unwrap() =
        Some(gc.cache.pin_revision_file(&gc.cache.object_path(&revision_key))?);
    let old_key = {
        let mut session = old_session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        session.publishable_origin_head_proxy_seq = Some(2);
        let head = session.segments.get_mut(&1).unwrap();
        head.origin_fetch_ref = None;
        head.status = SegmentCacheStatus::Ready { content_length: 20, ready_at_ms: 0 };
        let key = head.cache_key.clone();
        let config = tuliprox_core::model::HlsStartupConfig::default();
        session.startup = Some(crate::HlsSessionStartup {
            config: config.clone(),
            first_data_timeout_ms: 1_000,
            worker: std::sync::Weak::new(),
            access_leases: Arc::new(RwLock::new(HlsAccessLeaseStore::default())),
            revisions: [(1, revision)].into(),
            policy_fixed: true,
            store,
            budget: crate::ProgressiveBudgetManager::new(config),
        });
        key
    };
    gc.cache.write_bytes_and_commit(&old_key, &[1; 20]).await?;
    gc.cache.update_cache_limits(50, 50);
    assert_eq!(gc.cache.capacity_usage(&incoming_id).await?.global_bytes, 40);
    let key = crate::SegmentCacheKey::new(incoming_id.clone(), 1, "ts");
    let peak = Arc::new(
        tokio::time::timeout(Duration::from_secs(2), gc.cache.reserve_revision_peak_reclaiming(&key, 40))
            .await
            .expect("global reclamation must finish without a periodic GC")?,
    );
    assert!(gc.cache.metadata(&old_key).await?.is_none());
    assert!(gc.cache.metadata(&revision_key).await?.is_none());
    assert_eq!(gc.cache.capacity_usage(&old_id).await?.session_bytes, 0);
    assert!(old_session.read().await.startup.as_ref().unwrap().revisions.is_empty());
    let staged = gc
        .cache
        .stage_temp_with_budget(
            &key,
            &[9; 40][..],
            tokio::time::Instant::now() + Duration::from_secs(2),
            Some(Arc::clone(&peak)),
        )
        .await?;
    gc.cache.commit_staged(&key, staged).await?;
    drop(peak);
    assert_eq!(gc.cache.remaining_revision_bytes(&incoming_id), 0);
    assert_eq!(gc.cache.capacity_usage(&incoming_id).await?.global_bytes, 40);
    assert_eq!(tokio::fs::read(gc.cache.object_path(&key)).await?, [9; 40]);
    Ok(())
}

#[tokio::test]
async fn targeted_reclamation_deletes_only_unpinned_revisions_of_the_same_session() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let (gc, session) = gc_with_session(&directory).await;
    let id = session.read().await.proxy_session_id.clone();
    let store = Arc::new(SegmentRevisionStore::default());
    gc.install_revision_store(&store);
    let metrics = Arc::new(crate::HlsCacheMetrics::default());
    gc.cache.install_revision_metrics(&metrics);
    let revision = store.create(id.clone(), 0, SegmentRevisionKind::Processed)?;
    gc.cache.write_bytes_and_commit(&revision.revision().key, &[7; 80]).await?;
    let reader = revision.clone();
    drop(revision);
    gc.cache.update_cache_limits(100, 100);
    let next = crate::SegmentCacheKey::new(id.clone(), 1, "ts");
    assert!(gc.cache.reserve_revision_peak_reclaiming(&next, 30).await.is_err());
    assert_eq!(gc.cache.capacity_usage(&id).await?.session_bytes, 80);
    drop(reader);
    let peak = gc.cache.reserve_revision_peak_reclaiming(&next, 30).await?;
    assert_eq!(gc.cache.capacity_usage(&id).await?.session_bytes, 0);
    assert_eq!(gc.cache.revision_disk_bytes(), gc.cache.scan_revision_disk_bytes().await?);
    assert_eq!(metrics.snapshot().revision_disk_bytes, 0);
    drop(peak);
    assert_eq!(gc.cache.remaining_revision_bytes(&id), 0);
    assert_eq!(gc.run_once(0).await?.revision_deletions_succeeded, 0);
    Ok(())
}

#[tokio::test]
async fn failed_revision_deletes_preserve_bytes_and_are_retried_idempotently() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let (gc, session) = gc_with_session(&directory).await;
    let id = session.read().await.proxy_session_id.clone();
    let store = Arc::new(SegmentRevisionStore::default());
    gc.install_revision_store(&store);
    let revision = store.create(id.clone(), 0, SegmentRevisionKind::Processed)?;
    let key = revision.revision().key.clone();
    gc.cache.write_bytes_and_commit(&key, &[7; 80]).await?;
    gc.cache.fail_next_cache_deletes(gc.cache.object_path(&key), 2);
    drop(revision);
    let first = gc.run_once(0).await?;
    assert_eq!(first.revision_deletions_failed, 2);
    assert_eq!(first.revision_bytes_reclaimed, 0);
    assert_eq!(gc.cache.capacity_usage(&id).await?.session_bytes, 80);
    assert_eq!(gc.cache.revision_disk_bytes(), gc.cache.scan_revision_disk_bytes().await?);
    let next = gc.run_once(0).await?;
    assert_eq!(next.revision_deletions_succeeded, 1);
    assert_eq!(next.revision_bytes_reclaimed, 80);
    assert!(!tokio::fs::try_exists(gc.cache.object_path(&key)).await?);
    assert_eq!(gc.run_once(0).await?.revision_deletions_planned, 0);
    Ok(())
}

#[tokio::test]
async fn obsolete_deferred_fifo_head_retires_without_skipping_a_protected_head() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let (gc, session) = gc_with_session(&directory).await;
    let key = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let head = session.segments.get_mut(&1).unwrap();
        head.status =
            SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Demand, deferred_at_ms: 1 };
        head.origin_fetch_ref = None;
        head.access.reader_started(1);
        head.cache_key.clone()
    };
    gc.cache.write_bytes_and_commit(&key, b"partial-commit").await?;
    let first = gc.run_once(0).await?;
    assert_eq!(first.segments_deleted(), 0);
    assert!(session.read().await.segments.contains_key(&1));
    session.write().await.segments.get_mut(&1).unwrap().access.reader_finished();
    let second = gc.run_once(0).await?;
    assert_eq!(second.segments_deleted(), 1);
    assert!(!session.read().await.segments.contains_key(&1));
    assert!(gc.cache.metadata(&key).await?.is_none());
    assert_eq!(gc.run_once(0).await?.segments_deleted(), 0);
    Ok(())
}

#[tokio::test]
async fn revision_cleanup_skips_contended_locks_and_pressure_keeps_protection_unknown() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let (gc, session) = gc_with_session(&directory).await;
    let id = session.read().await.proxy_session_id.clone();
    let leases = Arc::new(tokio::sync::RwLock::new(crate::HlsAccessLeaseStore::default()));
    gc.install_access_leases(&leases);
    let lease_guard = leases.write().await;
    let mut report = GarbageCollectionReport::default();
    tokio::time::timeout(Duration::from_secs(2), gc.cleanup_revisions(&[Arc::clone(&session)], &mut report))
        .await
        .expect("revision cleanup must not wait while the lease lock is held")?;
    assert_eq!(report.revision_lock_conflicts, 1);
    drop(lease_guard);

    let key = crate::SegmentCacheKey::new(id.clone(), 0, "ts");
    gc.cache.write_bytes_and_commit(&key, &[7; 80]).await?;
    let session_guard = session.write().await;
    let mut report = GarbageCollectionReport::default();
    tokio::time::timeout(Duration::from_secs(2), gc.cleanup_revisions(&[Arc::clone(&session)], &mut report))
        .await
        .expect("revision cleanup must not reverse the lease/session lock order")?;
    assert_eq!(report.revision_lock_conflicts, 1);
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        gc.reclaim_for_projected_write(crate::HlsCacheCapacityReclaimRequest {
            proxy_session_id: id.clone(),
            target_path: gc.cache.object_path(&key),
            required_session_bytes: 1,
            required_global_bytes: 0,
            staged_bytes: 1,
        }),
    )
    .await
    .expect("targeted selection must not wait for a contended session")?;
    assert_eq!(outcome.protected_working_set_bytes, 0, "a lock conflict cannot claim all 80 bytes are protected");
    assert_eq!(outcome.reclaimed_session_bytes, 0);
    assert_eq!(gc.cache.capacity_usage(&id).await?.session_bytes, 80);
    drop(session_guard);
    Ok(())
}

#[tokio::test]
async fn shared_revision_cleanup_retires_a_bounded_batch_and_drains_idempotently() -> std::io::Result<()> {
    let directory = tempfile::tempdir()?;
    let (gc, session) = gc_with_session(&directory).await;
    let id = session.read().await.proxy_session_id.clone();
    let store = Arc::new(SegmentRevisionStore::default());
    gc.install_revision_store(&store);
    let limit = super::super::MAX_CACHE_DELETE_RETRIES_PER_RUN;
    for seq in 0..limit + 3 {
        let revision = store.create(id.clone(), u64::try_from(seq).unwrap(), SegmentRevisionKind::Processed)?;
        gc.cache.write_bytes_and_commit(&revision.revision().key, &[7]).await?;
    }
    for expected in [limit, 3, 0] {
        let mut report = GarbageCollectionReport::default();
        gc.cleanup_revisions(&[Arc::clone(&session)], &mut report).await?;
        assert_eq!(report.revision_deletions_planned, expected);
        assert_eq!(report.revision_deletions_succeeded, expected);
        assert_eq!(report.revision_deletions_failed, 0);
        assert_eq!(report.revision_bytes_reclaimed, u64::try_from(expected).unwrap());
        assert_eq!(gc.cache.revision_disk_bytes(), gc.cache.scan_revision_disk_bytes().await?);
    }
    assert_eq!(gc.cache.capacity_usage(&id).await?.session_bytes, 0);
    assert_eq!(gc.cache.remaining_revision_bytes(&id), 0);
    Ok(())
}
