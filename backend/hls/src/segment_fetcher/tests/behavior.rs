use super::{
    clear_scheduled_prefetch, cold_startup_round, commit_test_key_ready, committed_segment, encode_test_body,
    encrypted_fetch_context, fetch_context, fetch_context_with_access_lease, grant_usable_worker_access_lease,
    grant_usable_worker_access_lease_at, install_startup, normal_manifest, prefix_drop_rounds,
    shared_key_fetch_and_wait, spawn_segment_server, spawn_sequence_response_server,
    take_queued_segment_fetch_candidate, temp_cache_files, test_segment_repair_manager,
    wait_for_segment_key_dependency, SegmentFetchContext, SegmentFetchPolicy, SegmentFetchPriority,
    TestContentEncoding, TestOriginResponse, TestSegmentServer,
};
use crate::{
    HlsSegmentCache, HlsSegmentWorkerPool, HlsSessionKey, HlsSessionStore, RenderedManifest, SegmentCacheStatus,
    TimelineMapError, TransientObjectFetchDecision, TransientResourceKind, TransientResourceRef,
};
use axum::http::HeaderMap;
use shared::model::{HlsCacheConfigDto, HlsStripMode};
use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{oneshot, Mutex},
};
use tuliprox_core::{model::HlsCacheConfig, utils::current_time_millis};
use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};

#[test]
fn missing_segment_behind_ready_media_remains_a_readiness_failure() -> std::io::Result<()> {
    let mut session = crate::HlsSession::new(HlsSessionKey::new(1, "startup"), b"secret", 0);
    session
        .apply_origin_manifest(&normal_manifest("#EXTM3U\n#EXTINF:4,\n0.ts\n#EXTINF:4,\n1.ts\n#EXTINF:4,\n2.ts\n"))
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let head = session.publishable_origin_head_proxy_seq.ok_or_else(|| std::io::Error::other("head"))?;
    session.segments.get_mut(&head).ok_or_else(|| std::io::Error::other("head entry"))?.status =
        SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 0 };
    session.segments.get_mut(&(head + 1)).ok_or_else(|| std::io::Error::other("missing entry"))?.status =
        SegmentCacheStatus::FailedPermanent { failed_at_ms: 1, status: Some(axum::http::StatusCode::GONE) };
    super::super::recompute_unpublished_live_head(&mut session);
    assert_eq!(session.publishable_origin_head_proxy_seq, Some(head));
    assert_eq!(session.origin_control.path_condition, crate::HlsOriginPathCondition::SegmentReadinessFailure);
    Ok(())
}

#[tokio::test]
async fn popped_candidate_without_usable_fetch_binding_returns_to_discovered() {
    let server = spawn_sequence_response_server(vec![TestOriginResponse {
        status: 200,
        headers: Vec::new(),
        body: b"unused".to_vec(),
    }])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy::default();
    let (_, context, _) = fetch_context(&server, &temp_dir, &policy).await;
    let mut session = context.session.write().await;
    let entry = session.segments.get_mut(&1).expect("segment");
    entry.status = SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, queued_at_ms: 1 };
    entry.origin_fetch_ref = None;

    assert!(take_queued_segment_fetch_candidate(&mut session, 1, SegmentFetchPriority::Prefetch, 10, true,).is_none());
    assert!(matches!(session.segments[&1].status, SegmentCacheStatus::Discovered));
}

#[test]
fn object_failure_threshold_is_independent_from_initial_strip() {
    let mut without_strip = HlsCacheConfigDto::default();
    without_strip.strip.mode = HlsStripMode::Segments;
    without_strip.strip.value = 0;
    let mut with_large_strip = without_strip.clone();
    with_large_strip.strip.mode = HlsStripMode::Seconds;
    with_large_strip.strip.value = u64::MAX;

    let without_strip = SegmentFetchPolicy::from_config(&HlsCacheConfig::from(&without_strip));
    let with_large_strip = SegmentFetchPolicy::from_config(&HlsCacheConfig::from(&with_large_strip));

    assert_eq!(without_strip.permanent_failure_segment_threshold, with_large_strip.permanent_failure_segment_threshold,);
    assert_eq!(
        without_strip.permanent_failure_segment_threshold,
        SegmentFetchPolicy::default().permanent_failure_segment_threshold
    );
}

#[tokio::test]
async fn segment_fetch_snapshot_uses_concrete_final_segment_fetch_url() {
    let manifest = match parse_origin_media_manifest(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:4.0,\nmedia/seg001.ts\n",
        "https://cdn.example.net/live/redirected/playlist.m3u8",
    ) {
        OriginManifestParseOutcome::Normal(manifest) => manifest,
        OriginManifestParseOutcome::TransientPassthrough { reason } => {
            panic!("expected normal manifest: {reason:?}")
        }
    };
    let store = HlsSessionStore::new();
    let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"secret", 0).await;
    {
        let mut session = session.write().await;
        session.configure_segment_prefetch_queue(SegmentFetchPolicy::default().max_prefetch_queue_depth);
        session.apply_origin_manifest(&manifest).expect("manifest maps");
        session.queue_manifest_prefetch_candidates(10);
    }

    let worker = Arc::new(HlsSegmentWorkerPool::new(SegmentFetchPolicy::default()));
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    grant_usable_worker_access_lease(&worker, &proxy_session_id).await;
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let context = SegmentFetchContext {
        session: Arc::clone(&session),
        segment_cache: Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path())),
        segment_repair: test_segment_repair_manager(),
        repair_access_lease_id: None,
        headers: HeaderMap::new(),
        origin_provider_session_headers: HeaderMap::new(),
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::new(),
        use_manual_redirects: false,
        origin_io: None,
    };

    let snapshot =
        worker.next_fetch_snapshot(&context, 11, &SegmentFetchPolicy::default()).await.expect("segment snapshot");

    assert_eq!(snapshot.fetch_ref.resolved_origin_url, "https://cdn.example.net/live/redirected/media/seg001.ts");
    assert_eq!(snapshot.fetch_ref.byte_range, None);
    assert_eq!(snapshot.origin_seq, 10);
}

pub(in crate::segment_fetcher::tests) async fn spawn_controlled_segment_server(
) -> (TestSegmentServer, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let task_requests = Arc::clone(&requests);
    let (request_seen_tx, request_seen_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 2048];
            let Ok(read) = socket.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        task_requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
        let _ = request_seen_tx.send(());
        if release_rx.await.is_err() {
            return;
        }
        let body = b"controlled-segment";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body).await;
    });
    (TestSegmentServer { base_url: format!("http://{addr}"), requests, task }, request_seen_rx, release_tx)
}

pub(in crate::segment_fetcher::tests) async fn assert_only_one_prefetch_is_active_before_controlled_release(
    policy: SegmentFetchPolicy,
) {
    let (server, request_seen, release_response) = spawn_controlled_segment_server().await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let (worker, context, _) = fetch_context(&server, &temp_dir, &policy).await;

    worker.wake_scheduler(context.clone(), 20).await;
    tokio::time::timeout(Duration::from_secs(10), request_seen)
        .await
        .expect("origin request starts before test deadline")
        .expect("controlled origin observes the request");

    let (active_proxy_seq, completion_notifier) = {
        let mut session = context.session.write().await;
        let fetching_proxy_seqs = session
            .segments
            .iter()
            .filter_map(|(proxy_seq, segment)| {
                matches!(segment.status, SegmentCacheStatus::Fetching { .. }).then_some(*proxy_seq)
            })
            .collect::<Vec<_>>();
        assert_eq!(session.active_segment_fetches, 1);
        assert_eq!(fetching_proxy_seqs.len(), 1);
        let active_proxy_seq = fetching_proxy_seqs[0];
        let completion_notifier = session.segment_fetch_notifiers.entry(active_proxy_seq).or_default().clone();
        session.invalidate_queued_origin_work();
        (active_proxy_seq, completion_notifier)
    };
    assert_eq!(server.requests.lock().await.len(), 1);

    let completion = completion_notifier.notified();
    tokio::pin!(completion);
    completion.as_mut().enable();
    release_response.send(()).expect("controlled origin response releases");
    tokio::time::timeout(Duration::from_secs(10), completion.as_mut())
        .await
        .expect("active prefetch completes before test deadline");

    let session = context.session.read().await;
    assert_eq!(session.active_segment_fetches, 0);
    assert!(matches!(
        session.segments.get(&active_proxy_seq).map(|segment| &segment.status),
        Some(SegmentCacheStatus::Discovered)
    ));
}

#[tokio::test]
async fn ready_encrypted_segments_schedule_one_key_only_fetch() {
    let server = spawn_sequence_response_server(vec![TestOriginResponse {
        status: 200,
        headers: Vec::new(),
        body: b"0123456789abcdef".to_vec(),
    }])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy =
        SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
    let (worker, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
    let proxy_session_id = {
        let mut session = context.session.write().await;
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 32, ready_at_ms: 10 };
        }
        session.last_rendered_manifest = Some(RenderedManifest {
            body: "#EXTM3U\n".to_string(),
            first_proxy_seq: 1,
            last_proxy_seq: 3,
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            playlist_duration_ms: 12_000,
            valid_until_ms: u64::MAX,
            render_gap_segments: 0,
            rendered_at_ms: 10,
            segment_proxy_seqs: vec![1, 2, 3],
        });
        session.proxy_session_id.clone()
    };
    let now_ms = current_time_millis();
    grant_usable_worker_access_lease_at(&worker, &proxy_session_id, now_ms).await;

    tokio::join!(worker.wake_scheduler(context.clone(), now_ms), worker.wake_scheduler(context.clone(), now_ms));

    let pending_key = {
        let mut session = context.session.write().await;
        let resource = session
            .transient
            .resources
            .values()
            .find(|resource| resource.kind == TransientResourceKind::Key)
            .cloned()
            .expect("encrypted fixture key resource");
        let extension = resource.file_ext_hint.clone().expect("key extension");
        let proxy_session_id = session.proxy_session_id.clone();
        let cache_duration_ms = session.transient.resource_ttl_ms;
        match session.transient.begin_object_fetch(&proxy_session_id, &resource, &extension, now_ms, cache_duration_ms)
        {
            TransientObjectFetchDecision::Ready => None,
            TransientObjectFetchDecision::Wait(notifier) => Some((notifier, resource.id, extension)),
            TransientObjectFetchDecision::Fetch(_) => {
                panic!("rendered READY media must already have scheduled its key-only fetch")
            }
        }
    };
    if let Some((notifier, resource_id, extension)) = pending_key {
        tokio::time::timeout(
            Duration::from_secs(10),
            wait_for_segment_key_dependency(&context, notifier, resource_id, extension, Duration::from_secs(10)),
        )
        .await
        .expect("key-only readiness completes before test deadline")
        .expect("key-only readiness succeeds");
    }

    let session = context.session.read().await;
    assert_eq!(session.active_segment_fetches, 0);
    assert!(session.ready_timeline_snapshot(1, now_ms).units.iter().all(|unit| unit.required_key_ready));
    assert!(session.segments.values().all(|segment| matches!(segment.status, SegmentCacheStatus::Ready { .. })));
    drop(session);
    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /key.key "));
}

#[tokio::test]
async fn completed_shared_key_before_wait_registration_is_observed_without_timeout() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy::default();
    let (_, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
    let now_ms = current_time_millis();
    let (fetch_token, notifier, resource_id, extension) = shared_key_fetch_and_wait(&context, now_ms).await;
    commit_test_key_ready(&context, &fetch_token, now_ms.saturating_add(1)).await;

    let result =
        wait_for_segment_key_dependency(&context, notifier, resource_id, extension, Duration::from_millis(1)).await;

    assert!(result.is_ok());
}

#[tokio::test]
async fn invalid_aes_key_size_blocks_segment_fetch_and_ready_reserve() {
    let server = spawn_sequence_response_server(vec![TestOriginResponse {
        status: 200,
        headers: Vec::new(),
        body: vec![b'k'; 15],
    }])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy =
        SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
    let (worker, context, segment_file) = encrypted_fetch_context(&server, &temp_dir, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Unavailable);
    let session = context.session.read().await;
    let first = session.segments.get(&1).expect("encrypted segment");
    assert!(matches!(first.status, SegmentCacheStatus::FailedPermanent { .. }));
    assert!(!session.ready_timeline_snapshot(1, 20).units[0].required_key_ready);
    assert_eq!(session.activity.media_readiness_generation, 0);
    drop(session);
    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /key.key "));
}

#[tokio::test]
async fn invalidated_generation_discards_late_segment_completion_without_sleep() {
    let (server, request_seen, release_response) = spawn_controlled_segment_server().await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;
    let fetch_context = context.clone();
    let fetch = tokio::spawn(async move { worker.demand_fetch_and_wait(fetch_context, &segment_file, 20).await });

    request_seen.await.expect("origin request starts");
    context.session.write().await.invalidate_queued_origin_work();
    release_response.send(()).expect("origin response released");

    assert_eq!(fetch.await.expect("fetch task joins"), super::super::SegmentDemandFetchOutcome::Unavailable);
    let cache_key = {
        let session = context.session.read().await;
        assert_eq!(session.active_segment_fetches, 0);
        let segment = session.segments.get(&1).expect("segment remains mapped");
        assert!(matches!(segment.status, SegmentCacheStatus::Discovered));
        segment.cache_key.clone()
    };
    assert!(context.segment_cache.metadata(&cache_key).await.expect("metadata reads").is_none());
}

#[tokio::test]
async fn same_origin_sequence_resource_conflict_preserves_in_flight_binding() {
    let (server, request_seen, release_response) = spawn_controlled_segment_server().await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;
    let fetch_context = context.clone();
    let fetch = tokio::spawn(async move { worker.demand_fetch_and_wait(fetch_context, &segment_file, 20).await });

    request_seen.await.expect("origin request starts");
    let key_uri = format!("{}/rebound.key", server.base_url);
    let key_resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        &key_uri,
        b"secret",
        0,
        u64::MAX,
        Some("key".to_string()),
    );
    let mut rebound_manifest = normal_manifest(&format!(
        "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"\n\
         #EXTINF:4.0,\n{}/rebound.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4.0,\n{}/2.ts\n\
         #EXTINF:4.0,\n{}/3.ts\n",
        server.base_url, server.base_url, server.base_url
    ));
    for encryption in rebound_manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
        encryption.proxy_resource_id = Some(key_resource.id.0.clone());
        encryption.proxy_resource_extension = Some("key".to_string());
    }
    let (origin_work_generation, cache_key) = {
        let mut session = context.session.write().await;
        let origin_work_generation = session.activity.origin_work_generation;
        session.transient.upsert_resources([key_resource]);
        assert!(matches!(
            session.apply_origin_manifest(&rebound_manifest),
            Err(TimelineMapError::OriginSequenceResourceConflict { candidate_origin_seq: 1, .. })
        ));
        assert_eq!(session.activity.origin_work_generation, origin_work_generation);
        let segment = session.segments.get(&1).expect("original proxy segment remains mapped");
        assert!(matches!(segment.status, SegmentCacheStatus::Fetching { .. }));
        assert_eq!(
            segment.origin_fetch_ref.as_ref().expect("original fetch ref").resolved_origin_url,
            format!("{}/1.ts", server.base_url)
        );
        assert!(segment.encryption.is_none());
        (origin_work_generation, segment.cache_key.clone())
    };

    release_response.send(()).expect("origin response released");

    assert_eq!(fetch.await.expect("fetch task joins"), super::super::SegmentDemandFetchOutcome::Ready);
    let session = context.session.read().await;
    assert_eq!(session.activity.origin_work_generation, origin_work_generation);
    assert_eq!(session.active_segment_fetches, 0);
    let segment = session.segments.get(&1).expect("original segment remains mapped");
    assert!(matches!(segment.status, SegmentCacheStatus::Ready { .. }));
    assert_eq!(
        segment.origin_fetch_ref.as_ref().expect("original fetch ref remains authoritative").resolved_origin_url,
        format!("{}/1.ts", server.base_url)
    );
    assert!(segment.encryption.is_none());
    drop(session);
    let (committed_key, bytes) = committed_segment(&context, 1).await;
    assert_eq!(committed_key, cache_key);
    assert_eq!(bytes, b"controlled-segment");
    assert!(temp_cache_files(temp_dir.path()).is_empty());
}

#[tokio::test]
async fn segment_declared_codings_decode_to_identity_before_cache_and_preserve_opaque_bytes() {
    let identity_bytes = b"\x00\xffopaque-hls-ciphertext\x1f\x8bmedia".to_vec();

    for encoding in [
        TestContentEncoding::Gzip,
        TestContentEncoding::RawDeflate,
        TestContentEncoding::Brotli,
        TestContentEncoding::Zstd,
    ] {
        let encoded = encode_test_body(&identity_bytes, encoding).await;
        let server =
            spawn_sequence_response_server(vec![TestOriginResponse::encoded(encoding.header_value(), encoded)]).await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let policy = SegmentFetchPolicy {
            retry_delays_ms: [0, 0, 0, 0, 0],
            retry_jitter_max_ms: 0,
            ..SegmentFetchPolicy::default()
        };
        let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
        clear_scheduled_prefetch(&context, &policy).await;

        let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

        assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready, "failed for {encoding:?}");
        let (cache_key, cached) = committed_segment(&context, 1).await;
        assert_eq!(cached, identity_bytes, "cache retained HTTP coding for {encoding:?}");
        let mut range = context.segment_cache.open_range(&cache_key, 5).await.expect("cache range opens");
        let mut ranged = Vec::new();
        range.read_to_end(&mut ranged).await.expect("cache range reads");
        assert_eq!(ranged, identity_bytes[5..], "range did not address Identity bytes for {encoding:?}");
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 1, "unexpected retries for {encoding:?}");
        assert!(
            requests[0].to_ascii_lowercase().contains("accept-encoding: identity"),
            "request did not enforce identity for {encoding:?}"
        );
    }
}

#[tokio::test]
async fn segment_decoder_failure_exhausts_attempt_budget_without_cache_or_temp_file() {
    let valid = encode_test_body(b"never committed", TestContentEncoding::Gzip).await;
    let mut corrupt = valid;
    corrupt.truncate(corrupt.len() / 2);
    let server =
        spawn_sequence_response_server((0..5).map(|_| TestOriginResponse::encoded("gzip", corrupt.clone())).collect())
            .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::TimedOut);
    assert_eq!(server.requests.lock().await.len(), 5);
    let cache_key = context.session.read().await.segments.get(&1).expect("segment").cache_key.clone();
    assert!(context.segment_cache.metadata(&cache_key).await.expect("metadata reads").is_none());
    assert!(!context.segment_cache.has_active_temp_files());
    assert!(temp_cache_files(temp_dir.path()).is_empty());
}

#[tokio::test]
async fn background_segment_fetch_without_usable_access_lease_resets_queue_without_origin_request() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, _) = fetch_context_with_access_lease(&server, &temp_dir, &policy, false).await;

    worker.wake_scheduler(context.clone(), 20).await;

    assert!(server.requests.lock().await.is_empty());
    let session = context.session.read().await;
    assert_eq!(session.active_segment_fetches, 0);
    assert!(session.segment_prefetch_queue.is_empty());
    assert!(session.segments.values().all(|segment| matches!(segment.status, SegmentCacheStatus::Discovered)));
}

#[tokio::test]
async fn demand_fetch_starts_without_worker_usable_access_lease() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context_with_access_lease(&server, &temp_dir, &policy, false).await;
    clear_scheduled_prefetch(&context, &policy).await;

    let outcome = worker.demand_fetch_and_wait(context.clone(), &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(server.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn demand_fetch_returns_unavailable_when_fetch_slots_are_saturated() {
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
    clear_scheduled_prefetch(&context, &policy).await;
    context.session.write().await.active_segment_fetches = 1;

    let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Unavailable);
    assert!(server.requests.lock().await.is_empty());
}

#[tokio::test]
async fn ready_cache_hit_is_allowed_when_fetch_slots_are_saturated() {
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
        session.active_segment_fetches = 1;
        session.segments.get_mut(&1).expect("segment").status =
            SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 20 };
    }

    let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::Ready);
    assert!(server.requests.lock().await.is_empty());
}

#[tokio::test]
async fn one_proxy_sequence_has_at_most_one_active_origin_fetch() {
    let server = spawn_segment_server(80).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        max_global_segment_fetches: 4,
        max_session_segment_fetches: 4,
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    clear_scheduled_prefetch(&context, &policy).await;

    let first = {
        let worker = Arc::clone(&worker);
        let context = context.clone();
        let segment_file = segment_file.clone();
        tokio::spawn(async move { worker.demand_fetch_and_wait(context, &segment_file, 20).await })
    };
    let second = {
        let worker = Arc::clone(&worker);
        let context = context.clone();
        let segment_file = segment_file.clone();
        tokio::spawn(async move { worker.demand_fetch_and_wait(context, &segment_file, 21).await })
    };

    assert_eq!(first.await.expect("task"), super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(second.await.expect("task"), super::super::SegmentDemandFetchOutcome::Ready);
    assert_eq!(server.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn session_limit_is_respected() {
    let policy = SegmentFetchPolicy {
        max_global_segment_fetches: 4,
        max_session_segment_fetches: 1,
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };

    assert_only_one_prefetch_is_active_before_controlled_release(policy).await;
}

#[tokio::test]
async fn global_limit_is_respected() {
    let policy = SegmentFetchPolicy {
        max_global_segment_fetches: 1,
        max_session_segment_fetches: 3,
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };

    assert_only_one_prefetch_is_active_before_controlled_release(policy).await;
}

#[tokio::test]
async fn unknown_body_reserves_more_disk_before_growing_its_raw_spool() -> std::io::Result<()> {
    let server = spawn_sequence_response_server(Vec::new()).await;
    let directory = tempfile::tempdir()?;
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    install_startup(&context, &worker, shared::model::HlsStartupMode::Progressive).await;
    let snapshot = worker
        .next_fetch_snapshot(&context, 10, &policy)
        .await
        .ok_or_else(|| std::io::Error::other("fetch snapshot"))?;
    let prepared = super::super::prepare_startup_fill(
        &context,
        &snapshot,
        None,
        tokio::time::Instant::now() + Duration::from_secs(1),
    )
    .await
    .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let mut observed = prepared.observed.lock().await;
    observed.limit = 8;
    observed.observe(b"segment-body").await?;
    assert_eq!(observed.additional_reservations.len(), 1);
    observed.finish().await?;
    let metadata =
        prepared.revision.revision().wait_complete(tokio::time::Instant::now() + Duration::from_secs(1)).await?;
    assert_eq!(tokio::fs::read(metadata.path).await?, b"segment-body");
    Ok(())
}

#[tokio::test]
#[ignore = "extended single-process loopback startup, origin-drop and retention soak"]
async fn extended_fast_start_soak() -> std::io::Result<()> {
    let seconds = std::env::var("TULIPROX_HLS_SOAK_SECS")
        .ok()
        .map_or(Ok(1800), |value| value.parse::<u64>().map_err(std::io::Error::other))?;
    let directory = tempfile::tempdir()?;
    let store = Arc::new(crate::SegmentRevisionStore::default());
    let budget = crate::ProgressiveBudgetManager::new(tuliprox_core::model::HlsStartupConfig::default());
    let cache = Arc::new(HlsSegmentCache::with_cache_path(directory.path()));
    cache.update_cache_limits(1000, 500);
    let started = tokio::time::Instant::now();
    let end = started + Duration::from_secs(seconds);
    let mut cycles = 0_u64;
    let mut reported = 0_u64;
    while tokio::time::Instant::now() < end {
        let mode = if cycles.is_multiple_of(2) {
            shared::model::HlsStartupMode::FirstReady
        } else {
            shared::model::HlsStartupMode::Progressive
        };
        cold_startup_round(mode, Some((&directory, &store, &budget, &cache))).await?;
        if cycles.is_multiple_of(10) {
            prefix_drop_rounds().await?;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        for owner in store.retire_unpinned()? {
            owner.file_pin.lock().map_err(|_| std::io::Error::other("file pin"))?.take();
            cache.delete_if_inactive(&owner.key).await?;
        }
        assert_eq!(budget.usage(), (0, 0));
        assert_eq!(cache.scan_revision_disk_bytes().await?, 0);
        assert!(store.retire_unpinned()?.is_empty());
        let tasks = tokio::runtime::Handle::current().metrics().num_alive_tasks();
        assert!(tasks <= 8, "completed startup retained {tasks} tasks");
        cycles += 1;
        let elapsed = started.elapsed().as_secs();
        if elapsed / 60 > reported {
            reported = elapsed / 60;
            let rss = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|status| status.lines().find(|line| line.starts_with("VmRSS:")).map(str::to_owned))
                .unwrap_or_default();
            println!("startup_soak elapsed_secs={elapsed} cycles={cycles} live_tasks={tasks} replay_bytes=0 revision_disk_bytes=0 {rss}");
        }
    }
    println!("startup_soak completed elapsed_secs={} cycles={cycles}", started.elapsed().as_secs());
    Ok(())
}

#[tokio::test]
async fn a_published_raw_session_rejects_later_incompatible_segments() -> std::io::Result<()> {
    let server = spawn_segment_server(0).await;
    let directory = tempfile::tempdir()?;
    let policy = SegmentFetchPolicy::default();
    let (worker, context, _) = fetch_context(&server, &directory, &policy).await;
    install_startup(&context, &worker, shared::model::HlsStartupMode::Progressive).await;
    let mut session = context.session.write().await;
    let id = session.proxy_session_id.clone();
    if let Some(startup) = &mut session.startup {
        for seq in 1..=3 {
            let owner = startup.store.create(id.clone(), seq, crate::SegmentRevisionKind::Raw)?;
            owner.revision().prefix_available.store(8, Ordering::Release);
            startup.revisions.insert(seq, owner);
        }
    }
    session
        .render_and_store_manifest(current_time_millis())
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let successor = session.segments.get_mut(&2).ok_or_else(|| std::io::Error::other("successor"))?;
    successor.proxy_file_ext = "m4s".into();
    assert!(session.render_and_store_manifest(current_time_millis()).is_err());
    assert_eq!(session.startup_mode(), shared::model::HlsStartupMode::Progressive);
    Ok(())
}
