use super::*;

#[tokio::test]
async fn concurrent_noop_capacity_failures_do_not_wake_each_other() {
    let (_temp_dir, cache, first, second) = cache_with_projected_session_pressure().await;
    let (first_error, second_error) = tokio::join!(
        cache.ensure_projected_write_capacity(&first, 3),
        cache.ensure_projected_write_capacity(&second, 3),
    );
    let first_error = first_error.expect_err("first projected object exceeds the remaining budget");
    let second_error = second_error.expect_err("second projected object exceeds the remaining budget");
    let first_revision =
        hls_cache_capacity_from_io(&first_error).expect("first typed capacity error").revision().clone();
    let second_revision =
        hls_cache_capacity_from_io(&second_error).expect("second typed capacity error").revision().clone();
    let first_wait = cache.wait_for_capacity_change(&first_revision);
    let second_wait = cache.wait_for_capacity_change(&second_revision);
    tokio::pin!(first_wait, second_wait);

    assert!(matches!(futures::poll!(first_wait.as_mut()), Poll::Pending));
    assert!(matches!(futures::poll!(second_wait.as_mut()), Poll::Pending));

    cache.notify_capacity_protection_changed();
    assert!(matches!(futures::poll!(first_wait.as_mut()), Poll::Ready(())));
    assert!(matches!(futures::poll!(second_wait.as_mut()), Poll::Ready(())));
}

#[tokio::test]
async fn abandoned_capacity_reservation_rolls_back_before_filesystem_mutation() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let path = temp_dir.path().join("session/000001.ts");
    {
        let mut capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        capacity.cache_path = temp_dir.path().to_path_buf();
        capacity.initialized = true;
        capacity.active_mutations.insert(path.clone());
    }
    let mut reservation = super::super::CapacityMutationReservation {
        path: path.clone(),
        cache_path: temp_dir.path().to_path_buf(),
        session_component: "session".to_string(),
        replacement: None,
        filesystem_mutation_started: false,
        capacity: Arc::clone(&cache.capacity),
        changed: Arc::clone(&cache.capacity_changed),
    };
    reservation.reserve_replacement(0, 3, 10, 10).expect("capacity reserves");
    let revision = cache.capacity_revision();

    drop(reservation);

    {
        let capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(capacity.initialized);
        assert_eq!(capacity.total_bytes, 0);
        assert!(!capacity.session_bytes.contains_key("session"));
        assert!(!capacity.active_mutations.contains(&path));
    }
    let wait = cache.wait_for_capacity_change(&revision);
    tokio::pin!(wait);
    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
}

#[tokio::test]
async fn abandoned_started_filesystem_mutation_invalidates_capacity_snapshot() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
    let path = temp_dir.path().join("session/000001.ts");
    {
        let mut capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        capacity.cache_path = temp_dir.path().to_path_buf();
        capacity.initialized = true;
        capacity.active_mutations.insert(path.clone());
    }
    let mut reservation = super::super::CapacityMutationReservation {
        path: path.clone(),
        cache_path: temp_dir.path().to_path_buf(),
        session_component: "session".to_string(),
        replacement: None,
        filesystem_mutation_started: false,
        capacity: Arc::clone(&cache.capacity),
        changed: Arc::clone(&cache.capacity_changed),
    };
    reservation.reserve_replacement(0, 3, 10, 10).expect("capacity reserves");
    reservation.mark_filesystem_mutation_started();
    let revision = cache.capacity_revision();

    drop(reservation);

    {
        let capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!capacity.initialized);
        assert!(!capacity.active_mutations.contains(&path));
    }
    let wait = cache.wait_for_capacity_change(&revision);
    tokio::pin!(wait);
    assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
}

#[tokio::test]
async fn concurrent_commits_reserve_the_global_budget_exactly_once() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    cache.update_cache_limits(5, 5);
    let first = SegmentCacheKey::new(ProxySessionId("first".to_string()), 1, "ts");
    let second = SegmentCacheKey::new(ProxySessionId("second".to_string()), 1, "ts");
    let first_cache = Arc::clone(&cache);
    let first_task = tokio::spawn(async move { first_cache.write_bytes_and_commit(&first, b"123").await });
    let second_cache = Arc::clone(&cache);
    let second_task = tokio::spawn(async move { second_cache.write_bytes_and_commit(&second, b"456").await });

    let (first_result, second_result) = tokio::join!(first_task, second_task);
    let results = [first_result.expect("first task joins"), second_result.expect("second task joins")];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
}

#[tokio::test]
async fn concurrent_commit_cannot_consume_bytes_reclaimed_for_an_in_flight_writer() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let proxy_session_id = ProxySessionId("serialized-reclamation".to_string());
    let first_resident = SegmentCacheKey::new(proxy_session_id.clone(), 1, "ts");
    let second_resident = SegmentCacheKey::new(proxy_session_id.clone(), 2, "ts");
    let first_target = SegmentCacheKey::new(proxy_session_id.clone(), 3, "ts");
    let second_target = SegmentCacheKey::new(proxy_session_id.clone(), 4, "ts");
    cache.update_cache_limits(100, 100);
    cache.write_bytes_and_commit(&first_resident, b"0123456789").await.expect("first resident commits");
    cache.write_bytes_and_commit(&second_resident, b"0123456789").await.expect("second resident commits");
    cache.update_cache_limits(25, 25);

    let first_staged = cache
        .stage_temp_with_deadline(
            &first_target,
            &b"abcdefghij"[..],
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("first target stages");
    let second_staged = cache
        .stage_temp_with_deadline(
            &second_target,
            &b"klmnopqrst"[..],
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("second target stages");
    let (first_reclaimed_tx, first_reclaimed_rx) = oneshot::channel();
    let (resume_first_tx, resume_first_rx) = oneshot::channel();
    let reclaimer = Arc::new(PausingCapacityReclaimer {
        cache: Arc::clone(&cache),
        victims: Mutex::new(VecDeque::from([first_resident, second_resident])),
        first_reclaimed: Mutex::new(Some(first_reclaimed_tx)),
        resume_first: Mutex::new(Some(resume_first_rx)),
    });
    cache.install_capacity_reclaimer(&reclaimer);

    let first_cache = Arc::clone(&cache);
    let first_task = tokio::spawn(async move { first_cache.commit_staged(&first_target, first_staged).await });
    first_reclaimed_rx.await.expect("first reclamation pauses after deleting one resident");

    let second_cache = Arc::clone(&cache);
    let second_task = tokio::spawn(async move { second_cache.commit_staged(&second_target, second_staged).await });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let paused_usage = cache.capacity_usage(&proxy_session_id).await.expect("paused capacity usage");
    assert_eq!(paused_usage.session_bytes, 10, "a later commit must wait for the reclaiming writer's retry");

    assert!(resume_first_tx.send(()).is_ok());
    let (first_result, second_result) = tokio::join!(first_task, second_task);
    assert!(first_result.expect("first task joins").is_ok());
    assert!(second_result.expect("second task joins").is_ok());
    let usage = cache.capacity_usage(&proxy_session_id).await.expect("final capacity usage");
    assert_eq!(usage.session_bytes, 20);
    assert!(usage.global_bytes <= 25);
}
