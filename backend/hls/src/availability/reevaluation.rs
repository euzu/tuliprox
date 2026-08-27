//! The availability reevaluation worker.
//!
//! One owner per session re-asks "is this session playable?" until the answer
//! settles or the deadline passes: schedule an attempt, evaluate the acceptance
//! directive it produces, hand off to the next owner or finish the cycle. The
//! backoff and the attempt/deadline caps that bound the loop live here too.

use super::{
    current_time_millis, hls_manifest_acceptance_directive_for_reevaluation, HlsManifestAcceptanceDirective,
    HlsTerminalFailedClosedReason, HLS_AVAILABILITY_REEVALUATION_DEADLINE_MS,
    HLS_AVAILABILITY_REEVALUATION_MAX_ATTEMPTS, HLS_AVAILABILITY_REEVALUATION_MAX_BACKOFF_MS,
};
use crate::{
    availability_reevaluation::{
        HlsAvailabilityOwnerHandoffDecision, HlsAvailabilityReevaluationFinishDecision,
        HlsAvailabilityReevaluationFinishReason, HlsAvailabilityReevaluationMode, HlsAvailabilityReevaluationOwnerKey,
        HlsAvailabilityReevaluationOwnership, HlsAvailabilityReevaluationRegistration,
    },
    hls_ctx::HlsCtx,
    post_refresh_availability::{
        evaluate_active_terminal_leases_for_reevaluation, evaluate_owner_failure_fallback, wait_for_owner_resolution,
        HlsLiveReserveDeadline, HlsPostRefreshOwnerResolution, HlsPostRefreshOwnerWaitOutcome,
        HlsPostRefreshTerminalEvaluation,
    },
    refresh::{
        maybe_trigger_origin_refresh_with_outcome, HlsOriginRefreshTriggerOutcome, HlsPostRefreshAvailabilityAction,
        OriginRefreshRequest,
    },
    session_store::HlsSessionHandle,
};
use log::warn;
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HlsAvailabilityReevaluationAttempt {
    Evaluated(Box<HlsManifestAcceptanceDirective>),
    StateContention,
    Superseded,
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
pub(super) enum HlsAvailabilityAttemptSchedule {
    Backoff,
    RefreshCompletion,
    DebouncedUntil { retry_at_ms: u64 },
}

impl HlsAvailabilityAttemptSchedule {
    pub(super) fn wake_at_ms(self, now_ms: u64, attempts_completed: u8, cycle_deadline_ms: u64) -> Option<u64> {
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
pub(super) enum HlsAvailabilityRefreshTriggerDecision {
    FinishCycle,
    Wait(HlsAvailabilityAttemptSchedule),
    Handoff,
}

pub(super) const fn availability_refresh_trigger_decision(
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
        crate::safe_proxy_session_id(&owner_key.proxy_session_id),
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
                crate::safe_proxy_session_id(&owner_key.proxy_session_id)
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

pub(super) fn register_hls_availability_reevaluation_with_mode(
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
