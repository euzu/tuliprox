use super::{
    availability::{
        commit_terminal_tail_if_lease_reserve_requires_cutover_detailed, current_time_millis,
        HlsDetailedTerminalResolution, HlsTerminalDecisionPurpose, HlsTerminalFailedClosedReason,
        HlsTerminalResolution,
    },
    availability_reevaluation::{HlsAvailabilityReevaluationOwnership, HlsAvailabilityReevaluationRegistration},
    hls_ctx::HlsCtx,
    media_reserve::HlsLeaseReserveSnapshot,
    recovery_timing::{HlsLeaseCutoverTiming, HlsTerminalCommitAcquisitionBudgetMs},
    refresh::HlsPostRefreshAvailabilityAction,
    HlsAccessLeaseId, HlsSessionHandle, ProxySessionId,
};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HlsLiveReserveDeadline {
    pub(super) next_reevaluation_at_ms: u64,
    pub(super) latest_safe_terminal_commit_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HlsPostRefreshTerminalEvaluation {
    NoLiveLease,
    RecoveredLive,
    LiveReserveRemains(HlsLiveReserveDeadline),
    PendingOwnerRegistered,
    TerminalCommitted,
    ReevaluateNow,
    FailedClosed(HlsTerminalFailedClosedReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HlsPostRefreshOwnerResolution {
    RetryAt { at_ms: u64 },
    RetryAfter { at_ms: u64, reason: HlsTerminalFailedClosedReason },
    WaitUntil(HlsLiveReserveDeadline),
    WaitForEvidence { reason: HlsTerminalFailedClosedReason },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HlsPostRefreshOwnerWaitOutcome {
    Cancelled,
    Woken,
    DeadlineReached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HlsPostRefreshLeaseFallbackDisposition {
    TerminalCommitted,
    PendingOwned,
    RecoveredLive,
    Superseded,
    RetryRequired(HlsTerminalFailedClosedReason),
}

#[derive(Debug, Default)]
pub(super) struct HlsPostRefreshFallbackAggregate {
    pub(super) total: usize,
    pub(super) terminal_committed: usize,
    pub(super) pending_owned: usize,
    pub(super) recovered_live: usize,
    pub(super) superseded: usize,
    pub(super) unresolved: Vec<HlsAccessLeaseId>,
    pub(super) earliest_deadline: Option<HlsLiveReserveDeadline>,
    first_unresolved_reason: Option<HlsTerminalFailedClosedReason>,
}

impl HlsPostRefreshFallbackAggregate {
    fn record(&mut self, lease_id: &HlsAccessLeaseId, disposition: HlsPostRefreshLeaseFallbackDisposition) {
        self.total = self.total.saturating_add(1);
        match disposition {
            HlsPostRefreshLeaseFallbackDisposition::TerminalCommitted => {
                self.terminal_committed = self.terminal_committed.saturating_add(1);
            }
            HlsPostRefreshLeaseFallbackDisposition::PendingOwned => {
                self.pending_owned = self.pending_owned.saturating_add(1);
            }
            HlsPostRefreshLeaseFallbackDisposition::RecoveredLive => {
                self.recovered_live = self.recovered_live.saturating_add(1);
            }
            HlsPostRefreshLeaseFallbackDisposition::Superseded => {
                self.superseded = self.superseded.saturating_add(1);
            }
            HlsPostRefreshLeaseFallbackDisposition::RetryRequired(reason) => {
                self.first_unresolved_reason.get_or_insert(reason);
                self.unresolved.push(lease_id.clone());
            }
        }
    }

    pub(super) fn is_complete(&self) -> bool {
        self.unresolved.is_empty()
    }

    fn outcome(&self) -> HlsPostRefreshFallbackOutcome {
        if let Some(reason) = self.first_unresolved_reason {
            HlsPostRefreshFallbackOutcome::FailedClosed(reason)
        } else if self.pending_owned > 0 {
            HlsPostRefreshFallbackOutcome::PendingOwnerRegistered
        } else if self.recovered_live > 0 {
            HlsPostRefreshFallbackOutcome::RecoveredLive
        } else if self.terminal_committed > 0 {
            HlsPostRefreshFallbackOutcome::TerminalCommitted
        } else if self.superseded > 0 {
            HlsPostRefreshFallbackOutcome::Superseded
        } else {
            HlsPostRefreshFallbackOutcome::NoLiveLease
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum HlsPostRefreshFallbackOutcome {
    TerminalCommitted,
    PendingOwnerRegistered,
    RecoveredLive,
    NoLiveLease,
    Superseded,
    FailedClosed(HlsTerminalFailedClosedReason),
}

impl HlsPostRefreshFallbackOutcome {
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::TerminalCommitted => "terminal_committed",
            Self::PendingOwnerRegistered => "pending_owner_registered",
            Self::RecoveredLive => "recovered_live",
            Self::NoLiveLease => "no_live_lease",
            Self::Superseded => "superseded",
            Self::FailedClosed(_) => "failed_closed",
        }
    }
}

pub(super) fn live_reserve_deadline(
    now_ms: u64,
    reserve: HlsLeaseReserveSnapshot,
    cutover_timing: HlsLeaseCutoverTiming,
) -> Option<HlsLiveReserveDeadline> {
    let trigger_reserve_ms = reserve
        .transition_margin
        .as_millis()
        .saturating_add(HlsTerminalCommitAcquisitionBudgetMs::from_retry_policy().as_millis());
    let next_reevaluation_at_ms =
        now_ms.saturating_add(reserve.guaranteed_reserve_ms.saturating_sub(trigger_reserve_ms));
    let latest_safe_terminal_commit_at_ms = cutover_timing.latest_safe_terminal_commit_at.as_millis_since_epoch();
    let latest_wake_at_ms = latest_safe_terminal_commit_at_ms.checked_sub(1)?;
    Some(HlsLiveReserveDeadline {
        next_reevaluation_at_ms: next_reevaluation_at_ms.min(latest_wake_at_ms),
        latest_safe_terminal_commit_at_ms,
    })
}

pub(super) async fn evaluate_active_terminal_leases_for_reevaluation(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    now_ms: u64,
) -> HlsPostRefreshTerminalEvaluation {
    let leases = ctx.hls_proxy.active_live_playback_snapshots_for_session(proxy_session_id, now_ms).await;
    if leases.is_empty() {
        return HlsPostRefreshTerminalEvaluation::NoLiveLease;
    }
    let mut committed = false;
    let mut recovered_live = false;
    let mut live_reserve_deadline: Option<HlsLiveReserveDeadline> = None;
    let mut reevaluate = false;
    let mut pending_owner_registered = false;
    let mut failed_closed = None;
    for lease in &leases {
        let detailed = commit_terminal_tail_if_lease_reserve_requires_cutover_detailed(
            ctx,
            session,
            proxy_session_id,
            lease,
            now_ms,
            HlsTerminalDecisionPurpose::OrdinaryCutover,
        )
        .await;
        match detailed.resolution {
            HlsTerminalResolution::Committed => committed = true,
            HlsTerminalResolution::LiveAllowed => {
                if let Some(deadline) = detailed.live_reserve_deadline {
                    live_reserve_deadline = Some(earliest_deadline(live_reserve_deadline, deadline));
                } else {
                    recovered_live = true;
                }
            }
            HlsTerminalResolution::Reevaluate => reevaluate = true,
            HlsTerminalResolution::Pending { .. } => pending_owner_registered = true,
            HlsTerminalResolution::FailedClosed { reason } => {
                failed_closed.get_or_insert(reason);
            }
        }
    }
    if let Some(reason) = failed_closed {
        HlsPostRefreshTerminalEvaluation::FailedClosed(reason)
    } else if reevaluate {
        HlsPostRefreshTerminalEvaluation::ReevaluateNow
    } else if let Some(deadline) = live_reserve_deadline {
        HlsPostRefreshTerminalEvaluation::LiveReserveRemains(deadline)
    } else if pending_owner_registered {
        HlsPostRefreshTerminalEvaluation::PendingOwnerRegistered
    } else if recovered_live {
        let path_degraded = session.read().await.origin_control.path_condition.is_degraded();
        if path_degraded {
            HlsPostRefreshTerminalEvaluation::ReevaluateNow
        } else {
            HlsPostRefreshTerminalEvaluation::RecoveredLive
        }
    } else if committed {
        HlsPostRefreshTerminalEvaluation::TerminalCommitted
    } else {
        HlsPostRefreshTerminalEvaluation::NoLiveLease
    }
}

fn earliest_deadline(
    current: Option<HlsLiveReserveDeadline>,
    incoming: HlsLiveReserveDeadline,
) -> HlsLiveReserveDeadline {
    current.map_or(incoming, |current| HlsLiveReserveDeadline {
        next_reevaluation_at_ms: current.next_reevaluation_at_ms.min(incoming.next_reevaluation_at_ms),
        latest_safe_terminal_commit_at_ms: current
            .latest_safe_terminal_commit_at_ms
            .min(incoming.latest_safe_terminal_commit_at_ms),
    })
}

pub(super) async fn evaluate_owner_failure_fallback(
    ctx: &HlsCtx,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    now_ms: u64,
) -> HlsPostRefreshFallbackAggregate {
    let leases = ctx.hls_proxy.active_live_playback_snapshots_for_session(proxy_session_id, now_ms).await;
    let mut aggregate = HlsPostRefreshFallbackAggregate::default();
    for lease in &leases {
        let HlsDetailedTerminalResolution { resolution, live_reserve_deadline } =
            commit_terminal_tail_if_lease_reserve_requires_cutover_detailed(
                ctx,
                session,
                proxy_session_id,
                lease,
                now_ms,
                HlsTerminalDecisionPurpose::AutonomousOwnerFailureFallback,
            )
            .await;
        if let Some(deadline) = live_reserve_deadline {
            aggregate.earliest_deadline = Some(earliest_deadline(aggregate.earliest_deadline, deadline));
        }
        let disposition = match resolution {
            HlsTerminalResolution::Committed => HlsPostRefreshLeaseFallbackDisposition::TerminalCommitted,
            HlsTerminalResolution::LiveAllowed => HlsPostRefreshLeaseFallbackDisposition::RecoveredLive,
            HlsTerminalResolution::Reevaluate => {
                let still_current = ctx
                    .hls_proxy
                    .access_lease_response_snapshot(&lease.lease_id, proxy_session_id, now_ms)
                    .await
                    .is_some_and(|current| current.media_identity() == lease.media_identity());
                if still_current {
                    HlsPostRefreshLeaseFallbackDisposition::RetryRequired(
                        HlsTerminalFailedClosedReason::LeaseStateUnavailable,
                    )
                } else {
                    HlsPostRefreshLeaseFallbackDisposition::Superseded
                }
            }
            HlsTerminalResolution::Pending { .. } => HlsPostRefreshLeaseFallbackDisposition::PendingOwned,
            HlsTerminalResolution::FailedClosed { reason } => {
                HlsPostRefreshLeaseFallbackDisposition::RetryRequired(reason)
            }
        };
        aggregate.record(&lease.lease_id, disposition);
    }
    aggregate
}

pub(super) async fn wait_for_owner_resolution(
    ownership: &HlsAvailabilityReevaluationOwnership,
    resolution: HlsPostRefreshOwnerResolution,
) -> HlsPostRefreshOwnerWaitOutcome {
    let wake_at_ms = match resolution {
        HlsPostRefreshOwnerResolution::RetryAt { at_ms } | HlsPostRefreshOwnerResolution::RetryAfter { at_ms, .. } => {
            Some(at_ms)
        }
        HlsPostRefreshOwnerResolution::WaitUntil(deadline) => Some(deadline.next_reevaluation_at_ms),
        HlsPostRefreshOwnerResolution::WaitForEvidence { .. } => None,
    };
    match wake_at_ms {
        Some(wake_at_ms) => {
            tokio::select! {
                () = ownership.cancelled() => HlsPostRefreshOwnerWaitOutcome::Cancelled,
                () = ownership.wake_requested() => HlsPostRefreshOwnerWaitOutcome::Woken,
                () = tokio::time::sleep(Duration::from_millis(
                    wake_at_ms.saturating_sub(current_time_millis())
                )) => HlsPostRefreshOwnerWaitOutcome::DeadlineReached,
            }
        }
        None => {
            tokio::select! {
                () = ownership.cancelled() => HlsPostRefreshOwnerWaitOutcome::Cancelled,
                () = ownership.wake_requested() => HlsPostRefreshOwnerWaitOutcome::Woken,
            }
        }
    }
}

pub async fn commit_post_refresh_terminal_fallback(
    ctx: HlsCtx,
    session: HlsSessionHandle,
    action: HlsPostRefreshAvailabilityAction,
    failure: HlsAvailabilityReevaluationRegistration,
) -> HlsPostRefreshFallbackOutcome {
    if !matches!(
        failure,
        HlsAvailabilityReevaluationRegistration::CapacityExceeded
            | HlsAvailabilityReevaluationRegistration::RuntimeUnavailable
    ) {
        return HlsPostRefreshFallbackOutcome::Superseded;
    }
    let HlsPostRefreshAvailabilityAction::Reevaluate { origin_progress_generation, media_readiness_generation, .. } =
        action
    else {
        return HlsPostRefreshFallbackOutcome::Superseded;
    };
    let (proxy_session_id, action_is_current) = {
        let session = session.read().await;
        (
            session.proxy_session_id.clone(),
            session.origin_control.progress_generation == origin_progress_generation
                && session.activity.media_readiness_generation >= media_readiness_generation
                && session.origin_control.path_condition.is_degraded(),
        )
    };
    if !action_is_current {
        return HlsPostRefreshFallbackOutcome::Superseded;
    }
    evaluate_owner_failure_fallback(&ctx, &session, &proxy_session_id, current_time_millis()).await.outcome()
}
