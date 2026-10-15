use super::{
    error::capacity_error, CacheCapacityPressure, CacheCapacityState, CachedSegmentMetadata, CapacityRevision,
    HlsCacheCapacityReclaimOutcome, HlsCacheCapacityRevision, HlsCacheObjectKey, HlsRevisionDiskReservation,
    HlsRevisionFilePin, HlsSegmentCache, StagedCacheObject, MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN,
};
use crate::sync_ext::MutexExt;
use std::{
    fmt, io,
    path::Path,
    sync::{atomic::Ordering, Arc},
    time::SystemTime,
};
use tokio::{
    fs,
    fs::{File, OpenOptions},
    time::Instant,
};

impl HlsRevisionDiskReservation {
    /// Records that a revision spool was written directly under this reservation.
    ///
    /// Those bytes bypass the staged commit accounting, so releasing the
    /// reservation must rescan the cache to pick them up.
    pub fn mark_unaccounted_writes(&mut self) { self.unaccounted_writes = true; }
}

impl Drop for HlsRevisionDiskReservation {
    fn drop(&mut self) {
        let mut capacity = self.capacity.lock_unpoisoned();
        let released = capacity.revision_reservations.remove(&self.id).is_some();
        capacity.reserved_spools.retain(|_, owner| *owner != self.id);
        if self.unaccounted_writes {
            capacity.initialized = false;
        }
        if released || self.unaccounted_writes {
            capacity.revision = Arc::new(CapacityRevision);
        }
        drop(capacity);
        self.changed.notify_waiters();
    }
}

impl fmt::Debug for HlsRevisionFilePin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("HlsRevisionFilePin") }
}

impl HlsSegmentCache {
    pub async fn reserve_revision_peak<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        bytes: u64,
    ) -> io::Result<HlsRevisionDiskReservation> {
        let _admission = self.capacity_admission_gate.lock().await;
        self.reserve_revision_peak_inner(key, bytes).await
    }

    pub(super) async fn reserve_revision_peak_inner<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        bytes: u64,
    ) -> io::Result<HlsRevisionDiskReservation> {
        let root = self.cache_path_snapshot();
        self.ensure_capacity_initialized(&root.path).await?;
        let session = key.session_path_component();
        let mut capacity = self.capacity.lock_unpoisoned();
        let (reserved_global, reserved_session) = revision_reserved_bytes(&capacity, &session);
        let global = capacity.total_bytes.saturating_add(reserved_global);
        let local = capacity.session_bytes.get(&session).copied().unwrap_or_default().saturating_add(reserved_session);
        let global_limit = self.max_cache_bytes.load(Ordering::Acquire);
        let local_limit = self.max_session_bytes.load(Ordering::Acquire);
        let required_global = global.saturating_add(bytes).saturating_sub(global_limit);
        let required_session = local.saturating_add(bytes).saturating_sub(local_limit);
        if !capacity.initialized || capacity.cache_path != root.path {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "revision disk reservation invalidated"));
        }
        if required_global > 0 || required_session > 0 {
            return Err(capacity_error(
                CacheCapacityPressure {
                    configured_session_bytes: local_limit,
                    configured_global_bytes: global_limit,
                    current_session_bytes: local,
                    current_global_bytes: global,
                    staged_bytes: bytes,
                    required_session_bytes: required_session,
                    required_global_bytes: required_global,
                },
                HlsCacheCapacityReclaimOutcome::default(),
                HlsCacheCapacityRevision(Arc::clone(&capacity.revision)),
            ));
        }
        let id =
            capacity.next_write_id.checked_add(1).ok_or_else(|| io::Error::other("cache write identity exhausted"))?;
        capacity.next_write_id = id;
        capacity.revision_reservations.insert(id, (session, bytes));
        capacity.revision = Arc::new(CapacityRevision);
        drop(capacity);
        self.capacity_changed.notify_waiters();
        Ok(HlsRevisionDiskReservation {
            id,
            capacity: Arc::clone(&self.capacity),
            changed: Arc::clone(&self.capacity_changed),
            unaccounted_writes: false,
            generation: root.generation,
            cache_path: root.path,
        })
    }

    pub fn pin_revision_file(&self, path: &Path) -> io::Result<HlsRevisionFilePin> {
        if !self.contains_current_cache_path(path) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "revision outside current cache root"));
        }
        Ok(HlsRevisionFilePin { _registration: self.register_active_temp_path(path)? })
    }

    pub async fn create_revision_spool<K: HlsCacheObjectKey>(&self, key: &K) -> io::Result<(File, HlsRevisionFilePin)> {
        self.create_revision_spool_with_budget(key, None).await
    }

    pub(crate) async fn create_revision_spool_with_budget<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        budget: Option<&HlsRevisionDiskReservation>,
    ) -> io::Result<(File, HlsRevisionFilePin)> {
        let commit_gate = Arc::clone(&self.cache_path_commit_gate).read_owned().await;
        let root = self.cache_path_snapshot();
        if budget.is_some_and(|budget| !Arc::ptr_eq(&budget.generation, &root.generation)) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "revision reservation root retired"));
        }
        self.ensure_cache_root_marker_for(&root.path).await?;
        let path = Self::path_for_key_in(&root.path, key);
        let pin = self.pin_revision_file(&path)?;
        let parent = path.parent().ok_or_else(|| io::Error::other("revision path has no parent"))?.to_path_buf();
        let mut mutation = self.begin_capacity_mutation(&root.path, &path, key.session_path_component()).await;
        if let Some(budget) = budget {
            self.register_reserved_spool(path.clone(), budget);
        }
        super::staging::run_owned_cache_operation(
            Arc::clone(&self.owned_operation_permits),
            "revision-create",
            async move {
                fs::create_dir_all(parent).await?;
                let file = OpenOptions::new().write(true).create_new(true).open(&path).await?;
                mutation.finish_mutation(None);
                drop(commit_gate);
                Ok((file, pin))
            },
        )
        .await
    }

    pub async fn delete_orphan_revision_files(&self, cutoff: SystemTime) -> io::Result<usize> {
        let root = self.cache_path_snapshot();
        self.ensure_capacity_initialized(&root.path).await?;
        let Some(mut sessions) = skip_not_found(fs::read_dir(&root.path).await)? else { return Ok(0) };
        let mut deleted = 0usize;
        while let Some(session) = sessions.next_entry().await? {
            let Some(session_type) = skip_not_found(session.file_type().await)? else { continue };
            if !session_type.is_dir() {
                continue;
            }
            let Some(mut entries) = skip_not_found(fs::read_dir(session.path()).await)? else { continue };
            while let Some(entry) = entries.next_entry().await? {
                if deleted >= MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN {
                    return Ok(deleted);
                }
                let Some(kind) = skip_not_found(entry.file_type().await)? else { continue };
                if !kind.is_file() || !is_revision_file_name(&entry.file_name()) {
                    continue;
                }
                let Some(metadata) = skip_not_found(entry.metadata().await)? else { continue };
                if metadata.modified().is_ok_and(|modified| modified > cutoff) {
                    continue;
                }
                let path = entry.path();
                let Some(deletion) = self.reserve_path_deletion(&path) else {
                    continue;
                };
                let reservation = self
                    .begin_capacity_mutation(&root.path, &path, session.file_name().to_string_lossy().into_owned())
                    .await;
                if self.delete_with_reservation(path, reservation).await.is_ok() {
                    deleted += 1;
                }
                drop(deletion);
            }
        }
        Ok(deleted)
    }

    pub async fn publish_processed_revision<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        source: &Path,
        deadline: Instant,
    ) -> io::Result<CachedSegmentMetadata> {
        self.publish_processed_revision_with_budget(key, source, deadline, None).await
    }

    #[allow(clippy::too_many_lines, reason = "Keeps source ownership, peak admission and immutable linking together.")]
    pub(crate) async fn publish_processed_revision_with_budget<K: HlsCacheObjectKey>(
        &self,
        key: &K,
        source: &Path,
        deadline: Instant,
        budget: Option<Arc<HlsRevisionDiskReservation>>,
    ) -> io::Result<CachedSegmentMetadata> {
        let root = self.cache_path_snapshot();
        let staged_path = Self::temp_path_for_key(key, &root.path);
        let size = fs::metadata(source).await?.len();
        let checks_source_owner = budget.is_some();
        let budget = match budget {
            Some(budget) => budget,
            None => Arc::new(self.reserve_revision_peak_reclaiming(key, size).await?),
        };
        let (size, source_lease) = loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "revision publication timed out"));
            }
            self.ensure_write_budget_available(key, &budget, size).await?;
            // Preserve admission -> path-mutation ordering. A replacement must
            // not change the source between its size check and the hardlink.
            let admission = self.capacity_admission_gate.lock().await;
            let source_lease = self.begin_capacity_mutation(&root.path, source, key.session_path_component()).await;
            let actual_size = fs::metadata(source).await?.len();
            let remaining = {
                let capacity = self.capacity.lock_unpoisoned();
                if checks_source_owner && capacity.write_owners.get(source) != Some(&budget.id) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "revision source write replaced"));
                }
                capacity.revision_reservations.get(&budget.id).map_or(0, |(_, bytes)| *bytes)
            };
            drop(admission);
            if actual_size <= remaining {
                break (actual_size, source_lease);
            }
            drop(source_lease);
            self.ensure_write_budget_available(key, &budget, actual_size).await?;
        };
        let budget = Some(budget);
        let registration = self.register_active_temp_path(&staged_path)?;
        let staged =
            StagedCacheObject::registered(staged_path.clone(), size, Arc::clone(&root.generation), registration)
                .with_write_budget(budget.clone())?;
        let source_path = source.to_path_buf();
        #[cfg(test)]
        let checkpoint = self.take_startup_write_checkpoint(super::StartupWritePhase::BeforeRevisionLink);
        // A Tokio filesystem call can finish after its caller is cancelled.
        // Retain staging and its budget until the link has a definite result.
        let (staged, fallback_source) = super::staging::run_owned_cache_operation(
            Arc::clone(&self.owned_operation_permits),
            "revision-link",
            async move {
                #[cfg(test)]
                if let Some(checkpoint) = checkpoint {
                    checkpoint.wait().await?;
                }
                let fallback = if fs::hard_link(&source_path, &staged_path).await.is_ok() {
                    None
                } else {
                    // Opening under the same source lease also freezes the
                    // inode used by a copy fallback after replacement resumes.
                    Some(File::open(source_path).await?)
                };
                drop(source_lease);
                Ok((staged, fallback))
            },
        )
        .await?;
        if let Some(file) = fallback_source {
            self.remove_staged(staged).await?;
            let staged = self.stage_temp_with_budget(key, file, deadline, budget).await?;
            self.commit_staged(key, staged).await
        } else {
            self.commit_staged(key, staged).await
        }
    }

    /// Current revision file bytes after completed accounting mutations.
    pub fn revision_disk_bytes(&self) -> u64 { self.capacity.lock_unpoisoned().revision_bytes }

    /// Walks the cache tree for the current revision file bytes.
    #[cfg(test)]
    pub async fn scan_revision_disk_bytes(&self) -> io::Result<u64> {
        super::capacity::scan_committed_cache_usage(&self.cache_path_snapshot().path).await.map(|(_, _, bytes)| bytes)
    }
}

pub(super) fn revision_reserved_bytes(capacity: &CacheCapacityState, session: &str) -> (u64, u64) {
    capacity.revision_reservations.values().fold((0u64, 0u64), |(global, local), (owner, bytes)| {
        (global.saturating_add(*bytes), local.saturating_add(if owner == session { *bytes } else { 0 }))
    })
}

/// Maps a vanished filesystem entry to `None`; other errors still fail.
fn skip_not_found<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_revision_file_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str().and_then(|name| name.strip_prefix("revision-")) else {
        return false;
    };
    let Some((generation, tail)) = name.split_once('-') else {
        return false;
    };
    let Some((ordinal, suffix)) = tail.split_once('-') else {
        return false;
    };
    [generation, ordinal].iter().all(|part| part.len() == 16 && part.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && matches!(suffix, "raw.ts" | "processed.ts")
}

pub(super) fn is_revision_cache_path(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
        let name = name.split_once(".tmp.").map_or(name, |(name, _)| name);
        is_revision_file_name(std::ffi::OsStr::new(name))
    })
}
