use super::*;

#[test]
fn temp_registration_and_last_owner_release_advance_inventory_revision() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    let initial = Arc::clone(&cache.temp_files.lock().unwrap().revision);
    let registration = cache.register_active_temp_path(&directory.path().join("segment.ts.tmp.test"))?;
    let registered = Arc::clone(&cache.temp_files.lock().unwrap().revision);
    assert!(!Arc::ptr_eq(&initial, &registered));
    let last_owner = Arc::clone(&registration);
    drop(registration);
    assert!(Arc::ptr_eq(&registered, &cache.temp_files.lock().unwrap().revision));
    drop(last_owner);
    assert!(!Arc::ptr_eq(&registered, &cache.temp_files.lock().unwrap().revision));
    assert!(cache.temp_files.lock().unwrap().active_files.is_empty());
    Ok(())
}

#[tokio::test]
async fn attaching_a_budget_to_shared_staging_returns_an_error_and_preserves_its_owner() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    let key = SegmentCacheKey::new(ProxySessionId("shared-staging".into()), 0, "ts");
    let budget = Arc::new(cache.reserve_revision_peak(&key, 20).await?);
    let staged = cache
        .stage_temp_with_budget(
            &key,
            b"test".as_slice(),
            Instant::now() + std::time::Duration::from_secs(2),
            Some(Arc::clone(&budget)),
        )
        .await?;
    let error = staged.clone().with_write_budget(None).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(tokio::fs::read(&staged.path).await?, b"test");
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 20);
    cache.commit_staged(&key, staged).await?;
    drop(budget);
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 0);
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.global_bytes, 4);
    Ok(())
}

#[tokio::test]
async fn inventory_scan_retries_when_a_reserved_temp_file_is_registered_after_its_snapshot() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = Arc::new(HlsSegmentCache::with_cache_path(directory.path()));
    cache.update_cache_limits(60, 60);
    let key = SegmentCacheKey::new(ProxySessionId("inventory-race".into()), 0, "ts");
    cache.write_bytes_and_commit(&key, &[1; 20]).await?;
    let budget = Arc::new(cache.reserve_revision_peak(&key, 20).await?);
    cache.invalidate_capacity_accounting();
    let (reached, resume) = cache.pause_startup_write(StartupWritePhase::AfterInventorySnapshot);
    let scan_cache = Arc::clone(&cache);
    let id = key.proxy_session_id().clone();
    let scan = tokio::spawn(async move { scan_cache.capacity_usage(&id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), reached).await.unwrap().unwrap();
    let staged = cache
        .stage_temp_with_budget(
            &key,
            &[2; 20][..],
            Instant::now() + std::time::Duration::from_secs(2),
            Some(Arc::clone(&budget)),
        )
        .await?;
    resume.send(()).unwrap();
    let usage = tokio::time::timeout(std::time::Duration::from_secs(2), scan).await.unwrap().unwrap()?;
    cache.remove_staged(staged).await?;
    drop(budget);
    assert_eq!(usage.global_bytes, 20, "reserved staging bytes must not enter the committed inventory");
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 20);
    let next = cache.reserve_revision_peak(&key, 30).await?;
    drop(next);
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 0);
    Ok(())
}

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

// These are the two real disk-cache transitions from debug/bug.log. Sparse
// fixtures preserve the exact byte counts without allocating a GiB of RAM/disk.
#[tokio::test]
async fn logged_normal_commit_consumes_its_own_peak_reservation() -> io::Result<()> {
    logged_peak_case(9_994_832).await
}

#[tokio::test]
async fn logged_revision_publication_consumes_its_remaining_peak_reservation() -> io::Result<()> {
    logged_peak_case(8_052_416).await
}

async fn logged_peak_case(size: u64) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    cache.update_cache_limits(1_073_741_824, 1_073_741_824);
    let key = SegmentCacheKey::new(ProxySessionId("logged-session".into()), 84, "ts");
    let existing = SegmentCacheKey::new(key.proxy_session_id().clone(), 0, "ts");
    let path = cache.object_path(&existing);
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let file = tokio::fs::File::create(path).await?;
    file.set_len(1_040_683_024).await?;
    drop(file);
    let reservation = std::sync::Arc::new(cache.reserve_revision_peak(&key, 3 * size).await?);
    let staged_path = cache.object_path(&key).with_extension("ts.tmp.regression");
    let file = tokio::fs::File::create(&staged_path).await?;
    file.set_len(size).await?;
    drop(file);
    let staged = cache.adopt_staged_file(staged_path, size)?.with_write_budget(Some(reservation.clone()))?;
    let committed = cache.commit_staged(&key, staged).await?;
    let store = crate::SegmentRevisionStore::default();
    let revision = store.create(key.proxy_session_id().clone(), 84, crate::SegmentRevisionKind::Processed)?;
    let published = cache
        .publish_processed_revision_with_budget(
            &revision.revision().key,
            &committed.path,
            Instant::now() + std::time::Duration::from_secs(5),
            Some(reservation.clone()),
        )
        .await?;
    assert_eq!(published.size, size);
    drop(reservation);
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 1_040_683_024 + 2 * size);
    assert_eq!(cache.revision_disk_bytes(), cache.scan_revision_disk_bytes().await?);
    Ok(())
}

#[tokio::test]
async fn processed_reservation_release_wakes_before_and_after_wait_registration() -> io::Result<()> {
    for registered in [false, true] {
        let directory = tempfile::tempdir()?;
        let cache = HlsSegmentCache::with_cache_path(directory.path());
        let key = SegmentCacheKey::new(ProxySessionId("release".into()), 0, "ts");
        let reservation = cache.reserve_revision_peak(&key, 30).await?;
        assert!(!reservation.unaccounted_writes);
        let revision = cache.capacity_revision();
        let waiting = cache.wait_for_capacity_change(&revision);
        tokio::pin!(waiting);
        if registered {
            assert!(futures::poll!(waiting.as_mut()).is_pending());
        }
        drop(reservation);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("releasing only a processed reservation must wake the waiter");
    }
    Ok(())
}

#[tokio::test]
async fn concurrent_peak_owners_cannot_consume_each_others_session_or_global_budget() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = std::sync::Arc::new(HlsSegmentCache::with_cache_path(directory.path()));
    cache.update_cache_limits(180, 120);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(4));
    let mut tasks = Vec::new();
    for (session, seq) in [("one", 0), ("one", 1), ("two", 0)] {
        let cache = cache.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let key = SegmentCacheKey::new(ProxySessionId(session.into()), seq, "ts");
            let peak = std::sync::Arc::new(cache.reserve_revision_peak(&key, 60).await?);
            barrier.wait().await;
            let staged = cache
                .stage_temp_with_budget(
                    &key,
                    &[7; 20][..],
                    Instant::now() + std::time::Duration::from_secs(2),
                    Some(peak.clone()),
                )
                .await?;
            let metadata = cache.commit_staged(&key, staged).await?;
            let store = crate::SegmentRevisionStore::default();
            let owner = store.create(key.proxy_session_id().clone(), seq, crate::SegmentRevisionKind::Processed)?;
            cache
                .publish_processed_revision_with_budget(
                    &owner.revision().key,
                    &metadata.path,
                    Instant::now() + std::time::Duration::from_secs(2),
                    Some(peak),
                )
                .await?;
            Ok::<_, io::Error>(())
        }));
    }
    barrier.wait().await;
    for task in tasks {
        task.await??;
    }
    let usage = cache.capacity_usage(&ProxySessionId("one".into())).await?;
    assert_eq!(usage, HlsCacheCapacityUsage { session_bytes: 80, global_bytes: 120 });
    assert_eq!(cache.remaining_revision_bytes(&ProxySessionId("one".into())), 0);
    assert_eq!(cache.remaining_revision_bytes(&ProxySessionId("two".into())), 0);
    Ok(())
}

#[tokio::test]
async fn old_write_cleanup_cannot_delete_a_newer_fetch_attempt() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    let key = SegmentCacheKey::new(ProxySessionId("replace".into()), 0, "ts");
    let old = std::sync::Arc::new(cache.reserve_revision_peak(&key, 30).await?);
    let staged = cache
        .stage_temp_with_budget(
            &key,
            b"old".as_slice(),
            Instant::now() + std::time::Duration::from_secs(2),
            Some(old.clone()),
        )
        .await?;
    cache.commit_staged(&key, staged).await?;
    let new = std::sync::Arc::new(cache.reserve_revision_peak(&key, 30).await?);
    let staged = cache
        .stage_temp_with_budget(
            &key,
            b"new".as_slice(),
            Instant::now() + std::time::Duration::from_secs(2),
            Some(new.clone()),
        )
        .await?;
    cache.commit_staged(&key, staged).await?;
    let store = crate::SegmentRevisionStore::default();
    let revision = store.create(key.proxy_session_id().clone(), 0, crate::SegmentRevisionKind::Processed)?;
    let error = cache
        .publish_processed_revision_with_budget(
            &revision.revision().key,
            &cache.object_path(&key),
            Instant::now() + std::time::Duration::from_secs(2),
            Some(old.clone()),
        )
        .await
        .expect_err("an old startup attempt cannot publish a replacement owned by a newer attempt");
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(cache.metadata(&revision.revision().key).await?.is_none());
    cache.delete_write_owned(&key, &old).await?;
    assert_eq!(tokio::fs::read(cache.object_path(&key)).await?, b"new");
    cache.delete_write_owned(&key, &new).await?;
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 0);
    Ok(())
}

#[tokio::test]
async fn pinned_revisions_remain_accounted_after_inventory_reconciliation_and_deletion() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    let metrics = std::sync::Arc::new(crate::HlsCacheMetrics::default());
    cache.install_revision_metrics(&metrics);
    let key = SegmentCacheKey::new(ProxySessionId("pinned-scan".into()), 0, "ts");
    cache.write_bytes_and_commit(&key, &[1; 20]).await?;
    let store = crate::SegmentRevisionStore::default();
    let owner = store.create(key.proxy_session_id().clone(), 0, crate::SegmentRevisionKind::Processed)?;
    let pin = cache.pin_revision_file(&cache.object_path(&owner.revision().key))?;
    *owner.revision().file_pin.lock().map_err(|_| io::Error::other("file pin"))? = Some(pin);
    cache
        .publish_processed_revision(
            &owner.revision().key,
            &cache.object_path(&key),
            Instant::now() + std::time::Duration::from_secs(2),
        )
        .await?;
    drop(cache.capacity_invalidation_guard());
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 40);
    assert_eq!(metrics.snapshot().revision_disk_bytes, cache.scan_revision_disk_bytes().await?);
    drop(owner);
    cache.delete_session_dir(key.proxy_session_id()).await?;
    assert_eq!(metrics.snapshot().revision_disk_bytes, 0);
    assert_eq!(cache.scan_revision_disk_bytes().await?, 0);
    Ok(())
}

#[tokio::test]
async fn failed_partial_commit_cleanup_retries_without_deleting_a_replacement() -> io::Result<()> {
    for replace in [false, true] {
        let directory = tempfile::tempdir()?;
        let cache = HlsSegmentCache::with_cache_path(directory.path());
        let key = SegmentCacheKey::new(ProxySessionId("cleanup-retry".into()), 0, "ts");
        let old = std::sync::Arc::new(cache.reserve_revision_peak(&key, 30).await?);
        let staged = cache
            .stage_temp_with_budget(
                &key,
                b"old".as_slice(),
                Instant::now() + std::time::Duration::from_secs(2),
                Some(old.clone()),
            )
            .await?;
        cache.commit_staged(&key, staged).await?;
        cache.fail_next_cache_deletes(cache.object_path(&key), 1);
        assert!(cache.delete_write_owned(&key, &old).await.is_err());
        drop(old);
        assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 0);
        assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 3);
        if replace {
            cache.write_bytes_and_commit(&key, b"new").await?;
        }
        cache.retry_abandoned_write_deletions().await;
        if replace {
            assert_eq!(tokio::fs::read(cache.object_path(&key)).await?, b"new");
        } else {
            assert!(cache.metadata(&key).await?.is_none());
            assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 0);
        }
        cache.retry_abandoned_write_deletions().await;
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_staging_keeps_its_peak_until_pending_file_io_is_joined() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    cache.update_cache_limits(20, 20);
    let key = SegmentCacheKey::new(ProxySessionId("pending-io".into()), 0, "ts");
    let budget = std::sync::Arc::new(cache.reserve_revision_peak(&key, 20).await?);
    let path = cache.object_path(&key).with_extension("ts.tmp.pending");
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let staged = cache.adopt_staged_file(path.clone(), 0)?.with_write_budget(Some(budget.clone()))?;
    let writer = staged.track_writer(tokio::fs::File::create(&path).await?);
    let mut in_flight_io = writer.lock().await;
    in_flight_io.as_mut().unwrap().write_all(b"pending").await?;
    drop(budget);
    let revision = cache.capacity_revision();
    drop(staged); // Models cancellation while the producer still owns its I/O handle.
    let released = cache.wait_for_capacity_change(&revision);
    tokio::pin!(released);
    assert!(futures::poll!(released.as_mut()).is_pending());
    assert!(cache.reserve_revision_peak(&key, 1).await.is_err());
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 20);
    drop(in_flight_io);
    tokio::time::timeout(std::time::Duration::from_secs(2), released).await.unwrap();
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 0);
    assert!(!tokio::fs::try_exists(path).await?);
    assert!(!cache.has_active_temp_files());
    Ok(())
}

#[tokio::test]
async fn cancelled_revision_link_keeps_its_peak_until_the_owned_link_and_unlink_finish() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    cache.update_cache_limits(20, 20);
    let key = SegmentCacheKey::new(ProxySessionId("pending-link".into()), 0, "ts");
    let budget = std::sync::Arc::new(cache.reserve_revision_peak(&key, 20).await?);
    let staged = cache
        .stage_temp_with_budget(
            &key,
            b"0123456789".as_slice(),
            Instant::now() + std::time::Duration::from_secs(5),
            Some(budget.clone()),
        )
        .await?;
    cache.commit_staged(&key, staged).await?;
    let store = crate::SegmentRevisionStore::default();
    let revision = store.create(key.proxy_session_id().clone(), 0, crate::SegmentRevisionKind::Processed)?;
    let (entered, resume) = cache.pause_startup_write(super::super::StartupWritePhase::BeforeRevisionLink);
    let source = cache.object_path(&key);
    let publish = cache.publish_processed_revision_with_budget(
        &revision.revision().key,
        &source,
        Instant::now() + std::time::Duration::from_secs(5),
        Some(budget),
    );
    let mut publish = Box::pin(publish);
    tokio::select! {
        entered = entered => entered.unwrap(),
        result = &mut publish => panic!("publication bypassed its link checkpoint: {result:?}"),
    }
    drop(publish);
    assert!(cache.reserve_revision_peak(&key, 1).await.is_err());
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 10);
    let token = cache.capacity_revision();
    resume.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), cache.wait_for_capacity_change(&token)).await.unwrap();
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 0);
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 10);
    assert_eq!(cache.revision_disk_bytes(), cache.scan_revision_disk_bytes().await?);
    assert!(!cache.has_active_temp_files());
    assert!(cache.metadata(&revision.revision().key).await?.is_none());
    assert_eq!(tokio::fs::read(source).await?, b"0123456789");
    Ok(())
}

#[tokio::test]
async fn concurrent_cache_replacement_cannot_change_a_processed_publication_source_or_its_accounted_size(
) -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    cache.update_cache_limits(64, 64);
    let key = SegmentCacheKey::new(ProxySessionId("publication-source".into()), 0, "ts");
    cache.write_bytes_and_commit(&key, b"old").await?;
    let store = crate::SegmentRevisionStore::default();
    let revision = store.create(key.proxy_session_id().clone(), 0, crate::SegmentRevisionKind::Processed)?;
    let source = cache.object_path(&key);
    let (entered, resume) = cache.pause_startup_write(super::super::StartupWritePhase::BeforeRevisionLink);
    let publication = cache.publish_processed_revision(
        &revision.revision().key,
        &source,
        Instant::now() + std::time::Duration::from_secs(5),
    );
    tokio::pin!(publication);
    tokio::select! {
        reached = entered => reached.unwrap(),
        result = &mut publication => panic!("publication bypassed the source checkpoint: {result:?}"),
    }
    let replacement = cache.write_bytes_and_commit(&key, b"larger-new-cache-body");
    tokio::pin!(replacement);
    assert!(futures::poll!(replacement.as_mut()).is_pending());
    resume.send(()).unwrap();
    let (published, replaced) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async { tokio::join!(publication, replacement) })
            .await
            .expect("source lease releases before publication commit reacquires admission");
    let published = published?;
    let replaced = replaced?;
    assert_eq!(published.size, 3);
    assert_eq!(tokio::fs::read(published.path).await?, b"old");
    assert_eq!(tokio::fs::read(replaced.path).await?, b"larger-new-cache-body");
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, replaced.size + 3);
    assert_eq!(cache.revision_disk_bytes(), cache.scan_revision_disk_bytes().await?);
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 0);
    Ok(())
}

#[tokio::test]
async fn shrinking_replacement_keeps_old_bytes_charged_until_rename_finishes() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    cache.update_cache_limits(110, 110);
    let key = SegmentCacheKey::new(ProxySessionId("shrinking".into()), 0, "ts");
    let other = SegmentCacheKey::new(ProxySessionId("other-session".into()), 0, "ts");
    cache.write_bytes_and_commit(&key, &[1; 100]).await?;
    let (entered, resume) = cache.pause_startup_write(super::super::StartupWritePhase::BeforeCacheRename);
    let replacement = cache.write_bytes_and_commit(&key, &[2; 10]);
    tokio::pin!(replacement);
    tokio::select! {
        reached = entered => reached.unwrap(),
        result = &mut replacement => panic!("replacement bypassed the rename checkpoint: {result:?}"),
    }
    assert_eq!(tokio::fs::read(cache.object_path(&key)).await?, [1; 100]);
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.session_bytes, 100);
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 10);
    assert!(cache.reserve_revision_peak(&key, 1).await.is_err());
    assert!(cache.reserve_revision_peak(&other, 1).await.is_err());
    resume.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), replacement).await.unwrap()?;
    assert_eq!(tokio::fs::read(cache.object_path(&key)).await?, [2; 10]);
    assert_eq!(cache.capacity_usage(key.proxy_session_id()).await?.global_bytes, 10);
    assert_eq!(cache.remaining_revision_bytes(key.proxy_session_id()), 0);
    let released = cache.reserve_revision_peak(&other, 100).await?;
    drop(released);
    Ok(())
}
