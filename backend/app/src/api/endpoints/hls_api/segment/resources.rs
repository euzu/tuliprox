use super::{
    current_hls_resource_lease, ensure_hls_cache_stream_registered, fetch_and_cache_transient_origin_response,
    fetch_transient_origin_response_with_provider_io, hls_cache_response_context, hls_lease_allows_live_origin_work,
    hls_origin_runtime_resource_failure_response, hls_resource_channel_unavailable_response,
    hls_resource_serve_outcome_response, hls_temporary_resource_unavailable_response, is_hls_media_activity_status,
    prepare_hls_resource_access, register_hls_cache_stream_for_successful_media_response,
    serve_transient_object_cache_response_and_mark_or_unavailable, wait_for_transient_object_cache_fetch,
    HlsProxyMapPathParams, HlsProxyResourcePathParams, HlsResourceAccess, HlsResourceEndpointContext,
    HlsTransientEndpointCacheFetchContext, HlsTransientEndpointOriginFetchRequest, HlsTransientOriginFetchResult,
    TransientObjectCacheServeContext, TransientObjectWaitContext, HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
};
use crate::{api::model::AppState, auth::Fingerprint};
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::debug;
use std::sync::Arc;
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    finite_hls_terminal_key_response, hls_transient_object_fetch_failure, hls_transient_origin_response,
    record_temporary_transient_segment_fetch_failure, resolve_hls_transient_object_cache_action,
    safe_hls_access_lease_id, safe_proxy_session_id, serve_hls_map_cache_outcome, HlsLeasePlaybackMode, HlsLogIdentity,
    HlsMapFile, HlsMediaActivityCommitOutcome, HlsOriginResourceFetchError, HlsTransientDirectResponseContext,
    HlsTransientObjectCacheAction, HlsTransientObjectFetchFailure, HlsTransientResourceLeaseContext, ProxySessionId,
    SegmentFetchPolicy, TransientResourceFile, TransientResourceKind, TransientResourceRef,
};

pub(in crate::api::endpoints::hls_api) async fn hls_proxy_map(
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
            Arc::clone(app_state.hls.proxy.segment_cache()),
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

pub(in crate::api::endpoints::hls_api) async fn hls_proxy_resource(
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

pub(in crate::api::endpoints::hls_api) async fn serve_hls_terminal_key_resource(
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

pub(in crate::api::endpoints::hls_api) async fn serve_hls_live_transient_resource(
    context: HlsResourceEndpointContext<'_>,
) -> axum::response::Response {
    let cache_duration_ms = context.app_state.hls.proxy.cache_duration_seconds().saturating_mul(1_000);
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

pub(in crate::api::endpoints::hls_api) struct HlsTransientPassthroughContext<'a> {
    pub(in crate::api::endpoints::hls_api) endpoint: HlsResourceEndpointContext<'a>,
    pub(in crate::api::endpoints::hls_api) resource: TransientResourceRef,
    pub(in crate::api::endpoints::hls_api) cache_action: HlsTransientObjectCacheAction,
    pub(in crate::api::endpoints::hls_api) origin_headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) origin_provider_session_headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) cache_duration_ms: u64,
}

pub(in crate::api::endpoints::hls_api) async fn fetch_or_passthrough_transient_resource(
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

    let policy = endpoint.app_state.hls.proxy.segment_fetch_policy();
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

#[allow(clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn serve_hls_transient_passthrough_result(
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
                    .hls
                    .proxy
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
            let media_marker = if matches!(resource.kind, TransientResourceKind::Segment | TransientResourceKind::Part)
                && response.decoded.status.is_success()
            {
                hls_cache_response_context(
                    endpoint.app_state,
                    endpoint.session,
                    endpoint.access_context,
                    endpoint.lease_identity,
                    endpoint.now_ms,
                )
                .await
                .media_activity_marker
            } else {
                None
            };
            let response = hls_transient_origin_response(
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
            );
            if let Some(marker) = media_marker {
                marker.confirm_media_response(response)
            } else {
                response
            }
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
