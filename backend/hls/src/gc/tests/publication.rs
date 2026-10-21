use super::*;

#[tokio::test]
async fn published_resource_history_survives_production_segment_removal_and_file_deletion() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let baseline = normal_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
             #EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n",
    );
    let replay_then_new = normal_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
             #EXTINF:4,\n480.ts\n#EXTINF:4,\n490.ts\n#EXTINF:4,\n491.ts\n#EXTINF:4,\n492.ts\n",
    );
    let removed_key = {
        let mut session = session.write().await;
        session.apply_origin_manifest(&baseline).expect("baseline maps");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
        }
        session.render_and_store_manifest(1).expect("baseline publishes");
        session.segments.get(&0).expect("published head").cache_key.clone()
    };
    gc.cache.write_bytes_and_commit(&removed_key, b"x").await.expect("cache object writes");
    {
        let mut session = session.write().await;
        assert_eq!(remove_segment_entry(&mut session, 0), Some(removed_key.clone()));
    }
    gc.cache.delete(&removed_key).await.expect("production cache file deletion succeeds");
    {
        let mut session = session.write().await;
        session.apply_origin_manifest(&replay_then_new).expect("history trims removed resource replay");
        assert!(!session.segments.contains_key(&0));
        assert_eq!(session.proxy_next_seq, Some(6));
        assert!(session.segments.get(&3).expect("first new media").discontinuity_before);
    }
    assert!(gc.cache.metadata(&removed_key).await.expect("metadata lookup succeeds").is_none());
}

#[tokio::test]
async fn gc_keeps_last_rendered_manifest_segments() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    populate_ready_segments(&gc, &session, 0).await;

    let report = gc.run_once(10_000).await.expect("gc should run");

    assert_eq!(report.segments_deleted_duration, 0);
    assert_eq!(session.read().await.segments.len(), 6);
}

#[tokio::test]
async fn finalized_transient_resource_mappings_in_current_manifest_are_protected() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let resource_id = {
        let mut session = session.write().await;
        let resource = crate::TransientResourceRef::new(
            crate::TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            0,
            10,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        session.transient.upsert_resources([resource]);
        session.transient.replace_manifest_with_semantics(
            format!("#EXTM3U\n#EXTINF:1,\n/hls/shared/live/session/lease/r/{}.ts\n#EXT-X-ENDLIST\n", resource_id.0),
            0,
            Some(1_000),
        );
        resource_id
    };

    let report = gc.run_once(20).await.expect("gc should run");

    assert_eq!(report.transient_resources_pruned, 0);
    assert!(session.read().await.transient.resources.contains_key(&resource_id));
}

#[test]
fn finalized_manifest_mapping_count_does_not_become_object_pin_count() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "large-finalized"), b"secret", 0);
    let resources = install_finalized_transient_manifest(&mut session, 1_643, 10);

    let protected = ProtectedSet::from_session(&session);

    assert_eq!(resources.len(), 1_643);
    assert_eq!(session.transient.current_manifest_resource_ids().len(), 1_643);
    assert!(protected.transient_object_ids.is_empty());
    assert!(resources.iter().all(|resource| session.transient.resolve_current_resource(&resource.id, 20).is_some()));
}

#[tokio::test]
async fn large_finalized_manifest_keeps_mappings_without_pinning_all_objects() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (resource, lookup_key, fetch_token) = {
        let mut session = session.write().await;
        let resources = install_finalized_transient_manifest(&mut session, 1_643, 10);
        let resource = resources.into_iter().next().expect("large manifest resource");
        let lookup_key = TransientPassthroughState::transient_object_key(&session.proxy_session_id, &resource.id, "ts");
        let proxy_session_id = session.proxy_session_id.clone();
        let fetch_token = match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 0, 10) {
            TransientObjectFetchDecision::Fetch(token) => token,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                panic!("first archive object starts a cache fill")
            }
        };
        (resource, lookup_key, fetch_token)
    };
    gc.cache.write_bytes_and_commit(fetch_token.cache_key(), b"archive-object").await.expect("archive object writes");
    {
        let mut session = session.write().await;
        assert!(session.commit_transient_object_ready_if_current(
            resource.kind,
            &fetch_token,
            "video/mp2t".to_string(),
            14,
            0,
            10,
        ));
        let protected = ProtectedSet::from_session(&session);
        assert_eq!(session.transient.current_manifest_resource_ids().len(), 1_643);
        assert!(!protected.transient_object_ids.contains(&resource.id));
    }

    let report = gc.run_once(20).await.expect("finalized archive GC runs");

    assert_eq!(report.transient_resources_pruned, 0);
    assert_eq!(report.transient_objects_deleted, 1);
    assert!(gc.cache.metadata(fetch_token.cache_key()).await.expect("object metadata reads").is_none());
    let mut session = session.write().await;
    assert_eq!(session.transient.resources.len(), 1_643);
    let current = session.transient.resolve_current_resource(&resource.id, 20).expect("mapping survives byte eviction");
    assert!(!session.transient.object_cache.contains_key(&lookup_key));
    let proxy_session_id = session.proxy_session_id.clone();
    assert!(matches!(
        session.transient.begin_object_fetch(&proxy_session_id, &current, "ts", 20, 10,),
        TransientObjectFetchDecision::Fetch(_)
    ));
}

#[tokio::test]
async fn transient_object_gc_deletes_the_physical_fill_generation_instead_of_the_logical_lookup_path() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (lookup_key, fetch_token, resource_kind) = {
        let mut session = session.write().await;
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/object1.ts",
            b"secret",
            0,
            10,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        session.transient.upsert_resources([resource]);
        let resource = session.transient.resources.get(&resource_id).expect("registered resource").clone();
        let proxy_session_id = session.proxy_session_id.clone();
        let lookup_key = TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "ts");
        let fetch_token = match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 0, 10) {
            TransientObjectFetchDecision::Fetch(token) => token,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                panic!("new transient object starts one physical cache fill")
            }
        };
        (lookup_key, fetch_token, resource.kind)
    };
    assert_ne!(&lookup_key, fetch_token.cache_key());
    gc.cache
        .write_bytes_and_commit(fetch_token.cache_key(), b"transient-body")
        .await
        .expect("physical generation writes");
    gc.cache.write_bytes_and_commit(&lookup_key, b"logical-decoy").await.expect("logical lookup decoy writes");
    assert!(session.write().await.commit_transient_object_ready_if_current(
        resource_kind,
        &fetch_token,
        "video/mp2t".to_string(),
        14,
        0,
        10,
    ));

    let report = gc.run_once(20).await.expect("gc should run");

    assert_eq!(report.transient_objects_deleted, 1);
    assert_eq!(report.transient_object_bytes_deleted, 14);
    assert!(gc.cache.metadata(fetch_token.cache_key()).await.expect("physical metadata read").is_none());
    assert!(gc.cache.metadata(&lookup_key).await.expect("logical metadata read").is_some());
}

#[test]
fn garbage_collection_preserves_playback_reserve_and_cutover_deadline() {
    use crate::{
        media_reserve::{
            evaluate_lease_reserve, HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsLeasePlaybackCursor,
            HlsLeaseReserveInput, HlsManifestCommitIdentity, HlsManifestDeliveryMode,
        },
        recovery_timing::{HlsLeaseCutoverTiming, HlsRecoveryTriggerBudgetMs},
        terminal_tail::HlsMediaContainer,
    };
    use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};

    let mut session = HlsSession::new(crate::HlsSessionKey::new(1, "gc-probe"), b"probe", 1_000);
    let mut body = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:0\n");
    for seq in 0..9 {
        write!(body, "#EXTINF:12.0,\n{seq}.ts\n").expect("test manifest writes");
    }
    let OriginManifestParseOutcome::Normal(origin) =
        parse_origin_media_manifest(&body, "http://origin.example/index.m3u8")
    else {
        panic!("normal manifest")
    };
    session.apply_origin_manifest(&origin).unwrap();
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1_000 };
    }
    session.publishable_origin_head_proxy_seq = Some(4);
    let mut cursor = HlsLeasePlaybackCursor::default();
    for seq in 0..8 {
        let token = cursor.record_request_started(seq, 1_000 + seq * 10_000);
        let token = cursor.bind_request_timing(token, &session.segments[&seq]);
        let _ = cursor.record_request_completed(token, 1_000 + seq * 10_000);
    }
    let manifest = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 71_000,
        first_proxy_seq: 4,
        last_proxy_seq: 6,
        visible_segments: (4..7)
            .map(|seq| HlsLeaseManifestSegment {
                proxy_seq: seq,
                duration_ms: 12_000,
                uri: format!("{seq}.ts").into(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: None,
            })
            .collect::<Vec<_>>()
            .into(),
        discontinuity_sequence: 0,
        target_duration_ms: 12_000,
        playlist_duration_ms: 36_000,
        last_visible_media_end_ms: 36_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    };
    let evaluate = |session: &HlsSession| {
        let timeline = session.ready_timeline_snapshot(cursor.ready_timeline_start_proxy_seq(4), 71_000);
        let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
            manifest: &manifest,
            cursor: &cursor,
            ready_timeline: &timeline,
            now_ms: 71_000,
            playback_rate_guard_milli: 1_050,
            recovery_trigger_budget: HlsRecoveryTriggerBudgetMs::from_millis(4_000),
            origin_path_degraded: true,
            recovery_committed: false,
        });
        (timeline.units.len(), reserve)
    };
    let (before_units, before) = evaluate(&session);
    assert_eq!(before.guaranteed_reserve_ms, 34_500);
    assert!(!ProtectedSet::from_session(&session).segment_proxy_seqs.contains(&0));
    // Run the actual metadata-removal function used by garbage collection.
    assert!(remove_segment_entry(&mut session, 0).is_some());
    let (after_units, after) = evaluate(&session);
    let still_ready = session.ready_timeline_snapshot(4, 71_000);
    assert_eq!(still_ready.units.len(), 5);
    assert_eq!(cursor.first_requested_proxy_seq, Some(0));
    assert_eq!(before_units, 5);
    assert_eq!(after_units, 5);
    assert_eq!(after, before);
    let timing =
        HlsLeaseCutoverTiming::from_reserve(71_000, after.guaranteed_reserve_ms, after.transition_margin, None);
    assert_eq!(timing.latest_safe_terminal_commit_at.as_millis_since_epoch(), 93_500);
    // Removing the entire historical prefix must keep exactly the same media clock.
    for seq in 1..4 {
        assert!(remove_segment_entry(&mut session, seq).is_some());
    }
    assert_eq!(evaluate(&session).1, before);
    // A real hole in the current READY span must still shorten the reserve.
    assert!(remove_segment_entry(&mut session, 7).is_some());
    assert_eq!(evaluate(&session).1.guaranteed_reserve_ms, 10_500);
}

fn install_fast_start(session: &mut HlsSession) {
    let config = tuliprox_core::model::HlsStartupConfig {
        mode: shared::model::HlsStartupMode::Optimistic,
        ..Default::default()
    };
    session.startup = Some(crate::HlsSessionStartup {
        config: config.clone(),
        worker: std::sync::Weak::new(),
        access_leases: Arc::new(RwLock::new(HlsAccessLeaseStore::default())),
        revisions: std::collections::BTreeMap::new(),
        policy_fixed: true,
        store: Arc::new(crate::SegmentRevisionStore::default()),
        budget: crate::ProgressiveBudgetManager::new(config),
    });
}

#[test]
fn metadata_only_prefix_does_not_block_reclaimable_ready_segments() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "review-fifo-prefix"), b"secret", 0);
    session.apply_origin_manifest(&six_segment_manifest()).unwrap();
    install_fast_start(&mut session);
    for seq in [3, 4, 5] {
        session.segments.get_mut(&seq).unwrap().status =
            SegmentCacheStatus::Ready { content_length: 1_000, ready_at_ms: 0 };
    }
    session.apply_origin_manifest(&normal_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:4,\n7.ts\n#EXTINF:4,\n8.ts\n#EXTINF:4,\n9.ts\n#EXTINF:4,\n10.ts\n#EXTINF:4,\n11.ts\n#EXTINF:4,\n12.ts\n"
    )).unwrap();
    let protected = ProtectedSet::from_session(&session);
    assert!(!protected.segment_proxy_seqs.contains(&3));
    assert!(matches!(session.segments[&0].status, SegmentCacheStatus::Discovered));
    let reclaim = crate::gc::selection::fifo_head_size_candidate(&session, &protected);
    assert_eq!(
        reclaim.map(|entry| entry.proxy_seq),
        Some(3),
        "unfetched metadata 0..2 must not prevent reclamation of expired media 3..5"
    );
    assert_eq!(
        crate::gc::selection::duration_expired_head_segment(&session, &protected, &test_policy(), 100_000),
        Some(3)
    );
    // The first actual media object still blocks FIFO if it is protected or unexpired.
    assert_eq!(crate::gc::selection::duration_expired_head_segment(&session, &protected, &test_policy(), 0), None);
    let mut protected = protected;
    protected.segment_proxy_seqs.insert(3);
    assert!(crate::gc::selection::fifo_head_size_candidate(&session, &protected).is_none());
    assert_eq!(
        crate::gc::selection::duration_expired_head_segment(&session, &protected, &test_policy(), 100_000),
        None
    );
}

#[test]
fn reinserted_metadata_does_not_block_subsequent_cache_deletions() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "review-fifo-delete"), b"secret", 0);
    session.apply_origin_manifest(&six_segment_manifest()).unwrap();
    install_fast_start(&mut session);
    for entry in session.segments.values_mut() {
        entry.status = SegmentCacheStatus::Ready { content_length: 1_000, ready_at_ms: 0 };
    }
    let protected = ProtectedSet::from_session(&session);
    assert_eq!(crate::gc::selection::fifo_head_size_candidate(&session, &protected).unwrap().proxy_seq, 0);
    remove_segment_entry(&mut session, 0).unwrap();
    let protected = ProtectedSet::from_session(&session);
    assert!(!protected.segment_proxy_seqs.contains(&1));
    let next = crate::gc::selection::fifo_head_size_candidate(&session, &protected);
    assert_eq!(
        next.map(|entry| entry.proxy_seq),
        Some(1),
        "preserving origin mapping must not prevent further capacity reclamation"
    );
    assert_eq!(
        crate::gc::selection::duration_expired_head_segment(&session, &protected, &test_policy(), 100_000),
        Some(1)
    );
    for seq in [1, 2] {
        remove_segment_entry(&mut session, seq).unwrap();
        assert!(matches!(session.segments[&seq].status, SegmentCacheStatus::Discovered));
    }
    let protected = ProtectedSet::from_session(&session);
    assert!(crate::gc::selection::fifo_head_size_candidate(&session, &protected).is_none());
    assert_eq!(
        crate::gc::selection::duration_expired_head_segment(&session, &protected, &test_policy(), 100_000),
        None
    );
}

#[tokio::test]
async fn fast_start_gc_reclaims_multiple_objects_while_retaining_origin_mappings() {
    for size_pressure in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let (gc, session) = gc_with_session(&directory).await;
        update_gc_policy(&gc, |policy| {
            policy.session_idle_timeout_ms = u64::MAX;
            if size_pressure {
                policy.cache_bytes_per_session = 36;
            }
        });
        let keys = {
            let mut state = session.write().await;
            state.apply_origin_manifest(&six_segment_manifest()).unwrap();
            install_fast_start(&mut state);
            for entry in state.segments.values_mut() {
                gc.cache.write_bytes_and_commit(&entry.cache_key, b"segment-body").await.unwrap();
                entry.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
            }
            state.segments.values().map(|entry| entry.cache_key.clone()).collect::<Vec<_>>()
        };
        let report = gc.run_once(if size_pressure { 0 } else { 100_000 }).await.unwrap();
        assert_eq!(report.segments_deleted_duration, if size_pressure { 0 } else { 3 });
        assert_eq!(report.segments_deleted_size_session, if size_pressure { 3 } else { 0 });
        let state = session.read().await;
        assert_eq!(state.origin_to_proxy.len(), 6);
        for seq in 0..3 {
            assert!(matches!(state.segments[&seq].status, SegmentCacheStatus::Discovered));
        }
        for (index, key) in keys.iter().enumerate() {
            assert_eq!(gc.cache.metadata(key).await.unwrap().is_some(), index >= 3);
        }
        drop(state);
        session.write().await.apply_origin_manifest(&rolling_metadata_manifest(7)).unwrap();
        gc.run_once(0).await.unwrap();
        let state = session.read().await;
        for seq in 0..3 {
            assert!(!state.segments.contains_key(&seq));
            assert!(!state.origin_to_proxy.values().any(|mapped| *mapped == seq));
        }
        assert!((6..12).all(|seq| state.segments.contains_key(&seq)));
    }
}

fn rolling_metadata_manifest(first: u64) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
    let mut body = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
    for seq in first..first + 6 {
        writeln!(body, "#EXTINF:4,\n{seq}.ts").unwrap();
    }
    normal_manifest(&body)
}

#[tokio::test]
async fn obsolete_metadata_stays_bounded_across_one_hundred_live_entries() {
    use shared::model::{HlsCacheConfigDto, HlsStartupMode, Secs};
    use tuliprox_core::model::HlsCacheConfig;

    for mode in [HlsStartupMode::FirstReady, HlsStartupMode::Progressive, HlsStartupMode::Optimistic] {
        let directory = tempfile::tempdir().unwrap();
        let mut dto = HlsCacheConfigDto {
            cache_path: Some(directory.path().to_string_lossy().into_owned()),
            cache_duration: Secs::new(1),
            session_idle_timeout: Secs::new(1_000_000),
            ..Default::default()
        };
        dto.startup.mode = mode;
        let manager = crate::HlsProxyManager::with_hls_cache_config(&HlsCacheConfig::from(&dto));
        let session = manager.get_or_create_session(HlsSessionKey::new(1, "metadata-soak"), b"secret", 0).await;
        for round in 0..100 {
            let first = round * 6;
            let keys = {
                let mut state = session.write().await;
                state.apply_origin_manifest_for_host(&rolling_metadata_manifest(first), 1).unwrap();
                let plan = state.plan_live_window(crate::live_window::HlsLiveWindowView::Entry).unwrap();
                assert_eq!(plan.visible, vec![first + 3, first + 4, first + 5]);
                plan.visible
                    .iter()
                    .map(|seq| {
                        let entry = state.segments.get_mut(seq).unwrap();
                        entry.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
                        entry.cache_key.clone()
                    })
                    .collect::<Vec<_>>()
            };
            for key in keys {
                manager.segment_cache().write_bytes_and_commit(&key, b"segment-body").await.unwrap();
            }
            manager.run_garbage_collection_once(100_000 + round).await.unwrap();
            let mut state = session.write().await;
            assert_eq!(state.segments.keys().copied().collect::<Vec<_>>(), (first..first + 6).collect::<Vec<_>>());
            assert_eq!(state.origin_to_proxy.len(), 6);
            assert!(state.origin_to_proxy.values().all(|seq| (first..first + 6).contains(seq)));
            assert_eq!(
                state.segments.values().filter(|entry| matches!(entry.status, SegmentCacheStatus::Discovered)).count(),
                3
            );
            // Refreshing the same origin must reuse even its unfetched mappings.
            state.apply_origin_manifest_for_host(&rolling_metadata_manifest(first), 1).unwrap();
            assert_eq!(state.live_origin_sequences(), (first..first + 6).collect::<Vec<_>>());
            assert_eq!(state.proxy_next_seq, Some(first + 6));
        }
    }
}

#[tokio::test]
async fn obsolete_metadata_is_removed_only_after_publication_and_reader_protection_release() {
    let directory = tempfile::tempdir().unwrap();
    let (gc, session) = gc_with_session(&directory).await;
    let lease_id = HlsAccessLeaseId("metadata-owner".into());
    {
        let mut state = session.write().await;
        state.apply_origin_manifest(&rolling_metadata_manifest(0)).unwrap();
        for entry in state.segments.values_mut() {
            entry.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        }
        state.render_and_store_manifest(0).unwrap();
        install_fast_start(&mut state);
        for entry in state.segments.values_mut() {
            entry.status = SegmentCacheStatus::Discovered;
        }
        state.apply_origin_manifest(&rolling_metadata_manifest(6)).unwrap();
        state.segments.get_mut(&0).unwrap().access.reader_started(0);
        state.live_window_demands.insert(
            lease_id.clone(),
            crate::live_window::HlsLiveWindowDemand {
                issued_at_ms: 0,
                view: crate::live_window::HlsLiveWindowView::Entry,
                published: vec![1],
            },
        );
        state.install_terminal_tail_protection(
            lease_id.clone(),
            HlsTerminalTailProtection {
                generation: HlsTerminalTailGeneration(1),
                base_proxy_seqs: Arc::from([2]),
                key_bindings: Arc::from([]),
            },
        );
    }
    gc.run_once(0).await.unwrap();
    assert_eq!(session.read().await.segments.len(), 12);
    session.write().await.last_rendered_manifest = None;
    gc.run_once(0).await.unwrap();
    {
        let mut state = session.write().await;
        assert_eq!(state.segments.keys().copied().collect::<Vec<_>>(), vec![0, 1, 2, 6, 7, 8, 9, 10, 11]);
        assert_eq!(state.origin_to_proxy.len(), 9);
        state.segments.get_mut(&0).unwrap().access.reader_finished();
        assert!(state.live_window_demands.remove(&lease_id).is_some());
        assert!(state.remove_terminal_tail_protection(&lease_id).is_some());
    }
    gc.run_once(0).await.unwrap();
    let state = session.read().await;
    assert_eq!(state.segments.keys().copied().collect::<Vec<_>>(), (6..12).collect::<Vec<_>>());
    assert_eq!(state.origin_to_proxy.len(), 6);
}

#[test]
fn obsolete_metadata_cleanup_preserves_non_fast_start_timeline_mappings() {
    for archive in [false, true] {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "retained-metadata"), b"secret", 0);
        session.apply_origin_manifest(&rolling_metadata_manifest(0)).unwrap();
        if archive {
            install_fast_start(&mut session);
            session.origin_source.archive_reference = Some(1_784_898_000);
        }
        session.segments.get_mut(&0).unwrap().discontinuity_before = true;
        session.discontinuity_sequence = 7;
        session.apply_origin_manifest(&rolling_metadata_manifest(6)).unwrap();
        assert!(!session.uses_fast_start_live_window());
        let protected = ProtectedSet::from_session(&session);
        assert!(!protected.segment_proxy_seqs.contains(&0));
        let mappings = session.origin_to_proxy.clone();
        let sequences = session.segments.keys().copied().collect::<Vec<_>>();
        let head = session.publishable_origin_head_proxy_seq;
        let next = session.proxy_next_seq;

        crate::gc::selection::remove_obsolete_segment_metadata(&mut session, &protected);

        assert_eq!(session.origin_to_proxy, mappings, "archive={archive}");
        assert_eq!(session.segments.keys().copied().collect::<Vec<_>>(), sequences);
        assert_eq!(session.discontinuity_sequence, 7);
        assert_eq!(session.publishable_origin_head_proxy_seq, head);
        let mut overlapping = rolling_metadata_manifest(0);
        overlapping.segments.extend(rolling_metadata_manifest(6).segments);
        session.apply_origin_manifest(&overlapping).unwrap();
        assert_eq!(session.proxy_next_seq, next, "an overlapping URI must retain its original proxy sequence");
    }
}

#[test]
fn obsolete_metadata_cleanup_requires_origin_evidence_and_preserves_other_cache_states() {
    use crate::gc::{policy::protected_set_for_cache_reclamation, selection::remove_obsolete_segment_metadata};

    let directory = tempfile::tempdir().unwrap();
    let cache = HlsSegmentCache::with_cache_path(directory.path());
    let mut session = HlsSession::new(HlsSessionKey::new(1, "metadata-states"), b"secret", 0);
    session.apply_origin_manifest(&rolling_metadata_manifest(0)).unwrap();
    install_fast_start(&mut session);
    session.origin_window_proxy_seqs = None;
    remove_obsolete_segment_metadata(&mut session, &ProtectedSet::default());
    assert_eq!(session.segments.len(), 6);
    session.origin_window_proxy_seqs = Some(Vec::new());
    for (seq, status) in [
        SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 },
        SegmentCacheStatus::Fetching { priority: SegmentFetchPriority::Demand, started_at_ms: 0 },
        SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Demand, queued_at_ms: 0 },
        SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Demand, deferred_at_ms: 0 },
        SegmentCacheStatus::FailedRetryable { failed_at_ms: 0, retry_after_ms: 0 },
    ]
    .into_iter()
    .enumerate()
    {
        session.segments.get_mut(&u64::try_from(seq).unwrap()).unwrap().status = status;
    }
    let excluded = cache.object_path(&session.segments[&5].cache_key);
    let protected = protected_set_for_cache_reclamation(&session, &cache, Some(&excluded), None);
    remove_obsolete_segment_metadata(&mut session, &protected);
    assert_eq!(session.segments.len(), 6);
    let protected = protected_set_for_cache_reclamation(&session, &cache, None, None);
    remove_obsolete_segment_metadata(&mut session, &protected);
    assert!(!session.segments.contains_key(&5));
    assert_eq!(session.segments.len(), 5);
    assert_eq!(session.origin_to_proxy.len(), 5);
}
