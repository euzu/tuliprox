use super::{
    origin::{build_hls_transient_resource_fetch_target, HlsTransientOriginFetchMode},
    run_hls_origin_resource_retry_loop_with_attempt_prepare, transient_object_expires_at, HlsAccessLeaseId,
    HlsOriginResourceBodyDeadline, HlsOriginResourceFetchError, HlsPublishedTransientResourceIds,
    HlsRepairRenderedObjectId, HlsResourceFetchAttempt, HlsSegmentCache, HlsSegmentRepairManager,
    HlsSegmentRepairObjectContext, HlsSegmentRepairSource, HlsSessionHandle, HlsSessionMode,
    HlsTransientObjectCacheActionInput, HlsTransientObjectFetchFinalizer, HlsTransientOriginFetchRequest,
    ProxySessionId, TransientObjectCacheKey, TransientObjectFetchDecision, TransientObjectFetchToken,
    TransientPassthroughState, TransientResourceFile, TransientResourceKind, TransientResourceRef,
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use futures::{future::BoxFuture, FutureExt};
use log::debug;
use std::sync::Arc;
use tokio::sync::Notify;
use tuliprox_core::utils::{content_coding::DecodedHttpResponse, current_time_millis};

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
pub struct HlsTransientResourceLeaseContext<'a> {
    pub access_lease_id: &'a HlsAccessLeaseId,
    pub lease_issued_at_ms: u64,
    pub published_resource_ids: &'a HlsPublishedTransientResourceIds,
}

pub async fn resolve_hls_transient_object_cache_action(
    session: &HlsSessionHandle,
    proxy_session_id: &ProxySessionId,
    lease: HlsTransientResourceLeaseContext<'_>,
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
    let Some(resource) = session.transient.resolve_resource_for_lease(
        &resource_file.resource_id,
        lease.access_lease_id,
        lease.lease_issued_at_ms,
        lease.published_resource_ids,
        now_ms,
    ) else {
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
    session: &mut super::super::HlsSession,
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
    if session.transient.ready_object(&cache_key, input.resource.kind, input.now_ms).is_some() {
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

pub(crate) async fn delete_superseded_transient_object(
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
    pub proxy_session_id: ProxySessionId,
    pub log_identity: super::super::HlsLogIdentity,
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
    let proxy_session_id = context.proxy_session_id.clone();
    let log_identity = context.log_identity.clone();
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
    let expires_at_ms = transient_object_expires_at(ready_at_ms, context.cache_duration_ms);
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
