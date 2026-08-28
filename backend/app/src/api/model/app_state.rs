use crate::{
    api::{
        endpoints::download_api::{resume_download_worker_if_needed, spawn_download_services},
        model::{
            load_target_into_memory_cache, recording_rule_scheduler::spawn_recording_rule_scheduler,
            ActiveProviderManager, ActiveUserManager, ConnectionManager, DownloadQueue, EventManager,
            HlsProvisioningState, PlaylistStorage, PlaylistStorageState, SharedStreamManager, UpdateGuard,
        },
        tasks::{exec_config_watch, exec_scheduler},
    },
    model::{
        AppConfig, Config, ConfigProvider, ConfigTarget, GracePeriodOptions, HdHomeRunConfig, HdHomeRunDeviceConfig,
        ProcessTargets, ReverseProxyDisabledHeaderConfig, ScheduleConfig, SourcesConfig,
    },
    repository::{get_geoip_path, GeoIp},
    utils::{
        reload_logger,
        request::{create_client, create_client_with_redirect, PublicIpResolver},
        LRUResourceCache,
    },
};
use arc_swap::{ArcSwap, ArcSwapOption};
use log::{error, info};
use reqwest::Client;
use shared::{
    create_bitset,
    error::TuliproxError,
    model::{RecordingConfigDto, UserConnectionPermission, WebAuthConfigDto},
    utils::small_vecs_equal_unordered,
};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicI8, Arc},
    time::Duration,
};
use tokio::{
    sync::{mpsc, RwLock},
    task,
};
use tokio_util::sync::CancellationToken;
use tuliprox_hls::api::HlsProxyManager;
use tuliprox_metadata::manager::MetadataUpdateManager;
use tuliprox_session::{provider_dns_manager::exec_provider_dns, qos_aggregation_manager::exec_qos_aggregation};

macro_rules! cancel_service {
    ($field: ident, $flag:expr, $changes:expr, $cancel_tokens:expr) => {
        if $changes.flags.contains($flag) {
            $cancel_tokens.$field.cancel();
            CancellationToken::default()
        } else {
            $cancel_tokens.$field.clone()
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TargetStatus {
    Old,
    New,
    Keep,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TargetCacheState {
    UnchangedFalse,
    UnchangedTrue,
    ChangedToTrue,
    ChangedToFalse,
}

struct TargetChanges {
    name: String,
    status: TargetStatus,
    cache_status: TargetCacheState,
    target: Arc<ConfigTarget>,
}

create_bitset!(
    u8,
    UpdateChangesFlags,
    Scheduler,
    Hdhomerun,
    FileWatch,
    Geoip,
    ProviderDns,
    Metadata,
    QosAggregation,
    Downloads
);

pub(in crate::api) struct UpdateChanges {
    flags: UpdateChangesFlagsSet,
    targets: Option<HashMap<String, TargetChanges>>,
}

impl UpdateChanges {
    pub(in crate::api) fn modified(&self) -> bool { !self.flags.is_empty() }

    fn set_flag_if(&mut self, condition: bool, flag: UpdateChangesFlags) {
        if condition {
            self.flags.set(flag);
        }
    }
}

async fn update_target_caches(app_state: &Arc<AppState>, target_changes: Option<&HashMap<String, TargetChanges>>) {
    if let Some(target_changes) = target_changes {
        let mut to_remove = Vec::new();
        for target in target_changes.values() {
            match target.status {
                TargetStatus::Old => {
                    to_remove.push(target.name.clone());
                }
                TargetStatus::New // Normally, a new target shouldn't require any updates, but attempting to load it does no harm.
                | TargetStatus::Keep => {
                    match target.cache_status {
                        TargetCacheState::UnchangedFalse | TargetCacheState::UnchangedTrue => {} // skip this
                        TargetCacheState::ChangedToTrue => {
                            load_target_into_memory_cache(&app_state.app_config, &app_state.playlists, &target.target).await;
                        }
                        TargetCacheState::ChangedToFalse => {
                            to_remove.push(target.name.clone());
                        }
                    }
                }
            }
        }
        if !to_remove.is_empty() {
            let mut guard = app_state.playlists.data.write().await;
            for name in to_remove {
                guard.remove(&name);
            }
        }
    }
}

pub async fn update_app_state_config(app_state: &Arc<AppState>, config: Config) -> Result<(), TuliproxError> {
    let updates = app_state.set_config(config).await?;
    restart_services(app_state, &updates);
    Ok(())
}

pub async fn update_app_state_sources(
    app_state: &Arc<AppState>,
    sources: SourcesConfig,
    prevalidated_targets: Option<Arc<ProcessTargets>>,
) -> Result<(), TuliproxError> {
    let targets = if let Some(prevalidated) = prevalidated_targets {
        prevalidated
    } else {
        let targets = sources.validate_targets(Some(&app_state.forced_targets.load().target_names))?;
        Arc::new(targets)
    };
    app_state.forced_targets.store(targets);
    let updates = app_state.set_sources(sources).await?;
    update_target_caches(app_state, updates.targets.as_ref()).await;
    restart_services(app_state, &updates);
    Ok(())
}

fn restart_services(app_state: &Arc<AppState>, changes: &UpdateChanges) {
    if !changes.modified() {
        return;
    }
    cancel_services(app_state, changes);
    start_services(app_state, changes);
}

fn cancel_services(app_state: &Arc<AppState>, changes: &UpdateChanges) {
    if !changes.modified() {
        return;
    }
    if changes.flags.contains(UpdateChangesFlags::Downloads) {
        app_state.downloads.request_worker_restart();
    }
    let cancel_tokens = app_state.cancel_tokens.load();

    let scheduler = cancel_service!(scheduler, UpdateChangesFlags::Scheduler, changes, cancel_tokens);
    let hdhomerun = cancel_service!(hdhomerun, UpdateChangesFlags::Hdhomerun, changes, cancel_tokens);
    let file_watch = cancel_service!(file_watch, UpdateChangesFlags::FileWatch, changes, cancel_tokens);
    let provider_dns = cancel_service!(provider_dns, UpdateChangesFlags::ProviderDns, changes, cancel_tokens);
    let metadata = if changes.flags.contains(UpdateChangesFlags::Metadata) {
        let token = CancellationToken::new();
        app_state.metadata_manager.rotate_cancel_token(token.clone());
        token
    } else {
        cancel_tokens.metadata.clone()
    };
    let qos_aggregation = cancel_service!(qos_aggregation, UpdateChangesFlags::QosAggregation, changes, cancel_tokens);
    let downloads = cancel_service!(downloads, UpdateChangesFlags::Downloads, changes, cancel_tokens);

    let tokens = CancelTokens {
        scheduler,
        hdhomerun,
        file_watch,
        provider_dns,
        metadata,
        qos_aggregation,
        downloads,
        hls_cache: cancel_tokens.hls_cache.clone(),
    };

    app_state.cancel_tokens.store(Arc::new(tokens));
}

fn start_services(app_state: &Arc<AppState>, changes: &UpdateChanges) {
    if !changes.modified() {
        return;
    }
    if changes.flags.contains(UpdateChangesFlags::Scheduler) {
        exec_scheduler(
            &Arc::clone(&app_state.http_client.load()),
            app_state,
            &app_state.cancel_tokens.load().scheduler,
        );
    }

    if changes.flags.contains(UpdateChangesFlags::Hdhomerun) && app_state.app_config.api_proxy.load().is_some() {
        let mut infos = Vec::new();
        crate::api::main_api::start_hdhomerun(
            &app_state.app_config,
            app_state,
            &mut infos,
            &app_state.cancel_tokens.load().hdhomerun,
        );
    }

    if changes.flags.contains(UpdateChangesFlags::FileWatch) {
        exec_config_watch(app_state, &app_state.cancel_tokens.load().file_watch);
    }

    if changes.flags.contains(UpdateChangesFlags::ProviderDns) {
        exec_provider_dns(&app_state.app_config, &app_state.cancel_tokens.load().provider_dns);
    }
    if changes.flags.contains(UpdateChangesFlags::QosAggregation) {
        exec_qos_aggregation(&app_state.app_config, &app_state.cancel_tokens.load().qos_aggregation);
        let history_cfg =
            app_state.app_config.config.load().reverse_proxy.as_ref().and_then(|rp| rp.stream_history.clone());
        let connection_manager = Arc::clone(&app_state.connection_manager);
        tokio::spawn(async move {
            connection_manager.reload_history_writer(history_cfg.as_ref()).await;
        });
    }
    if changes.flags.contains(UpdateChangesFlags::Downloads) {
        spawn_download_services(app_state, &app_state.cancel_tokens.load().downloads);
        spawn_recording_rule_scheduler(&app_state.recording_ctx(), &app_state.cancel_tokens.load().downloads);
        let config = app_state.app_config.config.load();
        if let Some(download_cfg) = config.recording.as_ref().cloned() {
            let app_state = Arc::clone(app_state);
            tokio::spawn(async move {
                for _ in 0..50 {
                    if !*app_state.downloads.worker_running.read().await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if let Err(err) = resume_download_worker_if_needed(app_state.as_ref(), &download_cfg).await {
                    error!("Failed to resume downloads after hot reload: {err}");
                }
            });
        }
    }
}

/// Creates the default HTTP client.
///
/// Fails if proxy configuration is present but the client cannot be built.
pub fn create_http_client(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    let builder = create_client(app_config).http1_only();
    let config = app_config.config.load();
    build_http_client_with_fallback(
        builder,
        &config,
        "Failed to create HTTP client with proxy configuration; refusing to fall back to unconfigured client",
        "HTTP client creation failed with proxy configured",
        "Failed to create HTTP client, using unconfigured http client",
        Client::new,
    )
}

/// Creates a no-redirect HTTP client.
///
/// Fails if proxy configuration is present but the client cannot be built.
///
/// Handling Streaming and Proxy with http/2 is hard, so we strictly use only http/1.1
pub fn create_http_client_no_redirect(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    let builder = create_client_with_redirect(app_config, reqwest::redirect::Policy::none()).http1_only();
    let config = app_config.config.load();
    build_http_client_with_fallback(
        builder,
        &config,
        "Failed to create HTTP client (no redirect) with proxy configuration; refusing to fall back to unconfigured client",
        "HTTP client (no redirect) creation failed with proxy configured",
        "Failed to create HTTP client (no redirect), using unconfigured http client",
        || {
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|err| {
                    error!("Failed to create fallback HTTP client (no redirect): {err}");
                    Client::new()
                })
        },
    )
}

/// Creates a direct no-redirect client whose connection-time resolver rejects
/// non-public destinations. It is intentionally separate from the general client so
/// configured internal providers keep working.
pub fn create_public_http_client_no_redirect(app_config: &AppConfig) -> Result<Client, TuliproxError> {
    let config = app_config.config.load();
    let mut builder = create_client_with_redirect(app_config, reqwest::redirect::Policy::none())
        .no_proxy()
        .dns_resolver(PublicIpResolver)
        .http1_only();
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    builder.build().map_err(|err| TuliproxError::Config(format!("Failed to create public-only HTTP client: {err}")))
}

fn build_http_client_with_fallback(
    mut builder: reqwest::ClientBuilder,
    config: &Arc<Config>,
    proxy_error_log: &str,
    proxy_error_msg: &str,
    fallback_log: &str,
    fallback_client: impl FnOnce() -> Client,
) -> Result<Client, TuliproxError> {
    let proxy_configured = config.proxy.is_some();

    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }

    if let Ok(client) = builder.build() {
        return Ok(client);
    }

    if proxy_configured {
        error!("{proxy_error_log}");
        return Err(TuliproxError::Config(proxy_error_msg.to_string()));
    }

    error!("{fallback_log}");
    Ok(fallback_client())
}

pub fn create_cache(config: &Config) -> Option<Arc<RwLock<LRUResourceCache>>> {
    let lru_cache = config.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref()).and_then(|c| {
        if c.enabled {
            Some(LRUResourceCache::new(c.size, c.directory.as_str()))
        } else {
            None
        }
    });
    let cache_enabled = lru_cache.is_some();
    if cache_enabled {
        info!("Scanning cache");
        if let Some(res_cache) = lru_cache {
            let cache = Arc::new(RwLock::new(res_cache));
            let cache_scanner = Arc::clone(&cache);
            tokio::spawn(async move {
                let scan_result = {
                    let mut cache = cache_scanner.write().await;
                    task::block_in_place(|| cache.scan())
                };
                if let Err(err) = scan_result {
                    error!("Failed to scan cache {err}");
                }
            });
            return Some(cache);
        }
    }
    None
}

pub struct CancelTokens {
    pub(crate) scheduler: CancellationToken,
    pub(crate) hdhomerun: CancellationToken,
    pub(crate) file_watch: CancellationToken,
    pub(crate) provider_dns: CancellationToken,
    pub(crate) metadata: CancellationToken,
    pub(crate) qos_aggregation: CancellationToken,
    pub(crate) downloads: CancellationToken,
    pub(crate) hls_cache: CancellationToken,
}
impl Default for CancelTokens {
    fn default() -> Self {
        Self {
            scheduler: CancellationToken::new(),
            hdhomerun: CancellationToken::new(),
            file_watch: CancellationToken::new(),
            provider_dns: CancellationToken::new(),
            metadata: CancellationToken::new(),
            qos_aggregation: CancellationToken::new(),
            downloads: CancellationToken::new(),
            hls_cache: CancellationToken::new(),
        }
    }
}

macro_rules! change_detect {
    ($fn_name:ident, $a:expr, $b: expr) => {
        match ($a, $b) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(o), Some(n)) => $fn_name(o, n),
        }
    };
}

fn recording_changed(a: &crate::model::RecordingConfig, b: &crate::model::RecordingConfig) -> bool {
    RecordingConfigDto::from(a) != RecordingConfigDto::from(b)
}

#[derive(Clone)]
pub struct ManualPlaylistUpdateRequest {
    pub targets: Arc<ProcessTargets>,
}

#[derive(Clone)]
pub struct AppState {
    pub forced_targets: Arc<ArcSwap<ProcessTargets>>, // as program arguments
    pub app_config: Arc<AppConfig>,
    pub http_client: Arc<ArcSwap<Client>>,
    pub http_client_no_redirect: Arc<ArcSwap<Client>>,
    pub public_http_client_no_redirect: Arc<ArcSwap<Client>>,
    pub downloads: Arc<DownloadQueue>,
    pub cache: Arc<ArcSwapOption<RwLock<LRUResourceCache>>>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    pub hls_proxy: Arc<HlsProxyManager>,
    pub hls_provisioning: Arc<HlsProvisioningState>,
    pub active_users: Arc<ActiveUserManager>,
    pub active_provider: Arc<ActiveProviderManager>,
    pub connection_manager: Arc<ConnectionManager>,
    pub event_manager: Arc<EventManager>,
    pub cancel_tokens: Arc<ArcSwap<CancelTokens>>,
    pub playlists: Arc<PlaylistStorageState>,
    pub geoip: Arc<ArcSwapOption<GeoIp>>,
    pub update_guard: UpdateGuard,
    pub metadata_manager: Arc<MetadataUpdateManager>,
    /// Bounded channel (capacity 1) for manual playlist update requests.
    /// `try_send` deduplicates rapid clicks: if an update is already pending
    /// or the channel is full, the request is silently dropped so at most one
    /// update is queued at any time regardless of how many times the button is clicked.
    pub manual_update_sender: mpsc::Sender<ManualPlaylistUpdateRequest>,
}

#[cfg(test)]
pub(crate) fn create_test_app_state(config: Config) -> Arc<AppState> {
    let app_config = Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(crate::utils::FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(shared::model::ConfigPaths {
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
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(crate::model::MediaToolCapabilities::new()),
    });
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_config, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    active_provider.set_shared_stream_manager(Arc::clone(&shared_stream_manager));

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let loaded_config = app_config.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&loaded_config, &geoip, &event_manager));
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, None));
    let tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));
    let (manual_update_sender, _) = mpsc::channel::<ManualPlaylistUpdateRequest>(1);

    Arc::new(AppState {
        forced_targets: Arc::new(ArcSwap::from_pointee(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        })),
        app_config,
        http_client: Arc::new(ArcSwap::from_pointee(Client::new())),
        http_client_no_redirect: Arc::new(ArcSwap::from_pointee(Client::new())),
        public_http_client_no_redirect: Arc::new(ArcSwap::from_pointee(Client::new())),
        downloads: Arc::new(DownloadQueue::new()),
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls_proxy: Arc::new(HlsProxyManager::new()),
        hls_provisioning: Arc::new(HlsProvisioningState::new()),
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: Arc::new(ArcSwap::from_pointee(tokens)),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        update_guard: UpdateGuard::new(),
        metadata_manager,
        manual_update_sender,
    })
}

impl AppState {
    pub(in crate::api::model) async fn set_config(&self, config: Config) -> Result<UpdateChanges, TuliproxError> {
        let current_config = self.app_config.config.load();
        let current_web_auth =
            current_config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).map(WebAuthConfigDto::from);
        let new_web_auth = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).map(WebAuthConfigDto::from);
        if current_web_auth != new_web_auth {
            return Err(TuliproxError::ConfigWebUi("web auth changes require a server restart".to_string()));
        }
        let old_storage_dir = current_config.storage_dir.clone();
        drop(current_config);
        let changes = self.detect_changes_for_config(&config);
        let config_log_level = config.log.as_ref().and_then(|log| log.log_level.clone());
        config.update_runtime();

        let use_geoip = config.is_geoip_enabled();
        let storage_dir = config.storage_dir.clone();

        self.active_users.update_config(&config);
        self.app_config.set_config(config)?;
        reload_logger(config_log_level.as_deref());
        self.active_provider.update_config(&self.app_config).await;
        self.hls_proxy.update_config(&self.app_config).await;
        self.update_config().await?;

        let geoip_reload_needed =
            changes.flags.contains(UpdateChangesFlags::Geoip) || (use_geoip && old_storage_dir != storage_dir);
        if geoip_reload_needed {
            let new_geoip = if use_geoip {
                let path = get_geoip_path(&storage_dir);
                let _file_lock = self.app_config.file_locks.read_lock(&path).await;
                GeoIp::load(&path).ok().map(Arc::new)
            } else {
                None
            };

            self.geoip.store(new_geoip);
        }

        shared::model::REGEX_CACHE.sweep();
        Ok(changes)
    }

    async fn update_config(&self) -> Result<(), TuliproxError> {
        // client
        let client = create_http_client(&self.app_config)?;
        self.http_client.store(Arc::new(client));
        let client_no_redirect = create_http_client_no_redirect(&self.app_config)?;
        self.http_client_no_redirect.store(Arc::new(client_no_redirect));
        let public_client_no_redirect = create_public_http_client_no_redirect(&self.app_config)?;
        self.public_http_client_no_redirect.store(Arc::new(public_client_no_redirect));

        // cache
        let config = self.app_config.config.load();
        let (enabled, size, cache_dir) = config
            .reverse_proxy
            .as_ref()
            .and_then(|r| r.cache.as_ref())
            .map_or((false, 0, ""), |c| (c.enabled, c.size, c.directory.as_str()));

        if let Some(cache) = self.cache.load().as_ref() {
            if enabled {
                cache.write().await.update_config(size, cache_dir);
            } else {
                self.cache.store(None);
            }
        } else {
            let cache = create_cache(&config);
            self.cache.store(cache);
        }
        Ok(())
    }

    pub(in crate::api::model) async fn set_sources(
        &self,
        sources: SourcesConfig,
    ) -> Result<UpdateChanges, TuliproxError> {
        let changes = self.detect_changes_for_sources(&sources);
        // Carry over DNS caches from old providers so resolved IPs survive hot-reloads
        // without waiting for the background resolver or the persisted-file seed.
        {
            let old_sources = self.app_config.sources.load();
            for new_provider in &sources.provider {
                if let Some(old_provider) = old_sources.get_provider_by_name(&new_provider.name) {
                    if new_provider.get_dns_config().is_some_and(|cfg| cfg.enabled) {
                        for (host, ips) in old_provider.snapshot_resolved() {
                            if !ips.is_empty() && new_provider.dns_cache.ip_count(&host) == 0 {
                                new_provider.dns_cache.store_resolved(&host, ips);
                            }
                        }
                    }
                }
            }
        }
        self.app_config.set_sources(sources)?;
        self.active_provider.update_config(&self.app_config).await;

        shared::model::REGEX_CACHE.sweep();
        Ok(changes)
    }

    pub async fn get_active_connections_for_user(&self, username: &str) -> u32 {
        self.active_users.user_connections(username).await
    }

    pub async fn get_connection_permission(
        &self,
        username: &str,
        max_connections: u32,
        soft_connections: u16,
    ) -> UserConnectionPermission {
        self.active_users.connection_permission(username, max_connections, soft_connections).await
    }

    fn detect_changes_for_config(&self, config: &Config) -> UpdateChanges {
        let old_config = self.app_config.config.load();
        let changed_schedules =
            change_detect!(schedules_changed, old_config.schedules.as_ref(), config.schedules.as_ref());
        let library_enabled = config.library.as_ref().is_some_and(|library| library.enabled);
        let old_library_enabled = old_config.library.as_ref().is_some_and(|library| library.enabled);
        let changed_library_enabled = library_enabled != old_library_enabled;
        let changed_hdhomerun =
            change_detect!(hdhomerun_changed, old_config.hdhomerun.as_ref(), config.hdhomerun.as_ref());
        let changed_file_watch =
            change_detect!(string_changed, old_config.mapping_path.as_ref(), config.mapping_path.as_ref())
                || change_detect!(string_changed, old_config.template_path.as_ref(), config.template_path.as_ref());

        let geoip_enabled = config.is_geoip_enabled();
        let geoip_enabled_old = old_config.is_geoip_enabled();
        let changed_storage_dir = old_config.storage_dir != config.storage_dir;
        let changed_qos_aggregation = qos_aggregation_changed(&old_config, config);
        let changed_video_download =
            change_detect!(recording_changed, old_config.recording.as_ref(), config.recording.as_ref());

        let mut changes = UpdateChanges { flags: UpdateChangesFlagsSet::new(), targets: None };
        changes.set_flag_if(
            changed_schedules || changed_library_enabled || geoip_enabled != geoip_enabled_old,
            UpdateChangesFlags::Scheduler,
        );
        changes.set_flag_if(changed_hdhomerun, UpdateChangesFlags::Hdhomerun);
        changes.set_flag_if(changed_file_watch, UpdateChangesFlags::FileWatch);
        changes.set_flag_if(geoip_enabled != geoip_enabled_old, UpdateChangesFlags::Geoip);
        changes.set_flag_if(changed_storage_dir, UpdateChangesFlags::Metadata);
        changes.set_flag_if(changed_qos_aggregation || changed_storage_dir, UpdateChangesFlags::QosAggregation);
        changes.set_flag_if(changed_video_download, UpdateChangesFlags::Downloads);
        changes
    }

    fn detect_changes_for_sources(&self, sources: &SourcesConfig) -> UpdateChanges {
        let (file_watch_changed, provider_dns_changed, target_changes) = {
            let old_sources = self.app_config.sources.load();
            let file_watch_changed = old_sources.get_input_files() != sources.get_input_files();
            let provider_dns_changed = providers_changed(&old_sources.provider, &sources.provider);

            let mut target_changes = HashMap::new();
            for source in &old_sources.sources {
                for target in &source.targets {
                    target_changes.insert(
                        target.name.clone(),
                        TargetChanges {
                            name: target.name.clone(),
                            status: TargetStatus::Old,
                            cache_status: if target.use_memory_cache {
                                TargetCacheState::UnchangedTrue
                            } else {
                                TargetCacheState::UnchangedFalse
                            },
                            target: Arc::clone(target),
                        },
                    );
                }
            }
            for source in &sources.sources {
                for target in &source.targets {
                    match target_changes.get_mut(&target.name) {
                        None => {
                            target_changes.insert(
                                target.name.clone(),
                                TargetChanges {
                                    name: target.name.clone(),
                                    status: TargetStatus::New,
                                    cache_status: if target.use_memory_cache {
                                        TargetCacheState::ChangedToTrue
                                    } else {
                                        TargetCacheState::ChangedToFalse
                                    },
                                    target: Arc::clone(target),
                                },
                            );
                        }
                        Some(changes) => {
                            changes.status = TargetStatus::Keep;
                            changes.cache_status = match (changes.cache_status, target.use_memory_cache) {
                                (TargetCacheState::UnchangedFalse, true) => TargetCacheState::ChangedToTrue,
                                (TargetCacheState::UnchangedTrue, false) => TargetCacheState::ChangedToFalse,
                                (x, _) => x,
                            };
                        }
                    }
                }
            }

            (file_watch_changed, provider_dns_changed, target_changes)
        };

        let mut changes = UpdateChanges { flags: UpdateChangesFlagsSet::new(), targets: Some(target_changes) };
        changes.set_flag_if(file_watch_changed, UpdateChangesFlags::FileWatch);
        changes.set_flag_if(provider_dns_changed, UpdateChangesFlags::ProviderDns);
        changes
    }

    pub async fn cache_playlist(&self, target_name: &str, playlist: PlaylistStorage) {
        self.playlists.cache_playlist(target_name, playlist).await;
    }

    pub fn get_disabled_headers(&self) -> Option<ReverseProxyDisabledHeaderConfig> {
        self.app_config.get_disabled_headers()
    }

    pub fn get_grace_options(&self) -> GracePeriodOptions { self.app_config.get_grace_options() }

    pub fn should_use_manual_redirects(&self) -> bool { crate::model::should_use_manual_redirects(&self.app_config) }

    pub fn get_encrypt_secret(&self) -> [u8; 16] { self.app_config.get_encrypt_secret() }
}

fn schedules_changed(a: &[ScheduleConfig], b: &[ScheduleConfig]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    let mut used = vec![false; b.len()];

    for schedule in a {
        let Some(found_idx) = b.iter().enumerate().find_map(|(idx, candidate)| {
            if used[idx] || candidate.schedule != schedule.schedule || candidate.task_type != schedule.task_type {
                return None;
            }
            let targets_match = match (schedule.targets.as_ref(), candidate.targets.as_ref()) {
                (None, None) => true,
                (Some(_), None) | (None, Some(_)) => false,
                (Some(a_targets), Some(b_targets)) => small_vecs_equal_unordered(a_targets, b_targets),
            };
            if targets_match {
                Some(idx)
            } else {
                None
            }
        }) else {
            return true;
        };
        used[found_idx] = true;
    }
    false
}

fn hdhomerun_changed(a: &HdHomeRunConfig, b: &HdHomeRunConfig) -> bool {
    if a.flags != b.flags {
        return true;
    }
    if !small_vecs_equal_unordered(a.devices.as_ref(), b.devices.as_ref()) {
        return true;
    }
    false
}

fn string_changed(a: &str, b: &str) -> bool { a != b }

fn providers_changed(a: &[Arc<ConfigProvider>], b: &[Arc<ConfigProvider>]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for lhs in a {
        let Some(rhs) = b.iter().find(|candidate| candidate.name == lhs.name) else {
            return true;
        };
        if lhs.urls != rhs.urls || lhs.dns != rhs.dns {
            return true;
        }
    }
    false
}

fn qos_aggregation_changed(old_config: &Config, new_config: &Config) -> bool {
    let old_reverse_proxy = old_config.reverse_proxy.as_ref();
    let new_reverse_proxy = new_config.reverse_proxy.as_ref();

    let old_stream_history = old_reverse_proxy.and_then(|rp| rp.stream_history.as_ref());
    let new_stream_history = new_reverse_proxy.and_then(|rp| rp.stream_history.as_ref());
    let old_qos = old_reverse_proxy.and_then(|rp| rp.qos_aggregation.as_ref());
    let new_qos = new_reverse_proxy.and_then(|rp| rp.qos_aggregation.as_ref());

    let stream_history_tuple = |cfg: Option<&crate::model::StreamHistoryConfig>| {
        cfg.map(|history| {
            (
                history.stream_history_enabled,
                history.stream_history_directory.clone(),
                history.stream_history_retention_days,
                history.stream_history_batch_size,
            )
        })
    };
    let qos_tuple = |cfg: Option<&crate::model::QosAggregationConfig>| {
        cfg.map(|qos| (qos.enabled, qos.interval_secs, qos.compaction_interval_secs))
    };

    stream_history_tuple(old_stream_history) != stream_history_tuple(new_stream_history)
        || qos_tuple(old_qos) != qos_tuple(new_qos)
}

#[derive(Clone)]
pub struct HdHomerunAppState {
    pub app_state: Arc<AppState>,
    pub device: Arc<HdHomeRunDeviceConfig>,
    pub hd_scan_state: Arc<AtomicI8>,
}

#[cfg(test)]
mod tests {
    use super::{qos_aggregation_changed, recording_changed, schedules_changed};
    use crate::model::{
        should_use_manual_redirect_for_proxy, should_use_manual_redirects_for_env_vars, Config, RecordingConfig,
        ScheduleConfig,
    };
    use shared::model::{
        QosAggregationConfigDto, ReverseProxyConfigDto, ScheduleTaskType, StreamHistoryConfigDto, WebAuthConfigDto,
        WebUiConfigDto,
    };

    fn config_with_web_auth(secret: &str) -> Config {
        let web_ui = WebUiConfigDto {
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "test".to_string(),
                secret: secret.to_string(),
                ..WebAuthConfigDto::default()
            }),
            ..WebUiConfigDto::default()
        };
        Config { web_ui: Some((&web_ui).into()), ..Config::default() }
    }

    #[tokio::test]
    async fn config_reload_rejects_enabling_web_auth_before_swap() {
        let state = super::create_test_app_state(Config::default());

        let result = state.set_config(config_with_web_auth("secret")).await;

        assert!(matches!(result, Err(shared::error::TuliproxError::ConfigWebUi(_))));
        assert!(state.app_config.config.load().web_ui.is_none());
    }

    #[tokio::test]
    async fn config_reload_rejects_web_auth_secret_change_before_swap() {
        let state = super::create_test_app_state(config_with_web_auth("old-secret"));

        let result = state.set_config(config_with_web_auth("new-secret")).await;

        assert!(matches!(result, Err(shared::error::TuliproxError::ConfigWebUi(_))));
        assert_eq!(
            state
                .app_config
                .config
                .load()
                .web_ui
                .as_ref()
                .and_then(|web_ui| web_ui.auth.as_ref())
                .map(|auth| auth.secret.as_str()),
            Some("old-secret")
        );
    }

    #[tokio::test]
    async fn config_reload_allows_unrelated_change() {
        let state = super::create_test_app_state(Config::default());
        let config = Config { default_user_agent: Some("changed".to_string()), ..Config::default() };

        assert!(state.set_config(config).await.is_ok());
        assert_eq!(state.app_config.config.load().default_user_agent.as_deref(), Some("changed"));
    }

    #[test]
    fn should_use_manual_redirect_for_proxy_only_http_or_https() {
        assert!(should_use_manual_redirect_for_proxy("http://proxy.local:8080"));
        assert!(should_use_manual_redirect_for_proxy("https://proxy.local:8443"));
        assert!(should_use_manual_redirect_for_proxy("proxy.local:8080"));
        assert!(should_use_manual_redirect_for_proxy("127.0.0.1:8888"));
        assert!(!should_use_manual_redirect_for_proxy("socks5://proxy.local:1080"));
        assert!(!should_use_manual_redirect_for_proxy("socks5h://proxy.local:1080"));
        assert!(!should_use_manual_redirect_for_proxy("://invalid"));
        assert!(!should_use_manual_redirect_for_proxy("/tmp/proxy.socket"));
    }

    #[test]
    fn should_use_manual_redirects_for_env_vars_only_when_http_proxy_is_present() {
        assert!(should_use_manual_redirects_for_env_vars(vec![(
            "HTTP_PROXY".to_string(),
            "http://proxy.local:8080".to_string(),
        )]));
        assert!(should_use_manual_redirects_for_env_vars(vec![(
            "all_proxy".to_string(),
            "https://proxy.local:8443".to_string(),
        )]));
        assert!(should_use_manual_redirects_for_env_vars(vec![(
            "HTTP_PROXY".to_string(),
            "127.0.0.1:8888".to_string(),
        )]));
        assert!(!should_use_manual_redirects_for_env_vars(vec![(
            "ALL_PROXY".to_string(),
            "socks5://proxy.local:1080".to_string(),
        )]));
        assert!(!should_use_manual_redirects_for_env_vars(vec![(
            "NO_PROXY".to_string(),
            "http://localhost".to_string(),
        )]));
    }

    #[test]
    fn schedules_changed_detects_task_type_changes() {
        let a = vec![ScheduleConfig {
            schedule: "0 0 3 * * * *".to_string(),
            task_type: ScheduleTaskType::PlaylistUpdate,
            targets: None,
        }];
        let b = vec![ScheduleConfig {
            schedule: "0 0 3 * * * *".to_string(),
            task_type: ScheduleTaskType::GeoIpUpdate,
            targets: None,
        }];
        assert!(schedules_changed(&a, &b));
    }

    #[test]
    fn schedules_changed_treats_same_entries_as_unchanged() {
        let a = vec![
            ScheduleConfig {
                schedule: "0 0 3 * * * *".to_string(),
                task_type: ScheduleTaskType::GeoIpUpdate,
                targets: None,
            },
            ScheduleConfig {
                schedule: "0 0 8 * * * *".to_string(),
                task_type: ScheduleTaskType::PlaylistUpdate,
                targets: Some(vec!["a".to_string(), "b".to_string()]),
            },
        ];
        let b = vec![
            ScheduleConfig {
                schedule: "0 0 8 * * * *".to_string(),
                task_type: ScheduleTaskType::PlaylistUpdate,
                targets: Some(vec!["b".to_string(), "a".to_string()]),
            },
            ScheduleConfig {
                schedule: "0 0 3 * * * *".to_string(),
                task_type: ScheduleTaskType::GeoIpUpdate,
                targets: None,
            },
        ];
        assert!(!schedules_changed(&a, &b));
    }

    #[test]
    fn recording_changed_detects_retry_policy_changes() {
        let base = RecordingConfig::from(&shared::model::RecordingConfigDto {
            directory: Some("/tmp/downloads".to_string()),
            reserve_slots_for_users: 1,
            max_background_per_provider: 2,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 2.0,
            retry_backoff_max_secs: 60,
            retry_backoff_jitter_percent: 5,
            retry_max_attempts: 5,
            ..Default::default()
        });
        let mut changed = base.clone();
        changed.retry_backoff_multiplier = 3.0;

        assert!(recording_changed(&base, &changed));
    }

    #[test]
    fn recording_changed_treats_equivalent_configs_as_unchanged() {
        let base = RecordingConfig::from(&shared::model::RecordingConfigDto {
            directory: Some("/tmp/downloads".to_string()),
            organize_into_directories: true,
            episode_pattern: Some("S(?P<episode>\\d+)".to_string()),
            priority: 1,
            reserve_slots_for_users: 2,
            max_background_per_provider: 3,
            retry_backoff_initial_secs: 3,
            retry_backoff_multiplier: 2.0,
            retry_backoff_max_secs: 60,
            retry_backoff_jitter_percent: 5,
            retry_max_attempts: 5,
            ..Default::default()
        });

        assert!(!recording_changed(&base, &base.clone()));
    }

    #[test]
    fn qos_aggregation_changed_detects_stream_history_batch_size_changes() {
        let old_config = Config {
            reverse_proxy: Some(crate::model::ReverseProxyConfig::from(&ReverseProxyConfigDto {
                stream_history: Some(StreamHistoryConfigDto {
                    stream_history_enabled: true,
                    stream_history_batch_size: 64,
                    stream_history_retention_days: 7,
                    stream_history_directory: "/tmp/history".to_string(),
                }),
                qos_aggregation: Some(QosAggregationConfigDto {
                    enabled: true,
                    interval_secs: 60,
                    ..Default::default()
                }),
                ..Default::default()
            })),
            ..Config::default()
        };

        let mut new_config = old_config.clone();
        if let Some(reverse_proxy) = new_config.reverse_proxy.as_mut() {
            if let Some(history) = reverse_proxy.stream_history.as_mut() {
                history.stream_history_batch_size = 128;
            }
        }

        assert!(qos_aggregation_changed(&old_config, &new_config));
    }

    #[test]
    fn qos_aggregation_changed_detects_compaction_interval_changes() {
        let old_config = Config {
            reverse_proxy: Some(crate::model::ReverseProxyConfig::from(&ReverseProxyConfigDto {
                qos_aggregation: Some(QosAggregationConfigDto {
                    enabled: true,
                    interval_secs: 60,
                    compaction_interval_secs: 86_400,
                }),
                ..Default::default()
            })),
            ..Config::default()
        };
        let mut new_config = old_config.clone();
        if let Some(qos) = new_config.reverse_proxy.as_mut().and_then(|proxy| proxy.qos_aggregation.as_mut()) {
            qos.compaction_interval_secs = 3_600;
        }

        assert!(qos_aggregation_changed(&old_config, &new_config));
    }
}
