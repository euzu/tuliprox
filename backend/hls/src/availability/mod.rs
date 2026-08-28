use super::{
    availability_reevaluation::{HlsAvailabilityReevaluationOwnerKey, HlsRecoveryPressureGuard},
    hls_ctx::HlsCtx,
    manager::{HlsCriticalHandoffStateAccess, HlsProxyManager},
    manifest_acceptance::HlsManifestAcceptanceTrigger,
    media_reserve::{
        evaluate_startup_admission, HlsLeaseManifestSnapshot, HlsStartupAdmissionDecision, HlsStartupAdmissionInput,
        HlsStartupAdmissionOriginState,
    },
    observability::HlsRecoveryTriggerDiagnostic,
    recovery_timing::{
        bounded_manifest_request_eta_ms, HlsAcceptanceEpisodeTimingSeed, HlsOperationTimeoutMs, HlsRecoveryEtaMs,
        HlsRecoveryTimingPolicy, HlsRecoveryWorkloadEnvelope,
    },
    segment_fetcher::HlsSegmentFetchWorkload,
    session_store::HlsSessionHandle,
    HlsLiveReserveDeadline, HlsOriginProgressDecision, ProxySessionId,
};

mod recovery_pressure;
mod reevaluation;
mod terminal_cutover;
mod terminal_pending_owner;
pub(crate) use self::terminal_cutover::commit_terminal_tail_if_lease_reserve_requires_cutover_detailed;
use self::{
    recovery_pressure::{
        evaluate_and_commit_session_recovery_pressure_in_snapshot, hls_recovery_pressure_policy,
        recovery_trigger_budget, terminal_media_timing_seed, HlsRecoveryPressureDecision,
    },
    reevaluation::HlsAvailabilityReevaluationAttempt,
};
pub use self::{
    reevaluation::{register_hls_availability_reevaluation, register_post_refresh_availability_reevaluation},
    terminal_cutover::{commit_prepared_runtime_custom_tail, commit_terminal_tail_if_lease_reserve_requires_cutover},
};
use std::future::Future;

pub const HLS_PLAYBACK_RATE_GUARD_MILLI: u16 = 1_050;
const HLS_AVAILABILITY_STATE_ACCESS_ATTEMPTS: usize = 3;
const HLS_AVAILABILITY_REEVALUATION_MAX_ATTEMPTS: u8 = 64;
const HLS_AVAILABILITY_REEVALUATION_DEADLINE_MS: u64 = 2_000;
const HLS_AVAILABILITY_REEVALUATION_MAX_BACKOFF_MS: u64 = 50;
const HLS_TERMINAL_PENDING_RETRY_AFTER_MS: u64 = 50;
const HLS_TERMINAL_ASSET_REVALIDATION_ATTEMPTS: usize = 2;

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
    const fn resolved(resolution: HlsTerminalResolution) -> Self {
        Self { resolution, live_reserve_deadline: None }
    }

    const fn with_deadline(
        resolution: HlsTerminalResolution,
        live_reserve_deadline: Option<HlsLiveReserveDeadline>,
    ) -> Self {
        Self { resolution, live_reserve_deadline }
    }
}

pub(super) use tuliprox_core::utils::current_time_millis;

#[cfg(test)]
mod tests;
