use super::{
    build_rewrite_secret_fingerprint, safe_proxy_session_id, safe_session_key, AccessLeaseReuseResult,
    GarbageCollectionPolicy, HlsAccessLease, HlsAccessLeaseActivation, HlsAccessLeaseId,
    HlsAccessLeaseLifecycleSnapshot, HlsAccessLeaseSessionSnapshot, HlsAccessLeaseState, HlsAccessLeaseStore,
    HlsAccessLeaseTiming, HlsAccessLeaseTouch, HlsCacheMetrics, HlsGarbageCollector, HlsLifecycleEvent,
    HlsLifecycleEventKey, HlsLifecycleManager, HlsMapWorkerPool, HlsOriginSource, HlsPlaybackFamilyKey,
    HlsSegmentCache, HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionHandle, HlsSessionKey, HlsSessionStore,
    HlsSessionStoreOutcome, ProxySessionId, SegmentFetchPolicy, TransientResourceStore,
};
use crate::{
    api::model::{ActiveProviderManager, ActiveUserManager, AppState},
    model::{AppConfig, HlsCacheConfig, StripConfig},
};
use arc_swap::ArcSwap;
use log::{debug, error, info};
use std::{io, path::PathBuf, sync::Arc};
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

/// Root runtime object for the future HLS cache proxy.
pub struct HlsProxyManager {
    sessions: Arc<HlsSessionStore>,
    segment_cache: Arc<HlsSegmentCache>,
    segment_repair: Arc<HlsSegmentRepairManager>,
    segment_worker_pool: Arc<HlsSegmentWorkerPool>,
    map_worker_pool: Arc<HlsMapWorkerPool>,
    runtime_config: ArcSwap<HlsProxyRuntimeConfig>,
    transient_resources: Arc<TransientResourceStore>,
    access_leases: Arc<RwLock<HlsAccessLeaseStore>>,
    lifecycle: Arc<HlsLifecycleManager>,
    metrics: Arc<HlsCacheMetrics>,
    gc: Arc<HlsGarbageCollector>,
}

#[derive(Debug, Clone)]
struct HlsProxyRuntimeConfig {
    segment_fetch_policy: SegmentFetchPolicy,
    cache_duration_seconds: u64,
    strip: StripConfig,
    origin_manifest_timeout_ms: u64,
    transient_resource_ttl_ms: u64,
    gc_policy: GarbageCollectionPolicy,
    rewrite_secret_fingerprint: String,
}

impl HlsProxyRuntimeConfig {
    fn from_config(config: &HlsCacheConfig, rewrite_secret: &[u8]) -> Self {
        Self {
            segment_fetch_policy: SegmentFetchPolicy::from_config(config),
            cache_duration_seconds: config.cache_duration,
            strip: config.strip.clone(),
            origin_manifest_timeout_ms: config.origin_manifest_timeout_ms,
            transient_resource_ttl_ms: config.session_idle_timeout.saturating_mul(1_000),
            gc_policy: GarbageCollectionPolicy::from_config(config),
            rewrite_secret_fingerprint: build_rewrite_secret_fingerprint(rewrite_secret),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
struct HlsProxySessionCleanupStats {
    access_leases: usize,
    repair_windows: usize,
    repair_generations: usize,
    repair_candidates: usize,
    repair_object_metadata: usize,
    repair_watchdog_metadata: usize,
    repair_watchdog_locks: usize,
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
    }
}

impl HlsProxyManager {
    pub fn new() -> Self {
        let default_dto = shared::model::HlsCacheConfigDto::default();
        let default_config = HlsCacheConfig::from(&default_dto);
        Self::with_hls_cache_config(&default_config)
    }

    pub fn from_hls_cache_config(config: Option<&HlsCacheConfig>) -> Self {
        Self::from_hls_cache_config_and_secret(config, &[])
    }

    pub fn from_hls_cache_config_and_secret(config: Option<&HlsCacheConfig>, rewrite_secret: &[u8]) -> Self {
        match config {
            Some(config) => Self::with_hls_cache_config_and_secret(config, rewrite_secret),
            None => Self::with_hls_cache_config_and_secret(
                &HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default()),
                rewrite_secret,
            ),
        }
    }

    pub fn with_cache_settings(cache_path: impl Into<PathBuf>, cache_duration_seconds: u64) -> Self {
        let default_dto = shared::model::HlsCacheConfigDto {
            cache_duration: cache_duration_seconds,
            cache_path: cache_path.into().to_string_lossy().to_string(),
            ..Default::default()
        };
        let default_config = HlsCacheConfig::from(&default_dto);
        let segment_fetch_policy = SegmentFetchPolicy::from_config(&default_config);
        let global_fetch_semaphore = Arc::new(Semaphore::new(segment_fetch_policy.max_global_segment_fetches));
        let sessions = Arc::new(HlsSessionStore::new());
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(PathBuf::from(&default_config.cache_path)));
        let segment_repair = Arc::new(HlsSegmentRepairManager::new(default_config.segment_repair.clone()));
        let metrics = Arc::new(HlsCacheMetrics::default());
        let access_leases = Arc::new(RwLock::new(HlsAccessLeaseStore::default()));
        let lifecycle = Arc::new(HlsLifecycleManager::new());
        let gc_policy = GarbageCollectionPolicy::from_config(&default_config);
        let runtime_config = HlsProxyRuntimeConfig::from_config(&default_config, &[]);
        let gc = Arc::new(HlsGarbageCollector::new_with_metrics(
            Arc::clone(&sessions),
            Arc::clone(&segment_cache),
            gc_policy.clone(),
            runtime_config.rewrite_secret_fingerprint.clone(),
            Arc::clone(&metrics),
        ));
        Self {
            sessions,
            segment_cache,
            segment_repair,
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::with_global_semaphore_and_metrics(
                segment_fetch_policy.clone(),
                Arc::clone(&global_fetch_semaphore),
                Arc::clone(&access_leases),
                Arc::clone(&metrics),
            )),
            map_worker_pool: Arc::new(HlsMapWorkerPool::with_global_semaphore_and_access_leases(
                segment_fetch_policy.clone(),
                global_fetch_semaphore,
                Arc::clone(&access_leases),
            )),
            runtime_config: ArcSwap::from_pointee(runtime_config),
            transient_resources: Arc::new(TransientResourceStore::new()),
            access_leases,
            lifecycle,
            metrics,
            gc,
        }
    }

    pub fn with_hls_cache_config(config: &HlsCacheConfig) -> Self {
        Self::with_hls_cache_config_and_secret(config, &[])
    }

    pub fn with_hls_cache_config_and_secret(config: &HlsCacheConfig, rewrite_secret: &[u8]) -> Self {
        let segment_fetch_policy = SegmentFetchPolicy::from_config(config);
        let global_fetch_semaphore = Arc::new(Semaphore::new(segment_fetch_policy.max_global_segment_fetches));
        let sessions = Arc::new(HlsSessionStore::new());
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(PathBuf::from(&config.cache_path)));
        let segment_repair = Arc::new(HlsSegmentRepairManager::new(config.segment_repair.clone()));
        let metrics = Arc::new(HlsCacheMetrics::default());
        let access_leases = Arc::new(RwLock::new(HlsAccessLeaseStore::default()));
        let lifecycle = Arc::new(HlsLifecycleManager::new());
        let gc_policy = GarbageCollectionPolicy::from_config(config);
        let runtime_config = HlsProxyRuntimeConfig::from_config(config, rewrite_secret);
        let gc = Arc::new(HlsGarbageCollector::new_with_metrics(
            Arc::clone(&sessions),
            Arc::clone(&segment_cache),
            gc_policy.clone(),
            runtime_config.rewrite_secret_fingerprint.clone(),
            Arc::clone(&metrics),
        ));
        Self {
            sessions,
            segment_cache,
            segment_repair,
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::with_global_semaphore_and_metrics(
                segment_fetch_policy.clone(),
                Arc::clone(&global_fetch_semaphore),
                Arc::clone(&access_leases),
                Arc::clone(&metrics),
            )),
            map_worker_pool: Arc::new(HlsMapWorkerPool::with_global_semaphore_and_access_leases(
                segment_fetch_policy.clone(),
                global_fetch_semaphore,
                Arc::clone(&access_leases),
            )),
            runtime_config: ArcSwap::from_pointee(runtime_config),
            transient_resources: Arc::new(TransientResourceStore::new()),
            access_leases,
            lifecycle,
            metrics,
            gc,
        }
    }

    pub fn sessions(&self) -> &Arc<HlsSessionStore> { &self.sessions }

    pub fn segment_cache(&self) -> &Arc<HlsSegmentCache> { &self.segment_cache }

    pub fn segment_repair(&self) -> &Arc<HlsSegmentRepairManager> { &self.segment_repair }

    pub fn segment_worker_pool(&self) -> &Arc<HlsSegmentWorkerPool> { &self.segment_worker_pool }

    pub fn map_worker_pool(&self) -> &Arc<HlsMapWorkerPool> { &self.map_worker_pool }

    pub fn segment_fetch_policy(&self) -> SegmentFetchPolicy { self.runtime_config.load().segment_fetch_policy.clone() }

    pub fn cache_duration_seconds(&self) -> u64 { self.runtime_config.load().cache_duration_seconds }

    pub fn session_idle_timeout_ms(&self) -> u64 { self.runtime_config.load().gc_policy.session_idle_timeout_ms }

    pub fn strip(&self) -> StripConfig { self.runtime_config.load().strip.clone() }

    pub fn origin_manifest_timeout_ms(&self) -> u64 { self.runtime_config.load().origin_manifest_timeout_ms }

    pub fn transient_resource_ttl_ms(&self) -> u64 { self.runtime_config.load().transient_resource_ttl_ms }

    pub fn transient_resources(&self) -> &Arc<TransientResourceStore> { &self.transient_resources }

    pub fn access_leases(&self) -> &Arc<RwLock<HlsAccessLeaseStore>> { &self.access_leases }

    pub fn lifecycle(&self) -> &Arc<HlsLifecycleManager> { &self.lifecycle }

    pub fn metrics(&self) -> &Arc<HlsCacheMetrics> { &self.metrics }

    pub fn garbage_collector(&self) -> &Arc<HlsGarbageCollector> { &self.gc }

    pub fn gc_policy(&self) -> GarbageCollectionPolicy { self.runtime_config.load().gc_policy.clone() }

    pub fn rewrite_secret_fingerprint(&self) -> String { self.runtime_config.load().rewrite_secret_fingerprint.clone() }

    pub async fn update_config(&self, app_config: &AppConfig) {
        let (hls_config, rewrite_secret) = {
            let config = app_config.config.load();
            let rewrite_secret = config
                .reverse_proxy
                .as_ref()
                .map_or(app_config.encrypt_secret, |reverse_proxy| reverse_proxy.rewrite_secret);
            let hls_config = config
                .reverse_proxy
                .as_ref()
                .and_then(|reverse_proxy| reverse_proxy.hls_cache.as_ref())
                .cloned()
                .unwrap_or_else(|| HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default()));
            (hls_config, rewrite_secret)
        };
        let runtime_config = HlsProxyRuntimeConfig::from_config(&hls_config, &rewrite_secret);
        let cache_path_changed = self.segment_cache.update_cache_path(PathBuf::from(&hls_config.cache_path));
        if cache_path_changed {
            self.clear_runtime_cache_state_for_cache_path_change().await;
        }
        for session in self.sessions.list_sessions().await {
            session
                .write()
                .await
                .configure_segment_prefetch_queue(runtime_config.segment_fetch_policy.max_prefetch_queue_depth);
        }
        self.segment_repair.update_config(hls_config.segment_repair.clone());
        let global_fetch_semaphore = Arc::new(Semaphore::new(runtime_config.segment_fetch_policy.max_global_segment_fetches));
        self.segment_worker_pool
            .update_config(runtime_config.segment_fetch_policy.clone(), Arc::clone(&global_fetch_semaphore));
        self.map_worker_pool
            .update_config(runtime_config.segment_fetch_policy.clone(), global_fetch_semaphore);
        self.gc
            .update_config(runtime_config.gc_policy.clone(), runtime_config.rewrite_secret_fingerprint.clone());
        self.runtime_config.store(Arc::new(runtime_config));
    }

    async fn clear_runtime_cache_state_for_cache_path_change(&self) {
        self.sessions.clear().await;
        let removed_leases = self.access_leases.write().await.clear();
        self.segment_repair.clear_runtime_state().await;
        debug!("HLS cache runtime state cleared after cache path change: access_leases_removed={removed_leases}");
    }

    pub async fn prepare_access_lease(&self, lease: HlsAccessLease) {
        self.schedule_access_lease_validity(&lease).await;
        self.access_leases.write().await.prepare_access_lease(lease);
    }

    pub async fn find_reusable_access_lease(
        &self,
        family_key: &HlsPlaybackFamilyKey,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> AccessLeaseReuseResult {
        self.access_leases.write().await.find_reusable_access_lease(family_key, proxy_session_id, now_ms)
    }

    pub async fn access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Option<HlsAccessLease> {
        let (lease, still_stored) = {
            let mut access_leases = self.access_leases.write().await;
            let lease = access_leases.access_lease(lease_id, proxy_session_id, now_ms);
            let still_stored = access_leases.lease_state(lease_id, now_ms).is_some();
            (lease, still_stored)
        };
        if lease.is_none() && !still_stored {
            self.segment_repair.remove_access_lease_window(lease_id).await;
        }
        lease
    }

    pub async fn update_access_lease_origin_acquire_policy(
        &self,
        lease_id: &HlsAccessLeaseId,
        connection_kind: crate::api::model::ConnectionKind,
        priority: i8,
    ) -> Option<HlsAccessLease> {
        self.access_leases.write().await.update_origin_acquire_policy(lease_id, connection_kind, priority)
    }

    pub async fn activate_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> HlsAccessLeaseActivation {
        let activation =
            self.access_leases.write().await.activate_access_lease(lease_id, proxy_session_id, now_ms, timing);
        if let HlsAccessLeaseActivation::Activated { lease, previous_state } = &activation {
            if matches!(previous_state, HlsAccessLeaseState::Pending | HlsAccessLeaseState::Idle) {
                self.segment_repair.start_access_lease_window(lease.lease_id.clone()).await;
            }
            self.schedule_access_lease_activity(lease).await;
            self.schedule_access_lease_validity(lease).await;
        }
        activation
    }

    pub async fn touch_manifest_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
        active_timing: Option<HlsAccessLeaseTiming>,
        ttl_ms: u64,
    ) -> HlsAccessLeaseTouch {
        let touch = self.access_leases.write().await.touch_manifest_access_lease(
            lease_id,
            proxy_session_id,
            now_ms,
            active_timing,
            ttl_ms,
        );
        if let HlsAccessLeaseTouch::Touched { lease } = &touch {
            if lease.state == HlsAccessLeaseState::Activated {
                self.schedule_access_lease_activity(lease).await;
            }
            self.schedule_access_lease_validity(lease).await;
        }
        touch
    }

    pub async fn touch_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> bool {
        let lease = self.access_leases.write().await.touch_access_lease_snapshot(lease_id, now_ms, timing);
        if let Some(lease) = lease {
            self.schedule_access_lease_activity(&lease).await;
            self.schedule_access_lease_validity(&lease).await;
            true
        } else {
            false
        }
    }

    pub async fn active_access_lease_count_for_session(&self, proxy_session_id: &ProxySessionId, now_ms: u64) -> usize {
        self.access_leases.write().await.active_access_lease_count_for_session(proxy_session_id, now_ms)
    }

    pub async fn has_usable_access_lease_for_session(&self, proxy_session_id: &ProxySessionId, now_ms: u64) -> bool {
        self.access_leases.write().await.has_usable_access_lease_for_session(proxy_session_id, now_ms)
    }

    pub async fn access_lease_session_snapshot(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> HlsAccessLeaseSessionSnapshot {
        self.access_leases.write().await.session_snapshot(proxy_session_id, now_ms)
    }

    async fn access_lease_lifecycle_snapshot(
        &self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseLifecycleSnapshot> {
        self.access_leases.write().await.lifecycle_snapshot(lease_id, now_ms)
    }

    async fn remove_access_lease(&self, lease_id: &HlsAccessLeaseId) {
        self.access_leases.write().await.remove_access_lease(lease_id);
        self.segment_repair.remove_access_lease_window(lease_id).await;
    }

    async fn cleanup_proxy_session_state(
        &self,
        proxy_session_id: &ProxySessionId,
        reason: &'static str,
    ) -> HlsProxySessionCleanupStats {
        let before = self.segment_repair.stats().await;
        let removed_lease_ids = self.access_leases.write().await.remove_access_leases_for_session(proxy_session_id);
        self.segment_repair.remove_proxy_session_state(proxy_session_id, &removed_lease_ids).await;
        let after = self.segment_repair.stats().await;
        let stats = HlsProxySessionCleanupStats {
            access_leases: removed_lease_ids.len(),
            repair_windows: before.windows.saturating_sub(after.windows),
            repair_generations: before.generations.saturating_sub(after.generations),
            repair_candidates: before.checked_candidates.saturating_sub(after.checked_candidates),
            repair_object_metadata: before.object_metadata.saturating_sub(after.object_metadata),
            repair_watchdog_metadata: before.watchdog_metadata.saturating_sub(after.watchdog_metadata),
            repair_watchdog_locks: before.watchdog_locks.saturating_sub(after.watchdog_locks),
        };
        if stats.did_cleanup() {
            debug!(
                "HLS proxy session state cleaned: session={} reason={} access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_object_metadata={} repair_watchdog_metadata={} repair_watchdog_locks={}",
                safe_proxy_session_id(proxy_session_id),
                reason,
                stats.access_leases,
                stats.repair_windows,
                stats.repair_generations,
                stats.repair_candidates,
                stats.repair_object_metadata,
                stats.repair_watchdog_metadata,
                stats.repair_watchdog_locks
            );
        }
        stats
    }

    async fn cleanup_all_runtime_state(&self, reason: &'static str) {
        let removed_access_leases = self.access_leases.write().await.clear();
        let before = self.segment_repair.stats().await;
        self.segment_repair.clear_runtime_state().await;
        if removed_access_leases > 0
            || before.windows > 0
            || before.generations > 0
            || before.checked_candidates > 0
            || before.metadata > 0
            || before.object_metadata > 0
            || before.locks > 0
            || before.watchdog_metadata > 0
            || before.watchdog_locks > 0
        {
            debug!(
                "HLS runtime state cleaned: reason={} access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_metadata={} repair_object_metadata={} repair_locks={} repair_watchdog_metadata={} repair_watchdog_locks={}",
                reason,
                removed_access_leases,
                before.windows,
                before.generations,
                before.checked_candidates,
                before.metadata,
                before.object_metadata,
                before.locks,
                before.watchdog_metadata,
                before.watchdog_locks
            );
        }
    }

    async fn cleanup_after_garbage_collection(&self, report: &super::GarbageCollectionReport) {
        if report.secret_cache_invalidated {
            self.cleanup_all_runtime_state("secret-cache-invalidated").await;
            return;
        }
        for proxy_session_id in &report.removed_session_ids {
            self.cleanup_proxy_session_state(proxy_session_id, "gc-session-removed").await;
        }
    }

    async fn schedule_access_lease_activity(&self, lease: &HlsAccessLease) {
        if let Some(active_until_ms) = lease.active_until_ms {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::AccessLeaseActive {
                        lease_id: lease.lease_id.clone(),
                        proxy_session_id: lease.proxy_session_id.clone(),
                    },
                    active_until_ms,
                )
                .await;
        }
    }

    async fn schedule_access_lease_validity(&self, lease: &HlsAccessLease) {
        self.lifecycle
            .schedule(
                HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: lease.lease_id.clone(),
                    proxy_session_id: lease.proxy_session_id.clone(),
                },
                lease.valid_until_ms,
            )
            .await;
    }

    async fn schedule_access_lease_lifecycle_snapshot(&self, snapshot: &HlsAccessLeaseLifecycleSnapshot) {
        if snapshot.state == HlsAccessLeaseState::Activated {
            if let Some(active_until_ms) = snapshot.active_until_ms {
                self.lifecycle
                    .schedule(
                        HlsLifecycleEventKey::AccessLeaseActive {
                            lease_id: snapshot.lease_id.clone(),
                            proxy_session_id: snapshot.proxy_session_id.clone(),
                        },
                        active_until_ms,
                    )
                    .await;
            }
        }
        if snapshot.state != HlsAccessLeaseState::Expired && snapshot.state != HlsAccessLeaseState::Denied {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: snapshot.lease_id.clone(),
                        proxy_session_id: snapshot.proxy_session_id.clone(),
                    },
                    snapshot.valid_until_ms,
                )
                .await;
        }
    }

    pub async fn schedule_session_idle_for_handle(&self, session: &HlsSessionHandle) {
        let session_idle_timeout_ms = self.session_idle_timeout_ms();
        let (proxy_session_id, due_at_ms) = {
            let session = session.read().await;
            (session.proxy_session_id.clone(), session.idle_expiry_due_at_ms(session_idle_timeout_ms))
        };
        self.lifecycle.schedule(HlsLifecycleEventKey::SessionIdle { proxy_session_id }, due_at_ms).await;
    }

    pub async fn handle_lifecycle_event(
        &self,
        active_users: &Arc<ActiveUserManager>,
        active_provider: &Arc<ActiveProviderManager>,
        event: HlsLifecycleEvent,
        now_ms: u64,
    ) {
        match event.key {
            HlsLifecycleEventKey::AccessLeaseActive { lease_id, proxy_session_id }
            | HlsLifecycleEventKey::AccessLeaseValidity { lease_id, proxy_session_id } => {
                if let Some(session) = self.sessions.get_by_proxy_session_id(&proxy_session_id).await {
                    self.sync_session_access_lease_count_and_detach_if_needed(
                        active_users,
                        active_provider,
                        &session,
                        &proxy_session_id,
                        now_ms,
                    )
                    .await;
                }
                if let Some(snapshot) = self.access_lease_lifecycle_snapshot(&lease_id, now_ms).await {
                    if let Some(release) = &snapshot.idle_release {
                        active_users
                            .release_session_streams_and_counted_reservation(
                                &release.username,
                                &release.user_session_token,
                            )
                            .await;
                        debug!(
                            "HLS access lease idled: lease={} proxy_session={} session={}",
                            super::safe_hls_access_lease_id(&release.lease_id),
                            safe_proxy_session_id(&snapshot.proxy_session_id),
                            super::safe_user_session_token(&release.user_session_token)
                        );
                    }
                    if matches!(snapshot.state, HlsAccessLeaseState::Expired | HlsAccessLeaseState::Denied) {
                        self.remove_access_lease(&snapshot.lease_id).await;
                        debug!(
                            "HLS access lease removed: lease={} proxy_session={} state={}",
                            super::safe_hls_access_lease_id(&snapshot.lease_id),
                            safe_proxy_session_id(&snapshot.proxy_session_id),
                            snapshot.state.as_log_value()
                        );
                        debug!("HLS lifecycle state snapshot: trigger=access-lease-removed {}", self.debug_state_summary().await);
                    } else {
                        self.schedule_access_lease_lifecycle_snapshot(&snapshot).await;
                    }
                }
            }
            HlsLifecycleEventKey::SessionIdle { proxy_session_id } => {
                self.handle_session_idle_lifecycle_event(&proxy_session_id, now_ms).await;
            }
        }
    }

    async fn handle_session_idle_lifecycle_event(&self, proxy_session_id: &ProxySessionId, now_ms: u64) {
        let Some(session) = self.sessions.get_by_proxy_session_id(proxy_session_id).await else {
            return;
        };
        let session_idle_timeout_ms = self.session_idle_timeout_ms();
        let (key, due_at_ms, can_remove) = {
            let session = session.read().await;
            (
                session.key.clone(),
                session.idle_expiry_due_at_ms(session_idle_timeout_ms),
                session.can_expire_idle_session(now_ms, session_idle_timeout_ms),
            )
        };
        if !can_remove {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    due_at_ms.max(now_ms.saturating_add(1)),
                )
                .await;
            return;
        }
        if self.segment_cache.has_active_temp_files_for_session(proxy_session_id).await {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    now_ms.saturating_add(1_000),
                )
                .await;
            return;
        }
        if self.sessions.remove_session(&key, proxy_session_id).await.is_some() {
            self.cleanup_proxy_session_state(proxy_session_id, "lifecycle-session-expired").await;
            if let Err(err) = self.segment_cache.delete_session_dir(proxy_session_id).await {
                error!(
                    "HLS session lifecycle cleanup failed: session={} error={err}",
                    safe_proxy_session_id(proxy_session_id)
                );
            } else {
                debug!("HLS session lifecycle expired: session={}", safe_proxy_session_id(proxy_session_id));
                debug!("HLS lifecycle state snapshot: trigger=session-expired {}", self.debug_state_summary().await);
            }
        }
    }

    pub async fn debug_state_summary(&self) -> String {
        let sessions = self.sessions.list_sessions().await;
        let access_leases = self.access_leases.read().await.len();
        let repair = self.segment_repair.stats().await;
        let mut segments = 0_usize;
        let mut maps = 0_usize;
        let mut transient_resources = 0_usize;
        let mut transient_objects = 0_usize;
        let mut active_origin_work = 0_usize;
        let mut active_segment_fetches = 0_usize;
        let mut active_map_fetches = 0_usize;
        for session in &sessions {
            let session = session.read().await;
            segments = segments.saturating_add(session.segments.len());
            maps = maps.saturating_add(session.maps.len());
            transient_resources = transient_resources.saturating_add(session.transient.resources.len());
            transient_objects = transient_objects.saturating_add(session.transient.object_cache.len());
            active_origin_work = active_origin_work.saturating_add(session.activity.active_origin_work_count);
            active_segment_fetches = active_segment_fetches.saturating_add(session.active_segment_fetches);
            active_map_fetches = active_map_fetches.saturating_add(session.active_map_fetches);
        }
        format!(
            "sessions={} access_leases={} repair_windows={} repair_generations={} repair_candidates={} repair_metadata={} repair_object_metadata={} repair_locks={} repair_watchdog_metadata={} repair_watchdog_locks={} segments={} maps={} transient_resources={} transient_objects={} active_origin_work={} active_segment_fetches={} active_map_fetches={}",
            sessions.len(),
            access_leases,
            repair.windows,
            repair.generations,
            repair.checked_candidates,
            repair.metadata,
            repair.object_metadata,
            repair.locks,
            repair.watchdog_metadata,
            repair.watchdog_locks,
            segments,
            maps,
            transient_resources,
            transient_objects,
            active_origin_work,
            active_segment_fetches,
            active_map_fetches
        )
    }

    pub async fn run_garbage_collection_once(&self, now_ms: u64) -> io::Result<super::GarbageCollectionReport> {
        let report = self.gc.run_once(now_ms).await?;
        self.cleanup_after_garbage_collection(&report).await;
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
        let snapshot = self.access_lease_session_snapshot(proxy_session_id, now_ms).await;
        for release in &snapshot.idle_releases {
            active_users
                .release_session_streams_and_counted_reservation(&release.username, &release.user_session_token)
                .await;
            debug!(
                "HLS access lease idled: lease={} proxy_session={} session={}",
                super::safe_hls_access_lease_id(&release.lease_id),
                safe_proxy_session_id(proxy_session_id),
                super::safe_user_session_token(&release.user_session_token)
            );
        }
        {
            let mut session = session.write().await;
            session.activity.active_access_lease_count = snapshot.active_count;
            session.reconcile_effective_origin_acquire_policy(snapshot.effective_origin_policy, now_ms);
        }
    }

    pub async fn sync_all_session_access_leases_and_detach_if_needed(
        &self,
        active_users: &Arc<ActiveUserManager>,
        active_provider: &Arc<ActiveProviderManager>,
        now_ms: u64,
    ) {
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

    pub async fn deny_access_lease(&self, lease_id: &HlsAccessLeaseId) {
        self.access_leases.write().await.deny_access_lease(lease_id);
    }

    pub async fn get_or_create_session(
        &self,
        key: HlsSessionKey,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> HlsSessionHandle {
        self.get_or_create_session_with_outcome(key, reverse_proxy_rewrite_secret, now_ms).await.0
    }

    pub async fn get_or_create_session_with_outcome(
        &self,
        key: HlsSessionKey,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> (HlsSessionHandle, HlsSessionStoreOutcome) {
        let origin_source = HlsOriginSource::from_session_key(&key);
        self.get_or_create_session_with_source_and_outcome(key, origin_source, reverse_proxy_rewrite_secret, now_ms)
            .await
    }

    pub async fn get_or_create_session_with_source_and_outcome(
        &self,
        key: HlsSessionKey,
        origin_source: HlsOriginSource,
        reverse_proxy_rewrite_secret: &[u8],
        now_ms: u64,
    ) -> (HlsSessionHandle, HlsSessionStoreOutcome) {
        let (session, outcome) = self
            .sessions
            .get_or_create_session_with_source_and_outcome(key, origin_source, reverse_proxy_rewrite_secret, now_ms)
            .await;
        let (proxy_session_id, session_key) = {
            let session_guard = session.read().await;
            (safe_proxy_session_id(&session_guard.proxy_session_id), safe_session_key(&session_guard.key))
        };
        match outcome {
            HlsSessionStoreOutcome::Created => {
                self.metrics.record_session_created();
                info!("HLS session created: session={session_key} proxy_session_id={proxy_session_id}");
            }
            HlsSessionStoreOutcome::Reused => {
                self.metrics.record_session_reused();
                debug!("HLS session reused: session={session_key} proxy_session_id={proxy_session_id}");
            }
        }
        self.schedule_session_idle_for_handle(&session).await;
        session
            .write()
            .await
            .configure_segment_prefetch_queue(self.segment_fetch_policy().max_prefetch_queue_depth);
        (session, outcome)
    }
}

impl Default for HlsProxyManager {
    fn default() -> Self { Self::new() }
}

pub fn exec_hls_lifecycle(app_state: &Arc<AppState>, cancel_token: &CancellationToken) {
    let hls_proxy = Arc::clone(&app_state.hls_proxy);
    let active_users = Arc::clone(&app_state.active_users);
    let active_provider = Arc::clone(&app_state.active_provider);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        while let Some(event) = hls_proxy.lifecycle().next_event(&cancel_token).await {
            let now_ms = current_time_millis();
            hls_proxy.handle_lifecycle_event(&active_users, &active_provider, event, now_ms).await;
        }
    });
}

fn current_time_millis() -> u64 { chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::HlsProxyManager;
    use crate::{
        api::model::HlsSessionKey,
        model::{
            AppConfig, Config, HlsCacheConfig, MediaToolCapabilities, ReverseProxyConfig, SourcesConfig,
        },
        utils::FileLockManager,
    };
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::model::{ConfigPaths, HlsCacheConfigDto, ReverseProxyConfigDto, StripConfigDto, StripModeDto};
    use std::sync::Arc;

    fn empty_paths() -> ConfigPaths {
        ConfigPaths {
            home_path: String::new(),
            config_path: String::new(),
            storage_path: String::new(),
            config_file_path: String::new(),
            sources_file_path: String::new(),
            mapping_file_path: None,
            mapping_files_used: None,
            template_file_path: None,
            template_files_used: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        }
    }

    fn test_app_config(config: Config) -> AppConfig {
        AppConfig {
            config: Arc::new(ArcSwap::from_pointee(config)),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(empty_paths())),
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [7; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        }
    }

    fn config_with_hls_cache(hls_cache: HlsCacheConfigDto) -> Config {
        Config {
            reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
                hls_cache: Some(hls_cache),
                ..ReverseProxyConfigDto::default()
            })),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn update_config_applies_hls_runtime_settings_to_existing_manager() {
        let initial_dto = HlsCacheConfigDto {
            cache_path: "/tmp/tuliprox/hls-a".to_string(),
            max_segments_prefetch: 1,
            ..Default::default()
        };
        let initial_config = HlsCacheConfig::from(&initial_dto);
        let manager = HlsProxyManager::with_hls_cache_config(&initial_config);
        let (session, _) = manager
            .get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 100)
            .await;

        let mut updated_dto = initial_dto.clone();
        updated_dto.max_segments_prefetch = 4;
        updated_dto.max_concurrent_segment_fetches_per_session = 5;
        updated_dto.max_concurrent_segment_fetches_global = 6;
        updated_dto.origin_manifest_timeout_ms = 1_234;
        updated_dto.origin_segment_timeout_ms = 5_678;
        updated_dto.cache_duration = 99;
        updated_dto.session_idle_timeout = 55;
        updated_dto.strip = StripConfigDto {
            mode: StripModeDto::Seconds,
            value: 7,
        };
        let app_config = test_app_config(config_with_hls_cache(updated_dto));

        manager.update_config(&app_config).await;

        assert_eq!(manager.segment_fetch_policy().max_prefetch_queue_depth, 4);
        assert_eq!(manager.segment_fetch_policy().max_session_segment_fetches, 5);
        assert_eq!(manager.segment_fetch_policy().max_global_segment_fetches, 6);
        assert_eq!(manager.segment_fetch_policy().origin_segment_timeout_ms, 5_678);
        assert_eq!(manager.origin_manifest_timeout_ms(), 1_234);
        assert_eq!(manager.cache_duration_seconds(), 99);
        assert_eq!(manager.session_idle_timeout_ms(), 55_000);
        assert_eq!(manager.strip().mode, crate::model::StripMode::Seconds);
        assert_eq!(manager.strip().value, 7);
        assert_eq!(session.read().await.segment_prefetch_queue.max_prefetch_depth(), 4);
    }

    #[tokio::test]
    async fn update_config_cache_path_change_clears_runtime_cache_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let old_cache = temp_dir.path().join("old");
        let new_cache = temp_dir.path().join("new");
        let initial_dto = HlsCacheConfigDto {
            cache_path: old_cache.to_string_lossy().to_string(),
            ..Default::default()
        };
        let initial_config = HlsCacheConfig::from(&initial_dto);
        let manager = HlsProxyManager::with_hls_cache_config(&initial_config);
        let _ = manager
            .get_or_create_session_with_outcome(HlsSessionKey::new(1, "stream-a"), b"secret", 100)
            .await;
        assert_eq!(manager.sessions().len().await, 1);

        let mut updated_dto = initial_dto;
        updated_dto.cache_path = new_cache.to_string_lossy().to_string();
        let app_config = test_app_config(config_with_hls_cache(updated_dto));

        manager.update_config(&app_config).await;

        assert!(manager.sessions().is_empty().await);
        assert_eq!(manager.segment_cache().cache_path(), new_cache);
    }
}
