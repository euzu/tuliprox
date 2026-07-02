use super::{
    renderer_candidate_window_proxy_seqs, safe_proxy_session_id, CacheInvalidationOutcome, HlsCacheMetrics,
    HlsSegmentCache, HlsSession, HlsExpiredSessionReason, HlsSessionHandle, HlsSessionStore, MapCacheKey,
    MapCacheStatus, ProxyMapId,
    ProxySessionId, SegmentCacheKey, SegmentCacheStatus, TransientObjectCacheKey,
};
use crate::{api::model::AppState, model::HlsCacheConfig};
use arc_swap::ArcSwap;
use log::{debug, error, info, warn};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fmt::Write as _,
    io,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio_util::sync::CancellationToken;

const HLS_CACHE_GC_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_TEMP_FILE_RETENTION_MS: u64 = 30_000;
const DEFAULT_FAILED_SEGMENT_RETENTION_MS: u64 = 10_000;

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
}

impl ProtectedSet {
    pub fn from_session(session: &HlsSession) -> Self {
        let mut protected = Self::default();

        if let Some(rendered) = &session.last_rendered_manifest {
            protected.segment_proxy_seqs.extend(rendered.segment_proxy_seqs.iter().copied());
        }
        protected.segment_proxy_seqs.extend(renderer_candidate_window_proxy_seqs(session));

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
            if let Some(map_ref) = session.segments.get(proxy_seq).and_then(|segment| segment.map_ref) {
                protected.map_ids.insert(map_ref);
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

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct GarbageCollectionReport {
    pub secret_cache_invalidated: bool,
    pub secret_cache_invalidation_deferred: bool,
    pub temp_files_deleted: usize,
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
            || self.stale_queue_entries_removed > 0
            || self.segments_deleted() > 0
            || self.maps_deleted > 0
            || self.sessions_deleted > 0
            || self.transient_resources_pruned > 0
            || self.transient_objects_deleted > 0
    }
}

pub struct HlsGarbageCollector {
    sessions: Arc<HlsSessionStore>,
    cache: Arc<HlsSegmentCache>,
    policy: ArcSwap<GarbageCollectionPolicy>,
    rewrite_secret_fingerprint: ArcSwap<String>,
    metrics: Arc<HlsCacheMetrics>,
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
        }
    }

    pub fn update_config(&self, policy: GarbageCollectionPolicy, rewrite_secret_fingerprint: String) {
        self.policy.store(Arc::new(policy));
        self.rewrite_secret_fingerprint.store(Arc::new(rewrite_secret_fingerprint));
    }

    pub fn policy(&self) -> Arc<GarbageCollectionPolicy> { self.policy.load_full() }

    pub fn rewrite_secret_fingerprint(&self) -> String { self.rewrite_secret_fingerprint.load().to_string() }

    pub async fn run_once(&self, now_ms: u64) -> io::Result<GarbageCollectionReport> {
        let policy = self.policy.load_full();
        let mut report = GarbageCollectionReport::default();
        if self.ensure_cache_marker(&mut report).await? {
            self.record_report_metrics(&report);
            return Ok(report);
        }

        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_millis(policy.temp_file_retention_ms))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        report.temp_files_deleted = self.cache.delete_temp_files_older_than(cutoff).await?;

        let sessions = self.sessions.list_sessions().await;
        let mut pending_deletions = Vec::new();
        for session in &sessions {
            let mut session = session.write().await;
            let mut deletions = Self::collect_session_deletions(&mut session, now_ms, &policy, &mut report);
            pending_deletions.append(&mut deletions);
        }
        self.delete_cache_objects(pending_deletions).await?;

        let global_deletions = self.collect_global_size_deletions(&sessions, &policy, &mut report).await;
        self.delete_cache_objects(global_deletions).await?;

        for session in &sessions {
            self.remove_idle_session_if_still_idle(session, now_ms, &policy, &mut report).await?;
        }

        self.record_report_metrics(&report);
        if report.did_cleanup_or_invalidate() {
            info!(
                "HLS session garbage collection completed: temp_files_deleted={} stale_queue_entries_removed={} segments_deleted={} maps_deleted={} transient_resources_pruned={} transient_objects_deleted={} transient_object_bytes_deleted={} sessions_deleted={}",
                report.temp_files_deleted,
                report.stale_queue_entries_removed,
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
        now_ms: u64,
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
    ) -> Vec<CacheObjectDeletion> {
        let mut deletions = Vec::new();

        report.stale_queue_entries_removed =
            report.stale_queue_entries_removed.saturating_add(remove_stale_queue_entries(session));

        let transient_before = session.transient.resources.len();
        session.transient.prune_expired(now_ms);
        report.transient_resources_pruned = report
            .transient_resources_pruned
            .saturating_add(transient_before.saturating_sub(session.transient.resources.len()));

        for removal in session.transient.prune_expired_objects(now_ms) {
            report.transient_objects_deleted = report.transient_objects_deleted.saturating_add(1);
            report.transient_object_bytes_deleted =
                report.transient_object_bytes_deleted.saturating_add(removal.content_length);
            deletions.push(CacheObjectDeletion::TransientObject(removal.key));
        }

        while let Some(proxy_seq) =
            duration_expired_head_segment(session, &ProtectedSet::from_session(session), policy, now_ms)
        {
            if let Some(deletion) = remove_segment_entry(session, proxy_seq) {
                report.segments_deleted_duration = report.segments_deleted_duration.saturating_add(1);
                deletions.push(CacheObjectDeletion::Segment(deletion));
            }
        }

        let mut session_size = session_cache_size(session);
        while session_size > policy.cache_bytes_per_session {
            let Some(removal) = session.transient.remove_oldest_ready_object() else {
                break;
            };
            session_size = session_size.saturating_sub(removal.content_length);
            report.transient_objects_deleted = report.transient_objects_deleted.saturating_add(1);
            report.transient_object_bytes_deleted =
                report.transient_object_bytes_deleted.saturating_add(removal.content_length);
            deletions.push(CacheObjectDeletion::TransientObject(removal.key));
        }
        while session_size > policy.cache_bytes_per_session {
            let Some(candidate) = fifo_head_size_candidate(session, &ProtectedSet::from_session(session)) else {
                break;
            };
            session_size = session_size.saturating_sub(candidate.content_length);
            if let Some(deletion) = remove_segment_entry(session, candidate.proxy_seq) {
                report.segments_deleted_size_session = report.segments_deleted_size_session.saturating_add(1);
                deletions.push(CacheObjectDeletion::Segment(deletion));
            }
        }

        for map_id in unprotected_unreferenced_map_ids(session, &ProtectedSet::from_session(session)) {
            if let Some(deletion) = remove_map_entry(session, map_id) {
                session_size = session_size.saturating_sub(deletion.content_length);
                report.maps_deleted = report.maps_deleted.saturating_add(1);
                deletions.push(CacheObjectDeletion::Map(deletion.key));
            }
        }

        deletions
    }

    async fn collect_global_size_deletions(
        &self,
        sessions: &[HlsSessionHandle],
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
    ) -> Vec<CacheObjectDeletion> {
        let mut total_size = total_sessions_cache_size(sessions).await;
        let mut deletions = Vec::new();

        loop {
            if total_size <= policy.cache_bytes_global {
                break;
            }
            let Some(candidate) = oldest_global_transient_object_candidate(sessions).await else {
                break;
            };
            let mut session = candidate.session.write().await;
            let Some(removal) = session.transient.remove_oldest_ready_object() else {
                continue;
            };
            total_size = total_size.saturating_sub(removal.content_length);
            report.transient_objects_deleted = report.transient_objects_deleted.saturating_add(1);
            report.transient_object_bytes_deleted =
                report.transient_object_bytes_deleted.saturating_add(removal.content_length);
            deletions.push(CacheObjectDeletion::TransientObject(removal.key));
        }

        loop {
            if total_size <= policy.cache_bytes_global {
                break;
            }
            let Some(candidate) = oldest_global_fifo_head_candidate(sessions).await else {
                break;
            };
            let mut session = candidate.session.write().await;
            let Some(current_head) = fifo_head_size_candidate(&session, &ProtectedSet::from_session(&session)) else {
                continue;
            };
            if current_head.proxy_seq != candidate.proxy_seq {
                continue;
            }
            let Some(deletion) = remove_segment_entry(&mut session, candidate.proxy_seq) else {
                continue;
            };
            total_size = total_size.saturating_sub(candidate.content_length);
            report.segments_deleted_size_global = report.segments_deleted_size_global.saturating_add(1);
            deletions.push(CacheObjectDeletion::Segment(deletion));

            for map_id in unprotected_unreferenced_map_ids(&session, &ProtectedSet::from_session(&session)) {
                if let Some(deletion) = remove_map_entry(&mut session, map_id) {
                    total_size = total_size.saturating_sub(deletion.content_length);
                    report.maps_deleted = report.maps_deleted.saturating_add(1);
                    deletions.push(CacheObjectDeletion::Map(deletion.key));
                }
            }
        }
        deletions
    }

    async fn remove_idle_session_if_still_idle(
        &self,
        session: &HlsSessionHandle,
        now_ms: u64,
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
    ) -> io::Result<()> {
        let (key, proxy_session_id) = {
            let mut session = session.write().await;
            if !Self::should_remove_idle_session(&session, now_ms, policy) {
                return Ok(());
            }
            session.mark_for_gc_removal();
            (session.key.clone(), session.proxy_session_id.clone())
        };

        if self.cache.has_active_temp_files_for_session(&proxy_session_id).await {
            session.write().await.clear_gc_removal_mark();
            return Ok(());
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
            self.cache.delete_session_dir(&proxy_session_id).await?;
            report.sessions_deleted = report.sessions_deleted.saturating_add(1);
            report.removed_session_ids.push(proxy_session_id);
        } else {
            session.write().await.clear_gc_removal_mark();
        }
        Ok(())
    }

    async fn delete_cache_objects(&self, deletions: Vec<CacheObjectDeletion>) -> io::Result<()> {
        for deletion in deletions {
            match deletion {
                CacheObjectDeletion::Segment(key) => {
                    info!(
                        "Segment '{:06}' removed: session={} source=normal",
                        key.proxy_seq(),
                        safe_proxy_session_id(key.proxy_session_id()),
                    );
                    self.cache.delete(&key).await?;
                }
                CacheObjectDeletion::Map(key) => {
                    self.cache.delete(&key).await?;
                }
                CacheObjectDeletion::TransientObject(key) => {
                    self.cache.delete(&key).await?;
                }
            }
        }
        Ok(())
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

pub fn build_rewrite_secret_fingerprint(rewrite_secret: &[u8]) -> String {
    let digest = Sha256::digest(rewrite_secret);
    digest[..8].iter().fold(String::with_capacity(16), |mut fingerprint, byte| {
        write!(fingerprint, "{byte:02x}").expect("writing to String must not fail");
        fingerprint
    })
}

pub fn exec_hls_cache_gc(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let hls_proxy = Arc::clone(&app_state.hls_proxy);
    let active_users = Arc::clone(&app_state.active_users);
    let active_provider = Arc::clone(&app_state.active_provider);
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

#[derive(Clone)]
enum CacheObjectDeletion {
    Segment(SegmentCacheKey),
    Map(MapCacheKey),
    TransientObject(TransientObjectCacheKey),
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
    proxy_seq: u64,
    content_length: u64,
    last_relevant_at_ms: u64,
}

#[derive(Clone)]
struct GlobalTransientObjectCandidate {
    session: HlsSessionHandle,
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

async fn oldest_global_fifo_head_candidate(sessions: &[HlsSessionHandle]) -> Option<GlobalSegmentCandidate> {
    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if let Some(candidate) = fifo_head_size_candidate(&session_guard, &ProtectedSet::from_session(&session_guard)) {
            candidates.push(GlobalSegmentCandidate {
                session: Arc::clone(session),
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
) -> Option<GlobalTransientObjectCandidate> {
    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        candidates.extend(session_guard.transient.object_cache.values().filter_map(|entry| {
            entry.ready_content_length()?;
            if entry.access.active_readers() > 0 {
                return None;
            }
            Some(GlobalTransientObjectCandidate {
                session: Arc::clone(session),
                last_accessed_at_ms: entry.last_accessed_at_ms,
            })
        }));
    }
    candidates.into_iter().min_by_key(|candidate| candidate.last_accessed_at_ms)
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
        SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. } | SegmentCacheStatus::Fetching { .. } => {
            return None
        }
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
        SegmentCacheStatus::Discovered | SegmentCacheStatus::Queued { .. } | SegmentCacheStatus::Fetching { .. } => {
            None
        }
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

fn remove_segment_entry(session: &mut HlsSession, proxy_seq: u64) -> Option<SegmentCacheKey> {
    let segment = session.segments.remove(&proxy_seq)?;
    session.segment_prefetch_queue.remove(proxy_seq);
    session.origin_to_proxy.retain(|_, mapped_seq| *mapped_seq != proxy_seq);
    if segment.discontinuity_before {
        session.discontinuity_sequence = session.discontinuity_sequence.saturating_add(1);
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
    let content_length = match map.status {
        MapCacheStatus::Ready { content_length, .. } => content_length,
        _ => 0,
    };
    session.origin_map_to_proxy.retain(|_, mapped_map_id| *mapped_map_id != map_id);
    Some(MapEntryDeletion { key: map.cache_key, content_length })
}

#[cfg(test)]
mod tests {
    use super::{
        build_rewrite_secret_fingerprint, GarbageCollectionPolicy, GarbageCollectionReport, HlsGarbageCollector,
    };
    use crate::{
        api::model::{
            HlsSegmentCache, HlsSessionKey, HlsSessionStore, MapCacheStatus, OriginMapKey, ProxyMapId,
            SegmentCacheStatus, SegmentFetchPriority, TransientPassthroughState, TransientResourceId,
        },
        processing::parser::hls::origin_manifest::{parse_origin_media_manifest, OriginManifestParseOutcome},
    };
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;

    const BASE_URL: &str = "http://origin.example.com/live/final/index.m3u8";

    fn normal_manifest(body: &str) -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
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

    async fn gc_with_session(
        temp_dir: &tempfile::TempDir,
    ) -> (HlsGarbageCollector, crate::api::model::HlsSessionHandle) {
        let sessions = Arc::new(HlsSessionStore::new());
        let cache = Arc::new(HlsSegmentCache::with_cache_path(temp_dir.path()));
        let session = sessions.get_or_create_session(HlsSessionKey::new(1, "12345"), b"secret", 0).await;
        let gc = HlsGarbageCollector::new(sessions, cache, test_policy(), build_rewrite_secret_fingerprint(b"secret"));
        (gc, session)
    }

    fn update_gc_policy(gc: &HlsGarbageCollector, update: impl FnOnce(&mut GarbageCollectionPolicy)) {
        let mut policy = gc.policy().as_ref().clone();
        update(&mut policy);
        gc.update_config(policy, gc.rewrite_secret_fingerprint());
    }

    fn six_segment_manifest() -> crate::processing::parser::hls::origin_manifest::ParsedOriginManifest {
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

    async fn populate_ready_segments(
        gc: &HlsGarbageCollector,
        session: &crate::api::model::HlsSessionHandle,
        ready_at_ms: u64,
    ) {
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
            let unreferenced_map = crate::api::model::MapEntry::new(
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
            let resource = crate::api::model::TransientResourceRef::new(
                crate::api::model::TransientResourceKind::Segment,
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
            let resource = crate::api::model::TransientResourceRef::new(
                crate::api::model::TransientResourceKind::Segment,
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

    #[tokio::test]
    async fn transient_object_cache_expires_by_cache_policy() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        let cache_key = {
            let mut session = session.write().await;
            let key = TransientPassthroughState::transient_object_key(
                &session.proxy_session_id,
                &TransientResourceId("object1".to_string()),
                "ts",
            );
            gc.cache.write_bytes_and_commit(&key, b"transient-body").await.expect("object writes");
            session.transient.mark_object_ready(&key, "video/mp2t".to_string(), 14, 0, 10);
            key
        };

        let report = gc.run_once(20).await.expect("gc should run");

        assert_eq!(report.transient_objects_deleted, 1);
        assert_eq!(report.transient_object_bytes_deleted, 14);
        assert!(gc.cache.metadata(&cache_key).await.expect("metadata read").is_none());
    }

    #[tokio::test]
    async fn session_size_gc_deletes_transient_objects_before_timeline_segments() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let (gc, session) = gc_with_session(&temp_dir).await;
        update_gc_policy(&gc, |policy| policy.cache_bytes_per_session = 20);
        let cache_key = {
            let mut session = session.write().await;
            apply_six_segment_manifest_for_gc(&mut session);
            let segment = session.segments.get_mut(&1).expect("segment");
            gc.cache.write_bytes_and_commit(&segment.cache_key, b"segment-body").await.expect("segment writes");
            segment.status = SegmentCacheStatus::Ready { content_length: 12, ready_at_ms: 100 };
            let key = TransientPassthroughState::transient_object_key(
                &session.proxy_session_id,
                &TransientResourceId("object1".to_string()),
                "ts",
            );
            gc.cache.write_bytes_and_commit(&key, b"transient-body").await.expect("object writes");
            session.transient.mark_object_ready(&key, "video/mp2t".to_string(), 14, 100, 10_000);
            key
        };

        let report = gc.run_once(100).await.expect("gc should run");

        assert_eq!(report.transient_objects_deleted, 1);
        assert_eq!(report.segments_deleted_size_session, 0);
        let session = session.read().await;
        assert!(session.segments.contains_key(&1));
        assert!(!session.transient.object_cache.contains_key(&cache_key));
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
        let (mut writer, reader) = tokio::io::duplex(64);
        let cache = Arc::clone(&gc.cache);
        let task_key = cache_key.clone();
        let write_task = tokio::spawn(async move { cache.write_temp_and_commit(&task_key, reader).await });
        for _ in 0..50 {
            if gc.cache.has_active_temp_files().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gc.cache.has_active_temp_files().await);

        let report = gc.run_once(1).await.expect("gc should run");

        assert!(!report.secret_cache_invalidated);
        assert!(report.secret_cache_invalidation_deferred);
        assert_eq!(gc.sessions.len().await, 1);
        assert_eq!(gc.cache.read_rewrite_secret_fingerprint().await.expect("marker read").as_deref(), Some("mismatch"));
        assert!(gc.cache.has_active_temp_files().await);
        writer.write_all(b"done").await.expect("write temp body");
        drop(writer);
        write_task.await.expect("temp write task joins").expect("temp write commits");
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
        let (mut writer, reader) = tokio::io::duplex(64);
        let cache = Arc::clone(&gc.cache);
        let task_key = cache_key.clone();
        let write_task = tokio::spawn(async move { cache.write_temp_and_commit(&task_key, reader).await });
        for _ in 0..50 {
            if gc.cache.has_active_temp_files().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gc.run_once(1).await.expect("first gc should run").secret_cache_invalidation_deferred);
        writer.write_all(b"done").await.expect("write temp body");
        drop(writer);
        write_task.await.expect("temp write task joins").expect("temp write commits");

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
        let (mut writer, reader) = tokio::io::duplex(64);
        let cache = Arc::clone(&gc.cache);
        let write_task = tokio::spawn(async move { cache.write_temp_and_commit(&cache_key, reader).await });
        for _ in 0..50 {
            if gc.cache.has_active_temp_files_for_session(&proxy_session_id).await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(gc.cache.has_active_temp_files_for_session(&proxy_session_id).await);

        let report = gc.run_once(2_000).await.expect("gc should run");

        assert_eq!(report.sessions_deleted, 0);
        assert!(!session.read().await.is_gc_marked_for_removal());
        assert_eq!(gc.sessions.len().await, 1);
        writer.write_all(b"done").await.expect("write temp body");
        drop(writer);
        write_task.await.expect("temp write task joins").expect("temp write commits");
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

        gc.remove_idle_session_if_still_idle(&session, 2_000, &policy, &mut report)
            .await
            .expect("session removal check should run");

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
            let resource = crate::api::model::TransientResourceRef::new(
                crate::api::model::TransientResourceKind::Segment,
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
