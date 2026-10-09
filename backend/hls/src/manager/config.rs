use super::{
    build_rewrite_secret_fingerprint, GarbageCollectionPolicy, HlsAccessLeaseStore,
    HlsAvailabilityReevaluationCoordinator, HlsCacheMetrics, HlsGarbageCollector, HlsLifecycleManager,
    HlsMapWorkerPool, HlsPreparedTerminalBundleCache, HlsProxyManager, HlsQosRegistry, HlsSegmentCache,
    HlsSegmentRepairManager, HlsSegmentWorkerPool, HlsSessionStore, HlsStandaloneCustomAccessStore,
    HlsStartupObservability, HlsTerminalCommitClock, HlsTerminalCommitRetryCoordinator, HlsTerminalPendingCoordinator,
    SegmentFetchPolicy, TransientResourceStore,
};
use arc_swap::ArcSwap;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::{RwLock, Semaphore};
use tuliprox_core::model::{AppConfig, HlsCacheConfig, HlsManifestRecoveryBurstConfig, StripConfig};

#[derive(Debug, Clone)]
pub(super) struct HlsProxyRuntimeConfig {
    pub(super) enabled: bool,
    pub(super) segment_fetch_policy: SegmentFetchPolicy,
    pub(super) cache_duration_seconds: u64,
    pub(super) strip: StripConfig,
    pub(super) origin_manifest_timeout_ms: u64,
    pub(super) initial_manifest_wait_timeout_secs: u64,
    pub(super) manifest_recovery_burst: HlsManifestRecoveryBurstConfig,
    pub(super) transient_resource_ttl_ms: u64,
    pub(super) gc_policy: GarbageCollectionPolicy,
    pub(super) rewrite_secret_fingerprint: String,
    pub(super) startup: tuliprox_core::model::HlsStartupConfig,
}

impl HlsProxyRuntimeConfig {
    pub(super) fn from_config(config: &HlsCacheConfig, rewrite_secret: &[u8]) -> Self {
        Self::from_config_with_enabled(config, rewrite_secret, true)
    }

    pub(super) fn from_config_with_enabled(config: &HlsCacheConfig, rewrite_secret: &[u8], enabled: bool) -> Self {
        Self {
            enabled,
            segment_fetch_policy: SegmentFetchPolicy::from_config(config),
            cache_duration_seconds: config.cache_duration.get(),
            strip: config.strip.clone(),
            origin_manifest_timeout_ms: config.origin_manifest_timeout_ms.get(),
            initial_manifest_wait_timeout_secs: config.initial_manifest_wait_timeout_secs.get(),
            manifest_recovery_burst: config.manifest_recovery_burst.clone(),
            transient_resource_ttl_ms: config.cache_duration.as_millis().get(),
            gc_policy: GarbageCollectionPolicy::from_config(config),
            rewrite_secret_fingerprint: build_rewrite_secret_fingerprint(rewrite_secret),
            startup: config.startup.clone(),
        }
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
        let default_config;
        let (config, enabled) = if let Some(config) = config {
            (config, true)
        } else {
            default_config = HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default());
            (&default_config, false)
        };
        Self::with_hls_cache_config_and_secret_enabled(config, rewrite_secret, enabled)
    }

    pub fn with_cache_settings(cache_path: impl Into<PathBuf>, cache_duration_seconds: u64) -> Self {
        let default_dto = shared::model::HlsCacheConfigDto {
            cache_duration: shared::model::Secs::new(cache_duration_seconds),
            cache_path: Some(cache_path.into().to_string_lossy().to_string()),
            ..Default::default()
        };
        let default_config = HlsCacheConfig::from(&default_dto);
        let segment_fetch_policy = SegmentFetchPolicy::from_config(&default_config);
        let global_fetch_semaphore = Arc::new(Semaphore::new(segment_fetch_policy.max_global_segment_fetches));
        let sessions = Arc::new(HlsSessionStore::new());
        let segment_cache = Arc::new(HlsSegmentCache::with_cache_path(PathBuf::from(&default_config.cache_path)));
        segment_cache
            .update_cache_limits(default_config.cache_bytes.get(), default_config.cache_bytes_per_session.get());
        let segment_repair = Arc::new(HlsSegmentRepairManager::new(default_config.segment_repair.clone()));
        let metrics = Arc::new(HlsCacheMetrics::default());
        let qos = Arc::new(HlsQosRegistry::default());
        let access_leases = Arc::new(RwLock::new(HlsAccessLeaseStore::default()));
        let lifecycle = Arc::new(HlsLifecycleManager::new());
        let account_overlap_cooldowns = Arc::new(RwLock::new(HashMap::new()));
        let gc_policy = GarbageCollectionPolicy::from_config(&default_config);
        let runtime_config = HlsProxyRuntimeConfig::from_config(&default_config, &[]);
        let gc = Arc::new(HlsGarbageCollector::new_with_metrics(
            Arc::clone(&sessions),
            Arc::clone(&segment_cache),
            gc_policy.clone(),
            runtime_config.rewrite_secret_fingerprint.clone(),
            Arc::clone(&metrics),
        ));
        segment_cache.install_capacity_reclaimer(&gc);
        gc.install_access_leases(&access_leases);
        let availability_reevaluations = Arc::new(HlsAvailabilityReevaluationCoordinator::default());
        Self {
            sessions,
            segment_cache,
            segment_repair,
            segment_worker_pool: Arc::new(HlsSegmentWorkerPool::with_global_semaphore_metrics_and_availability(
                segment_fetch_policy.clone(),
                Arc::clone(&global_fetch_semaphore),
                Arc::clone(&access_leases),
                Arc::clone(&metrics),
                Some(Arc::clone(&availability_reevaluations)),
            )),
            map_worker_pool: Arc::new(HlsMapWorkerPool::with_global_semaphore_access_leases_and_availability(
                segment_fetch_policy.clone(),
                global_fetch_semaphore,
                Arc::clone(&access_leases),
                Some(Arc::clone(&availability_reevaluations)),
            )),
            progressive_budget: super::super::ProgressiveBudgetManager::new(runtime_config.startup.clone()),
            revision_store: Arc::new(super::super::SegmentRevisionStore::default()),
            revision_reconciliation_pending: AtomicBool::new(true),
            runtime_config: ArcSwap::from_pointee(runtime_config),
            transient_resources: Arc::new(TransientResourceStore::new()),
            access_leases,
            lifecycle,
            account_overlap_cooldowns,
            metrics,
            qos,
            gc,
            prepared_terminal_bundles: Arc::new(HlsPreparedTerminalBundleCache::new()),
            standalone_custom_access: Arc::new(HlsStandaloneCustomAccessStore::default()),
            terminal_commit_retries: Arc::new(HlsTerminalCommitRetryCoordinator::default()),
            terminal_pending: Arc::new(HlsTerminalPendingCoordinator::default()),
            availability_reevaluations,
            terminal_commit_clock: Arc::new(HlsTerminalCommitClock::default()),
            startup_observability: Arc::new(HlsStartupObservability::default()),
        }
    }

    pub fn segment_fetch_policy(&self) -> SegmentFetchPolicy { self.runtime_config.load().segment_fetch_policy.clone() }

    pub fn is_enabled(&self) -> bool { self.runtime_config.load().enabled }

    pub fn cache_duration_seconds(&self) -> u64 { self.runtime_config.load().cache_duration_seconds }

    pub fn session_idle_timeout_ms(&self) -> u64 { self.runtime_config.load().gc_policy.session_idle_timeout_ms }

    pub fn strip(&self) -> StripConfig { self.runtime_config.load().strip.clone() }

    pub fn origin_manifest_timeout_ms(&self) -> u64 { self.runtime_config.load().origin_manifest_timeout_ms }

    pub fn initial_manifest_wait_timeout_secs(&self) -> u64 {
        self.runtime_config.load().initial_manifest_wait_timeout_secs
    }

    pub fn manifest_recovery_burst(&self) -> HlsManifestRecoveryBurstConfig {
        self.runtime_config.load().manifest_recovery_burst.clone()
    }

    pub fn transient_resource_ttl_ms(&self) -> u64 { self.runtime_config.load().transient_resource_ttl_ms }

    pub fn gc_policy(&self) -> GarbageCollectionPolicy { self.runtime_config.load().gc_policy.clone() }

    pub fn rewrite_secret_fingerprint(&self) -> String { self.runtime_config.load().rewrite_secret_fingerprint.clone() }

    pub async fn update_config(&self, app_config: &AppConfig) {
        let (hls_config, rewrite_secret, enabled) = {
            let config = app_config.config.load();
            let rewrite_secret = config
                .reverse_proxy
                .as_ref()
                .map_or(app_config.encrypt_secret, |reverse_proxy| reverse_proxy.rewrite_secret);
            let hls_config =
                config.reverse_proxy.as_ref().and_then(|reverse_proxy| reverse_proxy.hls_cache.as_ref()).cloned();
            let enabled = hls_config.is_some();
            let hls_config =
                hls_config.unwrap_or_else(|| HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default()));
            (hls_config, rewrite_secret, enabled)
        };
        let runtime_config = HlsProxyRuntimeConfig::from_config_with_enabled(&hls_config, &rewrite_secret, enabled);
        let cache_path_changed = self.gc.update_cache_path(PathBuf::from(&hls_config.cache_path)).await;
        self.progressive_budget.update_limits(hls_config.startup.clone());
        self.segment_cache.update_cache_limits(hls_config.cache_bytes.get(), hls_config.cache_bytes_per_session.get());
        if cache_path_changed {
            self.revision_reconciliation_pending.store(true, Ordering::Release);
            self.clear_runtime_cache_state_for_cache_path_change().await;
        }
        for session in self.sessions.list_sessions().await {
            session
                .write()
                .await
                .configure_segment_prefetch_queue(runtime_config.segment_fetch_policy.max_prefetch_queue_depth);
        }
        self.segment_repair.update_config(hls_config.segment_repair.clone());
        let global_fetch_semaphore =
            Arc::new(Semaphore::new(runtime_config.segment_fetch_policy.max_global_segment_fetches));
        self.segment_worker_pool
            .update_config(runtime_config.segment_fetch_policy.clone(), Arc::clone(&global_fetch_semaphore));
        self.map_worker_pool.update_config(runtime_config.segment_fetch_policy.clone(), global_fetch_semaphore);
        self.gc.update_config(runtime_config.gc_policy.clone(), runtime_config.rewrite_secret_fingerprint.clone());
        self.runtime_config.store(Arc::new(runtime_config));
    }
}
