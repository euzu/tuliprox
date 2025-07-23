use crate::api::model::active_provider_manager::ActiveProviderManager;
use crate::api::model::active_user_manager::ActiveUserManager;
use crate::api::model::download::DownloadQueue;
use crate::api::model::streams::shared_stream_manager::SharedStreamManager;
use crate::api::scheduler::exec_scheduler;
use crate::model::{AppConfig, Config, HdHomeRunConfig, HdHomeRunDeviceConfig, ProcessTargets, ScheduleConfig, SourcesConfig};
use crate::tools::lru_cache::LRUResourceCache;
use crate::utils::request::create_client;
use arc_swap::{ArcSwap, ArcSwapAny};
use log::error;
use reqwest::Client;
use shared::error::TuliproxError;
use shared::model::UserConnectionPermission;
use shared::utils::small_vecs_equal_unordered;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use crate::api::config_watch::exec_config_watch;
use crate::api::model::event_manager::EventManager;

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

pub(in crate::api) struct UpdateChanges {
    scheduler: bool,
    hdhomerun: bool,
    file_watch: bool,
}

impl UpdateChanges {
    pub(in crate::api) fn modified(&self) -> bool {
        self.scheduler || self.hdhomerun || self.file_watch
    }
}

pub async fn update_app_state_config(app_state: &Arc<AppState>, config: Config) -> Result<(), TuliproxError> {
    let updates = app_state.set_config(config).await?;
    restart_services(app_state, &updates);
    Ok(())
}

pub async fn update_app_state_sources(app_state: &Arc<AppState>, sources: SourcesConfig) -> Result<(), TuliproxError> {
    let targets = sources.validate_targets(Some(&app_state.forced_targets.load().target_names))?;
    app_state.forced_targets.store(Arc::new(targets));
    let updates = app_state.set_sources(sources).await?;
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
        exec_scheduler(&Arc::clone(&app_state.http_client.load()), &app_state.app_config,
                       &app_state.forced_targets.load(), &app_state.cancel_tokens.load().scheduler);
    }

    if changes.hdhomerun && app_state.app_config.api_proxy.load().is_some() {
        let mut infos = Vec::new();
        crate::api::main_api::start_hdhomerun(&app_state.app_config, app_state, &mut infos,
                                              &app_state.cancel_tokens.load().hdhomerun);
    }

    if changes.file_watch {
        exec_config_watch(app_state, &app_state.cancel_tokens.load().file_watch);
    }
}

pub fn create_http_client(app_config: &AppConfig) -> Client {
    let mut builder = create_client(app_config).http1_only();
    let config = app_config.config.load(); // because of RAII connection dropping
    if config.connect_timeout_secs > 0 {
        builder = builder.connect_timeout(Duration::from_secs(u64::from(config.connect_timeout_secs)));
    }
    builder.build().unwrap_or_else(|_| Client::new())
}

pub fn create_cache(config: &Config) -> Option<Arc<Mutex<LRUResourceCache>>> {
    let lru_cache = config.reverse_proxy.as_ref().and_then(|r| r.cache.as_ref()).and_then(|c| if c.enabled {
        Some(LRUResourceCache::new(c.size, c.dir.as_str()))
    } else { None });
    let cache_enabled = lru_cache.is_some();
    if cache_enabled {
        if let Some(res_cache) = lru_cache {
            let cache = Arc::new(Mutex::new(res_cache));
            let cache_scanner = Arc::clone(&cache);
            tokio::spawn(async move {
                let mut c = cache_scanner.lock().await;
                if let Err(err) = (*c).scan() {
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
            (Some(_), None) |
            (None, Some(_)) => true,
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
    pub cache: Arc<ArcSwapAny<Option<Arc<Mutex<LRUResourceCache>>>>>,
    pub shared_stream_manager: Arc<SharedStreamManager>,
    pub active_users: Arc<ActiveUserManager>,
    pub active_provider: Arc<ActiveProviderManager>,
    pub event_manager: Arc<EventManager>,
    pub cancel_tokens: Arc<ArcSwap<CancelTokens>>,
}

impl AppState {
    pub(in crate::api::model) async fn set_config(&self, config: Config) -> Result<UpdateChanges, TuliproxError> {
        let changes = self.detect_changes_for_config(&config);
        config.update_runtime();
        self.active_users.update_config(&config);
        self.app_config.set_config(config)?;
        self.active_provider.update_config(&self.app_config).await;
        self.update_config().await;
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

    pub(in crate::api::model) async fn set_sources(&self, sources: SourcesConfig) -> Result<UpdateChanges, TuliproxError> {
        let changes = self.detect_changes_for_sources(&sources);
        self.app_config.set_sources(sources)?;
        self.active_provider.update_config(&self.app_config).await;
        Ok(changes)
    }

    pub fn get_active_connections_for_user(&self, username: &str) -> u32 {
        self.active_users.user_connections(username)
    }

    pub fn get_connection_permission(&self, username: &str, max_connections: u32) -> UserConnectionPermission {
        self.active_users.connection_permission(username, max_connections)
    }

    fn detect_changes_for_config(&self, config: &Config) -> UpdateChanges {
        let old_config = self.app_config.config.load();
        let changed_schedules = change_detect!(schedules_changed, old_config.schedules.as_ref(), config.schedules.as_ref());
        let changed_hdhomerun = change_detect!(hdhomerun_changed, old_config.hdhomerun.as_ref(), config.hdhomerun.as_ref());
        let changed_file_watch = change_detect!(string_changed, old_config.mapping_path.as_ref(), config.mapping_path.as_ref());

        UpdateChanges {
            scheduler: changed_schedules,
            hdhomerun: changed_hdhomerun,
            file_watch: changed_file_watch,
        }
    }

    fn detect_changes_for_sources(&self, sources: &SourcesConfig) -> UpdateChanges {
        let file_watch_changed = {
            let old_sources = self.app_config.sources.load();
            old_sources.get_input_files() !=  sources.get_input_files()
        };

        UpdateChanges {
            scheduler: false,
            hdhomerun: false,
            file_watch: file_watch_changed,
        }
    }
}

fn schedules_changed(a: &[ScheduleConfig], b: &[ScheduleConfig]) -> bool {
    if a.len() != b.len() {
        return true;
    }
    for schedule in a {
        if let Some(found) = b.iter().find(|&s| s.schedule == schedule.schedule) {
            match (schedule.targets.as_ref(), found.targets.as_ref()) {
                (None, None) => return false,
                (Some(_targets), None) |
                (None, Some(_targets)) => return true,
                (Some(a_targets), Some(b_targets)) => {
                    if !small_vecs_equal_unordered(a_targets, b_targets) {
                        return true;
                    }
                }
            }
        } else {
            return true;
        }
    }
    false
}

fn hdhomerun_changed(a: &HdHomeRunConfig, b: &HdHomeRunConfig) -> bool {
    if a.enabled != b.enabled || a.auth != b.auth {
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
}
