use super::*;

#[test]
fn protected_encrypted_segment_also_protects_its_key_resource() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
    let key_resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://origin.example.com/live/final/key.bin",
        b"secret",
        0,
        u64::MAX,
        Some("bin".to_string()),
    );
    let mut manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\n1.ts\n",
        );
    for encryption in manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
        encryption.proxy_resource_id = Some(key_resource.id.0.clone());
        encryption.proxy_resource_extension = Some("bin".to_string());
    }
    session.transient.upsert_resources([key_resource]);
    session.apply_origin_manifest(&manifest).expect("manifest maps");
    let segment = session.segments.get_mut(&0).expect("segment");
    segment.access.reader_started(1);
    let resource_id = segment.encryption.as_ref().expect("encryption").resource_id.clone();

    let protected = ProtectedSet::from_session(&session);

    assert!(protected.segment_proxy_seqs.contains(&0));
    assert!(protected.key_resource_ids.contains(&resource_id));
}

#[tokio::test]
async fn cache_root_handoff_discards_old_logical_deletion_tickets_before_new_root_use() {
    let old_root = tempfile::tempdir().expect("old cache root");
    let new_root = tempfile::tempdir().expect("new cache root");
    let (gc, session) = gc_with_session(&old_root).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let key = crate::SegmentCacheKey::new(proxy_session_id, 1, "ts");
    {
        let mut queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.pending.push_back(PendingCacheObjectDeletion {
            deletion: CacheObjectDeletion::Segment { key: key.clone(), reason: SegmentCacheDeletionReason::Duration },
            attempts: 1,
        });
    }

    assert!(gc.update_cache_path(new_root.path()).await);
    gc.cache.write_bytes_and_commit(&key, b"new-root-sentinel").await.expect("new-root sentinel writes");
    let report = gc.run_once(0).await.expect("new-root gc runs");

    assert_eq!(gc.pending_cache_deletion_count(), 0);
    assert_eq!(report.cache_object_deletions_succeeded, 0);
    let metadata = gc.cache.metadata(&key).await.expect("new-root metadata reads").expect("sentinel remains");
    assert_eq!(tokio::fs::read(metadata.path).await.expect("sentinel reads"), b"new-root-sentinel");
}

#[tokio::test]
async fn global_size_gc_subtracts_map_bytes_after_segment_removal() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    update_gc_policy(&gc, |policy| policy.cache_bytes_global = 25);
    let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init0.mp4\"\n#EXTINF:4.0,\n0.m4s\n#EXT-X-MAP:URI=\"init1.mp4\"\n#EXTINF:4.0,\n1.m4s\n",
        );
    {
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        for (proxy_seq, ready_at_ms) in [(0, 0), (1, 1)] {
            let segment = session.segments.get_mut(&proxy_seq).expect("segment");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
        }
        session.maps.get_mut(&ProxyMapId(0)).expect("first map").status =
            MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
        session.maps.get_mut(&ProxyMapId(1)).expect("second map").status =
            MapCacheStatus::Ready { content_length: 10, ready_at_ms: 1 };
    }

    let report = gc.run_once(100).await.expect("gc should run");

    assert_eq!(report.segments_deleted_size_global, 1);
    assert_eq!(report.maps_deleted, 1);
    let session = session.read().await;
    assert!(!session.segments.contains_key(&0));
    assert!(session.segments.contains_key(&1));
    assert!(!session.maps.contains_key(&ProxyMapId(0)));
    assert!(session.maps.contains_key(&ProxyMapId(1)));
}

#[tokio::test]
async fn transient_object_gc_expires_stale_fetching_metadata_deletes_its_fill_and_wakes_waiters() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    let (lookup_key, fetch_token, notifier) = {
        let mut session = session.write().await;
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/stale.ts",
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
        let notifier = match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 1, 10) {
            TransientObjectFetchDecision::Wait(notifier) => notifier,
            TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Fetch(_) => {
                panic!("a concurrent request waits for the current cache fill")
            }
        };
        (lookup_key, fetch_token, notifier)
    };
    gc.cache
        .write_bytes_and_commit(fetch_token.cache_key(), b"partial")
        .await
        .expect("stale physical fill fixture writes");
    let waiter = notifier.notified();
    tokio::pin!(waiter);
    assert!(matches!(futures::poll!(&mut waiter), Poll::Pending));

    let report = gc.run_once(20).await.expect("gc should run");

    assert!(matches!(futures::poll!(&mut waiter), Poll::Ready(())));
    assert_eq!(report.transient_objects_deleted, 1);
    assert_eq!(report.transient_object_bytes_deleted, 0);
    assert!(!session.read().await.transient.object_cache.contains_key(&lookup_key));
    assert!(gc.cache.metadata(fetch_token.cache_key()).await.expect("metadata reads").is_none());
}

#[tokio::test]
async fn session_size_gc_deletes_transient_objects_before_timeline_segments() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    update_gc_policy(&gc, |policy| policy.cache_bytes_per_session = 20);
    let (segment_key, object_lookup_key, object_fetch_token, resource_kind) = {
        let mut session = session.write().await;
        apply_six_segment_manifest_for_gc(&mut session);
        let segment_key = session.segments.get(&1).expect("segment").cache_key.clone();
        let resource = TransientResourceRef::new(
            TransientResourceKind::Segment,
            "http://origin.example.com/live/object1.ts",
            b"secret",
            100,
            10_000,
            Some("ts".to_string()),
        );
        let resource_id = resource.id.clone();
        session.transient.upsert_resources([resource]);
        let resource = session.transient.resources.get(&resource_id).expect("registered resource").clone();
        let proxy_session_id = session.proxy_session_id.clone();
        let object_lookup_key = TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "ts");
        let object_fetch_token =
            match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 100, 10_000) {
                TransientObjectFetchDecision::Fetch(token) => token,
                TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                    panic!("new transient object starts one physical cache fill")
                }
            };
        (segment_key, object_lookup_key, object_fetch_token, resource.kind)
    };
    gc.cache.write_bytes_and_commit(&segment_key, b"segment-body").await.expect("segment writes");
    gc.cache
        .write_bytes_and_commit(object_fetch_token.cache_key(), b"transient-body")
        .await
        .expect("transient object writes");
    {
        let mut session = session.write().await;
        session.segments.get_mut(&1).expect("segment").status =
            SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 100 };
        assert!(session.commit_transient_object_ready_if_current(
            resource_kind,
            &object_fetch_token,
            "video/mp2t".to_string(),
            14,
            100,
            10_000,
        ));
    }

    let report = gc.run_once(100).await.expect("gc should run");

    assert_eq!(report.transient_objects_deleted, 1);
    assert_eq!(report.segments_deleted_size_session, 0);
    let session = session.read().await;
    assert!(session.segments.contains_key(&1));
    assert!(!session.transient.object_cache.contains_key(&object_lookup_key));
    drop(session);
    assert!(gc.cache.metadata(object_fetch_token.cache_key()).await.expect("metadata reads").is_none());
}

#[tokio::test]
async fn active_transient_resource_reader_protects_idle_session() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let (gc, session) = gc_with_session(&temp_dir).await;
    {
        let mut session = session.write().await;
        let resource = crate::TransientResourceRef::new(
            crate::TransientResourceKind::Segment,
            "http://origin.example.com/live/seg.ts",
            b"secret",
            0,
            10,
            Some("ts".to_string()),
        );
        resource.access.reader_started(1);
        session.transient.upsert_resources([resource]);
    }

    let report = gc.run_once(2_000).await.expect("gc should run");

    assert_eq!(report.sessions_deleted, 0);
    assert_eq!(gc.sessions.len().await, 1);
}
