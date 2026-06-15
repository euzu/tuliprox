#![allow(clippy::large_futures)]

use super::xtream_api::{get_query_path, get_xtream_player_api_stream_url, ApiStreamContext};
use crate::{
    api::{
        api_utils::{
            connection_priority_for_kind, create_api_proxy_user, create_playback_session_fingerprint,
            create_session_fingerprint, force_provider_stream_response, get_headers_from_request,
            get_hls_session_ttl_secs, get_stream_alternative_url, is_seek_request, local_stream_response,
            mark_response_as_uncompressed, record_connect_failed_attempt, resolve_playback_request_admission,
            try_option_bad_request, try_unwrap_body, ConnectFailedAttempt, EvictionReentryGuard, HeaderFilter,
        },
        model::{
            begin_hls_origin_account_io, build_hls_origin_session_owner, build_proxy_session_id,
            classify_hls_resource_status, cold_start_retry_after_seconds, create_channel_unavailable_stream,
            finish_hls_origin_account_io, hls_custom_video_manifest_response_with_virtual_id, hls_object_body_deadline,
            hls_origin_account_status, hls_provisioning_discontinuity_sequence,
            hls_shared_panel_provisioning_manifest_path, hls_shared_panel_provisioning_manifest_response,
            hls_virtual_entry_redirect_response, log_hls_resource_attempt_started,
            log_hls_resource_attempt_succeeded, log_hls_resource_fetch_failed, log_hls_resource_retry_scheduled,
            log_hls_resource_timeout, maybe_trigger_origin_refresh, new_hls_access_lease_id,
            origin_account_binding_from_allocation, retry_after_secs_from_ms, safe_hls_access_lease_id,
            safe_proxy_session_id, safe_user_session_token, scrub_hls_origin_headers, serve_hls_map_cache_response,
            serve_hls_segment_cache_response, serve_hls_transient_object_cache_response, should_remove_hls_origin_header,
            start_hls_panel_provisioning_once,
            try_hls_panel_provisioning_manifest_response, validate_hls_access_lease, AccessLeaseReuseBlock,
            AccessLeaseReuseResult, AppState, CacheAccessState, CustomVideoStreamType, HlsAccessAdmissionMode,
            HlsAccessContext, HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId, HlsAccessLeaseState,
            HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsAccessLeaseValidationError, HlsAccountBindingProtection,
            HlsCacheResponseContext, HlsEffectiveOriginAcquirePolicy, HlsMapFile, HlsOriginAccountBinding,
            HlsOriginAccountBindingMode, HlsOriginAccountDetachedReason, HlsOriginAccountIoLeaseGuard,
            HlsOriginAccountStatus, HlsOriginIoContext, HlsOriginSource, HlsOriginSourceKind, HlsOriginWorkClass,
            HlsPanelProvisioningRedirectPaths, HlsPlaybackFamilyKey, HlsProvisioningStatus, HlsRepairRenderedObjectId,
            HlsResourceFetchAttempt, HlsResourceFetchKind, HlsResourceFetchLogContext, HlsResourceFetchLogStatus,
            HlsResourceStatusClass, HlsSegmentFile, HlsSegmentRepairObjectContext, HlsSegmentRepairSource,
            HlsSessionHandle, HlsSessionKey, HlsSessionMode, HlsSessionStoreOutcome, LiveHlsOriginEntry,
            OriginRefreshRequest, ProviderAllocation,
            ProviderConfig as RuntimeProviderConfig, ProviderHandle, ProxySessionId, RetryPolicy, SegmentCacheStatus,
            SegmentDemandFetchOutcome, SegmentFetchContext, SegmentFetchPolicy, TransientObjectCacheKey,
            TransientObjectFetchDecision, TransientObjectUnavailableState, TransientResourceFile,
            TransientResourceKind, UserSession,
            HLS_ACCESS_LEASE_ID_PLACEHOLDER,
        },
        panel_api::can_provision_on_exhausted,
    },
    auth::{check_network_access_only, Fingerprint},
    model::{
        ConfigInput, ConfigInputFlags, ConfigProvider, ConfigTarget, InputSource, ProxyUserCredentials,
        ReverseProxyDisabledHeaderConfig,
    },
    processing::parser::hls::{
        get_hls_session_token_and_url_from_token, rewrite_hls,
        transient_manifest::{materialize_initial_transient_strip_view, TransientInitialStripOutcome},
        RewriteHlsProps,
    },
    repository::{m3u_get_item_for_stream_id, storage_const, xtream_get_item_for_stream_id},
    utils::{debug_if_enabled, request, request::is_file_url},
};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use futures::{StreamExt, TryStreamExt};
use log::{debug, error, warn};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shared::{
    model::{
        ConnectFailureReason, FailureStage, InputType, PlaylistItemType, StreamChannel, StreamInfo, TargetType,
        UserConnectionPermission, XtreamCluster,
    },
    utils::{generate_random_string, is_hls_url, replace_url_extension, sanitize_sensitive_info, Internable, HLS_EXT},
};
use std::{borrow::Cow, collections::HashMap, io, sync::Arc, time::{Duration, Instant}};
use tokio_util::io::StreamReader;
use url::Url;

const MAX_MANUAL_REDIRECTS: usize = 10;
const HLS_TEMPORARY_RESOURCE_RETRY_AFTER_SECS: u64 = 1;
const HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS: u64 = HLS_TEMPORARY_RESOURCE_RETRY_AFTER_SECS * 1_000;

fn is_m3u_catchup_session_token(session_token: &str) -> bool {
    session_token.starts_with("m3u-catchup|") || session_token.starts_with("catchup|")
}

fn query_flag_is_archive(key: &str) -> bool { key.eq_ignore_ascii_case("utc") }

fn query_flag_marks_start_context(key: &str) -> bool {
    key.eq_ignore_ascii_case("end") || key.eq_ignore_ascii_case("duration") || key.eq_ignore_ascii_case("lutc")
}

pub(in crate::api) fn m3u_archive_epg_reference_ts(stream_url: &str) -> Option<i64> {
    let parsed = Url::parse(stream_url).ok()?;
    let path = parsed.path();
    if let Some(rest) = path.split("/archive-").nth(1) {
        let start = rest.split('-').next()?;
        if let Ok(ts) = start.parse::<i64>() {
            return Some(ts);
        }
    }
    if let Some(rest) = path.split("/timeshift_abs-").nth(1) {
        let start = rest.trim_end_matches(".ts").trim_end_matches(".m3u8");
        if let Ok(ts) = start.parse::<i64>() {
            return Some(ts);
        }
    }
    let mut start_ts = None;
    let mut has_start_context = false;
    for (key, value) in parsed.query_pairs() {
        if query_flag_is_archive(&key) {
            if let Ok(ts) = value.parse::<i64>() {
                return Some(ts);
            }
        } else if key.eq_ignore_ascii_case("start") {
            start_ts = value.parse::<i64>().ok();
        } else if query_flag_marks_start_context(&key) {
            has_start_context = true;
        }
    }

    has_start_context.then_some(start_ts).flatten()
}

#[derive(Debug, Deserialize)]
struct HlsApiPathParams {
    username: String,
    password: String,
    target_id: u16,
    input_id: u16,
    stream_id: u32,
    token: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxySegmentPathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
    segment_file: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxyManifestPathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxyMapPathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
    map_file: String,
}

#[derive(Debug, Deserialize)]
struct HlsProxyResourcePathParams {
    proxy_session_id: String,
    hls_access_lease_id: String,
    resource_file: String,
}

fn hls_response(hls_content: String) -> impl IntoResponse + Send {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(axum::http::header::CACHE_CONTROL, "no-store, no-cache, must-revalidate",)
        .body(hls_content))
}

fn extract_hls_provider_session_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let cookies = headers
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next().map(str::trim))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    let mut session_headers = HashMap::new();
    if !cookies.is_empty() {
        session_headers.insert(String::from("cookie"), cookies.join("; "));
    }
    session_headers
}

fn hls_custom_video_type_for_failure_reason(reason: ConnectFailureReason) -> CustomVideoStreamType {
    match reason {
        ConnectFailureReason::UserAccountExpired => CustomVideoStreamType::UserAccountExpired,
        ConnectFailureReason::UserConnectionsExhausted => CustomVideoStreamType::UserConnectionsExhausted,
        ConnectFailureReason::ProviderConnectionsExhausted => CustomVideoStreamType::ProviderConnectionsExhausted,
        ConnectFailureReason::Preempted => CustomVideoStreamType::LowPriorityPreempted,
        ConnectFailureReason::Provisioning => CustomVideoStreamType::Provisioning,
        ConnectFailureReason::ProviderError
        | ConnectFailureReason::ProviderClosed
        | ConnectFailureReason::ChannelUnavailable
        | ConnectFailureReason::SessionExpired => CustomVideoStreamType::ChannelUnavailable,
    }
}

pub(crate) fn hls_custom_video_manifest_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
) -> axum::response::Response {
    hls_custom_video_manifest_response_with_virtual_id(app_state, user, video_type, fallback_status, None)
}

pub(crate) fn hls_admission_failure_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    stream_channel: StreamChannel,
    provider_name: Arc<str>,
    req_headers: &HeaderMap,
    reason: ConnectFailureReason,
) -> axum::response::Response {
    record_connect_failed_attempt(ConnectFailedAttempt {
        app_state,
        fingerprint,
        user,
        stream_channel,
        provider_name,
        req_headers,
        reason,
        failure_stage: FailureStage::Admission,
    });
    hls_custom_video_manifest_response(
        app_state,
        user,
        hls_custom_video_type_for_failure_reason(reason),
        StatusCode::FORBIDDEN,
    )
}

fn apply_hls_proxy_public_path_prefix(hls_content: String, server_path: Option<&str>) -> String {
    let Some(path_prefix) = normalize_hls_proxy_public_path_prefix(server_path) else {
        return hls_content;
    };

    let uri_attr_prefix = format!("URI=\"{path_prefix}/proxy/hls/live/");
    let hls_content = hls_content.replace("URI=\"/proxy/hls/live/", &uri_attr_prefix);
    if hls_content.is_empty() {
        return hls_content;
    }
    let mut prefixed = String::with_capacity(hls_content.len().saturating_add(path_prefix.len().saturating_mul(4)));

    for part in hls_content.split_inclusive('\n') {
        let (line, line_ending) = split_hls_line_ending(part);
        if line.starts_with("/proxy/hls/live/") {
            prefixed.push_str(&path_prefix);
        }
        prefixed.push_str(line);
        prefixed.push_str(line_ending);
    }

    prefixed
}

fn normalize_hls_proxy_public_path_prefix(server_path: Option<&str>) -> Option<String> {
    let path = server_path?.trim().trim_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(format!("/{path}"))
}

fn split_hls_line_ending(part: &str) -> (&str, &str) {
    if let Some(line) = part.strip_suffix("\r\n") {
        (line, "\r\n")
    } else if let Some(line) = part.strip_suffix('\n') {
        (line, "\n")
    } else {
        (part, "")
    }
}

fn materialize_hls_access_manifest(
    hls_content: &str,
    lease_id: &HlsAccessLeaseId,
    server_path: Option<&str>,
) -> String {
    let hls_content = hls_content.replace(HLS_ACCESS_LEASE_ID_PLACEHOLDER, &lease_id.0);
    apply_hls_proxy_public_path_prefix(hls_content, server_path)
}

fn materialize_transient_hls_access_manifest(
    hls_content: &str,
    proxy_session_id: &ProxySessionId,
    lease_id: &HlsAccessLeaseId,
    lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
) -> String {
    let response_body = if lease_state == HlsAccessLeaseState::Pending {
        let view = materialize_initial_transient_strip_view(hls_content, strip);
        match view.outcome {
            TransientInitialStripOutcome::Applied { mode, configured, effective, visible_segments } => {
                debug!(
                    "HLS transient initial strip applied: lease={} session={} mode={} configured={} effective={} visible_segments={}",
                    safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(proxy_session_id),
                    mode,
                    configured,
                    effective,
                    visible_segments
                );
            }
            TransientInitialStripOutcome::Skipped { reason, visible_segments } => {
                debug!(
                    "HLS transient initial strip skipped: lease={} session={} reason={} visible_segments={}",
                    safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(proxy_session_id),
                    reason.as_log_reason(),
                    visible_segments
                );
            }
        }
        view.body
    } else {
        let reason =
            if lease_state == HlsAccessLeaseState::Activated { "lease-activated" } else { "lease-not-pending" };
        debug!(
            "HLS transient initial strip skipped: lease={} session={} reason={}",
            safe_hls_access_lease_id(lease_id),
            safe_proxy_session_id(proxy_session_id),
            reason
        );
        hls_content.to_string()
    };
    materialize_hls_access_manifest(&response_body, lease_id, server_path)
}

fn hls_access_lease_ttl_ms(app_state: &Arc<AppState>) -> u64 { app_state.hls_proxy.session_idle_timeout_ms() }

async fn hls_access_lease_timing_for_session(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
) -> HlsAccessLeaseTiming {
    let timing = session.read().await.account_overlap_timing();
    let active_window_ms = timing.hard_active_window_ms.saturating_mul(2);
    HlsAccessLeaseTiming { active_window_ms, valid_window_ms: hls_access_lease_ttl_ms(app_state) }
}

struct HlsResourceAccess {
    session: HlsSessionHandle,
    access_context: HlsAccessContext,
}

async fn prepare_hls_resource_access(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    request_kind: &'static str,
) -> Result<HlsResourceAccess, axum::response::Response> {
    let Some(session) = app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await else {
        return Err(StatusCode::NOT_FOUND.into_response());
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
    let access_context = validate_hls_proxy_access_request(
        app_state,
        fingerprint,
        proxy_session_id,
        hls_access_lease_id,
        now_ms,
        hls_access_lease_timing_for_session(app_state, &session).await,
        request_kind,
    )
    .await
    .map_err(hls_access_lease_validation_response)?;
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
    Ok(HlsResourceAccess { session, access_context })
}

fn create_hls_cache_user_session_token(fingerprint: &Fingerprint, username: &str, virtual_id: u32) -> String {
    let base = create_playback_session_fingerprint(fingerprint, username, virtual_id, PlaylistItemType::LiveHls, None);
    format!("{base}|hls-cache|{}", generate_random_string(16))
}

fn hls_access_lease_reuse_skip_log_reason(
    reason: AccessLeaseReuseBlock,
    state: Option<HlsAccessLeaseState>,
) -> &'static str {
    match (reason, state) {
        (AccessLeaseReuseBlock::StateNotPending, Some(HlsAccessLeaseState::Activated)) => "activated",
        (AccessLeaseReuseBlock::StateNotPending, Some(HlsAccessLeaseState::Idle)) => "idle",
        (AccessLeaseReuseBlock::ReuseWindowExpired, _) => "reuse-window-expired",
        _ => reason.as_log_reason(),
    }
}

fn is_hls_media_activity_status(status: StatusCode) -> bool {
    matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
}

fn hls_cache_response_context(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    now_ms: u64,
) -> HlsCacheResponseContext {
    HlsCacheResponseContext::new(
        access_context.lease_id.clone(),
        app_state.hls_proxy.cache_duration_seconds(),
        Arc::clone(app_state.hls_proxy.metrics()),
        Arc::clone(app_state.hls_proxy.segment_repair()),
        now_ms,
    )
}

async fn hls_proxy_segment(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxySegmentPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(segment_file) = HlsSegmentFile::parse(&params.segment_file) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let now_ms = current_time_millis();
    let HlsResourceAccess { session, access_context } = match prepare_hls_resource_access(
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
        Err(response) => return response,
    };
    {
        let session_guard = session.read().await;
        if session_guard.is_gc_marked_for_removal() {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
        let Some(entry) = session_guard.segments.get(&segment_file.proxy_seq) else {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        };
        if entry.proxy_file_ext != segment_file.extension {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    }

    let preacquired_provider_handle = if hls_segment_request_requires_origin_work(&session, &segment_file).await {
        match prepare_hls_origin_binding_for_authorized_resource_work(
            &app_state,
            &session,
            &access_context,
            &fingerprint,
            &headers,
            HlsOriginWorkKind::Segment,
            now_ms,
        )
        .await
        {
            Ok(handle) => handle,
            Err(status) => return hls_canonical_status_response(status),
        }
    } else {
        None
    };

    match demand_fetch_hls_segment_if_needed(
        &app_state,
        &session,
        &segment_file,
        &access_context,
        &fingerprint,
        preacquired_provider_handle,
        now_ms,
    )
    .await
    {
        SegmentDemandFetchOutcome::NotFound => return axum::http::StatusCode::NOT_FOUND.into_response(),
        SegmentDemandFetchOutcome::Ready
        | SegmentDemandFetchOutcome::QueuedOrFetching
        | SegmentDemandFetchOutcome::Unavailable
        | SegmentDemandFetchOutcome::TimedOut => {}
    }

    let response = serve_hls_segment_cache_response(
        Arc::clone(app_state.hls_proxy.segment_cache()),
        Arc::clone(&session),
        segment_file,
        headers.get(header::RANGE).cloned(),
        &hls_cache_response_context(&app_state, &access_context, now_ms),
    )
    .await;
    if is_hls_media_activity_status(response.status()) {
        mark_hls_authorized_media_access(&app_state, &session, now_ms).await;
        let _ = ensure_hls_cache_stream_registered(&app_state, &fingerprint, &headers, &access_context, &session).await;
    }
    response
}

async fn demand_fetch_hls_segment_if_needed(
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

async fn build_hls_segment_fetch_context(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    repair_access_lease_id: Option<HlsAccessLeaseId>,
    fingerprint: &Fingerprint,
    preacquired_provider_handle: Option<ProviderHandle>,
) -> SegmentFetchContext {
    let (headers, origin_policy) = {
        let session = session.read().await;
        (session.origin_request_headers.clone(), session.effective_origin_acquire_policy_or_default())
    };
    let mut origin_io = HlsOriginIoContext {
        app_state: Arc::clone(app_state),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs: get_hls_session_ttl_secs(app_state),
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
        client: app_state.http_client.load().as_ref().clone(),
        no_redirect_client: app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        origin_io: Some(origin_io),
    }
}

async fn hls_effective_origin_acquire_policy(session: &HlsSessionHandle) -> HlsEffectiveOriginAcquirePolicy {
    session.read().await.effective_origin_acquire_policy_or_default()
}

async fn hls_proxy_map(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyMapPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(map_file) = HlsMapFile::parse(&params.map_file) else {
        return axum::http::StatusCode::NOT_FOUND.into_response();
    };
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let now_ms = current_time_millis();
    let HlsResourceAccess { session, access_context } = match prepare_hls_resource_access(
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
        Err(response) => return response,
    };
    {
        let session_guard = session.read().await;
        if session_guard.is_gc_marked_for_removal() {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
        let Some(entry) = session_guard.maps.get(&map_file.proxy_map_id.into()) else {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        };
        if entry.proxy_file_ext != map_file.extension {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
    }

    let response = serve_hls_map_cache_response(
        Arc::clone(app_state.hls_proxy.segment_cache()),
        Arc::clone(&session),
        map_file,
        headers.get(header::RANGE).cloned(),
        &hls_cache_response_context(&app_state, &access_context, now_ms),
    )
    .await;
    if is_hls_media_activity_status(response.status()) {
        mark_hls_authorized_media_access(&app_state, &session, now_ms).await;
        let _ = ensure_hls_cache_stream_registered(&app_state, &fingerprint, &headers, &access_context, &session).await;
    }
    response
}

async fn hls_proxy_resource(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyResourcePathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let Some(resource_file) = TransientResourceFile::parse(&params.resource_file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let now_ms = current_time_millis();
    let HlsResourceAccess { session, access_context } = match prepare_hls_resource_access(
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
        Err(response) => return response,
    };
    let range_header = headers.get(header::RANGE).cloned();
    let cache_duration_ms = app_state.hls_proxy.cache_duration_seconds().saturating_mul(1_000);
    let Ok((resource, origin_headers, cache_action)) = resolve_transient_object_cache_action(
        &session,
        &proxy_session_id,
        &resource_file,
        range_header.as_ref(),
        now_ms,
        cache_duration_ms,
    )
    .await
    else {
        return hls_channel_unavailable_or_not_found_response(&app_state);
    };

    match cache_action {
        TransientObjectCacheAction::ServeReady => {
            return serve_transient_object_cache_response_and_mark_or_unavailable(TransientObjectCacheServeContext {
                app_state: &app_state,
                session: &session,
                fingerprint: &fingerprint,
                headers: &headers,
                access_context: &access_context,
                resource_file,
                range_header,
                now_ms,
            })
            .await;
        }
        TransientObjectCacheAction::WaitForFetch(notifier) => {
            return wait_for_transient_object_cache_fetch(TransientObjectWaitContext {
                app_state: &app_state,
                session: &session,
                fingerprint: &fingerprint,
                headers: &headers,
                access_context: &access_context,
                resource_file,
                range_header,
                notifier,
            })
            .await;
        }
        TransientObjectCacheAction::FetchAndCache(_) | TransientObjectCacheAction::PassthroughNoCache => {}
    }

    fetch_or_passthrough_transient_resource(
        &app_state,
        &session,
        &fingerprint,
        &headers,
        &access_context,
        &resource,
        resource_file,
        cache_action,
        origin_headers,
        range_header,
        cache_duration_ms,
        now_ms,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn fetch_or_passthrough_transient_resource(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    access_context: &HlsAccessContext,
    resource: &crate::api::model::TransientResourceRef,
    resource_file: TransientResourceFile,
    cache_action: TransientObjectCacheAction,
    origin_headers: HeaderMap,
    range_header: Option<HeaderValue>,
    cache_duration_ms: u64,
    now_ms: u64,
) -> axum::response::Response {
    let transient_origin_guard = match prepare_hls_transient_origin_io_for_authorized_resource_work(
        app_state,
        session,
        access_context,
        fingerprint,
        headers,
        now_ms,
    )
    .await
    {
        Ok(handle) => handle,
        Err(status) => return hls_canonical_status_response(status),
    };

    if let TransientObjectCacheAction::FetchAndCache(cache_key) = cache_action {
        return fetch_and_cache_transient_origin_response(TransientOriginCacheFetchContext {
            app_state,
            session,
            fingerprint,
            headers,
            access_context,
            resource,
            resource_file,
            cache_key,
            origin_headers,
            range_header,
            cache_duration_ms,
            origin_io_guard: transient_origin_guard,
        })
        .await;
    }

    let client = app_state.http_client.load().as_ref().clone();
    let no_redirect_client = app_state.http_client_no_redirect.load().as_ref().clone();
    let origin_request_headers = build_transient_resource_origin_headers(&origin_headers, range_header.clone());
    let policy = app_state.hls_proxy.segment_fetch_policy();
    match fetch_transient_resource_with_retries(
        &resource.resolved_origin_uri,
        &origin_request_headers,
        &client,
        &no_redirect_client,
        app_state.should_use_manual_redirects(),
        &policy,
        resource_file.resource_id.0.as_str(),
        resource.kind,
    )
    .await
    {
        Ok(response) => {
            if response.status().is_success() {
                mark_hls_authorized_media_access(app_state, session, now_ms).await;
                let _ =
                    ensure_hls_cache_stream_registered(app_state, fingerprint, headers, access_context, session).await;
            }
            let proxy_session_id = session.read().await.proxy_session_id.0.clone();
            transient_origin_response(
                response,
                Arc::clone(&resource.access),
                transient_origin_guard,
                now_ms,
                proxy_session_id,
                resource_file.resource_id.0.clone(),
                resource.kind,
                resource.resolved_origin_uri.clone(),
                app_state.hls_proxy.segment_fetch_policy().origin_segment_timeout_ms,
            )
        }
        Err(_) => hls_channel_unavailable_or_not_found_response(app_state),
    }
}

enum TransientObjectCacheAction {
    ServeReady,
    FetchAndCache(TransientObjectCacheKey),
    WaitForFetch(Arc<tokio::sync::Notify>),
    PassthroughNoCache,
}

#[allow(clippy::too_many_arguments)]
struct TransientOriginCacheFetchContext<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    access_context: &'a HlsAccessContext,
    resource: &'a crate::api::model::TransientResourceRef,
    resource_file: TransientResourceFile,
    cache_key: TransientObjectCacheKey,
    origin_headers: HeaderMap,
    range_header: Option<HeaderValue>,
    cache_duration_ms: u64,
    origin_io_guard: Option<TransientOriginIoGuard>,
}

struct TransientObjectFetchFinalizer {
    session: HlsSessionHandle,
    cache_key: TransientObjectCacheKey,
    completed: bool,
}

impl TransientObjectFetchFinalizer {
    fn new(session: HlsSessionHandle, cache_key: TransientObjectCacheKey) -> Self {
        Self { session, cache_key, completed: false }
    }

    fn complete(&mut self) { self.completed = true; }
}

impl Drop for TransientObjectFetchFinalizer {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let session = Arc::clone(&self.session);
        let cache_key = self.cache_key.clone();
        tokio::spawn(async move {
            session.write().await.transient.mark_object_failed_retryable(
                &cache_key,
                current_time_millis(),
                HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
            );
        });
    }
}

async fn resolve_transient_object_cache_action(
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    resource_file: &TransientResourceFile,
    range_header: Option<&HeaderValue>,
    now_ms: u64,
    cache_duration_ms: u64,
) -> Result<(crate::api::model::TransientResourceRef, HeaderMap, TransientObjectCacheAction), StatusCode> {
    let mut session = session.write().await;
    if session.is_gc_marked_for_removal() {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(resource) = session.transient.get_valid_resource(&resource_file.resource_id, now_ms) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if resource.file_ext_hint.as_deref().is_some_and(|extension| extension != resource_file.extension) {
        return Err(StatusCode::NOT_FOUND);
    }
    let cache_action = transient_object_cache_action(
        &mut session,
        proxy_session_id,
        &resource,
        resource_file,
        range_header,
        now_ms,
        cache_duration_ms,
    );
    Ok((resource, session.origin_request_headers.clone(), cache_action))
}

struct TransientObjectWaitContext<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    access_context: &'a HlsAccessContext,
    resource_file: TransientResourceFile,
    range_header: Option<HeaderValue>,
    notifier: Arc<tokio::sync::Notify>,
}

struct TransientObjectCacheServeContext<'a> {
    app_state: &'a Arc<AppState>,
    session: &'a HlsSessionHandle,
    fingerprint: &'a Fingerprint,
    headers: &'a HeaderMap,
    access_context: &'a HlsAccessContext,
    resource_file: TransientResourceFile,
    range_header: Option<HeaderValue>,
    now_ms: u64,
}

async fn serve_transient_object_cache_response_and_mark(
    context: TransientObjectCacheServeContext<'_>,
) -> axum::response::Response {
    let response = serve_hls_transient_object_cache_response(
        Arc::clone(context.app_state.hls_proxy.segment_cache()),
        Arc::clone(context.session),
        context.resource_file,
        context.range_header,
        &hls_cache_response_context(context.app_state, context.access_context, context.now_ms),
    )
    .await;
    if is_hls_media_activity_status(response.status()) {
        mark_hls_authorized_media_access(context.app_state, context.session, context.now_ms).await;
        let _ = ensure_hls_cache_stream_registered(
            context.app_state,
            context.fingerprint,
            context.headers,
            context.access_context,
            context.session,
        )
        .await;
    }
    response
}

async fn serve_transient_object_cache_response_and_mark_or_unavailable(
    context: TransientObjectCacheServeContext<'_>,
) -> axum::response::Response {
    let app_state = Arc::clone(context.app_state);
    let session = Arc::clone(context.session);
    let resource_file = context.resource_file.clone();
    let now_ms = context.now_ms;
    let response = serve_transient_object_cache_response_and_mark(context).await;
    if response.status() == StatusCode::SERVICE_UNAVAILABLE {
        return hls_transient_object_unavailable_response(&app_state, &session, &resource_file, now_ms).await;
    }
    response
}

async fn wait_for_transient_object_cache_fetch(context: TransientObjectWaitContext<'_>) -> axum::response::Response {
    let wait_timeout = context.app_state.hls_proxy.segment_fetch_policy().demand_wait_timeout();
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
        )
        .await;
    }
    let response = serve_transient_object_cache_response_and_mark_or_unavailable(TransientObjectCacheServeContext {
        app_state: context.app_state,
        session: context.session,
        fingerprint: context.fingerprint,
        headers: context.headers,
        access_context: context.access_context,
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

fn transient_object_cache_action(
    session: &mut crate::api::model::HlsSession,
    proxy_session_id: &ProxySessionId,
    resource: &crate::api::model::TransientResourceRef,
    resource_file: &TransientResourceFile,
    range_header: Option<&HeaderValue>,
    now_ms: u64,
    cache_duration_ms: u64,
) -> TransientObjectCacheAction {
    if matches!(resource.kind, TransientResourceKind::Key) {
        return TransientObjectCacheAction::PassthroughNoCache;
    }
    let cache_key = crate::api::model::TransientPassthroughState::transient_object_key(
        proxy_session_id,
        &resource.id,
        resource_file.extension.clone(),
    );
    if session.transient.ready_object(&cache_key, now_ms).is_some() {
        return TransientObjectCacheAction::ServeReady;
    }
    if !is_transient_full_object_cacheable_request(range_header) {
        return TransientObjectCacheAction::PassthroughNoCache;
    }
    session
        .transient
        .begin_object_fetch(proxy_session_id, resource, &resource_file.extension, now_ms, cache_duration_ms)
        .into()
}

impl From<TransientObjectFetchDecision> for TransientObjectCacheAction {
    fn from(decision: TransientObjectFetchDecision) -> Self {
        match decision {
            TransientObjectFetchDecision::Ready => Self::ServeReady,
            TransientObjectFetchDecision::Fetch(cache_key) => Self::FetchAndCache(cache_key),
            TransientObjectFetchDecision::Wait(notifier) => Self::WaitForFetch(notifier),
        }
    }
}

fn safe_transient_resource_id(resource_id: &crate::api::model::TransientResourceId) -> String {
    let mut value = resource_id.0.chars().take(8).collect::<String>();
    if resource_id.0.len() > value.len() {
        value.push_str("...");
    }
    value
}

fn is_transient_full_object_cacheable_request(range_header: Option<&HeaderValue>) -> bool {
    let Some(range_header) = range_header else {
        return true;
    };
    range_header.to_str().is_ok_and(|range| range.trim() == "bytes=0-")
}

async fn validate_hls_proxy_access_request(
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
    match app_state.hls_proxy.activate_access_lease(&context.lease_id, proxy_session_id, now_ms, timing).await {
        HlsAccessLeaseActivation::Activated { .. } => {
            debug!(
                "HLS access lease accepted: lease={} proxy_session={} session={} request={request_kind}",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            Ok(context)
        }
        HlsAccessLeaseActivation::Denied => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} session={} request={request_kind} reason=denied",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            Err(HlsAccessLeaseValidationError::AdmissionDenied)
        }
        HlsAccessLeaseActivation::Expired
        | HlsAccessLeaseActivation::UnknownLease
        | HlsAccessLeaseActivation::SessionMismatch => {
            warn!(
                "HLS access lease rejected: lease={} proxy_session={} session={} request={request_kind} reason=expired",
                safe_hls_access_lease_id(&context.lease_id),
                safe_proxy_session_id(proxy_session_id),
                safe_user_session_token(&context.user_session_token)
            );
            Err(HlsAccessLeaseValidationError::Expired)
        }
    }
}

async fn ensure_hls_cache_stream_registered(
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
    let mut stream_channel = build_hls_cache_stream_channel(app_state, access, &origin_source, &proxy_session_id).await;
    let provider = if user_session.provider.is_empty() {
        origin_account_binding
            .as_ref()
            .filter(|binding| binding.is_active())
            .map_or_else(|| Arc::clone(&origin_source.input_name), |binding| Arc::clone(&binding.account_name))
    } else {
        Arc::clone(&user_session.provider)
    };
    let user_agent = req_headers
        .get(header::USER_AGENT)
        .map_or_else(|| Cow::Borrowed(""), |value| String::from_utf8_lossy(value.as_bytes()));

    stream_channel.url = Arc::from(hls_cache_stream_stats_url(&proxy_session_id));
    stream_channel.item_type = PlaylistItemType::LiveHls;
    stream_channel.cluster = XtreamCluster::try_from(PlaylistItemType::LiveHls).unwrap_or(stream_channel.cluster);
    let shared_stream_id = hls_cache_shared_stream_id(&proxy_session_id);
    stream_channel.shared = true;
    stream_channel.shared_stream_id = Some(shared_stream_id);
    stream_channel.shared_joined_existing = Some(
        hls_cache_shared_joined_existing(app_state, shared_stream_id, &access.username, &access.user_session_token)
            .await,
    );

    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 0,
            username: &access.username,
            max_connections: user.max_connections,
            soft_connections: user.soft_connections,
            connection_kind: user_session.connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal),
            priority: user.priority,
            soft_priority: user.soft_priority,
            fingerprint,
            provider,
            stream_channel: &stream_channel,
            user_agent,
            session_token: Some(&access.user_session_token),
        })
        .await
}

async fn build_hls_cache_stream_channel(
    app_state: &Arc<AppState>,
    access: &HlsAccessContext,
    origin_source: &HlsOriginSource,
    proxy_session_id: &ProxySessionId,
) -> StreamChannel {
    if let Some((_, target)) = app_state.app_config.get_target_for_username(&access.username) {
        if let Some(mut channel) = get_stream_channel(app_state, &target, access.virtual_id).await {
            channel.url = Arc::from(hls_cache_stream_stats_url(proxy_session_id));
            channel.item_type = PlaylistItemType::LiveHls;
            channel.cluster = XtreamCluster::try_from(PlaylistItemType::LiveHls).unwrap_or(channel.cluster);
            return channel;
        }
        return fallback_hls_cache_stream_channel(target.id, access.virtual_id, origin_source, proxy_session_id);
    }

    fallback_hls_cache_stream_channel(0, access.virtual_id, origin_source, proxy_session_id)
}

fn fallback_hls_cache_stream_channel(
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
    }
}

fn hls_cache_stream_stats_url(proxy_session_id: &ProxySessionId) -> String {
    format!("/proxy/hls/live/{}/manifest.m3u8", proxy_session_id.0)
}

fn hls_cache_shared_stream_id(proxy_session_id: &ProxySessionId) -> u64 {
    let digest = Sha256::digest(proxy_session_id.0.as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 digest is at least 8 bytes"))
}

async fn hls_cache_shared_joined_existing(
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

async fn validate_hls_proxy_access_context(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    hls_access_lease_id: &str,
    now_ms: u64,
    admission_mode: HlsAccessAdmissionMode,
) -> Result<HlsAccessContext, HlsAccessLeaseValidationError> {
    validate_hls_access_lease(
        app_state,
        fingerprint,
        proxy_session_id,
        &HlsAccessLeaseId(hls_access_lease_id.to_string()),
        now_ms,
        admission_mode,
    )
    .await
}

fn hls_access_lease_validation_response(err: HlsAccessLeaseValidationError) -> axum::response::Response {
    match err {
        HlsAccessLeaseValidationError::AdmissionDenied => StatusCode::FORBIDDEN.into_response(),
        HlsAccessLeaseValidationError::InvalidLease
        | HlsAccessLeaseValidationError::SessionMismatch
        | HlsAccessLeaseValidationError::UserSessionMissing
        | HlsAccessLeaseValidationError::Expired => StatusCode::NOT_FOUND.into_response(),
    }
}

fn hls_custom_video_manifest_response_for_username(
    app_state: &Arc<AppState>,
    username: &str,
    video_type: CustomVideoStreamType,
    fallback_status: StatusCode,
) -> axum::response::Response {
    if let Some(user) = app_state.app_config.get_user_credentials(username) {
        return hls_custom_video_manifest_response(app_state, &user, video_type, fallback_status);
    }
    fallback_status.into_response()
}

fn hls_session_or_lease_expired_manifest_response(
    app_state: &Arc<AppState>,
    username: &str,
) -> axum::response::Response {
    hls_custom_video_manifest_response_for_username(
        app_state,
        username,
        CustomVideoStreamType::HlsSessionOrLeaseExpired,
        StatusCode::NOT_FOUND,
    )
}

async fn hls_manifest_access_lease_validation_response(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    lease_snapshot: Option<&HlsAccessLease>,
    now_ms: u64,
    err: HlsAccessLeaseValidationError,
) -> axum::response::Response {
    let marker = if lease_snapshot.is_none() {
        app_state.hls_proxy.expired_session_marker(proxy_session_id, now_ms).await
    } else {
        None
    };
    let username =
        lease_snapshot.map(|lease| lease.username.as_str()).or_else(|| marker.as_ref()?.username.as_deref());
    match err {
        HlsAccessLeaseValidationError::AdmissionDenied => username.map_or_else(
            || StatusCode::FORBIDDEN.into_response(),
            |username| {
                hls_custom_video_manifest_response_for_username(
                    app_state,
                    username,
                    CustomVideoStreamType::UserConnectionsExhausted,
                    StatusCode::FORBIDDEN,
                )
            },
        ),
        HlsAccessLeaseValidationError::InvalidLease
        | HlsAccessLeaseValidationError::SessionMismatch
        | HlsAccessLeaseValidationError::UserSessionMissing
        | HlsAccessLeaseValidationError::Expired => username.map_or_else(
            || StatusCode::NOT_FOUND.into_response(),
            |username| hls_session_or_lease_expired_manifest_response(app_state, username),
        ),
    }
}

async fn hls_manifest_access_context_and_state(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_snapshot: Option<&HlsAccessLease>,
    now_ms: u64,
) -> Result<(HlsAccessContext, HlsAccessLeaseState), axum::response::Response> {
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
                "HLS access lease rejected: lease={} proxy_session={} session=<unknown> reason={err:?}",
                safe_hls_access_lease_id(access_lease_id),
                safe_proxy_session_id(proxy_session_id)
            );
            return Err(hls_manifest_access_lease_validation_response(
                app_state,
                proxy_session_id,
                access_lease_snapshot,
                now_ms,
                err,
            )
            .await);
        }
    };
    if app_state.hls_proxy.sessions().get_by_proxy_session_id(proxy_session_id).await.is_none()
        && app_state.hls_proxy.expired_session_marker(proxy_session_id, now_ms).await.is_some()
    {
        return Err(hls_session_or_lease_expired_manifest_response(app_state, &access_context.username));
    }
    debug!(
        "HLS access lease accepted: lease={} proxy_session={} session={} request=manifest",
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
            hls_access_lease_ttl_ms(app_state),
        )
        .await
    {
        HlsAccessLeaseTouch::Touched { lease } => lease.state,
        HlsAccessLeaseTouch::Denied => {
            return Err(hls_custom_video_manifest_response_for_username(
                app_state,
                &access_context.username,
                CustomVideoStreamType::UserConnectionsExhausted,
                StatusCode::FORBIDDEN,
            ));
        }
        HlsAccessLeaseTouch::Expired | HlsAccessLeaseTouch::UnknownLease | HlsAccessLeaseTouch::SessionMismatch => {
            return Err(hls_session_or_lease_expired_manifest_response(app_state, &access_context.username));
        }
    };

    Ok((access_context, access_lease_state))
}

fn hls_channel_unavailable_or_not_found_response(app_state: &Arc<AppState>) -> axum::response::Response {
    if let (Some(stream), _) = create_channel_unavailable_stream(&app_state.app_config, &[], StatusCode::NOT_FOUND) {
        let mut response = try_unwrap_body!(axum::response::Response::builder()
            .status(StatusCode::OK)
            .body(Body::from_stream(stream)));
        mark_response_as_uncompressed(&mut response);
        return response;
    }
    StatusCode::NOT_FOUND.into_response()
}

fn hls_temporary_resource_unavailable_response(retry_after_ms: u64) -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::RETRY_AFTER, retry_after_secs_from_ms(retry_after_ms).to_string())
        .body(Body::empty()))
}

async fn hls_transient_object_unavailable_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    resource_file: &TransientResourceFile,
    now_ms: u64,
) -> axum::response::Response {
    let state = {
        let session = session.read().await;
        let key = crate::api::model::TransientPassthroughState::transient_object_key(
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
            hls_channel_unavailable_or_not_found_response(app_state)
        }
    }
}

#[derive(Debug)]
enum TransientResourceFetchError {
    PermanentStatus(StatusCode),
    RetryableStatus(StatusCode),
    NonRetryableStatus(StatusCode),
    InvalidOriginUrl,
    Request,
    Redirect,
    Timeout,
    Cache,
}

enum TransientObjectFetchFailure {
    Retryable,
    Permanent { status: Option<StatusCode> },
}

impl TransientResourceFetchError {
    fn object_failure(&self) -> TransientObjectFetchFailure {
        match self {
            Self::PermanentStatus(status) | Self::NonRetryableStatus(status) => {
                TransientObjectFetchFailure::Permanent { status: Some(*status) }
            }
            Self::InvalidOriginUrl => TransientObjectFetchFailure::Permanent { status: None },
            Self::RetryableStatus(_)
            | Self::Request
            | Self::Redirect
            | Self::Timeout
            | Self::Cache => TransientObjectFetchFailure::Retryable,
        }
    }
}

async fn fetch_transient_resource(
    resolved_origin_uri: &str,
    headers: HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
) -> Result<reqwest::Response, TransientResourceFetchError> {
    let url = Url::parse(resolved_origin_uri).map_err(|_| TransientResourceFetchError::InvalidOriginUrl)?;
    if use_manual_redirects {
        fetch_transient_resource_with_manual_redirects(&url, headers, no_redirect_client).await
    } else {
        client.get(url).headers(headers).send().await.map_err(|_| TransientResourceFetchError::Request)
    }
}

async fn fetch_transient_resource_with_retries(
    resolved_origin_uri: &str,
    headers: &HeaderMap,
    client: &Client,
    no_redirect_client: &Client,
    use_manual_redirects: bool,
    policy: &SegmentFetchPolicy,
    resource_id: &str,
    resource_kind: TransientResourceKind,
) -> Result<reqwest::Response, TransientResourceFetchError> {
    let attempts = policy.retry_delays_ms.len();
    for attempt_index in 0..attempts {
        let attempt = HlsResourceFetchAttempt { attempt_index, attempts };
        let delay_ms = policy.retry_delay_ms(attempt_index);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }

        log_hls_resource_attempt_started(transient_retry_log_context(resource_id, resource_kind, resolved_origin_uri), attempt);
        let attempt_started_at = Instant::now();
        let result = fetch_transient_resource(
            resolved_origin_uri,
            headers.clone(),
            client,
            no_redirect_client,
            use_manual_redirects,
        )
        .await
        .and_then(classify_transient_resource_response);

        match result {
            Ok(response) => {
                log_hls_resource_attempt_succeeded(
                    transient_retry_log_context(resource_id, resource_kind, resolved_origin_uri),
                    attempt_started_at.elapsed(),
                );
                return Ok(response);
            }
            Err(
                err @ (TransientResourceFetchError::PermanentStatus(_)
                | TransientResourceFetchError::NonRetryableStatus(_)
                | TransientResourceFetchError::InvalidOriginUrl
                | TransientResourceFetchError::Cache),
            ) => {
                log_hls_resource_fetch_failed(
                    transient_retry_log_context(resource_id, resource_kind, resolved_origin_uri),
                    attempt,
                    transient_fetch_error_log_status(&err),
                );
                return Err(err);
            }
            Err(err) if attempt_index + 1 == attempts => {
                log_hls_resource_fetch_failed(
                    transient_retry_log_context(resource_id, resource_kind, resolved_origin_uri),
                    attempt,
                    transient_fetch_error_log_status(&err),
                );
                return Err(err);
            }
            Err(err) => {
                log_hls_resource_retry_scheduled(
                    transient_retry_log_context(resource_id, resource_kind, resolved_origin_uri),
                    attempt,
                    transient_fetch_error_log_status(&err),
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default(),
                );
            }
        }
    }

    Err(TransientResourceFetchError::Timeout)
}

fn classify_transient_resource_response(
    response: reqwest::Response,
) -> Result<reqwest::Response, TransientResourceFetchError> {
    let status = response.status();
    match classify_hls_resource_status(status) {
        HlsResourceStatusClass::Success => Ok(response),
        HlsResourceStatusClass::Retryable => Err(TransientResourceFetchError::RetryableStatus(status)),
        HlsResourceStatusClass::Permanent => Err(TransientResourceFetchError::PermanentStatus(status)),
        HlsResourceStatusClass::NonRetryable => Err(TransientResourceFetchError::NonRetryableStatus(status)),
    }
}

#[allow(clippy::too_many_lines)]
async fn fetch_and_cache_transient_origin_response(
    context: TransientOriginCacheFetchContext<'_>,
) -> axum::response::Response {
    let policy = context.app_state.hls_proxy.segment_fetch_policy();
    let client = context.app_state.http_client.load().as_ref().clone();
    let no_redirect_client = context.app_state.http_client_no_redirect.load().as_ref().clone();
    let mut origin_io_guard = context.origin_io_guard;
    let mut fetch_finalizer =
        TransientObjectFetchFinalizer::new(Arc::clone(context.session), context.cache_key.clone());
    let mut final_failure = TransientObjectFetchFailure::Retryable;
    let resource_id = context.resource_file.resource_id.0.as_str();
    let attempts = policy.retry_delays_ms.len();

    for attempt_index in 0..attempts {
        let attempt = HlsResourceFetchAttempt { attempt_index, attempts };
        let delay_ms = policy.retry_delay_ms(attempt_index);
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        log_hls_resource_attempt_started(
            transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
            attempt,
        );
        let attempt_started_at = Instant::now();
        let origin_request_headers =
            build_transient_resource_origin_headers(&context.origin_headers, context.range_header.clone());
        let fetch_result = fetch_transient_resource(
            &context.resource.resolved_origin_uri,
            origin_request_headers,
            &client,
            &no_redirect_client,
            context.app_state.should_use_manual_redirects(),
        )
        .await
        .and_then(classify_transient_resource_response);

        let attempt_result = match fetch_result {
            Ok(response) => {
                cache_transient_origin_response_attempt(
                    context.app_state,
                    context.session,
                    context.fingerprint,
                    context.headers,
                    context.access_context,
                    context.resource,
                    context.resource_file.clone(),
                    &context.cache_key,
                    response,
                    context.range_header.clone(),
                    context.cache_duration_ms,
                    &policy,
                    attempt_index,
                    policy.retry_delays_ms.len(),
                )
                .await
            }
            Err(err) => Err(err),
        };

        match attempt_result {
            Ok(response) => {
                log_hls_resource_attempt_succeeded(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt_started_at.elapsed(),
                );
                fetch_finalizer.complete();
                drop(origin_io_guard.take());
                return response;
            }
            Err(
                TransientResourceFetchError::PermanentStatus(status)
                | TransientResourceFetchError::NonRetryableStatus(status),
            ) => {
                log_hls_resource_fetch_failed(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    HlsResourceFetchLogStatus::Http(status),
                );
                final_failure = TransientObjectFetchFailure::Permanent { status: Some(status) };
                break;
            }
            Err(TransientResourceFetchError::RetryableStatus(status)) if attempt_index + 1 == attempts =>
            {
                log_hls_resource_fetch_failed(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    HlsResourceFetchLogStatus::Http(status),
                );
                final_failure = TransientObjectFetchFailure::Retryable;
                break;
            }
            Err(TransientResourceFetchError::RetryableStatus(status)) => {
                log_hls_resource_retry_scheduled(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    HlsResourceFetchLogStatus::Http(status),
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default()
                );
            }
            Err(TransientResourceFetchError::Timeout) if attempt_index + 1 == attempts => {
                log_hls_resource_fetch_failed(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    HlsResourceFetchLogStatus::Timeout,
                );
                final_failure = TransientObjectFetchFailure::Retryable;
                break;
            }
            Err(TransientResourceFetchError::Timeout) => {
                log_hls_resource_retry_scheduled(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    HlsResourceFetchLogStatus::Timeout,
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default()
                );
            }
            Err(err @ (TransientResourceFetchError::Request | TransientResourceFetchError::Redirect))
                if attempt_index + 1 == attempts =>
            {
                let status = transient_fetch_error_log_status(&err);
                log_hls_resource_fetch_failed(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    status,
                );
                final_failure = TransientObjectFetchFailure::Retryable;
                break;
            }
            Err(err @ (TransientResourceFetchError::Request | TransientResourceFetchError::Redirect)) => {
                let status = transient_fetch_error_log_status(&err);
                log_hls_resource_retry_scheduled(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    status,
                    policy.retry_delays_ms.get(attempt_index + 1).copied().unwrap_or_default()
                );
            }
            Err(err @ (TransientResourceFetchError::InvalidOriginUrl | TransientResourceFetchError::Cache)) => {
                log_hls_resource_fetch_failed(
                    transient_retry_log_context(resource_id, context.resource.kind, &context.resource.resolved_origin_uri),
                    attempt,
                    transient_fetch_error_log_status(&err),
                );
                final_failure = err.object_failure();
                break;
            }
        }
    }

    let failed_at_ms = current_time_millis();
    match final_failure {
        TransientObjectFetchFailure::Retryable => {
            context.session.write().await.transient.mark_object_failed_retryable(
                &context.cache_key,
                failed_at_ms,
                HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
            );
        }
        TransientObjectFetchFailure::Permanent { status } => {
            context.session.write().await.transient.mark_object_failed_permanent(
                &context.cache_key,
                failed_at_ms,
                status,
            );
        }
    }
    fetch_finalizer.complete();
    drop(origin_io_guard);
    hls_transient_object_unavailable_response(
        context.app_state,
        context.session,
        &context.resource_file,
        failed_at_ms,
    )
    .await
}

fn transient_fetch_error_log_status(error: &TransientResourceFetchError) -> HlsResourceFetchLogStatus {
    match error {
        TransientResourceFetchError::PermanentStatus(status)
        | TransientResourceFetchError::RetryableStatus(status)
        | TransientResourceFetchError::NonRetryableStatus(status) => HlsResourceFetchLogStatus::Http(*status),
        TransientResourceFetchError::Timeout => HlsResourceFetchLogStatus::Timeout,
        TransientResourceFetchError::Request => HlsResourceFetchLogStatus::TransportError,
        TransientResourceFetchError::Redirect => HlsResourceFetchLogStatus::RedirectError,
        TransientResourceFetchError::Cache => HlsResourceFetchLogStatus::CacheCommitError,
        TransientResourceFetchError::InvalidOriginUrl => HlsResourceFetchLogStatus::TransportError,
    }
}

fn transient_retry_log_context<'a>(
    resource_id: &'a str,
    resource_kind: TransientResourceKind,
    origin_url: &'a str,
) -> HlsResourceFetchLogContext<'a> {
    HlsResourceFetchLogContext {
        kind: transient_resource_fetch_kind(resource_kind),
        object_id: resource_id,
        origin_url: Some(origin_url),
    }
}

fn transient_resource_fetch_kind(resource_kind: TransientResourceKind) -> HlsResourceFetchKind {
    match resource_kind {
        TransientResourceKind::Key => HlsResourceFetchKind::Key,
        TransientResourceKind::Map => HlsResourceFetchKind::Map,
        TransientResourceKind::Segment | TransientResourceKind::Other => HlsResourceFetchKind::Segment,
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn cache_transient_origin_response_attempt(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    fingerprint: &Fingerprint,
    headers: &HeaderMap,
    access_context: &HlsAccessContext,
    resource: &crate::api::model::TransientResourceRef,
    resource_file: TransientResourceFile,
    cache_key: &TransientObjectCacheKey,
    response: reqwest::Response,
    range_header: Option<HeaderValue>,
    cache_duration_ms: u64,
    policy: &SegmentFetchPolicy,
    attempt_index: usize,
    attempts: usize,
) -> Result<axum::response::Response, TransientResourceFetchError> {
    let response_headers = response.headers().clone();
    let content_type = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| resource.content_type_hint.clone())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let deadline = hls_object_body_deadline(policy.origin_segment_timeout_ms);

    let stream_reader = StreamReader::new(response.bytes_stream().map_err(io::Error::other));
    let commit = app_state
        .hls_proxy
        .segment_repair()
        .commit_origin_response(
            app_state.hls_proxy.segment_cache(),
            cache_key,
            stream_reader,
            deadline,
            HlsSegmentRepairObjectContext {
                source: HlsSegmentRepairSource::Transient,
                proxy_session_id: session.read().await.proxy_session_id.clone(),
                hls_access_lease_id: Some(access_context.lease_id.clone()),
                rendered_object_id: HlsRepairRenderedObjectId::Transient {
                    resource_id: resource_file.resource_id.0.clone(),
                },
                resource_id: resource_file.resource_id.0.clone(),
                file_ext: resource_file.extension.clone(),
                normalized_origin_uri: resource.resolved_origin_uri.clone(),
                media_sequence: None,
                discontinuity_sequence: None,
                complete_object: is_transient_full_object_cacheable_request(range_header.as_ref()),
                encrypted: resource.kind == TransientResourceKind::Key,
                custom_response: false,
            },
        )
        .await;
    let ready_at_ms = current_time_millis();
    let metadata = match commit {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::TimedOut => {
            let proxy_session_id = session.read().await.proxy_session_id.0.clone();
            log_hls_resource_timeout(
                &proxy_session_id,
                transient_retry_log_context(
                    resource_file.resource_id.0.as_str(),
                    resource.kind,
                    &resource.resolved_origin_uri,
                ),
                HlsResourceFetchAttempt { attempt_index, attempts },
                deadline.as_millis(),
            );
            return Err(TransientResourceFetchError::Timeout);
        }
        Err(_) => {
            return Err(TransientResourceFetchError::Cache);
        }
    };
    let content_length = metadata.size;

    let expires_at_ms = ready_at_ms.saturating_add(cache_duration_ms).max(resource.expires_at_ms);
    session.write().await.transient.mark_object_ready(
        cache_key,
        content_type,
        content_length,
        ready_at_ms,
        expires_at_ms,
    );
    let response = serve_hls_transient_object_cache_response(
        Arc::clone(app_state.hls_proxy.segment_cache()),
        Arc::clone(session),
        resource_file,
        range_header,
        &hls_cache_response_context(app_state, access_context, ready_at_ms),
    )
    .await;
    if is_hls_media_activity_status(response.status()) {
        mark_hls_authorized_media_access(app_state, session, ready_at_ms).await;
        let _ = ensure_hls_cache_stream_registered(app_state, fingerprint, headers, access_context, session).await;
    }
    Ok(response)
}

async fn fetch_transient_resource_with_manual_redirects(
    entry_url: &Url,
    headers: HeaderMap,
    client: &Client,
) -> Result<reqwest::Response, TransientResourceFetchError> {
    let mut current_url = entry_url.clone();
    let mut current_headers = headers;
    let mut remaining_redirects = MAX_MANUAL_REDIRECTS;

    loop {
        let response = client
            .get(current_url.clone())
            .headers(current_headers.clone())
            .send()
            .await
            .map_err(|_| TransientResourceFetchError::Request)?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if remaining_redirects == 0 {
            return Err(TransientResourceFetchError::Redirect);
        }
        let response_url = response.url().clone();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(TransientResourceFetchError::Redirect)?;
        let next_url = response_url
            .join(location)
            .or_else(|_| Url::parse(location))
            .map_err(|_| TransientResourceFetchError::Redirect)?;
        if !same_origin(&response_url, &next_url) {
            strip_sensitive_headers_for_cross_origin_redirect(&mut current_headers);
        }
        current_url = next_url;
        remaining_redirects = remaining_redirects.saturating_sub(1);
    }
}

fn build_transient_resource_origin_headers(source_headers: &HeaderMap, client_range: Option<HeaderValue>) -> HeaderMap {
    let mut headers = source_headers.clone();
    scrub_hls_origin_headers(&mut headers, None);
    headers.remove(header::RANGE);
    headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if let Some(range) = client_range {
        headers.insert(header::RANGE, range);
    }
    headers
}

fn transient_origin_response(
    response: reqwest::Response,
    access: Arc<CacheAccessState>,
    origin_io_guard: Option<TransientOriginIoGuard>,
    now_ms: u64,
    proxy_session_id: String,
    resource_id: String,
    resource_kind: TransientResourceKind,
    origin_url: String,
    origin_segment_timeout_ms: u64,
) -> axum::response::Response {
    let mut builder = axum::response::Response::builder().status(response.status());
    for header_name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CACHE_CONTROL,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        if let Some(value) = response.headers().get(&header_name) {
            builder = builder.header(header_name, value.clone());
        }
    }

    let guard = TransientReadGuard::new(access, now_ms);
    let deadline = hls_object_body_deadline(origin_segment_timeout_ms);
    let stream = futures::stream::unfold(
        (response.bytes_stream(), Some(guard), origin_io_guard, false),
        move |(mut stream, guard, origin_io_guard, finished)| {
            let proxy_session_id = proxy_session_id.clone();
            let resource_id = resource_id.clone();
            let resource_kind = resource_kind;
            let origin_url = origin_url.clone();
            async move {
                if finished {
                    return None;
                }
                let next_chunk = tokio::time::timeout(deadline, stream.next()).await;
                match next_chunk {
                    Ok(Some(Ok(chunk))) => Some((Ok(chunk), (stream, guard, origin_io_guard, false))),
                    Ok(Some(Err(err))) => {
                        Some((Err(io::Error::other(err)), (stream, guard, origin_io_guard, true)))
                    }
                    Ok(None) => None,
                    Err(_) => {
                        log_hls_resource_timeout(
                            &proxy_session_id,
                            transient_retry_log_context(&resource_id, resource_kind, &origin_url),
                            HlsResourceFetchAttempt { attempt_index: 0, attempts: 1 },
                            deadline.as_millis(),
                        );
                        Some((
                            Err(io::Error::new(io::ErrorKind::TimedOut, "transient passthrough body timed out")),
                            (stream, guard, origin_io_guard, true),
                        ))
                    }
                }
            }
        },
    );
    try_unwrap_body!(builder.body(Body::from_stream(stream)))
}

struct TransientReadGuard {
    access: Arc<CacheAccessState>,
}

impl TransientReadGuard {
    fn new(access: Arc<CacheAccessState>, now_ms: u64) -> Self {
        access.reader_started(now_ms);
        Self { access }
    }
}

impl Drop for TransientReadGuard {
    fn drop(&mut self) { self.access.reader_finished(); }
}

fn same_origin(lhs: &Url, rhs: &Url) -> bool {
    lhs.scheme().eq_ignore_ascii_case(rhs.scheme())
        && lhs.host_str() == rhs.host_str()
        && lhs.port_or_known_default() == rhs.port_or_known_default()
}

fn strip_sensitive_headers_for_cross_origin_redirect(headers: &mut HeaderMap) {
    scrub_hls_origin_headers(headers, None);
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

async fn release_prepared_hls_manifest_session(
    app_state: &Arc<AppState>,
    username: &str,
    session_token: &str,
    addr: &std::net::SocketAddr,
) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.release_unbound_session_reservation(username, session_token, None, false).await;
    app_state.active_users.clear_unbound_session_addr(username, session_token, addr).await;
}

async fn terminate_failed_hls_manifest_session(app_state: &Arc<AppState>, username: &str, session_token: &str) {
    let _transition_guard = app_state.active_users.acquire_playback_transition(username, session_token).await;
    app_state.active_users.terminate_session(username, session_token).await;
    app_state.active_provider.clear_provider_reservation(session_token).await;
}

fn normalize_xtream_live_hls_url(hls_url: &str, input: &ConfigInput) -> String {
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

fn ensure_hls_manifest_extension(url: &str) -> String {
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

fn build_hls_manifest_request_headers(
    input_headers: &HashMap<String, String>,
    req_headers: &HeaderMap,
    disabled_headers: Option<&ReverseProxyDisabledHeaderConfig>,
    default_user_agent: Option<&str>,
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
    scrub_hls_origin_headers(&mut headers, disabled_headers);
    headers
}

struct HlsCacheManifestOrigin<'a> {
    raw_request_url: &'a str,
    session_entry_url: &'a str,
    input: &'a ConfigInput,
    origin_source: HlsOriginSource,
    failover_provider: Option<Arc<ConfigProvider>>,
}

struct HlsCacheOriginResolution {
    hls_url: String,
    session_entry_url: String,
    failover_provider: Option<Arc<ConfigProvider>>,
}

fn resolve_hls_cache_origin_entry_url(input: &ConfigInput, url: &str) -> Option<HlsCacheOriginResolution> {
    if let Some(provider) = input.get_resolve_provider(url) {
        return Some(HlsCacheOriginResolution {
            session_entry_url: url.to_string(),
            hls_url: url.to_string(),
            failover_provider: Some(Arc::clone(&provider)),
        });
    }

    let parsed = Url::parse(url).ok()?;
    if matches!(parsed.scheme(), "http" | "https") {
        return Some(HlsCacheOriginResolution {
            hls_url: url.to_string(),
            session_entry_url: url.to_string(),
            failover_provider: None,
        });
    }

    warn!("HLS origin entry URL is not supported: url={}", sanitize_sensitive_info(url));
    None
}

fn is_http_hls_origin_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

fn is_supported_hls_origin_url(input: &ConfigInput, url: &str) -> bool {
    input.get_resolve_provider(url).is_some() || is_http_hls_origin_url(url)
}

fn hls_stream_ref_from_virtual_id(virtual_id: u32) -> String { virtual_id.to_string() }

fn build_hls_origin_source(input: &ConfigInput, stream_ref: impl Into<String>) -> HlsOriginSource {
    HlsOriginSource::new(input.id, Arc::clone(&input.name), stream_ref, hls_origin_source_kind(input.input_type))
}

fn hls_origin_source_kind(input_type: InputType) -> HlsOriginSourceKind {
    if input_type.is_xtream() {
        HlsOriginSourceKind::XtreamLive
    } else if input_type.is_m3u() {
        HlsOriginSourceKind::M3uMediaPlaylist
    } else {
        HlsOriginSourceKind::DirectMediaPlaylist
    }
}

fn build_hls_origin_resolution(input: &ConfigInput, media_playlist_url: &str) -> Option<HlsCacheOriginResolution> {
    let candidate = match hls_origin_source_kind(input.input_type) {
        HlsOriginSourceKind::XtreamLive => {
            ensure_hls_manifest_extension(&normalize_xtream_live_hls_url(media_playlist_url, input))
        }
        HlsOriginSourceKind::M3uMediaPlaylist | HlsOriginSourceKind::DirectMediaPlaylist => {
            ensure_hls_manifest_extension(media_playlist_url)
        }
    };
    resolve_hls_cache_origin_entry_url(input, &candidate)
}

#[derive(Clone, Copy)]
enum HlsOriginWorkKind {
    Manifest,
    Segment,
    Resource,
}

impl HlsOriginWorkKind {
    const fn as_log_value(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Segment => "segment",
            Self::Resource => "resource",
        }
    }
}

fn build_hls_origin_fetch_url(
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    provider_config: Option<&Arc<RuntimeProviderConfig>>,
) -> Option<String> {
    let url = if let Some(provider_config) = provider_config {
        get_stream_alternative_url(raw_request_url, input, provider_config)
            .or_else(|| get_stream_alternative_url(session_entry_url, input, provider_config))
            .unwrap_or_else(|| session_entry_url.to_string())
    } else {
        session_entry_url.to_string()
    };

    if is_supported_hls_origin_url(input, &url) {
        Some(url)
    } else {
        None
    }
}

struct PreparedHlsOriginRuntime {
    fetch_url: String,
    failover_provider: Option<Arc<ConfigProvider>>,
    binding_to_store: Option<HlsOriginAccountBinding>,
    preacquired_provider_handle: Option<ProviderHandle>,
}

#[derive(Debug, PartialEq, Eq)]
enum HlsOriginRuntimeAcquireError {
    NoAccountAvailable,
    Fatal(StatusCode),
}

impl HlsOriginRuntimeAcquireError {
    const fn status_code(&self) -> StatusCode {
        match self {
            Self::NoAccountAvailable | Self::Fatal(StatusCode::SERVICE_UNAVAILABLE) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Fatal(status) => *status,
        }
    }
}

#[derive(Clone)]
struct HlsAccountOverlapCandidate {
    proxy_session_id: ProxySessionId,
    account_name: Arc<str>,
    session_owner: String,
    reclaim_until_ms: u64,
    last_media_at_ms: u64,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn prepare_hls_origin_runtime(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    promote_elapsed_hls_account_overlaps(app_state, now_ms).await;
    detach_unprotected_hls_origin_account_bindings(app_state, now_ms).await;
    reclaim_hls_account_overlap_if_needed(app_state, session, now_ms).await;
    let existing_binding = session.read().await.origin_account_binding.clone();
    let reacquire_detached_binding = existing_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached);
    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_started(session, work_kind).await;
    }
    if let Some(binding) = existing_binding {
        if binding.is_active() {
            match hls_origin_account_status(app_state, &binding) {
                stale_status @ (HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired) => {
                    return rebind_hls_origin_account(
                        app_state,
                        session,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                        stale_status,
                        fingerprint,
                        connection_kind,
                        priority,
                        now_ms,
                    )
                    .await;
                }
                HlsOriginAccountStatus::Known => {
                    return Ok(prepared_hls_origin_runtime_for_known_binding(
                        app_state,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                    ));
                }
            }
        }
    }

    match prepare_hls_origin_runtime_with_new_account(
        app_state,
        input,
        raw_request_url,
        session_entry_url,
        proxy_session_id,
        fingerprint,
        connection_kind,
        priority,
        false,
        work_kind,
        work_class,
        now_ms,
    )
    .await
    {
        Ok(prepared) => {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
        Err(HlsOriginRuntimeAcquireError::Fatal(status)) => return Err(HlsOriginRuntimeAcquireError::Fatal(status)),
        Err(HlsOriginRuntimeAcquireError::NoAccountAvailable) => {}
    }

    if work_class.allows_speculative_overlap() {
        if let Ok(prepared) = prepare_hls_speculative_origin_runtime(
            app_state,
            session,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            now_ms,
        )
        .await
        {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.binding_to_store.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
    } else {
        debug!("HLS account overlap skipped: work_class={} reason=background-origin-work", work_class.as_log_value());
    }

    if work_class.allows_grace() {
        match prepare_hls_origin_runtime_with_new_account(
            app_state,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            true,
            work_kind,
            work_class,
            now_ms,
        )
        .await
        {
            Ok(prepared) => {
                if reacquire_detached_binding {
                    if let Some(binding) = prepared.binding_to_store.as_ref() {
                        log_hls_origin_binding_reacquired(session, binding).await;
                    }
                }
                return Ok(prepared);
            }
            Err(HlsOriginRuntimeAcquireError::Fatal(status)) => {
                return Err(HlsOriginRuntimeAcquireError::Fatal(status))
            }
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable) => {}
        }
    } else {
        debug!(
            "HLS origin account grace skipped: work_class={} reason=background-origin-work",
            work_class.as_log_value()
        );
    }

    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_failed(session, "no-account-available").await;
    }
    Err(HlsOriginRuntimeAcquireError::NoAccountAvailable)
}

#[allow(clippy::too_many_arguments)]
async fn prepare_hls_origin_runtime_with_new_account(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    allow_grace: bool,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            allow_grace,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        debug!(
            "HLS origin account acquire unavailable: work={} work_class={} grace={}",
            work_kind.as_log_value(),
            work_class.as_log_value(),
            if allow_grace { "attempted" } else { "disabled" }
        );
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable);
    };

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(binding) = origin_account_binding_from_allocation(
        Arc::clone(&input.name),
        proxy_session_id,
        &provider_handle.allocation,
        now_ms,
    ) else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let grace_state = if matches!(provider_handle.allocation, ProviderAllocation::GracePeriod(_)) {
        "granted"
    } else if allow_grace {
        "not-needed"
    } else {
        "disabled"
    };
    debug!(
        "HLS origin account binding created: account={} owner={} work={} work_class={} grace={}",
        sanitize_sensitive_info(binding.account_name.as_ref()),
        sanitize_sensitive_info(&binding.session_owner),
        work_kind.as_log_value(),
        work_class.as_log_value(),
        grace_state
    );

    Ok(PreparedHlsOriginRuntime {
        failover_provider: input.get_resolve_provider(&fetch_url),
        fetch_url,
        binding_to_store: Some(binding),
        preacquired_provider_handle: Some(provider_handle),
    })
}

fn prepared_hls_origin_runtime_for_known_binding(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    binding: &HlsOriginAccountBinding,
) -> PreparedHlsOriginRuntime {
    let fetch_url = app_state
        .active_provider
        .find_provider_config(&binding.account_name)
        .as_ref()
        .and_then(|provider_config| {
            build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(provider_config))
        })
        .unwrap_or_else(|| session_entry_url.to_string());

    PreparedHlsOriginRuntime {
        failover_provider: input.get_resolve_provider(&fetch_url),
        fetch_url,
        binding_to_store: None,
        preacquired_provider_handle: None,
    }
}

async fn log_hls_origin_binding_reacquire_started(session: &HlsSessionHandle, work_kind: HlsOriginWorkKind) {
    let session_guard = session.read().await;
    let mode = match session_guard.mode {
        HlsSessionMode::NormalCacheTimeline => "normal",
        HlsSessionMode::TransientPassthrough { .. } => "transient",
    };
    debug!(
        "HLS origin binding reacquire started: session={} mode={} work={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        mode,
        work_kind.as_log_value()
    );
}

async fn log_hls_origin_binding_reacquired(session: &HlsSessionHandle, binding: &HlsOriginAccountBinding) {
    let session_guard = session.read().await;
    debug!(
        "HLS origin binding reacquired: session={} account={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        sanitize_sensitive_info(binding.account_name.as_ref())
    );
}

async fn log_hls_origin_binding_reacquire_failed(session: &HlsSessionHandle, reason: &str) {
    let session_guard = session.read().await;
    debug!(
        "HLS origin binding reacquire failed: session={} reason={} retry_after_ms={}",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        reason,
        cold_start_retry_after_seconds().saturating_mul(1_000)
    );
}

async fn detach_unprotected_hls_origin_account_bindings(app_state: &Arc<AppState>, now_ms: u64) {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let binding = {
            let mut session_guard = session.write().await;
            let Some(binding) = session_guard.origin_account_binding.clone() else {
                continue;
            };
            if !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
                continue;
            }
            let timing = session_guard.account_overlap_timing();
            let protection = session_guard.account_binding_protection(now_ms);
            debug!(
                "HLS account protection classified: session={} state={} target_duration_ms={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                protection.as_log_state(),
                timing.target_duration_ms
            );
            if !matches!(protection, HlsAccountBindingProtection::Expired)
                || session_guard.activity.active_origin_work_count > 0
            {
                continue;
            }
            if !matches!(hls_origin_account_status(app_state, &binding), HlsOriginAccountStatus::Known) {
                continue;
            }
            if let Some(binding) = session_guard.origin_account_binding.as_mut() {
                binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, now_ms);
            }
            session_guard.invalidate_queued_origin_work();
            debug!(
                "HLS origin binding detached: session={} account={} reason={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(binding.account_name.as_ref()),
                HlsOriginAccountDetachedReason::SoftWindowElapsed.as_log_reason()
            );
            binding
        };
        app_state.active_provider.clear_provider_reservation(&binding.session_owner).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_hls_speculative_origin_runtime(
    app_state: &Arc<AppState>,
    new_session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    let Some(candidate) = find_hls_account_overlap_candidate(app_state, &input.name, proxy_session_id, now_ms).await
    else {
        debug!("HLS account overlap denied: reason=no-soft-active-candidate");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    app_state.active_provider.clear_provider_reservation(&candidate.session_owner).await;
    let session_owner = build_hls_origin_session_owner(proxy_session_id);
    let Some(provider_handle) = app_state
        .active_provider
        .acquire_exact_connection_with_grace_for_session(
            &candidate.account_name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&session_owner),
        )
        .await
    else {
        debug!("HLS account overlap denied: reason=speculative-acquire-failed");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        debug!("HLS account overlap denied: reason=missing-provider-config");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        debug!("HLS account overlap denied: reason=invalid-origin-url");
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };
    let binding = HlsOriginAccountBinding::speculative_from(
        Arc::clone(&input.name),
        Arc::clone(&candidate.account_name),
        proxy_session_id,
        candidate.proxy_session_id.clone(),
        candidate.reclaim_until_ms,
        now_ms,
    );
    {
        let mut session_guard = new_session.write().await;
        session_guard.origin_account_binding = Some(binding.clone());
    }
    debug!(
        "HLS account overlap granted: account={} old_session={} new_session={} reclaim_until_ms={}",
        sanitize_sensitive_info(candidate.account_name.as_ref()),
        safe_proxy_session_id(&candidate.proxy_session_id),
        safe_proxy_session_id(proxy_session_id),
        candidate.reclaim_until_ms
    );
    Ok(PreparedHlsOriginRuntime {
        failover_provider: input.get_resolve_provider(&fetch_url),
        fetch_url,
        binding_to_store: Some(binding),
        preacquired_provider_handle: Some(provider_handle),
    })
}

async fn find_hls_account_overlap_candidate(
    app_state: &Arc<AppState>,
    input_name: &Arc<str>,
    new_proxy_session_id: &ProxySessionId,
    now_ms: u64,
) -> Option<HlsAccountOverlapCandidate> {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    let mut speculative_accounts = Vec::new();
    for session in &sessions {
        let session = session.read().await;
        let Some(binding) = session.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != *input_name {
            continue;
        }
        if matches!(
            binding.binding_mode,
            HlsOriginAccountBindingMode::Speculative { reclaim_until_ms, .. } if now_ms <= reclaim_until_ms
        ) {
            speculative_accounts.push(Arc::clone(&binding.account_name));
        }
    }

    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if session_guard.proxy_session_id == *new_proxy_session_id {
            continue;
        }
        let Some(binding) = session_guard.origin_account_binding.as_ref() else {
            continue;
        };
        if binding.input_name != *input_name
            || speculative_accounts.iter().any(|account| account == &binding.account_name)
        {
            continue;
        }
        if !matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active) {
            continue;
        }
        if session_guard.activity.active_origin_work_count > 0 {
            continue;
        }
        let timing = session_guard.account_overlap_timing();
        let protection = session_guard.account_binding_protection(now_ms);
        debug!(
            "HLS account protection classified: session={} state={} target_duration_ms={}",
            safe_proxy_session_id(&session_guard.proxy_session_id),
            protection.as_log_state(),
            timing.target_duration_ms
        );
        let HlsAccountBindingProtection::SoftActive { reclaim_until_ms } = protection else {
            continue;
        };
        let last_media_at_ms = session_guard.activity.last_authorized_media_at_ms.unwrap_or_default();
        candidates.push(HlsAccountOverlapCandidate {
            proxy_session_id: session_guard.proxy_session_id.clone(),
            account_name: Arc::clone(&binding.account_name),
            session_owner: binding.session_owner.clone(),
            reclaim_until_ms,
            last_media_at_ms,
        });
    }
    candidates.sort_by_key(|candidate| candidate.last_media_at_ms);
    candidates.into_iter().next()
}

async fn reclaim_hls_account_overlap_if_needed(
    app_state: &Arc<AppState>,
    winner_session: &HlsSessionHandle,
    now_ms: u64,
) {
    let winner_proxy_session_id = winner_session.read().await.proxy_session_id.clone();
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let (loser_proxy_session_id, loser_binding) = {
            let session_guard = session.read().await;
            let Some(binding) = session_guard.origin_account_binding.clone() else {
                continue;
            };
            let HlsOriginAccountBindingMode::Speculative { displaced_proxy_session_id, reclaim_until_ms } =
                &binding.binding_mode
            else {
                continue;
            };
            if displaced_proxy_session_id != &winner_proxy_session_id || now_ms > *reclaim_until_ms {
                continue;
            }
            (session_guard.proxy_session_id.clone(), binding)
        };
        app_state.active_provider.clear_provider_reservation(&loser_binding.session_owner).await;
        {
            let mut loser = session.write().await;
            if let Some(binding) = loser.origin_account_binding.as_mut() {
                binding.detach(HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner, now_ms);
            }
            loser.invalidate_queued_origin_work();
        }
        {
            let mut winner = winner_session.write().await;
            if let Some(binding) = winner.origin_account_binding.as_mut() {
                binding.promote_to_active();
            }
        }
        debug!(
            "HLS account overlap reclaimed: account={} winner={} loser={}",
            sanitize_sensitive_info(loser_binding.account_name.as_ref()),
            safe_proxy_session_id(&winner_proxy_session_id),
            safe_proxy_session_id(&loser_proxy_session_id)
        );
        debug!(
            "HLS origin binding detached: session={} account={} reason={}",
            safe_proxy_session_id(&loser_proxy_session_id),
            sanitize_sensitive_info(loser_binding.account_name.as_ref()),
            HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner.as_log_reason()
        );
    }
}

async fn promote_elapsed_hls_account_overlaps(app_state: &Arc<AppState>, now_ms: u64) {
    let sessions = app_state.hls_proxy.sessions().list_sessions().await;
    for session in sessions {
        let (account_name, promoted_session_id, displaced_session_id) = {
            let mut session_guard = session.write().await;
            let Some(binding) = session_guard.origin_account_binding.as_mut() else {
                continue;
            };
            let HlsOriginAccountBindingMode::Speculative { displaced_proxy_session_id, reclaim_until_ms } =
                &binding.binding_mode
            else {
                continue;
            };
            if now_ms <= *reclaim_until_ms {
                continue;
            }
            let displaced_session_id = displaced_proxy_session_id.clone();
            let account_name = Arc::clone(&binding.account_name);
            binding.promote_to_active();
            (account_name, session_guard.proxy_session_id.clone(), displaced_session_id)
        };
        if let Some(displaced) = app_state.hls_proxy.sessions().get_by_proxy_session_id(&displaced_session_id).await {
            let mut detached = false;
            let mut displaced = displaced.write().await;
            if displaced.origin_account_binding.as_ref().is_some_and(|binding| binding.account_name == account_name) {
                if let Some(binding) = displaced.origin_account_binding.as_mut() {
                    binding.detach(HlsOriginAccountDetachedReason::SoftWindowElapsed, now_ms);
                    detached = true;
                }
                displaced.invalidate_queued_origin_work();
            }
            if detached {
                debug!(
                    "HLS origin binding detached: session={} account={} reason={}",
                    safe_proxy_session_id(&displaced_session_id),
                    sanitize_sensitive_info(account_name.as_ref()),
                    HlsOriginAccountDetachedReason::SoftWindowElapsed.as_log_reason()
                );
            }
        }
        debug!(
            "HLS account overlap promoted: account={} session={}",
            sanitize_sensitive_info(account_name.as_ref()),
            safe_proxy_session_id(&promoted_session_id)
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn rebind_hls_origin_account(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    stale_binding: &HlsOriginAccountBinding,
    stale_status: HlsOriginAccountStatus,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    {
        let mut session_guard = session.write().await;
        if !session_guard.origin_account_rebind.is_allowed_now(now_ms) {
            debug!(
                "HLS origin account rebind skipped by backoff: session={} old_account={} retry_after_ms=2000",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref())
            );
            return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
        }
        session_guard.origin_account_rebind.mark_attempt_started(Arc::clone(&stale_binding.account_name), now_ms);
    }

    let safe_session_id = {
        let session_guard = session.read().await;
        safe_proxy_session_id(&session_guard.proxy_session_id)
    };
    debug!(
        "HLS origin account rebind started: session={} old_account={} reason={stale_status:?}",
        safe_session_id,
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
    app_state.active_provider.clear_provider_reservation(&stale_binding.session_owner).await;
    {
        let mut session_guard = session.write().await;
        if let Some(binding) = session_guard.origin_account_binding.as_mut().filter(|binding| {
            binding.account_name == stale_binding.account_name && binding.session_owner == stale_binding.session_owner
        }) {
            binding.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, now_ms);
            debug!(
                "HLS origin binding detached: session={} account={} reason={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref()),
                HlsOriginAccountDetachedReason::AccountMissingOrExpired.as_log_reason()
            );
        }
        session_guard.invalidate_queued_origin_work();
    }

    let Some(provider_handle) = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            false,
            priority,
            connection_kind,
            Some(&stale_binding.session_owner),
        )
        .await
    else {
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_account_available").await;
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable);
    };

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_provider_config").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(fetch_url) = build_hls_origin_fetch_url(input, raw_request_url, session_entry_url, Some(&provider_config))
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "invalid_origin_url").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(new_account_name) = provider_handle.allocation.get_provider_name() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "missing_account_name").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let new_binding = HlsOriginAccountBinding::rebound(
        Arc::clone(&input.name),
        new_account_name,
        stale_binding.session_owner.clone(),
        stale_binding.generation.saturating_add(1),
        now_ms,
    );
    {
        let mut session_guard = session.write().await;
        session_guard.origin_account_binding = Some(new_binding.clone());
        session_guard.origin_account_rebind.mark_success();
    }
    debug!(
        "HLS origin account rebound: old_account={} new_account={}",
        sanitize_sensitive_info(stale_binding.account_name.as_ref()),
        sanitize_sensitive_info(new_binding.account_name.as_ref())
    );

    Ok(PreparedHlsOriginRuntime {
        failover_provider: input.get_resolve_provider(&fetch_url),
        fetch_url,
        binding_to_store: None,
        preacquired_provider_handle: Some(provider_handle),
    })
}

async fn mark_hls_origin_rebind_failed(
    session: &HlsSessionHandle,
    stale_binding: &HlsOriginAccountBinding,
    now_ms: u64,
    reason: &str,
) {
    let mut session_guard = session.write().await;
    session_guard.origin_account_rebind.mark_failed(now_ms);
    debug!(
        "HLS origin account rebind failed: session={} old_account={} reason={reason} retry_after_ms=2000",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
}

#[allow(clippy::too_many_arguments)]
async fn prepare_hls_cache_user_session(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    session_token: &str,
    virtual_id: u32,
    request_url: &str,
    input: &ConfigInput,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
) -> String {
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user,
            session_token,
            virtual_id,
            provider: input.name.as_ref(),
            stream_url: request_url,
            addr: &fingerprint.addr,
            connection_permission,
            connection_kind: Some(connection_kind),
            socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
        })
        .await
}

#[allow(clippy::too_many_arguments)]
async fn try_hls_cache_entry_redirect(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    origin_source: HlsOriginSource,
    virtual_id: u32,
    _existing_user_session: Option<&UserSession>,
    request_url: &str,
    input: &ConfigInput,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    server_path: Option<&str>,
) -> Option<axum::response::Response> {
    if !hls_cache_enabled(app_state) {
        return None;
    }

    let session_key = origin_source.session_key();
    let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    let family_key = HlsPlaybackFamilyKey::new(user.username.clone(), fingerprint.key.clone());
    let now_ms = current_time_millis();
    let reuse_result = app_state.hls_proxy.find_reusable_access_lease(&family_key, &proxy_session_id, now_ms).await;
    let (access_lease_id, _session_token) = if let AccessLeaseReuseResult::Reusable(lease) = reuse_result {
        let session_token = prepare_hls_cache_user_session(
            app_state,
            fingerprint,
            user,
            &lease.user_session_token,
            virtual_id,
            request_url,
            input,
            connection_permission,
            connection_kind,
        )
        .await;
        debug!(
            "HLS access lease prepared: lease={} proxy_session={} session={} action=reused",
            safe_hls_access_lease_id(&lease.lease_id),
            safe_proxy_session_id(&proxy_session_id),
            safe_user_session_token(&session_token)
        );
        (lease.lease_id, session_token)
    } else {
        if let AccessLeaseReuseResult::NotReusable { lease_id, reason, state, age_ms } = reuse_result {
            if reason.as_log_reason() != "none" {
                let reason = hls_access_lease_reuse_skip_log_reason(reason, state);
                let state = state.map_or("<none>", HlsAccessLeaseState::as_log_value);
                let age_ms = age_ms.map_or_else(|| "<none>".to_string(), |age_ms| age_ms.to_string());
                debug!(
                    "HLS access lease reuse skipped: lease={} proxy_session={} reason={} state={} age_ms={}",
                    lease_id.as_ref().map_or_else(|| "<unknown>".to_string(), safe_hls_access_lease_id),
                    safe_proxy_session_id(&proxy_session_id),
                    reason,
                    state,
                    age_ms
                );
            }
        }
        let access_lease_id = new_hls_access_lease_id();
        let session_token = create_hls_cache_user_session_token(fingerprint, &user.username, virtual_id);
        let session_token = prepare_hls_cache_user_session(
            app_state,
            fingerprint,
            user,
            &session_token,
            virtual_id,
            request_url,
            input,
            connection_permission,
            connection_kind,
        )
        .await;
        app_state
            .hls_proxy
            .prepare_access_lease(
                HlsAccessLease::pending(
                    access_lease_id.clone(),
                    family_key,
                    proxy_session_id.clone(),
                    user.username.clone(),
                    session_token.clone(),
                    origin_source.input_id,
                    origin_source.stream_ref.clone(),
                    virtual_id,
                    now_ms,
                    hls_access_lease_ttl_ms(app_state),
                )
                .with_origin_acquire_policy(connection_kind, connection_priority_for_kind(user, connection_kind)),
            )
            .await;
        debug!(
            "HLS access lease prepared: lease={} proxy_session={} session={} action=created reason=new-playback",
            safe_hls_access_lease_id(&access_lease_id),
            safe_proxy_session_id(&proxy_session_id),
            safe_user_session_token(&session_token)
        );
        (access_lease_id, session_token)
    };
    Some(hls_canonical_manifest_redirect(&proxy_session_id, &access_lease_id, server_path))
}

fn hls_canonical_manifest_redirect(
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    server_path: Option<&str>,
) -> axum::response::Response {
    let path_prefix = normalize_hls_proxy_public_path_prefix(server_path).unwrap_or_default();
    let location = format!("{path_prefix}{}", hls_canonical_manifest_path(proxy_session_id, access_lease_id));
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::TEMPORARY_REDIRECT)
        .header(header::LOCATION, location)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::empty()))
}

fn hls_canonical_manifest_path(proxy_session_id: &ProxySessionId, access_lease_id: &HlsAccessLeaseId) -> String {
    format!("/proxy/hls/live/{}/{}/manifest.m3u8", proxy_session_id.0, access_lease_id.0)
}

fn hls_canonical_retry_after_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        .header(axum::http::header::RETRY_AFTER, cold_start_retry_after_seconds().to_string())
        .body(axum::body::Body::empty()))
}

fn hls_canonical_status_response(status: StatusCode) -> axum::response::Response {
    if status == StatusCode::SERVICE_UNAVAILABLE {
        hls_canonical_retry_after_response()
    } else {
        status.into_response()
    }
}

struct HlsEntryOriginAccountReservation {
    request_url: String,
    session_token: String,
    provider_handle: Option<ProviderHandle>,
    selected_provider_config: Option<Arc<RuntimeProviderConfig>>,
}

#[allow(clippy::too_many_arguments)]
async fn try_reserve_hls_entry_origin_account_for_redirect(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    virtual_id: u32,
    request_url: &str,
    user_session_token: &str,
    session_owner: &str,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    create_user_session: bool,
) -> Option<HlsEntryOriginAccountReservation> {
    let provider_handle = app_state
        .active_provider
        .acquire_connection_with_grace_for_session(
            &input.name,
            &fingerprint.addr,
            false,
            connection_priority_for_kind(user, connection_kind),
            connection_kind,
            Some(session_owner),
        )
        .await?;

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return None;
    };
    let Some(stream_url) = get_stream_alternative_url(request_url, input, &provider_config) else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return None;
    };

    let session_token = if create_user_session {
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token: user_session_token,
                virtual_id,
                provider: &provider_config.name,
                stream_url: &stream_url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(connection_kind),
                socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
            })
            .await
    } else {
        user_session_token.to_string()
    };

    app_state
        .active_provider
        .refresh_provider_reservation(&provider_config.name, session_owner, get_hls_session_ttl_secs(app_state))
        .await;

    Some(HlsEntryOriginAccountReservation {
        request_url: stream_url,
        session_token,
        provider_handle: Some(provider_handle),
        selected_provider_config: Some(provider_config),
    })
}

#[allow(clippy::too_many_arguments)]
async fn try_reserve_hls_virtual_entry_origin_account_for_redirect(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    virtual_id: u32,
) -> bool {
    let session_token =
        create_playback_session_fingerprint(fingerprint, &user.username, virtual_id, PlaylistItemType::LiveHls, None);
    let (connection_admission, _, _) = resolve_playback_request_admission(
        app_state,
        user,
        fingerprint,
        PlaylistItemType::LiveHls,
        None,
        &session_token,
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id },
        false,
        false,
    )
    .await;
    if connection_admission.permission == UserConnectionPermission::Exhausted {
        return false;
    }

    let Some(channel) = get_stream_channel(app_state, target, virtual_id).await else {
        return false;
    };
    let Ok(origin_playlist_url) =
        resolve_hls_origin_playlist_url(app_state, target, input, virtual_id, channel.url.as_ref()).await
    else {
        return false;
    };
    let Some(hls_cache_origin) = build_hls_origin_resolution(input, &origin_playlist_url) else {
        return false;
    };
    let connection_kind = connection_admission.kind.unwrap_or(crate::api::model::ConnectionKind::Normal);
    let shared_hls_session_owner = if hls_cache_enabled(app_state) {
        let origin_source = build_hls_origin_source(input, hls_stream_ref_from_virtual_id(virtual_id));
        let proxy_session_id = build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());
        Some(build_hls_origin_session_owner(&proxy_session_id))
    } else {
        None
    };
    let session_owner = shared_hls_session_owner.as_deref().unwrap_or(session_token.as_str());

    let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
        app_state,
        fingerprint,
        user,
        input,
        virtual_id,
        &hls_cache_origin.session_entry_url,
        &session_token,
        session_owner,
        connection_admission.permission,
        connection_kind,
        false,
    )
    .await
    else {
        return false;
    };

    app_state.connection_manager.release_provider_handle(reservation.provider_handle).await;
    true
}

async fn mark_hls_provisioning_handoff_discontinuity(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: Option<&HlsAccessLeaseId>,
    now_ms: u64,
) -> bool {
    if !hls_cache_enabled(app_state) {
        return false;
    }
    let origin_source = build_hls_origin_source(input, hls_stream_ref_from_virtual_id(virtual_id));
    let Some(session) = app_state.hls_proxy.sessions().get_by_key(&origin_source.session_key()).await else {
        return false;
    };
    mark_hls_provisioning_handoff_discontinuity_once_for_session(
        app_state,
        &session,
        input,
        virtual_id,
        access_lease_id,
        now_ms,
    )
    .await
}

async fn mark_hls_provisioning_handoff_discontinuity_once_for_session(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: Option<&HlsAccessLeaseId>,
    now_ms: u64,
) -> bool {
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    if !app_state.hls_provisioning.mark_handoff_once(
        &input.name,
        virtual_id,
        Some(&proxy_session_id),
        access_lease_id,
        now_ms,
    ) {
        debug!(
            "HLS provisioning handoff discontinuity already marked: proxy_session_id={}",
            safe_proxy_session_id(&proxy_session_id)
        );
        return false;
    }
    mark_hls_provisioning_handoff_discontinuity_for_session(session, now_ms).await;
    true
}

async fn mark_hls_provisioning_handoff_discontinuity_for_session(session: &HlsSessionHandle, now_ms: u64) {
    let discontinuity_sequence = hls_provisioning_discontinuity_sequence(now_ms);
    let proxy_session_id = {
        let mut session = session.write().await;
        session.mark_pending_handoff_discontinuity(discontinuity_sequence);
        session.proxy_session_id.clone()
    };
    debug!(
        "HLS provisioning handoff discontinuity marked: proxy_session_id={} discontinuity_sequence={}",
        safe_proxy_session_id(&proxy_session_id),
        discontinuity_sequence
    );
}

fn clear_hls_provisioning_handoff_consumer(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    virtual_id: u32,
    now_ms: u64,
) {
    if !app_state.hls_provisioning.take_ready_slot_for_consumer(&input.name, virtual_id, now_ms) {
        app_state.hls_provisioning.clear_consumer(&input.name, virtual_id);
    }
}

async fn maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> Option<u64> {
    if !app_state.hls_provisioning.has_consumer(&input.name, virtual_id, now_ms) {
        return None;
    }
    let previous_manifest_rendered_at_ms = latest_shared_hls_manifest_rendered_at_ms(session).await;
    mark_hls_provisioning_handoff_discontinuity_once_for_session(
        app_state,
        session,
        input,
        virtual_id,
        Some(access_lease_id),
        now_ms,
    )
    .await
    .then_some(previous_manifest_rendered_at_ms)
}

async fn latest_shared_hls_manifest_rendered_at_ms(session: &HlsSessionHandle) -> u64 {
    let session = session.read().await;
    session
        .last_rendered_manifest
        .as_ref()
        .map_or(0, |rendered| rendered.rendered_at_ms)
        .max(session.transient.last_manifest_rendered_at_ms.unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api) async fn hls_panel_provisioning_poll_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    virtual_id: u32,
    original_hls_entry_path: &str,
    server_path: Option<&str>,
) -> axum::response::Response {
    hls_panel_provisioning_poll_response(
        app_state,
        fingerprint,
        user,
        target,
        input,
        virtual_id,
        original_hls_entry_path,
        server_path,
        HlsProvisioningPollResponseKind::Legacy,
    )
    .await
}

enum HlsProvisioningPollResponseKind<'a> {
    Legacy,
    Shared { proxy_session_id: &'a ProxySessionId, access_lease_id: &'a HlsAccessLeaseId },
}

impl<'a> HlsProvisioningPollResponseKind<'a> {
    fn access_lease_id(&self) -> Option<&'a HlsAccessLeaseId> {
        match self {
            Self::Legacy => None,
            Self::Shared { access_lease_id, .. } => Some(*access_lease_id),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn hls_panel_provisioning_poll_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    virtual_id: u32,
    ready_redirect_path: &str,
    server_path: Option<&str>,
    response_kind: HlsProvisioningPollResponseKind<'_>,
) -> axum::response::Response {
    let now_ms = current_time_millis();
    app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), virtual_id, now_ms);

    let existing_status = app_state.hls_provisioning.consumer_status(&input.name, virtual_id, now_ms);

    if try_reserve_hls_virtual_entry_origin_account_for_redirect(
        app_state,
        fingerprint,
        user,
        target,
        input,
        virtual_id,
    )
    .await
    {
        mark_hls_provisioning_handoff_discontinuity(
            app_state,
            input,
            virtual_id,
            response_kind.access_lease_id(),
            now_ms,
        )
        .await;
        clear_hls_provisioning_handoff_consumer(app_state, input, virtual_id, current_time_millis());
        return hls_virtual_entry_redirect_response(ready_redirect_path, server_path);
    }

    let provisioning_enabled = can_provision_on_exhausted(app_state.as_ref(), input);
    if provisioning_enabled {
        start_hls_panel_provisioning_once(app_state, input);
    }

    let status = existing_status.unwrap_or(if provisioning_enabled {
        HlsProvisioningStatus::InProgress
    } else {
        HlsProvisioningStatus::ProviderExhausted
    });

    match status {
        HlsProvisioningStatus::Ready | HlsProvisioningStatus::InProgress => match response_kind {
            HlsProvisioningPollResponseKind::Legacy => hls_custom_video_manifest_response_with_virtual_id(
                app_state,
                user,
                CustomVideoStreamType::Provisioning,
                StatusCode::SERVICE_UNAVAILABLE,
                Some(virtual_id),
            ),
            HlsProvisioningPollResponseKind::Shared { proxy_session_id, access_lease_id } => {
                hls_shared_panel_provisioning_manifest_response(
                    app_state,
                    user,
                    proxy_session_id,
                    access_lease_id,
                    StatusCode::SERVICE_UNAVAILABLE,
                )
            }
        },
        HlsProvisioningStatus::ProviderExhausted => hls_custom_video_manifest_response_with_virtual_id(
            app_state,
            user,
            CustomVideoStreamType::ProviderConnectionsExhausted,
            StatusCode::SERVICE_UNAVAILABLE,
            Some(virtual_id),
        ),
    }
}

async fn validate_hls_shared_panel_provisioning_context(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
) -> Result<HlsAccessContext, axum::response::Response> {
    let now_ms = current_time_millis();
    let context = validate_hls_access_lease(
        app_state,
        fingerprint,
        proxy_session_id,
        access_lease_id,
        now_ms,
        HlsAccessAdmissionMode::ManifestPrepare,
    )
    .await
    .map_err(hls_access_lease_validation_response)?;
    match app_state
        .hls_proxy
        .touch_manifest_access_lease(
            access_lease_id,
            proxy_session_id,
            now_ms,
            None,
            hls_access_lease_ttl_ms(app_state),
        )
        .await
    {
        HlsAccessLeaseTouch::Touched { .. } => Ok(context),
        HlsAccessLeaseTouch::Denied => {
            Err(hls_access_lease_validation_response(HlsAccessLeaseValidationError::AdmissionDenied))
        }
        HlsAccessLeaseTouch::Expired | HlsAccessLeaseTouch::UnknownLease | HlsAccessLeaseTouch::SessionMismatch => {
            Err(hls_access_lease_validation_response(HlsAccessLeaseValidationError::Expired))
        }
    }
}

pub(in crate::api) async fn validate_hls_shared_panel_provisioning_segment_access(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &str,
    access_lease_id: &str,
) -> Result<(), axum::response::Response> {
    let proxy_session_id = ProxySessionId(proxy_session_id.to_string());
    prepare_hls_resource_access(
        app_state,
        fingerprint,
        &proxy_session_id,
        access_lease_id,
        current_time_millis(),
        "provisioning_segment",
    )
    .await
    .map(|_| ())
}

pub(in crate::api) async fn hls_shared_panel_provisioning_poll_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    proxy_session_id: &str,
    access_lease_id: &str,
    virtual_id: u32,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(proxy_session_id.to_string());
    let access_lease_id = HlsAccessLeaseId(access_lease_id.to_string());
    let context = match validate_hls_shared_panel_provisioning_context(
        app_state,
        fingerprint,
        &proxy_session_id,
        &access_lease_id,
    )
    .await
    {
        Ok(context) => context,
        Err(response) => return response,
    };
    if context.virtual_id != virtual_id {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some((user, target)) = app_state.app_config.get_target_for_username(&context.username) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(input) = resolve_hls_virtual_input_for_target(app_state, &target, virtual_id).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if input.id != context.input_id {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ready_redirect_path = hls_canonical_manifest_path(&proxy_session_id, &access_lease_id);
    let server_path = app_state.app_config.get_user_server_info(&user).and_then(|server| server.path);
    hls_panel_provisioning_poll_response(
        app_state,
        fingerprint,
        &user,
        &target,
        &input,
        virtual_id,
        &ready_redirect_path,
        server_path.as_deref(),
        HlsProvisioningPollResponseKind::Shared {
            proxy_session_id: &proxy_session_id,
            access_lease_id: &access_lease_id,
        },
    )
    .await
}

async fn hls_panel_provisioning_or_status_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    virtual_id: u32,
    original_hls_entry_path: &str,
    server_path: Option<&str>,
    fallback_status: StatusCode,
) -> axum::response::Response {
    try_hls_panel_provisioning_manifest_response(
        app_state,
        user,
        input,
        virtual_id,
        HlsPanelProvisioningRedirectPaths {
            ready_entry_path: Some(original_hls_entry_path),
            waiting_manifest_path: None,
        },
        server_path,
        fallback_status,
    )
    .await
    .unwrap_or_else(|| fallback_status.into_response())
}

async fn hls_panel_provisioning_or_retry_after_response(
    app_state: &Arc<AppState>,
    username: &str,
    input: &ConfigInput,
    virtual_id: u32,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    server_path: Option<&str>,
) -> axum::response::Response {
    if let Some((user, _target)) = app_state.app_config.get_target_for_username(username) {
        let provisioning_manifest_path =
            hls_shared_panel_provisioning_manifest_path(proxy_session_id, access_lease_id, virtual_id);
        let ready_redirect_path = hls_canonical_manifest_path(proxy_session_id, access_lease_id);
        if let Some(response) = try_hls_panel_provisioning_manifest_response(
            app_state,
            &user,
            input,
            virtual_id,
            HlsPanelProvisioningRedirectPaths {
                ready_entry_path: Some(&ready_redirect_path),
                waiting_manifest_path: Some(&provisioning_manifest_path),
            },
            server_path,
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await
        {
            return response;
        }
    }
    hls_canonical_retry_after_response()
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn try_hls_cache_canonical_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    context: &HlsAccessContext,
    path_proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    origin: HlsCacheManifestOrigin<'_>,
    headers: HeaderMap,
    hls_session_ttl_secs: u64,
    server_path: Option<&str>,
    _original_hls_entry_path: &str,
) -> Option<axum::response::Response> {
    if !hls_cache_enabled(app_state) {
        return None;
    }
    if origin.origin_source.input_id != context.input_id || origin.origin_source.stream_ref != context.stream_ref {
        return Some(StatusCode::NOT_FOUND.into_response());
    }

    let session_key = origin.origin_source.session_key();
    let expected_proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
    if &expected_proxy_session_id != path_proxy_session_id {
        return Some(StatusCode::NOT_FOUND.into_response());
    }
    let now_ms = current_time_millis();
    let rewrite_secret = app_state.get_encrypt_secret();
    let (session, session_outcome) = app_state
        .hls_proxy
        .get_or_create_session_with_source_and_outcome(
            session_key,
            origin.origin_source.clone(),
            &rewrite_secret,
            now_ms,
        )
        .await;
    if access_lease_state == HlsAccessLeaseState::Activated {
        let timing = hls_access_lease_timing_for_session(app_state, &session).await;
        let _ = app_state
            .hls_proxy
            .touch_manifest_access_lease(
                access_lease_id,
                path_proxy_session_id,
                now_ms,
                Some(timing),
                hls_access_lease_ttl_ms(app_state),
            )
            .await;
    }
    app_state
        .hls_proxy
        .sync_session_access_lease_count_and_detach_if_needed(
            &app_state.active_users,
            &app_state.active_provider,
            &session,
            path_proxy_session_id,
            now_ms,
        )
        .await;
    let origin_policy = hls_effective_origin_acquire_policy(&session).await;
    let prepared_origin = match prepare_hls_origin_runtime(
        app_state,
        &session,
        origin.input,
        origin.raw_request_url,
        origin.session_entry_url,
        path_proxy_session_id,
        fingerprint,
        origin_policy.connection_kind,
        origin_policy.priority,
        HlsOriginWorkKind::Manifest,
        HlsOriginWorkClass::ManifestInteractive,
        now_ms,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(HlsOriginRuntimeAcquireError::NoAccountAvailable) => {
            return Some(
                hls_panel_provisioning_or_retry_after_response(
                    app_state,
                    &context.username,
                    origin.input,
                    context.virtual_id,
                    path_proxy_session_id,
                    access_lease_id,
                    server_path,
                )
                .await,
            );
        }
        Err(HlsOriginRuntimeAcquireError::Fatal(status)) => return Some(hls_canonical_status_response(status)),
    };
    let origin_entry = LiveHlsOriginEntry::parse_with_provider(
        &prepared_origin.fetch_url,
        prepared_origin.failover_provider.or_else(|| origin.failover_provider.clone()),
    )?;
    {
        let mut session_guard = session.write().await;
        if session_guard.is_gc_marked_for_removal() {
            return Some(hls_canonical_retry_after_response());
        }
        if prepared_origin.binding_to_store.is_some() {
            session_guard.origin_account_binding = prepared_origin.binding_to_store;
        }
    }
    mark_hls_authorized_manifest_access(app_state, &session, now_ms).await;
    let selected_account = session.read().await.origin_account_binding.as_ref().map_or_else(
        || "<none>".to_string(),
        |binding| sanitize_sensitive_info(binding.account_name.as_ref()).to_string(),
    );
    debug!(
        "HLS origin account selected: proxy_session_id={} account={} url_failover={}",
        safe_proxy_session_id(path_proxy_session_id),
        selected_account,
        if origin.failover_provider.is_some() { "enabled" } else { "disabled" }
    );
    let handoff_previous_rendered_at_ms = maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
        app_state,
        &session,
        origin.input,
        context.virtual_id,
        access_lease_id,
        now_ms,
    )
    .await;

    let mut origin_io = HlsOriginIoContext {
        app_state: Arc::clone(app_state),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::ManifestInteractive.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs: hls_session_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
    };
    if let Some(provider_handle) = prepared_origin.preacquired_provider_handle {
        origin_io = origin_io.with_preacquired_provider_handle(provider_handle);
    }

    let origin_input_source = origin_entry.to_input_source();
    let refresh_request = OriginRefreshRequest {
        app_config: Arc::clone(&app_state.app_config),
        session: Arc::clone(&session),
        origin_entry,
        origin_input_source,
        headers,
        client: app_state.http_client.load().as_ref().clone(),
        no_redirect_client: app_state.http_client_no_redirect.load().as_ref().clone(),
        use_manual_redirects: app_state.should_use_manual_redirects(),
        segment_cache: Arc::clone(app_state.hls_proxy.segment_cache()),
        segment_repair: Arc::clone(app_state.hls_proxy.segment_repair()),
        segment_worker_pool: Arc::clone(app_state.hls_proxy.segment_worker_pool()),
        map_worker_pool: Arc::clone(app_state.hls_proxy.map_worker_pool()),
        origin_manifest_timeout_ms: app_state.hls_proxy.origin_manifest_timeout_ms(),
        strip: app_state.hls_proxy.strip().clone(),
        retry_policy: RetryPolicy::default(),
        reverse_proxy_rewrite_secret: rewrite_secret.to_vec(),
        transient_resource_ttl_ms: app_state.hls_proxy.transient_resource_ttl_ms(),
        disabled_headers: app_state.get_disabled_headers(),
        now_ms,
        origin_io: Some(origin_io),
    };
    if let Some(previous_rendered_at_ms) = handoff_previous_rendered_at_ms {
        let wait_timeout = hls_manifest_commit_wait_timeout(app_state);
        let _started = maybe_trigger_origin_refresh(refresh_request).await;
        let strip = app_state.hls_proxy.strip();
        if let Some(response) = try_hls_cached_manifest_response(
            app_state,
            &session,
            access_lease_id,
            access_lease_state,
            &strip,
            server_path,
            HlsCachedManifestOptions::initial(wait_timeout).requiring_newer_manifest(previous_rendered_at_ms),
        )
        .await
        {
            clear_hls_provisioning_handoff_consumer(app_state, origin.input, context.virtual_id, current_time_millis());
            return Some(response);
        }
        return Some(hls_canonical_retry_after_response());
    }
    match session_outcome {
        HlsSessionStoreOutcome::Created => {
            let _started = maybe_trigger_origin_refresh(refresh_request).await;
            let strip = app_state.hls_proxy.strip();
            if let Some(response) = try_hls_cached_manifest_response(
                app_state,
                &session,
                access_lease_id,
                access_lease_state,
                &strip,
                server_path,
                HlsCachedManifestOptions::initial(hls_manifest_commit_wait_timeout(app_state)),
            )
            .await
            {
                return Some(response);
            }
        }
        HlsSessionStoreOutcome::Reused => {
            let _started = maybe_trigger_origin_refresh(refresh_request).await;
            let wait_timeout =
                hls_initial_manifest_wait_timeout(&session, hls_manifest_commit_wait_timeout(app_state)).await;
            let strip = app_state.hls_proxy.strip();
            if let Some(response) = try_hls_cached_manifest_response(
                app_state,
                &session,
                access_lease_id,
                access_lease_state,
                &strip,
                server_path,
                HlsCachedManifestOptions::initial(wait_timeout),
            )
            .await
            {
                return Some(response);
            }
        }
    }

    Some(hls_canonical_retry_after_response())
}

fn hls_manifest_commit_wait_timeout(app_state: &Arc<AppState>) -> Duration {
    Duration::from_millis(app_state.hls_proxy.origin_manifest_timeout_ms().max(1).saturating_add(250))
}

async fn hls_initial_manifest_wait_timeout(session: &HlsSessionHandle, wait_timeout: Duration) -> Duration {
    let session = session.read().await;
    if matches!(
        session.account_binding_protection(current_time_millis()),
        HlsAccountBindingProtection::NoMediaYet | HlsAccountBindingProtection::Expired
    ) {
        wait_timeout
    } else {
        Duration::ZERO
    }
}

async fn hls_segment_request_requires_origin_work(session: &HlsSessionHandle, segment_file: &HlsSegmentFile) -> bool {
    let session = session.read().await;
    let Some(entry) = session.segments.get(&segment_file.proxy_seq) else {
        return false;
    };
    if entry.proxy_file_ext != segment_file.extension {
        return false;
    }
    matches!(entry.status, SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. })
        && entry.origin_fetch_ref.is_some()
}

async fn hls_origin_binding_needs_reacquire(session: &HlsSessionHandle) -> bool {
    let session = session.read().await;
    session.origin_account_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached)
}

fn hls_transient_origin_binding_requires_runtime_prepare(
    app_state: &Arc<AppState>,
    binding: &HlsOriginAccountBinding,
) -> bool {
    binding.is_detached()
        || (binding.is_active()
            && matches!(
                hls_origin_account_status(app_state, binding),
                HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired
            ))
}

async fn prepare_hls_origin_binding_for_authorized_resource_work(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    work_kind: HlsOriginWorkKind,
    now_ms: u64,
) -> Result<Option<ProviderHandle>, StatusCode> {
    if !hls_origin_binding_needs_reacquire(session).await {
        return Ok(None);
    }
    if session.read().await.activity.active_origin_work_count > 0 {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let request_context = resolve_hls_playback_manifest_request_context(app_state, access_context, req_headers).await?;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let origin_policy = hls_effective_origin_acquire_policy(session).await;
    let prepared_origin = prepare_hls_origin_runtime(
        app_state,
        session,
        &request_context.input,
        &request_context.hls_url,
        &request_context.session_entry_url,
        &proxy_session_id,
        fingerprint,
        origin_policy.connection_kind,
        origin_policy.priority,
        work_kind,
        HlsOriginWorkClass::Demand,
        now_ms,
    )
    .await
    .map_err(|err| err.status_code())?;
    if let Some(binding) = prepared_origin.binding_to_store {
        session.write().await.origin_account_binding = Some(binding);
    }
    Ok(prepared_origin.preacquired_provider_handle)
}

async fn prepare_hls_transient_origin_io_for_authorized_resource_work(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_context: &HlsAccessContext,
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    now_ms: u64,
) -> Result<Option<TransientOriginIoGuard>, StatusCode> {
    let existing_binding = session.read().await.origin_account_binding.clone();
    let origin_policy = hls_effective_origin_acquire_policy(session).await;
    if let Some(binding) = existing_binding.as_ref().filter(|binding| binding.is_active()) {
        match hls_origin_account_status(app_state, binding) {
            HlsOriginAccountStatus::Known => {
                let origin_io = HlsOriginIoContext {
                    app_state: Arc::clone(app_state),
                    client_addr: fingerprint.addr,
                    allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
                    priority: origin_policy.priority,
                    connection_kind: origin_policy.connection_kind,
                    reservation_ttl_secs: get_hls_session_ttl_secs(app_state),
                    preacquired_provider_handle: None,
                    started_generation: None,
                };
                let started_generation = session.write().await.start_origin_work();
                if let Ok(lease_guard) = begin_hls_origin_account_io(&origin_io, session, binding).await {
                    return Ok(Some(TransientOriginIoGuard::new(
                        Arc::clone(session),
                        origin_io,
                        lease_guard,
                        started_generation,
                    )));
                }
                session.write().await.finish_origin_work(started_generation);
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            HlsOriginAccountStatus::Missing | HlsOriginAccountStatus::Expired => {}
        }
    }

    if !existing_binding
        .as_ref()
        .is_some_and(|binding| hls_transient_origin_binding_requires_runtime_prepare(app_state, binding))
    {
        return Ok(None);
    }
    if session.read().await.activity.active_origin_work_count > 0 {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let request_context = resolve_hls_playback_manifest_request_context(app_state, access_context, req_headers).await?;
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let prepared_origin = prepare_hls_origin_runtime(
        app_state,
        session,
        &request_context.input,
        &request_context.hls_url,
        &request_context.session_entry_url,
        &proxy_session_id,
        fingerprint,
        origin_policy.connection_kind,
        origin_policy.priority,
        HlsOriginWorkKind::Resource,
        HlsOriginWorkClass::Demand,
        now_ms,
    )
    .await
    .map_err(|err| err.status_code())?;
    if let Some(binding) = prepared_origin.binding_to_store {
        session.write().await.origin_account_binding = Some(binding);
    }
    let Some(provider_handle) = prepared_origin.preacquired_provider_handle else {
        return Ok(None);
    };
    let Some(binding) = session.read().await.origin_account_binding.clone().filter(HlsOriginAccountBinding::is_active)
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle)).await;
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let origin_io = HlsOriginIoContext {
        app_state: Arc::clone(app_state),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::Demand.allows_grace(),
        priority: origin_policy.priority,
        connection_kind: origin_policy.connection_kind,
        reservation_ttl_secs: get_hls_session_ttl_secs(app_state),
        preacquired_provider_handle: None,
        started_generation: None,
    }
    .with_preacquired_provider_handle(provider_handle);
    let started_generation = session.write().await.start_origin_work();
    let Ok(lease_guard) = begin_hls_origin_account_io(&origin_io, session, &binding).await else {
        session.write().await.finish_origin_work(started_generation);
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    Ok(Some(TransientOriginIoGuard::new(Arc::clone(session), origin_io, lease_guard, started_generation)))
}

struct TransientOriginIoGuard {
    session: HlsSessionHandle,
    origin_io: HlsOriginIoContext,
    lease_guard: Option<HlsOriginAccountIoLeaseGuard>,
    started_generation: u64,
}

impl TransientOriginIoGuard {
    fn new(
        session: HlsSessionHandle,
        origin_io: HlsOriginIoContext,
        lease_guard: HlsOriginAccountIoLeaseGuard,
        started_generation: u64,
    ) -> Self {
        Self { session, origin_io, lease_guard: Some(lease_guard), started_generation }
    }
}

impl Drop for TransientOriginIoGuard {
    fn drop(&mut self) {
        let Some(lease_guard) = self.lease_guard.take() else {
            return;
        };
        let session = Arc::clone(&self.session);
        let origin_io = self.origin_io.clone();
        let started_generation = self.started_generation;
        tokio::spawn(async move {
            let generation_valid = {
                let mut session = session.write().await;
                session.finish_origin_work(started_generation)
            };
            let refresh_reservation = if generation_valid {
                session.read().await.should_refresh_origin_reservation(current_time_millis())
            } else {
                false
            };
            finish_hls_origin_account_io(&origin_io, &session, lease_guard, generation_valid && refresh_reservation)
                .await;
        });
    }
}

async fn try_hls_cached_manifest_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
    options: HlsCachedManifestOptions,
) -> Option<axum::response::Response> {
    let started_at = tokio::time::Instant::now();
    let started_at_ms = current_time_millis();
    loop {
        let (transient_body, rendered_body, should_wait, wait_for_initial_commit) = {
            let session = session.read().await;
            let now_ms = current_time_millis();
            let protection = session.account_binding_protection(now_ms);
            let transient_manifest_rendered_at_ms = session.transient.last_manifest_rendered_at_ms;
            let transient_manifest_refreshed_after_wait_started =
                transient_manifest_rendered_at_ms.is_some_and(|rendered_at_ms| rendered_at_ms >= started_at_ms);
            let should_wait = session.origin_refresh.in_flight
                || session.active_segment_fetches > 0
                || session.active_map_fetches > 0;
            (
                if can_serve_committed_transient_manifest(
                    &session,
                    protection,
                    options.policy,
                    transient_manifest_refreshed_after_wait_started,
                ) && manifest_rendered_after_required_boundary(transient_manifest_rendered_at_ms, options)
                {
                    session.transient.last_manifest_body.clone()
                } else {
                    None
                },
                session.last_rendered_manifest.as_ref().and_then(|rendered| {
                    manifest_rendered_after_required_boundary(Some(rendered.rendered_at_ms), options)
                        .then(|| rendered.body.clone())
                }),
                should_wait,
                matches!(protection, HlsAccountBindingProtection::NoMediaYet | HlsAccountBindingProtection::Expired)
                    && should_wait
                    && !options.wait_timeout.is_zero(),
            )
        };
        if !wait_for_initial_commit {
            if let Some(body) = transient_body {
                let proxy_session_id = session.read().await.proxy_session_id.clone();
                let body = materialize_transient_hls_access_manifest(
                    &body,
                    &proxy_session_id,
                    access_lease_id,
                    access_lease_state,
                    strip,
                    server_path,
                );
                mark_successful_canonical_manifest_activity(app_state, session, current_time_millis()).await;
                return Some(hls_response(body).into_response());
            }
            if let Some(body) = rendered_body {
                let body = materialize_hls_access_manifest(&body, access_lease_id, server_path);
                mark_successful_canonical_manifest_activity(app_state, session, current_time_millis()).await;
                return Some(hls_response(body).into_response());
            }
        }
        if options.wait_timeout.is_zero() || !should_wait || started_at.elapsed() >= options.wait_timeout {
            return None;
        }

        let remaining = options.wait_timeout.saturating_sub(started_at.elapsed());
        tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
    }
}

async fn mark_successful_canonical_manifest_activity(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    mark_hls_authorized_media_access(app_state, session, now_ms).await;
}

async fn mark_hls_authorized_manifest_access(app_state: &Arc<AppState>, session: &HlsSessionHandle, now_ms: u64) {
    session.write().await.mark_authorized_manifest_access(now_ms);
    app_state.hls_proxy.schedule_session_idle_for_handle(session).await;
}

async fn mark_hls_authorized_media_access(app_state: &Arc<AppState>, session: &HlsSessionHandle, now_ms: u64) {
    session.write().await.mark_authorized_media_access(now_ms);
    app_state.hls_proxy.schedule_session_idle_for_handle(session).await;
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy)]
enum HlsCachedManifestPolicy {
    CommittedOnly,
    AllowInitialNoMediaYet,
}

#[derive(Clone, Copy)]
struct HlsCachedManifestOptions {
    wait_timeout: Duration,
    policy: HlsCachedManifestPolicy,
    newer_than_rendered_at_ms: Option<u64>,
}

impl HlsCachedManifestOptions {
    #[cfg_attr(not(test), allow(dead_code))]
    const fn committed_only(wait_timeout: Duration) -> Self {
        Self { wait_timeout, policy: HlsCachedManifestPolicy::CommittedOnly, newer_than_rendered_at_ms: None }
    }

    const fn initial(wait_timeout: Duration) -> Self {
        Self { wait_timeout, policy: HlsCachedManifestPolicy::AllowInitialNoMediaYet, newer_than_rendered_at_ms: None }
    }

    const fn requiring_newer_manifest(mut self, rendered_at_ms: u64) -> Self {
        self.newer_than_rendered_at_ms = Some(rendered_at_ms);
        self
    }
}

fn manifest_rendered_after_required_boundary(rendered_at_ms: Option<u64>, options: HlsCachedManifestOptions) -> bool {
    let Some(boundary) = options.newer_than_rendered_at_ms else {
        return true;
    };
    rendered_at_ms.is_some_and(|rendered_at_ms| rendered_at_ms > boundary)
}

fn can_serve_committed_transient_manifest(
    session: &crate::api::model::HlsSession,
    protection: HlsAccountBindingProtection,
    policy: HlsCachedManifestPolicy,
    refreshed_after_wait_started: bool,
) -> bool {
    if !matches!(session.mode, HlsSessionMode::TransientPassthrough { .. }) {
        return false;
    }

    match protection {
        HlsAccountBindingProtection::HardActive { .. } | HlsAccountBindingProtection::SoftActive { .. } => true,
        HlsAccountBindingProtection::NoMediaYet => matches!(policy, HlsCachedManifestPolicy::AllowInitialNoMediaYet),
        HlsAccountBindingProtection::Expired => {
            matches!(policy, HlsCachedManifestPolicy::AllowInitialNoMediaYet) && refreshed_after_wait_started
        }
    }
}

fn hls_cache_enabled(app_state: &Arc<AppState>) -> bool {
    let config = app_state.app_config.config.load();
    config.reverse_proxy.as_ref().is_some_and(|reverse_proxy| reverse_proxy.hls_cache.is_some())
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
    target_id: u16,
    user_session: Option<&UserSession>,
    hls_url: &str,
    _archive_reference: Option<i64>,
    virtual_id: u32,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    original_hls_entry_path: &str,
) -> impl IntoResponse + Send {
    if app_state.active_users.is_user_blocked_for_stream(&user.username, virtual_id).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let stream_ref = hls_stream_ref_from_virtual_id(virtual_id);
    let normalized_hls_url = normalize_xtream_live_hls_url(hls_url, input);
    if normalized_hls_url != hls_url {
        debug_if_enabled!(
            "Normalized xtream hls url from {} to {}",
            sanitize_sensitive_info(hls_url),
            sanitize_sensitive_info(&normalized_hls_url)
        );
    }
    let url = ensure_hls_manifest_extension(&normalized_hls_url);
    let hls_cache_origin = build_hls_origin_resolution(input, &url);
    let hls_origin_source = hls_cache_origin.as_ref().map(|_| build_hls_origin_source(input, stream_ref.clone()));
    let server_info = app_state.app_config.get_user_server_info(user);
    let Some(server_info) = server_info else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let headers = build_hls_manifest_request_headers(
        &input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
    );
    let hls_session_ttl_secs = get_hls_session_ttl_secs(app_state);

    if hls_cache_enabled(app_state) {
        let Some(origin_source) = hls_origin_source.clone() else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        if let Some(response) = try_hls_cache_entry_redirect(
            app_state,
            fingerprint,
            user,
            origin_source,
            virtual_id,
            user_session,
            hls_cache_origin.as_ref().map_or(url.as_str(), |origin| origin.session_entry_url.as_str()),
            input,
            connection_permission,
            connection_kind,
            server_info.path.as_deref(),
        )
        .await
        {
            return response;
        }
    }

    let (request_url, session_token, provider_handle, _selected_provider_config) = if let Some(session) = user_session {
        let pinned_provider = if session.provider.is_empty() { &input.name } else { &session.provider };
        let provider_handle = if let Some(handle) = app_state
            .active_provider
            .acquire_exact_connection_with_grace_for_session(
                pinned_provider,
                &fingerprint.addr,
                false,
                connection_priority_for_kind(user, session.connection_kind.unwrap_or(connection_kind)),
                session.connection_kind.unwrap_or(connection_kind),
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
                let Some(stream_url) = get_stream_alternative_url(&url, input, cfg) else {
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
                        connection_kind: session.connection_kind,
                        socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
                    })
                    .await;
                app_state
                    .active_provider
                    .refresh_provider_reservation(&cfg.name, &session_token, hls_session_ttl_secs)
                    .await;
                (stream_url, Some(session_token), provider_handle, Some(selected_provider_config))
            }
            None => (url, None, None, None),
        }
    } else {
        let user_session_token = create_playback_session_fingerprint(
            fingerprint,
            &user.username,
            virtual_id,
            PlaylistItemType::LiveHls,
            None,
        );
        let hls_session_owner = if hls_cache_enabled(app_state) {
            let session_key = HlsSessionKey::new(input.id, virtual_id.to_string());
            let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
            Some(build_hls_origin_session_owner(&proxy_session_id))
        } else {
            None
        };
        let session_owner = hls_session_owner.as_deref().unwrap_or(user_session_token.as_str());
        let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
            app_state,
            fingerprint,
            user,
            input,
            virtual_id,
            &url,
            &user_session_token,
            session_owner,
            connection_permission,
            connection_kind,
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

    // Playlist requests only need the chosen provider account to derive the URL and pin the session.
    // Holding the provider slot until the first segment request causes stale active connections and
    // breaks forced same-account reuse on the next HLS/Catchup stream request.
    app_state.connection_manager.release_provider_handle(provider_handle).await;

    let input_source = InputSource::from(input).with_url(request_url);
    let use_manual_redirects = app_state.should_use_manual_redirects();
    let download_result = if use_manual_redirects {
        request::download_text_content_with_manual_redirects_and_headers(
            &app_state.app_config,
            &app_state.http_client_no_redirect.load(),
            &input_source,
            Some(&headers),
            false,
            MAX_MANUAL_REDIRECTS,
        )
        .await
    } else {
        request::download_text_content_with_headers(
            &app_state.app_config,
            &app_state.http_client.load(),
            &input_source,
            Some(&headers),
            false,
        )
        .await
    };
    match download_result {
        Ok((content, response_url, response_headers)) => {
            let encrypt_secret = app_state.get_encrypt_secret();
            let base_url = server_info.get_base_url();
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
                content: &content,
                hls_url: response_url,
                target_id,
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
            error!("Failed to download m3u8 {}", sanitize_sensitive_info(&err.to_string()));
            if let Some(session_token) = session_token.as_deref() {
                terminate_failed_hls_manifest_session(app_state, &user.username, session_token).await;
            }

            hls_custom_video_manifest_response(
                app_state,
                user,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
        }
    }
}

async fn get_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Option<StreamChannel> {
    if target.has_output(TargetType::Xtream) {
        if let Ok(pli) = xtream_get_item_for_stream_id(virtual_id, app_state, target, None).await {
            return Some(pli.to_stream_channel(target.id));
        }
    }
    let target_id = target.id;
    m3u_get_item_for_stream_id(virtual_id, app_state, target).await.ok().map(|pli| pli.to_stream_channel(target_id))
}

pub(in crate::api) async fn resolve_hls_virtual_input_for_target(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Option<Arc<ConfigInput>> {
    let channel = get_stream_channel(app_state, target, virtual_id).await?;
    app_state.app_config.get_input_by_name(&channel.input_name)
}

async fn resolve_hls_origin_playlist_url(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    virtual_id: u32,
    fallback_url: &str,
) -> Result<String, StatusCode> {
    if input.input_type.is_xtream() && target.has_output(TargetType::Xtream) {
        let pli = xtream_get_item_for_stream_id(virtual_id, app_state, target, None)
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

async fn resolve_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    input: &Arc<ConfigInput>,
    virtual_id: u32,
    hls_url: &str,
    archive_reference: Option<i64>,
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
        },
    };

    if archive_reference.is_some() {
        channel.item_type = PlaylistItemType::Catchup;
        channel.cluster = XtreamCluster::Video;
        channel.epg_reference_ts = archive_reference;
    } else {
        channel.item_type = PlaylistItemType::LiveHls;
        channel.epg_reference_ts = None;
    }
    channel
}

struct HlsAccessManifestRequestContext {
    input: Arc<ConfigInput>,
    hls_url: String,
    session_entry_url: String,
    original_hls_entry_path: String,
    origin_source: HlsOriginSource,
    failover_provider: Option<Arc<ConfigProvider>>,
    headers: HeaderMap,
    server_path: Option<String>,
}

async fn resolve_hls_playback_manifest_request_context(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    req_headers: &HeaderMap,
) -> Result<HlsAccessManifestRequestContext, StatusCode> {
    let Some((user, target)) = app_state.app_config.get_target_for_username(&access_context.username) else {
        return Err(StatusCode::NOT_FOUND);
    };
    let Some(input) = app_state.app_config.get_input_by_id(access_context.input_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if app_state.active_users.is_user_blocked_for_stream(&user.username, access_context.virtual_id).await {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(channel) = get_stream_channel(app_state, &target, access_context.virtual_id).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    let origin_playlist_url =
        resolve_hls_origin_playlist_url(app_state, &target, &input, access_context.virtual_id, channel.url.as_ref())
            .await?;
    let Some(hls_cache_origin) = build_hls_origin_resolution(&input, &origin_playlist_url) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let origin_source = build_hls_origin_source(&input, access_context.stream_ref.clone());
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
    );

    let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, access_context.virtual_id);

    Ok(HlsAccessManifestRequestContext {
        input,
        hls_url: hls_cache_origin.hls_url,
        session_entry_url: hls_cache_origin.session_entry_url,
        original_hls_entry_path,
        origin_source,
        failover_provider: hls_cache_origin.failover_provider,
        headers,
        server_path: server_info.path.clone(),
    })
}

async fn hls_proxy_manifest(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyManifestPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let access_lease_id = HlsAccessLeaseId(params.hls_access_lease_id);
    let now_ms = current_time_millis();
    let access_lease_snapshot = app_state
        .hls_proxy
        .access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms)
        .await;
    if let Some(session) = app_state.hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await {
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
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
        Err(response) => return response,
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
            session_entry_url: &request_context.session_entry_url,
            input: &request_context.input,
            origin_source: request_context.origin_source,
            failover_provider: request_context.failover_provider,
        },
        request_context.headers,
        get_hls_session_ttl_secs(&app_state),
        request_context.server_path.as_deref(),
        &request_context.original_hls_entry_path,
    )
    .await
    .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

#[allow(clippy::too_many_lines)]
async fn hls_api_stream(
    fingerprint: Fingerprint,
    req_headers: HeaderMap,
    axum::extract::Path(params): axum::extract::Path<HlsApiPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
) -> impl IntoResponse + Send {
    let api_proxy_user = create_api_proxy_user(&app_state);
    let (user, target) = if params.username == api_proxy_user.username && params.password == api_proxy_user.password {
        let Some(target) = app_state.app_config.get_target_by_id(params.target_id) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        (Arc::new(api_proxy_user), target)
    } else {
        let Some((user, target)) = app_state.app_config.get_target_for_user(&params.username, &params.password) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        if target.id != params.target_id {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        }
        (user, target)
    };
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
async fn hls_api_stream_resolved(
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
    if let Err(e) = check_network_access_only(&user, &fingerprint, &app_state) {
        return e.into_player_response(app_state.app_config.get_auth_error_status());
    }
    let target_name = &target.name;
    let virtual_id = stream_id;
    let input = try_option_bad_request!(
        app_state.app_config.get_input_by_id(input_id),
        true,
        format!("Can't find input {} for target {target_name}, stream_id {virtual_id}, hls", input_id)
    );

    if user.permission_denied(&app_state) {
        let stream_channel = resolve_stream_channel(&app_state, &target, &input, virtual_id, "", None).await;
        return hls_admission_failure_manifest_response(
            &app_state,
            &fingerprint,
            &user,
            stream_channel,
            input.name.clone(),
            &req_headers,
            ConnectFailureReason::UserAccountExpired,
        );
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

    if let Some(session) = &mut user_session {
        let decoded_archive_reference = m3u_archive_epg_reference_ts(&decoded_hls_token.1);
        if session.permission == UserConnectionPermission::Exhausted {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.1,
                decoded_archive_reference,
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
            );
        }

        if app_state.active_provider.is_over_limit(&session.provider).await {
            let stream_channel = resolve_stream_channel(
                &app_state,
                &target,
                &input,
                virtual_id,
                &decoded_hls_token.1,
                decoded_archive_reference,
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
            );
        }

        let hls_url = match decoded_hls_token {
            (Some(session_token), hls_url) if session.token.eq(&session_token) => hls_url,
            (None, hls_url) => hls_url,
            _ => return axum::http::StatusCode::BAD_REQUEST.into_response(),
        };
        let hls_url = hls_url.intern();
        let archive_reference = m3u_archive_epg_reference_ts(&hls_url);
        session.stream_url = hls_url.clone();
        if session.virtual_id == virtual_id {
            app_state.connection_manager.touch_http_activity(&user.username, &session.token, &fingerprint.addr).await;
            let stream_channel =
                resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url, archive_reference).await;
            if is_seek_request(stream_channel.cluster, &req_headers).await {
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
                &app_state,
                &user,
                &fingerprint,
                if is_m3u_catchup_session_token(&session.token) || archive_reference.is_some() {
                    PlaylistItemType::Catchup
                } else {
                    PlaylistItemType::LiveHls
                },
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
        let connection_kind =
            connection_admission.kind.or(session.connection_kind).unwrap_or(crate::api::model::ConnectionKind::Normal);
        session.permission = connection_permission;
        session.connection_kind = Some(connection_kind);
        if connection_permission == UserConnectionPermission::Exhausted {
            let provider = if session.provider.is_empty() { input.name.clone() } else { session.provider.clone() };
            let stream_channel =
                resolve_stream_channel(&app_state, &target, &input, virtual_id, &session.stream_url, archive_reference)
                    .await;
            return hls_admission_failure_manifest_response(
                &app_state,
                &fingerprint,
                &user,
                stream_channel,
                provider,
                &req_headers,
                ConnectFailureReason::UserConnectionsExhausted,
            );
        }

        if is_hls_url(&session.stream_url) {
            let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, virtual_id);
            return handle_hls_stream_request(
                &fingerprint,
                &app_state,
                &user,
                target.id,
                Some(session),
                &session.stream_url,
                archive_reference,
                virtual_id,
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
            let stream_channel =
                resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url, archive_reference).await;
            return local_stream_response(
                &fingerprint,
                &app_state,
                stream_channel,
                &req_headers,
                &input,
                &target,
                &user,
                connection_permission,
                connection_kind,
                Some(&session.token),
                Some(request_class),
                false,
            )
            .await
            .into_response();
        }

        let stream_channel =
            resolve_stream_channel(&app_state, &target, &input, virtual_id, &hls_url, archive_reference).await;
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
            },
            grace_mode,
        )
        .await
        .into_response()
    } else {
        axum::http::StatusCode::BAD_REQUEST.into_response()
    }
}

pub fn hls_api_register() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route(
            "/hls/{username}/{password}/{target_id}/{input_id}/{stream_id}/{token}",
            axum::routing::get(hls_api_stream),
        )
        .route(
            "/proxy/hls/live/{proxy_session_id}/{hls_access_lease_id}/manifest.m3u8",
            axum::routing::get(hls_proxy_manifest),
        )
        .route(
            "/proxy/hls/live/{proxy_session_id}/{hls_access_lease_id}/{segment_file}",
            axum::routing::get(hls_proxy_segment),
        )
        .route(
            "/proxy/hls/live/{proxy_session_id}/{hls_access_lease_id}/map/{map_file}",
            axum::routing::get(hls_proxy_map),
        )
        .route(
            "/proxy/hls/live/{proxy_session_id}/{hls_access_lease_id}/r/{resource_file}",
            axum::routing::get(hls_proxy_resource),
        )
    //cfg.service(web::resource("/hls/{token}/{stream}").route(web::get().to(xtream_player_api_hls_stream)));
    //cfg.service(web::resource("/play/{token}/{type}").route(web::get().to(xtream_player_api_play_stream)));
}

#[cfg(test)]
mod tests {
    use super::{
        build_hls_manifest_request_headers, extract_hls_provider_session_headers, hls_api_register,
        m3u_archive_epg_reference_ts,
    };
    use crate::{
        api::model::{
            begin_hls_origin_account_io, build_hls_custom_video_manifest_body, build_proxy_session_id,
            build_transient_resource_id, finish_hls_origin_account_io, ActiveProviderManager, ActiveUserManager,
            AppState, CacheAccessState, CancelTokens, ConnectionKind, ConnectionManager, CreateUserSessionParams,
            CustomVideoStreamType, EventManager, HlsAccessContext, HlsAccessLease, HlsAccessLeaseId,
            HlsAccessLeaseState, HlsAccessLeaseTiming, HlsLifecycleEvent, HlsLifecycleEventKey,
            HlsOriginAccountBinding, HlsOriginAccountBindingMode, HlsOriginAccountDetachedReason, HlsOriginIoContext,
            HlsPlaybackFamilyKey, HlsProxyManager, HlsSegmentFile, HlsSessionHandle, HlsSessionKey, HlsSessionMode,
            ManualPlaylistUpdateRequest, MapCacheStatus, MapEntry, MetadataUpdateManager, OriginMapKey,
            OriginSegmentFetchRef, OriginSegmentKey, PlaylistStorageState, ProviderConfig as RuntimeProviderConfig,
            ProviderConfigConnection, ProxyMapId, ProxySessionId, RenderedManifest, SegmentCacheKey, SegmentCacheStatus,
            SegmentEntry, SegmentFetchPriority, SharedStreamManager, TransientResourceKind, TransientResourceRef,
            UpdateGuard,
        },
        auth::Fingerprint,
        model::{
            ApiProxyConfig, AppConfig, Config, ConfigInput, ConfigProvider, ConfigTarget, HlsCacheConfig,
            ProcessTargets, ProxyUserCredentials, ReverseProxyConfig, ReverseProxyDisabledHeaderConfig, SourcesConfig,
            StripConfig, StripMode, TargetUser,
        },
        processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
        utils::GeoIp,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use axum::{
        body::Body,
        extract::ConnectInfo,
        http::{header, HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode},
        response::IntoResponse,
    };
    use http_body_util::BodyExt;
    use shared::model::{
        ConfigPaths, ConfigProviderDto, HlsCacheConfigDto, HlsSegmentRepairModeDto, InputType, PlaylistItemType,
        ProviderUrlSelectionPolicy, ReverseProxyConfigDto, UserConnectionPermission,
    };
    use std::{collections::HashMap, fmt::Write as _, net::SocketAddr, sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::mpsc,
    };
    use tower::ServiceExt;

    #[test]
    fn archive_epg_reference_supports_query_and_path_formats() {
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?utc=1700000000&lutc=1700003600"),
            Some(1_700_000_000)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/archive-1700003600-1700007200.m3u8"),
            Some(1_700_003_600)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/timeshift_abs-1700007200.ts"),
            Some(1_700_007_200)
        );
        assert_eq!(
            m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000&end=1700003600"),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn archive_epg_reference_rejects_plain_start_queries() {
        assert_eq!(m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000"), None);
    }

    #[test]
    fn extract_hls_provider_session_headers_converts_set_cookie_to_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", "sid=abc; Path=/; HttpOnly".parse().expect("valid cookie"));
        headers.append("set-cookie", "pref=1; Secure".parse().expect("valid cookie"));

        let session_headers = extract_hls_provider_session_headers(&headers);

        assert_eq!(session_headers.get("cookie").map(String::as_str), Some("sid=abc; pref=1"));
    }

    fn test_app_config() -> Arc<AppConfig> {
        let mut hls_user = ProxyUserCredentials::default();
        hls_user.username = "hls-user".to_string();
        hls_user.password = "hls-pass".to_string();
        hls_user.max_connections = 1;
        let api_proxy = ApiProxyConfig {
            user: vec![TargetUser { target: "default".to_string(), credentials: vec![Arc::new(hls_user)] }],
            ..Default::default()
        };
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig {
                batch_files: vec![],
                provider: vec![],
                inputs: vec![],
                sources: vec![],
                templates: None,
            })),
            hdhomerun: Arc::new(ArcSwapOption::empty()),
            api_proxy: Arc::new(ArcSwapOption::from(Some(Arc::new(api_proxy)))),
            file_locks: Arc::new(crate::utils::FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::empty()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        })
    }

    fn hls_custom_video_test_user() -> ProxyUserCredentials {
        let mut user = ProxyUserCredentials::default();
        user.username = "viewer".to_string();
        user.password = "secret".to_string();
        user
    }

    #[test]
    fn hls_custom_video_manifest_uses_single_endlist_segment_for_non_provisioning() {
        let user = hls_custom_video_test_user();
        let manifest = build_hls_custom_video_manifest_body(
            "https://example.test/iptv/",
            &user,
            CustomVideoStreamType::UserConnectionsExhausted,
            42_000,
            None,
        );

        assert!(manifest.contains("#EXT-X-TARGETDURATION:10"));
        assert!(manifest.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(manifest.contains("#EXT-X-ENDLIST"));
        assert!(manifest.contains("https://example.test/iptv/cvs/hls/viewer/secret/user_connections_exhausted.ts"));
        assert_eq!(manifest.matches("#EXTINF:10.0,").count(), 1);
    }

    #[test]
    fn hls_custom_video_manifest_uses_endlist_for_session_or_lease_expired() {
        let user = hls_custom_video_test_user();
        let manifest = build_hls_custom_video_manifest_body(
            "https://example.test/iptv",
            &user,
            CustomVideoStreamType::HlsSessionOrLeaseExpired,
            42_000,
            None,
        );

        assert!(manifest.contains("#EXT-X-ENDLIST"));
        assert!(manifest.contains(
            "https://example.test/iptv/cvs/hls/viewer/secret/hls_session_or_lease_expired.ts"
        ));
        assert_eq!(manifest.matches("#EXTINF:10.0,").count(), 1);
    }

    #[test]
    fn hls_custom_video_manifest_uses_live_six_segment_window_for_provisioning() {
        let user = hls_custom_video_test_user();
        let manifest = build_hls_custom_video_manifest_body(
            "https://example.test/iptv",
            &user,
            CustomVideoStreamType::Provisioning,
            123_456,
            Some(80510),
        );

        assert!(manifest.contains("#EXT-X-TARGETDURATION:2"));
        assert!(manifest.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(
            manifest.contains("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-DISCONTINUITY-SEQUENCE:0")
        );
        assert!(manifest.contains("#EXT-X-DISCONTINUITY-SEQUENCE:0"));
        assert!(!manifest.contains("#EXT-X-SESSION-DATA"));
        assert!(!manifest.contains("#EXT-X-ENDLIST"));
        assert_eq!(manifest.matches("#EXT-X-DISCONTINUITY\n#EXTINF:2.000000,").count(), 6);
        for index in 0..6 {
            assert!(manifest
                .contains(&format!("https://example.test/iptv/cvs/hls/viewer/secret/provisioning_{index:03}.ts")));
        }
        assert!(!manifest.contains("https://example.test/iptv/cvs/hls/viewer/secret/provisioning.ts"));
        assert_eq!(
            crate::api::model::hls_panel_provisioning_manifest_path(&user, 80510),
            "/cvs/hls/viewer/secret/provisioning.m3u8?id=80510"
        );
        assert!(!manifest.contains("provisioning.ts?"));
        assert!(!manifest.contains("virtual_id"));
        assert!(!manifest.contains("&seq="));
    }

    #[test]
    fn hls_response_uses_rfc8216_manifest_content_type() {
        let response = super::hls_response("#EXTM3U\n".to_string()).into_response();

        assert_eq!(response.headers().get(header::CONTENT_TYPE).unwrap(), "application/vnd.apple.mpegurl");
    }

    #[test]
    fn virtual_hls_entry_path_uses_single_manifest_extension() {
        let user = hls_custom_video_test_user();
        let xtream_target = ConfigTarget::from(&shared::model::ConfigTargetDto {
            output: vec![shared::model::TargetOutputDto::Xtream(shared::model::XtreamTargetOutputDto::default())],
            ..Default::default()
        });
        let xtream_input = ConfigInput { input_type: InputType::Xtream, ..ConfigInput::default() };
        let m3u_target = ConfigTarget::from(&shared::model::ConfigTargetDto::default());
        let m3u_input = ConfigInput { input_type: InputType::M3u, ..ConfigInput::default() };

        let xtream_path = super::build_virtual_hls_entry_path(&xtream_target, &xtream_input, &user, 59);
        let m3u_path = super::build_virtual_hls_entry_path(&m3u_target, &m3u_input, &user, 59);

        assert_eq!(xtream_path, "/live/viewer/secret/59.m3u8");
        assert_eq!(m3u_path, "/m3u-stream/live/viewer/secret/59.m3u8");
        assert!(!xtream_path.contains("..m3u8"));
        assert!(!m3u_path.contains("..m3u8"));
    }

    #[test]
    fn hls_manifest_headers_apply_disabled_headers_and_default_user_agent_policy() {
        let mut input_headers = HashMap::new();
        input_headers.insert("User-Agent".to_string(), "Input-UA".to_string());
        input_headers.insert("Accept-Language".to_string(), "de".to_string());
        input_headers.insert("Authorization".to_string(), "Bearer input-secret".to_string());
        input_headers.insert("X-Origin-Secret".to_string(), "input-secret".to_string());

        let mut request_headers = HeaderMap::new();
        request_headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        request_headers.insert(header::USER_AGENT, HeaderValue::from_static("Client-UA"));
        request_headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer client-secret"));
        request_headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
        request_headers
            .insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        request_headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
        request_headers.insert(HeaderName::from_static("x-blocked"), HeaderValue::from_static("client"));
        request_headers.insert(HeaderName::from_static("cf-ray"), HeaderValue::from_static("cf"));

        let disabled = ReverseProxyDisabledHeaderConfig {
            referer_header: false,
            x_header: true,
            cloudflare_header: true,
            custom_header: vec!["X-Origin-Secret".to_string()],
        };
        let headers =
            build_hls_manifest_request_headers(&input_headers, &request_headers, Some(&disabled), Some("Default-UA"));

        assert_eq!(headers.get(header::USER_AGENT).expect("user agent"), "Input-UA");
        assert_eq!(headers.get("accept-language").expect("accept language"), "de");
        assert!(!headers.contains_key(header::RANGE));
        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key(header::HOST));
        assert!(!headers.contains_key("x-origin-secret"));
        assert!(!headers.contains_key("x-blocked"));
        assert!(!headers.contains_key("cf-ray"));
    }

    #[test]
    fn hls_proxy_public_path_prefix_rewrites_only_proxy_hls_uri_surfaces() {
        let body = concat!(
            "#EXTM3U\n",
            "#EXT-X-KEY:METHOD=AES-128,URI=\"/proxy/hls/live/proxy-id/r/key.key\",IV=0x1\n",
            "#EXT-X-MAP:URI=\"/proxy/hls/live/proxy-id/map/000000.mp4\",BYTERANGE=\"10@0\"\n",
            "#EXT-X-PART:DURATION=1.0,URI=\"/proxy/hls/live/proxy-id/r/part.m4s\"\n",
            "#EXT-X-MEDIA-SEQUENCE:7\n",
            "#EXTINF:4.0,\n",
            "/proxy/hls/live/proxy-id/000007.ts\n",
            "https://origin.example.com/not-proxy.ts\n",
        );

        let prefixed = super::apply_hls_proxy_public_path_prefix(body.to_string(), Some("/iptv/"));

        assert!(prefixed.contains("URI=\"/iptv/proxy/hls/live/proxy-id/r/key.key\""));
        assert!(prefixed.contains("URI=\"/iptv/proxy/hls/live/proxy-id/map/000000.mp4\""));
        assert!(prefixed.contains("URI=\"/iptv/proxy/hls/live/proxy-id/r/part.m4s\""));
        assert!(prefixed.contains("\n/iptv/proxy/hls/live/proxy-id/000007.ts\n"));
        assert!(prefixed.contains("#EXT-X-MEDIA-SEQUENCE:7"));
        assert!(prefixed.contains("https://origin.example.com/not-proxy.ts"));
    }

    #[test]
    fn hls_proxy_public_path_prefix_keeps_body_unchanged_without_server_path() {
        let body = "#EXTM3U\n#EXTINF:4.0,\n/proxy/hls/live/proxy-id/000007.ts\n".to_string();

        assert_eq!(super::apply_hls_proxy_public_path_prefix(body.clone(), None), body);
        assert_eq!(super::apply_hls_proxy_public_path_prefix(body.clone(), Some("/")), body);
    }

    #[test]
    fn hls_manifest_materialization_uses_proxy_paths_without_provider_or_legacy_route() {
        let body = format!(
            "#EXTM3U\n#EXTINF:4.0,\n/proxy/hls/live/proxy-id/{}/000123.ts\n",
            crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
        );
        let lease_id = crate::api::model::HlsAccessLeaseId("access-lease".to_string());

        let materialized = super::materialize_hls_access_manifest(&body, &lease_id, Some("/iptv"));

        assert!(materialized.contains("/iptv/proxy/hls/live/proxy-id/access-lease/000123.ts"));
        assert!(!materialized.contains("provider://"));
        assert!(!materialized.contains("/hls/hls-user/"));
        assert!(!materialized.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    }

    #[test]
    fn hls_cache_origin_entry_url_preserves_provider_scheme_as_failover() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "demo".into(),
            urls: vec!["http://origin.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let input = ConfigInput { provider_configs: Some(vec![Arc::clone(&provider)]), ..ConfigInput::default() };

        let origin =
            super::resolve_hls_cache_origin_entry_url(&input, "provider://demo/live/account-a/token-a/1025130.m3u8")
                .expect("provider entry url should resolve");

        assert_eq!(origin.session_entry_url, "provider://demo/live/account-a/token-a/1025130.m3u8");
        assert_eq!(origin.failover_provider.as_ref().expect("provider failover config").name.as_ref(), "demo");
        let provider_key = super::build_hls_origin_source(&input, "1025130").session_key();
        let direct_key = super::build_hls_origin_source(&input, "1025130").session_key();
        assert_eq!(provider_key, direct_key);
        assert_eq!(provider_key.stable_value(), "input:0|hls|1025130");
        assert!(!provider_key.stable_value().contains("provider://"));
        assert!(!provider_key.stable_value().contains("origin.example.com"));
    }

    #[test]
    fn hls_origin_source_kind_covers_xtream_m3u_and_direct_media_playlist() {
        assert_eq!(
            super::hls_origin_source_kind(InputType::Xtream),
            crate::api::model::HlsOriginSourceKind::XtreamLive
        );
        assert_eq!(
            super::hls_origin_source_kind(InputType::M3u),
            crate::api::model::HlsOriginSourceKind::M3uMediaPlaylist
        );
        assert_eq!(
            super::hls_origin_source_kind(InputType::Library),
            crate::api::model::HlsOriginSourceKind::DirectMediaPlaylist
        );
    }

    #[test]
    fn hls_manifest_extension_helper_does_not_create_double_dot_urls() {
        assert_eq!(
            super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123.m3u8"),
            "http://origin.example.com/live/user/pass/1025123.m3u8"
        );
        assert_eq!(
            super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123..m3u8"),
            "http://origin.example.com/live/user/pass/1025123.m3u8"
        );
        assert_eq!(
            super::ensure_hls_manifest_extension("http://origin.example.com/live/user/pass/1025123..?token=1"),
            "http://origin.example.com/live/user/pass/1025123.m3u8?token=1"
        );
        assert_eq!(
            super::ensure_hls_manifest_extension("provider://mirror/live/user/pass/1025123..m3u8"),
            "provider://mirror/live/user/pass/1025123.m3u8"
        );
    }

    #[test]
    fn hls_origin_resolution_preserves_legacy_built_xtream_origin_url() {
        let input = ConfigInput {
            id: 7,
            name: Arc::from("xtream"),
            input_type: InputType::Xtream,
            url: "http://origin.example.com/base".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            ..ConfigInput::default()
        };

        let origin =
            super::build_hls_origin_resolution(&input, "http://other.example.com/live/other/creds/1025126.m3u8")
                .expect("xtream origin should resolve");

        assert_eq!(origin.session_entry_url, "http://other.example.com/live/other/creds/1025126.m3u8");
        assert_eq!(origin.hls_url, origin.session_entry_url);
    }

    #[test]
    fn hls_origin_resolution_keeps_provider_failover_out_of_identity() {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "mirror-group".into(),
            urls: vec!["http://mirror-a.example.com".into(), "http://mirror-b.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let input = ConfigInput {
            id: 7,
            name: Arc::from("xtream"),
            input_type: InputType::Xtream,
            url: "provider://mirror-group".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            provider_configs: Some(vec![Arc::clone(&provider)]),
            ..ConfigInput::default()
        };

        let origin = super::build_hls_origin_resolution(
            &input,
            "provider://mirror-group/live/source-user/source-pass/1025126.m3u8",
        )
        .expect("provider failover origin should resolve");
        let failover_key = super::build_hls_origin_source(&input, "80510").session_key();
        let direct_key = super::build_hls_origin_source(&input, "80510").session_key();

        assert_eq!(origin.session_entry_url, "provider://mirror-group/live/source-user/source-pass/1025126.m3u8");
        assert!(origin.failover_provider.is_some());
        assert_eq!(failover_key, direct_key);
        assert!(!failover_key.stable_value().contains("provider://"));
        assert!(!failover_key.stable_value().contains("mirror-a.example.com"));
        assert!(!failover_key.stable_value().contains("mirror-b.example.com"));
    }

    #[test]
    fn hls_origin_resolution_uses_m3u_playlist_item_url() {
        let input = ConfigInput {
            id: 9,
            name: Arc::from("m3u"),
            input_type: InputType::M3u,
            url: "http://playlist.example.com/list.m3u".to_string(),
            ..ConfigInput::default()
        };

        let origin = super::build_hls_origin_resolution(&input, "http://media.example.com/live/channel/index.m3u8")
            .expect("m3u hls origin should resolve");
        let source = super::build_hls_origin_source(&input, "stable-item");

        assert_eq!(origin.session_entry_url, "http://media.example.com/live/channel/index.m3u8");
        assert_eq!(source.source_kind, crate::api::model::HlsOriginSourceKind::M3uMediaPlaylist);
        assert_eq!(source.session_key().stable_value(), "input:9|hls|stable-item");
    }

    #[test]
    fn provider_failover_mirror_change_keeps_same_hls_session_identity() {
        let provider_a = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "mirror-group".into(),
            urls: vec!["http://mirror-a.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let provider_b = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "mirror-group".into(),
            urls: vec!["http://mirror-b.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::ResumeLastWorking,
            dns: None,
        }));
        let input_a = ConfigInput {
            id: 7,
            name: Arc::from("xtream"),
            input_type: InputType::Xtream,
            url: "provider://mirror-group".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            provider_configs: Some(vec![provider_a]),
            ..ConfigInput::default()
        };
        let input_b = ConfigInput { provider_configs: Some(vec![provider_b]), ..input_a.clone() };

        let origin_a = super::build_hls_origin_resolution(&input_a, "provider://mirror-group/a.m3u8")
            .expect("provider failover origin a should resolve");
        let origin_b = super::build_hls_origin_resolution(&input_b, "provider://mirror-group/b.m3u8")
            .expect("provider failover origin b should resolve");
        let key_a = super::build_hls_origin_source(&input_a, "80510").session_key();
        let key_b = super::build_hls_origin_source(&input_b, "80510").session_key();
        let secret = b"rewrite-secret";

        assert!(origin_a.failover_provider.is_some());
        assert!(origin_b.failover_provider.is_some());
        assert_eq!(key_a, key_b);
        assert_eq!(key_a.stable_value(), "input:7|hls|80510");
        assert_eq!(build_proxy_session_id(&key_a, secret), build_proxy_session_id(&key_b, secret));
        assert!(!key_a.stable_value().contains("provider://"));
        assert!(!key_a.stable_value().contains("mirror-a.example.com"));
        assert!(!key_a.stable_value().contains("mirror-b.example.com"));
    }

    #[test]
    fn hls_runtime_origin_fetch_url_uses_selected_provider_account() {
        let input = ConfigInput {
            name: Arc::from("source"),
            url: "http://source.example.com".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let provider_input = ConfigInput {
            id: 7,
            name: Arc::from("selected-provider"),
            url: "http://provider.example.com".to_string(),
            username: Some("provider-user".to_string()),
            password: Some("provider-pass".to_string()),
            input_type: InputType::Xtream,
            max_connections: 1,
            ..ConfigInput::default()
        };
        let provider = Arc::new(RuntimeProviderConfig::new(
            &provider_input,
            Arc::new(tokio::sync::RwLock::new(ProviderConfigConnection::default())),
            Arc::new(|_, _| {}),
        ));

        let fetch_url = super::build_hls_origin_fetch_url(
            &input,
            "http://source.example.com/live/source-user/source-pass/12345.m3u8",
            "http://source.example.com/live/source-user/source-pass/12345.m3u8",
            Some(&provider),
        )
        .expect("fetch url should be rewritten");

        assert_eq!(fetch_url, "http://provider.example.com/live/provider-user/provider-pass/12345.m3u8");

        let provider_scheme_input = ConfigInput {
            name: Arc::from("source"),
            url: "provider://demo".to_string(),
            username: Some("source-user".to_string()),
            password: Some("source-pass".to_string()),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let provider_scheme_fetch_url = super::build_hls_origin_fetch_url(
            &provider_scheme_input,
            "provider://demo/live/source-user/source-pass/12345.m3u8",
            "http://resolved.example.com/live/source-user/source-pass/12345.m3u8",
            Some(&provider),
        )
        .expect("provider scheme fetch url should use selected runtime account");

        assert_eq!(
            provider_scheme_fetch_url,
            "http://provider.example.com/live/provider-user/provider-pass/12345.m3u8"
        );
    }

    #[tokio::test]
    async fn hls_access_lease_validity_uses_session_idle_timeout_not_cache_duration() {
        let hls_dto = HlsCacheConfigDto { cache_duration: 900, session_idle_timeout: 42, ..Default::default() };
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));

        assert_eq!(super::hls_access_lease_ttl_ms(&app_state), 42_000);
    }

    #[tokio::test]
    async fn hls_access_lease_active_window_uses_two_target_durations() {
        let app_state = test_app_state();
        let key = HlsSessionKey::new(1, "access-window-stream");
        let (session, _) =
            app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", super::current_time_millis()).await;
        session.write().await.target_duration = Some(11);

        let timing = super::hls_access_lease_timing_for_session(&app_state, &session).await;

        assert_eq!(timing.active_window_ms, 22_000);
        assert_eq!(timing.valid_window_ms, super::hls_access_lease_ttl_ms(&app_state));
    }

    #[tokio::test]
    async fn hls_lifecycle_active_timer_moves_access_lease_to_idle() {
        let app_state = test_app_state();
        let now_ms = super::current_time_millis();
        let lease_id = HlsAccessLeaseId("lifecycle-lease".to_string());
        let key = HlsSessionKey::new(1, "lifecycle-stream");
        let (session, _) = app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", now_ms).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();

        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "lifecycle-stream".to_string(),
                123,
                now_ms,
                60_000,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 1, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());
        session.write().await.activity.active_access_lease_count = 1;

        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::AccessLeaseActive {
                        lease_id: lease_id.clone(),
                        proxy_session_id: proxy_session_id.clone(),
                    },
                    due_at_ms: now_ms.saturating_add(1),
                },
                now_ms.saturating_add(2),
            )
            .await;

        assert_eq!(
            app_state.hls_proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
            Some(HlsAccessLeaseState::Idle)
        );
        assert_eq!(session.read().await.activity.active_access_lease_count, 0);
    }

    #[tokio::test]
    async fn hls_lifecycle_validity_timer_removes_expired_access_lease() {
        let mut hls_cache = HlsCacheConfigDto::default();
        hls_cache.segment_repair.max_level = HlsSegmentRepairModeDto::Low;
        let hls_config = HlsCacheConfig::from(&hls_cache);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
        let now_ms = super::current_time_millis();
        let proxy_session_id = ProxySessionId("lifecycle-validity-proxy".to_string());
        let lease_id = HlsAccessLeaseId("lifecycle-validity-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "lifecycle-stream".to_string(),
                123,
                now_ms,
                1,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 1, valid_window_ms: 1 },
            )
            .await
            .is_activated());
        let repair_before = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_before.windows, 1);
        assert_eq!(repair_before.generations, 1);

        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: lease_id.clone(),
                        proxy_session_id: proxy_session_id.clone(),
                    },
                    due_at_ms: now_ms.saturating_add(1),
                },
                now_ms.saturating_add(2),
            )
            .await;

        assert_eq!(
            app_state.hls_proxy.access_leases().write().await.lease_state(&lease_id, now_ms.saturating_add(2)),
            None
        );
        let repair_after = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_after.windows, 0);
        assert_eq!(repair_after.generations, 0);
    }

    #[tokio::test]
    async fn hls_lifecycle_session_idle_timer_removes_idle_session() {
        let mut hls_dto = HlsCacheConfigDto { session_idle_timeout: 1, ..Default::default() };
        hls_dto.segment_repair.max_level = HlsSegmentRepairModeDto::Low;
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
        let now_ms = super::current_time_millis();
        let key = HlsSessionKey::new(1, "expired-session");
        let (session, _) =
            app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", now_ms.saturating_sub(2_000)).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("session-idle-cleanup-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "expired-session".to_string(),
                123,
                now_ms,
                60_000,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 1);
        assert_eq!(app_state.hls_proxy.segment_repair().stats().await.windows, 1);

        app_state
            .hls_proxy
            .handle_lifecycle_event(
                &app_state.active_users,
                &app_state.active_provider,
                HlsLifecycleEvent {
                    key: HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    due_at_ms: now_ms.saturating_sub(1_000),
                },
                now_ms,
            )
            .await;

        assert!(app_state.hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.is_none());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 0);
        let repair_after = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_after.windows, 0);
        assert_eq!(repair_after.generations, 0);
    }

    #[tokio::test]
    async fn hls_gc_session_removal_cleans_access_leases_and_repair_state() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let mut hls_dto = HlsCacheConfigDto {
            cache_path: temp_dir.path().to_string_lossy().into_owned(),
            session_idle_timeout: 1,
            ..Default::default()
        };
        hls_dto.segment_repair.max_level = HlsSegmentRepairModeDto::Low;
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
        let now_ms = super::current_time_millis();
        let key = HlsSessionKey::new(1, "gc-cleanup-session");
        let (session, _) =
            app_state.hls_proxy.get_or_create_session_with_outcome(key, b"secret", now_ms.saturating_sub(2_000)).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let lease_id = HlsAccessLeaseId("gc-cleanup-lease".to_string());
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", "client"),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "gc-cleanup-session".to_string(),
                123,
                now_ms,
                60_000,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: 30_000, valid_window_ms: 60_000 },
            )
            .await
            .is_activated());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 1);
        assert_eq!(app_state.hls_proxy.segment_repair().stats().await.windows, 1);

        let report = app_state.hls_proxy.run_garbage_collection_once(now_ms).await.expect("gc should run");

        assert_eq!(report.sessions_deleted, 1);
        assert!(app_state.hls_proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await.is_none());
        assert_eq!(app_state.hls_proxy.access_leases().read().await.len(), 0);
        let repair_after = app_state.hls_proxy.segment_repair().stats().await;
        assert_eq!(repair_after.windows, 0);
        assert_eq!(repair_after.generations, 0);
    }

    fn test_app_state() -> Arc<AppState> { test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::new())) }

    fn test_app_state_with_inputs(inputs: Vec<Arc<ConfigInput>>) -> Arc<AppState> {
        test_app_state_with_hls_proxy_and_inputs(Arc::new(HlsProxyManager::new()), inputs)
    }

    fn enable_hls_cache(app_state: &Arc<AppState>) {
        let config = Config {
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                hls_cache: Some(HlsCacheConfigDto::default()),
                ..Default::default()
            })),
            ..Default::default()
        };
        app_state.app_config.config.store(Arc::new(config));
    }

    fn test_app_state_with_hls_proxy(hls_proxy: Arc<HlsProxyManager>) -> Arc<AppState> {
        test_app_state_with_hls_proxy_and_inputs(hls_proxy, Vec::new())
    }

    fn test_app_state_with_hls_proxy_and_inputs(
        hls_proxy: Arc<HlsProxyManager>,
        inputs: Vec<Arc<ConfigInput>>,
    ) -> Arc<AppState> {
        let app_config = test_app_config();
        if !inputs.is_empty() {
            app_config.sources.store(Arc::new(SourcesConfig {
                batch_files: vec![],
                provider: vec![],
                inputs,
                sources: vec![],
                templates: None,
            }));
        }
        let event_manager = Arc::new(EventManager::new());
        let active_provider = Arc::new(ActiveProviderManager::new(&app_config, &event_manager));
        let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
        active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

        let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
        let config = app_config.config.load();
        let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
        let connection_manager = Arc::new(ConnectionManager::new(
            &active_users,
            &active_provider,
            &shared_stream_manager,
            &event_manager,
            None,
        ));
        let cancel_tokens = CancelTokens::default();
        let metadata_manager = Arc::new(MetadataUpdateManager::new(cancel_tokens.metadata.clone()));
        let (manual_update_sender, _) = mpsc::channel::<ManualPlaylistUpdateRequest>(1);

        Arc::new(AppState {
            forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
                enabled: false,
                inputs: Vec::new(),
                targets: Vec::new(),
                target_names: Vec::new(),
            })),
            app_config,
            http_client: Arc::new(ArcSwap::from_pointee(reqwest::Client::new())),
            http_client_no_redirect: Arc::new(ArcSwap::from_pointee(
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .expect("no-redirect client builds"),
            )),
            downloads: Arc::new(crate::api::model::DownloadQueue::new()),
            cache: Arc::new(ArcSwapOption::default()),
            shared_stream_manager,
            hls_proxy,
            hls_provisioning: Arc::new(crate::api::model::HlsProvisioningState::new()),
            active_users,
            active_provider,
            connection_manager,
            event_manager,
            cancel_tokens: Arc::new(ArcSwap::from_pointee(cancel_tokens)),
            playlists: Arc::new(PlaylistStorageState::new()),
            geoip,
            update_guard: UpdateGuard::new(),
            metadata_manager,
            manual_update_sender,
        })
    }

    async fn create_bound_hls_test_session(
        app_state: &Arc<AppState>,
        input: &ConfigInput,
        stream_ref: &str,
        account_name: &str,
        now_ms: u64,
    ) -> HlsSessionHandle {
        let origin_source = super::build_hls_origin_source(input, stream_ref);
        let session_key = origin_source.session_key();
        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key,
                origin_source,
                &app_state.get_encrypt_secret(),
                now_ms,
            )
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        {
            let mut session_guard = session.write().await;
            session_guard.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::clone(&input.name),
                Arc::from(account_name),
                &proxy_session_id,
                now_ms,
            ));
        }
        session
    }

    async fn create_unbound_hls_test_session(
        app_state: &Arc<AppState>,
        input: &ConfigInput,
        stream_ref: &str,
        now_ms: u64,
    ) -> HlsSessionHandle {
        let origin_source = super::build_hls_origin_source(input, stream_ref);
        let session_key = origin_source.session_key();
        app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key,
                origin_source,
                &app_state.get_encrypt_secret(),
                now_ms,
            )
            .await
            .0
    }

    fn test_hls_origin_io_context(app_state: &Arc<AppState>) -> HlsOriginIoContext {
        HlsOriginIoContext {
            app_state: Arc::clone(app_state),
            client_addr: test_fingerprint().addr,
            allow_grace: false,
            priority: 0,
            connection_kind: ConnectionKind::Normal,
            reservation_ttl_secs: 60,
            preacquired_provider_handle: None,
            started_generation: None,
        }
    }

    fn single_hls_provider_input(name: &str) -> ConfigInput {
        ConfigInput {
            id: 1,
            name: Arc::from(name),
            input_type: InputType::Xtream,
            url: "http://account.example.com".to_string(),
            username: Some("account-user".to_string()),
            password: Some("account-pass".to_string()),
            enabled: true,
            priority: 0,
            max_connections: 1,
            ..ConfigInput::default()
        }
    }

    fn overlap_provider_input() -> ConfigInput {
        ConfigInput {
            id: 1,
            name: Arc::from("overlap-input"),
            input_type: InputType::Xtream,
            url: "http://root.example.com".to_string(),
            username: Some("root-user".to_string()),
            password: Some("root-pass".to_string()),
            enabled: true,
            priority: 10,
            max_connections: 1,
            aliases: Some(vec![crate::model::ConfigInputAlias {
                id: 2,
                name: Arc::from("account-a"),
                url: "http://account.example.com".to_string(),
                username: Some("account-user".to_string()),
                password: Some("account-pass".to_string()),
                priority: 0,
                max_connections: 1,
                exp_date: None,
                enabled: true,
            }]),
            ..ConfigInput::default()
        }
    }

    #[tokio::test]
    async fn hls_origin_account_io_lease_allows_parallel_same_session_origin_work() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let binding = session.read().await.origin_account_binding.clone().expect("binding exists");
        let origin_io = test_hls_origin_io_context(&app_state);

        let first = begin_hls_origin_account_io(&origin_io, &session, &binding)
            .await
            .expect("first same-session origin io acquires provider account");
        wait_for_provider_connection_count(&app_state, 1).await;
        let second = begin_hls_origin_account_io(&origin_io, &session, &binding)
            .await
            .expect("second same-session origin io joins session lease");
        wait_for_provider_connection_count(&app_state, 1).await;
        assert_eq!(
            session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .expect("session provider lease exists")
                .active_io_count,
            2
        );

        finish_hls_origin_account_io(&origin_io, &session, first, true).await;
        wait_for_provider_connection_count(&app_state, 1).await;
        assert_eq!(
            session
                .read()
                .await
                .origin_account_io_lease
                .as_ref()
                .expect("session provider lease remains while second io is active")
                .active_io_count,
            1
        );

        finish_hls_origin_account_io(&origin_io, &session, second, true).await;
        wait_for_provider_connection_count(&app_state, 0).await;
        assert!(
            app_state
                .active_provider
                .is_provider_reserved_for_other_session(&binding.account_name, Some("other-hls-session"))
                .await
        );
    }

    #[tokio::test]
    async fn hls_origin_account_io_lease_blocks_other_hls_sessions_for_same_account() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let first_session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let second_session = create_bound_hls_test_session(&app_state, &input, "67890", "account-a", 1_000).await;
        let first_binding = first_session.read().await.origin_account_binding.clone().expect("binding exists");
        let second_binding = second_session.read().await.origin_account_binding.clone().expect("binding exists");
        let origin_io = test_hls_origin_io_context(&app_state);

        let first = begin_hls_origin_account_io(&origin_io, &first_session, &first_binding)
            .await
            .expect("first session acquires account");
        wait_for_provider_connection_count(&app_state, 1).await;

        assert!(begin_hls_origin_account_io(&origin_io, &second_session, &second_binding).await.is_err());
        wait_for_provider_connection_count(&app_state, 1).await;

        finish_hls_origin_account_io(&origin_io, &first_session, first, true).await;
        wait_for_provider_connection_count(&app_state, 0).await;
    }

    #[tokio::test]
    async fn transient_origin_binding_requires_runtime_prepare_for_missing_or_detached_account() {
        let input = single_hls_provider_input("known-account");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let proxy_session_id = ProxySessionId("shared-hls-session".to_string());
        let known_binding =
            HlsOriginAccountBinding::new(Arc::clone(&input.name), Arc::clone(&input.name), &proxy_session_id, 1_000);
        let missing_binding = HlsOriginAccountBinding::new(
            Arc::clone(&input.name),
            Arc::from("removed-account"),
            &proxy_session_id,
            1_000,
        );
        let mut detached_binding = known_binding.clone();
        detached_binding.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, 2_000);

        assert!(!super::hls_transient_origin_binding_requires_runtime_prepare(&app_state, &known_binding));
        assert!(super::hls_transient_origin_binding_requires_runtime_prepare(&app_state, &missing_binding));
        assert!(super::hls_transient_origin_binding_requires_runtime_prepare(&app_state, &detached_binding));
    }

    #[tokio::test]
    async fn hls_account_overlap_selects_soft_candidate_but_not_hard_active() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let session = create_bound_hls_test_session(&app_state, &input, "old", "account-a", 1_000).await;
        {
            let mut session = session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
        }
        let new_proxy_session_id = ProxySessionId("new-session".to_string());

        let hard_candidate =
            super::find_hls_account_overlap_candidate(&app_state, &input.name, &new_proxy_session_id, 5_000).await;
        assert!(hard_candidate.is_none(), "hard-active sessions must not be overbooked");

        let soft_candidate =
            super::find_hls_account_overlap_candidate(&app_state, &input.name, &new_proxy_session_id, 12_000)
                .await
                .expect("soft-active session can be overbooked");
        assert_eq!(soft_candidate.account_name.as_ref(), "account-a");
        assert_eq!(soft_candidate.last_media_at_ms, 1_000);
        assert_eq!(soft_candidate.reclaim_until_ms, 31_000);
    }

    #[tokio::test]
    async fn hls_account_overlap_reclaim_preempts_speculative_session() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let winner = create_bound_hls_test_session(&app_state, &input, "winner", "account-a", 1_000).await;
        let loser = create_bound_hls_test_session(&app_state, &input, "loser", "account-a", 1_000).await;
        let winner_proxy_session_id = winner.read().await.proxy_session_id.clone();
        let loser_proxy_session_id = loser.read().await.proxy_session_id.clone();
        {
            let mut loser = loser.write().await;
            loser.origin_account_binding = Some(HlsOriginAccountBinding::speculative_from(
                Arc::clone(&input.name),
                Arc::from("account-a"),
                &loser_proxy_session_id,
                winner_proxy_session_id.clone(),
                20_000,
                2_000,
            ));
        }
        let loser_generation = loser.read().await.activity.origin_work_generation;

        super::reclaim_hls_account_overlap_if_needed(&app_state, &winner, 10_000).await;

        let loser_binding_mode = loser.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(
            loser_binding_mode,
            HlsOriginAccountBindingMode::Detached {
                reason: HlsOriginAccountDetachedReason::ReclaimedByOriginalOwner,
                ..
            }
        ));
        assert_eq!(loser.read().await.activity.origin_work_generation, loser_generation + 1);
        let winner_binding_mode = winner.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(winner_binding_mode, HlsOriginAccountBindingMode::Active));
    }

    #[tokio::test]
    async fn hls_account_overlap_promotes_speculative_session_after_soft_window() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let displaced = create_bound_hls_test_session(&app_state, &input, "displaced", "account-a", 1_000).await;
        let promoted = create_bound_hls_test_session(&app_state, &input, "promoted", "account-a", 1_000).await;
        let displaced_proxy_session_id = displaced.read().await.proxy_session_id.clone();
        let promoted_proxy_session_id = promoted.read().await.proxy_session_id.clone();
        {
            let mut promoted = promoted.write().await;
            promoted.origin_account_binding = Some(HlsOriginAccountBinding::speculative_from(
                Arc::clone(&input.name),
                Arc::from("account-a"),
                &promoted_proxy_session_id,
                displaced_proxy_session_id,
                20_000,
                2_000,
            ));
        }
        let displaced_generation = displaced.read().await.activity.origin_work_generation;

        super::promote_elapsed_hls_account_overlaps(&app_state, 20_001).await;

        let displaced_binding_mode =
            displaced.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(
            displaced_binding_mode,
            HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::SoftWindowElapsed, .. }
        ));
        assert_eq!(displaced.read().await.activity.origin_work_generation, displaced_generation + 1);
        let promoted_binding_mode = promoted.read().await.origin_account_binding.as_ref().unwrap().binding_mode.clone();
        assert!(matches!(promoted_binding_mode, HlsOriginAccountBindingMode::Active));
    }

    #[tokio::test]
    async fn hls_account_binding_soft_expiry_retains_session_and_reacquires_on_authorized_manifest_work() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        {
            let mut session = session.write().await;
            session.target_duration = Some(1);
            session.mark_authorized_media_access(1_000);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        super::detach_unprotected_hls_origin_account_bindings(&app_state, 4_001).await;

        {
            let session = session.read().await;
            let binding = session.origin_account_binding.as_ref().expect("detached binding is retained");
            assert!(matches!(
                binding.binding_mode,
                HlsOriginAccountBindingMode::Detached { reason: HlsOriginAccountDetachedReason::SoftWindowElapsed, .. }
            ));
            assert_eq!(session.activity.origin_work_generation, old_generation + 1);
        }

        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let prepared_origin = super::prepare_hls_origin_runtime(
            &app_state,
            &session,
            &input,
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            "http://root.example.com/live/root-user/root-pass/12345.m3u8",
            &proxy_session_id,
            &test_fingerprint(),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Manifest,
            super::HlsOriginWorkClass::ManifestInteractive,
            4_100,
        )
        .await
        .expect("authorized origin work can reacquire a detached binding");

        let binding = prepared_origin.binding_to_store.as_ref().expect("new binding should be stored by caller");
        assert_eq!(binding.account_name.as_ref(), "account-a");
        assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
        assert_eq!(prepared_origin.fetch_url, "http://account.example.com/live/account-user/account-pass/12345.m3u8");
        app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_provider_handle).await;
    }

    #[tokio::test]
    async fn hls_origin_runtime_uses_soft_overlap_before_grace_for_interactive_work() {
        let input = single_hls_provider_input("soft-overlap-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let old_session = create_bound_hls_test_session(&app_state, &input, "old", input.name.as_ref(), 1_000).await;
        let old_proxy_session_id = old_session.read().await.proxy_session_id.clone();
        {
            let mut session = old_session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
        }
        let old_binding = old_session.read().await.origin_account_binding.clone().expect("binding exists");
        app_state
            .active_provider
            .refresh_provider_reservation(&old_binding.account_name, &old_binding.session_owner, 60)
            .await;

        let new_session = create_unbound_hls_test_session(&app_state, &input, "new", 12_000).await;
        let new_proxy_session_id = new_session.read().await.proxy_session_id.clone();
        let prepared_origin = super::prepare_hls_origin_runtime(
            &app_state,
            &new_session,
            &input,
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            &new_proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55201)),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Manifest,
            super::HlsOriginWorkClass::ManifestInteractive,
            12_000,
        )
        .await
        .expect("interactive work should use soft-active overlap before grace");

        let binding = prepared_origin.binding_to_store.as_ref().expect("speculative binding should be prepared");
        assert_eq!(binding.account_name, input.name);
        assert!(matches!(
            &binding.binding_mode,
            HlsOriginAccountBindingMode::Speculative {
                displaced_proxy_session_id,
                ..
            } if displaced_proxy_session_id == &old_proxy_session_id
        ));
        assert!(matches!(
            prepared_origin.preacquired_provider_handle.as_ref().map(|handle| &handle.allocation),
            Some(super::ProviderAllocation::Available(_))
        ));

        app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_provider_handle).await;
    }

    #[tokio::test]
    async fn hls_origin_runtime_uses_grace_as_interactive_fallback_after_overlap_fails() {
        let input = single_hls_provider_input("grace-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let occupied = app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &test_addr_with_port(55211),
                false,
                0,
                ConnectionKind::Normal,
                Some("external-owner"),
            )
            .await
            .expect("test should occupy the only provider account");
        let session = create_unbound_hls_test_session(&app_state, &input, "12345", 2_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();

        let prepared_origin = super::prepare_hls_origin_runtime(
            &app_state,
            &session,
            &input,
            "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            &proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55212)),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Manifest,
            super::HlsOriginWorkClass::ManifestInteractive,
            2_000,
        )
        .await
        .expect("interactive work can use grace when normal acquire and overlap fail");

        assert!(matches!(
            prepared_origin.preacquired_provider_handle.as_ref().map(|handle| &handle.allocation),
            Some(super::ProviderAllocation::GracePeriod(_))
        ));
        assert_eq!(
            prepared_origin
                .binding_to_store
                .as_ref()
                .expect("grace binding should still bind the selected account")
                .account_name
                .as_ref(),
            input.name.as_ref()
        );

        app_state.connection_manager.release_provider_handle(prepared_origin.preacquired_provider_handle).await;
        app_state.connection_manager.release_provider_handle(Some(occupied)).await;
    }

    #[tokio::test]
    async fn hls_origin_runtime_background_skips_soft_overlap_and_grace() {
        let input = single_hls_provider_input("background-input");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let old_session = create_bound_hls_test_session(&app_state, &input, "old", input.name.as_ref(), 1_000).await;
        {
            let mut session = old_session.write().await;
            session.target_duration = Some(10);
            session.mark_authorized_media_access(1_000);
        }
        let old_binding = old_session.read().await.origin_account_binding.clone().expect("binding exists");
        app_state
            .active_provider
            .refresh_provider_reservation(&old_binding.account_name, &old_binding.session_owner, 60)
            .await;
        let new_session = create_unbound_hls_test_session(&app_state, &input, "new", 12_000).await;
        let new_proxy_session_id = new_session.read().await.proxy_session_id.clone();

        let result = super::prepare_hls_origin_runtime(
            &app_state,
            &new_session,
            &input,
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            "http://account.example.com/live/account-user/account-pass/new.m3u8",
            &new_proxy_session_id,
            &test_fingerprint_with_addr(test_addr_with_port(55221)),
            ConnectionKind::Normal,
            0,
            super::HlsOriginWorkKind::Segment,
            super::HlsOriginWorkClass::Background,
            12_000,
        )
        .await;

        assert_eq!(result.err(), Some(super::HlsOriginRuntimeAcquireError::NoAccountAvailable));
        let old_session = old_session.read().await;
        assert!(matches!(
            old_session.origin_account_binding.as_ref().expect("old binding remains").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert!(new_session.read().await.origin_account_binding.is_none());
    }

    #[tokio::test]
    async fn hls_account_binding_without_media_activity_is_not_detached() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        {
            let mut session = session.write().await;
            session.target_duration = Some(12);
            session.mark_authorized_manifest_access(1_000);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        super::detach_unprotected_hls_origin_account_bindings(&app_state, 60_000).await;

        let session = session.read().await;
        let binding = session.origin_account_binding.as_ref().expect("binding remains");
        assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
        assert_eq!(session.activity.origin_work_generation, old_generation);
        assert_eq!(session.activity.last_authorized_media_at_ms, None);
        assert_eq!(session.account_overlap_timing().target_duration_ms, 12_000);
    }

    #[tokio::test]
    async fn hls_ready_cache_hit_does_not_require_origin_reacquire_when_binding_is_detached() {
        let app_state = test_app_state();
        let input = ConfigInput { id: 1, name: Arc::from("overlap-input"), ..ConfigInput::default() };
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        {
            let mut session = session.write().await;
            session
                .origin_account_binding
                .as_mut()
                .expect("binding exists")
                .detach(HlsOriginAccountDetachedReason::Cleanup, 2_000);
            session.segments.insert(
                123,
                SegmentEntry {
                    origin_key: OriginSegmentKey { origin_epoch: 0, origin_seq: 123 },
                    proxy_seq: 123,
                    duration_ms: 4_000,
                    proxy_file_ext: "ts".to_string(),
                    content_type: "video/MP2T".to_string(),
                    cache_key: SegmentCacheKey::new(proxy_session_id, 123, "ts"),
                    discontinuity_before: false,
                    program_date_time: None,
                    daterange_tags_before: Vec::new(),
                    origin_byte_range: None,
                    map_ref: None,
                    origin_fetch_ref: Some(OriginSegmentFetchRef {
                        resolved_origin_url: "http://origin.example.com/123.ts".to_string(),
                        byte_range: None,
                        valid_until_ms: None,
                    }),
                    status: SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 1_000 },
                    last_rendered_at_ms: None,
                    access: Arc::new(CacheAccessState::new()),
                },
            );
        }

        let segment_file = HlsSegmentFile { proxy_seq: 123, extension: "ts".to_string() };
        assert!(!super::hls_segment_request_requires_origin_work(&session, &segment_file).await);
        assert!(super::hls_origin_binding_needs_reacquire(&session).await);
    }

    async fn activate_test_hls_access_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        lease_id: &str,
        now_ms: u64,
        ttl_ms: u64,
    ) {
        let lease_id = HlsAccessLeaseId(lease_id.to_string());
        let valid_window_ms = ttl_ms.saturating_mul(10).max(ttl_ms);
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                valid_window_ms,
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming { active_window_ms: ttl_ms, valid_window_ms },
            )
            .await
            .is_activated());
    }

    async fn register_test_hls_stream_for_lease_release(
        app_state: &Arc<AppState>,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        provider: &str,
    ) {
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.max_connections = 1;
        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "hls-session-token",
                virtual_id: 12345,
                provider,
                stream_url: "http://origin.example.com/live/user/pass/12345.m3u8",
                addr: &test_addr(),
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let mut stream_channel =
            super::fallback_hls_cache_stream_channel(0, 12345, &session.read().await.origin_source, proxy_session_id);
        stream_channel.shared = true;
        stream_channel.shared_stream_id = Some(super::hls_cache_shared_stream_id(proxy_session_id));
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 0,
                username: &user.username,
                max_connections: user.max_connections,
                soft_connections: user.soft_connections,
                connection_kind: ConnectionKind::Normal,
                priority: user.priority,
                soft_priority: user.soft_priority,
                fingerprint: &test_fingerprint(),
                provider: Arc::from(provider),
                stream_channel: &stream_channel,
                user_agent: std::borrow::Cow::Borrowed("test"),
                session_token: Some("hls-session-token"),
            })
            .await;
    }

    fn test_segment_entry(
        proxy_session_id: &ProxySessionId,
        proxy_seq: u64,
        status: SegmentCacheStatus,
    ) -> SegmentEntry {
        SegmentEntry {
            origin_key: OriginSegmentKey { origin_epoch: 0, origin_seq: proxy_seq },
            proxy_seq,
            duration_ms: 4_000,
            proxy_file_ext: "ts".to_string(),
            content_type: "video/MP2T".to_string(),
            cache_key: SegmentCacheKey::new(proxy_session_id.clone(), proxy_seq, "ts"),
            discontinuity_before: false,
            program_date_time: None,
            daterange_tags_before: Vec::new(),
            origin_byte_range: None,
            map_ref: None,
            origin_fetch_ref: Some(OriginSegmentFetchRef {
                resolved_origin_url: format!("http://origin.example.com/{proxy_seq}.ts"),
                byte_range: None,
                valid_until_ms: None,
            }),
            status,
            last_rendered_at_ms: None,
            access: Arc::new(CacheAccessState::new()),
        }
    }

    #[tokio::test]
    async fn hls_access_lease_idle_releases_user_but_keeps_origin_binding_and_queues() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let binding = session.read().await.origin_account_binding.clone().expect("binding exists");
        let account_name = Arc::from("account-a");
        app_state.active_provider.refresh_provider_reservation(&account_name, &binding.session_owner, 60).await;
        assert!(
            app_state.active_provider.is_provider_reserved_for_other_session(&account_name, Some("other-owner")).await
        );
        activate_test_hls_access_lease(&app_state, &proxy_session_id, "detach-lease", 1_000, 1_000).await;
        register_test_hls_stream_for_lease_release(&app_state, &session, &proxy_session_id, input.name.as_ref()).await;
        assert_eq!(app_state.active_users.active_streams().await.len(), 1);
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                1_000,
            )
            .await;
        {
            let mut session = session.write().await;
            assert_eq!(session.activity.active_access_lease_count, 1);
            session.segments.insert(1, test_segment_entry(&proxy_session_id, 1, SegmentCacheStatus::Discovered));
            session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Prefetch, 1_100);
            session.segments.insert(
                2,
                test_segment_entry(
                    &proxy_session_id,
                    2,
                    SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 1_000 },
                ),
            );
            let map_id = ProxyMapId(1);
            let mut map = MapEntry::new(
                &proxy_session_id,
                map_id,
                OriginMapKey {
                    origin_epoch: 0,
                    resolved_origin_uri: "http://origin.example.com/init.mp4".to_string(),
                    byte_range: None,
                },
                "mp4".to_string(),
            );
            map.status = MapCacheStatus::Queued { queued_at_ms: 1_100 };
            session.maps.insert(map_id, map);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                3_000,
            )
            .await;

        {
            let session = session.read().await;
            assert_eq!(session.activity.active_access_lease_count, 0);
            let binding = session.origin_account_binding.as_ref().expect("binding is retained");
            assert!(matches!(binding.binding_mode, HlsOriginAccountBindingMode::Active));
            assert_eq!(session.activity.origin_work_generation, old_generation);
            assert!(!session.segment_prefetch_queue.is_empty());
            assert!(matches!(
                session.segments.get(&1).expect("queued segment remains").status,
                SegmentCacheStatus::Queued { .. }
            ));
            assert!(matches!(
                session.segments.get(&2).expect("ready segment remains").status,
                SegmentCacheStatus::Ready { .. }
            ));
            assert!(matches!(
                session.maps.get(&ProxyMapId(1)).expect("map remains").status,
                MapCacheStatus::Queued { .. }
            ));
        }
        assert!(
            app_state.active_provider.is_provider_reserved_for_other_session(&account_name, Some("other-owner")).await
        );
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn hls_access_lease_sync_keeps_binding_and_queue_when_active_count_remains_positive() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        activate_test_hls_access_lease(&app_state, &proxy_session_id, "still-active-lease", 1_000, 10_000).await;
        {
            let mut session = session.write().await;
            session.activity.active_access_lease_count = 1;
            session.segments.insert(1, test_segment_entry(&proxy_session_id, 1, SegmentCacheStatus::Discovered));
            session.queue_segment_fetch_candidate(1, SegmentFetchPriority::Prefetch, 1_100);
        }
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                1_500,
            )
            .await;

        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 1);
        assert!(matches!(
            session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert_eq!(session.activity.origin_work_generation, old_generation);
        assert!(!session.segment_prefetch_queue.is_empty());
        assert!(matches!(
            session.segments.get(&1).expect("segment remains queued").status,
            SegmentCacheStatus::Queued { .. }
        ));
    }

    #[tokio::test]
    async fn hls_access_lease_sync_zero_to_zero_is_idempotent() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                3_000,
            )
            .await;

        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 0);
        assert!(matches!(
            session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert_eq!(session.activity.origin_work_generation, old_generation);
    }

    #[tokio::test]
    async fn hls_access_lease_gc_prepass_releases_user_without_detaching_origin_binding() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let session = create_bound_hls_test_session(&app_state, &input, "12345", "account-a", 1_000).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        activate_test_hls_access_lease(&app_state, &proxy_session_id, "gc-expired-lease", 1_000, 1_000).await;
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                1_000,
            )
            .await;
        let old_generation = session.read().await.activity.origin_work_generation;

        app_state
            .hls_proxy
            .sync_all_session_access_leases_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                3_000,
            )
            .await;
        let _ = app_state.hls_proxy.run_garbage_collection_once(3_000).await.expect("gc should run");

        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("detach keeps shared hls session");
        let session = session.read().await;
        assert_eq!(session.activity.active_access_lease_count, 0);
        assert!(matches!(
            session.origin_account_binding.as_ref().expect("binding exists").binding_mode,
            HlsOriginAccountBindingMode::Active
        ));
        assert_eq!(session.activity.origin_work_generation, old_generation);
    }

    fn test_addr() -> SocketAddr { "127.0.0.1:55123".parse().unwrap_or_else(|_| unreachable!()) }

    fn test_addr_with_port(port: u16) -> SocketAddr { SocketAddr::from(([127, 0, 0, 1], port)) }

    fn test_fingerprint() -> Fingerprint { Fingerprint::new("test".to_string(), "127.0.0.1".to_string(), test_addr()) }

    fn test_fingerprint_with_addr(addr: SocketAddr) -> Fingerprint {
        Fingerprint::new(format!("test-{}", addr.port()), "127.0.0.1".to_string(), addr)
    }

    async fn create_active_hls_user_session(app_state: &Arc<AppState>) {
        create_active_hls_user_session_with(
            app_state,
            "hls-session-token",
            "origin-provider",
            "http://origin.example.com/live/12345.m3u8",
            test_addr(),
        )
        .await;
    }

    async fn create_active_hls_user_session_with(
        app_state: &Arc<AppState>,
        session_token: &str,
        provider: &str,
        stream_url: &str,
        addr: SocketAddr,
    ) {
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        user.max_connections = 1;
        app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token,
                virtual_id: 12345,
                provider,
                stream_url,
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
    }

    fn test_hls_access_context(
        proxy_session_id: ProxySessionId,
        access_lease_id: HlsAccessLeaseId,
    ) -> HlsAccessContext {
        test_hls_access_context_with(proxy_session_id, access_lease_id, "hls-session-token", test_fingerprint().key)
    }

    fn test_hls_access_context_with(
        proxy_session_id: ProxySessionId,
        access_lease_id: HlsAccessLeaseId,
        user_session_token: &str,
        client_fingerprint: String,
    ) -> HlsAccessContext {
        HlsAccessContext {
            username: "hls-user".to_string(),
            user_session_token: user_session_token.to_string(),
            proxy_session_id,
            input_id: 1,
            stream_ref: "12345".to_string(),
            virtual_id: 12345,
            lease_id: access_lease_id,
            family_key: HlsPlaybackFamilyKey::new("hls-user", client_fingerprint),
        }
    }

    async fn prepare_pending_test_hls_access_lease(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        access_lease_id: &HlsAccessLeaseId,
    ) {
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                access_lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                super::current_time_millis(),
                super::hls_access_lease_ttl_ms(app_state),
            ))
            .await;
    }

    #[tokio::test]
    async fn hls_cache_manifest_cold_start_synchronously_returns_initial_manifest() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
        let input_name = Arc::<str>::from("test-input");
        let input = ConfigInput {
            id: 1,
            name: Arc::clone(&input_name),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: &request_url,
                input: &input,
                origin_source,
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("hls cache should handle valid live hls entrypoint");

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:123"));
        assert!(body.contains(&format!("/proxy/hls/live/{}/{}/000123.ts", proxy_session_id.0, access_lease_id.0)));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_key(&session_key)
            .await
            .expect("cold start should create shared hls session");
        {
            let session = session.read().await;
            assert!(session.last_rendered_manifest.is_some());
            let binding = session.origin_account_binding.as_ref().expect("plain http input still has account binding");
            assert_eq!(binding.input_name.as_ref(), "test-input");
            assert_eq!(binding.account_name.as_ref(), "test-input");
        }
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn canonical_recovery_from_provisioning_marks_normal_handoff_boundary() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
        app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), 12345, super::current_time_millis());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: &request_url,
                input: &input,
                origin_source,
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("canonical hls cache should recover from provisioning");

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
        assert!(body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.000,"));
        assert!(!app_state.hls_provisioning.has_consumer(&input.name, 12345, super::current_time_millis()));
    }

    #[tokio::test]
    async fn canonical_recovery_from_provisioning_marks_transient_handoff_boundary() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:123\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.key\"\n#EXTINF:4.0,\n000123.ts\n#EXTINF:4.0,\n000124.ts\n#EXTINF:4.0,\n000125.ts\n",
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;
        app_state.hls_provisioning.touch_consumer(Arc::clone(&input.name), 12345, super::current_time_millis());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: &request_url,
                input: &input,
                origin_source,
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("canonical hls cache should recover from provisioning");

        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");
        assert!(body.contains("#EXT-X-DISCONTINUITY\n#EXTINF:4.0,"));
        assert!(!app_state.hls_provisioning.has_consumer(&input.name, 12345, super::current_time_millis()));
    }

    #[tokio::test]
    async fn hls_cache_manifest_cold_start_supports_m3u_hls_origin_source() {
        let origin = spawn_test_segment_origin(
            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:77\n#EXTINF:4.0,\nseg-77.ts\n#EXTINF:4.0,\nseg-78.ts\n#EXTINF:4.0,\nseg-79.ts\n",
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("m3u-input"),
            input_type: InputType::M3u,
            url: origin.base_url.clone(),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/channel/index.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: &request_url,
                session_entry_url: &request_url,
                input: &input,
                origin_source,
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            None,
            "/m3u-stream/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("hls cache should handle m3u hls media playlists");

        assert_eq!(response.status(), StatusCode::OK);
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_key(&session_key)
            .await
            .expect("m3u hls should create shared session");
        let session = session.read().await;
        assert_eq!(session.origin_source.source_kind, crate::api::model::HlsOriginSourceKind::M3uMediaPlaylist);
        let binding = session.origin_account_binding.as_ref().expect("m3u hls input still has account binding");
        assert_eq!(binding.input_name.as_ref(), "m3u-input");
        assert_eq!(binding.account_name.as_ref(), "m3u-input");
    }

    #[tokio::test]
    async fn hls_cache_manifest_cold_start_unreachable_origin_returns_service_unavailable() {
        let hls_dto = HlsCacheConfigDto { origin_manifest_timeout_ms: 1, ..Default::default() };
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)));
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let input_name = Arc::<str>::from("test-input");
        let input = ConfigInput { id: 1, name: Arc::clone(&input_name), ..ConfigInput::default() };
        let request_url = "http://127.0.0.1:9/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: request_url,
                session_entry_url: request_url,
                input: &input,
                origin_source,
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("hls cache should handle valid live hls entrypoint");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry after"), "2");
    }

    #[tokio::test]
    async fn hls_cache_manifest_cold_start_client_abort_does_not_leave_refresh_in_flight() {
        let origin = spawn_test_transient_origin_with_delayed_response(
            "200 OK",
            &[("Content-Type", "application/vnd.apple.mpegurl")],
            "#EXTM3U\n#EXT-X-VERSION:3\n",
            Duration::from_millis(100),
        )
        .await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("test-input"),
            input_type: InputType::Xtream,
            url: origin.base_url.clone(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let hls_dto = HlsCacheConfigDto { origin_manifest_timeout_ms: 1_000, ..Default::default() };
        let hls_config = HlsCacheConfig::from(&hls_dto);
        let app_state = test_app_state_with_hls_proxy_and_inputs(
            Arc::new(HlsProxyManager::with_hls_cache_config(&hls_config)),
            vec![Arc::new(input.clone())],
        );
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let request_url = format!("{}/live/user/pass/12345.m3u8", origin.base_url);
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        prepare_pending_test_hls_access_lease(&app_state, &proxy_session_id, &access_lease_id).await;

        let app_state_for_request = Arc::clone(&app_state);
        let request_handle = tokio::spawn(async move {
            super::try_hls_cache_canonical_manifest_response(
                &app_state_for_request,
                &test_fingerprint(),
                &access_context,
                &proxy_session_id,
                &access_lease_id,
                HlsAccessLeaseState::Pending,
                super::HlsCacheManifestOrigin {
                    raw_request_url: &request_url,
                    session_entry_url: &request_url,
                    input: &input,
                    origin_source,
                    failover_provider: None,
                },
                HeaderMap::new(),
                60,
                None,
                "/live/hls-user/hls-pass/12345.m3u8",
            )
            .await
        });

        let session = wait_for_hls_test_session(&app_state, &session_key).await;
        wait_for_hls_refresh_in_flight(&session).await;
        request_handle.abort();
        let _ = request_handle.await;

        for _ in 0..200 {
            if !session.read().await.origin_refresh.in_flight {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let session = session.read().await;
        assert!(!session.origin_refresh.in_flight);
        assert!(session.origin_refresh.last_fetch_finished_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_canonical_prepare_service_unavailable_sets_retry_after() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let input = ConfigInput {
            id: 1,
            name: Arc::from("alias-input"),
            aliases: Some(vec![crate::model::ConfigInputAlias {
                id: 2,
                name: Arc::from("alias-account"),
                url: "http://alias.example.com".to_string(),
                username: Some("alias-user".to_string()),
                password: Some("alias-pass".to_string()),
                priority: 0,
                max_connections: 1,
                exp_date: None,
                enabled: true,
            }]),
            ..ConfigInput::default()
        };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: request_url,
                session_entry_url: request_url,
                input: &input,
                origin_source,
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("canonical hls cache response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry after"), "2");
    }

    #[tokio::test]
    async fn hls_origin_account_rebind_failure_sets_backoff_without_changing_session_identity() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let input = ConfigInput { id: 1, name: Arc::from("stale-input"), ..ConfigInput::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());
        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key.clone(),
                origin_source.clone(),
                &app_state.get_encrypt_secret(),
                1_000,
            )
            .await;
        {
            let mut session_guard = session.write().await;
            session_guard.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::clone(&input.name),
                Arc::from("removed-account"),
                &proxy_session_id,
                1_000,
            ));
        }

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: request_url,
                session_entry_url: request_url,
                input: &input,
                origin_source,
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            None,
            "/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("canonical hls cache response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).expect("retry after"), "2");
        let session_guard = session.read().await;
        assert_eq!(session_guard.key, session_key);
        assert_eq!(session_guard.proxy_session_id, proxy_session_id);
        let binding = session_guard.origin_account_binding.as_ref().expect("stale binding remains");
        assert_eq!(binding.account_name.as_ref(), "removed-account");
        assert_eq!(binding.generation, 0);
        assert_eq!(session_guard.origin_account_rebind.consecutive_rebind_failures, 1);
        assert!(session_guard.origin_account_rebind.next_rebind_allowed_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_entry_returns_temporary_redirect_without_origin_refresh_or_session_creation() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();

        let response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            Some("/iptv"),
        )
        .await
        .expect("hls cache entry should redirect");

        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.headers().get(header::CACHE_CONTROL).expect("cache control"), "no-store");
        let location =
            response.headers().get(header::LOCATION).and_then(|value| value.to_str().ok()).expect("location header");
        assert!(location.starts_with("/iptv/proxy/hls/live/"));
        assert!(location.ends_with("/manifest.m3u8"));
        let access_lease_id =
            location.trim_end_matches("/manifest.m3u8").rsplit('/').next().expect("access lease id in redirect");
        assert_eq!(access_lease_id.len(), 22);
        assert!(app_state.hls_proxy.sessions().get_by_key(&session_key).await.is_none());
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    #[tokio::test]
    async fn hls_cache_entry_leases_update_effective_origin_acquire_policy_for_shared_session() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut soft_user = ProxyUserCredentials::default();
        soft_user.username = "soft-user".to_string();
        soft_user.soft_priority = 20;
        let mut normal_user = ProxyUserCredentials::default();
        normal_user.username = "normal-user".to_string();
        normal_user.priority = -5;
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let session_key = origin_source.session_key();
        let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());

        let soft_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &soft_user,
            origin_source.clone(),
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Soft,
            None,
        )
        .await
        .expect("soft hls cache entry should redirect");
        assert_eq!(soft_response.status(), StatusCode::TEMPORARY_REDIRECT);
        let soft_snapshot =
            app_state.hls_proxy.access_lease_session_snapshot(&proxy_session_id, super::current_time_millis()).await;
        let soft_policy = soft_snapshot.effective_origin_policy.expect("soft policy");
        assert_eq!(soft_policy.connection_kind, ConnectionKind::Soft);
        assert_eq!(soft_policy.priority, soft_user.soft_priority);

        // Different user/family, same shared HLS session. Normal media admission must upgrade
        // the future origin-account acquire policy without changing the shared session identity.
        let normal_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &normal_user,
            origin_source,
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("normal hls cache entry should redirect");
        assert_eq!(normal_response.status(), StatusCode::TEMPORARY_REDIRECT);
        let normal_snapshot =
            app_state.hls_proxy.access_lease_session_snapshot(&proxy_session_id, super::current_time_millis()).await;
        let normal_policy = normal_snapshot.effective_origin_policy.expect("normal policy");
        assert_eq!(normal_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(normal_policy.priority, normal_user.priority);

        let (session, _) = app_state
            .hls_proxy
            .get_or_create_session_with_source_and_outcome(
                session_key,
                super::build_hls_origin_source(&input, "12345"),
                &app_state.get_encrypt_secret(),
                super::current_time_millis(),
            )
            .await;
        app_state
            .hls_proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                &session,
                &proxy_session_id,
                super::current_time_millis(),
            )
            .await;
        let session_policy = session.read().await.effective_origin_acquire_policy_or_default();
        assert_eq!(session_policy.connection_kind, ConnectionKind::Normal);
        assert_eq!(session_policy.priority, normal_user.priority);
    }

    #[tokio::test]
    async fn hls_entry_origin_reservation_requires_real_provider_handle() {
        let app_state = test_app_state();
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = single_hls_provider_input("missing-provider");

        let reservation = super::try_reserve_hls_entry_origin_account_for_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            &input,
            12345,
            "http://origin.example.com/live/source-user/source-pass/12345.m3u8",
            "hls-session-token",
            "hls-cache:test-session",
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            false,
        )
        .await;

        assert!(reservation.is_none(), "provisioning redirect must not use an exhausted/counter-only check");
    }

    #[tokio::test]
    async fn hls_entry_origin_reservation_sets_owner_reservation_before_redirect() {
        let input = single_hls_provider_input("available-provider");
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let session_owner = "hls-cache:test-session";

        let reservation = super::try_reserve_hls_entry_origin_account_for_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            &input,
            12345,
            "http://account.example.com/live/account-user/account-pass/12345.m3u8",
            "hls-session-token",
            session_owner,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            false,
        )
        .await
        .expect("provider reservation should be acquired before provisioning redirect");

        assert_eq!(reservation.request_url, "http://account.example.com/live/account-user/account-pass/12345.m3u8");
        assert!(reservation.selected_provider_config.is_some());
        assert!(app_state.active_users.active_streams().await.is_empty());
        app_state.connection_manager.release_provider_handle(reservation.provider_handle).await;

        assert!(
            app_state
                .active_provider
                .acquire_connection_with_grace_for_session(
                    &input.name,
                    &test_addr_with_port(55251),
                    false,
                    0,
                    ConnectionKind::Normal,
                    Some("other-owner"),
                )
                .await
                .is_none(),
            "reserved provider must stay blocked for other HLS sessions"
        );

        let same_owner_handle = app_state
            .active_provider
            .acquire_connection_with_grace_for_session(
                &input.name,
                &test_addr_with_port(55252),
                false,
                0,
                ConnectionKind::Normal,
                Some(session_owner),
            )
            .await;
        assert!(same_owner_handle.is_some(), "reserved provider must be reusable by the same HLS session owner");
        app_state.connection_manager.release_provider_handle(same_owner_handle).await;
    }

    #[tokio::test]
    async fn hls_cache_entry_reuses_existing_lease_for_same_user_session_and_proxy_session() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");

        let first_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("first hls cache entry should redirect");
        let first_location = first_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("first location header");
        let proxy_session_id = ProxySessionId(proxy_session_id_from_redirect_location(first_location).to_string());
        let first_access_lease_id =
            HlsAccessLeaseId(access_lease_id_from_redirect_location(first_location).to_string());
        let first_session_token =
            access_lease_session_token(&app_state, &proxy_session_id, &first_access_lease_id).await;

        let second_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("second hls cache entry should redirect");
        let second_location = second_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("second location header");
        assert_eq!(
            access_lease_id_from_redirect_location(first_location),
            access_lease_id_from_redirect_location(second_location)
        );
        let second_access_lease_id =
            HlsAccessLeaseId(access_lease_id_from_redirect_location(second_location).to_string());
        let second_session_token =
            access_lease_session_token(&app_state, &proxy_session_id, &second_access_lease_id).await;
        assert_eq!(first_session_token, second_session_token);
    }

    #[tokio::test]
    async fn hls_cache_entry_reuses_existing_lease_after_manifest_touch() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");

        let first_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("first hls cache entry should redirect");
        let first_location = first_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("first location header");
        let first_lease_id = HlsAccessLeaseId(access_lease_id_from_redirect_location(first_location).to_string());
        let proxy_session_id = ProxySessionId(proxy_session_id_from_redirect_location(first_location).to_string());
        let first_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_lease_id).await;
        let now_ms = super::current_time_millis();

        assert!(matches!(
            app_state
                .hls_proxy
                .touch_manifest_access_lease(
                    &first_lease_id,
                    &proxy_session_id,
                    now_ms,
                    None,
                    super::hls_access_lease_ttl_ms(&app_state),
                )
                .await,
            crate::api::model::HlsAccessLeaseTouch::Touched { .. }
        ));

        let second_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("second hls cache entry should redirect");
        let second_location = second_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("second location header");

        assert_eq!(
            access_lease_id_from_redirect_location(first_location),
            access_lease_id_from_redirect_location(second_location)
        );
        let second_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_lease_id).await;
        assert_eq!(first_session_token, second_session_token);
    }

    #[tokio::test]
    async fn hls_cache_entry_does_not_reuse_activated_access_lease() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");

        let first_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("first hls cache entry should redirect");
        let first_location = first_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("first location header");
        let first_lease_id = HlsAccessLeaseId(access_lease_id_from_redirect_location(first_location).to_string());
        let proxy_session_id = ProxySessionId(proxy_session_id_from_redirect_location(first_location).to_string());
        let now_ms = super::current_time_millis();
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &first_lease_id,
                &proxy_session_id,
                now_ms,
                HlsAccessLeaseTiming {
                    active_window_ms: 5_000,
                    valid_window_ms: super::hls_access_lease_ttl_ms(&app_state),
                },
            )
            .await
            .is_activated());

        let second_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("second hls cache entry should redirect");
        let second_location = second_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("second location header");
        let second_lease_id = HlsAccessLeaseId(access_lease_id_from_redirect_location(second_location).to_string());

        assert_ne!(first_lease_id, second_lease_id);
        let first_session_token = access_lease_session_token(&app_state, &proxy_session_id, &first_lease_id).await;
        let second_session_token = access_lease_session_token(&app_state, &proxy_session_id, &second_lease_id).await;
        assert_ne!(first_session_token, second_session_token);
        assert!(
            app_state
                .hls_proxy
                .touch_access_lease(
                    &first_lease_id,
                    super::current_time_millis(),
                    HlsAccessLeaseTiming {
                        active_window_ms: 5_000,
                        valid_window_ms: super::hls_access_lease_ttl_ms(&app_state),
                    },
                )
                .await
        );
    }

    #[tokio::test]
    async fn hls_cache_entry_creates_new_session_token_after_reuse_window_expires() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let proxy_session_id = build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());
        let old_lease_id = HlsAccessLeaseId("old-pending-lease".to_string());
        let old_session_token = "old-hls-session-token";
        let old_issued_at_ms = super::current_time_millis().saturating_sub(6_000);
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                old_lease_id.clone(),
                HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key),
                proxy_session_id.clone(),
                "hls-user".to_string(),
                old_session_token.to_string(),
                1,
                "12345".to_string(),
                12345,
                old_issued_at_ms,
                super::hls_access_lease_ttl_ms(&app_state),
            ))
            .await;

        let response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("hls cache entry should redirect");
        let location =
            response.headers().get(header::LOCATION).and_then(|value| value.to_str().ok()).expect("location header");
        let new_lease_id = HlsAccessLeaseId(access_lease_id_from_redirect_location(location).to_string());
        let new_session_token = access_lease_session_token(&app_state, &proxy_session_id, &new_lease_id).await;

        assert_ne!(old_lease_id, new_lease_id);
        assert_ne!(old_session_token, new_session_token);
    }

    #[tokio::test]
    async fn hls_cache_parallel_real_playbacks_same_virtual_id_register_distinct_streams() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput { id: 1, name: Arc::from("test-input"), ..Default::default() };
        let request_url = "http://origin.example.com/live/user/pass/12345.m3u8";
        let origin_source = super::build_hls_origin_source(&input, "12345");
        let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;

        let first_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source.clone(),
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("first hls cache entry should redirect");
        let first_location = first_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("first location header");
        let first_lease_id = access_lease_id_from_redirect_location(first_location);
        assert_eq!(proxy_session_id_from_redirect_location(first_location), proxy_session_id);
        let proxy_session = ProxySessionId(proxy_session_id.clone());
        let first_session_token =
            access_lease_session_token(&app_state, &proxy_session, &HlsAccessLeaseId(first_lease_id.to_string())).await;
        let first_segment_uri = format!("/proxy/hls/live/{proxy_session_id}/{first_lease_id}/000123.ts");
        assert_eq!(get_status(Arc::clone(&app_state), &first_segment_uri).await, StatusCode::OK);

        let second_response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            12345,
            None,
            request_url,
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("second hls cache entry should redirect");
        let second_location = second_response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("second location header");
        let second_lease_id = access_lease_id_from_redirect_location(second_location);
        assert_ne!(first_lease_id, second_lease_id);
        assert_eq!(proxy_session_id_from_redirect_location(second_location), proxy_session_id);
        let second_session_token =
            access_lease_session_token(&app_state, &proxy_session, &HlsAccessLeaseId(second_lease_id.to_string()))
                .await;
        assert_ne!(first_session_token, second_session_token);
        let second_segment_uri = format!("/proxy/hls/live/{proxy_session_id}/{second_lease_id}/000123.ts");
        assert_eq!(get_status(Arc::clone(&app_state), &second_segment_uri).await, StatusCode::OK);

        let streams = app_state.active_users.active_streams().await;
        assert_eq!(streams.len(), 2);
        let first_stream = streams
            .iter()
            .find(|stream| stream.session_token.as_deref() == Some(first_session_token.as_str()))
            .expect("first stream should be registered");
        let second_stream = streams
            .iter()
            .find(|stream| stream.session_token.as_deref() == Some(second_session_token.as_str()))
            .expect("second stream should be registered");
        let shared_stream_id = super::hls_cache_shared_stream_id(&proxy_session);
        assert_ne!(first_stream.session_token, second_stream.session_token);
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(second_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
        assert_eq!(second_stream.channel.shared_joined_existing, Some(true));
    }

    #[tokio::test]
    async fn hls_cache_entry_redirect_for_xtream_uses_stream_ref_session_identity() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input = ConfigInput {
            id: 7,
            name: Arc::from("xtream-input"),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let origin_source = super::build_hls_origin_source(&input, "80510");
        let expected_proxy_session_id =
            build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());

        let response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            80510,
            None,
            "http://origin.example.com/live/user/pass/80510.m3u8",
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            None,
        )
        .await
        .expect("xtream hls cache entry should redirect");

        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        let location =
            response.headers().get(header::LOCATION).and_then(|value| value.to_str().ok()).expect("location header");
        assert!(location.starts_with(&format!("/proxy/hls/live/{}/", expected_proxy_session_id.0)));
        assert!(location.ends_with("/manifest.m3u8"));
        assert!(app_state.hls_proxy.sessions().get_by_key(&HlsSessionKey::new(7, "80510")).await.is_none());
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
    }

    #[tokio::test]
    async fn hls_cache_entry_redirect_for_m3u_uses_stream_ref_session_identity() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let mut user = ProxyUserCredentials::default();
        user.username = "hls-user".to_string();
        let input =
            ConfigInput { id: 9, name: Arc::from("m3u-input"), input_type: InputType::M3u, ..ConfigInput::default() };
        let origin_source = super::build_hls_origin_source(&input, "70001");
        let expected_proxy_session_id =
            build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());

        let response = super::try_hls_cache_entry_redirect(
            &app_state,
            &test_fingerprint(),
            &user,
            origin_source,
            70001,
            None,
            "http://media.example.com/channel/playlist.m3u8",
            &input,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            Some("/iptv"),
        )
        .await
        .expect("m3u hls cache entry should redirect");

        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        let location =
            response.headers().get(header::LOCATION).and_then(|value| value.to_str().ok()).expect("location header");
        assert!(location.starts_with(&format!("/iptv/proxy/hls/live/{}/", expected_proxy_session_id.0)));
        assert!(location.ends_with("/manifest.m3u8"));
        assert!(app_state.hls_proxy.sessions().get_by_key(&HlsSessionKey::new(9, "70001")).await.is_none());
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
    }

    #[tokio::test]
    async fn hls_proxy_manifest_invalid_token_starts_no_origin_work() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);

        let response = get_response(
            Arc::clone(&app_state),
            "/proxy/hls/live/a8f31c9eQ7sLk92pV0mTaw/not-a-valid-token/manifest.m3u8",
            None,
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(app_state.hls_proxy.metrics().snapshot().refresh_started, 0);
        assert!(app_state.hls_proxy.sessions().is_empty().await);
    }

    #[tokio::test]
    async fn hls_cache_manifest_response_applies_current_users_server_path_without_mutating_session_body() {
        let input_name = Arc::<str>::from("test-input");
        let input = ConfigInput {
            id: 1,
            name: Arc::clone(&input_name),
            input_type: InputType::Xtream,
            url: "http://origin.example.com".to_string(),
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            max_connections: 1,
            enabled: true,
            ..ConfigInput::default()
        };
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        enable_hls_cache(&app_state);
        create_active_hls_user_session(&app_state).await;
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.origin_refresh.next_fetch_allowed_at_ms = u64::MAX;
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.last_rendered_manifest = Some(RenderedManifest {
                body: format!(
                    "#EXTM3U\n#EXT-X-MAP:URI=\"/proxy/hls/live/{proxy_session_id}/{}/map/000000.mp4\"\n#EXTINF:4.0,\n/proxy/hls/live/{proxy_session_id}/{}/000123.ts\n",
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER,
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                ),
                first_proxy_seq: 123,
                last_proxy_seq: 123,
                playlist_duration_ms: 4_000,
                render_gap_segments: 0,
                rendered_at_ms: 100,
                segment_proxy_seqs: vec![123],
            });
            proxy_session_id
        };
        let proxy_session_id = ProxySessionId(proxy_session_id);
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let access_context = test_hls_access_context(proxy_session_id.clone(), access_lease_id.clone());

        let response = super::try_hls_cache_canonical_manifest_response(
            &app_state,
            &test_fingerprint(),
            &access_context,
            &proxy_session_id,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            super::HlsCacheManifestOrigin {
                raw_request_url: "http://origin.example.com/live/user/pass/12345.m3u8",
                session_entry_url: "http://origin.example.com/live/user/pass/12345.m3u8",
                input: &input,
                origin_source: super::build_hls_origin_source(&input, "12345"),
                failover_provider: None,
            },
            HeaderMap::new(),
            60,
            Some("/iptv"),
            "/live/hls-user/hls-pass/12345.m3u8",
        )
        .await
        .expect("hls cache should handle valid live hls entrypoint");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest should be utf8");

        assert!(body.contains(&format!("/iptv/proxy/hls/live/{}/", proxy_session_id.0)));
        assert!(body.contains("/map/000000.mp4"));
        assert!(body.contains("/000123.ts"));
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        let stored_body = session.read().await.last_rendered_manifest.as_ref().expect("stored manifest").body.clone();
        assert!(stored_body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert!(stored_body.contains(&format!("/proxy/hls/live/{}/", proxy_session_id.0)));
        assert!(!stored_body.contains("/iptv/proxy/hls/live/"));
        assert!(!stored_body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    fn transient_manifest_body(proxy_session_id: &str) -> String {
        let mut body = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n".to_string();
        for index in 0..6 {
            body.push_str("#EXTINF:10.0,\n");
            let _ = writeln!(
                body,
                "/proxy/hls/live/{proxy_session_id}/{}/r/seg{index}.ts",
                crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
            );
        }
        body
    }

    fn media_uri_count(body: &str) -> usize {
        body.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).count()
    }

    #[tokio::test]
    async fn hls_cache_pending_transient_manifest_applies_initial_strip_without_mutating_shared_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            session.mark_authorized_media_access(super::current_time_millis());
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 3 };
        let stored_before = session.read().await.transient.last_manifest_body.clone().expect("transient manifest body");

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
        assert_eq!(
            session.read().await.transient.last_manifest_body.as_ref().expect("stored manifest"),
            &stored_before
        );
        assert_eq!(media_uri_count(&stored_before), 6);
    }

    #[tokio::test]
    async fn hls_cache_activated_transient_manifest_skips_initial_strip() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            session.mark_authorized_media_access(super::current_time_millis().saturating_sub(16_000));
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 3 };

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await
        .expect("transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 6);
        assert!(body.contains(&access_lease_id.0));
        assert!(!body.contains(crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER));
    }

    #[tokio::test]
    async fn hls_cache_transient_manifest_without_media_activity_is_not_served_from_committed_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 3 };

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn hls_cache_no_media_yet_transient_manifest_is_served_for_initial_canonical_response() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 3 };

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::ZERO),
        )
        .await
        .expect("initial transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_transient_manifest_outside_soft_window_is_not_served_from_committed_body() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let _proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            let proxy_session_id = session.proxy_session_id.0.clone();
            session.transient.replace_manifest(transient_manifest_body(&proxy_session_id), 100);
            session.mark_authorized_media_access(super::current_time_millis().saturating_sub(60_000));
            proxy_session_id
        };
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 3 };

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Activated,
            &strip,
            None,
            super::HlsCachedManifestOptions::committed_only(Duration::ZERO),
        )
        .await;

        assert!(response.is_none());
    }

    #[tokio::test]
    async fn hls_cache_no_media_yet_waits_for_first_normal_manifest_commit() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.origin_refresh.in_flight = true;
            session.proxy_session_id.clone()
        };
        let session_for_commit = Arc::clone(&session);
        let proxy_session_for_body = proxy_session_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut session = session_for_commit.write().await;
            session.last_rendered_manifest = Some(RenderedManifest {
                body: format!(
                    "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:4.0,\n/proxy/hls/live/{proxy_session_for_body}/{}/000100.ts\n",
                    crate::api::model::HLS_ACCESS_LEASE_ID_PLACEHOLDER
                ),
                first_proxy_seq: 100,
                last_proxy_seq: 100,
                playlist_duration_ms: 4_000,
                render_gap_segments: 0,
                rendered_at_ms: 100,
                segment_proxy_seqs: vec![100],
            });
            session.origin_refresh.in_flight = false;
        });
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 0 };

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
        )
        .await
        .expect("initial normal manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_no_media_yet_waits_for_first_transient_manifest_commit() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            session.origin_refresh.in_flight = true;
            session.proxy_session_id.clone()
        };
        let session_for_commit = Arc::clone(&session);
        let proxy_session_for_body = proxy_session_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut session = session_for_commit.write().await;
            session
                .transient
                .replace_manifest(transient_manifest_body(&proxy_session_for_body), super::current_time_millis());
            session.origin_refresh.in_flight = false;
        });
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 3 };

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
        )
        .await
        .expect("initial transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    #[tokio::test]
    async fn hls_cache_expired_transient_manifest_waits_for_revalidation_commit() {
        let app_state = test_app_state();
        enable_hls_cache(&app_state);
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            session.mark_authorized_media_access(super::current_time_millis().saturating_sub(60_000));
            session.origin_refresh.in_flight = true;
            session.proxy_session_id.clone()
        };
        let session_for_commit = Arc::clone(&session);
        let proxy_session_for_body = proxy_session_id.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut session = session_for_commit.write().await;
            session
                .transient
                .replace_manifest(transient_manifest_body(&proxy_session_for_body), super::current_time_millis());
            session.origin_refresh.in_flight = false;
        });
        let access_lease_id = HlsAccessLeaseId("access-lease".to_string());
        let strip = StripConfig { mode: StripMode::Segments, value: 3 };

        let response = super::try_hls_cached_manifest_response(
            &app_state,
            &session,
            &access_lease_id,
            HlsAccessLeaseState::Pending,
            &strip,
            None,
            super::HlsCachedManifestOptions::initial(Duration::from_millis(200)),
        )
        .await
        .expect("revalidated transient manifest response");
        let body = String::from_utf8(response_body(response).await.to_vec()).expect("manifest utf8");

        assert_eq!(media_uri_count(&body), 3);
        assert!(body.contains(&access_lease_id.0));
        assert!(session.read().await.activity.last_authorized_media_at_ms.is_some());
    }

    async fn grant_hls_proxy_lease(app_state: &Arc<AppState>, proxy_session_id: &str) -> String {
        create_active_hls_user_session(app_state).await;
        let now_ms = super::current_time_millis();
        let lease_id = HlsAccessLeaseId(format!("test-access-lease-{proxy_session_id}"));
        let family_key = HlsPlaybackFamilyKey::new("hls-user", test_fingerprint().key);
        app_state
            .hls_proxy
            .prepare_access_lease(HlsAccessLease::pending(
                lease_id.clone(),
                family_key,
                ProxySessionId(proxy_session_id.to_string()),
                "hls-user".to_string(),
                "hls-session-token".to_string(),
                1,
                "12345".to_string(),
                12345,
                now_ms,
                super::hls_access_lease_ttl_ms(app_state),
            ))
            .await;
        assert!(app_state
            .hls_proxy
            .activate_access_lease(
                &lease_id,
                &ProxySessionId(proxy_session_id.to_string()),
                now_ms,
                HlsAccessLeaseTiming {
                    active_window_ms: 5_000,
                    valid_window_ms: super::hls_access_lease_ttl_ms(app_state),
                },
            )
            .await
            .is_activated());
        lease_id.0
    }

    async fn hls_proxy_uri(app_state: &Arc<AppState>, proxy_session_id: &str, suffix: &str) -> String {
        let access_lease_id = grant_hls_proxy_lease(app_state, proxy_session_id).await;
        format!("/proxy/hls/live/{proxy_session_id}/{access_lease_id}/{suffix}")
    }

    async fn wait_for_provider_connection_count(app_state: &Arc<AppState>, expected: usize) {
        for _ in 0..50 {
            let actual = app_state.active_provider.get_provider_connections_count().await;
            if actual == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(app_state.active_provider.get_provider_connections_count().await, expected);
    }

    fn normal_manifest(body: &str) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, "http://origin.example.com/live/final/index.m3u8") {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    async fn map_segment(app_state: &Arc<AppState>, proxy_seq: u64, extension: &str) -> String {
        map_segment_with_origin_url(app_state, proxy_seq, extension, &format!("{proxy_seq}.{extension}")).await
    }

    async fn map_segment_with_origin_url(
        app_state: &Arc<AppState>,
        proxy_seq: u64,
        _extension: &str,
        origin_url: &str,
    ) -> String {
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let manifest =
            normal_manifest(&format!("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:{proxy_seq}\n#EXTINF:4.0,\n{origin_url}\n"));
        let mut session = session.write().await;
        session.apply_origin_manifest(&manifest).expect("manifest should map");
        session.proxy_session_id.0.clone()
    }

    async fn map_ready_segment(app_state: &Arc<AppState>, proxy_seq: u64, extension: &str, body: &[u8]) -> String {
        let proxy_session_id = map_ready_segment_without_lease(app_state, proxy_seq, extension, body).await;
        grant_hls_proxy_lease(app_state, &proxy_session_id).await;
        proxy_session_id
    }

    async fn map_ready_segment_without_lease(
        app_state: &Arc<AppState>,
        proxy_seq: u64,
        extension: &str,
        body: &[u8],
    ) -> String {
        let proxy_session_id = map_segment(app_state, proxy_seq, extension).await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&crate::api::model::ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        let cache_key = {
            let session = session.read().await;
            session.segments.get(&proxy_seq).expect("segment should be mapped").cache_key.clone()
        };
        let metadata = app_state
            .hls_proxy
            .segment_cache()
            .write_bytes_and_commit(&cache_key, body)
            .await
            .expect("cache commit should succeed");
        {
            let mut session = session.write().await;
            session.segments.get_mut(&proxy_seq).expect("segment should be mapped").status =
                SegmentCacheStatus::Ready { content_length: metadata.size, ready_at_ms: 200 };
        }
        proxy_session_id
    }

    async fn map_hls_map(app_state: &Arc<AppState>, body: &[u8], grant_lease: bool) -> String {
        let session = app_state
            .hls_proxy
            .get_or_create_session(HlsSessionKey::new(1, "12345"), &app_state.get_encrypt_secret(), 100)
            .await;
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n000123.m4s\n");
        let proxy_session_id = {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest should map");
            session.proxy_session_id.0.clone()
        };
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&crate::api::model::ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        let cache_key = {
            let session = session.read().await;
            session.maps.get(&ProxyMapId(0)).expect("map should be mapped").cache_key.clone()
        };
        let metadata = app_state
            .hls_proxy
            .segment_cache()
            .write_bytes_and_commit(&cache_key, body)
            .await
            .expect("map cache commit should succeed");
        {
            let mut session = session.write().await;
            session.maps.get_mut(&ProxyMapId(0)).expect("map should be mapped").status =
                MapCacheStatus::Ready { content_length: metadata.size, ready_at_ms: 200 };
        }
        if grant_lease {
            grant_hls_proxy_lease(app_state, &proxy_session_id).await;
        }
        proxy_session_id
    }

    async fn map_transient_resource(
        app_state: &Arc<AppState>,
        origin_url: &str,
        extension: &str,
        grant_lease: bool,
    ) -> (String, String) {
        map_transient_resource_with_kind(app_state, origin_url, extension, grant_lease, TransientResourceKind::Segment)
            .await
    }

    async fn map_transient_resource_with_kind(
        app_state: &Arc<AppState>,
        origin_url: &str,
        extension: &str,
        grant_lease: bool,
        kind: TransientResourceKind,
    ) -> (String, String) {
        let secret = b"rewrite-secret";
        let now_ms = super::current_time_millis();
        let session = app_state.hls_proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), secret, now_ms).await;
        let resource_id = build_transient_resource_id(origin_url, secret);
        let proxy_session_id = {
            let mut session = session.write().await;
            session.mode =
                HlsSessionMode::TransientPassthrough { reason: crate::api::model::TransientPassthroughReason::ExtXKey };
            session.transient.upsert_resources([TransientResourceRef::new(
                kind,
                origin_url,
                secret,
                now_ms,
                300_000,
                Some(extension.to_string()),
            )]);
            session.proxy_session_id.0.clone()
        };
        if grant_lease {
            grant_hls_proxy_lease(app_state, &proxy_session_id).await;
        }
        (proxy_session_id, resource_id.0)
    }

    async fn get_response(app_state: Arc<AppState>, uri: &str, range: Option<&str>) -> Response<Body> {
        let router = hls_api_register().with_state(app_state);
        let mut request = Request::builder().uri(uri);
        if let Some(range) = range {
            request = request.header(header::RANGE, range);
        }
        let mut request = request.body(Body::empty()).expect("request should build");
        request.extensions_mut().insert(ConnectInfo(test_addr()));
        router.oneshot(request).await.expect("response")
    }

    async fn get_status(app_state: Arc<AppState>, uri: &str) -> StatusCode {
        get_response(app_state, uri, None).await.status()
    }

    async fn hls_session_last_media_at_ms(app_state: &Arc<AppState>, proxy_session_id: &str) -> Option<u64> {
        app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&ProxySessionId(proxy_session_id.to_string()))
            .await
            .expect("session should exist")
            .read()
            .await
            .activity
            .last_authorized_media_at_ms
    }

    async fn assert_no_hls_cache_stream_registered(app_state: &Arc<AppState>) {
        assert!(app_state.active_users.active_streams().await.is_empty());
    }

    async fn response_body(response: Response<Body>) -> bytes::Bytes {
        response.into_body().collect().await.expect("body should collect").to_bytes()
    }

    fn access_lease_id_from_redirect_location(location: &str) -> &str {
        location.trim_end_matches("/manifest.m3u8").rsplit('/').next().expect("access lease id in redirect")
    }

    fn proxy_session_id_from_redirect_location(location: &str) -> &str {
        let mut parts = location.trim_end_matches("/manifest.m3u8").rsplit('/');
        let _access_lease_id = parts.next().expect("access lease id in redirect");
        parts.next().expect("proxy session id in redirect")
    }

    async fn access_lease_session_token(
        app_state: &Arc<AppState>,
        proxy_session_id: &ProxySessionId,
        access_lease_id: &HlsAccessLeaseId,
    ) -> String {
        app_state
            .hls_proxy
            .access_lease(access_lease_id, proxy_session_id, super::current_time_millis())
            .await
            .expect("access lease should exist")
            .user_session_token
    }

    #[test]
    fn transient_full_object_cacheable_request_accepts_open_zero_range() {
        assert!(super::is_transient_full_object_cacheable_request(None));
        assert!(super::is_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=0-"))));
        assert!(!super::is_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=4-"))));
        assert!(!super::is_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=-4"))));
        assert!(!super::is_transient_full_object_cacheable_request(Some(&HeaderValue::from_static("bytes=0-1,4-5"))));
    }

    async fn assert_hls_cache_stream_registered(app_state: &Arc<AppState>, proxy_session_id: &str) {
        let streams = app_state.active_users.active_streams().await;
        assert_eq!(streams.len(), 1);
        let stream = &streams[0];
        assert_eq!(stream.username, "hls-user");
        assert_eq!(stream.session_token.as_deref(), Some("hls-session-token"));
        assert_eq!(stream.provider.as_ref(), "origin-provider");
        assert_eq!(stream.channel.item_type, PlaylistItemType::LiveHls);
        assert!(stream.channel.shared);
        assert_eq!(
            stream.channel.shared_stream_id,
            Some(super::hls_cache_shared_stream_id(&ProxySessionId(proxy_session_id.to_string())))
        );
        assert_eq!(stream.channel.shared_joined_existing, Some(false));
        assert_eq!(stream.channel.url.as_ref(), format!("/proxy/hls/live/{proxy_session_id}/manifest.m3u8"));
        assert!(!stream.channel.url.contains("test-access-lease"));
        assert!(!stream.channel.url.contains("hls-session-token"));
        assert!(!stream.channel.url.contains("origin.example.com"));
        assert!(!stream.channel.url.contains("/hls/hls-user/"));
    }

    async fn register_hls_cache_stream_for_stats_test(
        app_state: &Arc<AppState>,
        session: &crate::api::model::HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        session_token: &str,
        fingerprint: &Fingerprint,
        lease_id: &str,
    ) {
        create_active_hls_user_session_with(
            app_state,
            session_token,
            "origin-provider",
            "http://origin.example.com/live/12345.m3u8",
            fingerprint.addr,
        )
        .await;
        let context = test_hls_access_context_with(
            proxy_session_id.clone(),
            HlsAccessLeaseId(lease_id.to_string()),
            session_token,
            fingerprint.key.clone(),
        );
        super::ensure_hls_cache_stream_registered(app_state, fingerprint, &HeaderMap::new(), &context, session)
            .await
            .expect("HLS stream registers");
    }

    fn find_stream_by_session_token(
        streams: &[shared::model::StreamInfo],
        session_token: &str,
    ) -> shared::model::StreamInfo {
        streams
            .iter()
            .find(|stream| stream.session_token.as_deref() == Some(session_token))
            .unwrap_or_else(|| panic!("{session_token} stream should exist"))
            .clone()
    }

    #[tokio::test]
    async fn hls_cache_stream_stats_mark_additional_viewers_as_joined_existing() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;
        let proxy_session_id = ProxySessionId(proxy_session_id);
        let shared_stream_id = super::hls_cache_shared_stream_id(&proxy_session_id);
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id)
            .await
            .expect("session should exist");
        let first_fingerprint = test_fingerprint();
        let second_fingerprint = test_fingerprint_with_addr(test_addr_with_port(55124));
        register_hls_cache_stream_for_stats_test(
            &app_state,
            &session,
            &proxy_session_id,
            "hls-session-token",
            &first_fingerprint,
            "first-access-lease",
        )
        .await;

        let streams = app_state.active_users.active_streams().await;
        let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
        assert!(first_stream.channel.shared);
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));

        register_hls_cache_stream_for_stats_test(
            &app_state,
            &session,
            &proxy_session_id,
            "hls-second-session-token",
            &second_fingerprint,
            "second-access-lease",
        )
        .await;

        let streams = app_state.active_users.active_streams().await;
        let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
        let second_stream = find_stream_by_session_token(&streams, "hls-second-session-token");
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
        assert_eq!(second_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(second_stream.channel.shared_joined_existing, Some(true));

        register_hls_cache_stream_for_stats_test(
            &app_state,
            &session,
            &proxy_session_id,
            "hls-session-token",
            &first_fingerprint,
            "first-access-lease",
        )
        .await;

        let streams = app_state.active_users.active_streams().await;
        let first_stream = find_stream_by_session_token(&streams, "hls-session-token");
        assert_eq!(first_stream.channel.shared_stream_id, Some(shared_stream_id));
        assert_eq!(first_stream.channel.shared_joined_existing, Some(false));
    }

    struct TestSegmentOrigin {
        base_url: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestSegmentOrigin {
        fn drop(&mut self) { self.task.abort(); }
    }

    async fn spawn_test_segment_origin(body: &'static [u8]) -> TestSegmentOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 2048];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let response =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                });
            }
        });
        TestSegmentOrigin { base_url: format!("http://{addr}"), task }
    }

    async fn wait_for_hls_test_session(app_state: &Arc<AppState>, session_key: &HlsSessionKey) -> HlsSessionHandle {
        for _ in 0..50 {
            if let Some(session) = app_state.hls_proxy.sessions().get_by_key(session_key).await {
                return session;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("shared hls test session should be created");
    }

    async fn wait_for_hls_refresh_in_flight(session: &HlsSessionHandle) {
        for _ in 0..50 {
            if session.read().await.origin_refresh.in_flight {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("shared hls refresh should be in flight");
    }

    struct TestTransientOrigin {
        base_url: String,
        requests: Arc<tokio::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestTransientOrigin {
        fn drop(&mut self) { self.task.abort(); }
    }

    async fn spawn_test_transient_origin() -> TestTransientOrigin {
        spawn_test_transient_origin_with_response(
            "206 Partial Content",
            &[
                ("Content-Type", "video/MP2T"),
                ("Content-Range", "bytes 2-15/16"),
                ("Accept-Ranges", "bytes"),
                ("Cache-Control", "no-store"),
                ("ETag", "\"abc\""),
                ("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
            ],
            "transient-body",
        )
        .await
    }

    async fn spawn_test_transient_origin_with_response(
        status_line: &'static str,
        response_headers: &'static [(&'static str, &'static str)],
        body: &'static str,
    ) -> TestTransientOrigin {
        spawn_test_transient_origin_with_delayed_response(status_line, response_headers, body, Duration::ZERO).await
    }

    async fn spawn_test_transient_origin_with_delayed_response(
        status_line: &'static str,
        response_headers: &'static [(&'static str, &'static str)],
        body: &'static str,
        response_delay: Duration,
    ) -> TestTransientOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                tokio::spawn(async move {
                    let mut buf = vec![0_u8; 4096];
                    let Ok(read) = socket.read(&mut buf).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buf[..read]).to_string();
                    requests.lock().await.push(request);
                    if !response_delay.is_zero() {
                        tokio::time::sleep(response_delay).await;
                    }
                    let mut response = format!("HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n", body.len());
                    for (name, value) in response_headers {
                        let _ = writeln!(&mut response, "{name}: {value}\r");
                    }
                    response.push_str("Connection: close\r\n\r\n");
                    response.push_str(body);
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        TestTransientOrigin { base_url: format!("http://{addr}"), requests, task }
    }

    #[tokio::test]
    async fn valid_hls_proxy_segment_without_session_returns_not_found() {
        let status = get_status(test_app_state(), "/proxy/hls/live/a8f31c9eQ7sLk92pV0mTaw/000123.ts").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn valid_hls_proxy_segment_with_not_ready_session_returns_not_found() {
        let app_state = test_app_state();
        let proxy_session_id = map_segment(&app_state, 123, "ts").await;

        let status = get_status(app_state, &format!("/proxy/hls/live/{proxy_session_id}/000123.ts")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn valid_hls_proxy_segment_with_not_ready_and_valid_lease_returns_service_unavailable() {
        let app_state = test_app_state();
        let proxy_session_id = map_segment(&app_state, 123, "ts").await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&crate::api::model::ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        session.write().await.segments.get_mut(&123).expect("segment should exist").origin_fetch_ref = None;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn not_ready_hls_proxy_segment_with_fetch_ref_demand_fetches_and_returns_ok() {
        let origin = spawn_test_segment_origin(b"0123456789").await;
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id =
            map_segment_with_origin_url(&app_state, 123, "ts", &format!("{}/seg.ts", origin.base_url)).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
    }

    #[tokio::test]
    async fn not_ready_hls_proxy_segment_with_range_waits_for_demand_fetch_then_returns_partial() {
        let origin = spawn_test_segment_origin(b"0123456789").await;
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id =
            map_segment_with_origin_url(&app_state, 123, "ts", &format!("{}/seg.ts", origin.base_url)).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
    }

    #[tokio::test]
    async fn not_ready_hls_proxy_segment_without_fetch_ref_returns_service_unavailable() {
        let app_state = test_app_state();
        let proxy_session_id = map_segment(&app_state, 123, "ts").await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&crate::api::model::ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        session.write().await.segments.get_mut(&123).expect("segment should exist").origin_fetch_ref = None;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_without_lease_returns_not_found() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment_without_lease(&app_state, 123, "ts", b"0123456789").await;

        let status = get_status(app_state, &format!("/proxy/hls/live/{proxy_session_id}/000123.ts")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_marked_for_gc_returns_not_found() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&crate::api::model::ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        session.write().await.mark_for_gc_removal();

        let status = get_status(app_state, &uri).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_without_range_returns_ok() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/MP2T");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=300, immutable");
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&crate::api::model::ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        {
            let session = session.read().await;
            let segment = session.segments.get(&123).expect("segment should exist");
            assert_eq!(segment.access.active_readers(), 1);
            assert!(matches!(segment.status, SegmentCacheStatus::Ready { content_length: 10, .. }));
        }

        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));

        {
            let session = session.read().await;
            let segment = session.segments.get(&123).expect("segment should exist");
            assert_eq!(segment.access.active_readers(), 0);
            assert!(segment.access.last_accessed_at_ms() > 0);
            assert!(session.activity.last_authorized_media_at_ms.is_some());
        }
        assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_range_zero_open_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "m4s", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.m4s").await;

        let response = get_response(app_state, &uri, Some("bytes=0-")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-9/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_range_start_open_returns_partial_content_from_offset() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "m4a", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.m4a").await;

        let response = get_response(app_state, &uri, Some("bytes=4-")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mp4");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 4-9/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "6");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"456789"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_range_start_end_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_suffix_range_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("bytes=-3")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 7-9/10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"789"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_unsatisfiable_range_returns_416() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=99-")).await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_multi_range_returns_416() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-1,4-5")).await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_without_lease_returns_not_found() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"map-body", false).await;

        let status = get_status(app_state, &format!("/proxy/hls/live/{proxy_session_id}/map/000000.mp4")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_without_range_returns_ok() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
        assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
        assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_range_returns_partial_content() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(app_state, &uri, Some("bytes=2-5")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"2345"));
    }

    #[tokio::test]
    async fn ready_hls_proxy_map_multi_range_returns_416_with_zero_content_length() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_hls_map(&app_state, b"0123456789", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-1,4-5")).await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn hls_proxy_map_not_ready_with_valid_lease_returns_service_unavailable() {
        let app_state = test_app_state();
        let session =
            app_state.hls_proxy.get_or_create_session(HlsSessionKey::new(1, "12345"), b"rewrite-secret", 100).await;
        let manifest = normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n000123.m4s\n");
        let proxy_session_id = {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest should map");
            session.proxy_session_id.0.clone()
        };
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "map/000000.mp4").await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_resource_without_lease_returns_not_found() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", false).await;

        let status = get_status(app_state, &format!("/proxy/hls/live/{proxy_session_id}/r/{resource_id}.ts")).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn transient_resource_with_valid_lease_streams_origin_response_and_headers() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&crate::api::model::ProxySessionId(proxy_session_id.clone()))
            .await
            .expect("session should exist");
        {
            let mut session = session.write().await;
            session.origin_request_headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
            session.origin_request_headers.insert(header::COOKIE, HeaderValue::from_static("sid=secret"));
            session
                .origin_request_headers
                .insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
            session.origin_request_headers.insert(header::HOST, HeaderValue::from_static("proxy.example.com"));
            session
                .origin_request_headers
                .insert(HeaderName::from_static("x-tuliprox-main-revision"), HeaderValue::from_static("secret"));
            session.origin_request_headers.insert(header::ACCEPT_LANGUAGE, HeaderValue::from_static("de"));
        }

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "video/MP2T");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-15/16");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()[header::ETAG], "\"abc\"");
        assert_eq!(response.headers()[header::LAST_MODIFIED], "Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"transient-body"));
        assert!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await.is_some());
        let origin_requests = origin.requests.lock().await;
        let origin_request = origin_requests.first().expect("origin request").to_ascii_lowercase();
        assert!(origin_request.contains("range: bytes=2-15"));
        assert!(origin_request.contains("accept-language: de"));
        assert!(!origin_request.contains("authorization: bearer secret"));
        assert!(!origin_request.contains("cookie: sid=secret"));
        assert!(!origin_request.contains("proxy-authorization: basic secret"));
        assert!(!origin_request.contains("host: proxy.example.com"));
        assert!(!origin_request.contains("x-tuliprox-main-revision"));
        assert_hls_cache_stream_registered(&app_state, &proxy_session_id).await;
    }

    #[tokio::test]
    async fn transient_resource_without_range_is_cached_after_first_fetch() {
        let app_state = test_app_state();
        let origin =
            spawn_test_transient_origin_with_response("200 OK", &[("Content-Type", "video/MP2T")], "0123456789").await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first = get_response(Arc::clone(&app_state), &uri, None).await;
        let second = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn transient_resource_range_from_zero_is_cached_as_full_object() {
        let app_state = test_app_state();
        let origin =
            spawn_test_transient_origin_with_response("200 OK", &[("Content-Type", "video/MP2T")], "0123456789").await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-")).await;
        let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=4-")).await;

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"456789"));
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn transient_resource_range_from_zero_waits_for_inflight_object_cache_fetch() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin_with_delayed_response(
            "200 OK",
            &[("Content-Type", "video/MP2T")],
            "0123456789",
            Duration::from_millis(150),
        )
        .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first_app_state = Arc::clone(&app_state);
        let first_uri = uri.clone();
        let first = tokio::spawn(async move { get_response(first_app_state, &first_uri, Some("bytes=0-")).await });
        for _ in 0..50 {
            if origin.requests.lock().await.len() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(origin.requests.lock().await.len(), 1);

        let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=0-")).await;
        let first = first.await.expect("first request joins");

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"0123456789"));
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn transient_resource_range_from_offset_without_ready_object_is_not_cached() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let first = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;
        let second = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(origin.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn transient_key_resource_is_not_cached() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin_with_response(
            "200 OK",
            &[("Content-Type", "application/octet-stream")],
            "key-bytes",
        )
        .await;
        let (proxy_session_id, resource_id) = map_transient_resource_with_kind(
            &app_state,
            &format!("{}/key.bin", origin.base_url),
            "key",
            true,
            TransientResourceKind::Key,
        )
        .await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.key")).await;

        let first = get_response(Arc::clone(&app_state), &uri, None).await;
        let second = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(response_body(first).await, bytes::Bytes::from_static(b"key-bytes"));
        assert_eq!(response_body(second).await, bytes::Bytes::from_static(b"key-bytes"));
        assert_eq!(origin.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn transient_resource_origin_error_does_not_mark_media_activity() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin_with_response(
            "500 Internal Server Error",
            &[("Content-Type", "text/plain")],
            "origin-error",
        )
        .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()), Some("1"));
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_resource_permanent_origin_error_returns_not_found() {
        let app_state = test_app_state();
        let origin =
            spawn_test_transient_origin_with_response("404 Not Found", &[("Content-Type", "text/plain")], "missing")
                .await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, None).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[tokio::test]
    async fn transient_resource_holds_provider_handle_until_origin_body_is_finished() {
        let input = overlap_provider_input();
        let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, resource_id) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let proxy_session_id_value = ProxySessionId(proxy_session_id.clone());
        let session = app_state
            .hls_proxy
            .sessions()
            .get_by_proxy_session_id(&proxy_session_id_value)
            .await
            .expect("session should exist");
        {
            let mut session = session.write().await;
            session.origin_account_binding = Some(HlsOriginAccountBinding::new(
                Arc::clone(&input.name),
                Arc::from("account-a"),
                &proxy_session_id_value,
                super::current_time_millis(),
            ));
        }
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, &format!("r/{resource_id}.ts")).await;

        let response = get_response(Arc::clone(&app_state), &uri, Some("bytes=2-15")).await;

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        wait_for_provider_connection_count(&app_state, 1).await;
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"transient-body"));
        wait_for_provider_connection_count(&app_state, 0).await;
    }

    #[tokio::test]
    async fn transient_unknown_resource_returns_not_found() {
        let app_state = test_app_state();
        let origin = spawn_test_transient_origin().await;
        let (proxy_session_id, _) =
            map_transient_resource(&app_state, &format!("{}/seg.ts", origin.base_url), "ts", true).await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "r/unknown.ts").await;

        let status = get_status(Arc::clone(&app_state), &uri).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(hls_session_last_media_at_ms(&app_state, &proxy_session_id).await, None);
        assert_no_hls_cache_stream_registered(&app_state).await;
    }

    #[test]
    fn transient_cross_origin_redirect_strips_sensitive_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        headers.insert(header::COOKIE, HeaderValue::from_static("session=secret"));
        headers.insert(HeaderName::from_static("proxy-authorization"), HeaderValue::from_static("Basic secret"));
        headers.insert(header::HOST, HeaderValue::from_static("origin.example.com"));
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1"));

        super::strip_sensitive_headers_for_cross_origin_redirect(&mut headers);

        assert!(!headers.contains_key(header::AUTHORIZATION));
        assert!(!headers.contains_key(header::COOKIE));
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key(header::HOST));
        assert_eq!(headers[header::RANGE], "bytes=0-1");
    }

    #[tokio::test]
    async fn ready_hls_proxy_segment_unknown_range_unit_is_ignored() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let hls_proxy = Arc::new(HlsProxyManager::with_cache_settings(temp_dir.path(), 300));
        let app_state = test_app_state_with_hls_proxy(hls_proxy);
        let proxy_session_id = map_ready_segment(&app_state, 123, "ts", b"0123456789").await;
        let uri = hls_proxy_uri(&app_state, &proxy_session_id, "000123.ts").await;

        let response = get_response(app_state, &uri, Some("items=0-1")).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_body(response).await, bytes::Bytes::from_static(b"0123456789"));
    }

    #[tokio::test]
    async fn invalid_hls_proxy_file_names_return_not_found() {
        let app_state = test_app_state();

        assert_eq!(
            get_status(Arc::clone(&app_state), "/proxy/hls/live/a8f31c9eQ7sLk92pV0mTaw/123.ts").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get_status(app_state, "/proxy/hls/live/a8f31c9eQ7sLk92pV0mTaw/map/000123.exe").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn legacy_hls_route_remains_registered() {
        let status = get_status(test_app_state(), "/hls/user/pass/1/2/3/not-a-token").await;

        assert_ne!(status, StatusCode::NOT_FOUND);
    }
}
