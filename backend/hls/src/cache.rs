use super::{safe_proxy_session_id, ProxyMapId, ProxySessionId, TransientResourceId};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak},
};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, Semaphore};

pub const DEFAULT_HLS_CACHE_PATH: &str = "/tmp/tuliprox/cache/hls";

const TEMP_CREATE_ATTEMPTS: usize = 8;

const MAX_CONCURRENT_OWNED_CACHE_OPERATIONS: usize = 64;

pub(crate) const MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN: usize = 128;

/// Stable cache key for one proxy-visible HLS segment.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct SegmentCacheKey {
    session_id: ProxySessionId,
    seq: u64,
    file_ext: String,
}

/// Stable cache key for one proxy-visible HLS EXT-X-MAP object.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct MapCacheKey {
    session_id: ProxySessionId,
    map_id: ProxyMapId,
    file_ext: String,
}

/// Stable cache key for one demand-cached transient passthrough object.
///
/// This key is not a provider-source identity. It keys the proxy-visible object for a concrete transient resource.
/// The concrete origin fetch URI is kept in `TransientResourceRef`; callers must not reconstruct it from this key or
/// force host-neutral cache hits across redirect/CDN contexts without a separate safe resource identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct TransientObjectCacheKey {
    session_id: ProxySessionId,
    resource_id: TransientResourceId,
    file_ext: String,
}

#[derive(Clone)]
pub struct StagedCacheObject {
    pub path: PathBuf,
    pub size: u64,
    cache_path_generation: Arc<CachePathGeneration>,
    _registration: Arc<ActiveTempFileRegistration>,
}

/// Typed source used to distinguish a decoded-object size violation from a filesystem failure.
#[derive(Debug, thiserror::Error)]
#[error("hls cache object exceeds configured size limit {limit}")]
pub struct HlsCacheObjectLimitError {
    limit: u64,
}

/// Revision of cache accounting or of the protected working set.
///
/// Deferred segment work waits for this opaque token to become stale before it is
/// requeued. This gives `LocalCacheCapacity` a concrete retry contract without a
/// timer-driven origin-download loop.
#[derive(Clone)]
pub struct HlsCacheCapacityRevision(Arc<CapacityRevision>);

/// Typed local budget deferral kept distinct from filesystem capacity failures.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "hls cache capacity unavailable (required_session_bytes={required_session_bytes}, required_global_bytes={required_global_bytes})"
)]
pub struct HlsCacheCapacityError {
    configured_session_bytes: u64,
    configured_global_bytes: u64,
    current_session_bytes: u64,
    current_global_bytes: u64,
    staged_bytes: u64,
    required_session_bytes: u64,
    required_global_bytes: u64,
    protected_working_set_bytes: u64,
    reclaimable_bytes: u64,
    revision: HlsCacheCapacityRevision,
}

/// File-backed cache for committed HLS segment objects.
///
/// Lock order: the capacity-admission gate serializes authoritative admission, projected reclamation, and the following
/// reservation. It is acquired before the cache-path read lease because cache-root handoff orders GC before the
/// exclusive cache-path gate. The read lease is released before calling the GC reclaimer and reacquired for generation
/// validation and reservation; after projected accounting is reserved, the pressure gate is released and the read lease
/// spans the atomic rename.
/// The `temp_files`, `capacity`, path, reclaimer, and marker standard-library locks are never nested with one another
/// and are always released before filesystem I/O or `.await`.
pub struct HlsSegmentCache {
    cache_path: StdRwLock<CachePathState>,
    cache_path_commit_gate: Arc<RwLock<()>>,
    temp_files: Arc<StdMutex<TempFileState>>,
    max_object_bytes: AtomicU64,
    max_cache_bytes: AtomicU64,
    max_session_bytes: AtomicU64,
    marker_path: StdRwLock<Option<PathBuf>>,
    // Projected totals and per-object mutation ownership are updated atomically under this short synchronous lock.
    capacity: Arc<StdMutex<CacheCapacityState>>,
    capacity_changed: Arc<Notify>,
    capacity_reclaimer: StdRwLock<Option<Weak<dyn HlsCacheCapacityReclaimer>>>,
    capacity_admission_gate: AsyncMutex<()>,
    owned_operation_permits: Arc<Semaphore>,
}

#[derive(Clone)]
struct CachePathState {
    path: PathBuf,
    generation: Arc<CachePathGeneration>,
}

struct CachePathGeneration;

#[derive(Default)]
struct CacheCapacityState {
    cache_path: PathBuf,
    initialized: bool,
    total_bytes: u64,
    session_bytes: HashMap<String, u64>,
    revision: Arc<CapacityRevision>,
    active_mutations: HashSet<PathBuf>,
    revision_reservations: HashMap<u64, (String, u64)>,
    /// Revision file bytes found by the last completed usage scan.
    revision_bytes: u64,
}

#[derive(Default)]
struct CapacityRevision;

#[derive(Default)]
struct TempFileState {
    active_files: HashSet<PathBuf>,
    deletion_reservations: HashSet<PathBuf>,
}

struct ActiveTempFileRegistration {
    path: PathBuf,
    state: Arc<StdMutex<TempFileState>>,
}

impl Drop for ActiveTempFileRegistration {
    fn drop(&mut self) {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).active_files.remove(&self.path);
    }
}

struct CachePathDeletionReservation {
    path: PathBuf,
    state: Arc<StdMutex<TempFileState>>,
}

struct CapacityInvalidationGuard {
    capacity: Arc<StdMutex<CacheCapacityState>>,
    changed: Arc<Notify>,
}

impl Drop for CachePathDeletionReservation {
    fn drop(&mut self) {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).deletion_reservations.remove(&self.path);
    }
}

struct CapacityMutationReservation {
    path: PathBuf,
    cache_path: PathBuf,
    session_component: String,
    replacement: Option<(u64, u64)>,
    filesystem_mutation_started: bool,
    capacity: Arc<StdMutex<CacheCapacityState>>,
    changed: Arc<Notify>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
struct CacheCapacityPressure {
    configured_session_bytes: u64,
    configured_global_bytes: u64,
    current_session_bytes: u64,
    current_global_bytes: u64,
    staged_bytes: u64,
    required_session_bytes: u64,
    required_global_bytes: u64,
}

impl Default for HlsSegmentCache {
    fn default() -> Self { Self::new() }
}

/// A peak reservation includes raw data, cache staging, repair output and immutable revisions.
pub struct HlsRevisionDiskReservation {
    id: u64,
    capacity: Arc<StdMutex<CacheCapacityState>>,
    changed: Arc<Notify>,
    /// Set when bytes were written under this reservation outside the staged commit accounting.
    unaccounted_writes: bool,
}

pub struct HlsRevisionFilePin {
    _registration: Arc<ActiveTempFileRegistration>,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod revision_capacity_tests;

mod capacity;
mod error;
mod keys;
mod lifecycle;
mod revisions;
mod staging;
#[cfg(test)]
use capacity::CapacityReservationError;
#[allow(unused_imports, reason = "Retains the existing module interface in production and test builds.")]
pub use capacity::{
    CacheInvalidationOutcome, HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest,
    HlsCacheCapacityReclaimer, HlsCacheCapacityUsage,
};
pub use error::{hls_cache_capacity_from_io, hls_cache_object_limit_from_io};
pub use keys::HlsCacheObjectKey;
pub use staging::CachedSegmentMetadata;
