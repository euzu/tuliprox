use super::{
    ffmpeg_identity_version, probe::adopt_repair_output, registry::repair_object_metadata_key,
    safe_hls_access_lease_id, safe_proxy_session_id, sha256_file, window::RepairStatus, CachedSegmentMetadata,
    HlsAccessLeaseId, HlsCacheObjectKey, HlsCorruptSegmentWatchdogManager, HlsPostProcessingDeadline,
    HlsRepairObjectMetadata, HlsRepairObjectMetadataKey, HlsRepairPrewarmGuard, HlsRepairRenderedObjectId,
    HlsRepairWindowRegistry, HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentRepairObjectContext,
    HlsSegmentRepairRuntime, HlsSegmentRepairStats, HlsSelectedRepairCandidate, ProxySessionId, RepairIdentity,
    SegmentRepairMetadata, COMMAND_VERSION, REPAIR_METADATA_MAX_ENTRIES, REPAIR_OBJECT_METADATA_MAX_ENTRIES,
};
use arc_swap::ArcSwap;
use log::debug;
use shared::model::HlsSegmentRepairMode;
use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::Arc,
};
use tokio::{
    fs,
    io::AsyncRead,
    sync::{Mutex, RwLock, Semaphore},
};
use tuliprox_core::model::{HlsCorruptSegmentWatchdogConfig, HlsSegmentRepairConfig};

impl HlsSegmentRepairRuntime {
    pub(super) fn new(config: HlsSegmentRepairConfig) -> Self {
        let semaphore = if config.max_parallel_repairs == 0 {
            None
        } else {
            Some(Arc::new(Semaphore::new(config.max_parallel_repairs)))
        };
        let watchdog_config = &config.corrupt_segment_watchdog;
        let watchdog_semaphore = Arc::new(Semaphore::new(watchdog_config.max_parallel_jobs.max(1)));
        Self { config, semaphore, watchdog_semaphore }
    }

    pub(super) fn repair_enabled(&self) -> bool {
        self.config.max_level != HlsSegmentRepairMode::Off && self.config.apply_to_first_segments > 0
    }

    pub(super) fn postprocessing_enabled(&self) -> bool {
        self.repair_enabled() || self.config.corrupt_segment_watchdog.mode.is_enabled()
    }

    pub(super) fn postprocess_timeout_ms(&self) -> u64 { self.config.postprocess_timeout_ms.get().max(100) }
}

fn log_segment_repair_config(config: &HlsSegmentRepairConfig) {
    debug!(
        "HLS segment repair configured: max_level={} segments={} max_parallel={} postprocess_timeout_ms={}",
        config.max_level.as_log_value(),
        config.apply_to_first_segments,
        config.max_parallel_repairs,
        config.postprocess_timeout_ms
    );
}

fn log_corrupt_segment_watchdog_config(config: &HlsCorruptSegmentWatchdogConfig) {
    debug!(
        "HLS corrupt segment watchdog configured: mode={} max_parallel_jobs={}",
        config.mode.as_log_value(),
        config.max_parallel_jobs
    );
}

impl HlsSegmentRepairManager {
    pub fn new(config: HlsSegmentRepairConfig) -> Self {
        let watchdog_config = config.corrupt_segment_watchdog.clone();
        log_segment_repair_config(&config);
        log_corrupt_segment_watchdog_config(&watchdog_config);
        Self {
            runtime: ArcSwap::from_pointee(HlsSegmentRepairRuntime::new(config)),
            watchdog: HlsCorruptSegmentWatchdogManager::new(),
            windows: RwLock::new(HlsRepairWindowRegistry::default()),
            metadata: RwLock::new(HashMap::new()),
            metadata_order: Mutex::new(VecDeque::new()),
            object_metadata: RwLock::new(HashMap::new()),
            object_metadata_order: Mutex::new(VecDeque::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn update_config(&self, config: HlsSegmentRepairConfig) {
        let current = self.runtime.load();
        if current.config == config {
            return;
        }
        let watchdog_config = config.corrupt_segment_watchdog.clone();
        log_segment_repair_config(&config);
        log_corrupt_segment_watchdog_config(&watchdog_config);
        self.runtime.store(Arc::new(HlsSegmentRepairRuntime::new(config)));
    }

    pub async fn start_access_lease_window(&self, lease_id: HlsAccessLeaseId) {
        let runtime = self.runtime.load_full();
        if !runtime.repair_enabled() {
            return;
        }
        self.windows.write().await.start_window(lease_id.clone(), &runtime.config);
        debug!(
            "HLS segment repair window started: lease={} max_level={} segments={}",
            safe_hls_access_lease_id(&lease_id),
            runtime.config.max_level.as_log_value(),
            runtime.config.apply_to_first_segments
        );
    }

    pub async fn ensure_access_lease_window(&self, lease_id: HlsAccessLeaseId) {
        let runtime = self.runtime.load_full();
        if !runtime.repair_enabled() {
            return;
        }
        let started = self.windows.write().await.ensure_window(lease_id.clone(), &runtime.config);
        if started {
            debug!(
                "HLS segment repair window started: lease={} max_level={} segments={}",
                safe_hls_access_lease_id(&lease_id),
                runtime.config.max_level.as_log_value(),
                runtime.config.apply_to_first_segments
            );
        }
    }

    pub fn prewarm_candidate_limit(&self) -> usize {
        let runtime = self.runtime.load();
        if runtime.repair_enabled() {
            usize::from(runtime.config.apply_to_first_segments)
        } else {
            0
        }
    }

    pub async fn spawn_ready_cache_prewarm<K>(
        self: &Arc<Self>,
        segment_cache: Arc<HlsSegmentCache>,
        candidates: Vec<(K, HlsSegmentRepairObjectContext)>,
        guard: HlsRepairPrewarmGuard,
    ) where
        K: HlsCacheObjectKey + Send + Sync + 'static,
    {
        self.spawn_ready_cache_prewarm_inner(segment_cache, candidates, Some(guard)).await;
    }

    pub(super) async fn spawn_ready_cache_prewarm_inner<K>(
        self: &Arc<Self>,
        segment_cache: Arc<HlsSegmentCache>,
        candidates: Vec<(K, HlsSegmentRepairObjectContext)>,
        guard: Option<HlsRepairPrewarmGuard>,
    ) where
        K: HlsCacheObjectKey + Send + Sync + 'static,
    {
        for (key, context) in candidates {
            let Some((mode, runtime, candidate)) = self.try_select_candidate(&context).await else {
                continue;
            };
            let manager = Arc::clone(self);
            let segment_cache = Arc::clone(&segment_cache);
            let guard = guard.clone();
            tokio::spawn(async move {
                let selection = HlsSelectedRepairCandidate { mode, runtime, candidate, prewarm_guard: guard.as_ref() };
                if let Err(err) =
                    manager.repair_ready_cache_hit_with_candidate(&segment_cache, &key, context, selection).await
                {
                    debug!("HLS segment repair prewarm ended without a reusable decision: error_kind={:?}", err.kind());
                }
            });
        }
    }

    pub async fn remove_access_lease_window(&self, lease_id: &HlsAccessLeaseId) {
        self.windows.write().await.remove_access_lease(lease_id);
    }

    pub async fn remove_proxy_session_state(&self, proxy_session_id: &ProxySessionId, lease_ids: &[HlsAccessLeaseId]) {
        self.windows.write().await.remove_proxy_session(proxy_session_id, lease_ids);
        self.object_metadata.write().await.retain(|key, _| key.proxy_session_id != *proxy_session_id);
        self.object_metadata_order.lock().await.retain(|key| key.proxy_session_id != *proxy_session_id);
    }

    pub async fn clear_runtime_state(&self) {
        self.windows.write().await.clear();
        self.metadata.write().await.clear();
        self.metadata_order.lock().await.clear();
        self.object_metadata.write().await.clear();
        self.object_metadata_order.lock().await.clear();
        self.locks.lock().await.clear();
        self.watchdog.clear_runtime_state().await;
    }

    pub async fn stats(&self) -> HlsSegmentRepairStats {
        let mut stats = self.windows.read().await.stats();
        stats.metadata = self.metadata.read().await.len();
        stats.object_metadata = self.object_metadata.read().await.len();
        stats.locks = self.locks.lock().await.len();
        let watchdog = self.watchdog.stats().await;
        stats.watchdog_metadata = watchdog.metadata;
        stats.watchdog_locks = watchdog.locks;
        stats
    }

    pub async fn commit_origin_response<K, R>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        reader: R,
        deadline: tokio::time::Instant,
        context: HlsSegmentRepairObjectContext,
    ) -> io::Result<CachedSegmentMetadata>
    where
        K: HlsCacheObjectKey,
        R: AsyncRead + Unpin,
    {
        let raw = segment_cache.stage_temp_with_deadline(key, reader, deadline).await?;
        let runtime = self.runtime.load_full();
        if !runtime.postprocessing_enabled() {
            return segment_cache.commit_staged(key, raw).await;
        }
        let postprocessing_deadline = HlsPostProcessingDeadline::new(runtime.postprocess_timeout_ms());
        self.process_staged_and_commit(segment_cache, key, raw, context, runtime, postprocessing_deadline).await
    }

    #[allow(clippy::too_many_lines)]
    pub async fn repair_ready_cache_hit<K>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        context: HlsSegmentRepairObjectContext,
    ) -> io::Result<Option<CachedSegmentMetadata>>
    where
        K: HlsCacheObjectKey,
    {
        let Some((mode, runtime, candidate)) = self.try_select_or_join_candidate(&context).await else {
            return Ok(None);
        };
        self.repair_ready_cache_hit_with_candidate(
            segment_cache,
            key,
            context,
            HlsSelectedRepairCandidate { mode, runtime, candidate, prewarm_guard: None },
        )
        .await
    }

    #[allow(clippy::too_many_lines)]
    pub(super) async fn repair_ready_cache_hit_with_candidate<K>(
        &self,
        segment_cache: &HlsSegmentCache,
        key: &K,
        context: HlsSegmentRepairObjectContext,
        selection: HlsSelectedRepairCandidate<'_>,
    ) -> io::Result<Option<CachedSegmentMetadata>>
    where
        K: HlsCacheObjectKey,
    {
        let HlsSelectedRepairCandidate { mode, runtime, candidate, prewarm_guard } = selection;
        let Some(metadata) = segment_cache.metadata(key).await? else {
            return Ok(None);
        };
        let raw_hash = sha256_file(&metadata.path).await?;
        let object_key = repair_object_metadata_key(&context, mode);
        if self.object_metadata_matches(&object_key, &raw_hash).await {
            return Ok(None);
        }
        let identity = RepairIdentity {
            raw_sha256: raw_hash.clone(),
            repair_mode: mode,
            command_version: COMMAND_VERSION,
            ffmpeg_version: ffmpeg_identity_version(),
        };
        if self.repair_metadata(&identity).await.is_some() {
            self.record_object_metadata_from_repair_identity(object_key, raw_hash.clone(), Some(raw_hash), &identity)
                .await;
            return Ok(None);
        }
        let lock = self.lock_for_identity(identity.clone()).await;
        let result = {
            let _guard = lock.lock().await;
            let prewarm_is_current = match prewarm_guard {
                Some(prewarm_guard) => prewarm_guard.is_current().await,
                None => true,
            };
            if !prewarm_is_current {
                Ok(None)
            } else if let Some(current_metadata) = segment_cache.metadata(key).await? {
                let current_hash = sha256_file(&current_metadata.path).await?;
                if current_hash != raw_hash {
                    Ok(None)
                } else if self.repair_metadata(&identity).await.is_some() {
                    self.record_object_metadata_from_repair_identity(
                        object_key.clone(),
                        current_hash.clone(),
                        Some(current_hash),
                        &identity,
                    )
                    .await;
                    Ok(None)
                } else {
                    let deadline = HlsPostProcessingDeadline::new(runtime.postprocess_timeout_ms());
                    if let Some(fixed_path) = self
                        .repair_file(
                            &current_metadata.path,
                            current_metadata.size,
                            &identity,
                            &context,
                            runtime.clone(),
                            &deadline,
                        )
                        .await?
                    {
                        let candidate_current = self.windows.read().await.candidate_is_current(&candidate)
                            && match prewarm_guard {
                                Some(prewarm_guard) => prewarm_guard.is_current().await,
                                None => true,
                            };
                        let latest_hash = match segment_cache.metadata(key).await? {
                            Some(latest_metadata) => Some(sha256_file(&latest_metadata.path).await?),
                            None => None,
                        };
                        if !candidate_current || latest_hash.as_deref() != Some(current_hash.as_str()) {
                            let _ = fs::remove_file(&fixed_path).await;
                            Ok(None)
                        } else {
                            let fixed_size = fs::metadata(&fixed_path).await?.len();
                            let staged = adopt_repair_output(segment_cache, fixed_path, fixed_size).await?;
                            let committed = segment_cache.commit_staged(key, staged).await?;
                            self.record_metadata(
                                identity.clone(),
                                RepairStatus::Fixed,
                                current_metadata.size,
                                committed.size,
                                None,
                            )
                            .await;
                            let committed_hash = sha256_file(&committed.path).await?;
                            self.record_object_metadata(
                                object_key.clone(),
                                HlsRepairObjectMetadata {
                                    committed_sha256: committed_hash,
                                    raw_sha256: Some(current_hash),
                                    status: RepairStatus::Fixed,
                                    raw_size: current_metadata.size,
                                    final_size: committed.size,
                                    validation_reason: None,
                                },
                            )
                            .await;
                            Ok(Some(committed))
                        }
                    } else {
                        self.record_object_metadata_from_repair_identity(
                            object_key.clone(),
                            current_hash.clone(),
                            Some(current_hash),
                            &identity,
                        )
                        .await;
                        Ok(None)
                    }
                }
            } else {
                Ok(None)
            }
        };
        self.remove_lock_if_unused(&identity, &lock).await;
        result
    }

    pub(super) async fn record_metadata(
        &self,
        identity: RepairIdentity,
        status: RepairStatus,
        raw_size: u64,
        final_size: u64,
        validation_reason: Option<String>,
    ) {
        let inserted_new = {
            let mut metadata = self.metadata.write().await;
            let inserted_new = !metadata.contains_key(&identity);
            metadata
                .insert(identity.clone(), SegmentRepairMetadata { status, raw_size, final_size, validation_reason });
            inserted_new
        };
        if inserted_new {
            self.metadata_order.lock().await.push_back(identity);
        }
        self.prune_metadata().await;
    }

    pub(super) async fn repair_metadata(&self, identity: &RepairIdentity) -> Option<SegmentRepairMetadata> {
        self.metadata.read().await.get(identity).cloned()
    }

    pub(super) async fn object_metadata_matches(
        &self,
        key: &HlsRepairObjectMetadataKey,
        committed_sha256: &str,
    ) -> bool {
        let matches = self
            .object_metadata
            .read()
            .await
            .get(key)
            .is_some_and(|metadata| metadata.committed_sha256 == committed_sha256);
        if matches {
            let (source, resource) = match &key.rendered_object_id {
                HlsRepairRenderedObjectId::Normal { proxy_seq } => ("normal", format!("{proxy_seq:06}")),
                HlsRepairRenderedObjectId::Transient { resource_id } => ("transient", resource_id.clone()),
            };
            debug!(
                "HLS segment repair object metadata hit: proxy_session={} source={} resource={} mode={}",
                safe_proxy_session_id(&key.proxy_session_id),
                source,
                resource,
                key.repair_mode.as_log_value()
            );
        }
        matches
    }

    pub(super) async fn record_object_metadata_from_repair_identity(
        &self,
        key: HlsRepairObjectMetadataKey,
        committed_sha256: String,
        raw_sha256: Option<String>,
        identity: &RepairIdentity,
    ) {
        let Some(metadata) = self.repair_metadata(identity).await else {
            return;
        };
        self.record_object_metadata(
            key,
            HlsRepairObjectMetadata {
                committed_sha256,
                raw_sha256,
                status: metadata.status,
                raw_size: metadata.raw_size,
                final_size: metadata.final_size,
                validation_reason: metadata.validation_reason,
            },
        )
        .await;
    }

    pub(super) async fn record_object_metadata(
        &self,
        key: HlsRepairObjectMetadataKey,
        metadata: HlsRepairObjectMetadata,
    ) {
        let inserted_new = {
            let mut object_metadata = self.object_metadata.write().await;
            let inserted_new = !object_metadata.contains_key(&key);
            object_metadata.insert(key.clone(), metadata);
            inserted_new
        };
        if inserted_new {
            self.object_metadata_order.lock().await.push_back(key);
        }
        self.prune_object_metadata().await;
    }

    pub(super) async fn lock_for_identity(&self, identity: RepairIdentity) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().await;
        Arc::clone(locks.entry(identity).or_insert_with(|| Arc::new(Mutex::new(()))))
    }

    pub(super) async fn remove_lock_if_unused(&self, identity: &RepairIdentity, lock: &Arc<Mutex<()>>) {
        let mut locks = self.locks.lock().await;
        if Arc::strong_count(lock) <= 2 && locks.get(identity).is_some_and(|current| Arc::ptr_eq(current, lock)) {
            locks.remove(identity);
        }
    }

    pub(super) async fn prune_metadata(&self) {
        loop {
            let should_prune = self.metadata.read().await.len() > REPAIR_METADATA_MAX_ENTRIES;
            if !should_prune {
                return;
            }
            let Some(oldest) = self.metadata_order.lock().await.pop_front() else {
                return;
            };
            self.metadata.write().await.remove(&oldest);
        }
    }

    pub(super) async fn prune_object_metadata(&self) {
        loop {
            let should_prune = self.object_metadata.read().await.len() > REPAIR_OBJECT_METADATA_MAX_ENTRIES;
            if !should_prune {
                return;
            }
            let Some(oldest) = self.object_metadata_order.lock().await.pop_front() else {
                return;
            };
            self.object_metadata.write().await.remove(&oldest);
        }
    }
}
