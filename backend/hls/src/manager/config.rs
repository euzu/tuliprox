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
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::{RwLock, Semaphore};
use tuliprox_core::model::{AppConfig, Config, HlsCacheConfig, HlsManifestRecoveryBurstConfig, StripConfig};

#[derive(Debug, Clone)]
pub(super) struct HlsProxyRuntimeConfig {
    pub(super) enabled: bool,
    pub(super) segment_fetch_policy: SegmentFetchPolicy,
    pub(super) cache_duration_seconds: u64,
    pub(super) strip: StripConfig,
    pub(super) origin_manifest_timeout_ms: u64,
    pub(super) min_origin_target_duration_secs: shared::model::Secs,
    pub(super) initial_manifest_wait_timeout_secs: u64,
    pub(super) manifest_recovery_burst: HlsManifestRecoveryBurstConfig,
    pub(super) transient_resource_ttl_ms: u64,
    pub(super) gc_policy: GarbageCollectionPolicy,
    pub(super) rewrite_secret_fingerprint: String,
    pub(super) startup: tuliprox_core::model::HlsStartupConfig,
}

impl HlsProxyRuntimeConfig {
    pub(super) fn from_config(config: &HlsCacheConfig, rewrite_secret: &[u8]) -> Self {
        Self::from_config_with_enabled(config, rewrite_secret, true, shared::defaults::default_connect_timeout_secs())
    }

    pub(super) fn from_config_with_enabled(
        config: &HlsCacheConfig,
        rewrite_secret: &[u8],
        enabled: bool,
        connect_timeout_secs: u32,
    ) -> Self {
        Self {
            enabled,
            segment_fetch_policy: SegmentFetchPolicy::from_config_with_connect_timeout(config, connect_timeout_secs),
            cache_duration_seconds: config.cache_duration.get(),
            strip: config.strip.clone(),
            origin_manifest_timeout_ms: config.origin_manifest_timeout_ms.get(),
            min_origin_target_duration_secs: config.min_origin_target_duration_secs,
            initial_manifest_wait_timeout_secs: config.initial_manifest_wait_timeout_secs.get(),
            manifest_recovery_burst: config.manifest_recovery_burst.clone(),
            transient_resource_ttl_ms: config.cache_duration.as_millis().get(),
            gc_policy: GarbageCollectionPolicy::from_config(config),
            rewrite_secret_fingerprint: build_rewrite_secret_fingerprint(rewrite_secret),
            startup: config.startup.clone(),
        }
    }
}

/// Cache directory owned by one manager and removed together with it.
pub(super) struct PrivateCacheDir(PathBuf);

impl PrivateCacheDir {
    fn create() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!("tuliprox-hls-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        Self(std::env::temp_dir().join(name))
    }
}

impl Drop for PrivateCacheDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

impl HlsProxyManager {
    /// Creates a manager with the default cache settings and a cache directory of its own.
    ///
    /// Sharing `DEFAULT_HLS_CACHE_PATH` between managers is unsafe: the orphan-session cleanup of one
    /// manager deletes the session directories of every other manager on that path.
    pub fn new() -> Self {
        let cache_dir = PrivateCacheDir::create();
        let default_dto = shared::model::HlsCacheConfigDto {
            cache_path: Some(cache_dir.0.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let default_config = HlsCacheConfig::from(&default_dto);
        let mut manager = Self::with_hls_cache_config(&default_config);
        manager.private_cache_dir = Some(cache_dir);
        manager
    }

    /// Initializes HLS workers with the same loaded connect setting as the HTTP clients.
    pub fn from_config(config: &Config, default_rewrite_secret: &[u8]) -> Self {
        let proxy = config.reverse_proxy.as_ref();
        let secret = proxy.map_or(default_rewrite_secret, |proxy| proxy.rewrite_secret.as_slice());
        Self::from_hls_cache_settings(
            proxy.and_then(|proxy| proxy.hls_cache.as_ref()),
            secret,
            config.connect_timeout_secs,
        )
    }

    pub fn from_hls_cache_config(config: Option<&HlsCacheConfig>) -> Self {
        Self::from_hls_cache_config_and_secret(config, &[])
    }

    pub fn from_hls_cache_config_and_secret(config: Option<&HlsCacheConfig>, rewrite_secret: &[u8]) -> Self {
        Self::from_hls_cache_settings(config, rewrite_secret, shared::defaults::default_connect_timeout_secs())
    }

    fn from_hls_cache_settings(
        config: Option<&HlsCacheConfig>,
        rewrite_secret: &[u8],
        connect_timeout_secs: u32,
    ) -> Self {
        let default_config;
        let (config, enabled) = if let Some(config) = config {
            (config, true)
        } else {
            default_config = HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default());
            (&default_config, false)
        };
        Self::with_hls_cache_config_and_secret_enabled(config, rewrite_secret, enabled, connect_timeout_secs)
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
        let mut runtime_config = HlsProxyRuntimeConfig::from_config(&default_config, &[]);
        runtime_config.segment_fetch_policy.timeouts = segment_fetch_policy.timeouts.clone();
        let gc = Arc::new(HlsGarbageCollector::new_with_metrics(
            Arc::clone(&sessions),
            Arc::clone(&segment_cache),
            gc_policy.clone(),
            runtime_config.rewrite_secret_fingerprint.clone(),
            Arc::clone(&metrics),
        ));
        segment_cache.install_capacity_reclaimer(&gc);
        gc.install_access_leases(&access_leases);
        let revision_store = Arc::new(super::super::SegmentRevisionStore::default());
        gc.install_revision_store(&revision_store);
        segment_cache.install_revision_metrics(&metrics);
        let availability_reevaluations = Arc::new(HlsAvailabilityReevaluationCoordinator::default());
        let manager = Self {
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
            revision_store,
            revision_reconciliation_pending: AtomicBool::new(true),
            runtime_config: ArcSwap::from_pointee(runtime_config),
            transient_resources: Arc::new(TransientResourceStore::new()),
            access_leases,
            lifecycle,
            lease_completion: Arc::new(super::lease_completion::HlsLeaseCompletion::default()),
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
            private_cache_dir: None,
        };
        manager.install_worker_revision_recovery();
        manager
    }

    pub fn segment_fetch_policy(&self) -> SegmentFetchPolicy { self.runtime_config.load().segment_fetch_policy.clone() }

    pub fn is_enabled(&self) -> bool { self.runtime_config.load().enabled }

    pub fn cache_duration_seconds(&self) -> u64 { self.runtime_config.load().cache_duration_seconds }

    pub fn session_idle_timeout_ms(&self) -> u64 { self.runtime_config.load().gc_policy.session_idle_timeout_ms }

    pub fn strip(&self) -> StripConfig { self.runtime_config.load().strip.clone() }

    pub fn origin_manifest_timeout_ms(&self) -> u64 { self.runtime_config.load().origin_manifest_timeout_ms }

    pub fn min_origin_target_duration_secs(&self) -> shared::model::Secs {
        self.runtime_config.load().min_origin_target_duration_secs
    }

    /// Applies the validated preference for future qualifications. Existing
    /// session snapshots retain the threshold with which they started.
    pub fn update_min_origin_target_duration_secs(&self, minimum: shared::model::Secs) {
        self.runtime_config.rcu(|current| {
            let mut next = current.as_ref().clone();
            next.min_origin_target_duration_secs = minimum;
            Arc::new(next)
        });
    }

    /// Applies validated HLS preferences to future attempts without replacing worker slots.
    pub fn update_origin_preferences(
        &self,
        minimum: shared::model::Secs,
        segment: shared::model::HlsSegmentTimeout,
        manifest_ms: u64,
    ) {
        let policy = self.segment_fetch_policy();
        let mut timeouts = policy.timeouts.config();
        timeouts.segment = segment;
        timeouts.manifest_ms = manifest_ms;
        policy.timeouts.update(timeouts);
        self.runtime_config.rcu(|current| {
            let mut next = current.as_ref().clone();
            next.min_origin_target_duration_secs = minimum;
            next.origin_manifest_timeout_ms = manifest_ms;
            Arc::new(next)
        });
    }

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
        let (hls_config, rewrite_secret, enabled, connect_timeout_secs) = {
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
            (hls_config, rewrite_secret, enabled, config.connect_timeout_secs)
        };
        let mut runtime_config = HlsProxyRuntimeConfig::from_config_with_enabled(
            &hls_config,
            &rewrite_secret,
            enabled,
            connect_timeout_secs,
        );
        let shared_timeouts = self.segment_fetch_policy().timeouts;
        let next_timeouts = runtime_config.segment_fetch_policy.timeouts.config();
        shared_timeouts.update(next_timeouts);
        runtime_config.segment_fetch_policy.timeouts = shared_timeouts;
        let cache_path_changed = self.gc.update_cache_path(PathBuf::from(&hls_config.cache_path)).await;
        self.progressive_budget.update_limits(hls_config.startup.clone());
        self.segment_cache.update_cache_limits(hls_config.cache_bytes.get(), hls_config.cache_bytes_per_session.get());
        if cache_path_changed {
            self.revision_reconciliation_pending.store(true, Ordering::Release);
            self.clear_runtime_cache_state_for_cache_path_change().await;
        }
        for session in self.sessions.list_sessions().await {
            let mut session = session.write().await;
            session.configure_segment_prefetch_queue(runtime_config.segment_fetch_policy.max_prefetch_queue_depth);
            session.live_window_strip.clone_from(&runtime_config.strip);
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
