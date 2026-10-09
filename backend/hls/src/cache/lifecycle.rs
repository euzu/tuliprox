use super::{
    revisions::is_revision_cache_path, staging::run_owned_cache_operation, ActiveTempFileRegistration,
    CacheCapacityState, CacheInvalidationOutcome, CachePathDeletionReservation, CachePathGeneration, CachePathState,
    CachedSegmentMetadata, CapacityMutationReservation, CapacityRevision, HlsCacheCapacityReclaimer,
    HlsCacheCapacityRevision, HlsCacheCapacityUsage, HlsCacheObjectKey, HlsSegmentCache, ProxySessionId, TempFileState,
    DEFAULT_HLS_CACHE_PATH, MAX_CONCURRENT_OWNED_CACHE_OPERATIONS, MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN,
};
use log::warn;
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex, RwLock as StdRwLock,
    },
    time::SystemTime,
};
use tokio::{
    fs,
    sync::{Mutex as AsyncMutex, Notify, RwLock, Semaphore},
};

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

    pub(super) async fn delete_with_reservation(
        &self,
        path: PathBuf,
        reservation: CapacityMutationReservation,
    ) -> io::Result<()> {
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
        let mut removed_revision = false;
        for path in candidates {
            let revision = is_revision_cache_path(&path);
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
                Ok(()) => {
                    deleted = deleted.saturating_add(1);
                    removed_revision |= revision;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => warn!("HLS temporary cache file cleanup deferred: error_kind={:?}", error.kind()),
            }
        }
        if removed_revision {
            drop(self.capacity_invalidation_guard());
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

    pub(super) async fn invalidate_all_unchecked(
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

    pub(super) fn path_for_key<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf {
        let cache_path = self.cache_path_snapshot();
        Self::path_for_key_in(&cache_path.path, key)
    }

    pub(super) fn path_for_key_in<K: HlsCacheObjectKey>(cache_path: &Path, key: &K) -> PathBuf {
        cache_path.join(key.session_path_component()).join(key.file_name())
    }

    pub(super) fn register_active_temp_path(&self, path: &Path) -> io::Result<Arc<ActiveTempFileRegistration>> {
        let mut state = self.temp_files.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.deletion_reservations.iter().any(|reserved| path.starts_with(reserved)) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "hls cache path is reserved for invalidation"));
        }
        if !state.active_files.insert(path.to_path_buf()) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "hls cache temp path is already active"));
        }
        Ok(Arc::new(ActiveTempFileRegistration { path: path.to_path_buf(), state: Arc::clone(&self.temp_files) }))
    }

    pub(super) fn reserve_path_deletion(&self, path: &Path) -> Option<CachePathDeletionReservation> {
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

    pub(super) async fn ensure_cache_root_marker(&self) -> io::Result<()> {
        let cache_path = self.cache_path_snapshot();
        self.ensure_cache_root_marker_for(&cache_path.path).await
    }

    pub(super) async fn ensure_cache_root_marker_for(&self, cache_path: &Path) -> io::Result<()> {
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

    pub(super) fn cache_path_snapshot(&self) -> CachePathState {
        self.cache_path.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

pub(super) const REWRITE_SECRET_FINGERPRINT_FILE: &str = ".rewrite_secret_fingerprint";

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

pub(super) fn is_temp_cache_file(path: &Path) -> bool {
    path.file_name().and_then(|file_name| file_name.to_str()).is_some_and(|file_name| file_name.contains(".tmp."))
}
