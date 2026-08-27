//! Cutting a lease over to the terminal tail.
//!
//! When a lease's reserve says the session cannot carry it any further, this is
//! what decides whether the terminal tail may be committed now, prepares the
//! splice, and commits it. The runtime-custom-tail variant is the same decision
//! against a configured asset rather than a recorded one.

use super::{
    recovery_pressure::{hls_recovery_pressure_policy, recovery_trigger_budget},
    terminal_pending_owner::{
        commit_prepared_terminal_unavailable_after_owner_failure, commit_terminal_unavailable,
        register_ready_terminal_pending_owner, register_terminal_pending_owner, terminal_asset_revision_guard,
        terminal_resolution_with_failed_closed_fallback,
    },
    HlsDetailedTerminalResolution, HlsTerminalDecisionPurpose, HlsTerminalFailedClosedReason, HlsTerminalResolution,
    HLS_PLAYBACK_RATE_GUARD_MILLI,
};
use crate::{
    hls_ctx::HlsCtx,
    lease::{HlsAccessLease, HlsTerminalTailPreparation},
    manager::{HlsTerminalCommitPayload, HlsTerminalCommitRequest, HlsTerminalTailPreparationRequest},
    media_reserve::{
        evaluate_lease_reserve, HlsLeaseManifestSnapshot, HlsLeaseReserveInput, HlsLeaseReserveSnapshot,
        HlsReadyTimelineSnapshot,
    },
    origin_progress::{
        evaluate_origin_progress, publication_late_after_ms, HlsOriginPathCondition, HlsOriginProgressDecision,
        HlsOriginProgressPhase, HlsOriginProgressSnapshot,
    },
    post_refresh_availability::{live_reserve_deadline, HlsLiveReserveDeadline},
    prepared_terminal_bundle::{
        anchor_prepared_terminal_bundle, prepared_terminal_bundle_key, HlsAnchoredTerminalBundle,
        HlsPreparedTerminalBundle, HlsPreparedTerminalBundleBuildError, HlsPreparedTerminalBundleFailure,
        HlsPreparedTerminalBundleIncompatibility, HlsPreparedTerminalBundleKey, HlsPreparedTerminalBundleState,
    },
    recovery_timing::{
        HlsLeaseCutoverTiming, HlsObservedRecoveryLatency, HlsRecoveryWorkloadEnvelope,
        HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitWindow,
    },
    runtime_custom_tail::{
        snapshot_hls_runtime_custom_tail_asset, HlsRuntimeCustomTailAsset, HlsRuntimeCustomTailAssetIdentity,
    },
    session_store::HlsSessionHandle,
    terminal_commit::HlsTerminalCommitOutcome,
    terminal_tail::{
        build_terminal_tail_plan, prepare_terminal_base_evidence, HlsMediaContainer, HlsTerminalBaseEvidence,
        HlsTerminalBaseTimingEvidence, HlsTerminalTailBuildInput, HlsTerminalTailCompatibility,
        HlsTerminalTailGeneration, HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    },
    HlsAccessLeaseId, ProxySessionId,
};
use log::debug;
use std::sync::Arc;
use tuliprox_mpegts::transport_stream_buffer::HlsTsSpliceAnchor;

struct HlsLeaseCutoverStateSnapshot {
    ready_timeline: HlsReadyTimelineSnapshot,
    capacity_recovery_blocks_ready_timeline: bool,
    progress_phase: HlsOriginProgressPhase,
    path_condition: HlsOriginPathCondition,
    progress_generation: u64,
    media_readiness_generation: u64,
    last_media_progress_at_ms: Option<u64>,
    target_duration_ms: u64,
    observed_latency: HlsObservedRecoveryLatency,
}

#[derive(Clone, Copy)]
struct HlsLeaseTerminalCutoverEvaluation {
    origin_path_degraded: bool,
    reserve: HlsLeaseReserveSnapshot,
    cutover_timing: HlsLeaseCutoverTiming,
    safe_deadline: Option<HlsLiveReserveDeadline>,
    commit_window: HlsTerminalCommitWindow,
    progress_decision: HlsOriginProgressDecision,
}

struct HlsLeaseTerminalDecisionContext<'a> {
    ctx: &'a HlsCtx,
    session: &'a HlsSessionHandle,
    proxy_session_id: &'a ProxySessionId,
    lease: &'a HlsAccessLease,
    manifest: &'a HlsLeaseManifestSnapshot,
    state: &'a HlsLeaseCutoverStateSnapshot,
    evaluation: HlsLeaseTerminalCutoverEvaluation,
    now_ms: u64,
    purpose: HlsTerminalDecisionPurpose,
}

impl<'a> HlsLeaseTerminalDecisionContext<'a> {
    fn preparation_request(&self) -> HlsTerminalTailPreparationRequest<'a> {
        HlsTerminalTailPreparationRequest {
            lease_id: &self.lease.lease_id,
            proxy_session_id: self.proxy_session_id,
            manifest_snapshot_generation: self.manifest.snapshot_generation,
            cursor_generation: self.lease.playback_cursor.cursor_generation,
            reserve: self.evaluation.reserve,
            cutover_timing: self.evaluation.cutover_timing,
            commit_window: self.evaluation.commit_window,
            now_ms: self.now_ms,
            origin_progress_generation: self.state.progress_generation,
            media_readiness_generation: self.state.media_readiness_generation,
            last_media_progress_at_ms: self.state.last_media_progress_at_ms,
        }
    }
}

fn evaluate_lease_terminal_cutover(
    ctx: &HlsCtx,
    lease: &HlsAccessLease,
    manifest: &HlsLeaseManifestSnapshot,
    state: &HlsLeaseCutoverStateSnapshot,
    now_ms: u64,
) -> HlsLeaseTerminalCutoverEvaluation {
    let origin_path_degraded = state.path_condition.is_degraded()
        || state.last_media_progress_at_ms.is_some_and(|last_progress_at_ms| {
            now_ms.saturating_sub(last_progress_at_ms) >= publication_late_after_ms(state.target_duration_ms)
        });
    let workload = HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling();
    let pressure_policy = hls_recovery_pressure_policy(&ctx.hls_proxy, ctx.hls_proxy.origin_manifest_timeout_ms());
    let recovery_trigger_budget =
        recovery_trigger_budget(pressure_policy, manifest.target_duration_ms, workload, state.observed_latency);
    let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
        manifest,
        cursor: &lease.playback_cursor,
        ready_timeline: &state.ready_timeline,
        now_ms,
        playback_rate_guard_milli: HLS_PLAYBACK_RATE_GUARD_MILLI,
        recovery_trigger_budget,
        origin_path_degraded,
        recovery_committed: false,
    });
    let cutover_timing =
        HlsLeaseCutoverTiming::from_reserve(now_ms, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
    let safe_deadline = live_reserve_deadline(now_ms, reserve, cutover_timing);
    let commit_window = cutover_timing.terminal_commit_window(
        origin_path_degraded,
        false,
        HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy(),
    );
    let progress_decision = evaluate_origin_progress(HlsOriginProgressSnapshot {
        phase: state.progress_phase,
        condition: state.path_condition,
        target_duration_ms: state.target_duration_ms,
        last_media_progress_at_ms: state.last_media_progress_at_ms,
        session_recovery_required: reserve.recovery_required,
        session_cutover_evaluation_required: reserve.cutover_required,
        recovery_committed: false,
        now_ms,
    });
    HlsLeaseTerminalCutoverEvaluation {
        origin_path_degraded,
        reserve,
        cutover_timing,
        safe_deadline,
        commit_window,
        progress_decision,
    }
}

async fn resolve_terminal_cutover_before_commit_window(
    context: &HlsLeaseTerminalDecisionContext<'_>,
) -> HlsDetailedTerminalResolution {
    if !context.evaluation.origin_path_degraded {
        return HlsDetailedTerminalResolution::resolved(HlsTerminalResolution::LiveAllowed);
    }
    if matches!(context.purpose, HlsTerminalDecisionPurpose::AutonomousOwnerFailureFallback) {
        let Some(preparation) = context
            .ctx
            .hls_proxy
            .prepare_access_lease_terminal_unavailable_after_owner_failure(context.preparation_request())
            .await
        else {
            return HlsDetailedTerminalResolution::with_deadline(
                HlsTerminalResolution::Reevaluate,
                context.evaluation.safe_deadline,
            );
        };
        return HlsDetailedTerminalResolution::with_deadline(
            commit_prepared_terminal_unavailable_after_owner_failure(
                context.ctx,
                context.session,
                context.proxy_session_id,
                &context.lease.lease_id,
                &preparation,
                context.now_ms,
            ),
            context.evaluation.safe_deadline,
        );
    }
    let Some(live_reserve_deadline) = context.evaluation.safe_deadline else {
        return HlsDetailedTerminalResolution::resolved(HlsTerminalResolution::Reevaluate);
    };
    HlsDetailedTerminalResolution {
        resolution: HlsTerminalResolution::LiveAllowed,
        live_reserve_deadline: Some(live_reserve_deadline),
    }
}

async fn commit_terminal_cutover(context: &HlsLeaseTerminalDecisionContext<'_>) -> HlsDetailedTerminalResolution {
    let Some(preparation) =
        context.ctx.hls_proxy.prepare_access_lease_terminal_tail(context.preparation_request()).await
    else {
        return HlsDetailedTerminalResolution::with_deadline(
            HlsTerminalResolution::Reevaluate,
            context.evaluation.safe_deadline,
        );
    };
    HlsDetailedTerminalResolution::with_deadline(
        commit_prepared_terminal_decision(
            context.ctx,
            context.session,
            context.proxy_session_id,
            &context.lease.lease_id,
            &preparation,
            context.now_ms,
        )
        .await,
        context.evaluation.safe_deadline,
    )
}

#[derive(Clone, Copy)]
pub(super) struct HlsTerminalCommitContext<'a> {
    pub(super) ctx: &'a HlsCtx,
    pub(super) session: &'a HlsSessionHandle,
    pub(super) proxy_session_id: &'a ProxySessionId,
    pub(super) lease_id: &'a HlsAccessLeaseId,
    pub(super) preparation: &'a HlsTerminalTailPreparation,
    pub(super) now_ms: u64,
}

/// Re-evaluates one live lease and, if its real READY reserve has reached the
/// transition margin, publishes a generation-bound terminal decision.
pub async fn commit_terminal_tail_if_lease_reserve_requires_cutover(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease: &HlsAccessLease,
    now_ms: u64,
) -> HlsTerminalResolution {
    commit_terminal_tail_if_lease_reserve_requires_cutover_detailed(
        ctx,
        session,
        proxy_session_id,
        lease,
        now_ms,
        HlsTerminalDecisionPurpose::OrdinaryCutover,
    )
    .await
    .resolution
}

pub(crate) async fn commit_terminal_tail_if_lease_reserve_requires_cutover_detailed(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease: &HlsAccessLease,
    now_ms: u64,
    purpose: HlsTerminalDecisionPurpose,
) -> HlsDetailedTerminalResolution {
    if lease.playback_mode != crate::terminal_tail::HlsLeasePlaybackMode::Live {
        return HlsDetailedTerminalResolution::resolved(HlsTerminalResolution::Committed);
    }
    let Some(manifest) = lease.last_manifest_snapshot.as_ref() else {
        return HlsDetailedTerminalResolution::resolved(HlsTerminalResolution::FailedClosed {
            reason: HlsTerminalFailedClosedReason::LeaseStateUnavailable,
        });
    };
    let state = snapshot_lease_cutover_state(
        session,
        lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
        manifest.target_duration_ms,
        now_ms,
    )
    .await;
    if state.capacity_recovery_blocks_ready_timeline && matches!(purpose, HlsTerminalDecisionPurpose::OrdinaryCutover) {
        return HlsDetailedTerminalResolution::resolved(HlsTerminalResolution::LiveAllowed);
    }
    let evaluation = evaluate_lease_terminal_cutover(ctx, lease, manifest, &state, now_ms);
    {
        let mut session = session.write().await;
        if session.origin_control.progress_generation == state.progress_generation
            && session.activity.media_readiness_generation == state.media_readiness_generation
            && session.origin_control.last_media_progress_at_ms == state.last_media_progress_at_ms
        {
            session.origin_control.progress_phase = evaluation.progress_decision.next_phase;
        } else {
            return HlsDetailedTerminalResolution::with_deadline(
                HlsTerminalResolution::Reevaluate,
                evaluation.safe_deadline,
            );
        }
    }
    let commit_window = evaluation.commit_window;
    let decision_context = HlsLeaseTerminalDecisionContext {
        ctx,
        session,
        proxy_session_id,
        lease,
        manifest,
        state: &state,
        evaluation,
        now_ms,
        purpose,
    };
    if commit_window == HlsTerminalCommitWindow::NotDue {
        resolve_terminal_cutover_before_commit_window(&decision_context).await
    } else {
        commit_terminal_cutover(&decision_context).await
    }
}

async fn snapshot_lease_cutover_state(
    session: &HlsSessionHandle,
    ready_timeline_start_proxy_seq: u64,
    manifest_target_duration_ms: u64,
    now_ms: u64,
) -> HlsLeaseCutoverStateSnapshot {
    let session = session.read().await;
    let ready_timeline = session.ready_timeline_snapshot(ready_timeline_start_proxy_seq, now_ms);
    let capacity_recovery_blocks_ready_timeline = session.capacity_recovery_blocks_ready_timeline(&ready_timeline);
    HlsLeaseCutoverStateSnapshot {
        ready_timeline,
        capacity_recovery_blocks_ready_timeline,
        progress_phase: session.origin_control.progress_phase,
        path_condition: session.origin_control.path_condition,
        progress_generation: session.origin_control.progress_generation,
        media_readiness_generation: session.activity.media_readiness_generation,
        last_media_progress_at_ms: session.origin_control.last_media_progress_at_ms,
        target_duration_ms: session.origin_control.target_duration_snapshot_ms.unwrap_or(manifest_target_duration_ms),
        observed_latency: session.origin_control.recovery_samples.latency_snapshot(),
    }
}

async fn commit_prepared_terminal_decision(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    preparation: &HlsTerminalTailPreparation,
    now_ms: u64,
) -> HlsTerminalResolution {
    let reason = preparation.trigger.reason();
    let asset = match snapshot_hls_runtime_custom_tail_asset(ctx, reason) {
        Ok(asset) => asset,
        Err(compatibility) => {
            let context = HlsTerminalCommitContext { ctx, session, proxy_session_id, lease_id, preparation, now_ms };
            let outcome = commit_terminal_unavailable(context, None, compatibility.terminal_compatibility());
            return terminal_resolution_with_failed_closed_fallback(context, outcome);
        }
    };
    commit_prepared_runtime_custom_tail(ctx, session, proxy_session_id, lease_id, preparation, now_ms, asset).await
}

pub async fn commit_prepared_runtime_custom_tail(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    preparation: &HlsTerminalTailPreparation,
    now_ms: u64,
    asset: HlsRuntimeCustomTailAsset,
) -> HlsTerminalResolution {
    let context = HlsTerminalCommitContext { ctx, session, proxy_session_id, lease_id, preparation, now_ms };
    if preparation.trigger.reason() != asset.reason {
        return HlsTerminalResolution::Reevaluate;
    }
    let expected_asset = HlsRuntimeCustomTailAssetIdentity::from_asset(&asset);
    let media_asset = asset.asset;
    let static_incompatibility = if preparation.manifest_snapshot.delivery_mode
        == crate::media_reserve::HlsManifestDeliveryMode::TransientPassthrough
    {
        Some(HlsTerminalTailCompatibility::TransientPassthroughUnsupported)
    } else if preparation.manifest_snapshot.active_map.is_some() {
        Some(HlsTerminalTailCompatibility::ActiveMapRequiresCompatibleFallback)
    } else if preparation.manifest_snapshot.container != HlsMediaContainer::MpegTs {
        Some(HlsTerminalTailCompatibility::ContainerMismatch)
    } else {
        None
    };
    if let Some(reason) = static_incompatibility {
        let outcome = commit_terminal_unavailable(context, Some(expected_asset), reason);
        return terminal_resolution_with_failed_closed_fallback(context, outcome);
    }
    let bundle_key = prepared_terminal_bundle_key(
        &media_asset,
        preparation.manifest_snapshot.target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    );
    let bundle_state = ctx.hls_proxy.prepared_terminal_bundle_state(bundle_key).unwrap_or_else(|| {
        ctx.hls_proxy.start_prepared_terminal_bundle(
            Arc::clone(&media_asset),
            bundle_key.target_duration_ms,
            bundle_key.segment_count,
        )
    });
    let prepared_bundle = match bundle_state {
        HlsPreparedTerminalBundleState::Ready { bundle } if bundle.matches_key_and_shape(bundle_key) => bundle,
        HlsPreparedTerminalBundleState::Ready { .. } => {
            let outcome = commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::AssetRevisionMismatch,
            );
            return terminal_resolution_with_failed_closed_fallback(context, outcome);
        }
        HlsPreparedTerminalBundleState::Preparing { .. } => {
            return register_terminal_pending_owner(context, media_asset, expected_asset, bundle_key).await;
        }
        HlsPreparedTerminalBundleState::Failed { key, reason } => {
            let compatibility = terminal_bundle_failure_compatibility(key, bundle_key, reason);
            let outcome = commit_terminal_unavailable(context, Some(expected_asset), compatibility);
            return terminal_resolution_with_failed_closed_fallback(context, outcome);
        }
        HlsPreparedTerminalBundleState::Incompatible { reason, .. } => {
            let reason = terminal_bundle_incompatibility(reason);
            let outcome = commit_terminal_unavailable(context, Some(expected_asset), reason);
            return terminal_resolution_with_failed_closed_fallback(context, outcome);
        }
    };
    if preparation.trigger.is_runtime_policy() {
        return register_ready_terminal_pending_owner(
            context,
            media_asset,
            expected_asset,
            bundle_key,
            prepared_bundle,
        );
    }
    let outcome = commit_ready_terminal_bundle(context, media_asset, expected_asset, bundle_key, prepared_bundle).await;
    terminal_resolution_with_failed_closed_fallback(context, outcome)
}

struct HlsReadyTerminalSplice {
    base_evidence: HlsTerminalBaseEvidence,
    base_timing: HlsTerminalBaseTimingEvidence,
    terminal_splice_evidence: crate::HlsTsSpliceEvidence,
    anchored_bundle: Arc<HlsAnchoredTerminalBundle>,
}

async fn prepare_ready_terminal_splice(
    context: HlsTerminalCommitContext<'_>,
    asset: &Arc<crate::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    prepared_bundle: &Arc<HlsPreparedTerminalBundle>,
) -> Result<HlsReadyTerminalSplice, HlsTerminalCommitOutcome> {
    let base_evidence = prepare_terminal_base_evidence(
        context.session,
        context.ctx.hls_proxy.segment_cache(),
        &context.preparation.manifest_snapshot,
        context.now_ms,
    )
    .await;
    let Some(base_timing) = base_evidence.timing().cloned() else {
        debug!("HLS terminal TS splice unavailable: reason=missing-base-timestamp-anchor");
        return Err(commit_terminal_unavailable(
            context,
            Some(expected_asset),
            HlsTerminalTailCompatibility::MissingTimestampAnchor,
        ));
    };
    if base_evidence.track_base() != Some(&base_timing.base) {
        debug!("HLS terminal TS splice unavailable: reason=base-track-timing-identity-mismatch");
        return Err(commit_terminal_unavailable(
            context,
            Some(expected_asset),
            HlsTerminalTailCompatibility::InvalidTimestampTransition,
        ));
    }
    let Some(asset_profile) = asset.timestamp_profile() else {
        debug!("HLS terminal TS splice unavailable: reason=missing-terminal-asset-timestamp-profile");
        return Err(commit_terminal_unavailable(
            context,
            Some(expected_asset),
            HlsTerminalTailCompatibility::InvalidTimestampTransition,
        ));
    };
    let Some(splice_anchor) = HlsTsSpliceAnchor::between(base_timing.profile, asset_profile) else {
        debug!("HLS terminal TS splice unavailable: reason=invalid-modular-timestamp-transition");
        return Err(commit_terminal_unavailable(
            context,
            Some(expected_asset),
            HlsTerminalTailCompatibility::InvalidTimestampTransition,
        ));
    };
    let anchor_asset = Arc::clone(asset);
    let anchor_prepared_bundle = Arc::clone(prepared_bundle);
    let anchored_bundle = tokio::task::spawn_blocking(move || {
        anchor_prepared_terminal_bundle(&anchor_asset, &anchor_prepared_bundle, splice_anchor)
    })
    .await;
    let Ok(Ok(anchored_bundle)) = anchored_bundle else {
        debug!("HLS terminal TS splice unavailable: reason=terminal-byte-finalization-failed");
        return Err(commit_terminal_unavailable(
            context,
            Some(expected_asset),
            HlsTerminalTailCompatibility::InvalidTimestampTransition,
        ));
    };
    let Some(terminal_zero) = anchored_bundle.segments.first().filter(|segment| segment.index == 0) else {
        return Err(commit_terminal_unavailable(
            context,
            Some(expected_asset),
            HlsTerminalTailCompatibility::InvalidAsset,
        ));
    };
    let terminal_zero_bytes = u64::try_from(terminal_zero.bytes.len()).unwrap_or(u64::MAX);
    let terminal_evidence = crate::inspect_mpeg_ts_media_evidence_async(
        std::io::Cursor::new(terminal_zero.bytes.clone()),
        crate::HlsTsProbeProtection::Clear,
        crate::HlsTsProbeBudget {
            max_bytes: terminal_zero_bytes.saturating_add(1),
            max_packets: terminal_zero_bytes.saturating_add(187).saturating_div(188).saturating_add(1),
            ..crate::HlsTsProbeBudget::default()
        },
        asset.duration_ticks_90khz(),
    )
    .await;
    let terminal_splice_evidence = match terminal_evidence {
        Ok(evidence) => evidence.splice_evidence,
        Err(_) => {
            return Err(commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::InvalidAsset,
            ));
        }
    };
    debug!(
        "HLS terminal TS splice prepared: base_proxy_seq={} live_last_clock_90khz={} \
         terminal_first_clock_90khz={} timestamp_delta_90khz={} \
         segment_stride_ticks_90khz={} discontinuity=first-packet-per-pid",
        base_timing.base.proxy_seq,
        splice_anchor.live_last_clock,
        splice_anchor.terminal_first_clock,
        splice_anchor.timestamp_delta_ticks,
        prepared_bundle.source_asset_duration_ticks_90khz,
    );
    Ok(HlsReadyTerminalSplice { base_evidence, base_timing, terminal_splice_evidence, anchored_bundle })
}

pub(super) async fn commit_ready_terminal_bundle(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<crate::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    prepared_bundle: Arc<HlsPreparedTerminalBundle>,
) -> HlsTerminalCommitOutcome {
    let mut preparation = context.preparation.clone();
    if let Err(reason) = preparation.bind_ready_terminal_media_requirement(bundle_key) {
        return commit_terminal_unavailable(
            HlsTerminalCommitContext { preparation: &preparation, ..context },
            Some(expected_asset),
            reason,
        );
    }
    let context = HlsTerminalCommitContext { preparation: &preparation, ..context };
    let HlsReadyTerminalSplice { base_evidence, base_timing, terminal_splice_evidence, anchored_bundle } =
        match prepare_ready_terminal_splice(context, &asset, expected_asset, &prepared_bundle).await {
            Ok(splice) => splice,
            Err(outcome) => return outcome,
        };
    let plan = build_terminal_tail_plan(HlsTerminalTailBuildInput {
        generation: HlsTerminalTailGeneration(preparation.decision_generation),
        created_at_ms: context.now_ms,
        base_manifest: preparation.manifest_snapshot.clone(),
        base_availability: base_evidence.availability(),
        base_track_signature: base_evidence.track_signature(),
        base_splice_evidence: base_evidence.splice_evidence().cloned(),
        terminal_splice_evidence: Some(terminal_splice_evidence),
        base_timing: Some(base_timing),
        base_key_bindings: base_evidence.key_bindings(),
        expected_asset,
        asset,
        anchored_bundle,
    });
    match plan {
        Ok(plan) => {
            if preparation.required_terminal_media_key == Some(plan.media_preparation_key()) {
                let commit_now_ms = context.ctx.hls_proxy.terminal_commit_now_ms();
                let generation = plan.generation.0;
                let base_proxy_tail = plan.base_manifest.last_proxy_seq;
                let outcome = context.ctx.hls_proxy.commit_access_lease_terminal_if_generation_matches(
                    HlsTerminalCommitRequest {
                        session: context.session,
                        lease_id: context.lease_id,
                        proxy_session_id: context.proxy_session_id,
                        preparation: &preparation,
                        now_ms: commit_now_ms,
                        payload: HlsTerminalCommitPayload::Tail {
                            plan: Arc::new(plan),
                            media_guard: base_evidence.into_commit_guard(),
                        },
                        asset_revision_guard: terminal_asset_revision_guard(
                            context.ctx,
                            expected_asset.reason,
                            Some(expected_asset),
                        ),
                    },
                );
                if outcome == HlsTerminalCommitOutcome::Committed {
                    debug!(
                        "HLS runtime custom tail committed: proxy_session={} lease={} reason={} \
                         generation={} base_proxy_tail={} asset_revision={:016x}",
                        crate::safe_proxy_session_id(context.proxy_session_id),
                        crate::safe_hls_access_lease_id(context.lease_id),
                        expected_asset.reason.as_label(),
                        generation,
                        base_proxy_tail,
                        expected_asset.media.revision
                    );
                }
                outcome
            } else {
                commit_terminal_unavailable(
                    context,
                    Some(expected_asset),
                    HlsTerminalTailCompatibility::AssetRevisionMismatch,
                )
            }
        }
        Err(reason) => commit_terminal_unavailable(context, Some(expected_asset), reason),
    }
}

pub(super) fn terminal_bundle_incompatibility(
    reason: HlsPreparedTerminalBundleIncompatibility,
) -> HlsTerminalTailCompatibility {
    match reason {
        HlsPreparedTerminalBundleIncompatibility::TargetDurationExceeded { asset_ms, target_ms } => {
            HlsTerminalTailCompatibility::TargetDurationExceeded { asset_ms, target_ms }
        }
        HlsPreparedTerminalBundleIncompatibility::EmptySegmentSet
        | HlsPreparedTerminalBundleIncompatibility::ZeroTargetDuration => HlsTerminalTailCompatibility::InvalidAsset,
    }
}

pub(super) fn terminal_bundle_failure_compatibility(
    actual_key: HlsPreparedTerminalBundleKey,
    required_key: HlsPreparedTerminalBundleKey,
    reason: HlsPreparedTerminalBundleFailure,
) -> HlsTerminalTailCompatibility {
    if actual_key != required_key {
        return HlsTerminalTailCompatibility::AssetRevisionMismatch;
    }
    match reason {
        HlsPreparedTerminalBundleFailure::Build(build_error) => match build_error {
            HlsPreparedTerminalBundleBuildError::Incompatible(reason) => terminal_bundle_incompatibility(reason),
            HlsPreparedTerminalBundleBuildError::AssetIdentityMismatch
            | HlsPreparedTerminalBundleBuildError::PublishedBundleKeyMismatch => {
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            }
            HlsPreparedTerminalBundleBuildError::TimestampOffsetOverflow { .. }
            | HlsPreparedTerminalBundleBuildError::FiniteSegmentRender(
                tuliprox_mpegts::transport_stream_buffer::HlsFiniteTsRenderError::InvalidAsset
                | tuliprox_mpegts::transport_stream_buffer::HlsFiniteTsRenderError::PreparedLayoutMismatch,
            )
            | HlsPreparedTerminalBundleBuildError::PublishedBundleShapeMismatch => {
                HlsTerminalTailCompatibility::InvalidAsset
            }
        },
        HlsPreparedTerminalBundleFailure::WorkerJoin
        | HlsPreparedTerminalBundleFailure::RuntimeUnavailable
        | HlsPreparedTerminalBundleFailure::PreparationCapacityExceeded
        | HlsPreparedTerminalBundleFailure::ByteCapacityExceeded { .. }
        | HlsPreparedTerminalBundleFailure::BundleSizeOverflow
        | HlsPreparedTerminalBundleFailure::GenerationExhausted => HlsTerminalTailCompatibility::TerminalMediaNotReady,
    }
}
