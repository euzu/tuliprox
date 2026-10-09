use super::*;

#[tokio::test]
async fn failed_projected_capacity_admission_returns_a_live_revision_token() {
    let (_temp_dir, cache, first, _) = cache_with_projected_session_pressure().await;
    let error = cache
        .ensure_projected_write_capacity(&first, 3)
        .await
        .expect_err("projected object exceeds the remaining session budget");
    let revision = hls_cache_capacity_from_io(&error).expect("typed capacity error").revision().clone();
    let wait = cache.wait_for_capacity_change(&revision);
    tokio::pin!(wait);

    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Pending));

    cache.notify_capacity_protection_changed();
    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
}

#[tokio::test]
async fn failed_admission_revision_is_captured_with_the_capacity_decision() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let path = temp_dir.path().join("session/000001.ts");
    {
        let mut capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        capacity.cache_path = temp_dir.path().to_path_buf();
        capacity.initialized = true;
        capacity.total_bytes = 2;
        capacity.session_bytes.insert("session".to_string(), 2);
    }
    let mut reservation =
        cache.try_begin_capacity_mutation(temp_dir.path(), &path, "session".to_string()).expect("mutation reservation");
    let error = reservation.reserve_replacement(0, 3, 2, 2).expect_err("capacity is exceeded");

    cache.notify_capacity_protection_changed();
    drop(reservation);

    let super::super::CapacityReservationError::Exceeded { revision, .. } = error else {
        panic!("expected typed capacity pressure");
    };
    let wait = cache.wait_for_capacity_change(&revision);
    tokio::pin!(wait);
    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
}

#[tokio::test]
async fn cache_path_change_during_write_rejects_old_generation_commit() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let old_root = temp_dir.path().join("old");
    let new_root = temp_dir.path().join("new");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(&old_root));
    let key = cache_key();
    let old_final_path = old_root.join("proxy_session/000123.ts");
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let reader = ControlledReader { started: Some(started_tx), release: release_rx, body: None };
    let task_cache = Arc::clone(&cache);
    let task_key = key.clone();
    let write_task = tokio::spawn(async move { task_cache.write_temp_and_commit(&task_key, reader).await });
    started_rx.await.expect("write reaches controlled reader");

    assert!(cache.update_cache_path(&new_root).await);
    release_tx.send(b"segment-body".to_vec()).expect("release write");
    let error = write_task.await.expect("write task joins").expect_err("old generation commit must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert!(!old_final_path.exists());
    assert!(cache.metadata(&key).await.expect("new-root metadata reads").is_none());
    assert!(!cache.has_active_temp_files());
}
