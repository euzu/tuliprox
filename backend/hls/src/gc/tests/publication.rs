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
