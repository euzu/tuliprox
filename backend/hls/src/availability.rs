use super::{
    availability_reevaluation::{
        HlsAvailabilityOwnerHandoffDecision, HlsAvailabilityReevaluationFinishDecision,
        HlsAvailabilityReevaluationFinishReason, HlsAvailabilityReevaluationMode, HlsAvailabilityReevaluationOwnerKey,
        HlsAvailabilityReevaluationOwnership, HlsAvailabilityReevaluationRegistration, HlsRecoveryPressureGuard,
    },
    hls_ctx::HlsCtx,
    lease::{HlsAccessLease, HlsAccessLeaseStore, HlsTerminalTailPreparation},
    manager::{
        HlsCriticalHandoffStateAccess, HlsProxyManager, HlsTerminalCommitPayload, HlsTerminalCommitRequest,
        HlsTerminalTailPreparationRequest,
    },
    manifest_acceptance::HlsManifestAcceptanceTrigger,
    media_reserve::{
        evaluate_lease_reserve, evaluate_startup_admission, HlsLeaseManifestSnapshot, HlsLeaseReserveInput,
        HlsLeaseReserveSnapshot, HlsReadyTimelineSnapshot, HlsStartupAdmissionDecision, HlsStartupAdmissionInput,
        HlsStartupAdmissionOriginState,
    },
    observability::{HlsRecoveryAvailabilityLogEvidence, HlsRecoveryTriggerDiagnostic, HlsRecoveryTriggerSource},
    origin_progress::{
        evaluate_origin_progress, publication_late_after_ms, HlsOriginPathCondition, HlsOriginProgressDecision,
        HlsOriginProgressPhase, HlsOriginProgressSnapshot,
    },
    post_refresh_availability::{
        evaluate_active_terminal_leases_for_reevaluation, evaluate_owner_failure_fallback, live_reserve_deadline,
        wait_for_owner_resolution, HlsLiveReserveDeadline, HlsPostRefreshOwnerResolution,
        HlsPostRefreshOwnerWaitOutcome, HlsPostRefreshTerminalEvaluation,
    },
    prepared_terminal_bundle::{
        anchor_prepared_terminal_bundle, prepared_terminal_bundle_key, HlsAnchoredTerminalBundle,
        HlsPreparedTerminalBundle, HlsPreparedTerminalBundleBuildError, HlsPreparedTerminalBundleCompletion,
        HlsPreparedTerminalBundleFailure, HlsPreparedTerminalBundleIncompatibility, HlsPreparedTerminalBundleKey,
        HlsPreparedTerminalBundleObservation, HlsPreparedTerminalBundleState,
    },
    recovery_timing::{
        bounded_manifest_request_eta_ms, HlsAcceptanceEpisodeTiming, HlsAcceptanceEpisodeTimingInput,
        HlsAcceptanceEpisodeTimingSeed, HlsLatestSafeTerminalCommitAtMs, HlsLeaseCutoverTiming,
        HlsObservedRecoveryLatency, HlsOperationTimeoutMs, HlsRecoveryEtaMs, HlsRecoveryTimingPolicy,
        HlsRecoveryTriggerBudgetMs, HlsRecoveryWorkload, HlsRecoveryWorkloadEnvelope,
        HlsTerminalCommitAcquisitionBudgetMs, HlsTerminalCommitWindow, HlsTerminalMediaPreparationState,
        HlsTransitionMarginMs,
    },
    refresh::{
        maybe_trigger_origin_refresh_with_outcome, HlsOriginRefreshTriggerOutcome, HlsPostRefreshAvailabilityAction,
        OriginRefreshRequest,
    },
    runtime_custom_tail::{
        current_hls_runtime_custom_tail_identity, snapshot_hls_runtime_custom_tail_asset, HlsRuntimeCustomTailAsset,
        HlsRuntimeCustomTailAssetIdentity, HlsRuntimeCustomTailReason,
    },
    segment_fetcher::HlsSegmentFetchWorkload,
    session_store::HlsSessionHandle,
    terminal_commit::{HlsTerminalAssetRevisionGuard, HlsTerminalCommitOutcome},
    terminal_pending::{HlsTerminalPendingOwnerKey, HlsTerminalPendingOwnership, HlsTerminalPendingRegistration},
    terminal_tail::{
        build_terminal_tail_plan, prepare_terminal_base_evidence, snapshot_terminal_media_asset,
        terminal_media_asset_identity, HlsMediaContainer, HlsTerminalBaseEvidence, HlsTerminalBaseTimingEvidence,
        HlsTerminalTailBuildInput, HlsTerminalTailCompatibility, HlsTerminalTailGeneration,
        HLS_TERMINAL_TAIL_SEGMENT_COUNT,
    },
    HlsAccessLeaseId, ProxySessionId,
};
use log::{debug, warn};
use std::{future::Future, sync::Arc, time::Duration};
use tuliprox_core::model::is_custom_video_stream_enabled;
use tuliprox_mpegts::transport_stream_buffer::HlsTsSpliceAnchor;

pub const HLS_PLAYBACK_RATE_GUARD_MILLI: u16 = 1_050;
const HLS_AVAILABILITY_STATE_ACCESS_ATTEMPTS: usize = 3;
const HLS_AVAILABILITY_REEVALUATION_MAX_ATTEMPTS: u8 = 64;
const HLS_AVAILABILITY_REEVALUATION_DEADLINE_MS: u64 = 2_000;
const HLS_AVAILABILITY_REEVALUATION_MAX_BACKOFF_MS: u64 = 50;
const HLS_TERMINAL_PENDING_RETRY_AFTER_MS: u64 = 50;
const HLS_TERMINAL_ASSET_REVALIDATION_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct HlsRecoveryBoundarySlackMs(i128);

impl HlsRecoveryBoundarySlackMs {
    fn from_reserve_and_boundary(guaranteed_reserve_ms: u64, recovery_boundary_ms: u64) -> Self {
        Self(i128::from(guaranteed_reserve_ms) - i128::from(recovery_boundary_ms))
    }
}

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
struct HlsTerminalCommitContext<'a> {
    ctx: &'a HlsCtx,
    session: &'a HlsSessionHandle,
    proxy_session_id: &'a ProxySessionId,
    lease_id: &'a HlsAccessLeaseId,
    preparation: &'a HlsTerminalTailPreparation,
    now_ms: u64,
}

#[derive(Clone)]
struct HlsLeaseRecoveryEvidence {
    lease_id: HlsAccessLeaseId,
    reserve: HlsLeaseReserveSnapshot,
    cursor: super::media_reserve::HlsLeasePlaybackCursor,
    workload: HlsRecoveryWorkload,
    target_duration_ms: u64,
    latest_safe_terminal_commit_at: HlsLatestSafeTerminalCommitAtMs,
    recovery_boundary_slack_ms: HlsRecoveryBoundarySlackMs,
}

struct HlsSessionRecoveryPressure {
    any_recovery_required: bool,
    any_cutover_required: bool,
    controlling: HlsLeaseRecoveryEvidence,
}

struct HlsRecoveryPressureDecision {
    decision: HlsOriginProgressDecision,
    timing_seed: HlsAcceptanceEpisodeTimingSeed,
    diagnostic: HlsRecoveryTriggerDiagnostic,
}

#[derive(Clone, Copy)]
struct HlsRecoveryPressurePolicy {
    burst_plan: shared::model::HlsManifestRecoveryBurstPlan,
    timing: HlsRecoveryTimingPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsManifestAcceptanceDirective {
    pub trigger: HlsManifestAcceptanceTrigger,
    pub timing_seed: Option<HlsAcceptanceEpisodeTimingSeed>,
    pub recovery_pressure_guard: Option<HlsRecoveryPressureGuard>,
    pub recovery_diagnostic: Option<HlsRecoveryTriggerDiagnostic>,
}

impl HlsManifestAcceptanceDirective {
    pub const fn none() -> Self {
        Self {
            trigger: HlsManifestAcceptanceTrigger::None,
            timing_seed: None,
            recovery_pressure_guard: None,
            recovery_diagnostic: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
#[allow(clippy::large_enum_variant)]
pub enum HlsManifestAcceptanceEvaluationOutcome {
    Evaluated(HlsManifestAcceptanceDirective),
    StateContention { owner_key: HlsAvailabilityReevaluationOwnerKey },
    SessionSuperseded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HlsAvailabilityReevaluationAttempt {
    Evaluated(Box<HlsManifestAcceptanceDirective>),
    StateContention,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsAvailabilitySnapshotAccessError {
    StateContention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlsTerminalFailedClosedReason {
    LeaseStateUnavailable,
    BundleNotReadyWithoutOwner,
    BundleIncompatible,
    SafeCommitDeadlineElapsed,
    RetryCapacityExceeded,
    RetryAttemptsExhausted,
    RuntimeUnavailable,
}

impl HlsTerminalFailedClosedReason {
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::LeaseStateUnavailable => "lease_state_unavailable",
            Self::BundleNotReadyWithoutOwner => "bundle_not_ready_without_owner",
            Self::BundleIncompatible => "bundle_incompatible",
            Self::SafeCommitDeadlineElapsed => "safe_commit_deadline_elapsed",
            Self::RetryCapacityExceeded => "retry_capacity_exceeded",
            Self::RetryAttemptsExhausted => "retry_attempts_exhausted",
            Self::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

/// Exhaustive endpoint-facing result of one lease-local terminal decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum HlsTerminalResolution {
    LiveAllowed,
    Committed,
    Reevaluate,
    Pending { retry_after_ms: u64 },
    FailedClosed { reason: HlsTerminalFailedClosedReason },
}

fn acceptance_directive_for_progress(
    decision: HlsOriginProgressDecision,
    timing_seed: HlsAcceptanceEpisodeTimingSeed,
    recovery_pressure_guard: Option<HlsRecoveryPressureGuard>,
    recovery_diagnostic: HlsRecoveryTriggerDiagnostic,
) -> HlsManifestAcceptanceDirective {
    let trigger = if decision.evaluate_lease_cutovers {
        HlsManifestAcceptanceTrigger::Critical
    } else if decision.close_admission {
        HlsManifestAcceptanceTrigger::RecoveryRequired
    } else if decision.start_acceptance_episode {
        HlsManifestAcceptanceTrigger::Observe
    } else {
        HlsManifestAcceptanceTrigger::None
    };
    // Lease evidence belongs to the refresh that observed it, even when the
    // origin-progress decision itself does not open an episode. A hard failure
    // later in that same refresh must not reconstruct timing from newer state.
    HlsManifestAcceptanceDirective {
        trigger,
        timing_seed: Some(timing_seed),
        recovery_pressure_guard,
        recovery_diagnostic: Some(recovery_diagnostic),
    }
}

pub fn hls_recovery_timing_policy(
    manager: &HlsProxyManager,
    origin_manifest_timeout_ms: u64,
) -> HlsRecoveryTimingPolicy {
    let segment_policy = manager.segment_fetch_policy();
    HlsRecoveryTimingPolicy::new(
        HlsOperationTimeoutMs::from_millis(origin_manifest_timeout_ms),
        HlsOperationTimeoutMs::from_millis(
            segment_policy.workload_budget_ms(HlsSegmentFetchWorkload::EncryptedWithKey),
        ),
        HlsRecoveryEtaMs::from_millis(bounded_manifest_request_eta_ms(origin_manifest_timeout_ms)),
        HlsRecoveryEtaMs::from_millis(segment_policy.recovery_object_eta_ms()),
    )
}

fn recovery_trigger_budget(
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

fn hls_recovery_pressure_policy(
    manager: &HlsProxyManager,
    origin_manifest_timeout_ms: u64,
) -> HlsRecoveryPressurePolicy {
    HlsRecoveryPressurePolicy {
        burst_plan: manager.manifest_recovery_burst().level.plan(),
        timing: hls_recovery_timing_policy(manager, origin_manifest_timeout_ms),
    }
}

fn aggregate_session_recovery_pressure(
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

fn acceptance_timing_seed_for_pressure(controlling: &HlsLeaseRecoveryEvidence) -> HlsAcceptanceEpisodeTimingSeed {
    HlsAcceptanceEpisodeTimingSeed {
        target_duration_ms: controlling.target_duration_ms,
        transition_margin: controlling.reserve.transition_margin,
        workload: controlling.workload,
        required_terminal_media_key: None,
        terminal_media_preparation: HlsTerminalMediaPreparationState::Failed { key: None },
    }
}

fn terminal_media_timing_seed(
    ctx: &HlsCtx,
    target_duration_ms: u64,
) -> (Option<super::recovery_timing::HlsTerminalMediaPreparationKey>, HlsTerminalMediaPreparationState) {
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

/// Evaluates startup admission from one authoritative lease manifest and one
/// immutable READY-timeline snapshot.
pub async fn hls_startup_admission_allows_snapshot(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    snapshot: &HlsLeaseManifestSnapshot,
    now_ms: u64,
) -> bool {
    let (ready_timeline, origin_state, observed_latency) = {
        let session = session.read().await;
        let degraded = session.origin_control.path_condition.is_degraded();
        (
            session.ready_timeline_snapshot(snapshot.first_proxy_seq, now_ms),
            if degraded { HlsStartupAdmissionOriginState::Degraded } else { HlsStartupAdmissionOriginState::Healthy },
            session.origin_control.recovery_samples.latency_snapshot(),
        )
    };
    let workload = HlsRecoveryWorkloadEnvelope::acceptance_policy().ceiling();
    let pressure_policy = hls_recovery_pressure_policy(&ctx.hls_proxy, ctx.hls_proxy.origin_manifest_timeout_ms());
    let admission = evaluate_startup_admission(HlsStartupAdmissionInput {
        manifest: snapshot,
        ready_timeline: &ready_timeline,
        origin_state,
        recovery_trigger_budget: recovery_trigger_budget(
            pressure_policy,
            snapshot.target_duration_ms,
            workload,
            observed_latency,
        ),
    });
    matches!(admission.decision, HlsStartupAdmissionDecision::Admit)
}

#[allow(clippy::too_many_lines)]
fn evaluate_and_commit_session_recovery_pressure(
    leases: &mut HlsAccessLeaseStore,
    session: &mut super::HlsSession,
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

const fn recovery_trigger_source(
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

fn evaluate_and_commit_session_recovery_pressure_in_snapshot(
    leases: &mut HlsAccessLeaseStore,
    session: &mut super::HlsSession,
    proxy_session_id: &ProxySessionId,
    policy: HlsRecoveryPressurePolicy,
    evaluation_clock: impl FnOnce() -> u64,
) -> Option<HlsRecoveryPressureDecision> {
    let evaluation_now_ms = evaluation_clock();
    evaluate_and_commit_session_recovery_pressure(leases, session, proxy_session_id, evaluation_now_ms, policy)
}

async fn retry_availability_state_access<T, Access, AccessFuture>(
    mut access: Access,
) -> HlsCriticalHandoffStateAccess<T>
where
    Access: FnMut() -> AccessFuture,
    AccessFuture: Future<Output = HlsCriticalHandoffStateAccess<T>>,
{
    for attempt in 0..HLS_AVAILABILITY_STATE_ACCESS_ATTEMPTS {
        match access().await {
            HlsCriticalHandoffStateAccess::Acquired(value) => {
                return HlsCriticalHandoffStateAccess::Acquired(value);
            }
            HlsCriticalHandoffStateAccess::LockBusy => {
                if attempt.saturating_add(1) < HLS_AVAILABILITY_STATE_ACCESS_ATTEMPTS {
                    tokio::task::yield_now().await;
                }
            }
        }
    }
    HlsCriticalHandoffStateAccess::LockBusy
}

fn availability_snapshot_or_contention<T>(
    access: HlsCriticalHandoffStateAccess<T>,
) -> Result<T, HlsAvailabilitySnapshotAccessError> {
    match access {
        HlsCriticalHandoffStateAccess::Acquired(value) => Ok(value),
        HlsCriticalHandoffStateAccess::LockBusy => Err(HlsAvailabilitySnapshotAccessError::StateContention),
    }
}

fn acceptance_directive_from_evidence(
    ctx: &HlsCtx,
    evidence: Option<HlsRecoveryPressureDecision>,
    recovery_pressure_guard: HlsRecoveryPressureGuard,
) -> HlsManifestAcceptanceDirective {
    let Some(HlsRecoveryPressureDecision { decision, mut timing_seed, diagnostic }) = evidence else {
        return HlsManifestAcceptanceDirective::none();
    };
    let (required_terminal_media_key, terminal_media_preparation) =
        terminal_media_timing_seed(ctx, timing_seed.target_duration_ms);
    timing_seed.required_terminal_media_key = required_terminal_media_key;
    timing_seed.terminal_media_preparation = terminal_media_preparation;
    acceptance_directive_for_progress(decision, timing_seed, Some(recovery_pressure_guard), diagnostic)
}

/// Computes and commits one shared origin-progress transition from an atomic
/// lease-store/session snapshot. Lock order is Lease Store -> Session and the
/// closure performs no network or filesystem I/O and contains no await. The
/// evaluation time is sampled only after both locks have been acquired.
pub async fn hls_manifest_acceptance_directive_for_session(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
) -> HlsManifestAcceptanceEvaluationOutcome {
    let Some(owner_key) = ctx.hls_proxy.availability_reevaluation_owner_key(session, proxy_session_id).await else {
        return HlsManifestAcceptanceEvaluationOutcome::SessionSuperseded;
    };
    let policy = hls_recovery_pressure_policy(&ctx.hls_proxy, ctx.hls_proxy.origin_manifest_timeout_ms());
    let state_access = retry_availability_state_access(|| {
        ctx.hls_proxy.with_critical_handoff_state(session, |leases, session| {
            let evidence = evaluate_and_commit_session_recovery_pressure_in_snapshot(
                leases,
                session,
                proxy_session_id,
                policy,
                current_time_millis,
            );
            let recovery_pressure_guard = HlsRecoveryPressureGuard {
                session_incarnation: owner_key.session_incarnation,
                proxy_session_id: session.proxy_session_id.clone(),
                origin_progress_generation: session.origin_control.progress_generation,
                media_readiness_generation: session.activity.media_readiness_generation,
                availability_evidence_generation: leases.availability_evidence_generation(proxy_session_id),
            };
            (evidence, recovery_pressure_guard)
        })
    })
    .await;
    let evidence = match availability_snapshot_or_contention(state_access) {
        Ok(evidence) => evidence,
        Err(HlsAvailabilitySnapshotAccessError::StateContention) => {
            return HlsManifestAcceptanceEvaluationOutcome::StateContention { owner_key };
        }
    };
    HlsManifestAcceptanceEvaluationOutcome::Evaluated(acceptance_directive_from_evidence(ctx, evidence.0, evidence.1))
}

async fn hls_manifest_acceptance_directive_for_reevaluation(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    owner_key: &HlsAvailabilityReevaluationOwnerKey,
) -> HlsAvailabilityReevaluationAttempt {
    if !ctx.hls_proxy.availability_reevaluation_session_is_current(session, owner_key).await {
        return HlsAvailabilityReevaluationAttempt::Superseded;
    }
    let policy = hls_recovery_pressure_policy(&ctx.hls_proxy, ctx.hls_proxy.origin_manifest_timeout_ms());
    let state_access = retry_availability_state_access(|| {
        ctx.hls_proxy.with_critical_handoff_state(session, |leases, session| {
            if session.proxy_session_id != owner_key.proxy_session_id
                || session.origin_control.progress_generation != owner_key.origin_progress_generation
                || session.activity.media_readiness_generation != owner_key.media_readiness_generation
                || leases.availability_evidence_generation(&owner_key.proxy_session_id)
                    != owner_key.availability_evidence_generation
                || session.is_gc_marked_for_removal()
            {
                return None;
            }
            let evidence = evaluate_and_commit_session_recovery_pressure_in_snapshot(
                leases,
                session,
                &owner_key.proxy_session_id,
                policy,
                current_time_millis,
            );
            if leases.availability_evidence_generation(&owner_key.proxy_session_id)
                != owner_key.availability_evidence_generation
            {
                return None;
            }
            Some((evidence, HlsRecoveryPressureGuard::from_owner_key(owner_key)))
        })
    })
    .await;
    let evidence = match availability_snapshot_or_contention(state_access) {
        Ok(Some(evidence)) => evidence,
        Ok(None) => return HlsAvailabilityReevaluationAttempt::Superseded,
        Err(HlsAvailabilitySnapshotAccessError::StateContention) => {
            return HlsAvailabilityReevaluationAttempt::StateContention;
        }
    };
    if !ctx.hls_proxy.availability_reevaluation_session_is_current(session, owner_key).await {
        return HlsAvailabilityReevaluationAttempt::Superseded;
    }
    HlsAvailabilityReevaluationAttempt::Evaluated(Box::new(acceptance_directive_from_evidence(
        ctx, evidence.0, evidence.1,
    )))
}

fn availability_reevaluation_backoff_ms(attempts_completed: u8) -> u64 {
    let exponent = u32::from(attempts_completed.saturating_sub(1)).min(31);
    1_u64.checked_shl(exponent).unwrap_or(u64::MAX).min(HLS_AVAILABILITY_REEVALUATION_MAX_BACKOFF_MS)
}

enum HlsAvailabilityWorkerHandoff {
    Continue { session: HlsSessionHandle, owner_key: HlsAvailabilityReevaluationOwnerKey },
    NoCurrentSession,
    Superseded,
}

async fn handoff_to_current_availability_reevaluation(
    ctx: &HlsCtx,
    stale_owner_key: &HlsAvailabilityReevaluationOwnerKey,
    ownership: &HlsAvailabilityReevaluationOwnership,
) -> HlsAvailabilityWorkerHandoff {
    let Some(current_session) =
        ctx.hls_proxy.sessions().get_by_proxy_session_id(&stale_owner_key.proxy_session_id).await
    else {
        return HlsAvailabilityWorkerHandoff::NoCurrentSession;
    };
    let Some(current_owner_key) =
        ctx.hls_proxy.availability_reevaluation_owner_key(&current_session, &stale_owner_key.proxy_session_id).await
    else {
        return HlsAvailabilityWorkerHandoff::NoCurrentSession;
    };
    match ownership.handoff_to(stale_owner_key, current_owner_key.clone()) {
        HlsAvailabilityOwnerHandoffDecision::Continue { owner_key } => {
            HlsAvailabilityWorkerHandoff::Continue { session: current_session, owner_key }
        }
        HlsAvailabilityOwnerHandoffDecision::AlreadyCurrent => {
            HlsAvailabilityWorkerHandoff::Continue { session: current_session, owner_key: current_owner_key }
        }
        HlsAvailabilityOwnerHandoffDecision::Superseded => HlsAvailabilityWorkerHandoff::Superseded,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsAvailabilityAttemptSchedule {
    Backoff,
    RefreshCompletion,
    DebouncedUntil { retry_at_ms: u64 },
}

impl HlsAvailabilityAttemptSchedule {
    fn wake_at_ms(self, now_ms: u64, attempts_completed: u8, cycle_deadline_ms: u64) -> Option<u64> {
        match self {
            Self::Backoff => {
                let retry_at_ms = now_ms.saturating_add(availability_reevaluation_backoff_ms(attempts_completed));
                (retry_at_ms <= cycle_deadline_ms).then_some(retry_at_ms)
            }
            Self::RefreshCompletion => Some(cycle_deadline_ms),
            Self::DebouncedUntil { retry_at_ms } => Some(retry_at_ms.min(cycle_deadline_ms)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsAvailabilityRefreshTriggerDecision {
    FinishCycle,
    Wait(HlsAvailabilityAttemptSchedule),
    Handoff,
}

const fn availability_refresh_trigger_decision(
    outcome: HlsOriginRefreshTriggerOutcome,
    terminal_evaluation_pending: bool,
) -> HlsAvailabilityRefreshTriggerDecision {
    match outcome {
        HlsOriginRefreshTriggerOutcome::Started if terminal_evaluation_pending => {
            HlsAvailabilityRefreshTriggerDecision::Wait(HlsAvailabilityAttemptSchedule::RefreshCompletion)
        }
        HlsOriginRefreshTriggerOutcome::Started => HlsAvailabilityRefreshTriggerDecision::FinishCycle,
        HlsOriginRefreshTriggerOutcome::InFlight => {
            HlsAvailabilityRefreshTriggerDecision::Wait(HlsAvailabilityAttemptSchedule::RefreshCompletion)
        }
        HlsOriginRefreshTriggerOutcome::DebouncedUntil { retry_at_ms } => {
            HlsAvailabilityRefreshTriggerDecision::Wait(HlsAvailabilityAttemptSchedule::DebouncedUntil { retry_at_ms })
        }
        HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded => HlsAvailabilityRefreshTriggerDecision::Handoff,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureStateContention
        | HlsOriginRefreshTriggerOutcome::SessionUnavailable => {
            HlsAvailabilityRefreshTriggerDecision::Wait(HlsAvailabilityAttemptSchedule::Backoff)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsAvailabilityWorkerDecision {
    ContinueAttempts(HlsAvailabilityAttemptSchedule),
    RestartCycle,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsAvailabilityOwnerWaitOutcome {
    Cancelled,
    Woken,
    DeadlineReached,
}

async fn wait_for_availability_owner_signal(
    ownership: &HlsAvailabilityReevaluationOwnership,
    wake_at_ms: u64,
) -> HlsAvailabilityOwnerWaitOutcome {
    tokio::select! {
        () = ownership.cancelled() => HlsAvailabilityOwnerWaitOutcome::Cancelled,
        () = ownership.wake_requested() => HlsAvailabilityOwnerWaitOutcome::Woken,
        () = tokio::time::sleep(Duration::from_millis(
            wake_at_ms.saturating_sub(current_time_millis())
        )) => HlsAvailabilityOwnerWaitOutcome::DeadlineReached,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HlsDetailedTerminalResolution {
    pub(super) resolution: HlsTerminalResolution,
    pub(super) live_reserve_deadline: Option<HlsLiveReserveDeadline>,
}

#[derive(Clone, Copy)]
pub(super) enum HlsTerminalDecisionPurpose {
    OrdinaryCutover,
    AutonomousOwnerFailureFallback,
}

impl HlsDetailedTerminalResolution {
    const fn resolved(resolution: HlsTerminalResolution) -> Self { Self { resolution, live_reserve_deadline: None } }

    const fn with_deadline(
        resolution: HlsTerminalResolution,
        live_reserve_deadline: Option<HlsLiveReserveDeadline>,
    ) -> Self {
        Self { resolution, live_reserve_deadline }
    }
}

fn finish_availability_reevaluation_cycle(
    ownership: &HlsAvailabilityReevaluationOwnership,
    owner_key: &HlsAvailabilityReevaluationOwnerKey,
    reason: HlsAvailabilityReevaluationFinishReason,
) -> HlsAvailabilityWorkerDecision {
    match ownership.finish_cycle(owner_key, reason) {
        HlsAvailabilityReevaluationFinishDecision::StartSuccessor => HlsAvailabilityWorkerDecision::RestartCycle,
        HlsAvailabilityReevaluationFinishDecision::Complete | HlsAvailabilityReevaluationFinishDecision::Superseded => {
            HlsAvailabilityWorkerDecision::Stop
        }
    }
}

async fn handoff_availability_reevaluation_worker(
    ctx: &HlsCtx,
    session: &mut HlsSessionHandle,
    owner_key: &mut HlsAvailabilityReevaluationOwnerKey,
    ownership: &HlsAvailabilityReevaluationOwnership,
    refresh_request: &mut OriginRefreshRequest,
) -> HlsAvailabilityWorkerDecision {
    match handoff_to_current_availability_reevaluation(ctx, owner_key, ownership).await {
        HlsAvailabilityWorkerHandoff::Continue { session: current_session, owner_key: current_owner_key } => {
            *session = current_session;
            *owner_key = current_owner_key;
            refresh_request.session = Arc::clone(session);
            HlsAvailabilityWorkerDecision::RestartCycle
        }
        HlsAvailabilityWorkerHandoff::NoCurrentSession | HlsAvailabilityWorkerHandoff::Superseded => {
            ownership.discard_superseded(owner_key);
            HlsAvailabilityWorkerDecision::Stop
        }
    }
}

async fn handle_evaluated_availability_directive(
    ctx: &HlsCtx,
    session: &mut HlsSessionHandle,
    owner_key: &mut HlsAvailabilityReevaluationOwnerKey,
    ownership: &HlsAvailabilityReevaluationOwnership,
    refresh_request: &mut OriginRefreshRequest,
    directive: HlsManifestAcceptanceDirective,
    terminal_evaluation_pending: bool,
) -> HlsAvailabilityWorkerDecision {
    if !directive.trigger.starts_episode() {
        if terminal_evaluation_pending {
            return HlsAvailabilityWorkerDecision::ContinueAttempts(HlsAvailabilityAttemptSchedule::Backoff);
        }
        return finish_availability_reevaluation_cycle(
            ownership,
            owner_key,
            HlsAvailabilityReevaluationFinishReason::Evaluated,
        );
    }
    if !ownership.is_current(owner_key) {
        ownership.discard_superseded(owner_key);
        return HlsAvailabilityWorkerDecision::Stop;
    }
    if !ctx.hls_proxy.availability_reevaluation_session_is_current(session, owner_key).await {
        return handoff_availability_reevaluation_worker(ctx, session, owner_key, ownership, refresh_request).await;
    }
    refresh_request.acceptance_directive = directive;
    refresh_request.now_ms = current_time_millis();
    let trigger_decision = availability_refresh_trigger_decision(
        maybe_trigger_origin_refresh_with_outcome(refresh_request.clone()).await,
        terminal_evaluation_pending,
    );
    match trigger_decision {
        HlsAvailabilityRefreshTriggerDecision::FinishCycle => finish_availability_reevaluation_cycle(
            ownership,
            owner_key,
            HlsAvailabilityReevaluationFinishReason::Evaluated,
        ),
        HlsAvailabilityRefreshTriggerDecision::Wait(schedule) => {
            HlsAvailabilityWorkerDecision::ContinueAttempts(schedule)
        }
        HlsAvailabilityRefreshTriggerDecision::Handoff => {
            handoff_availability_reevaluation_worker(ctx, session, owner_key, ownership, refresh_request).await
        }
    }
}

struct HlsAvailabilityReevaluationCycle {
    deadline_ms: u64,
    attempts_completed: u8,
    live_reserve_deadline: Option<HlsLiveReserveDeadline>,
    retain_post_refresh_owner: bool,
    failed_closed_retry: Option<HlsTerminalFailedClosedReason>,
}

impl HlsAvailabilityReevaluationCycle {
    fn new(now_ms: u64) -> Self {
        Self {
            deadline_ms: now_ms.saturating_add(HLS_AVAILABILITY_REEVALUATION_DEADLINE_MS),
            attempts_completed: 0,
            live_reserve_deadline: None,
            retain_post_refresh_owner: false,
            failed_closed_retry: None,
        }
    }

    fn retained_owner_resolution(&self, now_ms: u64) -> Option<HlsPostRefreshOwnerResolution> {
        if !self.retain_post_refresh_owner {
            return None;
        }
        let resolution = if let Some(reason) = self.failed_closed_retry {
            match self.live_reserve_deadline {
                Some(deadline) if now_ms < deadline.latest_safe_terminal_commit_at_ms.saturating_sub(1) => {
                    HlsPostRefreshOwnerResolution::RetryAfter {
                        at_ms: now_ms
                            .saturating_add(HLS_AVAILABILITY_REEVALUATION_MAX_BACKOFF_MS)
                            .min(deadline.next_reevaluation_at_ms),
                        reason,
                    }
                }
                Some(_) => HlsPostRefreshOwnerResolution::WaitForEvidence { reason },
                None => HlsPostRefreshOwnerResolution::RetryAfter {
                    at_ms: now_ms.saturating_add(HLS_AVAILABILITY_REEVALUATION_MAX_BACKOFF_MS),
                    reason,
                },
            }
        } else if let Some(deadline) = self.live_reserve_deadline {
            HlsPostRefreshOwnerResolution::WaitUntil(deadline)
        } else {
            HlsPostRefreshOwnerResolution::RetryAt {
                at_ms: now_ms.saturating_add(HLS_AVAILABILITY_REEVALUATION_MAX_BACKOFF_MS),
            }
        };
        Some(resolution)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsAvailabilityCycleDecision {
    ContinueAttempts(HlsAvailabilityAttemptSchedule),
    RestartCycle,
    FinishCycle,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsTerminalReevaluationDecision {
    Continue { terminal_evaluation_pending: bool },
    RestartCycle,
    Stop,
}

async fn handle_failed_closed_terminal_reevaluation(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    owner_key: &HlsAvailabilityReevaluationOwnerKey,
    ownership: &HlsAvailabilityReevaluationOwnership,
    cycle: &mut HlsAvailabilityReevaluationCycle,
    reason: HlsTerminalFailedClosedReason,
    now_ms: u64,
) -> HlsTerminalReevaluationDecision {
    let fallback = evaluate_owner_failure_fallback(ctx, session, &owner_key.proxy_session_id, now_ms).await;
    if fallback.is_complete() {
        return match finish_availability_reevaluation_cycle(
            ownership,
            owner_key,
            HlsAvailabilityReevaluationFinishReason::Evaluated,
        ) {
            HlsAvailabilityWorkerDecision::RestartCycle => HlsTerminalReevaluationDecision::RestartCycle,
            HlsAvailabilityWorkerDecision::Stop | HlsAvailabilityWorkerDecision::ContinueAttempts(_) => {
                HlsTerminalReevaluationDecision::Stop
            }
        };
    }
    log::error!(
        "HLS post-refresh terminal evaluation retained: proxy_session={} reason={} leases={} terminal_committed={} pending_owned={} recovered_live={} superseded={} unresolved={}",
        super::safe_proxy_session_id(&owner_key.proxy_session_id),
        reason.as_label(),
        fallback.total,
        fallback.terminal_committed,
        fallback.pending_owned,
        fallback.recovered_live,
        fallback.superseded,
        fallback.unresolved.len()
    );
    cycle.live_reserve_deadline = fallback.earliest_deadline;
    cycle.failed_closed_retry = Some(reason);
    cycle.retain_post_refresh_owner = true;
    HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending: true }
}

async fn evaluate_post_refresh_terminal_attempt(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    owner_key: &HlsAvailabilityReevaluationOwnerKey,
    ownership: &HlsAvailabilityReevaluationOwnership,
    mode: HlsAvailabilityReevaluationMode,
    cycle: &mut HlsAvailabilityReevaluationCycle,
    now_ms: u64,
) -> HlsTerminalReevaluationDecision {
    if !matches!(mode, HlsAvailabilityReevaluationMode::PostRefresh(_)) {
        return HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending: false };
    }
    match evaluate_active_terminal_leases_for_reevaluation(ctx, session, &owner_key.proxy_session_id, now_ms).await {
        HlsPostRefreshTerminalEvaluation::ReevaluateNow => {
            cycle.live_reserve_deadline = None;
            cycle.retain_post_refresh_owner = true;
            HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending: true }
        }
        HlsPostRefreshTerminalEvaluation::LiveReserveRemains(deadline) => {
            cycle.live_reserve_deadline = Some(deadline);
            cycle.retain_post_refresh_owner = true;
            HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending: true }
        }
        HlsPostRefreshTerminalEvaluation::FailedClosed(reason) => {
            handle_failed_closed_terminal_reevaluation(ctx, session, owner_key, ownership, cycle, reason, now_ms).await
        }
        HlsPostRefreshTerminalEvaluation::NoLiveLease
        | HlsPostRefreshTerminalEvaluation::RecoveredLive
        | HlsPostRefreshTerminalEvaluation::PendingOwnerRegistered => {
            match finish_availability_reevaluation_cycle(
                ownership,
                owner_key,
                HlsAvailabilityReevaluationFinishReason::Evaluated,
            ) {
                HlsAvailabilityWorkerDecision::RestartCycle => HlsTerminalReevaluationDecision::RestartCycle,
                HlsAvailabilityWorkerDecision::Stop | HlsAvailabilityWorkerDecision::ContinueAttempts(_) => {
                    HlsTerminalReevaluationDecision::Stop
                }
            }
        }
        HlsPostRefreshTerminalEvaluation::TerminalCommitted => {
            log::debug!(
                "HLS post-refresh terminal evaluation completed: proxy_session={} outcome=terminal_committed",
                super::safe_proxy_session_id(&owner_key.proxy_session_id)
            );
            match finish_availability_reevaluation_cycle(
                ownership,
                owner_key,
                HlsAvailabilityReevaluationFinishReason::Evaluated,
            ) {
                HlsAvailabilityWorkerDecision::RestartCycle => HlsTerminalReevaluationDecision::RestartCycle,
                HlsAvailabilityWorkerDecision::Stop | HlsAvailabilityWorkerDecision::ContinueAttempts(_) => {
                    HlsTerminalReevaluationDecision::Stop
                }
            }
        }
    }
}

async fn evaluate_availability_refresh_attempt(
    ctx: &HlsCtx,
    session: &mut HlsSessionHandle,
    owner_key: &mut HlsAvailabilityReevaluationOwnerKey,
    ownership: &HlsAvailabilityReevaluationOwnership,
    refresh_request: &mut OriginRefreshRequest,
    terminal_evaluation_pending: bool,
) -> HlsAvailabilityCycleDecision {
    let decision = match hls_manifest_acceptance_directive_for_reevaluation(ctx, session, owner_key).await {
        HlsAvailabilityReevaluationAttempt::Evaluated(directive) => {
            handle_evaluated_availability_directive(
                ctx,
                session,
                owner_key,
                ownership,
                refresh_request,
                *directive,
                terminal_evaluation_pending,
            )
            .await
        }
        HlsAvailabilityReevaluationAttempt::Superseded => {
            handoff_availability_reevaluation_worker(ctx, session, owner_key, ownership, refresh_request).await
        }
        HlsAvailabilityReevaluationAttempt::StateContention => {
            HlsAvailabilityWorkerDecision::ContinueAttempts(HlsAvailabilityAttemptSchedule::Backoff)
        }
    };
    match decision {
        HlsAvailabilityWorkerDecision::ContinueAttempts(schedule) => {
            HlsAvailabilityCycleDecision::ContinueAttempts(schedule)
        }
        HlsAvailabilityWorkerDecision::RestartCycle => HlsAvailabilityCycleDecision::RestartCycle,
        HlsAvailabilityWorkerDecision::Stop => HlsAvailabilityCycleDecision::Stop,
    }
}

async fn run_hls_availability_reevaluation_attempt(
    ctx: &HlsCtx,
    session: &mut HlsSessionHandle,
    owner_key: &mut HlsAvailabilityReevaluationOwnerKey,
    ownership: &HlsAvailabilityReevaluationOwnership,
    refresh_request: &mut OriginRefreshRequest,
    cycle: &mut HlsAvailabilityReevaluationCycle,
) -> HlsAvailabilityCycleDecision {
    if !ownership.is_current(owner_key) {
        ownership.discard_superseded(owner_key);
        return HlsAvailabilityCycleDecision::Stop;
    }
    let Some(mode) = ownership.current_mode(owner_key) else {
        ownership.discard_superseded(owner_key);
        return HlsAvailabilityCycleDecision::Stop;
    };
    let attempt_now_ms = current_time_millis();
    if attempt_now_ms > cycle.deadline_ms {
        warn!("HLS availability reevaluation cycle stopped: reason=deadline_elapsed");
        return HlsAvailabilityCycleDecision::FinishCycle;
    }
    let terminal_evaluation_pending =
        match evaluate_post_refresh_terminal_attempt(ctx, session, owner_key, ownership, mode, cycle, attempt_now_ms)
            .await
        {
            HlsTerminalReevaluationDecision::Continue { terminal_evaluation_pending } => terminal_evaluation_pending,
            HlsTerminalReevaluationDecision::RestartCycle => return HlsAvailabilityCycleDecision::RestartCycle,
            HlsTerminalReevaluationDecision::Stop => return HlsAvailabilityCycleDecision::Stop,
        };
    let schedule = match evaluate_availability_refresh_attempt(
        ctx,
        session,
        owner_key,
        ownership,
        refresh_request,
        terminal_evaluation_pending,
    )
    .await
    {
        HlsAvailabilityCycleDecision::ContinueAttempts(schedule) => schedule,
        decision => return decision,
    };
    cycle.attempts_completed = cycle.attempts_completed.saturating_add(1);
    if cycle.attempts_completed >= HLS_AVAILABILITY_REEVALUATION_MAX_ATTEMPTS {
        warn!("HLS availability reevaluation cycle stopped: reason=attempts_exhausted");
        return HlsAvailabilityCycleDecision::FinishCycle;
    }
    let Some(retry_at_ms) = schedule.wake_at_ms(current_time_millis(), cycle.attempts_completed, cycle.deadline_ms)
    else {
        warn!("HLS availability reevaluation cycle stopped: reason=deadline_elapsed");
        return HlsAvailabilityCycleDecision::FinishCycle;
    };
    match wait_for_availability_owner_signal(ownership, retry_at_ms).await {
        HlsAvailabilityOwnerWaitOutcome::Cancelled => {
            ownership.discard_superseded(owner_key);
            HlsAvailabilityCycleDecision::Stop
        }
        HlsAvailabilityOwnerWaitOutcome::Woken => HlsAvailabilityCycleDecision::RestartCycle,
        HlsAvailabilityOwnerWaitOutcome::DeadlineReached if retry_at_ms >= cycle.deadline_ms => {
            HlsAvailabilityCycleDecision::FinishCycle
        }
        HlsAvailabilityOwnerWaitOutcome::DeadlineReached => HlsAvailabilityCycleDecision::ContinueAttempts(schedule),
    }
}

async fn run_hls_availability_reevaluation(
    ctx: HlsCtx,
    mut session: HlsSessionHandle,
    mut owner_key: HlsAvailabilityReevaluationOwnerKey,
    ownership: HlsAvailabilityReevaluationOwnership,
    mut refresh_request: OriginRefreshRequest,
) {
    'cycles: loop {
        let mut cycle = HlsAvailabilityReevaluationCycle::new(current_time_millis());
        loop {
            match run_hls_availability_reevaluation_attempt(
                &ctx,
                &mut session,
                &mut owner_key,
                &ownership,
                &mut refresh_request,
                &mut cycle,
            )
            .await
            {
                HlsAvailabilityCycleDecision::ContinueAttempts(_) => {}
                HlsAvailabilityCycleDecision::RestartCycle => continue 'cycles,
                HlsAvailabilityCycleDecision::FinishCycle => break,
                HlsAvailabilityCycleDecision::Stop => return,
            }
        }
        if let Some(resolution) = cycle.retained_owner_resolution(current_time_millis()) {
            match wait_for_owner_resolution(&ownership, resolution).await {
                HlsPostRefreshOwnerWaitOutcome::Cancelled => {
                    ownership.discard_superseded(&owner_key);
                    return;
                }
                HlsPostRefreshOwnerWaitOutcome::Woken | HlsPostRefreshOwnerWaitOutcome::DeadlineReached => {}
            }
            continue;
        }
        match ownership.finish_cycle(&owner_key, HlsAvailabilityReevaluationFinishReason::CycleBudgetExhausted) {
            HlsAvailabilityReevaluationFinishDecision::StartSuccessor => {}
            HlsAvailabilityReevaluationFinishDecision::Complete
            | HlsAvailabilityReevaluationFinishDecision::Superseded => return,
        }
    }
}

pub async fn register_post_refresh_availability_reevaluation(
    ctx: HlsCtx,
    session: HlsSessionHandle,
    refresh_request: OriginRefreshRequest,
    action: HlsPostRefreshAvailabilityAction,
) -> HlsAvailabilityReevaluationRegistration {
    let HlsPostRefreshAvailabilityAction::Reevaluate { origin_progress_generation, media_readiness_generation, .. } =
        action
    else {
        return HlsAvailabilityReevaluationRegistration::Superseded;
    };
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let Some(owner_key) = ctx.hls_proxy.availability_reevaluation_owner_key(&session, &proxy_session_id).await else {
        return HlsAvailabilityReevaluationRegistration::Superseded;
    };
    let path_degraded = session.read().await.origin_control.path_condition.is_degraded();
    // READY materialization may advance after the failure snapshot without
    // superseding the degraded origin evidence. The retained owner binds to
    // the newer readiness generation and recomputes every lease reserve.
    if owner_key.origin_progress_generation != origin_progress_generation
        || owner_key.media_readiness_generation < media_readiness_generation
        || !path_degraded
    {
        return HlsAvailabilityReevaluationRegistration::Superseded;
    }
    let Some(reason) = action.reason() else {
        return HlsAvailabilityReevaluationRegistration::Superseded;
    };
    register_hls_availability_reevaluation_with_mode(
        ctx,
        session,
        owner_key,
        refresh_request,
        HlsAvailabilityReevaluationMode::PostRefresh(reason),
    )
}

pub fn register_hls_availability_reevaluation(
    ctx: HlsCtx,
    session: HlsSessionHandle,
    owner_key: HlsAvailabilityReevaluationOwnerKey,
    refresh_request: OriginRefreshRequest,
) -> HlsAvailabilityReevaluationRegistration {
    register_hls_availability_reevaluation_with_mode(
        ctx,
        session,
        owner_key,
        refresh_request,
        HlsAvailabilityReevaluationMode::RecoveryPressure,
    )
}

fn register_hls_availability_reevaluation_with_mode(
    ctx: HlsCtx,
    session: HlsSessionHandle,
    owner_key: HlsAvailabilityReevaluationOwnerKey,
    refresh_request: OriginRefreshRequest,
    mode: HlsAvailabilityReevaluationMode,
) -> HlsAvailabilityReevaluationRegistration {
    let coordinator = ctx.hls_proxy.availability_reevaluations();
    let registration_key = owner_key.clone();
    coordinator.register(registration_key, mode, move |ownership| {
        run_hls_availability_reevaluation(ctx, session, owner_key, ownership, refresh_request)
    })
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

pub(super) async fn commit_terminal_tail_if_lease_reserve_requires_cutover_detailed(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease: &HlsAccessLease,
    now_ms: u64,
    purpose: HlsTerminalDecisionPurpose,
) -> HlsDetailedTerminalResolution {
    if lease.playback_mode != super::terminal_tail::HlsLeasePlaybackMode::Live {
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
        == super::media_reserve::HlsManifestDeliveryMode::TransientPassthrough
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
    terminal_splice_evidence: super::HlsTsSpliceEvidence,
    anchored_bundle: Arc<HlsAnchoredTerminalBundle>,
}

async fn prepare_ready_terminal_splice(
    context: HlsTerminalCommitContext<'_>,
    asset: &Arc<super::terminal_tail::HlsTerminalMediaAsset>,
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
    let terminal_evidence = super::inspect_mpeg_ts_media_evidence_async(
        std::io::Cursor::new(terminal_zero.bytes.clone()),
        super::HlsTsProbeProtection::Clear,
        super::HlsTsProbeBudget {
            max_bytes: terminal_zero_bytes.saturating_add(1),
            max_packets: terminal_zero_bytes.saturating_add(187).saturating_div(188).saturating_add(1),
            ..super::HlsTsProbeBudget::default()
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

async fn commit_ready_terminal_bundle(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<super::terminal_tail::HlsTerminalMediaAsset>,
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
                        super::safe_proxy_session_id(context.proxy_session_id),
                        super::safe_hls_access_lease_id(context.lease_id),
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

fn terminal_bundle_incompatibility(reason: HlsPreparedTerminalBundleIncompatibility) -> HlsTerminalTailCompatibility {
    match reason {
        HlsPreparedTerminalBundleIncompatibility::TargetDurationExceeded { asset_ms, target_ms } => {
            HlsTerminalTailCompatibility::TargetDurationExceeded { asset_ms, target_ms }
        }
        HlsPreparedTerminalBundleIncompatibility::EmptySegmentSet
        | HlsPreparedTerminalBundleIncompatibility::ZeroTargetDuration => HlsTerminalTailCompatibility::InvalidAsset,
    }
}

fn terminal_bundle_failure_compatibility(
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

async fn register_terminal_pending_owner(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<super::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
) -> HlsTerminalResolution {
    let ticket = match context.ctx.hls_proxy.observe_prepared_terminal_bundle(bundle_key) {
        HlsPreparedTerminalBundleObservation::Flight(ticket) => ticket,
        HlsPreparedTerminalBundleObservation::Settled(state) => {
            return terminal_resolution_after_settled_bundle_observation(
                context,
                asset,
                expected_asset,
                bundle_key,
                Some(state),
            )
            .await;
        }
        HlsPreparedTerminalBundleObservation::Missing => {
            return terminal_resolution_after_settled_bundle_observation(
                context,
                asset,
                expected_asset,
                bundle_key,
                None,
            )
            .await;
        }
    };
    let Some(owner_key) = terminal_pending_owner_key(context, expected_asset, bundle_key) else {
        return HlsTerminalResolution::Reevaluate;
    };
    let latest_safe_commit_at_ms =
        context.preparation.cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    let coordinator = context.ctx.hls_proxy.terminal_pending();
    let task_ctx = context.ctx.clone();
    let task_session = Arc::clone(context.session);
    let task_proxy_session_id = context.proxy_session_id.clone();
    let task_lease_id = context.lease_id.clone();
    let task_preparation = context.preparation.clone();
    let asset_guard = terminal_asset_revision_guard(context.ctx, expected_asset.reason, Some(expected_asset));
    let registration = coordinator.register(owner_key, &asset_guard, move |ownership| {
        run_terminal_pending_owner(
            task_ctx,
            task_session,
            task_proxy_session_id,
            task_lease_id,
            task_preparation,
            asset,
            expected_asset,
            bundle_key,
            ticket,
            ownership,
        )
    });
    terminal_resolution_for_pending_registration(context, expected_asset, latest_safe_commit_at_ms, registration)
}

fn terminal_pending_owner_key(
    context: HlsTerminalCommitContext<'_>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
) -> Option<HlsTerminalPendingOwnerKey> {
    let session_incarnation = context.ctx.hls_proxy.sessions().session_incarnation(context.session)?;
    Some(HlsTerminalPendingOwnerKey {
        session_incarnation,
        proxy_session_id: context.proxy_session_id.clone(),
        lease_id: context.lease_id.clone(),
        lease_issued_at_ms: context.preparation.lease_issued_at_ms,
        expected_admission_generation: context.preparation.expected_admission_generation,
        manifest_snapshot_generation: context.preparation.manifest_snapshot_generation,
        cursor_generation: context.preparation.cursor_generation,
        decision_generation: context.preparation.decision_generation,
        reason: expected_asset.reason,
        bundle_key,
        latest_safe_commit_at_ms: context
            .preparation
            .cutover_timing
            .latest_safe_terminal_commit_at
            .as_millis_since_epoch(),
    })
}

fn register_ready_terminal_pending_owner(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<super::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    bundle: Arc<HlsPreparedTerminalBundle>,
) -> HlsTerminalResolution {
    let Some(owner_key) = terminal_pending_owner_key(context, expected_asset, bundle_key) else {
        return HlsTerminalResolution::Reevaluate;
    };
    let latest_safe_commit_at_ms = owner_key.latest_safe_commit_at_ms;
    let coordinator = context.ctx.hls_proxy.terminal_pending();
    let task_ctx = context.ctx.clone();
    let task_session = Arc::clone(context.session);
    let task_proxy_session_id = context.proxy_session_id.clone();
    let task_lease_id = context.lease_id.clone();
    let task_preparation = context.preparation.clone();
    let asset_guard = terminal_asset_revision_guard(context.ctx, expected_asset.reason, Some(expected_asset));
    let registration = coordinator.register(owner_key, &asset_guard, move |ownership| async move {
        if !ownership.is_current() {
            return;
        }
        let now_ms = task_ctx.hls_proxy.terminal_commit_now_ms();
        let task_context = HlsTerminalCommitContext {
            ctx: &task_ctx,
            session: &task_session,
            proxy_session_id: &task_proxy_session_id,
            lease_id: &task_lease_id,
            preparation: &task_preparation,
            now_ms,
        };
        let outcome = commit_ready_terminal_bundle(task_context, asset, expected_asset, bundle_key, bundle).await;
        if ownership.is_current() {
            observe_autonomous_terminal_resolution(terminal_resolution_with_failed_closed_fallback(
                task_context,
                outcome,
            ));
        }
    });
    terminal_resolution_for_pending_registration(context, expected_asset, latest_safe_commit_at_ms, registration)
}

fn terminal_resolution_for_pending_registration(
    context: HlsTerminalCommitContext<'_>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    latest_safe_commit_at_ms: u64,
    registration: HlsTerminalPendingRegistration,
) -> HlsTerminalResolution {
    match registration {
        HlsTerminalPendingRegistration::Scheduled | HlsTerminalPendingRegistration::AlreadyOwned => {
            HlsTerminalResolution::Pending {
                retry_after_ms: terminal_pending_retry_after_ms(context.now_ms, latest_safe_commit_at_ms),
            }
        }
        HlsTerminalPendingRegistration::Superseded => HlsTerminalResolution::Reevaluate,
        HlsTerminalPendingRegistration::CapacityExceeded | HlsTerminalPendingRegistration::RuntimeUnavailable => {
            let outcome = commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::TerminalMediaNotReady,
            );
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
    }
}

async fn terminal_resolution_after_settled_bundle_observation(
    context: HlsTerminalCommitContext<'_>,
    asset: Arc<super::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    state: Option<HlsPreparedTerminalBundleState>,
) -> HlsTerminalResolution {
    match state {
        Some(HlsPreparedTerminalBundleState::Ready { bundle }) if bundle.matches_key_and_shape(bundle_key) => {
            let outcome = commit_ready_terminal_bundle(context, asset, expected_asset, bundle_key, bundle).await;
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Ready { .. }) => {
            let outcome = commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::AssetRevisionMismatch,
            );
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Failed { key, reason }) => {
            let compatibility = terminal_bundle_failure_compatibility(key, bundle_key, reason);
            let outcome = commit_terminal_unavailable(context, Some(expected_asset), compatibility);
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Incompatible { key, reason }) => {
            let compatibility = if key == bundle_key {
                terminal_bundle_incompatibility(reason)
            } else {
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            };
            let outcome = commit_terminal_unavailable(context, Some(expected_asset), compatibility);
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
        Some(HlsPreparedTerminalBundleState::Preparing { .. }) | None => {
            let outcome = commit_terminal_unavailable(
                context,
                Some(expected_asset),
                HlsTerminalTailCompatibility::TerminalMediaNotReady,
            );
            terminal_resolution_with_failed_closed_fallback(context, outcome)
        }
    }
}

#[derive(Debug)]
enum HlsTerminalPendingDecision {
    Ready(Arc<HlsPreparedTerminalBundle>),
    Unavailable(HlsTerminalTailCompatibility),
}

async fn await_terminal_pending_decision<Fallback>(
    ticket: super::prepared_terminal_bundle::HlsPreparedTerminalBundleCompletionTicket,
    ownership: &HlsTerminalPendingOwnership,
    bundle_key: HlsPreparedTerminalBundleKey,
    fallback: Fallback,
) -> Option<HlsTerminalPendingDecision>
where
    Fallback: Future<Output = ()>,
{
    tokio::pin!(fallback);
    let completion = tokio::select! {
        biased;
        () = ownership.cancelled() => return None,
        completion = ticket.wait() => Some(completion),
        () = &mut fallback => None,
    };
    if !ownership.is_current() {
        return None;
    }
    match completion {
        Some(HlsPreparedTerminalBundleCompletion::Ready { bundle }) if bundle.matches_key_and_shape(bundle_key) => {
            Some(HlsTerminalPendingDecision::Ready(bundle))
        }
        Some(HlsPreparedTerminalBundleCompletion::Ready { .. }) => {
            Some(HlsTerminalPendingDecision::Unavailable(HlsTerminalTailCompatibility::AssetRevisionMismatch))
        }
        Some(HlsPreparedTerminalBundleCompletion::Failed { key, reason }) => Some(
            HlsTerminalPendingDecision::Unavailable(terminal_bundle_failure_compatibility(key, bundle_key, reason)),
        ),
        Some(HlsPreparedTerminalBundleCompletion::Incompatible { key, reason }) => {
            let compatibility = if key == bundle_key {
                terminal_bundle_incompatibility(reason)
            } else {
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            };
            Some(HlsTerminalPendingDecision::Unavailable(compatibility))
        }
        Some(HlsPreparedTerminalBundleCompletion::FlightReplaced { key, generation }) => {
            let compatibility = if key == bundle_key && generation > 0 {
                HlsTerminalTailCompatibility::TerminalMediaNotReady
            } else {
                HlsTerminalTailCompatibility::AssetRevisionMismatch
            };
            Some(HlsTerminalPendingDecision::Unavailable(compatibility))
        }
        None => Some(HlsTerminalPendingDecision::Unavailable(HlsTerminalTailCompatibility::TerminalMediaNotReady)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_terminal_pending_owner(
    ctx: HlsCtx,
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    lease_id: HlsAccessLeaseId,
    preparation: HlsTerminalTailPreparation,
    asset: Arc<super::terminal_tail::HlsTerminalMediaAsset>,
    expected_asset: HlsRuntimeCustomTailAssetIdentity,
    bundle_key: HlsPreparedTerminalBundleKey,
    ticket: super::prepared_terminal_bundle::HlsPreparedTerminalBundleCompletionTicket,
    ownership: HlsTerminalPendingOwnership,
) {
    let latest_safe_commit_at_ms = ownership.latest_safe_commit_at_ms();
    // Terminal-media preparation is started before this path and may use the
    // acquisition window. The final bounded handoff still reserves enough time
    // for an initially contended fail-closed CAS and one retry, both strictly
    // before the exclusive safe deadline.
    let fallback_commit_at_ms = terminal_pending_fallback_commit_at_ms(latest_safe_commit_at_ms);
    let fallback_wait_ms = fallback_commit_at_ms.saturating_sub(ctx.hls_proxy.terminal_commit_now_ms());
    let Some(decision) = await_terminal_pending_decision(
        ticket,
        &ownership,
        bundle_key,
        tokio::time::sleep(Duration::from_millis(fallback_wait_ms)),
    )
    .await
    else {
        return;
    };
    let now_ms = ctx.hls_proxy.terminal_commit_now_ms();
    let context = HlsTerminalCommitContext {
        ctx: &ctx,
        session: &session,
        proxy_session_id: &proxy_session_id,
        lease_id: &lease_id,
        preparation: &preparation,
        now_ms,
    };
    let outcome = match decision {
        HlsTerminalPendingDecision::Ready(bundle) => {
            commit_ready_terminal_bundle(context, asset, expected_asset, bundle_key, bundle).await
        }
        HlsTerminalPendingDecision::Unavailable(compatibility) => {
            commit_terminal_unavailable(context, Some(expected_asset), compatibility)
        }
    };
    observe_autonomous_terminal_resolution(terminal_resolution_with_failed_closed_fallback(context, outcome));
}

fn terminal_pending_retry_after_ms(now_ms: u64, latest_safe_commit_at_ms: u64) -> u64 {
    latest_safe_commit_at_ms.saturating_sub(now_ms).clamp(1, HLS_TERMINAL_PENDING_RETRY_AFTER_MS)
}

fn terminal_pending_fallback_commit_at_ms(latest_safe_commit_at_ms: u64) -> u64 {
    latest_safe_commit_at_ms
        .saturating_sub(HlsTerminalCommitAcquisitionBudgetMs::fail_closed_handoff_from_retry_policy().as_millis())
}

fn terminal_resolution_for_commit_outcome(outcome: HlsTerminalCommitOutcome, now_ms: u64) -> HlsTerminalResolution {
    match outcome {
        HlsTerminalCommitOutcome::Committed | HlsTerminalCommitOutcome::AlreadyCommitted => {
            HlsTerminalResolution::Committed
        }
        HlsTerminalCommitOutcome::SupersededGeneration
        | HlsTerminalCommitOutcome::LeaseNoLongerEligible
        | HlsTerminalCommitOutcome::RecoveryCommitted
        | HlsTerminalCommitOutcome::CutoverNoLongerRequired => HlsTerminalResolution::Reevaluate,
        HlsTerminalCommitOutcome::BundleNotReady => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::BundleNotReadyWithoutOwner }
        }
        HlsTerminalCommitOutcome::BundleIncompatible => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::BundleIncompatible }
        }
        HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed }
        }
        HlsTerminalCommitOutcome::RetryCapacityExceeded => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::RetryCapacityExceeded }
        }
        HlsTerminalCommitOutcome::RetryAttemptsExhausted => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::RetryAttemptsExhausted }
        }
        HlsTerminalCommitOutcome::RetryWorkerUnavailable => {
            HlsTerminalResolution::FailedClosed { reason: HlsTerminalFailedClosedReason::RuntimeUnavailable }
        }
        HlsTerminalCommitOutcome::LockBusy { retry_before_ms } => {
            HlsTerminalResolution::Pending { retry_after_ms: retry_before_ms.saturating_sub(now_ms).max(1) }
        }
    }
}

fn terminal_resolution_with_failed_closed_fallback(
    context: HlsTerminalCommitContext<'_>,
    outcome: HlsTerminalCommitOutcome,
) -> HlsTerminalResolution {
    let resolution = terminal_resolution_for_commit_outcome(outcome, context.now_ms);
    if !matches!(resolution, HlsTerminalResolution::FailedClosed { .. }) {
        return resolution;
    }
    if context.preparation.trigger.is_runtime_policy() {
        return resolution;
    }
    commit_prepared_terminal_unavailable_after_owner_failure(
        context.ctx,
        context.session,
        context.proxy_session_id,
        context.lease_id,
        context.preparation,
        context.now_ms,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HlsAutonomousTerminalObservation {
    Committed,
    StateSuperseded,
    CommitRetry { retry_after_ms: u64 },
    FailedClosed { reason: HlsTerminalFailedClosedReason },
    NoCutoverRequired,
}

fn classify_autonomous_terminal_resolution(resolution: HlsTerminalResolution) -> HlsAutonomousTerminalObservation {
    match resolution {
        HlsTerminalResolution::Committed => HlsAutonomousTerminalObservation::Committed,
        HlsTerminalResolution::Reevaluate => HlsAutonomousTerminalObservation::StateSuperseded,
        HlsTerminalResolution::Pending { retry_after_ms } => {
            HlsAutonomousTerminalObservation::CommitRetry { retry_after_ms }
        }
        HlsTerminalResolution::FailedClosed { reason } => HlsAutonomousTerminalObservation::FailedClosed { reason },
        HlsTerminalResolution::LiveAllowed => HlsAutonomousTerminalObservation::NoCutoverRequired,
    }
}

fn observe_autonomous_terminal_resolution(resolution: HlsTerminalResolution) {
    match classify_autonomous_terminal_resolution(resolution) {
        HlsAutonomousTerminalObservation::Committed => {
            log::debug!("HLS autonomous terminal owner completed: outcome=committed");
        }
        HlsAutonomousTerminalObservation::StateSuperseded => {
            log::debug!("HLS autonomous terminal owner stopped: outcome=state_superseded");
        }
        HlsAutonomousTerminalObservation::CommitRetry { retry_after_ms } => {
            log::debug!(
                "HLS autonomous terminal owner handed off: outcome=commit_retry retry_after_ms={retry_after_ms}"
            );
        }
        HlsAutonomousTerminalObservation::FailedClosed { reason } => {
            warn!("HLS autonomous terminal owner failed closed: reason={}", reason.as_label());
        }
        HlsAutonomousTerminalObservation::NoCutoverRequired => {
            log::debug!("HLS autonomous terminal owner stopped: outcome=no_cutover_required");
        }
    }
}

fn commit_terminal_unavailable(
    context: HlsTerminalCommitContext<'_>,
    mut expected_asset: Option<HlsRuntimeCustomTailAssetIdentity>,
    mut reason: HlsTerminalTailCompatibility,
) -> HlsTerminalCommitOutcome {
    let manager = &context.ctx.hls_proxy;
    let commit_now_ms = manager.terminal_commit_now_ms();
    let mut remaining_revalidations = HLS_TERMINAL_ASSET_REVALIDATION_ATTEMPTS;
    loop {
        let outcome = manager.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
            session: context.session,
            lease_id: context.lease_id,
            proxy_session_id: context.proxy_session_id,
            preparation: context.preparation,
            now_ms: commit_now_ms.max(context.now_ms),
            payload: HlsTerminalCommitPayload::Unavailable(reason),
            asset_revision_guard: terminal_asset_revision_guard(
                context.ctx,
                context.preparation.trigger.reason(),
                expected_asset,
            ),
        });
        if outcome != HlsTerminalCommitOutcome::BundleIncompatible || remaining_revalidations == 0 {
            return outcome;
        }
        remaining_revalidations = remaining_revalidations.saturating_sub(1);
        expected_asset = configured_terminal_asset_identity(context.ctx, context.preparation.trigger.reason());
        reason = if expected_asset.is_some() {
            HlsTerminalTailCompatibility::AssetRevisionMismatch
        } else {
            HlsTerminalTailCompatibility::MissingAsset
        };
    }
}

fn commit_prepared_terminal_unavailable_after_owner_failure(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    preparation: &HlsTerminalTailPreparation,
    now_ms: u64,
) -> HlsTerminalResolution {
    let expected_asset = configured_terminal_asset_identity(ctx, HlsRuntimeCustomTailReason::ChannelUnavailable);
    let outcome = ctx.hls_proxy.commit_access_lease_terminal_if_generation_matches(HlsTerminalCommitRequest {
        session,
        lease_id,
        proxy_session_id,
        preparation,
        now_ms: ctx.hls_proxy.terminal_commit_now_ms().max(now_ms),
        payload: HlsTerminalCommitPayload::UnavailableAfterOwnerFailure(
            HlsTerminalTailCompatibility::TerminalMediaNotReady,
        ),
        asset_revision_guard: terminal_asset_revision_guard(
            ctx,
            HlsRuntimeCustomTailReason::ChannelUnavailable,
            expected_asset,
        ),
    });
    terminal_resolution_for_commit_outcome(outcome, now_ms)
}

fn configured_terminal_asset_identity(
    ctx: &HlsCtx,
    reason: HlsRuntimeCustomTailReason,
) -> Option<HlsRuntimeCustomTailAssetIdentity> {
    current_hls_runtime_custom_tail_identity(ctx, reason)
}

fn terminal_asset_revision_guard(
    ctx: &HlsCtx,
    reason: HlsRuntimeCustomTailReason,
    expected: Option<HlsRuntimeCustomTailAssetIdentity>,
) -> HlsTerminalAssetRevisionGuard {
    let custom_stream_response = Arc::clone(&ctx.app_config.custom_stream_response);
    let custom_video_stream_enabled = Arc::clone(&ctx.app_config.config);
    HlsTerminalAssetRevisionGuard::for_optional_runtime_tail(reason, expected, move || {
        if !custom_video_stream_enabled.load().custom_stream_response_enabled {
            return None;
        }
        custom_stream_response
            .load_full()
            .as_ref()
            .and_then(|responses| match reason {
                HlsRuntimeCustomTailReason::ChannelUnavailable => responses.channel_unavailable.as_ref(),
                HlsRuntimeCustomTailReason::LowPriorityPreempted => responses.low_priority_preempted.as_ref(),
                HlsRuntimeCustomTailReason::UserConnectionsExhausted => responses.user_connections_exhausted.as_ref(),
                HlsRuntimeCustomTailReason::ProviderConnectionsExhausted => {
                    responses.provider_connections_exhausted.as_ref()
                }
                HlsRuntimeCustomTailReason::UserAccountExpired => responses.user_account_expired.as_ref(),
                HlsRuntimeCustomTailReason::SessionOrLeaseExpired => responses.hls_session_or_lease_expired.as_ref(),
            })
            .and_then(terminal_media_asset_identity)
            .map(|media| HlsRuntimeCustomTailAssetIdentity { reason, media })
    })
}

pub(super) fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{
        super::{
            lease::HlsAccessLeaseTiming,
            media_reserve::{
                HlsLeaseManifestSegment, HlsLeasePlaybackCursor, HlsLeaseReserveAvailabilityBasis,
                HlsManifestDeliveryMode, HlsManifestSourceRenderMarker, HlsReadyMediaState, HlsReadyTimelineUnit,
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
            terminal_pending::{
                HlsTerminalPendingCoordinator, HlsTerminalPendingOwnerKey, HlsTerminalPendingRegistration,
            },
            terminal_tail::{
                terminal_tail_manifest_body, HlsLeasePlaybackMode, HlsMapSignature, HlsMediaContainer,
                HlsTerminalAssetIdentity, HlsTerminalSegmentPath, HlsTerminalTailPlan,
            },
            CacheAccessState, HlsAccessLease, HlsAccessLeaseState, HlsPlaybackFamilyKey, HlsSession, HlsSessionKey,
            OriginSegmentKey, SegmentCacheKey, SegmentCacheStatus, SegmentEntry, SegmentFetchPriority,
        },
        *,
    };
    use crate::prepared_terminal_bundle::build_prepared_terminal_bundle;
    use bytes::Bytes;
    use std::sync::Arc;
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
    const PROVIDER_EXHAUSTED_ASSET_BYTES: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test/fixtures/hls/provider_connections_exhausted.ts"
    ));
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

        assert_eq!(
            terminal_media_timing_seed(ctx, 10_000),
            (None, HlsTerminalMediaPreparationState::Failed { key: None })
        );
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
            (
                HlsTerminalCommitOutcome::SafeCommitDeadlineElapsed,
                HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed,
            ),
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
            terminal_resolution_for_commit_outcome(
                HlsTerminalCommitOutcome::LockBusy { retry_before_ms: 1_025 },
                1_000,
            ),
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
        let handoff_budget_ms =
            HlsTerminalCommitAcquisitionBudgetMs::fail_closed_handoff_from_retry_policy().as_millis();

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
            source_render_marker: HlsManifestSourceRenderMarker::new(1),
            snapshot_generation: 0,
            delivered_at_ms: 0,
            first_proxy_seq: 40,
            last_proxy_seq: 40,
            visible_segments: Arc::from([HlsLeaseManifestSegment {
                proxy_seq: 40,
                duration_ms,
                uri: format!("/hls/shared/live/{}/{}/40.ts", proxy_session_id.0, lease_id.0),
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
            HlsLeasePlaybackMode::TerminalUnavailable {
                reason: HlsTerminalTailCompatibility::MissingTimestampAnchor,
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn hls_terminal_commit_pending_owner_store_fallback_commits_terminal_unavailable() {
        let fixture = terminal_pending_commit_fixture("pending-owner-fallback-store").await;
        let (ticket, _publisher) = prepared_terminal_bundle_completion_channel_for_test(fixture.bundle_key);
        let completed = fixture.register_owner(ticket);

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(
            HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis(),
        ))
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
            HlsLeasePlaybackMode::TerminalUnavailable {
                reason: HlsTerminalTailCompatibility::TerminalMediaNotReady,
                ..
            }
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
            HlsLeasePlaybackMode::TerminalUnavailable {
                reason: HlsTerminalTailCompatibility::TerminalMediaNotReady,
                ..
            }
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
            source_render_marker: HlsManifestSourceRenderMarker::new(1),
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
            commit_terminal_tail_if_lease_reserve_requires_cutover(ctx, &session, &proxy_session_id, &lease, now_ms)
                .await;

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

    async fn wait_for_availability_owner_completion(fixture: &PostRefreshTerminalFixture) {
        let coordinator = fixture.ctx.hls_proxy.availability_reevaluations();
        while let Some(mut observer) = coordinator.observe_owner(&fixture.proxy_session_id) {
            let _ = observer.changed().await;
        }
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
            format!("/hls/shared/live/{}/{}/0.ts", proxy_session_id.0, lease_id.0);
        assert!(ctx
            .hls_proxy
            .commit_access_lease_manifest_publication(
                &lease_id,
                proxy_session_id,
                publication,
                manifest_snapshot,
                now_ms,
            )
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
        let lease_id = publish_post_refresh_terminal_lease(
            ctx,
            &proxy_session_id,
            name,
            terminal_asset,
            target_duration_ms,
            now_ms,
        )
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
            HlsRuntimeCustomTailReason::ProviderConnectionsExhausted => {
                responses.provider_connections_exhausted.as_ref()
            }
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
        let channel =
            snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, HlsRuntimeCustomTailReason::ChannelUnavailable)
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
                &snapshot_hls_runtime_custom_tail_asset(&fixture.ctx, reason)
                    .expect("reason-specific configured asset")
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
        assert_active_policy_reason_commits(
            "runtime-user-exhausted",
            HlsRuntimeCustomTailReason::UserConnectionsExhausted,
        )
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
        let old_key =
            prepared_terminal_bundle_key(&old_asset.asset, target_duration_ms, HLS_TERMINAL_TAIL_SEGMENT_COUNT);
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
        let old_bundle =
            build_prepared_terminal_bundle(&old_asset.asset, old_key).expect("old controlled relative bundle");
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
            format!("/hls/shared/live/{}/{}/0.ts", fixture.proxy_session_id.0, lease_id.0);
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

        fixture.ctx.hls_proxy.set_terminal_commit_retry_capacity_for_test(
            super::super::terminal_commit::HLS_TERMINAL_COMMIT_RETRY_CAPACITY,
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
        let fixture_name =
            if reverse_insertion { "multi-lease-fallback-reverse" } else { "multi-lease-fallback-forward" };
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

        let aggregate = evaluate_owner_failure_fallback(
            &fixture.ctx,
            &fixture.session,
            &fixture.proxy_session_id,
            evaluation_now_ms,
        )
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
        let safe_deadline = HlsLeaseCutoverTiming::from_reserve(
            fixture.now_ms,
            12_900,
            HlsTransitionMarginMs::from_millis(12_000),
            None,
        )
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
        let guard =
            store.prepare_manifest_publication(&lease_id, proxy_session_id, 1).expect("manifest publication guard");
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
        assert_eq!(
            directive.timing_seed.map(|seed| seed.workload.burst),
            Some(HlsRecoveryBurstWorkload::FullBurstPending)
        );

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

        let forward =
            aggregate_session_recovery_pressure([lease_b.clone(), lease_a.clone()]).expect("forward pressure");
        let reverse = aggregate_session_recovery_pressure([lease_a, lease_b]).expect("reverse pressure");

        assert_eq!(forward.controlling.lease_id.0, "lease-a");
        assert_eq!(reverse.controlling.lease_id.0, "lease-a");
    }

    #[test]
    fn hls_session_recovery_pressure_cursor_change_is_observed_by_atomic_commit() {
        let mut session = atomic_pressure_session();
        let proxy_session_id = session.proxy_session_id.clone();
        let mut leases = HlsAccessLeaseStore::default();
        let lease_id = install_atomic_pressure_lease(
            &mut leases,
            &proxy_session_id,
            "lease-a",
            pressure_manifest_at(0, 8_000),
            1_000,
        );
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
        install_atomic_pressure_lease(
            &mut leases,
            &proxy_session_id,
            "lease-b",
            pressure_manifest_at(2, 10_000),
            1_000,
        );

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
        install_atomic_pressure_lease(
            &mut leases,
            &proxy_session_id,
            "lease-a",
            pressure_manifest_at(0, 8_000),
            20_000,
        );
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
}
