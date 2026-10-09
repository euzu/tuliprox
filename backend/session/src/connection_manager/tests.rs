use super::{
    history::{playback_outcome_for_reason, resolve_disconnect_reason, resolve_disconnect_reason_from_provider_end},
    *,
};
use crate::{
    ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager, CreateUserSessionParams, EventManager,
    SharedStreamManager,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::{
    model::{
        ConfigPaths, CustomVideoStreamType, DisconnectReason, FailureStage, InputFetchMethod, InputType,
        PlaylistItemType, ProxyType, StreamChannel, StreamInfo, UserConnectionPermission, XtreamCluster,
    },
    utils::Internable,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
};
use tokio::sync::mpsc;
use tuliprox_core::{
    model::{
        AppConfig, Config, ConfigInput, DisconnectQos, MediaToolCapabilities, PlaybackRequestOutcome,
        ProxyUserCredentials, SourcesConfig,
    },
    utils::FileLockManager,
};
use tuliprox_repository::GeoIp;

fn make_stream_info(provider: &str, title: &str) -> StreamInfo {
    let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap_or_else(|_| unreachable!());
    let channel = StreamChannel {
        target_id: 1,
        virtual_id: 1,
        provider_id: 1,
        input_name: "input".intern(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        group: "".intern(),
        title: title.intern(),
        url: "".intern(),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    };
    StreamInfo::new(shared::model::StreamInfoParams {
        uid: 0,
        meter_uid: 0,
        username: "test",
        addr: &addr,
        client_ip: "127.0.0.1",
        provider: provider.intern(),
        stream_channel: channel,
        user_agent: String::new(),
        country_code: None,
        session_token: None,
    })
}

fn create_test_app_config() -> AppConfig {
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

fn create_test_connection_manager() -> Arc<ConnectionManager> {
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

fn create_test_proxy_user(username: &str) -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user.password = "password".to_string();
    user.proxy = ProxyType::Reverse(None);
    user.max_connections = 1;
    user
}

mod admission;
mod http;
mod lifecycle;
mod playlist;
mod policy;
mod streaming;
mod terminal;
mod transport;
