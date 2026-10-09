use super::{
    hls_ctx::HlsCtx, renderer_candidate_window_proxy_seqs, safe_proxy_session_id,
    transient::extract_transient_resource_ids, CacheInvalidationOutcome, HlsAccessLeaseStore,
    HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest, HlsCacheCapacityReclaimer, HlsCacheMetrics,
    HlsExpiredSessionReason, HlsSegmentCache, HlsSession, HlsSessionHandle, HlsSessionStore, MapCacheKey,
    MapCacheStatus, ProxyMapId, ProxySessionId, SegmentCacheKey, SegmentCacheStatus, TransientObjectCacheKey,
    TransientResourceId,
};
use arc_swap::ArcSwap;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak},
    time::Duration,
};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

const HLS_CACHE_GC_INTERVAL: Duration = Duration::from_secs(30);

const DEFAULT_TEMP_FILE_RETENTION_MS: u64 = 30_000;

const DEFAULT_FAILED_SEGMENT_RETENTION_MS: u64 = 10_000;

const MAX_PENDING_CACHE_DELETIONS: usize = 1_024;

const MAX_CACHE_DELETE_RETRIES_PER_RUN: usize = 128;

// A switch can own one segment and one MAP rollback. Keep capacity for 64 concurrent handoffs even while a GC batch
// is selecting ordinary deletions; the shared hard bound remains MAX_PENDING_CACHE_DELETIONS.
const SWITCH_CACHE_CLEANUP_HEADROOM: usize = 128;

pub struct HlsGarbageCollector {
    sessions: Arc<HlsSessionStore>,
    cache: Arc<HlsSegmentCache>,
    policy: ArcSwap<GarbageCollectionPolicy>,
    rewrite_secret_fingerprint: ArcSwap<String>,
    metrics: Arc<HlsCacheMetrics>,
    pending_cache_deletions: Arc<StdMutex<CacheDeletionQueueState>>,
    access_leases: StdRwLock<Option<Weak<RwLock<HlsAccessLeaseStore>>>>,
    run_once_gate: AsyncMutex<()>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct PendingCacheObjectDeletion {
    deletion: CacheObjectDeletion,
    attempts: u16,
}

struct CacheDeletionQueueState {
    pending: VecDeque<PendingCacheObjectDeletion>,
    reserved_slots: usize,
    generation: Arc<CacheDeletionQueueGeneration>,
}

/// Bounded rollback ownership for one physically committed but not yet timeline-committed switch object.
pub struct HlsSwitchCacheCleanupReservation {
    queue: Arc<StdMutex<CacheDeletionQueueState>>,
    queue_generation: Arc<CacheDeletionQueueGeneration>,
    deletion: Option<CacheObjectDeletion>,
    slot_reserved: bool,
}

/// Owns a bounded reservation in the retry queue while session metadata is changed.
///
/// Lock order: callers may hold a session write lock while appending to this in-memory batch, but the queue mutex is
/// acquired only by `persist`/`drop` and never across filesystem I/O or an `.await`. Drop atomically persists any
/// collected deletions before releasing unused slots, so cancellation cannot orphan files whose metadata is gone.
struct CacheDeletionBatch {
    queue: Arc<StdMutex<CacheDeletionQueueState>>,
    reserved_slots: usize,
    deletions: Vec<CacheObjectDeletion>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct SegmentDeleteCandidate {
    proxy_seq: u64,
    content_length: u64,
    last_relevant_at_ms: u64,
}

#[derive(Clone)]
struct GlobalSegmentCandidate {
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    proxy_seq: u64,
    content_length: u64,
    last_relevant_at_ms: u64,
}

#[derive(Clone)]
struct GlobalTransientObjectCandidate {
    session: HlsSessionHandle,
    proxy_session_id: ProxySessionId,
    last_accessed_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct CapacityReclamationEvidence {
    protected_working_set_bytes: u64,
    reclaimable_bytes: u64,
}

struct MapEntryDeletion {
    key: MapCacheKey,
    content_length: u64,
}

#[cfg(test)]
mod tests;

mod collector;
mod deletion;
mod policy;
mod reclaim;
mod report;
mod selection;
pub use collector::{build_rewrite_secret_fingerprint, exec_hls_cache_gc};
use deletion::{CacheDeletionQueueGeneration, CacheObjectDeletion};
pub use policy::{GarbageCollectionPolicy, ProtectedSet};
pub use report::GarbageCollectionReport;
