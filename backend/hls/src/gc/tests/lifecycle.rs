use super::*;

#[test]
fn terminal_tail_protects_live_base_until_lease_protection_is_released() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    session.install_terminal_tail_protection(
        lease_id.clone(),
        HlsTerminalTailProtection {
            generation: HlsTerminalTailGeneration(1),
            base_proxy_seqs: Arc::from([41, 42]),
            key_bindings: Arc::from([]),
        },
    );

    let protected = ProtectedSet::from_session(&session);
    assert!(protected.segment_proxy_seqs.contains(&41));
    assert!(protected.segment_proxy_seqs.contains(&42));

    assert!(session.remove_terminal_tail_protection(&lease_id).is_some());
    let released = ProtectedSet::from_session(&session);
    assert!(!released.segment_proxy_seqs.contains(&41));
    assert!(!released.segment_proxy_seqs.contains(&42));
}

#[tokio::test]
async fn gc_reclaims_terminal_base_only_after_lease_protection_release() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let lease_id = HlsAccessLeaseId("terminal-lease".to_string());
    let cache_key = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let segment = session.segments.get_mut(&1).expect("segment");
        gc.cache.write_bytes_and_commit(&segment.cache_key, b"segment-body").await.expect("cache write should succeed");
        segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        let cache_key = segment.cache_key.clone();
        session.install_terminal_tail_protection(
            lease_id.clone(),
            HlsTerminalTailProtection {
                generation: HlsTerminalTailGeneration(1),
                base_proxy_seqs: Arc::from([1_u64]),
                key_bindings: Arc::from([]),
            },
        );
        cache_key
    };

    let protected_report = gc.run_once(10_000).await.expect("protected gc should run");
    assert_eq!(protected_report.segments_deleted_duration, 0);
    assert!(session.read().await.segments.contains_key(&1));
    assert!(gc.cache.metadata(&cache_key).await.expect("metadata reads").is_some());

    assert!(session.write().await.remove_terminal_tail_protection(&lease_id).is_some());
    let released_report = gc.run_once(10_001).await.expect("released gc should run");

    assert_eq!(released_report.segments_deleted_duration, 1);
    assert!(gc.cache.metadata(&cache_key).await.expect("metadata reads").is_none());
}

#[test]
fn gc_report_logs_when_cleanup_or_invalidation_happened() {
    let mut report = GarbageCollectionReport { temp_files_deleted: 1, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());

    report = GarbageCollectionReport { stale_queue_entries_removed: 1, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());

    report = GarbageCollectionReport { segments_deleted_duration: 1, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());

    report = GarbageCollectionReport { maps_deleted: 1, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());

    report = GarbageCollectionReport { sessions_deleted: 1, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());

    report = GarbageCollectionReport { transient_resources_pruned: 1, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());

    report = GarbageCollectionReport { secret_cache_invalidated: true, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());

    report = GarbageCollectionReport { secret_cache_invalidation_deferred: true, ..GarbageCollectionReport::default() };
    assert!(report.did_cleanup_or_invalidate());
}

#[tokio::test]
async fn protected_startup_tail_defers_until_playback_releases_fifo_head() {
    const SESSION_LIMIT: u64 = 125 * 1_024 * 1_024;
    const RESIDENT_SIZES: [u64; 5] = [21_000_000, 22_000_000, 21_500_000, 22_500_000, 22_221_044];
    const STAGED_SIZE: u64 = 22_551_164;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (proxy_session_id, keys) = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let keys = (1_u64..=6)
            .map(|proxy_seq| session.segments.get(&proxy_seq).expect("segment").cache_key.clone())
            .collect::<Vec<_>>();
        (session.proxy_session_id.clone(), keys)
    };
    for (key, size) in keys.iter().take(5).zip(RESIDENT_SIZES) {
        commit_sparse_segment(&gc, key, size).await;
    }
    {
        let mut session = session.write().await;
        for (proxy_seq, content_length) in (1_u64..=5).zip(RESIDENT_SIZES) {
            session.segments.get_mut(&proxy_seq).expect("resident segment").status =
                SegmentCacheStatus::Ready { content_length, ready_at_ms: proxy_seq };
        }
        session.segments.get_mut(&6).expect("tail segment").status =
            SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, queued_at_ms: 1 };
        session.render_and_store_manifest(10).expect("six-segment canonical render");
    }

    let mut lease = activated_startup_lease(&proxy_session_id);
    let access_leases = Arc::new(RwLock::new(HlsAccessLeaseStore::default()));
    assert!(access_leases.write().await.prepare_access_lease(lease.clone()));
    gc.install_access_leases(&access_leases);
    gc.cache.update_cache_limits(512 * 1_024 * 1_024, SESSION_LIMIT);

    for key in keys.iter().take(3) {
        assert!(gc.cache.metadata(key).await.expect("visible metadata").is_some());
    }
    let deferred = gc
        .cache
        .ensure_projected_write_capacity(&keys[5], STAGED_SIZE)
        .await
        .expect_err("fully protected startup window defers the tail");
    let capacity = super::super::super::cache::hls_cache_capacity_from_io(&deferred).expect("typed capacity deferral");
    assert_eq!(capacity.configured_session_bytes(), SESSION_LIMIT);
    assert_eq!(capacity.current_session_bytes(), RESIDENT_SIZES.into_iter().sum::<u64>());
    assert_eq!(capacity.staged_bytes(), STAGED_SIZE);
    assert_eq!(capacity.required_session_bytes(), 700_208);
    assert_eq!(capacity.protected_working_set_bytes(), RESIDENT_SIZES.into_iter().sum::<u64>());
    assert_eq!(capacity.reclaimable_bytes(), 0);
    assert!(gc.cache.metadata(&keys[5]).await.expect("tail metadata").is_none());

    {
        let mut session = session.write().await;
        session.segments.get_mut(&6).expect("tail segment").status =
            SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Prefetch, deferred_at_ms: 11 };
        let safe_render = session.render_and_store_manifest(11).expect("deferred tail truncates safely");
        assert_eq!(safe_render.last_proxy_seq, 5);
        assert!(!safe_render.body.contains("000006.ts"));
    }

    lease.playback_cursor.highest_contiguous_completed_proxy_seq = Some(1);
    assert!(access_leases.write().await.prepare_access_lease(lease));
    gc.cache.notify_capacity_protection_changed();
    commit_sparse_segment(&gc, &keys[5], STAGED_SIZE).await;
    {
        let mut session = session.write().await;
        session.segments.get_mut(&6).expect("tail segment").status =
            SegmentCacheStatus::Ready { content_length: STAGED_SIZE, ready_at_ms: 12 };
        let resumed = session.render_and_store_manifest(12).expect("window resumes after reclamation");
        assert_eq!(resumed.first_proxy_seq, 2);
        assert_eq!(resumed.last_proxy_seq, 6);
    }
    assert!(gc.cache.metadata(&keys[0]).await.expect("released head metadata").is_none());
    assert!(gc.cache.metadata(&keys[5]).await.expect("resumed tail metadata").is_some());
    let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("bounded usage");
    assert!(usage.session_bytes <= SESSION_LIMIT);
}

#[tokio::test]
async fn terminal_evidence_pins_ready_key_object_and_mapping_across_gc_until_release() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (segment_cache_key, proxy_session_id, resource, resource_id, object_lookup_key, object_fetch_token) = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let segment_cache_key = session.segments.get(&1).expect("segment").cache_key.clone();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            "http://origin.example.com/live/key.bin",
            b"secret",
            0,
            10,
            Some("key".to_string()),
        );
        let resource_id = resource.id.clone();
        session.transient.upsert_resources([resource]);
        let resource = session.transient.resources.get(&resource_id).expect("registered key resource").clone();
        session.segments.get_mut(&1).expect("encrypted segment").encryption = Some(HlsSegmentEncryption {
            resource_id: resource_id.clone(),
            resource_extension: "key".to_string(),
            iv: Some("0x00000000000000000000000000000001".to_string()),
            key_format: Some("identity".to_string()),
            key_format_versions: Some("1".to_string()),
        });
        let proxy_session_id = session.proxy_session_id.clone();
        let object_lookup_key = TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "key");
        let object_fetch_token = match session.transient.begin_object_fetch(&proxy_session_id, &resource, "key", 0, 10)
        {
            TransientObjectFetchDecision::Fetch(token) => token,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                panic!("new key object starts one physical cache fill")
            }
        };
        (segment_cache_key, proxy_session_id, resource, resource_id, object_lookup_key, object_fetch_token)
    };
    gc.cache.write_bytes_and_commit(&segment_cache_key, b"ready-media").await.expect("media cache write");
    gc.cache
        .write_bytes_and_commit(object_fetch_token.cache_key(), b"0123456789abcdef")
        .await
        .expect("key cache write");
    {
        let mut session = session.write().await;
        session.segments.get_mut(&1).expect("segment").status =
            SegmentCacheStatus::Ready { content_length: 11, ready_at_ms: 0 };
        assert!(session.commit_transient_object_ready_if_current(
            resource.kind,
            &object_fetch_token,
            "application/octet-stream".to_string(),
            16,
            0,
            10,
        ));
    }
    let manifest = encrypted_terminal_evidence_manifest(&proxy_session_id, &resource_id);

    let evidence = prepare_terminal_base_evidence(&session, &gc.cache, &manifest, 5).await;
    assert!(evidence.availability()[0].required_key_ready);
    {
        let mut session = session.write().await;
        session.transient.resources.get_mut(&resource_id).expect("key mapping").expires_at_ms = 0;
        session.transient.object_cache.get_mut(&object_lookup_key).expect("key object").expires_at_ms = 0;
    }

    let pinned_report = gc.run_once(20).await.expect("pinned GC");
    assert_eq!(pinned_report.transient_resources_pruned, 0);
    assert_eq!(pinned_report.transient_objects_deleted, 0);
    {
        let session = session.read().await;
        assert!(session.transient.resources.contains_key(&resource_id));
        assert!(session.transient.object_cache.contains_key(&object_lookup_key));
    }

    evidence.release();
    let generation_before_release_gc = session.read().await.activity.media_readiness_generation;
    let released_report = gc.run_once(21).await.expect("released GC");
    assert_eq!(released_report.transient_resources_pruned, 1);
    assert_eq!(released_report.transient_objects_deleted, 1);
    let session = session.read().await;
    assert_eq!(
        session.activity.media_readiness_generation,
        generation_before_release_gc.saturating_add(1),
        "mapping and READY key object removal advance readiness once per session sweep"
    );
    assert!(!session.transient.resources.contains_key(&resource_id));
    assert!(!session.transient.object_cache.contains_key(&object_lookup_key));
}

#[tokio::test]
async fn session_gc_removes_idle_session_without_activity() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (key, proxy_session_id) = {
        let session = session.read().await;
        (session.key.clone(), session.proxy_session_id.clone())
    };

    let report = gc.run_once(2_000).await.expect("gc should run");

    assert_eq!(report.sessions_deleted, 1);
    assert!(gc.sessions.get_by_key(&key).await.is_none());
    assert!(gc.sessions.get_by_proxy_session_id(&proxy_session_id).await.is_none());
}
