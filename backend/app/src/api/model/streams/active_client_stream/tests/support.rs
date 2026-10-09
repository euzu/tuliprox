use super::{ActiveClientStream, ActiveClientStreamState, CustomVideoBuffers, DirectBodyIdleTimeout, StreamMode};
use crate::{
    api::model::{
        connection_manager::PROVIDER_END_NOT_SET, ActiveProviderManager, ActiveUserManager, AppState, CancelTokens,
        ConnectionManager, EventManager, MetadataUpdateManager, PlaylistStorageState,
        ProviderContentRepresentationMode, RecordingQueue, SharedStreamManager, StreamDetails,
    },
    auth::Fingerprint,
    model::{
        AppConfig, Config, ConfigInput, GracePeriodOptions, MediaToolCapabilities, ProxyUserCredentials, SourcesConfig,
    },
    repository::GeoIp,
    utils::FileLockManager,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use futures::{pin_mut, StreamExt};
use shared::{
    model::{ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, StreamChannel, XtreamCluster},
    utils::Internable,
};
use std::{
    collections::HashMap,
    sync::{atomic::AtomicU8, Arc},
};

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_app_config() -> AppConfig {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "provider_1".intern(),
        input_type: InputType::Xtream,
        headers: HashMap::default(),
        url: "http://provider-1.example".to_string(),
        username: Some("user1".to_string()),
        password: Some("pass1".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        method: InputFetchMethod::default(),
        aliases: None,
        ..ConfigInput::default()
    });
    let sources = SourcesConfig { inputs: vec![input], ..SourcesConfig::default() };

    AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(sources)),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
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
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    }
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_connection_manager(
) -> Arc<ConnectionManager> {
    let app_cfg = create_test_app_config();
    let event_manager = Arc::new(EventManager::new());
    let provider_manager = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
    provider_manager.set_shared_stream_manager(&shared_manager);

    let geo_ip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_cfg.config.load();
    let user_manager = Arc::new(ActiveUserManager::new(&config, &geo_ip, &event_manager));

    Arc::new(ConnectionManager::new(&user_manager, &provider_manager, &shared_manager, &event_manager, None))
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_app_state() -> Arc<AppState> {
    let app_cfg = Arc::new(create_test_app_config());
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_cfg, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    active_provider.set_shared_stream_manager(&shared_stream_manager);

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_cfg.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, None));

    let tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(tokens.metadata.clone()));

    Arc::new(AppState {
        recording_capacity: crate::api::model::recording_runtime::ProviderCapacityAdapter::new(
            Arc::clone(&active_provider),
            Arc::clone(&connection_manager),
        ),
        app_config: app_cfg,
        http_clients: Arc::default(),
        recordings: Arc::new(RecordingQueue::new()),
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
        playlist_updates: crate::api::model::PlaylistUpdateControl::for_tests(),
    })
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_user(
    username: &str,
) -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user.max_connections = 1;
    user
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_fingerprint(
    addr: std::net::SocketAddr,
) -> Fingerprint {
    Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr)
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_test_stream_channel(
    virtual_id: u32,
    url: &str,
) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id,
        provider_id: 1,
        input_name: "input".intern(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        group: "Live".intern(),
        title: "Test Channel".intern(),
        url: url.into(),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

pub(in crate::api::model::streams::active_client_stream::tests) fn create_deferred_provider_grace_details(
    provider_name: &Arc<str>,
    provider_handle: tuliprox_session::ManagedProviderHandle,
) -> StreamDetails {
    StreamDetails {
        shared_subscriber_id: None,
        stream: None,
        stream_info: None,
        provider_name: Some(Arc::clone(provider_name)),
        request_url: Some("http://provider-1.example/live/1".intern()),
        session_headers: None,
        provider_session_headers: tuliprox_session::ProviderSessionHeaders::default(),
        user_agent_stream_index: None,
        grace_period: GracePeriodOptions { period_millis: 100, timeout_secs: 0, hold_stream: true },
        provider_grace_active: true,
        disable_provider_grace: false,
        reconnect_flag: None,
        provider_handle: Some(provider_handle),
        content_representation: ProviderContentRepresentationMode::PreserveOrigin,
        grace_resolution_context: None,
        custom_reason: None,
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        session_registration: None,
    }
}

pub(in crate::api::model::streams::active_client_stream::tests) fn custom_video_test_state(
    mode: StreamMode,
    provisionable: bool,
) -> ActiveClientStreamState {
    let connection_manager = create_test_connection_manager();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 55001));

    ActiveClientStreamState {
        response_mode: tuliprox_session::stream_options::StreamResponseMode::default(),
        inner: None,
        send_custom_stream_flag: Some(Arc::new(AtomicU8::new(mode as u8))),
        provider_handle: None,
        deferred_provider_open: None,
        timed_stream_context: None,
        preempt_cancelled: None,
        grace_task_handle: None,
        provisioning_stop_signal: None,
        provisionable,
        custom_video: CustomVideoBuffers {
            user_exhausted: None,
            provider_exhausted: None,
            unavailable: None,
            provisioning: None,
            low_priority_preempted: None,
        },
        meter: None,
        event_manager: Arc::new(EventManager::new()),
        waker: None,
        connection_manager,
        fingerprint: Arc::new(Fingerprint::new("fp-key".to_string(), "127.0.0.1".to_string(), addr)),
        stream_uid: None,
        provider_stopped: true,
        user_stream_released: true,
        provider_handle_released: true,
        custom_video_timeout_secs: 5,
        custom_video_timeout_mode: None,
        custom_video_timeout_sleep: None,
        direct_body_idle_timeout: DirectBodyIdleTimeout::disabled(),
        provider_end_reason: AtomicU8::new(PROVIDER_END_NOT_SET),
        provider_error_class: None,
        provider_http_status: None,
        provider_reconnect_count: AtomicU8::new(0),
        lease_owner: None,
        media_started: None,
        lease_confirmed: false,
        lease_request_id: None,
        request_cleanup: None,
    }
}

pub(in crate::api::model::streams::active_client_stream::tests) async fn assert_missing_custom_video_terminates(
    mode: StreamMode,
    provisionable: bool,
) {
    let stream = ActiveClientStream { state: custom_video_test_state(mode, provisionable) };
    pin_mut!(stream);
    assert!(stream.next().await.is_none());
}
