//! How hard the session is currently trying to recover, aggregated across leases.
//!
//! Each lease contributes evidence - how long it has been waiting, what it is
//! waiting for - and the aggregate decides the recovery trigger budget and the
//! timing seed the acceptance episode starts from. Committed back onto the
//! session under a CAS loop, so a concurrent refresh cannot lose the decision.

use super::{hls_recovery_timing_policy, HLS_PLAYBACK_RATE_GUARD_MILLI};
use crate::{
    hls_ctx::HlsCtx,
    lease::HlsAccessLeaseStore,
    manager::HlsProxyManager,
    media_reserve::{evaluate_lease_reserve, HlsLeaseReserveInput, HlsLeaseReserveSnapshot},
    observability::{HlsRecoveryAvailabilityLogEvidence, HlsRecoveryTriggerDiagnostic, HlsRecoveryTriggerSource},
    origin_progress::{
        evaluate_origin_progress, publication_late_after_ms, HlsOriginPathCondition, HlsOriginProgressDecision,
        HlsOriginProgressPhase, HlsOriginProgressSnapshot,
    },
    prepared_terminal_bundle::{prepared_terminal_bundle_key, HlsPreparedTerminalBundleState},
    recovery_timing::{
        HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput, HlsAcceptanceEpisodeTimingSeed,
        HlsLatestSafeTerminalCommitAtMs, HlsLeaseCutoverTiming, HlsObservedRecoveryLatency, HlsRecoveryTimingPolicy,
        HlsRecoveryTriggerBudgetMs, HlsRecoveryWorkload, HlsRecoveryWorkloadEnvelope, HlsTerminalMediaPreparationState,
        HlsTransitionMarginMs,
    },
    terminal_tail::{snapshot_terminal_media_asset, HLS_TERMINAL_TAIL_SEGMENT_COUNT},
    HlsAccessLeaseId, ProxySessionId,
};
use tuliprox_core::model::is_custom_video_stream_enabled;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct HlsRecoveryBoundarySlackMs(i128);

impl HlsRecoveryBoundarySlackMs {
    pub(super) fn from_reserve_and_boundary(guaranteed_reserve_ms: u64, recovery_boundary_ms: u64) -> Self {
        Self(i128::from(guaranteed_reserve_ms) - i128::from(recovery_boundary_ms))
    }
}

#[derive(Clone)]
pub(super) struct HlsLeaseRecoveryEvidence {
    pub(super) lease_id: HlsAccessLeaseId,
    pub(super) reserve: HlsLeaseReserveSnapshot,
    pub(super) cursor: crate::media_reserve::HlsLeasePlaybackCursor,
    pub(super) workload: HlsRecoveryWorkload,
    pub(super) target_duration_ms: u64,
    pub(super) latest_safe_terminal_commit_at: HlsLatestSafeTerminalCommitAtMs,
    pub(super) recovery_boundary_slack_ms: HlsRecoveryBoundarySlackMs,
}

pub(super) struct HlsSessionRecoveryPressure {
    pub(super) any_recovery_required: bool,
    pub(super) any_cutover_required: bool,
    pub(super) controlling: HlsLeaseRecoveryEvidence,
}

pub(super) struct HlsRecoveryPressureDecision {
    pub(super) decision: HlsOriginProgressDecision,
    pub(super) timing_seed: HlsAcceptanceEpisodeTimingSeed,
    pub(super) diagnostic: HlsRecoveryTriggerDiagnostic,
}

#[derive(Clone, Copy)]
pub(super) struct HlsRecoveryPressurePolicy {
    pub(super) burst_plan: shared::model::HlsManifestRecoveryBurstPlan,
    pub(super) timing: HlsRecoveryTimingPolicy,
}

pub(super) fn recovery_trigger_budget(
    policy: HlsRecoveryPressurePolicy,
    target_duration_ms: u64,
    workload: HlsRecoveryWorkload,
    observed_latency: HlsObservedRecoveryLatency,
) -> HlsRecoveryTriggerBudgetMs {
    HlsAcceptanceEpisodeTiming::from_input(&HlsAcceptanceEpisodeTimingInput {
        started_at_ms: 0,
        burst_plan: policy.burst_plan,
        target_duration_ms,
        transition_margin: HlsTransitionMarginMs::from_millis(target_duration_ms),
        workload,
        observed_latency,
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
        policy: policy.timing,
    })
    .trigger_budget
}

pub(super) fn hls_recovery_pressure_policy(
    manager: &HlsProxyManager,
    origin_manifest_timeout_ms: u64,
) -> HlsRecoveryPressurePolicy {
    HlsRecoveryPressurePolicy {
        burst_plan: manager.manifest_recovery_burst().level.plan(),
        timing: hls_recovery_timing_policy(manager, origin_manifest_timeout_ms),
    }
}

pub(super) fn aggregate_session_recovery_pressure(
    evidence: impl IntoIterator<Item = HlsLeaseRecoveryEvidence>,
) -> Option<HlsSessionRecoveryPressure> {
    let mut evidence = evidence.into_iter();
    let first = evidence.next()?;
    let mut any_recovery_required = first.reserve.recovery_required;
    let mut any_cutover_required = first.reserve.cutover_required;
    let mut controlling = first;
    for candidate in evidence {
        any_recovery_required |= candidate.reserve.recovery_required;
        any_cutover_required |= candidate.reserve.cutover_required;
        if lease_recovery_pressure_cmp(&candidate, &controlling).is_lt() {
            controlling = candidate;
        }
    }
    Some(HlsSessionRecoveryPressure { any_recovery_required, any_cutover_required, controlling })
}

fn lease_recovery_pressure_cmp(
    left: &HlsLeaseRecoveryEvidence,
    right: &HlsLeaseRecoveryEvidence,
) -> std::cmp::Ordering {
    right
        .reserve
        .cutover_required
        .cmp(&left.reserve.cutover_required)
        .then_with(|| right.reserve.recovery_required.cmp(&left.reserve.recovery_required))
        .then_with(|| left.latest_safe_terminal_commit_at.cmp(&right.latest_safe_terminal_commit_at))
        .then_with(|| left.recovery_boundary_slack_ms.cmp(&right.recovery_boundary_slack_ms))
        .then_with(|| left.lease_id.0.cmp(&right.lease_id.0))
}

pub(super) fn acceptance_timing_seed_for_pressure(
    controlling: &HlsLeaseRecoveryEvidence,
) -> HlsAcceptanceEpisodeTimingSeed {
    HlsAcceptanceEpisodeTimingSeed {
        target_duration_ms: controlling.target_duration_ms,
        transition_margin: controlling.reserve.transition_margin,
        workload: controlling.workload,
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
    }
}

pub(super) fn terminal_media_timing_seed(
    ctx: &HlsCtx,
    target_duration_ms: u64,
) -> (Option<crate::recovery_timing::HlsTerminalMediaPreparationKey>, HlsTerminalMediaPreparationState) {
    if !is_custom_video_stream_enabled(&ctx.app_config) {
        return (None, HlsTerminalMediaPreparationState::Failed { key: None });
    }
    let responses = ctx.app_config.custom_stream_response.load_full();
    let Some(buffer) = responses.as_ref().and_then(|responses| responses.channel_unavailable.as_ref()) else {
        return (None, HlsTerminalMediaPreparationState::Failed { key: None });
    };
    let Ok(asset) = snapshot_terminal_media_asset(buffer) else {
        return (None, HlsTerminalMediaPreparationState::Incompatible { key: None });
    };
    let key = prepared_terminal_bundle_key(&asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
    let preparation = match ctx.hls_proxy.start_prepared_terminal_bundle(
        asset,
        target_duration_ms,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    ) {
        HlsPreparedTerminalBundleState::Ready { bundle } => HlsTerminalMediaPreparationState::Ready { key: bundle.key },
        HlsPreparedTerminalBundleState::Preparing { key } => HlsTerminalMediaPreparationState::Preparing { key },
        HlsPreparedTerminalBundleState::Failed { key, .. } => {
            HlsTerminalMediaPreparationState::Failed { key: Some(key) }
        }
        HlsPreparedTerminalBundleState::Incompatible { key, .. } => {
            HlsTerminalMediaPreparationState::Incompatible { key: Some(key) }
        }
    };
    (Some(key), preparation)
}

#[allow(clippy::too_many_lines)]
pub(super) fn evaluate_and_commit_session_recovery_pressure(
    leases: &mut HlsAccessLeaseStore,
    session: &mut crate::HlsSession,
    proxy_session_id: &ProxySessionId,
    now_ms: u64,
    policy: HlsRecoveryPressurePolicy,
) -> Option<HlsRecoveryPressureDecision> {
    if session.proxy_session_id != *proxy_session_id {
        return None;
    }
    let active_leases = leases.active_live_playback_snapshots_for_session(proxy_session_id, now_ms);
    let fallback_target_duration_ms = active_leases
        .iter()
        .filter_map(|lease| lease.last_manifest_snapshot.as_ref().map(|manifest| manifest.target_duration_ms))
        .min()?;
    let publication_target_duration_ms = session
        .origin_control
        .target_duration_snapshot_ms
        .or_else(|| session.target_duration.map(|seconds| u64::from(seconds).saturating_mul(1_000)))
        .unwrap_or(fallback_target_duration_ms);
    let publication_late = session.origin_control.last_media_progress_at_ms.is_some_and(|last_progress_at_ms| {
        now_ms.saturating_sub(last_progress_at_ms) >= publication_late_after_ms(publication_target_duration_ms)
    });
    let effective_origin_degraded = session.origin_control.path_condition.is_degraded() || publication_late;
    let observed_latency = session.origin_control.recovery_samples.latency_snapshot();
    let workload = HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling();
    let pressure = aggregate_session_recovery_pressure(active_leases.iter().filter_map(|lease| {
        let manifest = lease.last_manifest_snapshot.as_ref()?;
        let ready_timeline = session.ready_timeline_snapshot(
            lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
            now_ms,
        );
        let recovery_trigger_budget =
            recovery_trigger_budget(policy, manifest.target_duration_ms, workload, observed_latency);
        let reserve = evaluate_lease_reserve(HlsLeaseReserveInput {
            manifest,
            cursor: &lease.playback_cursor,
            ready_timeline: &ready_timeline,
            now_ms,
            playback_rate_guard_milli: HLS_PLAYBACK_RATE_GUARD_MILLI,
            recovery_trigger_budget,
            origin_path_degraded: effective_origin_degraded,
            recovery_committed: false,
        });
        let recovery_boundary_ms =
            recovery_trigger_budget.as_millis().saturating_add(reserve.transition_margin.as_millis());
        let cutover_timing =
            HlsLeaseCutoverTiming::from_reserve(now_ms, reserve.guaranteed_reserve_ms, reserve.transition_margin, None);
        Some(HlsLeaseRecoveryEvidence {
            lease_id: lease.lease_id.clone(),
            reserve,
            cursor: lease.playback_cursor.clone(),
            workload,
            target_duration_ms: manifest.target_duration_ms,
            latest_safe_terminal_commit_at: cutover_timing.latest_safe_terminal_commit_at,
            recovery_boundary_slack_ms: HlsRecoveryBoundarySlackMs::from_reserve_and_boundary(
                reserve.guaranteed_reserve_ms,
                recovery_boundary_ms,
            ),
        })
    }))?;
    let progress_phase_before = session.origin_control.progress_phase;
    let progress_condition_before = session.origin_control.path_condition;
    let decision = evaluate_origin_progress(HlsOriginProgressSnapshot {
        phase: progress_phase_before,
        condition: progress_condition_before,
        target_duration_ms: publication_target_duration_ms,
        last_media_progress_at_ms: session.origin_control.last_media_progress_at_ms,
        session_recovery_required: pressure.any_recovery_required,
        session_cutover_evaluation_required: pressure.any_cutover_required,
        recovery_committed: false,
        now_ms,
    });
    session.origin_control.progress_phase = decision.next_phase;
    if session.origin_control.path_condition == HlsOriginPathCondition::ProgressExpected
        && matches!(
            decision.next_phase,
            HlsOriginProgressPhase::PublicationLate
                | HlsOriginProgressPhase::RecoveryRequired
                | HlsOriginProgressPhase::Critical
        )
    {
        session.origin_control.path_condition = HlsOriginPathCondition::PublicationLate;
    }
    let controlling = pressure.controlling;
    let trigger_source = recovery_trigger_source(
        progress_condition_before,
        publication_late,
        pressure.any_recovery_required,
        pressure.any_cutover_required,
    );
    let diagnostic = HlsRecoveryTriggerDiagnostic::availability(
        trigger_source,
        HlsRecoveryAvailabilityLogEvidence {
            progress_phase_before,
            progress_condition_before,
            progress_phase_after: session.origin_control.progress_phase,
            progress_condition_after: session.origin_control.path_condition,
            controlling_lease_id: controlling.lease_id.clone(),
            cursor: controlling.cursor.clone(),
            guaranteed_reserve_ms: controlling.reserve.guaranteed_reserve_ms,
            recovery_required: controlling.reserve.recovery_required,
            cutover_required: controlling.reserve.cutover_required,
        },
    );
    Some(HlsRecoveryPressureDecision {
        decision,
        timing_seed: acceptance_timing_seed_for_pressure(&controlling),
        diagnostic,
    })
}

pub(super) const fn recovery_trigger_source(
    condition: HlsOriginPathCondition,
    publication_late: bool,
    recovery_required: bool,
    cutover_required: bool,
) -> HlsRecoveryTriggerSource {
    if recovery_required || cutover_required {
        return HlsRecoveryTriggerSource::ReservePressure;
    }
    if publication_late || matches!(condition, HlsOriginPathCondition::PublicationLate) {
        return HlsRecoveryTriggerSource::PublicationLate;
    }
    match condition {
        HlsOriginPathCondition::HardFetchFailure => HlsRecoveryTriggerSource::HardFetchFailure,
        HlsOriginPathCondition::ProgressExpected
        | HlsOriginPathCondition::PublicationLate
        | HlsOriginPathCondition::RetryableFetchFailure
        | HlsOriginPathCondition::AcceptanceConflict
        | HlsOriginPathCondition::SegmentReadinessFailure => HlsRecoveryTriggerSource::Other,
    }
}

pub(super) fn evaluate_and_commit_session_recovery_pressure_in_snapshot(
    leases: &mut HlsAccessLeaseStore,
    session: &mut crate::HlsSession,
    proxy_session_id: &ProxySessionId,
    policy: HlsRecoveryPressurePolicy,
    evaluation_clock: impl FnOnce() -> u64,
) -> Option<HlsRecoveryPressureDecision> {
    let evaluation_now_ms = evaluation_clock();
    evaluate_and_commit_session_recovery_pressure(leases, session, proxy_session_id, evaluation_now_ms, policy)
}
