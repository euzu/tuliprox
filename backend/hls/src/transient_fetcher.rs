use super::{
    build_hls_origin_resource_headers_with_client_range, hls_client_body_send_deadline,
    refresh_hls_client_body_send_deadline,
    resource_fetch::{log_hls_resource_body_failure, HlsResourceFetchLogContext},
    run_hls_origin_resource_retry_loop_with_attempt_prepare,
    transient::transient_object_expires_at,
    CacheAccessState, HlsAccessLeaseId, HlsLogIdentity, HlsOriginAccountIoLeaseGuard, HlsOriginByteRangeExpectation,
    HlsOriginIoContext, HlsOriginResourceBodyDeadline, HlsOriginResourceClients, HlsOriginResourceFetchError,
    HlsOriginResourceFetchTarget, HlsPublishedTransientResourceIds, HlsRepairRenderedObjectId, HlsResourceFetchAttempt,
    HlsResourceFetchKind, HlsResourceFetchSource, HlsSegmentCache, HlsSegmentFailureObject,
    HlsSegmentFailureTransition, HlsSegmentRepairManager, HlsSegmentRepairObjectContext, HlsSegmentRepairSource,
    HlsSessionHandle, HlsSessionMode, ProxySessionId, SegmentFetchPolicy, TransientObjectCacheKey,
    TransientObjectFetchDecision, TransientObjectFetchToken, TransientPassthroughState, TransientResourceFile,
    TransientResourceKind, TransientResourceRef,
};
use axum::http::HeaderValue;
use std::sync::Arc;

#[derive(Clone, Copy)]
struct HlsTransientObjectCacheActionInput<'a> {
    proxy_session_id: &'a ProxySessionId,
    resource: &'a TransientResourceRef,
    resource_file: &'a TransientResourceFile,
    range_header: Option<&'a HeaderValue>,
    now_ms: u64,
    cache_duration_ms: u64,
    key_object_cache_allowed: bool,
}

pub struct HlsTransientObjectFetchFinalizer {
    session: HlsSessionHandle,
    segment_cache: Arc<HlsSegmentCache>,
    fetch_token: TransientObjectFetchToken,
    completed: bool,
    retry_after_ms: u64,
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

struct HlsTransientReadGuard {
    access: Arc<CacheAccessState>,
}

pub struct HlsTransientOriginIoGuard {
    session: HlsSessionHandle,
    active_origin_work_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    origin_io: HlsOriginIoContext,
    lease_guard: Option<HlsOriginAccountIoLeaseGuard>,
    started_generation: u64,
    origin_work_finished: bool,
}

#[cfg(test)]
mod tests;

mod cache;
mod lifecycle;
mod origin;
mod response;
pub(super) use cache::delete_superseded_transient_object;
#[allow(unused_imports, reason = "Preserves the existing module interface.")]
pub use cache::{
    fetch_and_commit_hls_transient_origin_response_with_attempt_prepare,
    is_hls_transient_full_object_cacheable_request, resolve_hls_transient_object_cache_action,
    HlsTransientCacheCommitContext, HlsTransientObjectCacheAction, HlsTransientObjectCacheResolution,
    HlsTransientOriginCacheFetchRequest, HlsTransientResourceLeaseContext,
};
pub use lifecycle::{record_successful_transient_segment_fetch, record_temporary_transient_segment_fetch_failure};
pub use origin::{
    fetch_hls_transient_origin_response_with_attempt_prepare, hls_transient_object_fetch_failure,
    hls_transient_resource_fetch_kind, HlsTransientDecodedOriginResponse, HlsTransientObjectFetchFailure,
    HlsTransientOriginFetchRequest,
};
#[cfg(test)]
use response::HlsTransientDirectStreamOutcome;
pub use response::{hls_transient_origin_response, HlsTransientDirectResponseContext};
