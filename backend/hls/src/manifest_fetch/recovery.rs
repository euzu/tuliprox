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
        manifest_acceptance_episode_snapshot, manifest_acceptance_exhaustion_reason,
        manifest_acceptance_generation_is_current, manifest_candidate_observation,
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
        is_same_host_reset_candidate, record_deterministic_conflict_receipt,
    },
    http::ManifestRecoveryAttemptLogContext,
    next_retry_delay_ms,
    quality::score_manifest_recovery_candidate_with_logging,
    selection_log::{
        log_manifest_recovery_candidate_rejected, log_manifest_recovery_selected, log_manifest_retry_scheduled,
        ManifestRetryLogKind,
    },
    FetchedOriginManifest, HlsManifestAttemptContext, HlsManifestCommitAcceptanceMode, HlsManifestFetchSelection,
    HlsManifestRecoveryCandidateScoreReport, HlsManifestRecoveryDiagnostics, HlsOriginManifestFetchContext,
    HlsOriginManifestFetchRequest,
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
use log::debug;
use shared::model::{HlsManifestRecoveryBurstLevel, HlsManifestRecoveryBurstPlan};
use std::{
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

#[cfg(test)]
tokio::task_local! {
    /// Signalled when a probe slot observes its mirror error and starts waiting for the in-flight probe.
    pub(crate) static PROBE_SLOT_MIRROR_ERROR_OBSERVED: std::sync::Arc<tokio::sync::Notify>;
}

enum HlsManifestRecoveryAttemptError<T> {
    Fetch(OriginManifestFetchError),
    Rejected(HlsManifestRejectLogReason),
    Requalified(HlsManifestAcceptanceGeneration),
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
    /// Origin requests actually issued by the joined slots, including a probe
    /// and the bound mirror request that share one slot.
    candidate_requests: usize,
    last_fetch_error: Option<OriginManifestFetchError>,
    last_reject_reason: Option<HlsManifestRejectLogReason>,
}

pub async fn retry_hls_origin_manifest_recovery_chain<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    binding: HlsManifestOriginBinding,
    reject_reason: Option<HlsManifestRejectLogReason>,
    initial_deterministic_conflict: Option<HlsDeterministicTimelineConflict>,
    trigger: HlsManifestAcceptanceTrigger,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    commit: C,
) -> Result<T, OriginManifestFetchError>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    retry_hls_origin_manifest_recovery_chain_with_diagnostics(
        context,
        binding,
        reject_reason,
        initial_deterministic_conflict,
        trigger,
        acceptance_mode,
        commit,
        &mut HlsManifestRecoveryDiagnostics::default(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[expect(clippy::too_many_lines, reason = "One loop owns admission, requalification and the shared retry budget.")]
pub(crate) async fn retry_hls_origin_manifest_recovery_chain_with_diagnostics<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    binding: HlsManifestOriginBinding,
    mut reject_reason: Option<HlsManifestRejectLogReason>,
    initial_deterministic_conflict: Option<HlsDeterministicTimelineConflict>,
    trigger: HlsManifestAcceptanceTrigger,
    acceptance_mode: HlsManifestCommitAcceptanceMode,
    mut commit: C,
    diagnostics: &mut HlsManifestRecoveryDiagnostics,
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
    let (resume, mut expected_generation) = {
        let session = context.session.read().await;
        let resume = session.origin_qualification_retry_target().is_some();
        (resume, resume.then_some(session.origin_control.acceptance_generation))
    };
    let mut next_attempt_is_full_plan = !resume;
    let mut requalifications = 0_u8;
    let mut attempt_index = usize::from(resume);
    let mut attempt_limit = if resume { 2 } else { attempts };
    let mut scheduled_retry_delay_ms = None;
    while attempt_index < attempt_limit {
        context.ensure_current_origin().await?;
        let shared_index = context.attempt.as_ref().map_or(attempt_index, |attempt| attempt.logical_attempts());
        let admission_delay_paid = context.attempt.as_ref().is_some_and(|attempt| attempt.admission_backoff_paid());
        if context.attempt.as_ref().is_some_and(|attempt| !attempt.begin_logical_attempt()) {
            return Err(last_error);
        }
        // A materially changed landscape is requalified immediately. Once
        // authorized, that new generation must still receive its complete
        // configured burst rather than losing candidates to a retry delay.
        let delay_ms = if admission_delay_paid || (attempt_index > 0 && next_attempt_is_full_plan) {
            0
        } else {
            scheduled_retry_delay_ms.take().unwrap_or_else(|| {
                let jitter = context.retry_policy.sample_jitter_ms();
                context.retry_policy.delay_for_attempt_ms(shared_index, jitter).unwrap_or_default()
            })
        };
        let acceptance_deadline = current_acceptance_deadline(context, expected_generation).await?;
        if !resume
            && !acceptance_attempt_may_start(
                next_attempt_is_full_plan,
                current_time_millis(),
                delay_ms,
                acceptance_deadline,
            )
        {
            return Err(last_error);
        }
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        let remaining_attempts = context
            .attempt
            .as_ref()
            .map_or_else(|| attempts.saturating_sub(attempt_index + 1), |attempt| attempt.remaining_attempts());
        let may_retry = remaining_attempts > 0 && attempt_index.saturating_add(1) < attempt_limit;
        let attempt_plan = HlsManifestRecoveryAttemptPlan {
            binding: &binding,
            attempt_index,
            attempts: attempt_limit,
            reject_reason: reject_reason.as_ref(),
            initial_deterministic_conflict: initial_deterministic_conflict.as_ref(),
            acceptance_mode,
            trigger,
            current_burst_is_full_plan: next_attempt_is_full_plan,
            // Do not replace the complete episode unless its mandatory full
            // burst still fits in this refresh's shared logical budget.
            may_requalify: remaining_attempts > 0
                && requalifications < HLS_MANIFEST_ACCEPTANCE_MAX_REQUALIFICATIONS_PER_REFRESH,
            requalification_window: if resume && attempt_index == 1 {
                HlsManifestRequalificationWindow::ResumedAfterSharedBackoff
            } else {
                HlsManifestRequalificationWindow::CurrentEpisode(acceptance_deadline)
            },
            completed_candidate_requests: diagnostics.candidate_requests(),
        };
        next_attempt_is_full_plan = false;
        match fetch_and_commit_manifest_recovery_attempt(
            context,
            attempt_plan,
            &mut expected_generation,
            &mut commit,
            diagnostics,
        )
        .await
        {
            HlsManifestRecoveryAttemptError::Committed(committed) => return Ok(committed),
            HlsManifestRecoveryAttemptError::Requalified(generation) => {
                // Only an explicitly authorized requalification may transfer chain ownership.
                expected_generation = Some(generation);
                requalifications = requalifications.saturating_add(1);
                next_attempt_is_full_plan = true;
                last_error = OriginManifestFetchError::RetryExhausted;
                attempt_limit = attempt_limit_for_started_requalification(attempt_limit, attempt_index, attempts);
            }
            HlsManifestRecoveryAttemptError::Rejected(reason) if may_retry => {
                scheduled_retry_delay_ms =
                    Some(log_manifest_recovery_retry(context, attempt_index, Some(&reason), None).await);
                reject_reason = Some(reason);
                last_error = OriginManifestFetchError::RetryExhausted;
            }
            HlsManifestRecoveryAttemptError::Rejected(_reason) => {
                return Err(OriginManifestFetchError::RetryExhausted);
            }
            HlsManifestRecoveryAttemptError::Fetch(err)
                if is_hls_retryable_manifest_reject_fetch_error(&err) && may_retry =>
            {
                scheduled_retry_delay_ms =
                    Some(log_manifest_recovery_retry(context, attempt_index, None, Some(&err)).await);
                last_error = err;
            }
            HlsManifestRecoveryAttemptError::Fetch(err) => return Err(err),
        }
        attempt_index = attempt_index.saturating_add(1);
    }

    Err(last_error)
}

async fn log_manifest_recovery_retry(
    context: &HlsOriginManifestFetchContext,
    attempt_index: usize,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    fetch_error: Option<&OriginManifestFetchError>,
) -> u64 {
    let next_index = context.attempt.as_ref().map_or(attempt_index + 1, |attempt| attempt.logical_attempts());
    let delay_ms = context
        .retry_policy
        .delay_for_attempt_ms(next_index, context.retry_policy.sample_jitter_ms())
        .unwrap_or_default();
    log_manifest_retry_scheduled(
        context,
        ManifestRetryLogKind::PinnedHostRecovery,
        next_index.saturating_sub(1),
        context.retry_policy.attempt_count(),
        delay_ms,
        reject_reason,
        fetch_error,
    )
    .await;
    delay_ms
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

async fn current_acceptance_deadline(
    context: &HlsOriginManifestFetchContext,
    expected_generation: Option<HlsManifestAcceptanceGeneration>,
) -> Result<HlsAcceptanceDeadlineMs, OriginManifestFetchError> {
    let session = context.session.read().await;
    if expected_generation.is_some_and(|generation| !manifest_acceptance_generation_is_current(&session, generation)) {
        return Err(OriginManifestFetchError::RecoveryUnavailable {
            reason: super::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
        });
    }
    Ok(session.origin_control.acceptance_episode.as_ref().map_or_else(
        || HlsAcceptanceDeadlineMs::from_millis_since_epoch(u64::MAX),
        |episode| episode.timing().acceptance_deadline,
    ))
}

pub(super) fn attempt_limit_for_started_requalification(
    attempt_limit: usize,
    attempt_index: usize,
    budget_limit: usize,
) -> usize {
    // A resumed reduced check may use a following slot for its mandatory
    // full burst, but never extend the configured logical attempt budget.
    attempt_limit.max(attempt_index.saturating_add(2)).min(budget_limit)
}

/// A due session retry can open a fresh episode only after changed origin evidence.
pub(super) enum HlsManifestRequalificationWindow {
    CurrentEpisode(HlsAcceptanceDeadlineMs),
    ResumedAfterSharedBackoff,
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
    requalification_window: HlsManifestRequalificationWindow,
    completed_candidate_requests: usize,
}

async fn fetch_and_commit_manifest_recovery_attempt<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: HlsManifestRecoveryAttemptPlan<'_>,
    expected_generation: &mut Option<HlsManifestAcceptanceGeneration>,
    commit: &mut C,
    diagnostics: &mut HlsManifestRecoveryDiagnostics,
) -> HlsManifestRecoveryAttemptError<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let burst_plan = recovery_burst_plan(context, plan.current_burst_is_full_plan);
    fetch_and_commit_manifest_recovery_burst_attempt(
        context,
        plan,
        burst_plan,
        expected_generation,
        commit,
        diagnostics,
    )
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "Hard failure cancellation precedes the existing acceptance and commit sequence."
)]
async fn fetch_and_commit_manifest_recovery_burst_attempt<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    expected_generation: &mut Option<HlsManifestAcceptanceGeneration>,
    commit: &mut C,
    diagnostics: &mut HlsManifestRecoveryDiagnostics,
) -> HlsManifestRecoveryAttemptError<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let episode_generation = begin_manifest_acceptance_episode(context, &plan, burst_plan, *expected_generation).await;
    if episode_generation.is_none() {
        return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::RecoveryUnavailable {
            reason: super::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
        });
    }
    *expected_generation = episode_generation;
    let HlsManifestRecoveryBurstCollection {
        fetched_candidates,
        completed_candidates,
        candidate_requests,
        last_fetch_error,
        mut last_reject_reason,
    } = fetch_manifest_recovery_burst_candidates(context, &plan, burst_plan).await;
    let last_fetch_error = match last_fetch_error {
        Some(error) if crate::refresh::failure::manifest_hard_fetch_error(&error) => {
            return HlsManifestRecoveryAttemptError::Fetch(error);
        }
        error => error,
    };
    diagnostics.record_attempt(plan.attempt_index + 1, candidate_requests, burst_plan.total_candidates() > 1);
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
            HlsManifestRecoveryBurstEvaluationOutcome::Requalified(generation) => {
                return HlsManifestRecoveryAttemptError::Requalified(generation);
            }
            HlsManifestRecoveryBurstEvaluationOutcome::Exhausted => {
                return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::RetryExhausted);
            }
            HlsManifestRecoveryBurstEvaluationOutcome::Superseded => {
                return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::RecoveryUnavailable {
                    reason: super::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
                });
            }
        };
    let outcome = commit_manifest_recovery_selection(
        context,
        &plan,
        burst_plan,
        episode_generation,
        &evaluation,
        &fetched_candidates,
        candidate_requests,
        commit,
    )
    .await;
    let selection_resumed = match outcome {
        HlsSelectedManifestRecoveryCommit::Committed(committed) => {
            return HlsManifestRecoveryAttemptError::Committed(committed);
        }
        HlsSelectedManifestRecoveryCommit::Rejected(reason) => {
            last_reject_reason = Some(reason);
            true
        }
        HlsSelectedManifestRecoveryCommit::SelectionRejected(HlsRecoveryWorkloadBindingUpdate::StaleGeneration) => {
            return HlsManifestRecoveryAttemptError::Fetch(OriginManifestFetchError::RecoveryUnavailable {
                reason: super::HlsManifestRecoveryUnavailableReason::BindingSuperseded,
            });
        }
        HlsSelectedManifestRecoveryCommit::SelectionRejected(_) => {
            last_reject_reason = Some(HlsManifestRejectLogReason::StagedSwitchInvalidated);
            false
        }
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
        HlsSelectedManifestRecoveryCommit::NotSelected => false,
    };

    hold_manifest_recovery_burst(
        context,
        episode_generation,
        &evaluation.observations,
        evaluation.held_alternative,
        evaluation.next_retry_at_ms,
        plan.current_burst_is_full_plan || selection_resumed,
    )
    .await;
    if let Some(reason) = last_reject_reason {
        return HlsManifestRecoveryAttemptError::Rejected(reason);
    }
    HlsManifestRecoveryAttemptError::Fetch(last_fetch_error.unwrap_or(OriginManifestFetchError::RetryExhausted))
}

/// Try eligible selections from the collected burst. A failed candidate keeps
/// its manifest evidence, but can no longer be selected for another commit.
#[allow(clippy::too_many_arguments)]
async fn commit_manifest_recovery_selection<T, C, Fut>(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
    episode_generation: Option<HlsManifestAcceptanceGeneration>,
    evaluation: &HlsManifestRecoveryBurstEvaluation,
    fetched_candidates: &[HlsManifestRecoveryCandidate],
    candidate_requests: usize,
    commit: &mut C,
) -> HlsSelectedManifestRecoveryCommit<T>
where
    C: FnMut(FetchedOriginManifest, HlsManifestCommitAcceptanceMode) -> Fut,
    Fut: Future<Output = Result<T, HlsManifestCommitError>>,
{
    let _probe_round = crate::refresh::anchor_probe::HlsAnchorProbeRoundGuard::install(
        &context.session,
        burst_plan.total_candidates(),
    )
    .await;
    let mut acceptance_plan = evaluation.acceptance_plan;
    let mut excluded_candidates = Vec::new();
    loop {
        let selected = selected_manifest_candidate(acceptance_plan);
        let outcome = commit_selected_manifest_recovery_candidate(
            context,
            plan,
            burst_plan,
            episode_generation,
            acceptance_plan,
            fetched_candidates,
            candidate_requests,
            commit,
        )
        .await;
        if !matches!(outcome, HlsSelectedManifestRecoveryCommit::Rejected(_)) {
            return outcome;
        }
        let Some((index, _)) = selected else { return outcome };
        let Some(candidate) = fetched_candidates.iter().find(|candidate| candidate.candidate_index == index) else {
            return outcome;
        };
        let identity = HlsManifestRecoveryCandidateIdentity::from_candidate(
            index,
            candidate.report.quality.effective_host.as_deref(),
            &candidate.fetched.body,
        );
        let mut session = context.session.write().await;
        let content_probe = matches!(selected, Some((_, HlsManifestCommitKind::ContentVerifiedAlternative)));
        if evaluation.minimum_target_duration.is_none()
            && !content_probe
            && !crate::refresh::anchor_probe::candidate_has_final_mismatch(&session, identity)
        {
            return outcome;
        }
        let Some(generation) = episode_generation else { return outcome };
        let Some(episode) = session.origin_control.acceptance_episode.as_mut() else { return outcome };
        if !episode.release_failed_candidate(generation, identity) {
            return outcome;
        }
        excluded_candidates.push(index);
        acceptance_plan = evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
            preferred_min_target_duration_secs: evaluation.minimum_target_duration,
            excluded_candidates: &excluded_candidates,
            full_burst_completed: episode.full_burst_completed,
            current_burst_is_full_plan: plan.current_burst_is_full_plan,
            trigger: episode.trigger(),
            previous_alternative: episode.held_alternative.as_ref(),
            observations: &evaluation.observations,
        });
        if selected_manifest_candidate(acceptance_plan).is_none() {
            return outcome;
        }
        episode.state = manifest_acceptance_state_for_plan(&acceptance_plan);
    }
}

async fn hold_manifest_recovery_burst(
    context: &HlsOriginManifestFetchContext,
    episode_generation: Option<HlsManifestAcceptanceGeneration>,
    observations: &[HlsManifestCandidateObservation],
    held_alternative: Option<HlsAlternativeOriginCohort>,
    next_retry_at_ms: u64,
    record_exhaustion: bool,
) {
    if record_exhaustion {
        record_manifest_acceptance_exhaustion(
            context,
            episode_generation,
            manifest_acceptance_exhaustion_reason(observations),
        )
        .await;
    }
    hold_uncommitted_manifest_acceptance_episode(context, episode_generation, held_alternative, next_retry_at_ms).await;
}

struct HlsManifestRecoveryBurstEvaluation {
    observations: Vec<HlsManifestCandidateObservation>,
    minimum_target_duration: Option<shared::model::Secs>,
    acceptance_plan: HlsManifestCommitPlan,
    held_alternative: Option<HlsAlternativeOriginCohort>,
    next_retry_at_ms: u64,
}

enum HlsManifestRecoveryBurstEvaluationOutcome {
    Continue(Box<HlsManifestRecoveryBurstEvaluation>),
    DeterministicConflict(Box<HlsDeterministicTimelineConflict>),
    Requalified(HlsManifestAcceptanceGeneration),
    Exhausted,
    Superseded,
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

async fn qualification_duration_preference(
    context: &HlsOriginManifestFetchContext,
    observations: &[HlsManifestCandidateObservation],
    episode_generation: Option<HlsManifestAcceptanceGeneration>,
) -> Option<shared::model::Secs> {
    let mut session = context.session.write().await;
    if !manifest_acceptance_generation_is_current(&session, episode_generation?) {
        return None;
    }
    for observation in observations {
        crate::refresh::observe_target_duration(
            &mut session,
            observation.target_duration_secs,
            observation.effective_host.as_deref(),
        );
    }
    session
        .origin_control
        .target_duration_qualification
        .as_ref()
        .filter(|check| check.result.is_none() && check.minimum.get() > 0)
        .map(|check| check.minimum)
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
    // Pressure comes from the authorized snapshot (or guarded promotion);
    // `Recovering` itself is deliberately not reserve evidence.
    let resource_evidence = committed_resource_evidence(context).await;
    let candidate_evaluations = fetched_candidates
        .iter()
        .map(|candidate| {
            let observation = manifest_candidate_observation(
                candidate,
                burst_plan,
                resource_evidence.anchor.as_ref(),
                &resource_evidence.published_identities,
            );
            let deterministic_conflict = deterministic_timeline_conflict_for_candidate(candidate, &resource_evidence);
            (observation, deterministic_conflict)
        })
        .collect::<Vec<_>>();
    let observations = candidate_evaluations.iter().map(|(observation, _)| observation.clone()).collect::<Vec<_>>();
    let minimum_target_duration = qualification_duration_preference(context, &observations, episode_generation).await;
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
    let Some(episode) = manifest_acceptance_episode_snapshot(context, episode_generation).await else {
        // An old response must not let the next attempt join a successor episode.
        return HlsManifestRecoveryBurstEvaluationOutcome::Superseded;
    };
    let mut acceptance_plan = evaluate_manifest_acceptance(HlsManifestAcceptanceInput {
        preferred_min_target_duration_secs: minimum_target_duration,
        excluded_candidates: &[],
        full_burst_completed: episode.full_burst_completed,
        current_burst_is_full_plan: plan.current_burst_is_full_plan,
        trigger: episode.trigger,
        previous_alternative: episode.held_alternative.as_ref(),
        observations: &observations,
    });
    let reduced_landscape_changed = !plan.current_burst_is_full_plan
        && episode
            .observed_landscape
            .as_ref()
            .map(|landscape| classify_reduced_retry_landscape(landscape, &observations))
            .is_some_and(HlsReducedRetryLandscapeChange::requires_full_requalification);
    let can_verify = followup_can_verify_without_landscape(context, plan, Some(&episode), acceptance_plan);
    if reduced_landscape_changed && !can_verify {
        if plan.may_requalify {
            if let Some(generation) = begin_requalified_manifest_acceptance_episode(
                context,
                episode_generation,
                plan.trigger,
                &plan.requalification_window,
            )
            .await
            {
                return HlsManifestRecoveryBurstEvaluationOutcome::Requalified(generation);
            }
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
            episode.held_alternative,
            next_retry_at_ms,
        )
        .await;
        return HlsManifestRecoveryBurstEvaluationOutcome::Exhausted;
    }
    if matches!(acceptance_plan, HlsManifestCommitPlan::RejectAll)
        && restart_probe_allowed(context, Some(&episode)).await
    {
        acceptance_plan = same_host_restart_probe_plan(fetched_candidates, &observations);
    }
    let held_alternative =
        held_alternative_after_burst(&observations, episode.held_alternative.as_ref(), plan.current_burst_is_full_plan);
    let next_retry_at_ms =
        current_time_millis().saturating_add(next_retry_delay_ms(&context.retry_policy, plan.attempt_index, None, 0));
    let next_state = manifest_acceptance_state_for_plan(&acceptance_plan);
    update_manifest_acceptance_episode_state(context, episode_generation, next_state).await;
    HlsManifestRecoveryBurstEvaluationOutcome::Continue(Box::new(HlsManifestRecoveryBurstEvaluation {
        observations,
        minimum_target_duration,
        acceptance_plan,
        held_alternative,
        next_retry_at_ms,
    }))
}

fn followup_can_verify_without_landscape(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    episode: Option<&super::episode::HlsManifestAcceptanceEpisodeSnapshot>,
    acceptance_plan: HlsManifestCommitPlan,
) -> bool {
    // A response from the pinned host already paid for by this logical slot
    // needs no cross-host landscape proof. Its ordinary commit guards still run.
    let pinned_progress =
        matches!(acceptance_plan, HlsManifestCommitPlan::Commit { kind: HlsManifestCommitKind::Pinned, .. });
    // A bound raw-content probe also proves its current candidate independently
    // of landscape consensus; a failed probe remains held within the same budget.
    let bound_content_probe = matches!(
        acceptance_plan,
        HlsManifestCommitPlan::StageAlternative { kind: HlsManifestCommitKind::ContentVerifiedAlternative, .. }
    );
    debug!("HLS manifest acceptance follow-up: full_plan={} completed_full_burst={} pinned_progress={} bound_content_probe={} remaining_attempts={} reason={}",
        plan.current_burst_is_full_plan, episode.is_some_and(|episode| episode.full_burst_completed),
        pinned_progress, bound_content_probe, context.attempt.as_ref().map_or_else(|| "none".to_string(), |attempt| attempt.remaining_attempts().to_string()),
        if pinned_progress { "paid-pinned-progress" } else if bound_content_probe { "current-ready-reference-probe" } else { "landscape-evaluation" });
    pinned_progress || bound_content_probe
}

async fn restart_probe_allowed(
    context: &HlsOriginManifestFetchContext,
    episode: Option<&super::episode::HlsManifestAcceptanceEpisodeSnapshot>,
) -> bool {
    episode.is_some_and(|episode| episode.full_burst_completed && episode.trigger.recovery_required())
        && !context.session.read().await.origin_qualification_awaits_publication()
}

fn same_host_restart_probe_plan(
    candidates: &[HlsManifestRecoveryCandidate],
    observations: &[HlsManifestCandidateObservation],
) -> HlsManifestCommitPlan {
    // A same-host sequence reset cannot prove progress from its metadata.
    // Let the existing commit callback stage and compare fresh raw media;
    // unchanged bytes remain a rejected recovery, within this same budget.
    candidates.iter().find(|candidate| {
        is_same_host_reset_candidate(&candidate.report.quality)
            && observations.iter().any(|observation| {
                observation.candidate_index == candidate.candidate_index
                    && observation.resource_timeline_evidence
                        == crate::manifest_acceptance::HlsResourceTimelineEvidence::Eligible
                    && observation.switch_segment_readiness
                        == crate::manifest_acceptance::HlsSwitchSegmentReadiness::RequiresStaging
                    && observation.emergency_evidence.live_handoff
                        == crate::manifest_acceptance::HlsEmergencyLiveHandoffCompatibility::RequiresStagedTrackVerification
            })
    }).map_or(HlsManifestCommitPlan::RejectAll, |candidate| HlsManifestCommitPlan::Commit {
        candidate_index: candidate.candidate_index,
        kind: HlsManifestCommitKind::Pinned,
    })
}

enum HlsSelectedManifestRecoveryCommit<T> {
    Committed(T),
    Rejected(HlsManifestRejectLogReason),
    SelectionRejected(HlsRecoveryWorkloadBindingUpdate),
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
    fetched_candidates: &[HlsManifestRecoveryCandidate],
    attempt_candidate_requests: usize,
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
        fetched_candidates.iter().find(|candidate| candidate.candidate_index == selected_candidate_index)
    else {
        return HlsSelectedManifestRecoveryCommit::NotSelected;
    };
    let candidate_index = candidate.candidate_index;
    let fetched = candidate.fetched.clone();
    let report = &candidate.report;
    let candidate_identity = HlsManifestRecoveryCandidateIdentity::from_candidate(
        candidate_index,
        report.quality.effective_host.as_deref(),
        &fetched.body,
    );
    let selection_update = select_manifest_recovery_candidate(
        context,
        episode_generation,
        candidate_identity,
        plan.current_burst_is_full_plan,
    )
    .await;
    if selection_update != HlsRecoveryWorkloadBindingUpdate::Applied {
        return HlsSelectedManifestRecoveryCommit::SelectionRejected(selection_update);
    }
    let acceptance_mode = selected_commit_acceptance_mode(selected_commit_kind, plan.acceptance_mode);
    let selection = if burst_plan.total_candidates() > 1 {
        HlsManifestFetchSelection::Burst
    } else {
        HlsManifestFetchSelection::Recovery
    };
    let candidate_requests = plan.completed_candidate_requests.saturating_add(attempt_candidate_requests);
    match commit(
        fetched.with_recovery_diagnostics(plan.attempt_index + 1, candidate_requests, selection),
        acceptance_mode,
    )
    .await
    {
        Ok(committed) => {
            complete_manifest_acceptance_episode(context, episode_generation).await;
            log_manifest_recovery_selected(context, candidate_index, burst_plan.total_candidates(), report).await;
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
    current_burst_is_full_plan: bool,
) -> HlsRecoveryWorkloadBindingUpdate {
    let attempt_type = if current_burst_is_full_plan { "full" } else { "reduced" };
    let Some(generation) = generation else {
        let session = context.session.read().await;
        debug!("HLS manifest candidate selection rejected: update=EpisodeInactive attempt_type={attempt_type} expected_generation=none current_generation={} episode_status={:?}",
            session.origin_control.acceptance_generation.0,
            crate::manifest_acceptance::manifest_acceptance_episode_status(session.origin_control.acceptance_episode.as_ref(), session.origin_control.acceptance_generation, current_time_millis()));
        return HlsRecoveryWorkloadBindingUpdate::EpisodeInactive;
    };
    let mut session = context.session.write().await;
    let current_generation = session.origin_control.acceptance_generation;
    let Some(episode) = session.origin_control.acceptance_episode.as_mut() else {
        debug!("HLS manifest candidate selection rejected: update=StaleGeneration attempt_type={attempt_type} expected_generation={} current_generation={} episode_status=Missing", generation.0, current_generation.0);
        return HlsRecoveryWorkloadBindingUpdate::StaleGeneration;
    };
    let update = if current_burst_is_full_plan {
        episode.select_candidate(generation, candidate_identity)
    } else {
        episode.select_candidate_for_reduced_retry(generation, candidate_identity)
    };
    if update != HlsRecoveryWorkloadBindingUpdate::Applied {
        debug!(
            "HLS manifest candidate selection rejected: update={update:?} attempt_type={attempt_type} expected_generation={} current_generation={} episode_status={:?}",
            generation.0,
            episode.generation.0,
            crate::manifest_acceptance::manifest_acceptance_episode_status(Some(episode), current_generation, current_time_millis()),
        );
    }
    update
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

struct PinnedProfileProbe {
    binding: HlsManifestOriginBinding,
    /// Absolute acceptance deadline the slot may never outlive.
    acceptance_deadline: HlsAcceptanceDeadlineMs,
}

// The first reduced observation can revisit the trusted profile endpoint that
// supplied the published timeline. Never reuse an earlier account's signed URL.
async fn pinned_profile_followup(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
) -> Option<PinnedProfileProbe> {
    let HlsManifestRequalificationWindow::CurrentEpisode(deadline) = &plan.requalification_window else {
        return None;
    };
    let entry = &context.origin_entry;
    if plan.trigger != HlsManifestAcceptanceTrigger::Observe
        || plan.current_burst_is_full_plan
        || plan.attempt_index != 1
        || !entry.profile_recovery_failover()
        || entry.url().scheme() != "provider"
    {
        return None;
    }
    let profile = entry.url_failover_provider()?;
    if profile.provider_url_selection_policy() != shared::model::ProviderUrlSelectionPolicy::RestartFromFirst
        || plan.binding.provider_url_index()? == 0
    {
        return None;
    }
    let input = entry.to_input_source();
    let resolve = |index| super::http::resolved_hls_manifest_request_url_from_input(&input, Some(index), entry.url());
    if &resolve(plan.binding.provider_url_index()?) != plan.binding.request_url() {
        return None;
    }
    let primary = resolve(0);
    let state = context.session.read().await;
    let pinned = state.origin_control.pinned_host.as_deref()?;
    if !state.origin_control.acceptance_episode.as_ref()?.full_burst_completed
        || primary.host_str() != Some(pinned)
        || plan.binding.request_url().host_str() == Some(pinned)
        || primary.scheme() != plan.binding.request_url().scheme()
    {
        return None;
    }
    // The probe shares its slot with the bound mirror request, so it may use
    // the whole remaining acceptance window instead of one retry interval.
    if deadline.as_millis_since_epoch() <= current_time_millis() {
        return None;
    }
    Some(PinnedProfileProbe {
        binding: HlsManifestOriginBinding::new(primary, Some(0)).ok()?,
        acceptance_deadline: *deadline,
    })
}

/// A request that races inside a probe slot reports to a detached attempt with the
/// shared attempt's playback policy. A discarded result must not close or fail the
/// shared attempt, while 407 handling and failover limits stay exactly as they are
/// for the bound request. Without a shared attempt the request stays unbound.
fn detached_slot_attempt(context: &HlsOriginManifestFetchContext) -> Option<Arc<HlsManifestAttemptContext>> {
    context.attempt.as_deref().map(|shared| {
        Arc::new(HlsManifestAttemptContext::new(
            shared.requirement,
            shared.active_live_playback,
            shared.revalidation_generation,
        ))
    })
}

fn pinned_profile_probe_slot(
    context: &HlsOriginManifestFetchContext,
    probe: PinnedProfileProbe,
) -> (HlsOriginManifestFetchContext, HlsManifestOriginBinding, HlsAcceptanceDeadlineMs) {
    let mut probe_context = context.clone();
    crate::scrub_hls_origin_headers(&mut probe_context.headers, None);
    probe_context.origin_entry = probe_context.origin_entry.with_profile_recovery_failover(false);
    // The slot already owns its logical attempt. A failed probe must not close the shared
    // attempt, or the concurrent bound mirror request would be refused.
    probe_context.attempt = detached_slot_attempt(context);
    debug!(
        "HLS reduced manifest retry revisits pinned profile endpoint: deadline_epoch_ms={}",
        probe.acceptance_deadline.as_millis_since_epoch()
    );
    (probe_context, probe.binding, probe.acceptance_deadline)
}

async fn fetch_manifest_recovery_burst_candidates(
    context: &HlsOriginManifestFetchContext,
    plan: &HlsManifestRecoveryAttemptPlan<'_>,
    burst_plan: HlsManifestRecoveryBurstPlan,
) -> HlsManifestRecoveryBurstCollection {
    let mut tasks = JoinSet::new();
    let candidates = burst_plan.total_candidates();
    let mut probe = pinned_profile_followup(context, plan).await;
    for candidate_index in 0..candidates {
        let context = context.clone();
        let probe = probe.take().map(|probe| pinned_profile_probe_slot(&context, probe));
        let binding = plan.binding.clone();
        let reject_reason = plan.reject_reason.cloned();
        let attempt_index = plan.attempt_index;
        let attempts = plan.attempts;
        #[cfg(test)]
        let probe_slot_mirror_error_observed = PROBE_SLOT_MIRROR_ERROR_OBSERVED.try_with(std::sync::Arc::clone).ok();
        tasks.spawn(async move {
            let candidate_future = async move {
                let stagger_ms = u64::try_from(burst_plan.slot_for_candidate(candidate_index))
                    .unwrap_or_default()
                    .saturating_mul(HLS_MANIFEST_RECOVERY_BURST_SLOT_DELAY_MS);
                if stagger_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(stagger_ms)).await;
                }
                let log = ManifestRecoveryAttemptLogContext { attempt_index, attempts, candidate_index, candidates };
                let fetch_started = Instant::now();
                let (origin_requests, result) =
                    fetch_probed_recovery_candidate(&context, probe, &binding, reject_reason.as_ref(), log).await;
                let fetch_elapsed_ms = u64::try_from(fetch_started.elapsed().as_millis()).unwrap_or(u64::MAX);
                (candidate_index, fetch_elapsed_ms, origin_requests, result)
            };
            #[cfg(test)]
            if let Some(signal) = probe_slot_mirror_error_observed {
                return PROBE_SLOT_MIRROR_ERROR_OBSERVED.scope(signal, candidate_future).await;
            }
            candidate_future.await
        });
    }

    let mut last_fetch_error = None;
    let mut last_reject_reason = None;
    let mut fetched_candidates = Vec::new();
    let mut completed_candidates = 0_usize;
    let mut origin_requests = 0_usize;
    while let Some(join_result) = tasks.join_next().await {
        completed_candidates = completed_candidates.saturating_add(1);
        let Ok((candidate_index, fetch_elapsed_ms, slot_requests, result)) = join_result else {
            last_fetch_error = Some(OriginManifestFetchError::Request("manifest recovery task failed".to_string()));
            continue;
        };
        origin_requests = origin_requests.saturating_add(slot_requests);
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
                if crate::refresh::failure::manifest_hard_fetch_error(&err) {
                    tasks.abort_all();
                    return HlsManifestRecoveryBurstCollection {
                        fetched_candidates: Vec::new(),
                        completed_candidates,
                        candidate_requests: origin_requests,
                        last_fetch_error: Some(err),
                        last_reject_reason: None,
                    };
                }
                last_fetch_error = Some(err);
            }
        }
    }
    HlsManifestRecoveryBurstCollection {
        fetched_candidates,
        completed_candidates,
        candidate_requests: origin_requests,
        last_fetch_error,
        last_reject_reason,
    }
}

/// A failed pinned primary probe must not cost the slot its bound mirror request.
/// Both requests share the slot: the probe is preferred when it succeeds, and
/// the slot is bounded by the remaining acceptance deadline.
async fn fetch_probed_recovery_candidate(
    context: &HlsOriginManifestFetchContext,
    probe: Option<(HlsOriginManifestFetchContext, HlsManifestOriginBinding, HlsAcceptanceDeadlineMs)>,
    binding: &HlsManifestOriginBinding,
    reject_reason: Option<&HlsManifestRejectLogReason>,
    log: ManifestRecoveryAttemptLogContext,
) -> (usize, Result<FetchedOriginManifest, OriginManifestFetchError>) {
    let Some((probe_context, primary, acceptance_deadline)) = probe else {
        let request = HlsOriginManifestFetchRequest::recovery_direct_target(context, binding, reject_reason, log);
        let dispatched = request.ensure_dispatch_allowed().await.is_ok();
        return (usize::from(dispatched), fetch_hls_origin_manifest_request(request).await);
    };
    // The slot owns the attempt semantics while it races two requests: a discarded
    // result must neither close nor fail the shared attempt, so both requests report to
    // detached attempts. Only the chosen result is classified against the shared attempt.
    let mut mirror_context = context.clone();
    mirror_context.attempt = detached_slot_attempt(context);

    let shared_closed = || context.attempt.as_ref().is_some_and(|attempt| attempt.is_closed());

    // Each request of the slot counts only when the shared pre-dispatch gate admits it:
    // the account binding and the shared attempt can refuse either request before any HTTP call.
    // The fetch repeats the same gate immediately before dispatch.
    let probe_requests = usize::from(!shared_closed() && probe_context.ensure_dispatch_allowed().await.is_ok());
    let mirror_requests = usize::from(!shared_closed() && mirror_context.ensure_dispatch_allowed().await.is_ok());
    let probe_request =
        HlsOriginManifestFetchRequest::recovery_direct_target(&probe_context, &primary, reject_reason, log);
    let mirror_request =
        HlsOriginManifestFetchRequest::recovery_direct_target(&mirror_context, binding, reject_reason, log);

    let remaining_ms = acceptance_deadline.as_millis_since_epoch().saturating_sub(current_time_millis());
    if remaining_ms == 0 {
        return (0, Err(OriginManifestFetchError::Timeout));
    }
    let acceptance_remaining = Duration::from_millis(remaining_ms);
    let probe_budget = Duration::from_millis(remaining_ms.min(probe_context.origin_manifest_timeout_ms));

    let probe_fetch = async {
        if shared_closed() {
            return Err(OriginManifestFetchError::RetryExhausted);
        }
        if probe_budget.is_zero() {
            return Err(OriginManifestFetchError::Timeout);
        }
        tokio::time::timeout(probe_budget, fetch_hls_origin_manifest_request(probe_request))
            .await
            .unwrap_or(Err(OriginManifestFetchError::Timeout))
    };
    let mirror_fetch = async {
        if shared_closed() {
            return Err(OriginManifestFetchError::RetryExhausted);
        }
        if acceptance_remaining.is_zero() {
            return Err(OriginManifestFetchError::Timeout);
        }
        tokio::time::timeout(acceptance_remaining, fetch_hls_origin_manifest_request(mirror_request))
            .await
            .unwrap_or(Err(OriginManifestFetchError::Timeout))
    };
    tokio::pin!(probe_fetch, mirror_fetch);
    // A successful mirror grants the probe one retry interval more. The nested timeout
    // never extends the probe budget or the acceptance deadline, because the probe keeps
    // its own bound and the mirror keeps the remaining acceptance window.
    let probe_grace = Duration::from_millis(
        context.retry_policy.delay_for_attempt_ms(log.attempt_index.saturating_add(1), 0).unwrap_or_default(),
    );
    let mut probe_failed = false;
    let chosen: Result<FetchedOriginManifest, OriginManifestFetchError> = loop {
        tokio::select! {
            biased;
            probe_result = &mut probe_fetch, if !probe_failed => {
                if let Ok(fetched) = probe_result {
                    break Ok(fetched);
                }
                debug!("HLS pinned profile endpoint probe failed; the slot uses its bound mirror");
                probe_failed = true;
            }
            mirror_result = &mut mirror_fetch => {
                if !probe_failed {
                    match mirror_result {
                        Ok(fetched_mirror) => {
                            // The probe keeps its own budget; a successful mirror grants it one retry
                            // interval more, whichever bound ends first.
                            let grace_result = match tokio::time::timeout(probe_grace, &mut probe_fetch).await {
                                Ok(probe_result) => probe_result,
                                Err(_) => Err(OriginManifestFetchError::Timeout),
                            };
                            if let Ok(fetched) = grace_result {
                                break Ok(fetched);
                            }
                            break Ok(fetched_mirror);
                        }
                        Err(mirror_error) => {
                            #[cfg(test)]
                            let _ = PROBE_SLOT_MIRROR_ERROR_OBSERVED.try_with(|observed| observed.notify_one());
                            // A failed mirror does not bound the probe by the grace; wait until
                            // the probe completes or its own budget/acceptance deadline expires.
                            if let Ok(fetched) = (&mut probe_fetch).await {
                                break Ok(fetched);
                            }
                            break Err(mirror_error);
                        }
                    }
                }
                break mirror_result;
            }
        }
    };
    let requests = probe_requests.saturating_add(mirror_requests);
    match chosen {
        Ok(fetched) => (requests, Ok(fetched)),
        Err(error) => (requests, Err(context.classify_current_error(error).await)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        recovery_timing::{HlsOperationTimeoutMs, HlsRecoveryEtaMs, HlsRecoveryTimingPolicy},
        HlsManifestCommitRequirement, HlsSession, HlsSessionHandle, HlsSessionKey,
    };
    use tokio::sync::RwLock;
    use tuliprox_core::model::HlsManifestRecoveryBurstConfig;

    /// A superseded account binding refuses the request before any HTTP call, so the
    /// slot must not report it.
    #[tokio::test]
    async fn superseded_binding_counts_no_origin_request() {
        let mut context = test_fetch_context();
        context.origin_account_snapshot = Some(crate::origin_account_runtime::HlsOriginAccountSnapshot {
            binding: None,
            generation: context.session.read().await.activity.origin_work_generation.saturating_add(1),
            provider_session_headers: axum::http::HeaderMap::default(),
        });
        let binding = HlsManifestOriginBinding::new(
            url::Url::parse("http://127.0.0.1:1/index.m3u8").expect("binding url"),
            Some(1),
        )
        .expect("binding");
        let (origin_requests, outcome) = fetch_probed_recovery_candidate(
            &context,
            None,
            &binding,
            None,
            ManifestRecoveryAttemptLogContext { attempt_index: 1, attempts: 2, candidate_index: 0, candidates: 1 },
        )
        .await;
        assert_eq!(origin_requests, 0, "a refused request never reached the origin");
        assert!(
            matches!(
                outcome,
                Err(OriginManifestFetchError::RecoveryUnavailable {
                    reason: crate::manifest_fetch::HlsManifestRecoveryUnavailableReason::BindingSuperseded
                })
            ),
            "{outcome:?}"
        );
    }

    /// An open shared attempt dispatches its bound request, so the slot reports it.
    #[tokio::test]
    async fn open_shared_attempt_counts_the_dispatched_origin_request() {
        let context = test_fetch_context();
        let binding = HlsManifestOriginBinding::new(
            url::Url::parse("http://127.0.0.1:1/index.m3u8").expect("binding url"),
            Some(1),
        )
        .expect("binding");
        let (origin_requests, outcome) = fetch_probed_recovery_candidate(
            &context,
            None,
            &binding,
            None,
            ManifestRecoveryAttemptLogContext { attempt_index: 1, attempts: 2, candidate_index: 0, candidates: 1 },
        )
        .await;
        assert_eq!(origin_requests, 1, "the bound request was dispatched");
        assert!(
            matches!(outcome, Err(OriginManifestFetchError::Request(_))),
            "a dispatched request against a closed port fails at the transport: {outcome:?}"
        );
    }

    /// The probe owns the slot when it wins: a mirror that the account binding refused
    /// before dispatch must not add an origin request to the slot's report.
    #[tokio::test]
    async fn probe_win_does_not_count_a_mirror_refused_before_dispatch() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe origin binds");
        let address = listener.local_addr().expect("probe origin address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("probe origin accepts");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("probe request read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nsegment.ts\n";
            let response =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.expect("probe response");
        });

        let mut context = test_fetch_context();
        // A finished mirror grants the probe one retry interval, so the slot must keep a real grace window.
        context.retry_policy =
            super::super::model::RetryPolicy { delays_ms: [0, 250, 1_000, 1_000, 1_000], jitter_max_ms: 0 };
        // The slot's own binding is superseded, so its mirror request is refused before any HTTP call.
        context.origin_account_snapshot = Some(crate::origin_account_runtime::HlsOriginAccountSnapshot {
            binding: None,
            generation: context.session.read().await.activity.origin_work_generation.saturating_add(1),
            provider_session_headers: axum::http::HeaderMap::default(),
        });
        let binding = HlsManifestOriginBinding::new(
            url::Url::parse("http://127.0.0.1:1/index.m3u8").expect("mirror binding url"),
            Some(1),
        )
        .expect("mirror binding");
        let probe_binding = HlsManifestOriginBinding::new(
            url::Url::parse(&format!("http://{address}/index.m3u8")).expect("probe binding url"),
            Some(1),
        )
        .expect("probe binding");

        let (origin_requests, outcome) = fetch_probed_recovery_candidate(
            &context,
            Some((
                test_fetch_context(),
                probe_binding,
                HlsAcceptanceDeadlineMs::from_millis_since_epoch(current_time_millis() + 1_000),
            )),
            &binding,
            None,
            ManifestRecoveryAttemptLogContext { attempt_index: 1, attempts: 2, candidate_index: 0, candidates: 1 },
        )
        .await;
        server.await.expect("probe origin completes");

        assert_eq!(origin_requests, 1, "only the probe reached the origin; the refused mirror did not");
        assert!(outcome.is_ok(), "the probe wins the slot: {:?}", outcome.as_ref().err());
    }

    /// When the shared attempt closes before dispatch, a probe slot with detached attempts
    /// must refuse both probe and mirror with `RetryExhausted` and count zero origin requests.
    #[tokio::test]
    async fn closed_shared_attempt_refuses_detached_probe_and_mirror_with_retry_exhausted() {
        let context = test_fetch_context();
        let binding = HlsManifestOriginBinding::new(
            url::Url::parse("http://127.0.0.1:1/index.m3u8").expect("mirror binding url"),
            Some(1),
        )
        .expect("mirror binding");
        let probe_binding = HlsManifestOriginBinding::new(
            url::Url::parse("http://127.0.0.1:2/index.m3u8").expect("probe binding url"),
            Some(1),
        )
        .expect("probe binding");
        let probe_context = test_fetch_context();

        // Close the shared attempt before dispatch
        context.attempt.as_ref().unwrap().finish(false);

        let (origin_requests, outcome) = fetch_probed_recovery_candidate(
            &context,
            Some((
                probe_context,
                probe_binding,
                HlsAcceptanceDeadlineMs::from_millis_since_epoch(current_time_millis() + 1_000),
            )),
            &binding,
            None,
            ManifestRecoveryAttemptLogContext { attempt_index: 1, attempts: 2, candidate_index: 0, candidates: 1 },
        )
        .await;

        assert_eq!(origin_requests, 0, "refused probe and mirror never reach the origin");
        assert!(
            matches!(outcome, Err(OriginManifestFetchError::RetryExhausted)),
            "expected RetryExhausted, got {outcome:?}"
        );
    }

    /// When the acceptance deadline has already expired before request dispatch,
    /// the probe slot computes a remaining budget of zero, refuses with Timeout,
    /// and issues zero origin requests.
    #[tokio::test]
    async fn expired_acceptance_deadline_refuses_detached_probe_and_mirror_with_timeout() {
        let context = test_fetch_context();
        let binding = HlsManifestOriginBinding::new(
            url::Url::parse("http://127.0.0.1:1/index.m3u8").expect("mirror binding url"),
            Some(1),
        )
        .expect("mirror binding");
        let probe_binding = HlsManifestOriginBinding::new(
            url::Url::parse("http://127.0.0.1:2/index.m3u8").expect("probe binding url"),
            Some(1),
        )
        .expect("probe binding");
        let probe_context = test_fetch_context();

        // Expired deadline in the past
        let expired_deadline =
            HlsAcceptanceDeadlineMs::from_millis_since_epoch(current_time_millis().saturating_sub(1_000));

        let (origin_requests, outcome) = fetch_probed_recovery_candidate(
            &context,
            Some((probe_context, probe_binding, expired_deadline)),
            &binding,
            None,
            ManifestRecoveryAttemptLogContext { attempt_index: 1, attempts: 2, candidate_index: 0, candidates: 1 },
        )
        .await;

        assert_eq!(origin_requests, 0, "an expired acceptance deadline never reaches the origin");
        assert!(matches!(outcome, Err(OriginManifestFetchError::Timeout)), "expected Timeout, got {outcome:?}");
    }

    /// When the mirror request finishes first with an error (e.g. 502), the slot must wait for
    /// the in-flight probe until it completes rather than dropping it after `probe_grace`.
    #[tokio::test]
    async fn probe_slot_waits_for_inflight_probe_when_mirror_fails_with_zero_retry_delay() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe origin binds");
        let probe_address = probe_listener.local_addr().expect("probe origin address");
        let probe_server = tokio::spawn(async move {
            let (mut socket, _) = probe_listener.accept().await.expect("probe origin accepts");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("probe request read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let body = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nsegment.ts\n";
            let response =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.expect("probe response");
        });

        let mirror_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("mirror origin binds");
        let mirror_address = mirror_listener.local_addr().expect("mirror origin address");
        let mirror_server = tokio::spawn(async move {
            let (mut socket, _) = mirror_listener.accept().await.expect("mirror origin accepts");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).await.expect("mirror request read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let response = "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            socket.write_all(response.as_bytes()).await.expect("mirror response");
        });

        let mut context = test_fetch_context();
        context.retry_policy = super::super::model::RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 };
        let binding = HlsManifestOriginBinding::new(
            url::Url::parse(&format!("http://{mirror_address}/index.m3u8")).expect("mirror binding url"),
            Some(1),
        )
        .expect("mirror binding");
        let probe_binding = HlsManifestOriginBinding::new(
            url::Url::parse(&format!("http://{probe_address}/index.m3u8")).expect("probe binding url"),
            Some(1),
        )
        .expect("probe binding");

        let (origin_requests, outcome) = fetch_probed_recovery_candidate(
            &context,
            Some((
                test_fetch_context(),
                probe_binding,
                HlsAcceptanceDeadlineMs::from_millis_since_epoch(current_time_millis() + 1_000),
            )),
            &binding,
            None,
            ManifestRecoveryAttemptLogContext { attempt_index: 1, attempts: 2, candidate_index: 0, candidates: 1 },
        )
        .await;
        probe_server.await.expect("probe server completes");
        mirror_server.await.expect("mirror server completes");

        assert_eq!(origin_requests, 2, "both probe and mirror reached the origin");
        assert!(outcome.is_ok(), "the probe wins the slot: {:?}", outcome.as_ref().err());
    }

    fn test_fetch_context() -> HlsOriginManifestFetchContext {
        let ctx = crate::HlsCtx::for_test(tuliprox_core::model::Config::default());
        let attempt = Arc::new(HlsManifestAttemptContext::new(
            HlsManifestCommitRequirement::CommittedManifestAllowed,
            true,
            None,
        ));
        assert!(attempt.begin_logical_attempt(), "the fixture opens the shared attempt");
        let session: HlsSessionHandle =
            Arc::new(RwLock::new(HlsSession::new(HlsSessionKey::new(1, "closed-attempt"), b"secret", 0)));
        let client = reqwest::Client::builder().no_proxy().build().expect("client");
        HlsOriginManifestFetchContext {
            origin_account_snapshot: None,
            attempt: Some(attempt),
            app_config: Arc::clone(&ctx.app_config),
            session,
            availability_runtime: None,
            origin_entry: super::super::model::LiveHlsOriginEntry::parse("http://127.0.0.1:1/index.m3u8")
                .expect("entry url"),
            headers: axum::http::HeaderMap::default(),
            client: client.clone(),
            no_redirect_client: client,
            use_manual_redirects: false,
            origin_manifest_timeout_ms: 1_000,
            manifest_recovery_burst: HlsManifestRecoveryBurstConfig::default(),
            retry_policy: super::super::model::RetryPolicy { delays_ms: [0, 0, 0, 0, 0], jitter_max_ms: 0 },
            recovery_timing_policy: HlsRecoveryTimingPolicy::new(
                HlsOperationTimeoutMs::from_millis(1_000),
                HlsOperationTimeoutMs::from_millis(1_000),
                HlsRecoveryEtaMs::from_millis(1_000),
                HlsRecoveryEtaMs::from_millis(1_000),
            ),
            acceptance_timing_seed: None,
            origin_telemetry: crate::origin_telemetry::OriginTelemetryRecorder::disabled(),
        }
    }
}
