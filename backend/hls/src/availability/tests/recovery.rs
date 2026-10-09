use super::*;

#[test]
fn hls_terminal_commit_pending_fallback_preserves_a_retryable_fail_closed_handoff() {
    let safe_deadline_ms = 10_000;
    let handoff_budget_ms = HlsTerminalCommitAcquisitionBudgetMs::fail_closed_handoff_from_retry_policy().as_millis();

    assert_eq!(
        terminal_pending_fallback_commit_at_ms(safe_deadline_ms),
        safe_deadline_ms.saturating_sub(handoff_budget_ms)
    );
    assert_eq!(
        safe_deadline_ms.saturating_sub(terminal_pending_fallback_commit_at_ms(safe_deadline_ms)),
        handoff_budget_ms
    );
    assert_eq!(terminal_pending_fallback_commit_at_ms(handoff_budget_ms.saturating_sub(1)), 0);
}

#[tokio::test]
async fn hls_terminal_commit_pending_ready_completion_selects_tail_without_a_client_retry() {
    let coordinator = Arc::new(HlsTerminalPendingCoordinator::default());
    let bundle_key = pending_decision_bundle_key();
    let bundle = pending_decision_ready_bundle(bundle_key);
    let (ticket, publisher) = prepared_terminal_bundle_completion_channel_for_test(bundle_key);
    let (_fallback_tx, fallback_rx) = oneshot::channel::<()>();
    let (decision_tx, decision_rx) = oneshot::channel();
    let asset_guard = HlsTerminalAssetRevisionGuard::matching_for_test(Some(bundle_key.asset));

    assert_eq!(
        coordinator.register(pending_decision_owner_key(bundle_key), &asset_guard, move |ownership| async move {
            let decision = await_terminal_pending_decision(ticket, &ownership, bundle_key, async move {
                assert!(fallback_rx.await.is_ok());
            })
            .await;
            assert!(decision_tx.send(decision).is_ok());
        },),
        HlsTerminalPendingRegistration::Scheduled
    );
    publisher.publish(HlsPreparedTerminalBundleCompletion::Ready { bundle: Arc::clone(&bundle) });

    let decision = decision_rx.await.expect("pending decision completes");
    assert!(matches!(decision, Some(HlsTerminalPendingDecision::Ready(actual)) if Arc::ptr_eq(&actual, &bundle)));
    assert_eq!(coordinator.owner_count(), 0);
}

#[tokio::test]
async fn hls_terminal_commit_pending_fallback_selects_terminal_unavailable_without_a_client_retry() {
    let coordinator = Arc::new(HlsTerminalPendingCoordinator::default());
    let bundle_key = pending_decision_bundle_key();
    let (ticket, _publisher) = prepared_terminal_bundle_completion_channel_for_test(bundle_key);
    let (fallback_tx, fallback_rx) = oneshot::channel::<()>();
    let (decision_tx, decision_rx) = oneshot::channel();
    let asset_guard = HlsTerminalAssetRevisionGuard::matching_for_test(Some(bundle_key.asset));

    assert_eq!(
        coordinator.register(pending_decision_owner_key(bundle_key), &asset_guard, move |ownership| async move {
            let decision = await_terminal_pending_decision(ticket, &ownership, bundle_key, async move {
                assert!(fallback_rx.await.is_ok());
            })
            .await;
            assert!(decision_tx.send(decision).is_ok());
        },),
        HlsTerminalPendingRegistration::Scheduled
    );
    assert!(fallback_tx.send(()).is_ok());

    assert!(matches!(
        decision_rx.await,
        Ok(Some(HlsTerminalPendingDecision::Unavailable(HlsTerminalTailCompatibility::TerminalMediaNotReady)))
    ));
    assert_eq!(coordinator.owner_count(), 0);
}

#[tokio::test]
async fn asset_reload_supersedes_pending_custom_tail_but_not_committed_bytes() {
    let fixture = post_refresh_terminal_fixture("runtime-asset-reload", true).await;
    let reason = HlsRuntimeCustomTailReason::LowPriorityPreempted;
    let old_asset = snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason).expect("old low-priority asset");
    let target_duration_ms = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .and_then(|lease| lease.last_manifest_snapshot.map(|manifest| manifest.target_duration_ms))
        .expect("published target duration");
    let old_key = prepared_terminal_bundle_key(&old_asset.asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let publisher = fixture
        .ctx
        .hls_proxy
        .install_controlled_terminal_bundle_flight_for_test(old_key)
        .expect("controlled old-asset preparation");
    let pending = commit_hls_runtime_custom_tail(
        fixture.ctx.clone(),
        HlsRuntimeCustomTailRequest {
            session: Arc::clone(&fixture.session),
            proxy_session_id: fixture.proxy_session_id.clone(),
            lease_id: fixture.lease_id.clone(),
            reason,
            now_ms: fixture.now_ms,
        },
    )
    .await;
    assert_eq!(pending, HlsRuntimeCustomTailOutcome::PendingOwnerRegistered);
    wait_for_terminal_pending_owners(&fixture, 1).await;

    let mut revised_bytes = LOW_PRIORITY_ASSET_BYTES.to_vec();
    *revised_bytes.last_mut().expect("non-empty low-priority asset") ^= 1;
    fixture
        .ctx
        .app_config
        .custom_stream_response
        .store(Some(runtime_custom_responses_with_low_priority(&revised_bytes)));
    let old_bundle = build_prepared_terminal_bundle(&old_asset.asset, old_key).expect("old controlled relative bundle");
    publisher.publish(HlsPreparedTerminalBundleCompletion::Ready { bundle: old_bundle });
    wait_for_terminal_pending_owners(&fixture, 0).await;
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("reloaded lease remains stored");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);

    let (_, committed) = commit_runtime_custom_reason(&fixture, reason, true).await;
    let committed_zero = segment_bytes(&committed, 0);
    fixture.ctx.app_config.custom_stream_response.store(Some(runtime_custom_responses()));
    let replay = wait_for_runtime_custom_plan(&fixture).await;

    assert_eq!(replay.asset_identity, committed.asset_identity);
    assert_eq!(segment_bytes(&replay, 0), committed_zero);
}

#[tokio::test(start_paused = true)]
async fn failed_closed_retry_capacity_retains_owner() {
    let fixture = post_refresh_terminal_fixture("capacity-retained-owner", false).await;
    fixture.ctx.hls_proxy.set_terminal_commit_retry_capacity_for_test(0);
    register_real_post_refresh_owner(
        &fixture,
        super::super::super::refresh::HlsPostRefreshAvailabilityReason::HardManifestFailure,
    )
    .await;
    assert_availability_owner_registered(&fixture);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fixture.ctx.hls_proxy.availability_reevaluations().owner_count(),
        1,
        "retry-capacity pressure must not drop the last Availability owner"
    );

    fixture.ctx.hls_proxy.set_terminal_commit_retry_capacity_for_test(
        super::super::super::terminal_commit::HLS_TERMINAL_COMMIT_RETRY_CAPACITY,
    );
    fixture.ctx.hls_proxy.notify_session_evidence_changed(&fixture.proxy_session_id);
    wait_for_availability_owner_completion(&fixture).await;
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("capacity retry resolves the live lease");
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
}

#[test]
fn hls_recovery_timing_publication_late_with_large_reserve_keeps_evidence_without_starting_burst() {
    assert_eq!(
        recovery_trigger_source(HlsOriginPathCondition::ProgressExpected, true, false, false),
        HlsRecoveryTriggerSource::PublicationLate
    );
    let directive = acceptance_directive_for_progress(
        publication_late_decision(30_000),
        lease_timing_seed(),
        None,
        HlsRecoveryTriggerDiagnostic::new(HlsRecoveryTriggerSource::PublicationLate),
    );

    assert_eq!(directive.trigger, HlsManifestAcceptanceTrigger::None);
    assert_eq!(directive.timing_seed, Some(lease_timing_seed()));
}

#[test]
fn hls_recovery_timing_publication_late_with_narrow_reserve_keeps_full_burst_pending() {
    let directive = acceptance_directive_for_progress(
        publication_late_decision(14_000),
        lease_timing_seed(),
        None,
        HlsRecoveryTriggerDiagnostic::new(HlsRecoveryTriggerSource::ReservePressure),
    );

    assert_eq!(directive.trigger, HlsManifestAcceptanceTrigger::RecoveryRequired);
    assert_eq!(directive.timing_seed.map(|seed| seed.workload.burst), Some(HlsRecoveryBurstWorkload::FullBurstPending));

    let plan = shared::model::HlsManifestRecoveryBurstLevel::Beast.plan();
    let seed = directive.timing_seed.expect("narrow reserve keeps timing evidence");
    let timing = HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms: 1_000,
        burst_plan: plan,
        target_duration_ms: seed.target_duration_ms,
        transition_margin: seed.transition_margin,
        workload: seed.workload,
        observed_latency: HlsObservedRecoveryLatency::default(),
        required_terminal_media_key: seed.required_terminal_media_key,
        terminal_media_preparation: seed.terminal_media_preparation,
        policy: HlsRecoveryTimingPolicy::new(
            HlsOperationTimeoutMs::from_millis(3_000),
            HlsOperationTimeoutMs::from_millis(30_000),
            HlsRecoveryEtaMs::from_millis(3_000),
            HlsRecoveryEtaMs::from_millis(13_000),
        ),
    });
    let mut episode = super::super::super::manifest_acceptance::HlsManifestAcceptanceEpisode::new(
        super::super::super::manifest_acceptance::HlsManifestAcceptanceGeneration(1),
        1_000,
        plan,
        directive.trigger,
        &timing,
    );
    assert_eq!(episode.required_candidates(), plan.total_candidates());
    episode.record_full_burst_candidates(plan.total_candidates());
    assert!(episode.full_burst_completed);
    assert_eq!(episode.completed_burst_candidates, plan.total_candidates());
}

#[test]
fn hls_recovery_timing_unbound_candidate_covers_key_and_map_independent_of_old_manifest() {
    let unknown = HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling();

    assert_eq!(unknown.burst, HlsRecoveryBurstWorkload::FullBurstPending);
    assert_eq!(unknown.segment, HlsRecoverySegmentWorkload::Aes128SegmentFetchWithKeyFetch);
    assert_eq!(unknown.map, HlsRecoveryMapWorkload::Fetch);
}

#[test]
fn hls_session_recovery_pressure_selects_required_lease_over_smaller_raw_reserve() {
    let smaller_not_required = evaluated_pressure("lease-a", 4_000, &[1_000, 9_000], 2_000);
    let larger_required = evaluated_pressure("lease-b", 8_000, &[8_000, 4_000], 5_000);

    assert!(smaller_not_required.reserve.guaranteed_reserve_ms < larger_required.reserve.guaranteed_reserve_ms);
    assert!(!smaller_not_required.reserve.recovery_required);
    assert!(larger_required.reserve.recovery_required);
    let pressure = aggregate_session_recovery_pressure([smaller_not_required, larger_required.clone()])
        .expect("active lease pressure");

    assert!(pressure.any_recovery_required);
    assert!(!pressure.any_cutover_required);
    assert_eq!(pressure.controlling.lease_id, larger_required.lease_id);
    let seed = acceptance_timing_seed_for_pressure(&pressure.controlling);
    assert_eq!(seed.target_duration_ms, 8_000);
    assert_eq!(seed.transition_margin.as_millis(), 8_000);
}

#[test]
fn hls_session_recovery_pressure_tie_break_is_stable_by_lease_id() {
    let lease_b = evaluated_pressure("lease-b", 4_000, &[2_000, 2_000], 4_000);
    let mut lease_a = lease_b.clone();
    lease_a.lease_id = HlsAccessLeaseId("lease-a".to_string());

    let forward = aggregate_session_recovery_pressure([lease_b.clone(), lease_a.clone()]).expect("forward pressure");
    let reverse = aggregate_session_recovery_pressure([lease_a, lease_b]).expect("reverse pressure");

    assert_eq!(forward.controlling.lease_id.0, "lease-a");
    assert_eq!(reverse.controlling.lease_id.0, "lease-a");
}

#[test]
fn hls_session_recovery_pressure_cursor_change_is_observed_by_atomic_commit() {
    let mut session = atomic_pressure_session();
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    let lease_id =
        install_atomic_pressure_lease(&mut leases, &proxy_session_id, "lease-a", pressure_manifest_at(0, 8_000), 1_000);
    let before = evaluate_and_commit_session_recovery_pressure(
        &mut leases,
        &mut session,
        &proxy_session_id,
        100,
        atomic_pressure_policy(),
    )
    .expect("initial pressure");
    assert!(!before.decision.evaluate_lease_cutovers);
    let identity = leases
        .response_snapshot(&lease_id, &proxy_session_id, 100)
        .and_then(|lease| lease.media_identity())
        .expect("live media identity");
    assert!(leases
        .record_segment_request_started_if_identity_matches(&lease_id, &proxy_session_id, identity, 2, 101,)
        .is_some());

    let after = evaluate_and_commit_session_recovery_pressure(
        &mut leases,
        &mut session,
        &proxy_session_id,
        101,
        atomic_pressure_policy(),
    )
    .expect("cursor pressure");

    assert!(after.decision.evaluate_lease_cutovers);
}

#[test]
fn hls_session_recovery_pressure_new_urgent_lease_controls_atomic_commit() {
    let mut session = atomic_pressure_session();
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    install_atomic_pressure_lease(&mut leases, &proxy_session_id, "lease-a", pressure_manifest_at(0, 8_000), 1_000);
    let before = evaluate_and_commit_session_recovery_pressure(
        &mut leases,
        &mut session,
        &proxy_session_id,
        100,
        atomic_pressure_policy(),
    )
    .expect("initial pressure");
    assert!(!before.decision.evaluate_lease_cutovers);
    install_atomic_pressure_lease(&mut leases, &proxy_session_id, "lease-b", pressure_manifest_at(2, 10_000), 1_000);

    let after = evaluate_and_commit_session_recovery_pressure(
        &mut leases,
        &mut session,
        &proxy_session_id,
        101,
        atomic_pressure_policy(),
    )
    .expect("urgent pressure");

    assert!(after.decision.evaluate_lease_cutovers);
    assert_eq!(after.timing_seed.target_duration_ms, 10_000);
}

#[test]
fn hls_session_recovery_pressure_expired_controller_is_excluded_atomically() {
    let mut session = atomic_pressure_session();
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    install_atomic_pressure_lease(&mut leases, &proxy_session_id, "lease-a", pressure_manifest_at(0, 8_000), 1_000);
    install_atomic_pressure_lease(&mut leases, &proxy_session_id, "lease-b", pressure_manifest_at(2, 10_000), 150);
    let urgent = evaluate_and_commit_session_recovery_pressure(
        &mut leases,
        &mut session,
        &proxy_session_id,
        100,
        atomic_pressure_policy(),
    )
    .expect("urgent pressure");
    assert_eq!(urgent.timing_seed.target_duration_ms, 10_000);

    let after_expiry = evaluate_and_commit_session_recovery_pressure(
        &mut leases,
        &mut session,
        &proxy_session_id,
        200,
        atomic_pressure_policy(),
    )
    .expect("remaining pressure");

    assert_eq!(after_expiry.timing_seed.target_duration_ms, 8_000);
    assert!(!after_expiry.decision.evaluate_lease_cutovers);
}

#[test]
fn hls_session_recovery_pressure_new_publication_lateness_uses_current_reserve_evidence() {
    let mut session = atomic_pressure_session();
    session.origin_control.path_condition = HlsOriginPathCondition::ProgressExpected;
    session.origin_control.last_media_progress_at_ms = Some(0);
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    install_atomic_pressure_lease(&mut leases, &proxy_session_id, "lease-a", pressure_manifest_at(0, 8_000), 20_000);
    let policy = HlsRecoveryPressurePolicy {
        burst_plan: shared::model::HlsManifestRecoveryBurstPlan { slots: 1, lanes_per_slot: 1 },
        timing: HlsRecoveryTimingPolicy::new(
            HlsOperationTimeoutMs::from_millis(1_000),
            HlsOperationTimeoutMs::from_millis(10_000),
            HlsRecoveryEtaMs::from_millis(0),
            HlsRecoveryEtaMs::from_millis(2_000),
        ),
    };

    let pressure =
        evaluate_and_commit_session_recovery_pressure(&mut leases, &mut session, &proxy_session_id, 12_000, policy)
            .expect("publication-late pressure");

    assert!(pressure.decision.start_acceptance_episode);
    assert!(pressure.decision.close_admission);
    assert!(!pressure.decision.evaluate_lease_cutovers);
}
