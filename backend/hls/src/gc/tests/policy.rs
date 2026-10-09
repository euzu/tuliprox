use super::*;

#[test]
fn gc_report_is_quiet_when_nothing_changed() {
    let report = GarbageCollectionReport::default();

    assert!(!report.did_cleanup_or_invalidate());
}

#[tokio::test]
async fn gc_keeps_active_readers() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let segment = session.segments.get_mut(&1).expect("segment");
        gc.cache.write_bytes_and_commit(&segment.cache_key, b"segment-body").await.expect("cache write should succeed");
        segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        segment.access.reader_started(1);
    }

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.segments_deleted_duration, 0);
    assert!(session.read().await.segments.contains_key(&1));
}

#[tokio::test]
async fn duration_gc_deletes_old_unprotected_segments() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let segment = session.segments.get_mut(&1).expect("segment");
        gc.cache.write_bytes_and_commit(&segment.cache_key, b"segment-body").await.expect("cache write should succeed");
        segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
    }

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.segments_deleted_duration, 1);
    assert!(!session.read().await.segments.contains_key(&1));
}

#[tokio::test]
async fn cache_delete_failure_does_not_block_later_deletes_and_retries_in_active_session() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (failed_key, successful_key) = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        session.last_client_access_at_ms = 10_000;
        for proxy_seq in [1, 2] {
            session.segments.get_mut(&proxy_seq).expect("segment").status =
                SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        }
        (
            session.segments.get(&1).expect("failed segment").cache_key.clone(),
            session.segments.get(&2).expect("successful segment").cache_key.clone(),
        )
    };
    gc.cache.write_bytes_and_commit(&failed_key, b"segment-body").await.expect("failed fixture writes");
    gc.cache.write_bytes_and_commit(&successful_key, b"segment-body").await.expect("successful fixture writes");
    let failed_path = gc.cache.object_path(&failed_key);
    tokio::fs::remove_file(&failed_path).await.expect("replace failed fixture");
    tokio::fs::create_dir(&failed_path).await.expect("directory makes remove_file fail");

    let first_report = gc.run_once(10_000).await.expect("first gc run");

    assert_eq!(first_report.cache_object_deletions_planned, 2);
    assert_eq!(first_report.cache_object_deletions_succeeded, 1);
    assert_eq!(first_report.cache_object_deletions_deferred, 1);
    assert_eq!(first_report.segments_deleted_duration, 1);
    assert!(gc.cache.metadata(&successful_key).await.expect("metadata reads").is_none());
    assert_eq!(gc.pending_cache_deletion_count(), 1);
    assert!(!session.read().await.segments.contains_key(&1));
    assert!(!session.read().await.segments.contains_key(&2));

    tokio::fs::remove_dir(&failed_path).await.expect("remove failing fixture");
    tokio::fs::write(&failed_path, b"replacement-body").await.expect("replacement fixture writes");

    let retry_report = gc.run_once(10_001).await.expect("retry gc run");

    assert_eq!(retry_report.cache_object_deletions_planned, 0);
    assert_eq!(retry_report.cache_object_deletions_succeeded, 1);
    assert_eq!(retry_report.cache_object_deletions_deferred, 0);
    assert_eq!(retry_report.segments_deleted_duration, 1);
    assert_eq!(gc.pending_cache_deletion_count(), 0);
    assert!(gc.cache.metadata(&failed_key).await.expect("metadata reads").is_none());
}

#[tokio::test]
async fn ordinary_gc_batch_preserves_bounded_switch_rollback_headroom() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let batch = gc.reserve_cache_deletion_batch();
    assert_eq!(batch.remaining_capacity(), MAX_PENDING_CACHE_DELETIONS - SWITCH_CACHE_CLEANUP_HEADROOM);

    let segment = gc.reserve_switch_segment_cleanup(crate::SegmentCacheKey::new(proxy_session_id.clone(), 1, "ts"));
    let map = gc.reserve_switch_map_cleanup(crate::MapCacheKey::new(proxy_session_id, ProxyMapId(1), "mp4"));

    assert!(segment.is_some());
    assert!(map.is_some());
    let queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(queue.pending.len().saturating_add(queue.reserved_slots) <= MAX_PENDING_CACHE_DELETIONS);
}

#[tokio::test]
async fn duration_gc_stops_at_protected_fifo_head() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        for proxy_seq in [1, 2] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        }
        session.segments.get_mut(&1).expect("head segment").access.reader_started(1);
    }

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.segments_deleted_duration, 0);
    let session = session.read().await;
    assert!(session.segments.contains_key(&1));
    assert!(session.segments.contains_key(&2));
}

#[tokio::test]
async fn duration_gc_stops_at_not_expired_fifo_head() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        for (proxy_seq, ready_at_ms) in [(1, 9_950), (2, 0)] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
        }
    }

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.segments_deleted_duration, 0);
    let session = session.read().await;
    assert!(session.segments.contains_key(&1));
    assert!(session.segments.contains_key(&2));
}

#[tokio::test]
async fn session_size_gc_deletes_oldest_unprotected_segment() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    update_gc_policy(&gc, |policy| policy.cache_bytes_per_session = 20);
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        for proxy_seq in [1, 2, 3] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
        }
    }

    let report = gc.run_once(100).await.expect("gc should run");

    assert_eq!(report.segments_deleted_size_session, 2);
    let session = session.read().await;
    assert!(!session.segments.contains_key(&1));
    assert!(!session.segments.contains_key(&2));
    assert!(session.segments.contains_key(&3));
}

#[tokio::test]
async fn projected_session_pressure_reclaims_before_the_limit_is_crossed() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
    }
    cache_selected_ready_segments(&gc, &session, &[1, 2], b"0123456789").await;
    gc.cache.update_cache_limits(100, 25);
    let target = session.read().await.segments.get(&3).expect("target").cache_key.clone();

    let committed = gc.cache.write_bytes_and_commit(&target, b"abcdefghij").await.expect("projected reclaim");

    assert_eq!(committed.size, 10);
    let session_guard = session.read().await;
    assert!(!session_guard.segments.contains_key(&1), "oldest unprotected FIFO head is reclaimed");
    assert!(session_guard.segments.contains_key(&2));
    let proxy_session_id = session_guard.proxy_session_id.clone();
    drop(session_guard);
    assert!(gc.cache.metadata(&target).await.expect("target metadata").is_some());
    let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
    assert_eq!(usage.session_bytes, 20);
    assert!(usage.session_bytes <= 25);
    assert!(!gc.cache.has_active_temp_files());
}

#[tokio::test]
async fn projected_pressure_never_skips_a_protected_fifo_head() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
    }
    cache_selected_ready_segments(&gc, &session, &[1, 2], b"0123456789").await;
    {
        let mut session = session.write().await;
        session.segments.get_mut(&1).expect("head").access.reader_started(1);
    }
    gc.cache.update_cache_limits(100, 25);
    let target = session.read().await.segments.get(&3).expect("target").cache_key.clone();

    let error = gc.cache.write_bytes_and_commit(&target, b"abcdefghij").await.expect_err("head is protected");

    assert!(matches!(
        HlsOriginResourceFetchError::cache_commit(&error),
        HlsOriginResourceFetchError::LocalCacheCapacity { .. }
    ));
    let session = session.read().await;
    assert!(session.segments.contains_key(&1));
    assert!(session.segments.contains_key(&2));
    drop(session);
    assert!(gc.cache.metadata(&target).await.expect("target metadata").is_none());
    assert!(!gc.cache.has_active_temp_files());
}

#[tokio::test]
async fn projected_global_pressure_uses_oldest_session_fifo_head() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, first_session) = gc_with_session(&temp_dir).await;
    let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(1, "67890"), b"secret", 0).await;
    for session in [&first_session, &second_session] {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
    }
    cache_selected_ready_segments(&gc, &first_session, &[1], b"12345678").await;
    cache_selected_ready_segments(&gc, &second_session, &[1], b"abcdefgh").await;
    {
        let mut first = first_session.write().await;
        let SegmentCacheStatus::Ready { ready_at_ms, .. } = &mut first.segments.get_mut(&1).expect("first head").status
        else {
            panic!("first head must be ready");
        };
        *ready_at_ms = 1;
    }
    {
        let mut second = second_session.write().await;
        let SegmentCacheStatus::Ready { ready_at_ms, .. } =
            &mut second.segments.get_mut(&1).expect("second head").status
        else {
            panic!("second head must be ready");
        };
        *ready_at_ms = 2;
    }
    gc.cache.update_cache_limits(20, 100);
    let (target, proxy_session_id) = {
        let session = second_session.read().await;
        (session.segments.get(&2).expect("target").cache_key.clone(), session.proxy_session_id.clone())
    };

    gc.cache.write_bytes_and_commit(&target, b"ijklmnop").await.expect("global projected reclaim");

    assert!(!first_session.read().await.segments.contains_key(&1));
    assert!(second_session.read().await.segments.contains_key(&1));
    let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
    assert_eq!(usage.global_bytes, 16);
    assert!(usage.global_bytes <= 20);
}

#[tokio::test]
async fn high_bitrate_sequence_advances_beyond_the_512_mib_aggregate_wall() {
    use std::fmt::Write as _;

    const SESSION_LIMIT: u64 = 512 * 1024 * 1024;
    const OLD_SEGMENT_BYTES: u64 = 21_825_922;
    const NEW_SEGMENT_BYTES: u64 = 22_000_000;
    const READY_SEGMENTS: u64 = 24;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let mut manifest = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:1\n");
    for origin_seq in 1..=READY_SEGMENTS.saturating_add(1) {
        writeln!(manifest, "#EXTINF:10.0,\n{origin_seq}.ts").expect("writing to a String cannot fail");
    }
    {
        let mut session = session.write().await;
        session.proxy_next_seq = Some(1);
        session.apply_origin_manifest(&normal_manifest(&manifest)).expect("high-bitrate manifest maps");
    }
    let keys = {
        let session = session.read().await;
        (1..=READY_SEGMENTS.saturating_add(1))
            .map(|proxy_seq| session.segments.get(&proxy_seq).expect("segment").cache_key.clone())
            .collect::<Vec<_>>()
    };
    for key in keys.iter().take(usize::try_from(READY_SEGMENTS).unwrap_or(usize::MAX)) {
        commit_sparse_segment(&gc, key, OLD_SEGMENT_BYTES).await;
    }
    {
        let mut session = session.write().await;
        for proxy_seq in 1..=READY_SEGMENTS {
            session.segments.get_mut(&proxy_seq).expect("ready segment").status =
                SegmentCacheStatus::Ready { content_length: OLD_SEGMENT_BYTES, ready_at_ms: proxy_seq };
        }
    }
    gc.cache.update_cache_limits(2 * SESSION_LIMIT, SESSION_LIMIT);
    let target = keys.last().expect("target key");

    commit_sparse_segment(&gc, target, NEW_SEGMENT_BYTES).await;
    {
        let mut session = session.write().await;
        session.segments.get_mut(&READY_SEGMENTS.saturating_add(1)).expect("newest segment").status =
            SegmentCacheStatus::Ready { content_length: NEW_SEGMENT_BYTES, ready_at_ms: READY_SEGMENTS + 1 };
    }

    let aggregate_sequence_bytes = READY_SEGMENTS.saturating_mul(OLD_SEGMENT_BYTES).saturating_add(NEW_SEGMENT_BYTES);
    assert!(aggregate_sequence_bytes > SESSION_LIMIT);
    let session_guard = session.read().await;
    assert!(!session_guard.segments.contains_key(&1));
    assert!(session_guard.segments.contains_key(&READY_SEGMENTS.saturating_add(1)));
    let proxy_session_id = session_guard.proxy_session_id.clone();
    drop(session_guard);
    let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
    assert_eq!(usage.session_bytes, READY_SEGMENTS.saturating_sub(1) * OLD_SEGMENT_BYTES + NEW_SEGMENT_BYTES);
    assert!(usage.session_bytes <= SESSION_LIMIT);
}

#[tokio::test]
async fn session_size_gc_stops_at_protected_fifo_head() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    update_gc_policy(&gc, |policy| policy.cache_bytes_per_session = 20);
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        for proxy_seq in [1, 2, 3] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
        }
        session.segments.get_mut(&1).expect("head segment").access.reader_started(1);
    }

    let report = gc.run_once(100).await.expect("gc should run");

    assert_eq!(report.segments_deleted_size_session, 0);
    let session = session.read().await;
    assert!(session.segments.contains_key(&1));
    assert!(session.segments.contains_key(&2));
    assert!(session.segments.contains_key(&3));
}

#[tokio::test]
async fn protected_map_remains_and_unreferenced_map_is_deleted() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let manifest = normal_manifest(
        "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXTINF:4.0,\n2.m4s\n#EXTINF:4.0,\n3.m4s\n",
    );
    {
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        }
        let protected_map_id = ProxyMapId(0);
        session.maps.get_mut(&protected_map_id).expect("map").status =
            MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
        let unreferenced_key = OriginMapKey {
            origin_epoch: 0,
            resolved_origin_uri: "http://origin.example.com/live/unused.mp4".to_string(),
            byte_range: None,
        };
        let unreferenced_map =
            crate::MapEntry::new(&session.proxy_session_id, ProxyMapId(1), unreferenced_key.clone(), "mp4".to_string());
        session.maps.insert(ProxyMapId(1), unreferenced_map);
        session.origin_map_to_proxy.insert(unreferenced_key, ProxyMapId(1));
        session.maps.get_mut(&ProxyMapId(1)).expect("map").status =
            MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
        session.render_and_store_manifest(1).expect("manifest should render");
    }

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.maps_deleted, 1);
    let session = session.read().await;
    assert!(session.maps.contains_key(&ProxyMapId(0)));
    assert!(!session.maps.contains_key(&ProxyMapId(1)));
}

#[tokio::test]
async fn map_referenced_by_remaining_segment_survives_fifo_gc() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n0.m4s\n#EXTINF:4.0,\n1.m4s\n");
    {
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.maps.get_mut(&ProxyMapId(0)).expect("map").status =
            MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
        for (proxy_seq, ready_at_ms) in [(0, 0), (1, 9_950)] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
        }
    }

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.segments_deleted_duration, 1);
    assert_eq!(report.maps_deleted, 0);
    let session = session.read().await;
    assert!(!session.segments.contains_key(&0));
    assert!(session.segments.contains_key(&1));
    assert!(session.maps.contains_key(&ProxyMapId(0)));
}

#[tokio::test]
async fn secret_fingerprint_mismatch_invalidates_cache_and_sessions() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let segment = session.segments.get_mut(&1).expect("segment");
        gc.cache.write_bytes_and_commit(&segment.cache_key, b"segment-body").await.expect("cache write should succeed");
    }
    gc.cache.write_rewrite_secret_fingerprint("mismatch").await.expect("marker write");

    let report = gc.run_once(1).await.expect("gc should run");

    assert!(report.secret_cache_invalidated);
    assert!(!report.secret_cache_invalidation_deferred);
    assert_eq!(gc.sessions.len().await, 0);
    let rewrite_secret_fingerprint = gc.rewrite_secret_fingerprint();
    assert_eq!(
        gc.cache.read_rewrite_secret_fingerprint().await.expect("marker read").as_deref(),
        Some(rewrite_secret_fingerprint.as_str())
    );
}

#[tokio::test]
async fn secret_fingerprint_mismatch_with_active_temp_defers_invalidation() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let cache_key = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        session.segments.get(&1).expect("segment").cache_key.clone()
    };
    gc.cache.write_rewrite_secret_fingerprint("mismatch").await.expect("marker write");
    let staged = gc
        .cache
        .stage_temp_with_deadline(&cache_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
        .await
        .expect("cache object stages");
    assert!(gc.cache.has_active_temp_files());

    let report = gc.run_once(1).await.expect("gc should run");

    assert!(!report.secret_cache_invalidated);
    assert!(report.secret_cache_invalidation_deferred);
    assert_eq!(gc.sessions.len().await, 1);
    assert_eq!(gc.cache.read_rewrite_secret_fingerprint().await.expect("marker read").as_deref(), Some("mismatch"));
    assert!(gc.cache.has_active_temp_files());
    gc.cache.commit_staged(&cache_key, staged).await.expect("staged object commits");
    assert!(gc.cache.metadata(&cache_key).await.expect("metadata should read").is_some());
}

#[tokio::test]
async fn deferred_secret_fingerprint_invalidation_runs_after_temp_commit() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let cache_key = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        session.segments.get(&1).expect("segment").cache_key.clone()
    };
    gc.cache.write_rewrite_secret_fingerprint("mismatch").await.expect("marker write");
    let staged = gc
        .cache
        .stage_temp_with_deadline(&cache_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
        .await
        .expect("cache object stages");
    assert!(gc.run_once(1).await.expect("first gc should run").secret_cache_invalidation_deferred);
    gc.cache.commit_staged(&cache_key, staged).await.expect("staged object commits");

    let report = gc.run_once(2).await.expect("second gc should run");

    assert!(report.secret_cache_invalidated);
    assert!(!report.secret_cache_invalidation_deferred);
    assert_eq!(gc.sessions.len().await, 0);
    assert_eq!(gc.cache.metadata(&cache_key).await.expect("metadata should read"), None);
    let rewrite_secret_fingerprint = gc.rewrite_secret_fingerprint();
    assert_eq!(
        gc.cache.read_rewrite_secret_fingerprint().await.expect("marker read").as_deref(),
        Some(rewrite_secret_fingerprint.as_str())
    );
}

#[tokio::test]
async fn global_candidate_selection_skips_a_session_that_failed_revalidation() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, first_session) = gc_with_session(&temp_dir).await;
    let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(2, "12345"), b"secret", 0).await;

    for (session, ready_at_ms) in [(&first_session, 1), (&second_session, 2)] {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        session.segments.get_mut(&1).expect("head").status =
            SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
    }
    let first_proxy_session_id = first_session.read().await.proxy_session_id.clone();
    let sessions = [Arc::clone(&first_session), Arc::clone(&second_session)];

    let first =
        oldest_global_fifo_head_candidate(&sessions, &gc.cache, None, &HashSet::new()).await.expect("candidate");
    assert_eq!(first.proxy_session_id, first_proxy_session_id);

    let skipped = HashSet::from([first_proxy_session_id]);
    let next = oldest_global_fifo_head_candidate(&sessions, &gc.cache, None, &skipped).await.expect("next candidate");
    assert_eq!(next.proxy_session_id, second_session.read().await.proxy_session_id);
}

#[tokio::test]
async fn global_size_gc_deletes_oldest_unprotected_segments() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, first_session) = gc_with_session(&temp_dir).await;
    update_gc_policy(&gc, |policy| policy.cache_bytes_global = 24);
    let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(2, "12345"), b"secret", 0).await;

    for session in [&first_session, &second_session] {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        for proxy_seq in [1, 2, 3] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
        }
    }

    let report = gc.run_once(100).await.expect("gc should run");

    assert_eq!(report.segments_deleted_size_global, 4);
    let remaining_size = {
        let first = first_session.read().await;
        let second = second_session.read().await;
        super::super::selection::session_cache_size(&first) + super::super::selection::session_cache_size(&second)
    };
    assert!(remaining_size <= 24);
}

#[tokio::test]
async fn global_size_gc_uses_only_current_fifo_heads() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, first_session) = gc_with_session(&temp_dir).await;
    update_gc_policy(&gc, |policy| policy.cache_bytes_global = 24);
    let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(2, "12345"), b"secret", 0).await;

    for session in [&first_session, &second_session] {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        for proxy_seq in [1, 2, 3] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
        }
    }
    first_session.write().await.segments.get_mut(&1).expect("head segment").access.reader_started(1);

    let report = gc.run_once(100).await.expect("gc should run");

    assert_eq!(report.segments_deleted_size_global, 3);
    let first = first_session.read().await;
    assert!(first.segments.contains_key(&1));
    assert!(first.segments.contains_key(&2));
    assert!(first.segments.contains_key(&3));
    let second = second_session.read().await;
    assert!(!second.segments.contains_key(&1));
    assert!(!second.segments.contains_key(&2));
    assert!(!second.segments.contains_key(&3));
}

#[tokio::test]
async fn temp_file_gc_deletes_old_tmp_files() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let temp_path = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let path = gc.cache.object_path(&session.segments.get(&1).expect("segment").cache_key);
        let parent = path.parent().expect("cache object has parent");
        tokio::fs::create_dir_all(parent).await.expect("parent dir");
        parent.join("000001.ts.tmp.old")
    };
    tokio::fs::write(&temp_path, b"partial").await.expect("temp write");
    let old_time = filetime::FileTime::from_unix_time(1, 0);
    filetime::set_file_mtime(&temp_path, old_time).expect("set mtime");

    let report = gc.run_once(1).await.expect("gc should run");

    assert_eq!(report.temp_files_deleted, 1);
    assert!(!temp_path.exists());
}

#[tokio::test]
async fn session_gc_keeps_idle_session_with_active_temp_file() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (cache_key, proxy_session_id) = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        (session.segments.get(&1).expect("segment").cache_key.clone(), session.proxy_session_id.clone())
    };
    let staged = gc
        .cache
        .stage_temp_with_deadline(&cache_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
        .await
        .expect("cache object stages");
    assert!(gc.cache.has_active_temp_files_for_session(&proxy_session_id));

    let report = gc.run_once(2_000).await.expect("gc should run");

    assert_eq!(report.sessions_deleted, 0);
    assert!(!session.read().await.is_gc_marked_for_removal());
    assert_eq!(gc.sessions.len().await, 1);
    gc.cache.commit_staged(&cache_key, staged).await.expect("staged object commits");
}

#[tokio::test]
async fn session_gc_final_recheck_keeps_new_activity() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    session.write().await.last_client_access_at_ms = 1_500;
    let mut report = super::super::GarbageCollectionReport::default();
    let policy = gc.policy();

    gc.remove_idle_session_if_still_idle(&session, 2_000, &policy, &mut report).await;

    assert_eq!(report.sessions_deleted, 0);
    assert_eq!(gc.sessions.len().await, 1);
    assert!(!session.read().await.is_gc_marked_for_removal());
}
