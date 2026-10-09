use super::{
    access_lease, begin_test_acceptance_episode, commit_terminal_plan, complete_failed_acceptance_episode,
    cutover_reserve, hls_acceptance_recovery_snapshot, live_media_fixture, manifest_snapshot,
    next_terminal_commit_retry, prepared_terminal_commit_fixture, publish_manifest_snapshot,
    spawn_terminal_commit_retry_worker, terminal_plan, terminal_preparation_request, HlsAccessLeaseDenialMode,
    HlsAccessLeaseDenialOutcome, HlsLeaseCutoverTiming, HlsMediaActivityCommitOutcome, HlsProxyManager,
    HlsRecoveryExecutionState, HlsRuntimeCustomTailAssetIdentity, HlsTerminalAssetRevisionGuard,
    HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitCommand, HlsTerminalCommitMediaGuard,
    HlsTerminalCommitOutcome, HlsTerminalCommitOwnerKey, HlsTerminalCommitPayload, HlsTerminalCommitRequest,
    HlsTerminalCommitRetryDecision, HlsTerminalCommitRetryScheduleDecision, HlsTerminalCommitSubmissionDecision,
    HlsTerminalCommitWindow, HlsTerminalLeaseDecision, HlsTerminalMediaRequirementSource,
    HlsTerminalTailPreparationRequest, HlsTerminalTailProtection, HLS_TERMINAL_TAIL_PROTECTION_CAPACITY,
};
use crate::{
    media_reserve::HlsLeaseReserveSnapshot, terminal_tail::HlsLeasePlaybackMode, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsManifestAcceptanceExhaustionReason, HlsSegmentFailureObject, HlsSessionKey, HlsSessionStoreOutcome,
    HlsTerminalAssetIdentity, HlsTerminalTailCompatibility, HlsTerminalTailGeneration, ProxySessionId,
};
use shared::model::HlsCacheConfigDto;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tuliprox_core::model::HlsCacheConfig;

#[tokio::test]
async fn late_live_completion_after_terminal_transition_updates_neither_cursor_nor_session_activity() {
    let (manager, session, proxy_session_id, lease_id, live_identity) = live_media_fixture("late-terminal").await;
    let token = manager
        .record_access_lease_segment_request_started_if_identity_matches(
            &lease_id,
            &proxy_session_id,
            live_identity,
            40,
            2_000,
        )
        .await
        .expect("current live request token");
    let plan = terminal_plan(7, &proxy_session_id, &lease_id);
    {
        let mut leases = manager.access_leases.write().await;
        let mut lease = leases.remove_access_lease(&lease_id).expect("live lease");
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(plan);
        leases.prepare_access_lease(lease);
    }

    let outcome = manager
        .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
            &session,
            &lease_id,
            &proxy_session_id,
            live_identity,
            token,
            2_100,
        )
        .await;

    assert_eq!(outcome, HlsMediaActivityCommitOutcome::StaleLeaseIdentity);
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, None);
    let lease =
        manager.access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100).await.expect("terminal lease");
    assert_eq!(lease.playback_cursor.highest_contiguous_completed_proxy_seq, None);
    assert_eq!(lease.playback_cursor.first_segment_completed_at_ms, None);
}

#[tokio::test]
async fn current_terminal_identity_waits_for_lock_contention_and_marks_activity() {
    let (manager, session, proxy_session_id, lease_id, _) = live_media_fixture("terminal-media").await;
    let manager = Arc::new(manager);
    let terminal_identity = {
        let mut leases = manager.access_leases.write().await;
        let mut lease = leases.remove_access_lease(&lease_id).expect("live lease");
        lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(terminal_plan(7, &proxy_session_id, &lease_id));
        let identity = lease.media_identity().expect("terminal identity");
        leases.prepare_access_lease(lease);
        identity
    };
    assert_eq!(
        manager
            .mark_authorized_media_access_for_lease_if_identity_matches(
                &session,
                &lease_id,
                &proxy_session_id,
                terminal_identity,
                2_000,
            )
            .await,
        HlsMediaActivityCommitOutcome::Committed
    );
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(2_000));

    session.write().await.activity.last_authorized_media_at_ms = None;
    let lease_guard = manager.access_leases.write().await;
    let task_manager = Arc::clone(&manager);
    let task_session = Arc::clone(&session);
    let task_lease_id = lease_id.clone();
    let task_proxy_session_id = proxy_session_id.clone();
    let commit_task = tokio::spawn(async move {
        task_manager
            .mark_authorized_media_access_for_lease_if_identity_matches(
                &task_session,
                &task_lease_id,
                &task_proxy_session_id,
                terminal_identity,
                2_100,
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(!commit_task.is_finished(), "current media activity must wait rather than be dropped");
    drop(lease_guard);
    assert_eq!(commit_task.await.expect("controlled commit task"), HlsMediaActivityCommitOutcome::Committed);
    assert_eq!(session.read().await.activity.last_authorized_media_at_ms, Some(2_100));
}

#[tokio::test]
async fn hls_terminal_commit_missing_acceptance_exact_ready_bundle_commits_tail() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    manager.terminal_commit_clock.set_fixed_now_ms(2_000);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "missing-acceptance"), b"secret", 1_000).await;
    let (proxy_session_id, progress_generation) = {
        let session = session.read().await;
        assert!(session.origin_control.acceptance_episode.is_none());
        (session.proxy_session_id.clone(), session.origin_control.progress_generation)
    };
    let lease_id = HlsAccessLeaseId("missing-acceptance".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let mut preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("missing acceptance still permits cutover-local terminal preparation");
    assert_eq!(
        preparation.terminal_media_requirement_source,
        HlsTerminalMediaRequirementSource::CutoverSnapshotPending {
            decision_generation: preparation.decision_generation,
        }
    );
    let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);
    let prepared_key = plan.media_preparation_key();
    preparation
        .bind_ready_terminal_media_requirement(prepared_key)
        .expect("the exact ready bundle binds to the cutover decision");
    assert_eq!(
        preparation.terminal_media_requirement_source,
        HlsTerminalMediaRequirementSource::CutoverSnapshot {
            decision_generation: preparation.decision_generation,
            asset: prepared_key.asset,
        }
    );

    let outcome = commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan);

    assert_eq!(outcome, HlsTerminalCommitOutcome::Committed);
    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("committed terminal lease");
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalTail(_)));
}

pub(in crate::manager::tests) async fn wait_for_terminal_commit_owner(manager: &HlsProxyManager) {
    for _ in 0..256 {
        if manager.terminal_commit_retries.owner_count() == 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(manager.terminal_commit_retries.owner_count(), 0, "terminal retry owner did not finish");
}

#[test]
fn hls_cutover_policy_manager_uses_only_matching_acceptance_work_as_recovery_evidence() {
    let mut session = super::super::super::HlsSession::new(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000);
    let generation = begin_test_acceptance_episode(&mut session, 2_000);
    session.origin_refresh.in_flight = false;

    assert!(matches!(
        hls_acceptance_recovery_snapshot(&session, 2_100).recovery,
        HlsRecoveryExecutionState::InFlight { .. }
    ));

    let episode = session.origin_control.acceptance_episode.as_mut().expect("acceptance episode");
    episode.record_full_burst();
    episode.record_exhaustion(HlsManifestAcceptanceExhaustionReason::AllFailed);
    session.origin_refresh.in_flight = true;

    let snapshot = hls_acceptance_recovery_snapshot(&session, 2_100);
    assert_eq!(snapshot.expected_generation, generation);
    assert_eq!(snapshot.recovery, HlsRecoveryExecutionState::Idle);
}

#[tokio::test]
async fn repeated_segment_failure_counter_does_not_terminalize_access_lease() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));

    {
        let mut session = session.write().await;
        for failure in 0_u64..100 {
            let _ = session.record_temporary_segment_fetch_failure(
                2_000_u64.saturating_add(failure),
                HlsSegmentFailureObject::Normal { proxy_seq: 40, origin_seq: 400 },
                1,
            );
        }
        assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 100);
    }

    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100)
        .await
        .expect("failure tracking must not remove the lease");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_cutover_policy_media_progress_supersedes_prepared_plan() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("failed acceptance permits terminal preparation");

    {
        let mut session = session.write().await;
        session.origin_control.record_media_progress(2_100, 12_000);
        assert_eq!(session.origin_control.progress_generation, progress_generation.saturating_add(1));
    }
    assert_eq!(
        commit_terminal_plan(
            &manager,
            &session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_100,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::RecoveryCommitted
    );

    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_200)
        .await
        .expect("lease remains available");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_cutover_policy_matching_acceptance_commit_without_media_progress_terminalizes_lease() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("terminal preparation");

    {
        let mut session = session.write().await;
        session.origin_control.acceptance_episode.as_mut().expect("acceptance episode").complete();
        assert_eq!(session.origin_control.progress_generation, preparation.origin_progress_generation);
    }

    assert_eq!(
        commit_terminal_plan(
            &manager,
            &session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_100,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::Committed
    );
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100)
            .await
            .expect("terminal lease")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
}

#[tokio::test]
async fn hls_cutover_policy_new_acceptance_episode_without_media_progress_does_not_supersede_cutover() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("terminal preparation");

    {
        let mut session = session.write().await;
        let previous_acceptance_generation = session.origin_control.acceptance_generation;
        let current_acceptance_generation = begin_test_acceptance_episode(&mut session, 2_050);
        assert_eq!(session.origin_control.acceptance_generation.0, previous_acceptance_generation.0.saturating_add(1));
        assert_eq!(current_acceptance_generation, session.origin_control.acceptance_generation);
        assert_eq!(session.origin_control.progress_generation, preparation.origin_progress_generation);
    }

    assert_eq!(
        commit_terminal_plan(
            &manager,
            &session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_100,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::Committed
    );
}

#[tokio::test]
async fn hls_terminal_commit_retry_asset_change_autonomously_fails_closed() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-asset-change").await;
    let expected_asset = plan.asset_identity;
    let asset_is_current = Arc::new(AtomicBool::new(true));
    let revision_guard = {
        let asset_is_current = Arc::clone(&asset_is_current);
        HlsTerminalAssetRevisionGuard::for_runtime_tail(expected_asset, move || {
            asset_is_current.load(Ordering::Acquire).then_some(expected_asset)
        })
    };
    let session_guard = session.write().await;

    assert!(matches!(
        manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
            session: &session,
            lease_id: &lease_id,
            proxy_session_id: &proxy_session_id,
            preparation: &preparation,
            now_ms: 2_000,
            payload: HlsTerminalCommitPayload::Tail {
                plan,
                media_guard: HlsTerminalCommitMediaGuard::empty_for_test(),
            },
            asset_revision_guard: revision_guard,
        }),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    asset_is_current.store(false, Ordering::Release);
    drop(session_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("asset change terminalizes without another request")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::MissingAsset, .. }
    ));
}

#[tokio::test]
async fn hls_terminal_commit_retry_lock_busy_without_a_second_client_request() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    manager.terminal_commit_clock.set_fixed_now_ms(2_000);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("terminal preparation");
    let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id);

    let session_guard = session.write().await;
    assert_eq!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, Arc::clone(&plan),),
        HlsTerminalCommitOutcome::LockBusy { retry_before_ms: 2_001 }
    );
    assert_eq!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::LockBusy { retry_before_ms: 2_001 }
    );
    assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
    drop(session_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("autonomous retry keeps the lease available")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
}

#[tokio::test]
async fn hls_terminal_commit_submission_delayed_stale_incoming_preserves_authoritative_tail() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("submission-stale-incoming").await;
    let current_asset = plan.asset_identity;
    let stale_asset = HlsTerminalAssetIdentity { revision: 999, fingerprint: [9; 32] };
    let Some(session_incarnation) = manager.sessions.session_incarnation(&session) else {
        panic!("fixture session has an incarnation");
    };
    let Some((cancellation_epoch, submission_token)) = manager.terminal_commit_retries.reserve_submission() else {
        panic!("fixture can reserve terminal submission");
    };
    let owner_key = HlsTerminalCommitOwnerKey::from_preparation(&proxy_session_id, &lease_id, &preparation);
    let command = HlsTerminalCommitCommand {
        key: owner_key,
        session: Arc::clone(&session),
        session_incarnation,
        preparation: preparation.clone(),
        decision: HlsTerminalLeaseDecision::Tail(Arc::clone(&plan)),
        media_guard: Some(HlsTerminalCommitMediaGuard::empty_for_test()),
        asset_revision_guard: HlsTerminalAssetRevisionGuard::matching_runtime_for_test(current_asset),
        cancellation_epoch,
        submission_token,
    };
    let HlsTerminalCommitSubmissionDecision::Attempt { command, owner_token } =
        manager.terminal_commit_retries.submit(command, 2_000)
    else {
        panic!("authoritative tail owns the initial submission");
    };
    let HlsTerminalCommitRetryDecision::Schedule { retry_at_ms, attempts_completed } = next_terminal_commit_retry(
        1,
        2_000,
        preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(),
    ) else {
        panic!("fixture has retry budget");
    };
    let HlsTerminalCommitRetryScheduleDecision::Scheduled { worker_token: Some(worker_token) } = manager
        .terminal_commit_retries
        .schedule_current(&command.key, owner_token, attempts_completed, retry_at_ms, 2_000)
    else {
        panic!("authoritative tail schedules one bounded worker");
    };

    let mut stale_preparation = preparation.clone();
    stale_preparation.cutover_timing = HlsLeaseCutoverTiming::from_reserve(
        2_000,
        preparation.reserve.transition_margin.as_millis().saturating_add(2),
        preparation.reserve.transition_margin,
        None,
    );
    let outcome = manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session: &session,
        lease_id: &lease_id,
        proxy_session_id: &proxy_session_id,
        preparation: &stale_preparation,
        now_ms: 2_000,
        payload: HlsTerminalCommitPayload::Unavailable(HlsTerminalTailCompatibility::AssetRevisionMismatch),
        asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(
            HlsRuntimeCustomTailAssetIdentity::new(current_asset.reason, stale_asset),
            move || Some(current_asset),
        ),
    });

    assert_eq!(outcome, HlsTerminalCommitOutcome::LockBusy { retry_before_ms: retry_at_ms });
    assert_eq!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("unauthorized incoming keeps lease available")
            .playback_mode,
        HlsLeasePlaybackMode::Live
    );
    spawn_terminal_commit_retry_worker(
        Arc::clone(&manager.sessions),
        Arc::clone(&manager.access_leases),
        Arc::clone(&manager.terminal_commit_retries),
        Arc::clone(&manager.terminal_commit_clock),
        worker_token,
        HlsProxyManager::try_commit_access_lease_terminal_decision,
    );
    wait_for_terminal_commit_owner(&manager).await;
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_001)
            .await
            .expect("authorized owner terminalizes the lease")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
}

#[tokio::test]
async fn hls_terminal_commit_submission_newer_command_replaces_lock_busy_owner() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-owner-replacement").await;
    let stale_asset_identity = plan.asset_identity;
    let session_guard = session.write().await;

    assert!(matches!(
        manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
            session: &session,
            lease_id: &lease_id,
            proxy_session_id: &proxy_session_id,
            preparation: &preparation,
            now_ms: 2_000,
            payload: HlsTerminalCommitPayload::Tail {
                plan,
                media_guard: HlsTerminalCommitMediaGuard::empty_for_test(),
            },
            asset_revision_guard: HlsTerminalAssetRevisionGuard::for_runtime_tail(stale_asset_identity, || None,),
        }),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    assert!(matches!(
        manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
            session: &session,
            lease_id: &lease_id,
            proxy_session_id: &proxy_session_id,
            preparation: &preparation,
            now_ms: 2_000,
            payload: HlsTerminalCommitPayload::Unavailable(HlsTerminalTailCompatibility::AssetRevisionMismatch,),
            asset_revision_guard: HlsTerminalAssetRevisionGuard::matching_for_test(None),
        }),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
    drop(session_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("replacement command terminalizes the lease")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::AssetRevisionMismatch, .. }
    ));
}

#[tokio::test]
async fn hls_terminal_commit_retry_never_commits_after_the_safe_deadline() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-deadline").await;
    let after_deadline_ms =
        preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch().saturating_add(1);
    manager.terminal_commit_clock.set_fixed_now_ms(after_deadline_ms);
    let session_guard = session.write().await;

    assert_eq!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed
    );
    assert_eq!(manager.terminal_commit_retries.owner_count(), 0);
    drop(session_guard);

    assert_eq!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, after_deadline_ms)
            .await
            .expect("deadline-expired lease remains live")
            .playback_mode,
        HlsLeasePlaybackMode::Live
    );
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_initial_and_retry_attempts_reject_exclusive_safe_deadline() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("initial-exclusive-deadline").await;
    let safe_deadline_ms = preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    manager.terminal_commit_clock.set_fixed_now_ms(safe_deadline_ms);

    assert_eq!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed
    );
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, safe_deadline_ms)
            .await
            .expect("exclusive deadline leaves the initial-attempt lease live")
            .playback_mode,
        HlsLeasePlaybackMode::Live
    ));
    assert!(!session.read().await.has_terminal_tail_protections());

    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-exclusive-deadline").await;
    let safe_deadline_ms = preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    let session_guard = session.write().await;
    assert!(matches!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
    manager.terminal_commit_clock.set_fixed_now_ms(safe_deadline_ms);
    drop(session_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, safe_deadline_ms)
            .await
            .expect("exclusive deadline leaves the retry lease live")
            .playback_mode,
        HlsLeasePlaybackMode::Live
    ));
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_session_replacement_cancels_the_detached_retry() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-session-replacement").await;
    let session_key = session.read().await.key.clone();
    let lease_guard = manager.access_leases.write().await;

    assert!(matches!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    manager.sessions.remove_session(&session_key, &proxy_session_id).await.expect("remove prepared session");
    let (replacement, outcome) = manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
    assert_eq!(outcome, HlsSessionStoreOutcome::Created);
    assert!(!Arc::ptr_eq(&session, &replacement));
    drop(lease_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert_eq!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("replacement race keeps lease live")
            .playback_mode,
        HlsLeasePlaybackMode::Live
    );
    assert!(!session.read().await.has_terminal_tail_protections());
    assert!(!replacement.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_submission_current_session_replaces_stale_incarnation_owner() {
    let (manager, stale_session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-current-session-replacement").await;
    let session_key = stale_session.read().await.key.clone();
    let lease_guard = manager.access_leases.write().await;

    assert!(matches!(
        commit_terminal_plan(
            &manager,
            &stale_session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_000,
            Arc::clone(&plan),
        ),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    manager.sessions.remove_session(&session_key, &proxy_session_id).await.expect("remove stale session incarnation");
    let (current_session, outcome) = manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
    assert_eq!(outcome, HlsSessionStoreOutcome::Created);
    assert!(!Arc::ptr_eq(&stale_session, &current_session));
    {
        let mut current = current_session.write().await;
        assert_eq!(complete_failed_acceptance_episode(&mut current, 2_000), preparation.origin_progress_generation);
    }

    assert!(matches!(
        commit_terminal_plan(&manager, &current_session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
    drop(lease_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("current session command terminalizes the lease")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
    assert!(!stale_session.read().await.has_terminal_tail_protections());
    assert!(current_session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_submission_stale_session_cannot_replace_current_owner() {
    let (manager, stale_session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-stale-session-later").await;
    let session_key = stale_session.read().await.key.clone();
    manager.sessions.remove_session(&session_key, &proxy_session_id).await.expect("remove stale session incarnation");
    let (current_session, outcome) = manager.get_or_create_session_with_outcome(session_key, b"secret", 2_000).await;
    assert_eq!(outcome, HlsSessionStoreOutcome::Created);
    {
        let mut current = current_session.write().await;
        assert_eq!(complete_failed_acceptance_episode(&mut current, 2_000), preparation.origin_progress_generation);
    }
    let lease_guard = manager.access_leases.write().await;
    let index_guard = manager.sessions.hold_index_write_for_test().await;

    assert!(matches!(
        commit_terminal_plan(
            &manager,
            &current_session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_000,
            Arc::clone(&plan),
        ),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    assert!(matches!(
        commit_terminal_plan(&manager, &stale_session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    assert_eq!(manager.terminal_commit_retries.owner_count(), 1);
    drop(index_guard);
    drop(lease_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert!(matches!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("current session owner survives stale later submission")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
    assert!(!stale_session.read().await.has_terminal_tail_protections());
    assert!(current_session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_media_progress_before_retry_cancels_owner_and_keeps_lease_live() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-recovery").await;
    let lease_guard = manager.access_leases.write().await;

    assert!(matches!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    {
        let mut session = session.write().await;
        assert_eq!(session.origin_control.progress_generation, preparation.origin_progress_generation);
        session.origin_control.record_media_progress(2_100, 12_000);
        assert_eq!(
            session.origin_control.progress_generation,
            preparation.origin_progress_generation.saturating_add(1)
        );
    }
    drop(lease_guard);

    wait_for_terminal_commit_owner(&manager).await;
    assert_eq!(
        manager
            .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
            .await
            .expect("recovered lease")
            .playback_mode,
        HlsLeasePlaybackMode::Live
    );
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_lease_end_before_retry_cancels_owner_without_mutation() {
    let (manager, session, proxy_session_id, lease_id, preparation, plan) =
        prepared_terminal_commit_fixture("retry-lease-end").await;
    let mut lease_guard = manager.access_leases.write().await;

    assert!(matches!(
        commit_terminal_plan(&manager, &session, &lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::LockBusy { .. }
    ));
    let _release = lease_guard.deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::ImmediateEnd);
    drop(lease_guard);

    wait_for_terminal_commit_owner(&manager).await;
    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("ended lease remains stored");
    assert_eq!(lease.state, HlsAccessLeaseState::Denied);
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Ended);
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_new_media_generation_supersedes_prepared_plan() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("failed acceptance permits terminal preparation");

    session.write().await.advance_media_readiness_generation();
    assert_eq!(
        commit_terminal_plan(
            &manager,
            &session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_100,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::SupersededGeneration
    );

    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_200)
        .await
        .expect("lease remains available");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(!session.read().await.has_terminal_tail_protections());
}

#[tokio::test]
async fn hls_terminal_commit_lease_end_prevents_commit_and_gc_protection() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("lease-a".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let mut preparation_request = terminal_preparation_request(&lease_id, &proxy_session_id, progress_generation);
    preparation_request.reserve.guaranteed_reserve_ms = 100_000;
    preparation_request.reserve.guaranteed_media_horizon_ms = preparation_request
        .reserve
        .conservative_playback_position_ms
        .saturating_add(preparation_request.reserve.guaranteed_reserve_ms);
    preparation_request.cutover_timing = HlsLeaseCutoverTiming::from_reserve(
        preparation_request.now_ms,
        preparation_request.reserve.guaranteed_reserve_ms,
        preparation_request.reserve.transition_margin,
        None,
    );
    let preparation = manager
        .prepare_access_lease_terminal_tail(preparation_request)
        .await
        .expect("failed acceptance permits terminal preparation");

    assert_eq!(
        commit_terminal_plan(
            &manager,
            &session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            61_000,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::LeaseNoLongerEligible
    );

    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 61_000)
        .await
        .expect("expired lease remains stored until lifecycle cleanup");
    assert_eq!(lease.state, HlsAccessLeaseState::Expired);
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Ended);
    assert!(!session.read().await.has_terminal_tail_protections());
}

pub(in crate::manager::tests) async fn assert_terminal_and_live_lease_modes(
    manager: &HlsProxyManager,
    proxy_session_id: &ProxySessionId,
    terminal_lease_id: &HlsAccessLeaseId,
    live_lease_id: &HlsAccessLeaseId,
) {
    assert!(matches!(
        manager
            .access_lease_response_snapshot(terminal_lease_id, proxy_session_id, 2_100)
            .await
            .expect("terminal lease")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
    assert_eq!(
        manager
            .access_lease_response_snapshot(live_lease_id, proxy_session_id, 2_100)
            .await
            .expect("live lease")
            .playback_mode,
        HlsLeasePlaybackMode::Live
    );
}

#[tokio::test]
async fn hls_cutover_policy_terminal_commit_uses_lease_deadline_and_keeps_farther_lease_live() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let terminal_lease_id = HlsAccessLeaseId("terminal".to_string());
    let live_lease_id = HlsAccessLeaseId("live".to_string());
    {
        let mut leases = manager.access_leases.write().await;
        leases.prepare_access_lease(access_lease(&terminal_lease_id.0, &proxy_session_id));
        leases.prepare_access_lease(access_lease(&live_lease_id.0, &proxy_session_id));
    }
    assert!(
        publish_manifest_snapshot(&manager, &terminal_lease_id, &proxy_session_id, manifest_snapshot(1), 2_000,).await
    );
    assert!(publish_manifest_snapshot(&manager, &live_lease_id, &proxy_session_id, manifest_snapshot(1), 2_000,).await);
    let progress_generation = {
        let mut session = session.write().await;
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let far_reserve = HlsLeaseReserveSnapshot {
        guaranteed_media_horizon_ms: 36_000,
        conservative_playback_position_ms: 12_000,
        guaranteed_reserve_ms: 24_000,
        cutover_required: false,
        ..cutover_reserve()
    };
    let far_timing = HlsLeaseCutoverTiming::from_reserve(
        2_000,
        far_reserve.guaranteed_reserve_ms,
        far_reserve.transition_margin,
        None,
    );
    assert!(manager
        .prepare_access_lease_terminal_tail(HlsTerminalTailPreparationRequest {
            lease_id: &live_lease_id,
            proxy_session_id: &proxy_session_id,
            manifest_snapshot_generation: 1,
            cursor_generation: 0,
            reserve: far_reserve,
            cutover_timing: far_timing,
            commit_window: HlsTerminalCommitWindow::NotDue,
            now_ms: 2_000,
            origin_progress_generation: progress_generation,
            media_readiness_generation: 0,
            last_media_progress_at_ms: None,
        })
        .await
        .is_none());
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &terminal_lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("terminal preparation");
    assert_eq!(
        preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(),
        2_000_u64.saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis())
    );
    assert_eq!(far_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(), 14_000);
    let plan = terminal_plan(preparation.decision_generation, &proxy_session_id, &terminal_lease_id);
    let expected_protection = Arc::clone(&plan.protected_base_proxy_seqs);

    assert_eq!(
        commit_terminal_plan(&manager, &session, &terminal_lease_id, &proxy_session_id, &preparation, 2_000, plan,),
        HlsTerminalCommitOutcome::Committed
    );

    let session = session.read().await;
    assert_eq!(
        session.terminal_tail_protection(&terminal_lease_id).map(|protection| &protection.base_proxy_seqs),
        Some(&expected_protection)
    );
    assert_eq!(
        session.origin_control.progress_phase,
        super::super::super::origin_progress::HlsOriginProgressPhase::TerminalPartial
    );
    drop(session);
    assert_terminal_and_live_lease_modes(&manager, &proxy_session_id, &terminal_lease_id, &live_lease_id).await;
}

#[tokio::test]
async fn resource_denial_does_not_destroy_existing_terminal_tail() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("terminal".to_string());
    let plan = terminal_plan(7, &proxy_session_id, &lease_id);
    let mut lease = access_lease(&lease_id.0, &proxy_session_id);
    lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&plan));
    manager.access_leases.write().await.prepare_access_lease(lease);
    {
        let mut session = session.write().await;
        assert_eq!(
            session.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation: plan.generation,
                    base_proxy_seqs: Arc::clone(&plan.protected_base_proxy_seqs),
                    key_bindings: plan.key_bindings(),
                },
            ),
            super::super::super::session::HlsTerminalTailProtectionInstall::Installed
        );
    }

    assert_eq!(
        manager.deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,).await,
        HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
    );

    let denied = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("denied lease remains stored until lifecycle cleanup");
    assert_eq!(denied.state, HlsAccessLeaseState::Denied);
    assert_eq!(denied.playback_mode, HlsLeasePlaybackMode::TerminalTail(Arc::clone(&plan)));
    assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());

    assert_eq!(
        manager.deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::ImmediateEnd).await,
        HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
    );
    assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());

    manager.remove_access_lease(&lease_id).await;
    assert!(session.read().await.terminal_tail_protection(&lease_id).is_none());
    assert_eq!(
        manager.access_leases.write().await.deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::ImmediateEnd),
        HlsAccessLeaseDenialOutcome::UnknownLease
    );
}

#[tokio::test]
async fn terminal_tail_protection_is_retained_until_bounded_lease_removal() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("terminal-stale-protection".to_string());
    let plan = terminal_plan(7, &proxy_session_id, &lease_id);
    let stale_generation = HlsTerminalTailGeneration(plan.generation.0.saturating_add(1));
    let mut lease = access_lease(&lease_id.0, &proxy_session_id);
    lease.playback_mode = HlsLeasePlaybackMode::TerminalTail(Arc::clone(&plan));
    manager.access_leases.write().await.prepare_access_lease(lease);
    assert_eq!(
        session.write().await.install_terminal_tail_protection(
            lease_id.clone(),
            HlsTerminalTailProtection {
                generation: stale_generation,
                base_proxy_seqs: Arc::from([9_999]),
                key_bindings: Arc::from([]),
            },
        ),
        super::super::super::session::HlsTerminalTailProtectionInstall::Installed
    );

    assert_eq!(
        manager.deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,).await,
        HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
    );

    assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());
    manager.remove_access_lease(&lease_id).await;
    assert!(session.read().await.terminal_tail_protection(&lease_id).is_none());
}

#[tokio::test]
async fn terminal_unavailable_protection_is_retained_until_bounded_lease_removal() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("terminal-unavailable".to_string());
    let decision_generation = 11;
    let generation = HlsTerminalTailGeneration(decision_generation);
    let mut lease = access_lease(&lease_id.0, &proxy_session_id);
    lease.playback_mode = HlsLeasePlaybackMode::TerminalUnavailable {
        decision_generation,
        reason: HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
    };
    manager.access_leases.write().await.prepare_access_lease(lease);
    assert_eq!(
        session.write().await.install_terminal_tail_protection(
            lease_id.clone(),
            HlsTerminalTailProtection { generation, base_proxy_seqs: Arc::from([41_u64]), key_bindings: Arc::from([]) },
        ),
        super::super::super::session::HlsTerminalTailProtectionInstall::Installed
    );

    assert_eq!(
        manager.deny_access_lease(&lease_id, HlsAccessLeaseDenialMode::PreserveCommittedFiniteTail,).await,
        HlsAccessLeaseDenialOutcome::FiniteDecisionPreserved
    );

    let denied = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_000)
        .await
        .expect("denied lease remains stored until lifecycle cleanup");
    assert_eq!(denied.state, HlsAccessLeaseState::Denied);
    assert!(matches!(denied.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
    assert!(session.read().await.terminal_tail_protection(&lease_id).is_some());
    manager.remove_access_lease(&lease_id).await;
    assert!(session.read().await.terminal_tail_protection(&lease_id).is_none());
}

#[tokio::test]
async fn hls_terminal_commit_protection_capacity_has_typed_unavailable_outcome() {
    let config = HlsCacheConfig::from(&HlsCacheConfigDto::default());
    let manager = HlsProxyManager::with_hls_cache_config(&config);
    let (session, _) =
        manager.get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 1_000).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId("overflow".to_string());
    manager.access_leases.write().await.prepare_access_lease(access_lease(&lease_id.0, &proxy_session_id));
    assert!(publish_manifest_snapshot(&manager, &lease_id, &proxy_session_id, manifest_snapshot(1), 2_000).await);
    let progress_generation = {
        let mut session = session.write().await;
        for index in 0..HLS_TERMINAL_TAIL_PROTECTION_CAPACITY {
            session.install_terminal_tail_protection(
                HlsAccessLeaseId(format!("occupied-{index}")),
                HlsTerminalTailProtection {
                    generation: HlsTerminalTailGeneration(1),
                    base_proxy_seqs: Arc::from([u64::try_from(index).unwrap_or(u64::MAX)]),
                    key_bindings: Arc::from([]),
                },
            );
        }
        complete_failed_acceptance_episode(&mut session, 2_000)
    };
    let preparation = manager
        .prepare_access_lease_terminal_tail(terminal_preparation_request(
            &lease_id,
            &proxy_session_id,
            progress_generation,
        ))
        .await
        .expect("terminal preparation");

    assert_eq!(
        commit_terminal_plan(
            &manager,
            &session,
            &lease_id,
            &proxy_session_id,
            &preparation,
            2_000,
            terminal_plan(preparation.decision_generation, &proxy_session_id, &lease_id),
        ),
        HlsTerminalCommitOutcome::Committed
    );

    let lease = manager
        .access_lease_response_snapshot(&lease_id, &proxy_session_id, 2_100)
        .await
        .expect("terminal unavailable lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable {
            reason: HlsTerminalTailCompatibility::ProtectionCapacityExceeded,
            ..
        }
    ));
    let session = session.read().await;
    assert_eq!(session.terminal_tail_protection_count(), HLS_TERMINAL_TAIL_PROTECTION_CAPACITY);
    assert!(session.terminal_tail_protection(&lease_id).is_none());
}
