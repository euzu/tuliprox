use super::{
    build_hls_origin_resource_headers_with_client_range, finish_hls_origin_account_io, hls_client_body_send_deadline,
    refresh_hls_client_body_send_deadline,
    resource_fetch::{log_hls_resource_body_failure, HlsResourceFetchLogContext},
    run_hls_origin_resource_retry_loop_with_attempt_prepare, CacheAccessState, HlsAccessLeaseId, HlsLogIdentity,
    HlsOriginAccountIoLeaseGuard, HlsOriginByteRangeExpectation, HlsOriginIoContext, HlsOriginResourceBodyDeadline,
    HlsOriginResourceClients, HlsOriginResourceFetchError, HlsOriginResourceFetchTarget,
    HlsRepairRenderedObjectId, HlsResourceFetchAttempt, HlsResourceFetchKind, HlsResourceFetchSource, HlsSegmentCache,
    HlsSegmentFailureObject, HlsSegmentFailureTransition, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource, HlsSessionHandle, HlsSessionMode, ProtectedSet, ProxySessionId, SegmentFetchPolicy,
    TransientObjectCacheKey, TransientObjectFetchDecision, TransientObjectFetchToken, TransientPassthroughState,
    TransientResourceFile, TransientResourceKind, TransientResourceRef,
};
use crate::{
    api::api_utils::{mark_response_as_uncompressed, try_unwrap_body},
    utils::content_coding::DecodedHttpResponse,
};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use futures::{future::BoxFuture, FutureExt, StreamExt};
use log::{debug, warn};
use std::{io, sync::Arc};
use tokio::{sync::Notify, time::sleep};
use tokio_util::io::ReaderStream;

pub enum HlsTransientObjectFetchFailure {
    Retryable,
    Permanent { status: Option<StatusCode> },
}

pub fn hls_transient_object_fetch_failure(error: &HlsOriginResourceFetchError) -> HlsTransientObjectFetchFailure {
    if let Some(status) = error.permanent_status() {
        return HlsTransientObjectFetchFailure::Permanent { status: Some(status) };
    }
    if !error.retryable_failure() {
        return HlsTransientObjectFetchFailure::Permanent { status: None };
    }
    HlsTransientObjectFetchFailure::Retryable
}

pub fn hls_transient_resource_fetch_kind(resource_kind: TransientResourceKind) -> HlsResourceFetchKind {
    match resource_kind {
        TransientResourceKind::Key => HlsResourceFetchKind::Key,
        TransientResourceKind::Map => HlsResourceFetchKind::Map,
        TransientResourceKind::Segment => HlsResourceFetchKind::Segment,
        TransientResourceKind::Part => HlsResourceFetchKind::Part,
        TransientResourceKind::Other => HlsResourceFetchKind::Other,
    }
}

fn build_hls_transient_resource_fetch_target(
    resolved_origin_uri: &str,
    origin_headers: &HeaderMap,
    origin_provider_session_headers: &HeaderMap,
    mode: HlsTransientOriginFetchMode,
    resource_id: &str,
    resource_kind: TransientResourceKind,
) -> Result<HlsOriginResourceFetchTarget, HlsOriginResourceFetchError> {
    let (range_header, byte_range_expectation) = match mode {
        HlsTransientOriginFetchMode::CacheFullObject => (None, HlsOriginByteRangeExpectation::FullObject),
        HlsTransientOriginFetchMode::DirectPassthrough { client_range } => {
            (client_range, HlsOriginByteRangeExpectation::AnySuccess)
        }
    };
    Ok(HlsOriginResourceFetchTarget {
        kind: hls_transient_resource_fetch_kind(resource_kind),
        source: HlsResourceFetchSource::Transient,
        object_id: resource_id.to_string(),
        origin_url: resolved_origin_uri.to_string(),
        headers: build_hls_origin_resource_headers_with_client_range(
            origin_headers,
            origin_provider_session_headers,
            range_header,
        )?,
        byte_range_expectation,
    })
}

/// Keeps full-object cache fills separate from decoded client-range passthrough.
enum HlsTransientOriginFetchMode {
    CacheFullObject,
    DirectPassthrough { client_range: Option<HeaderValue> },
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
    pub log_identity: HlsLogIdentity,
}

/// A decoded direct-origin response together with the attempt and guards that own its origin work.
pub struct HlsTransientDecodedOriginResponse<G> {
    pub decoded: DecodedHttpResponse,
    pub body_deadline: HlsOriginResourceBodyDeadline,
    pub attempt: HlsResourceFetchAttempt,
    pub guard: G,
}

pub async fn fetch_hls_transient_origin_response_with_attempt_prepare<G, P>(
    request: HlsTransientOriginFetchRequest,
    prepare_attempt: P,
) -> Result<HlsTransientDecodedOriginResponse<G>, HlsOriginResourceFetchError>
where
    G: Send + 'static,
    P: FnMut(HlsResourceFetchAttempt) -> BoxFuture<'static, Result<G, HlsOriginResourceFetchError>>,
{
    let target = build_hls_transient_resource_fetch_target(
        &request.resolved_origin_uri,
        &request.origin_headers,
        &request.origin_provider_session_headers,
        HlsTransientOriginFetchMode::DirectPassthrough { client_range: request.range_header },
        request.resource_file.resource_id.0.as_str(),
        request.resource_kind,
    )?;
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        request.clients,
        &request.policy,
        &request.log_identity,
        prepare_attempt,
        |guard| async move { drop(guard) }.boxed(),
        |decoded, attempt, body_deadline, guard| {
            async move { Ok(HlsTransientDecodedOriginResponse { decoded, body_deadline, attempt, guard }) }.boxed()
        },
    )
    .await
}

pub enum HlsTransientObjectCacheAction {
    ServeReady,
    FetchAndCache(Box<TransientObjectFetchToken>),
    WaitForFetch(Arc<Notify>),
    PassthroughNoCache,
}

pub struct HlsTransientObjectCacheResolution {
    pub resource: TransientResourceRef,
    pub origin_headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub action: HlsTransientObjectCacheAction,
}

#[derive(Clone, Copy)]
struct HlsTransientObjectCacheActionInput<'a> {
    proxy_session_id: &'a ProxySessionId,
    resource: &'a TransientResourceRef,
    resource_file: &'a TransientResourceFile,
    range_header: Option<&'a HeaderValue>,
    now_ms: u64,
    cache_duration_ms: u64,
    protected: bool,
    key_object_cache_allowed: bool,
}

pub async fn resolve_hls_transient_object_cache_action(
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    resource_file: &TransientResourceFile,
    range_header: Option<&HeaderValue>,
    now_ms: u64,
    cache_duration_ms: u64,
) -> Result<HlsTransientObjectCacheResolution, StatusCode> {
    // `is_gc_marked_for_removal` is `&self` and is the dominant early-exit on a
    // busy session (GC sweeps mark sessions on a timer). Resolve it under a read
    // lock so we don't pay for the exclusive write-lock acquisition when the
    // session is already doomed.
    if session.read().await.is_gc_marked_for_removal() {
        return Err(StatusCode::NOT_FOUND);
    }
    let mut session = session.write().await;
    let protected = ProtectedSet::from_session(&session);
    session.transient.prune_expired_except(now_ms, &protected.key_resource_ids);
    let Some(resource) = session.transient.resources.get(&resource_file.resource_id).cloned() else {
        return Err(StatusCode::NOT_FOUND);
    };
    if resource.file_ext_hint.as_deref().is_some_and(|extension| extension != resource_file.extension) {
        return Err(StatusCode::NOT_FOUND);
    }
    let key_object_cache_allowed =
        resource.kind != TransientResourceKind::Key || session.mode == HlsSessionMode::NormalCacheTimeline;
    let action = transient_object_cache_action(
        &mut session,
        HlsTransientObjectCacheActionInput {
            proxy_session_id,
            resource: &resource,
            resource_file,
            range_header,
            now_ms,
            cache_duration_ms,
            protected: protected.key_resource_ids.contains(&resource_file.resource_id),
            key_object_cache_allowed,
        },
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
    input: HlsTransientObjectCacheActionInput<'_>,
) -> HlsTransientObjectCacheAction {
    if input.resource.kind == TransientResourceKind::Key && !input.key_object_cache_allowed {
        return HlsTransientObjectCacheAction::PassthroughNoCache;
    }
    let cache_key = TransientPassthroughState::transient_object_key(
        input.proxy_session_id,
        &input.resource.id,
        input.resource_file.extension.clone(),
    );
    if session
        .transient
        .ready_object(&cache_key, input.resource.kind, input.now_ms, input.protected)
        .is_some()
    {
        return HlsTransientObjectCacheAction::ServeReady;
    }
    if !is_hls_transient_full_object_cacheable_request(input.range_header) {
        return HlsTransientObjectCacheAction::PassthroughNoCache;
    }
    session
        .transient
        .begin_object_fetch(
            input.proxy_session_id,
            input.resource,
            &input.resource_file.extension,
            input.now_ms,
            input.cache_duration_ms,
        )
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
    segment_cache: Arc<HlsSegmentCache>,
    fetch_token: TransientObjectFetchToken,
    completed: bool,
    retry_after_ms: u64,
}

impl HlsTransientObjectFetchFinalizer {
    pub fn new(
        session: HlsSessionHandle,
        segment_cache: Arc<HlsSegmentCache>,
        fetch_token: TransientObjectFetchToken,
        retry_after_ms: u64,
    ) -> Self {
        Self { session, segment_cache, fetch_token, completed: false, retry_after_ms }
    }

    pub fn complete(&mut self) { self.completed = true; }
}

impl Drop for HlsTransientObjectFetchFinalizer {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let session = Arc::clone(&self.session);
        let segment_cache = Arc::clone(&self.segment_cache);
        let fetch_token = self.fetch_token.clone();
        let retry_after_ms = self.retry_after_ms;
        tokio::spawn(async move {
            if let Err(err) = segment_cache.delete(fetch_token.cache_key()).await {
                debug!("HLS abandoned transient cache fill cleanup failed: error={err}");
            }
            delete_superseded_transient_object(&segment_cache, &fetch_token).await;
            session.write().await.fail_transient_object_retryable_if_current(
                &fetch_token,
                current_time_millis(),
                retry_after_ms,
            );
        });
    }
}

pub(super) async fn delete_superseded_transient_object(
    segment_cache: &HlsSegmentCache,
    fetch_token: &TransientObjectFetchToken,
) {
    let Some(superseded) = fetch_token.superseded_object() else {
        return;
    };
    if let Err(err) = segment_cache.delete(&superseded.key).await {
        debug!("HLS superseded transient cache object cleanup failed: error={err}");
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
    pub fetch_token: TransientObjectFetchToken,
    pub cache_duration_ms: u64,
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
        HlsTransientOriginFetchMode::CacheFullObject,
        request.fetch.resource_file.resource_id.0.as_str(),
        request.fetch.resource_kind,
    )?;
    let commit = request.commit;
    run_hls_origin_resource_retry_loop_with_attempt_prepare(
        target,
        request.fetch.clients,
        &request.fetch.policy,
        &request.fetch.log_identity,
        prepare_attempt,
        |guard| async move { drop(guard) }.boxed(),
        move |decoded, _attempt, body_deadline, guard| {
            let commit = commit.clone();
            async move {
                let result = commit_hls_transient_origin_response_attempt(commit, decoded, body_deadline).await;
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
    decoded: DecodedHttpResponse,
    body_deadline: HlsOriginResourceBodyDeadline,
) -> Result<(), HlsOriginResourceFetchError> {
    let content_type = decoded
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| context.resource.content_type_hint.clone())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let (proxy_session_id, log_identity) = {
        let session = context.session.read().await;
        (session.proxy_session_id.clone(), super::HlsLogIdentity::from_session(&session))
    };
    let repair_context = HlsSegmentRepairObjectContext {
        source: HlsSegmentRepairSource::Transient,
        log_identity,
        proxy_session_id,
        hls_access_lease_id: Some(context.access_lease_id.clone()),
        rendered_object_id: HlsRepairRenderedObjectId::Transient {
            resource_id: context.resource_file.resource_id.0.clone(),
        },
        resource_id: context.resource_file.resource_id.0.clone(),
        file_ext: context.resource_file.extension.clone(),
        origin_fetch_uri_for_diagnostics: context.resource.resolved_origin_uri.clone(),
        media_sequence: None,
        discontinuity_sequence: None,
        complete_object: true,
        encrypted: context.resource.encrypted_media || context.resource.kind == TransientResourceKind::Key,
        custom_response: false,
    };
    let cache_key = context.fetch_token.cache_key().clone();
    let commit = Box::pin(context.segment_repair.commit_origin_response(
        &context.segment_cache,
        &cache_key,
        decoded.body,
        body_deadline.deadline(),
        repair_context,
    ))
    .await;
    let ready_at_ms = current_time_millis();
    let metadata = match commit {
        Ok(metadata) => metadata,
        Err(err) => return Err(HlsOriginResourceFetchError::cache_body(&err)),
    };
    let expires_at_ms = ready_at_ms.saturating_add(context.cache_duration_ms).max(context.resource.expires_at_ms);
    let mut session = context.session.write().await;
    if context.resource.kind == TransientResourceKind::Key && metadata.size != 16 {
        session.fail_transient_object_permanent_if_current(
            &context.fetch_token,
            ready_at_ms,
            Some(StatusCode::BAD_GATEWAY),
        );
        drop(session);
        delete_stale_transient_fill(&context.segment_cache, &cache_key).await;
        return Err(HlsOriginResourceFetchError::NonRetryableStatus(StatusCode::BAD_GATEWAY));
    }
    let committed = session.commit_transient_object_ready_if_current(
        context.resource.kind,
        &context.fetch_token,
        content_type,
        metadata.size,
        ready_at_ms,
        expires_at_ms,
    );
    drop(session);
    if !committed {
        delete_stale_transient_fill(&context.segment_cache, &cache_key).await;
        return Err(HlsOriginResourceFetchError::Superseded);
    }
    delete_superseded_transient_object(&context.segment_cache, &context.fetch_token).await;
    Ok(())
}

async fn delete_stale_transient_fill(segment_cache: &HlsSegmentCache, cache_key: &TransientObjectCacheKey) {
    if let Err(err) = segment_cache.delete(cache_key).await {
        debug!("HLS stale transient cache fill cleanup failed: error={err}");
    }
}

/// Context retained until a direct transient response body reaches one terminal outcome.
pub struct HlsTransientDirectResponseContext {
    pub session: HlsSessionHandle,
    pub resource: TransientResourceRef,
    pub policy: SegmentFetchPolicy,
    pub now_ms: u64,
    pub log_identity: HlsLogIdentity,
}

#[derive(Clone, Copy)]
enum HlsTransientDirectStreamOutcome {
    CleanEof,
    OriginBodyFailure,
    ClientAborted,
}

// Sole owner of the direct-body outcome: EOF is success, origin read failures
// degrade affected media/dependencies, and downstream cancellation remains neutral.
struct HlsTransientDirectResponseFinalizer {
    context: Option<HlsTransientDirectResponseLifecycleContext>,
}

struct HlsTransientDirectResponseLifecycleContext {
    session: HlsSessionHandle,
    resource: TransientResourceRef,
    policy: SegmentFetchPolicy,
}

impl HlsTransientDirectResponseFinalizer {
    fn new(context: HlsTransientDirectResponseLifecycleContext) -> Self {
        let context = transient_resource_affects_media_readiness(context.resource.kind).then_some(context);
        Self { context }
    }

    async fn finish(&mut self, outcome: HlsTransientDirectStreamOutcome) {
        let Some(completion) = self.begin_finish(outcome) else {
            return;
        };
        if let Err(error) = completion.await {
            warn!(
                "HLS transient direct lifecycle task failed: cancelled={} panic={}",
                error.is_cancelled(),
                error.is_panic()
            );
        }
    }

    fn begin_finish(&mut self, outcome: HlsTransientDirectStreamOutcome) -> Option<tokio::task::JoinHandle<()>> {
        let context = self.context.take()?;
        if matches!(outcome, HlsTransientDirectStreamOutcome::ClientAborted) {
            return None;
        }
        // Once EOF or an origin-body failure has been observed, its state transition
        // must survive cancellation of the downstream body poll.
        Some(tokio::spawn(async move {
            match outcome {
                HlsTransientDirectStreamOutcome::CleanEof => {
                    record_successful_transient_segment_fetch(&context.session, &context.resource).await;
                }
                HlsTransientDirectStreamOutcome::OriginBodyFailure => {
                    let failed_at_ms = current_time_millis();
                    record_temporary_transient_segment_fetch_failure(
                        &context.session,
                        &context.resource,
                        &context.policy,
                        failed_at_ms,
                    )
                    .await;
                }
                HlsTransientDirectStreamOutcome::ClientAborted => {}
            }
        }))
    }

    fn finish_client_aborted(&mut self) { self.context.take(); }
}

impl Drop for HlsTransientDirectResponseFinalizer {
    fn drop(&mut self) {
        // A body dropped before EOF is a downstream abort. It must release the
        // retained guards without changing provider/segment failure state.
        self.finish_client_aborted();
    }
}

pub fn hls_transient_origin_response(
    response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
    context: HlsTransientDirectResponseContext,
) -> axum::response::Response {
    let mut builder = axum::response::Response::builder().status(response.decoded.status);
    for header_name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CACHE_CONTROL,
        header::ETAG,
        header::LAST_MODIFIED,
    ] {
        if let Some(value) = response.decoded.headers.get(&header_name) {
            builder = builder.header(header_name, value.clone());
        }
    }

    let mut response = try_unwrap_body!(builder.body(hls_transient_direct_body(response, context)));
    mark_response_as_uncompressed(&mut response);
    response
}

fn hls_transient_direct_body(
    response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
    context: HlsTransientDirectResponseContext,
) -> Body {
    let HlsTransientDecodedOriginResponse { decoded, body_deadline, attempt, guard: origin_io_guard } = response;
    let HlsTransientDirectResponseContext {
        session,
        resource,
        policy,
        now_ms,
        log_identity,
    } = context;

    let guard = HlsTransientReadGuard::new(Arc::clone(&resource.access), now_ms);
    let finalizer = HlsTransientDirectResponseFinalizer::new(HlsTransientDirectResponseLifecycleContext {
        session,
        resource: resource.clone(),
        policy,
    });
    let resource_id = resource.id.0.clone();
    let resource_kind = resource.kind;
    let stream = futures::stream::unfold(
        (
            ReaderStream::new(decoded.body),
            Some(guard),
            origin_io_guard,
            finalizer,
            Box::pin(sleep(hls_client_body_send_deadline())),
            false,
        ),
        move |(mut stream, guard, origin_io_guard, mut finalizer, mut send_deadline, finished)| {
            let log_identity = log_identity.clone();
            let resource_id = resource_id.clone();
            async move {
                if finished {
                    return None;
                }
                let next_chunk = tokio::select! {
                    () = send_deadline.as_mut() => {
                        finalizer.finish(HlsTransientDirectStreamOutcome::ClientAborted).await;
                        return Some((
                            Err(io::Error::new(io::ErrorKind::TimedOut, "hls client body send timed out")),
                            (stream, None, None, finalizer, send_deadline, true),
                        ));
                    }
                    // Decoder setup is bounded by the absolute attempt deadline before this
                    // response is built. Once handed to the client, retain the existing
                    // per-chunk origin-body idle timeout instead of imposing a total-body limit.
                    next_chunk = tokio::time::timeout(body_deadline.timeout(), stream.next()) => next_chunk,
                };
                match next_chunk {
                    Ok(Some(Ok(chunk))) => {
                        refresh_hls_client_body_send_deadline(send_deadline.as_mut());
                        Some((
                            Ok(chunk),
                            (stream, guard, origin_io_guard, finalizer, send_deadline, false),
                        ))
                    }
                    Ok(Some(Err(err))) => {
                        log_hls_resource_body_failure(
                            &log_identity,
                            hls_transient_direct_log_context(&resource_id, resource_kind),
                            attempt,
                            &err,
                            body_deadline.timeout().as_millis(),
                        );
                        finalizer.finish(HlsTransientDirectStreamOutcome::OriginBodyFailure).await;
                        Some((Err(err), (stream, None, None, finalizer, send_deadline, true)))
                    }
                    Ok(None) => {
                        finalizer.finish(HlsTransientDirectStreamOutcome::CleanEof).await;
                        None
                    }
                    Err(_) => {
                        let error = io::Error::new(io::ErrorKind::TimedOut, "transient passthrough body timed out");
                        log_hls_resource_body_failure(
                            &log_identity,
                            hls_transient_direct_log_context(&resource_id, resource_kind),
                            attempt,
                            &error,
                            body_deadline.timeout().as_millis(),
                        );
                        finalizer.finish(HlsTransientDirectStreamOutcome::OriginBodyFailure).await;
                        Some((Err(error), (stream, None, None, finalizer, send_deadline, true)))
                    }
                }
            }
        },
    );
    Body::from_stream(stream)
}

fn hls_transient_direct_log_context(
    resource_id: &str,
    resource_kind: TransientResourceKind,
) -> HlsResourceFetchLogContext<'_> {
    HlsResourceFetchLogContext {
        kind: hls_transient_resource_fetch_kind(resource_kind),
        source: HlsResourceFetchSource::Transient,
        object_id: resource_id,
        origin_url: None,
    }
}

pub async fn record_successful_transient_segment_fetch(session: &HlsSessionHandle, resource: &TransientResourceRef) {
    if !transient_resource_is_media(resource.kind) {
        return;
    }
    let mut session = session.write().await;
    if !transient_resource_is_current(&session, resource) {
        return;
    }
    if let Some(reset_failures) = session.record_successful_segment_fetch() {
        if log::log_enabled!(log::Level::Debug) {
            let identity = HlsLogIdentity::from_session(&session);
            debug!(
                "HLS segment temporary failure counter reset: session={} proxy_session={} previous_failures={reset_failures}",
                identity.session(),
                identity.proxy_session()
            );
        }
    }
}

pub async fn record_temporary_transient_segment_fetch_failure(
    session: &HlsSessionHandle,
    resource: &TransientResourceRef,
    policy: &SegmentFetchPolicy,
    now_ms: u64,
) -> bool {
    if !transient_resource_affects_media_readiness(resource.kind) {
        return false;
    }
    let mut session = session.write().await;
    if !transient_resource_is_current(&session, resource) {
        return false;
    }
    session.origin_control.path_condition = super::origin_progress::HlsOriginPathCondition::SegmentReadinessFailure;
    if !transient_resource_is_media(resource.kind) {
        return false;
    }
    let threshold = policy.permanent_failure_segment_threshold.max(1);
    match session.record_temporary_segment_fetch_failure(
        now_ms,
        HlsSegmentFailureObject::Transient { resource_id: resource.id.0.clone() },
        threshold,
    ) {
        HlsSegmentFailureTransition::StillRetryable { failures, threshold } => {
            if log::log_enabled!(log::Level::Debug) {
                let identity = HlsLogIdentity::from_session(&session);
                debug!(
                    "HLS segment temporary failure counted: session={} proxy_session={} object={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    resource.id.0,
                    failures,
                    threshold
                );
            }
            false
        }
        HlsSegmentFailureTransition::BecamePermanentlyFailed { failures, threshold } => {
            if log::log_enabled!(log::Level::Warn) {
                let identity = HlsLogIdentity::from_session(&session);
                warn!(
                    "HLS segment temporary failure threshold reached: session={} proxy_session={} failures={} threshold={}",
                    identity.session(),
                    identity.proxy_session(),
                    failures,
                    threshold
                );
            }
            session.invalidate_queued_origin_work();
            true
        }
    }
}

const fn transient_resource_is_media(kind: TransientResourceKind) -> bool {
    matches!(kind, TransientResourceKind::Segment | TransientResourceKind::Part)
}

const fn transient_resource_affects_media_readiness(kind: TransientResourceKind) -> bool {
    matches!(
        kind,
        TransientResourceKind::Segment
            | TransientResourceKind::Part
            | TransientResourceKind::Key
            | TransientResourceKind::Map
    )
}

fn transient_resource_is_current(session: &super::HlsSession, resource: &TransientResourceRef) -> bool {
    session.transient.resources.get(&resource.id).is_some_and(|current| {
        current.kind == resource.kind
            && current.resolved_origin_uri == resource.resolved_origin_uri
            && Arc::ptr_eq(&current.access, &resource.access)
    })
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
        // Decrement the origin work count synchronously when the lock is free so an
        // immediate retry is not rejected by the admission check (active_origin_work_count > 0)
        // while the spawned cleanup is still pending
        let pre_finished = session.try_write().map(|mut guard| guard.finish_origin_work(started_generation)).ok();
        tokio::spawn(async move {
            let generation_valid = match pre_finished {
                Some(valid) => valid,
                None => {
                    let mut session = session.write().await;
                    session.finish_origin_work(started_generation)
                }
            };
            let refresh_reservation = if generation_valid {
                session.read().await.should_refresh_origin_reservation(
                    chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default(),
                )
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

#[cfg(test)]
mod tests {
    use super::{
        super::{
            origin_progress::{HlsOriginPathCondition, HlsOriginProgressPhase},
            resource_fetch::take_body_failure_log_attempts,
        },
        fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
        fetch_hls_transient_origin_response_with_attempt_prepare, hls_transient_object_fetch_failure,
        hls_transient_origin_response, record_temporary_transient_segment_fetch_failure,
        HlsTransientCacheCommitContext, HlsTransientDecodedOriginResponse, HlsTransientDirectResponseContext,
        HlsTransientDirectResponseFinalizer, HlsTransientDirectResponseLifecycleContext,
        HlsTransientDirectStreamOutcome, HlsTransientObjectFetchFailure, HlsTransientOriginCacheFetchRequest,
        HlsTransientOriginFetchRequest, HlsTransientOriginIoGuard, HlsLogIdentity,
    };
    use crate::{
        api::{
            api_utils::should_compress_response,
            model::{
                HlsAccessLeaseId, HlsOriginResourceClients, HlsOriginResourceFetchError, HlsSegmentCache,
                HlsSegmentFailureObject, HlsSegmentRepairManager, HlsSession, HlsSessionHandle, HlsSessionKey,
                HlsSessionStore, SegmentFetchPolicy, TransientObjectCacheKey, TransientObjectCacheStatus,
                TransientObjectFetchDecision, TransientObjectFetchToken, TransientPassthroughState,
                TransientResourceFile, TransientResourceKind, TransientResourceRef,
            },
        },
        model::HlsSegmentRepairConfig,
    };
    use async_compression::tokio::write::{BrotliEncoder, GzipEncoder, ZstdEncoder};
    use axum::{
        body::to_bytes,
        http::{header, HeaderMap, HeaderValue, StatusCode},
    };
    use futures::FutureExt;
    use shared::model::HlsSegmentRepairMode;
    use std::{
        io,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{Mutex, RwLock},
    };

    struct TestOrigin {
        base_url: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TestOrigin {
        fn drop(&mut self) { self.task.abort(); }
    }

    #[test]
    fn storage_full_transient_cache_failure_is_permanent() {
        let error = HlsOriginResourceFetchError::cache_commit(&io::Error::from_raw_os_error(28));

        assert!(matches!(
            hls_transient_object_fetch_failure(&error),
            HlsTransientObjectFetchFailure::Permanent { status: None }
        ));
    }

    async fn spawn_test_origin(
        status_line: &'static str,
        response_headers: Vec<(&'static str, &'static str)>,
        body: Vec<u8>,
    ) -> TestOrigin {
        spawn_test_origin_in_chunks(status_line, response_headers, vec![body], Duration::ZERO).await
    }

    async fn spawn_test_origin_in_chunks(
        status_line: &'static str,
        response_headers: Vec<(&'static str, &'static str)>,
        body_chunks: Vec<Vec<u8>>,
        inter_chunk_delay: Duration,
    ) -> TestOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("test origin address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let body_chunks = Arc::new(body_chunks);
        let content_length = body_chunks.iter().map(Vec::len).sum::<usize>();
        let response_headers = Arc::new(response_headers);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let body_chunks = Arc::clone(&body_chunks);
                let response_headers = Arc::clone(&response_headers);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let mut chunk = [0_u8; 2048];
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                    }
                    requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
                    let mut response = format!("HTTP/1.1 {status_line}\r\nContent-Length: {content_length}\r\n");
                    for (name, value) in response_headers.iter() {
                        response.push_str(name);
                        response.push_str(": ");
                        response.push_str(value);
                        response.push_str("\r\n");
                    }
                    response.push_str("Connection: close\r\n\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    for (index, chunk) in body_chunks.iter().enumerate() {
                        if index > 0 && !inter_chunk_delay.is_zero() {
                            tokio::time::sleep(inter_chunk_delay).await;
                        }
                        if socket.write_all(chunk).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        TestOrigin { base_url: format!("http://{addr}"), requests, task }
    }

    async fn spawn_retry_then_body_origin(
        response_headers: Vec<(&'static str, &'static str)>,
        body: Vec<u8>,
    ) -> TestOrigin {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
        let addr = listener.local_addr().expect("test origin address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let attempts = Arc::new(AtomicUsize::new(0));
        let task_attempts = Arc::clone(&attempts);
        let body = Arc::new(body);
        let response_headers = Arc::new(response_headers);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&task_requests);
                let attempts = Arc::clone(&task_attempts);
                let body = Arc::clone(&body);
                let response_headers = Arc::clone(&response_headers);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let mut chunk = [0_u8; 2048];
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                    }
                    requests.lock().await.push(String::from_utf8_lossy(&request).to_string());
                    if attempts.fetch_add(1, Ordering::Relaxed) == 0 {
                        let _ = socket
                            .write_all(
                                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        return;
                    }
                    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len());
                    for (name, value) in response_headers.iter() {
                        response.push_str(name);
                        response.push_str(": ");
                        response.push_str(value);
                        response.push_str("\r\n");
                    }
                    response.push_str("Connection: close\r\n\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });

        TestOrigin { base_url: format!("http://{addr}"), requests, task }
    }

    async fn spawn_zstd_origin(body: Vec<u8>) -> TestOrigin {
        spawn_test_origin(
            "200 OK",
            vec![("Content-Encoding", "zstd"), ("Content-Type", "application/octet-stream")],
            body,
        )
        .await
    }

    async fn gzip_encode(body: &[u8]) -> Vec<u8> {
        let mut encoder = GzipEncoder::new(Vec::new());
        encoder.write_all(body).await.expect("test body encodes");
        encoder.shutdown().await.expect("test encoder finishes");
        encoder.into_inner()
    }

    async fn brotli_encode(body: &[u8]) -> Vec<u8> {
        let mut encoder = BrotliEncoder::new(Vec::new());
        encoder.write_all(body).await.expect("test body encodes");
        encoder.shutdown().await.expect("test encoder finishes");
        encoder.into_inner()
    }

    async fn zstd_encode(body: &[u8]) -> Vec<u8> {
        let mut encoder = ZstdEncoder::new(Vec::new());
        encoder.write_all(body).await.expect("test body encodes");
        encoder.shutdown().await.expect("test encoder finishes");
        encoder.into_inner()
    }

    async fn fetch_direct_decoded(
        origin_url: String,
        resource_kind: TransientResourceKind,
        range_header: Option<HeaderValue>,
    ) -> Result<HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>, HlsOriginResourceFetchError> {
        fetch_direct_decoded_with_timeout(origin_url, resource_kind, range_header, 1_000).await
    }

    async fn fetch_direct_decoded_with_timeout(
        origin_url: String,
        resource_kind: TransientResourceKind,
        range_header: Option<HeaderValue>,
        origin_segment_timeout_ms: u64,
    ) -> Result<HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>, HlsOriginResourceFetchError> {
        let resource = TransientResourceRef::new(
            resource_kind,
            origin_url,
            b"rewrite-secret",
            10,
            60_000,
            Some("bin".to_string()),
        );
        let request = HlsTransientOriginFetchRequest {
            resolved_origin_uri: resource.resolved_origin_uri.clone(),
            origin_headers: HeaderMap::new(),
            origin_provider_session_headers: HeaderMap::new(),
            range_header,
            resource_file: TransientResourceFile { resource_id: resource.id, extension: "bin".to_string() },
            resource_kind,
            clients: HlsOriginResourceClients {
                client: reqwest::Client::new(),
                no_redirect_client: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .expect("no-redirect client builds"),
                use_manual_redirects: false,
            },
            policy: SegmentFetchPolicy {
                origin_segment_timeout_ms,
                retry_delays_ms: [0; 5],
                retry_jitter_max_ms: 0,
                ..SegmentFetchPolicy::default()
            },
            log_identity: HlsLogIdentity::for_test(
                "direct-transient-content-session",
                "direct-transient-proxy-session",
            ),
        };
        fetch_hls_transient_origin_response_with_attempt_prepare(request, |_| async { Ok(None) }.boxed()).await
    }

    fn direct_client_response(
        response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
        resource_kind: TransientResourceKind,
    ) -> axum::response::Response {
        TestDirectResponseFixture::new(resource_kind, "http://127.0.0.1/origin").response(response)
    }

    struct TestDirectResponseFixture {
        session: HlsSessionHandle,
        resource: TransientResourceRef,
        policy: SegmentFetchPolicy,
        log_identity: HlsLogIdentity,
    }

    impl TestDirectResponseFixture {
        fn new(resource_kind: TransientResourceKind, origin_url: impl Into<String>) -> Self {
            let resource = TransientResourceRef::new(
                resource_kind,
                origin_url,
                b"rewrite-secret",
                10,
                60_000,
                Some("bin".to_string()),
            );
            let policy =
                SegmentFetchPolicy { retry_delays_ms: [0; 5], retry_jitter_max_ms: 0, ..SegmentFetchPolicy::default() };
            let mut session = HlsSession::new(HlsSessionKey::new(1, "direct-test"), b"rewrite-secret", 10);
            session.transient.upsert_resources([resource.clone()]);
            let log_identity = HlsLogIdentity::from_session(&session);
            Self { session: Arc::new(RwLock::new(session)), resource, policy, log_identity }
        }

        fn response(
            &self,
            response: HlsTransientDecodedOriginResponse<Option<HlsTransientOriginIoGuard>>,
        ) -> axum::response::Response {
            hls_transient_origin_response(response, self.response_context())
        }

        fn response_context(&self) -> HlsTransientDirectResponseContext {
            HlsTransientDirectResponseContext {
                session: Arc::clone(&self.session),
                resource: self.resource.clone(),
                policy: self.policy.clone(),
                now_ms: 10,
                log_identity: self.log_identity.clone(),
            }
        }

        fn lifecycle_context(&self) -> HlsTransientDirectResponseLifecycleContext {
            HlsTransientDirectResponseLifecycleContext {
                session: Arc::clone(&self.session),
                resource: self.resource.clone(),
                policy: self.policy.clone(),
            }
        }

        async fn seed_segment_failure(&self) {
            assert!(
                !record_temporary_transient_segment_fetch_failure(&self.session, &self.resource, &self.policy, 9).await
            );
        }

        async fn segment_failure_count(&self) -> u32 {
            self.session.read().await.segment_failure_tracker.consecutive_temporary_failures
        }

        async fn wait_for_segment_failure_count(&self, expected: u32) {
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if self.segment_failure_count().await == expected {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("segment failure state reaches expected value");
        }

        async fn last_segment_failure(&self) -> Option<HlsSegmentFailureObject> {
            self.session.read().await.segment_failure_tracker.last_failed_object.clone()
        }
    }

    struct TestTransientCacheFixture {
        temp_dir: tempfile::TempDir,
        segment_cache: Arc<HlsSegmentCache>,
        segment_repair: Arc<HlsSegmentRepairManager>,
        session: HlsSessionHandle,
        log_identity: HlsLogIdentity,
        resource: TransientResourceRef,
        resource_file: TransientResourceFile,
        lookup_key: TransientObjectCacheKey,
        cache_key: TransientObjectCacheKey,
        fetch_token: TransientObjectFetchToken,
    }

    impl TestTransientCacheFixture {
        async fn new(origin_url: String) -> Self {
            let now_ms = super::current_time_millis();
            let resource = TransientResourceRef::new(
                TransientResourceKind::Segment,
                origin_url,
                b"rewrite-secret",
                now_ms,
                60_000,
                Some("ts".to_string()),
            );
            let resource_file = TransientResourceFile { resource_id: resource.id.clone(), extension: "ts".to_string() };
            let store = HlsSessionStore::new();
            let session = store.get_or_create_session(HlsSessionKey::new(1, "1"), b"rewrite-secret", 10).await;
            let (proxy_session_id, log_identity) = {
                let session = session.read().await;
                (session.proxy_session_id.clone(), HlsLogIdentity::from_session(&session))
            };
            let lookup_key = TransientPassthroughState::transient_object_key(
                &proxy_session_id,
                &resource.id,
                resource_file.extension.clone(),
            );
            let (resource, fetch_token) = {
                let mut session = session.write().await;
                session.transient.upsert_resources([resource]);
                let resource = session
                    .transient
                    .resources
                    .get(&resource_file.resource_id)
                    .expect("registered transient resource")
                    .clone();
                match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", now_ms, 60_000) {
                    TransientObjectFetchDecision::Fetch(fetch_token) => (resource, *fetch_token),
                    TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                        panic!("new transient resource should start a cache fetch")
                    }
                }
            };
            let cache_key = fetch_token.cache_key().clone();
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
            let segment_repair = Arc::new(HlsSegmentRepairManager::new(HlsSegmentRepairConfig {
                max_level: HlsSegmentRepairMode::Off,
                apply_to_first_segments: 1,
                max_parallel_repairs: 1,
                ..Default::default()
            }));
            Self {
                temp_dir,
                segment_cache,
                segment_repair,
                session,
                log_identity,
                resource,
                resource_file,
                lookup_key,
                cache_key,
                fetch_token,
            }
        }

        fn request(&self, range_header: Option<HeaderValue>) -> HlsTransientOriginCacheFetchRequest {
            HlsTransientOriginCacheFetchRequest {
                fetch: HlsTransientOriginFetchRequest {
                    resolved_origin_uri: self.resource.resolved_origin_uri.clone(),
                    origin_headers: HeaderMap::new(),
                    origin_provider_session_headers: HeaderMap::new(),
                    range_header,
                    resource_file: self.resource_file.clone(),
                    resource_kind: self.resource.kind,
                    clients: HlsOriginResourceClients {
                        client: reqwest::Client::new(),
                        no_redirect_client: reqwest::Client::builder()
                            .redirect(reqwest::redirect::Policy::none())
                            .build()
                            .expect("no-redirect client builds"),
                        use_manual_redirects: false,
                    },
                    policy: SegmentFetchPolicy {
                        origin_segment_timeout_ms: 1_000,
                        retry_delays_ms: [0; 5],
                        retry_jitter_max_ms: 0,
                        ..SegmentFetchPolicy::default()
                    },
                    log_identity: self.log_identity.clone(),
                },
                commit: HlsTransientCacheCommitContext {
                    segment_cache: Arc::clone(&self.segment_cache),
                    segment_repair: Arc::clone(&self.segment_repair),
                    session: Arc::clone(&self.session),
                    access_lease_id: HlsAccessLeaseId("transient-content-coding-test".to_string()),
                    resource: self.resource.clone(),
                    resource_file: self.resource_file.clone(),
                    fetch_token: self.fetch_token.clone(),
                    cache_duration_ms: 60_000,
                },
            }
        }
    }

    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::Relaxed); }
    }

    #[tokio::test]
    async fn direct_key_decodes_declared_gzip_brotli_and_zstd_to_exact_identity_bytes() {
        let identity = b"\x00\xffdirect-key-identity\x10\x80";
        let encoded_bodies = [
            ("gzip", gzip_encode(identity).await),
            ("br", brotli_encode(identity).await),
            ("zstd", zstd_encode(identity).await),
        ];

        for (encoding, encoded) in encoded_bodies {
            let origin = spawn_test_origin(
                "200 OK",
                vec![("Content-Encoding", encoding), ("Content-Type", "application/octet-stream")],
                encoded,
            )
            .await;
            let decoded =
                fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
                    .await
                    .expect("declared direct key coding decodes");
            let response = direct_client_response(decoded, TransientResourceKind::Key);

            assert!(!should_compress_response(&response));
            assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
            assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.expect("decoded key body streams"),
                identity.as_slice()
            );

            let requests = origin.requests.lock().await;
            assert_eq!(requests.len(), 1);
            assert!(requests[0].to_ascii_lowercase().contains("accept-encoding: identity"));
        }
    }

    #[tokio::test]
    async fn direct_map_and_part_or_other_resources_decode_to_identity() {
        let identity = b"shared-direct-map-part-other";
        for (resource_kind, encoding, encoded) in [
            (TransientResourceKind::Map, "gzip", gzip_encode(identity).await),
            (TransientResourceKind::Part, "br", brotli_encode(identity).await),
            (TransientResourceKind::Other, "zstd", zstd_encode(identity).await),
        ] {
            let origin = spawn_test_origin(
                "200 OK",
                vec![("Content-Encoding", encoding), ("Content-Type", "application/octet-stream")],
                encoded,
            )
            .await;
            let decoded = fetch_direct_decoded(format!("{}/object.bin", origin.base_url), resource_kind, None)
                .await
                .expect("declared direct resource coding decodes");
            let response = direct_client_response(decoded, resource_kind);

            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.expect("decoded resource body streams"),
                identity.as_slice()
            );
            assert_eq!(origin.requests.lock().await.len(), 1);
        }
    }

    #[tokio::test]
    async fn direct_binary_declared_only_leaves_headerless_gzip_magic_unchanged() {
        let identity = b"headerless-gzip-representation";
        let headerless_encoded = gzip_encode(identity).await;
        let random_magic = vec![0x1f, 0x8b, 0x11, 0x00, 0xff, 0x42, 0x7e];

        for body in [headerless_encoded, random_magic] {
            let origin =
                spawn_test_origin("200 OK", vec![("Content-Type", "application/octet-stream")], body.clone()).await;
            let decoded =
                fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
                    .await
                    .expect("headerless binary is not inspected");
            let response = direct_client_response(decoded, TransientResourceKind::Key);

            assert_eq!(to_bytes(response.into_body(), usize::MAX).await.expect("headerless binary streams"), body);
            assert_eq!(origin.requests.lock().await.len(), 1);
        }
    }

    #[tokio::test]
    async fn decoded_direct_response_normalizes_headers_and_disables_tower_compression() {
        let identity = b"normalized-direct-response";
        let encoded = gzip_encode(identity).await;
        let origin = spawn_test_origin(
            "200 OK",
            vec![
                ("Content-Encoding", "gzip"),
                ("Content-Type", "application/octet-stream"),
                ("Content-Range", "bytes 0-9/10"),
                ("Accept-Ranges", "bytes"),
                ("Cache-Control", "private, max-age=5"),
                ("ETag", "strong-origin-representation"),
                ("Last-Modified", "Wed, 21 Oct 2015 07:28:00 GMT"),
                ("Set-Cookie", "provider_session=secret"),
                ("X-Provider-Secret", "do-not-forward"),
            ],
            encoded,
        )
        .await;
        let decoded = fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
            .await
            .expect("direct response decodes");
        let response = direct_client_response(decoded, TransientResourceKind::Key);
        let headers = response.headers();

        assert!(!should_compress_response(&response));
        for removed in [
            header::CONTENT_ENCODING,
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::ETAG,
            header::TRANSFER_ENCODING,
            header::SET_COOKIE,
        ] {
            assert!(headers.get(&removed).is_none(), "{removed} must not describe the decoded response");
        }
        assert!(headers.get("x-provider-secret").is_none());
        assert_eq!(headers.get(header::CONTENT_TYPE).expect("content type"), "application/octet-stream");
        assert_eq!(headers.get(header::CACHE_CONTROL).expect("cache control"), "private, max-age=5");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.expect("normalized body streams"),
            identity.as_slice()
        );
    }

    #[tokio::test]
    async fn direct_encoded_partial_content_is_rejected_before_client_response() {
        let encoded = zstd_encode(b"encoded-range").await;
        let origin = spawn_test_origin(
            "206 Partial Content",
            vec![
                ("Content-Encoding", "zstd"),
                ("Content-Type", "application/octet-stream"),
                ("Content-Range", "bytes 2-5/16"),
            ],
            encoded,
        )
        .await;

        let result = fetch_direct_decoded(
            format!("{}/key.bin", origin.base_url),
            TransientResourceKind::Key,
            Some(HeaderValue::from_static("bytes=2-5")),
        )
        .await;

        assert!(matches!(result, Err(HlsOriginResourceFetchError::ContentCoding(_))));
        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = requests[0].to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity"));
        assert!(request.contains("range: bytes=2-5"));
    }

    #[tokio::test]
    async fn direct_identity_partial_content_preserves_consistent_range_headers() {
        let identity = b"part";
        let origin = spawn_test_origin(
            "206 Partial Content",
            vec![
                ("Content-Type", "application/octet-stream"),
                ("Content-Range", "bytes 2-5/16"),
                ("Accept-Ranges", "bytes"),
            ],
            identity.to_vec(),
        )
        .await;
        let decoded = fetch_direct_decoded(
            format!("{}/key.bin", origin.base_url),
            TransientResourceKind::Key,
            Some(HeaderValue::from_static("bytes=2-5")),
        )
        .await
        .expect("identity partial response is allowed");
        let response = direct_client_response(decoded, TransientResourceKind::Key);

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers().get(header::CONTENT_LENGTH).expect("content length"), "4");
        assert_eq!(response.headers().get(header::CONTENT_RANGE).expect("content range"), "bytes 2-5/16");
        assert_eq!(response.headers().get(header::ACCEPT_RANGES).expect("accept ranges"), "bytes");
        assert!(!should_compress_response(&response));
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.expect("identity partial body streams"),
            identity.as_slice()
        );
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn direct_fetch_propagates_retry_attempt_and_identity_to_client_response() {
        let identity = b"successful-second-attempt";
        let origin =
            spawn_retry_then_body_origin(vec![("Content-Type", "application/octet-stream")], identity.to_vec()).await;
        let origin_url = format!("{}/segment.ts", origin.base_url);
        let decoded = fetch_direct_decoded(origin_url.clone(), TransientResourceKind::Segment, None)
            .await
            .expect("second origin attempt succeeds");

        assert_eq!(decoded.attempt.attempt_index, 1);
        assert_eq!(decoded.attempt.attempts, 5);

        let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url);
        let response = fixture.response(decoded);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.expect("retried response body streams"),
            identity.as_slice()
        );

        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| request.to_ascii_lowercase().contains("accept-encoding: identity")));
    }

    #[tokio::test]
    async fn direct_segment_success_resets_failure_state_only_after_clean_eof() {
        let identity = b"complete-segment";
        let origin =
            spawn_test_origin("200 OK", vec![("Content-Type", "application/octet-stream")], identity.to_vec()).await;
        let origin_url = format!("{}/segment.ts", origin.base_url);
        let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
        fixture.seed_segment_failure().await;
        let decoded = fetch_direct_decoded(origin_url, TransientResourceKind::Segment, None)
            .await
            .expect("direct response setup succeeds");
        let response = fixture.response(decoded);

        assert_eq!(fixture.segment_failure_count().await, 1, "headers alone must not report body success");
        assert_eq!(fixture.resource.access.active_readers(), 1);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.expect("complete segment streams"),
            identity.as_slice()
        );
        fixture.wait_for_segment_failure_count(0).await;
        assert_eq!(fixture.resource.access.active_readers(), 0);
    }

    #[tokio::test]
    async fn direct_terminal_transition_survives_dropped_completion_waiter() {
        let fixture =
            TestDirectResponseFixture::new(TransientResourceKind::Segment, "http://127.0.0.1/cancelled-body-poll.ts");
        fixture.seed_segment_failure().await;
        let session_lock = fixture.session.write().await;
        let mut finalizer = HlsTransientDirectResponseFinalizer::new(fixture.lifecycle_context());

        let transition = finalizer
            .begin_finish(HlsTransientDirectStreamOutcome::CleanEof)
            .expect("clean EOF starts an owned transition");
        drop(finalizer);
        drop(transition);
        drop(session_lock);

        fixture.wait_for_segment_failure_count(0).await;
    }

    #[tokio::test]
    async fn direct_segment_decoder_failure_after_retry_uses_selected_attempt_once() {
        let _ = take_body_failure_log_attempts();
        let mut truncated = gzip_encode(b"decoder failure after response headers").await;
        truncated.truncate(truncated.len().saturating_sub(8));
        let origin = spawn_retry_then_body_origin(
            vec![("Content-Encoding", "gzip"), ("Content-Type", "application/octet-stream")],
            truncated,
        )
        .await;
        let origin_url = format!("{}/segment.ts", origin.base_url);
        let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
        let decoded = fetch_direct_decoded(origin_url, TransientResourceKind::Segment, None)
            .await
            .expect("decoder setup succeeds before streaming");
        assert_eq!(decoded.attempt.attempt_index, 1);
        assert_eq!(decoded.attempt.attempts, 5);
        let response = fixture.response(decoded);

        assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
        fixture.wait_for_segment_failure_count(1).await;
        assert_eq!(
            fixture.last_segment_failure().await,
            Some(HlsSegmentFailureObject::Transient { resource_id: fixture.resource.id.0.clone() })
        );
        tokio::task::yield_now().await;
        assert_eq!(fixture.segment_failure_count().await, 1, "terminal body failure must not finalize twice");
        assert_eq!(fixture.resource.access.active_readers(), 0);
        assert_eq!(origin.requests.lock().await.len(), 2, "streaming failure must not trigger a hidden retry");
        assert_eq!(
            take_body_failure_log_attempts(),
            vec![(1, 5)],
            "test hook intentionally records the zero-based raw index of the selected second attempt"
        );
    }

    #[tokio::test]
    async fn unrelated_transient_objects_do_not_start_media_lifecycle_tasks() {
        let kind = TransientResourceKind::Other;
        for outcome in [HlsTransientDirectStreamOutcome::CleanEof, HlsTransientDirectStreamOutcome::OriginBodyFailure] {
            let fixture = TestDirectResponseFixture::new(kind, "http://127.0.0.1/non-segment.bin");
            let mut finalizer = HlsTransientDirectResponseFinalizer::new(fixture.lifecycle_context());

            assert!(finalizer.begin_finish(outcome).is_none(), "{kind:?} must not start a segment lifecycle task");
        }
    }

    #[tokio::test]
    async fn key_and_map_body_failures_degrade_readiness_without_incrementing_media_failure_count() {
        for kind in [TransientResourceKind::Key, TransientResourceKind::Map] {
            let fixture = TestDirectResponseFixture::new(kind, "http://127.0.0.1/media-dependency.bin");
            let mut finalizer = HlsTransientDirectResponseFinalizer::new(fixture.lifecycle_context());

            finalizer
                .begin_finish(HlsTransientDirectStreamOutcome::OriginBodyFailure)
                .expect("media dependency starts a lifecycle task")
                .await
                .expect("media dependency lifecycle task joins");

            let session = fixture.session.read().await;
            assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 0);
            assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
        }
    }

    #[tokio::test]
    async fn direct_segment_origin_idle_timeout_is_counted_exactly_once() {
        let origin = spawn_test_origin_in_chunks(
            "200 OK",
            vec![("Content-Type", "application/octet-stream")],
            vec![b"first".to_vec(), b"late".to_vec()],
            Duration::from_millis(200),
        )
        .await;
        let origin_url = format!("{}/segment.ts", origin.base_url);
        let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
        let decoded = fetch_direct_decoded_with_timeout(origin_url, TransientResourceKind::Segment, None, 50)
            .await
            .expect("direct response setup succeeds");
        let response = fixture.response(decoded);

        assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
        fixture.wait_for_segment_failure_count(1).await;
        tokio::task::yield_now().await;
        assert_eq!(fixture.segment_failure_count().await, 1);
        assert_eq!(fixture.resource.access.active_readers(), 0);
    }

    #[tokio::test]
    async fn dropping_direct_segment_body_is_client_abort_without_state_transition() {
        let _ = take_body_failure_log_attempts();
        let origin = spawn_test_origin(
            "200 OK",
            vec![("Content-Type", "application/octet-stream")],
            b"unconsumed-segment".to_vec(),
        )
        .await;
        let origin_url = format!("{}/segment.ts", origin.base_url);
        let fixture = TestDirectResponseFixture::new(TransientResourceKind::Segment, origin_url.clone());
        fixture.seed_segment_failure().await;
        let decoded = fetch_direct_decoded(origin_url, TransientResourceKind::Segment, None)
            .await
            .expect("direct response setup succeeds");
        let response = fixture.response(decoded);
        assert_eq!(fixture.resource.access.active_readers(), 1);

        drop(response);

        assert_eq!(fixture.segment_failure_count().await, 1);
        assert_eq!(fixture.resource.access.active_readers(), 0);
        assert_eq!(origin.requests.lock().await.len(), 1);
        assert!(take_body_failure_log_attempts().is_empty());
    }

    #[tokio::test]
    async fn direct_non_media_body_failures_are_failure_tracker_neutral() {
        for kind in [TransientResourceKind::Key, TransientResourceKind::Map, TransientResourceKind::Other] {
            let mut truncated = gzip_encode(b"non-segment decoder failure").await;
            truncated.truncate(truncated.len().saturating_sub(8));
            let origin = spawn_test_origin(
                "200 OK",
                vec![("Content-Encoding", "gzip"), ("Content-Type", "application/octet-stream")],
                truncated,
            )
            .await;
            let origin_url = format!("{}/object.bin", origin.base_url);
            let fixture = TestDirectResponseFixture::new(kind, origin_url.clone());
            let decoded =
                fetch_direct_decoded(origin_url, kind, None).await.expect("decoder setup succeeds before streaming");
            let response = fixture.response(decoded);

            assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
            assert_eq!(fixture.segment_failure_count().await, 0, "{kind:?} must not affect segment failure state");
            let path_condition = fixture.session.read().await.origin_control.path_condition;
            if matches!(kind, TransientResourceKind::Key | TransientResourceKind::Map) {
                assert_eq!(path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
            } else {
                assert_eq!(path_condition, HlsOriginPathCondition::ProgressExpected);
            }
        }
    }

    #[tokio::test]
    async fn transient_media_failure_degrades_availability_without_direct_terminal_transition() {
        for kind in [TransientResourceKind::Segment, TransientResourceKind::Part] {
            let fixture = TestDirectResponseFixture::new(kind, "http://127.0.0.1/media.bin");

            assert!(
                !record_temporary_transient_segment_fetch_failure(
                    &fixture.session,
                    &fixture.resource,
                    &fixture.policy,
                    100,
                )
                .await
            );

            let session = fixture.session.read().await;
            assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 1);
            assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
            assert!(session.origin_control.acceptance_episode.is_none());
            assert!(!matches!(
                session.origin_control.progress_phase,
                HlsOriginProgressPhase::Terminal | HlsOriginProgressPhase::TerminalPartial
            ));
            drop(session);

            super::record_successful_transient_segment_fetch(&fixture.session, &fixture.resource).await;
            let session = fixture.session.read().await;
            assert_eq!(session.segment_failure_tracker.consecutive_temporary_failures, 0);
            assert_eq!(session.origin_control.path_condition, HlsOriginPathCondition::SegmentReadinessFailure);
        }
    }

    #[tokio::test]
    async fn direct_stream_decoder_failure_aborts_body_without_origin_retry() {
        let mut truncated = gzip_encode(b"decoder failure after response headers").await;
        truncated.truncate(truncated.len().saturating_sub(8));
        let origin = spawn_test_origin(
            "200 OK",
            vec![("Content-Encoding", "gzip"), ("Content-Type", "application/octet-stream")],
            truncated,
        )
        .await;
        let decoded = fetch_direct_decoded(format!("{}/key.bin", origin.base_url), TransientResourceKind::Key, None)
            .await
            .expect("decoder setup succeeds before streaming");
        let response = direct_client_response(decoded, TransientResourceKind::Key);

        assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
        assert_eq!(origin.requests.lock().await.len(), 1, "streaming errors must not trigger a hidden retry");
    }

    #[tokio::test]
    async fn direct_stream_refreshes_origin_body_idle_deadline_after_each_chunk() {
        const BODY_IDLE_TIMEOUT_MS: u64 = 500;
        let chunks = [b"slow-".to_vec(), b"but-".to_vec(), b"continuous-".to_vec(), b"body".to_vec()];
        let expected = chunks.concat();
        let origin = spawn_test_origin_in_chunks(
            "200 OK",
            vec![("Content-Type", "application/octet-stream")],
            chunks.into(),
            Duration::from_millis(200),
        )
        .await;
        let decoded = fetch_direct_decoded_with_timeout(
            format!("{}/key.bin", origin.base_url),
            TransientResourceKind::Key,
            None,
            BODY_IDLE_TIMEOUT_MS,
        )
        .await
        .expect("direct response setup succeeds");
        let response = direct_client_response(decoded, TransientResourceKind::Key);

        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.expect("progressing body outlives total timeout"),
            expected
        );
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn cacheable_transient_rejects_identity_partial_response_before_commit_and_cleans_guard() {
        let origin = spawn_test_origin(
            "206 Partial Content",
            vec![("Content-Type", "video/mp2t"), ("Content-Range", "bytes 0-3/10")],
            b"part".to_vec(),
        )
        .await;
        let fixture = TestTransientCacheFixture::new(format!("{}/partial.ts", origin.base_url)).await;
        let dropped_guards = Arc::new(AtomicUsize::new(0));
        let dropped_guards_for_prepare = Arc::clone(&dropped_guards);

        let result =
            fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(fixture.request(None), move |_| {
                let dropped_guards = Arc::clone(&dropped_guards_for_prepare);
                async move { Ok(DropCounter(dropped_guards)) }.boxed()
            })
            .await;

        assert!(matches!(result, Err(HlsOriginResourceFetchError::UnexpectedByteRangeStatus)));
        assert_eq!(dropped_guards.load(Ordering::Relaxed), 1);
        assert!(fixture.segment_cache.metadata(&fixture.cache_key).await.expect("cache metadata reads").is_none());
        assert!(!fixture.segment_cache.has_active_temp_files());
        assert_eq!(std::fs::read_dir(fixture.temp_dir.path()).expect("cache root reads").count(), 0);
        let session = fixture.session.read().await;
        let entry = session.transient.object_cache.get(&fixture.lookup_key).expect("fetching cache entry remains");
        assert!(!matches!(entry.status, TransientObjectCacheStatus::Ready { .. }));
        drop(session);

        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = requests[0].to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity"));
        assert!(!request.contains("\r\nrange:"));
    }

    #[tokio::test]
    async fn stale_resource_revision_commit_deletes_the_physical_fill_after_controlled_mapping_replacement() {
        let origin = spawn_test_origin("200 OK", vec![("Content-Type", "video/mp2t")], b"stale-body".to_vec()).await;
        let fixture = TestTransientCacheFixture::new(format!("{}/stale.ts", origin.base_url)).await;
        {
            let mut session = fixture.session.write().await;
            let replacement_now_ms = super::current_time_millis();
            let mut replacement = TransientResourceRef::new(
                TransientResourceKind::Segment,
                "http://replacement.example.com/live/replacement.ts",
                b"rewrite-secret",
                replacement_now_ms,
                60_000,
                Some("ts".to_string()),
            );
            replacement.id = fixture.resource.id.clone();
            session.transient.upsert_resources([replacement]);
        }

        let result = fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(fixture.request(None), |_| {
            async { Ok(()) }.boxed()
        })
        .await;

        assert!(
            matches!(result, Err(HlsOriginResourceFetchError::Superseded)),
            "the superseded resource revision aborts without a retryable timeout"
        );
        assert!(fixture.segment_cache.metadata(&fixture.cache_key).await.expect("cache metadata reads").is_none());
        assert!(!fixture.segment_cache.has_active_temp_files());
        assert!(!fixture.session.read().await.transient.object_cache.contains_key(&fixture.lookup_key));
        assert_eq!(origin.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn cacheable_transient_without_range_decodes_to_identity_before_cache_and_range_reads() {
        let ciphertext =
            vec![0x00, 0xff, 0x92, 0x10, 0x7e, 0x33, 0xc4, 0x81, 0xa8, 0x09, 0x5d, 0xe2, 0x44, 0x17, 0xb0, 0x6f];
        assert!(std::str::from_utf8(&ciphertext).is_err());
        let encoded = zstd_encode(&ciphertext).await;
        assert_ne!(encoded, ciphertext);
        let origin = spawn_zstd_origin(encoded).await;
        let fixture = TestTransientCacheFixture::new(format!("{}/cipher.ts", origin.base_url)).await;

        fetch_and_commit_hls_transient_origin_response_with_attempt_prepare(fixture.request(None), |_| {
            async { Ok(()) }.boxed()
        })
        .await
        .expect("encoded transient cache fetch succeeds");

        let metadata = fixture
            .segment_cache
            .metadata(&fixture.cache_key)
            .await
            .expect("cache metadata reads")
            .expect("decoded object is cached");
        assert_eq!(metadata.size, ciphertext.len() as u64);
        assert_eq!(tokio::fs::read(&metadata.path).await.expect("cache body reads"), ciphertext);
        let mut range = Vec::new();
        fixture
            .segment_cache
            .open_range(&fixture.cache_key, 5)
            .await
            .expect("decoded cache range opens")
            .take(4)
            .read_to_end(&mut range)
            .await
            .expect("decoded cache range reads");
        assert_eq!(range, ciphertext[5..9]);

        let session = fixture.session.read().await;
        let entry = session.transient.object_cache.get(&fixture.lookup_key).expect("transient cache entry");
        assert!(matches!(
            entry.status,
            TransientObjectCacheStatus::Ready { content_length, .. }
                if content_length == ciphertext.len() as u64
        ));
        assert_eq!(entry.content_type, "application/octet-stream");
        drop(session);

        let requests = origin.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = requests[0].to_ascii_lowercase();
        assert!(request.contains("accept-encoding: identity"));
        assert!(!request.contains("\r\nrange:"));
    }
}
