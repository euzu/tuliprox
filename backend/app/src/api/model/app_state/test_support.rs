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

pub fn create_test_app_state(config: Config) -> Arc<AppState> {
    create_test_app_state_with_playlist_updates(config, PlaylistUpdateControl::for_tests())
}

/// Like [`create_test_app_state`], with a manual update channel whose receiver the test keeps.
pub fn create_test_app_state_with_manual_update_sender(
    config: Config,
    manual_update_sender: tokio::sync::mpsc::Sender<super::ManualPlaylistUpdateRequest>,
) -> Arc<AppState> {
    create_test_app_state_with_playlist_updates(
        config,
        PlaylistUpdateControl::for_tests_with_sender(manual_update_sender),
    )
}

/// `Config::update_runtime` points the process-wide temp dir at the storage dir of the loaded config. A test that
/// reloads the config would leave every later `tempfile` of the test process pointing into its own, already removed,
/// storage dir. Every test state is created through this module, so the override is switched off before the first one.
fn disable_temp_dir_override() {
    static DISABLED: std::sync::Once = std::sync::Once::new();
    DISABLED.call_once(|| std::env::set_var("TULIPROX_DISABLE_TEMP_DIR_OVERRIDE", "1"));
}

fn create_test_app_state_with_playlist_updates(
    config: Config,
    playlist_updates: PlaylistUpdateControl,
) -> Arc<AppState> {
    disable_temp_dir_override();
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
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(crate::model::MediaToolCapabilities::new()),
        failover: Arc::default(),
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
        recordings: Arc::new(RecordingQueue::new()),
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls: HlsState::new(Arc::new(HlsProxyManager::new())),
        stalker_resolve_coordinator: crate::api::model::StalkerResolveCoordinator::default(),
        active_users,
        recording_capacity: crate::api::model::recording_runtime::ProviderCapacityAdapter::new(
            Arc::clone(&active_provider),
            Arc::clone(&connection_manager),
            Arc::clone(&maintenance),
        ),
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: ArcSwap::from_pointee(tokens),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        metadata_manager,
        auth: AuthState::for_tests(),
        playlist_updates,
        maintenance,
        member_health: Arc::new(tuliprox_session::MemberHealthIndex::default()),
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
