use super::*;
use crate::{
    late_custom_response::prepare_late_custom_response,
    manager::{HlsTerminalCommitPayload, HlsTerminalCommitRequest},
};

async fn late_fixture(name: &str) -> HlsTerminalPendingCommitFixture {
    let fixture = terminal_pending_commit_fixture(name).await;
    fixture.session.write().await.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
    fixture
}

async fn prepare(
    fixture: &HlsTerminalPendingCommitFixture,
) -> (Arc<crate::late_custom_response::HlsLateCustomResponsePlan>, HlsRuntimeCustomTailAssetIdentity) {
    prepare_late_custom_response(
        &fixture.ctx,
        &fixture.proxy_session_id,
        &fixture.lease_id,
        &fixture.preparation,
        fixture.now_ms,
    )
    .await
    .unwrap_or_else(|_| panic!("late custom plan"))
}

fn commit(
    fixture: &HlsTerminalPendingCommitFixture,
    plan: Arc<crate::late_custom_response::HlsLateCustomResponsePlan>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    now_ms: u64,
) -> HlsTerminalCommitOutcome {
    fixture.ctx.hls_proxy.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session: &fixture.session,
        lease_id: &fixture.lease_id,
        proxy_session_id: &fixture.proxy_session_id,
        preparation: &fixture.preparation,
        now_ms,
        payload: HlsTerminalCommitPayload::CustomResponse(plan),
        asset_revision_guard: terminal_asset_revision_guard(&fixture.ctx, expected_asset.reason, Some(expected_asset)),
    })
}

#[tokio::test]
async fn late_custom_response_commits_without_cached_base_and_serves_frozen_media() {
    let fixture = late_fixture("late-without-base").await;
    let (plan, asset) = prepare(&fixture).await;
    let fingerprint = crate::runtime_custom_tail::standalone_custom_asset_fingerprint(asset.media);
    assert!(
        fixture
            .ctx
            .hls_proxy
            .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, 0, fixture.now_ms)
            .await
            .is_err(),
        "preparation cannot authorize uncommitted custom media"
    );
    fixture.session.write().await.segments.clear();
    let now_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    assert_eq!(commit(&fixture, Arc::clone(&plan), asset, now_ms), HlsTerminalCommitOutcome::Committed);
    assert!(!fixture.session.read().await.has_terminal_tail_protections());
    assert!(plan.manifest_body().contains("#EXT-X-MEDIA-SEQUENCE:41\n"));
    assert!(!plan.manifest_body().contains("/hls/shared/live/"));
    let segment = fixture
        .ctx
        .hls_proxy
        .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, 11, now_ms)
        .await
        .expect("committed late media");
    assert!(!segment.bytes.is_empty());
    fixture.ctx.app_config.custom_stream_response.store(None);
    assert_eq!(commit(&fixture, Arc::clone(&plan), asset, now_ms + 1), HlsTerminalCommitOutcome::AlreadyCommitted);
    let replay = fixture
        .ctx
        .hls_proxy
        .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, 11, now_ms + 1)
        .await
        .expect("frozen media after config reload");
    assert_eq!(replay.bytes, segment.bytes);
    assert!(fixture
        .ctx
        .hls_proxy
        .resolve_standalone_custom_segment(&fixture.lease_id, "wrong", 11, now_ms)
        .await
        .is_err());
    assert!(fixture
        .ctx
        .hls_proxy
        .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, 12, now_ms)
        .await
        .is_err());
    assert!(fixture
        .ctx
        .hls_proxy
        .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, 0, fixture.now_ms + 60_000)
        .await
        .is_err());
}

#[tokio::test]
async fn late_custom_response_does_not_commit_over_recovered_origin_or_changed_asset() {
    for changed_asset in [false, true] {
        let fixture = late_fixture(if changed_asset { "late-asset-changed" } else { "late-origin-recovered" }).await;
        let (plan, asset) = prepare(&fixture).await;
        let now_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
        let expected = if changed_asset {
            fixture.ctx.app_config.custom_stream_response.store(None);
            HlsTerminalCommitOutcome::SupersededGeneration
        } else {
            fixture.session.write().await.origin_control.record_media_progress(now_ms, 12_000);
            HlsTerminalCommitOutcome::RecoveryCommitted
        };
        assert_eq!(commit(&fixture, plan, asset, now_ms), expected);
        let lease = fixture
            .ctx
            .hls_proxy
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, now_ms)
            .await
            .expect("lease");
        assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
    }
}

#[tokio::test]
async fn late_custom_response_preserves_the_prepared_asset_reason_guard() {
    let fixture = late_fixture("late-wrong-asset-reason").await;
    let asset = snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, HlsRuntimeCustomTailReason::LowPriorityPreempted)
        .expect("configured alternate custom asset");
    let now_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    assert_eq!(
        commit_prepared_runtime_custom_tail(
            &fixture.ctx,
            &fixture.session,
            &fixture.proxy_session_id,
            &fixture.lease_id,
            &fixture.preparation,
            now_ms,
            asset,
        )
        .await,
        HlsTerminalResolution::Reevaluate
    );
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, now_ms)
        .await
        .expect("live lease");
    assert_eq!(lease.playback_mode, HlsLeasePlaybackMode::Live);
}

#[tokio::test]
async fn late_custom_response_preserves_target_duration_exceeded_through_terminal_commit() {
    let mut fixture = late_fixture("late-target-duration-error").await;
    let mut manifest = fixture.preparation.manifest_snapshot.clone();
    manifest.target_duration_ms = 1_000;
    manifest.playlist_duration_ms = 1_000;
    manifest.last_visible_media_end_ms = 1_000;
    Arc::make_mut(&mut manifest.visible_segments)[0].duration_ms = 1_000;
    let guard = fixture
        .ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("publication guard");
    assert!(fixture
        .ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            guard,
            manifest,
            fixture.now_ms,
        )
        .await
        .is_committed());
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("lease with short target duration");
    fixture.preparation.manifest_snapshot = lease.last_manifest_snapshot.expect("published snapshot");
    fixture.preparation.manifest_snapshot_generation = fixture.preparation.manifest_snapshot.snapshot_generation;
    let expected = HlsTerminalTailCompatibility::TargetDurationExceeded {
        asset_ms: fixture.asset.duration_ms(),
        target_ms: 1_000,
    };
    assert!(matches!(prepare_late_custom_response(
        &fixture.ctx, &fixture.proxy_session_id, &fixture.lease_id, &fixture.preparation, fixture.now_ms,
    ).await, Err(crate::late_custom_response::HlsLateCustomResponseError::Unavailable(reason)) if reason == expected));
    let now_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    assert_eq!(
        super::super::terminal_pending_owner::commit_late_custom_response(HlsTerminalCommitContext {
            ctx: &fixture.ctx,
            session: &fixture.session,
            proxy_session_id: &fixture.proxy_session_id,
            lease_id: &fixture.lease_id,
            preparation: &fixture.preparation,
            now_ms,
        })
        .await,
        HlsTerminalResolution::Committed
    );
    let terminal = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, now_ms)
        .await
        .expect("terminal unavailable lease");
    assert_eq!(
        terminal.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable {
            decision_generation: fixture.preparation.decision_generation,
            reason: expected,
        }
    );
}

async fn committed_late_activity_fixture(name: &str) -> HlsTerminalPendingCommitFixture {
    let fixture = late_fixture(name).await;
    let mut config = fixture.ctx.app_config.config.load_full().as_ref().clone();
    config.reverse_proxy =
        Some(tuliprox_core::model::ReverseProxyConfig::from(&shared::model::ReverseProxyConfigDto {
            hls_cache: Some(shared::model::HlsCacheConfigDto {
                session_idle_timeout: shared::model::Secs::new(60),
                ..Default::default()
            }),
            ..Default::default()
        }));
    fixture.ctx.app_config.config.store(Arc::new(config));
    fixture.ctx.hls_proxy.update_config(&fixture.ctx.app_config).await;
    let now_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    assert!(fixture
        .ctx
        .hls_proxy
        .activate_access_lease(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    let (plan, asset) = prepare(&fixture).await;
    assert_eq!(commit(&fixture, plan, asset, now_ms), HlsTerminalCommitOutcome::Committed);
    fixture
}

#[tokio::test]
async fn late_custom_media_requests_keep_lease_and_session_alive_beyond_initial_sixty_seconds() {
    use crate::{
        lifecycle::{HlsLifecycleEvent, HlsLifecycleEventKey},
        HlsMediaActivityCommitOutcome,
    };
    let fixture = committed_late_activity_fixture("late-active-media").await;
    let manager = &fixture.ctx.hls_proxy;
    let started_at_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    let before = manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, started_at_ms)
        .await
        .expect("terminal lease");
    let fingerprint = crate::runtime_custom_tail::standalone_custom_asset_fingerprint(fixture.expected_asset.media);
    for index in 1_u16..=11 {
        let now_ms = started_at_ms + u64::from(index) * 10_000;
        let access = manager
            .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, index, now_ms)
            .await
            .expect("custom media remains available");
        assert_eq!(
            manager.record_standalone_custom_segment_activity(&fixture.lease_id, &access, now_ms).await,
            HlsMediaActivityCommitOutcome::Committed
        );
        if index == 6 {
            for key in [
                HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: fixture.lease_id.clone(),
                    proxy_session_id: fixture.proxy_session_id.clone(),
                },
                HlsLifecycleEventKey::SessionIdle { proxy_session_id: fixture.proxy_session_id.clone() },
            ] {
                manager
                    .handle_lifecycle_event(
                        &fixture.ctx.active_users,
                        &fixture.ctx.active_provider,
                        HlsLifecycleEvent { key, due_at_ms: started_at_ms + 60_000 },
                        now_ms,
                    )
                    .await;
            }
        }
        let lease = manager
            .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, now_ms)
            .await
            .expect("active terminal lease");
        assert_eq!(lease.state, HlsAccessLeaseState::Activated);
        assert_eq!(lease.last_seen_at_ms, now_ms);
        assert_eq!(lease.valid_until_ms, now_ms + 60_000);
        assert!(lease.active_until_ms.is_some_and(|until| until > now_ms));
        assert_eq!(lease.playback_mode, before.playback_mode);
        assert_eq!(lease.playback_cursor, before.playback_cursor);
        assert_eq!(lease.admission_generation, before.admission_generation);
        assert_eq!(fixture.session.read().await.last_client_access_at_ms, now_ms);
    }
    let expired_at_ms = started_at_ms + 170_000;
    assert!(manager
        .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, 0, expired_at_ms)
        .await
        .is_err());
    let expired = manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, expired_at_ms)
        .await
        .expect("expired lease");
    assert_eq!(expired.state, HlsAccessLeaseState::Expired);
    assert_eq!(expired.last_seen_at_ms, started_at_ms + 110_000);
}

#[tokio::test]
async fn late_custom_media_activity_reactivates_idle_lease_but_rejects_stale_or_expired_access() {
    use crate::{
        lifecycle::{HlsLifecycleEvent, HlsLifecycleEventKey},
        HlsMediaActivityCommitOutcome,
    };
    let fixture = committed_late_activity_fixture("late-idle-media").await;
    let manager = &fixture.ctx.hls_proxy;
    let started_at_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    manager
        .handle_lifecycle_event(
            &fixture.ctx.active_users,
            &fixture.ctx.active_provider,
            HlsLifecycleEvent {
                key: HlsLifecycleEventKey::AccessLeaseActive {
                    lease_id: fixture.lease_id.clone(),
                    proxy_session_id: fixture.proxy_session_id.clone(),
                },
                due_at_ms: started_at_ms + 30_000,
            },
            started_at_ms + 40_000,
        )
        .await;
    let idle = manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, started_at_ms + 40_000)
        .await
        .expect("idle lease");
    assert_eq!(idle.state, HlsAccessLeaseState::Idle);
    let fingerprint = crate::runtime_custom_tail::standalone_custom_asset_fingerprint(fixture.expected_asset.media);
    let mut access = manager
        .resolve_standalone_custom_segment(&fixture.lease_id, &fingerprint, 0, started_at_ms + 40_000)
        .await
        .expect("idle custom media");
    let original_generation = access.terminal_generation;
    access.terminal_generation = Some(crate::HlsTerminalTailGeneration(fixture.preparation.decision_generation + 1));
    assert_eq!(
        manager.record_standalone_custom_segment_activity(&fixture.lease_id, &access, started_at_ms + 40_000).await,
        HlsMediaActivityCommitOutcome::StaleLeaseIdentity
    );
    let unchanged = manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, started_at_ms + 40_000)
        .await
        .expect("unchanged idle lease");
    assert_eq!(unchanged.last_seen_at_ms, idle.last_seen_at_ms);
    assert_eq!(unchanged.valid_until_ms, idle.valid_until_ms);
    access.terminal_generation = original_generation;
    assert_eq!(
        manager.record_standalone_custom_segment_activity(&fixture.lease_id, &access, started_at_ms + 40_000).await,
        HlsMediaActivityCommitOutcome::Committed
    );
    let active = manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, started_at_ms + 40_000)
        .await
        .expect("reactivated lease");
    assert_eq!(active.state, HlsAccessLeaseState::Activated);
    assert_eq!(active.valid_until_ms, started_at_ms + 100_000);
    assert_eq!(
        manager.record_standalone_custom_segment_activity(&fixture.lease_id, &access, started_at_ms + 100_000).await,
        HlsMediaActivityCommitOutcome::StaleLeaseIdentity
    );
    let expired = manager
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, started_at_ms + 100_000)
        .await
        .expect("expired lease");
    assert_eq!(expired.state, HlsAccessLeaseState::Expired);
    assert_eq!(expired.last_seen_at_ms, active.last_seen_at_ms);
    assert_eq!(expired.valid_until_ms, active.valid_until_ms);
}

#[tokio::test]
async fn late_custom_response_retries_contended_commit_once_and_remains_immutable() {
    let mut fixture = late_fixture("late-contended-commit").await;
    let now_ms = fixture.ctx.hls_proxy.terminal_commit_now_ms();
    fixture.preparation.cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(now_ms, 0, fixture.preparation.reserve.transition_margin, None);
    let (plan, asset) = prepare(&fixture).await;
    let guard = fixture.session.read().await;
    assert!(matches!(commit(&fixture, Arc::clone(&plan), asset, now_ms), HlsTerminalCommitOutcome::LockBusy { .. }));
    assert!(matches!(commit(&fixture, Arc::clone(&plan), asset, now_ms), HlsTerminalCommitOutcome::LockBusy { .. }));
    drop(guard);
    let committed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let lease = fixture
                .ctx
                .hls_proxy
                .access_lease_response_snapshot(
                    &fixture.lease_id,
                    &fixture.proxy_session_id,
                    fixture.ctx.hls_proxy.terminal_commit_now_ms(),
                )
                .await
                .expect("lease");
            if let HlsLeasePlaybackMode::TerminalCustomResponse(committed) = lease.playback_mode {
                break committed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("autonomous late custom commit");
    assert_eq!(committed, plan);
    assert_eq!(
        commit(&fixture, plan, asset, fixture.ctx.hls_proxy.terminal_commit_now_ms()),
        HlsTerminalCommitOutcome::AlreadyCommitted
    );
}

#[tokio::test]
async fn late_custom_response_uses_highwater_even_when_latest_manifest_is_shorter() {
    let mut fixture = late_fixture("late-highwater").await;
    for (last_seq, discontinuity_sequence, discontinuity_before) in [(100, 7, true), (50, 3, false)] {
        let guard = fixture
            .ctx
            .hls_proxy
            .prepare_access_lease_manifest_publication(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
            .await
            .expect("publication guard");
        let mut manifest = fixture.preparation.manifest_snapshot.clone();
        manifest.first_proxy_seq = last_seq;
        manifest.last_proxy_seq = last_seq;
        manifest.discontinuity_sequence = discontinuity_sequence;
        let segment = Arc::make_mut(&mut manifest.visible_segments).first_mut().expect("segment");
        segment.proxy_seq = last_seq;
        segment.discontinuity_before = discontinuity_before;
        assert!(fixture
            .ctx
            .hls_proxy
            .commit_access_lease_manifest_publication(
                &fixture.lease_id,
                &fixture.proxy_session_id,
                guard,
                manifest,
                fixture.now_ms
            )
            .await
            .is_committed());
    }
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("lease");
    fixture.preparation.manifest_snapshot = lease.last_manifest_snapshot.expect("latest snapshot");
    fixture.preparation.manifest_snapshot_generation = fixture.preparation.manifest_snapshot.snapshot_generation;
    let (plan, _) = prepare(&fixture).await;
    assert!(plan.manifest_body().contains("#EXT-X-MEDIA-SEQUENCE:101\n"));
    assert!(plan.manifest_body().contains("#EXT-X-DISCONTINUITY-SEQUENCE:8\n"));
}

#[tokio::test]
async fn late_custom_response_cas_budget_cannot_be_renewed_or_poisoned_by_polls() {
    use crate::terminal_commit::{
        terminal_commit_retry_schedule_budget_ms, HlsTerminalCommitCommand, HlsTerminalCommitOwnerKey,
        HlsTerminalCommitRetryCoordinator, HlsTerminalCommitSubmissionDecision, HlsTerminalLeaseDecision,
    };
    let fixture = late_fixture("late-bounded-cas").await;
    let (plan, asset) = prepare(&fixture).await;
    let coordinator = HlsTerminalCommitRetryCoordinator::default();
    let now_ms = fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    let command = |decision| {
        let (cancellation_epoch, submission_token) = coordinator.reserve_submission().expect("submission binding");
        HlsTerminalCommitCommand {
            key: HlsTerminalCommitOwnerKey {
                proxy_session_id: fixture.proxy_session_id.clone(),
                lease_id: fixture.lease_id.clone(),
                lease_issued_at_ms: fixture.preparation.lease_issued_at_ms,
                decision_generation: fixture.preparation.decision_generation,
            },
            session: Arc::clone(&fixture.session),
            session_incarnation: fixture
                .ctx
                .hls_proxy
                .sessions()
                .session_incarnation(&fixture.session)
                .expect("session incarnation"),
            preparation: fixture.preparation.clone(),
            decision,
            media_guard: None,
            asset_revision_guard: terminal_asset_revision_guard(&fixture.ctx, asset.reason, Some(asset)),
            cancellation_epoch,
            submission_token,
        }
    };
    assert!(matches!(
        coordinator.submit(
            command(HlsTerminalLeaseDecision::Unavailable(HlsTerminalTailCompatibility::TerminalMediaNotReady)),
            now_ms
        ),
        HlsTerminalCommitSubmissionDecision::Failed(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed)
    ));
    assert!(matches!(
        coordinator.submit(command(HlsTerminalLeaseDecision::CustomResponse(Arc::clone(&plan))), now_ms),
        HlsTerminalCommitSubmissionDecision::Attempt { .. }
    ));
    assert!(matches!(
        coordinator.submit(
            command(HlsTerminalLeaseDecision::Unavailable(HlsTerminalTailCompatibility::TerminalMediaNotReady)),
            now_ms + 1
        ),
        HlsTerminalCommitSubmissionDecision::PendingExisting { .. }
    ));
    assert!(matches!(
        coordinator.submit(command(HlsTerminalLeaseDecision::CustomResponse(Arc::clone(&plan))), now_ms + 100),
        HlsTerminalCommitSubmissionDecision::PendingExisting { .. }
    ));
    assert!(matches!(
        coordinator.submit(
            command(HlsTerminalLeaseDecision::CustomResponse(plan)),
            now_ms + terminal_commit_retry_schedule_budget_ms()
        ),
        HlsTerminalCommitSubmissionDecision::Failed(HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed)
    ));
}
