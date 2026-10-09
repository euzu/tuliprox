use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, AppState, ConnectionManager, EventManager, MetadataUpdateManager,
        PlaylistStorageState, SharedStreamManager,
    },
    model::{AppConfig, Config, ConfigInput, ConfigSource, ConfigTarget, SourcesConfig, StreamHistoryConfig},
    repository::GeoIp,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use chrono::Utc;
use shared::{
    foundation::Filter,
    model::{
        provider_saturation::build_group_lookup, ConfigPaths, EpgSourceDto, EpgSourceTypeDto, IcsEpgSourceConfigDto,
        ProcessingOrder,
    },
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(in crate::api::endpoints::v1_api_playlist::tests) fn playlist_update_target(
    id: u16,
    name: &str,
) -> Arc<ConfigTarget> {
    Arc::new(ConfigTarget {
        curation: None,
        id,
        enabled: true,
        name: name.to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: vec![],
        rename: None,
        mapping_ids: None,
        mapping: Arc::default(),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    })
}

/// Generate an XMLTV datetime string in the format `YYYYMMDDHHmmss +0000`
/// offset by `hours_from_now` hours from the current time.
pub(in crate::api::endpoints::v1_api_playlist::tests) fn epg_dt(hours_from_now: i64) -> String {
    let dt = Utc::now() + chrono::Duration::hours(hours_from_now);
    dt.format("%Y%m%d%H%M%S %z").to_string()
}

pub(in crate::api::endpoints::v1_api_playlist::tests) fn xmltv_source_dto(url: &str, priority: i16) -> EpgSourceDto {
    EpgSourceDto { url: url.to_string(), priority, ..EpgSourceDto::default() }
}

pub(in crate::api::endpoints::v1_api_playlist::tests) fn ics_source_dto(
    url: &str,
    channel_id: &str,
    priority: i16,
) -> EpgSourceDto {
    EpgSourceDto {
        source_type: EpgSourceTypeDto::Ics,
        url: url.to_string(),
        priority,
        channel_id: Some(channel_id.to_string()),
        channel_title: Some("Formula 1".to_string()),
        ics: Some(IcsEpgSourceConfigDto::default()),
        ..EpgSourceDto::default()
    }
}

pub(in crate::api::endpoints::v1_api_playlist::tests) fn test_app_config(
    input: Arc<ConfigInput>,
    source: ConfigSource,
) -> AppConfig {
    let inputs = vec![input];
    let sources = SourcesConfig {
        batch_files: vec![],
        provider: vec![],
        group_lookup: build_group_lookup(&inputs),
        inputs,
        sources: vec![source],
        templates: None,
    };

    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::empty()),
        api_proxy: Arc::new(ArcSwapOption::empty()),
        file_locks: Arc::new(crate::utils::FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
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
        custom_stream_response: Arc::new(ArcSwapOption::empty()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
    }
}

pub(in crate::api::endpoints::v1_api_playlist::tests) fn test_app_state(app_cfg: Arc<AppConfig>) -> Arc<AppState> {
    let (manual_update_sender, _) = mpsc::channel::<crate::api::model::ManualPlaylistUpdateRequest>(1);
    test_app_state_with_manual_update_sender(app_cfg, manual_update_sender)
}

pub(in crate::api::endpoints::v1_api_playlist::tests) fn test_app_state_with_manual_update_sender(
    app_cfg: Arc<AppConfig>,
    manual_update_sender: mpsc::Sender<crate::api::model::ManualPlaylistUpdateRequest>,
) -> Arc<AppState> {
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    let history_config = Some(StreamHistoryConfig::default());
    active_provider.set_shared_stream_manager(&shared_stream_manager);

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_cfg.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    let connection_manager = Arc::new(ConnectionManager::new(
        &active_users,
        &active_provider,
        &shared_stream_manager,
        &event_manager,
        history_config.as_ref(),
    ));

    let tokens = crate::api::model::CancelTokens {
        scheduler: CancellationToken::new(),
        hdhomerun: CancellationToken::new(),
        file_watch: CancellationToken::new(),
        provider_dns: CancellationToken::new(),
        metadata: CancellationToken::new(),
        qos_aggregation: CancellationToken::new(),
        recordings: CancellationToken::new(),
        hls_cache: CancellationToken::new(),
    };
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));

    Arc::new(AppState {
        recording_capacity: crate::api::model::recording_runtime::ProviderCapacityAdapter::new(
            Arc::clone(&active_provider),
            Arc::clone(&connection_manager),
        ),
        app_config: app_cfg,
        http_clients: Arc::default(),
        recordings: Arc::new(crate::api::model::RecordingQueue::new()),
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls: crate::api::model::HlsState::new(Arc::new(crate::api::model::HlsProxyManager::new())),
        stalker_resolve_coordinator: crate::api::model::StalkerResolveCoordinator::default(),
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: ArcSwap::from_pointee(tokens),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        metadata_manager,
        auth: crate::api::model::AuthState::for_tests(),
        playlist_updates: crate::api::model::PlaylistUpdateControl::for_tests_with_sender(manual_update_sender),
    })
}
