use super::*;

#[tokio::test]
async fn direct_segment_success_resets_failure_state_only_after_clean_eof() {
    let identity = b"complete-segment";
    let origin =
        spawn_test_origin("200 OK", vec![("Content-Type", "application/octet-stream")], identity.to_vec()).await;
    let origin_url = format!("{}/segment.ts", origin.base_url);
    let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
    fixture.seed_segment_failure().await;
    let decoded = fetch_direct_decoded(origin_url, TransientResourceKind::Segment, None)
        .await
        .expect("direct response setup succeeds");
    let response = fixture.response(decoded);

    assert_eq!(fixture.segment_failure_count().await, 1, "headers alone must not report body success");
    assert_eq!(fixture.resource.access.active_readers(), 1);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.expect("complete segment streams"),
        identity.as_slice()
    );
    fixture.wait_for_segment_failure_count(0).await;
    assert_eq!(fixture.resource.access.active_readers(), 0);
}

#[tokio::test]
async fn direct_terminal_transition_survives_dropped_completion_waiter() {
    let fixture =
        TestDirectResponseFixture::new(TransientResourceKind::Segment, "http://127.0.0.1/cancelled-body-poll.ts");
    fixture.seed_segment_failure().await;
    let session_lock = fixture.session.write().await;
    let mut finalizer = HlsTransientDirectResponseFinalizer::new(fixture.lifecycle_context());

    let transition = finalizer
        .begin_finish(HlsTransientDirectStreamOutcome::CleanEof)
        .expect("clean EOF starts an owned transition");
    drop(finalizer);
    drop(transition);
    drop(session_lock);

    fixture.wait_for_segment_failure_count(0).await;
}

#[tokio::test]
async fn direct_segment_decoder_failure_after_retry_uses_selected_attempt_once() {
    let _ = take_body_failure_log_attempts();
    let mut truncated = gzip_encode(b"decoder failure after response headers").await;
    truncated.truncate(truncated.len().saturating_sub(8));
    let origin = spawn_retry_then_body_origin(
        vec![("Content-Encoding", "gzip"), ("Content-Type", "application/octet-stream")],
        truncated,
    )
    .await;
    let origin_url = format!("{}/segment.ts", origin.base_url);
    let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
    let decoded = fetch_direct_decoded(origin_url, TransientResourceKind::Segment, None)
        .await
        .expect("decoder setup succeeds before streaming");
    assert_eq!(decoded.attempt.attempt_index, 1);
    assert_eq!(decoded.attempt.attempts, 5);
    let response = fixture.response(decoded);

    assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
    fixture.wait_for_segment_failure_count(1).await;
    assert_eq!(
        fixture.last_segment_failure().await,
        Some(HlsSegmentFailureObject::Transient { resource_id: fixture.resource.id.0.clone() })
    );
    tokio::task::yield_now().await;
    assert_eq!(fixture.segment_failure_count().await, 1, "terminal body failure must not finalize twice");
    assert_eq!(fixture.resource.access.active_readers(), 0);
    assert_eq!(origin.requests.lock().await.len(), 2, "streaming failure must not trigger a hidden retry");
    assert_eq!(
        take_body_failure_log_attempts(),
        vec![(1, 5)],
        "test hook intentionally records the zero-based raw index of the selected second attempt"
    );
}

#[tokio::test]
async fn unrelated_transient_objects_do_not_start_media_lifecycle_tasks() {
    let kind = TransientResourceKind::Other;
    for outcome in [HlsTransientDirectStreamOutcome::CleanEof, HlsTransientDirectStreamOutcome::OriginBodyFailure] {
        let fixture = TestDirectResponseFixture::new(kind, "http://127.0.0.1/non-segment.bin");
        let mut finalizer = HlsTransientDirectResponseFinalizer::new(fixture.lifecycle_context());

        assert!(finalizer.begin_finish(outcome).is_none(), "{kind:?} must not start a segment lifecycle task");
    }
}

#[tokio::test]
async fn key_and_map_body_failures_degrade_readiness_without_incrementing_media_failure_count() {
    for kind in [TransientResourceKind::Key, TransientResourceKind::Map] {
        let fixture = TestDirectResponseFixture::new(kind, "http://127.0.0.1/media-dependency.bin");
        let mut finalizer = HlsTransientDirectResponseFinalizer::new(fixture.lifecycle_context());

        finalizer
            .begin_finish(HlsTransientDirectStreamOutcome::OriginBodyFailure)
            .expect("media dependency starts a lifecycle task")
            .await
            .expect("media dependency lifecycle task joins");

        let session = fixture.session.read().await;
        assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 0);
        assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
    }
}

#[tokio::test]
async fn direct_segment_origin_idle_timeout_is_counted_exactly_once() {
    let origin = spawn_test_origin_in_chunks(
        "200 OK",
        vec![("Content-Type", "application/octet-stream")],
        vec![b"first".to_vec(), b"late".to_vec()],
        Duration::from_millis(200),
    )
    .await;
    let origin_url = format!("{}/segment.ts", origin.base_url);
    let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
    let decoded = fetch_direct_decoded_with_timeout(origin_url, TransientResourceKind::Segment, None, 50)
        .await
        .expect("direct response setup succeeds");
    let response = fixture.response(decoded);

    assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
    fixture.wait_for_segment_failure_count(1).await;
    tokio::task::yield_now().await;
    assert_eq!(fixture.segment_failure_count().await, 1);
    assert_eq!(fixture.resource.access.active_readers(), 0);
}

#[tokio::test]
async fn dropping_direct_segment_body_is_client_abort_without_state_transition() {
    let _ = take_body_failure_log_attempts();
    let origin =
        spawn_test_origin("200 OK", vec![("Content-Type", "application/octet-stream")], b"unconsumed-segment".to_vec())
            .await;
    let origin_url = format!("{}/segment.ts", origin.base_url);
    let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
    fixture.seed_segment_failure().await;
    let decoded = fetch_direct_decoded(origin_url, TransientResourceKind::Segment, None)
        .await
        .expect("direct response setup succeeds");
    let response = fixture.response(decoded);
    assert_eq!(fixture.resource.access.active_readers(), 1);

    drop(response);

    assert_eq!(fixture.segment_failure_count().await, 1);
    assert_eq!(fixture.resource.access.active_readers(), 0);
    assert_eq!(origin.requests.lock().await.len(), 1);
    assert!(take_body_failure_log_attempts().is_empty());
}

#[tokio::test]
async fn direct_non_media_body_failures_are_failure_tracker_neutral() {
    for kind in [TransientResourceKind::Key, TransientResourceKind::Map, TransientResourceKind::Other] {
        let mut truncated = gzip_encode(b"non-segment decoder failure").await;
        truncated.truncate(truncated.len().saturating_sub(8));
        let origin = spawn_test_origin(
            "200 OK",
            vec![("Content-Encoding", "gzip"), ("Content-Type", "application/octet-stream")],
            truncated,
        )
        .await;
        let origin_url = format!("{}/object.bin", origin.base_url);
        let fixture = TestDirectResponseFixture::new(kind, origin_url.clone());
        let decoded =
            fetch_direct_decoded(origin_url, kind, None).await.expect("decoder setup succeeds before streaming");
        let response = fixture.response(decoded);

        assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
        assert_eq!(fixture.segment_failure_count().await, 0, "{kind:?} must not affect segment failure state");
        let path_condition = fixture.session.read().await.origin_control.path_condition;
        if matches!(kind, TransientResourceKind::Key | TransientResourceKind::Map) {
            assert_eq!(path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
        } else {
            assert_eq!(path_condition, HlsOriginPathCondition::ProgressExpected);
        }
    }
}

#[tokio::test]
async fn transient_media_failure_degrades_availability_without_direct_terminal_transition() {
    for kind in [TransientResourceKind::Segment, TransientResourceKind::Part] {
        let fixture = TestDirectResponseFixture::new(kind, "http://127.0.0.1/media.bin");

        assert!(
            !record_temporary_transient_segment_fetch_failure(
                &fixture.session,
                &fixture.resource,
                &fixture.policy,
                100,
            )
            .await
        );

        let session = fixture.session.read().await;
        assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 1);
        assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
        assert!(session.origin_control.acceptance_episode.is_none());
        assert!(!matches!(
            session.origin_control.progress_phase,
            HlsOriginProgressPhase::Terminal | HlsOriginProgressPhase::TerminalPartial
        ));
        drop(session);

        super::super::record_successful_transient_segment_fetch(&fixture.session, &fixture.resource).await;
        let session = fixture.session.read().await;
        assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 0);
        assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
    }
}

#[tokio::test]
async fn direct_stream_decoder_failure_aborts_body_without_origin_retry() {
    let mut truncated = gzip_encode(b"decoder failure after response headers").await;
    truncated.truncate(truncated.len().saturating_sub(8));
    let origin = spawn_test_origin(
        "200 OK",
        vec![("Content-Encoding", "gzip"), ("Content-Type", "application/octet-stream")],
        truncated,
    )
    .await;
    let decoded = fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
        .await
        .expect("decoder setup succeeds before streaming");
    let response = direct_client_response(decoded, TransientResourceKind::Key);

    assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
    assert_eq!(origin.requests.lock().await.len(), 1, "streaming errors must not trigger a hidden retry");
}

#[tokio::test]
async fn direct_stream_refreshes_origin_body_idle_deadline_after_each_chunk() {
    const BODY_IDLE_TIMEOUT_MS: u64 = 500;
    let chunks = [b"slow-".to_vec(), b"but-".to_vec(), b"continuous-".to_vec(), b"body".to_vec()];
    let expected = chunks.concat();
    let origin = spawn_test_origin_in_chunks(
        "200 OK",
        vec![("Content-Type", "application/octet-stream")],
        chunks.into(),
        Duration::from_millis(200),
    )
    .await;
    let decoded = fetch_direct_decoded_with_timeout(
        format!("{}/key.bin", origin.base_url),
        TransientResourceKind::Key,
        None,
        BODY_IDLE_TIMEOUT_MS,
    )
    .await
    .expect("direct response setup succeeds");
    let response = direct_client_response(decoded, TransientResourceKind::Key);

    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.expect("progressing body outlives total timeout"),
        expected
    );
    assert_eq!(origin.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn transient_origin_io_guard_finish_clean_decrements_work_once() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "clean-finish"), b"secret", 1_000);
    let binding =
        HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
    session.origin_account_io_lease = Some(std::sync::Arc::new(HlsOriginAccountIoLease::active_for_test(&binding, 2)));
    session.origin_account_binding = Some(binding.clone());
    session.activity.active_origin_work_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(2));
    let started_gen = session.activity.origin_work_generation;
    let session = Arc::new(tokio::sync::RwLock::new(session));
    let ctx = crate::hls_ctx::HlsCtx::for_test(tuliprox_core::model::Config::default());

    let lease_guard = HlsOriginAccountIoLeaseGuard::new(
        binding.clone(),
        Arc::clone(session.try_read().unwrap().origin_account_io_lease.as_ref().unwrap()),
    );
    let origin_io = HlsOriginIoContext {
        ctx: ctx.clone(),
        client_addr: "127.0.0.1:8080".parse().unwrap(),
        allow_grace: false,
        priority: 0,
        connection_kind: tuliprox_session::ConnectionKind::Normal,
        reservation_ttl_secs: 60,
        preacquired_provider_handle: None,
        started_generation: Some(started_gen),
    };

    let guard = HlsTransientOriginIoGuard::new(
        Arc::clone(&session),
        Arc::clone(&session.try_read().unwrap().activity.active_origin_work_count),
        origin_io,
        lease_guard,
        started_gen,
    );

    guard.finish_clean().await;
    assert_eq!(
        session.read().await.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire),
        1,
        "finish_clean and subsequent drop must decrement active_origin_work_count exactly once"
    );
}

#[tokio::test]
async fn transient_origin_io_guard_drop_under_lock_contention_decrements_work() {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "contention-drop"), b"secret", 1_000);
    let binding =
        HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("account"), &session.proxy_session_id, 1_000);
    session.origin_account_io_lease = Some(std::sync::Arc::new(HlsOriginAccountIoLease::active_for_test(&binding, 2)));
    session.origin_account_binding = Some(binding.clone());
    session.activity.active_origin_work_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(2));
    let started_gen = session.activity.origin_work_generation;
    let session = Arc::new(tokio::sync::RwLock::new(session));
    let ctx = crate::hls_ctx::HlsCtx::for_test(tuliprox_core::model::Config::default());

    let lease_guard = HlsOriginAccountIoLeaseGuard::new(
        binding.clone(),
        Arc::clone(session.try_read().unwrap().origin_account_io_lease.as_ref().unwrap()),
    );
    let origin_io = HlsOriginIoContext {
        ctx: ctx.clone(),
        client_addr: "127.0.0.1:8080".parse().unwrap(),
        allow_grace: false,
        priority: 0,
        connection_kind: tuliprox_session::ConnectionKind::Normal,
        reservation_ttl_secs: 60,
        preacquired_provider_handle: None,
        started_generation: Some(started_gen),
    };

    let guard = HlsTransientOriginIoGuard::new(
        Arc::clone(&session),
        Arc::clone(&session.try_read().unwrap().activity.active_origin_work_count),
        origin_io,
        lease_guard,
        started_gen,
    );

    let lock = session.read().await;
    drop(guard);
    assert_eq!(
        lock.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire),
        1,
        "drop MUST synchronously decrement active_origin_work_count"
    );
    drop(lock);

    for _ in 0..100 {
        if session.read().await.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        session.read().await.activity.active_origin_work_count.load(std::sync::atomic::Ordering::Acquire),
        1,
        "deferred cleanup must decrement active_origin_work_count under contention"
    );
}
