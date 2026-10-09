use super::{
    deletion::CacheObjectDeletion, safe_proxy_session_id, CacheDeletionBatch, CacheDeletionQueueState,
    CacheInvalidationOutcome, GarbageCollectionPolicy, GarbageCollectionReport, HlsAccessLeaseStore,
    HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest, HlsCacheCapacityReclaimer, HlsCacheMetrics, HlsCtx,
    HlsExpiredSessionReason, HlsGarbageCollector, HlsSegmentCache, HlsSession, HlsSessionHandle, HlsSessionStore,
    HlsSwitchCacheCleanupReservation, MapCacheKey, SegmentCacheKey, HLS_CACHE_GC_INTERVAL,
    MAX_CACHE_DELETE_RETRIES_PER_RUN,
};
use arc_swap::ArcSwap;
use futures::{future::BoxFuture, FutureExt};
use log::{debug, error, info, warn};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock},
    time::{Duration, SystemTime},
};
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tokio_util::sync::CancellationToken;

impl HlsGarbageCollector {
    pub fn new(
        sessions: Arc<HlsSessionStore>,
        cache: Arc<HlsSegmentCache>,
        policy: GarbageCollectionPolicy,
        rewrite_secret_fingerprint: String,
    ) -> Self {
        Self::new_with_metrics(
            sessions,
            cache,
            policy,
            rewrite_secret_fingerprint,
            Arc::new(HlsCacheMetrics::default()),
        )
    }

    pub fn new_with_metrics(
        sessions: Arc<HlsSessionStore>,
        cache: Arc<HlsSegmentCache>,
        policy: GarbageCollectionPolicy,
        rewrite_secret_fingerprint: String,
        metrics: Arc<HlsCacheMetrics>,
    ) -> Self {
        Self {
            sessions,
            cache,
            policy: ArcSwap::from_pointee(policy),
            rewrite_secret_fingerprint: ArcSwap::from_pointee(rewrite_secret_fingerprint),
            metrics,
            pending_cache_deletions: Arc::new(StdMutex::new(CacheDeletionQueueState::default())),
            access_leases: StdRwLock::new(None),
            run_once_gate: AsyncMutex::new(()),
        }
    }

    pub fn install_access_leases(&self, access_leases: &Arc<RwLock<HlsAccessLeaseStore>>) {
        *self.access_leases.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::downgrade(access_leases));
    }

    pub fn update_config(&self, policy: GarbageCollectionPolicy, rewrite_secret_fingerprint: String) {
        self.policy.store(Arc::new(policy));
        self.rewrite_secret_fingerprint.store(Arc::new(rewrite_secret_fingerprint));
    }

    /// Serializes a cache-root handoff with GC and discards logical deletion tickets bound to the previous root.
    pub async fn update_cache_path(&self, cache_path: impl Into<PathBuf>) -> bool {
        let _run_once = self.run_once_gate.lock().await;
        let changed = self.cache.update_cache_path(cache_path).await;
        if changed {
            let mut queue = self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *queue = CacheDeletionQueueState::default();
        }
        changed
    }

    pub fn policy(&self) -> Arc<GarbageCollectionPolicy> { self.policy.load_full() }

    pub fn rewrite_secret_fingerprint(&self) -> String { self.rewrite_secret_fingerprint.load().to_string() }

    pub async fn run_once(&self, now_ms: u64) -> io::Result<GarbageCollectionReport> {
        let _run_once = self.run_once_gate.lock().await;
        let policy = self.policy.load_full();
        let mut report = GarbageCollectionReport::default();
        let mut deletion_attempt_budget = MAX_CACHE_DELETE_RETRIES_PER_RUN;
        self.retry_pending_cache_deletions(&mut report, &mut deletion_attempt_budget).await;
        if self.ensure_cache_marker(&mut report).await? {
            report.cache_object_deletions_deferred = self.pending_cache_deletion_count();
            self.record_report_metrics(&report);
            return Ok(report);
        }

        // Captured before the in-memory session snapshot so any directory committed
        // after this instant is treated as a potential concurrent create and is
        // skipped by the orphan cleanup freshness guard.
        let gc_start = SystemTime::now();
        let cutoff = gc_start
            .checked_sub(Duration::from_millis(policy.temp_file_retention_ms))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        report.temp_files_deleted = self.cache.delete_temp_files_older_than(cutoff).await?;

        let sessions = self.sessions.list_sessions().await;
        let mut active_session_ids = HashSet::with_capacity(sessions.len());
        for session in &sessions {
            active_session_ids.insert(session.read().await.proxy_session_id.clone());
        }
        report.orphan_session_dirs_deleted =
            self.cache.delete_orphan_session_dirs(&active_session_ids, gc_start).await?;
        for session in &sessions {
            let mut session_deletions = self.reserve_cache_deletion_batch();
            let mut session = session.write().await;
            Self::collect_session_deletions(
                &mut session,
                &self.cache,
                now_ms,
                &policy,
                &mut report,
                &mut session_deletions,
            );
            session_deletions.persist(&mut report);
        }

        let mut global_deletions = self.reserve_cache_deletion_batch();
        self.collect_global_size_deletions(&sessions, &policy, &mut report, &mut global_deletions).await;
        global_deletions.persist(&mut report);
        self.retry_pending_cache_deletions(&mut report, &mut deletion_attempt_budget).await;

        for session in &sessions {
            self.remove_idle_session_if_still_idle(session, now_ms, &policy, &mut report).await;
        }

        report.cache_object_deletions_deferred = self.pending_cache_deletion_count();
        self.record_report_metrics(&report);
        if report.did_cleanup_or_invalidate() {
            info!(
                "HLS session garbage collection completed: temp_files_deleted={} orphan_session_dirs_deleted={} stale_queue_entries_removed={} cache_deletes_planned={} cache_deletes_succeeded={} cache_deletes_deferred={} segments_deleted={} maps_deleted={} transient_resources_pruned={} transient_objects_deleted={} transient_object_bytes_deleted={} sessions_deleted={}",
                report.temp_files_deleted,
                report.orphan_session_dirs_deleted,
                report.stale_queue_entries_removed,
                report.cache_object_deletions_planned,
                report.cache_object_deletions_succeeded,
                report.cache_object_deletions_deferred,
                report.segments_deleted(),
                report.maps_deleted,
                report.transient_resources_pruned,
                report.transient_objects_deleted,
                report.transient_object_bytes_deleted,
                report.sessions_deleted,
            );
        }
        Ok(report)
    }

    pub(super) async fn ensure_cache_marker(&self, report: &mut GarbageCollectionReport) -> io::Result<bool> {
        let rewrite_secret_fingerprint = self.rewrite_secret_fingerprint.load_full();
        match self.cache.read_rewrite_secret_fingerprint().await? {
            Some(current) if current == *rewrite_secret_fingerprint => Ok(false),
            Some(_) => {
                self.metrics.record_secret_marker_mismatch();
                warn!("HLS rewrite secret changed or cache marker mismatch detected: action=validate-cache-marker");
                match self.cache.invalidate_all_if_no_active_temp_files().await? {
                    CacheInvalidationOutcome::Invalidated => {
                        self.sessions.clear().await;
                        self.cache.write_rewrite_secret_fingerprint(&rewrite_secret_fingerprint).await?;
                        report.secret_cache_invalidated = true;
                        info!("HLS rewrite secret changed or cache marker mismatch detected: action=cache-invalidated");
                        Ok(true)
                    }
                    CacheInvalidationOutcome::DeferredActiveTempFiles => {
                        report.secret_cache_invalidation_deferred = true;
                        self.metrics.record_secret_invalidation_deferred();
                        warn!(
                            "HLS rewrite secret changed or cache marker mismatch detected: action=deferred-active-temp-files"
                        );
                        Ok(true)
                    }
                }
            }
            None => {
                self.cache.write_rewrite_secret_fingerprint(&rewrite_secret_fingerprint).await?;
                Ok(false)
            }
        }
    }

    pub(super) async fn remove_idle_session_if_still_idle(
        &self,
        session: &HlsSessionHandle,
        now_ms: u64,
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
    ) {
        let (key, proxy_session_id) = {
            let mut session = session.write().await;
            if !Self::should_remove_idle_session(&session, now_ms, policy) {
                return;
            }
            session.mark_for_gc_removal();
            (session.key.clone(), session.proxy_session_id.clone())
        };

        if self.cache.has_active_temp_files_for_session(&proxy_session_id) {
            session.write().await.clear_gc_removal_mark();
            return;
        }

        if self
            .sessions
            .remove_session_marking_expired(
                &key,
                &proxy_session_id,
                now_ms,
                HlsExpiredSessionReason::SessionIdleTimeout,
                None,
            )
            .await
            .is_some()
        {
            report.sessions_deleted = report.sessions_deleted.saturating_add(1);
            report.removed_session_ids.push(proxy_session_id.clone());
            if let Err(error) = self.cache.delete_session_dir(&proxy_session_id).await {
                warn!(
                    "HLS idle session cache directory cleanup deferred: session={} error_kind={:?}",
                    safe_proxy_session_id(&proxy_session_id),
                    error.kind(),
                );
            }
        } else {
            session.write().await.clear_gc_removal_mark();
        }
    }

    pub(super) fn reserve_cache_deletion_batch(&self) -> CacheDeletionBatch {
        CacheDeletionBatch::reserve(Arc::clone(&self.pending_cache_deletions))
    }

    pub fn reserve_switch_segment_cleanup(&self, key: SegmentCacheKey) -> Option<HlsSwitchCacheCleanupReservation> {
        HlsSwitchCacheCleanupReservation::reserve(
            Arc::clone(&self.pending_cache_deletions),
            CacheObjectDeletion::UncommittedSwitchSegment(key),
        )
    }

    pub fn reserve_switch_map_cleanup(&self, key: MapCacheKey) -> Option<HlsSwitchCacheCleanupReservation> {
        HlsSwitchCacheCleanupReservation::reserve(
            Arc::clone(&self.pending_cache_deletions),
            CacheObjectDeletion::UncommittedSwitchMap(key),
        )
    }

    pub fn has_pending_switch_cleanup(&self, segment_key: &SegmentCacheKey, map_key: Option<&MapCacheKey>) -> bool {
        let state = self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.iter().any(|pending| match &pending.deletion {
            CacheObjectDeletion::UncommittedSwitchSegment(key) => key == segment_key,
            CacheObjectDeletion::UncommittedSwitchMap(key) => map_key == Some(key),
            CacheObjectDeletion::Segment { .. }
            | CacheObjectDeletion::Map(_)
            | CacheObjectDeletion::TransientObject { .. } => false,
        })
    }

    pub(super) fn pending_cache_deletion_count(&self) -> usize {
        self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pending.len()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn cache_deletion_queue_usage(&self) -> (usize, usize) {
        let state = self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.pending.len(), state.reserved_slots)
    }

    pub(super) async fn retry_pending_cache_deletions(
        &self,
        report: &mut GarbageCollectionReport,
        attempt_budget: &mut usize,
    ) {
        let attempts = self.pending_cache_deletion_count().min(*attempt_budget);
        for _ in 0..attempts {
            let pending = {
                let queue = self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                queue.pending.front().cloned()
            };
            let Some(pending) = pending else {
                break;
            };
            *attempt_budget = attempt_budget.saturating_sub(1);
            let result = pending.deletion.delete_from(&self.cache).await;
            let mut queue = self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(mut completed) = queue.pending.pop_front() else {
                continue;
            };
            if result.is_ok() {
                drop(queue);
                completed.deletion.record_success(report);
                completed.deletion.log_success();
            } else {
                completed.attempts = completed.attempts.saturating_add(1);
                let attempts = completed.attempts;
                let kind = result.as_ref().err().map(io::Error::kind);
                completed.deletion.log_deferred(attempts, kind);
                queue.pending.push_back(completed);
            }
        }
    }

    pub(super) fn record_report_metrics(&self, report: &GarbageCollectionReport) {
        self.metrics.record_gc_run();
        self.metrics.record_segments_removed(report.segments_deleted());
        self.metrics.record_maps_removed(report.maps_deleted);
    }

    pub(super) fn should_remove_idle_session(
        session: &HlsSession,
        now_ms: u64,
        policy: &GarbageCollectionPolicy,
    ) -> bool {
        session.can_expire_idle_session(now_ms, policy.session_idle_timeout_ms)
    }
}

impl HlsCacheCapacityReclaimer for HlsGarbageCollector {
    fn reclaim_capacity(
        &self,
        request: HlsCacheCapacityReclaimRequest,
    ) -> BoxFuture<'_, io::Result<HlsCacheCapacityReclaimOutcome>> {
        async move { self.reclaim_for_projected_write(request).await }.boxed()
    }
}

pub fn build_rewrite_secret_fingerprint(rewrite_secret: &[u8]) -> String {
    let digest = Sha256::digest(rewrite_secret);
    let value = digest.iter().take(8).fold(0_u64, |value, byte| (value << 8) | u64::from(*byte));
    format!("{value:016x}")
}

pub fn exec_hls_cache_gc(ctx: &HlsCtx, cancel_token: &CancellationToken) {
    let hls_proxy = Arc::clone(&ctx.hls_proxy);
    let active_users = Arc::clone(&ctx.active_users);
    let active_provider = Arc::clone(&ctx.active_provider);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        loop {
            let now_ms = chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default();
            hls_proxy
                .sync_all_session_access_leases_and_detach_if_needed(&active_users, &active_provider, now_ms)
                .await;
            match hls_proxy.run_garbage_collection_once(now_ms).await {
                Ok(report) if report.did_cleanup_or_invalidate() => {
                    debug!(
                        "HLS cache state snapshot after garbage collection: {}",
                        hls_proxy.debug_state_summary().await
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    error!("HLS cache garbage collection failed: {err}");
                }
            }
            tokio::select! {
                () = cancel_token.cancelled() => break,
                () = tokio::time::sleep(HLS_CACHE_GC_INTERVAL) => {}
            }
        }
    });
}
