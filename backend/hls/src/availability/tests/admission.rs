use super::*;

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
