use super::{
    error::{cache_object_limit_error, capacity_error},
    hls_cache_capacity_from_io, safe_proxy_session_id, ActiveTempFileRegistration, CacheCapacityPressure,
    CachePathGeneration, CachePathState, HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest,
    HlsCacheObjectKey, HlsSegmentCache, StagedCacheObject, TEMP_CREATE_ATTEMPTS,
};
use log::warn;
use std::{
    fmt,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
};
use tokio::{
    fs,
    fs::{File, OpenOptions},
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::Semaphore,
    time::{timeout_at, Instant},
};

/// Filesystem metadata for a committed HLS cache object.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CachedSegmentMetadata {
    pub path: PathBuf,
    pub size: u64,
}

impl StagedCacheObject {
    pub(super) fn registered(
        path: PathBuf,
        size: u64,
        cache_path_generation: Arc<CachePathGeneration>,
        registration: Arc<ActiveTempFileRegistration>,
    ) -> Self {
        Self {
            cleanup: Arc::new(super::StagedFileCleanup {
                path: path.clone(),
                registration: Arc::clone(&registration),
                budget: None,
                completed: std::sync::atomic::AtomicBool::new(false),
                writer: std::sync::Mutex::new(None),
            }),
            path,
            size,
            cache_path_generation,
            _registration: registration,
            write_budget: None,
        }
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

impl HlsSegmentCache {
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
        let cleanup = Arc::clone(&staged.cleanup);
        let result = self.commit_staged_inner(key, staged, guard).await;
        if result.is_err() {
            cleanup.completed.store(remove_temp_file_after_error(&staged_path, "commit").await, Ordering::Release);
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn commit_staged_inner<K, G>(
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
                .prepare_capacity_replacement_with_budget(
                    &cache_path.path,
                    &final_path,
                    key.session_path_component(),
                    staged.size,
                    self.max_cache_bytes.load(Ordering::Acquire),
                    self.max_session_bytes.load(Ordering::Acquire),
                    staged.write_budget.clone(),
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
            #[cfg(test)]
            let checkpoint = self.take_startup_write_checkpoint(super::StartupWritePhase::BeforeCacheRename);
            // Once spawned, the owned mutation deliberately outlives cancellation of the requesting future. This
            // keeps the path lease, temp registration, and accounting reservation alive until the atomic filesystem
            // operation has a definite result. A runtime shutdown may abort the task, but no cache work can follow
            // that shutdown; panic/abort drops the reservation and forces accounting rollback/invalidation.
            return run_owned_cache_operation(Arc::clone(&self.owned_operation_permits), "commit", async move {
                let mut reservation = reservation;
                #[cfg(test)]
                if let Some(checkpoint) = checkpoint {
                    checkpoint.wait().await?;
                }
                reservation.mark_filesystem_mutation_started();
                if let Err(err) = fs::rename(&staged_path, &final_path).await {
                    remove_temp_file_after_error(&staged_path, "rename").await;
                    return Err(err);
                }
                reservation.finish_replacement();
                staged.cleanup.completed.store(true, Ordering::Release);
                drop(staged);
                drop(commit_gate);
                Ok((CachedSegmentMetadata { path: final_path, size: committed_size }, guard))
            })
            .await;
        }
    }

    pub async fn remove_staged(&self, staged: StagedCacheObject) -> io::Result<()> {
        staged.finish_writer().await;
        #[cfg(test)]
        self.check_cache_delete_test_failure(&staged.path)?;
        let result = match fs::remove_file(&staged.path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
        if result.is_ok() {
            staged.cleanup.completed.store(true, Ordering::Release);
        }
        drop(staged);
        result
    }

    pub(super) async fn write_temp_and_commit_inner<K, R>(
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

    pub(super) async fn stage_temp_inner<K, R>(
        &self,
        key: &K,
        reader: &mut R,
        deadline: Option<Instant>,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.stage_temp_inner_with_budget(key, reader, deadline, None).await
    }

    pub(super) async fn stage_temp_inner_with_budget<K, R>(
        &self,
        key: &K,
        reader: &mut R,
        deadline: Option<Instant>,
        budget: Option<Arc<super::HlsRevisionDiskReservation>>,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
        }
        let budget = match budget {
            Some(budget) => budget,
            None => Arc::new(self.reserve_revision_peak_reclaiming(key, 0).await?),
        };
        let cache_path = self.cache_path_snapshot();
        self.ensure_cache_root_marker_for(&cache_path.path).await?;
        let final_path = Self::path_for_key_in(&cache_path.path, key);
        let Some(parent) = final_path.parent() else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cache path has no parent"));
        };
        fs::create_dir_all(parent).await?;

        let (temp_path, temp_file, registration) = self.create_temp_file(key, &cache_path).await?;
        let cleanup = StagedCacheObject::registered(
            temp_path.clone(),
            0,
            Arc::clone(&cache_path.generation),
            Arc::clone(&registration),
        )
        .with_write_budget(Some(Arc::clone(&budget)))?;
        let writer = cleanup.track_writer(temp_file);
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            cleanup.finish_writer().await;
            cleanup
                .cleanup
                .completed
                .store(remove_temp_file_after_error(&temp_path, "expired-write").await, Ordering::Release);
            return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
        }
        let copy = async {
            let mut writer = writer.lock().await;
            let temp_file = writer.as_mut().ok_or_else(|| io::Error::other("staging writer already closed"))?;
            let max_object_bytes = self.max_object_bytes.load(Ordering::Acquire);
            let mut limited = reader.take(max_object_bytes.saturating_add(1));
            let mut size = 0u64;
            let mut buffer = [0u8; 16 * 1024];
            loop {
                let read = limited.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                let next_size = size.saturating_add(u64::try_from(read).map_err(io::Error::other)?);
                if next_size > max_object_bytes {
                    return Err(cache_object_limit_error(max_object_bytes));
                }
                // Admission precedes every filesystem write, including headerless
                // bodies and standalone cache users without a startup transaction.
                self.ensure_write_budget_available(key, &budget, next_size).await?;
                temp_file.write_all(&buffer[..read]).await?;
                size = next_size;
            }
            if size > max_object_bytes {
                return Err(cache_object_limit_error(max_object_bytes));
            }
            Ok::<u64, io::Error>(size)
        };
        let copy_result = if let Some(deadline) = deadline {
            if let Ok(result) = timeout_at(deadline, copy).await {
                result
            } else {
                cleanup.finish_writer().await;
                cleanup
                    .cleanup
                    .completed
                    .store(remove_temp_file_after_error(&temp_path, "timed-out-write").await, Ordering::Release);
                return Err(io::Error::new(io::ErrorKind::TimedOut, "hls cache object write timed out"));
            }
        } else {
            copy.await
        };
        // EOF completes the origin transfer. Flushing local bytes afterwards
        // must not consume the origin's remaining fetch budget.
        let copy_result = match copy_result {
            Ok(size) => {
                let mut writer = writer.lock().await;
                let result = match writer.as_mut() {
                    Some(file) => file.flush().await,
                    None => Err(io::Error::other("staging writer already closed")),
                };
                writer.take();
                result.map(|()| size)
            }
            Err(error) => Err(error),
        };
        match copy_result {
            Ok(size) => {
                let mut staged = cleanup;
                staged.size = size;
                Ok(staged)
            }
            Err(err) => {
                cleanup.finish_writer().await;
                cleanup
                    .cleanup
                    .completed
                    .store(remove_temp_file_after_error(&temp_path, "failed-write").await, Ordering::Release);
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

    pub(super) async fn create_temp_file<K: HlsCacheObjectKey>(
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

    pub(super) fn temp_path_for_key<K: HlsCacheObjectKey>(key: &K, cache_path: &Path) -> PathBuf {
        let suffix = fastrand::u64(..);
        cache_path.join(key.session_path_component()).join(format!("{}.tmp.{suffix:016x}", key.file_name()))
    }
}

async fn remove_temp_file_after_error(path: &Path, operation: &'static str) -> bool {
    match fs::remove_file(path).await {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            warn!("HLS cache temporary file cleanup failed: operation={operation} error_kind={:?}", error.kind());
            false
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
pub(super) async fn run_owned_cache_operation<T, F>(
    permits: Arc<Semaphore>,
    operation: &'static str,
    future: F,
) -> io::Result<T>
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
