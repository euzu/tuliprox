use super::*;

#[tokio::test]
async fn orphan_session_cleanup_preserves_active_session_directories() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    cache.write_rewrite_secret_fingerprint("secret").await.expect("marker");
    let active = ProxySessionId("active".to_string());
    let orphan = ProxySessionId("orphan".to_string());
    tokio::fs::create_dir_all(temp_dir.path().join("active")).await.expect("active dir");
    tokio::fs::create_dir_all(temp_dir.path().join("orphan")).await.expect("orphan dir");
    let cutoff = SystemTime::now();

    let removed = cache.delete_orphan_session_dirs(&HashSet::from([active]), cutoff).await.expect("cleanup");

    assert_eq!(removed, 1);
    assert!(temp_dir.path().join("active").exists());
    assert!(!temp_dir.path().join(orphan.0).exists());
}

#[tokio::test]
async fn orphan_session_cleanup_skips_directories_newer_than_freshness_cutoff() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    cache.write_rewrite_secret_fingerprint("secret").await.expect("marker");
    let stale = ProxySessionId("stale".to_string());
    let fresh = ProxySessionId("fresh".to_string());
    tokio::fs::create_dir_all(temp_dir.path().join(stale.0.clone())).await.expect("stale dir");
    tokio::fs::create_dir_all(temp_dir.path().join(fresh.0.clone())).await.expect("fresh dir");

    // Set the fresh dir's mtime to the future relative to the cutoff.
    let cutoff = SystemTime::now();
    let future = cutoff + std::time::Duration::from_mins(1);
    filetime::set_file_mtime(temp_dir.path().join(fresh.0.clone()), filetime::FileTime::from_system_time(future))
        .expect("set fresh mtime");

    let removed = cache.delete_orphan_session_dirs(&HashSet::new(), cutoff).await.expect("cleanup");

    assert_eq!(removed, 1, "stale orphan dir should be removed");
    assert!(!temp_dir.path().join(stale.0).exists());
    assert!(
        temp_dir.path().join(fresh.0).exists(),
        "directory newer than the freshness cutoff must be preserved to avoid racing concurrent session creation"
    );
}

#[tokio::test]
async fn owned_cache_operation_survives_caller_cancellation() {
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (completed_tx, completed_rx) = oneshot::channel();
    let permits = Arc::new(Semaphore::new(1));
    let caller = tokio::spawn(async move {
        run_owned_cache_operation(permits, "test", async move {
            started_tx.send(()).map_err(|()| io::Error::other("start receiver dropped"))?;
            release_rx.await.map_err(|_| io::Error::other("release sender dropped"))?;
            completed_tx.send(()).map_err(|()| io::Error::other("completion receiver dropped"))?;
            Ok(())
        })
        .await
    });
    started_rx.await.expect("owned operation starts");
    caller.abort();
    release_tx.send(()).expect("release owned operation");

    completed_rx.await.expect("detached owned operation completes");
    assert!(caller.await.expect_err("caller is cancelled").is_cancelled());
}

#[tokio::test]
async fn saturated_owned_operation_limit_prevents_spawn_and_mutation_until_permit_release() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();
    let permit_count = u32::try_from(MAX_CONCURRENT_OWNED_CACHE_OPERATIONS).expect("owned operation limit fits u32");
    let held_permits = Arc::clone(&cache.owned_operation_permits)
        .try_acquire_many_owned(permit_count)
        .expect("test exclusively holds owned operation permits");
    let operation_started = Arc::new(AtomicBool::new(false));
    let operation_started_in_task = Arc::clone(&operation_started);

    let spawn_error =
        run_owned_cache_operation(Arc::clone(&cache.owned_operation_permits), "saturated-test", async move {
            operation_started_in_task.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect_err("saturated helper must reject before spawning");

    let error = cache
        .write_bytes_and_commit(&key, b"segment-body")
        .await
        .expect_err("saturated cache operation must be rejected");

    assert_eq!(spawn_error.kind(), io::ErrorKind::WouldBlock);
    assert!(!operation_started.load(Ordering::SeqCst));
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(cache.metadata(&key).await.expect("metadata reads after rejection").is_none());
    assert!(!cache.has_active_temp_files());
    {
        let capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(capacity.total_bytes, 0);
        assert!(!capacity.session_bytes.contains_key("proxy_session"));
        assert!(capacity.active_mutations.is_empty(), "pre-spawn rejection must roll back the reservation");
    }

    drop(held_permits);
    cache.write_bytes_and_commit(&key, b"segment-body").await.expect("released permits allow a later mutation");
    assert!(cache.metadata(&key).await.expect("metadata reads after commit").is_some());
}

#[tokio::test]
async fn old_temp_file_cleanup_processes_a_bounded_batch_per_run() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let total_files = MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN.saturating_add(3);
    for index in 0..total_files {
        let session = if index % 2 == 0 { "session-a" } else { "session-b" };
        let directory = temp_dir.path().join(session).join(if index % 3 == 0 { "map" } else { "r" });
        tokio::fs::create_dir_all(&directory).await.expect("fixture directory writes");
        tokio::fs::write(directory.join(format!("object-{index}.ts.tmp.fixture")), b"stale")
            .await
            .expect("temporary fixture writes");
    }
    let cutoff = SystemTime::now() + Duration::from_mins(1);

    let first = cache.delete_temp_files_older_than(cutoff).await.expect("first cleanup run succeeds");
    let second = cache.delete_temp_files_older_than(cutoff).await.expect("second cleanup run succeeds");
    let third = cache.delete_temp_files_older_than(cutoff).await.expect("third cleanup run succeeds");

    assert_eq!(first, MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN);
    assert_eq!(second, total_files.saturating_sub(MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN));
    assert_eq!(third, 0);
}

#[tokio::test]
async fn delete_removes_committed_file_idempotently() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let key = cache_key();
    cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

    cache.delete(&key).await.expect("delete should succeed");
    cache.delete(&key).await.expect("second delete should be idempotent");

    assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
}
