use crate::{
    api::{
        config_watch::exec_config_watch,
        model::provider_dns_manager::exec_provider_dns,
        model::{
            metadata_update_manager::MetadataUpdateManager, ActiveProviderManager, ActiveUserManager,
            ConnectionManager, DownloadQueue, EventManager, PlaylistStorage, PlaylistStorageState, SharedStreamManager,
            UpdateGuard,
        },
        scheduler::exec_scheduler,
    },
    model::{
        AppConfig, Config, ConfigProvider, ConfigTarget, GracePeriodOptions, HdHomeRunConfig, HdHomeRunDeviceConfig,
        ProcessTargets, ReverseProxyDisabledHeaderConfig, ScheduleConfig, SourcesConfig,
    },
    repository::{get_geoip_path, load_target_into_memory_cache},
    utils::{
        request::{create_client, create_client_with_redirect},
        GeoIp,
    },
};
use arc_swap::{ArcSwap, ArcSwapOption};
use log::{error, info};
use reqwest::Client;
use shared::{
    create_bitset, error::TuliproxError, info_err_res, model::UserConnectionPermission,
    utils::small_vecs_equal_unordered,
};
use std::{
    collections::HashMap,
    ffi::OsStr,
    sync::{atomic::AtomicI8, Arc},
    time::Duration,
};
use tokio::sync::{mpsc, Mutex};
use tokio::task;
use tokio_util::sync::CancellationToken;
use url::Url;
use crate::utils::LRUResourceCache;

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

create_bitset!(u8, UpdateChangesFlags, Scheduler, Hdhomerun, FileWatch, Geoip, ProviderDns, Metadata);

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
                            load_target_into_memory_cache(app_state, &target.target).await;
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

    let tokens = CancelTokens {
        scheduler,
        hdhomerun,
        file_watch,
        provider_dns,
        metadata,
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
        exec_provider_dns(app_state, &app_state.cancel_tokens.load().provider_dns);
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
        return info_err_res!("{proxy_error_msg}");
    }

    error!("{fallback_log}");
    Ok(fallback_client())
}

pub fn create_cache(config: &Config) -> Option<Arc<Mutex<LRUResourceCache>>> {
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
            let cache = Arc::new(Mutex::new(res_cache));
            let cache_scanner = Arc::clone(&cache);
            tokio::spawn(async move {
                let scan_result = {
                    let mut cache = cache_scanner.lock().await;
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
}
impl Default for CancelTokens {
    fn default() -> Self {
        Self {
            scheduler: CancellationToken::new(),
            hdhomerun: CancellationToken::new(),
            file_watch: CancellationToken::new(),
            provider_dns: CancellationToken::new(),
            metadata: CancellationToken::new(),
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

#[derive(Clone)]
pub struct AppState {
    pub forced_targets: Arc<ArcSwap<ProcessTargets>>, // as program arguments
    pub app_config: Arc<AppConfig>,
    pub http_client: Arc<ArcSwap<Client>>,
    pub http_client_no_redirect: Arc<ArcSwap<Client>>,
    pub downloads: Arc<DownloadQueue>,
    pub cache: Arc<ArcSwapOption<Mutex<LRUResourceCache>>>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
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
    pub manual_update_sender: mpsc::Sender<Arc<ProcessTargets>>,
}

impl AppState {
    pub(in crate::api::model) async fn set_config(&self, config: Config) -> Result<UpdateChanges, TuliproxError> {
        let old_storage_dir = self.app_config.config.load().storage_dir.clone();
        let changes = self.detect_changes_for_config(&config);
        config.update_runtime();

        let use_geoip = config.is_geoip_enabled();
        let storage_dir = config.storage_dir.clone();

        self.active_users.update_config(&config);
        self.app_config.set_config(config)?;
        self.active_provider.update_config(&self.app_config).await;
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

        // cache
        let config = self.app_config.config.load();
        let (enabled, size, cache_dir) = config
            .reverse_proxy
            .as_ref()
            .and_then(|r| r.cache.as_ref())
            .map_or((false, 0, ""), |c| (c.enabled, c.size, c.directory.as_str()));

        if let Some(cache) = self.cache.load().as_ref() {
            if enabled {
                cache.lock().await.update_config(size, cache_dir);
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

    pub async fn get_connection_permission(&self, username: &str, max_connections: u32) -> UserConnectionPermission {
        self.active_users.connection_permission(username, max_connections).await
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

        let mut changes = UpdateChanges { flags: UpdateChangesFlagsSet::new(), targets: None };
        changes.set_flag_if(changed_schedules || changed_library_enabled || geoip_enabled != geoip_enabled_old, UpdateChangesFlags::Scheduler);
        changes.set_flag_if(changed_hdhomerun, UpdateChangesFlags::Hdhomerun);
        changes.set_flag_if(changed_file_watch, UpdateChangesFlags::FileWatch);
        changes.set_flag_if(geoip_enabled != geoip_enabled_old, UpdateChangesFlags::Geoip);
        changes.set_flag_if(changed_storage_dir, UpdateChangesFlags::Metadata);
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

    pub fn should_use_manual_redirects(&self) -> bool {
        let config = self.app_config.config.load();
        config.proxy.as_ref().is_some_and(|proxy| should_use_manual_redirect_for_proxy(proxy.url.as_str()))
            || proxy_env_present()
    }

    pub fn get_encrypt_secret(&self) -> [u8;16] {
        self.app_config
            .get_reverse_proxy_rewrite_secret()
            .unwrap_or(self.app_config.encrypt_secret)
    }
}

fn proxy_env_present() -> bool { should_use_manual_redirects_for_env_vars(std::env::vars_os()) }

fn parse_proxy_url_with_http_fallback(proxy_url: &str) -> Option<Url> {
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(url) = Url::parse(trimmed) {
        if matches!(url.scheme().to_ascii_lowercase().as_str(), "http" | "https") {
            return Some(url);
        }
        if trimmed.contains("://") {
            return None;
        }
    }

    if trimmed.contains("://") {
        return None;
    }
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return None;
    }

    Url::parse(format!("http://{trimmed}").as_str()).ok()
}

fn should_use_manual_redirect_for_proxy(proxy_url: &str) -> bool {
    parse_proxy_url_with_http_fallback(proxy_url).is_some_and(|url| {
        matches!(url.scheme().to_ascii_lowercase().as_str(), "http" | "https") && url.host_str().is_some()
    })
}

fn should_use_manual_redirects_for_env_vars<I, K, V>(vars: I) -> bool
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    const ENV_KEYS: [&str; 3] = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];

    vars.into_iter().any(|(key, value)| {
        let Some(key) = key.as_ref().to_str() else {
            return false;
        };
        let Some(value) = value.as_ref().to_str() else {
            return false;
        };
        let value = value.trim();
        ENV_KEYS.iter().any(|candidate| candidate.eq_ignore_ascii_case(key))
            && !value.is_empty()
            && should_use_manual_redirect_for_proxy(value)
    })
}

fn schedules_changed(a: &[ScheduleConfig], b: &[ScheduleConfig]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    let mut used = vec![false; b.len()];

    for schedule in a {
        let Some(found_idx) = b.iter().enumerate().find_map(|(idx, candidate)| {
            if used[idx]
                || candidate.schedule != schedule.schedule
                || candidate.task_type != schedule.task_type
            {
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

#[derive(Clone)]
pub struct HdHomerunAppState {
    pub app_state: Arc<AppState>,
    pub device: Arc<HdHomeRunDeviceConfig>,
    pub hd_scan_state: Arc<AtomicI8>,
}

#[cfg(test)]
mod tests {
    use super::{schedules_changed, should_use_manual_redirect_for_proxy, should_use_manual_redirects_for_env_vars};
    use crate::model::ScheduleConfig;
    use shared::model::ScheduleTaskType;

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
}
