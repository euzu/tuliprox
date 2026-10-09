use super::{
    bind_refresh_request_to_app_state, build_manifest_refresh_timing,
    cancel_superseded_terminal_work_after_media_progress, commit_fetched_manifest, fetched_manifest,
    mark_origin_refresh_started, mark_origin_refresh_started_with_outcome, post_refresh_live_manifest_snapshot,
    refresh_and_commit, spawn_test_origin, test_origin_refresh_request, test_session,
    HlsAvailabilityReevaluationRegistration, HlsManifestCommitProgressEvidence, HlsManifestProgress,
    HlsOriginRefreshTriggerOutcome, HlsPostRefreshAvailabilityAction, HlsPostRefreshAvailabilityReason,
    HlsPreparedTerminalBundleKey, HlsRuntimeCustomTailReason, HlsSessionIncarnation, HlsTerminalAssetIdentity,
    HlsTerminalAssetRevisionGuard, HlsTerminalPendingOwnerKey, HlsTerminalPendingRegistration, LiveHlsOriginEntry,
    OriginRefreshState,
};
use crate::{
    HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseTiming, HlsPlaybackFamilyKey, HlsProxyManager, HlsSessionKey,
    ProxySessionId, SegmentCacheStatus, SegmentFetchPriority,
};
use std::sync::Arc;
use tokio::sync::oneshot;
use tuliprox_core::{model::Config, utils::current_time_millis};

#[test]
fn origin_refresh_state_starts_only_when_due_and_not_in_flight() {
    let mut state = OriginRefreshState { next_fetch_allowed_at_ms: 100, ..OriginRefreshState::default() };
    assert!(!state.is_due(99));
    assert!(state.is_due(100));
    state.mark_started(100);
    assert!(!state.is_due(101));
}

#[tokio::test]
async fn hls_terminal_commit_media_progress_cancels_pending_owner_after_session_lock_release() {
    let session = test_session();
    let request = test_origin_refresh_request(Arc::clone(&session));
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let coordinator = request.hls_proxy.terminal_pending();
    let (started_tx, started_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let owner_key = HlsTerminalPendingOwnerKey {
        session_incarnation: HlsSessionIncarnation::for_test(1),
        proxy_session_id,
        lease_id: HlsAccessLeaseId("progress-cancel".to_string()),
        lease_issued_at_ms: 10,
        expected_admission_generation: 20,
        manifest_snapshot_generation: 30,
        cursor_generation: 40,
        decision_generation: 50,
        reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
        bundle_key: HlsPreparedTerminalBundleKey {
            asset: HlsTerminalAssetIdentity { revision: 60, fingerprint: [6; 32] },
            target_duration_ms: 4_000,
            segment_count: super::super::HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        },
        latest_safe_commit_at_ms: 10_000,
    };
    let asset_guard = HlsTerminalAssetRevisionGuard::matching_for_test(Some(owner_key.bundle_key.asset));
    assert_eq!(
        coordinator.register(owner_key, &asset_guard, move |ownership| async move {
            assert!(started_tx.send(()).is_ok());
            ownership.cancelled().await;
            assert!(cancelled_tx.send(()).is_ok());
        }),
        HlsTerminalPendingRegistration::Scheduled
    );
    assert!(started_rx.await.is_ok());

    let commit_result = Ok((
        HlsManifestCommitProgressEvidence::CacheTimeline(build_manifest_refresh_timing(
            None,
            Some(4_000),
            HlsManifestProgress::Advanced,
        )),
        false,
        false,
    ));
    cancel_superseded_terminal_work_after_media_progress(&request, &commit_result).await;

    assert!(cancelled_rx.await.is_ok());
    assert_eq!(coordinator.owner_count(), 0);
}

#[tokio::test]
async fn successful_refresh_wakes_sleeping_post_refresh_owner() {
    let hls_ctx = crate::HlsCtx::for_test(Config::default());
    let ctx = &hls_ctx;
    let now_ms = current_time_millis();
    let progressed_body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:481\n\
         #EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n#EXTINF:4,\n483.ts\n";
    let server = spawn_test_origin(Arc::new(move |_path| (200, Vec::new(), progressed_body.to_string()))).await;
    let (session, _) = ctx
        .hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "post-refresh-wakeup"), b"secret", now_ms)
        .await;
    let manifest_url = format!("{}/live/user/pass/12345.m3u8", server.base_url);
    let mut request = bind_refresh_request_to_app_state(test_origin_refresh_request(Arc::clone(&session)), ctx);
    request.origin_entry = LiveHlsOriginEntry::parse(&manifest_url).expect("local successful origin entry");
    request.now_ms = now_ms;
    let mut baseline = fetched_manifest(
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
         #EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n",
    );
    baseline.final_manifest_url = manifest_url.clone();
    baseline.resolved_request_url = manifest_url;
    baseline.redirect_host = Some("127.0.0.1".to_string());
    {
        let mut session = session.write().await;
        commit_fetched_manifest(&mut session, &baseline, &request, now_ms).expect("baseline commits");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: now_ms };
        }
        session.render_and_store_manifest(now_ms).expect("baseline publishes");
        session.segments.get_mut(&1).expect("deferred boundary segment").status =
            SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Prefetch, deferred_at_ms: now_ms };
        session.origin_control.path_condition =
            super::super::super::origin_progress::HlsOriginPathCondition::AcceptanceConflict;
    }
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("post-refresh-wakeup-lease".to_string());
    let mut lease = HlsAccessLease::pending(
        lease_id,
        HlsPlaybackFamilyKey::new("post-refresh-user", "post-refresh-client"),
        proxy_session_id,
        "post-refresh-user".to_string(),
        "post-refresh-token".to_string(),
        1,
        "post-refresh-stream".to_string(),
        1,
        now_ms,
        60_000,
    );
    lease.state = crate::HlsAccessLeaseState::Activated;
    lease.active_until_ms = Some(now_ms.saturating_add(60_000));
    lease.pending_deadline = None;
    lease.last_manifest_snapshot = Some(post_refresh_live_manifest_snapshot());
    ctx.hls_proxy.prepare_access_lease(lease).await;

    assert!(mark_origin_refresh_started(&mut request, now_ms).await);
    let coordinator = ctx.hls_proxy.availability_reevaluations();
    let initial_permits = coordinator.available_task_permits_for_test();
    let (origin_progress_generation, media_readiness_generation) = {
        let session = session.read().await;
        (session.origin_control.progress_generation, session.activity.media_readiness_generation)
    };
    assert_eq!(
        super::super::super::availability::register_post_refresh_availability_reevaluation(
            ctx.clone(),
            Arc::clone(&session),
            request.clone(),
            HlsPostRefreshAvailabilityAction::Reevaluate {
                reason: HlsPostRefreshAvailabilityReason::HardManifestFailure,
                origin_progress_generation,
                media_readiness_generation,
            },
        )
        .await,
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
    assert_eq!(coordinator.owner_count(), 1);
    assert_eq!(coordinator.available_task_permits_for_test(), initial_permits.saturating_sub(1));
    let progress_generation_before = session.read().await.origin_control.progress_generation;

    refresh_and_commit(request, now_ms).await;
    for _ in 0..256 {
        if coordinator.owner_count() == 0 && coordinator.available_task_permits_for_test() == initial_permits {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert!(session.read().await.origin_control.progress_generation > progress_generation_before);
    assert_eq!(coordinator.owner_count(), 0);
    assert_eq!(coordinator.available_task_permits_for_test(), initial_permits);
}

#[tokio::test]
async fn concurrent_refresh_suppression_reports_in_flight_state() {
    let session = test_session();
    session.write().await.origin_refresh.mark_started(900);
    let mut request = test_origin_refresh_request(Arc::clone(&session));

    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 1_000).await,
        HlsOriginRefreshTriggerOutcome::InFlight
    );
    assert_eq!(session.read().await.origin_refresh.last_fetch_started_at_ms, Some(900));
}

#[tokio::test]
async fn hls_availability_reevaluation_cursor_evidence_supersedes_refresh_guard() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "cursor-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("cursor-pressure-guard".to_string());
    {
        let mut leases = hls_proxy.access_leases().write().await;
        assert!(leases.prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("user", "client"),
            proxy_session_id.clone(),
            "user".to_string(),
            "session-token".to_string(),
            1,
            "cursor-pressure-guard".to_string(),
            1,
            100,
            60_000,
        )));
        assert!(leases
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                100,
                HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
            )
            .is_activated());
    }
    let owner_key = hls_proxy
        .availability_reevaluation_owner_key(&session, &proxy_session_id)
        .await
        .expect("live lease has availability evidence");
    {
        let mut leases = hls_proxy.access_leases().write().await;
        let identity = leases
            .response_snapshot(&lease_id, &proxy_session_id, 100)
            .and_then(|lease| lease.media_identity())
            .expect("live lease identity");
        assert!(leases
            .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 20, 101,)
            .is_some());
    }
    let current_owner_key = hls_proxy
        .availability_reevaluation_owner_key(&session, &proxy_session_id)
        .await
        .expect("updated availability evidence");
    assert!(current_owner_key.availability_evidence_generation > owner_key.availability_evidence_generation);
    assert_eq!(current_owner_key.origin_progress_generation, owner_key.origin_progress_generation);
    assert_eq!(current_owner_key.media_readiness_generation, owner_key.media_readiness_generation);

    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.recovery_pressure_guard =
        Some(super::super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key));
    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 102).await,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded
    );
    assert!(!session.read().await.origin_refresh.in_flight);
}

#[tokio::test]
async fn hls_availability_reevaluation_guard_contention_is_typed() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "contended-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let owner_key =
        hls_proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await.expect("session evidence");
    let mut request = test_origin_refresh_request(Arc::clone(&session));
    request.hls_proxy = Arc::clone(&hls_proxy);
    request.acceptance_directive.recovery_pressure_guard =
        Some(super::super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key));
    let lease_guard = hls_proxy.access_leases().write().await;

    assert_eq!(
        mark_origin_refresh_started_with_outcome(&mut request, 100).await,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureStateContention
    );
    assert!(!session.read().await.origin_refresh.in_flight);
    drop(lease_guard);
}

#[tokio::test]
async fn hls_availability_reevaluation_other_session_evidence_keeps_guard_current() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) =
        hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, "guard-session-a"), b"secret", 100).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let owner_key =
        hls_proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await.expect("session evidence");
    let other_proxy_session_id = ProxySessionId("guard-session-b".to_string());
    assert!(hls_proxy.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
        HlsAccessLeaseId("guard-session-b".to_string()),
        HlsPlaybackFamilyKey::new("other", "client"),
        other_proxy_session_id,
        "other".to_string(),
        "other-session".to_string(),
        1,
        "guard-session-b".to_string(),
        2,
        100,
        60_000,
    )));
    let guard = super::super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key);

    assert!(matches!(
        hls_proxy.with_current_recovery_pressure_session(&session, &guard, |_| 7_u8),
        super::super::super::availability_reevaluation::HlsRecoveryPressureGuardAccess::Acquired(7)
    ));
}

#[tokio::test]
async fn hls_availability_reevaluation_removed_lease_supersedes_old_guard() {
    let hls_proxy = Arc::new(HlsProxyManager::new());
    let (session, _) = hls_proxy
        .get_or_create_session_with_outcome(HlsSessionKey::new(1, "removed-pressure-guard"), b"secret", 100)
        .await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("removed-pressure-guard".to_string());
    assert!(hls_proxy.access_leases().write().await.prepare_access_lease(HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new("user", "client"),
        proxy_session_id.clone(),
        "user".to_string(),
        "session-token".to_string(),
        1,
        "removed-pressure-guard".to_string(),
        1,
        100,
        60_000,
    )));
    let owner_key =
        hls_proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await.expect("lease evidence");
    assert!(hls_proxy.access_leases().write().await.remove_access_lease(&lease_id).is_some());
    let guard = super::super::super::availability_reevaluation::HlsRecoveryPressureGuard::from_owner_key(&owner_key);

    assert!(matches!(
        hls_proxy.with_current_recovery_pressure_session(&session, &guard, |_| ()),
        super::super::super::availability_reevaluation::HlsRecoveryPressureGuardAccess::Superseded
    ));
    assert!(!session.read().await.origin_refresh.in_flight);
}
