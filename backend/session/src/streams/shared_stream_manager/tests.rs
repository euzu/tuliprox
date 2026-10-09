use super::{
    buffer::{send_client_chunk, SendOutcome},
    BudgetedChunk, BurstBuffer, SharedStreamManager, SharedStreamState, MIN_BURST_BUFFER_CHUNK_ACCOUNTING_BYTES,
    SHARED_CLEANUP_ADMISSION_TIMEOUT,
};
use crate::{
    streams::buffered_stream::CHANNEL_SIZE, ActiveProviderManager, ActiveUserConnectionParams, ActiveUserManager,
    ConnectionKind, ConnectionManager, ConnectionRejectionReason, EventManager, ManagedProviderHandle,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use bytes::Bytes;
use futures::StreamExt;
use shared::{
    model::{ConfigPaths, InputFetchMethod, InputType, PlaylistItemType, StreamChannel, XtreamCluster},
    utils::Internable,
};
use std::{borrow::Cow, collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    sync::{mpsc, Semaphore},
    time::{timeout, Duration, Instant},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tuliprox_core::{
    model::{AppConfig, Config, ConfigInput, Fingerprint, MediaToolCapabilities, SharedSubscriberId, SourcesConfig},
    utils::FileLockManager,
};

fn create_test_app_config() -> AppConfig { create_test_app_config_with_conns(1) }

fn create_test_app_config_with_conns(max_connections: u16) -> AppConfig {
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
        max_connections,
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

fn create_test_stream_channel(url: &str) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id: 1,
        provider_id: 1,
        input_name: "provider_1".intern(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        group: "group".intern(),
        title: "title".intern(),
        url: url.intern(),
        shared: true,
        shared_joined_existing: Some(false),
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    }
}

fn create_test_connection_manager(
    app_cfg: &AppConfig,
    event_manager: &Arc<EventManager>,
) -> (Arc<ActiveProviderManager>, Arc<ActiveUserManager>, Arc<SharedStreamManager>, Arc<ConnectionManager>) {
    let provider_manager = Arc::new(ActiveProviderManager::new(app_cfg, event_manager));
    let geoip = Arc::new(ArcSwapOption::default());
    let user_manager = Arc::new(ActiveUserManager::new(&Config::default(), &geoip, event_manager));
    let shared_manager = Arc::new(SharedStreamManager::new(Arc::clone(&provider_manager)));
    let connection_manager =
        Arc::new(ConnectionManager::new(&user_manager, &provider_manager, &shared_manager, event_manager, None));
    (provider_manager, user_manager, shared_manager, connection_manager)
}

async fn register_user(
    users: &ActiveUserManager,
    addr: SocketAddr,
    uid: u32,
    username: &str,
    stream_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let fingerprint = Fingerprint::new(username.to_string(), username.to_string(), addr);
    let channel = create_test_stream_channel(stream_url);
    let stream = users
        .update_connection(ActiveUserConnectionParams {
            uid,
            meter_uid: 0,
            username,
            max_connections: 5,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fingerprint,
            provider: "provider_1".intern(),
            stream_channel: &channel,
            user_agent: Cow::Borrowed("test"),
            session_token: None,
        })
        .await
        .ok_or("user stream missing")?;
    assert_eq!(stream.uid, uid);
    Ok(())
}

mod admission;
mod lifecycle;
mod policy;
mod streaming;
mod terminal;
mod transport;
