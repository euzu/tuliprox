use super::{
    build_hls_manifest_request_headers, build_hls_origin_resolution, build_hls_origin_source_for_playback,
    build_virtual_hls_entry_path, get_stream_channel, hls_canonical_status_response,
    hls_manifest_access_context_and_state, hls_manifest_terminal_preflight, hls_terminal_failed_closed_response,
    hls_terminal_playback_response, resolve_hls_origin_playlist_url, resolve_hls_terminal_manifest_state,
    try_hls_cache_canonical_manifest_response, HlsCacheManifestOrigin, HlsManifestRefreshOrdering,
    HlsManifestTerminalPreflight, HlsOriginEntryUrl, HlsProxyManifestPathParams,
};
use crate::{
    api::{api_utils::is_hls_stream_share_enabled, model::AppState},
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use shared::model::VirtualId;
use std::sync::Arc;
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    HlsAccessContext, HlsAccessLease, HlsAccessLeaseId, HlsLeasePlaybackMode, HlsOriginSource,
    HlsRuntimeCustomTailReason, HlsSessionHandle, HlsTerminalFailedClosedReason, ProxySessionId,
};

pub(in crate::api::endpoints::hls_api) async fn mark_successful_canonical_manifest_activity(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    mark_hls_authorized_media_access(app_state, session, now_ms).await;
}

pub(in crate::api::endpoints::hls_api) async fn mark_hls_authorized_manifest_access(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    session.write().await.mark_authorized_manifest_access(now_ms);
    app_state.hls.proxy.schedule_session_idle_for_handle(session).await;
}

pub(in crate::api::endpoints::hls_api) async fn mark_hls_authorized_media_access(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    session.write().await.mark_authorized_media_access(now_ms);
    app_state.hls.proxy.schedule_session_idle_for_handle(session).await;
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_configured(app_state: &Arc<AppState>) -> bool {
    let config = app_state.app_config.config.load();
    config.reverse_proxy.as_ref().is_some_and(|reverse_proxy| reverse_proxy.hls_cache.is_some())
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_enabled_for_target(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
) -> bool {
    hls_cache_configured(app_state) && is_hls_stream_share_enabled(target)
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_enabled_for_user(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
) -> bool {
    // Recording headers can select different origin content from ordinary playback.
    !user.is_recording_proxy_user() && hls_cache_enabled_for_target(app_state, target)
}

pub(in crate::api::endpoints::hls_api) struct HlsAccessManifestRequestContext {
    pub(in crate::api::endpoints::hls_api) input: Arc<ConfigInput>,
    pub(in crate::api::endpoints::hls_api) hls_url: String,
    pub(in crate::api::endpoints::hls_api) session_entry_url: HlsOriginEntryUrl,
    pub(in crate::api::endpoints::hls_api) original_hls_entry_path: String,
    pub(in crate::api::endpoints::hls_api) origin_source: HlsOriginSource,
    pub(in crate::api::endpoints::hls_api) headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) server_path: Option<String>,
}

pub(in crate::api::endpoints::hls_api) async fn resolve_hls_playback_manifest_request_context(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    req_headers: &HeaderMap,
) -> Result<HlsAccessManifestRequestContext, StatusCode> {
    let Some((user, target)) = app_state.app_config.get_target_for_username(&access_context.username) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if !hls_cache_enabled_for_target(app_state, &target) {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(input) = app_state.app_config.get_input_by_id(access_context.input_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if app_state
        .active_users
        .is_user_blocked_for_stream(&user.username, VirtualId::new(access_context.virtual_id))
        .await
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(channel) = get_stream_channel(app_state, &target, access_context.virtual_id).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    let origin_playlist_url = if let Some(archive_url) = access_context.archive_origin_url.as_ref() {
        archive_url.clone()
    } else {
        resolve_hls_origin_playlist_url(app_state, &target, &input, access_context.virtual_id, channel.url.as_ref())
            .await?
    };
    let Some(hls_cache_origin) = build_hls_origin_resolution(&input, &origin_playlist_url) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let origin_source = build_hls_origin_source_for_playback(
        &input,
        access_context.stream_ref.clone(),
        access_context.epg_reference_ts,
        Some(&origin_playlist_url),
    );
    let Some(server_info) = app_state.app_config.get_user_server_info(&user) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let headers = build_hls_manifest_request_headers(
        &input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
        channel.upstream_user_agent.as_deref(),
    );

    let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, access_context.virtual_id);

    Ok(HlsAccessManifestRequestContext {
        input,
        hls_url: hls_cache_origin.hls_url,
        session_entry_url: hls_cache_origin.session_entry_url,
        original_hls_entry_path,
        origin_source,
        headers,
        server_path: server_info.path.clone(),
    })
}

pub(in crate::api::endpoints::hls_api) async fn hls_manifest_preflight_refresh_ordering(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    lease: &HlsAccessLease,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> Result<HlsManifestRefreshOrdering, Box<axum::response::Response>> {
    match hls_manifest_terminal_preflight(session, lease, now_ms).await {
        HlsManifestTerminalPreflight::ServeCommittedPlayback => {
            Err(Box::new(hls_terminal_playback_response(lease, proxy_session_id, access_lease_id).unwrap_or_else(
                || hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::RuntimeUnavailable),
            )))
        }
        HlsManifestTerminalPreflight::BootstrapPendingLease => Ok(HlsManifestRefreshOrdering::Background),
        HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation => {
            Ok(HlsManifestRefreshOrdering::AwaitBeforeTerminalEvaluation)
        }
        HlsManifestTerminalPreflight::EvaluateTerminal => resolve_hls_terminal_manifest_state(
            app_state,
            session,
            proxy_session_id,
            access_lease_id,
            lease.clone(),
            now_ms,
        )
        .await
        .map(|_| HlsManifestRefreshOrdering::Background),
        HlsManifestTerminalPreflight::FailClosed { reason } => {
            Err(Box::new(hls_terminal_failed_closed_response(reason)))
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn hls_proxy_manifest(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyManifestPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let access_lease_id = HlsAccessLeaseId(params.hls_access_lease_id);
    let now_ms = current_time_millis();
    let access_lease_snapshot =
        app_state.hls.proxy.access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms).await;
    if let Some(lease) = access_lease_snapshot.as_ref() {
        let standalone_policy_response_required = lease.playback_mode == HlsLeasePlaybackMode::Ended
            && lease
                .runtime_policy_denial_reason()
                .is_some_and(HlsRuntimeCustomTailReason::permits_unpublished_lease_standalone_tail);
        if let (false, Some(response)) = (
            standalone_policy_response_required,
            hls_terminal_playback_response(lease, &proxy_session_id, &access_lease_id),
        ) {
            return response;
        }
    }
    let session = app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await;
    if let Some(session) = session.as_ref() {
        app_state
            .hls
            .proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                session,
                &proxy_session_id,
                now_ms,
            )
            .await;
    }
    let (access_context, access_lease_state) = match hls_manifest_access_context_and_state(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &access_lease_id,
        access_lease_snapshot.as_ref(),
        now_ms,
    )
    .await
    {
        Ok(context_and_state) => context_and_state,
        Err(response) => return *response,
    };
    let refresh_ordering = if let (Some(session), Some(lease)) = (session.as_ref(), access_lease_snapshot.as_ref()) {
        match hls_manifest_preflight_refresh_ordering(
            &app_state,
            session,
            lease,
            &proxy_session_id,
            &access_lease_id,
            now_ms,
        )
        .await
        {
            Ok(ordering) => ordering,
            Err(response) => return *response,
        }
    } else {
        HlsManifestRefreshOrdering::Background
    };
    let request_context =
        match resolve_hls_playback_manifest_request_context(&app_state, &access_context, &headers).await {
            Ok(context) => context,
            Err(status) => return hls_canonical_status_response(status),
        };

    try_hls_cache_canonical_manifest_response(
        &app_state,
        &fingerprint,
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        access_lease_state,
        HlsCacheManifestOrigin {
            raw_request_url: &request_context.hls_url,
            session_entry_url: request_context.session_entry_url.clone(),
            input: &request_context.input,
            origin_source: request_context.origin_source,
        },
        request_context.headers,
        request_context.server_path.as_deref(),
        &request_context.original_hls_entry_path,
        refresh_ordering,
    )
    .await
    .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}
