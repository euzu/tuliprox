use crate::api::config_watch::exec_config_watch;
use crate::api::model::{ActiveProviderManager, ConnectionManager, EventManager, PlaylistStorage, PlaylistStorageState, SharedStreamManager};
use crate::api::model::{ActiveUserManager, DownloadQueue};
use crate::api::scheduler::exec_scheduler;
use crate::model::{AppConfig, Config, ConfigTarget, HdHomeRunConfig, HdHomeRunDeviceConfig, ProcessTargets, ReverseProxyDisabledHeaderConfig, ScheduleConfig, SourcesConfig};
use crate::repository::playlist_repository::load_target_into_memory_cache;
use crate::tools::lru_cache::LRUResourceCache;
use crate::utils::request::create_client;
use arc_swap::{ArcSwap, ArcSwapOption};
use log::{error, info};
use reqwest::Client;
use shared::error::TuliproxError;
use shared::model::{UserConnectionPermission};
use shared::utils::small_vecs_equal_unordered;
use std::collections::HashMap;
use std::sync::atomic::AtomicI8;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex};
use tokio::task;
use tokio_util::sync::CancellationToken;
use crate::api::model::UpdateGuard;
use crate::repository::storage::get_geoip_path;
use crate::utils::GeoIp;

macro_rules! cancel_service {
    ($field: ident, $changes:expr, $cancel_tokens:expr) => {
        if $changes.$field {
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

#[allow(clippy::struct_excessive_bools)]
pub(in crate::api) struct UpdateChanges {
    scheduler: bool,
    hdhomerun: bool,
    file_watch: bool,
    geoip: bool,
    targets: Option<HashMap<String, TargetChanges>>,
}

impl UpdateChanges {
    pub(in crate::api) fn modified(&self) -> bool {
        self.scheduler || self.hdhomerun || self.file_watch || self.geoip
    }
}

async fn update_target_caches(
    app_state: &Arc<AppState>,
    target_changes: Option<&HashMap<String, TargetChanges>>,
) {
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

pub async fn update_app_state_config(
    app_state: &Arc<AppState>,
    config: Config,
) -> Result<(), TuliproxError> {
    let updates = app_state.set_config(config).await?;
    restart_services(app_state, &updates);
    Ok(())
}

pub async fn update_app_state_sources(
    app_state: &Arc<AppState>,
    sources: SourcesConfig,
) -> Result<(), TuliproxError> {
    let targets = sources.validate_targets(Some(&app_state.forced_targets.load().target_names))?;
    app_state.forced_targets.store(Arc::new(targets));
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

    let scheduler = cancel_service!(scheduler, changes, cancel_tokens);
    let hdhomerun = cancel_service!(hdhomerun, changes, cancel_tokens);
    let file_watch = cancel_service!(file_watch, changes, cancel_tokens);

    let tokens = CancelTokens {
        scheduler,
        hdhomerun,
        file_watch,
    };

    app_state.cancel_tokens.store(Arc::new(tokens));
}

fn start_services(app_state: &Arc<AppState>, changes: &UpdateChanges) {
    if !changes.modified() {
        return;
    }
    if changes.scheduler {
        exec_scheduler(
            &Arc::clone(&app_state.http_client.load()),
            app_state,
            &app_state.forced_targets.load(),
            &app_state.cancel_tokens.load().scheduler,
        );
    }

    if changes.hdhomerun && app_state.app_config.api_proxy.load().is_some() {
        let mut infos = Vec::new();
        crate::api::main_api::start_hdhomerun(
            &app_state.app_config,
            app_state,
            &mut infos,
            &app_state.cancel_tokens.load().hdhomerun,
        );
    }

    if changes.file_watch {
        exec_config_watch(app_state, &app_state.cancel_tokens.load().file_watch);
    }
}

pub fn create_http_client(app_config: &AppConfig) -> Client {
    let mut builder = create_client(app_config).http1_only();
    let config = app_config.config.load(); // because of RAII connection dropping
    if config.connect_timeout_secs > 0 {
        builder =
            builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    builder.build().unwrap_or_else(|_| Client::new())
}

pub fn create_cache(config: &Config) -> Option<Arc<Mutex<LRUResourceCache>>> {
    let lru_cache = config
        .reverse_proxy
        .as_ref()
        .and_then(|r| r.cache.as_ref())
        .and_then(|c| {
            if c.enabled {
                Some(LRUResourceCache::new(c.size, c.dir.as_str()))
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
}
impl Default for CancelTokens {
    fn default() -> Self {
        Self {
            scheduler: CancellationToken::new(),
            hdhomerun: CancellationToken::new(),
            file_watch: CancellationToken::new(),
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
}

impl AppState {

    pub(in crate::api::model) async fn set_config(&self,config: Config) -> Result<UpdateChanges, TuliproxError> {
        let changes = self.detect_changes_for_config(&config);
        config.update_runtime();

        let use_geoip = config.is_geoip_enabled();
        let working_dir = config.working_dir.clone();

        self.active_users.update_config(&config);
        self.app_config.set_config(config)?;
        self.active_provider.update_config(&self.app_config).await;
        self.update_config().await;

        if changes.geoip {
            let new_geoip = if use_geoip {
                let path = get_geoip_path(&working_dir);
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

    async fn update_config(&self) {
        // client
        let client = create_http_client(&self.app_config);
        self.http_client.store(Arc::new(client));

        // cache
        let config = self.app_config.config.load();
        let (enabled, size, cache_dir) = config
            .reverse_proxy
            .as_ref()
            .and_then(|r| r.cache.as_ref())
            .map_or((false, 0, ""), |c| (c.enabled, c.size, c.dir.as_str()));

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
    }

    pub(in crate::api::model) async fn set_sources(&self,sources: SourcesConfig) -> Result<UpdateChanges, TuliproxError> {
        let changes = self.detect_changes_for_sources(&sources);
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
    ) -> UserConnectionPermission {
        self.active_users
            .connection_permission(username, max_connections)
            .await
    }

    fn detect_changes_for_config(&self, config: &Config) -> UpdateChanges {
        let old_config = self.app_config.config.load();
        let changed_schedules =
            change_detect!(schedules_changed, old_config.schedules.as_ref(), config.schedules.as_ref());
        let changed_hdhomerun =
            change_detect!(hdhomerun_changed, old_config.hdhomerun.as_ref(), config.hdhomerun.as_ref());
        let changed_file_watch = change_detect!(
            string_changed,
            old_config.mapping_path.as_ref(),
            config.mapping_path.as_ref()
        );

        let geoip_enabled = config.is_geoip_enabled();
        let geoip_enabled_old = old_config.is_geoip_enabled();

        UpdateChanges {
            scheduler: changed_schedules,
            hdhomerun: changed_hdhomerun,
            file_watch: changed_file_watch,
            targets: None,
            geoip: geoip_enabled != geoip_enabled_old,
        }
    }

    fn detect_changes_for_sources(&self, sources: &SourcesConfig) -> UpdateChanges {
        let (file_watch_changed, target_changes) = {
            let old_sources = self.app_config.sources.load();
            let file_watch_changed = old_sources.get_input_files() != sources.get_input_files();

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
                                (TargetCacheState::UnchangedFalse, true) => {
                                    TargetCacheState::ChangedToTrue
                                }
                                (TargetCacheState::UnchangedTrue, false) => {
                                    TargetCacheState::ChangedToFalse
                                }
                                (x, _) => x,
                            };
                        }
                    }
                }
            }

            (file_watch_changed, target_changes)
        };

        UpdateChanges {
            scheduler: false,
            hdhomerun: false,
            file_watch: file_watch_changed,
            geoip: false,
            targets: Some(target_changes),
        }
    }

    pub async fn cache_playlist(&self, target_name: &str, playlist: PlaylistStorage) {
        self.playlists.cache_playlist(target_name, playlist).await;
    }

    pub fn get_disabled_headers(&self) -> Option<ReverseProxyDisabledHeaderConfig> {
        self
            .app_config
            .config
            .load()
            .reverse_proxy
            .as_ref()
            .and_then(|r| r.disabled_header.clone())
    }
}

fn schedules_changed(a: &[ScheduleConfig], b: &[ScheduleConfig]) -> bool {
   if a.len() != b.len() {
       return true;
   }
   for schedule in a {
       let Some(found) = b.iter().find(|&s| s.schedule == schedule.schedule) else {
           return true;
       };
       match (schedule.targets.as_ref(), found.targets.as_ref()) {
           (None, None) => {}
           (Some(_), None) | (None, Some(_)) => return true,
           (Some(a_targets), Some(b_targets)) => {
               if !small_vecs_equal_unordered(a_targets, b_targets) {
                   return true;
               }
           }
       }
   }
   false
}

fn hdhomerun_changed(a: &HdHomeRunConfig, b: &HdHomeRunConfig) -> bool {
    if a.enabled != b.enabled
        || a.auth != b.auth
        || a.ssdp_discovery != b.ssdp_discovery
        || a.proprietary_discovery != b.proprietary_discovery
    {
        return true;
    }
    if !small_vecs_equal_unordered(a.devices.as_ref(), b.devices.as_ref()) {
        return true;
    }
    false
}

fn string_changed(a: &str, b: &str) -> bool {
    a != b
}

#[derive(Clone)]
pub struct HdHomerunAppState {
    pub app_state: Arc<AppState>,
    pub device: Arc<HdHomeRunDeviceConfig>,
    pub hd_scan_state: Arc<AtomicI8>,
}
