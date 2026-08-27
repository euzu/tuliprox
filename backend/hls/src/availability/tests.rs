//! Tests for availability evaluation.
//!
//! These drive the reevaluation worker and the terminal decision paths end to
//! end, so they stay with the orchestration in `super` rather than with the
//! individual steps in `recovery_pressure`, `reevaluation`, `terminal_cutover`
//! and `terminal_pending_owner`.

use super::{
    super::{
        lease::HlsAccessLeaseTiming,
        media_reserve::{
            HlsLeaseManifestSegment, HlsLeasePlaybackCursor, HlsLeaseReserveAvailabilityBasis,
            HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsReadyMediaState, HlsReadyTimelineUnit,
        },
        post_refresh_availability::{commit_post_refresh_terminal_fallback, HlsPostRefreshFallbackOutcome},
        prepared_terminal_bundle::{
            prepared_terminal_bundle_completion_channel_for_test, HlsPreparedTerminalBundleCompletionTicket,
            HlsPreparedTerminalSegment,
        },
        recovery_timing::{HlsRecoveryBurstWorkload, HlsRecoveryMapWorkload, HlsRecoverySegmentWorkload},
        runtime_custom_tail::{
            commit_hls_runtime_custom_tail, snapshot_hls_runtime_custom_tail_asset, HlsRuntimeCustomTailOutcome,
            HlsRuntimeCustomTailRequest,
        },
        session_store::HlsSessionIncarnation,
        terminal_pending::{HlsTerminalPendingCoordinator, HlsTerminalPendingOwnerKey, HlsTerminalPendingRegistration},
        terminal_tail::{
            terminal_tail_manifest_body, HlsLeasePlaybackMode, HlsMapSignature, HlsMediaContainer,
            HlsTerminalAssetIdentity, HlsTerminalSegmentPath, HlsTerminalTailPlan,
        },
        CacheAccessState, HlsAccessLease, HlsAccessLeaseState, HlsPlaybackFamilyKey, HlsSession, HlsSessionKey,
        OriginSegmentKey, SegmentCacheKey, SegmentCacheStatus, SegmentEntry, SegmentFetchPriority,
    },
    recovery_pressure::{
        acceptance_timing_seed_for_pressure, aggregate_session_recovery_pressure,
        evaluate_and_commit_session_recovery_pressure, recovery_trigger_source, HlsLeaseRecoveryEvidence,
        HlsRecoveryBoundarySlackMs, HlsRecoveryPressurePolicy,
    },
    reevaluation::{
        availability_refresh_trigger_decision, register_hls_availability_reevaluation_with_mode,
        HlsAvailabilityAttemptSchedule, HlsAvailabilityRefreshTriggerDecision,
    },
    terminal_cutover::HlsTerminalCommitContext,
    terminal_pending_owner::{
        await_terminal_pending_decision, classify_autonomous_terminal_resolution, run_terminal_pending_owner,
        terminal_asset_revision_guard, terminal_pending_fallback_commit_at_ms, terminal_resolution_for_commit_outcome,
        terminal_resolution_for_pending_registration, HlsAutonomousTerminalObservation, HlsTerminalPendingDecision,
    },
    *,
};
use crate::{
    api::{HlsRuntimeCustomTailReason, OriginRefreshRequest},
    evaluate_lease_reserve,
    lease::HlsTerminalTailPreparation,
    manager::HlsTerminalTailPreparationRequest,
    media_reserve::{HlsLeaseReserveSnapshot, HlsReadyTimelineSnapshot},
    origin_progress::{
        evaluate_origin_progress, HlsOriginPathCondition, HlsOriginProgressPhase, HlsOriginProgressSnapshot,
    },
    post_refresh_availability::{
        evaluate_active_terminal_leases_for_reevaluation, evaluate_owner_failure_fallback, live_reserve_deadline,
        HlsPostRefreshTerminalEvaluation,
    },
    prepared_terminal_bundle::{
        build_prepared_terminal_bundle, HlsPreparedTerminalBundle, HlsPreparedTerminalBundleCompletion,
        HlsPreparedTerminalBundleKey,
    },
    prepared_terminal_bundle_key,
    recovery_timing::{
        HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsLeaseCutoverTiming, HlsRecoveryTriggerBudgetMs,
        HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitWindow, HlsTerminalMediaPreparationState,
        HlsTransitionMarginMs,
    },
    refresh::{HlsOriginRefreshTriggerOutcome, HlsPostRefreshAvailabilityAction},
    snapshot_terminal_media_asset,
    terminal_commit::{HlsTerminalAssetRevisionGuard, HlsTerminalCommitOutcome},
    HlsAccessLeaseId, HlsAccessLeaseStore, HlsAvailabilityReevaluationMode, HlsAvailabilityReevaluationRegistration,
    HlsLeaseReserveInput, HlsObservedRecoveryLatency, HlsPreparedTerminalBundleState, HlsRecoveryTriggerSource,
    HlsRuntimeCustomTailAssetIdentity, HlsTerminalTailCompatibility, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
};
use bytes::Bytes;
use std::{sync::Arc, time::Duration};
use tokio::sync::oneshot;
use tuliprox_core::model::{Config, CustomStreamResponse};
use tuliprox_mpegts::transport_stream_buffer::{HlsTsSpliceAnchor, TransportStreamBuffer};
use tuliprox_parser::hls::origin_manifest::{
    parse_origin_media_manifest, OriginManifestParseOutcome, ParsedOriginManifest,
};

const TERMINAL_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"));
const LOW_PRIORITY_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/low_priority_preempted.ts"));
const PROVIDER_EXHAUSTED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/provider_connections_exhausted.ts"));
const USER_EXHAUSTED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/user_connections_exhausted.ts"));
const ACCOUNT_EXPIRED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/user_account_expired.ts"));
const SESSION_EXPIRED_ASSET_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/hls_session_or_lease_expired.ts"));

#[test]
fn availability_refresh_waits_for_completion_instead_of_polling_in_flight() {
    for outcome in [HlsOriginRefreshTriggerOutcome::Started, HlsOriginRefreshTriggerOutcome::InFlight] {
        assert_eq!(
            availability_refresh_trigger_decision(outcome, true),
            HlsAvailabilityRefreshTriggerDecision::Wait(HlsAvailabilityAttemptSchedule::RefreshCompletion)
        );
    }
    assert_eq!(HlsAvailabilityAttemptSchedule::RefreshCompletion.wake_at_ms(100, 1, 2_100), Some(2_100));
}

#[test]
fn availability_refresh_waits_until_concrete_debounce_boundary() {
    let schedule = HlsAvailabilityAttemptSchedule::DebouncedUntil { retry_at_ms: 1_100 };
    assert_eq!(
        availability_refresh_trigger_decision(
            HlsOriginRefreshTriggerOutcome::DebouncedUntil { retry_at_ms: 1_100 },
            true,
        ),
        HlsAvailabilityRefreshTriggerDecision::Wait(schedule)
    );
    assert_eq!(schedule.wake_at_ms(100, 1, 2_100), Some(1_100));
    assert_eq!(
        HlsAvailabilityAttemptSchedule::DebouncedUntil { retry_at_ms: 3_000 }.wake_at_ms(100, 1, 2_100),
        Some(2_100)
    );
}

fn runtime_custom_responses() -> Arc<CustomStreamResponse> {
    runtime_custom_responses_with_low_priority(LOW_PRIORITY_ASSET_BYTES)
}

fn runtime_custom_responses_with_low_priority(low_priority_bytes: &[u8]) -> Arc<CustomStreamResponse> {
    Arc::new(CustomStreamResponse {
        channel_unavailable: Some(TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec())),
        user_connections_exhausted: Some(TransportStreamBuffer::new(USER_EXHAUSTED_ASSET_BYTES.to_vec())),
        provider_connections_exhausted: Some(TransportStreamBuffer::new(PROVIDER_EXHAUSTED_ASSET_BYTES.to_vec())),
        low_priority_preempted: Some(TransportStreamBuffer::new(low_priority_bytes.to_vec())),
        user_account_expired: Some(TransportStreamBuffer::new(ACCOUNT_EXPIRED_ASSET_BYTES.to_vec())),
        panel_api_provisioning: None,
        hls_session_or_lease_expired: Some(TransportStreamBuffer::new(SESSION_EXPIRED_ASSET_BYTES.to_vec())),
        panel_api_provisioning_hls_segments: Vec::new(),
    })
}

#[tokio::test]
async fn disabled_custom_responses_do_not_seed_terminal_media_preparation() {
    let hls_ctx = crate::HlsCtx::for_test(Config { custom_stream_response_enabled: false, ..Config::default() });
    let ctx = &hls_ctx;
    ctx.app_config.custom_stream_response.store(Some(runtime_custom_responses()));

    assert_eq!(terminal_media_timing_seed(ctx, 10_000), (None, HlsTerminalMediaPreparationState::Failed { key: None }));
}

fn publication_late_decision(reserve_ms: u64) -> HlsOriginProgressDecision {
    evaluate_origin_progress(HlsOriginProgressSnapshot {
        phase: HlsOriginProgressPhase::Fresh,
        condition: HlsOriginPathCondition::ProgressExpected,
        target_duration_ms: 10_000,
        last_media_progress_at_ms: Some(0),
        session_recovery_required: reserve_ms <= 14_000,
        session_cutover_evaluation_required: reserve_ms <= 10_000,
        recovery_committed: false,
        now_ms: 15_000,
    })
}

fn lease_timing_seed() -> HlsAcceptanceEpisodeTimingSeed {
    HlsAcceptanceEpisodeTimingSeed {
        target_duration_ms: 10_000,
        transition_margin: HlsTransitionMarginMs::from_millis(10_000),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
    }
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

fn pending_decision_bundle_key() -> HlsPreparedTerminalBundleKey {
    HlsPreparedTerminalBundleKey {
        asset: HlsTerminalAssetIdentity { revision: 7, fingerprint: [7; 32] },
        target_duration_ms: 1_000,
        segment_count: 2,
    }
}

fn pending_decision_owner_key(bundle_key: HlsPreparedTerminalBundleKey) -> HlsTerminalPendingOwnerKey {
    HlsTerminalPendingOwnerKey {
        session_incarnation: HlsSessionIncarnation::for_test(1),
        proxy_session_id: ProxySessionId("pending-session".to_string()),
        lease_id: HlsAccessLeaseId("pending-lease".to_string()),
        lease_issued_at_ms: 10,
        expected_admission_generation: 20,
        manifest_snapshot_generation: 30,
        cursor_generation: 40,
        decision_generation: 50,
        reason: HlsRuntimeCustomTailReason::ChannelUnavailable,
        bundle_key,
        latest_safe_commit_at_ms: 10_000,
    }
}

fn pending_decision_ready_bundle(bundle_key: HlsPreparedTerminalBundleKey) -> Arc<HlsPreparedTerminalBundle> {
    let segments = (0..bundle_key.segment_count)
        .map(|index| HlsPreparedTerminalSegment {
            index,
            timestamp_offset_ticks_90khz: u64::from(index).saturating_mul(45_000),
            bytes: Bytes::from_static(b"terminal"),
        })
        .collect::<Vec<_>>();
    Arc::new(HlsPreparedTerminalBundle {
        key: bundle_key,
        source_asset_duration_ms: 500,
        source_asset_duration_ticks_90khz: 45_000,
        segments: segments.into(),
    })
}

struct HlsTerminalPendingCommitFixture {
    ctx: HlsCtx,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    preparation: HlsTerminalTailPreparation,
    asset: Arc<super::super::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    now_ms: u64,
}

impl HlsTerminalPendingCommitFixture {
    fn owner_key(&self) -> HlsTerminalPendingOwnerKey {
        HlsTerminalPendingOwnerKey {
            session_incarnation: self
                .ctx
                .hls_proxy
                .sessions()
                .session_incarnation(&self.session)
                .expect("fixture session has a current incarnation"),
            proxy_session_id: self.proxy_session_id.clone(),
            lease_id: self.lease_id.clone(),
            lease_issued_at_ms: self.preparation.lease_issued_at_ms,
            expected_admission_generation: self.preparation.expected_admission_generation,
            manifest_snapshot_generation: self.preparation.manifest_snapshot_generation,
            cursor_generation: self.preparation.cursor_generation,
            decision_generation: self.preparation.decision_generation,
            reason: self.expected_asset.reason,
            bundle_key: self.bundle_key,
            latest_safe_commit_at_ms: self
                .preparation
                .cutover_timing
                .latest_safe_terminal_commit_at
                .as_millis_since_epoch(),
        }
    }

    fn ready_bundle(&self) -> Arc<HlsPreparedTerminalBundle> {
        build_prepared_terminal_bundle(&self.asset, self.bundle_key).expect("fixture relative terminal bundle")
    }

    fn register_owner(&self, ticket: HlsPreparedTerminalBundleCompletionTicket) -> oneshot::Receiver<()> {
        let coordinator = self.ctx.hls_proxy.terminal_pending();
        let owner_key = self.owner_key();
        let ctx = self.ctx.clone();
        let session = Arc::clone(&self.session);
        let proxy_session_id = self.proxy_session_id.clone();
        let lease_id = self.lease_id.clone();
        let preparation = self.preparation.clone();
        let asset = Arc::clone(&self.asset);
        let expected_asset = self.expected_asset;
        let bundle_key = self.bundle_key;
        let (completed_tx, completed_rx) = oneshot::channel();
        let asset_guard = terminal_asset_revision_guard(&self.ctx, expected_asset.reason, Some(expected_asset));

        assert_eq!(
            coordinator.register(owner_key, &asset_guard, move |ownership| async move {
                run_terminal_pending_owner(
                    ctx,
                    session,
                    proxy_session_id,
                    lease_id,
                    preparation,
                    asset,
                    expected_asset,
                    bundle_key,
                    ticket,
                    ownership,
                )
                .await;
                assert!(completed_tx.send(()).is_ok());
            }),
            HlsTerminalPendingRegistration::Scheduled
        );
        completed_rx
    }
}

fn terminal_pending_commit_reserve() -> HlsLeaseReserveSnapshot {
    let transition_margin = HlsTransitionMarginMs::from_millis(12_000);
    let guaranteed_reserve_ms = transition_margin
        .as_millis()
        .saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis());
    HlsLeaseReserveSnapshot {
        availability_basis: HlsLeaseReserveAvailabilityBasis::ReadyCacheTimeline,
        guaranteed_media_horizon_ms: guaranteed_reserve_ms,
        conservative_playback_position_ms: 0,
        guaranteed_reserve_ms,
        initial_hidden_ready_duration_ms: 0,
        transition_margin,
        key_readiness_valid_until_ms: None,
        recovery_required: true,
        cutover_required: false,
    }
}

fn terminal_pending_commit_manifest(
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    duration_ms: u64,
) -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: 0,
        first_proxy_seq: 40,
        last_proxy_seq: 40,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: 40,
            duration_ms,
            uri: format!("/hls/shared/live/{}/{}/40.ts", proxy_session_id.0, lease_id.0).into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms: 12_000,
        playlist_duration_ms: duration_ms,
        last_visible_media_end_ms: duration_ms,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

async fn terminal_pending_commit_fixture(name: &str) -> HlsTerminalPendingCommitFixture {
    terminal_pending_commit_fixture_with_base(name, TERMINAL_ASSET_BYTES).await
}

async fn install_terminal_pending_ready_base(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    asset: &super::super::terminal_tail::HlsTerminalMediaAsset,
    base_segment_bytes: &[u8],
    now_ms: u64,
) {
    let cache_key = SegmentCacheKey::new(proxy_session_id.clone(), 40, "ts");
    ctx.hls_proxy
        .segment_cache()
        .write_bytes_and_commit(&cache_key, base_segment_bytes)
        .await
        .expect("READY terminal-base bytes commit");
    let mut session = session.write().await;
    let origin_epoch = session.origin_control.origin_epoch;
    session.segments.insert(
        40,
        SegmentEntry {
            origin_key: OriginSegmentKey {
                origin_epoch,
                effective_host_id: 1,
                host_local_sequence: 40,
                host_local_index: 0,
            },
            proxy_seq: 40,
            duration_ms: asset.duration_ms(),
            proxy_file_ext: "ts".to_string(),
            content_type: "video/mp2t".to_string(),
            cache_key,
            discontinuity_before: false,
            program_date_time: None,
            daterange_tags_before: Vec::new(),
            origin_byte_range: None,
            map_ref: None,
            encryption: None,
            origin_fetch_ref: None,
            status: SegmentCacheStatus::Ready {
                content_length: u64::try_from(base_segment_bytes.len()).unwrap_or(u64::MAX),
                ready_at_ms: now_ms,
            },
            last_rendered_at_ms: Some(now_ms),
            access: Arc::new(CacheAccessState::new()),
        },
    );
}

async fn publish_terminal_pending_lease(
    ctx: &HlsCtx,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    name: &str,
    duration_ms: u64,
    now_ms: u64,
) {
    ctx.hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new("pending-owner", name),
            proxy_session_id.clone(),
            "pending-owner".to_string(),
            name.to_string(),
            1,
            name.to_string(),
            1,
            now_ms,
            60_000,
        ))
        .await;
    let publication = ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication(lease_id, proxy_session_id, now_ms)
        .await
        .expect("manifest publication guard");
    assert!(ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(
            lease_id,
            proxy_session_id,
            publication,
            terminal_pending_commit_manifest(proxy_session_id, lease_id, duration_ms),
            now_ms,
        )
        .await
        .is_committed());
}

async fn terminal_pending_commit_fixture_with_base(
    name: &str,
    base_segment_bytes: &[u8],
) -> HlsTerminalPendingCommitFixture {
    let hls_ctx = crate::HlsCtx::for_test(Config { custom_stream_response_enabled: true, ..Config::default() });
    let ctx = &hls_ctx;
    let terminal_buffer = TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec());
    let asset = snapshot_terminal_media_asset(&terminal_buffer).expect("valid terminal asset fixture");
    ctx.app_config.custom_stream_response.store(Some(runtime_custom_responses()));
    let now_ms = ctx.hls_proxy.terminal_commit_now_ms();
    let (session, _) =
        ctx.hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, name), b"secret", now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let lease_id = HlsAccessLeaseId(name.to_string());
    install_terminal_pending_ready_base(ctx, &session, &proxy_session_id, &asset, base_segment_bytes, now_ms).await;
    publish_terminal_pending_lease(ctx, &proxy_session_id, &lease_id, name, asset.duration_ms(), now_ms).await;
    let (origin_progress_generation, media_readiness_generation, last_media_progress_at_ms) = {
        let session = session.read().await;
        (
            session.origin_control.progress_generation,
            session.activity.media_readiness_generation,
            session.origin_control.last_media_progress_at_ms,
        )
    };
    let reserve = terminal_pending_commit_reserve();
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(now_ms, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
    let preparation = ctx
        .hls_proxy
        .prepare_access_lease_terminal_tail(HlsTerminalTailPreparationRequest {
            lease_id: &lease_id,
            proxy_session_id: &proxy_session_id,
            manifest_snapshot_generation: 1,
            cursor_generation: 0,
            reserve,
            cutover_timing,
            commit_window: HlsTerminalCommitWindow::AcquisitionOpen,
            now_ms,
            origin_progress_generation,
            media_readiness_generation,
            last_media_progress_at_ms,
        })
        .await
        .expect("cutover-local terminal preparation");
    let expected_asset =
        HlsRuntimeCustomTailAssetIdentity::channel_unavailable(HlsTerminalAssetIdentity::from_asset(&asset));
    let bundle_key = prepared_terminal_bundle_key(
        &asset,
        preparation.manifest_snapshot.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );

    HlsTerminalPendingCommitFixture {
        ctx: ctx.clone(),
        session,
        proxy_session_id,
        lease_id,
        preparation,
        asset,
        expected_asset,
        bundle_key,
        now_ms,
    }
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

fn terminal_base_without_timestamps() -> Vec<u8> {
    let mut bytes = TERMINAL_ASSET_BYTES.to_vec();
    for packet in bytes.as_chunks_mut::<188>().0 {
        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        if matches!(adaptation_field_control, 0b10 | 0b11) && packet[4] > 0 {
            packet[5] &= !0x10;
        }
        if packet[1] & 0x40 == 0 {
            continue;
        }
        let payload_offset = match adaptation_field_control {
            0b01 => 4,
            0b11 => 5usize.saturating_add(usize::from(packet[4])),
            _ => continue,
        };
        let Some(payload) = packet.get_mut(payload_offset..) else {
            continue;
        };
        if payload.len() >= 9 && payload.starts_with(&[0x00, 0x00, 0x01]) {
            payload[7] &= 0x3F;
        }
    }
    bytes
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

async fn assert_terminal_pending_registration_failure_commits_unavailable(
    failure: HlsTerminalPendingRegistration,
    name: &str,
) {
    let fixture = terminal_pending_commit_fixture(name).await;
    let resolution = terminal_resolution_for_pending_registration(
        HlsTerminalCommitContext {
            ctx: &fixture.ctx,
            session: &fixture.session,
            proxy_session_id: &fixture.proxy_session_id,
            lease_id: &fixture.lease_id,
            preparation: &fixture.preparation,
            now_ms: fixture.now_ms,
        },
        fixture.expected_asset,
        fixture.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch(),
        failure,
    );

    assert_eq!(resolution, HlsTerminalResolution::Committed);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal unavailable lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::TerminalMediaNotReady, .. }
    ));
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
async fn terminal_pending_runtime_failure_commits_terminal_unavailable() {
    assert_terminal_pending_registration_failure_commits_unavailable(
        HlsTerminalPendingRegistration::RuntimeUnavailable,
        "pending-runtime-failure",
    )
    .await;
}

async fn assert_post_refresh_registration_failure_leaves_no_unowned_live_lease(
    failure: HlsAvailabilityReevaluationRegistration,
    name: &str,
) {
    let fixture = terminal_pending_commit_fixture(name).await;
    let (origin_progress_generation, media_readiness_generation) = {
        let mut session = fixture.session.write().await;
        session.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
        let origin_epoch = session.origin_control.origin_epoch;
        for proxy_seq in 41_u64..=44 {
            session.segments.insert(
                proxy_seq,
                SegmentEntry {
                    origin_key: OriginSegmentKey {
                        origin_epoch,
                        effective_host_id: 1,
                        host_local_sequence: proxy_seq,
                        host_local_index: u32::try_from(proxy_seq.saturating_sub(40)).unwrap_or(u32::MAX),
                    },
                    proxy_seq,
                    duration_ms: 20_000,
                    proxy_file_ext: "ts".to_string(),
                    content_type: "video/mp2t".to_string(),
                    cache_key: SegmentCacheKey::new(fixture.proxy_session_id.clone(), proxy_seq, "ts"),
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    encryption: None,
                    origin_fetch_ref: None,
                    status: SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: fixture.now_ms },
                    last_rendered_at_ms: None,
                    access: Arc::new(CacheAccessState::new()),
                },
            );
        }
        (session.origin_control.progress_generation, session.activity.media_readiness_generation)
    };
    assert!(fixture
        .ctx
        .hls_proxy
        .activate_access_lease(
            &fixture.lease_id,
            &fixture.proxy_session_id,
            fixture.now_ms,
            HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    let outcome = commit_post_refresh_terminal_fallback(
        fixture.ctx.clone(),
        Arc::clone(&fixture.session),
        HlsPostRefreshAvailabilityAction::Reevaluate {
            reason: super::super::refresh::HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
            origin_progress_generation,
            media_readiness_generation,
        },
        failure,
    )
    .await;

    assert_eq!(outcome, HlsPostRefreshFallbackOutcome::TerminalCommitted);
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("fallback lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::TerminalMediaNotReady, .. }
    ));
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
async fn post_refresh_runtime_failure_leaves_no_unowned_live_lease() {
    assert_post_refresh_registration_failure_leaves_no_unowned_live_lease(
        HlsAvailabilityReevaluationRegistration::RuntimeUnavailable,
        "post-refresh-runtime-failure",
    )
    .await;
}

fn pressure_manifest(target_duration_ms: u64) -> HlsLeaseManifestSnapshot {
    HlsLeaseManifestSnapshot {
        delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
        source_commit_identity: HlsManifestCommitIdentity::new(1),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 1,
        delivered_at_ms: 1,
        first_proxy_seq: 0,
        last_proxy_seq: 0,
        visible_segments: Arc::from([HlsLeaseManifestSegment {
            proxy_seq: 0,
            duration_ms: 4_000,
            uri: "0.ts".into(),
            discontinuity_before: false,
            map_ref_ready: true,
            encryption: None,
        }]),
        discontinuity_sequence: 0,
        target_duration_ms,
        playlist_duration_ms: 4_000,
        last_visible_media_end_ms: 4_000,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    }
}

fn pressure_manifest_at(proxy_seq: u64, target_duration_ms: u64) -> HlsLeaseManifestSnapshot {
    let mut manifest = pressure_manifest(target_duration_ms);
    manifest.first_proxy_seq = proxy_seq;
    manifest.last_proxy_seq = proxy_seq;
    Arc::make_mut(&mut manifest.visible_segments)[0].proxy_seq = proxy_seq;
    manifest
}

fn pressure_timeline(hidden_durations_ms: &[u64]) -> HlsReadyTimelineSnapshot {
    let mut start_ms = 0_u64;
    let mut units = Vec::with_capacity(hidden_durations_ms.len().saturating_add(1));
    for (index, duration_ms) in std::iter::once(4_000_u64).chain(hidden_durations_ms.iter().copied()).enumerate() {
        units.push(HlsReadyTimelineUnit {
            proxy_seq: u64::try_from(index).unwrap_or(u64::MAX),
            start_ms,
            duration_ms,
            state: HlsReadyMediaState::Ready,
            required_map_ready: true,
            required_key_ready: true,
            key_ready_valid_until_ms: None,
        });
        start_ms = start_ms.saturating_add(duration_ms);
    }
    HlsReadyTimelineSnapshot { units: units.into() }
}

fn evaluated_pressure(
    lease_id: &str,
    target_duration_ms: u64,
    hidden_durations_ms: &[u64],
    recovery_budget_ms: u64,
) -> HlsLeaseRecoveryEvidence {
    let manifest = pressure_manifest(target_duration_ms);
    let ready_timeline = pressure_timeline(hidden_durations_ms);
    let recovery_trigger_budget = HlsRecoveryTriggerBudgetMs::from_millis(recovery_budget_ms);
    let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
        manifest: &manifest,
        cursor: &HlsLeasePlaybackCursor::default(),
        ready_timeline: &ready_timeline,
        now_ms: 100,
        playback_rate_guard_milli: HLS_PLAYBACK_RATE_GUARD_MILLI,
        recovery_trigger_budget,
        origin_path_degraded: true,
        recovery_committed: false,
    });
    let boundary_ms = recovery_trigger_budget.as_millis().saturating_add(reserve.transition_margin.as_millis());
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(100, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
    HlsLeaseRecoveryEvidence {
        lease_id: HlsAccessLeaseId(lease_id.to_string()),
        reserve,
        cursor: HlsLeasePlaybackCursor::default(),
        workload: HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling(),
        target_duration_ms,
        latest_safe_terminal_commit_at: cutover_timing.latest_safe_terminal_commit_at,
        recovery_boundary_slack_ms: HlsRecoveryBoundarySlackMs::from_reserve_and_boundary(
            reserve.guaranteed_reserve_ms,
            boundary_ms,
        ),
    }
}

fn atomic_pressure_policy() -> HlsRecoveryPressurePolicy {
    HlsRecoveryPressurePolicy {
        burst_plan: shared::model::HlsManifestRecoveryBurstPlan { slots: 1, lanes_per_slot: 1 },
        timing: HlsRecoveryTimingPolicy::new(
            HlsOperationTimeoutMs::from_millis(1_000),
            HlsOperationTimeoutMs::from_millis(1_000),
            HlsRecoveryEtaMs::from_millis(0),
            HlsRecoveryEtaMs::from_millis(0),
        ),
    }
}

fn atomic_pressure_session() -> HlsSession {
    let mut session = HlsSession::new(HlsSessionKey::new(1, "pressure"), b"secret", 0);
    let body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:8\n\
        #EXTINF:4.0,\n0.ts\n#EXTINF:8.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n";
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(body, "http://origin.example/live/index.m3u8")
    else {
        panic!("pressure manifest parses");
    };
    session.apply_origin_manifest(&manifest).expect("pressure timeline applies");
    for segment in session.segments.values_mut() {
        segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
    }
    session.origin_control.path_condition = HlsOriginPathCondition::RetryableFetchFailure;
    session.origin_control.last_media_progress_at_ms = Some(90);
    session.origin_control.target_duration_snapshot_ms = Some(8_000);
    session
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

struct PostRefreshTerminalFixture {
    ctx: HlsCtx,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    now_ms: u64,
}

fn post_refresh_owner_request(fixture: &PostRefreshTerminalFixture) -> OriginRefreshRequest {
    let origin_entry =
        super::super::manifest_fetch::LiveHlsOriginEntry::parse("http://127.0.0.1:9/live/user/pass/12345.m3u8")
            .expect("test origin entry parses");
    OriginRefreshRequest {
        app_config: Arc::clone(&fixture.ctx.app_config),
        session: Arc::clone(&fixture.session),
        origin_entry,
        headers: axum::http::HeaderMap::new(),
        origin_provider_session_headers: axum::http::HeaderMap::new(),
        disabled_headers: None,
        client: reqwest::Client::new(),
        no_redirect_client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("test client builds"),
        use_manual_redirects: false,
        segment_cache: Arc::clone(fixture.ctx.hls_proxy.segment_cache()),
        hls_proxy: Arc::clone(&fixture.ctx.hls_proxy),
        segment_repair: Arc::clone(fixture.ctx.hls_proxy.segment_repair()),
        segment_worker_pool: Arc::clone(fixture.ctx.hls_proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(fixture.ctx.hls_proxy.map_worker_pool()),
        origin_manifest_timeout_ms: fixture.ctx.hls_proxy.origin_manifest_timeout_ms(),
        manifest_recovery_burst: fixture.ctx.hls_proxy.manifest_recovery_burst(),
        strip: fixture.ctx.hls_proxy.strip(),
        retry_policy: super::super::manifest_fetch::RetryPolicy { delays_ms: [0; 5], jitter_max_ms: 0 },
        reverse_proxy_rewrite_secret: b"secret".to_vec(),
        transient_resource_ttl_ms: 300_000,
        manifest_commit_requirement: super::super::refresh::HlsManifestCommitRequirement::CommittedManifestAllowed,
        fresh_manifest_requirement_generation: None,
        acceptance_directive: HlsManifestAcceptanceDirective::none(),
        access_lease_id: None,
        now_ms: fixture.now_ms,
        origin_io: None,
        post_refresh_runtime: Some(super::super::refresh::HlsPostRefreshRuntime { ctx: fixture.ctx.downgrade() }),
    }
}

async fn register_real_post_refresh_owner(
    fixture: &PostRefreshTerminalFixture,
    reason: super::super::refresh::HlsPostRefreshAvailabilityReason,
) {
    let (origin_progress_generation, media_readiness_generation) = {
        let session = fixture.session.read().await;
        (session.origin_control.progress_generation, session.activity.media_readiness_generation)
    };
    assert_eq!(
        register_post_refresh_availability_reevaluation(
            fixture.ctx.clone(),
            Arc::clone(&fixture.session),
            post_refresh_owner_request(fixture),
            HlsPostRefreshAvailabilityAction::Reevaluate {
                reason,
                origin_progress_generation,
                media_readiness_generation,
            },
        )
        .await,
        HlsAvailabilityReevaluationRegistration::Scheduled
    );
}

fn assert_availability_owner_registered(fixture: &PostRefreshTerminalFixture) {
    assert_eq!(fixture.ctx.hls_proxy.availability_reevaluations().owner_count(), 1);
}

/// How long the owner gets to finish before the wait gives up.
///
/// These tests run on a paused clock, so this is logical time and costs
/// nothing when the owner completes normally.
const AVAILABILITY_OWNER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of observer revisions to accept before concluding the owner is
/// cycling without converging.
const AVAILABILITY_OWNER_MAX_REVISIONS: usize = 1_024;

async fn wait_for_availability_owner_completion(fixture: &PostRefreshTerminalFixture) {
    let coordinator = fixture.ctx.hls_proxy.availability_reevaluations();
    // Bounded twice over, because the two ways this can fail to converge
    // need different guards. A parked owner never wakes the observer, and
    // on a paused clock the runtime is idle, so the timeout fires. An owner
    // that keeps re-arming a successor wakes the observer forever and keeps
    // the runtime busy, so the clock never advances and only the revision
    // count catches it. Either way the test fails with a diagnostic instead
    // of hanging the suite.
    let wait = async {
        let mut revisions = 0usize;
        while let Some(mut observer) = coordinator.observe_owner(&fixture.proxy_session_id) {
            if matches!(
                observer.changed().await,
                crate::availability_reevaluation::HlsAvailabilityReevaluationObservation::OwnerFinished
            ) {
                // The owner is gone; re-check the map and leave the loop.
                continue;
            }
            revisions += 1;
            assert!(
                revisions <= AVAILABILITY_OWNER_MAX_REVISIONS,
                "availability owner for {:?} produced {revisions} revisions without completing",
                fixture.proxy_session_id
            );
        }
    };
    assert!(
        tokio::time::timeout(AVAILABILITY_OWNER_COMPLETION_TIMEOUT, wait).await.is_ok(),
        "availability owner for {:?} did not complete within {AVAILABILITY_OWNER_COMPLETION_TIMEOUT:?}; \
         {} owner(s) still registered",
        fixture.proxy_session_id,
        coordinator.owner_count()
    );
    assert_eq!(coordinator.owner_count(), 0);
}

async fn assert_post_refresh_owner_checks_refresh_gate_once(fixture: &PostRefreshTerminalFixture) {
    let refresh_skipped_before = fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped;
    register_real_post_refresh_owner(
        fixture,
        super::super::refresh::HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
    )
    .await;
    for _ in 0..256 {
        if fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped == refresh_skipped_before.saturating_add(1) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped,
        refresh_skipped_before.saturating_add(1),
        "the owner must observe the unavailable refresh gate once"
    );

    for _ in 0..10 {
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fixture.ctx.hls_proxy.metrics().snapshot().refresh_skipped,
        refresh_skipped_before.saturating_add(1),
        "unchanged refresh evidence must not produce repeated gate attempts"
    );

    fixture.ctx.hls_proxy.availability_reevaluations().cancel_session(&fixture.proxy_session_id);
    wait_for_availability_owner_completion(fixture).await;
}

async fn post_refresh_terminal_fixture(name: &str, terminal_asset: bool) -> PostRefreshTerminalFixture {
    post_refresh_terminal_fixture_with_progress(name, terminal_asset, true).await
}

async fn post_refresh_terminal_fixture_with_progress(
    name: &str,
    terminal_asset: bool,
    complete_playback: bool,
) -> PostRefreshTerminalFixture {
    post_refresh_terminal_fixture_with_bundle_state(name, terminal_asset, complete_playback, true).await
}

fn post_refresh_origin_manifest(terminal_asset: bool) -> (u64, ParsedOriginManifest) {
    let (target_duration_ms, body) = if terminal_asset {
        (
            12_000,
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:12\n\
             #EXTINF:12.0,\n0.ts\n#EXTINF:12.0,\n1.ts\n#EXTINF:12.0,\n2.ts\n",
        )
    } else {
        (
            8_000,
            "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-TARGETDURATION:8\n\
             #EXTINF:4.0,\n0.ts\n#EXTINF:8.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n",
        )
    };
    let OriginManifestParseOutcome::Normal(manifest) =
        parse_origin_media_manifest(body, "http://origin.example/live/index.m3u8")
    else {
        panic!("post-refresh terminal fixture parses");
    };
    (target_duration_ms, manifest)
}

async fn prepare_post_refresh_terminal_base(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    manifest: &ParsedOriginManifest,
    terminal_asset: bool,
    prepare_terminal_bundle: bool,
    target_duration_ms: u64,
    now_ms: u64,
) {
    let terminal_base_cache_key = {
        let mut session = session.write().await;
        session.apply_origin_manifest(manifest).expect("fixture timeline applies");
        for segment in session.segments.values_mut() {
            segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: now_ms };
        }
        session.origin_control.path_condition = HlsOriginPathCondition::AcceptanceConflict;
        session.origin_control.last_media_progress_at_ms = Some(now_ms);
        session.origin_control.target_duration_snapshot_ms = Some(target_duration_ms);
        session.segments.get(&0).expect("fixture terminal-base segment").cache_key.clone()
    };
    if !terminal_asset {
        return;
    }
    ctx.hls_proxy
        .segment_cache()
        .write_bytes_and_commit(&terminal_base_cache_key, TERMINAL_ASSET_BYTES)
        .await
        .expect("terminal-compatible READY base bytes commit");
    session.write().await.segments.get_mut(&0).expect("fixture terminal-base segment").status =
        SegmentCacheStatus::Ready {
            content_length: u64::try_from(TERMINAL_ASSET_BYTES.len()).unwrap_or(u64::MAX),
            ready_at_ms: now_ms,
        };
    if prepare_terminal_bundle {
        let asset = snapshot_terminal_media_asset(&TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec()))
            .expect("fixture terminal asset parses");
        let key = prepared_terminal_bundle_key(&asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
        let state = ctx.hls_proxy.start_prepared_terminal_bundle(
            Arc::clone(&asset),
            target_duration_ms,
            HLS_TERMINAL_TAIL_SEGMENT_COUNT,
        );
        let state = match state {
            HlsPreparedTerminalBundleState::Preparing { .. } => {
                ctx.hls_proxy.wait_for_prepared_terminal_bundle(key).await
            }
            HlsPreparedTerminalBundleState::Ready { .. }
            | HlsPreparedTerminalBundleState::Failed { .. }
            | HlsPreparedTerminalBundleState::Incompatible { .. } => Some(state),
        };
        assert!(
            matches!(state, Some(HlsPreparedTerminalBundleState::Ready { .. })),
            "terminal preparation must be ready: {state:?}"
        );
    }
}

async fn publish_post_refresh_terminal_lease(
    ctx: &HlsCtx,
    proxy_session_id: &ProxySessionId,
    name: &str,
    terminal_asset: bool,
    target_duration_ms: u64,
    now_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId(format!("{name}-lease"));
    ctx.hls_proxy
        .prepare_access_lease(HlsAccessLease::pending(
            lease_id.clone(),
            HlsPlaybackFamilyKey::new(name, name),
            proxy_session_id.clone(),
            name.to_string(),
            "token".to_string(),
            1,
            "stream".to_string(),
            1,
            now_ms,
            60_000,
        ))
        .await;
    let publication = ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, proxy_session_id, now_ms)
        .await
        .expect("terminal fixture publication guard");
    let mut manifest_snapshot = pressure_manifest(target_duration_ms);
    if terminal_asset {
        Arc::make_mut(&mut manifest_snapshot.visible_segments)[0].duration_ms = target_duration_ms;
        manifest_snapshot.playlist_duration_ms = target_duration_ms;
        manifest_snapshot.last_visible_media_end_ms = target_duration_ms;
    }
    Arc::make_mut(&mut manifest_snapshot.visible_segments)[0].uri =
        format!("/hls/shared/live/{}/{}/0.ts", proxy_session_id.0, lease_id.0).into();
    assert!(ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(&lease_id, proxy_session_id, publication, manifest_snapshot, now_ms,)
        .await
        .is_committed());
    assert!(ctx
        .hls_proxy
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            now_ms,
            HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
    assert!(
        ctx.hls_proxy.access_lease_response_snapshot(&lease_id, proxy_session_id, now_ms).await.is_some(),
        "activated terminal fixture lease remains available"
    );
    lease_id
}

async fn post_refresh_terminal_fixture_with_bundle_state(
    name: &str,
    terminal_asset: bool,
    complete_playback: bool,
    prepare_terminal_bundle: bool,
) -> PostRefreshTerminalFixture {
    let hls_ctx = crate::HlsCtx::for_test(Config { custom_stream_response_enabled: true, ..Config::default() });
    let ctx = &hls_ctx;
    if terminal_asset {
        ctx.app_config.custom_stream_response.store(Some(runtime_custom_responses()));
    }
    let now_ms = ctx.hls_proxy.terminal_commit_now_ms();
    let (session, _) =
        ctx.hls_proxy.get_or_create_session_with_outcome(HlsSessionKey::new(1, name), b"secret", now_ms).await;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let (target_duration_ms, manifest) = post_refresh_origin_manifest(terminal_asset);
    prepare_post_refresh_terminal_base(
        ctx,
        &session,
        &manifest,
        terminal_asset,
        prepare_terminal_bundle,
        target_duration_ms,
        now_ms,
    )
    .await;
    let lease_id =
        publish_post_refresh_terminal_lease(ctx, &proxy_session_id, name, terminal_asset, target_duration_ms, now_ms)
            .await;
    if complete_playback {
        advance_post_refresh_fixture_playback(
            ctx,
            &session,
            &proxy_session_id,
            &lease_id,
            if terminal_asset { 22_000 } else { 7_000 },
            now_ms,
        )
        .await;
    }
    PostRefreshTerminalFixture { ctx: ctx.clone(), session, proxy_session_id, lease_id, now_ms }
}

async fn prepare_runtime_custom_bundle(fixture: &PostRefreshTerminalFixture, reason: HlsRuntimeCustomTailReason) {
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("runtime custom-tail fixture lease");
    let target_duration_ms =
        lease.last_manifest_snapshot.as_ref().expect("published runtime custom-tail manifest").target_duration_ms;
    let asset =
        snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason).expect("configured runtime custom-tail asset");
    let key = prepared_terminal_bundle_key(&asset.asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let state = fixture.ctx.hls_proxy.start_prepared_terminal_bundle(
        Arc::clone(&asset.asset),
        target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let state = match state {
        HlsPreparedTerminalBundleState::Preparing { .. } => fixture
            .ctx
            .hls_proxy
            .wait_for_prepared_terminal_bundle(key)
            .await
            .expect("runtime custom-tail bundle completion"),
        state => state,
    };
    assert!(
        matches!(state, HlsPreparedTerminalBundleState::Ready { ref bundle } if bundle.matches_key_and_shape(key)),
        "runtime custom-tail bundle must be READY: {state:?}"
    );
}

async fn wait_for_runtime_custom_plan(fixture: &PostRefreshTerminalFixture) -> Arc<HlsTerminalTailPlan> {
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let lease = fixture
                .ctx
                .hls_proxy
                .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
                .await
                .expect("runtime custom-tail lease remains stored");
            if let HlsLeasePlaybackMode::TerminalTail(plan) = lease.playback_mode {
                return plan;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Ok(plan) = completed {
        return plan;
    }
    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await;
    let state = lease.as_ref().map_or("missing", |lease| lease.state.as_log_value());
    let playback = lease.as_ref().map_or("missing", |lease| match lease.playback_mode {
        HlsLeasePlaybackMode::Live => "live",
        HlsLeasePlaybackMode::TerminalTail(_) => "terminal-tail",
        HlsLeasePlaybackMode::TerminalUnavailable { .. } => "terminal-unavailable",
        HlsLeasePlaybackMode::Ended => "ended",
    });
    panic!(
        "runtime custom-tail owner deadline: state={state} playback={playback} owners={}",
        fixture.ctx.hls_proxy.terminal_pending().owner_count()
    );
}

async fn commit_runtime_custom_reason(
    fixture: &PostRefreshTerminalFixture,
    reason: HlsRuntimeCustomTailReason,
    prewarm: bool,
) -> (HlsRuntimeCustomTailOutcome, Arc<HlsTerminalTailPlan>) {
    if prewarm {
        prepare_runtime_custom_bundle(fixture, reason).await;
    }
    let outcome = commit_hls_runtime_custom_tail(
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
    assert!(matches!(
        outcome,
        HlsRuntimeCustomTailOutcome::Committed
            | HlsRuntimeCustomTailOutcome::AlreadyCommitted
            | HlsRuntimeCustomTailOutcome::PendingOwnerRegistered
    ));
    let plan = wait_for_runtime_custom_plan(fixture).await;
    (outcome, plan)
}

fn configured_runtime_custom_buffer(
    fixture: &PostRefreshTerminalFixture,
    reason: HlsRuntimeCustomTailReason,
) -> TransportStreamBuffer {
    let responses = fixture.ctx.app_config.custom_stream_response.load_full().expect("runtime custom responses");
    match reason {
        HlsRuntimeCustomTailReason::ChannelUnavailable => responses.channel_unavailable.as_ref(),
        HlsRuntimeCustomTailReason::LowPriorityPreempted => responses.low_priority_preempted.as_ref(),
        HlsRuntimeCustomTailReason::UserConnectionsExhausted => responses.user_connections_exhausted.as_ref(),
        HlsRuntimeCustomTailReason::ProviderConnectionsExhausted => responses.provider_connections_exhausted.as_ref(),
        HlsRuntimeCustomTailReason::UserAccountExpired => responses.user_account_expired.as_ref(),
        HlsRuntimeCustomTailReason::SessionOrLeaseExpired => responses.hls_session_or_lease_expired.as_ref(),
    }
    .expect("reason-specific runtime buffer")
    .clone()
}

fn segment_bytes(plan: &HlsTerminalTailPlan, index: u16) -> Bytes {
    plan.segment_bytes(HlsTerminalSegmentPath { generation: plan.generation, index })
        .expect("committed immutable custom-tail segment")
}

fn payload_continuity_bounds(bytes: &[u8]) -> std::collections::HashMap<u16, (u8, u8, bool)> {
    let mut payload_bounds = std::collections::HashMap::<u16, (u8, u8)>::new();
    let mut first_packet_discontinuity = std::collections::HashMap::<u16, bool>::new();
    for packet in bytes.as_chunks::<188>().0.iter().filter(|packet| packet[0] == 0x47) {
        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        let discontinuity = matches!(adaptation_field_control, 0b10 | 0b11)
            && packet[4] > 0
            && packet.get(5).is_some_and(|flags| flags & 0x80 != 0);
        first_packet_discontinuity.entry(pid).or_insert(discontinuity);
        if !matches!(adaptation_field_control, 0b01 | 0b11) {
            continue;
        }
        let counter = packet[3] & 0x0f;
        payload_bounds.entry(pid).and_modify(|entry| entry.1 = counter).or_insert((counter, counter));
    }
    payload_bounds
        .into_iter()
        .map(|(pid, (first, last))| {
            let discontinuity = first_packet_discontinuity.get(&pid).copied().unwrap_or(false);
            (pid, (first, last, discontinuity))
        })
        .collect()
}

fn with_internal_payload_continuity_jump(bytes: &[u8]) -> Vec<u8> {
    let mut corrupted = bytes.to_vec();
    let mut first_payload_pid = None;
    for packet in corrupted.as_chunks_mut::<188>().0.iter_mut().filter(|packet| packet[0] == 0x47) {
        let adaptation_field_control = (packet[3] >> 4) & 0b11;
        if !matches!(adaptation_field_control, 0b01 | 0b11) {
            continue;
        }
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        if pid == 0x1fff {
            continue;
        }
        match first_payload_pid {
            None => first_payload_pid = Some(pid),
            Some(first_pid) if first_pid == pid => {
                let continuity_counter = packet[3] & 0x0f;
                packet[3] = (packet[3] & 0xf0) | (continuity_counter.wrapping_add(3) & 0x0f);
                return corrupted;
            }
            Some(_) => {}
        }
    }
    panic!("terminal fixture must contain two payload packets for one PID");
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
                super::super::HlsTsSpliceIncompatibility::ContinuityFailure { .. }
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
async fn preemption_does_not_fetch_another_origin_manifest() {
    let fixture = post_refresh_terminal_fixture("runtime-preemption-no-origin", true).await;
    let refresh_before = fixture.session.read().await.origin_refresh.clone();

    let _ = commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;

    assert_eq!(fixture.session.read().await.origin_refresh, refresh_before);
}

async fn assert_active_policy_reason_commits(name: &str, reason: HlsRuntimeCustomTailReason) {
    let fixture = post_refresh_terminal_fixture(name, true).await;
    let (_, plan) = commit_runtime_custom_reason(&fixture, reason, true).await;
    assert_eq!(plan.reason, reason);
    assert_eq!(plan.segment_duration_ms, 10_027);
    assert_eq!(
        plan.asset_identity,
        HlsRuntimeCustomTailAssetIdentity::from_asset(
            &snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason).expect("reason-specific configured asset")
        )
    );
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
async fn first_committed_custom_reason_is_immutable() {
    let fixture = post_refresh_terminal_fixture("runtime-first-reason", true).await;
    let (_, first) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::LowPriorityPreempted, true).await;

    let outcome = commit_hls_runtime_custom_tail(
        fixture.ctx.clone(),
        HlsRuntimeCustomTailRequest {
            session: Arc::clone(&fixture.session),
            proxy_session_id: fixture.proxy_session_id.clone(),
            lease_id: fixture.lease_id.clone(),
            reason: HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
            now_ms: fixture.now_ms.saturating_add(1),
        },
    )
    .await;
    let replay = wait_for_runtime_custom_plan(&fixture).await;

    assert_eq!(outcome, HlsRuntimeCustomTailOutcome::AlreadyCommitted);
    assert_eq!(replay.reason, HlsRuntimeCustomTailReason::LowPriorityPreempted);
    assert_eq!(replay.generation, first.generation);
    assert_eq!(segment_bytes(&replay, 0), segment_bytes(&first, 0));
}

#[tokio::test]
async fn different_late_reason_cannot_replace_committed_plan() {
    let fixture = post_refresh_terminal_fixture("runtime-late-reason", true).await;
    let (_, first) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::UserConnectionsExhausted, true).await;

    let (outcome, replay) =
        commit_runtime_custom_reason(&fixture, HlsRuntimeCustomTailReason::SessionOrLeaseExpired, false).await;

    assert_eq!(outcome, HlsRuntimeCustomTailOutcome::AlreadyCommitted);
    assert_eq!(replay.reason, HlsRuntimeCustomTailReason::UserConnectionsExhausted);
    assert_eq!(replay.asset_identity, first.asset_identity);
}

async fn wait_for_terminal_pending_owners(fixture: &PostRefreshTerminalFixture, expected: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fixture.ctx.hls_proxy.terminal_pending().owner_count() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "terminal-pending owner deadline: actual={} expected={expected}",
            fixture.ctx.hls_proxy.terminal_pending().owner_count()
        )
    });
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

#[tokio::test]
async fn same_reason_singleflight_has_one_media_finalizer() {
    let fixture = post_refresh_terminal_fixture("runtime-same-reason-singleflight", true).await;
    let reason = HlsRuntimeCustomTailReason::LowPriorityPreempted;
    let buffer = configured_runtime_custom_buffer(&fixture, reason);
    let asset = snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason).expect("singleflight asset");
    let target_duration_ms = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .and_then(|lease| lease.last_manifest_snapshot.map(|manifest| manifest.target_duration_ms))
        .expect("published target duration");
    let key = prepared_terminal_bundle_key(&asset.asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let publisher = fixture
        .ctx
        .hls_proxy
        .install_controlled_terminal_bundle_flight_for_test(key)
        .expect("controlled singleflight preparation");
    let request = || HlsRuntimeCustomTailRequest {
        session: Arc::clone(&fixture.session),
        proxy_session_id: fixture.proxy_session_id.clone(),
        lease_id: fixture.lease_id.clone(),
        reason,
        now_ms: fixture.now_ms,
    };

    let (first, second) = tokio::join!(
        commit_hls_runtime_custom_tail(fixture.ctx.clone(), request()),
        commit_hls_runtime_custom_tail(fixture.ctx.clone(), request()),
    );
    assert_eq!(first, HlsRuntimeCustomTailOutcome::PendingOwnerRegistered);
    assert_eq!(second, HlsRuntimeCustomTailOutcome::PendingOwnerRegistered);
    assert_eq!(fixture.ctx.hls_proxy.terminal_pending().owner_count(), 1);
    let bundle = build_prepared_terminal_bundle(&asset.asset, key).expect("single relative bundle");
    publisher.publish(HlsPreparedTerminalBundleCompletion::Ready { bundle });
    let plan = wait_for_runtime_custom_plan(&fixture).await;

    assert_eq!(plan.reason, reason);
    assert_eq!(buffer.finite_hls_render_count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
    assert_eq!(buffer.finite_hls_finalize_count(), usize::from(HLS_TERMINAL_TAIL_SEGMENT_COUNT));
}

async fn add_active_post_refresh_lease(
    fixture: &PostRefreshTerminalFixture,
    lease_id: HlsAccessLeaseId,
    active_map: Option<HlsMapSignature>,
) {
    let lease = HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new("multi-lease", &lease_id.0),
        fixture.proxy_session_id.clone(),
        "multi-lease".to_string(),
        lease_id.0.clone(),
        1,
        "stream".to_string(),
        1,
        fixture.now_ms,
        60_000,
    );
    fixture.ctx.hls_proxy.prepare_access_lease(lease).await;
    let publication = fixture
        .ctx
        .hls_proxy
        .prepare_access_lease_manifest_publication(&lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("second lease publication guard");
    let mut manifest = pressure_manifest(12_000);
    manifest.active_map = active_map;
    Arc::make_mut(&mut manifest.visible_segments)[0].uri =
        format!("/hls/shared/live/{}/{}/0.ts", fixture.proxy_session_id.0, lease_id.0).into();
    assert!(fixture
        .ctx
        .hls_proxy
        .commit_access_lease_manifest_publication(
            &lease_id,
            &fixture.proxy_session_id,
            publication,
            manifest,
            fixture.now_ms,
        )
        .await
        .is_committed());
    assert!(fixture
        .ctx
        .hls_proxy
        .activate_access_lease(
            &lease_id,
            &fixture.proxy_session_id,
            fixture.now_ms,
            HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 60_000 },
        )
        .await
        .is_activated());
}

async fn advance_post_refresh_fixture_playback(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    playback_elapsed_ms: u64,
    now_ms: u64,
) {
    let lease = ctx
        .hls_proxy
        .access_lease_response_snapshot(lease_id, proxy_session_id, now_ms)
        .await
        .expect("live terminal fixture remains available");
    let identity = lease.media_identity().expect("live terminal fixture identity");
    let playback_at_ms = now_ms.saturating_sub(playback_elapsed_ms);
    for proxy_seq in [0_u64, 1] {
        let token = ctx
            .hls_proxy
            .record_access_lease_segment_request_started_if_identity_matches(
                lease_id,
                proxy_session_id,
                identity,
                proxy_seq,
                playback_at_ms,
            )
            .await
            .expect("fixture segment request starts");
        assert_eq!(
            ctx.hls_proxy
                .record_access_lease_segment_request_completed_and_mark_media_if_identity_matches(
                    session,
                    lease_id,
                    proxy_session_id,
                    identity,
                    token,
                    playback_at_ms,
                )
                .await,
            super::super::manager::HlsMediaActivityCommitOutcome::Committed
        );
    }
}

#[tokio::test(start_paused = true)]
async fn in_flight_post_refresh_owner_does_not_poll_refresh_gate() {
    let fixture = post_refresh_terminal_fixture_with_progress("in-flight-refresh-wait", true, false).await;
    fixture.session.write().await.origin_refresh.mark_started(fixture.now_ms);
    assert_post_refresh_owner_checks_refresh_gate_once(&fixture).await;
}

#[tokio::test(start_paused = true)]
async fn debounced_post_refresh_owner_does_not_poll_refresh_gate() {
    let fixture = post_refresh_terminal_fixture_with_progress("debounced-refresh-wait", true, false).await;
    fixture.session.write().await.origin_refresh.next_fetch_allowed_at_ms = fixture.now_ms.saturating_add(10_000);
    assert_post_refresh_owner_checks_refresh_gate_once(&fixture).await;
}

#[tokio::test(start_paused = true)]
async fn persistent_conflict_real_owner_commits_before_exclusive_deadline() {
    let fixture = post_refresh_terminal_fixture_with_progress("real-owner-terminal", true, false).await;
    register_real_post_refresh_owner(
        &fixture,
        super::super::refresh::HlsPostRefreshAvailabilityReason::DeterministicTimelineConflict,
    )
    .await;
    assert_availability_owner_registered(&fixture);

    tokio::time::advance(Duration::from_millis(2_100)).await;
    assert_eq!(
        fixture.ctx.hls_proxy.availability_reevaluations().owner_count(),
        1,
        "the real owner must survive its rapid evaluation budget while reserve remains"
    );
    advance_post_refresh_fixture_playback(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        &fixture.lease_id,
        20_100,
        fixture.now_ms,
    )
    .await;
    wait_for_availability_owner_completion(&fixture).await;

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("terminal lease remains stored");
    assert!(matches!(lease.playback_mode, HlsLeasePlaybackMode::TerminalTail(_)));
}

#[tokio::test(start_paused = true)]
async fn hard_failure_real_owner_commits_unavailable_before_exclusive_deadline() {
    let fixture = post_refresh_terminal_fixture_with_progress("real-owner-unavailable", false, false).await;
    register_real_post_refresh_owner(
        &fixture,
        super::super::refresh::HlsPostRefreshAvailabilityReason::HardManifestFailure,
    )
    .await;
    assert_availability_owner_registered(&fixture);

    tokio::time::advance(Duration::from_millis(2_100)).await;
    assert_eq!(fixture.ctx.hls_proxy.availability_reevaluations().owner_count(), 1);
    advance_post_refresh_fixture_playback(
        &fixture.ctx,
        &fixture.session,
        &fixture.proxy_session_id,
        &fixture.lease_id,
        7_000,
        fixture.now_ms,
    )
    .await;
    wait_for_availability_owner_completion(&fixture).await;

    let lease = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&fixture.lease_id, &fixture.proxy_session_id, fixture.now_ms)
        .await
        .expect("unavailable lease remains stored");
    assert!(matches!(
        lease.playback_mode,
        HlsLeasePlaybackMode::TerminalUnavailable { reason: HlsTerminalTailCompatibility::MissingAsset, .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn failed_closed_retry_capacity_retains_owner() {
    let fixture = post_refresh_terminal_fixture("capacity-retained-owner", false).await;
    fixture.ctx.hls_proxy.set_terminal_commit_retry_capacity_for_test(0);
    register_real_post_refresh_owner(
        &fixture,
        super::super::refresh::HlsPostRefreshAvailabilityReason::HardManifestFailure,
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

    fixture
        .ctx
        .hls_proxy
        .set_terminal_commit_retry_capacity_for_test(super::super::terminal_commit::HLS_TERMINAL_COMMIT_RETRY_CAPACITY);
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
                super::super::refresh::HlsPostRefreshAvailabilityReason::HardManifestFailure,
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

async fn assert_multi_lease_fallback_handles_pending_and_unavailable(reverse_insertion: bool) {
    let fixture_name = if reverse_insertion { "multi-lease-fallback-reverse" } else { "multi-lease-fallback-forward" };
    let fixture = post_refresh_terminal_fixture_with_bundle_state(fixture_name, true, true, false).await;
    let incompatible_lease_id = HlsAccessLeaseId("multi-lease-incompatible".to_string());
    add_active_post_refresh_lease(
        &fixture,
        incompatible_lease_id.clone(),
        Some(HlsMapSignature { fingerprint: [0x5a; 32], container: HlsMediaContainer::FragmentedMp4 }),
    )
    .await;
    if reverse_insertion {
        let mut leases = fixture.ctx.hls_proxy.hold_access_lease_store_for_test().await;
        let primary = leases.remove_access_lease(&fixture.lease_id).expect("primary live lease remains stored");
        leases.prepare_access_lease(primary);
    }
    let asset = snapshot_terminal_media_asset(&TransportStreamBuffer::new(TERMINAL_ASSET_BYTES.to_vec()))
        .expect("controlled terminal asset parses");
    let bundle_key = prepared_terminal_bundle_key(&asset, 12_000, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let _controlled_flight = fixture
        .ctx
        .hls_proxy
        .install_controlled_terminal_bundle_flight_for_test(bundle_key)
        .expect("controlled terminal preparation is unique");
    let evaluation_now_ms = fixture.ctx.hls_proxy.terminal_commit_now_ms();

    let aggregate =
        evaluate_owner_failure_fallback(&fixture.ctx, &fixture.session, &fixture.proxy_session_id, evaluation_now_ms)
            .await;

    assert_eq!(aggregate.total, 2);
    assert_eq!(aggregate.pending_owned, 1);
    assert_eq!(aggregate.terminal_committed, 1, "{aggregate:?}");
    assert!(aggregate.unresolved.is_empty());
    assert_eq!(fixture.ctx.hls_proxy.terminal_pending().owner_count(), 1);
    let unavailable = fixture
        .ctx
        .hls_proxy
        .access_lease_response_snapshot(&incompatible_lease_id, &fixture.proxy_session_id, evaluation_now_ms)
        .await
        .expect("incompatible lease remains stored");
    assert!(matches!(unavailable.playback_mode, HlsLeasePlaybackMode::TerminalUnavailable { .. }));
    fixture.ctx.hls_proxy.terminal_pending().cancel_session(&fixture.proxy_session_id);
}

#[tokio::test(start_paused = true)]
async fn multi_lease_fallback_keeps_pending_owner_and_commits_other_unavailable() {
    assert_multi_lease_fallback_handles_pending_and_unavailable(false).await;
}

#[tokio::test(start_paused = true)]
async fn multi_lease_fallback_handles_reverse_insertion_without_early_return() {
    assert_multi_lease_fallback_handles_pending_and_unavailable(true).await;
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

fn install_atomic_pressure_lease(
    store: &mut HlsAccessLeaseStore,
    proxy_session_id: &ProxySessionId,
    lease_id: &str,
    manifest: HlsLeaseManifestSnapshot,
    valid_window_ms: u64,
) -> HlsAccessLeaseId {
    let lease_id = HlsAccessLeaseId(lease_id.to_string());
    store.prepare_access_lease(HlsAccessLease::pending(
        lease_id.clone(),
        HlsPlaybackFamilyKey::new(lease_id.0.clone(), lease_id.0.clone()),
        proxy_session_id.clone(),
        lease_id.0.clone(),
        "token".to_string(),
        1,
        "stream".to_string(),
        1,
        0,
        valid_window_ms,
    ));
    let guard = store.prepare_manifest_publication(&lease_id, proxy_session_id, 1).expect("manifest publication guard");
    assert!(store.commit_manifest_publication(&lease_id, proxy_session_id, guard, manifest, 1).is_committed());
    assert!(store
        .activate_access_lease(
            &lease_id,
            proxy_session_id,
            2,
            HlsAccessLeaseTiming { active_window_ms: valid_window_ms, valid_window_ms },
        )
        .is_activated());
    lease_id
}

#[tokio::test]
async fn hls_manifest_acceptance_directive_transient_lock_busy_retries_regular_evaluation() {
    let mut session = atomic_pressure_session();
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    install_atomic_pressure_lease(
        &mut leases,
        &proxy_session_id,
        "availability-transient",
        pressure_manifest_at(0, 8_000),
        1_000,
    );
    let attempts = std::cell::Cell::new(0_usize);
    let clock_calls = std::cell::Cell::new(0_usize);

    let access = retry_availability_state_access(|| {
        let attempt = attempts.get();
        attempts.set(attempt.saturating_add(1));
        let access = if attempt == 0 {
            HlsCriticalHandoffStateAccess::LockBusy
        } else {
            HlsCriticalHandoffStateAccess::Acquired(evaluate_and_commit_session_recovery_pressure_in_snapshot(
                &mut leases,
                &mut session,
                &proxy_session_id,
                atomic_pressure_policy(),
                || {
                    assert_eq!(attempts.get(), 2);
                    clock_calls.set(clock_calls.get().saturating_add(1));
                    101
                },
            ))
        };
        std::future::ready(access)
    })
    .await;

    let evidence = availability_snapshot_or_contention(access)
        .expect("transient contention must reach the regular snapshot evaluation")
        .expect("the active lease supplies recovery evidence");
    assert_eq!(evidence.timing_seed.target_duration_ms, 8_000);
    assert_eq!(attempts.get(), 2);
    assert_eq!(clock_calls.get(), 1);
}

#[tokio::test]
async fn hls_manifest_acceptance_directive_exhausted_contention_is_typed() {
    let attempts = std::cell::Cell::new(0_usize);
    let access: HlsCriticalHandoffStateAccess<u8> = retry_availability_state_access(|| {
        attempts.set(attempts.get().saturating_add(1));
        std::future::ready(HlsCriticalHandoffStateAccess::LockBusy)
    })
    .await;

    let outcome = availability_snapshot_or_contention(access)
        .expect_err("exhausted contention must remain a typed endpoint outcome");

    assert_eq!(attempts.get(), HLS_AVAILABILITY_STATE_ACCESS_ATTEMPTS);
    assert_eq!(outcome, HlsAvailabilitySnapshotAccessError::StateContention);
}

#[test]
fn hls_manifest_acceptance_directive_samples_time_inside_snapshot_scope() {
    let mut session = atomic_pressure_session();
    let proxy_session_id = session.proxy_session_id.clone();
    let mut leases = HlsAccessLeaseStore::default();
    install_atomic_pressure_lease(
        &mut leases,
        &proxy_session_id,
        "availability-clock",
        pressure_manifest_at(0, 8_000),
        150,
    );
    let clock_calls = std::cell::Cell::new(0_usize);

    let evidence = evaluate_and_commit_session_recovery_pressure_in_snapshot(
        &mut leases,
        &mut session,
        &proxy_session_id,
        atomic_pressure_policy(),
        || {
            clock_calls.set(clock_calls.get().saturating_add(1));
            200
        },
    );

    assert_eq!(clock_calls.get(), 1);
    assert!(evidence.is_none(), "the snapshot-local clock must exclude the lease expired at evaluation time");
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
    let mut episode = super::super::manifest_acceptance::HlsManifestAcceptanceEpisode::new(
        super::super::manifest_acceptance::HlsManifestAcceptanceGeneration(1),
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
