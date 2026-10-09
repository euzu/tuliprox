#![allow(clippy::large_futures)]

use super::{
    begin_hls_origin_account_io_bounded, build_hls_origin_resource_headers, classify_hls_backpressure,
    fetch_hls_transient_origin_response_with_attempt_prepare, finish_hls_origin_account_io, hls_object_body_deadline,
    run_hls_origin_resource_retry_loop_with_attempt_prepare, transient::transient_object_expires_at,
    CachedSegmentMetadata, HlsAccessLeaseId, HlsAccessLeaseStore, HlsBackpressureState,
    HlsBoundAccountAcquireErrorKind, HlsCacheCapacityRevision, HlsCacheMetrics, HlsCacheObjectKey,
    HlsOriginAccountIoLeaseGuard, HlsOriginByteRangeExpectation, HlsOriginIoContext, HlsOriginResourceBodyDeadline,
    HlsOriginResourceClients, HlsOriginResourceFetchError, HlsOriginResourceFetchTarget, HlsRepairRenderedObjectId,
    HlsResourceFetchKind, HlsResourceFetchSource, HlsSegmentCache, HlsSegmentEncryption, HlsSegmentFailureObject,
    HlsSegmentFailureTransition, HlsSegmentFile, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairSource, HlsSessionHandle, HlsTransientObjectFetchFinalizer, HlsTransientOriginFetchRequest,
    OriginSegmentFetchRef, OriginSegmentKey, ProxySessionId, SegmentCacheKey, SegmentCacheStatus, SegmentEntry,
    SegmentFetchPriority, StagedCacheObject, TransientObjectFetchDecision, TransientObjectFetchToken,
    TransientObjectUnavailableState, TransientPassthroughState, TransientResourceFile, TransientResourceId,
    TransientResourceKind, TransientResourceRef,
};
use arc_swap::ArcSwap;
use axum::http::HeaderMap;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;

const DEFAULT_MAX_GLOBAL_SEGMENT_FETCHES: usize = 64;

const DEFAULT_MAX_SESSION_SEGMENT_FETCHES: usize = 2;

const DEFAULT_MAX_PREFETCH_QUEUE_DEPTH: usize = 6;

const DEFAULT_ORIGIN_SEGMENT_TIMEOUT_MS: u64 = 10_000;

const DEFAULT_REPAIR_POSTPROCESS_TIMEOUT_MS: u64 = 2_000;

const SEGMENT_FETCH_SCHEDULING_MARGIN_MS: u64 = 1_000;

/// Origin-object work which must complete before one media segment is usable.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsSegmentFetchWorkload {
    Clear,
    EncryptedWithKey,
}

/// Runtime policy for bounded live HLS segment origin fetches.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SegmentFetchPolicy {
    pub max_global_segment_fetches: usize,
    pub max_session_segment_fetches: usize,
    pub max_prefetch_queue_depth: usize,
    pub origin_segment_timeout_ms: u64,
    pub effective_repair_postprocess_timeout_ms: u64,
    pub retry_delays_ms: [u64; 5],
    pub retry_jitter_max_ms: u64,
    pub permanent_failure_segment_threshold: u32,
}

/// Shared context required to schedule a segment fetch without holding session locks.
#[derive(Clone)]
pub struct SegmentFetchContext {
    pub session: HlsSessionHandle,
    pub segment_cache: Arc<HlsSegmentCache>,
    pub segment_repair: Arc<HlsSegmentRepairManager>,
    pub repair_access_lease_id: Option<HlsAccessLeaseId>,
    pub headers: HeaderMap,
    pub origin_provider_session_headers: HeaderMap,
    pub client: Client,
    pub no_redirect_client: Client,
    pub use_manual_redirects: bool,
    pub origin_io: Option<HlsOriginIoContext>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SegmentDemandFetchOutcome {
    Ready,
    QueuedOrFetching,
    NotFound,
    Unavailable,
    TimedOut,
}

/// Resource class staged before a cross-host manifest handoff is allowed to mutate the timeline.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum HlsSwitchResourceKind {
    Segment,
    Map,
}

/// Bounded scheduler for live HLS segment demand fetches and prefetches.
pub struct HlsSegmentWorkerPool {
    runtime: ArcSwap<SegmentWorkerRuntime>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    metrics: Arc<HlsCacheMetrics>,
    availability_reevaluations: Option<Arc<super::availability_reevaluation::HlsAvailabilityReevaluationCoordinator>>,
}

#[cfg(test)]
mod tests;

mod commit;
mod demand;
mod error;
mod key_dependency;
mod origin;
mod policy;
mod scheduler;
mod startup_fill;
mod switch_staging;

#[cfg(test)]
use self::commit::commit_failed_segment_fetch;
pub(crate) use self::commit::recompute_unpublished_live_head;
#[cfg(test)]
use self::key_dependency::wait_for_segment_key_dependency;
#[cfg(test)]
use self::origin::fetch_segment_with_retries_into_cache;
#[cfg(test)]
use self::scheduler::take_queued_segment_fetch_candidate;
#[cfg(test)]
use self::startup_fill::prepare_startup_fill;
pub use self::switch_staging::stage_hls_switch_resource;
use self::{
    commit::{
        segment_fetch_attempt_matches, segment_fetch_binding_matches, SegmentFetchCommit, SegmentOriginWorkFinish,
    },
    error::SegmentFetchError,
    key_dependency::{
        ensure_segment_key_dependency_ready, fetch_segment_key_dependency_into_cache, select_key_dependency,
        select_segment_key_dependency, ReadySegmentKeyFetchSnapshot, SegmentKeyBindingSnapshot, SegmentKeyDependency,
        SegmentKeyDependencySelection,
    },
    origin::{
        build_segment_origin_headers, fetch_segment_into_cache, finish_segment_origin_attempt,
        prepare_segment_origin_attempt,
    },
    scheduler::{
        mark_segment_discovered, ScheduledSegmentRetry, SegmentFetchSnapshot, SegmentKeyFetchDependency,
        SegmentRetryWake, SegmentWorkerRuntime,
    },
    startup_fill::{commit_startup_response, reliable_decoded_content_length},
};
