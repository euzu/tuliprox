use super::{
    ensure_hls_cache_stream_registered, hls_entry_user_session_token, hls_initial_manifest_decision_wait_timeout,
    hls_resource_access_lease_validation_response, reclaim_hls_account_overlap_if_needed,
    HlsInitialStripPublicationDiagnostic, HlsMaterializedSharedManifest,
};
use crate::{
    api::model::{hls_cache::initial_strip::HlsInitialStripOutcome, AppState, StreamMeterHandle},
    auth::Fingerprint,
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::{debug, warn};
use shared::utils::generate_random_string;
use std::{sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    safe_hls_access_lease_id, safe_proxy_session_id, safe_user_session_token, validate_hls_access_lease,
    HlsAccessAdmissionMode, HlsAccessContext, HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId,
    HlsAccessLeaseState, HlsAccessLeaseTiming, HlsAccessLeaseValidationError, HlsCacheResponseContext,
    HlsLeasePlaybackMode, HlsLeaseStartupAdmissionState, HlsLogIdentity, HlsMediaActivityMarker, HlsMediaLeaseIdentity,
    HlsQosMeterInit, HlsQosRuntimeConfig, HlsSessionHandle, ProxySessionId,
};

pub(in crate::api::endpoints::hls_api) fn log_hls_initial_strip_publication(
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    diagnostic: HlsInitialStripPublicationDiagnostic,
) {
    match diagnostic {
        HlsInitialStripPublicationDiagnostic::Applied { mode, strip_mode, configured, effective, visible_segments } => {
            debug!(
                "HLS initial strip applied: mode={} lease={} proxy_session={} strip_mode={} configured={} effective={} visible_segments={}",
                mode,
                safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(proxy_session_id),
                strip_mode,
                configured,
                effective,
                visible_segments
            );
        }
        HlsInitialStripPublicationDiagnostic::Skipped { mode, reason, visible_segments } => {
            debug!(
                "HLS initial strip skipped: mode={} lease={} proxy_session={} reason={} visible_segments={}",
                mode,
                safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(proxy_session_id),
                reason.as_log_reason(),
                visible_segments
            );
        }
        HlsInitialStripPublicationDiagnostic::SkippedForLeaseState { mode, reason } => {
            debug!(
                "HLS initial strip skipped: mode={} lease={} proxy_session={} reason={}",
                mode,
                safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(proxy_session_id),
                reason.as_log_reason()
            );
        }
    }
}

pub(in crate::api::endpoints::hls_api) fn stripped_tail_segments(
    materialized: &HlsMaterializedSharedManifest,
) -> usize {
    materialized.initial_strip_outcome.as_ref().map_or(0, |outcome| match outcome {
        HlsInitialStripOutcome::Applied { effective, .. } => *effective,
        HlsInitialStripOutcome::Skipped { .. } => 0,
    })
}

pub(in crate::api::endpoints::hls_api) fn hls_access_lease_ttl_ms(app_state: &Arc<AppState>) -> u64 {
    app_state.hls.proxy.session_idle_timeout_ms()
}

pub(in crate::api::endpoints::hls_api) fn duration_to_millis_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(in crate::api::endpoints::hls_api) fn hls_pending_bootstrap_window_ms(app_state: &Arc<AppState>) -> u64 {
    duration_to_millis_saturating(hls_initial_manifest_decision_wait_timeout(app_state))
}

pub(in crate::api::endpoints::hls_api) async fn hls_access_lease_timing_for_session(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
) -> HlsAccessLeaseTiming {
    let timing = session.read().await.account_overlap_timing();
    let active_window_ms = timing.hard_active_window_ms.saturating_mul(2);
    HlsAccessLeaseTiming { active_window_ms, valid_window_ms: hls_access_lease_ttl_ms(app_state) }
}

pub(in crate::api::endpoints::hls_api) async fn touch_pending_manifest_follow_up_window(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
) {
    if access_lease_state != HlsAccessLeaseState::Pending {
        return;
    }
    let (proxy_session_id, target_duration) = {
        let session = session.read().await;
        (session.proxy_session_id.clone(), session.target_duration)
    };
    let now_ms = current_time_millis();
    if !app_state
        .hls
        .proxy
        .mark_pending_manifest_follow_up_for_lease(access_lease_id, &proxy_session_id, now_ms, target_duration)
        .await
    {
        debug!(
            "HLS pending manifest follow-up skipped: lease={} proxy_session={} reason=expired-or-generation-race",
            safe_hls_access_lease_id(access_lease_id),
            safe_proxy_session_id(&proxy_session_id)
        );
    }
}

pub(in crate::api::endpoints::hls_api) struct HlsResourceAccess {
    pub(in crate::api::endpoints::hls_api) session: HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api) access_context: HlsAccessContext,
    pub(in crate::api::endpoints::hls_api) lease: HlsAccessLease,
}

pub(in crate::api::endpoints::hls_api) async fn prepare_hls_resource_access(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    request_kind: &'static str,
) -> Result<HlsResourceAccess, Box<axum::response::Response>> {
    let Some(session) = app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    app_state
        .hls
        .proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            proxy_session_id,
            now_ms,
        )
        .await;
    let access_context = match validate_hls_proxy_access_request(
        app_state,
        fingerprint,
        proxy_session_id,
        hls_access_lease_id,
        now_ms,
        hls_access_lease_timing_for_session(app_state, &session).await,
        request_kind,
    )
    .await
    {
        Ok(context) => context,
        Err(err) => {
            return Err(Box::new(hls_resource_access_lease_validation_response(&err)));
        }
    };
    reclaim_hls_account_overlap_if_needed(app_state, &session, now_ms).await;
    let Some(lease) =
        app_state.hls.proxy.access_lease_response_snapshot(&access_context.lease_id, proxy_session_id, now_ms).await
    else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    Ok(HlsResourceAccess { session, access_context, lease })
}

pub(in crate::api::endpoints::hls_api) fn hls_lease_allows_live_origin_work(lease: &HlsAccessLease) -> bool {
    matches!(lease.state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle)
        && lease.playback_mode == HlsLeasePlaybackMode::Live
}

pub(in crate::api::endpoints::hls_api) fn hls_lease_allows_cached_segment(
    lease: &HlsAccessLease,
    proxy_seq: u64,
) -> bool {
    match &lease.playback_mode {
        HlsLeasePlaybackMode::Live => true,
        HlsLeasePlaybackMode::TerminalTail(plan) => plan.protected_base_proxy_seqs.contains(&proxy_seq),
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => false,
    }
}

pub(in crate::api::endpoints::hls_api) async fn current_hls_resource_lease(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
) -> Option<HlsAccessLease> {
    app_state
        .hls
        .proxy
        .access_lease_response_snapshot(
            &access_context.lease_id,
            &access_context.proxy_session_id,
            current_time_millis(),
        )
        .await
}

pub(in crate::api::endpoints::hls_api) async fn hls_live_lease_identity_is_current(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    expected_identity: HlsMediaLeaseIdentity,
) -> bool {
    current_hls_resource_lease(app_state, access_context).await.is_some_and(|lease| {
        lease.playback_mode == HlsLeasePlaybackMode::Live && lease.media_identity() == Some(expected_identity)
    })
}

pub(in crate::api::endpoints::hls_api) fn create_hls_cache_user_session_token(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    existing_session_token: Option<&str>,
    archive_reference: Option<i64>,
) -> String {
    let base =
        hls_entry_user_session_token(fingerprint, username, virtual_id, existing_session_token, archive_reference);
    format!("{base}|hls-cache|{}", generate_random_string(16))
}

pub(in crate::api::endpoints::hls_api) fn is_hls_media_activity_status(status: StatusCode) -> bool {
    matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
}

pub(in crate::api::endpoints::hls_api) async fn hls_cache_response_context(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    lease_identity: HlsMediaLeaseIdentity,
    now_ms: u64,
) -> HlsCacheResponseContext {
    let qos_meter = app_state.hls.proxy.qos().meter_for_access_lease(&access_context.lease_id).await;
    let (log_identity, session_owner, playback_request_id) = {
        let session = session.read().await;
        let binding = session.origin_account_binding.as_ref().filter(|binding| binding.is_active());
        (
            HlsLogIdentity::from_session(&session),
            binding.map(|binding| binding.session_owner.clone()),
            binding.and_then(|binding| binding.playback_request_id),
        )
    };
    HlsCacheResponseContext::new(
        access_context.lease_id.clone(),
        log_identity,
        app_state.hls.proxy.cache_duration_seconds(),
        Arc::clone(app_state.hls.proxy.metrics()),
        Arc::clone(app_state.hls.proxy.segment_repair()),
        qos_meter,
        Some(
            HlsMediaActivityMarker::new(
                Arc::clone(&app_state.hls.proxy),
                Arc::clone(session),
                access_context.proxy_session_id.clone(),
                access_context.lease_id.clone(),
                lease_identity,
            )
            .with_active_provider(
                Arc::clone(&app_state.active_provider),
                session_owner,
                playback_request_id,
            ),
        ),
        now_ms,
    )
}

pub(in crate::api::endpoints::hls_api) fn hls_qos_meter_init(
    app_state: &Arc<AppState>,
    qos_config: HlsQosRuntimeConfig,
) -> Option<HlsQosMeterInit> {
    if !qos_config.live_metering_enabled {
        return None;
    }
    let meter_uid = app_state.connection_manager.next_stream_uid();
    let meter = Arc::new(StreamMeterHandle::new(meter_uid, Arc::downgrade(&app_state.event_manager)));
    Some(HlsQosMeterInit { meter_uid, meter })
}

pub(in crate::api::endpoints::hls_api) async fn register_hls_cache_stream_for_successful_media_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    access_context: &HlsAccessContext,
    session: &HlsSessionHandle,
    response_context: &HlsCacheResponseContext,
) {
    if ensure_hls_cache_stream_registered(app_state, fingerprint, headers, access_context, session).await.is_none() {
        debug!(
            "HLS media registration skipped: lease={} reason=session-or-connection-unavailable",
            safe_hls_access_lease_id(&access_context.lease_id)
        );
    }
    response_context.set_qos_meter(app_state.hls.proxy.qos().meter_for_access_lease(&access_context.lease_id).await);
}

pub(in crate::api::endpoints::hls_api) async fn validate_hls_proxy_access_request(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    timing: HlsAccessLeaseTiming,
    request_kind: &'static str,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    let context = validate_hls_proxy_access_context(
        app_state,
        fingerprint,
        proxy_session_id,
        hls_access_lease_id,
        now_ms,
        HlsAccessAdmissionMode::ResourceAccess,
    )
    .await?;
    let startup_admission_pending = app_state
        .hls
        .proxy
        .access_lease_response_snapshot(&context.lease_id, proxy_session_id, now_ms)
        .await
        .is_some_and(|lease| {
            lease.state == HlsAccessLeaseState::Pending
                && lease.startup_admission == HlsLeaseStartupAdmissionState::Pending
        });
    if startup_admission_pending {
        return Err(HlsAccessLeaseValidationError::AvailabilityPending);
    }
    match app_state.hls.proxy.activate_access_lease(&context.lease_id, proxy_session_id, now_ms, timing).await {
        HlsAccessLeaseActivation::Activated { .. } => {
            debug!(
                "HLS access lease accepted: lease={} proxy_session={} user_session={} request={request_kind}",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            Ok(context)
        }
        HlsAccessLeaseActivation::Denied => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} user_session={} request={request_kind} reason=denied",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            let (runtime_tail, reason) = app_state
                .hls
                .proxy
                .access_lease_response_snapshot(&context.lease_id, proxy_session_id, now_ms)
                .await
                .map_or((None, None), |lease| {
                    (lease.runtime_policy_revocation_outcome(), lease.runtime_policy_denial_reason())
                });
            Err(HlsAccessLeaseValidationError::AdmissionDenied { runtime_tail, reason })
        }
        HlsAccessLeaseActivation::Expired
        | HlsAccessLeaseActivation::UnknownLease
        | HlsAccessLeaseActivation::SessionMismatch => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} user_session={} request={request_kind} reason=expired",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            Err(HlsAccessLeaseValidationError::Expired)
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn validate_hls_proxy_access_context(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    admission_mode: HlsAccessAdmissionMode,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    validate_hls_access_lease(
        &app_state.hls_ctx(),
        fingerprint,
        proxy_session_id,
        &HlsAccessLeaseId(hls_access_lease_id.to_string()),
        now_ms,
        admission_mode,
    )
    .await
}
