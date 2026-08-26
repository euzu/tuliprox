use super::{
    hls_ctx::HlsCtx, renderer_candidate_window_proxy_seqs, safe_proxy_session_id,
    transient::extract_transient_resource_ids, CacheInvalidationOutcome, HlsAccessLeaseStore,
    HlsCacheCapacityReclaimOutcome, HlsCacheCapacityReclaimRequest, HlsCacheCapacityReclaimer, HlsCacheMetrics,
    HlsExpiredSessionReason, HlsSegmentCache, HlsSession, HlsSessionHandle, HlsSessionStore, MapCacheKey,
    MapCacheStatus, ProxyMapId, ProxySessionId, SegmentCacheKey, SegmentCacheStatus, TransientObjectCacheKey,
    TransientResourceId,
};
use arc_swap::ArcSwap;
use futures::{future::BoxFuture, FutureExt};
use log::{debug, error, info, warn};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    io,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock, Weak},
    time::{Duration, SystemTime},
};
use tokio::sync::{Mutex as AsyncMutex, RwLock};
use tokio_util::sync::CancellationToken;
use tuliprox_core::model::HlsCacheConfig;

const HLS_CACHE_GC_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_TEMP_FILE_RETENTION_MS: u64 = 30_000;
const DEFAULT_FAILED_SEGMENT_RETENTION_MS: u64 = 10_000;
const MAX_PENDING_CACHE_DELETIONS: usize = 1_024;
const MAX_CACHE_DELETE_RETRIES_PER_RUN: usize = 128;
// A switch can own one segment and one MAP rollback. Keep capacity for 64 concurrent handoffs even while a GC batch
// is selecting ordinary deletions; the shared hard bound remains MAX_PENDING_CACHE_DELETIONS.
const SWITCH_CACHE_CLEANUP_HEADROOM: usize = 128;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GarbageCollectionPolicy {
    pub cache_duration_ms: u64,
    pub cache_bytes_global: u64,
    pub cache_bytes_per_session: u64,
    pub session_idle_timeout_ms: u64,
    pub temp_file_retention_ms: u64,
    pub failed_segment_retention_ms: u64,
}

impl GarbageCollectionPolicy {
    pub fn from_config(config: &HlsCacheConfig) -> Self {
        Self {
            cache_duration_ms: config.cache_duration.saturating_mul(1_000),
            cache_bytes_global: config.cache_bytes,
            cache_bytes_per_session: config.cache_bytes_per_session,
            session_idle_timeout_ms: config.session_idle_timeout.saturating_mul(1_000),
            temp_file_retention_ms: DEFAULT_TEMP_FILE_RETENTION_MS,
            failed_segment_retention_ms: DEFAULT_FAILED_SEGMENT_RETENTION_MS,
        }
    }
}

impl Default for GarbageCollectionPolicy {
    fn default() -> Self {
        let default_config = HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default());
        Self::from_config(&default_config)
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct ProtectedSet {
    pub segment_proxy_seqs: HashSet<u64>,
    pub map_ids: HashSet<ProxyMapId>,
    pub key_resource_ids: HashSet<TransientResourceId>,
}

impl ProtectedSet {
    pub fn from_session(session: &HlsSession) -> Self { Self::from_session_for_capacity(session, None) }

    fn from_session_for_capacity(session: &HlsSession, release_through: Option<u64>) -> Self {
        let mut protected = Self::default();

        if let Some(rendered) = &session.last_rendered_manifest {
            protected.segment_proxy_seqs.extend(
                rendered
                    .segment_proxy_seqs
                    .iter()
                    .copied()
                    .filter(|proxy_seq| release_through.is_none_or(|release| *proxy_seq > release)),
            );
        }
        if let Some(rendered) = &session.last_rendered_manifest {
            protected.key_resource_ids.extend(extract_transient_resource_ids(&rendered.body));
        }
        protected.key_resource_ids.extend(session.transient.protected_manifest_resource_ids());
        for (_, protection) in session.terminal_tail_protections() {
            protected.segment_proxy_seqs.extend(protection.base_proxy_seqs.iter().copied());
        }
        protected.segment_proxy_seqs.extend(
            renderer_candidate_window_proxy_seqs(session)
                .into_iter()
                .filter(|proxy_seq| release_through.is_none_or(|release| *proxy_seq > release)),
        );

        for (proxy_seq, entry) in &session.segments {
            if entry.access.active_readers() > 0
                || matches!(entry.status, SegmentCacheStatus::Fetching { .. })
                || (entry.origin_fetch_ref.is_some()
                    && (matches!(entry.status, SegmentCacheStatus::Queued { .. })
                        || session.segment_prefetch_queue.contains(*proxy_seq)))
            {
                protected.segment_proxy_seqs.insert(*proxy_seq);
            }
        }

        for proxy_seq in &protected.segment_proxy_seqs {
            if let Some(segment) = session.segments.get(proxy_seq) {
                if let Some(map_ref) = segment.map_ref {
                    protected.map_ids.insert(map_ref);
                }
                if let Some(encryption) = &segment.encryption {
                    protected.key_resource_ids.insert(encryption.resource_id.clone());
                }
            }
        }
        for (map_id, map) in &session.maps {
            if map.access.active_readers() > 0
                || matches!(map.status, MapCacheStatus::Queued { .. } | MapCacheStatus::Fetching { .. })
            {
                protected.map_ids.insert(*map_id);
            }
        }

        protected
    }
}

fn protected_set_for_cache_reclamation(
    session: &HlsSession,
    cache: &HlsSegmentCache,
    excluded_path: Option<&std::path::Path>,
    release_through: Option<u64>,
) -> ProtectedSet {
    let mut protected = ProtectedSet::from_session_for_capacity(session, release_through);
    for (proxy_seq, segment) in &session.segments {
        let path = cache.object_path(&segment.cache_key);
        if cache.has_active_mutation(&path) || excluded_path == Some(path.as_path()) {
            protected.segment_proxy_seqs.insert(*proxy_seq);
        }
    }
    for (map_id, map) in &session.maps {
        let path = cache.object_path(&map.cache_key);
        if cache.has_active_mutation(&path) || excluded_path == Some(path.as_path()) {
            protected.map_ids.insert(*map_id);
        }
    }
    for entry in session.transient.object_cache.values() {
        let path = cache.object_path(&entry.key);
        if cache.has_active_mutation(&path) || excluded_path == Some(path.as_path()) {
            protected.key_resource_ids.insert(entry.key.transient_resource_id().clone());
        }
    }
    protected
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct GarbageCollectionReport {
    pub secret_cache_invalidated: bool,
    pub secret_cache_invalidation_deferred: bool,
    pub temp_files_deleted: usize,
    pub orphan_session_dirs_deleted: usize,
    pub stale_queue_entries_removed: usize,
    pub segments_deleted_duration: usize,
    pub segments_deleted_size_session: usize,
    pub segments_deleted_size_global: usize,
    pub maps_deleted: usize,
    pub sessions_deleted: usize,
    pub removed_session_ids: Vec<ProxySessionId>,
    pub transient_resources_pruned: usize,
    pub transient_objects_deleted: usize,
    pub transient_object_bytes_deleted: u64,
    pub cache_object_deletions_planned: usize,
    pub cache_object_deletions_succeeded: usize,
    pub cache_object_deletions_deferred: usize,
}

impl GarbageCollectionReport {
    fn segments_deleted(&self) -> usize {
        self.segments_deleted_duration
            .saturating_add(self.segments_deleted_size_session)
            .saturating_add(self.segments_deleted_size_global)
    }

    pub fn did_cleanup_or_invalidate(&self) -> bool {
        self.secret_cache_invalidated
            || self.secret_cache_invalidation_deferred
            || self.temp_files_deleted > 0
            || self.orphan_session_dirs_deleted > 0
            || self.stale_queue_entries_removed > 0
            || self.segments_deleted() > 0
            || self.maps_deleted > 0
            || self.sessions_deleted > 0
            || self.transient_resources_pruned > 0
            || self.transient_objects_deleted > 0
            || self.cache_object_deletions_planned > 0
            || self.cache_object_deletions_succeeded > 0
            || self.cache_object_deletions_deferred > 0
    }
}

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

    #[allow(clippy::too_many_lines)]
    async fn reclaim_for_projected_write(
        &self,
        request: HlsCacheCapacityReclaimRequest,
    ) -> io::Result<HlsCacheCapacityReclaimOutcome> {
        let _run_once = self.run_once_gate.lock().await;
        if !self.cache.contains_current_cache_path(&request.target_path) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "hls cache path changed before capacity reclamation",
            ));
        }
        let before = self.cache.capacity_usage(&request.proxy_session_id).await?;
        let mut report = GarbageCollectionReport::default();
        let mut deletion_attempt_budget = MAX_CACHE_DELETE_RETRIES_PER_RUN;
        let mut protected_working_set_bytes = 0_u64;
        let mut reclaimable_bytes = 0_u64;

        let mut pending_attempt_budget = deletion_attempt_budget / 2;
        let pending_attempts_before = pending_attempt_budget;
        self.retry_pending_cache_deletions(&mut report, &mut pending_attempt_budget).await;
        deletion_attempt_budget =
            deletion_attempt_budget.saturating_sub(pending_attempts_before.saturating_sub(pending_attempt_budget));
        let after_pending = self.cache.capacity_usage(&request.proxy_session_id).await?;
        let required_session_bytes = request
            .required_session_bytes
            .saturating_sub(before.session_bytes.saturating_sub(after_pending.session_bytes));
        if required_session_bytes > 0 {
            if let Some(session) = self.sessions.get_by_proxy_session_id(&request.proxy_session_id).await {
                let mut deletions = self.reserve_cache_deletion_batch();
                let access_leases = self
                    .access_leases
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .and_then(Weak::upgrade);
                let lease_guard = match &access_leases {
                    Some(access_leases) => Some(access_leases.read().await),
                    None => None,
                };
                // Projected-capacity selection follows the runtime lock order
                // lease store -> session. The session lock is non-blocking
                // while the lease snapshot is frozen, and neither guard
                // crosses filesystem I/O or an await. Contention installs a
                // bounded wake task instead of reversing the lock order.
                let release_through =
                    lease_guard.as_ref().and_then(|leases| leases.capacity_release_through(&request.proxy_session_id));
                let mut session_lock_busy = false;
                if let Ok(mut session) = session.try_write() {
                    let protected = protected_set_for_cache_reclamation(
                        &session,
                        &self.cache,
                        Some(&request.target_path),
                        release_through,
                    );
                    let evidence = capacity_reclamation_evidence(&session, &protected);
                    protected_working_set_bytes = evidence.protected_working_set_bytes;
                    reclaimable_bytes = evidence.reclaimable_bytes;
                    collect_projected_session_reclamation(
                        &mut session,
                        &self.cache,
                        &request.target_path,
                        release_through,
                        required_session_bytes,
                        deletion_attempt_budget,
                        &mut deletions,
                    );
                    if !deletions.deletions.is_empty() {
                        let rendered_at_ms = session
                            .last_rendered_manifest
                            .as_ref()
                            .map_or(session.last_client_access_at_ms, |rendered| {
                                rendered.rendered_at_ms.saturating_add(1)
                            });
                        let _ = session.render_and_store_manifest(rendered_at_ms);
                    }
                } else {
                    protected_working_set_bytes = after_pending.session_bytes;
                    session_lock_busy = true;
                }
                drop(lease_guard);
                if session_lock_busy {
                    let wake_session = Arc::clone(&session);
                    let wake_cache = Arc::clone(&self.cache);
                    tokio::spawn(async move {
                        let guard = wake_session.write().await;
                        drop(guard);
                        wake_cache.notify_capacity_protection_changed();
                    });
                }
                deletions.execute_prioritized(&self.cache, &mut report, &mut deletion_attempt_budget).await;
            }
        }

        let after_session = self.cache.capacity_usage(&request.proxy_session_id).await?;
        let remaining_session_bytes = request
            .required_session_bytes
            .saturating_sub(before.session_bytes.saturating_sub(after_session.session_bytes));
        if remaining_session_bytes > 0 {
            return Ok(HlsCacheCapacityReclaimOutcome {
                reclaimed_session_bytes: before.session_bytes.saturating_sub(after_session.session_bytes),
                reclaimed_global_bytes: before.global_bytes.saturating_sub(after_session.global_bytes),
                protected_working_set_bytes,
                reclaimable_bytes,
            });
        }
        let required_global_bytes = request
            .required_global_bytes
            .saturating_sub(before.global_bytes.saturating_sub(after_session.global_bytes));
        if required_global_bytes > 0 {
            let sessions = self.sessions.list_sessions().await;
            let mut deletions = self.reserve_cache_deletion_batch();
            self.collect_projected_global_reclamation(
                &sessions,
                &request.target_path,
                required_global_bytes,
                deletion_attempt_budget,
                &mut deletions,
            )
            .await;
            deletions.execute_prioritized(&self.cache, &mut report, &mut deletion_attempt_budget).await;
        }

        let after = self.cache.capacity_usage(&request.proxy_session_id).await?;
        Ok(HlsCacheCapacityReclaimOutcome {
            reclaimed_session_bytes: before.session_bytes.saturating_sub(after.session_bytes),
            reclaimed_global_bytes: before.global_bytes.saturating_sub(after.global_bytes),
            protected_working_set_bytes,
            reclaimable_bytes,
        })
    }

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

    async fn ensure_cache_marker(&self, report: &mut GarbageCollectionReport) -> io::Result<bool> {
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

    fn collect_session_deletions(
        session: &mut HlsSession,
        cache: &HlsSegmentCache,
        now_ms: u64,
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
        deletions: &mut CacheDeletionBatch,
    ) {
        report.stale_queue_entries_removed =
            report.stale_queue_entries_removed.saturating_add(remove_stale_queue_entries(session));

        let transient_before = session.transient.resources.len();
        let protected = protected_set_for_cache_reclamation(session, cache, None, None);
        session.transient.prune_expired_except(now_ms, &protected.key_resource_ids);
        let transient_resources_pruned = transient_before.saturating_sub(session.transient.resources.len());
        let mut transient_readiness_changed = transient_resources_pruned > 0;
        report.transient_resources_pruned =
            report.transient_resources_pruned.saturating_add(transient_resources_pruned);

        let expired_transient_objects = take_expired_transient_objects(
            session,
            now_ms,
            &protected.key_resource_ids,
            deletions.remaining_capacity(),
        );
        transient_readiness_changed |= !expired_transient_objects.is_empty();
        for (key, content_length) in expired_transient_objects {
            deletions.push(CacheObjectDeletion::TransientObject { key, content_length });
        }

        while deletions.has_capacity() {
            let Some(proxy_seq) = duration_expired_head_segment(
                session,
                &protected_set_for_cache_reclamation(session, cache, None, None),
                policy,
                now_ms,
            ) else {
                break;
            };
            if let Some(deletion) = remove_segment_entry(session, proxy_seq) {
                deletions
                    .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::Duration });
            }
        }

        let mut session_size = session_cache_size(session);
        while session_size > policy.cache_bytes_per_session && deletions.has_capacity() {
            let protected = protected_set_for_cache_reclamation(session, cache, None, None);
            let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.key_resource_ids) else {
                break;
            };
            session_size = session_size.saturating_sub(removal.content_length);
            transient_readiness_changed = true;
            deletions.push(CacheObjectDeletion::TransientObject {
                key: removal.key,
                content_length: removal.content_length,
            });
        }
        while session_size > policy.cache_bytes_per_session && deletions.has_capacity() {
            let Some(candidate) =
                fifo_head_size_candidate(session, &protected_set_for_cache_reclamation(session, cache, None, None))
            else {
                break;
            };
            session_size = session_size.saturating_sub(candidate.content_length);
            if let Some(deletion) = remove_segment_entry(session, candidate.proxy_seq) {
                deletions.push(CacheObjectDeletion::Segment {
                    key: deletion,
                    reason: SegmentCacheDeletionReason::SessionSize,
                });
            }
        }

        for map_id in
            unprotected_unreferenced_map_ids(session, &protected_set_for_cache_reclamation(session, cache, None, None))
        {
            if !deletions.has_capacity() {
                break;
            }
            if let Some(deletion) = remove_map_entry(session, map_id) {
                session_size = session_size.saturating_sub(deletion.content_length);
                deletions.push(CacheObjectDeletion::Map(deletion.key));
            }
        }
        if transient_readiness_changed {
            session.advance_media_readiness_generation();
        }
    }

    async fn collect_global_size_deletions(
        &self,
        sessions: &[HlsSessionHandle],
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
        deletions: &mut CacheDeletionBatch,
    ) {
        let mut total_size = total_sessions_cache_size(sessions).await;
        let mut transient_readiness_advanced = HashSet::new();
        let mut skipped_transient_sessions = HashSet::new();

        loop {
            if total_size <= policy.cache_bytes_global || !deletions.has_capacity() {
                break;
            }
            let Some(candidate) =
                oldest_global_transient_object_candidate(sessions, &self.cache, None, &skipped_transient_sessions)
                    .await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let protected = protected_set_for_cache_reclamation(&session, &self.cache, None, None);
            let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.key_resource_ids) else {
                skipped_transient_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            total_size = total_size.saturating_sub(removal.content_length);
            if transient_readiness_advanced.insert(session.proxy_session_id.clone()) {
                session.advance_media_readiness_generation();
            }
            deletions.push(CacheObjectDeletion::TransientObject {
                key: removal.key,
                content_length: removal.content_length,
            });
            deletions.persist_pending(report);
        }

        let mut skipped_segment_sessions = HashSet::new();
        loop {
            if total_size <= policy.cache_bytes_global || !deletions.has_capacity() {
                break;
            }
            let Some(candidate) =
                oldest_global_fifo_head_candidate(sessions, &self.cache, None, &skipped_segment_sessions).await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let Some(current_head) = fifo_head_size_candidate(
                &session,
                &protected_set_for_cache_reclamation(&session, &self.cache, None, None),
            ) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            if current_head.proxy_seq != candidate.proxy_seq {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            }
            let Some(deletion) = remove_segment_entry(&mut session, candidate.proxy_seq) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            total_size = total_size.saturating_sub(candidate.content_length);
            deletions
                .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::GlobalSize });
            deletions.persist_pending(report);

            for map_id in unprotected_unreferenced_map_ids(
                &session,
                &protected_set_for_cache_reclamation(&session, &self.cache, None, None),
            ) {
                if !deletions.has_capacity() {
                    break;
                }
                if let Some(deletion) = remove_map_entry(&mut session, map_id) {
                    total_size = total_size.saturating_sub(deletion.content_length);
                    deletions.push(CacheObjectDeletion::Map(deletion.key));
                    deletions.persist_pending(report);
                }
            }
        }
    }

    async fn collect_projected_global_reclamation(
        &self,
        sessions: &[HlsSessionHandle],
        excluded_path: &std::path::Path,
        required_bytes: u64,
        max_deletions: usize,
        deletions: &mut CacheDeletionBatch,
    ) {
        let mut planned_bytes = 0_u64;
        let mut planned_deletions = 0_usize;
        let mut transient_readiness_advanced = HashSet::new();
        let mut skipped_transient_sessions = HashSet::new();
        while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
            let Some(candidate) = oldest_global_transient_object_candidate(
                sessions,
                &self.cache,
                Some(excluded_path),
                &skipped_transient_sessions,
            )
            .await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let protected = protected_set_for_cache_reclamation(&session, &self.cache, Some(excluded_path), None);
            let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.key_resource_ids) else {
                skipped_transient_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            planned_bytes = planned_bytes.saturating_add(removal.content_length);
            if transient_readiness_advanced.insert(session.proxy_session_id.clone()) {
                session.advance_media_readiness_generation();
            }
            deletions.push(CacheObjectDeletion::TransientObject {
                key: removal.key,
                content_length: removal.content_length,
            });
            planned_deletions = planned_deletions.saturating_add(1);
        }

        let mut skipped_segment_sessions = HashSet::new();
        while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
            let Some(candidate) = oldest_global_fifo_head_candidate(
                sessions,
                &self.cache,
                Some(excluded_path),
                &skipped_segment_sessions,
            )
            .await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let protected = protected_set_for_cache_reclamation(&session, &self.cache, Some(excluded_path), None);
            let Some(current_head) = fifo_head_size_candidate(&session, &protected) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            if current_head.proxy_seq != candidate.proxy_seq {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            }
            let Some(deletion) = remove_segment_entry(&mut session, candidate.proxy_seq) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            planned_bytes = planned_bytes.saturating_add(candidate.content_length);
            deletions
                .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::GlobalSize });
            planned_deletions = planned_deletions.saturating_add(1);

            for map_id in unprotected_unreferenced_map_ids(
                &session,
                &protected_set_for_cache_reclamation(&session, &self.cache, Some(excluded_path), None),
            ) {
                if planned_bytes >= required_bytes || planned_deletions >= max_deletions || !deletions.has_capacity() {
                    break;
                }
                if let Some(deletion) = remove_map_entry(&mut session, map_id) {
                    planned_bytes = planned_bytes.saturating_add(deletion.content_length);
                    deletions.push(CacheObjectDeletion::Map(deletion.key));
                    planned_deletions = planned_deletions.saturating_add(1);
                }
            }
        }
    }

    async fn remove_idle_session_if_still_idle(
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

    fn reserve_cache_deletion_batch(&self) -> CacheDeletionBatch {
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

    fn pending_cache_deletion_count(&self) -> usize {
        self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pending.len()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn cache_deletion_queue_usage(&self) -> (usize, usize) {
        let state = self.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.pending.len(), state.reserved_slots)
    }

    async fn retry_pending_cache_deletions(&self, report: &mut GarbageCollectionReport, attempt_budget: &mut usize) {
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

    fn record_report_metrics(&self, report: &GarbageCollectionReport) {
        self.metrics.record_gc_run();
        self.metrics.record_segments_removed(report.segments_deleted());
        self.metrics.record_maps_removed(report.maps_deleted);
    }

    fn should_remove_idle_session(session: &HlsSession, now_ms: u64, policy: &GarbageCollectionPolicy) -> bool {
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

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SegmentCacheDeletionReason {
    Duration,
    SessionSize,
    GlobalSize,
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum CacheObjectDeletion {
    Segment { key: SegmentCacheKey, reason: SegmentCacheDeletionReason },
    Map(MapCacheKey),
    TransientObject { key: TransientObjectCacheKey, content_length: u64 },
    UncommittedSwitchSegment(SegmentCacheKey),
    UncommittedSwitchMap(MapCacheKey),
}

impl CacheObjectDeletion {
    async fn delete_from(&self, cache: &HlsSegmentCache) -> io::Result<()> {
        match self {
            Self::Segment { key, .. } | Self::UncommittedSwitchSegment(key) => cache.delete_if_inactive(key).await,
            Self::Map(key) | Self::UncommittedSwitchMap(key) => cache.delete_if_inactive(key).await,
            Self::TransientObject { key, .. } => cache.delete_if_inactive(key).await,
        }
    }

    fn record_success(&self, report: &mut GarbageCollectionReport) {
        report.cache_object_deletions_succeeded = report.cache_object_deletions_succeeded.saturating_add(1);
        match self {
            Self::Segment { reason: SegmentCacheDeletionReason::Duration, .. } => {
                report.segments_deleted_duration = report.segments_deleted_duration.saturating_add(1);
            }
            Self::Segment { reason: SegmentCacheDeletionReason::SessionSize, .. } => {
                report.segments_deleted_size_session = report.segments_deleted_size_session.saturating_add(1);
            }
            Self::Segment { reason: SegmentCacheDeletionReason::GlobalSize, .. } => {
                report.segments_deleted_size_global = report.segments_deleted_size_global.saturating_add(1);
            }
            Self::Map(_) => {
                report.maps_deleted = report.maps_deleted.saturating_add(1);
            }
            Self::TransientObject { content_length, .. } => {
                report.transient_objects_deleted = report.transient_objects_deleted.saturating_add(1);
                report.transient_object_bytes_deleted =
                    report.transient_object_bytes_deleted.saturating_add(*content_length);
            }
            Self::UncommittedSwitchSegment(_) | Self::UncommittedSwitchMap(_) => {}
        }
    }

    fn log_success(&self) {
        match self {
            Self::Segment { key, .. } => {
                info!(
                    "Segment '{:06}' removed: session={} source=normal",
                    key.proxy_seq(),
                    safe_proxy_session_id(key.proxy_session_id()),
                );
            }
            Self::UncommittedSwitchSegment(_) | Self::UncommittedSwitchMap(_) => {
                debug!("HLS uncommitted switch cache object removed");
            }
            Self::Map(_) | Self::TransientObject { .. } => {}
        }
    }

    fn log_deferred(&self, attempts: u16, error_kind: Option<io::ErrorKind>) {
        let object_kind = match self {
            Self::Segment { .. } => "segment",
            Self::Map(_) => "map",
            Self::TransientObject { .. } => "transient-object",
            Self::UncommittedSwitchSegment(_) => "uncommitted-switch-segment",
            Self::UncommittedSwitchMap(_) => "uncommitted-switch-map",
        };
        warn!(
            "HLS cache object deletion deferred: object_kind={object_kind} attempts={attempts} error_kind={error_kind:?}"
        );
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct PendingCacheObjectDeletion {
    deletion: CacheObjectDeletion,
    attempts: u16,
}

struct CacheDeletionQueueGeneration;

struct CacheDeletionQueueState {
    pending: VecDeque<PendingCacheObjectDeletion>,
    reserved_slots: usize,
    generation: Arc<CacheDeletionQueueGeneration>,
}

impl Default for CacheDeletionQueueState {
    fn default() -> Self {
        Self { pending: VecDeque::new(), reserved_slots: 0, generation: Arc::new(CacheDeletionQueueGeneration) }
    }
}

/// Bounded rollback ownership for one physically committed but not yet timeline-committed switch object.
pub struct HlsSwitchCacheCleanupReservation {
    queue: Arc<StdMutex<CacheDeletionQueueState>>,
    queue_generation: Arc<CacheDeletionQueueGeneration>,
    deletion: Option<CacheObjectDeletion>,
    slot_reserved: bool,
}

impl HlsSwitchCacheCleanupReservation {
    fn reserve(queue: Arc<StdMutex<CacheDeletionQueueState>>, deletion: CacheObjectDeletion) -> Option<Self> {
        let queue_generation = {
            let mut state = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.pending.len().saturating_add(state.reserved_slots) >= MAX_PENDING_CACHE_DELETIONS {
                return None;
            }
            state.reserved_slots = state.reserved_slots.saturating_add(1);
            Arc::clone(&state.generation)
        };
        Some(Self { queue, queue_generation, deletion: Some(deletion), slot_reserved: true })
    }

    pub fn disarm(&mut self) {
        self.deletion = None;
        self.release_slot();
    }

    fn release_slot(&mut self) {
        if !self.slot_reserved {
            return;
        }
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::ptr_eq(&state.generation, &self.queue_generation) {
            state.reserved_slots = state.reserved_slots.saturating_sub(1);
        }
        self.slot_reserved = false;
    }
}

impl std::fmt::Debug for HlsSwitchCacheCleanupReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HlsSwitchCacheCleanupReservation")
            .field("armed", &self.deletion.is_some())
            .field("slot_reserved", &self.slot_reserved)
            .finish_non_exhaustive()
    }
}

impl Drop for HlsSwitchCacheCleanupReservation {
    fn drop(&mut self) {
        if !self.slot_reserved {
            return;
        }
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::ptr_eq(&state.generation, &self.queue_generation) {
            state.reserved_slots = state.reserved_slots.saturating_sub(1);
            if let Some(deletion) = self.deletion.take() {
                state.pending.push_back(PendingCacheObjectDeletion { deletion, attempts: 0 });
            }
        }
        self.slot_reserved = false;
    }
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

impl CacheDeletionBatch {
    fn reserve(queue: Arc<StdMutex<CacheDeletionQueueState>>) -> Self {
        let reserved_slots = {
            let mut state = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let batch_limit = MAX_PENDING_CACHE_DELETIONS.saturating_sub(SWITCH_CACHE_CLEANUP_HEADROOM);
            let available = batch_limit.saturating_sub(state.pending.len()).saturating_sub(state.reserved_slots);
            state.reserved_slots = state.reserved_slots.saturating_add(available);
            available
        };
        Self { queue, reserved_slots, deletions: Vec::new() }
    }

    fn has_capacity(&self) -> bool { self.deletions.len() < self.reserved_slots }

    fn remaining_capacity(&self) -> usize { self.reserved_slots.saturating_sub(self.deletions.len()) }

    fn push(&mut self, deletion: CacheObjectDeletion) { self.deletions.push(deletion); }

    fn persist_pending(&mut self, report: &mut GarbageCollectionReport) {
        let planned = self.deletions.len();
        if planned == 0 {
            return;
        }
        {
            let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.reserved_slots = state.reserved_slots.saturating_sub(planned);
            state
                .pending
                .extend(self.deletions.drain(..).map(|deletion| PendingCacheObjectDeletion { deletion, attempts: 0 }));
        }
        self.reserved_slots = self.reserved_slots.saturating_sub(planned);
        report.cache_object_deletions_planned = report.cache_object_deletions_planned.saturating_add(planned);
    }

    fn persist(mut self, report: &mut GarbageCollectionReport) { self.persist_pending(report); }

    async fn execute_prioritized(
        mut self,
        cache: &HlsSegmentCache,
        report: &mut GarbageCollectionReport,
        attempt_budget: &mut usize,
    ) {
        while *attempt_budget > 0 {
            let Some(deletion) = self.deletions.first().cloned() else {
                break;
            };
            *attempt_budget = attempt_budget.saturating_sub(1);
            let result = deletion.delete_from(cache).await;
            let completed = self.deletions.remove(0);
            self.release_reserved_slot();
            report.cache_object_deletions_planned = report.cache_object_deletions_planned.saturating_add(1);
            match result {
                Ok(()) => {
                    completed.record_success(report);
                    completed.log_success();
                }
                Err(error) => {
                    completed.log_deferred(1, Some(error.kind()));
                    self.persist_failed(completed);
                }
            }
        }
        self.persist_pending(report);
    }

    fn release_reserved_slot(&mut self) {
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reserved_slots = state.reserved_slots.saturating_sub(1);
        self.reserved_slots = self.reserved_slots.saturating_sub(1);
    }

    fn persist_failed(&mut self, deletion: CacheObjectDeletion) {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .push_back(PendingCacheObjectDeletion { deletion, attempts: 1 });
    }
}

impl Drop for CacheDeletionBatch {
    fn drop(&mut self) {
        if self.reserved_slots == 0 && self.deletions.is_empty() {
            return;
        }
        let mut state = self.queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reserved_slots = state.reserved_slots.saturating_sub(self.reserved_slots);
        state
            .pending
            .extend(self.deletions.drain(..).map(|deletion| PendingCacheObjectDeletion { deletion, attempts: 0 }));
        self.reserved_slots = 0;
    }
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

async fn total_sessions_cache_size(sessions: &[HlsSessionHandle]) -> u64 {
    let mut total_size = 0_u64;
    for session in sessions {
        let session = session.read().await;
        total_size = total_size.saturating_add(session_cache_size(&session));
    }
    total_size
}

fn collect_projected_session_reclamation(
    session: &mut HlsSession,
    cache: &HlsSegmentCache,
    excluded_path: &std::path::Path,
    release_through: Option<u64>,
    required_bytes: u64,
    max_deletions: usize,
    deletions: &mut CacheDeletionBatch,
) {
    let mut planned_bytes = 0_u64;
    let mut planned_deletions = 0_usize;
    let mut transient_readiness_changed = false;
    while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
        let protected = protected_set_for_cache_reclamation(session, cache, Some(excluded_path), release_through);
        let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.key_resource_ids) else {
            break;
        };
        planned_bytes = planned_bytes.saturating_add(removal.content_length);
        transient_readiness_changed = true;
        deletions
            .push(CacheObjectDeletion::TransientObject { key: removal.key, content_length: removal.content_length });
        planned_deletions = planned_deletions.saturating_add(1);
    }
    while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
        let protected = protected_set_for_cache_reclamation(session, cache, Some(excluded_path), release_through);
        let Some(candidate) = fifo_head_size_candidate(session, &protected) else {
            break;
        };
        if let Some(deletion) = remove_segment_entry(session, candidate.proxy_seq) {
            planned_bytes = planned_bytes.saturating_add(candidate.content_length);
            deletions
                .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::SessionSize });
            planned_deletions = planned_deletions.saturating_add(1);
        }
    }
    for map_id in unprotected_unreferenced_map_ids(
        session,
        &protected_set_for_cache_reclamation(session, cache, Some(excluded_path), release_through),
    ) {
        if planned_bytes >= required_bytes || planned_deletions >= max_deletions || !deletions.has_capacity() {
            break;
        }
        if let Some(deletion) = remove_map_entry(session, map_id) {
            planned_bytes = planned_bytes.saturating_add(deletion.content_length);
            deletions.push(CacheObjectDeletion::Map(deletion.key));
            planned_deletions = planned_deletions.saturating_add(1);
        }
    }
    if transient_readiness_changed {
        session.advance_media_readiness_generation();
    }
}

async fn oldest_global_fifo_head_candidate(
    sessions: &[HlsSessionHandle],
    cache: &HlsSegmentCache,
    excluded_path: Option<&std::path::Path>,
    skipped_sessions: &HashSet<ProxySessionId>,
) -> Option<GlobalSegmentCandidate> {
    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if skipped_sessions.contains(&session_guard.proxy_session_id) {
            continue;
        }
        if let Some(candidate) = fifo_head_size_candidate(
            &session_guard,
            &protected_set_for_cache_reclamation(&session_guard, cache, excluded_path, None),
        ) {
            candidates.push(GlobalSegmentCandidate {
                session: Arc::clone(session),
                proxy_session_id: session_guard.proxy_session_id.clone(),
                proxy_seq: candidate.proxy_seq,
                content_length: candidate.content_length,
                last_relevant_at_ms: candidate.last_relevant_at_ms,
            });
        }
    }
    candidates.into_iter().min_by_key(|candidate| (candidate.last_relevant_at_ms, candidate.proxy_seq))
}

async fn oldest_global_transient_object_candidate(
    sessions: &[HlsSessionHandle],
    cache: &HlsSegmentCache,
    excluded_path: Option<&std::path::Path>,
    skipped_sessions: &HashSet<ProxySessionId>,
) -> Option<GlobalTransientObjectCandidate> {
    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if skipped_sessions.contains(&session_guard.proxy_session_id) {
            continue;
        }
        let protected = protected_set_for_cache_reclamation(&session_guard, cache, excluded_path, None);
        candidates.extend(session_guard.transient.object_cache.values().filter_map(|entry| {
            entry.ready_content_length()?;
            if entry.access.active_readers() > 0
                || protected.key_resource_ids.contains(entry.key.transient_resource_id())
            {
                return None;
            }
            Some(GlobalTransientObjectCandidate {
                session: Arc::clone(session),
                proxy_session_id: session_guard.proxy_session_id.clone(),
                last_accessed_at_ms: entry.last_accessed_at_ms,
            })
        }));
    }
    candidates.into_iter().min_by_key(|candidate| candidate.last_accessed_at_ms)
}

fn take_expired_transient_objects(
    session: &mut HlsSession,
    now_ms: u64,
    protected: &HashSet<TransientResourceId>,
    limit: usize,
) -> Vec<(TransientObjectCacheKey, u64)> {
    session
        .transient
        .take_expired_object_removals_except(now_ms, protected, limit)
        .into_iter()
        .map(|removal| (removal.key, removal.content_length))
        .collect()
}

fn remove_stale_queue_entries(session: &mut HlsSession) -> usize {
    let mut removed = 0_usize;
    for proxy_seq in session.segment_prefetch_queue.proxy_seqs() {
        let stale = session.segments.get(&proxy_seq).is_none_or(|segment| {
            segment.origin_fetch_ref.is_none() || !matches!(segment.status, SegmentCacheStatus::Queued { .. })
        });
        if stale && session.segment_prefetch_queue.remove(proxy_seq).is_some() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

fn duration_expired_head_segment(
    session: &HlsSession,
    protected: &ProtectedSet,
    policy: &GarbageCollectionPolicy,
    now_ms: u64,
) -> Option<u64> {
    let (proxy_seq, segment) = session.segments.iter().next()?;
    if protected.segment_proxy_seqs.contains(proxy_seq) {
        return None;
    }
    let last_relevant_at_ms = segment_last_relevant_at_ms(segment)?;
    let retention_ms = match segment.status {
        SegmentCacheStatus::FailedRetryable { .. }
        | SegmentCacheStatus::FailedPermanent { .. }
        | SegmentCacheStatus::Expired => policy.failed_segment_retention_ms,
        SegmentCacheStatus::Ready { .. } => policy
            .cache_duration_ms
            .max(segment.duration_ms.saturating_add(session.longest_rendered_playlist_duration_ms)),
        SegmentCacheStatus::Discovered
        | SegmentCacheStatus::Queued { .. }
        | SegmentCacheStatus::Fetching { .. }
        | SegmentCacheStatus::CapacityDeferred { .. } => return None,
    };
    (now_ms.saturating_sub(last_relevant_at_ms) >= retention_ms).then_some(*proxy_seq)
}

fn fifo_head_size_candidate(session: &HlsSession, protected: &ProtectedSet) -> Option<SegmentDeleteCandidate> {
    let (proxy_seq, segment) = session.segments.iter().next()?;
    if protected.segment_proxy_seqs.contains(proxy_seq) {
        return None;
    }
    let SegmentCacheStatus::Ready { content_length, .. } = segment.status else {
        return None;
    };
    Some(SegmentDeleteCandidate {
        proxy_seq: *proxy_seq,
        content_length,
        last_relevant_at_ms: segment_last_relevant_at_ms(segment).unwrap_or_default(),
    })
}

fn segment_last_relevant_at_ms(segment: &super::SegmentEntry) -> Option<u64> {
    let status_at = match segment.status {
        SegmentCacheStatus::Ready { ready_at_ms, .. } => Some(ready_at_ms),
        SegmentCacheStatus::FailedRetryable { failed_at_ms, .. }
        | SegmentCacheStatus::FailedPermanent { failed_at_ms, .. } => Some(failed_at_ms),
        SegmentCacheStatus::Expired => segment.last_rendered_at_ms,
        SegmentCacheStatus::Discovered
        | SegmentCacheStatus::Queued { .. }
        | SegmentCacheStatus::Fetching { .. }
        | SegmentCacheStatus::CapacityDeferred { .. } => None,
    };
    [status_at, segment.last_rendered_at_ms, Some(segment.access.last_accessed_at_ms()).filter(|value| *value > 0)]
        .into_iter()
        .flatten()
        .max()
}

fn session_cache_size(session: &HlsSession) -> u64 {
    let segment_bytes = session
        .segments
        .values()
        .map(|segment| match segment.status {
            SegmentCacheStatus::Ready { content_length, .. } => content_length,
            _ => 0,
        })
        .sum::<u64>();
    let map_bytes = session
        .maps
        .values()
        .map(|map| match map.status {
            MapCacheStatus::Ready { content_length, .. } => content_length,
            _ => 0,
        })
        .sum::<u64>();
    segment_bytes.saturating_add(map_bytes).saturating_add(session.transient.ready_object_cache_size())
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct CapacityReclamationEvidence {
    protected_working_set_bytes: u64,
    reclaimable_bytes: u64,
}

fn capacity_reclamation_evidence(session: &HlsSession, protected: &ProtectedSet) -> CapacityReclamationEvidence {
    let protected_segment_bytes = session.segments.iter().fold(0_u64, |bytes, (proxy_seq, segment)| {
        if !protected.segment_proxy_seqs.contains(proxy_seq) {
            return bytes;
        }
        match segment.status {
            SegmentCacheStatus::Ready { content_length, .. } => bytes.saturating_add(content_length),
            _ => bytes,
        }
    });
    let protected_map_bytes = session.maps.iter().fold(0_u64, |bytes, (map_id, map)| {
        if !protected.map_ids.contains(map_id) {
            return bytes;
        }
        match map.status {
            MapCacheStatus::Ready { content_length, .. } => bytes.saturating_add(content_length),
            _ => bytes,
        }
    });
    let reclaimable_bytes =
        fifo_head_size_candidate(session, protected).map_or(0, |candidate| candidate.content_length);
    CapacityReclamationEvidence {
        protected_working_set_bytes: protected_segment_bytes
            .saturating_add(protected_map_bytes)
            // Treat transient media conservatively: referenced keys are tiny,
            // and over-reporting protection is safer than claiming bytes can
            // be reclaimed while a lease still names them.
            .saturating_add(session.transient.ready_object_cache_size()),
        reclaimable_bytes,
    }
}

fn remove_segment_entry(session: &mut HlsSession, proxy_seq: u64) -> Option<SegmentCacheKey> {
    let segment = session.segments.remove(&proxy_seq)?;
    let removed_ready_media = matches!(segment.status, SegmentCacheStatus::Ready { .. });
    session.segment_prefetch_queue.remove(proxy_seq);
    session.origin_to_proxy.retain(|_, mapped_seq| *mapped_seq != proxy_seq);
    if segment.discontinuity_before {
        session.discontinuity_sequence = session.discontinuity_sequence.saturating_add(1);
    }
    if removed_ready_media {
        session.advance_media_readiness_generation();
    }
    if session.publishable_origin_head_proxy_seq == Some(proxy_seq) {
        session.publishable_origin_head_proxy_seq = session
            .segments
            .range(proxy_seq.saturating_add(1)..)
            .find_map(|(next_proxy_seq, segment)| segment.origin_fetch_ref.as_ref().map(|_| *next_proxy_seq));
    }
    Some(segment.cache_key)
}

fn unprotected_unreferenced_map_ids(session: &HlsSession, protected: &ProtectedSet) -> Vec<ProxyMapId> {
    let referenced = session.segments.values().filter_map(|segment| segment.map_ref).collect::<HashSet<_>>();
    session
        .maps
        .iter()
        .filter_map(|(map_id, map)| {
            if referenced.contains(map_id)
                || protected.map_ids.contains(map_id)
                || map.access.active_readers() > 0
                || matches!(map.status, MapCacheStatus::Queued { .. } | MapCacheStatus::Fetching { .. })
            {
                return None;
            }
            Some(*map_id)
        })
        .collect()
}

struct MapEntryDeletion {
    key: MapCacheKey,
    content_length: u64,
}

fn remove_map_entry(session: &mut HlsSession, map_id: ProxyMapId) -> Option<MapEntryDeletion> {
    let map = session.maps.remove(&map_id)?;
    let (content_length, removed_ready_media) = match map.status {
        MapCacheStatus::Ready { content_length, .. } => (content_length, true),
        _ => (0, false),
    };
    if removed_ready_media {
        session.advance_media_readiness_generation();
    }
    session.origin_map_to_proxy.retain(|_, mapped_map_id| *mapped_map_id != map_id);
    Some(MapEntryDeletion { key: map.cache_key, content_length })
}

#[cfg(test)]
mod tests {
    use super::{
        super::{
            media_reserve::{
                HlsLeaseManifestSegment, HlsLeaseManifestSnapshot, HlsManifestDeliveryMode,
                HlsManifestSourceRenderMarker,
            },
            terminal_tail::{HlsEncryptionSignature, HlsMediaContainer, HlsTerminalTailGeneration},
        },
        build_rewrite_secret_fingerprint, oldest_global_fifo_head_candidate, remove_segment_entry, CacheObjectDeletion,
        GarbageCollectionPolicy, GarbageCollectionReport, HlsGarbageCollector, PendingCacheObjectDeletion,
        ProtectedSet, SegmentCacheDeletionReason, MAX_PENDING_CACHE_DELETIONS, SWITCH_CACHE_CLEANUP_HEADROOM,
    };
    use crate::{
        prepare_terminal_base_evidence, HlsAccessLease, HlsAccessLeaseId, HlsAccessLeaseStore,
        HlsOriginResourceFetchError, HlsPlaybackFamilyKey, HlsSegmentCache, HlsSegmentEncryption, HlsSession,
        HlsSessionKey, HlsSessionStore, HlsTerminalTailProtection, MapCacheStatus, OriginMapKey, ProxyMapId,
        ProxySessionId, SegmentCacheStatus, SegmentFetchPriority, TransientObjectFetchDecision,
        TransientPassthroughState, TransientResourceId, TransientResourceKind, TransientResourceRef,
    };
    use std::{collections::HashSet, sync::Arc, task::Poll, time::Duration};
    use tokio::sync::RwLock;
    use tuliprox_parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome};

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    fn normal_manifest(body: &str) -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
        match parse_origin_media_manifest(body, BASE_URL) {
            OriginManifestParseOutcome::Normal(manifest) => manifest,
            OriginManifestParseOutcome::TransientPassthrough { reason } => {
                panic!("expected normal manifest: {reason:?}")
            }
        }
    }

    fn test_policy() -> GarbageCollectionPolicy {
        GarbageCollectionPolicy {
            cache_duration_ms: 300,
            cache_bytes_global: 10_000,
            cache_bytes_per_session: 10_000,
            session_idle_timeout_ms: 1_000,
            temp_file_retention_ms: 30_000,
            failed_segment_retention_ms: 10,
        }
    }

    async fn gc_with_session(temp_dir: &tempfile::TempDir) -> (Arc<HlsGarbageCollector>, crate::HlsSessionHandle) {
        let sessions = Arc::new(HlsSessionStore::new());
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let session = sessions.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
        let gc = Arc::new(HlsGarbageCollector::new(
            sessions,
            Arc::clone(&cache),
            test_policy(),
            build_rewrite_secret_fingerprint(b"secret"),
        ));
        cache.install_capacity_reclaimer(&gc);
        (gc, session)
    }

    fn update_gc_policy(gc: &HlsGarbageCollector, update: impl FnOnce(&mut GarbageCollectionPolicy)) {
        let mut policy = gc.policy().as_ref().clone();
        update(&mut policy);
        gc.update_config(policy, gc.rewrite_secret_fingerprint());
    }

    fn six_segment_manifest() -> tuliprox_parser::hls::origin_manifest::ParsedOriginManifest {
        normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXTINF:4.0,\n1.ts\n#EXTINF:4.0,\n2.ts\n#EXTINF:4.0,\n3.ts\n#EXTINF:4.0,\n4.ts\n#EXTINF:4.0,\n5.ts\n#EXTINF:4.0,\n6.ts\n",
        )
    }

    fn apply_six_segment_manifest_for_gc(session: &mut super::HlsSession) {
        session.proxy_next_seq = Some(1);
        session.apply_origin_manifest(&six_segment_manifest()).expect("manifest should map");
    }

    #[test]
    fn gc_report_is_quiet_when_nothing_changed() {
        let report = GarbageCollectionReport::default();

        assert!(!report.did_cleanup_or_invalidate());
    }

    #[tokio::test]
    async fn published_resource_history_survives_production_segment_removal_and_file_deletion() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let baseline = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:480\n\
             #EXTINF:4,\n480.ts\n#EXTINF:4,\n481.ts\n#EXTINF:4,\n482.ts\n",
        );
        let replay_then_new = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:490\n\
             #EXTINF:4,\n480.ts\n#EXTINF:4,\n490.ts\n#EXTINF:4,\n491.ts\n#EXTINF:4,\n492.ts\n",
        );
        let removed_key = {
            let mut session = session.write().await;
            session.apply_origin_manifest(&baseline).expect("baseline maps");
            for segment in session.segments.values_mut() {
                segment.status = SegmentCacheStatus::Ready { content_length: 1, ready_at_ms: 1 };
            }
            session.render_and_store_manifest(1).expect("baseline publishes");
            session.segments.get(&0).expect("published head").cache_key.clone()
        };
        gc.cache.write_bytes_and_commit(&removed_key, b"x").await.expect("cache object writes");
        {
            let mut session = session.write().await;
            assert_eq!(remove_segment_entry(&mut session, 0), Some(removed_key.clone()));
        }
        gc.cache.delete(&removed_key).await.expect("production cache file deletion succeeds");
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&replay_then_new).expect("history trims removed resource replay");
            assert!(!session.segments.contains_key(&0));
            assert_eq!(session.proxy_next_seq, Some(6));
            assert!(session.segments.get(&3).expect("first new media").discontinuity_before);
        }
        assert!(gc.cache.metadata(&removed_key).await.expect("metadata lookup succeeds").is_none());
    }

    #[test]
    fn terminal_tail_protects_live_base_until_lease_protection_is_released() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let lease_id = HlsAccessLeaseId("lease-a".to_string());
        session.install_terminal_tail_protection(
            lease_id.clone(),
            HlsTerminalTailProtection {
                generation: HlsTerminalTailGeneration(1),
                base_proxy_seqs: Arc::from([41, 42]),
                key_bindings: Arc::from([]),
            },
        );

        let protected = ProtectedSet::from_session(&session);
        assert!(protected.segment_proxy_seqs.contains(&41));
        assert!(protected.segment_proxy_seqs.contains(&42));

        assert!(session.remove_terminal_tail_protection(&lease_id).is_some());
        let released = ProtectedSet::from_session(&session);
        assert!(!released.segment_proxy_seqs.contains(&41));
        assert!(!released.segment_proxy_seqs.contains(&42));
    }

    #[test]
    fn protected_encrypted_segment_also_protects_its_key_resource() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "12345"), b"secret", 0);
        let key_resource = TransientResourceRef::new(
            TransientResourceKind::Key,
            "http://origin.example.com/live/final/key.bin",
            b"secret",
            0,
            u64::MAX,
            Some("bin".to_string()),
        );
        let mut manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4.0,\n1.ts\n",
        );
        for encryption in manifest.segments.iter_mut().filter_map(|segment| segment.encryption.as_mut()) {
            encryption.proxy_resource_id = Some(key_resource.id.0.clone());
            encryption.proxy_resource_extension = Some("bin".to_string());
        }
        session.transient.upsert_resources([key_resource]);
        session.apply_origin_manifest(&manifest).expect("manifest maps");
        let segment = session.segments.get_mut(&0).expect("segment");
        segment.access.reader_started(1);
        let resource_id = segment.encryption.as_ref().expect("encryption").resource_id.clone();

        let protected = ProtectedSet::from_session(&session);

        assert!(protected.segment_proxy_seqs.contains(&0));
        assert!(protected.key_resource_ids.contains(&resource_id));
    }

    #[tokio::test]
    async fn gc_reclaims_terminal_base_only_after_lease_protection_release() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let lease_id = HlsAccessLeaseId("terminal-lease".to_string());
        let cache_key = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let segment = session.segments.get_mut(&1).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
            let cache_key = segment.cache_key.clone();
            session.install_terminal_tail_protection(
                lease_id.clone(),
                HlsTerminalTailProtection {
                    generation: HlsTerminalTailGeneration(1),
                    base_proxy_seqs: Arc::from([1_u64]),
                    key_bindings: Arc::from([]),
                },
            );
            cache_key
        };

        let protected_report = gc.run_once(10_000).await.expect("protected gc should run");
        assert_eq!(protected_report.segments_deleted_duration, 0);
        assert!(session.read().await.segments.contains_key(&1));
        assert!(gc.cache.metadata(&cache_key).await.expect("metadata reads").is_some());

        assert!(session.write().await.remove_terminal_tail_protection(&lease_id).is_some());
        let released_report = gc.run_once(10_001).await.expect("released gc should run");

        assert_eq!(released_report.segments_deleted_duration, 1);
        assert!(gc.cache.metadata(&cache_key).await.expect("metadata reads").is_none());
    }

    #[test]
    fn gc_report_logs_when_cleanup_or_invalidation_happened() {
        let mut report = GarbageCollectionReport { temp_files_deleted: 1, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());

        report = GarbageCollectionReport { stale_queue_entries_removed: 1, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());

        report = GarbageCollectionReport { segments_deleted_duration: 1, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());

        report = GarbageCollectionReport { maps_deleted: 1, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());

        report = GarbageCollectionReport { sessions_deleted: 1, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());

        report = GarbageCollectionReport { transient_resources_pruned: 1, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());

        report = GarbageCollectionReport { secret_cache_invalidated: true, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());

        report =
            GarbageCollectionReport { secret_cache_invalidation_deferred: true, ..GarbageCollectionReport::default() };
        assert!(report.did_cleanup_or_invalidate());
    }

    async fn populate_ready_segments(gc: &HlsGarbageCollector, session: &crate::HlsSessionHandle, ready_at_ms: u64) {
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            for segment in session.segments.values_mut() {
                gc.cache
                    .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                    .await
                    .expect("cache write should succeed");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
            }
            session.render_and_store_manifest(ready_at_ms).expect("ready manifest should render");
        }
    }

    async fn cache_selected_ready_segments(
        gc: &HlsGarbageCollector,
        session: &crate::HlsSessionHandle,
        proxy_seqs: &[u64],
        body: &[u8],
    ) {
        let keys = {
            let session = session.read().await;
            proxy_seqs
                .iter()
                .map(|proxy_seq| (*proxy_seq, session.segments.get(proxy_seq).expect("segment").cache_key.clone()))
                .collect::<Vec<_>>()
        };
        for (_, key) in &keys {
            gc.cache.write_bytes_and_commit(key, body).await.expect("cache fixture writes");
        }
        let mut session = session.write().await;
        for (proxy_seq, _) in keys {
            session.segments.get_mut(&proxy_seq).expect("segment").status = SegmentCacheStatus::Ready {
                content_length: u64::try_from(body.len()).unwrap_or(u64::MAX),
                ready_at_ms: proxy_seq,
            };
        }
    }

    async fn commit_sparse_segment(gc: &HlsGarbageCollector, key: &crate::SegmentCacheKey, size: u64) {
        let final_path = gc.cache.object_path(key);
        let parent = final_path.parent().expect("cache object parent");
        tokio::fs::create_dir_all(parent).await.expect("cache object parent creates");
        let temp_path = final_path.with_extension(format!("ts.tmp.fixture-{}", key.proxy_seq()));
        let file = tokio::fs::File::create(&temp_path).await.expect("sparse temp creates");
        file.set_len(size).await.expect("sparse temp length");
        drop(file);
        let staged = gc.cache.adopt_staged_file(temp_path, size).expect("sparse staged object adopts");
        gc.cache.commit_staged(key, staged).await.expect("sparse object commits");
    }

    #[tokio::test]
    async fn gc_keeps_last_rendered_manifest_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        populate_ready_segments(&gc, &session, 0).await;

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.segments_deleted_duration, 0);
        assert_eq!(session.read().await.segments.len(), 6);
    }

    #[tokio::test]
    async fn gc_keeps_active_readers() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let segment = session.segments.get_mut(&1).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
            segment.access.reader_started(1);
        }

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.segments_deleted_duration, 0);
        assert!(session.read().await.segments.contains_key(&1));
    }

    #[tokio::test]
    async fn gc_keeps_fetching_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            session.segments.get_mut(&1).expect("segment").status =
                SegmentCacheStatus::Fetching { priority: SegmentFetchPriority::Prefetch, started_at_ms: 1 };
        }

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.segments_deleted_duration, 0);
        assert!(session.read().await.segments.contains_key(&1));
    }

    #[tokio::test]
    async fn duration_gc_deletes_old_unprotected_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let segment = session.segments.get_mut(&1).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        }

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.segments_deleted_duration, 1);
        assert!(!session.read().await.segments.contains_key(&1));
    }

    #[tokio::test]
    async fn cache_delete_failure_does_not_block_later_deletes_and_retries_in_active_session() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let (failed_key, successful_key) = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            session.last_client_access_at_ms = 10_000;
            for proxy_seq in [1, 2] {
                session.segments.get_mut(&proxy_seq).expect("segment").status =
                    SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
            }
            (
                session.segments.get(&1).expect("failed segment").cache_key.clone(),
                session.segments.get(&2).expect("successful segment").cache_key.clone(),
            )
        };
        gc.cache.write_bytes_and_commit(&failed_key, b"segment-body").await.expect("failed fixture writes");
        gc.cache.write_bytes_and_commit(&successful_key, b"segment-body").await.expect("successful fixture writes");
        let failed_path = gc.cache.object_path(&failed_key);
        tokio::fs::remove_file(&failed_path).await.expect("replace failed fixture");
        tokio::fs::create_dir(&failed_path).await.expect("directory makes remove_file fail");

        let first_report = gc.run_once(10_000).await.expect("first gc run");

        assert_eq!(first_report.cache_object_deletions_planned, 2);
        assert_eq!(first_report.cache_object_deletions_succeeded, 1);
        assert_eq!(first_report.cache_object_deletions_deferred, 1);
        assert_eq!(first_report.segments_deleted_duration, 1);
        assert!(gc.cache.metadata(&successful_key).await.expect("metadata reads").is_none());
        assert_eq!(gc.pending_cache_deletion_count(), 1);
        assert!(!session.read().await.segments.contains_key(&1));
        assert!(!session.read().await.segments.contains_key(&2));

        tokio::fs::remove_dir(&failed_path).await.expect("remove failing fixture");
        tokio::fs::write(&failed_path, b"replacement-body").await.expect("replacement fixture writes");

        let retry_report = gc.run_once(10_001).await.expect("retry gc run");

        assert_eq!(retry_report.cache_object_deletions_planned, 0);
        assert_eq!(retry_report.cache_object_deletions_succeeded, 1);
        assert_eq!(retry_report.cache_object_deletions_deferred, 0);
        assert_eq!(retry_report.segments_deleted_duration, 1);
        assert_eq!(gc.pending_cache_deletion_count(), 0);
        assert!(gc.cache.metadata(&failed_key).await.expect("metadata reads").is_none());
    }

    #[tokio::test]
    async fn full_retry_queue_preserves_session_metadata_until_a_slot_is_reserved() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            session.segments.get_mut(&1).expect("segment").status =
                SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
        }
        {
            let mut queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for proxy_seq in 0..MAX_PENDING_CACHE_DELETIONS {
                queue.pending.push_back(PendingCacheObjectDeletion {
                    deletion: CacheObjectDeletion::Segment {
                        key: crate::SegmentCacheKey::new(
                            crate::ProxySessionId("queued".to_string()),
                            u64::try_from(proxy_seq).unwrap_or_default(),
                            "ts",
                        ),
                        reason: SegmentCacheDeletionReason::Duration,
                    },
                    attempts: 0,
                });
            }
        }
        let mut report = GarbageCollectionReport::default();
        let policy = gc.policy();
        let mut batch = gc.reserve_cache_deletion_batch();
        assert_eq!(batch.remaining_capacity(), 0);

        {
            let mut session = session.write().await;
            HlsGarbageCollector::collect_session_deletions(
                &mut session,
                &gc.cache,
                10_000,
                &policy,
                &mut report,
                &mut batch,
            );
            assert!(session.segments.contains_key(&1));
        }
        batch.persist(&mut report);

        assert_eq!(report.cache_object_deletions_planned, 0);
        assert_eq!(gc.pending_cache_deletion_count(), MAX_PENDING_CACHE_DELETIONS);
    }

    #[tokio::test]
    async fn dropping_unpersisted_deletion_batch_keeps_reserved_deletion_retryable() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, _) = gc_with_session(&temp_dir).await;
        let key = crate::SegmentCacheKey::new(crate::ProxySessionId("cancelled".to_string()), 1, "ts");
        {
            let mut batch = gc.reserve_cache_deletion_batch();
            batch.push(CacheObjectDeletion::Segment { key: key.clone(), reason: SegmentCacheDeletionReason::Duration });
        }

        let queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(queue.reserved_slots, 0);
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(
            queue.pending.front().map(|pending| &pending.deletion),
            Some(&CacheObjectDeletion::Segment { key, reason: SegmentCacheDeletionReason::Duration })
        );
    }

    #[tokio::test]
    async fn ordinary_gc_batch_preserves_bounded_switch_rollback_headroom() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let batch = gc.reserve_cache_deletion_batch();
        assert_eq!(batch.remaining_capacity(), MAX_PENDING_CACHE_DELETIONS - SWITCH_CACHE_CLEANUP_HEADROOM);

        let segment = gc.reserve_switch_segment_cleanup(crate::SegmentCacheKey::new(proxy_session_id.clone(), 1, "ts"));
        let map = gc.reserve_switch_map_cleanup(crate::MapCacheKey::new(proxy_session_id, ProxyMapId(1), "mp4"));

        assert!(segment.is_some());
        assert!(map.is_some());
        let queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(queue.pending.len().saturating_add(queue.reserved_slots) <= MAX_PENDING_CACHE_DELETIONS);
    }

    #[tokio::test]
    async fn cache_root_handoff_discards_old_logical_deletion_tickets_before_new_root_use() {
        let old_root = tempfile::tempdir().expect("old cache root");
        let new_root = tempfile::tempdir().expect("new cache root");
        let (gc, session) = gc_with_session(&old_root).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let key = crate::SegmentCacheKey::new(proxy_session_id, 1, "ts");
        {
            let mut queue = gc.pending_cache_deletions.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.pending.push_back(PendingCacheObjectDeletion {
                deletion: CacheObjectDeletion::Segment {
                    key: key.clone(),
                    reason: SegmentCacheDeletionReason::Duration,
                },
                attempts: 1,
            });
        }

        assert!(gc.update_cache_path(new_root.path()).await);
        gc.cache.write_bytes_and_commit(&key, b"new-root-sentinel").await.expect("new-root sentinel writes");
        let report = gc.run_once(0).await.expect("new-root gc runs");

        assert_eq!(gc.pending_cache_deletion_count(), 0);
        assert_eq!(report.cache_object_deletions_succeeded, 0);
        let metadata = gc.cache.metadata(&key).await.expect("new-root metadata reads").expect("sentinel remains");
        assert_eq!(tokio::fs::read(metadata.path).await.expect("sentinel reads"), b"new-root-sentinel");
    }

    #[tokio::test]
    async fn cache_root_handoff_invalidates_armed_switch_cleanup_reservations() {
        let old_root = tempfile::tempdir().expect("old cache root");
        let new_root = tempfile::tempdir().expect("new cache root");
        let (gc, session) = gc_with_session(&old_root).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let key = crate::SegmentCacheKey::new(proxy_session_id, 1, "ts");
        let cleanup = gc.reserve_switch_segment_cleanup(key.clone()).expect("switch cleanup reservation");
        assert_eq!(gc.cache_deletion_queue_usage(), (0, 1));

        assert!(gc.update_cache_path(new_root.path()).await);
        assert_eq!(gc.cache_deletion_queue_usage(), (0, 0));
        drop(cleanup);
        assert_eq!(gc.cache_deletion_queue_usage(), (0, 0));

        gc.cache.write_bytes_and_commit(&key, b"new-root-sentinel").await.expect("new-root sentinel writes");
        let report = gc.run_once(0).await.expect("new-root gc runs");

        assert_eq!(report.cache_object_deletions_succeeded, 0);
        let metadata = gc.cache.metadata(&key).await.expect("new-root metadata reads").expect("sentinel remains");
        assert_eq!(tokio::fs::read(metadata.path).await.expect("sentinel reads"), b"new-root-sentinel");
    }

    #[tokio::test]
    async fn failed_switch_rollback_blocks_same_key_until_gc_retry_succeeds() {
        let cache_root = tempfile::tempdir().expect("cache root");
        let (gc, session) = gc_with_session(&cache_root).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let key = crate::SegmentCacheKey::new(proxy_session_id, 1, "ts");
        gc.cache.write_bytes_and_commit(&key, b"uncommitted-switch").await.expect("switch fixture writes");
        let object_path = gc.cache.object_path(&key);
        tokio::fs::remove_file(&object_path).await.expect("switch fixture file removes");
        tokio::fs::create_dir(&object_path).await.expect("directory forces remove-file failure");
        let cleanup = gc.reserve_switch_segment_cleanup(key.clone()).expect("switch cleanup reservation");
        drop(cleanup);

        assert!(gc.has_pending_switch_cleanup(&key, None));
        let deferred = gc.run_once(0).await.expect("deferred rollback GC runs");
        assert_eq!(deferred.cache_object_deletions_succeeded, 0);
        assert_eq!(deferred.cache_object_deletions_deferred, 1);
        assert_eq!(gc.cache_deletion_queue_usage(), (1, 0));
        assert!(gc.has_pending_switch_cleanup(&key, None));

        tokio::fs::remove_dir(&object_path).await.expect("failing directory removes");
        tokio::fs::write(&object_path, b"uncommitted-switch").await.expect("retry fixture writes");
        let retried = gc.run_once(1).await.expect("rollback retry GC runs");

        assert_eq!(retried.cache_object_deletions_succeeded, 1);
        assert_eq!(gc.cache_deletion_queue_usage(), (0, 0));
        assert!(!gc.has_pending_switch_cleanup(&key, None));
        assert!(gc.cache.metadata(&key).await.expect("rolled-back metadata reads").is_none());
    }

    #[tokio::test]
    async fn duration_gc_stops_at_protected_fifo_head() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            for proxy_seq in [1, 2] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                gc.cache
                    .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                    .await
                    .expect("cache write should succeed");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
            }
            session.segments.get_mut(&1).expect("head segment").access.reader_started(1);
        }

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.segments_deleted_duration, 0);
        let session = session.read().await;
        assert!(session.segments.contains_key(&1));
        assert!(session.segments.contains_key(&2));
    }

    #[tokio::test]
    async fn duration_gc_stops_at_not_expired_fifo_head() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            for (proxy_seq, ready_at_ms) in [(1, 9_950), (2, 0)] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                gc.cache
                    .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                    .await
                    .expect("cache write should succeed");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
            }
        }

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.segments_deleted_duration, 0);
        let session = session.read().await;
        assert!(session.segments.contains_key(&1));
        assert!(session.segments.contains_key(&2));
    }

    #[tokio::test]
    async fn session_size_gc_deletes_oldest_unprotected_segment() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        update_gc_policy(&gc, |policy| policy.cache_bytes_per_session = 20);
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            for proxy_seq in [1, 2, 3] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                gc.cache
                    .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                    .await
                    .expect("cache write should succeed");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
            }
        }

        let report = gc.run_once(100).await.expect("gc should run");

        assert_eq!(report.segments_deleted_size_session, 2);
        let session = session.read().await;
        assert!(!session.segments.contains_key(&1));
        assert!(!session.segments.contains_key(&2));
        assert!(session.segments.contains_key(&3));
    }

    #[tokio::test]
    async fn projected_session_pressure_reclaims_before_the_limit_is_crossed() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
        }
        cache_selected_ready_segments(&gc, &session, &[1, 2], b"0123456789").await;
        gc.cache.update_cache_limits(100, 25);
        let target = session.read().await.segments.get(&3).expect("target").cache_key.clone();

        let committed = gc.cache.write_bytes_and_commit(&target, b"abcdefghij").await.expect("projected reclaim");

        assert_eq!(committed.size, 10);
        let session_guard = session.read().await;
        assert!(!session_guard.segments.contains_key(&1), "oldest unprotected FIFO head is reclaimed");
        assert!(session_guard.segments.contains_key(&2));
        let proxy_session_id = session_guard.proxy_session_id.clone();
        drop(session_guard);
        assert!(gc.cache.metadata(&target).await.expect("target metadata").is_some());
        let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
        assert_eq!(usage.session_bytes, 20);
        assert!(usage.session_bytes <= 25);
        assert!(!gc.cache.has_active_temp_files());
    }

    fn activated_startup_lease(proxy_session_id: &ProxySessionId) -> HlsAccessLease {
        let snapshot = HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(10),
            snapshot_generation: 1,
            delivered_at_ms: 10,
            first_proxy_seq: 1,
            last_proxy_seq: 3,
            visible_segments: Arc::from(
                (1_u64..=3)
                    .map(|proxy_seq| HlsLeaseManifestSegment {
                        proxy_seq,
                        duration_ms: 4_000,
                        uri: format!("/hls/shared/live/session/lease/{proxy_seq:06}.ts"),
                        discontinuity_before: false,
                        map_ref_ready: true,
                        encryption: None,
                    })
                    .collect::<Vec<_>>(),
            ),
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            playlist_duration_ms: 12_000,
            last_visible_media_end_ms: 12_000,
            active_map: None,
            active_encryption: None,
            container: HlsMediaContainer::MpegTs,
        };
        let mut lease = HlsAccessLease::pending(
            HlsAccessLeaseId("startup-lease".to_string()),
            HlsPlaybackFamilyKey::new("user", "player"),
            proxy_session_id.clone(),
            "user".to_string(),
            "user-session".to_string(),
            1,
            "12345".to_string(),
            1,
            10,
            60_000,
        );
        lease.state = super::super::HlsAccessLeaseState::Activated;
        lease.active_until_ms = Some(60_000);
        lease.last_manifest_snapshot = Some(snapshot);
        lease
    }

    #[tokio::test]
    async fn protected_startup_tail_defers_until_playback_releases_fifo_head() {
        const SESSION_LIMIT: u64 = 125 * 1_024 * 1_024;
        const RESIDENT_SIZES: [u64; 5] = [21_000_000, 22_000_000, 21_500_000, 22_500_000, 22_221_044];
        const STAGED_SIZE: u64 = 22_551_164;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let (proxy_session_id, keys) = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let keys = (1_u64..=6)
                .map(|proxy_seq| session.segments.get(&proxy_seq).expect("segment").cache_key.clone())
                .collect::<Vec<_>>();
            (session.proxy_session_id.clone(), keys)
        };
        for (key, size) in keys.iter().take(5).zip(RESIDENT_SIZES) {
            commit_sparse_segment(&gc, key, size).await;
        }
        {
            let mut session = session.write().await;
            for (proxy_seq, content_length) in (1_u64..=5).zip(RESIDENT_SIZES) {
                session.segments.get_mut(&proxy_seq).expect("resident segment").status =
                    SegmentCacheStatus::Ready { content_length, ready_at_ms: proxy_seq };
            }
            session.segments.get_mut(&6).expect("tail segment").status =
                SegmentCacheStatus::Queued { priority: SegmentFetchPriority::Prefetch, queued_at_ms: 1 };
            session.render_and_store_manifest(10).expect("six-segment canonical render");
        }

        let mut lease = activated_startup_lease(&proxy_session_id);
        let access_leases = Arc::new(RwLock::new(HlsAccessLeaseStore::default()));
        assert!(access_leases.write().await.prepare_access_lease(lease.clone()));
        gc.install_access_leases(&access_leases);
        gc.cache.update_cache_limits(512 * 1_024 * 1_024, SESSION_LIMIT);

        for key in keys.iter().take(3) {
            assert!(gc.cache.metadata(key).await.expect("visible metadata").is_some());
        }
        let deferred = gc
            .cache
            .ensure_projected_write_capacity(&keys[5], STAGED_SIZE)
            .await
            .expect_err("fully protected startup window defers the tail");
        let capacity = super::super::cache::hls_cache_capacity_from_io(&deferred).expect("typed capacity deferral");
        assert_eq!(capacity.configured_session_bytes(), SESSION_LIMIT);
        assert_eq!(capacity.current_session_bytes(), RESIDENT_SIZES.into_iter().sum::<u64>());
        assert_eq!(capacity.staged_bytes(), STAGED_SIZE);
        assert_eq!(capacity.required_session_bytes(), 700_208);
        assert_eq!(capacity.protected_working_set_bytes(), RESIDENT_SIZES.into_iter().sum::<u64>());
        assert_eq!(capacity.reclaimable_bytes(), 0);
        assert!(gc.cache.metadata(&keys[5]).await.expect("tail metadata").is_none());

        {
            let mut session = session.write().await;
            session.segments.get_mut(&6).expect("tail segment").status =
                SegmentCacheStatus::CapacityDeferred { priority: SegmentFetchPriority::Prefetch, deferred_at_ms: 11 };
            let safe_render = session.render_and_store_manifest(11).expect("deferred tail truncates safely");
            assert_eq!(safe_render.last_proxy_seq, 5);
            assert!(!safe_render.body.contains("000006.ts"));
        }

        lease.playback_cursor.highest_contiguous_completed_proxy_seq = Some(1);
        assert!(access_leases.write().await.prepare_access_lease(lease));
        gc.cache.notify_capacity_protection_changed();
        commit_sparse_segment(&gc, &keys[5], STAGED_SIZE).await;
        {
            let mut session = session.write().await;
            session.segments.get_mut(&6).expect("tail segment").status =
                SegmentCacheStatus::Ready { content_length: STAGED_SIZE, ready_at_ms: 12 };
            let resumed = session.render_and_store_manifest(12).expect("window resumes after reclamation");
            assert_eq!(resumed.first_proxy_seq, 2);
            assert_eq!(resumed.last_proxy_seq, 6);
        }
        assert!(gc.cache.metadata(&keys[0]).await.expect("released head metadata").is_none());
        assert!(gc.cache.metadata(&keys[5]).await.expect("resumed tail metadata").is_some());
        let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("bounded usage");
        assert!(usage.session_bytes <= SESSION_LIMIT);
    }

    #[tokio::test]
    async fn concurrent_projected_pressure_is_serialized_without_over_deletion() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
        }
        cache_selected_ready_segments(&gc, &session, &[1, 2], b"0123456789").await;
        gc.cache.update_cache_limits(100, 25);
        let (third, fourth, proxy_session_id) = {
            let session = session.read().await;
            (
                session.segments.get(&3).expect("third").cache_key.clone(),
                session.segments.get(&4).expect("fourth").cache_key.clone(),
                session.proxy_session_id.clone(),
            )
        };
        let first_cache = Arc::clone(&gc.cache);
        let first = tokio::spawn(async move { first_cache.write_bytes_and_commit(&third, b"abcdefghij").await });
        let second_cache = Arc::clone(&gc.cache);
        let second = tokio::spawn(async move { second_cache.write_bytes_and_commit(&fourth, b"klmnopqrst").await });

        let (first, second) = tokio::join!(first, second);

        assert!(first.expect("first task").is_ok());
        assert!(second.expect("second task").is_ok());
        let session = session.read().await;
        assert!(!session.segments.contains_key(&1));
        assert!(!session.segments.contains_key(&2));
        assert!(session.segments.contains_key(&3));
        assert!(session.segments.contains_key(&4));
        drop(session);
        let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
        assert_eq!(usage.session_bytes, 20);
        assert!(usage.global_bytes <= 25);
    }

    #[tokio::test]
    async fn projected_pressure_never_skips_a_protected_fifo_head() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
        }
        cache_selected_ready_segments(&gc, &session, &[1, 2], b"0123456789").await;
        {
            let mut session = session.write().await;
            session.segments.get_mut(&1).expect("head").access.reader_started(1);
        }
        gc.cache.update_cache_limits(100, 25);
        let target = session.read().await.segments.get(&3).expect("target").cache_key.clone();

        let error = gc.cache.write_bytes_and_commit(&target, b"abcdefghij").await.expect_err("head is protected");

        assert!(matches!(
            HlsOriginResourceFetchError::cache_commit(&error),
            HlsOriginResourceFetchError::LocalCacheCapacity { .. }
        ));
        let session = session.read().await;
        assert!(session.segments.contains_key(&1));
        assert!(session.segments.contains_key(&2));
        drop(session);
        assert!(gc.cache.metadata(&target).await.expect("target metadata").is_none());
        assert!(!gc.cache.has_active_temp_files());
    }

    #[tokio::test]
    async fn projected_global_pressure_uses_oldest_session_fifo_head() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, first_session) = gc_with_session(&temp_dir).await;
        let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(1, "67890"), b"secret", 0).await;
        for session in [&first_session, &second_session] {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
        }
        cache_selected_ready_segments(&gc, &first_session, &[1], b"12345678").await;
        cache_selected_ready_segments(&gc, &second_session, &[1], b"abcdefgh").await;
        {
            let mut first = first_session.write().await;
            let SegmentCacheStatus::Ready { ready_at_ms, .. } =
                &mut first.segments.get_mut(&1).expect("first head").status
            else {
                panic!("first head must be ready");
            };
            *ready_at_ms = 1;
        }
        {
            let mut second = second_session.write().await;
            let SegmentCacheStatus::Ready { ready_at_ms, .. } =
                &mut second.segments.get_mut(&1).expect("second head").status
            else {
                panic!("second head must be ready");
            };
            *ready_at_ms = 2;
        }
        gc.cache.update_cache_limits(20, 100);
        let (target, proxy_session_id) = {
            let session = second_session.read().await;
            (session.segments.get(&2).expect("target").cache_key.clone(), session.proxy_session_id.clone())
        };

        gc.cache.write_bytes_and_commit(&target, b"ijklmnop").await.expect("global projected reclaim");

        assert!(!first_session.read().await.segments.contains_key(&1));
        assert!(second_session.read().await.segments.contains_key(&1));
        let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
        assert_eq!(usage.global_bytes, 16);
        assert!(usage.global_bytes <= 20);
    }

    #[tokio::test]
    async fn high_bitrate_sequence_advances_beyond_the_512_mib_aggregate_wall() {
        use std::fmt::Write as _;

        const SESSION_LIMIT: u64 = 512 * 1024 * 1024;
        const OLD_SEGMENT_BYTES: u64 = 21_825_922;
        const NEW_SEGMENT_BYTES: u64 = 22_000_000;
        const READY_SEGMENTS: u64 = 24;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let mut manifest = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXT-X-MEDIA-SEQUENCE:1\n");
        for origin_seq in 1..=READY_SEGMENTS.saturating_add(1) {
            writeln!(manifest, "#EXTINF:10.0,\n{origin_seq}.ts").expect("writing to a String cannot fail");
        }
        {
            let mut session = session.write().await;
            session.proxy_next_seq = Some(1);
            session.apply_origin_manifest(&normal_manifest(&manifest)).expect("high-bitrate manifest maps");
        }
        let keys = {
            let session = session.read().await;
            (1..=READY_SEGMENTS.saturating_add(1))
                .map(|proxy_seq| session.segments.get(&proxy_seq).expect("segment").cache_key.clone())
                .collect::<Vec<_>>()
        };
        for key in keys.iter().take(usize::try_from(READY_SEGMENTS).unwrap_or(usize::MAX)) {
            commit_sparse_segment(&gc, key, OLD_SEGMENT_BYTES).await;
        }
        {
            let mut session = session.write().await;
            for proxy_seq in 1..=READY_SEGMENTS {
                session.segments.get_mut(&proxy_seq).expect("ready segment").status =
                    SegmentCacheStatus::Ready { content_length: OLD_SEGMENT_BYTES, ready_at_ms: proxy_seq };
            }
        }
        gc.cache.update_cache_limits(2 * SESSION_LIMIT, SESSION_LIMIT);
        let target = keys.last().expect("target key");

        commit_sparse_segment(&gc, target, NEW_SEGMENT_BYTES).await;
        {
            let mut session = session.write().await;
            session.segments.get_mut(&READY_SEGMENTS.saturating_add(1)).expect("newest segment").status =
                SegmentCacheStatus::Ready { content_length: NEW_SEGMENT_BYTES, ready_at_ms: READY_SEGMENTS + 1 };
        }

        let aggregate_sequence_bytes =
            READY_SEGMENTS.saturating_mul(OLD_SEGMENT_BYTES).saturating_add(NEW_SEGMENT_BYTES);
        assert!(aggregate_sequence_bytes > SESSION_LIMIT);
        let session_guard = session.read().await;
        assert!(!session_guard.segments.contains_key(&1));
        assert!(session_guard.segments.contains_key(&READY_SEGMENTS.saturating_add(1)));
        let proxy_session_id = session_guard.proxy_session_id.clone();
        drop(session_guard);
        let usage = gc.cache.capacity_usage(&proxy_session_id).await.expect("capacity usage");
        assert_eq!(usage.session_bytes, READY_SEGMENTS.saturating_sub(1) * OLD_SEGMENT_BYTES + NEW_SEGMENT_BYTES);
        assert!(usage.session_bytes <= SESSION_LIMIT);
    }

    #[tokio::test]
    async fn session_size_gc_stops_at_protected_fifo_head() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        update_gc_policy(&gc, |policy| policy.cache_bytes_per_session = 20);
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            for proxy_seq in [1, 2, 3] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                gc.cache
                    .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                    .await
                    .expect("cache write should succeed");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
            }
            session.segments.get_mut(&1).expect("head segment").access.reader_started(1);
        }

        let report = gc.run_once(100).await.expect("gc should run");

        assert_eq!(report.segments_deleted_size_session, 0);
        let session = session.read().await;
        assert!(session.segments.contains_key(&1));
        assert!(session.segments.contains_key(&2));
        assert!(session.segments.contains_key(&3));
    }

    #[tokio::test]
    async fn protected_map_remains_and_unreferenced_map_is_deleted() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n1.m4s\n#EXTINF:4.0,\n2.m4s\n#EXTINF:4.0,\n3.m4s\n",
        );
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest should map");
            for segment in session.segments.values_mut() {
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 0 };
            }
            let protected_map_id = ProxyMapId(0);
            session.maps.get_mut(&protected_map_id).expect("map").status =
                MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
            let unreferenced_key = OriginMapKey {
                origin_epoch: 0,
                resolved_origin_uri: "http://origin.example.com/live/unused.mp4".to_string(),
                byte_range: None,
            };
            let unreferenced_map = crate::MapEntry::new(
                &session.proxy_session_id,
                ProxyMapId(1),
                unreferenced_key.clone(),
                "mp4".to_string(),
            );
            session.maps.insert(ProxyMapId(1), unreferenced_map);
            session.origin_map_to_proxy.insert(unreferenced_key, ProxyMapId(1));
            session.maps.get_mut(&ProxyMapId(1)).expect("map").status =
                MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
            session.render_and_store_manifest(1).expect("manifest should render");
        }

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.maps_deleted, 1);
        let session = session.read().await;
        assert!(session.maps.contains_key(&ProxyMapId(0)));
        assert!(!session.maps.contains_key(&ProxyMapId(1)));
    }

    #[tokio::test]
    async fn map_referenced_by_remaining_segment_survives_fifo_gc() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let manifest =
            normal_manifest("#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\n0.m4s\n#EXTINF:4.0,\n1.m4s\n");
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest should map");
            session.maps.get_mut(&ProxyMapId(0)).expect("map").status =
                MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
            for (proxy_seq, ready_at_ms) in [(0, 0), (1, 9_950)] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
            }
        }

        let report = gc.run_once(10_000).await.expect("gc should run");

        assert_eq!(report.segments_deleted_duration, 1);
        assert_eq!(report.maps_deleted, 0);
        let session = session.read().await;
        assert!(!session.segments.contains_key(&0));
        assert!(session.segments.contains_key(&1));
        assert!(session.maps.contains_key(&ProxyMapId(0)));
    }

    #[tokio::test]
    async fn global_size_gc_subtracts_map_bytes_after_segment_removal() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        update_gc_policy(&gc, |policy| policy.cache_bytes_global = 25);
        let manifest = normal_manifest(
            "#EXTM3U\n#EXT-X-MAP:URI=\"init0.mp4\"\n#EXTINF:4.0,\n0.m4s\n#EXT-X-MAP:URI=\"init1.mp4\"\n#EXTINF:4.0,\n1.m4s\n",
        );
        {
            let mut session = session.write().await;
            session.apply_origin_manifest(&manifest).expect("manifest should map");
            for (proxy_seq, ready_at_ms) in [(0, 0), (1, 1)] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
            }
            session.maps.get_mut(&ProxyMapId(0)).expect("first map").status =
                MapCacheStatus::Ready { content_length: 10, ready_at_ms: 0 };
            session.maps.get_mut(&ProxyMapId(1)).expect("second map").status =
                MapCacheStatus::Ready { content_length: 10, ready_at_ms: 1 };
        }

        let report = gc.run_once(100).await.expect("gc should run");

        assert_eq!(report.segments_deleted_size_global, 1);
        assert_eq!(report.maps_deleted, 1);
        let session = session.read().await;
        assert!(!session.segments.contains_key(&0));
        assert!(session.segments.contains_key(&1));
        assert!(!session.maps.contains_key(&ProxyMapId(0)));
        assert!(session.maps.contains_key(&ProxyMapId(1)));
    }

    #[tokio::test]
    async fn transient_resource_mappings_expire_unless_active() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let resource_id = {
            let mut session = session.write().await;
            let resource = crate::TransientResourceRef::new(
                crate::TransientResourceKind::Segment,
                "http://origin.example.com/live/seg.ts",
                b"secret",
                0,
                10,
                Some("ts".to_string()),
            );
            let resource_id = resource.id.clone();
            session.transient.upsert_resources([resource]);
            resource_id
        };

        let report = gc.run_once(20).await.expect("gc should run");

        assert_eq!(report.transient_resources_pruned, 1);
        assert!(!session.read().await.transient.resources.contains_key(&resource_id));
    }

    #[tokio::test]
    async fn transient_resource_mappings_in_last_manifest_are_protected() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let resource_id = {
            let mut session = session.write().await;
            let resource = crate::TransientResourceRef::new(
                crate::TransientResourceKind::Segment,
                "http://origin.example.com/live/seg.ts",
                b"secret",
                0,
                10,
                Some("ts".to_string()),
            );
            let resource_id = resource.id.clone();
            session.transient.upsert_resources([resource]);
            session.transient.replace_manifest(
                format!("#EXTM3U\n#EXTINF:1,\n/hls/shared/live/session/lease/r/{}.ts\n", resource_id.0),
                0,
            );
            resource_id
        };

        let report = gc.run_once(20).await.expect("gc should run");

        assert_eq!(report.transient_resources_pruned, 0);
        assert!(session.read().await.transient.resources.contains_key(&resource_id));
    }

    fn encrypted_terminal_evidence_manifest(
        proxy_session_id: &ProxySessionId,
        resource_id: &TransientResourceId,
    ) -> HlsLeaseManifestSnapshot {
        let encryption = HlsEncryptionSignature {
            method: "AES-128".to_string(),
            key_uri: Some(format!("/hls/shared/live/{}/lease/r/{}.key", proxy_session_id.0, resource_id.0)),
            iv: Some("0x00000000000000000000000000000001".to_string()),
            key_format: Some("identity".to_string()),
            key_format_versions: Some("1".to_string()),
            can_reset_to_clear: true,
        };
        HlsLeaseManifestSnapshot {
            delivery_mode: HlsManifestDeliveryMode::NormalCacheTimeline,
            source_render_marker: HlsManifestSourceRenderMarker::new(1),
            snapshot_generation: 1,
            delivered_at_ms: 0,
            first_proxy_seq: 1,
            last_proxy_seq: 1,
            visible_segments: Arc::from([HlsLeaseManifestSegment {
                proxy_seq: 1,
                duration_ms: 4_000,
                uri: "/hls/shared/live/session/lease/1.ts".to_string(),
                discontinuity_before: false,
                map_ref_ready: true,
                encryption: Some(encryption.clone()),
            }]),
            discontinuity_sequence: 0,
            target_duration_ms: 4_000,
            playlist_duration_ms: 4_000,
            last_visible_media_end_ms: 4_000,
            active_map: None,
            active_encryption: Some(encryption),
            container: HlsMediaContainer::MpegTs,
        }
    }

    #[tokio::test]
    async fn terminal_evidence_pins_ready_key_object_and_mapping_across_gc_until_release() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let (segment_cache_key, proxy_session_id, resource, resource_id, object_lookup_key, object_fetch_token) = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let segment_cache_key = session.segments.get(&1).expect("segment").cache_key.clone();
            let resource = TransientResourceRef::new(
                TransientResourceKind::Key,
                "http://origin.example.com/live/key.bin",
                b"secret",
                0,
                10,
                Some("key".to_string()),
            );
            let resource_id = resource.id.clone();
            session.transient.upsert_resources([resource]);
            let resource = session.transient.resources.get(&resource_id).expect("registered key resource").clone();
            session.segments.get_mut(&1).expect("encrypted segment").encryption = Some(HlsSegmentEncryption {
                resource_id: resource_id.clone(),
                resource_extension: "key".to_string(),
                iv: Some("0x00000000000000000000000000000001".to_string()),
                key_format: Some("identity".to_string()),
                key_format_versions: Some("1".to_string()),
            });
            let proxy_session_id = session.proxy_session_id.clone();
            let object_lookup_key =
                TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "key");
            let object_fetch_token =
                match session.transient.begin_object_fetch(&proxy_session_id, &resource, "key", 0, 10) {
                    TransientObjectFetchDecision::Fetch(token) => token,
                    TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                        panic!("new key object starts one physical cache fill")
                    }
                };
            (segment_cache_key, proxy_session_id, resource, resource_id, object_lookup_key, object_fetch_token)
        };
        gc.cache.write_bytes_and_commit(&segment_cache_key, b"ready-media").await.expect("media cache write");
        gc.cache
            .write_bytes_and_commit(object_fetch_token.cache_key(), b"0123456789abcdef")
            .await
            .expect("key cache write");
        {
            let mut session = session.write().await;
            session.segments.get_mut(&1).expect("segment").status =
                SegmentCacheStatus::Ready { content_length: 11, ready_at_ms: 0 };
            assert!(session.commit_transient_object_ready_if_current(
                resource.kind,
                &object_fetch_token,
                "application/octet-stream".to_string(),
                16,
                0,
                10,
            ));
        }
        let manifest = encrypted_terminal_evidence_manifest(&proxy_session_id, &resource_id);

        let evidence = prepare_terminal_base_evidence(&session, &gc.cache, &manifest, 5).await;
        assert!(evidence.availability()[0].required_key_ready);
        {
            let mut session = session.write().await;
            session.transient.resources.get_mut(&resource_id).expect("key mapping").expires_at_ms = 0;
            session.transient.object_cache.get_mut(&object_lookup_key).expect("key object").expires_at_ms = 0;
        }

        let pinned_report = gc.run_once(20).await.expect("pinned GC");
        assert_eq!(pinned_report.transient_resources_pruned, 0);
        assert_eq!(pinned_report.transient_objects_deleted, 0);
        {
            let session = session.read().await;
            assert!(session.transient.resources.contains_key(&resource_id));
            assert!(session.transient.object_cache.contains_key(&object_lookup_key));
        }

        evidence.release();
        let generation_before_release_gc = session.read().await.activity.media_readiness_generation;
        let released_report = gc.run_once(21).await.expect("released GC");
        assert_eq!(released_report.transient_resources_pruned, 1);
        assert_eq!(released_report.transient_objects_deleted, 1);
        let session = session.read().await;
        assert_eq!(
            session.activity.media_readiness_generation,
            generation_before_release_gc.saturating_add(1),
            "mapping and READY key object removal advance readiness once per session sweep"
        );
        assert!(!session.transient.resources.contains_key(&resource_id));
        assert!(!session.transient.object_cache.contains_key(&object_lookup_key));
    }

    #[tokio::test]
    async fn transient_object_gc_deletes_the_physical_fill_generation_instead_of_the_logical_lookup_path() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let (lookup_key, fetch_token, resource_kind) = {
            let mut session = session.write().await;
            let resource = TransientResourceRef::new(
                TransientResourceKind::Segment,
                "http://origin.example.com/live/object1.ts",
                b"secret",
                0,
                10,
                Some("ts".to_string()),
            );
            let resource_id = resource.id.clone();
            session.transient.upsert_resources([resource]);
            let resource = session.transient.resources.get(&resource_id).expect("registered resource").clone();
            let proxy_session_id = session.proxy_session_id.clone();
            let lookup_key = TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "ts");
            let fetch_token = match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 0, 10) {
                TransientObjectFetchDecision::Fetch(token) => token,
                TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                    panic!("new transient object starts one physical cache fill")
                }
            };
            (lookup_key, fetch_token, resource.kind)
        };
        assert_ne!(&lookup_key, fetch_token.cache_key());
        gc.cache
            .write_bytes_and_commit(fetch_token.cache_key(), b"transient-body")
            .await
            .expect("physical generation writes");
        gc.cache.write_bytes_and_commit(&lookup_key, b"logical-decoy").await.expect("logical lookup decoy writes");
        assert!(session.write().await.commit_transient_object_ready_if_current(
            resource_kind,
            &fetch_token,
            "video/mp2t".to_string(),
            14,
            0,
            10,
        ));

        let report = gc.run_once(20).await.expect("gc should run");

        assert_eq!(report.transient_objects_deleted, 1);
        assert_eq!(report.transient_object_bytes_deleted, 14);
        assert!(gc.cache.metadata(fetch_token.cache_key()).await.expect("physical metadata read").is_none());
        assert!(gc.cache.metadata(&lookup_key).await.expect("logical metadata read").is_some());
    }

    #[tokio::test]
    async fn transient_object_gc_expires_stale_fetching_metadata_deletes_its_fill_and_wakes_waiters() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let (lookup_key, fetch_token, notifier) = {
            let mut session = session.write().await;
            let resource = TransientResourceRef::new(
                TransientResourceKind::Segment,
                "http://origin.example.com/live/stale.ts",
                b"secret",
                0,
                10,
                Some("ts".to_string()),
            );
            let resource_id = resource.id.clone();
            session.transient.upsert_resources([resource]);
            let resource = session.transient.resources.get(&resource_id).expect("registered resource").clone();
            let proxy_session_id = session.proxy_session_id.clone();
            let lookup_key = TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "ts");
            let fetch_token = match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 0, 10) {
                TransientObjectFetchDecision::Fetch(token) => token,
                TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                    panic!("new transient object starts one physical cache fill")
                }
            };
            let notifier = match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 1, 10) {
                TransientObjectFetchDecision::Wait(notifier) => notifier,
                TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Fetch(_) => {
                    panic!("a concurrent request waits for the current cache fill")
                }
            };
            (lookup_key, fetch_token, notifier)
        };
        gc.cache
            .write_bytes_and_commit(fetch_token.cache_key(), b"partial")
            .await
            .expect("stale physical fill fixture writes");
        let waiter = notifier.notified();
        tokio::pin!(waiter);
        assert!(matches!(futures::poll!(&mut waiter), Poll::Pending));

        let report = gc.run_once(20).await.expect("gc should run");

        assert!(matches!(futures::poll!(&mut waiter), Poll::Ready(())));
        assert_eq!(report.transient_objects_deleted, 1);
        assert_eq!(report.transient_object_bytes_deleted, 0);
        assert!(!session.read().await.transient.object_cache.contains_key(&lookup_key));
        assert!(gc.cache.metadata(fetch_token.cache_key()).await.expect("metadata reads").is_none());
    }

    #[tokio::test]
    async fn session_size_gc_deletes_transient_objects_before_timeline_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        update_gc_policy(&gc, |policy| policy.cache_bytes_per_session = 20);
        let (segment_key, object_lookup_key, object_fetch_token, resource_kind) = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let segment_key = session.segments.get(&1).expect("segment").cache_key.clone();
            let resource = TransientResourceRef::new(
                TransientResourceKind::Segment,
                "http://origin.example.com/live/object1.ts",
                b"secret",
                100,
                10_000,
                Some("ts".to_string()),
            );
            let resource_id = resource.id.clone();
            session.transient.upsert_resources([resource]);
            let resource = session.transient.resources.get(&resource_id).expect("registered resource").clone();
            let proxy_session_id = session.proxy_session_id.clone();
            let object_lookup_key =
                TransientPassthroughState::transient_object_key(&proxy_session_id, &resource_id, "ts");
            let object_fetch_token =
                match session.transient.begin_object_fetch(&proxy_session_id, &resource, "ts", 100, 10_000) {
                    TransientObjectFetchDecision::Fetch(token) => token,
                    TransientObjectFetchDecision::Ready | TransientObjectFetchDecision::Wait(_) => {
                        panic!("new transient object starts one physical cache fill")
                    }
                };
            (segment_key, object_lookup_key, object_fetch_token, resource.kind)
        };
        gc.cache.write_bytes_and_commit(&segment_key, b"segment-body").await.expect("segment writes");
        gc.cache
            .write_bytes_and_commit(object_fetch_token.cache_key(), b"transient-body")
            .await
            .expect("transient object writes");
        {
            let mut session = session.write().await;
            session.segments.get_mut(&1).expect("segment").status =
                SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 100 };
            assert!(session.commit_transient_object_ready_if_current(
                resource_kind,
                &object_fetch_token,
                "video/mp2t".to_string(),
                14,
                100,
                10_000,
            ));
        }

        let report = gc.run_once(100).await.expect("gc should run");

        assert_eq!(report.transient_objects_deleted, 1);
        assert_eq!(report.segments_deleted_size_session, 0);
        let session = session.read().await;
        assert!(session.segments.contains_key(&1));
        assert!(!session.transient.object_cache.contains_key(&object_lookup_key));
        drop(session);
        assert!(gc.cache.metadata(object_fetch_token.cache_key()).await.expect("metadata reads").is_none());
    }

    #[tokio::test]
    async fn secret_fingerprint_mismatch_invalidates_cache_and_sessions() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let segment = session.segments.get_mut(&1).expect("segment");
            gc.cache
                .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                .await
                .expect("cache write should succeed");
        }
        gc.cache.write_rewrite_secret_fingerprint("mismatch").await.expect("marker write");

        let report = gc.run_once(1).await.expect("gc should run");

        assert!(report.secret_cache_invalidated);
        assert!(!report.secret_cache_invalidation_deferred);
        assert_eq!(gc.sessions.len().await, 0);
        let rewrite_secret_fingerprint = gc.rewrite_secret_fingerprint();
        assert_eq!(
            gc.cache.read_rewrite_secret_fingerprint().await.expect("marker read").as_deref(),
            Some(rewrite_secret_fingerprint.as_str())
        );
    }

    #[tokio::test]
    async fn secret_fingerprint_mismatch_with_active_temp_defers_invalidation() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let cache_key = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            session.segments.get(&1).expect("segment").cache_key.clone()
        };
        gc.cache.write_rewrite_secret_fingerprint("mismatch").await.expect("marker write");
        let staged = gc
            .cache
            .stage_temp_with_deadline(&cache_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
            .await
            .expect("cache object stages");
        assert!(gc.cache.has_active_temp_files());

        let report = gc.run_once(1).await.expect("gc should run");

        assert!(!report.secret_cache_invalidated);
        assert!(report.secret_cache_invalidation_deferred);
        assert_eq!(gc.sessions.len().await, 1);
        assert_eq!(gc.cache.read_rewrite_secret_fingerprint().await.expect("marker read").as_deref(), Some("mismatch"));
        assert!(gc.cache.has_active_temp_files());
        gc.cache.commit_staged(&cache_key, staged).await.expect("staged object commits");
        assert!(gc.cache.metadata(&cache_key).await.expect("metadata should read").is_some());
    }

    #[tokio::test]
    async fn deferred_secret_fingerprint_invalidation_runs_after_temp_commit() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let cache_key = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            session.segments.get(&1).expect("segment").cache_key.clone()
        };
        gc.cache.write_rewrite_secret_fingerprint("mismatch").await.expect("marker write");
        let staged = gc
            .cache
            .stage_temp_with_deadline(&cache_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
            .await
            .expect("cache object stages");
        assert!(gc.run_once(1).await.expect("first gc should run").secret_cache_invalidation_deferred);
        gc.cache.commit_staged(&cache_key, staged).await.expect("staged object commits");

        let report = gc.run_once(2).await.expect("second gc should run");

        assert!(report.secret_cache_invalidated);
        assert!(!report.secret_cache_invalidation_deferred);
        assert_eq!(gc.sessions.len().await, 0);
        assert_eq!(gc.cache.metadata(&cache_key).await.expect("metadata should read"), None);
        let rewrite_secret_fingerprint = gc.rewrite_secret_fingerprint();
        assert_eq!(
            gc.cache.read_rewrite_secret_fingerprint().await.expect("marker read").as_deref(),
            Some(rewrite_secret_fingerprint.as_str())
        );
    }

    #[tokio::test]
    async fn global_candidate_selection_skips_a_session_that_failed_revalidation() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, first_session) = gc_with_session(&temp_dir).await;
        let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(2, "12345"), b"secret", 0).await;

        for (session, ready_at_ms) in [(&first_session, 1), (&second_session, 2)] {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            session.segments.get_mut(&1).expect("head").status =
                SegmentCacheStatus::Ready { content_length: 12, ready_at_ms };
        }
        let first_proxy_session_id = first_session.read().await.proxy_session_id.clone();
        let sessions = [Arc::clone(&first_session), Arc::clone(&second_session)];

        let first =
            oldest_global_fifo_head_candidate(&sessions, &gc.cache, None, &HashSet::new()).await.expect("candidate");
        assert_eq!(first.proxy_session_id, first_proxy_session_id);

        let skipped = HashSet::from([first_proxy_session_id]);
        let next =
            oldest_global_fifo_head_candidate(&sessions, &gc.cache, None, &skipped).await.expect("next candidate");
        assert_eq!(next.proxy_session_id, second_session.read().await.proxy_session_id);
    }

    #[tokio::test]
    async fn global_size_gc_deletes_oldest_unprotected_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, first_session) = gc_with_session(&temp_dir).await;
        update_gc_policy(&gc, |policy| policy.cache_bytes_global = 24);
        let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(2, "12345"), b"secret", 0).await;

        for session in [&first_session, &second_session] {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            for proxy_seq in [1, 2, 3] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                gc.cache
                    .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                    .await
                    .expect("cache write should succeed");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
            }
        }

        let report = gc.run_once(100).await.expect("gc should run");

        assert_eq!(report.segments_deleted_size_global, 4);
        let remaining_size = {
            let first = first_session.read().await;
            let second = second_session.read().await;
            super::session_cache_size(&first) + super::session_cache_size(&second)
        };
        assert!(remaining_size <= 24);
    }

    #[tokio::test]
    async fn global_size_gc_uses_only_current_fifo_heads() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, first_session) = gc_with_session(&temp_dir).await;
        update_gc_policy(&gc, |policy| policy.cache_bytes_global = 24);
        let second_session = gc.sessions.get_or_create_session(HlsSessionKey::new(2, "12345"), b"secret", 0).await;

        for session in [&first_session, &second_session] {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            for proxy_seq in [1, 2, 3] {
                let segment = session.segments.get_mut(&proxy_seq).expect("segment");
                gc.cache
                    .write_bytes_and_commit(&segment.cache_key, b"segment-body")
                    .await
                    .expect("cache write should succeed");
                segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: proxy_seq };
            }
        }
        first_session.write().await.segments.get_mut(&1).expect("head segment").access.reader_started(1);

        let report = gc.run_once(100).await.expect("gc should run");

        assert_eq!(report.segments_deleted_size_global, 3);
        let first = first_session.read().await;
        assert!(first.segments.contains_key(&1));
        assert!(first.segments.contains_key(&2));
        assert!(first.segments.contains_key(&3));
        let second = second_session.read().await;
        assert!(!second.segments.contains_key(&1));
        assert!(!second.segments.contains_key(&2));
        assert!(!second.segments.contains_key(&3));
    }

    #[tokio::test]
    async fn temp_file_gc_deletes_old_tmp_files() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let temp_path = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let path = gc.cache.object_path(&session.segments.get(&1).expect("segment").cache_key);
            let parent = path.parent().expect("cache object has parent");
            tokio::fs::create_dir_all(parent).await.expect("parent dir");
            parent.join("000001.ts.tmp.old")
        };
        tokio::fs::write(&temp_path, b"partial").await.expect("temp write");
        let old_time = filetime::FileTime::from_unix_time(1, 0);
        filetime::set_file_mtime(&temp_path, old_time).expect("set mtime");

        let report = gc.run_once(1).await.expect("gc should run");

        assert_eq!(report.temp_files_deleted, 1);
        assert!(!temp_path.exists());
    }

    #[tokio::test]
    async fn session_gc_keeps_idle_session_with_active_temp_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let (cache_key, proxy_session_id) = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            (session.segments.get(&1).expect("segment").cache_key.clone(), session.proxy_session_id.clone())
        };
        let staged = gc
            .cache
            .stage_temp_with_deadline(&cache_key, &b"done"[..], tokio::time::Instant::now() + Duration::from_mins(1))
            .await
            .expect("cache object stages");
        assert!(gc.cache.has_active_temp_files_for_session(&proxy_session_id));

        let report = gc.run_once(2_000).await.expect("gc should run");

        assert_eq!(report.sessions_deleted, 0);
        assert!(!session.read().await.is_gc_marked_for_removal());
        assert_eq!(gc.sessions.len().await, 1);
        gc.cache.commit_staged(&cache_key, staged).await.expect("staged object commits");
    }

    #[tokio::test]
    async fn session_gc_removes_idle_session_without_activity() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let (key, proxy_session_id) = {
            let session = session.read().await;
            (session.key.clone(), session.proxy_session_id.clone())
        };

        let report = gc.run_once(2_000).await.expect("gc should run");

        assert_eq!(report.sessions_deleted, 1);
        assert!(gc.sessions.get_by_key(&key).await.is_none());
        assert!(gc.sessions.get_by_proxy_session_id(&proxy_session_id).await.is_none());
    }

    #[tokio::test]
    async fn session_gc_final_recheck_keeps_new_activity() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        session.write().await.last_client_access_at_ms = 1_500;
        let mut report = super::GarbageCollectionReport::default();
        let policy = gc.policy();

        gc.remove_idle_session_if_still_idle(&session, 2_000, &policy, &mut report).await;

        assert_eq!(report.sessions_deleted, 0);
        assert_eq!(gc.sessions.len().await, 1);
        assert!(!session.read().await.is_gc_marked_for_removal());
    }

    #[tokio::test]
    async fn active_transient_resource_reader_protects_idle_session() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        {
            let mut session = session.write().await;
            let resource = crate::TransientResourceRef::new(
                crate::TransientResourceKind::Segment,
                "http://origin.example.com/live/seg.ts",
                b"secret",
                0,
                10,
                Some("ts".to_string()),
            );
            resource.access.reader_started(1);
            session.transient.upsert_resources([resource]);
        }

        let report = gc.run_once(2_000).await.expect("gc should run");

        assert_eq!(report.sessions_deleted, 0);
        assert_eq!(gc.sessions.len().await, 1);
    }
}
