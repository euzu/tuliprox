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
    })
}
