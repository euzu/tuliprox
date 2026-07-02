use super::{
    build_hls_origin_resource_headers_with_client_range,
    finish_hls_origin_account_io, hls_client_body_send_deadline, hls_object_body_deadline,
    log_hls_resource_timeout, run_hls_origin_resource_retry_loop_with_attempt_prepare, CacheAccessState,
    HlsAccessLeaseId, HlsMediaActivityMarker, HlsOriginAccountIoLeaseGuard, HlsOriginByteRangeExpectation,
    HlsOriginIoContext, HlsOriginResourceClients, HlsOriginResourceFetchError, HlsOriginResourceFetchTarget,
    HlsRepairRenderedObjectId, HlsResourceFetchAttempt, HlsResourceFetchKind, HlsResourceFetchLogContext,
    HlsResourceFetchSource, HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource, HlsSessionHandle, ProxySessionId, SegmentFetchPolicy, TransientObjectCacheKey,
    TransientObjectFetchDecision, TransientPassthroughState, TransientResourceFile, TransientResourceKind,
    TransientResourceRef,
};
use crate::api::api_utils::try_unwrap_body;
use axum::{body::Body, http::{header, HeaderMap, HeaderValue, StatusCode}, response::IntoResponse};
use futures::{future::BoxFuture, FutureExt, StreamExt, TryStreamExt};
use std::{io, sync::Arc};
use tokio::{sync::Notify, time::sleep};
use tokio_util::io::StreamReader;

pub enum HlsTransientObjectFetchFailure {
    Retryable,
    Permanent { status: Option<StatusCode> },
}

pub fn hls_transient_object_fetch_failure(error: &HlsOriginResourceFetchError) -> HlsTransientObjectFetchFailure {
    match error {
        HlsOriginResourceFetchError::PermanentStatus(status)
        | HlsOriginResourceFetchError::NonRetryableStatus(status) => {
            HlsTransientObjectFetchFailure::Permanent { status: Some(*status) }
        }
        HlsOriginResourceFetchError::InvalidOriginUrl
        | HlsOriginResourceFetchError::InvalidByteRange
        | HlsOriginResourceFetchError::UnexpectedByteRangeStatus => {
            HlsTransientObjectFetchFailure::Permanent { status: None }
        }
        HlsOriginResourceFetchError::RetryableStatus(_)
        | HlsOriginResourceFetchError::Transport(_)
        | HlsOriginResourceFetchError::Redirect
        | HlsOriginResourceFetchError::Timeout
        | HlsOriginResourceFetchError::CacheCommit(_) => HlsTransientObjectFetchFailure::Retryable,
        HlsOriginResourceFetchError::ProviderUnavailable(kind) if kind.is_retryable_resource_failure() => {
            HlsTransientObjectFetchFailure::Retryable
        }
        HlsOriginResourceFetchError::ProviderUnavailable(_) => HlsTransientObjectFetchFailure::Permanent { status: None },
    }
}

pub fn hls_transient_resource_fetch_kind(resource_kind: TransientResourceKind) -> HlsResourceFetchKind {
    match resource_kind {
        TransientResourceKind::Key => HlsResourceFetchKind::Key,
        TransientResourceKind::Map => HlsResourceFetchKind::Map,
        TransientResourceKind::Segment => HlsResourceFetchKind::Segment,
        TransientResourceKind::Other => HlsResourceFetchKind::Other,
    }
}

fn build_hls_transient_resource_fetch_target(
    resolved_origin_uri: &str,
    origin_headers: &HeaderMap,
    origin_provider_session_headers: &HeaderMap,
    range_header: Option<HeaderValue>,
    resource_id: &str,
    resource_kind: TransientResourceKind,
) -> HlsOriginResourceFetchTarget {
    HlsOriginResourceFetchTarget {
        kind: hls_transient_resource_fetch_kind(resource_kind),
        source: HlsResourceFetchSource::Transient,
        object_id: resource_id.to_string(),
        origin_url: resolved_origin_uri.to_string(),
        headers: build_hls_origin_resource_headers_with_client_range(
            origin_headers,
            origin_provider_session_headers,
            range_header,
        ),
        byte_range_expectation: HlsOriginByteRangeExpectation::AnySuccess,
    }
}

pub struct HlsTransientOriginFetchRequest {
    pub resolved_origin_uri: String,
    pub origin_headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub range_header: Option<HeaderValue>,
    pub resource_file: TransientResourceFile,
    pub resource_kind: TransientResourceKind,
    pub clients: HlsOriginResourceClients,
    pub policy: SegmentFetchPolicy,
    pub session_log_id: String,
}

pub async fn fetch_hls_transient_origin_response_with_attempt_prepare<G, P>(
    request: HlsTransientOriginFetchRequest,
    prepare_attempt: P,
) -> Result<(reqwest::Response, G), HlsOriginResourceFetchError>
where
    G: Send + 'static,
    P: FnMut(HlsResourceFetchAttempt) -> BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>,
{
    let target = build_hls_transient_resource_fetch_target(
        &request.resolved_origin_uri,
        &request.origin_headers,
        &request.origin_provider_session_headers,
        request.range_header,
        request.resource_file.resource_id.0.as_str(),
        request.resource_kind,
    );
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        request.clients,
        &request.policy,
        &request.session_log_id,
        prepare_attempt,
        |guard| async move { drop(guard) }.boxed(),
        |response, _attempt, guard| async move { Ok((response, guard)) }.boxed(),
    )
    .await
}

pub enum HlsTransientObjectCacheAction {
    ServeReady,
    FetchAndCache(TransientObjectCacheKey),
    WaitForFetch(Arc<Notify>),
    PassthroughNoCache,
}

pub struct HlsTransientObjectCacheResolution {
    pub resource: TransientResourceRef,
    pub origin_headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub action: HlsTransientObjectCacheAction,
}

pub async fn resolve_hls_transient_object_cache_action(
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    resource_file: &TransientResourceFile,
    range_header: Option<&HeaderValue>,
    now_ms: u64,
    cache_duration_ms: u64,
) -> Result<HlsTransientObjectCacheResolution, StatusCode> {
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
    let action = transient_object_cache_action(
        &mut session,
        proxy_session_id,
        &resource,
        resource_file,
        range_header,
        now_ms,
        cache_duration_ms,
    );
    Ok(HlsTransientObjectCacheResolution {
        resource,
        origin_headers: session.origin_request_headers.clone(),
        origin_provider_session_headers: session.origin_provider_session_headers.clone(),
        action,
    })
}

fn transient_object_cache_action(
    session: &mut super::HlsSession,
    proxy_session_id: &ProxySessionId,
    resource: &TransientResourceRef,
    resource_file: &TransientResourceFile,
    range_header: Option<&HeaderValue>,
    now_ms: u64,
    cache_duration_ms: u64,
) -> HlsTransientObjectCacheAction {
    if matches!(resource.kind, TransientResourceKind::Key) {
        return HlsTransientObjectCacheAction::PassthroughNoCache;
    }
    let cache_key =
        TransientPassthroughState::transient_object_key(proxy_session_id, &resource.id, resource_file.extension.clone());
    if session.transient.ready_object(&cache_key, now_ms).is_some() {
        return HlsTransientObjectCacheAction::ServeReady;
    }
    if !is_hls_transient_full_object_cacheable_request(range_header) {
        return HlsTransientObjectCacheAction::PassthroughNoCache;
    }
    session
        .transient
        .begin_object_fetch(proxy_session_id, resource, &resource_file.extension, now_ms, cache_duration_ms)
        .into()
}

impl From<TransientObjectFetchDecision> for HlsTransientObjectCacheAction {
    fn from(decision: TransientObjectFetchDecision) -> Self {
        match decision {
            TransientObjectFetchDecision::Ready => Self::ServeReady,
            TransientObjectFetchDecision::Fetch(cache_key) => Self::FetchAndCache(cache_key),
            TransientObjectFetchDecision::Wait(notifier) => Self::WaitForFetch(notifier),
        }
    }
}

pub fn is_hls_transient_full_object_cacheable_request(range_header: Option<&HeaderValue>) -> bool {
    let Some(range_header) = range_header else {
        return true;
    };
    range_header.to_str().is_ok_and(|range| range.trim() == "bytes=0-")
}

pub struct HlsTransientObjectFetchFinalizer {
    session: HlsSessionHandle,
    cache_key: TransientObjectCacheKey,
    completed: bool,
    retry_after_ms: u64,
}

impl HlsTransientObjectFetchFinalizer {
    pub fn new(session: HlsSessionHandle, cache_key: TransientObjectCacheKey, retry_after_ms: u64) -> Self {
        Self { session, cache_key, completed: false, retry_after_ms }
    }

    pub fn complete(&mut self) { self.completed = true; }
}

impl Drop for HlsTransientObjectFetchFinalizer {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let session = Arc::clone(&self.session);
        let cache_key = self.cache_key.clone();
        let retry_after_ms = self.retry_after_ms;
        tokio::spawn(async move {
            session.write().await.transient.mark_object_failed_retryable(
                &cache_key,
                current_time_millis(),
                retry_after_ms,
            );
        });
    }
}

#[derive(Clone)]
pub struct HlsTransientCacheCommitContext {
    pub segment_cache: Arc<HlsSegmentCache>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub session: HlsSessionHandle,
    pub access_lease_id: HlsAccessLeaseId,
    pub resource: TransientResourceRef,
    pub resource_file: TransientResourceFile,
    pub cache_key: TransientObjectCacheKey,
    pub range_header: Option<HeaderValue>,
    pub cache_duration_ms: u64,
    pub origin_segment_timeout_ms: u64,
}

pub struct HlsTransientOriginCacheFetchRequest {
    pub fetch: HlsTransientOriginFetchRequest,
    pub commit: HlsTransientCacheCommitContext,
}

pub async fn fetch_and_commit_hls_transient_origin_response_with_attempt_prepare<G, P>(
    request: HlsTransientOriginCacheFetchRequest,
    prepare_attempt: P,
) -> Result<(), HlsOriginResourceFetchError>
where
    G: Send + 'static,
    P: FnMut(HlsResourceFetchAttempt) -> BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>,
{
    let target = build_hls_transient_resource_fetch_target(
        &request.fetch.resolved_origin_uri,
        &request.fetch.origin_headers,
        &request.fetch.origin_provider_session_headers,
        request.fetch.range_header,
        request.fetch.resource_file.resource_id.0.as_str(),
        request.fetch.resource_kind,
    );
    let commit = request.commit;
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        request.fetch.clients,
        &request.fetch.policy,
        &request.fetch.session_log_id,
        prepare_attempt,
        |guard| async move { drop(guard) }.boxed(),
        move |response, _attempt, guard| {
            let commit = commit.clone();
            async move {
                let result = commit_hls_transient_origin_response_attempt(commit, response).await;
                drop(guard);
                result
            }
            .boxed()
        },
    )
    .await
}

async fn commit_hls_transient_origin_response_attempt(
    context: HlsTransientCacheCommitContext,
    response: reqwest::Response,
) -> Result<(), HlsOriginResourceFetchError> {
    let response_headers = response.headers().clone();
    let content_type = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| context.resource.content_type_hint.clone())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let deadline = hls_object_body_deadline(context.origin_segment_timeout_ms);
    let stream_reader = StreamReader::new(response.bytes_stream().map_err(io::Error::other));
    let repair_context = HlsSegmentRepairObjectContext {
        source: HlsSegmentRepairSource::Transient,
        proxy_session_id: context.session.read().await.proxy_session_id.clone(),
        hls_access_lease_id: Some(context.access_lease_id.clone()),
        rendered_object_id: HlsRepairRenderedObjectId::Transient {
            resource_id: context.resource_file.resource_id.0.clone(),
        },
        resource_id: context.resource_file.resource_id.0.clone(),
        file_ext: context.resource_file.extension.clone(),
        origin_fetch_uri_for_diagnostics: context.resource.resolved_origin_uri.clone(),
        media_sequence: None,
        discontinuity_sequence: None,
        complete_object: is_hls_transient_full_object_cacheable_request(context.range_header.as_ref()),
        encrypted: context.resource.kind == TransientResourceKind::Key,
        custom_response: false,
    };
    let commit = Box::pin(context.segment_repair.commit_origin_response(
        &context.segment_cache,
        &context.cache_key,
        stream_reader,
        deadline,
        repair_context,
    ))
    .await;
    let ready_at_ms = current_time_millis();
    let metadata = match commit {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::TimedOut => return Err(HlsOriginResourceFetchError::Timeout),
        Err(err) => return Err(HlsOriginResourceFetchError::cache_commit(&err)),
    };
    let expires_at_ms = ready_at_ms
        .saturating_add(context.cache_duration_ms)
        .max(context.resource.expires_at_ms);
    context.session.write().await.transient.mark_object_ready(
        &context.cache_key,
        content_type,
        metadata.size,
        ready_at_ms,
        expires_at_ms,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn hls_transient_origin_response(
    response: reqwest::Response,
    access: Arc<CacheAccessState>,
    origin_io_guard: Option<HlsTransientOriginIoGuard>,
    media_activity_marker: Option<HlsMediaActivityMarker>,
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

    let guard = HlsTransientReadGuard::new(access, now_ms);
    let media_activity_guard = HlsTransientMediaActivityGuard::new(media_activity_marker, now_ms);
    let deadline = hls_object_body_deadline(origin_segment_timeout_ms);
    let stream = futures::stream::unfold(
        (
            response.bytes_stream(),
            Some(guard),
            origin_io_guard,
            Some(media_activity_guard),
            Box::pin(sleep(hls_client_body_send_deadline())),
            false,
        ),
        move |(mut stream, guard, origin_io_guard, media_activity_guard, mut send_deadline, finished)| {
            let proxy_session_id = proxy_session_id.clone();
            let resource_id = resource_id.clone();
            let origin_url = origin_url.clone();
            async move {
                if finished {
                    return None;
                }
                let next_chunk = tokio::select! {
                    () = send_deadline.as_mut() => {
                        log_hls_resource_timeout(
                            &proxy_session_id,
                            HlsResourceFetchLogContext {
                                kind: hls_transient_resource_fetch_kind(resource_kind),
                                source: HlsResourceFetchSource::Transient,
                                object_id: &resource_id,
                                origin_url: Some(&origin_url),
                            },
                            HlsResourceFetchAttempt { attempt_index: 0, attempts: 1 },
                            hls_client_body_send_deadline().as_millis(),
                        );
                        return Some((
                            Err(io::Error::new(io::ErrorKind::TimedOut, "hls client body send timed out")),
                            (stream, guard, origin_io_guard, media_activity_guard, send_deadline, true),
                        ));
                    }
                    next_chunk = tokio::time::timeout(deadline, stream.next()) => next_chunk,
                };
                match next_chunk {
                    Ok(Some(Ok(chunk))) => {
                        Some((Ok(chunk), (stream, guard, origin_io_guard, media_activity_guard, send_deadline, false)))
                    }
                    Ok(Some(Err(err))) => Some((
                        Err(io::Error::other(err)),
                        (stream, guard, origin_io_guard, media_activity_guard, send_deadline, true),
                    )),
                    Ok(None) => None,
                    Err(_) => {
                        log_hls_resource_timeout(
                            &proxy_session_id,
                            HlsResourceFetchLogContext {
                                kind: hls_transient_resource_fetch_kind(resource_kind),
                                source: HlsResourceFetchSource::Transient,
                                object_id: &resource_id,
                                origin_url: Some(&origin_url),
                            },
                            HlsResourceFetchAttempt { attempt_index: 0, attempts: 1 },
                            deadline.as_millis(),
                        );
                        Some((
                            Err(io::Error::new(io::ErrorKind::TimedOut, "transient passthrough body timed out")),
                            (stream, guard, origin_io_guard, media_activity_guard, send_deadline, true),
                        ))
                    }
                }
            }
        },
    );
    try_unwrap_body!(builder.body(Body::from_stream(stream)))
}

struct HlsTransientReadGuard {
    access: Arc<CacheAccessState>,
}

impl HlsTransientReadGuard {
    fn new(access: Arc<CacheAccessState>, now_ms: u64) -> Self {
        access.reader_started(now_ms);
        Self { access }
    }
}

impl Drop for HlsTransientReadGuard {
    fn drop(&mut self) { self.access.reader_finished(); }
}

struct HlsTransientMediaActivityGuard {
    marker: Option<HlsMediaActivityMarker>,
}

impl HlsTransientMediaActivityGuard {
    fn new(marker: Option<HlsMediaActivityMarker>, _now_ms: u64) -> Self {
        Self { marker }
    }
}

impl Drop for HlsTransientMediaActivityGuard {
    fn drop(&mut self) {
        if let Some(marker) = &self.marker {
            marker.spawn_mark_now();
        }
    }
}

pub struct HlsTransientOriginIoGuard {
    session: HlsSessionHandle,
    origin_io: HlsOriginIoContext,
    lease_guard: Option<HlsOriginAccountIoLeaseGuard>,
    started_generation: u64,
}

impl HlsTransientOriginIoGuard {
    pub fn new(
        session: HlsSessionHandle,
        origin_io: HlsOriginIoContext,
        lease_guard: HlsOriginAccountIoLeaseGuard,
        started_generation: u64,
    ) -> Self {
        Self { session, origin_io, lease_guard: Some(lease_guard), started_generation }
    }
}

impl Drop for HlsTransientOriginIoGuard {
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
                session.read().await.should_refresh_origin_reservation(chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default())
            } else {
                false
            };
            finish_hls_origin_account_io(&origin_io, &session, lease_guard, refresh_reservation).await;
            let mut session = session.write().await;
            if let Some(binding) = session.origin_account_binding.as_mut() {
                let now_ms = chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default();
                binding.last_origin_io_at_ms = Some(now_ms);
                if refresh_reservation {
                    binding.last_reservation_refresh_at_ms = Some(now_ms);
                }
            }
        });
    }
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }
