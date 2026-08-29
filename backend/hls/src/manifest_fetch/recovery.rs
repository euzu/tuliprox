//! The recovery chain: retrying a manifest fetch across candidate origins.
//!
//! `retry_hls_origin_manifest_recovery_chain` drives it - fetch a burst of
//! candidates, evaluate them together, select one, and commit it. The burst is
//! evaluated as a set rather than candidate by candidate, which is what lets a
//! deterministic conflict be proven rather than guessed.

use super::{
    current_time_millis,
    episode::{
        begin_manifest_acceptance_episode, begin_requalified_manifest_acceptance_episode, committed_resource_evidence,
        complete_manifest_acceptance_episode, hold_uncommitted_manifest_acceptance_episode,
        manifest_acceptance_episode_snapshot, manifest_acceptance_exhaustion_reason, manifest_candidate_observation,
        record_full_manifest_acceptance_burst, record_manifest_acceptance_exhaustion,
        record_manifest_acceptance_landscape, update_manifest_acceptance_episode_state,
    },
    error::{
        is_hls_retryable_manifest_reject_fetch_error, HlsManifestCommitError, HlsManifestRejectLogReason,
        OriginManifestFetchError,
    },
    fetch_hls_origin_manifest_request,
    fingerprint::{
        deterministic_conflict_proven_by_full_burst, deterministic_timeline_conflict_for_candidate,
        record_deterministic_conflict_receipt,
    },
    http::ManifestRecoveryAttemptLogContext,
    next_retry_delay_ms,
    quality::score_manifest_recovery_candidate_with_logging,
    selection_log::{
        log_manifest_recovery_candidate_rejected, log_manifest_recovery_selected, log_manifest_retry_scheduled,
        ManifestRetryLogKind,
    },
    FetchedOriginManifest, HlsManifestCommitAcceptanceMode, HlsManifestFetchSelection,
    HlsManifestRecoveryCandidateScoreReport, HlsOriginManifestFetchContext, HlsOriginManifestFetchRequest,
};
use crate::{
    deterministic_conflict::HlsDeterministicTimelineConflict,
    manifest_acceptance::{
        classify_reduced_retry_landscape, evaluate_manifest_acceptance, held_alternative_after_burst,
        manifest_acceptance_landscape, HlsAlternativeOriginCohort, HlsManifestAcceptanceGeneration,
        HlsManifestAcceptanceInput, HlsManifestAcceptanceState, HlsManifestAcceptanceTrigger,
        HlsManifestCandidateObservation, HlsManifestCommitKind, HlsManifestCommitPlan,
        HlsManifestRecoveryCandidateIdentity, HlsRecoveryWorkloadBindingUpdate, HlsReducedRetryLandscapeChange,
        HLS_MANIFEST_ACCEPTANCE_MAX_REQUALIFICATIONS_PER_REFRESH, HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS,
    },
    manifest_origin_binding::HlsManifestOriginBinding,
    recovery_timing::HlsAcceptanceDeadlineMs,
};
use shared::model::{HlsManifestRecoveryBurstLevel, HlsManifestRecoveryBurstPlan};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

enum HlsManifestRecoveryAttemptError<T> {
    Fetch(OriginManifestFetchError),
    Rejected(HlsManifestRejectLogReason),
    Requalified,
    Committed(T),
}

#[derive(Debug)]
pub(super) struct HlsManifestRecoveryCandidate {
    pub(super) candidate_index: usize,
    pub(super) fetch_elapsed_ms: u64,
    pub(super) fetched: FetchedOriginManifest,
    pub(super) report: HlsManifestRecoveryCandidateScoreReport,
}

struct HlsManifestRecoveryBurstCollection {
    fetched_candidates: Vec<HlsManifestRecoveryCandidate>,
    completed_candidates: usize,
    last_fetch_error: Option<OriginManifestFetchError>,
    last_reject_reason: Option<HlsManifestRejectLogReason>,
}

pub async fn retry_hls_origin_manifest_recovery_chain<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    binding: HlsManifestOriginBinding,
    mut reject_reason: Option<HlsManifestRejectLogReason>,
    initial_deterministic_conflict: Option<HlsDeterministicTimelineConflict>,
    trigger: HlsManifestAcceptanceTrigger,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    mut commit: C,
) -> Result<T, OriginManifestFetchError>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    if !trigger.starts_episode() {
        return Err(OriginManifestFetchError::RetryExhausted);
    }
    let attempts = context.retry_policy.attempt_count();
    let mut last_error = OriginManifestFetchError::RetryExhausted;
    let mut next_attempt_is_full_plan = true;
    let mut requalifications = 0_u8;
    let mut attempt_index = 0_usize;
    let mut attempt_limit = attempts;
    let mut completed_candidate_requests = 0_usize;
    while attempt_index < attempt_limit {
        // A materially changed landscape is requalified immediately. Once
        // authorized, that new generation must still receive its complete
        // configured burst rather than losing candidates to a retry delay.
        let delay_ms = if attempt_index > 0 && next_attempt_is_full_plan {
            0
        } else {
            let jitter = context.retry_policy.sample_jitter_ms();
            context.retry_policy.delay_for_attempt_ms(attempt_index, jitter).unwrap_or_default()
        };
        let acceptance_deadline = current_acceptance_deadline(context).await;
        if !acceptance_attempt_may_start(
            next_attempt_is_full_plan,
            current_time_millis(),
            delay_ms,
            acceptance_deadline,
        ) {
            return Err(last_error);
        }
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let attempt_plan = HlsManifestRecoveryAttemptPlan {
            binding: &binding,
            attempt_index,
            attempts: attempt_limit,
            reject_reason: reject_reason.as_ref(),
            initial_deterministic_conflict: initial_deterministic_conflict.as_ref(),
            acceptance_mode,
            trigger,
            current_burst_is_full_plan: next_attempt_is_full_plan,
            may_requalify: requalifications < HLS_MANIFEST_ACCEPTANCE_MAX_REQUALIFICATIONS_PER_REFRESH,
            acceptance_deadline,
            completed_candidate_requests,
        };
        let candidate_requests_in_attempt = recovery_burst_plan(context, next_attempt_is_full_plan).total_candidates();
        next_attempt_is_full_plan = false;
        match fetch_and_commit_manifest_recovery_attempt(context, attempt_plan, &mut commit).await {
            HlsManifestRecoveryAttemptError::Committed(committed) => return Ok(committed),
            HlsManifestRecoveryAttemptError::Requalified => {
                requalifications = requalifications.saturating_add(1);
                next_attempt_is_full_plan = true;
                last_error = OriginManifestFetchError::RetryExhausted;
                attempt_limit = attempt_limit_for_started_requalification(attempt_limit, attempt_index);
            }
            HlsManifestRecoveryAttemptError::Rejected(reason) if attempt_index.saturating_add(1) < attempt_limit => {
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::PinnedHostRecovery,
                    attempt_index,
                    attempt_limit,
                    next_retry_delay_ms(&context.retry_policy, attempt_index, None, 0),
                    Some(&reason),
                    None,
                )
                .await;
                reject_reason = Some(reason);
                last_error = OriginManifestFetchError::RetryExhausted;
            }
            HlsManifestRecoveryAttemptError::Rejected(_reason) => {
                return Err(OriginManifestFetchError::RetryExhausted);
            }
            HlsManifestRecoveryAttemptError::Fetch(err)
                if is_hls_retryable_manifest_reject_fetch_error(&err)
                    && attempt_index.saturating_add(1) < attempt_limit =>
            {
                log_manifest_retry_scheduled(
                    context,
                    ManifestRetryLogKind::PinnedHostRecovery,
                    attempt_index,
                    attempt_limit,
                    next_retry_delay_ms(&context.retry_policy, attempt_index, None, 0),
                    None,
                    Some(&err),
                )
                .await;
                last_error = err;
            }
            HlsManifestRecoveryAttemptError::Fetch(err) => return Err(err),
        }
        completed_candidate_requests = completed_candidate_requests.saturating_add(candidate_requests_in_attempt);
        attempt_index = attempt_index.saturating_add(1);
    }

    Err(last_error)
}

pub(super) fn acceptance_attempt_may_start(
    current_burst_is_full_plan: bool,
    now_ms: u64,
    delay_ms: u64,
    acceptance_deadline: HlsAcceptanceDeadlineMs,
) -> bool {
    // Every new episode owns one unconditional configured burst. Budget
    // enforcement bounds reduced retries and whether a requalification may
    // begin, but never abandons a generation after it has begun.
    current_burst_is_full_plan || now_ms.saturating_add(delay_ms) < acceptance_deadline.as_millis_since_epoch()
}

async fn current_acceptance_deadline(context: &HlsOriginManifestFetchContext) -> HlsAcceptanceDeadlineMs {
    context.session.read().await.origin_control.acceptance_episode.as_ref().map_or_else(
        || HlsAcceptanceDeadlineMs::from_millis_since_epoch(u64::MAX),
        |episode| episode.timing().acceptance_deadline,
    )
}

pub(super) fn attempt_limit_for_started_requalification(attempt_limit: usize, attempt_index: usize) -> usize {
    // A requalification normally consumes the next configured retry slot. If
    // the landscape changed in the last slot, reserve exactly one additional
    // slot for the newly started generation's mandatory configured burst.
    attempt_limit.max(attempt_index.saturating_add(2))
}

pub(super) struct HlsManifestRecoveryAttemptPlan<'a> {
    pub(super) binding: &'a HlsManifestOriginBinding,
    pub(super) attempt_index: usize,
    attempts: usize,
    reject_reason: Option<&'a HlsManifestRejectLogReason>,
    initial_deterministic_conflict: Option<&'a HlsDeterministicTimelineConflict>,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    pub(super) trigger: HlsManifestAcceptanceTrigger,
    current_burst_is_full_plan: bool,
    may_requalify: bool,
    acceptance_deadline: HlsAcceptanceDeadlineMs,
    completed_candidate_requests: usize,
}

async fn fetch_and_commit_manifest_recovery_attempt<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: HlsManifestRecoveryAttemptPlan<'_>,
    commit: &mut C,
) -> HlsManifestRecoveryAttemptError<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let burst_plan = recovery_burst_plan(context, plan.current_burst_is_full_plan);
    fetch_and_commit_manifest_recovery_burst_attempt(context, plan, burst_plan, commit).await
}

async fn fetch_and_commit_manifest_recovery_burst_attempt<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    commit: &mut C,
) -> HlsManifestRecoveryAttemptError<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let episode_generation = begin_manifest_acceptance_episode(context, &plan, burst_plan).await;
    let HlsManifestRecoveryBurstCollection {
        fetched_candidates,
        completed_candidates,
        last_fetch_error,
        mut last_reject_reason,
    } = fetch_manifest_recovery_burst_candidates(context, &plan, burst_plan).await;
    record_full_manifest_acceptance_burst(
        context,
        episode_generation,
        plan.current_burst_is_full_plan,
        completed_candidates,
    )
    .await;
    let evaluation =
        match evaluate_manifest_recovery_burst(context, &plan, burst_plan, episode_generation, &fetched_candidates)
            .await
        {
            HlsManifestRecoveryBurstEvaluationOutcome::Continue(evaluation) => *evaluation,
            HlsManifestRecoveryBurstEvaluationOutcome::DeterministicConflict(conflict) => {
                return HlsManifestRecoveryAttemptError::Fetch(
                    OriginManifestFetchError::DeterministicTimelineConflict(conflict),
                );
            }
            HlsManifestRecoveryBurstEvaluationOutcome::Requalified => {
                return HlsManifestRecoveryAttemptError::Requalified;
            }
            HlsManifestRecoveryBurstEvaluationOutcome::Exhausted => {
                return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::RetryExhausted);
            }
        };
    match commit_selected_manifest_recovery_candidate(
        context,
        &plan,
        burst_plan,
        episode_generation,
        evaluation.acceptance_plan,
        fetched_candidates,
        completed_candidates,
        commit,
    )
    .await
    {
        HlsSelectedManifestRecoveryCommit::Committed(committed) => {
            return HlsManifestRecoveryAttemptError::Committed(committed);
        }
        HlsSelectedManifestRecoveryCommit::Rejected(reason) => last_reject_reason = Some(reason),
        HlsSelectedManifestRecoveryCommit::LocalRepresentationLimit(violation) => {
            return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::LocalRepresentationLimit(
                violation,
            ));
        }
        HlsSelectedManifestRecoveryCommit::MalformedTransientRepresentation => {
            return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::MalformedTransientRepresentation);
        }
        HlsSelectedManifestRecoveryCommit::CommitGenerationExhausted => {
            return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::CommitGenerationExhausted);
        }
        HlsSelectedManifestRecoveryCommit::NotSelected => {}
    }

    if plan.current_burst_is_full_plan {
        record_manifest_acceptance_exhaustion(
            context,
            episode_generation,
            manifest_acceptance_exhaustion_reason(&evaluation.observations),
        )
        .await;
    }
    hold_uncommitted_manifest_acceptance_episode(
        context,
        episode_generation,
        evaluation.held_alternative,
        evaluation.next_retry_at_ms,
    )
    .await;
    if let Some(reason) = last_reject_reason {
        return HlsManifestRecoveryAttemptError::Rejected(reason);
    }
    HlsManifestRecoveryAttemptError::Fetch(last_fetch_error.unwrap_or(OriginManifestFetchError::RetryExhausted))
}

struct HlsManifestRecoveryBurstEvaluation {
    observations: Vec<HlsManifestCandidateObservation>,
    acceptance_plan: HlsManifestCommitPlan,
    held_alternative: Option<HlsAlternativeOriginCohort>,
    next_retry_at_ms: u64,
}

enum HlsManifestRecoveryBurstEvaluationOutcome {
    Continue(Box<HlsManifestRecoveryBurstEvaluation>),
    DeterministicConflict(Box<HlsDeterministicTimelineConflict>),
    Requalified,
    Exhausted,
}

const fn manifest_acceptance_state_for_plan(plan: &HlsManifestCommitPlan) -> HlsManifestAcceptanceState {
    match plan {
        HlsManifestCommitPlan::Commit { .. } => HlsManifestAcceptanceState::Committing,
        HlsManifestCommitPlan::StageAlternative { .. } => HlsManifestAcceptanceState::StagingSwitchSegment,
        HlsManifestCommitPlan::HoldAlternative | HlsManifestCommitPlan::RejectAll => {
            HlsManifestAcceptanceState::Holding
        }
    }
}

async fn evaluate_manifest_recovery_burst(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    episode_generation: Option<HlsManifestAcceptanceGeneration>,
    fetched_candidates: &[HlsManifestRecoveryCandidate],
) -> HlsManifestRecoveryBurstEvaluationOutcome {
    // Candidate order is scheduler order only. In particular, a numerically
    // larger highwater from a different effective host is never a sort key.
    // Pressure is the immutable pre-episode snapshot; `Recovering` itself is
    // deliberately not reserve evidence.
    let resource_evidence = committed_resource_evidence(context).await;
    let candidate_evaluations = fetched_candidates
        .iter()
        .map(|candidate| {
            let observation = manifest_candidate_observation(
                candidate,
                burst_plan,
                &resource_evidence.ready_identities,
                &resource_evidence.published_identities,
            );
            let deterministic_conflict = deterministic_timeline_conflict_for_candidate(candidate, &resource_evidence);
            (observation, deterministic_conflict)
        })
        .collect::<Vec<_>>();
    let observations = candidate_evaluations.iter().map(|(observation, _)| observation.clone()).collect::<Vec<_>>();
    if plan.current_burst_is_full_plan {
        record_manifest_acceptance_landscape(context, episode_generation, manifest_acceptance_landscape(&observations))
            .await;
        if let Some(conflict) = deterministic_conflict_proven_by_full_burst(
            plan.initial_deterministic_conflict,
            &candidate_evaluations,
            fetched_candidates.len(),
            burst_plan.total_candidates(),
        ) {
            record_deterministic_conflict_receipt(context, episode_generation, conflict.clone(), &resource_evidence)
                .await;
            return HlsManifestRecoveryBurstEvaluationOutcome::DeterministicConflict(Box::new(conflict));
        }
    }
    let episode_snapshot = manifest_acceptance_episode_snapshot(context, episode_generation).await;
    let reduced_landscape_changed = !plan.current_burst_is_full_plan
        && episode_snapshot
            .as_ref()
            .and_then(|episode| episode.observed_landscape.as_ref())
            .map(|landscape| classify_reduced_retry_landscape(landscape, &observations))
            .is_some_and(HlsReducedRetryLandscapeChange::requires_full_requalification);
    if reduced_landscape_changed {
        if plan.may_requalify
            && begin_requalified_manifest_acceptance_episode(
                context,
                episode_generation,
                plan.trigger,
                plan.acceptance_deadline,
            )
            .await
        {
            return HlsManifestRecoveryBurstEvaluationOutcome::Requalified;
        }
        // A changed landscape may never fall through into reduced-burst
        // cross-host acceptance when its full requalification budget is gone.
        let next_retry_at_ms = current_time_millis().saturating_add(next_retry_delay_ms(
            &context.retry_policy,
            plan.attempt_index,
            None,
            0,
        ));
        hold_uncommitted_manifest_acceptance_episode(
            context,
            episode_generation,
            episode_snapshot.and_then(|episode| episode.held_alternative),
            next_retry_at_ms,
        )
        .await;
        return HlsManifestRecoveryBurstEvaluationOutcome::Exhausted;
    }
    let acceptance_plan = episode_snapshot.as_ref().map_or(HlsManifestCommitPlan::RejectAll, |episode| {
        evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
            full_burst_completed: episode.full_burst_completed,
            current_burst_is_full_plan: plan.current_burst_is_full_plan,
            trigger: episode.trigger,
            previous_alternative: episode.held_alternative.as_ref(),
            observations: &observations,
        })
    });
    let held_alternative = episode_snapshot.as_ref().and_then(|episode| {
        held_alternative_after_burst(&observations, episode.held_alternative.as_ref(), plan.current_burst_is_full_plan)
    });
    let next_retry_at_ms =
        current_time_millis().saturating_add(next_retry_delay_ms(&context.retry_policy, plan.attempt_index, None, 0));
    let next_state = manifest_acceptance_state_for_plan(&acceptance_plan);
    update_manifest_acceptance_episode_state(context, episode_generation, next_state).await;
    HlsManifestRecoveryBurstEvaluationOutcome::Continue(Box::new(HlsManifestRecoveryBurstEvaluation {
        observations,
        acceptance_plan,
        held_alternative,
        next_retry_at_ms,
    }))
}

enum HlsSelectedManifestRecoveryCommit<T> {
    Committed(T),
    Rejected(HlsManifestRejectLogReason),
    LocalRepresentationLimit(crate::manifest_limits::HlsManifestLimitViolation),
    MalformedTransientRepresentation,
    CommitGenerationExhausted,
    NotSelected,
}

#[allow(clippy::too_many_arguments)]
async fn commit_selected_manifest_recovery_candidate<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    episode_generation: Option<HlsManifestAcceptanceGeneration>,
    acceptance_plan: HlsManifestCommitPlan,
    fetched_candidates: Vec<HlsManifestRecoveryCandidate>,
    completed_candidates: usize,
    commit: &mut C,
) -> HlsSelectedManifestRecoveryCommit<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let Some((selected_candidate_index, selected_commit_kind)) = selected_manifest_candidate(acceptance_plan) else {
        return HlsSelectedManifestRecoveryCommit::NotSelected;
    };
    let Some(candidate) =
        fetched_candidates.into_iter().find(|candidate| candidate.candidate_index == selected_candidate_index)
    else {
        return HlsSelectedManifestRecoveryCommit::NotSelected;
    };
    let HlsManifestRecoveryCandidate { candidate_index, fetched, report, .. } = candidate;
    let candidate_identity = HlsManifestRecoveryCandidateIdentity::from_candidate(
        candidate_index,
        report.quality.effective_host.as_deref(),
        &fetched.body,
    );
    if !select_manifest_recovery_candidate(context, episode_generation, candidate_identity).await {
        return HlsSelectedManifestRecoveryCommit::Rejected(HlsManifestRejectLogReason::StagedSwitchInvalidated);
    }
    let acceptance_mode = selected_commit_acceptance_mode(selected_commit_kind, plan.acceptance_mode);
    let selection = if burst_plan.total_candidates() > 1 {
        HlsManifestFetchSelection::Burst
    } else {
        HlsManifestFetchSelection::Recovery
    };
    let candidate_requests = plan.completed_candidate_requests.saturating_add(completed_candidates);
    match commit(
        fetched.with_recovery_diagnostics(plan.attempt_index + 1, candidate_requests, selection),
        acceptance_mode,
    )
    .await
    {
        Ok(committed) => {
            complete_manifest_acceptance_episode(context, episode_generation).await;
            log_manifest_recovery_selected(context, candidate_index, burst_plan.total_candidates(), &report).await;
            HlsSelectedManifestRecoveryCommit::Committed(committed)
        }
        Err(HlsManifestCommitError::LocalRepresentationLimit(violation)) => {
            complete_manifest_acceptance_episode(context, episode_generation).await;
            HlsSelectedManifestRecoveryCommit::LocalRepresentationLimit(violation)
        }
        Err(HlsManifestCommitError::MalformedTransientRepresentation) => {
            complete_manifest_acceptance_episode(context, episode_generation).await;
            HlsSelectedManifestRecoveryCommit::MalformedTransientRepresentation
        }
        Err(HlsManifestCommitError::CommitGenerationExhausted) => {
            complete_manifest_acceptance_episode(context, episode_generation).await;
            HlsSelectedManifestRecoveryCommit::CommitGenerationExhausted
        }
        Err(HlsManifestCommitError::TimelineRejected { reason }) => {
            log_manifest_recovery_candidate_rejected(
                context,
                candidate_index,
                burst_plan.total_candidates(),
                report.quality.effective_host.as_deref(),
                report.quality.origin_highwater,
                &reason,
            )
            .await;
            HlsSelectedManifestRecoveryCommit::Rejected(reason)
        }
        Err(HlsManifestCommitError::RetryCurrentTarget) => {
            let reason = HlsManifestRejectLogReason::PinnedHostRecoveryRejected;
            log_manifest_recovery_candidate_rejected(
                context,
                candidate_index,
                burst_plan.total_candidates(),
                report.quality.effective_host.as_deref(),
                report.quality.origin_highwater,
                &reason,
            )
            .await;
            HlsSelectedManifestRecoveryCommit::Rejected(reason)
        }
    }
}

async fn select_manifest_recovery_candidate(
    context: &HlsOriginManifestFetchContext,
    generation: Option<HlsManifestAcceptanceGeneration>,
    candidate_identity: HlsManifestRecoveryCandidateIdentity,
) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    let mut session = context.session.write().await;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        return false;
    };
    episode.select_candidate(generation, candidate_identity) == HlsRecoveryWorkloadBindingUpdate::Applied
}

const fn selected_commit_acceptance_mode(
    commit_kind: HlsManifestCommitKind,
    fallback: HlsManifestCommitAcceptanceMode,
) -> HlsManifestCommitAcceptanceMode {
    match commit_kind {
        HlsManifestCommitKind::AnchoredAlternative | HlsManifestCommitKind::AlternativeAsNewEpoch => {
            HlsManifestCommitAcceptanceMode::AllowHeldHostSwitchCandidate
        }
        HlsManifestCommitKind::ContentVerifiedAlternative => {
            HlsManifestCommitAcceptanceMode::AllowVerifiedContentAnchorHostSwitchCandidate
        }
        HlsManifestCommitKind::EmergencyAlternativeAsNewEpoch => {
            HlsManifestCommitAcceptanceMode::AllowVerifiedEmergencyHostSwitchCandidate
        }
        HlsManifestCommitKind::Pinned => fallback,
    }
}

pub(super) fn selected_manifest_candidate(plan: HlsManifestCommitPlan) -> Option<(usize, HlsManifestCommitKind)> {
    match plan {
        HlsManifestCommitPlan::Commit { candidate_index, kind }
        | HlsManifestCommitPlan::StageAlternative { candidate_index, kind } => Some((candidate_index, kind)),
        HlsManifestCommitPlan::HoldAlternative | HlsManifestCommitPlan::RejectAll => None,
    }
}

async fn fetch_manifest_recovery_burst_candidates(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> HlsManifestRecoveryBurstCollection {
    let mut tasks = JoinSet::new();
    let candidates = burst_plan.total_candidates();
    for candidate_index in 0..candidates {
        let context = context.clone();
        let binding = plan.binding.clone();
        let reject_reason = plan.reject_reason.cloned();
        let attempt_index = plan.attempt_index;
        let attempts = plan.attempts;
        tasks.spawn(async move {
            let stagger_ms = u64::try_from(burst_plan.slot_for_candidate(candidate_index))
                .unwrap_or_default()
                .saturating_mul(HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS);
            if stagger_ms > 0 {
                tokio::time::sleep(Duration::from_millis(stagger_ms)).await;
            }
            let request = HlsOriginManifestFetchRequest::recovery_direct_target(
                &context,
                &binding,
                reject_reason.as_ref(),
                ManifestRecoveryAttemptLogContext { attempt_index, attempts, candidate_index, candidates },
            );
            let fetch_started = Instant::now();
            let result = fetch_hls_origin_manifest_request(request).await;
            let fetch_elapsed_ms = u64::try_from(fetch_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            (candidate_index, fetch_elapsed_ms, result)
        });
    }

    let mut last_fetch_error = None;
    let mut last_reject_reason = None;
    let mut fetched_candidates = Vec::new();
    let mut completed_candidates = 0_usize;
    while let Some(join_result) = tasks.join_next().await {
        completed_candidates = completed_candidates.saturating_add(1);
        let Ok((candidate_index, fetch_elapsed_ms, result)) = join_result else {
            last_fetch_error = Some(OriginManifestFetchError::Request("manifest recovery task failed".to_string()));
            continue;
        };
        match result {
            Ok(fetched) => {
                match score_manifest_recovery_candidate_with_logging(
                    context,
                    candidate_index,
                    candidates,
                    &fetched,
                    plan.acceptance_mode,
                )
                .await
                {
                    Ok(report) => {
                        fetched_candidates.push(HlsManifestRecoveryCandidate {
                            candidate_index,
                            fetch_elapsed_ms,
                            fetched,
                            report,
                        });
                    }
                    Err(reason) => last_reject_reason = Some(reason),
                }
            }
            Err(err) => {
                last_fetch_error = Some(err);
            }
        }
    }
    HlsManifestRecoveryBurstCollection {
        fetched_candidates,
        completed_candidates,
        last_fetch_error,
        last_reject_reason,
    }
}

fn recovery_burst_plan(
    context: &HlsOriginManifestFetchContext,
    current_burst_is_full_plan: bool,
) -> HlsManifestRecoveryBurstPlan {
    if current_burst_is_full_plan {
        context.manifest_recovery_burst.level.plan()
    } else {
        HlsManifestRecoveryBurstLevel::Off.plan()
    }
}
