use super::{
    encrypted_fetch_context, fetch_context, spawn_segment_server, spawn_sequence_response_server,
    take_queued_segment_fetch_candidate, SegmentFetchPolicy, SegmentFetchPriority, TestOriginResponse,
};
use crate::{
    api::{
        HlsOriginAccountRuntime, HlsOriginAccountSnapshot, HlsOriginAccountWork, HlsPreparedOriginAccount,
        LiveHlsOriginEntry,
    },
    HlsSegmentFile, SegmentCacheStatus, TransientResourceKind,
};
use std::time::Duration;
use tuliprox_core::utils::current_time_millis;

#[derive(Debug)]
struct InvalidatingPrepare {
    first: std::sync::atomic::AtomicBool,
    handoff: bool,
}

impl HlsOriginAccountRuntime for InvalidatingPrepare {
    fn prepare<'a>(
        &'a self,
        session: &'a crate::HlsSessionHandle,
        _work: HlsOriginAccountWork,
        _class: crate::HlsOriginWorkClass,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<HlsPreparedOriginAccount, crate::HlsBoundAccountAcquireErrorKind>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let mut s = session.write().await;
            if self.first.swap(false, std::sync::atomic::Ordering::AcqRel) {
                if self.handoff {
                    let binding = crate::HlsOriginAccountBinding::new(
                        "input".into(),
                        "new-account".into(),
                        &s.proxy_session_id,
                        current_time_millis(),
                    );
                    s.handoff_origin_account(binding);
                } else {
                    s.invalidate_queued_origin_work();
                }
            }
            Ok(HlsPreparedOriginAccount {
                entry: LiveHlsOriginEntry::parse("http://unused.example/index.m3u8").unwrap(),
                snapshot: HlsOriginAccountSnapshot::from_session(&s),
            })
        })
    }
}

#[tokio::test]
async fn scheduler_replans_valid_manifest_work_after_prepare_invalidates_generation() {
    use std::sync::{atomic::AtomicBool, Arc};
    use tuliprox_core::model::{Config, PlaybackKind};
    use tuliprox_session::ConnectionKind;
    for handoff in [false, true] {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy::default();
        let (worker, mut context, _) = fetch_context(&server, &temp_dir, &policy).await;
        super::support::grant_usable_worker_access_lease_at(
            &worker,
            &context.session.read().await.proxy_session_id,
            current_time_millis(),
        )
        .await;
        let original = context.session.read().await.segment_prefetch_queue.proxy_seqs();
        assert_eq!(original.len(), 3);
        let runtime = Arc::new(InvalidatingPrepare { first: AtomicBool::new(true), handoff });
        context.origin_io = Some(crate::HlsOriginIoContext {
            ctx: crate::HlsCtx::for_test(Config::default()),
            client_addr: "127.0.0.1:12345".parse().unwrap(),
            allow_grace: false,
            priority: 0,
            connection_kind: ConnectionKind::Normal,
            reservation_ttl_secs: 1,
            preacquired_provider_handle: None,
            started_generation: None,
            prepared_account: None,
            playback_kind: PlaybackKind::LiveHls,
            account_runtime: Some(runtime.clone()),
        });
        worker.wake_scheduler(context.clone(), current_time_millis()).await;
        assert!(!runtime.first.load(std::sync::atomic::Ordering::Acquire), "account preparation actually ran");
        assert_eq!(context.session.read().await.activity.origin_work_generation, 1);
        if handoff {
            let s = context.session.read().await;
            assert!(s.origin_account_manifest_pending.is_some());
            assert!(s.segment_prefetch_queue.is_empty());
            assert!(original.iter().all(|seq| s.segments[seq].origin_fetch_ref.is_none()));
            assert!(server.requests.lock().await.is_empty(), "unqualified old-account resources stay invalid");
        } else {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let s = context.session.read().await;
                    if original.iter().all(|seq| matches!(s.segments[seq].status, SegmentCacheStatus::Ready { .. })) {
                        break;
                    }
                    drop(s);
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("valid jobs survive invalidation in prepare without test-side requeue");
            assert_eq!(server.requests.lock().await.len(), original.len());
        }
    }
}

#[tokio::test]
async fn scheduler_replan_uses_current_backpressure_for_prefetch() {
    use crate::HlsBackpressureState;
    use std::sync::{atomic::AtomicBool, Arc};
    use tuliprox_core::model::{Config, PlaybackKind};
    use tuliprox_session::ConnectionKind;

    for expected_pressure in
        [HlsBackpressureState::Degraded, HlsBackpressureState::Saturated, HlsBackpressureState::Normal]
    {
        let server = spawn_segment_server(0).await;
        let temp_dir = tempfile::tempdir().unwrap();
        let policy = SegmentFetchPolicy { max_global_segment_fetches: 2, ..SegmentFetchPolicy::default() };
        let (worker, mut context, _) = fetch_context(&server, &temp_dir, &policy).await;
        super::support::grant_usable_worker_access_lease_at(
            &worker,
            &context.session.read().await.proxy_session_id,
            current_time_millis(),
        )
        .await;
        let semaphore = Arc::clone(&worker.runtime.load().global_semaphore);
        let mut held_permits = vec![semaphore.try_acquire().unwrap()];
        let previous_generation = {
            let mut s = context.session.write().await;
            s.invalidate_queued_origin_work();
            s.initial_prefetch_gap_segments = 1;
            let pressure = worker.classify_backpressure_for_session(&s);
            assert_eq!(pressure, HlsBackpressureState::Degraded);
            let report = s.queue_manifest_fetch_candidates(current_time_millis(), pressure.allows_prefetch());
            assert_eq!(report.render_window_queued, 2);
            assert_eq!(report.prefetch_queued, 0);
            assert_eq!(report.prefetch_skipped, 1);
            s.activity.origin_work_generation
        };
        let runtime = Arc::new(InvalidatingPrepare { first: AtomicBool::new(true), handoff: false });
        context.origin_io = Some(crate::HlsOriginIoContext {
            ctx: crate::HlsCtx::for_test(Config::default()),
            client_addr: "127.0.0.1:12345".parse().unwrap(),
            allow_grace: false,
            priority: 0,
            connection_kind: ConnectionKind::Normal,
            reservation_ttl_secs: 1,
            preacquired_provider_handle: None,
            started_generation: None,
            prepared_account: None,
            playback_kind: PlaybackKind::LiveHls,
            account_runtime: Some(runtime.clone()),
        });
        let barrier = worker.pause_next_origin_prepare_for_test();
        let scheduler_worker = Arc::clone(&worker);
        let scheduler_context = context.clone();
        let task = tokio::spawn(async move {
            scheduler_worker.wake_scheduler(scheduler_context, current_time_millis()).await;
        });
        tokio::time::timeout(Duration::from_secs(3), barrier.entered.notified()).await.unwrap();
        // Pressure can change after the manifest commit. Replanning must use its
        // current classification, including recovery back to Normal.
        match expected_pressure {
            HlsBackpressureState::Normal => held_permits.clear(),
            HlsBackpressureState::Degraded => {}
            HlsBackpressureState::Saturated => held_permits.push(semaphore.try_acquire().unwrap()),
        }
        barrier.prepare.notify_one();
        tokio::time::timeout(Duration::from_secs(3), barrier.prepared.notified()).await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled(), "observe the replan before workers consume its queue");

        assert!(!runtime.first.load(std::sync::atomic::Ordering::Acquire), "account preparation actually ran");
        let s = context.session.read().await;
        assert_eq!(s.activity.origin_work_generation, previous_generation + 1);
        let pressure = worker.classify_backpressure_for_session(&s);
        assert_eq!(pressure, expected_pressure);
        for seq in [1, 2] {
            assert!(s.segment_prefetch_queue.contains(seq), "{pressure:?}: restore render-window work");
            assert!(matches!(
                s.segments[&seq].status,
                SegmentCacheStatus::Queued { priority: SegmentFetchPriority::RenderWindow, .. }
            ));
        }
        let prefetch_queued = s.segment_prefetch_queue.contains(3);
        eprintln!(
            "replan pressure={pressure:?} generation={} render_window=[1, 2] prefetch_queued={prefetch_queued}",
            s.activity.origin_work_generation
        );
        assert_eq!(
            prefetch_queued,
            pressure.allows_prefetch(),
            "{pressure:?}: replan must respect the current prefetch policy"
        );
        assert_eq!(
            matches!(
                s.segments[&3].status,
                SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, .. }
            ),
            pressure.allows_prefetch()
        );
        assert_eq!(s.segment_prefetch_queue.len(), if pressure.allows_prefetch() { 3 } else { 2 });
        assert!(server.requests.lock().await.is_empty(), "the barrier precedes origin segment work");
    }
}

#[tokio::test]
async fn account_handoff_during_segment_prepare_blocks_old_origin_request() {
    use crate::{origin_attempt::tests::bound_origin_io, HlsOriginAccountIoLease, HlsOriginResourceFetchError};
    use std::sync::{atomic::Ordering, Arc};

    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy =
        SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
    let (worker, mut context, _) = fetch_context(&server, &temp_dir, &policy).await;
    let (io, replacement) = bound_origin_io(&context.session).await;
    context.origin_io = Some(io);
    let snapshot = worker.next_fetch_snapshot(&context, 10, &policy).await.expect("segment snapshot");
    let replacement_lease = Arc::new(HlsOriginAccountIoLease::active_for_test(&replacement, 1));
    let read_blocker = context.session.read().await;
    let mut fetch = Box::pin(super::super::fetch_segment_with_retries_into_cache(&context, &snapshot, &policy));
    assert!(futures::poll!(fetch.as_mut()).is_pending());
    let mut handoff = Box::pin(context.session.write());
    assert!(futures::poll!(handoff.as_mut()).is_pending());
    drop(read_blocker);
    assert!(futures::poll!(fetch.as_mut()).is_pending());
    {
        let mut session = handoff.await;
        session.handoff_origin_account(replacement);
        session.origin_account_io_lease = Some(Arc::clone(&replacement_lease));
    }

    let result = fetch.await;
    assert!(server.requests.lock().await.is_empty(), "stale segment URL must never reach the origin");
    assert!(matches!(result, Err(HlsOriginResourceFetchError::ProviderUnavailable(_))));
    let session = context.session.read().await;
    assert_eq!(session.activity.active_origin_work_count.load(Ordering::Acquire), 0);
    assert_eq!(replacement_lease.active_io_count.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn popped_candidate_with_expired_fetch_binding_returns_to_discovered() {
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
    entry.origin_fetch_ref.as_mut().expect("fetch binding").valid_until_ms = Some(9);

    assert!(take_queued_segment_fetch_candidate(&mut session, 1, SegmentFetchPriority::Prefetch, 10, true,).is_none());
    assert!(matches!(session.segments[&1].status, SegmentCacheStatus::Discovered));
}

#[tokio::test]
async fn expired_aes_key_is_refetched_before_a_second_segment_becomes_ready() {
    let server = spawn_sequence_response_server(vec![
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"0123456789abcdef".to_vec() },
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"encrypted-media-a".to_vec() },
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"fedcba9876543210".to_vec() },
        TestOriginResponse { status: 200, headers: Vec::new(), body: b"encrypted-media-b".to_vec() },
    ])
    .await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy =
        SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
    let (worker, context, _) = encrypted_fetch_context(&server, &temp_dir, &policy).await;
    {
        let mut session = context.session.write().await;
        session.transient.set_resource_ttl_ms(1);
    }

    let first_segment = HlsSegmentFile { proxy_seq: 1, extension: "ts".to_string() };
    let first_outcome = worker.demand_fetch_and_wait(context.clone(), &first_segment, current_time_millis()).await;
    assert_eq!(first_outcome, super::super::SegmentDemandFetchOutcome::Ready);

    tokio::time::sleep(Duration::from_millis(50)).await;
    {
        let session = context.session.read().await;
        let key = session
            .transient
            .resources
            .values()
            .find(|resource| resource.kind == TransientResourceKind::Key)
            .cloned()
            .expect("encrypted fixture key resource");
        assert!(session
            .transient
            .ready_key_object_valid_until_ms(
                &session.proxy_session_id,
                &key.id,
                key.file_ext_hint.as_deref().expect("key extension"),
                current_time_millis(),
            )
            .is_none());
    }

    let second_segment = HlsSegmentFile { proxy_seq: 2, extension: "ts".to_string() };
    let second_outcome = worker.demand_fetch_and_wait(context.clone(), &second_segment, current_time_millis()).await;
    assert_eq!(second_outcome, super::super::SegmentDemandFetchOutcome::Ready);

    {
        let session = context.session.read().await;
        assert!(matches!(
            session.segments.get(&2).map(|segment| &segment.status),
            Some(SegmentCacheStatus::Ready { .. })
        ));
    }

    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 4, "requests={requests:?}");
    assert!(requests[0].starts_with("GET /key.key "));
    assert!(requests[1].starts_with("GET /1.ts "));
    assert!(requests[2].starts_with("GET /key.key "));
    assert!(requests[3].starts_with("GET /2.ts "));
}

#[tokio::test]
async fn demand_fetch_is_blocked_for_gc_marked_session() {
    let server = spawn_segment_server(0).await;
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let policy = SegmentFetchPolicy {
        retry_delays_ms: [0, 0, 0, 0, 0],
        retry_jitter_max_ms: 0,
        ..SegmentFetchPolicy::default()
    };
    let (worker, context, segment_file) = fetch_context(&server, &temp_dir, &policy).await;
    context.session.write().await.mark_for_gc_removal();

    let outcome = worker.demand_fetch_and_wait(context, &segment_file, 20).await;

    assert_eq!(outcome, super::super::SegmentDemandFetchOutcome::NotFound);
    assert!(server.requests.lock().await.is_empty());
}

#[tokio::test]
async fn encrypted_demand_wait_uses_manifest_key_budget_before_the_short_segment_budget() {
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let task = tokio::spawn(async move {
        for key in [true, false] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
            }
            observed.lock().await.push(String::from_utf8(request).unwrap());
            if key {
                tokio::time::sleep(Duration::from_millis(2_400)).await;
            }
            let body: &[u8] = if key { b"0123456789abcdef" } else { b"encrypted-media" };
            socket
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                        .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(body).await.unwrap();
        }
    });
    let server = super::TestSegmentServer { base_url: format!("http://{address}"), requests, task };
    let temp = tempfile::tempdir().unwrap();
    let policy = SegmentFetchPolicy {
        timeouts: crate::HlsOriginTimeoutPolicy::fixed(100, 3_000),
        retry_delays_ms: [0; 5],
        retry_jitter_max_ms: 0,
        max_prefetch_queue_depth: 0,
        ..Default::default()
    };
    let (worker, context, segment) = encrypted_fetch_context(&server, &temp, &policy).await;
    let result = worker.demand_fetch_and_wait(context.clone(), &segment, current_time_millis()).await;
    assert_eq!(result, super::super::SegmentDemandFetchOutcome::Ready);
    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /key.key "));
    assert!(requests[1].starts_with("GET /1.ts "));
    assert_eq!(context.session.read().await.active_segment_fetches, 0);
    assert!(!context.segment_cache.has_active_temp_files());
}
