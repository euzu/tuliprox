use super::{
    safe_proxy_session_id, HlsAccessLeaseSessionSnapshot, HlsExpiredSessionMarker, HlsProxyManager, HlsSessionHandle,
    ProxySessionId,
};
use crate::sync_ext::MutexExt;
use log::debug;
use std::{
    io,
    sync::{atomic::Ordering, Arc},
};
use tuliprox_session::{ActiveProviderManager, ActiveUserManager};

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub(super) struct HlsProxySessionCleanupStats {
    access_leases: usize,
    repair_windows: usize,
    repair_generations: usize,
    repair_candidates: usize,
    repair_object_metadata: usize,
    repair_watchdog_metadata: usize,
    repair_watchdog_locks: usize,
    qos_access_leases: usize,
}

impl HlsProxySessionCleanupStats {
    fn did_cleanup(self) -> bool {
        self.access_leases > 0
            || self.repair_windows > 0
            || self.repair_generations > 0
            || self.repair_candidates > 0
            || self.repair_object_metadata > 0
            || self.repair_watchdog_metadata > 0
            || self.repair_watchdog_locks > 0
            || self.qos_access_leases > 0
    }
}

impl HlsProxyManager {
    pub fn reserve_switch_segment_cleanup(
        &self,
        key: super::super::SegmentCacheKey,
    ) -> Option<super::super::gc::HlsSwitchCacheCleanupReservation> {
        self.gc.reserve_switch_segment_cleanup(key)
    }

    pub fn reserve_switch_map_cleanup(
        &self,
        key: super::super::MapCacheKey,
    ) -> Option<super::super::gc::HlsSwitchCacheCleanupReservation> {
        self.gc.reserve_switch_map_cleanup(key)
    }

    pub fn has_pending_switch_cleanup(
        &self,
        segment_key: &super::super::SegmentCacheKey,
        map_key: Option<&super::super::MapCacheKey>,
    ) -> bool {
        self.gc.has_pending_switch_cleanup(segment_key, map_key)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn cache_deletion_queue_usage(&self) -> (usize, usize) { self.gc.cache_deletion_queue_usage() }

    pub(super) async fn cleanup_proxy_session_state(
        &self,
        proxy_session_id: &ProxySessionId,
        reason: &'static str,
    ) -> HlsProxySessionCleanupStats {
        self.availability_reevaluations.cancel_session(proxy_session_id);
        self.terminal_pending.cancel_session(proxy_session_id);
        self.terminal_commit_retries.cancel_session(proxy_session_id);
        let before = self.segment_repair.stats().await;
        let removed_leases = self.access_leases.write().await.remove_access_leases_for_session(proxy_session_id);
        let username = removed_leases.first().map(|lease| lease.username.clone());
        self.sessions.update_expired_session_marker_username(proxy_session_id, username).await;
        let removed_lease_ids = removed_leases.iter().map(|lease| lease.lease_id.clone()).collect::<Vec<_>>();
        for lease_id in &removed_lease_ids {
            self.standalone_custom_access.remove(lease_id);
        }
        self.segment_repair.remove_proxy_session_state(proxy_session_id, &removed_lease_ids).await;
        for lease_id in &removed_lease_ids {
            self.startup_observability.remove_access_lease(lease_id);
        }
        let removed_qos = self.qos.remove_access_leases(&removed_lease_ids).await;
        let removed_qos = removed_qos.saturating_add(self.qos.remove_proxy_session_state(proxy_session_id).await);
        let after = self.segment_repair.stats().await;
        let stats = HlsProxySessionCleanupStats {
            access_leases: removed_lease_ids.len(),
            repair_windows: before.windows.saturating_sub(after.windows),
            repair_generations: before.generations.saturating_sub(after.generations),
            repair_candidates: before.checked_candidates.saturating_sub(after.checked_candidates),
            repair_object_metadata: before.object_metadata.saturating_sub(after.object_metadata),
            repair_watchdog_metadata: before.watchdog_metadata.saturating_sub(after.watchdog_metadata),
            repair_watchdog_locks: before.watchdog_locks.saturating_sub(after.watchdog_locks),
            qos_access_leases: removed_qos,
        };
        if stats.did_cleanup() {
            debug!(
                "HLS proxy session state cleaned: proxy_session={} reason={} access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_object_metadata={} repair_watchdog_metadata={} repair_watchdog_locks={} qos_access_leases={}",
                safe_proxy_session_id(proxy_session_id),
                reason,
                stats.access_leases,
                stats.repair_windows,
                stats.repair_generations,
                stats.repair_candidates,
                stats.repair_object_metadata,
                stats.repair_watchdog_metadata,
                stats.repair_watchdog_locks,
                stats.qos_access_leases
            );
        }
        stats
    }

    pub async fn expired_session_marker(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsExpiredSessionMarker> {
        self.sessions
            .expired_session_marker(proxy_session_id, now_ms, self.session_idle_timeout_ms().saturating_mul(2).max(1))
            .await
    }

    async fn cleanup_all_runtime_state(&self, reason: &'static str) {
        self.availability_reevaluations.clear();
        self.terminal_pending.clear();
        self.terminal_commit_retries.clear();
        let removed_access_leases = self.access_leases.write().await.clear();
        self.standalone_custom_access.clear();
        self.terminal_commit_retries.clear();
        for session in self.sessions.list_sessions().await {
            session.write().await.clear_terminal_tail_protections();
        }
        self.account_overlap_cooldowns.write().await.clear();
        let removed_qos = self.qos.clear().await;
        let before = self.segment_repair.stats().await;
        self.segment_repair.clear_runtime_state().await;
        self.startup_observability.clear();
        if removed_access_leases > 0
            || before.windows > 0
            || before.generations > 0
            || before.checked_candidates > 0
            || before.metadata > 0
            || before.object_metadata > 0
            || before.locks > 0
            || before.watchdog_metadata > 0
            || before.watchdog_locks > 0
            || removed_qos > 0
        {
            debug!(
                "HLS runtime state cleaned: reason={} access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_metadata={} repair_object_metadata={} repair_locks={} repair_watchdog_metadata={} repair_watchdog_locks={} qos_access_leases={}",
                reason,
                removed_access_leases,
                before.windows,
                before.generations,
                before.checked_candidates,
                before.metadata,
                before.object_metadata,
                before.locks,
                before.watchdog_metadata,
                before.watchdog_locks,
                removed_qos
            );
        }
    }

    async fn cleanup_after_garbage_collection(&self, report: &super::super::GarbageCollectionReport) {
        if report.secret_cache_invalidated {
            self.cleanup_all_runtime_state("secret-cache-invalidated").await;
            return;
        }
        for proxy_session_id in &report.removed_session_ids {
            self.cleanup_proxy_session_state(proxy_session_id, "gc-session-removed").await;
        }
    }

    pub async fn run_garbage_collection_once(&self, now_ms: u64) -> io::Result<super::super::GarbageCollectionReport> {
        if !self.is_enabled() {
            return Ok(super::super::GarbageCollectionReport::default());
        }
        let report = self.gc.run_once(now_ms).await?;
        self.cleanup_after_garbage_collection(&report).await;
        if self.revision_store.has_created_revisions() {
            for handle in self.sessions.list_sessions().await {
                let snapshot = {
                    let mut session = handle.write().await;
                    let super::super::HlsSession { segments, startup, proxy_session_id, .. } = &mut *session;
                    startup.as_mut().map(|startup| {
                        startup.revisions.retain(|seq, _| segments.contains_key(seq));
                        (proxy_session_id.clone(), segments.first_key_value().map_or(u64::MAX, |(seq, _)| *seq))
                    })
                };
                if let Some((id, floor)) = snapshot {
                    self.access_leases.write().await.prune_revision_bindings(&id, floor);
                }
            }
        }
        if self.revision_reconciliation_pending.load(Ordering::Acquire) {
            let deleted = self.segment_cache.delete_orphan_revision_files(std::time::SystemTime::now()).await?;
            if deleted < super::super::cache::MAX_TEMP_FILE_CLEANUP_CANDIDATES_PER_RUN {
                self.revision_reconciliation_pending.store(false, Ordering::Release);
            }
        }
        for revision in self.revision_store.retire_unpinned()? {
            revision.file_pin.lock_unpoisoned().take();
            if self.segment_cache.delete_if_inactive(&revision.key).await.is_err() {
                self.revision_store.requeue_retired(revision)?;
            }
        }
        if self.revision_store.has_created_revisions() {
            let bytes = self.segment_cache.revision_disk_bytes();
            self.metrics.record_revision_disk_bytes(bytes);
            let (fills, replay_bytes, repairs) = self.progressive_budget.usage_snapshot();
            self.metrics.record_progressive_usage(fills, replay_bytes, repairs);
            debug!(
                "HLS progressive usage: active_fills={fills} replay_bytes={replay_bytes} deferred_repairs={repairs}"
            );
            debug!("HLS revision disk usage: hls_revision_disk_bytes={bytes}");
        }
        Ok(report)
    }

    pub async fn sync_session_access_lease_count_and_detach_if_needed(
        &self,
        active_users: &Arc<ActiveUserManager>,
        _active_provider: &Arc<ActiveProviderManager>,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) {
        let snapshot = self.reconcile_session_access_lease_snapshot(session, proxy_session_id, now_ms).await;
        for release in &snapshot.idle_releases {
            active_users
                .release_session_streams_and_counted_reservation(&release.username, &release.user_session_token)
                .await;
            debug!(
                "HLS access lease idled: lease={} proxy_session={} user_session={}",
                super::super::safe_hls_access_lease_id(&release.lease_id),
                safe_proxy_session_id(proxy_session_id),
                super::super::safe_user_session_token(&release.user_session_token)
            );
        }
    }

    pub(super) async fn reconcile_session_access_lease_snapshot(
        &self,
        session: &HlsSessionHandle,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> HlsAccessLeaseSessionSnapshot {
        // Keep snapshot creation and application in the established lease-store -> session
        // lock transaction. Otherwise a publication/removal can be overwritten by a stale
        // wholesale binding reconciliation.
        let mut leases = self.access_leases.write().await;
        let snapshot = leases.session_snapshot(proxy_session_id, now_ms);
        let mut session = session.write().await;
        session.activity.active_access_lease_count = snapshot.active_count;
        session.reconcile_effective_origin_acquire_policy(snapshot.effective_origin_policy, now_ms);
        session.transient.reconcile_finalized_manifest_lease_bindings(&snapshot.finalized_transient_manifest_bindings);
        snapshot
    }

    pub async fn sync_all_session_access_leases_and_detach_if_needed(
        &self,
        active_users: &Arc<ActiveUserManager>,
        active_provider: &Arc<ActiveProviderManager>,
        now_ms: u64,
    ) {
        if !self.is_enabled() {
            return;
        }
        for session in self.sessions.list_sessions().await {
            let proxy_session_id = session.read().await.proxy_session_id.clone();
            self.sync_session_access_lease_count_and_detach_if_needed(
                active_users,
                active_provider,
                &session,
                &proxy_session_id,
                now_ms,
            )
            .await;
        }
    }
}
