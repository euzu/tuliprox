use crate::api::{
    api_utils::try_unwrap_body,
    model::{
        commit_terminal_tail_if_lease_reserve_requires_cutover, finite_hls_immutable_media_response,
        finite_hls_media_head_response, finite_hls_media_response, publication_late_after_ms,
        retry_after_secs_from_ms, terminal_tail_manifest_body,
        AppState, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseState, HlsCacheResponseContext,
        HlsLeasePlaybackMode, HlsSessionHandle, HlsTerminalFailedClosedReason, HlsTerminalResolution,
        HlsTerminalSegmentPath, HlsTerminalTailPlan, ProxySessionId,
    },
};
use axum::{
    body::Body,
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
};
use log::warn;
use std::sync::Arc;

const HLS_TERMINAL_ENDPOINT_MAX_REEVALUATIONS: usize = 2;
const HLS_TERMINAL_SEGMENT_CACHE_CONTROL: &str = "private, max-age=300, immutable";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsTerminalEndpointAction {
    ServeLive,
    ReloadTerminal,
    Reevaluate,
    RetryAfter { retry_after_ms: u64 },
    FailClosed { reason: HlsTerminalFailedClosedReason },
}

/// Orders lease-bound manifest work without treating a valid first publication
/// as terminal evidence or failing a request solely on an old publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HlsManifestTerminalPreflight {
    ServeCommittedPlayback,
    BootstrapPendingLease,
    RefreshBeforeTerminalEvaluation,
    EvaluateTerminal,
    FailClosed { reason: HlsTerminalFailedClosedReason },
}

pub(super) async fn hls_manifest_terminal_preflight(
    session: &HlsSessionHandle,
    lease: &HlsAccessLease,
    now_ms: u64,
) -> HlsManifestTerminalPreflight {
    match &lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(_)
        | HlsLeasePlaybackMode::TerminalUnavailable { .. }
        | HlsLeasePlaybackMode::Ended => return HlsManifestTerminalPreflight::ServeCommittedPlayback,
        HlsLeasePlaybackMode::Live => {}
    }
    let Some(manifest) = lease.last_manifest_snapshot.as_ref() else {
        return if lease.state == HlsAccessLeaseState::Pending {
            HlsManifestTerminalPreflight::BootstrapPendingLease
        } else {
            HlsManifestTerminalPreflight::FailClosed {
                reason: HlsTerminalFailedClosedReason::LeaseStateUnavailable,
            }
        };
    };
    let (publication_late, capacity_recovery_blocks_ready_timeline) = {
        let session = session.read().await;
        let target_duration_ms = session
            .origin_control
            .target_duration_snapshot_ms
            .unwrap_or(manifest.target_duration_ms);
        let ready_timeline = session.ready_timeline_snapshot(
            lease.playback_cursor.ready_timeline_start_proxy_seq(manifest.first_proxy_seq),
            now_ms,
        );
        (
            session.origin_control.last_media_progress_at_ms.is_some_and(|last_progress_at_ms| {
                now_ms.saturating_sub(last_progress_at_ms) >= publication_late_after_ms(target_duration_ms)
            }),
            session.capacity_recovery_blocks_ready_timeline(&ready_timeline),
        )
    };
    if capacity_recovery_blocks_ready_timeline {
        HlsManifestTerminalPreflight::EvaluateTerminal
    } else if publication_late {
        HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation
    } else {
        HlsManifestTerminalPreflight::EvaluateTerminal
    }
}

pub(super) fn hls_terminal_endpoint_action(resolution: HlsTerminalResolution) -> HlsTerminalEndpointAction {
    match resolution {
        HlsTerminalResolution::LiveAllowed => HlsTerminalEndpointAction::ServeLive,
        HlsTerminalResolution::Committed => HlsTerminalEndpointAction::ReloadTerminal,
        HlsTerminalResolution::Reevaluate => HlsTerminalEndpointAction::Reevaluate,
        HlsTerminalResolution::Pending { retry_after_ms } => {
            HlsTerminalEndpointAction::RetryAfter { retry_after_ms }
        }
        HlsTerminalResolution::FailedClosed { reason } => HlsTerminalEndpointAction::FailClosed { reason },
    }
}

pub(super) fn hls_response(hls_content: String) -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-store, no-cache, must-revalidate")
        .body(hls_content))
}

pub(super) fn hls_temporary_resource_unavailable_response(retry_after_ms: u64) -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::RETRY_AFTER, retry_after_secs_from_ms(retry_after_ms).to_string())
        .body(Body::empty()))
}

pub(super) fn terminal_tail_plan_for_current_route(
    evaluated: &HlsAccessLease,
    current: &HlsAccessLease,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> Option<Arc<HlsTerminalTailPlan>> {
    let HlsLeasePlaybackMode::TerminalTail(evaluated_plan) = &evaluated.playback_mode else {
        return None;
    };
    let HlsLeasePlaybackMode::TerminalTail(current_plan) = &current.playback_mode else {
        return None;
    };
    (current_plan.generation == evaluated_plan.generation
        && evaluated_plan.matches_route(proxy_session_id, access_lease_id)
        && current_plan.matches_route(proxy_session_id, access_lease_id))
    .then(|| Arc::clone(current_plan))
}

pub(super) fn terminal_segment_head_response(
    plan: &HlsTerminalTailPlan,
    path: HlsTerminalSegmentPath,
    range: Option<&HeaderValue>,
) -> Option<axum::response::Response> {
    let content_length = plan.segment_content_length(path)?;
    Some(finite_hls_media_head_response(
        content_length,
        range,
        plan.segment_content_type(),
        HLS_TERMINAL_SEGMENT_CACHE_CONTROL,
    ))
}

pub(super) fn terminal_segment_get_response(
    plan: &HlsTerminalTailPlan,
    path: HlsTerminalSegmentPath,
    range: Option<&HeaderValue>,
    response_context: &HlsCacheResponseContext,
    proxy_session_id: &ProxySessionId,
) -> Option<axum::response::Response> {
    let bytes = plan.segment_bytes(path)?;
    Some(finite_hls_media_response(
        bytes,
        range,
        plan.segment_content_type(),
        HLS_TERMINAL_SEGMENT_CACHE_CONTROL,
        response_context,
        proxy_session_id,
        format!("terminal/{}/{}", path.generation.0, path.index),
    ))
}

pub(super) fn terminal_segment_immutable_replay_response(
    plan: &HlsTerminalTailPlan,
    path: HlsTerminalSegmentPath,
    range: Option<&HeaderValue>,
    head_only: bool,
) -> Option<axum::response::Response> {
    let bytes = plan.segment_bytes(path)?;
    Some(finite_hls_immutable_media_response(
        bytes,
        range,
        plan.segment_content_type(),
        HLS_TERMINAL_SEGMENT_CACHE_CONTROL,
        head_only,
    ))
}

pub(super) fn hls_terminal_playback_response(
    lease: &HlsAccessLease,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> Option<axum::response::Response> {
    match &lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(plan) => {
            let Ok(body) = terminal_tail_manifest_body(plan, proxy_session_id, access_lease_id) else {
                return Some(StatusCode::NOT_FOUND.into_response());
            };
            Some(hls_response(body.to_owned()))
        }
        HlsLeasePlaybackMode::TerminalUnavailable { .. } => Some(StatusCode::SERVICE_UNAVAILABLE.into_response()),
        HlsLeasePlaybackMode::Live => None,
        HlsLeasePlaybackMode::Ended => Some(StatusCode::NOT_FOUND.into_response()),
    }
}

pub(super) fn hls_terminal_failed_closed_response(
    reason: HlsTerminalFailedClosedReason,
) -> axum::response::Response {
    warn!("HLS terminal manifest failed closed: reason={}", reason.as_label());
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

fn hls_terminal_live_decision_is_current(evaluated: &HlsAccessLease, current: &HlsAccessLease) -> bool {
    evaluated.issued_at_ms == current.issued_at_ms
        && evaluated.state == current.state
        && evaluated.admission_generation == current.admission_generation
        && evaluated.playback_cursor.cursor_generation == current.playback_cursor.cursor_generation
        && evaluated.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation)
            == current.last_manifest_snapshot.as_ref().map(|snapshot| snapshot.snapshot_generation)
        && current.playback_mode == HlsLeasePlaybackMode::Live
}

async fn current_hls_terminal_lease(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> Result<HlsAccessLease, axum::response::Response> {
    app_state
        .hls_proxy
        .access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms)
        .await
        .ok_or_else(|| hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::LeaseStateUnavailable))
}

async fn current_hls_terminal_session(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
) -> Result<HlsSessionHandle, axum::response::Response> {
    app_state
        .hls_proxy
        .sessions()
        .get_by_proxy_session_id(proxy_session_id)
        .await
        .ok_or_else(|| hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::LeaseStateUnavailable))
}

pub(super) async fn resolve_hls_terminal_manifest_state(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    initial_lease: HlsAccessLease,
    now_ms: u64,
) -> Result<HlsAccessLease, axum::response::Response> {
    let mut lease = initial_lease;
    let mut decision_session = Arc::clone(session);
    let mut evaluation_now_ms = now_ms;
    let mut reevaluations = 0_usize;

    loop {
        if let Some(response) = hls_terminal_playback_response(&lease, proxy_session_id, access_lease_id) {
            return Err(response);
        }
        let resolution = commit_terminal_tail_if_lease_reserve_requires_cutover(
            app_state,
            &decision_session,
            proxy_session_id,
            &lease,
            evaluation_now_ms,
        )
        .await;
        match hls_terminal_endpoint_action(resolution) {
            HlsTerminalEndpointAction::ServeLive => {
                let snapshot_now_ms = current_time_millis();
                let current =
                    current_hls_terminal_lease(app_state, proxy_session_id, access_lease_id, snapshot_now_ms).await?;
                if let Some(response) = hls_terminal_playback_response(&current, proxy_session_id, access_lease_id) {
                    return Err(response);
                }
                if hls_terminal_live_decision_is_current(&lease, &current) {
                    return Ok(current);
                }
                if reevaluations >= HLS_TERMINAL_ENDPOINT_MAX_REEVALUATIONS {
                    return Err(hls_terminal_failed_closed_response(
                        HlsTerminalFailedClosedReason::RuntimeUnavailable,
                    ));
                }
                let current_session = current_hls_terminal_session(app_state, proxy_session_id).await?;
                lease = current;
                decision_session = current_session;
                evaluation_now_ms = snapshot_now_ms;
                reevaluations = reevaluations.saturating_add(1);
            }
            HlsTerminalEndpointAction::ReloadTerminal => {
                let reloaded =
                    current_hls_terminal_lease(app_state, proxy_session_id, access_lease_id, current_time_millis())
                        .await?;
                return match hls_terminal_playback_response(&reloaded, proxy_session_id, access_lease_id) {
                    Some(response) => Err(response),
                    None => Err(hls_terminal_failed_closed_response(
                        HlsTerminalFailedClosedReason::RuntimeUnavailable,
                    )),
                };
            }
            HlsTerminalEndpointAction::Reevaluate => {
                if reevaluations >= HLS_TERMINAL_ENDPOINT_MAX_REEVALUATIONS {
                    return Err(hls_terminal_failed_closed_response(
                        HlsTerminalFailedClosedReason::RuntimeUnavailable,
                    ));
                }
                evaluation_now_ms = current_time_millis();
                let reloaded =
                    current_hls_terminal_lease(app_state, proxy_session_id, access_lease_id, evaluation_now_ms).await?;
                let reloaded_session = current_hls_terminal_session(app_state, proxy_session_id).await?;
                lease = reloaded;
                decision_session = reloaded_session;
                reevaluations = reevaluations.saturating_add(1);
            }
            HlsTerminalEndpointAction::RetryAfter { retry_after_ms } => {
                return Err(hls_temporary_resource_unavailable_response(retry_after_ms));
            }
            HlsTerminalEndpointAction::FailClosed { reason } => {
                return Err(hls_terminal_failed_closed_response(reason));
            }
        }
    }
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }
