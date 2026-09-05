#![allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn log_hls_initial_strip_publication(
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

pub(super) fn stripped_tail_segments(materialized: &HlsMaterializedSharedManifest) -> usize {
    materialized.initial_strip_outcome.as_ref().map_or(0, |outcome| match outcome {
        HlsInitialStripOutcome::Applied { effective, .. } => *effective,
        HlsInitialStripOutcome::Skipped { .. } => 0,
    })
}

pub(super) fn hls_access_lease_ttl_ms(app_state: &Arc<AppState>) -> u64 {
    app_state.hls_proxy.session_idle_timeout_ms()
}

pub(super) fn duration_to_millis_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(super) fn hls_pending_bootstrap_window_ms(app_state: &Arc<AppState>) -> u64 {
    duration_to_millis_saturating(hls_initial_manifest_decision_wait_timeout(app_state))
}

pub(super) async fn hls_access_lease_timing_for_session(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
) -> HlsAccessLeaseTiming {
    let timing = session.read().await.account_overlap_timing();
    let active_window_ms = timing.hard_active_window_ms.saturating_mul(2);
    HlsAccessLeaseTiming { active_window_ms, valid_window_ms: hls_access_lease_ttl_ms(app_state) }
}

pub(super) async fn touch_pending_manifest_follow_up_window(
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
        .hls_proxy
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

pub(super) struct HlsResourceAccess {
    pub(super) session: HlsSessionHandle,
    pub(super) access_context: HlsAccessContext,
    pub(super) lease: HlsAccessLease,
}

pub(super) async fn prepare_hls_resource_access(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    request_kind: &'static str,
) -> Result<HlsResourceAccess, Box<axum::response::Response>> {
    let Some(session) = app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    app_state
        .hls_proxy
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
    app_state
        .hls_proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            proxy_session_id,
            now_ms,
        )
        .await;
    reclaim_hls_account_overlap_if_needed(app_state, &session, now_ms).await;
    let Some(lease) =
        app_state.hls_proxy.access_lease_response_snapshot(&access_context.lease_id, proxy_session_id, now_ms).await
    else {
        return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
    };
    Ok(HlsResourceAccess { session, access_context, lease })
}

pub(super) fn hls_lease_allows_live_origin_work(lease: &HlsAccessLease) -> bool {
    matches!(lease.state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Activated | HlsAccessLeaseState::Idle)
        && lease.playback_mode == HlsLeasePlaybackMode::Live
}

pub(super) fn hls_lease_allows_cached_segment(lease: &HlsAccessLease, proxy_seq: u64) -> bool {
    match &lease.playback_mode {
        HlsLeasePlaybackMode::Live => true,
        HlsLeasePlaybackMode::TerminalTail(plan) => plan.protected_base_proxy_seqs.contains(&proxy_seq),
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => false,
    }
}

pub(super) async fn current_hls_resource_lease(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
) -> Option<HlsAccessLease> {
    app_state
        .hls_proxy
        .access_lease_response_snapshot(
            &access_context.lease_id,
            &access_context.proxy_session_id,
            current_time_millis(),
        )
        .await
}

pub(super) async fn hls_live_lease_identity_is_current(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    expected_identity: HlsMediaLeaseIdentity,
) -> bool {
    current_hls_resource_lease(app_state, access_context).await.is_some_and(|lease| {
        lease.playback_mode == HlsLeasePlaybackMode::Live && lease.media_identity() == Some(expected_identity)
    })
}

pub(super) fn create_hls_cache_user_session_token(
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

pub(super) fn is_hls_media_activity_status(status: StatusCode) -> bool {
    matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
}

pub(super) async fn hls_cache_response_context(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    lease_identity: HlsMediaLeaseIdentity,
    now_ms: u64,
) -> HlsCacheResponseContext {
    let qos_meter = app_state.hls_proxy.qos().meter_for_access_lease(&access_context.lease_id).await;
    let log_identity = {
        let session = session.read().await;
        HlsLogIdentity::from_session(&session)
    };
    HlsCacheResponseContext::new(
        access_context.lease_id.clone(),
        log_identity,
        app_state.hls_proxy.cache_duration_seconds(),
        Arc::clone(app_state.hls_proxy.metrics()),
        Arc::clone(app_state.hls_proxy.segment_repair()),
        qos_meter,
        Some(HlsMediaActivityMarker::new(
            Arc::clone(&app_state.hls_proxy),
            Arc::clone(session),
            access_context.proxy_session_id.clone(),
            access_context.lease_id.clone(),
            lease_identity,
        )),
        now_ms,
    )
}

pub(super) fn hls_qos_meter_init(
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

pub(super) async fn register_hls_cache_stream_for_successful_media_response(
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
    response_context.set_qos_meter(app_state.hls_proxy.qos().meter_for_access_lease(&access_context.lease_id).await);
}

pub(super) async fn hls_proxy_segment(
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
    let demand_result = if allows_origin_work {
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

pub(super) async fn validate_hls_segment_entry(
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

pub(super) async fn demand_fetch_hls_live_segment(
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

pub(super) async fn serve_hls_segment_for_current_lease(
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
            Arc::clone(app_state.hls_proxy.segment_cache()),
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

pub(super) async fn hls_proxy_terminal_segment(
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
        app_state.hls_proxy.access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms).await.and_then(
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

pub(super) async fn demand_fetch_hls_segment_if_needed(
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
    app_state.hls_proxy.segment_worker_pool().demand_fetch_and_wait(context, segment_file, now_ms).await
}

pub(super) async fn build_hls_segment_fetch_context(
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
        segment_cache: Arc::clone(app_state.hls_proxy.segment_cache()),
        segment_repair: Arc::clone(app_state.hls_proxy.segment_repair()),
        repair_access_lease_id,
        headers,
        origin_provider_session_headers,
        client: app_state.http_client.load().as_ref().clone(),
        no_redirect_client: app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        origin_io: Some(origin_io),
    }
}

pub(super) async fn hls_effective_origin_acquire_policy(session: &HlsSessionHandle) -> HlsEffectiveOriginAcquirePolicy {
    session.read().await.effective_origin_acquire_policy_or_default()
}

pub(super) async fn hls_origin_account_reservation_ttl_secs_for_session(session: &HlsSessionHandle) -> u64 {
    session.read().await.account_overlap_timing().reservation_ttl_secs()
}

pub(super) fn hls_origin_account_reservation_ttl_secs_fallback() -> u64 {
    HlsAccountOverlapTiming::from_target_duration_secs(None).reservation_ttl_secs()
}

pub(super) async fn hls_proxy_map(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyMapPathParams>,
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
        "map",
    )
    .await
    {
        Ok(access) => access,
        Err(response) => return *response,
    };
    if !hls_lease_allows_live_origin_work(&lease) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(map_file) = HlsMapFile::parse(&params.map_file) else {
        return hls_resource_channel_unavailable_response(&app_state, &access_context);
    };
    {
        let session_guard = session.read().await;
        if session_guard.is_gc_marked_for_removal() {
            return StatusCode::NOT_FOUND.into_response();
        }
        let Some(entry) = session_guard.maps.get(&map_file.proxy_map_id.into()) else {
            return hls_resource_channel_unavailable_response(&app_state, &access_context);
        };
        if entry.proxy_file_ext != map_file.extension {
            return hls_resource_channel_unavailable_response(&app_state, &access_context);
        }
    }

    let Some(current_lease) = current_hls_resource_lease(&app_state, &access_context).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if current_lease.playback_mode != HlsLeasePlaybackMode::Live {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(lease_identity) = current_lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let response_context =
        hls_cache_response_context(&app_state, &session, &access_context, lease_identity, now_ms).await;
    let response = hls_resource_serve_outcome_response(
        &app_state,
        &access_context,
        serve_hls_map_cache_outcome(
            Arc::clone(app_state.hls_proxy.segment_cache()),
            Arc::clone(&session),
            map_file,
            headers.get(header::RANGE).cloned(),
            &response_context,
        )
        .await,
    );
    if is_hls_media_activity_status(response.status()) {
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

pub(super) async fn hls_proxy_resource(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyResourcePathParams>,
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
        "resource",
    )
    .await
    {
        Ok(access) => access,
        Err(response) => return *response,
    };
    let Some(resource_file) = TransientResourceFile::parse(&params.resource_file) else {
        return hls_resource_channel_unavailable_response(&app_state, &access_context);
    };
    let Some(lease_identity) = lease.media_identity() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let context = HlsResourceEndpointContext {
        app_state: &app_state,
        session: &session,
        fingerprint: &fingerprint,
        headers: &headers,
        access_context: &access_context,
        lease_identity,
        published_resource_ids: lease.published_transient_resource_ids().clone(),
        resource_file,
        range_header: headers.get(header::RANGE).cloned(),
        now_ms,
    };
    match &lease.playback_mode {
        HlsLeasePlaybackMode::Live => serve_hls_live_transient_resource(context).await,
        HlsLeasePlaybackMode::TerminalTail(_) => serve_hls_terminal_key_resource(context, &lease.playback_mode).await,
        HlsLeasePlaybackMode::TerminalUnavailable { .. } | HlsLeasePlaybackMode::Ended => {
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

pub(super) struct HlsResourceEndpointContext<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) session: &'a HlsSessionHandle,
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) headers: &'a HeaderMap,
    pub(super) access_context: &'a HlsAccessContext,
    pub(super) lease_identity: HlsMediaLeaseIdentity,
    published_resource_ids: HlsPublishedTransientResourceIds,
    pub(super) resource_file: TransientResourceFile,
    pub(super) range_header: Option<HeaderValue>,
    pub(super) now_ms: u64,
}

pub(super) async fn serve_hls_terminal_key_resource(
    context: HlsResourceEndpointContext<'_>,
    playback_mode: &HlsLeasePlaybackMode,
) -> axum::response::Response {
    let HlsLeasePlaybackMode::TerminalTail(plan) = playback_mode else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let proxy_session_id = &context.access_context.proxy_session_id;
    let Some(binding) =
        plan.terminal_key_binding(proxy_session_id, &context.access_context.lease_id, &context.resource_file)
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !context.session.read().await.terminal_key_binding_is_current(
        &context.access_context.lease_id,
        plan.generation,
        &binding,
    ) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response_context = hls_cache_response_context(
        context.app_state,
        context.session,
        context.access_context,
        context.lease_identity,
        context.now_ms,
    )
    .await;
    let response = finite_hls_terminal_key_response(
        binding.bytes(),
        context.range_header.as_ref(),
        binding.content_type(),
        "private, max-age=300, immutable",
        &response_context,
        proxy_session_id,
        context.resource_file.resource_id.0,
    );
    if is_hls_media_activity_status(response.status()) {
        response_context.mark_media_activity().await;
        register_hls_cache_stream_for_successful_media_response(
            context.app_state,
            context.fingerprint,
            context.headers,
            context.access_context,
            context.session,
            &response_context,
        )
        .await;
    }
    response
}

pub(super) async fn serve_hls_live_transient_resource(
    context: HlsResourceEndpointContext<'_>,
) -> axum::response::Response {
    let cache_duration_ms = context.app_state.hls_proxy.cache_duration_seconds().saturating_mul(1_000);
    let Ok(cache_resolution) = resolve_hls_transient_object_cache_action(
        context.session,
        &context.access_context.proxy_session_id,
        HlsTransientResourceLeaseContext {
            access_lease_id: &context.access_context.lease_id,
            lease_issued_at_ms: context.lease_identity.lease_issued_at_ms(),
            published_resource_ids: &context.published_resource_ids,
        },
        &context.resource_file,
        context.range_header.as_ref(),
        context.now_ms,
        cache_duration_ms,
    )
    .await
    else {
        return hls_resource_channel_unavailable_response(context.app_state, context.access_context);
    };
    let resource = cache_resolution.resource;
    let origin_headers = cache_resolution.origin_headers;
    let origin_provider_session_headers = cache_resolution.origin_provider_session_headers;
    let cache_action = cache_resolution.action;

    match cache_action {
        HlsTransientObjectCacheAction::ServeReady => {
            return serve_transient_object_cache_response_and_mark_or_unavailable(TransientObjectCacheServeContext {
                app_state: context.app_state,
                session: context.session,
                fingerprint: context.fingerprint,
                headers: context.headers,
                access_context: context.access_context,
                lease_identity: context.lease_identity,
                resource_file: context.resource_file,
                range_header: context.range_header,
                now_ms: context.now_ms,
            })
            .await;
        }
        HlsTransientObjectCacheAction::WaitForFetch(notifier) => {
            return wait_for_transient_object_cache_fetch(TransientObjectWaitContext {
                app_state: context.app_state,
                session: context.session,
                fingerprint: context.fingerprint,
                headers: context.headers,
                access_context: context.access_context,
                lease_identity: context.lease_identity,
                resource_file: context.resource_file,
                range_header: context.range_header,
                notifier,
            })
            .await;
        }
        HlsTransientObjectCacheAction::FetchAndCache(_) | HlsTransientObjectCacheAction::PassthroughNoCache => {}
    }

    fetch_or_passthrough_transient_resource(HlsTransientPassthroughContext {
        endpoint: context,
        resource,
        cache_action,
        origin_headers,
        origin_provider_session_headers,
        cache_duration_ms,
    })
    .await
}

pub(super) struct HlsTransientPassthroughContext<'a> {
    pub(super) endpoint: HlsResourceEndpointContext<'a>,
    pub(super) resource: TransientResourceRef,
    pub(super) cache_action: HlsTransientObjectCacheAction,
    pub(super) origin_headers: HeaderMap,
    pub(super) origin_provider_session_headers: HeaderMap,
    pub(super) cache_duration_ms: u64,
}

pub(super) async fn fetch_or_passthrough_transient_resource(
    context: HlsTransientPassthroughContext<'_>,
) -> axum::response::Response {
    let HlsTransientPassthroughContext {
        endpoint,
        resource,
        cache_action,
        origin_headers,
        origin_provider_session_headers,
        cache_duration_ms,
    } = context;
    if let HlsTransientObjectCacheAction::FetchAndCache(fetch_token) = cache_action {
        return fetch_and_cache_transient_origin_response(HlsTransientEndpointCacheFetchContext {
            app_state: endpoint.app_state,
            session: endpoint.session,
            fingerprint: endpoint.fingerprint,
            headers: endpoint.headers,
            access_context: endpoint.access_context,
            lease_identity: endpoint.lease_identity,
            resource: &resource,
            resource_file: endpoint.resource_file,
            fetch_token: *fetch_token,
            origin_headers,
            origin_provider_session_headers,
            range_header: endpoint.range_header,
            cache_duration_ms,
        })
        .await;
    }

    let policy = endpoint.app_state.hls_proxy.segment_fetch_policy();
    let fetch_result = fetch_transient_origin_response_with_provider_io(HlsTransientEndpointOriginFetchRequest {
        app_state: endpoint.app_state,
        session: endpoint.session,
        access_context: endpoint.access_context,
        fingerprint: endpoint.fingerprint,
        headers: endpoint.headers,
        resource: &resource,
        resource_file: &endpoint.resource_file,
        origin_headers,
        origin_provider_session_headers,
        range_header: endpoint.range_header.clone(),
        policy: policy.clone(),
    })
    .await;
    serve_hls_transient_passthrough_result(endpoint, resource, policy, fetch_result).await
}

pub(super) async fn serve_hls_transient_passthrough_result(
    endpoint: HlsResourceEndpointContext<'_>,
    resource: TransientResourceRef,
    policy: SegmentFetchPolicy,
    fetch_result: HlsTransientOriginFetchResult,
) -> axum::response::Response {
    match fetch_result.result {
        Ok(response) => {
            if response.decoded.status.is_success() {
                let activity_outcome = endpoint
                    .app_state
                    .hls_proxy
                    .mark_authorized_media_access_for_lease_if_identity_matches(
                        endpoint.session,
                        &endpoint.access_context.lease_id,
                        &endpoint.access_context.proxy_session_id,
                        endpoint.lease_identity,
                        endpoint.now_ms,
                    )
                    .await;
                match activity_outcome {
                    HlsMediaActivityCommitOutcome::Committed => {}
                    HlsMediaActivityCommitOutcome::StaleLeaseIdentity => {
                        debug!(
                            "HLS transient media response discarded: lease={} proxy_session={} reason=playback-generation-race",
                            safe_hls_access_lease_id(&endpoint.access_context.lease_id),
                            safe_proxy_session_id(&endpoint.access_context.proxy_session_id)
                        );
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    HlsMediaActivityCommitOutcome::DeferredLockContention => {
                        debug!(
                            "HLS transient media response deferred: lease={} proxy_session={} reason=lock-contention",
                            safe_hls_access_lease_id(&endpoint.access_context.lease_id),
                            safe_proxy_session_id(&endpoint.access_context.proxy_session_id)
                        );
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                }
                if ensure_hls_cache_stream_registered(
                    endpoint.app_state,
                    endpoint.fingerprint,
                    endpoint.headers,
                    endpoint.access_context,
                    endpoint.session,
                )
                .await
                .is_none()
                {
                    debug!(
                        "HLS transient media registration skipped: lease={} reason=session-or-connection-unavailable",
                        safe_hls_access_lease_id(&endpoint.access_context.lease_id)
                    );
                }
            }
            hls_transient_origin_response(
                response,
                HlsTransientDirectResponseContext {
                    session: Arc::clone(endpoint.session),
                    resource,
                    policy: policy.clone(),
                    now_ms: endpoint.now_ms,
                    log_identity: {
                        let session = endpoint.session.read().await;
                        HlsLogIdentity::from_session(&session)
                    },
                },
            )
        }
        Err(err) => {
            if matches!(err, HlsOriginResourceFetchError::ProviderUnavailable(_)) {
                if let Some(runtime_err) = fetch_result.runtime_prepare_error {
                    return hls_origin_runtime_resource_failure_response(
                        endpoint.app_state,
                        endpoint.access_context,
                        runtime_err,
                    );
                }
            }
            match hls_transient_object_fetch_failure(&err) {
                HlsTransientObjectFetchFailure::Retryable => {
                    let failed_at_ms = current_time_millis();
                    if record_temporary_transient_segment_fetch_failure(
                        endpoint.session,
                        &resource,
                        &policy,
                        failed_at_ms,
                    )
                    .await
                    {
                        hls_resource_channel_unavailable_response(endpoint.app_state, endpoint.access_context)
                    } else {
                        hls_temporary_resource_unavailable_response(HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS)
                    }
                }
                HlsTransientObjectFetchFailure::Permanent { status: _ } => {
                    hls_resource_channel_unavailable_response(endpoint.app_state, endpoint.access_context)
                }
            }
        }
    }
}

pub(super) struct HlsTransientEndpointOriginFetchRequest<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) session: &'a HlsSessionHandle,
    pub(super) access_context: &'a HlsAccessContext,
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) headers: &'a HeaderMap,
    pub(super) resource: &'a TransientResourceRef,
    pub(super) resource_file: &'a TransientResourceFile,
    pub(super) origin_headers: HeaderMap,
    pub(super) origin_provider_session_headers: HeaderMap,
    pub(super) range_header: Option<HeaderValue>,
    pub(super) policy: SegmentFetchPolicy,
}

pub(super) struct HlsTransientOriginFetchResult {
    pub(super) result:
        Result<HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>, HlsOriginResourceFetchError>,
    pub(super) runtime_prepare_error: Option<HlsOriginRuntimeAcquireError>,
}

pub(super) async fn fetch_transient_origin_response_with_provider_io(
    request: HlsTransientEndpointOriginFetchRequest<'_>,
) -> HlsTransientOriginFetchResult {
    let clients = HlsOriginResourceClients {
        client: request.app_state.http_client.load().as_ref().clone(),
        no_redirect_client: request.app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: request.app_state.should_use_manual_redirects(),
    };
    let log_identity = {
        let session = request.session.read().await;
        HlsLogIdentity::from_session(&session)
    };
    let fetch_request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: request.resource.resolved_origin_uri.clone(),
        origin_headers: request.origin_headers,
        origin_provider_session_headers: request.origin_provider_session_headers,
        range_header: request.range_header,
        resource_file: request.resource_file.clone(),
        resource_kind: request.resource.kind,
        clients,
        policy: request.policy,
        log_identity,
    };
    let runtime_prepare_error = Arc::new(tokio::sync::Mutex::new(None));
    let prepare_attempt = hls_transient_origin_prepare_closure(
        request.app_state,
        request.session,
        request.access_context,
        request.fingerprint,
        request.headers,
        &runtime_prepare_error,
    );
    let result = fetch_hls_transient_origin_response_with_attempt_prepare(fetch_request, prepare_attempt).await;
    let runtime_prepare_error = *runtime_prepare_error.lock().await;
    HlsTransientOriginFetchResult { result, runtime_prepare_error }
}

/// Builds the shared per-attempt prepare closure for transient origin fetches.
/// Runtime acquire failures are captured in `runtime_prepare_error` and mapped
/// to a provider-unavailable fetch error so the retry loop can proceed uniformly.
pub(super) fn hls_transient_origin_prepare_closure(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    runtime_prepare_error: &Arc<tokio::sync::Mutex<Option<HlsOriginRuntimeAcquireError>>>,
) -> impl FnMut(
    HlsResourceFetchAttempt,
) -> futures::future::BoxFuture<
    'static,
    Result<Option<HlsTransientOriginIoGuard>, HlsOriginResourceFetchError>,
> {
    let app_state = Arc::clone(app_state);
    let session = Arc::clone(session);
    let access_context = access_context.clone();
    let fingerprint = fingerprint.clone();
    let headers = headers.clone();
    let runtime_prepare_error = Arc::clone(runtime_prepare_error);
    move |_attempt| {
        let app_state = Arc::clone(&app_state);
        let session = Arc::clone(&session);
        let access_context = access_context.clone();
        let fingerprint = fingerprint.clone();
        let headers = headers.clone();
        let runtime_prepare_error = Arc::clone(&runtime_prepare_error);
        async move {
            match prepare_hls_transient_origin_io_for_authorized_resource_work(
                &app_state,
                &session,
                &access_context,
                &fingerprint,
                &headers,
                current_time_millis(),
            )
            .await
            {
                Ok(guard) => Ok(guard),
                Err(err) => {
                    *runtime_prepare_error.lock().await = Some(err);
                    Err(HlsOriginResourceFetchError::ProviderUnavailable(HlsBoundAccountAcquireErrorKind::Unavailable))
                }
            }
        }
        .boxed()
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) struct HlsTransientEndpointCacheFetchContext<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) session: &'a HlsSessionHandle,
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) headers: &'a HeaderMap,
    pub(super) access_context: &'a HlsAccessContext,
    pub(super) lease_identity: HlsMediaLeaseIdentity,
    pub(super) resource: &'a TransientResourceRef,
    pub(super) resource_file: TransientResourceFile,
    pub(super) fetch_token: TransientObjectFetchToken,
    pub(super) origin_headers: HeaderMap,
    pub(super) origin_provider_session_headers: HeaderMap,
    pub(super) range_header: Option<HeaderValue>,
    pub(super) cache_duration_ms: u64,
}

pub(super) struct TransientObjectWaitContext<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) session: &'a HlsSessionHandle,
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) headers: &'a HeaderMap,
    pub(super) access_context: &'a HlsAccessContext,
    pub(super) lease_identity: HlsMediaLeaseIdentity,
    pub(super) resource_file: TransientResourceFile,
    pub(super) range_header: Option<HeaderValue>,
    pub(super) notifier: Arc<tokio::sync::Notify>,
}

pub(super) struct TransientObjectCacheServeContext<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) session: &'a HlsSessionHandle,
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) headers: &'a HeaderMap,
    pub(super) access_context: &'a HlsAccessContext,
    pub(super) lease_identity: HlsMediaLeaseIdentity,
    pub(super) resource_file: TransientResourceFile,
    pub(super) range_header: Option<HeaderValue>,
    pub(super) now_ms: u64,
}

pub(super) async fn serve_transient_object_cache_response_and_mark(
    context: TransientObjectCacheServeContext<'_>,
) -> axum::response::Response {
    if !hls_live_lease_identity_is_current(context.app_state, context.access_context, context.lease_identity).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response_context = hls_cache_response_context(
        context.app_state,
        context.session,
        context.access_context,
        context.lease_identity,
        context.now_ms,
    )
    .await;
    let response = hls_resource_serve_outcome_response(
        context.app_state,
        context.access_context,
        serve_hls_transient_object_cache_outcome(
            Arc::clone(context.app_state.hls_proxy.segment_cache()),
            Arc::clone(context.session),
            context.resource_file,
            context.range_header,
            &response_context,
        )
        .await,
    );
    if is_hls_media_activity_status(response.status()) {
        register_hls_cache_stream_for_successful_media_response(
            context.app_state,
            context.fingerprint,
            context.headers,
            context.access_context,
            context.session,
            &response_context,
        )
        .await;
    }
    response
}

pub(super) async fn serve_transient_object_cache_response_and_mark_or_unavailable(
    context: TransientObjectCacheServeContext<'_>,
) -> axum::response::Response {
    serve_transient_object_cache_response_and_mark(context).await
}

pub(super) async fn wait_for_transient_object_cache_fetch(
    context: TransientObjectWaitContext<'_>,
) -> axum::response::Response {
    let wait_timeout = context.app_state.hls_proxy.segment_fetch_policy().origin_object_wait_timeout();
    let safe_resource_id = safe_transient_resource_id(&context.resource_file.resource_id);
    debug!(
        "HLS transient object wait started: resource_id={} lease={} state=inflight",
        safe_resource_id,
        safe_hls_access_lease_id(&context.access_context.lease_id)
    );
    let wait_result = tokio::time::timeout(wait_timeout, context.notifier.notified()).await;
    if wait_result.is_err() {
        debug!(
            "HLS transient object wait timed out: resource_id={} lease={} state=inflight",
            safe_resource_id,
            safe_hls_access_lease_id(&context.access_context.lease_id)
        );
        return hls_transient_object_unavailable_response(
            context.app_state,
            context.session,
            &context.resource_file,
            current_time_millis(),
            context.access_context,
        )
        .await;
    }
    let response = serve_transient_object_cache_response_and_mark_or_unavailable(TransientObjectCacheServeContext {
        app_state: context.app_state,
        session: context.session,
        fingerprint: context.fingerprint,
        headers: context.headers,
        access_context: context.access_context,
        lease_identity: context.lease_identity,
        resource_file: context.resource_file,
        range_header: context.range_header,
        now_ms: current_time_millis(),
    })
    .await;
    debug!(
        "HLS transient object wait completed: resource_id={} lease={} status={}",
        safe_resource_id,
        safe_hls_access_lease_id(&context.access_context.lease_id),
        response.status()
    );
    response
}

pub(super) fn safe_transient_resource_id(resource_id: &TransientResourceId) -> String {
    // Truncate at the first char boundary at or before byte 8 to avoid allocating
    // a temporary `String` of 8 chars (and a second UTF-8 walk via `len()`).
    let full = resource_id.0.as_str();
    let truncate_at = full.char_indices().nth(8).map_or(full.len(), |(byte_idx, _)| byte_idx);
    if truncate_at == full.len() {
        return full.to_owned();
    }
    let mut out = String::with_capacity(truncate_at + 3);
    out.push_str(&full[..truncate_at]);
    out.push_str("...");
    out
}

pub(super) async fn validate_hls_proxy_access_request(
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
        .hls_proxy
        .access_lease_response_snapshot(&context.lease_id, proxy_session_id, now_ms)
        .await
        .is_some_and(|lease| {
            lease.state == HlsAccessLeaseState::Pending
                && lease.startup_admission == HlsLeaseStartupAdmissionState::Pending
        });
    if startup_admission_pending {
        return Err(HlsAccessLeaseValidationError::AvailabilityPending);
    }
    match app_state.hls_proxy.activate_access_lease(&context.lease_id, proxy_session_id, now_ms, timing).await {
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
                .hls_proxy
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

pub(super) async fn ensure_hls_cache_stream_registered(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    access: &HlsAccessContext,
    session: &HlsSessionHandle,
) -> Option<StreamInfo> {
    let (proxy_session_id, origin_source, origin_account_binding) = {
        let session = session.read().await;
        if session.is_gc_marked_for_removal() {
            return None;
        }
        (session.proxy_session_id.clone(), session.origin_source.clone(), session.origin_account_binding.clone())
    };
    let user = app_state.app_config.get_user_credentials(&access.username)?;
    let user_session =
        app_state.active_users.get_and_update_user_session(&access.username, &access.user_session_token).await?;
    let connection_kind = user_session.connection_kind?;
    let priority = connection_priority_for_kind(&user, connection_kind);
    let mut stream_channel = build_hls_cache_stream_channel(app_state, access, &origin_source, &proxy_session_id).await;
    let provider = hls_cache_stats_provider(&origin_source, origin_account_binding.as_ref(), &user_session);
    let user_agent = req_headers
        .get(header::USER_AGENT)
        .map_or_else(|| Cow::Borrowed(""), |value| String::from_utf8_lossy(value.as_bytes()));

    stream_channel.url = Arc::from(hls_cache_stream_stats_url(&proxy_session_id));
    // Panel Streams/History read this item_type. Shared HLS transport is still HLS, but
    // archive/catchup leases must never be published as Live/LiveHls.
    let panel_archive_reference = origin_source
        .archive_reference
        .or(access.epg_reference_ts)
        .or_else(|| access.archive_origin_url.as_deref().and_then(m3u_archive_epg_reference_ts))
        .or_else(|| m3u_catchup_epg_reference_from_session_token(&access.user_session_token))
        .or(stream_channel.epg_reference_ts);
    let is_archive_playback = panel_archive_reference.is_some()
        || access.archive_origin_url.is_some()
        || origin_source.archive_reference.is_some()
        || is_m3u_catchup_session_token(&access.user_session_token)
        || stream_channel.item_type == PlaylistItemType::Catchup;
    if is_archive_playback {
        stream_channel.item_type = PlaylistItemType::Catchup;
        stream_channel.cluster = XtreamCluster::Video;
        stream_channel.epg_reference_ts = panel_archive_reference;
    } else {
        stream_channel.item_type = PlaylistItemType::LiveHls;
        stream_channel.cluster = PlaylistItemType::LiveHls.cluster();
    }
    let shared_stream_id = hls_cache_shared_stream_id(&proxy_session_id);
    stream_channel.shared = true;
    stream_channel.shared_stream_id = Some(shared_stream_id);
    stream_channel.shared_joined_existing = Some(
        hls_cache_shared_joined_existing(app_state, shared_stream_id, &access.username, &access.user_session_token)
            .await,
    );
    let qos_config = HlsQosRuntimeConfig::from_app_config(&app_state.app_config);
    let qos_registration = app_state
        .hls_proxy
        .qos()
        .ensure_access_lease(
            &access.lease_id,
            &proxy_session_id,
            current_time_millis(),
            hls_qos_meter_init(app_state, qos_config),
        )
        .await;
    if let Some(meter) = qos_registration.register_meter.as_ref() {
        app_state.event_manager.register_meter(Arc::clone(meter)).await;
    }
    let history_mode = if qos_registration.emit_connect_record {
        ConnectionHistoryMode::EmitConnect
    } else {
        ConnectionHistoryMode::RefreshOnly
    };

    app_state
        .connection_manager
        .update_connection_with_history_mode(
            crate::api::model::ConnectionParams {
                meter_uid: qos_registration.meter_uid,
                username: &access.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind,
                priority,
                soft_priority: user.soft_priority,
                fingerprint,
                provider,
                stream_channel: &stream_channel,
                user_agent,
                session_token: Some(&access.user_session_token),
            },
            history_mode,
        )
        .await
}

pub(super) fn hls_cache_stats_provider(
    origin_source: &HlsOriginSource,
    origin_account_binding: Option<&HlsOriginAccountBinding>,
    user_session: &UserSession,
) -> Arc<str> {
    origin_account_binding.filter(|binding| binding.is_active()).map_or_else(
        || {
            if user_session.provider.is_empty() {
                Arc::clone(&origin_source.input_name)
            } else {
                Arc::clone(&user_session.provider)
            }
        },
        |binding| Arc::clone(&binding.account_name),
    )
}

pub(super) async fn build_hls_cache_stream_channel(
    app_state: &Arc<AppState>,
    access: &HlsAccessContext,
    origin_source: &HlsOriginSource,
    proxy_session_id: &ProxySessionId,
) -> StreamChannel {
    let mut channel = if let Some((_, target)) = app_state.app_config.get_target_for_username(&access.username) {
        if let Some(mut channel) = get_stream_channel(app_state, &target, access.virtual_id).await {
            channel.url = Arc::from(hls_cache_stream_stats_url(proxy_session_id));
            channel
        } else {
            fallback_hls_cache_stream_channel(target.id, access.virtual_id, origin_source, proxy_session_id)
        }
    } else {
        fallback_hls_cache_stream_channel(0, access.virtual_id, origin_source, proxy_session_id)
    };

    let archive_reference = access
        .epg_reference_ts
        .or_else(|| access.archive_origin_url.as_deref().and_then(m3u_archive_epg_reference_ts))
        .or_else(|| m3u_catchup_epg_reference_from_session_token(&access.user_session_token));

    if archive_reference.is_some()
        || access.archive_origin_url.is_some()
        || is_m3u_catchup_session_token(&access.user_session_token)
    {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.cluster = PlaylistItemType::LiveHls.cluster();
        channel.epg_reference_ts = None;
    }
    channel
}

pub(super) fn fallback_hls_cache_stream_channel(
    target_id: u16,
    virtual_id: u32,
    origin_source: &HlsOriginSource,
    proxy_session_id: &ProxySessionId,
) -> StreamChannel {
    let unknown = "Unknown".intern();
    StreamChannel {
        target_id,
        virtual_id,
        provider_id: 0,
        input_name: Arc::clone(&origin_source.input_name),
        item_type: PlaylistItemType::LiveHls,
        cluster: XtreamCluster::Live,
        group: unknown.clone(),
        title: unknown,
        url: Arc::from(hls_cache_stream_stats_url(proxy_session_id)),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

pub(super) fn hls_cache_stream_stats_url(proxy_session_id: &ProxySessionId) -> String {
    format!("/hls/shared/live/{}/manifest.m3u8", proxy_session_id.0)
}

pub(super) fn hls_cache_shared_stream_id(proxy_session_id: &ProxySessionId) -> u64 {
    let digest = Sha256::digest(proxy_session_id.0.as_bytes());
    digest.iter().take(8).fold(0_u64, |value, byte| (value << 8) | u64::from(*byte))
}

pub(super) async fn hls_cache_shared_joined_existing(
    app_state: &Arc<AppState>,
    shared_stream_id: u64,
    username: &str,
    session_token: &str,
) -> bool {
    let streams = app_state.active_users.active_streams().await;
    if let Some(existing) = streams.iter().find(|stream| {
        stream.username == username
            && stream.session_token.as_deref() == Some(session_token)
            && stream.channel.shared
            && stream.channel.shared_stream_id == Some(shared_stream_id)
    }) {
        return existing.channel.shared_joined_existing.unwrap_or(false);
    }

    streams.iter().any(|stream| {
        stream.channel.shared
            && stream.channel.shared_stream_id == Some(shared_stream_id)
            && (stream.username != username || stream.session_token.as_deref() != Some(session_token))
    })
}

pub(super) async fn validate_hls_proxy_access_context(
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

pub(super) async fn hls_custom_video_manifest_response_for_username(
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

pub(super) async fn hls_custom_video_manifest_response_for_lease(
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

pub(super) async fn hls_runtime_custom_tail_response(
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
            app_state.hls_proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
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

pub(super) async fn hls_runtime_or_standalone_custom_tail_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let now_ms = current_time_millis();
    let Some(lease) =
        app_state.hls_proxy.access_lease_response_snapshot(access_lease_id, proxy_session_id, now_ms).await
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

pub(super) async fn hls_manifest_channel_unavailable_response_for_username(
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
pub(super) async fn hls_unpublished_lease_channel_unavailable_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> axum::response::Response {
    let reason = HlsRuntimeCustomTailReason::ChannelUnavailable;
    let lease = app_state
        .hls_proxy
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

pub(super) async fn hls_manifest_access_denial_runtime_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_snapshot: Option<&HlsAccessLease>,
    reason: HlsRuntimeCustomTailReason,
    fallback_status: StatusCode,
) -> axum::response::Response {
    let Some(lease) = lease_snapshot else {
        return fallback_status.into_response();
    };
    let Some(session) = app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await else {
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

pub(super) fn hls_resource_channel_unavailable_response(
    _app_state: &Arc<AppState>,
    _access_context: &HlsAccessContext,
) -> axum::response::Response {
    StatusCode::NOT_FOUND.into_response()
}

pub(super) fn hls_origin_runtime_resource_failure_response(
    _app_state: &Arc<AppState>,
    _access_context: &HlsAccessContext,
    err: HlsOriginRuntimeAcquireError,
) -> axum::response::Response {
    match err {
        HlsOriginRuntimeAcquireError::NoAccountAvailable { .. } => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        HlsOriginRuntimeAcquireError::Fatal(status) => hls_canonical_status_response(status),
    }
}

pub(super) fn hls_resource_serve_outcome_response(
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

pub(super) async fn hls_manifest_access_lease_validation_response(
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

pub(super) fn hls_resource_access_lease_validation_response(
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

pub(super) async fn hls_manifest_access_context_and_state(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_snapshot: Option<&HlsAccessLease>,
    now_ms: u64,
) -> Result<(HlsAccessContext, HlsAccessLeaseState), Box<axum::response::Response>> {
    app_state.hls_proxy.startup_observability().record_media_manifest_request(access_lease_id, now_ms);
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
        && app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await.is_none()
        && app_state.hls_proxy.expired_session_marker(proxy_session_id, now_ms).await.is_some()
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
        .hls_proxy
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

pub(super) async fn hls_transient_object_unavailable_response(
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

#[allow(clippy::too_many_lines)]
pub(super) async fn fetch_and_cache_transient_origin_response(
    context: HlsTransientEndpointCacheFetchContext<'_>,
) -> axum::response::Response {
    let policy = context.app_state.hls_proxy.segment_fetch_policy();
    let mut fetch_finalizer = HlsTransientObjectFetchFinalizer::new(
        Arc::clone(context.session),
        Arc::clone(context.app_state.hls_proxy.segment_cache()),
        context.fetch_token.clone(),
        HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
    );
    let clients = HlsOriginResourceClients {
        client: context.app_state.http_client.load().as_ref().clone(),
        no_redirect_client: context.app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: context.app_state.should_use_manual_redirects(),
    };
    let log_identity = {
        let session = context.session.read().await;
        HlsLogIdentity::from_session(&session)
    };
    let fetch_request = HlsTransientOriginFetchRequest {
        resolved_origin_uri: context.resource.resolved_origin_uri.clone(),
        origin_headers: context.origin_headers.clone(),
        origin_provider_session_headers: context.origin_provider_session_headers.clone(),
        range_header: None,
        resource_file: context.resource_file.clone(),
        resource_kind: context.resource.kind,
        clients,
        policy: policy.clone(),
        log_identity,
    };
    let cache_fetch_request = HlsTransientOriginCacheFetchRequest {
        fetch: fetch_request,
        commit: HlsTransientCacheCommitContext {
            segment_cache: Arc::clone(context.app_state.hls_proxy.segment_cache()),
            segment_repair: Arc::clone(context.app_state.hls_proxy.segment_repair()),
            session: Arc::clone(context.session),
            access_lease_id: context.access_context.lease_id.clone(),
            resource: context.resource.clone(),
            resource_file: context.resource_file.clone(),
            fetch_token: context.fetch_token.clone(),
            cache_duration_ms: context.cache_duration_ms,
        },
    };
    let runtime_prepare_error = Arc::new(tokio::sync::Mutex::new(None));
    let prepare_attempt = hls_transient_origin_prepare_closure(
        context.app_state,
        context.session,
        context.access_context,
        context.fingerprint,
        context.headers,
        &runtime_prepare_error,
    );
    let final_failure =
        match fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(cache_fetch_request, prepare_attempt)
            .await
        {
            Ok(()) => {
                let ready_at_ms = current_time_millis();
                let response_context = hls_cache_response_context(
                    context.app_state,
                    context.session,
                    context.access_context,
                    context.lease_identity,
                    ready_at_ms,
                )
                .await;
                let response = serve_hls_transient_object_cache_response(
                    Arc::clone(context.app_state.hls_proxy.segment_cache()),
                    Arc::clone(context.session),
                    context.resource_file.clone(),
                    context.range_header.clone(),
                    &response_context,
                )
                .await;
                if is_hls_media_activity_status(response.status()) {
                    register_hls_cache_stream_for_successful_media_response(
                        context.app_state,
                        context.fingerprint,
                        context.headers,
                        context.access_context,
                        context.session,
                        &response_context,
                    )
                    .await;
                }
                record_successful_transient_segment_fetch(context.session, context.resource).await;
                fetch_finalizer.complete();
                return response;
            }
            Err(err) => {
                if matches!(err, HlsOriginResourceFetchError::ProviderUnavailable(_)) {
                    let runtime_prepare_error = *runtime_prepare_error.lock().await;
                    if let Some(runtime_err) = runtime_prepare_error {
                        context.session.write().await.fail_transient_object_retryable_if_current(
                            &context.fetch_token,
                            current_time_millis(),
                            HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
                        );
                        return hls_origin_runtime_resource_failure_response(
                            context.app_state,
                            context.access_context,
                            runtime_err,
                        );
                    }
                }
                hls_transient_object_fetch_failure(&err)
            }
        };

    let failed_at_ms = current_time_millis();
    match final_failure {
        HlsTransientObjectFetchFailure::Retryable => {
            if record_temporary_transient_segment_fetch_failure(
                context.session,
                context.resource,
                &policy,
                failed_at_ms,
            )
            .await
            {
                context.session.write().await.fail_transient_object_permanent_if_current(
                    &context.fetch_token,
                    failed_at_ms,
                    None,
                );
            } else {
                context.session.write().await.fail_transient_object_retryable_if_current(
                    &context.fetch_token,
                    failed_at_ms,
                    HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
                );
            }
        }
        HlsTransientObjectFetchFailure::Permanent { status } => {
            context.session.write().await.fail_transient_object_permanent_if_current(
                &context.fetch_token,
                failed_at_ms,
                status,
            );
        }
    }
    hls_transient_object_unavailable_response(
        context.app_state,
        context.session,
        &context.resource_file,
        failed_at_ms,
        context.access_context,
    )
    .await
}

use tuliprox_core::utils::current_time_millis;

pub(super) async fn release_prepared_hls_manifest_session(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
    addr: &std::net::SocketAddr,
) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.release_unbound_session_reservation(username, session_token, None, false).await;
    app_state.active_users.clear_unbound_session_addr(username, session_token, addr).await;
}

pub(super) async fn terminate_failed_hls_manifest_session(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.terminate_session(username, session_token).await;
    app_state.active_provider.clear_provider_reservation(session_token).await;
}

pub(super) fn normalize_xtream_live_hls_url(hls_url: &str, input: &ConfigInput) -> String {
    if !input.input_type.is_xtream() || !input.has_flag(ConfigInputFlags::XtreamLiveStreamUsePrefix) {
        return hls_url.to_string();
    }

    let (Some(username), Some(password)) = (input.username.as_deref(), input.password.as_deref()) else {
        return hls_url.to_string();
    };

    let Ok(mut parsed) = Url::parse(hls_url) else {
        return hls_url.to_string();
    };
    let Some(segments) = parsed.path_segments() else {
        return hls_url.to_string();
    };

    let parts: Vec<&str> = segments.collect();
    if parts.len() >= 3 && parts[0] == username && parts[1] == password {
        parsed.set_path(&format!("/live/{}", parts.join("/")));
        return parsed.to_string();
    }

    hls_url.to_string()
}

pub(super) fn ensure_hls_manifest_extension(url: &str) -> String {
    let with_extension = replace_url_extension(url, HLS_EXT);
    let (base_url, suffix) = match with_extension.find(['?', '#'].as_ref()) {
        Some(pos) => (&with_extension[..pos], &with_extension[pos..]),
        None => (with_extension.as_str(), ""),
    };
    let Some(path_without_ext) = base_url.strip_suffix(HLS_EXT) else {
        return with_extension;
    };
    format!("{}{}{}", path_without_ext.trim_end_matches('.'), HLS_EXT, suffix)
}

pub(super) fn build_hls_manifest_request_headers(
    input_headers: &HashMap<String, String>,
    req_headers: &HeaderMap,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
    upstream_user_agent: Option<&str>,
) -> HeaderMap {
    let input_headers = input_headers
        .iter()
        .filter(|(key, _)| !should_remove_hls_origin_header(key, disabled_headers))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    let disabled_headers_for_filter = disabled_headers.cloned();
    let filter_header: HeaderFilter = Some(Box::new(move |name: &str| {
        !name.eq_ignore_ascii_case("range")
            && !should_remove_hls_origin_header(name, disabled_headers_for_filter.as_ref())
    }));
    let forwarded = get_headers_from_request(req_headers, &filter_header);
    let mut headers =
        request::get_request_headers(Some(&input_headers), Some(&forwarded), disabled_headers, default_user_agent);
    request::overlay_upstream_user_agent(&mut headers, upstream_user_agent, disabled_headers);
    scrub_hls_origin_headers(&mut headers, disabled_headers);
    force_identity_without_range(&mut headers);
    headers
}

pub(super) async fn download_legacy_hls_manifest(
    app_state: &Arc<AppState>,
    input: &InputSource,
    headers: &HeaderMap,
) -> Result<(String, String, HeaderMap), std::io::Error> {
    let deadline = Duration::from_millis(app_state.hls_proxy.origin_manifest_timeout_ms().max(1));
    let fetch_options = request::RequestFetchOptions::with_attempt_idle_timeout(deadline)
        .with_content_coding(OutboundContentCodingPolicy::Identity);
    let body_options = request::TextContentBodyOptions::hls_manifest(MAX_HLS_MANIFEST_BYTES, deadline);
    let options = request::TextContentFetchOptions::new(fetch_options, body_options);

    if app_state.should_use_manual_redirects() {
        request::download_text_content_with_manual_redirects_and_headers_and_options(
            &app_state.app_config,
            &app_state.http_client_no_redirect.load(),
            input,
            Some(headers),
            false,
            MAX_MANUAL_REDIRECTS,
            options,
        )
        .await
    } else {
        request::download_text_content_with_headers_and_options(
            &app_state.app_config,
            &app_state.http_client.load(),
            input,
            Some(headers),
            false,
            options,
        )
        .await
    }
}

pub(super) struct HlsCacheManifestOrigin<'a> {
    pub(super) raw_request_url: &'a str,
    pub(super) session_entry_url: HlsOriginEntryUrl,
    pub(super) input: &'a ConfigInput,
    pub(super) origin_source: HlsOriginSource,
}

pub(super) struct HlsCacheOriginResolution {
    pub(super) hls_url: String,
    pub(super) session_entry_url: HlsOriginEntryUrl,
}

pub(in crate::api) fn build_virtual_hls_entry_path(
    target: &ConfigTarget,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    virtual_id: u32,
) -> String {
    if input.input_type.is_m3u() && !target.has_output(TargetType::Xtream) {
        format!("/{}/live/{}/{}/{}{HLS_EXT}", storage_const::M3U_STREAM_PATH, user.username, user.password, virtual_id)
    } else {
        format!("/live/{}/{}/{}{HLS_EXT}", user.username, user.password, virtual_id)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api) async fn handle_hls_stream_request(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    user_session: Option<&UserSession>,
    session_token_hint: Option<&str>,
    hls_url: &str,
    archive_reference: Option<i64>,
    stream_context: HlsEntryStreamContext,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
    original_hls_entry_path: &str,
) -> impl IntoResponse + Send {
    let virtual_id = stream_context.virtual_id();
    if app_state.active_users.is_user_blocked_for_stream(&user.username, VirtualId::new(virtual_id)).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let stream_ref = stream_context.stream_ref().to_string();
    let normalized_hls_url = normalize_xtream_live_hls_url(hls_url, input);
    if normalized_hls_url != hls_url {
        debug_if_enabled!(
            "Normalized xtream hls url from {} to {}",
            sanitize_sensitive_info(hls_url),
            sanitize_sensitive_info(&normalized_hls_url)
        );
    }
    let url = ensure_hls_manifest_extension(&normalized_hls_url);
    // Recover archive context when callers (esp. Xtream timeshift) pass None but the
    // resolved provider URL / catchup session still carries Flussonic archive markers.
    let archive_reference = archive_reference.or_else(|| m3u_archive_epg_reference_ts(&url)).or_else(|| {
        user_session
            .map(|session| session.token.as_str())
            .or(session_token_hint)
            .and_then(m3u_catchup_epg_reference_from_session_token)
    });
    let hls_cache_origin = build_hls_origin_resolution(input, &url);
    let hls_origin_source = hls_cache_origin.as_ref().map(|_| {
        build_hls_origin_source_for_playback(input, stream_ref.clone(), archive_reference, Some(url.as_str()))
    });
    let server_info = app_state.app_config.get_user_server_info(user);
    let Some(server_info) = server_info else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let mut headers = build_hls_manifest_request_headers(
        &input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
        stream_context.identity().upstream_user_agent(),
    );

    if hls_cache_enabled_for_target(app_state, target) {
        let Some(origin_source) = hls_origin_source.clone() else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        return create_hls_cache_entry_master_playlist_response(
            app_state,
            fingerprint,
            user,
            origin_source,
            virtual_id,
            user_session,
            stream_context.known_bitrate_bps(),
            session_token_hint,
            if archive_reference.is_some() {
                url.as_str()
            } else {
                hls_cache_origin.as_ref().map_or(url.as_str(), |origin| origin.session_entry_url.as_str())
            },
            input,
            connection_permission,
            connection_kind,
            server_info.path.as_deref(),
        )
        .await;
    }

    let fallback_connection_kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);
    let (request_url, session_token, provider_handle, _selected_provider_config) = if let Some(session) = user_session {
        let pinned_provider = if session.provider.is_empty() { &input.name } else { &session.provider };
        let provider_handle = if let Some(handle) = app_state
            .active_provider
            .acquire_exact_connection_with_grace_for_session(
                pinned_provider,
                &fingerprint.addr,
                false,
                connection_priority_for_kind(
                    user,
                    session.connection_kind.or(connection_kind).unwrap_or(crate::api::model::ConnectionKind::Normal),
                ),
                session.connection_kind.or(connection_kind).unwrap_or(crate::api::model::ConnectionKind::Normal),
                Some(session.token.as_str()),
            )
            .await
        {
            Some(handle)
        } else {
            debug_if_enabled!(
                "HLS pinned provider {} unavailable for {}; aborting allocation to prevent mid-session migration",
                sanitize_sensitive_info(pinned_provider),
                sanitize_sensitive_info(&fingerprint.addr.to_string())
            );
            None
        };

        if provider_handle.is_none() {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        }
        match provider_handle.as_ref().map(|handle| &handle.allocation) {
            Some(ProviderAllocation::Exhausted) => (url, None, provider_handle, None),
            Some(ProviderAllocation::Available(cfg) | ProviderAllocation::GracePeriod(cfg)) => {
                let selected_provider_config = Arc::clone(cfg);
                let Some((_provider_name, stream_url)) =
                    select_provider_stream_url(&url, input, cfg, false, &app_state.app_config).await
                else {
                    app_state.connection_manager.release_provider_handle(provider_handle).await;
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                let session_token = app_state
                    .active_users
                    .create_user_session(crate::api::model::CreateUserSessionParams {
                        user,
                        session_token: &session.token,
                        virtual_id,
                        provider: &cfg.name,
                        stream_url: &stream_url,
                        addr: &fingerprint.addr,
                        connection_permission,
                        connection_kind: session.connection_kind.or(connection_kind),
                        socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
                    })
                    .await;
                let hls_session_ttl_secs = get_hls_session_ttl_secs(app_state);
                app_state
                    .active_provider
                    .refresh_provider_reservation(&cfg.name, &session_token, hls_session_ttl_secs)
                    .await;
                (stream_url, Some(session_token), provider_handle, Some(selected_provider_config))
            }
            None => (url, None, None, None),
        }
    } else {
        // Append/shift catchup must keep an m3u-catchup session token even when shared HLS
        // cache is off; otherwise rewritten segments register as LiveHls in the panel.
        let user_session_token = hls_entry_user_session_token(
            fingerprint,
            &user.username,
            virtual_id,
            session_token_hint,
            archive_reference,
        );
        let hls_session_owner = if hls_cache_enabled_for_target(app_state, target) {
            let session_key = HlsSessionKey::new(input.id, stream_context.stream_ref());
            let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
            Some(build_hls_origin_session_owner(&proxy_session_id))
        } else {
            None
        };
        let session_owner = hls_session_owner.as_deref().unwrap_or(user_session_token.as_str());
        let hls_session_ttl_secs = get_hls_session_ttl_secs(app_state);
        let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
            app_state,
            fingerprint,
            user,
            input,
            virtual_id,
            &url,
            &user_session_token,
            session_owner,
            hls_session_ttl_secs,
            connection_permission,
            fallback_connection_kind,
            true,
        )
        .await
        else {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };
        debug_if_enabled!(
            "API endpoint [HLS] create_session_fingerprint user={} virtual_id={virtual_id} provider={} stream_url={}",
            sanitize_sensitive_info(&user.username),
            reservation.selected_provider_config.as_ref().map_or("<unknown>", |provider| provider.name.as_ref()),
            sanitize_sensitive_info(&reservation.request_url)
        );
        (
            reservation.request_url,
            Some(reservation.session_token),
            reservation.provider_handle,
            reservation.selected_provider_config,
        )
    };

    let user_agent_stream_index = crate::api::api_utils::resolve_stream_user_agent_index(
        app_state,
        input,
        provider_handle.is_some(),
        &user.username,
        session_token.as_deref(),
    )
    .await;
    if let Some(stream_index) = user_agent_stream_index {
        request::append_user_agent_stream_index(&mut headers, stream_index);
        if let Some(session_token) = session_token.as_deref() {
            app_state
                .active_users
                .set_user_agent_stream_index_if_absent(&user.username, session_token, stream_index)
                .await;
        }
    }

    // Playlist requests only need the chosen provider account to derive the URL and pin the session.
    // Holding the provider slot until the first segment request causes stale active connections and
    // breaks forced same-account reuse on the next HLS/Catchup stream request.
    app_state.connection_manager.release_provider_handle(provider_handle).await;

    let input_source = InputSource::from(input).with_url(request_url);
    let download_result = download_legacy_hls_manifest(app_state, &input_source, &headers).await;
    match download_result {
        Ok((content, response_url, response_headers)) => {
            let encrypt_secret = app_state.get_encrypt_secret();
            let base_url = server_info.get_base_url();
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
                content: &content,
                hls_url: response_url,
                target_id: target.id,
                virtual_id,
                input_id: input.id,
                user_token: session_token.as_deref(),
            };
            let hls_content = rewrite_hls(user, &rewrite_hls_props);
            if let Some(session_token) = session_token.as_deref() {
                let session_headers = extract_hls_provider_session_headers(&response_headers);
                if !session_headers.is_empty() {
                    app_state
                        .active_users
                        .update_session_provider_headers(&user.username, session_token, &session_headers)
                        .await;
                }
                release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                    .await;
            }
            hls_response(hls_content).into_response()
        }
        Err(err) => {
            error!("Failed to download m3u8: {}", request::text_response_error_log_label(&err));
            if let Some(session_token) = session_token.as_deref() {
                terminate_failed_hls_manifest_session(app_state, &user.username, session_token).await;
            }

            hls_custom_video_manifest_response(
                app_state,
                user,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
            .await
        }
    }
}

pub(super) async fn get_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Option<StreamChannel> {
    if target.has_output(TargetType::Xtream) {
        if let Ok(pli) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            return Some(pli.to_stream_channel(target.id));
        }
    }
    let target_id = target.id;
    m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
        .await
        .ok()
        .map(|pli| pli.to_stream_channel(target_id))
}

pub(super) fn hls_stream_context_or_unavailable(
    item: &impl PlaylistEntry,
    virtual_id: u32,
) -> Result<HlsEntryStreamContext, StatusCode> {
    HlsEntryStreamContext::from_playlist_item(item).ok_or_else(|| {
        warn!("HLS input stream identity missing for virtual_id={virtual_id}; refresh target playlist");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

pub(in crate::api) async fn resolve_hls_virtual_source_for_target(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Result<HlsResolvedVirtualSource, StatusCode> {
    let (input_name, stream_context) = if target.has_output(TargetType::Xtream) {
        if let Ok(item) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
            (Arc::clone(&item.input_name), stream_context)
        } else {
            let item = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
                .await
                .map_err(|_| StatusCode::NOT_FOUND)?;
            let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
            (Arc::clone(&item.input_name), stream_context)
        }
    } else {
        let item = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let stream_context = hls_stream_context_or_unavailable(&item, virtual_id)?;
        (Arc::clone(&item.input_name), stream_context)
    };
    let input = app_state.app_config.get_input_by_name(&input_name).ok_or(StatusCode::NOT_FOUND)?;
    Ok(HlsResolvedVirtualSource { input, stream_context })
}

pub(super) async fn resolve_hls_origin_playlist_url(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    virtual_id: u32,
    fallback_url: &str,
) -> Result<String, StatusCode> {
    if input.input_type.is_xtream() && target.has_output(TargetType::Xtream) {
        let pli = xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let hls_extension = format!(".{HLS_EXT}");
        let (query_path, _) = get_query_path("", Some(&hls_extension), &pli, app_state);
        return get_xtream_player_api_stream_url(input, ApiStreamContext::Live, &query_path, &pli.url)
            .map(|url| url.to_string())
            .ok_or(StatusCode::SERVICE_UNAVAILABLE);
    }

    Ok(fallback_url.to_string())
}

pub(super) async fn resolve_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &Arc<ConfigInput>,
    virtual_id: u32,
    hls_url: &str,
    archive_reference: Option<i64>,
    session_token: Option<&str>,
) -> StreamChannel {
    let unknown = "Unknown".intern();
    let mut channel = match get_stream_channel(app_state, target, virtual_id).await {
        Some(mut channel) => {
            channel.url = Arc::from(hls_url);
            channel
        }
        None => StreamChannel {
            target_id: target.id,
            virtual_id,
            provider_id: 0,
            input_name: Arc::clone(&input.name),
            item_type: PlaylistItemType::LiveHls,
            cluster: XtreamCluster::Live,
            group: unknown.clone(),
            title: unknown,
            url: Arc::from(hls_url),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        },
    };

    let archive_reference = archive_reference.or_else(|| epg_reference_ts_from_date_tree_path(hls_url));
    // Append/shift catchup often loses utc/utcstart on rewritten segment URLs; the session
    // token still identifies archive playback for Streams/History (Catchup, not Live/HLS).
    let is_archive_playback = archive_reference.is_some()
        || looks_like_archive_media_path(hls_url)
        || session_token.is_some_and(is_m3u_catchup_session_token);
    if is_archive_playback {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.epg_reference_ts = None;
    }
    channel
}

pub(super) fn hls_entry_user_session_token(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    session_token_hint: Option<&str>,
    archive_reference: Option<i64>,
) -> String {
    if let Some(hint) = session_token_hint.filter(|token| is_m3u_catchup_session_token(token)) {
        return hint.to_string();
    }
    if let Some(timestamp) = archive_reference {
        return create_m3u_catchup_session_key(fingerprint, username, virtual_id, &format!("archive|{timestamp}|0"));
    }
    let base = create_playback_session_fingerprint(fingerprint, username, virtual_id, PlaylistItemType::LiveHls, None);
    format!("{base}|hls|{}", generate_random_string(16))
}

#[allow(clippy::too_many_lines)]
pub(super) async fn hls_api_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    axum::extract::Path(params): axum::extract::Path<HlsApiPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let api_proxy_user = create_api_proxy_user(&app_state);
    let (user, target) = if params.username == api_proxy_user.username
        && crate::auth::constant_time_eq(params.password.as_bytes(), api_proxy_user.password.as_bytes())
    {
        let Some(target) = app_state.app_config.get_target_by_id(params.target_id) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        (Arc::new(api_proxy_user), target)
    } else {
        let Some((user, target)) = app_state.app_config.get_target_for_user(&params.username, &params.password) else {
            // Credential failure is an auth error, not a malformed request
            return app_state.app_config.get_auth_error_status().into_response();
        };
        if target.id != params.target_id {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        (user, target)
    };

    // Nested path = relative origin segment that leaked past rewrite_hls (e.g. dvr-YYYY/...).
    if params.token.contains('/') {
        let Some((token, relative_path)) = params.token.split_once('/') else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let encrypt_secret = app_state.get_encrypt_secret();
        let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, token) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let lookup_session_token = decoded_hls_token
            .0
            .clone()
            .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, params.stream_id, false));
        let Some(input) = app_state.app_config.get_input_by_id(params.input_id) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let Some(session) = app_state
            .active_users
            .find_latest_session_for_target_stream(
                &user.username,
                target.id,
                input.name.as_ref(),
                params.stream_id,
                lookup_session_token.as_str(),
            )
            .await
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !legacy_hls_route_allowed_with_cache(
            hls_cache_enabled_for_target(&app_state, &target),
            decoded_hls_token.0.as_deref(),
            Some(session.token.as_str()),
        ) {
            return hls_custom_video_manifest_response_for_username(
                &app_state,
                &user.username,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
            .await;
        }
        return hls_api_stream_leaked_relative(
            fingerprint,
            req_headers,
            app_state,
            user,
            target,
            input,
            params.stream_id,
            session,
            decoded_hls_token.1,
            relative_path.to_string(),
            raw_query.as_deref(),
        )
        .await;
    }

    hls_api_stream_resolved(
        fingerprint,
        req_headers,
        app_state,
        user,
        target,
        params.input_id,
        params.stream_id,
        params.token,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn hls_api_stream_resolved(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    app_state: Arc<AppState>,
    user: Arc<ProxyUserCredentials>,
    target: Arc<ConfigTarget>,
    input_id: u16,
    stream_id: u32,
    token: String,
) -> axum::response::Response {
    // Network access check only - permission check is done later with full stream info
    if let Err(e) = check_network_access_only(&user, &fingerprint, &app_state.app_config, &app_state.geoip) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }
    let target_name = &target.name;
    let virtual_id = stream_id;
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_id(input_id),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {virtual_id}, hls", input_id)
    );

    if user.permission_denied(&app_state.app_config) {
        let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, "", None, None).await;
        return hls_admission_failure_manifest_response(
            &app_state,
            &fingerprint,
            &user,
            stream_channel,
            input.name.clone(),
            &req_headers,
            ConnectFailureReason::UserAccountExpired,
        )
        .await;
    }

    debug_if_enabled!("ID chain for hls endpoint: request_stream_id={stream_id} -> virtual_id={virtual_id}");
    let encrypt_secret = app_state.get_encrypt_secret();
    let Some(decoded_hls_token) = get_hls_session_token_and_url_from_token(&encrypt_secret, &token) else {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    };
    let lookup_session_token = decoded_hls_token
        .0
        .clone()
        .unwrap_or_else(|| create_session_fingerprint(&fingerprint, &user.username, virtual_id, false));
    let mut user_session =
        app_state.active_users.get_and_update_user_session(&user.username, &lookup_session_token).await;
    if !legacy_hls_route_allowed_with_cache(
        hls_cache_enabled_for_target(&app_state, &target),
        decoded_hls_token.0.as_deref(),
        user_session.as_ref().map(|session| session.token.as_str()),
    ) {
        return hls_manifest_channel_unavailable_response_for_username(&app_state, &user.username).await;
    }

    if let Some(session) = &mut user_session {
        let decoded_archive_reference =
            resolve_m3u_archive_reference(&decoded_hls_token.1, Some(lookup_session_token.as_str()));
        if session.permission == UserConnectionPermission::Exhausted {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.1,
                decoded_archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await;
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.1,
                decoded_archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                session.provider.clone(),
                &req_headers,
                ConnectFailureReason::ProviderConnectionsExhausted,
            )
            .await;
        }

        let hls_url = match decoded_hls_token {
            (Some(session_token), hls_url) if session.token.eq(&session_token) => hls_url,
            (None, hls_url) => hls_url,
            _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };
        let hls_url = hls_url.intern();
        // Recover utc/utcstart from the prior playlist URL before overwriting with a segment URL
        // that usually drops append/shift query params.
        let archive_reference = resolve_m3u_archive_reference(&hls_url, Some(session.token.as_str()))
            .or_else(|| m3u_archive_epg_reference_ts(session.stream_url.as_ref()));
        session.stream_url = hls_url.clone();
        if session.virtual_id == virtual_id {
            app_state.connection_manager.touch_http_activity(&user.username, &session.token, &fingerprint.addr).await;
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &hls_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            if is_seekable_media_request(stream_channel.cluster, &req_headers, extract_extension_from_url(&hls_url)) {
                // partial request means we are in reverse proxy mode, seek happened
                return force_provider_stream_response(
                    &fingerprint,
                    &app_state,
                    session,
                    stream_channel,
                    crate::api::api_utils::ForceStreamRequestContext {
                        req_headers: &req_headers,
                        input: &input,
                        user: &user,
                        session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
                        content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
                    },
                    None,
                )
                .await
                .into_response();
            }
        } else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }

        let (connection_admission, grace_mode, request_class) =
            crate::api::api_utils::resolve_playback_request_admission(
                &app_state.admission_ctx(),
                &user,
                &fingerprint,
                Some(session),
                &session.token,
                true,
                crate::api::api_utils::EvictionReentryGuard::Session(&session.token),
                // HLS playlist requests (.m3u8) are explicit Prepare: they set up session metadata
                // but do not consume an admission slot. Segment and other media requests use Activate.
                is_hls_url(&hls_url),
                false,
            )
            .await;
        let connection_permission = connection_admission.permission;
        let connection_kind = connection_admission.kind.or(session.connection_kind);
        session.permission = connection_permission;
        if let Some(connection_kind) = connection_kind {
            session.connection_kind = Some(connection_kind);
        }
        if connection_permission == UserConnectionPermission::Exhausted
            || (connection_permission == UserConnectionPermission::GracePeriod && connection_kind.is_none())
        {
            let provider = if session.provider.is_empty() { input.name.clone() } else { session.provider.clone() };
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &session.stream_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                provider,
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            )
            .await;
        }
        let fallback_connection_kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);

        if is_hls_url(&session.stream_url) {
            let source = match resolve_hls_virtual_source_for_target(&app_state, &target, virtual_id).await {
                Ok(source) if source.input.id == input.id => source,
                Ok(source) => {
                    warn!(
                        "HLS input context mismatch for virtual_id={virtual_id}: expected_input_id={}, resolved_input_id={}",
                        input.id, source.input.id
                    );
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Err(status) => return status.into_response(),
            };
            let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, virtual_id);
            return handle_hls_stream_request(
                &fingerprint,
                &app_state,
                &user,
                &target,
                Some(session),
                None,
                &session.stream_url,
                archive_reference,
                source.stream_context,
                &input,
                &req_headers,
                connection_permission,
                connection_kind,
                &original_hls_entry_path,
            )
            .await
            .into_response();
        }

        if is_file_url(&session.stream_url) {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &hls_url,
                archive_reference,
                Some(session.token.as_str()),
            )
            .await;
            return local_stream_response(
                &fingerprint,
                &app_state,
                stream_channel,
                &req_headers,
                &input,
                &target,
                &user,
                connection_permission,
                fallback_connection_kind,
                Some(&session.token),
                Some(request_class),
                false,
            )
            .await
            .into_response();
        }

        let stream_channel = resolve_stream_channel(
            &app_state,
            &target,
            &input,
            virtual_id,
            &hls_url,
            archive_reference,
            Some(session.token.as_str()),
        )
        .await;
        force_provider_stream_response(
            &fingerprint,
            &app_state,
            session,
            stream_channel,
            crate::api::api_utils::ForceStreamRequestContext {
                req_headers: &req_headers,
                input: &input,
                user: &user,
                session_reservation_ttl_secs: get_hls_session_ttl_secs(&app_state),
                content_representation: crate::api::model::ProviderContentRepresentationMode::Identity,
            },
            grace_mode,
        )
        .await
        .into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}
