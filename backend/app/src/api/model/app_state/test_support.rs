use super::{AppState, AuthState, CancelTokens, HlsState, PlaylistUpdateControl};
use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, ConnectionManager, EventManager, PlaylistStorageState,
        RecordingQueue, SharedStreamManager,
    },
    model::{AppConfig, Config, SourcesConfig},
    repository::GeoIp,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use std::sync::Arc;
use tuliprox_hls::api::HlsProxyManager;
use tuliprox_metadata::manager::MetadataUpdateManager;

/// A current-thread runtime and a count of the threads it has started. It has no
/// worker threads, so each one is a blocking-pool thread: a test sees that code
/// ran `spawn_blocking` when the count grows.
///
/// # Panics
/// When the runtime cannot be built.
pub fn counting_runtime() -> (tokio::runtime::Runtime, Arc<std::sync::atomic::AtomicUsize>) {
    let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&started);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .build()
        .expect("test runtime");
    (runtime, started)
}

/// A plugin system holding the UI-only plugin `demo`, whose settings schema
/// `config-schema.json` is a file in the package.
///
/// # Panics
/// When the package cannot be written or the plugin system does not load.
#[cfg(feature = "plugins")]
pub fn create_test_plugin_system(root: &std::path::Path) -> tuliprox_plugins::PluginSystem {
    let dir = root.join("plugins").join("demo");
    std::fs::create_dir_all(&dir).expect("plugin dir");
    std::fs::write(
        dir.join("manifest.yml"),
        "id: demo\nname: Demo\nversion: 1.0.0\napi_version: 1\nui:\n  config_section:\n    schema: config-schema.json\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("config-schema.json"), r#"{"type":"object"}"#).expect("schema");
    let config = tuliprox_core::model::PluginsConfig {
        enabled: true,
        directory: root.join("plugins").to_string_lossy().to_string(),
        ..tuliprox_core::model::PluginsConfig::default()
    };
    let services = tuliprox_plugins::PluginServices::new(reqwest::Client::new(), root.join("data"));
    tuliprox_plugins::PluginSystem::load(&config, services).expect("load").expect("plugins are enabled")
}

pub fn create_test_app_state(config: Config) -> Arc<AppState> {
    create_test_app_state_with_plugins(config, super::AppPlugins::default())
}

pub fn create_test_app_state_with_plugins(config: Config, plugins: super::AppPlugins) -> Arc<AppState> {
    let app_config = Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(config)),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        plugins: Arc::default(),
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
    active_provider.bind_event_manager(&event_manager);
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    active_provider.set_shared_stream_manager(&shared_stream_manager);

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let loaded_config = app_config.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&loaded_config, &geoip, &event_manager));
    let cleanup_capacity = loaded_config
        .reverse_proxy
        .as_ref()
        .and_then(|reverse_proxy| reverse_proxy.stream.as_ref())
        .map_or_else(shared::defaults::default_cleanup_queue_capacity, |stream| stream.cleanup_queue_capacity);
    drop(loaded_config);
    let connection_manager = Arc::new(ConnectionManager::new_with_capacity(
        &active_users,
        &active_provider,
        &shared_stream_manager,
        &event_manager,
        None,
        cleanup_capacity,
    ));
    let tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));

    let maintenance = super::create_test_maintenance(&app_config.config.load().storage_dir);
    Arc::new(AppState {
        app_config,
        http_clients: Arc::default(),
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls: HlsState::new(Arc::new(HlsProxyManager::new())),
        stalker_resolve_coordinator: crate::api::model::StalkerResolveCoordinator::default(),
        epg_scans: crate::api::model::EpgScanCoordinator::default(),
        active_users,
        recording: crate::api::model::RecordingState::new(
            Arc::new(RecordingQueue::new()),
            crate::api::model::recording_runtime::ProviderCapacityAdapter::new(
                Arc::clone(&active_provider),
                Arc::clone(&connection_manager),
                Arc::clone(&maintenance),
            ),
        ),
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: ArcSwap::from_pointee(tokens),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        metadata_manager,
        auth: AuthState::for_tests(),
        playlist_updates: PlaylistUpdateControl::for_tests(),
        maintenance,
        plugins,
    })
}

/// A maintenance registry for tests: persisted under the configured storage dir,
/// or in memory only, so a test never writes into the working directory.
pub fn create_test_maintenance(storage_dir: &str) -> Arc<crate::model::MaintenanceRegistry> {
    if storage_dir.is_empty() {
        return Arc::new(crate::model::MaintenanceRegistry::in_memory());
    }
    Arc::new(crate::model::MaintenanceRegistry::new(
        std::path::PathBuf::from(storage_dir).join(crate::model::MAINTENANCE_FILE),
    ))
}
