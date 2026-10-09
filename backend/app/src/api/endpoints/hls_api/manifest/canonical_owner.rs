use super::{
    duration_to_millis_saturating, hls_terminal_failed_closed_response, hls_terminal_playback_response,
    hls_unpublished_lease_channel_unavailable_response, resolve_hls_terminal_manifest_state,
    try_hls_cached_manifest_response, HlsRuntimeBandwidthLearningContext,
};
use crate::api::{api_utils::try_unwrap_body, model::AppState};
use axum::{http::StatusCode, response::IntoResponse};
use log::{debug, warn};
use std::{sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    cold_start_retry_after_seconds, hls_cached_manifest_options_for_requirement,
    maybe_trigger_origin_refresh_with_outcome, register_hls_availability_reevaluation, safe_hls_access_lease_id,
    safe_proxy_session_id, trigger_origin_refresh_sync, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState,
    HlsAvailabilityReevaluationObservation, HlsAvailabilityReevaluationRegistration, HlsLeasePlaybackMode,
    HlsManifestCommitRequirement, HlsOriginRefreshTriggerOutcome, HlsSessionHandle, HlsTerminalFailedClosedReason,
    OriginRefreshRequest, ProxySessionId,
};

pub(in crate::api::endpoints::hls_api) fn hls_canonical_manifest_path(
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> String {
    format!("/hls/shared/live/{}/{}/manifest.m3u8", proxy_session_id.0, access_lease_id.0)
}

pub(in crate::api::endpoints::hls_api) fn hls_canonical_retry_after_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        .header(axum::http::header::RETRY_AFTER, cold_start_retry_after_seconds().to_string())
        .body(axum::body::Body::empty()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum HlsCanonicalOwnerRegistration {
    Join(HlsCanonicalOwnerRegistrationKind),
    FailClosed(HlsCanonicalOwnerRegistrationFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum HlsCanonicalOwnerRegistrationKind {
    Scheduled,
    AlreadyOwned,
}

impl HlsCanonicalOwnerRegistrationKind {
    pub(in crate::api::endpoints::hls_api) const fn as_label(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::AlreadyOwned => "already_owned",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum HlsCanonicalOwnerRegistrationFailure {
    CapacityExceeded,
    RuntimeUnavailable,
}

impl HlsCanonicalOwnerRegistrationFailure {
    pub(in crate::api::endpoints::hls_api) const fn as_label(self) -> &'static str {
        match self {
            Self::CapacityExceeded => "capacity_exceeded",
            Self::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

pub(in crate::api::endpoints::hls_api) const fn hls_canonical_owner_registration(
    registration: HlsAvailabilityReevaluationRegistration,
) -> HlsCanonicalOwnerRegistration {
    match registration {
        HlsAvailabilityReevaluationRegistration::Scheduled => {
            HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::Scheduled)
        }
        HlsAvailabilityReevaluationRegistration::AlreadyOwned | HlsAvailabilityReevaluationRegistration::Superseded => {
            HlsCanonicalOwnerRegistration::Join(HlsCanonicalOwnerRegistrationKind::AlreadyOwned)
        }
        HlsAvailabilityReevaluationRegistration::CapacityExceeded => {
            HlsCanonicalOwnerRegistration::FailClosed(HlsCanonicalOwnerRegistrationFailure::CapacityExceeded)
        }
        HlsAvailabilityReevaluationRegistration::RuntimeUnavailable => {
            HlsCanonicalOwnerRegistration::FailClosed(HlsCanonicalOwnerRegistrationFailure::RuntimeUnavailable)
        }
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_availability_reevaluation_registration_failure_response(
    failure: HlsCanonicalOwnerRegistrationFailure,
) -> axum::response::Response {
    warn!("HLS availability reevaluation not registered: reason={}", failure.as_label());
    hls_canonical_retry_after_response()
}

pub(in crate::api::endpoints::hls_api) enum HlsCanonicalOwnerResolution {
    Live(axum::response::Response),
    Terminal(axum::response::Response),
    Standalone(axum::response::Response),
    FailedClosed { reason: HlsCanonicalOwnerFailureReason, response: axum::response::Response },
}

impl HlsCanonicalOwnerResolution {
    pub(in crate::api::endpoints::hls_api) const fn outcome_label(&self) -> &'static str {
        match self {
            Self::Live(_) => "live",
            Self::Terminal(_) => "terminal",
            Self::Standalone(_) => "standalone",
            Self::FailedClosed { reason, .. } => reason.as_label(),
        }
    }

    pub(in crate::api::endpoints::hls_api) fn into_response(self) -> axum::response::Response {
        match self {
            Self::Live(response)
            | Self::Terminal(response)
            | Self::Standalone(response)
            | Self::FailedClosed { response, .. } => response,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum HlsCanonicalOwnerFailureReason {
    Superseded,
    DeadlineElapsed,
    LeaseUnavailable,
}

impl HlsCanonicalOwnerFailureReason {
    pub(in crate::api::endpoints::hls_api) const fn as_label(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::DeadlineElapsed => "deadline_elapsed",
            Self::LeaseUnavailable => "lease_unavailable",
        }
    }

    pub(in crate::api::endpoints::hls_api) fn response(self) -> axum::response::Response {
        let reason = match self {
            Self::DeadlineElapsed => HlsTerminalFailedClosedReason::SafeCommitDeadlineElapsed,
            Self::Superseded | Self::LeaseUnavailable => HlsTerminalFailedClosedReason::LeaseStateUnavailable,
        };
        hls_terminal_failed_closed_response(reason)
    }
}

pub(in crate::api::endpoints::hls_api) struct HlsCanonicalOwnerPending {
    pub(in crate::api::endpoints::hls_api) deadline_ms: u64,
    pub(in crate::api::endpoints::hls_api) current_session_available: bool,
}

pub(in crate::api::endpoints::hls_api) enum HlsCanonicalOwnerEvaluation {
    Resolved(HlsCanonicalOwnerResolution),
    Pending(HlsCanonicalOwnerPending),
}

pub(in crate::api::endpoints::hls_api) fn hls_canonical_owner_failed(
    reason: HlsCanonicalOwnerFailureReason,
) -> HlsCanonicalOwnerEvaluation {
    HlsCanonicalOwnerEvaluation::Resolved(HlsCanonicalOwnerResolution::FailedClosed {
        reason,
        response: reason.response(),
    })
}

pub(in crate::api::endpoints::hls_api) struct HlsCanonicalOwnerHandoffContext<'a> {
    pub(in crate::api::endpoints::hls_api) app_state: &'a Arc<AppState>,
    pub(in crate::api::endpoints::hls_api) proxy_session_id: &'a ProxySessionId,
    pub(in crate::api::endpoints::hls_api) access_lease_id: &'a HlsAccessLeaseId,
    pub(in crate::api::endpoints::hls_api) expected_lease_issued_at_ms: Option<u64>,
    pub(in crate::api::endpoints::hls_api) strip: &'a crate::model::StripConfig,
    pub(in crate::api::endpoints::hls_api) server_path: Option<&'a str>,
    pub(in crate::api::endpoints::hls_api) manifest_commit_requirement: HlsManifestCommitRequirement,
    pub(in crate::api::endpoints::hls_api) manifest_boundary_rendered_at_ms: u64,
    pub(in crate::api::endpoints::hls_api) bandwidth_learning: HlsRuntimeBandwidthLearningContext<'a>,
    pub(in crate::api::endpoints::hls_api) request_deadline_ms: u64,
    pub(in crate::api::endpoints::hls_api) safe_session: String,
}

pub(in crate::api::endpoints::hls_api) fn hls_canonical_owner_lease_deadline_ms(lease: &HlsAccessLease) -> u64 {
    if lease.state == HlsAccessLeaseState::Pending {
        lease.pending_deadline_ms().unwrap_or(lease.valid_until_ms)
    } else {
        lease.valid_until_ms
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_canonical_owner_request_deadline_ms(
    lease: &HlsAccessLease,
    wait_timeout: Duration,
    now_ms: u64,
) -> u64 {
    let lease_deadline_ms = hls_canonical_owner_lease_deadline_ms(lease);
    if wait_timeout.is_zero() && lease.state == HlsAccessLeaseState::Pending {
        lease_deadline_ms
    } else {
        lease_deadline_ms.min(now_ms.saturating_add(duration_to_millis_saturating(wait_timeout)))
    }
}

pub(in crate::api::endpoints::hls_api) async fn evaluate_hls_canonical_owner_handoff(
    context: &HlsCanonicalOwnerHandoffContext<'_>,
) -> HlsCanonicalOwnerEvaluation {
    let now_ms = current_time_millis();
    let Some(lease) = context
        .app_state
        .hls
        .proxy
        .access_lease_response_snapshot(context.access_lease_id, context.proxy_session_id, now_ms)
        .await
    else {
        return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
    };
    if context.expected_lease_issued_at_ms != Some(lease.issued_at_ms) {
        return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
    }
    match &lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(_) => {
            let Some(response) =
                hls_terminal_playback_response(&lease, context.proxy_session_id, context.access_lease_id)
            else {
                return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
            };
            return HlsCanonicalOwnerEvaluation::Resolved(HlsCanonicalOwnerResolution::Terminal(response));
        }
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => {
            return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
        }
        HlsLeasePlaybackMode::Live => {}
    }
    if matches!(
        lease.state,
        HlsAccessLeaseState::PolicyRevoking | HlsAccessLeaseState::Expired | HlsAccessLeaseState::Denied
    ) {
        return hls_canonical_owner_failed(HlsCanonicalOwnerFailureReason::LeaseUnavailable);
    }

    let current_session =
        context.app_state.hls.proxy.sessions().get_by_proxy_session_id(context.proxy_session_id).await;
    if let Some(current_session) = current_session.as_ref() {
        let options = hls_cached_manifest_options_for_requirement(
            Duration::ZERO,
            context.manifest_commit_requirement,
            context.manifest_boundary_rendered_at_ms,
        );
        if let Some(response) = try_hls_cached_manifest_response(
            context.app_state,
            current_session,
            context.access_lease_id,
            lease.state,
            context.strip,
            context.server_path,
            options,
            context.bandwidth_learning,
        )
        .await
        .filter(|response| response.status() == StatusCode::OK)
        {
            return HlsCanonicalOwnerEvaluation::Resolved(HlsCanonicalOwnerResolution::Live(response));
        }
    }
    HlsCanonicalOwnerEvaluation::Pending(HlsCanonicalOwnerPending {
        deadline_ms: hls_canonical_owner_lease_deadline_ms(&lease).min(context.request_deadline_ms),
        current_session_available: current_session.is_some(),
    })
}

pub(in crate::api::endpoints::hls_api) async fn finalize_hls_canonical_owner_handoff(
    context: &HlsCanonicalOwnerHandoffContext<'_>,
    pending: HlsCanonicalOwnerPending,
    deadline_elapsed: bool,
) -> HlsCanonicalOwnerResolution {
    match evaluate_hls_canonical_owner_handoff(context).await {
        HlsCanonicalOwnerEvaluation::Resolved(resolution) => resolution,
        HlsCanonicalOwnerEvaluation::Pending(current) => {
            let response = hls_unpublished_lease_channel_unavailable_response(
                context.app_state,
                context.proxy_session_id,
                context.access_lease_id,
            )
            .await;
            if response.status() == StatusCode::OK {
                return HlsCanonicalOwnerResolution::Standalone(response);
            }
            let reason = if deadline_elapsed {
                HlsCanonicalOwnerFailureReason::DeadlineElapsed
            } else if !current.current_session_available && !pending.current_session_available {
                HlsCanonicalOwnerFailureReason::Superseded
            } else {
                HlsCanonicalOwnerFailureReason::LeaseUnavailable
            };
            HlsCanonicalOwnerResolution::FailedClosed { reason, response: reason.response() }
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn join_hls_canonical_manifest_owner(
    context: HlsCanonicalOwnerHandoffContext<'_>,
    registration: HlsCanonicalOwnerRegistrationKind,
) -> axum::response::Response {
    let started_at = tokio::time::Instant::now();
    let coordinator = context.app_state.hls.proxy.availability_reevaluations();
    let resolution = loop {
        let mut observer = coordinator.observe_owner(context.proxy_session_id);
        let pending = match evaluate_hls_canonical_owner_handoff(&context).await {
            HlsCanonicalOwnerEvaluation::Resolved(resolution) => break resolution,
            HlsCanonicalOwnerEvaluation::Pending(pending) => pending,
        };
        let now_ms = current_time_millis();
        if now_ms >= pending.deadline_ms {
            break finalize_hls_canonical_owner_handoff(&context, pending, true).await;
        }
        let Some(observer) = observer.as_mut() else {
            break finalize_hls_canonical_owner_handoff(&context, pending, false).await;
        };
        let remaining_ms = pending.deadline_ms.saturating_sub(now_ms);
        match tokio::time::timeout(Duration::from_millis(remaining_ms), observer.changed()).await {
            Ok(
                HlsAvailabilityReevaluationObservation::EvidenceChanged
                | HlsAvailabilityReevaluationObservation::OwnerFinished,
            ) => {}
            Err(_) => break finalize_hls_canonical_owner_handoff(&context, pending, true).await,
        }
    };
    debug!(
        "HLS canonical manifest owner handoff completed: session={} proxy_session={} lease={} registration={} outcome={} wait_ms={}",
        context.safe_session,
        safe_proxy_session_id(context.proxy_session_id),
        safe_hls_access_lease_id(context.access_lease_id),
        registration.as_label(),
        resolution.outcome_label(),
        duration_to_millis_saturating(started_at.elapsed())
    );
    resolution.into_response()
}

pub(in crate::api::endpoints::hls_api) async fn hls_direct_refresh_follow_up(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    refresh_request: OriginRefreshRequest,
    outcome: HlsOriginRefreshTriggerOutcome,
) -> Option<axum::response::Response> {
    match outcome {
        HlsOriginRefreshTriggerOutcome::Started
        | HlsOriginRefreshTriggerOutcome::SessionUnavailable
        | HlsOriginRefreshTriggerOutcome::InFlight
        | HlsOriginRefreshTriggerOutcome::DebouncedUntil { .. } => return None,
        HlsOriginRefreshTriggerOutcome::RecoveryPressureSuperseded => {
            warn!("HLS direct origin refresh evidence superseded; scheduling current availability reevaluation");
        }
        HlsOriginRefreshTriggerOutcome::RecoveryPressureStateContention => {
            warn!("HLS direct origin refresh state contended; scheduling current availability reevaluation");
        }
    }
    let Some(owner_key) = app_state.hls.proxy.availability_reevaluation_owner_key(session, proxy_session_id).await
    else {
        warn!("HLS direct origin refresh follow-up unavailable: reason=session_superseded");
        return Some(hls_canonical_retry_after_response());
    };
    match register_hls_availability_reevaluation(app_state.hls_ctx(), Arc::clone(session), owner_key, refresh_request) {
        HlsAvailabilityReevaluationRegistration::Scheduled
        | HlsAvailabilityReevaluationRegistration::AlreadyOwned
        | HlsAvailabilityReevaluationRegistration::Superseded => None,
        HlsAvailabilityReevaluationRegistration::CapacityExceeded => {
            warn!("HLS direct origin refresh follow-up unavailable: reason=capacity_exceeded");
            Some(hls_canonical_retry_after_response())
        }
        HlsAvailabilityReevaluationRegistration::RuntimeUnavailable => {
            warn!("HLS direct origin refresh follow-up unavailable: reason=runtime_unavailable");
            Some(hls_canonical_retry_after_response())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::api::endpoints::hls_api) enum HlsManifestRefreshOrdering {
    Background,
    AwaitBeforeTerminalEvaluation,
}

pub(in crate::api::endpoints::hls_api) async fn trigger_hls_canonical_manifest_refresh(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    refresh_request: OriginRefreshRequest,
    ordering: HlsManifestRefreshOrdering,
) -> Option<axum::response::Response> {
    match ordering {
        HlsManifestRefreshOrdering::Background => {
            let outcome = maybe_trigger_origin_refresh_with_outcome(refresh_request.clone()).await;
            hls_direct_refresh_follow_up(app_state, session, proxy_session_id, refresh_request, outcome).await
        }
        HlsManifestRefreshOrdering::AwaitBeforeTerminalEvaluation => {
            // An already-owned refresh cannot be joined through this call, but
            // it must not bypass the lease-specific terminal decision. The
            // in-flight owner will still publish its eventual progress/failure.
            let _refresh_started = trigger_origin_refresh_sync(refresh_request).await;
            let now_ms = current_time_millis();
            let Some(lease) =
                app_state.hls.proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
            else {
                return Some(hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::LeaseStateUnavailable));
            };
            resolve_hls_terminal_manifest_state(app_state, session, proxy_session_id, access_lease_id, lease, now_ms)
                .await
                .err()
                .map(|response| *response)
        }
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_canonical_status_response(
    status: StatusCode,
) -> axum::response::Response {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        hls_canonical_retry_after_response()
    } else {
        status.into_response()
    }
}
