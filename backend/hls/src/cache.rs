use super::{safe_proxy_session_id, ProxyMapId, ProxySessionId, TransientResourceId};
use futures::future::BoxFuture;
use log::warn;
use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak,
    },
    time::SystemTime,
};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::{Mutex as AsyncMutex, Notify, RwLock, Semaphore},
    time::{timeout_at, Instant},
};

pub const DEFAULT_HLS_CACHE_PATH: &str = "/tmp/tuliprox/cache/hls";
const TEMP_CREATE_ATTEMPTS: usize = 8;
const MAX_CONCURRENT_OWNED_CACHE_OPERATIONS: usize = 64;
const MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN: usize = 128;

/// Stable cache key for one proxy-visible HLS segment.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct SegmentCacheKey {
    session_id: ProxySessionId,
    seq: u64,
    file_ext: String,
}

impl SegmentCacheKey {
    pub fn new(proxy_session_id: ProxySessionId, proxy_seq: u64, proxy_file_ext: impl Into<String>) -> Self {
        Self { session_id: proxy_session_id, seq: proxy_seq, file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls:{}:{:020}", self.session_id.0, self.seq) }

    pub fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    pub fn proxy_seq(&self) -> u64 { self.seq }
}

impl fmt::Debug for SegmentCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentCacheKey")
            .field("session_id", &"<redacted>")
            .field("seq", &self.seq)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

/// Stable cache key for one proxy-visible HLS EXT-X-MAP object.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct MapCacheKey {
    session_id: ProxySessionId,
    map_id: ProxyMapId,
    file_ext: String,
}

impl MapCacheKey {
    pub fn new(
        proxy_session_id: ProxySessionId,
        proxy_map_id: impl Into<ProxyMapId>,
        proxy_file_ext: impl Into<String>,
    ) -> Self {
        Self { session_id: proxy_session_id, map_id: proxy_map_id.into(), file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls-map:{}:{:020}", self.session_id.0, self.map_id.0) }

    pub fn proxy_map_id(&self) -> ProxyMapId { self.map_id }
}

impl fmt::Debug for MapCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapCacheKey")
            .field("session_id", &"<redacted>")
            .field("map_id", &self.map_id.0)
            .field("file_ext", &self.file_ext)
            .finish()
    }
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

impl TransientObjectCacheKey {
    pub fn new(
        proxy_session_id: ProxySessionId,
        transient_resource_id: TransientResourceId,
        proxy_file_ext: impl Into<String>,
    ) -> Self {
        Self { session_id: proxy_session_id, resource_id: transient_resource_id, file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls-transient:{}:{}", self.session_id.0, self.resource_id.0) }

    pub fn transient_resource_id(&self) -> &TransientResourceId { &self.resource_id }
}

impl fmt::Debug for TransientObjectCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientObjectCacheKey")
            .field("session_id", &"<redacted>")
            .field("resource_id", &self.resource_id)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

/// Resolves a proxy cache object to a safe path below the configured HLS cache root.
pub trait HlsCacheObjectKey {
    fn proxy_session_id(&self) -> &ProxySessionId;
    fn session_path_component(&self) -> String;
    fn file_name(&self) -> String;
}

impl HlsCacheObjectKey for SegmentCacheKey {
    fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("{:06}.{}", self.seq, self.file_ext) }
}

impl HlsCacheObjectKey for MapCacheKey {
    fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("map/{:06}.{}", self.map_id.0, self.file_ext) }
}

impl HlsCacheObjectKey for TransientObjectCacheKey {
    fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("r/{}.{}", self.resource_id.0, self.file_ext) }
}

/// Filesystem metadata for a committed HLS cache object.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CachedSegmentMetadata {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Clone)]
pub struct StagedCacheObject {
    pub path: PathBuf,
    pub size: u64,
    cache_path_generation: Arc<CachePathGeneration>,
    _registration: Arc<ActiveTempFileRegistration>,
}

impl StagedCacheObject {
    fn registered(
        path: PathBuf,
        size: u64,
        cache_path_generation: Arc<CachePathGeneration>,
        registration: Arc<ActiveTempFileRegistration>,
    ) -> Self {
        Self { path, size, cache_path_generation, _registration: registration }
    }
}

impl fmt::Debug for StagedCacheObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedCacheObject")
            .field("path", &self.path)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl PartialEq for StagedCacheObject {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.size == other.size
            && Arc::ptr_eq(&self.cache_path_generation, &other.cache_path_generation)
    }
}

impl Eq for StagedCacheObject {}

/// Typed source used to distinguish a decoded-object size violation from a filesystem failure.
#[derive(Debug, thiserror::Error)]
#[error("hls cache object exceeds configured size limit {limit}")]
pub struct HlsCacheObjectLimitError {
    limit: u64,
}

impl HlsCacheObjectLimitError {
    pub fn limit(&self) -> u64 { self.limit }
}

pub fn hls_cache_object_limit_from_io(error: &io::Error) -> Option<&HlsCacheObjectLimitError> {
    let mut source: &(dyn std::error::Error + 'static) = error.get_ref()?;
    loop {
        if let Some(limit_error) = source.downcast_ref::<HlsCacheObjectLimitError>() {
            return Some(limit_error);
        }
        source = source.source()?;
    }
}

fn cache_object_limit_error(limit: u64) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, HlsCacheObjectLimitError { limit })
}

/// Revision of cache accounting or of the protected working set.
///
/// Deferred segment work waits for this opaque token to become stale before it is
/// requeued. This gives `LocalCacheCapacity` a concrete retry contract without a
/// timer-driven origin-download loop.
#[derive(Clone)]
pub struct HlsCacheCapacityRevision(Arc<CapacityRevision>);

impl fmt::Debug for HlsCacheCapacityRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HlsCacheCapacityRevision(<opaque>)")
    }
}

impl HlsCacheCapacityRevision {
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test() -> Self { Self(Arc::new(CapacityRevision)) }
}

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

impl HlsCacheCapacityError {
    pub fn configured_session_bytes(&self) -> u64 { self.configured_session_bytes }

    pub fn configured_global_bytes(&self) -> u64 { self.configured_global_bytes }

    pub fn current_session_bytes(&self) -> u64 { self.current_session_bytes }

    pub fn current_global_bytes(&self) -> u64 { self.current_global_bytes }

    pub fn staged_bytes(&self) -> u64 { self.staged_bytes }

    pub fn required_session_bytes(&self) -> u64 { self.required_session_bytes }

    pub fn required_global_bytes(&self) -> u64 { self.required_global_bytes }

    pub fn revision(&self) -> &HlsCacheCapacityRevision { &self.revision }

    #[cfg(any(test, feature = "test-support"))]
    pub fn protected_working_set_bytes(&self) -> u64 { self.protected_working_set_bytes }

    #[cfg(any(test, feature = "test-support"))]
    pub fn reclaimable_bytes(&self) -> u64 { self.reclaimable_bytes }

    fn pressure(&self) -> CacheCapacityPressure {
        CacheCapacityPressure {
            configured_session_bytes: self.configured_session_bytes,
            configured_global_bytes: self.configured_global_bytes,
            current_session_bytes: self.current_session_bytes,
            current_global_bytes: self.current_global_bytes,
            staged_bytes: self.staged_bytes,
            required_session_bytes: self.required_session_bytes,
            required_global_bytes: self.required_global_bytes,
        }
    }
}

pub fn hls_cache_capacity_from_io(error: &io::Error) -> Option<&HlsCacheCapacityError> {
    let mut source: &(dyn std::error::Error + 'static) = error.get_ref()?;
    loop {
        if let Some(capacity_error) = source.downcast_ref::<HlsCacheCapacityError>() {
            return Some(capacity_error);
        }
        source = source.source()?;
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct HlsCacheCapacityReclaimRequest {
    pub proxy_session_id: ProxySessionId,
    pub target_path: PathBuf,
    pub required_session_bytes: u64,
    pub required_global_bytes: u64,
    pub staged_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct HlsCacheCapacityReclaimOutcome {
    pub reclaimed_session_bytes: u64,
    pub reclaimed_global_bytes: u64,
    pub protected_working_set_bytes: u64,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct HlsCacheCapacityUsage {
    pub session_bytes: u64,
    pub global_bytes: u64,
}

/// Existing GC integration used to make room for one already-staged cache object.
pub trait HlsCacheCapacityReclaimer: Send + Sync {
    fn reclaim_capacity(
        &self,
        request: HlsCacheCapacityReclaimRequest,
    ) -> BoxFuture<'_, io::Result<HlsCacheCapacityReclaimOutcome>>;
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CacheInvalidationOutcome {
    Invalidated,
    DeferredActiveTempFiles,
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

impl Drop for CapacityInvalidationGuard {
    fn drop(&mut self) { invalidate_capacity_state(&self.capacity, &self.changed); }
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

impl CapacityMutationReservation {
    fn projected_usage(&self) -> HlsCacheCapacityUsage {
        let capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        HlsCacheCapacityUsage {
            session_bytes: capacity.session_bytes.get(&self.session_component).copied().unwrap_or_default(),
            global_bytes: capacity.total_bytes,
        }
    }

    fn reserve_replacement(
        &mut self,
        old_size: u64,
        new_size: u64,
        max_cache_bytes: u64,
        max_session_bytes: u64,
    ) -> Result<(), CapacityReservationError> {
        let mut capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !capacity.initialized || capacity.cache_path != self.cache_path {
            return Err(CapacityReservationError::Retry);
        }
        if !capacity.active_mutations.contains(&self.path) {
            return Err(CapacityReservationError::Invalidated);
        }
        let session_size = capacity.session_bytes.get(&self.session_component).copied().unwrap_or_default();
        let projected_total = capacity.total_bytes.saturating_sub(old_size).saturating_add(new_size);
        let projected_session = session_size.saturating_sub(old_size).saturating_add(new_size);
        if projected_total > max_cache_bytes || projected_session > max_session_bytes {
            return Err(CapacityReservationError::Exceeded {
                pressure: CacheCapacityPressure {
                    configured_session_bytes: max_session_bytes,
                    configured_global_bytes: max_cache_bytes,
                    current_session_bytes: session_size,
                    current_global_bytes: capacity.total_bytes,
                    staged_bytes: new_size,
                    required_session_bytes: projected_session.saturating_sub(max_session_bytes),
                    required_global_bytes: projected_total.saturating_sub(max_cache_bytes),
                },
                // Capture the opaque token in the same critical section as the
                // failed admission decision. A later accounting mutation must
                // make this token stale instead of being missed by the waiter.
                revision: HlsCacheCapacityRevision(Arc::clone(&capacity.revision)),
            });
        }
        capacity.total_bytes = projected_total;
        store_session_bytes(&mut capacity, &self.session_component, projected_session);
        self.replacement = Some((old_size, new_size));
        Ok(())
    }

    fn mark_filesystem_mutation_started(&mut self) { self.filesystem_mutation_started = true; }

    fn finish_replacement(mut self) {
        self.replacement = None;
        self.finish_mutation(None);
    }

    fn finish_delete(mut self, deleted_size: u64) {
        self.finish_mutation(Some(deleted_size));
        self.replacement = None;
    }

    fn finish_mutation(&mut self, deleted_size: Option<u64>) {
        let mut capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !capacity.active_mutations.contains(&self.path) {
            return;
        }
        capacity.active_mutations.remove(&self.path);
        if capacity.initialized && capacity.cache_path == self.cache_path {
            if let Some(deleted_size) = deleted_size {
                capacity.total_bytes = capacity.total_bytes.saturating_sub(deleted_size);
                let session_bytes = capacity.session_bytes.get(&self.session_component).copied().unwrap_or_default();
                store_session_bytes(&mut capacity, &self.session_component, session_bytes.saturating_sub(deleted_size));
            }
        }
        capacity.revision = Arc::new(CapacityRevision);
        drop(capacity);
        self.changed.notify_waiters();
    }
}

impl Drop for CapacityMutationReservation {
    fn drop(&mut self) {
        let mut capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !capacity.active_mutations.contains(&self.path) {
            return;
        }
        capacity.active_mutations.remove(&self.path);
        let revision_changed = if self.filesystem_mutation_started {
            capacity.initialized = false;
            true
        } else if capacity.initialized && capacity.cache_path == self.cache_path {
            if let Some((old_size, new_size)) = self.replacement.take() {
                capacity.total_bytes = capacity.total_bytes.saturating_sub(new_size).saturating_add(old_size);
                let session_size = capacity.session_bytes.get(&self.session_component).copied().unwrap_or_default();
                store_session_bytes(
                    &mut capacity,
                    &self.session_component,
                    session_size.saturating_sub(new_size).saturating_add(old_size),
                );
                true
            } else {
                false
            }
        } else {
            false
        };
        if revision_changed {
            capacity.revision = Arc::new(CapacityRevision);
        }
        drop(capacity);
        // Mutation waiters share this notifier with revision waiters. A no-op
        // release must wake the former, while the latter observe the unchanged
        // opaque token and continue waiting.
        self.changed.notify_waiters();
    }
}

#[derive(Debug, Clone)]
enum CapacityReservationError {
    Retry,
    Invalidated,
    Exceeded { pressure: CacheCapacityPressure, revision: HlsCacheCapacityRevision },
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

fn store_session_bytes(capacity: &mut CacheCapacityState, session_component: &str, bytes: u64) {
    if bytes == 0 {
        capacity.session_bytes.remove(session_component);
    } else {
        capacity.session_bytes.insert(session_component.to_string(), bytes);
    }
}

impl HlsSegmentCache {
    pub fn new() -> Self { Self::with_cache_path(DEFAULT_HLS_CACHE_PATH) }

    pub fn with_cache_path(cache_path: impl Into<PathBuf>) -> Self {
        Self {
            cache_path: StdRwLock::new(CachePathState {
                path: cache_path.into(),
                generation: Arc::new(CachePathGeneration),
            }),
            cache_path_commit_gate: Arc::new(RwLock::new(())),
            temp_files: Arc::new(StdMutex::new(TempFileState::default())),
            max_object_bytes: AtomicU64::new(u64::MAX),
            max_cache_bytes: AtomicU64::new(u64::MAX),
            max_session_bytes: AtomicU64::new(u64::MAX),
            marker_path: StdRwLock::new(None),
            capacity: Arc::new(StdMutex::new(CacheCapacityState::default())),
            capacity_changed: Arc::new(Notify::new()),
            capacity_reclaimer: StdRwLock::new(None),
            capacity_admission_gate: AsyncMutex::new(()),
            owned_operation_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_OWNED_CACHE_OPERATIONS)),
        }
    }

    pub fn install_capacity_reclaimer<T>(&self, reclaimer: &Arc<T>)
    where
        T: HlsCacheCapacityReclaimer + 'static,
    {
        let reclaimer: Arc<dyn HlsCacheCapacityReclaimer> = reclaimer.clone();
        *self.capacity_reclaimer.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::downgrade(&reclaimer));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn capacity_revision(&self) -> HlsCacheCapacityRevision {
        let capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        HlsCacheCapacityRevision(Arc::clone(&capacity.revision))
    }

    /// Waits until accounting or playback protection has changed since a
    /// capacity deferral. Registering with `Notify` before comparing tokens
    /// prevents a missed wake between the failed commit and this wait.
    pub async fn wait_for_capacity_change(&self, revision: &HlsCacheCapacityRevision) {
        loop {
            let notified = self.capacity_changed.notified();
            let changed = {
                let capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                !Arc::ptr_eq(&capacity.revision, &revision.0)
            };
            if changed {
                return;
            }
            notified.await;
        }
    }

    /// Announces a cursor/window change which may release protected cache
    /// objects. No accounting totals are modified by this operation.
    pub fn notify_capacity_protection_changed(&self) {
        let mut capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        capacity.revision = Arc::new(CapacityRevision);
        drop(capacity);
        self.capacity_changed.notify_waiters();
    }

    pub fn cache_path(&self) -> PathBuf { self.cache_path_snapshot().path }

    pub async fn update_cache_path(&self, cache_path: impl Into<PathBuf>) -> bool {
        let cache_path = cache_path.into();
        // Commits hold a read lease from their final generation check through the atomic rename. Taking the write
        // lease makes a cache-root transition linearizable without holding a filesystem/accounting mutex over I/O.
        let _transition = self.cache_path_commit_gate.write().await;
        let mut current = self.cache_path.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.path == cache_path {
            return false;
        }
        current.path = cache_path;
        current.generation = Arc::new(CachePathGeneration);
        drop(current);
        *self.marker_path.write().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.invalidate_capacity_accounting();
        true
    }

    pub fn update_cache_limits(&self, max_cache_bytes: u64, max_session_bytes: u64) {
        let max_cache_bytes = max_cache_bytes.max(1);
        let max_session_bytes = max_session_bytes.max(1);
        self.max_cache_bytes.store(max_cache_bytes, Ordering::Release);
        self.max_session_bytes.store(max_session_bytes, Ordering::Release);
        self.max_object_bytes.store(max_cache_bytes.min(max_session_bytes), Ordering::Release);
    }

    pub async fn metadata<K: HlsCacheObjectKey>(&self, key: &K) -> io::Result<Option<CachedSegmentMetadata>> {
        let path = self.path_for_key(key);
        match fs::metadata(&path).await {
            Ok(metadata) => Ok(Some(CachedSegmentMetadata { path, size: metadata.len() })),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Performs capacity admission from reliable response metadata before the
    /// decoded body is consumed. The authoritative staged commit repeats the
    /// same reservation after download, so concurrent mutations cannot create
    /// an overshoot.
    pub async fn ensure_projected_write_capacity<K>(&self, key: &K, content_length: u64) -> io::Result<()>
    where
        K: HlsCacheObjectKey,
    {
        let max_object_bytes = self.max_object_bytes.load(Ordering::Acquire);
        if content_length > max_object_bytes {
            let configured_session_bytes = self.max_session_bytes.load(Ordering::Acquire);
            let configured_global_bytes = self.max_cache_bytes.load(Ordering::Acquire);
            let usage = self.capacity_usage(key.proxy_session_id()).await.unwrap_or_default();
            log::warn!(
                "HLS cache capacity decision: proxy_session={} resource={} configured_session_bytes={} configured_global_bytes={} current_session_bytes={} current_global_bytes={} staged_bytes={} protected_working_set_bytes=0 reclaimable_bytes=0 required_session_bytes={} required_global_bytes={} outcome=permanently-infeasible",
                safe_proxy_session_id(key.proxy_session_id()),
                key.file_name(),
                configured_session_bytes,
                configured_global_bytes,
                usage.session_bytes,
                usage.global_bytes,
                content_length,
                content_length.saturating_sub(configured_session_bytes),
                content_length.saturating_sub(configured_global_bytes),
            );
            return Err(cache_object_limit_error(max_object_bytes));
        }

        let _admission_gate = self.capacity_admission_gate.lock().await;
        let mut reclamation_attempted = false;
        let mut reclamation_outcome = HlsCacheCapacityReclaimOutcome::default();
        let mut reclamation_pressure: Option<CacheCapacityPressure> = None;
        loop {
            let cache_path = self.cache_path_snapshot();
            let final_path = Self::path_for_key_in(&cache_path.path, key);
            match self
                .prepare_capacity_replacement(
                    &cache_path.path,
                    &final_path,
                    key.session_path_component(),
                    content_length,
                    self.max_cache_bytes.load(Ordering::Acquire),
                    self.max_session_bytes.load(Ordering::Acquire),
                )
                .await
            {
                Ok(reservation) => {
                    drop(reservation);
                    if let Some(pressure) = reclamation_pressure {
                        let usage = self.capacity_usage(key.proxy_session_id()).await.unwrap_or_default();
                        log::info!(
                            "HLS cache capacity decision: proxy_session={} resource={} configured_session_bytes={} configured_global_bytes={} current_session_bytes={} current_global_bytes={} staged_bytes={} protected_working_set_bytes={} reclaimable_bytes={} required_session_bytes={} required_global_bytes={} outcome=reclaimed",
                            safe_proxy_session_id(key.proxy_session_id()),
                            key.file_name(),
                            pressure.configured_session_bytes,
                            pressure.configured_global_bytes,
                            usage.session_bytes,
                            usage.global_bytes,
                            content_length,
                            reclamation_outcome.protected_working_set_bytes,
                            reclamation_outcome.reclaimable_bytes,
                            pressure.required_session_bytes,
                            pressure.required_global_bytes,
                        );
                    }
                    return Ok(());
                }
                Err(error) => {
                    let Some(capacity) = hls_cache_capacity_from_io(&error) else {
                        return Err(error);
                    };
                    if reclamation_attempted {
                        log::warn!(
                            "HLS cache capacity decision: proxy_session={} resource={} configured_session_bytes={} configured_global_bytes={} current_session_bytes={} current_global_bytes={} staged_bytes={} protected_working_set_bytes={} reclaimable_bytes={} required_session_bytes={} required_global_bytes={} outcome=deferred-protected wake_reason=capacity-or-protection-revision",
                            safe_proxy_session_id(key.proxy_session_id()),
                            key.file_name(),
                            capacity.configured_session_bytes(),
                            capacity.configured_global_bytes(),
                            capacity.current_session_bytes(),
                            capacity.current_global_bytes(),
                            capacity.staged_bytes(),
                            reclamation_outcome.protected_working_set_bytes,
                            reclamation_outcome.reclaimable_bytes,
                            capacity.required_session_bytes(),
                            capacity.required_global_bytes(),
                        );
                        return Err(capacity_error(
                            capacity.pressure(),
                            reclamation_outcome,
                            capacity.revision().clone(),
                        ));
                    }
                    reclamation_attempted = true;
                    reclamation_pressure = Some(capacity.pressure());
                    reclamation_outcome = self
                        .reclaim_capacity(HlsCacheCapacityReclaimRequest {
                            proxy_session_id: key.proxy_session_id().clone(),
                            target_path: final_path,
                            required_session_bytes: capacity.required_session_bytes(),
                            required_global_bytes: capacity.required_global_bytes(),
                            staged_bytes: content_length,
                        })
                        .await?;
                }
            }
        }
    }

    pub async fn open_range<K: HlsCacheObjectKey>(&self, key: &K, start: u64) -> io::Result<File> {
        let mut file = File::open(self.path_for_key(key)).await?;
        file.seek(SeekFrom::Start(start)).await?;
        Ok(file)
    }

    pub async fn write_temp_and_commit<K, R>(&self, key: &K, mut reader: R) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.write_temp_and_commit_inner(key, &mut reader, None).await
    }

    pub async fn write_temp_and_commit_with_deadline<K, R>(
        &self,
        key: &K,
        mut reader: R,
        deadline: Instant,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.write_temp_and_commit_inner(key, &mut reader, Some(deadline)).await
    }

    pub async fn stage_temp_with_deadline<K, R>(
        &self,
        key: &K,
        mut reader: R,
        deadline: Instant,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.stage_temp_inner(key, &mut reader, Some(deadline)).await
    }

    pub async fn commit_staged<K>(&self, key: &K, staged: StagedCacheObject) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        self.commit_staged_with_guard(key, staged, ()).await.map(|(metadata, ())| metadata)
    }

    /// Commits a staged object while transferring a cancellation cleanup guard through the owned rename task.
    pub async fn commit_staged_with_guard<K, G>(
        &self,
        key: &K,
        staged: StagedCacheObject,
        guard: G,
    ) -> io::Result<(CachedSegmentMetadata, G)>
    where
        K: HlsCacheObjectKey,
        G: Send + 'static,
    {
        let staged_path = staged.path.clone();
        let result = self.commit_staged_inner(key, staged, guard).await;
        if result.is_err() {
            remove_temp_file_after_error(&staged_path, "commit").await;
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    async fn commit_staged_inner<K, G>(
        &self,
        key: &K,
        staged: StagedCacheObject,
        guard: G,
    ) -> io::Result<(CachedSegmentMetadata, G)>
    where
        K: HlsCacheObjectKey,
        G: Send + 'static,
    {
        // Every authoritative admission participates in this gate. Otherwise a new writer that happens to fit after
        // reclamation can consume the reclaimed bytes before the reclaiming writer performs its retry.
        let admission_gate = self.capacity_admission_gate.lock().await;
        let mut reclamation_attempted = false;
        let mut reclamation_outcome = HlsCacheCapacityReclaimOutcome::default();
        let mut reclamation_pressure: Option<CacheCapacityPressure> = None;
        loop {
            let commit_gate = Arc::clone(&self.cache_path_commit_gate).read_owned().await;
            let cache_path = self.cache_path_snapshot();
            if !Arc::ptr_eq(&staged.cache_path_generation, &cache_path.generation)
                || !staged.path.starts_with(&cache_path.path)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "hls cache path changed before staged object commit",
                ));
            }
            self.ensure_cache_root_marker_for(&cache_path.path).await?;
            let max_object_bytes = self.max_object_bytes.load(Ordering::Acquire);
            if staged.size > max_object_bytes {
                return Err(cache_object_limit_error(max_object_bytes));
            }
            let final_path = Self::path_for_key_in(&cache_path.path, key);
            let Some(parent) = final_path.parent() else {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
            };
            fs::create_dir_all(parent).await?;
            let reservation = self
                .prepare_capacity_replacement(
                    &cache_path.path,
                    &final_path,
                    key.session_path_component(),
                    staged.size,
                    self.max_cache_bytes.load(Ordering::Acquire),
                    self.max_session_bytes.load(Ordering::Acquire),
                )
                .await;
            let reservation = match reservation {
                Ok(reservation) => {
                    if let Some(pressure) = reclamation_pressure {
                        let usage = reservation.projected_usage();
                        log::info!(
                            "HLS cache capacity decision: proxy_session={} resource={} configured_session_bytes={} configured_global_bytes={} current_session_bytes={} current_global_bytes={} staged_bytes={} protected_working_set_bytes={} reclaimable_bytes={} required_session_bytes={} required_global_bytes={} outcome=reclaimed",
                            safe_proxy_session_id(key.proxy_session_id()),
                            key.file_name(),
                            pressure.configured_session_bytes,
                            pressure.configured_global_bytes,
                            usage.session_bytes,
                            usage.global_bytes,
                            staged.size,
                            reclamation_outcome.protected_working_set_bytes,
                            reclamation_outcome.reclaimable_bytes,
                            pressure.required_session_bytes,
                            pressure.required_global_bytes,
                        );
                    }
                    reservation
                }
                Err(error) => {
                    let Some(capacity) = hls_cache_capacity_from_io(&error) else {
                        return Err(error);
                    };
                    if reclamation_attempted {
                        log::warn!(
                            "HLS cache capacity decision: proxy_session={} resource={} configured_session_bytes={} configured_global_bytes={} current_session_bytes={} current_global_bytes={} staged_bytes={} protected_working_set_bytes={} reclaimable_bytes={} required_session_bytes={} required_global_bytes={} outcome=deferred-protected wake_reason=capacity-or-protection-revision",
                            safe_proxy_session_id(key.proxy_session_id()),
                            key.file_name(),
                            capacity.configured_session_bytes(),
                            capacity.configured_global_bytes(),
                            capacity.current_session_bytes(),
                            capacity.current_global_bytes(),
                            capacity.staged_bytes(),
                            reclamation_outcome.protected_working_set_bytes,
                            reclamation_outcome.reclaimable_bytes,
                            capacity.required_session_bytes(),
                            capacity.required_global_bytes(),
                        );
                        return Err(capacity_error(
                            capacity.pressure(),
                            reclamation_outcome,
                            capacity.revision().clone(),
                        ));
                    }
                    let request = HlsCacheCapacityReclaimRequest {
                        proxy_session_id: key.proxy_session_id().clone(),
                        target_path: final_path,
                        required_session_bytes: capacity.required_session_bytes(),
                        required_global_bytes: capacity.required_global_bytes(),
                        staged_bytes: staged.size,
                    };
                    log::warn!(
                        "HLS cache capacity pressure detected: proxy_session={} staged_bytes={} required_session_bytes={} required_global_bytes={}",
                        safe_proxy_session_id(key.proxy_session_id()),
                        staged.size,
                        capacity.required_session_bytes(),
                        capacity.required_global_bytes(),
                    );
                    // The admission gate remains held through reclamation and the next authoritative reservation. The
                    // path read lease cannot: cache-root handoff uses run-gate -> path-write ordering.
                    drop(commit_gate);
                    reclamation_attempted = true;
                    reclamation_pressure = Some(capacity.pressure());
                    match self.reclaim_capacity(request).await {
                        Ok(outcome) => {
                            reclamation_outcome = outcome;
                            log::info!(
                                "HLS cache capacity reclamation completed: proxy_session={} reclaimed_session_bytes={} reclaimed_global_bytes={} protected_working_set_bytes={} reclaimable_bytes={}",
                                safe_proxy_session_id(key.proxy_session_id()),
                                outcome.reclaimed_session_bytes,
                                outcome.reclaimed_global_bytes,
                                outcome.protected_working_set_bytes,
                                outcome.reclaimable_bytes,
                            );
                        }
                        Err(reclaim_error) => {
                            log::warn!(
                                "HLS cache capacity reclamation failed: proxy_session={} error_kind={:?}",
                                safe_proxy_session_id(key.proxy_session_id()),
                                reclaim_error.kind(),
                            );
                            return Err(reclaim_error);
                        }
                    }
                    continue;
                }
            };
            drop(admission_gate);
            let staged_path = staged.path.clone();
            let committed_size = staged.size;
            // Once spawned, the owned mutation deliberately outlives cancellation of the requesting future. This
            // keeps the path lease, temp registration, and accounting reservation alive until the atomic filesystem
            // operation has a definite result. A runtime shutdown may abort the task, but no cache work can follow
            // that shutdown; panic/abort drops the reservation and forces accounting rollback/invalidation.
            return run_owned_cache_operation(Arc::clone(&self.owned_operation_permits), "commit", async move {
                let mut reservation = reservation;
                reservation.mark_filesystem_mutation_started();
                if let Err(err) = fs::rename(&staged_path, &final_path).await {
                    remove_temp_file_after_error(&staged_path, "rename").await;
                    return Err(err);
                }
                reservation.finish_replacement();
                drop(staged);
                drop(commit_gate);
                Ok((CachedSegmentMetadata { path: final_path, size: committed_size }, guard))
            })
            .await;
        }
    }

    async fn reclaim_capacity(
        &self,
        request: HlsCacheCapacityReclaimRequest,
    ) -> io::Result<HlsCacheCapacityReclaimOutcome> {
        let reclaimer = self
            .capacity_reclaimer
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade);
        match reclaimer {
            Some(reclaimer) => reclaimer.reclaim_capacity(request).await,
            // Standalone cache users have no session graph to reclaim. This is
            // a zero-reclamation capacity result, not a storage I/O failure.
            None => Ok(HlsCacheCapacityReclaimOutcome::default()),
        }
    }

    pub async fn remove_staged(&self, staged: StagedCacheObject) -> io::Result<()> {
        let result = match fs::remove_file(&staged.path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        drop(staged);
        result
    }

    async fn write_temp_and_commit_inner<K, R>(
        &self,
        key: &K,
        reader: &mut R,
        deadline: Option<Instant>,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        let staged = self.stage_temp_inner(key, reader, deadline).await?;
        self.commit_staged(key, staged).await
    }

    async fn stage_temp_inner<K, R>(
        &self,
        key: &K,
        reader: &mut R,
        deadline: Option<Instant>,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
        }
        let cache_path = self.cache_path_snapshot();
        self.ensure_cache_root_marker_for(&cache_path.path).await?;
        let final_path = Self::path_for_key_in(&cache_path.path, key);
        let Some(parent) = final_path.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
        };
        fs::create_dir_all(parent).await?;

        let (temp_path, mut temp_file, registration) = self.create_temp_file(key, &cache_path).await?;
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            drop(temp_file);
            remove_temp_file_after_error(&temp_path, "expired-write").await;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
        }
        let copy = async {
            let max_object_bytes = self.max_object_bytes.load(Ordering::Acquire);
            let mut limited = reader.take(max_object_bytes.saturating_add(1));
            let size = tokio::io::copy(&mut limited, &mut temp_file).await?;
            if size > max_object_bytes {
                return Err(cache_object_limit_error(max_object_bytes));
            }
            temp_file.flush().await?;
            drop(temp_file);
            Ok::<u64, io::Error>(size)
        };
        let copy_result = if let Some(deadline) = deadline {
            if let Ok(result) = timeout_at(deadline, copy).await {
                result
            } else {
                remove_temp_file_after_error(&temp_path, "timed-out-write").await;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
            }
        } else {
            copy.await
        };
        match copy_result {
            Ok(size) => {
                Ok(StagedCacheObject::registered(temp_path, size, Arc::clone(&cache_path.generation), registration))
            }
            Err(err) => {
                remove_temp_file_after_error(&temp_path, "failed-write").await;
                Err(err)
            }
        }
    }

    pub async fn write_bytes_and_commit<K>(&self, key: &K, bytes: &[u8]) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
    {
        self.write_temp_and_commit(key, bytes).await
    }

    pub async fn delete<K: HlsCacheObjectKey>(&self, key: &K) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        let path = Self::path_for_key_in(&cache_path.path, key);
        let reservation = self.begin_capacity_mutation(&cache_path.path, &path, key.session_path_component()).await;
        self.delete_with_reservation(path, reservation).await
    }

    pub async fn delete_if_inactive<K: HlsCacheObjectKey>(&self, key: &K) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        let path = Self::path_for_key_in(&cache_path.path, key);
        let reservation = self
            .try_begin_capacity_mutation(&cache_path.path, &path, key.session_path_component())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "hls cache object has an active mutation"))?;
        self.delete_with_reservation(path, reservation).await
    }

    async fn delete_with_reservation(&self, path: PathBuf, reservation: CapacityMutationReservation) -> io::Result<()> {
        run_owned_cache_operation(Arc::clone(&self.owned_operation_permits), "delete", async move {
            let size = match fs::metadata(&path).await {
                Ok(metadata) => metadata.len(),
                Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
                Err(err) => return Err(err),
            };
            let mut reservation = reservation;
            reservation.mark_filesystem_mutation_started();
            match fs::remove_file(&path).await {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            reservation.finish_delete(size);
            Ok(())
        })
        .await
    }

    pub fn object_path<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf { self.path_for_key(key) }

    pub fn contains_current_cache_path(&self, path: &Path) -> bool { path.starts_with(self.cache_path_snapshot().path) }

    pub fn has_active_mutation(&self, path: &Path) -> bool {
        self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner).active_mutations.contains(path)
    }

    pub async fn capacity_usage(&self, proxy_session_id: &ProxySessionId) -> io::Result<HlsCacheCapacityUsage> {
        let cache_path = self.cache_path_snapshot();
        self.ensure_capacity_initialized(&cache_path.path).await?;
        let capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !capacity.initialized || capacity.cache_path != cache_path.path {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "hls cache usage snapshot invalidated"));
        }
        Ok(HlsCacheCapacityUsage {
            session_bytes: capacity
                .session_bytes
                .get(&safe_session_path_component(proxy_session_id))
                .copied()
                .unwrap_or_default(),
            global_bytes: capacity.total_bytes,
        })
    }

    pub async fn delete_temp_files_older_than(&self, cutoff: SystemTime) -> io::Result<usize> {
        let cache_path = self.cache_path_snapshot();
        let candidates = old_temp_files(&cache_path.path, cutoff, MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN).await?;
        let mut deleted = 0_usize;
        for path in candidates {
            let Some(deletion) = self.reserve_path_deletion(&path) else {
                continue;
            };
            let result = run_owned_cache_operation(
                Arc::clone(&self.owned_operation_permits),
                "temporary-file-delete",
                async move {
                    let result = fs::remove_file(path).await;
                    drop(deletion);
                    result
                },
            )
            .await;
            match result {
                Ok(()) => deleted = deleted.saturating_add(1),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => warn!("HLS temporary cache file cleanup deferred: error_kind={:?}", error.kind()),
            }
        }
        Ok(deleted)
    }

    pub async fn delete_session_dir(&self, proxy_session_id: &ProxySessionId) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        let path = cache_path.path.join(safe_session_path_component(proxy_session_id));
        let deletion = self.reserve_path_deletion(&path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "hls cache session directory still has active temp files")
        })?;
        let capacity_invalidation = self.capacity_invalidation_guard();
        run_owned_cache_operation(Arc::clone(&self.owned_operation_permits), "session-delete", async move {
            let _capacity_invalidation = capacity_invalidation;
            let result = match fs::remove_dir_all(path).await {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            };
            drop(deletion);
            result
        })
        .await
    }

    pub async fn delete_orphan_session_dirs(
        &self,
        active_session_ids: &HashSet<ProxySessionId>,
        freshness_cutoff: SystemTime,
    ) -> io::Result<usize> {
        let cache_path = self.cache_path_snapshot();
        ensure_safe_cache_root(&cache_path.path).await?;
        let active_paths = active_session_ids
            .iter()
            .map(|id| cache_path.path.join(safe_session_path_component(id)))
            .collect::<HashSet<_>>();
        let mut entries = fs::read_dir(&cache_path.path).await?;
        let mut removed = 0_usize;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warn!("HLS orphan session entry inspection deferred: error_kind={:?}", error.kind());
                    continue;
                }
            };
            if !file_type.is_dir() || active_paths.contains(&path) {
                continue;
            }
            // Freshness guard: skip directories committed after the GC took its
            // in-memory session snapshot. Their owning session may not yet be
            // visible in `active_session_ids`, so deleting them would race with
            // a concurrent segment write that just created the directory.
            let metadata = match entry.metadata().await {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warn!("HLS orphan session metadata inspection deferred: error_kind={:?}", error.kind());
                    continue;
                }
            };
            if let Ok(modified) = metadata.modified() {
                if modified > freshness_cutoff {
                    continue;
                }
            }
            let Some(deletion) = self.reserve_path_deletion(&path) else {
                continue;
            };
            let capacity_invalidation = self.capacity_invalidation_guard();
            let result = run_owned_cache_operation(
                Arc::clone(&self.owned_operation_permits),
                "orphan-session-delete",
                async move {
                    let _capacity_invalidation = capacity_invalidation;
                    let result = fs::remove_dir_all(&path).await;
                    drop(deletion);
                    result
                },
            )
            .await;
            match result {
                Ok(()) => removed = removed.saturating_add(1),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => {
                    warn!("HLS orphan session directory cleanup deferred: error_kind={:?}", err.kind());
                }
            }
        }
        Ok(removed)
    }

    pub fn has_active_temp_files_for_session(&self, proxy_session_id: &ProxySessionId) -> bool {
        let session_path = self.cache_path_snapshot().path.join(safe_session_path_component(proxy_session_id));
        self.temp_files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_files
            .iter()
            .any(|path| path.starts_with(&session_path))
    }

    pub fn has_active_temp_files(&self) -> bool {
        !self.temp_files.lock().unwrap_or_else(std::sync::PoisonError::into_inner).active_files.is_empty()
    }

    pub async fn invalidate_all_if_no_active_temp_files(&self) -> io::Result<CacheInvalidationOutcome> {
        let cache_path = self.cache_path_snapshot();
        let Some(deletion) = self.reserve_path_deletion(&cache_path.path) else {
            return Ok(CacheInvalidationOutcome::DeferredActiveTempFiles);
        };
        self.invalidate_all_unchecked(cache_path.path, deletion).await?;
        Ok(CacheInvalidationOutcome::Invalidated)
    }

    pub async fn invalidate_all(&self) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        let deletion = self.reserve_path_deletion(&cache_path.path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "hls cache invalidation conflicts with an active object write")
        })?;
        self.invalidate_all_unchecked(cache_path.path, deletion).await
    }

    async fn invalidate_all_unchecked(
        &self,
        cache_path: PathBuf,
        deletion: CachePathDeletionReservation,
    ) -> io::Result<()> {
        let capacity_invalidation = self.capacity_invalidation_guard();
        run_owned_cache_operation(Arc::clone(&self.owned_operation_permits), "invalidate", async move {
            let _capacity_invalidation = capacity_invalidation;
            let result = async {
                ensure_safe_cache_root(&cache_path).await?;
                fs::create_dir_all(&cache_path).await?;
                let mut entries = fs::read_dir(&cache_path).await?;
                while let Some(entry) = entries.next_entry().await? {
                    if entry.file_name() == REWRITE_SECRET_FINGERPRINT_FILE
                        || entry.file_name() == HLS_CACHE_ROOT_MARKER_FILE
                    {
                        continue;
                    }
                    let file_type = entry.file_type().await?;
                    if file_type.is_dir() {
                        fs::remove_dir_all(entry.path()).await?;
                    } else {
                        fs::remove_file(entry.path()).await?;
                    }
                }
                Ok(())
            }
            .await;
            drop(deletion);
            result
        })
        .await
    }

    pub async fn read_rewrite_secret_fingerprint(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(self.rewrite_secret_fingerprint_path()).await {
            Ok(value) => Ok(Some(value.trim().to_string())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn write_rewrite_secret_fingerprint(&self, fingerprint: &str) -> io::Result<()> {
        self.ensure_cache_root_marker().await?;
        fs::write(self.rewrite_secret_fingerprint_path(), fingerprint).await
    }

    fn path_for_key<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf {
        let cache_path = self.cache_path_snapshot();
        Self::path_for_key_in(&cache_path.path, key)
    }

    fn path_for_key_in<K: HlsCacheObjectKey>(cache_path: &Path, key: &K) -> PathBuf {
        cache_path.join(key.session_path_component()).join(key.file_name())
    }

    async fn create_temp_file<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        cache_path: &CachePathState,
    ) -> io::Result<(PathBuf, File, Arc<ActiveTempFileRegistration>)> {
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let temp_path = Self::temp_path_for_key(key, &cache_path.path);
            let registration = self.register_active_temp_path(&temp_path)?;
            match OpenOptions::new().write(true).create_new(true).open(&temp_path).await {
                Ok(file) => {
                    return Ok((temp_path, file, registration));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    drop(registration);
                }
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, "could not create unique hls cache temp file"))
    }

    pub fn adopt_staged_file(&self, path: PathBuf, size: u64) -> io::Result<StagedCacheObject> {
        let cache_path = self.cache_path_snapshot();
        if !path.starts_with(&cache_path.path) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "staged hls cache file is outside the cache root"));
        }
        let registration = self.register_active_temp_path(&path)?;
        Ok(StagedCacheObject::registered(path, size, Arc::clone(&cache_path.generation), registration))
    }

    fn temp_path_for_key<K: HlsCacheObjectKey>(key: &K, cache_path: &Path) -> PathBuf {
        let suffix = fastrand::u64(..);
        cache_path.join(key.session_path_component()).join(format!("{}.tmp.{suffix:016x}", key.file_name()))
    }

    fn register_active_temp_path(&self, path: &Path) -> io::Result<Arc<ActiveTempFileRegistration>> {
        let mut state = self.temp_files.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.deletion_reservations.iter().any(|reserved| path.starts_with(reserved)) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "hls cache path is reserved for invalidation"));
        }
        if !state.active_files.insert(path.to_path_buf()) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "hls cache temp path is already active"));
        }
        Ok(Arc::new(ActiveTempFileRegistration { path: path.to_path_buf(), state: Arc::clone(&self.temp_files) }))
    }

    fn reserve_path_deletion(&self, path: &Path) -> Option<CachePathDeletionReservation> {
        let mut state = self.temp_files.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let conflicts_with_active = state.active_files.iter().any(|active| active.starts_with(path));
        let conflicts_with_delete =
            state.deletion_reservations.iter().any(|reserved| reserved.starts_with(path) || path.starts_with(reserved));
        if conflicts_with_active || conflicts_with_delete {
            return None;
        }
        let path = path.to_path_buf();
        state.deletion_reservations.insert(path.clone());
        Some(CachePathDeletionReservation { path, state: Arc::clone(&self.temp_files) })
    }

    async fn ensure_capacity_initialized(&self, cache_path: &Path) -> io::Result<()> {
        loop {
            let notified = self.capacity_changed.notified();
            let revision = {
                let capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if capacity.initialized && capacity.cache_path == cache_path {
                    return Ok(());
                }
                if capacity.active_mutations.iter().any(|path| path.starts_with(cache_path)) {
                    None
                } else {
                    Some(Arc::clone(&capacity.revision))
                }
            };
            let Some(revision) = revision else {
                notified.await;
                continue;
            };
            let (total_bytes, session_bytes) = scan_committed_cache_usage(cache_path).await?;
            if self.cache_path_snapshot().path != cache_path {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "hls cache path changed during usage scan"));
            }
            let scan_invalidated = {
                let mut capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if !Arc::ptr_eq(&capacity.revision, &revision)
                    || capacity.active_mutations.iter().any(|path| path.starts_with(cache_path))
                {
                    true
                } else {
                    capacity.cache_path = cache_path.to_path_buf();
                    capacity.initialized = true;
                    capacity.total_bytes = total_bytes;
                    capacity.session_bytes = session_bytes;
                    false
                }
            };
            if scan_invalidated {
                // The scan result was invalidated by a real accounting or
                // mutation transition. Wait on the notification registered
                // before the scan instead of immediately walking the complete
                // cache tree again under sustained commit churn.
                notified.await;
                continue;
            }
            return Ok(());
        }
    }

    async fn begin_capacity_mutation(
        &self,
        cache_path: &Path,
        path: &Path,
        session_component: String,
    ) -> CapacityMutationReservation {
        loop {
            let notified = self.capacity_changed.notified();
            let Some(reservation) = self.try_begin_capacity_mutation(cache_path, path, session_component.clone())
            else {
                notified.await;
                continue;
            };
            return reservation;
        }
    }

    fn try_begin_capacity_mutation(
        &self,
        cache_path: &Path,
        path: &Path,
        session_component: String,
    ) -> Option<CapacityMutationReservation> {
        let mut capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !capacity.active_mutations.insert(path.to_path_buf()) {
            return None;
        }
        drop(capacity);
        Some(CapacityMutationReservation {
            path: path.to_path_buf(),
            cache_path: cache_path.to_path_buf(),
            session_component,
            replacement: None,
            filesystem_mutation_started: false,
            capacity: Arc::clone(&self.capacity),
            changed: Arc::clone(&self.capacity_changed),
        })
    }

    async fn prepare_capacity_replacement(
        &self,
        cache_path: &Path,
        final_path: &Path,
        session_component: String,
        new_size: u64,
        max_cache_bytes: u64,
        max_session_bytes: u64,
    ) -> io::Result<CapacityMutationReservation> {
        loop {
            self.ensure_capacity_initialized(cache_path).await?;
            let mut reservation = self.begin_capacity_mutation(cache_path, final_path, session_component.clone()).await;
            let old_size = match fs::metadata(final_path).await {
                Ok(metadata) => metadata.len(),
                Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
                Err(err) => return Err(err),
            };
            if self.cache_path_snapshot().path != cache_path {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "hls cache path changed during object write"));
            }
            match reservation.reserve_replacement(old_size, new_size, max_cache_bytes, max_session_bytes) {
                Ok(()) => return Ok(reservation),
                Err(CapacityReservationError::Retry | CapacityReservationError::Invalidated) => {
                    drop(reservation);
                }
                Err(CapacityReservationError::Exceeded { pressure, revision }) => {
                    return Err(capacity_error(pressure, HlsCacheCapacityReclaimOutcome::default(), revision));
                }
            }
        }
    }

    fn invalidate_capacity_accounting(&self) { invalidate_capacity_state(&self.capacity, &self.capacity_changed); }

    fn capacity_invalidation_guard(&self) -> CapacityInvalidationGuard {
        CapacityInvalidationGuard { capacity: Arc::clone(&self.capacity), changed: Arc::clone(&self.capacity_changed) }
    }

    fn rewrite_secret_fingerprint_path(&self) -> PathBuf {
        self.cache_path_snapshot().path.join(REWRITE_SECRET_FINGERPRINT_FILE)
    }

    async fn ensure_cache_root_marker(&self) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        self.ensure_cache_root_marker_for(&cache_path.path).await
    }

    async fn ensure_cache_root_marker_for(&self, cache_path: &Path) -> io::Result<()> {
        let marker_matches = {
            let marker_path = self.marker_path.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            marker_path.as_deref() == Some(cache_path)
        };
        if marker_matches {
            return Ok(());
        }
        ensure_not_root_like_cache_path(cache_path)?;
        fs::create_dir_all(cache_path).await?;
        fs::write(cache_path.join(HLS_CACHE_ROOT_MARKER_FILE), b"tuliprox-hls-cache\n").await?;
        *self.marker_path.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cache_path.to_path_buf());
        Ok(())
    }

    fn cache_path_snapshot(&self) -> CachePathState {
        self.cache_path.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

fn capacity_error(
    pressure: CacheCapacityPressure,
    reclaim: HlsCacheCapacityReclaimOutcome,
    revision: HlsCacheCapacityRevision,
) -> io::Error {
    io::Error::other(HlsCacheCapacityError {
        configured_session_bytes: pressure.configured_session_bytes,
        configured_global_bytes: pressure.configured_global_bytes,
        current_session_bytes: pressure.current_session_bytes,
        current_global_bytes: pressure.current_global_bytes,
        staged_bytes: pressure.staged_bytes,
        required_session_bytes: pressure.required_session_bytes,
        required_global_bytes: pressure.required_global_bytes,
        protected_working_set_bytes: reclaim.protected_working_set_bytes,
        reclaimable_bytes: reclaim.reclaimable_bytes,
        revision,
    })
}

fn invalidate_capacity_state(capacity: &StdMutex<CacheCapacityState>, changed: &Notify) {
    let mut capacity = capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    capacity.initialized = false;
    capacity.revision = Arc::new(CapacityRevision);
    drop(capacity);
    changed.notify_waiters();
}

async fn remove_temp_file_after_error(path: &Path, operation: &'static str) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            warn!("HLS cache temporary file cleanup failed: operation={operation} error_kind={:?}", error.kind());
        }
    }
}

/// Runs one filesystem mutation with all of its reservations owned by the spawned task.
///
/// A per-cache permit is acquired without waiting before the spawn. Saturation returns `WouldBlock` and drops the
/// supplied future synchronously, which releases any reservations it owns without creating a detached task.
///
/// Dropping the caller future detaches, rather than cancels, the mutation. A returned `JoinError` therefore denotes a
/// task panic/abort; unwinding drops the owned guards. Runtime shutdown is the only case where completion is not
/// observed, and no later cache accounting or root transition can run after that shutdown.
async fn run_owned_cache_operation<T, F>(permits: Arc<Semaphore>, operation: &'static str, future: F) -> io::Result<T>
where
    T: Send + 'static,
    F: Future<Output = io::Result<T>> + Send + 'static,
{
    let permit = permits
        .try_acquire_owned()
        .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "hls cache owned operation capacity exhausted"))?;
    tokio::spawn(async move {
        let _permit = permit;
        future.await
    })
    .await
    .map_err(|error| io::Error::other(format!("hls cache {operation} task failed: {error}")))?
}

const REWRITE_SECRET_FINGERPRINT_FILE: &str = ".rewrite_secret_fingerprint";
const HLS_CACHE_ROOT_MARKER_FILE: &str = ".tuliprox-hls-cache-root";

fn safe_session_path_component(proxy_session_id: &ProxySessionId) -> String {
    let value = &proxy_session_id.0;
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
        return value.clone();
    }
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn ensure_not_root_like_cache_path(cache_path: &Path) -> io::Result<()> {
    if cache_path.as_os_str().is_empty() || cache_path.parent().is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to invalidate unsafe hls cache root"));
    }
    Ok(())
}

async fn ensure_safe_cache_root(cache_path: &Path) -> io::Result<()> {
    ensure_not_root_like_cache_path(cache_path)?;
    match fs::metadata(cache_path.join(HLS_CACHE_ROOT_MARKER_FILE)).await {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to invalidate hls cache root without marker file",
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to invalidate hls cache root without marker file",
        )),
        Err(err) => Err(err),
    }
}

async fn scan_committed_cache_usage(cache_path: &Path) -> io::Result<(u64, HashMap<String, u64>)> {
    let mut total_bytes = 0_u64;
    let mut session_bytes = HashMap::new();
    let mut root_entries = match fs::read_dir(cache_path).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, session_bytes)),
        Err(err) => return Err(err),
    };
    while let Some(root_entry) = root_entries.next_entry().await? {
        if !root_entry.file_type().await?.is_dir() {
            continue;
        }
        let session_component = root_entry.file_name().to_string_lossy().into_owned();
        let mut bytes = 0_u64;
        let mut pending_dirs = vec![root_entry.path()];
        while let Some(dir) = pending_dirs.pop() {
            let mut entries = fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    pending_dirs.push(entry.path());
                } else if file_type.is_file() && !is_temp_cache_file(&entry.path()) {
                    bytes = bytes.saturating_add(entry.metadata().await?.len());
                }
            }
        }
        total_bytes = total_bytes.saturating_add(bytes);
        if bytes > 0 {
            session_bytes.insert(session_component, bytes);
        }
    }
    Ok((total_bytes, session_bytes))
}

async fn old_temp_files(root: &Path, cutoff: SystemTime, limit: usize) -> io::Result<Vec<PathBuf>> {
    let mut candidates = Vec::with_capacity(limit);
    if limit == 0 {
        return Ok(candidates);
    }
    let mut root_entries = match fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(candidates),
        Err(err) => return Err(err),
    };
    while candidates.len() < limit {
        let Some(entry) = root_entries.next_entry().await? else {
            break;
        };
        let path = entry.path();
        let file_type = match entry.file_type().await {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!("HLS temporary cache entry inspection deferred: error_kind={:?}", error.kind());
                continue;
            }
        };
        if file_type.is_dir() {
            append_old_temp_files_in_session(&path, cutoff, limit, &mut candidates).await?;
        } else {
            append_old_temp_file_candidate(&entry, cutoff, limit, &mut candidates).await;
        }
    }
    Ok(candidates)
}

async fn append_old_temp_files_in_session(
    session_root: &Path,
    cutoff: SystemTime,
    limit: usize,
    candidates: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let mut pending_dirs = vec![session_root.to_path_buf()];
    while let Some(dir) = pending_dirs.pop() {
        if candidates.len() >= limit {
            break;
        }
        let mut entries = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        while candidates.len() < limit {
            let Some(entry) = entries.next_entry().await? else {
                break;
            };
            let path = entry.path();
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warn!("HLS temporary cache entry inspection deferred: error_kind={:?}", error.kind());
                    continue;
                }
            };
            if file_type.is_dir() {
                pending_dirs.push(path);
                continue;
            }
            append_old_temp_file_candidate(&entry, cutoff, limit, candidates).await;
        }
    }
    Ok(())
}

async fn append_old_temp_file_candidate(
    entry: &fs::DirEntry,
    cutoff: SystemTime,
    limit: usize,
    candidates: &mut Vec<PathBuf>,
) {
    let path = entry.path();
    if candidates.len() >= limit || !is_temp_cache_file(&path) {
        return;
    }
    let metadata = match entry.metadata().await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            warn!("HLS temporary cache metadata inspection deferred: error_kind={:?}", error.kind());
            return;
        }
    };
    if metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH) < cutoff {
        candidates.push(path);
    }
}

fn is_temp_cache_file(path: &Path) -> bool {
    path.file_name().and_then(|file_name| file_name.to_str()).is_some_and(|file_name| file_name.contains(".tmp."))
}

impl Default for HlsSegmentCache {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::{
        hls_cache_capacity_from_io, hls_cache_object_limit_from_io, run_owned_cache_operation,
        CacheInvalidationOutcome, HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest,
        HlsCacheCapacityReclaimer, HlsCacheObjectKey, HlsSegmentCache, MapCacheKey, SegmentCacheKey,
        TransientObjectCacheKey, MAX_CONCURRENT_OWNED_CACHE_OPERATIONS, MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN,
    };
    use crate::{transient::build_transient_resource_id, HlsOriginResourceFetchError, ProxySessionId};
    use futures::{future::BoxFuture, FutureExt};
    use std::{
        collections::{HashSet, VecDeque},
        future::Future,
        io,
        pin::Pin,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        task::{Context, Poll},
        time::{Duration, SystemTime},
    };
    use tokio::{
        io::{AsyncRead, AsyncReadExt, ReadBuf},
        sync::{oneshot, Semaphore},
    };

    fn cache_key() -> SegmentCacheKey { SegmentCacheKey::new(ProxySessionId("proxy_session".to_string()), 123, "ts") }

    async fn cache_with_projected_session_pressure(
    ) -> (tempfile::TempDir, Arc<HlsSegmentCache>, SegmentCacheKey, SegmentCacheKey) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let proxy_session_id = ProxySessionId("capacity-session".to_string());
        let resident = SegmentCacheKey::new(proxy_session_id.clone(), 0, "ts");
        let first = SegmentCacheKey::new(proxy_session_id.clone(), 1, "ts");
        let second = SegmentCacheKey::new(proxy_session_id, 2, "ts");
        cache.update_cache_limits(5, 5);
        cache.write_bytes_and_commit(&resident, b"123").await.expect("resident object commits");
        (temp_dir, cache, first, second)
    }

    struct PausingCapacityReclaimer {
        cache: Arc<HlsSegmentCache>,
        victims: Mutex<VecDeque<SegmentCacheKey>>,
        first_reclaimed: Mutex<Option<oneshot::Sender<()>>>,
        resume_first: Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl HlsCacheCapacityReclaimer for PausingCapacityReclaimer {
        fn reclaim_capacity(
            &self,
            _request: HlsCacheCapacityReclaimRequest,
        ) -> BoxFuture<'_, io::Result<HlsCacheCapacityReclaimOutcome>> {
            async move {
                let victim = self.victims.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop_front();
                let Some(victim) = victim else {
                    return Ok(HlsCacheCapacityReclaimOutcome::default());
                };
                self.cache.delete_if_inactive(&victim).await?;
                let first_reclaimed =
                    self.first_reclaimed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
                if let Some(first_reclaimed) = first_reclaimed {
                    let resume_first =
                        self.resume_first.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
                    let _ = first_reclaimed.send(());
                    if let Some(resume_first) = resume_first {
                        let _ = resume_first.await;
                    }
                }
                Ok(HlsCacheCapacityReclaimOutcome {
                    reclaimed_session_bytes: 10,
                    reclaimed_global_bytes: 10,
                    protected_working_set_bytes: 0,
                    reclaimable_bytes: 10,
                })
            }
            .boxed()
        }
    }

    struct ControlledReader {
        started: Option<oneshot::Sender<()>>,
        release: oneshot::Receiver<Vec<u8>>,
        body: Option<std::io::Cursor<Vec<u8>>>,
    }

    impl Unpin for ControlledReader {}

    impl AsyncRead for ControlledReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if let Some(started) = self.started.take() {
                let _send_result = started.send(());
            }
            if self.body.is_none() {
                match Pin::new(&mut self.release).poll(context) {
                    Poll::Ready(Ok(body)) => self.body = Some(std::io::Cursor::new(body)),
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "controlled cache reader release dropped",
                        )))
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
            let body = self.body.as_mut().expect("controlled reader body is initialized");
            let position = usize::try_from(body.position()).unwrap_or(body.get_ref().len());
            let remaining = &body.get_ref()[position.min(body.get_ref().len())..];
            let copied = remaining.len().min(buffer.remaining());
            buffer.put_slice(&remaining[..copied]);
            let next_position = body.position().saturating_add(u64::try_from(copied).unwrap_or_default());
            body.set_position(next_position);
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn segment_cache_key_contains_no_origin_data() {
        let key = cache_key();

        assert_eq!(key.stable_value(), "hls:proxy_session:00000000000000000123");
    }

    #[test]
    fn cache_key_debug_redacts_proxy_session_id() {
        let key = SegmentCacheKey::new(ProxySessionId("secretToken".to_string()), 123, "ts");
        let debug = format!("{key:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secretToken"));
        assert!(!debug.contains(&key.stable_value()));
    }

    #[test]
    fn transient_object_cache_key_keeps_redirect_hosts_distinct_without_leaking_urls() {
        let proxy_session_id = ProxySessionId("proxy_session".to_string());
        let first_resource =
            build_transient_resource_id("https://cdn-a.example.net/live/redirected/seg001.ts", b"secret");
        let second_resource =
            build_transient_resource_id("https://cdn-b.example.net/live/redirected/seg001.ts", b"secret");

        let first = TransientObjectCacheKey::new(proxy_session_id.clone(), first_resource, "ts");
        let second = TransientObjectCacheKey::new(proxy_session_id, second_resource, "ts");

        assert_ne!(first, second);
        assert_ne!(first.stable_value(), second.stable_value());
        for value in [first.stable_value(), second.stable_value()] {
            assert!(!value.contains("provider://"));
            assert!(!value.contains("cdn-a.example.net"));
            assert!(!value.contains("cdn-b.example.net"));
            assert!(!value.contains("/live/redirected/seg001.ts"));
        }
    }

    #[tokio::test]
    async fn write_temp_and_commit_creates_final_segment_cache_file_with_proxy_layout() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();

        let metadata = cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

        assert_eq!(metadata.size, 12);
        assert!(metadata.path.exists());
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), Some(metadata.clone()));
        assert!(metadata.path.ends_with("proxy_session/000123.ts"));
    }

    #[tokio::test]
    async fn write_temp_and_commit_creates_final_map_cache_file_with_proxy_layout() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = MapCacheKey::new(ProxySessionId("proxy_session".to_string()), 0, "mp4");

        let metadata = cache.write_bytes_and_commit(&key, b"map-body").await.expect("commit should succeed");

        assert_eq!(metadata.size, 8);
        assert!(metadata.path.ends_with("proxy_session/map/000000.mp4"));
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), Some(metadata));
    }

    #[tokio::test]
    async fn write_temp_and_commit_with_deadline_cleans_active_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        let (_writer, reader) = tokio::io::duplex(64);

        let result = cache
            .write_temp_and_commit_with_deadline(&key, reader, tokio::time::Instant::now() + Duration::from_millis(1))
            .await;

        assert_eq!(result.expect_err("commit should time out").kind(), io::ErrorKind::TimedOut);
        assert!(!cache.has_active_temp_files());
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
    }

    #[tokio::test]
    async fn exhausted_write_deadline_rejects_even_an_immediately_available_body() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();

        let result = cache.write_temp_and_commit_with_deadline(&key, &b"ready"[..], tokio::time::Instant::now()).await;

        assert_eq!(result.expect_err("expired deadline must time out").kind(), io::ErrorKind::TimedOut);
        assert!(!cache.has_active_temp_files());
        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
        assert_eq!(std::fs::read_dir(temp_dir.path()).expect("cache root reads").count(), 0);
    }

    #[tokio::test]
    async fn cache_object_write_rejects_bytes_above_the_configured_limit() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.update_cache_limits(3, 3);
        let key = SegmentCacheKey::new(ProxySessionId("session".to_string()), 1, "ts");

        let result = cache.write_bytes_and_commit(&key, b"four").await;

        let error = result.expect_err("decoded object above the configured limit must fail");
        let limit_error = hls_cache_object_limit_from_io(&error).expect("typed object-limit source");
        assert_eq!(limit_error.limit(), 3);
        assert!(matches!(
            HlsOriginResourceFetchError::cache_body(&error),
            HlsOriginResourceFetchError::CacheObjectLimit { limit: 3 }
        ));
        assert!(cache.metadata(&key).await.expect("metadata").is_none());
        assert!(!cache.has_active_temp_files());
    }

    #[tokio::test]
    async fn orphan_session_cleanup_preserves_active_session_directories() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.write_rewrite_secret_fingerprint("secret").await.expect("marker");
        let active = ProxySessionId("active".to_string());
        let orphan = ProxySessionId("orphan".to_string());
        tokio::fs::create_dir_all(temp_dir.path().join("active")).await.expect("active dir");
        tokio::fs::create_dir_all(temp_dir.path().join("orphan")).await.expect("orphan dir");
        let cutoff = SystemTime::now();

        let removed = cache.delete_orphan_session_dirs(&HashSet::from([active]), cutoff).await.expect("cleanup");

        assert_eq!(removed, 1);
        assert!(temp_dir.path().join("active").exists());
        assert!(!temp_dir.path().join(orphan.0).exists());
    }

    #[tokio::test]
    async fn orphan_session_cleanup_skips_directories_newer_than_freshness_cutoff() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.write_rewrite_secret_fingerprint("secret").await.expect("marker");
        let stale = ProxySessionId("stale".to_string());
        let fresh = ProxySessionId("fresh".to_string());
        tokio::fs::create_dir_all(temp_dir.path().join(stale.0.clone())).await.expect("stale dir");
        tokio::fs::create_dir_all(temp_dir.path().join(fresh.0.clone())).await.expect("fresh dir");

        // Set the fresh dir's mtime to the future relative to the cutoff.
        let cutoff = SystemTime::now();
        let future = cutoff + std::time::Duration::from_mins(1);
        filetime::set_file_mtime(temp_dir.path().join(fresh.0.clone()), filetime::FileTime::from_system_time(future))
            .expect("set fresh mtime");

        let removed = cache.delete_orphan_session_dirs(&HashSet::new(), cutoff).await.expect("cleanup");

        assert_eq!(removed, 1, "stale orphan dir should be removed");
        assert!(!temp_dir.path().join(stale.0).exists());
        assert!(
            temp_dir.path().join(fresh.0).exists(),
            "directory newer than the freshness cutoff must be preserved to avoid racing concurrent session creation"
        );
    }

    #[tokio::test]
    async fn cache_commits_enforce_the_global_budget_across_sessions() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        cache.update_cache_limits(5, 5);
        let first = SegmentCacheKey::new(ProxySessionId("first".to_string()), 1, "ts");
        let second = SegmentCacheKey::new(ProxySessionId("second".to_string()), 1, "ts");

        assert!(cache.write_bytes_and_commit(&first, b"123").await.is_ok());
        let error = cache.write_bytes_and_commit(&second, b"456").await.expect_err("global limit rejects commit");
        let capacity = hls_cache_capacity_from_io(&error).expect("typed local capacity error");
        assert_eq!(capacity.required_session_bytes(), 0);
        assert_eq!(capacity.required_global_bytes(), 1);
        assert!(cache.metadata(&second).await.expect("metadata").is_none());
        assert!(!cache.has_active_temp_files());
        let first_usage = cache.capacity_usage(first.proxy_session_id()).await.expect("usage");
        assert_eq!(first_usage.global_bytes, 3);
        assert_eq!(first_usage.session_bytes, 3);
    }

    #[tokio::test]
    async fn failed_projected_capacity_admission_returns_a_live_revision_token() {
        let (_temp_dir, cache, first, _) = cache_with_projected_session_pressure().await;
        let error = cache
            .ensure_projected_write_capacity(&first, 3)
            .await
            .expect_err("projected object exceeds the remaining session budget");
        let revision = hls_cache_capacity_from_io(&error).expect("typed capacity error").revision().clone();
        let wait = cache.wait_for_capacity_change(&revision);
        tokio::pin!(wait);

        assert!(matches!(futures::poll!(wait.as_mut()), Poll::Pending));

        cache.notify_capacity_protection_changed();
        assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
    }

    #[tokio::test]
    async fn failed_admission_revision_is_captured_with_the_capacity_decision() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let path = temp_dir.path().join("session/000001.ts");
        {
            let mut capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            capacity.cache_path = temp_dir.path().to_path_buf();
            capacity.initialized = true;
            capacity.total_bytes = 2;
            capacity.session_bytes.insert("session".to_string(), 2);
        }
        let mut reservation = cache
            .try_begin_capacity_mutation(temp_dir.path(), &path, "session".to_string())
            .expect("mutation reservation");
        let error = reservation.reserve_replacement(0, 3, 2, 2).expect_err("capacity is exceeded");

        cache.notify_capacity_protection_changed();
        drop(reservation);

        let super::CapacityReservationError::Exceeded { revision, .. } = error else {
            panic!("expected typed capacity pressure");
        };
        let wait = cache.wait_for_capacity_change(&revision);
        tokio::pin!(wait);
        assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
    }

    #[tokio::test]
    async fn concurrent_noop_capacity_failures_do_not_wake_each_other() {
        let (_temp_dir, cache, first, second) = cache_with_projected_session_pressure().await;
        let (first_error, second_error) = tokio::join!(
            cache.ensure_projected_write_capacity(&first, 3),
            cache.ensure_projected_write_capacity(&second, 3),
        );
        let first_error = first_error.expect_err("first projected object exceeds the remaining budget");
        let second_error = second_error.expect_err("second projected object exceeds the remaining budget");
        let first_revision =
            hls_cache_capacity_from_io(&first_error).expect("first typed capacity error").revision().clone();
        let second_revision =
            hls_cache_capacity_from_io(&second_error).expect("second typed capacity error").revision().clone();
        let first_wait = cache.wait_for_capacity_change(&first_revision);
        let second_wait = cache.wait_for_capacity_change(&second_revision);
        tokio::pin!(first_wait, second_wait);

        assert!(matches!(futures::poll!(first_wait.as_mut()), Poll::Pending));
        assert!(matches!(futures::poll!(second_wait.as_mut()), Poll::Pending));

        cache.notify_capacity_protection_changed();
        assert!(matches!(futures::poll!(first_wait.as_mut()), Poll::Ready(())));
        assert!(matches!(futures::poll!(second_wait.as_mut()), Poll::Ready(())));
    }

    #[tokio::test]
    async fn abandoned_capacity_reservation_rolls_back_before_filesystem_mutation() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let path = temp_dir.path().join("session/000001.ts");
        {
            let mut capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            capacity.cache_path = temp_dir.path().to_path_buf();
            capacity.initialized = true;
            capacity.active_mutations.insert(path.clone());
        }
        let mut reservation = super::CapacityMutationReservation {
            path: path.clone(),
            cache_path: temp_dir.path().to_path_buf(),
            session_component: "session".to_string(),
            replacement: None,
            filesystem_mutation_started: false,
            capacity: Arc::clone(&cache.capacity),
            changed: Arc::clone(&cache.capacity_changed),
        };
        reservation.reserve_replacement(0, 3, 10, 10).expect("capacity reserves");
        let revision = cache.capacity_revision();

        drop(reservation);

        {
            let capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(capacity.initialized);
            assert_eq!(capacity.total_bytes, 0);
            assert!(!capacity.session_bytes.contains_key("session"));
            assert!(!capacity.active_mutations.contains(&path));
        }
        let wait = cache.wait_for_capacity_change(&revision);
        tokio::pin!(wait);
        assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
    }

    #[tokio::test]
    async fn abandoned_started_filesystem_mutation_invalidates_capacity_snapshot() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let path = temp_dir.path().join("session/000001.ts");
        {
            let mut capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            capacity.cache_path = temp_dir.path().to_path_buf();
            capacity.initialized = true;
            capacity.active_mutations.insert(path.clone());
        }
        let mut reservation = super::CapacityMutationReservation {
            path: path.clone(),
            cache_path: temp_dir.path().to_path_buf(),
            session_component: "session".to_string(),
            replacement: None,
            filesystem_mutation_started: false,
            capacity: Arc::clone(&cache.capacity),
            changed: Arc::clone(&cache.capacity_changed),
        };
        reservation.reserve_replacement(0, 3, 10, 10).expect("capacity reserves");
        reservation.mark_filesystem_mutation_started();
        let revision = cache.capacity_revision();

        drop(reservation);

        {
            let capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert!(!capacity.initialized);
            assert!(!capacity.active_mutations.contains(&path));
        }
        let wait = cache.wait_for_capacity_change(&revision);
        tokio::pin!(wait);
        assert!(matches!(futures::poll!(wait.as_mut()), Poll::Ready(())));
    }

    #[tokio::test]
    async fn concurrent_commits_reserve_the_global_budget_exactly_once() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        cache.update_cache_limits(5, 5);
        let first = SegmentCacheKey::new(ProxySessionId("first".to_string()), 1, "ts");
        let second = SegmentCacheKey::new(ProxySessionId("second".to_string()), 1, "ts");
        let first_cache = Arc::clone(&cache);
        let first_task = tokio::spawn(async move { first_cache.write_bytes_and_commit(&first, b"123").await });
        let second_cache = Arc::clone(&cache);
        let second_task = tokio::spawn(async move { second_cache.write_bytes_and_commit(&second, b"456").await });

        let (first_result, second_result) = tokio::join!(first_task, second_task);
        let results = [first_result.expect("first task joins"), second_result.expect("second task joins")];

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    }

    #[tokio::test]
    async fn concurrent_commit_cannot_consume_bytes_reclaimed_for_an_in_flight_writer() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let proxy_session_id = ProxySessionId("serialized-reclamation".to_string());
        let first_resident = SegmentCacheKey::new(proxy_session_id.clone(), 1, "ts");
        let second_resident = SegmentCacheKey::new(proxy_session_id.clone(), 2, "ts");
        let first_target = SegmentCacheKey::new(proxy_session_id.clone(), 3, "ts");
        let second_target = SegmentCacheKey::new(proxy_session_id.clone(), 4, "ts");
        cache.update_cache_limits(100, 100);
        cache.write_bytes_and_commit(&first_resident, b"0123456789").await.expect("first resident commits");
        cache.write_bytes_and_commit(&second_resident, b"0123456789").await.expect("second resident commits");
        cache.update_cache_limits(25, 25);

        let first_staged = cache
            .stage_temp_with_deadline(
                &first_target,
                &b"abcdefghij"[..],
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .expect("first target stages");
        let second_staged = cache
            .stage_temp_with_deadline(
                &second_target,
                &b"klmnopqrst"[..],
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .expect("second target stages");
        let (first_reclaimed_tx, first_reclaimed_rx) = oneshot::channel();
        let (resume_first_tx, resume_first_rx) = oneshot::channel();
        let reclaimer = Arc::new(PausingCapacityReclaimer {
            cache: Arc::clone(&cache),
            victims: Mutex::new(VecDeque::from([first_resident, second_resident])),
            first_reclaimed: Mutex::new(Some(first_reclaimed_tx)),
            resume_first: Mutex::new(Some(resume_first_rx)),
        });
        cache.install_capacity_reclaimer(&reclaimer);

        let first_cache = Arc::clone(&cache);
        let first_task = tokio::spawn(async move { first_cache.commit_staged(&first_target, first_staged).await });
        first_reclaimed_rx.await.expect("first reclamation pauses after deleting one resident");

        let second_cache = Arc::clone(&cache);
        let second_task = tokio::spawn(async move { second_cache.commit_staged(&second_target, second_staged).await });
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        let paused_usage = cache.capacity_usage(&proxy_session_id).await.expect("paused capacity usage");
        assert_eq!(paused_usage.session_bytes, 10, "a later commit must wait for the reclaiming writer's retry");

        assert!(resume_first_tx.send(()).is_ok());
        let (first_result, second_result) = tokio::join!(first_task, second_task);
        assert!(first_result.expect("first task joins").is_ok());
        assert!(second_result.expect("second task joins").is_ok());
        let usage = cache.capacity_usage(&proxy_session_id).await.expect("final capacity usage");
        assert_eq!(usage.session_bytes, 20);
        assert!(usage.global_bytes <= 25);
    }

    #[tokio::test]
    async fn gc_style_delete_defers_while_the_object_has_an_active_mutation() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        cache.write_bytes_and_commit(&key, b"segment-body").await.expect("fixture commit");
        let cache_path = cache.cache_path_snapshot();
        let path = cache.object_path(&key);
        let reservation = cache
            .try_begin_capacity_mutation(&cache_path.path, &path, key.session_path_component())
            .expect("test mutation reserves");

        let error = cache.delete_if_inactive(&key).await.expect_err("active mutation protects object");

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(cache.metadata(&key).await.expect("metadata").is_some());
        drop(reservation);
        cache.delete_if_inactive(&key).await.expect("delete resumes after mutation");
        assert!(cache.metadata(&key).await.expect("metadata").is_none());
    }

    #[tokio::test]
    async fn cache_path_change_during_write_rejects_old_generation_commit() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let old_root = temp_dir.path().join("old");
        let new_root = temp_dir.path().join("new");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(&old_root));
        let key = cache_key();
        let old_final_path = old_root.join("proxy_session/000123.ts");
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let reader = ControlledReader { started: Some(started_tx), release: release_rx, body: None };
        let task_cache = Arc::clone(&cache);
        let task_key = key.clone();
        let write_task = tokio::spawn(async move { task_cache.write_temp_and_commit(&task_key, reader).await });
        started_rx.await.expect("write reaches controlled reader");

        assert!(cache.update_cache_path(&new_root).await);
        release_tx.send(b"segment-body".to_vec()).expect("release write");
        let error = write_task.await.expect("write task joins").expect_err("old generation commit must be rejected");

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!old_final_path.exists());
        assert!(cache.metadata(&key).await.expect("new-root metadata reads").is_none());
        assert!(!cache.has_active_temp_files());
    }

    #[tokio::test]
    async fn owned_cache_operation_survives_caller_cancellation() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let (completed_tx, completed_rx) = oneshot::channel();
        let permits = Arc::new(Semaphore::new(1));
        let caller = tokio::spawn(async move {
            run_owned_cache_operation(permits, "test", async move {
                started_tx.send(()).map_err(|()| io::Error::other("start receiver dropped"))?;
                release_rx.await.map_err(|_| io::Error::other("release sender dropped"))?;
                completed_tx.send(()).map_err(|()| io::Error::other("completion receiver dropped"))?;
                Ok(())
            })
            .await
        });
        started_rx.await.expect("owned operation starts");
        caller.abort();
        release_tx.send(()).expect("release owned operation");

        completed_rx.await.expect("detached owned operation completes");
        assert!(caller.await.expect_err("caller is cancelled").is_cancelled());
    }

    #[tokio::test]
    async fn saturated_owned_operation_limit_prevents_spawn_and_mutation_until_permit_release() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        let permit_count =
            u32::try_from(MAX_CONCURRENT_OWNED_CACHE_OPERATIONS).expect("owned operation limit fits u32");
        let held_permits = Arc::clone(&cache.owned_operation_permits)
            .try_acquire_many_owned(permit_count)
            .expect("test exclusively holds owned operation permits");
        let operation_started = Arc::new(AtomicBool::new(false));
        let operation_started_in_task = Arc::clone(&operation_started);

        let spawn_error =
            run_owned_cache_operation(Arc::clone(&cache.owned_operation_permits), "saturated-test", async move {
                operation_started_in_task.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .expect_err("saturated helper must reject before spawning");

        let error = cache
            .write_bytes_and_commit(&key, b"segment-body")
            .await
            .expect_err("saturated cache operation must be rejected");

        assert_eq!(spawn_error.kind(), io::ErrorKind::WouldBlock);
        assert!(!operation_started.load(Ordering::SeqCst));
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(cache.metadata(&key).await.expect("metadata reads after rejection").is_none());
        assert!(!cache.has_active_temp_files());
        {
            let capacity = cache.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(capacity.total_bytes, 0);
            assert!(!capacity.session_bytes.contains_key("proxy_session"));
            assert!(capacity.active_mutations.is_empty(), "pre-spawn rejection must roll back the reservation");
        }

        drop(held_permits);
        cache.write_bytes_and_commit(&key, b"segment-body").await.expect("released permits allow a later mutation");
        assert!(cache.metadata(&key).await.expect("metadata reads after commit").is_some());
    }

    #[tokio::test]
    async fn open_range_reads_from_requested_offset() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        cache.write_bytes_and_commit(&key, b"0123456789").await.expect("commit should succeed");

        let file = cache.open_range(&key, 4).await.expect("range should open");
        let mut body = Vec::new();
        file.take(3).read_to_end(&mut body).await.expect("range should read");

        assert_eq!(body, b"456");
    }

    #[tokio::test]
    async fn temp_file_collision_does_not_overwrite_existing_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        let parent = temp_dir.path().join("proxy_session");
        tokio::fs::create_dir_all(&parent).await.expect("parent should be created");
        let existing_temp = parent.join("000123.ts.tmp.0000000000000000");
        tokio::fs::write(&existing_temp, b"existing").await.expect("temp fixture should write");

        cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

        assert_eq!(tokio::fs::read(&existing_temp).await.expect("existing temp should remain"), b"existing");
    }

    #[tokio::test]
    async fn old_temp_file_cleanup_processes_a_bounded_batch_per_run() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let total_files = MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN.saturating_add(3);
        for index in 0..total_files {
            let session = if index % 2 == 0 { "session-a" } else { "session-b" };
            let directory = temp_dir.path().join(session).join(if index % 3 == 0 { "map" } else { "r" });
            tokio::fs::create_dir_all(&directory).await.expect("fixture directory writes");
            tokio::fs::write(directory.join(format!("object-{index}.ts.tmp.fixture")), b"stale")
                .await
                .expect("temporary fixture writes");
        }
        let cutoff = SystemTime::now() + Duration::from_mins(1);

        let first = cache.delete_temp_files_older_than(cutoff).await.expect("first cleanup run succeeds");
        let second = cache.delete_temp_files_older_than(cutoff).await.expect("second cleanup run succeeds");
        let third = cache.delete_temp_files_older_than(cutoff).await.expect("third cleanup run succeeds");

        assert_eq!(first, MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN);
        assert_eq!(second, total_files.saturating_sub(MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN));
        assert_eq!(third, 0);
    }

    #[tokio::test]
    async fn invalidate_all_if_no_active_temp_files_deletes_only_when_no_temp_write_is_active() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let committed_key = cache_key();
        cache.write_bytes_and_commit(&committed_key, b"segment-body").await.expect("commit should succeed");

        let outcome = cache.invalidate_all_if_no_active_temp_files().await.expect("invalidation should succeed");

        assert_eq!(outcome, CacheInvalidationOutcome::Invalidated);
        assert_eq!(cache.metadata(&committed_key).await.expect("metadata should read"), None);

        cache.write_bytes_and_commit(&committed_key, b"segment-body").await.expect("second commit should succeed");
        let active_key = SegmentCacheKey::new(ProxySessionId("proxy_session".to_string()), 124, "ts");
        let staged = cache
            .stage_temp_with_deadline(&active_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
            .await
            .expect("active object stages");
        assert!(cache.has_active_temp_files());

        let outcome =
            cache.invalidate_all_if_no_active_temp_files().await.expect("deferred invalidation should succeed");

        assert_eq!(outcome, CacheInvalidationOutcome::DeferredActiveTempFiles);
        assert!(cache.metadata(&committed_key).await.expect("metadata should read").is_some());
        cache.commit_staged(&active_key, staged).await.expect("staged object commits");
    }

    #[tokio::test]
    async fn invalidate_all_refuses_unmarked_cache_root() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let unrelated_file = temp_dir.path().join("unrelated");
        tokio::fs::write(&unrelated_file, b"keep").await.expect("fixture should write");

        let err = cache.invalidate_all().await.expect_err("unmarked root should be refused");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(unrelated_file.exists());
    }

    #[tokio::test]
    async fn delete_removes_committed_file_idempotently() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache = HlsSegmentCache::with_cache_path(temp_dir.path());
        let key = cache_key();
        cache.write_bytes_and_commit(&key, b"segment-body").await.expect("commit should succeed");

        cache.delete(&key).await.expect("delete should succeed");
        cache.delete(&key).await.expect("second delete should be idempotent");

        assert_eq!(cache.metadata(&key).await.expect("metadata should read"), None);
    }
}
