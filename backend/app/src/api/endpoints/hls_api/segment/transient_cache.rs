use super::{
    hls_cache_response_context, hls_live_lease_identity_is_current, hls_origin_runtime_resource_failure_response,
    hls_resource_serve_outcome_response, hls_transient_object_unavailable_response, is_hls_media_activity_status,
    prepare_hls_transient_origin_io_for_authorized_resource_work,
    register_hls_cache_stream_for_successful_media_response, HlsOriginRuntimeAcquireError,
    HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
};
use crate::{api::model::AppState, auth::Fingerprint};
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use futures::FutureExt;
use log::debug;
use std::sync::Arc;
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
    fetch_hls_transient_origin_response_with_attempt_prepare, hls_transient_object_fetch_failure,
    record_successful_transient_segment_fetch, record_temporary_transient_segment_fetch_failure,
    safe_hls_access_lease_id, serve_hls_transient_object_cache_outcome, serve_hls_transient_object_cache_response,
    HlsAccessContext, HlsBoundAccountAcquireErrorKind, HlsLogIdentity, HlsMediaLeaseIdentity, HlsOriginResourceClients,
    HlsOriginResourceFetchError, HlsResourceFetchAttempt, HlsSessionHandle, HlsTransientCacheCommitContext,
    HlsTransientDecodedOriginResponse, HlsTransientObjectFetchFailure, HlsTransientObjectFetchFinalizer,
    HlsTransientOriginCacheFetchRequest, HlsTransientOriginFetchRequest, HlsTransientOriginIoGuard, SegmentFetchPolicy,
    TransientObjectFetchToken, TransientResourceFile, TransientResourceId, TransientResourceRef,
};

pub(in crate::api::endpoints::hls_api) struct HlsTransientEndpointOriginFetchRequest<'a> {
    pub(in crate::api::endpoints::hls_api) app_state: &'a Arc<AppState>,
    pub(in crate::api::endpoints::hls_api) session: &'a HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api) access_context: &'a HlsAccessContext,
    pub(in crate::api::endpoints::hls_api) fingerprint: &'a Fingerprint,
    pub(in crate::api::endpoints::hls_api) headers: &'a HeaderMap,
    pub(in crate::api::endpoints::hls_api) resource: &'a TransientResourceRef,
    pub(in crate::api::endpoints::hls_api) resource_file: &'a TransientResourceFile,
    pub(in crate::api::endpoints::hls_api) origin_headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) origin_provider_session_headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) range_header: Option<HeaderValue>,
    pub(in crate::api::endpoints::hls_api) policy: SegmentFetchPolicy,
}

pub(in crate::api::endpoints::hls_api) struct HlsTransientOriginFetchResult {
    pub(in crate::api::endpoints::hls_api) result:
        Result<HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>, HlsOriginResourceFetchError>,
    pub(in crate::api::endpoints::hls_api) runtime_prepare_error: Option<HlsOriginRuntimeAcquireError>,
}

pub(in crate::api::endpoints::hls_api) async fn fetch_transient_origin_response_with_provider_io(
    request: HlsTransientEndpointOriginFetchRequest<'_>,
) -> HlsTransientOriginFetchResult {
    let clients = HlsOriginResourceClients {
        client: request.app_state.http_clients.default.load().as_ref().clone(),
        no_redirect_client: request.app_state.http_clients.no_redirect.load().as_ref().clone(),
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
pub(in crate::api::endpoints::hls_api) fn hls_transient_origin_prepare_closure(
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
pub(in crate::api::endpoints::hls_api) struct HlsTransientEndpointCacheFetchContext<'a> {
    pub(in crate::api::endpoints::hls_api) app_state: &'a Arc<AppState>,
    pub(in crate::api::endpoints::hls_api) session: &'a HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api) fingerprint: &'a Fingerprint,
    pub(in crate::api::endpoints::hls_api) headers: &'a HeaderMap,
    pub(in crate::api::endpoints::hls_api) access_context: &'a HlsAccessContext,
    pub(in crate::api::endpoints::hls_api) lease_identity: HlsMediaLeaseIdentity,
    pub(in crate::api::endpoints::hls_api) resource: &'a TransientResourceRef,
    pub(in crate::api::endpoints::hls_api) resource_file: TransientResourceFile,
    pub(in crate::api::endpoints::hls_api) fetch_token: TransientObjectFetchToken,
    pub(in crate::api::endpoints::hls_api) origin_headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) origin_provider_session_headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) range_header: Option<HeaderValue>,
    pub(in crate::api::endpoints::hls_api) cache_duration_ms: u64,
}

pub(in crate::api::endpoints::hls_api) struct TransientObjectWaitContext<'a> {
    pub(in crate::api::endpoints::hls_api) app_state: &'a Arc<AppState>,
    pub(in crate::api::endpoints::hls_api) session: &'a HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api) fingerprint: &'a Fingerprint,
    pub(in crate::api::endpoints::hls_api) headers: &'a HeaderMap,
    pub(in crate::api::endpoints::hls_api) access_context: &'a HlsAccessContext,
    pub(in crate::api::endpoints::hls_api) lease_identity: HlsMediaLeaseIdentity,
    pub(in crate::api::endpoints::hls_api) resource_file: TransientResourceFile,
    pub(in crate::api::endpoints::hls_api) range_header: Option<HeaderValue>,
    pub(in crate::api::endpoints::hls_api) notifier: Arc<tokio::sync::Notify>,
}

pub(in crate::api::endpoints::hls_api) struct TransientObjectCacheServeContext<'a> {
    pub(in crate::api::endpoints::hls_api) app_state: &'a Arc<AppState>,
    pub(in crate::api::endpoints::hls_api) session: &'a HlsSessionHandle,
    pub(in crate::api::endpoints::hls_api) fingerprint: &'a Fingerprint,
    pub(in crate::api::endpoints::hls_api) headers: &'a HeaderMap,
    pub(in crate::api::endpoints::hls_api) access_context: &'a HlsAccessContext,
    pub(in crate::api::endpoints::hls_api) lease_identity: HlsMediaLeaseIdentity,
    pub(in crate::api::endpoints::hls_api) resource_file: TransientResourceFile,
    pub(in crate::api::endpoints::hls_api) range_header: Option<HeaderValue>,
    pub(in crate::api::endpoints::hls_api) now_ms: u64,
}

pub(in crate::api::endpoints::hls_api) async fn serve_transient_object_cache_response_and_mark(
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
            Arc::clone(context.app_state.hls.proxy.segment_cache()),
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

pub(in crate::api::endpoints::hls_api) async fn serve_transient_object_cache_response_and_mark_or_unavailable(
    context: TransientObjectCacheServeContext<'_>,
) -> axum::response::Response {
    serve_transient_object_cache_response_and_mark(context).await
}

pub(in crate::api::endpoints::hls_api) async fn wait_for_transient_object_cache_fetch(
    context: TransientObjectWaitContext<'_>,
) -> axum::response::Response {
    let wait_timeout = context.app_state.hls.proxy.segment_fetch_policy().origin_object_wait_timeout();
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

pub(in crate::api::endpoints::hls_api) fn safe_transient_resource_id(resource_id: &TransientResourceId) -> String {
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

#[allow(clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn fetch_and_cache_transient_origin_response(
    context: HlsTransientEndpointCacheFetchContext<'_>,
) -> axum::response::Response {
    let policy = context.app_state.hls.proxy.segment_fetch_policy();
    let mut fetch_finalizer = HlsTransientObjectFetchFinalizer::new(
        Arc::clone(context.session),
        Arc::clone(context.app_state.hls.proxy.segment_cache()),
        context.fetch_token.clone(),
        HLS_TEMPORARY_RESOURCE_RETRY_AFTER_MS,
    );
    let clients = HlsOriginResourceClients {
        client: context.app_state.http_clients.default.load().as_ref().clone(),
        no_redirect_client: context.app_state.http_clients.no_redirect.load().as_ref().clone(),
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
        log_identity: log_identity.clone(),
    };
    let cache_fetch_request = HlsTransientOriginCacheFetchRequest {
        fetch: fetch_request,
        commit: HlsTransientCacheCommitContext {
            segment_cache: Arc::clone(context.app_state.hls.proxy.segment_cache()),
            segment_repair: Arc::clone(context.app_state.hls.proxy.segment_repair()),
            session: Arc::clone(context.session),
            proxy_session_id: context.access_context.proxy_session_id.clone(),
            log_identity: log_identity.clone(),
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
                    Arc::clone(context.app_state.hls.proxy.segment_cache()),
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
