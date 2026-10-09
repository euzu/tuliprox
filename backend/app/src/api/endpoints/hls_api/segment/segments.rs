use super::{
    current_hls_resource_lease, hls_cache_response_context, hls_lease_allows_cached_segment,
    hls_lease_allows_live_origin_work, hls_origin_runtime_resource_failure_response,
    hls_resource_channel_unavailable_response, hls_resource_serve_outcome_response,
    hls_segment_request_requires_origin_work, is_hls_media_activity_status,
    prepare_hls_origin_binding_for_authorized_resource_work, prepare_hls_resource_access,
    register_hls_cache_stream_for_successful_media_response, terminal_segment_get_response,
    terminal_segment_head_response, terminal_segment_immutable_replay_response, terminal_tail_plan_for_current_route,
    HlsOriginWorkKind, HlsProxySegmentPathParams, HlsProxyTerminalSegmentPathParams, HlsResourceAccess,
};
use crate::{
    api::model::{AppState, ProviderHandle},
    auth::Fingerprint,
};
use axum::{
    http::{header, HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use std::sync::Arc;
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    serve_hls_segment_cache_outcome, HlsAccessContext, HlsAccessLeaseId, HlsAccountOverlapTiming,
    HlsEffectiveOriginAcquirePolicy, HlsLeasePlaybackMode, HlsOriginIoContext, HlsOriginWorkClass, HlsSegmentFile,
    HlsSessionHandle, HlsTerminalSegmentPath, ProxySessionId, SegmentCacheStatus, SegmentDemandFetchOutcome,
    SegmentFetchContext,
};

pub(in crate::api::endpoints::hls_api) async fn hls_proxy_segment(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxySegmentPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let now_ms = current_time_millis();
    let HlsResourceAccess { session, access_context, lease } = match prepare_hls_resource_access(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &params.hls_access_lease_id,
        now_ms,
        "segment",
    )
    .await
    {
        Ok(access) => access,
        Err(response) => return *response,
    };
    let Some(segment_file) = HlsSegmentFile::parse(&params.segment_file) else {
        return hls_resource_channel_unavailable_response(&app_state, &access_context);
    };
    if !hls_lease_allows_cached_segment(&lease, segment_file.proxy_seq) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let allows_origin_work = hls_lease_allows_live_origin_work(&lease);
    if let Err(response) =
        validate_hls_segment_entry(&app_state, &session, &access_context, &segment_file, allows_origin_work).await
    {
        return *response;
    }
    let revision_delivery = session.read().await.startup.is_some();
    let demand_result = if allows_origin_work && !revision_delivery {
        demand_fetch_hls_live_segment(
            &app_state,
            &session,
            &segment_file,
            &access_context,
            &fingerprint,
            &headers,
            now_ms,
        )
        .await
    } else {
        Ok(())
    };
    if let Err(response) = demand_result {
        return *response;
    }

    serve_hls_segment_for_current_lease(
        &app_state,
        &session,
        &access_context,
        &fingerprint,
        &headers,
        segment_file,
        now_ms,
    )
    .await
}

pub(in crate::api::endpoints::hls_api) async fn validate_hls_segment_entry(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    segment_file: &HlsSegmentFile,
    allows_origin_work: bool,
) -> Result<(), Box<axum::response::Response>> {
    let session = session.read().await;
    if session.is_gc_marked_for_removal() {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
        return Err(Box::new(hls_resource_channel_unavailable_response(app_state, access_context)));
    };
    if entry.proxy_file_ext != segment_file.extension {
        return Err(Box::new(hls_resource_channel_unavailable_response(app_state, access_context)));
    }
    if !allows_origin_work && !matches!(&entry.status, SegmentCacheStatus::Ready { .. }) {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    }
    Ok(())
}

pub(in crate::api::endpoints::hls_api) async fn demand_fetch_hls_live_segment(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    now_ms: u64,
) -> Result<(), Box<axum::response::Response>> {
    let preacquired_provider_handle = if hls_segment_request_requires_origin_work(session, segment_file).await {
        match prepare_hls_origin_binding_for_authorized_resource_work(
            app_state,
            session,
            access_context,
            fingerprint,
            headers,
            HlsOriginWorkKind::Segment,
            now_ms,
        )
        .await
        {
            Ok(handle) => handle,
            Err(err) => {
                return Err(Box::new(hls_origin_runtime_resource_failure_response(app_state, access_context, err)))
            }
        }
    } else {
        None
    };
    match demand_fetch_hls_segment_if_needed(
        app_state,
        session,
        segment_file,
        access_context,
        fingerprint,
        preacquired_provider_handle,
        now_ms,
    )
    .await
    {
        SegmentDemandFetchOutcome::NotFound => {
            Err(Box::new(hls_resource_channel_unavailable_response(app_state, access_context)))
        }
        SegmentDemandFetchOutcome::Ready
        | SegmentDemandFetchOutcome::QueuedOrFetching
        | SegmentDemandFetchOutcome::Unavailable
        | SegmentDemandFetchOutcome::TimedOut => Ok(()),
    }
}

pub(in crate::api::endpoints::hls_api) async fn serve_hls_segment_for_current_lease(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    segment_file: HlsSegmentFile,
    now_ms: u64,
) -> axum::response::Response {
    let Some(current_lease) = current_hls_resource_lease(app_state, access_context).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !hls_lease_allows_cached_segment(&current_lease, segment_file.proxy_seq) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let current_allows_origin_work = hls_lease_allows_live_origin_work(&current_lease);
    let Some(lease_identity) = current_lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let response_context = hls_cache_response_context(app_state, session, access_context, lease_identity, now_ms).await;
    let response = hls_resource_serve_outcome_response(
        app_state,
        access_context,
        serve_hls_segment_cache_outcome(
            Arc::clone(app_state.hls.proxy.segment_cache()),
            Arc::clone(session),
            segment_file,
            headers.get(header::RANGE).cloned(),
            &response_context,
        )
        .await,
    );
    if current_allows_origin_work && is_hls_media_activity_status(response.status()) {
        register_hls_cache_stream_for_successful_media_response(
            app_state,
            fingerprint,
            headers,
            access_context,
            session,
            &response_context,
        )
        .await;
    }
    response
}

pub(in crate::api::endpoints::hls_api) async fn hls_proxy_terminal_segment(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyTerminalSegmentPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let Some(path) = HlsTerminalSegmentPath::parse(&params.generation, &params.terminal_file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let now_ms = current_time_millis();
    let access_lease_id = HlsAccessLeaseId(params.hls_access_lease_id.clone());
    let immutable_replay_plan =
        app_state.hls.proxy.access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms).await.and_then(
            |lease| match lease.playback_mode {
                HlsLeasePlaybackMode::TerminalTail(plan) if plan.matches_route(&proxy_session_id, &access_lease_id) => {
                    Some(plan)
                }
                HlsLeasePlaybackMode::Live
                | HlsLeasePlaybackMode::TerminalTail(_)
                | HlsLeasePlaybackMode::TerminalUnavailable { .. }
                | HlsLeasePlaybackMode::Ended => None,
            },
        );
    let access = prepare_hls_resource_access(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &params.hls_access_lease_id,
        now_ms,
        "terminal-segment",
    )
    .await;
    let Ok(HlsResourceAccess { session, access_context, lease }) = access else {
        return immutable_replay_plan
            .and_then(|plan| {
                terminal_segment_immutable_replay_response(
                    &plan,
                    path,
                    headers.get(header::RANGE),
                    method == Method::HEAD,
                )
            })
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response());
    };
    let Some(current_lease) = current_hls_resource_lease(&app_state, &access_context).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(plan) =
        terminal_tail_plan_for_current_route(&lease, &current_lease, &proxy_session_id, &access_context.lease_id)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if method == Method::HEAD {
        return terminal_segment_head_response(&plan, path, headers.get(header::RANGE))
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response());
    }
    let Some(lease_identity) = current_lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let response_context =
        hls_cache_response_context(&app_state, &session, &access_context, lease_identity, now_ms).await;
    let Some(response) =
        terminal_segment_get_response(&plan, path, headers.get(header::RANGE), &response_context, &proxy_session_id)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if is_hls_media_activity_status(response.status()) {
        response_context.mark_media_activity().await;
        register_hls_cache_stream_for_successful_media_response(
            &app_state,
            &fingerprint,
            &headers,
            &access_context,
            &session,
            &response_context,
        )
        .await;
    }
    response
}

pub(in crate::api::endpoints::hls_api) async fn demand_fetch_hls_segment_if_needed(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    segment_file: &HlsSegmentFile,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    preacquired_provider_handle: Option<ProviderHandle>,
    now_ms: u64,
) -> SegmentDemandFetchOutcome {
    let context = build_hls_segment_fetch_context(
        app_state,
        session,
        Some(access_context.lease_id.clone()),
        fingerprint,
        preacquired_provider_handle,
    )
    .await;
    app_state.hls.proxy.segment_worker_pool().demand_fetch_and_wait(context, segment_file, now_ms).await
}

pub(in crate::api::endpoints::hls_api) async fn build_hls_segment_fetch_context(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    repair_access_lease_id: Option<HlsAccessLeaseId>,
    fingerprint: &Fingerprint,
    preacquired_provider_handle: Option<ProviderHandle>,
) -> SegmentFetchContext {
    let (headers, origin_provider_session_headers, origin_policy, reservation_ttl_secs) = {
        let session = session.read().await;
        (
            session.origin_request_headers.clone(),
            session.origin_provider_session_headers.clone(),
            session.effective_origin_acquire_policy_or_default(),
            session.account_overlap_timing().reservation_ttl_secs(),
        )
    };
    let mut origin_io = HlsOriginIoContext {
        ctx: app_state.hls_ctx(),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
    };
    if let Some(provider_handle) = preacquired_provider_handle {
        origin_io = origin_io.with_preacquired_provider_handle(provider_handle);
    }
    SegmentFetchContext {
        session: Arc::clone(session),
        segment_cache: Arc::clone(app_state.hls.proxy.segment_cache()),
        segment_repair: Arc::clone(app_state.hls.proxy.segment_repair()),
        repair_access_lease_id,
        headers,
        origin_provider_session_headers,
        client: app_state.http_clients.default.load().as_ref().clone(),
        no_redirect_client: app_state.http_clients.no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        origin_io: Some(origin_io),
    }
}

pub(in crate::api::endpoints::hls_api) async fn hls_effective_origin_acquire_policy(
    session: &HlsSessionHandle,
) -> HlsEffectiveOriginAcquirePolicy {
    session.read().await.effective_origin_acquire_policy_or_default()
}

pub(in crate::api::endpoints::hls_api) async fn hls_origin_account_reservation_ttl_secs_for_session(
    session: &HlsSessionHandle,
) -> u64 {
    session.read().await.account_overlap_timing().reservation_ttl_secs()
}

pub(in crate::api::endpoints::hls_api) fn hls_origin_account_reservation_ttl_secs_fallback() -> u64 {
    HlsAccountOverlapTiming::from_target_duration_secs(None).reservation_ttl_secs()
}
