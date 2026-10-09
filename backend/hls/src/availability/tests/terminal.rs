use super::*;

#[tokio::test]
async fn disabled_custom_responses_do_not_seed_terminal_media_preparation() {
    let hls_ctx = crate::HlsCtx::for_test(Config { custom_stream_response_enabled: false, ..Config::default() });
    let ctx = &hls_ctx;
    ctx.app_config.custom_stream_response.store(Some(runtime_custom_responses()));

    assert_eq!(terminal_media_timing_seed(ctx, 10_000), (None, HlsTerminalMediaPreparationState::Failed { key: None }));
}

#[test]
fn hls_terminal_commit_outcomes_never_turn_failures_into_live_serving() {
    for (outcome, reason) in [
        (HlsTerminalCommitOutcome::BundleNotReady, HlsTerminalFailedClosedReason::BundleNotReadyWithoutOwner),
        (HlsTerminalCommitOutcome::BundleIncompatible, HlsTerminalFailedClosedReason::BundleIncompatible),
        (HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed, HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed),
        (HlsTerminalCommitOutcome::RetryCapacityExceeded, HlsTerminalFailedClosedReason::RetryCapacityExceeded),
        (HlsTerminalCommitOutcome::RetryAttemptsExhausted, HlsTerminalFailedClosedReason::RetryAttemptsExhausted),
        (HlsTerminalCommitOutcome::RetryWorkerUnavailable, HlsTerminalFailedClosedReason::RuntimeUnavailable),
    ] {
        assert_eq!(
            terminal_resolution_for_commit_outcome(outcome, 1_000),
            HlsTerminalResolution::FailedClosed { reason }
        );
    }
    assert_eq!(
        terminal_resolution_for_commit_outcome(HlsTerminalCommitOutcome::LockBusy { retry_before_ms: 1_025 }, 1_000,),
        HlsTerminalResolution::Pending { retry_after_ms: 25 }
    );
}

#[test]
fn autonomous_terminal_live_allowed_means_no_cutover_is_required() {
    assert_eq!(
        classify_autonomous_terminal_resolution(HlsTerminalResolution::LiveAllowed),
        HlsAutonomousTerminalObservation::NoCutoverRequired,
    );
}

#[test]
fn live_reserve_wake_is_strictly_before_safe_terminal_deadline() {
    let now_ms = 1_000;
    let mut reserve = terminal_pending_commit_reserve();
    reserve.guaranteed_reserve_ms = reserve.guaranteed_reserve_ms.saturating_add(5_000);
    reserve.guaranteed_media_horizon_ms = reserve.guaranteed_reserve_ms;
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(now_ms, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);

    let deadline = live_reserve_deadline(now_ms, reserve, cutover_timing).expect("future acquisition wake");
    assert_eq!(deadline.next_reevaluation_at_ms, now_ms.saturating_add(5_000));
    assert!(deadline.next_reevaluation_at_ms < deadline.latest_safe_terminal_commit_at_ms);
}

#[tokio::test]
async fn hls_terminal_commit_pending_owner_store_ready_completion_commits_terminal_tail() {
    let fixture = terminal_pending_commit_fixture("pending-owner-ready-store").await;
    let bundle = fixture.ready_bundle();
    let (ticket, publisher) = prepared_terminal_bundle_completion_channel_for_test(fixture.bundle_key);
    let completed = fixture.register_owner(ticket);

    publisher.publish(HlsPreparedTerminalBundleCompletion::Ready { bundle });
    completed.await.expect("productive pending owner completes");

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalTail(ref plan)
            if plan.generation.0 == fixture.preparation.decision_generation
    ));
    assert_eq!(fixture.ctx.hls_proxy.terminal_pending().owner_count(), 0);
}

#[tokio::test]
async fn missing_terminal_base_timestamp_profile_commits_terminal_unavailable() {
    let base_bytes = terminal_base_without_timestamps();
    let fixture = terminal_pending_commit_fixture_with_base("pending-owner-missing-timestamp", &base_bytes).await;
    let bundle = fixture.ready_bundle();
    let (ticket, publisher) = prepared_terminal_bundle_completion_channel_for_test(fixture.bundle_key);
    let completed = fixture.register_owner(ticket);

    publisher.publish(HlsPreparedTerminalBundleCompletion::Ready { bundle });
    completed.await.expect("terminal owner completes fail-closed decision");

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal unavailable lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::MissingTimestampAnchor, .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn hls_terminal_commit_pending_owner_store_fallback_commits_terminal_unavailable() {
    let fixture = terminal_pending_commit_fixture("pending-owner-fallback-store").await;
    let (ticket, _publisher) = prepared_terminal_bundle_completion_channel_for_test(fixture.bundle_key);
    let completed = fixture.register_owner(ticket);

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis()))
        .await;
    completed.await.expect("productive fallback owner completes");

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable {
            decision_generation,
            reason: HlsTerminalTailCompatibility::TerminalMediaNotReady,
        } if decision_generation == fixture.preparation.decision_generation
    ));
    assert_eq!(fixture.ctx.hls_proxy.terminal_pending().owner_count(), 0);
}

#[tokio::test]
async fn hls_terminal_commit_pending_owner_store_progress_supersession_keeps_lease_live() {
    let fixture = terminal_pending_commit_fixture("pending-owner-progress-store").await;
    let bundle = fixture.ready_bundle();
    let (ticket, publisher) = prepared_terminal_bundle_completion_channel_for_test(fixture.bundle_key);
    let completed = fixture.register_owner(ticket);
    tokio::task::yield_now().await;

    {
        let mut session = fixture.session.write().await;
        session.origin_control.progress_generation = session.origin_control.progress_generation.saturating_add(1);
        session.origin_control.last_media_progress_at_ms = Some(fixture.now_ms.saturating_add(1));
    }
    fixture.ctx.hls_proxy.cancel_superseded_terminal_work_for_session(&fixture.proxy_session_id);
    publisher.publish(HlsPreparedTerminalBundleCompletion::Ready { bundle });
    completed.await.expect("cancelled pending owner completes");

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("superseded lease remains stored");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    assert!(!fixture.session.read().await.has_terminal_tail_protections());
    assert_eq!(fixture.ctx.hls_proxy.terminal_pending().owner_count(), 0);
}

#[tokio::test]
async fn terminal_pending_runtime_failure_commits_terminal_unavailable() {
    assert_terminal_pending_registration_failure_commits_unavailable(
        HlsTerminalPendingRegistration::RuntimeUnavailable,
        "pending-runtime-failure",
    )
    .await;
}

#[tokio::test]
async fn active_hls_preemption_commits_low_priority_preempted_tail_without_redirect() {
    let fixture = post_refresh_terminal_fixture("runtime-preemption", true).await;

    let (_, plan) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;
    let manifest = terminal_tail_manifest_body(&plan, &fixture.proxy_session_id, &fixture.lease_id)
        .expect("preemption plan route binding");

    assert_eq!(plan.reason, HlsRuntimeCustomTailReason::LowPriorityPreempted);
    assert!(manifest.ends_with("#EXT-X-ENDLIST\n"));
    assert!(!manifest.contains("/cvs/hls/"));
    assert!(matches!(
        fixture
            .ctx
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
            .await
            .expect("committed preemption lease")
            .playback_mode,
        HlsLeasePlaybackMode::TerminalTail(_)
    ));
}

#[tokio::test]
async fn unsafe_live_transport_evidence_commits_unavailable_without_terminal_bytes() {
    let fixture = post_refresh_terminal_fixture("runtime-unsafe-live-splice", true).await;
    prepare_runtime_custom_bundle(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted).await;
    let cache_key = fixture.session.read().await.segments.get(&0).expect("terminal-base segment").cache_key.clone();
    let metadata = fixture
        .ctx
        .hls_proxy
        .segment_cache()
        .metadata(&cache_key)
        .await
        .expect("cache metadata lookup")
        .expect("terminal-base metadata");
    tokio::fs::write(&metadata.path, with_internal_payload_continuity_jump(TERMINAL_ASSET_BYTES))
        .await
        .expect("replace test fixture with same-size unsafe bytes");

    let outcome = commit_hls_runtime_custom_tail(
        fixture.ctx.clone(),
        HlsRuntimeCustomTailRequest {
            session: Arc::clone(&fixture.session),
            proxy_session_id: fixture.proxy_session_id.clone(),
            lease_id: fixture.lease_id.clone(),
            reason: HlsRuntimeCustomTailReason::LowPriorityPreempted,
            now_ms: fixture.now_ms,
        },
    )
    .await;
    wait_for_terminal_pending_owners(&fixture, 0).await;
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("unsafe splice lease remains stored");

    assert_eq!(outcome, HlsRuntimeCustomTailOutcome::PendingOwnerRegistered);
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable {
            reason: HlsTerminalTailCompatibility::SpliceTransportFailure(
                super::super::super::HlsTsSpliceIncompatibility::ContinuityFailure { .. }
            ),
            ..
        }
    ));
    assert!(fixture.session.read().await.terminal_tail_protection(&fixture.lease_id).is_none());
}

#[tokio::test]
async fn preemption_tail_uses_low_priority_asset_not_channel_unavailable_asset() {
    let fixture = post_refresh_terminal_fixture("runtime-preemption-asset", true).await;
    let low_priority =
        snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, HlsRuntimeCustomTailReason::LowPriorityPreempted)
            .expect("low-priority asset");
    let channel = snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, HlsRuntimeCustomTailReason::ChannelUnavailable)
        .expect("channel-unavailable asset");

    let (_, plan) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;

    assert_eq!(plan.asset_identity, HlsRuntimeCustomTailAssetIdentity::from_asset(&low_priority));
    assert_ne!(plan.asset_identity.media, HlsRuntimeCustomTailAssetIdentity::from_asset(&channel).media);
}

#[tokio::test]
async fn preemption_tail_preserves_live_to_custom_pts_dts_pcr_and_cc() {
    let fixture = post_refresh_terminal_fixture("runtime-preemption-splice", true).await;
    let low_priority = configured_runtime_custom_buffer(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted);
    let live = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let expected_anchor = HlsTsSpliceAnchor::between(
        live.finite_hls_timestamp_profile().expect("live timestamp profile"),
        low_priority.finite_hls_timestamp_profile().expect("custom timestamp profile"),
    )
    .expect("compatible live-to-custom splice");

    let (_, plan) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;
    let first = segment_bytes(&plan, 0);
    let second = segment_bytes(&plan, 1);
    let first_profile = TransportStreamBuffer::new(first.to_vec())
        .finite_hls_timestamp_profile()
        .expect("first anchored custom profile");
    let second_profile = TransportStreamBuffer::new(second.to_vec())
        .finite_hls_timestamp_profile()
        .expect("second anchored custom profile");
    assert_eq!(first_profile.first_clock_90khz, expected_anchor.terminal_first_clock);
    assert!(first_profile.observed_pts_or_dts && first_profile.observed_pcr);
    assert!(second_profile.observed_pts_or_dts && second_profile.observed_pcr);
    assert_eq!(
        second_profile.first_clock_90khz.wrapping_add(1_u64 << 33).wrapping_sub(first_profile.first_clock_90khz)
            % (1_u64 << 33),
        902_400
    );
    let first_cc = payload_continuity_bounds(&first);
    let second_cc = payload_continuity_bounds(&second);
    assert!(!first_cc.is_empty());
    assert!(first_cc.iter().all(|(pid, (_, _, discontinuity))| *pid == 0x1fff || *discontinuity), "{first_cc:?}");
    for (pid, (_, last, _)) in first_cc {
        let (next, _, _) = second_cc.get(&pid).expect("PID continues into second custom segment");
        assert_eq!(*next, last.wrapping_add(1) & 0x0f, "PID {pid} continuity");
    }
}

#[tokio::test]
async fn preemption_tail_is_committed_at_safe_segment_boundary_without_waiting_for_reserve_cutover() {
    let fixture = post_refresh_terminal_fixture_with_progress("runtime-preemption-immediate", true, false).await;
    let lease_before = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("live lease before immediate cutover");
    assert_eq!(lease_before.playback_mode, HlsLeasePlaybackMode::Live);

    let (outcome, plan) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;

    assert!(matches!(
        outcome,
        HlsRuntimeCustomTailOutcome::Committed | HlsRuntimeCustomTailOutcome::PendingOwnerRegistered
    ));
    assert_eq!(plan.base_manifest.last_proxy_seq, lease_before.last_manifest_snapshot.unwrap().last_proxy_seq);
}

#[tokio::test]
async fn active_provider_exhaustion_commits_provider_exhausted_tail_after_grace() {
    assert_active_policy_reason_commits(
        "runtime-provider-exhausted",
        HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
    )
    .await;
}

#[tokio::test]
async fn active_user_exhaustion_commits_user_exhausted_tail() {
    assert_active_policy_reason_commits("runtime-user-exhausted", HlsRuntimeCustomTailReason::UserConnectionsExhausted)
        .await;
}

#[tokio::test]
async fn active_user_account_expiry_commits_account_expired_tail() {
    assert_active_policy_reason_commits("runtime-account-expired", HlsRuntimeCustomTailReason::UserAccountExpired)
        .await;
}

#[tokio::test]
async fn replay_conflict_commits_prepared_terminal_tail_before_safe_deadline() {
    let fixture = post_refresh_terminal_fixture("post-refresh-terminal", true).await;
    let safe_deadline =
        HlsLeaseCutoverTiming::from_reserve(fixture.now_ms, 12_900, HlsTransitionMarginMs::from_millis(12_000), None)
            .latest_safe_terminal_commit_at
            .as_millis_since_epoch();

    let evaluation = evaluate_active_terminal_leases_for_reevaluation(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        fixture.now_ms,
    )
    .await;

    assert_eq!(evaluation, HlsPostRefreshTerminalEvaluation::TerminalCommitted);
    assert!(fixture.now_ms < safe_deadline);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal lease remains available");
    assert!(
        matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalTail(_)),
        "prepared compatible asset must commit a terminal tail: {:?}",
        lease.playback_mode
    );
    assert_eq!(
        commit_terminal_tail_if_lease_reserve_requires_cutover(
            &fixture.ctx,
            &fixture.session,
            &fixture.proxy_session_id,
            &lease,
            safe_deadline.saturating_add(1),
        )
        .await,
        HlsTerminalResolution::Committed,
        "a later client observation sees the immutable terminal decision"
    );
}

#[tokio::test]
async fn incompatible_terminal_asset_commits_terminal_unavailable_without_client_request() {
    let fixture = post_refresh_terminal_fixture("post-refresh-unavailable", false).await;

    let evaluation = evaluate_active_terminal_leases_for_reevaluation(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        fixture.now_ms,
    )
    .await;

    assert_eq!(evaluation, HlsPostRefreshTerminalEvaluation::TerminalCommitted);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal-unavailable lease remains available");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::MissingAsset, .. }
    ));
    assert_eq!(
        commit_terminal_tail_if_lease_reserve_requires_cutover(
            &fixture.ctx,
            &fixture.session,
            &fixture.proxy_session_id,
            &lease,
            fixture.now_ms.saturating_add(60_000),
        )
        .await,
        HlsTerminalResolution::Committed,
        "a later client observation cannot reopen safe-deadline failure"
    );
}
