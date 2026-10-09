use super::{
    error::{cache_object_limit_error, capacity_error},
    hls_cache_capacity_from_io,
    lifecycle::{is_temp_cache_file, REWRITE_SECRET_FINGERPRINT_FILE},
    revisions::{is_revision_cache_path, revision_reserved_bytes},
    safe_proxy_session_id, CacheCapacityPressure, CacheCapacityState, CapacityInvalidationGuard,
    CapacityMutationReservation, CapacityRevision, HlsCacheCapacityRevision, HlsCacheObjectKey, HlsSegmentCache,
    ProxySessionId,
};
use futures::future::BoxFuture;
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex as StdMutex, Weak},
};
use tokio::{fs, sync::Notify};

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

impl Drop for CapacityInvalidationGuard {
    fn drop(&mut self) { invalidate_capacity_state(&self.capacity, &self.changed); }
}

impl CapacityMutationReservation {
    pub(super) fn projected_usage(&self) -> HlsCacheCapacityUsage {
        let capacity = self.capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        HlsCacheCapacityUsage {
            session_bytes: capacity.session_bytes.get(&self.session_component).copied().unwrap_or_default(),
            global_bytes: capacity.total_bytes,
        }
    }

    pub(super) fn reserve_replacement(
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
        let (reserved_global, reserved_session) = revision_reserved_bytes(&capacity, &self.session_component);
        if projected_total.saturating_add(reserved_global) > max_cache_bytes
            || projected_session.saturating_add(reserved_session) > max_session_bytes
        {
            return Err(CapacityReservationError::Exceeded {
                pressure: CacheCapacityPressure {
                    configured_session_bytes: max_session_bytes,
                    configured_global_bytes: max_cache_bytes,
                    current_session_bytes: session_size,
                    current_global_bytes: capacity.total_bytes,
                    staged_bytes: new_size,
                    required_session_bytes: projected_session
                        .saturating_add(reserved_session)
                        .saturating_sub(max_session_bytes),
                    required_global_bytes: projected_total
                        .saturating_add(reserved_global)
                        .saturating_sub(max_cache_bytes),
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

    pub(super) fn mark_filesystem_mutation_started(&mut self) { self.filesystem_mutation_started = true; }

    pub(super) fn finish_replacement(mut self) {
        self.replacement = None;
        self.finish_mutation(None);
    }

    pub(super) fn finish_delete(mut self, deleted_size: u64) {
        self.finish_mutation(Some(deleted_size));
        self.replacement = None;
    }

    pub(super) fn finish_mutation(&mut self, deleted_size: Option<u64>) {
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
pub(super) enum CapacityReservationError {
    Retry,
    Invalidated,
    Exceeded { pressure: CacheCapacityPressure, revision: HlsCacheCapacityRevision },
}

fn store_session_bytes(capacity: &mut CacheCapacityState, session_component: &str, bytes: u64) {
    if bytes == 0 {
        capacity.session_bytes.remove(session_component);
    } else {
        capacity.session_bytes.insert(session_component.to_string(), bytes);
    }
}

impl HlsSegmentCache {
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

    pub(super) async fn reclaim_capacity(
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

    pub(super) async fn ensure_capacity_initialized(&self, cache_path: &Path) -> io::Result<()> {
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
            let (total_bytes, session_bytes, revision_bytes) = scan_committed_cache_usage(cache_path).await?;
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
                    capacity.revision_bytes = revision_bytes;
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

    pub(super) async fn begin_capacity_mutation(
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

    pub(super) fn try_begin_capacity_mutation(
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

    pub(super) async fn prepare_capacity_replacement(
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

    pub(super) fn invalidate_capacity_accounting(&self) {
        invalidate_capacity_state(&self.capacity, &self.capacity_changed);
    }

    pub(super) fn capacity_invalidation_guard(&self) -> CapacityInvalidationGuard {
        CapacityInvalidationGuard { capacity: Arc::clone(&self.capacity), changed: Arc::clone(&self.capacity_changed) }
    }

    pub(super) fn rewrite_secret_fingerprint_path(&self) -> PathBuf {
        self.cache_path_snapshot().path.join(REWRITE_SECRET_FINGERPRINT_FILE)
    }
}

fn invalidate_capacity_state(capacity: &StdMutex<CacheCapacityState>, changed: &Notify) {
    let mut capacity = capacity.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    capacity.initialized = false;
    capacity.revision = Arc::new(CapacityRevision);
    drop(capacity);
    changed.notify_waiters();
}

pub(super) async fn scan_committed_cache_usage(cache_path: &Path) -> io::Result<(u64, HashMap<String, u64>, u64)> {
    let mut total_bytes = 0_u64;
    let mut revision_bytes = 0_u64;
    let mut session_bytes = HashMap::new();
    let mut root_entries = match fs::read_dir(cache_path).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, session_bytes, 0)),
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
                } else if file_type.is_file() {
                    let revision = is_revision_cache_path(&entry.path());
                    if revision || !is_temp_cache_file(&entry.path()) {
                        let size = entry.metadata().await?.len();
                        bytes = bytes.saturating_add(size);
                        if revision {
                            revision_bytes = revision_bytes.saturating_add(size);
                        }
                    }
                }
            }
        }
        total_bytes = total_bytes.saturating_add(bytes);
        if bytes > 0 {
            session_bytes.insert(session_component, bytes);
        }
    }
    Ok((total_bytes, session_bytes, revision_bytes))
}
