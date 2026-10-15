//! Ownership of peak bytes across staging, rename, and immutable publication.
use super::{
    error::capacity_error, revisions::revision_reserved_bytes, ActiveTempFileRegistration, CacheCapacityPressure,
    CapacityRevision, HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest, HlsCacheCapacityRevision,
    HlsCacheObjectKey, HlsRevisionDiskReservation, HlsSegmentCache, StagedCacheObject,
};
use crate::sync_ext::MutexExt;
use std::{
    io,
    path::PathBuf,
    sync::{atomic::Ordering, Arc},
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncWriteExt},
    time::Instant,
};

type TrackedWriter = Arc<tokio::sync::Mutex<Option<fs::File>>>;

pub(super) struct StagedFileCleanup {
    pub(super) path: PathBuf,
    pub(super) registration: Arc<ActiveTempFileRegistration>,
    pub(super) budget: Option<Arc<HlsRevisionDiskReservation>>,
    pub(super) completed: std::sync::atomic::AtomicBool,
    pub(super) writer: std::sync::Mutex<Option<TrackedWriter>>,
}

impl Drop for StagedFileCleanup {
    fn drop(&mut self) {
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        let path = self.path.clone();
        let registration = Arc::clone(&self.registration);
        let budget = self.budget.clone();
        let writer = self.writer.lock_unpoisoned().clone();
        // Keep both the file registration and peak budget until removal completes.
        // A successful rename leaves no file at this unique temporary path.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                finish_writer(writer).await;
                if let Err(error) = fs::remove_file(path).await {
                    if error.kind() != io::ErrorKind::NotFound {
                        log::warn!("HLS staging cleanup deferred: error_kind={:?}", error.kind());
                        if let Some(budget) = &budget {
                            let mut capacity = budget.capacity.lock_unpoisoned();
                            capacity.initialized = false;
                            capacity.revision = Arc::new(CapacityRevision);
                        }
                    }
                }
                drop(registration);
                drop(budget);
            });
        }
    }
}

async fn finish_writer(writer: Option<TrackedWriter>) {
    if let Some(writer) = writer {
        let mut guard = writer.lock().await;
        if let Some(mut file) = guard.take() {
            // Tokio filesystem writes may outlive cancellation of write_all.
            // Flushing the same handle joins that pending write before unlink.
            let _ = file.flush().await;
        }
    }
}

impl StagedCacheObject {
    pub(crate) fn track_writer(&self, file: fs::File) -> TrackedWriter {
        let writer = Arc::new(tokio::sync::Mutex::new(Some(file)));
        *self.cleanup.writer.lock_unpoisoned() = Some(Arc::clone(&writer));
        writer
    }

    pub(super) async fn finish_writer(&self) {
        let writer = self.cleanup.writer.lock_unpoisoned().clone();
        finish_writer(writer).await;
    }

    pub(crate) fn with_write_budget(mut self, budget: Option<Arc<HlsRevisionDiskReservation>>) -> io::Result<Self> {
        Arc::get_mut(&mut self.cleanup)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cannot change a shared staged file's budget"))?
            .budget
            .clone_from(&budget);
        self.write_budget = budget;
        Ok(self)
    }
}

impl HlsSegmentCache {
    pub(crate) async fn stage_temp_with_budget<K, R>(
        &self,
        key: &K,
        mut reader: R,
        deadline: Instant,
        budget: Option<Arc<HlsRevisionDiskReservation>>,
    ) -> io::Result<StagedCacheObject>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        self.stage_temp_inner_with_budget(key, &mut reader, Some(deadline), budget).await
    }

    pub(crate) async fn ensure_write_budget_available<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        budget: &HlsRevisionDiskReservation,
        size: u64,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(&self.capacity, &budget.capacity)
            || !Arc::ptr_eq(&budget.generation, &self.cache_path_snapshot().generation)
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "foreign or retired write reservation"));
        }
        let remaining = self
            .capacity
            .lock_unpoisoned()
            .revision_reservations
            .get(&budget.id)
            .filter(|(owner, _)| owner == &key.session_path_component())
            .map(|(_, bytes)| *bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Interrupted, "write reservation no longer current"))?;
        if size > remaining {
            if let Err(error) = self.grow_revision_peak(key, budget, size - remaining).await {
                if let Some(capacity) = super::hls_cache_capacity_from_io(&error) {
                    let mut pressure = capacity.pressure();
                    pressure.staged_bytes = size;
                    return Err(capacity_error(
                        pressure,
                        HlsCacheCapacityReclaimOutcome {
                            protected_working_set_bytes: capacity.protected_working_set_bytes,
                            reclaimable_bytes: capacity.reclaimable_bytes,
                            ..Default::default()
                        },
                        capacity.revision().clone(),
                    ));
                }
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) async fn grow_revision_peak<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        budget: &HlsRevisionDiskReservation,
        additional: u64,
    ) -> io::Result<()> {
        let _admission = self.capacity_admission_gate.lock().await;
        let root = self.cache_path_snapshot();
        self.ensure_capacity_initialized(&root.path).await?;
        let error = match self.try_grow_revision_peak(key, budget, additional) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let Some(capacity) = super::hls_cache_capacity_from_io(&error) else { return Err(error) };
        let outcome = self
            .reclaim_capacity(HlsCacheCapacityReclaimRequest {
                proxy_session_id: key.proxy_session_id().clone(),
                target_path: self.object_path(key),
                required_session_bytes: capacity.required_session_bytes(),
                required_global_bytes: capacity.required_global_bytes(),
                staged_bytes: additional,
            })
            .await?;
        self.ensure_capacity_initialized(&root.path).await?;
        self.try_grow_revision_peak(key, budget, additional).map_err(|error| {
            match super::hls_cache_capacity_from_io(&error) {
                Some(capacity) => capacity_error(capacity.pressure(), outcome, capacity.revision().clone()),
                None => error,
            }
        })
    }

    fn try_grow_revision_peak<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        budget: &HlsRevisionDiskReservation,
        additional: u64,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(&self.capacity, &budget.capacity)
            || !Arc::ptr_eq(&budget.generation, &self.cache_path_snapshot().generation)
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "foreign or retired cache write reservation"));
        }
        let session = key.session_path_component();
        let mut capacity = self.capacity.lock_unpoisoned();
        let (global, local) = revision_reserved_bytes(&capacity, &session);
        let global = global.saturating_add(capacity.total_bytes);
        let local = local.saturating_add(capacity.session_bytes.get(&session).copied().unwrap_or_default());
        let global_limit = self.max_cache_bytes.load(Ordering::Acquire);
        let local_limit = self.max_session_bytes.load(Ordering::Acquire);
        if global.saturating_add(additional) > global_limit || local.saturating_add(additional) > local_limit {
            return Err(capacity_error(
                CacheCapacityPressure {
                    configured_session_bytes: local_limit,
                    configured_global_bytes: global_limit,
                    current_session_bytes: local,
                    current_global_bytes: global,
                    staged_bytes: additional,
                    required_session_bytes: local.saturating_add(additional).saturating_sub(local_limit),
                    required_global_bytes: global.saturating_add(additional).saturating_sub(global_limit),
                },
                HlsCacheCapacityReclaimOutcome::default(),
                HlsCacheCapacityRevision(Arc::clone(&capacity.revision)),
            ));
        }
        let (owner, remaining) = capacity
            .revision_reservations
            .get_mut(&budget.id)
            .filter(|(owner, _)| owner == &session)
            .ok_or_else(|| io::Error::other("write reservation no longer current"))?;
        let _ = owner;
        *remaining = remaining.checked_add(additional).ok_or_else(|| io::Error::other("write budget overflow"))?;
        capacity.revision = Arc::new(CapacityRevision);
        drop(capacity);
        self.capacity_changed.notify_waiters();
        Ok(())
    }

    /// Admission of a complete fresh attempt; unlike replacement admission this
    /// cannot credit a leftover stable file against the next attempt's peak.
    pub(crate) async fn ensure_revision_peak_capacity<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        peak: u64,
    ) -> io::Result<()> {
        let reservation = self.reserve_revision_peak_reclaiming(key, peak).await?;
        drop(reservation);
        Ok(())
    }

    pub(crate) async fn reserve_revision_peak_reclaiming<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        peak: u64,
    ) -> io::Result<HlsRevisionDiskReservation> {
        // Reserve and reclaim under one admission gate; the reclaimer must never
        // reenter admission. It only deletes already-owned files.
        let _admission = self.capacity_admission_gate.lock().await;
        let mut outcome = HlsCacheCapacityReclaimOutcome::default();
        for attempt in 0..2 {
            match self.reserve_revision_peak_inner(key, peak).await {
                Ok(reservation) => return Ok(reservation),
                Err(error) => {
                    let Some(capacity) = super::hls_cache_capacity_from_io(&error) else { return Err(error) };
                    if attempt == 1 {
                        return Err(capacity_error(capacity.pressure(), outcome, capacity.revision().clone()));
                    }
                    outcome = self
                        .reclaim_capacity(HlsCacheCapacityReclaimRequest {
                            proxy_session_id: key.proxy_session_id().clone(),
                            target_path: self.object_path(key),
                            required_session_bytes: capacity.required_session_bytes(),
                            required_global_bytes: capacity.required_global_bytes(),
                            staged_bytes: peak,
                        })
                        .await?;
                }
            }
        }
        unreachable!("bounded admission loop returns on second attempt")
    }

    /// Deletes only the stable file installed by this exact peak owner. The
    /// path mutation ticket serializes this comparison with every later rename.
    pub(crate) async fn delete_write_owned<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        budget: &HlsRevisionDiskReservation,
    ) -> io::Result<()> {
        let path = Self::path_for_key_in(&budget.cache_path, key);
        let mutation = self.begin_capacity_mutation(&budget.cache_path, &path, key.session_path_component()).await;
        let owned = self.capacity.lock_unpoisoned().write_owners.get(&path) == Some(&budget.id);
        if owned {
            let result = self.delete_with_reservation(path.clone(), mutation).await;
            if result.is_err() {
                self.capacity
                    .lock_unpoisoned()
                    .abandoned_writes
                    .insert(path, (budget.id, key.session_path_component()));
            }
            result
        } else {
            Ok(())
        }
    }

    pub(crate) async fn retry_abandoned_write_deletions(&self) {
        let pending: Vec<_> = self
            .capacity
            .lock_unpoisoned()
            .abandoned_writes
            .iter()
            .take(super::MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN)
            .map(|(path, owner)| (path.clone(), owner.clone()))
            .collect();
        let root = self.cache_path_snapshot();
        for (path, (owner, session)) in pending {
            if !path.starts_with(&root.path) {
                self.capacity.lock_unpoisoned().abandoned_writes.remove(&path);
                continue;
            }
            let mutation = self.begin_capacity_mutation(&root.path, &path, session).await;
            let owned = self.capacity.lock_unpoisoned().write_owners.get(&path) == Some(&owner);
            if owned {
                let _ = self.delete_with_reservation(path, mutation).await;
            } else {
                self.capacity.lock_unpoisoned().abandoned_writes.remove(&path);
            }
        }
    }

    pub(crate) fn register_reserved_spool(&self, path: PathBuf, budget: &HlsRevisionDiskReservation) {
        let mut capacity = self.capacity.lock_unpoisoned();
        capacity.write_owners.insert(path.clone(), budget.id);
        capacity.reserved_spools.insert(path, budget.id);
    }

    pub(crate) fn account_reserved_spool(
        &self,
        path: &std::path::Path,
        size: u64,
        budget: &HlsRevisionDiskReservation,
    ) -> io::Result<()> {
        let mut capacity = self.capacity.lock_unpoisoned();
        let (session, remaining) = capacity
            .revision_reservations
            .get_mut(&budget.id)
            .ok_or_else(|| io::Error::other("spool reservation expired"))?;
        let session = session.clone();
        *remaining = remaining.checked_sub(size).ok_or_else(|| io::Error::other("spool exceeds peak budget"))?;
        capacity.reserved_spools.remove(path);
        capacity.total_bytes = capacity.total_bytes.saturating_add(size);
        *capacity.session_bytes.entry(session).or_default() += size;
        capacity.revision_bytes = capacity.revision_bytes.saturating_add(size);
        super::capacity::update_revision_metrics(&capacity);
        capacity.revision = Arc::new(CapacityRevision);
        drop(capacity);
        self.capacity_changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn install_revision_metrics(&self, metrics: &Arc<crate::HlsCacheMetrics>) {
        let mut capacity = self.capacity.lock_unpoisoned();
        capacity.revision_metrics = Some(Arc::downgrade(metrics));
        super::capacity::update_revision_metrics(&capacity);
    }

    pub(crate) fn accounted_session_bytes(&self, session: &crate::ProxySessionId) -> u64 {
        let key = super::SegmentCacheKey::new(session.clone(), 0, "ts");
        self.capacity.lock_unpoisoned().session_bytes.get(&key.session_path_component()).copied().unwrap_or_default()
    }

    pub(crate) fn accounted_global_bytes(&self) -> u64 { self.capacity.lock_unpoisoned().total_bytes }

    pub(crate) async fn session_revision_disk_bytes(&self, session: &crate::ProxySessionId) -> io::Result<u64> {
        let key = super::SegmentCacheKey::new(session.clone(), 0, "ts");
        let mut entries = match fs::read_dir(self.cache_path_snapshot().path.join(key.session_path_component())).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut bytes = 0u64;
        while let Some(entry) = entries.next_entry().await? {
            if super::revisions::is_revision_cache_path(&entry.path()) && entry.file_type().await?.is_file() {
                match entry.metadata().await {
                    Ok(metadata) => bytes = bytes.saturating_add(metadata.len()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(bytes)
    }

    pub(crate) fn remaining_revision_bytes(&self, session: &crate::ProxySessionId) -> u64 {
        let key = super::SegmentCacheKey::new(session.clone(), 0, "ts");
        revision_reserved_bytes(&self.capacity.lock_unpoisoned(), &key.session_path_component()).1
    }
}

impl HlsRevisionDiskReservation {
    /// Resolves cleanup against this write's original root, even after handoff.
    pub(crate) fn object_path<K: HlsCacheObjectKey>(&self, key: &K) -> PathBuf {
        HlsSegmentCache::path_for_key_in(&self.cache_path, key)
    }
}
