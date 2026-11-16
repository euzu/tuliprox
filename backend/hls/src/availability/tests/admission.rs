use super::*;
use crate::{SegmentFetchPolicy, TransientResourceKind, TransientResourceRef};

#[tokio::test]
async fn catchup_reentry_after_key_failure_allows_new_current_manifest() {
    let ctx = crate::HlsCtx::for_test(Config::default());
    let mut state =
        HlsSession::new(HlsSessionKey::new(1, "archive-key-reentry").with_archive_reference(123), b"secret", 0);
    state.mode = crate::HlsSessionMode::TransientPassthrough {
        reason: crate::TransientPassthroughReason::UnsupportedTag { tag: "EXT-X-ENDLIST".into() },
    };
    let key_resource = TransientResourceRef::new(
        TransientResourceKind::Key,
        "http://127.0.0.1/enc.key",
        b"secret",
        0,
        60_000,
        Some("key".to_string()),
    );
    state.transient.upsert_resources([key_resource.clone()]);
    let session = Arc::new(tokio::sync::RwLock::new(state));
    let policy = SegmentFetchPolicy::default();

    assert!(
        !crate::transient_fetcher::record_temporary_transient_segment_fetch_failure(
            &session,
            &key_resource,
            &policy,
            100,
        )
        .await
    );

    let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=AES-128,URI=\"enc.key\"\n#EXTINF:4,\n0.ts\n#EXT-X-ENDLIST\n";
    session.write().await.transient.replace_manifest_with_semantics(body.into(), 101, Some(4_000));
    let mut snapshot = pressure_manifest(4_000);
    snapshot.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;
    snapshot.source_commit_identity = session.read().await.transient.last_manifest_commit_identity().unwrap();

    assert!(
        hls_startup_admission_allows_snapshot(&ctx, &session, &snapshot, 102).await,
        "a fresh current catchup manifest must be admitted after key failure"
    );
    assert_eq!(
        session.read().await.segment_failure_tracker.consecutive_temporary_failures,
        0,
        "failure counter stays unchanged after key-only failure"
    );
    assert_eq!(session.read().await.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
}

#[tokio::test]
async fn catchup_reentry_after_media_failure_can_retry_only_a_new_current_manifest() {
    let ctx = crate::HlsCtx::for_test(Config::default());
    let mut state = HlsSession::new(HlsSessionKey::new(1, "archive-reentry").with_archive_reference(123), b"secret", 0);
    state.mode = crate::HlsSessionMode::TransientPassthrough {
        reason: crate::TransientPassthroughReason::UnsupportedTag { tag: "EXT-X-ENDLIST".into() },
    };
    state.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    state.segment_failure_tracker.last_failure_at_ms = Some(100);
    state.segment_failure_tracker.consecutive_temporary_failures = 3;
    let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\n0.ts\n#EXT-X-ENDLIST\n";
    state.transient.replace_manifest_with_semantics(body.into(), 101, Some(4_000));
    let mut snapshot = pressure_manifest(4_000);
    snapshot.delivery_mode = HlsManifestDeliveryMode::TransientPassthrough;
    snapshot.source_commit_identity = state.transient.last_manifest_commit_identity().unwrap();
    let session = Arc::new(tokio::sync::RwLock::new(state));
    assert!(
        hls_startup_admission_allows_snapshot(&ctx, &session, &snapshot, 102).await,
        "a new archive entry must be able to attempt media after a fresh manifest"
    );
    assert_eq!(
        session.read().await.segment_failure_tracker.consecutive_temporary_failures,
        3,
        "manifest publication must not pretend that media recovered"
    );
    assert_eq!(session.read().await.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
    session.write().await.segment_failure_tracker.last_failure_at_ms = Some(101);
    assert!(
        !hls_startup_admission_allows_snapshot(&ctx, &session, &snapshot, 102).await,
        "old manifest cannot revive a later failed attempt"
    );
    session.write().await.segment_failure_tracker.last_failure_at_ms = Some(100);
    snapshot.source_commit_identity = HlsManifestCommitIdentity::committed(99, 102);
    assert!(
        !hls_startup_admission_allows_snapshot(&ctx, &session, &snapshot, 103).await,
        "foreign snapshot cannot grant retry"
    );
    snapshot.source_commit_identity = session.read().await.transient.last_manifest_commit_identity().unwrap();
    session.write().await.key = HlsSessionKey::new(1, "live");
    assert!(
        !hls_startup_admission_allows_snapshot(&ctx, &session, &snapshot, 103).await,
        "live admission stays unchanged"
    );
    session.write().await.key = HlsSessionKey::new(1, "archive-reentry").with_archive_reference(123);
    session.write().await.origin_control.path_condition = HlsOriginPathCondition::HardFetchFailure;
    assert!(
        !hls_startup_admission_allows_snapshot(&ctx, &session, &snapshot, 103).await,
        "manifest failure cannot be bypassed"
    );
    session.write().await.origin_control.path_condition = HlsOriginPathCondition::SegmentReadinessFailure;
    session.write().await.require_live_entry_revalidation(1);
    assert!(
        !hls_startup_admission_allows_snapshot(&ctx, &session, &snapshot, 103).await,
        "revalidation remains fail closed"
    );
}

#[tokio::test]
async fn terminal_pending_capacity_failure_commits_terminal_unavailable() {
    assert_terminal_pending_registration_failure_commits_unavailable(
        HlsTerminalPendingRegistration::CapacityExceeded,
        "pending-capacity-failure",
    )
    .await;
}

#[tokio::test]
async fn post_refresh_coordinator_capacity_failure_leaves_no_unowned_live_lease() {
    assert_post_refresh_registration_failure_leaves_no_unowned_live_lease(
        HlsAvailabilityReevaluationRegistration::CapacityExceeded,
        "post-refresh-capacity-failure",
    )
    .await;
}

#[tokio::test]
async fn capacity_deferred_ready_boundary_keeps_affected_lease_live() {
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let now_ms = ctx.hls_proxy.terminal_commit_now_ms();
    let mut session = atomic_pressure_session();
    session.segments.get_mut(&1).expect("second segment").status =
        SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Prefetch, deferred_at_ms: 100 };
    let proxy_session_id = session.proxy_session_id.clone();
    let session = Arc::new(tokio::sync::RwLock::new(session));
    let lease_id = HlsAccessLeaseId("capacity-deferred-live".to_string());
    let mut lease = HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new("capacity-user", "capacity-client"),
        proxy_session_id.clone(),
        "capacity-user".to_string(),
        "capacity-session".to_string(),
        1,
        "capacity-stream".to_string(),
        1,
        now_ms,
        60_000,
    );
    lease.state = HlsAccessLeaseState::Activated;
    lease.active_until_ms = Some(now_ms.saturating_add(60_000));
    lease.pending_deadline = None;
    lease.last_manifest_snapshot = Some(pressure_manifest_at(0, 8_000));
    ctx.hls_proxy.prepare_access_lease(lease.clone()).await;

    let resolution =
        commit_terminal_tail_if_lease_reserve_requires_cutover(ctx, &session, &proxy_session_id, &lease, now_ms).await;

    assert_eq!(resolution, HlsTerminalResolution::LiveAllowed);
    let current = ctx
        .hls_proxy
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, now_ms)
        .await
        .expect("capacity-deferred lease remains available");
    assert_eq!(current.state, HlsAccessLeaseState::Activated);
    assert_eq!(current.playback_mode, HlsLeasePlaybackMode::Live);
}

#[tokio::test(start_paused = true)]
async fn failed_closed_lock_contention_retains_owner() {
    let fixture = post_refresh_terminal_fixture("lock-retained-owner", false).await;
    let owner_key = fixture
        .ctx
        .hls_proxy
        .availability_reevaluation_owner_key(&fixture.session, &fixture.proxy_session_id)
        .await
        .expect("lock-contention owner key");
    let lease_guard = fixture.ctx.hls_proxy.hold_access_lease_store_for_test().await;
    assert_eq!(
        register_hls_availability_reevaluation_with_mode(
            fixture.ctx.clone(),
            Arc::clone(&fixture.session),
            owner_key,
            post_refresh_owner_request(&fixture),
            HlsAvailabilityReevaluationMode::PostRefresh(
                super::super::super::refresh::HlsPostRefreshAvailabilityReason::HardManifestFailure,
            ),
        ),
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
    assert_availability_owner_registered(&fixture);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fixture.ctx.hls_proxy.availability_reevaluations().owner_count(),
        1,
        "lease-store contention must retain session ownership"
    );

    drop(lease_guard);
    wait_for_availability_owner_completion(&fixture).await;
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("lock release resolves the live lease");
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
}
