use super::{
    clear_scheduled_prefetch, commit_failed_segment_fetch, committed_segment, fetch_context,
    grant_usable_worker_access_lease, normal_manifest, spawn_segment_server, spawn_sequence_response_server,
    test_segment_repair_manager, SegmentFetchContext, SegmentFetchPolicy, SegmentFetchPriority, TestOriginResponse,
};
use crate::{
    build_rewrite_secret_fingerprint, GarbageCollectionPolicy, HlsGarbageCollector, HlsOriginResourceFetchError,
    HlsSegmentCache, HlsSegmentFile, HlsSegmentWorkerPool, HlsSessionKey, HlsSessionStore, SegmentCacheStatus,
};
use axum::http::HeaderMap;
use std::sync::Arc;

#[tokio::test]
async fn local_capacity_failure_does_not_change_origin_progress_or_failure_counter() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;
    let snapshot = worker.next_fetch_snapshot(&context, 20, &policy).await.expect("fetch snapshot");
    let error = HlsOriginResourceFetchError::LocalCacheCapacity {
        required_session_bytes: 10,
        required_global_bytes: 0,
        projected_write_bytes: 10,
        revision: context.segment_cache.capacity_revision(),
    };
    let mut session = context.session.write().await;
    let original_path_condition = session.origin_control.path_condition;

    commit_failed_segment_fetch(&mut session, &snapshot, &policy, &error, 21);

    assert_eq!(session.origin_control.path_condition, original_path_condition);
    assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 0);
    assert!(matches!(
        session.segments.get(&snapshot.proxy_seq).map(|segment| &segment.status),
        Some(SegmentCacheStatus::CapacityDeferred { .. })
    ));
}

#[tokio::test]
async fn projected_capacity_reclamation_commits_one_origin_download() {
    let server = spawn_sequence_response_server(vec![TestOriginResponse {
        status: 200,
        headers: Vec::new(),
        body: b"abcdefghij".to_vec(),
    }])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let store = Arc::new(HlsSessionStore::new());
    let session = store.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
    {
        let mut session = session.write().await;
        session.configure_segment_prefetch_queue(policy.max_prefetch_queue_depth);
        session.proxy_next_seq = Some(1);
        session
            .apply_origin_manifest(&normal_manifest(&format!(
                "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n{0}/1.ts\n#EXTINF:4.0,\n{0}/2.ts\n#EXTINF:4.0,\n{0}/3.ts\n#EXTINF:4.0,\n{0}/4.ts\n#EXTINF:4.0,\n{0}/5.ts\n#EXTINF:4.0,\n{0}/6.ts\n",
                server.base_url
            )))
            .expect("manifest maps");
    }
    let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
    let gc = Arc::new(HlsGarbageCollector::new(
        Arc::clone(&store),
        Arc::clone(&cache),
        GarbageCollectionPolicy::default(),
        build_rewrite_secret_fingerprint(b"secret"),
    ));
    cache.install_capacity_reclaimer(&gc);
    let old_keys = {
        let session = session.read().await;
        assert!(session.segments.contains_key(&6), "mapped proxy sequences: {:?}", session.segments.keys());
        [1_u64, 2].map(|proxy_seq| session.segments.get(&proxy_seq).expect("old segment").cache_key.clone())
    };
    for key in &old_keys {
        cache.write_bytes_and_commit(key, b"0123456789").await.expect("old segment commits");
    }
    {
        let mut session = session.write().await;
        for proxy_seq in [1_u64, 2] {
            session.segments.get_mut(&proxy_seq).expect("old segment").status =
                SegmentCacheStatus::Ready { content_length: 10, ready_at_ms: proxy_seq };
        }
    }
    cache.update_cache_limits(100, 25);
    let worker = Arc::new(HlsSegmentWorkerPool::new(policy.clone()));
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    grant_usable_worker_access_lease(&worker, &proxy_session_id).await;
    let context = SegmentFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::clone(&cache),
        segment_repair: test_segment_repair_manager(),
        repair_access_lease_id: None,
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client builds"),
        use_manual_redirects: true,
        origin_io: None,
    };
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker
        .demand_fetch_and_wait(context.clone(), &HlsSegmentFile { proxy_seq: 6, extension: "ts".to_string() }, 20)
        .await;

    let target_status = format!("{:?}", session.read().await.segments.get(&6).map(|segment| &segment.status));
    assert_eq!(
        outcome,
        super::super::SegmentDemandFetchOutcome::Ready,
        "requests={} target_status={target_status}",
        server.requests.lock().await.len(),
    );
    assert_eq!(server.requests.lock().await.len(), 1, "staged body is not downloaded again");
    assert_eq!(committed_segment(&context, 6).await.1, b"abcdefghij");
    let session = session.read().await;
    assert!(!session.segments.contains_key(&1));
    assert!(session.segments.contains_key(&2));
    drop(session);
    let usage = cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
    assert_eq!(usage.session_bytes, 20);
    assert!(!cache.has_active_temp_files());
}

#[tokio::test]
async fn demand_priority_runs_before_prefetch() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        max_global_segment_fetches: 1,
        max_session_segment_fetches: 1,
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    {
        let mut session = context.session.write().await;
        session.segment_prefetch_queue = crate::SegmentPrefetchQueue::new(6);
        session.segments.get_mut(&1).expect("segment").status = SegmentCacheStatus::Discovered;
        session.segments.get_mut(&2).expect("segment").status = SegmentCacheStatus::Discovered;
        session.queue_segment_fetch_candidate(2, SegmentFetchPriority::Prefetch, 10);
    }

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    let requests = server.requests.lock().await;
    let first_request = requests.first().expect("request should be made");
    assert!(first_request.starts_with("GET /1.ts "));
}
