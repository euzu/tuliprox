use super::{
    hls_access_lease_ttl_ms, hls_canonical_status_response, hls_custom_video_manifest_response,
    hls_pending_bootstrap_window_ms, hls_temporary_resource_unavailable_response, hls_terminal_playback_response,
    validate_hls_proxy_access_context, HlsOriginRuntimeAcquireError, HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
};
use crate::{
    api::model::{hls_custom_video_manifest_response_for_access_lease, AppState, CustomVideoStreamType},
    auth::Fingerprint,
};
use axum::{http::StatusCode, response::IntoResponse};
use log::{debug, warn};
use std::sync::Arc;
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    commit_hls_runtime_custom_tail, safe_hls_access_lease_id, safe_proxy_session_id, safe_user_session_token,
    HlsAccessAdmissionMode, HlsAccessContext, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeasePendingDeadline,
    HlsAccessLeaseState, HlsAccessLeaseTouch, HlsAccessLeaseValidationError, HlsLeasePlaybackMode,
    HlsResourceServeFailure, HlsResourceServeOutcome, HlsRuntimeCustomTailOutcome, HlsRuntimeCustomTailReason,
    HlsRuntimeCustomTailRequest, HlsSessionHandle, ProxySessionId, TransientObjectUnavailableState,
    TransientPassthroughState, TransientResourceFile,
};

pub(in crate::api::endpoints::hls_api) async fn hls_custom_video_manifest_response_for_username(
    app_state: &Arc<AppState>,
    username: &str,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
) -> axum::response::Response {
    if let Some(user) = app_state.app_config.get_user_credentials(username) {
        return hls_custom_video_manifest_response(app_state, &user, video_type, fallback_status).await;
    }
    fallback_status.into_response()
}

pub(in crate::api::endpoints::hls_api) async fn hls_custom_video_manifest_response_for_lease(
    app_state: &Arc<AppState>,
    lease: &HlsAccessLease,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let Some(user) = app_state.app_config.get_user_credentials(&lease.username) else {
        return fallback_status.into_response();
    };
    hls_custom_video_manifest_response_for_access_lease(app_state, &user, video_type, fallback_status, lease).await
}

pub(in crate::api::endpoints::hls_api) async fn hls_runtime_custom_tail_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let outcome = commit_hls_runtime_custom_tail(
        app_state.hls_ctx(),
        HlsRuntimeCustomTailRequest {
            session: Arc::clone(session),
            proxy_session_id: proxy_session_id.clone(),
            lease_id: access_lease_id.clone(),
            reason,
            now_ms: current_time_millis(),
        },
    )
    .await;
    if matches!(outcome, HlsRuntimeCustomTailOutcome::Committed | HlsRuntimeCustomTailOutcome::AlreadyCommitted) {
        let now_ms = current_time_millis();
        if let Some(lease) =
            app_state.hls.proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
        {
            if let Some(response) = hls_terminal_playback_response(&lease, proxy_session_id, access_lease_id) {
                return response;
            }
        }
    }
    if outcome == HlsRuntimeCustomTailOutcome::PendingOwnerRegistered {
        return hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS);
    }
    fallback_status.into_response()
}

pub(in crate::api::endpoints::hls_api) async fn hls_runtime_or_standalone_custom_tail_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let now_ms = current_time_millis();
    let Some(lease) =
        app_state.hls.proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
    else {
        return fallback_status.into_response();
    };
    match &lease.playback_mode {
        HlsLeasePlaybackMode::TerminalTail(_) | HlsLeasePlaybackMode::TerminalUnavailable { .. } => {
            if let Some(response) = hls_terminal_playback_response(&lease, proxy_session_id, access_lease_id) {
                return response;
            }
        }
        HlsLeasePlaybackMode::Live
            if lease.last_manifest_snapshot.is_some()
                && matches!(lease.state, HlsAccessLeaseState::Activated | HlsAccessLeaseState::PolicyRevoking) =>
        {
            return hls_runtime_custom_tail_response(
                app_state,
                session,
                proxy_session_id,
                access_lease_id,
                reason,
                fallback_status,
            )
            .await;
        }
        HlsLeasePlaybackMode::Ended if !reason.permits_unpublished_lease_standalone_tail() => {
            return fallback_status.into_response();
        }
        HlsLeasePlaybackMode::Live | HlsLeasePlaybackMode::Ended => {}
    }
    hls_custom_video_manifest_response_for_lease(app_state, &lease, reason.video_type(), fallback_status).await
}

pub(in crate::api::endpoints::hls_api) async fn hls_manifest_channel_unavailable_response_for_username(
    app_state: &Arc<AppState>,
    username: &str,
) -> axum::response::Response {
    hls_custom_video_manifest_response_for_username(
        app_state,
        username,
        CustomVideoStreamType::ChannelUnavailable,
        StatusCode::NOT_FOUND,
    )
    .await
}

/// Resolves the final canonical-manifest fallback after refresh and cached-live
/// publication have both produced no response.
pub(in crate::api::endpoints::hls_api) async fn hls_unpublished_lease_channel_unavailable_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> axum::response::Response {
    let reason = HlsRuntimeCustomTailReason::ChannelUnavailable;
    let lease = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(access_lease_id, proxy_session_id, current_time_millis())
        .await;
    if let Some(lease) = lease.filter(|lease| lease.permits_unpublished_standalone_tail(reason)) {
        return hls_custom_video_manifest_response_for_lease(
            app_state,
            &lease,
            CustomVideoStreamType::ChannelUnavailable,
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

pub(in crate::api::endpoints::hls_api) async fn hls_manifest_access_denial_runtime_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_snapshot: Option<&HlsAccessLease>,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let Some(lease) = lease_snapshot else {
        return fallback_status.into_response();
    };
    let Some(session) = app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await else {
        return fallback_status.into_response();
    };
    hls_runtime_or_standalone_custom_tail_response(
        app_state,
        &session,
        proxy_session_id,
        &lease.lease_id,
        reason,
        fallback_status,
    )
    .await
}

pub(in crate::api::endpoints::hls_api) fn hls_resource_channel_unavailable_response(
    _app_state: &Arc<AppState>,
    _access_context: &HlsAccessContext,
) -> axum::response::Response {
    StatusCode::NOT_FOUND.into_response()
}

pub(in crate::api::endpoints::hls_api) fn hls_origin_runtime_resource_failure_response(
    _app_state: &Arc<AppState>,
    _access_context: &HlsAccessContext,
    err: HlsOriginRuntimeAcquireError,
) -> axum::response::Response {
    match err {
        HlsOriginRuntimeAcquireError::NoAccountAvailable { .. } => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        HlsOriginRuntimeAcquireError::Fatal(status) => hls_canonical_status_response(status),
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_resource_serve_outcome_response(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    outcome: HlsResourceServeOutcome,
) -> axum::response::Response {
    match outcome {
        HlsResourceServeOutcome::Ready(response) => response,
        HlsResourceServeOutcome::Failure(HlsResourceServeFailure::TemporaryUnavailable { retry_after_ms }) => {
            hls_temporary_resource_unavailable_response(retry_after_ms)
        }
        HlsResourceServeOutcome::Failure(
            HlsResourceServeFailure::Missing
            | HlsResourceServeFailure::Expired
            | HlsResourceServeFailure::PermanentFailed { .. },
        ) => hls_resource_channel_unavailable_response(app_state, access_context),
    }
}

pub(in crate::api::endpoints::hls_api) async fn hls_manifest_access_lease_validation_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_snapshot: Option<&HlsAccessLease>,
    err: HlsAccessLeaseValidationError,
) -> axum::response::Response {
    match err {
        HlsAccessLeaseValidationError::AdmissionDenied { reason, .. } => {
            hls_manifest_access_denial_runtime_response(
                app_state,
                proxy_session_id,
                lease_snapshot,
                reason.unwrap_or(HlsRuntimeCustomTailReason::UserConnectionsExhausted),
                StatusCode::FORBIDDEN,
            )
            .await
        }
        HlsAccessLeaseValidationError::AvailabilityPending => {
            hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
        }
        HlsAccessLeaseValidationError::UserSessionMissing { .. } => {
            hls_manifest_access_denial_runtime_response(
                app_state,
                proxy_session_id,
                lease_snapshot,
                HlsRuntimeCustomTailReason::SessionOrLeaseExpired,
                StatusCode::NOT_FOUND,
            )
            .await
        }
        HlsAccessLeaseValidationError::UserAccountExpired { .. } => {
            hls_manifest_access_denial_runtime_response(
                app_state,
                proxy_session_id,
                lease_snapshot,
                HlsRuntimeCustomTailReason::UserAccountExpired,
                StatusCode::FORBIDDEN,
            )
            .await
        }
        HlsAccessLeaseValidationError::Expired => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(in crate::api::endpoints::hls_api) fn hls_resource_access_lease_validation_response(
    err: &HlsAccessLeaseValidationError,
) -> axum::response::Response {
    match err {
        HlsAccessLeaseValidationError::AdmissionDenied { .. }
        | HlsAccessLeaseValidationError::UserAccountExpired { .. } => StatusCode::FORBIDDEN.into_response(),
        HlsAccessLeaseValidationError::AvailabilityPending => {
            hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
        }
        HlsAccessLeaseValidationError::UserSessionMissing { .. } | HlsAccessLeaseValidationError::Expired => {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn hls_manifest_access_context_and_state(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_snapshot: Option<&HlsAccessLease>,
    now_ms: u64,
) -> Result<(HlsAccessContext, HlsAccessLeaseState), Box<axum::response::Response>> {
    app_state.hls.proxy.startup_observability().record_media_manifest_request(access_lease_id, now_ms);
    let access_context = match validate_hls_proxy_access_context(
        app_state,
        fingerprint,
        proxy_session_id,
        &access_lease_id.0,
        now_ms,
        HlsAccessAdmissionMode::ManifestPrepare,
    )
    .await
    {
        Ok(context) => context,
        Err(err) => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} user_session=none reason={err:?}",
                safe_hls_access_lease_id(access_lease_id),
                safe_proxy_session_id(proxy_session_id)
            );
            return Err(Box::new(
                hls_manifest_access_lease_validation_response(app_state, proxy_session_id, access_lease_snapshot, err)
                    .await,
            ));
        }
    };
    if access_lease_snapshot.is_none()
        && app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await.is_none()
        && app_state.hls.proxy.expired_session_marker(proxy_session_id, now_ms).await.is_some()
    {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    debug!(
        "HLS access lease accepted: lease={} proxy_session={} user_session={} request=manifest",
        safe_hls_access_lease_id(&access_context.lease_id),
        safe_proxy_session_id(proxy_session_id),
        safe_user_session_token(&access_context.user_session_token)
    );

    let access_lease_state = match app_state
        .hls
        .proxy
        .touch_manifest_access_lease(
            &access_context.lease_id,
            proxy_session_id,
            now_ms,
            None,
            Some(HlsAccessLeasePendingDeadline::Bootstrap {
                deadline_ms: now_ms.saturating_add(hls_pending_bootstrap_window_ms(app_state)),
            }),
            hls_access_lease_ttl_ms(app_state),
        )
        .await
    {
        HlsAccessLeaseTouch::Touched { lease } => lease.state,
        HlsAccessLeaseTouch::Denied => {
            return Err(Box::new(
                hls_manifest_access_denial_runtime_response(
                    app_state,
                    proxy_session_id,
                    access_lease_snapshot,
                    HlsRuntimeCustomTailReason::UserConnectionsExhausted,
                    StatusCode::FORBIDDEN,
                )
                .await,
            ));
        }
        HlsAccessLeaseTouch::Expired | HlsAccessLeaseTouch::UnknownLease | HlsAccessLeaseTouch::SessionMismatch => {
            return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
        }
    };

    Ok((access_context, access_lease_state))
}

pub(in crate::api::endpoints::hls_api) async fn hls_transient_object_unavailable_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    resource_file: &TransientResourceFile,
    now_ms: u64,
    access_context: &HlsAccessContext,
) -> axum::response::Response {
    let state = {
        let session = session.read().await;
        let key = TransientPassthroughState::transient_object_key(
            &session.proxy_session_id,
            &resource_file.resource_id,
            resource_file.extension.clone(),
        );
        session.transient.object_unavailable_state(&key, now_ms)
    };
    match state {
        TransientObjectUnavailableState::Fetching => {
            hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
        }
        TransientObjectUnavailableState::FailedRetryable { retry_after_ms } => {
            hls_temporary_resource_unavailable_response(retry_after_ms)
        }
        TransientObjectUnavailableState::FailedPermanent | TransientObjectUnavailableState::Missing => {
            hls_resource_channel_unavailable_response(app_state, access_context)
        }
    }
}
