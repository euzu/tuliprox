use crate::{
    api::model::{
        ActiveProviderManager, ActiveUserManager, AppState, CancelTokens, ConnectionManager, EventManager,
        MetadataUpdateManager, PlaylistStorageState, SharedStreamManager, UserSession,
    },
    auth::Fingerprint,
    model::{
        AppConfig, Config, ConfigInput, ConfigInputAlias, ConfigTarget, MediaToolCapabilities, NetworkAccess,
        ProxyUserCredentials, SourcesConfig, StreamHistoryConfig,
    },
    repository::GeoIp,
    utils::FileLockManager,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use axum::http::StatusCode;
use bytes::Bytes;
use shared::{
    foundation::Filter,
    model::{
        ClusterFlags, ConfigPaths, ConfigTargetOptions, InputFetchMethod, InputType, PlaylistItemType, ProcessingOrder,
        ProxyType, StreamChannel, UserConnectionPermission, XtreamCluster,
    },
    utils::Internable,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(in crate::api::api_utils::tests) async fn spawn_legacy_hls_test_origin(
    response_head: String,
    response_body: Vec<u8>,
) -> (SocketAddr, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let origin_addr = listener.local_addr().expect("test origin address");
    let origin_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("test origin accepts request");
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut chunk = [0_u8; 1024];
            let read = socket.read(&mut chunk).await.expect("test origin reads request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        socket.write_all(response_head.as_bytes()).await.expect("test origin writes response headers");
        socket.write_all(&response_body).await.expect("test origin writes response body");
        String::from_utf8_lossy(&request).into_owned()
    });
    (origin_addr, origin_task)
}

pub(in crate::api::api_utils::tests) async fn spawn_range_aware_test_origin(
    body: &'static [u8],
    connections: usize,
) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let origin_addr = listener.local_addr().expect("test origin address");
    let origin_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..connections {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.expect("test origin reads request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            let request_lower = String::from_utf8_lossy(&request).to_ascii_lowercase();
            requests.push(request_lower.clone());

            let range = request_lower
                .lines()
                .find_map(|line| line.strip_prefix("range: bytes="))
                .and_then(|spec| spec.split_once('-'))
                .and_then(|(start, end)| {
                    let start: usize = start.trim().parse().ok()?;
                    let end: usize =
                        if end.trim().is_empty() { body.len().saturating_sub(1) } else { end.trim().parse().ok()? };
                    Some((start, end.min(body.len().saturating_sub(1))))
                })
                .unwrap_or((0, body.len().saturating_sub(1)));

            let slice = &body[range.0..=range.1];
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp2t\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                range.0,
                range.1,
                body.len(),
                slice.len()
            );
            socket.write_all(head.as_bytes()).await.expect("test origin writes response headers");
            socket.write_all(slice).await.expect("test origin writes response body");
        }
        requests
    });
    (origin_addr, origin_task)
}

#[allow(dead_code)]
#[derive(Clone)]
pub(in crate::api::api_utils::tests) enum FakeOriginMode {
    EmptyBody,
    BlockFirstChunk(Arc<tokio::sync::Notify>),
    ErrorBeforeFirstChunk,
    ErrorAfterFirstChunk(Vec<u8>),
    FixedBytes(Vec<u8>),
}

pub(in crate::api::api_utils::tests) async fn spawn_controlled_fake_origin(
    mode: FakeOriginMode,
    max_requests: usize,
) -> (SocketAddr, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("test origin binds");
    let origin_addr = listener.local_addr().expect("test origin address");
    let origin_task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..max_requests {
            let Ok((mut socket, _)) = listener.accept().await else { break };
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut chunk = [0_u8; 1024];
                let Ok(read) = socket.read(&mut chunk).await else { break };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            if request.is_empty() {
                continue;
            }
            let request_lower = String::from_utf8_lossy(&request).to_ascii_lowercase();
            requests.push(request_lower);

            match &mode {
                FakeOriginMode::EmptyBody => {
                    let head =
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::BlockFirstChunk(notify) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let mut probe = [0_u8; 1];
                    tokio::select! {
                        () = notify.notified() => {},
                        _ = socket.read(&mut probe) => {},
                    }
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::ErrorBeforeFirstChunk => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::ErrorAfterFirstChunk(chunk) => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    let chunk_hdr = format!("{:x}\r\n", chunk.len());
                    let _ = socket.write_all(chunk_hdr.as_bytes()).await;
                    let _ = socket.write_all(chunk).await;
                    let _ = socket.write_all(b"\r\n").await;
                    let _ = socket.flush().await;
                    let _ = socket.shutdown().await;
                }
                FakeOriginMode::FixedBytes(bytes) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(bytes).await;
                    let _ = socket.shutdown().await;
                }
            }
        }
        requests
    });
    (origin_addr, origin_task)
}

pub(in crate::api::api_utils::tests) async fn spawn_load_test_origin(body: &'static [u8]) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("load origin binds");
    let addr = listener.local_addr().expect("load origin address");
    let app = axum::Router::new().fallback(move || async move {
        ([(axum::http::header::CONTENT_TYPE, "video/mp2t")], Bytes::from_static(body))
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

pub(in crate::api::api_utils::tests) fn validate_load_response(
    status: StatusCode,
    payload: &[u8],
    expected: &[u8],
) -> Result<(), String> {
    if status != StatusCode::OK {
        return Err(format!("unexpected HTTP status {status}"));
    }
    if payload != expected {
        return Err(format!("payload mismatch: expected {} bytes, received {}", expected.len(), payload.len()));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(in crate::api::api_utils::tests) fn load_test_rss_kib() -> Option<u64> {
    let Ok(content) = std::fs::read_to_string("/proc/self/status") else {
        return None;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub(in crate::api::api_utils::tests) fn load_test_rss_kib() -> Option<u64> { None }

pub(in crate::api::api_utils::tests) fn load_test_user(username: &str) -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = username.to_string();
    user
}

pub(in crate::api::api_utils::tests) fn load_test_session(
    input: &ConfigInput,
    origin_addr: SocketAddr,
    token: &str,
    addr: SocketAddr,
) -> UserSession {
    UserSession {
        token: token.to_string(),
        transition_version: 1,
        virtual_id: 42,
        provider: Arc::clone(&input.name),
        stream_url: format!("http://{origin_addr}/live/42.ts").intern(),
        provider_session_headers: HashMap::new(),
        provider_session_headers_host: None,
        media_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        user_agent_stream_index: None,
        addr,
        socket_bound: false,
        active_addrs: vec![addr],
        ts: 1,
        started_at: 1,
        permission: UserConnectionPermission::Allowed,
        connection_kind: Some(crate::api::model::ConnectionKind::Normal),
        lifecycle: crate::api::model::PlaybackLifecycle::Active,
        ..Default::default()
    }
}

pub(in crate::api::api_utils::tests) fn load_test_channel(
    input: &ConfigInput,
    origin_addr: SocketAddr,
) -> StreamChannel {
    let mut channel = create_test_live_channel(&format!("http://{origin_addr}/live/42.ts"));
    channel.provider_id = u32::from(input.id);
    channel.input_name = Arc::clone(&input.name);
    channel.url = format!("http://{origin_addr}/live/42.ts").intern();
    channel
}

pub(in crate::api::api_utils::tests) fn create_test_app_config() -> AppConfig {
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "local_media".intern(),
        input_type: InputType::Library,
        headers: HashMap::default(),
        url: "file:///tmp".to_string(),
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

pub(in crate::api::api_utils::tests) fn create_test_provider_app_config() -> AppConfig {
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

pub(in crate::api::api_utils::tests) fn create_test_dual_provider_app_config() -> AppConfig {
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
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "provider_2".intern(),
            url: "http://provider-2.example".to_string(),
            username: Some("user2".to_string()),
            password: Some("pass2".to_string()),
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
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

pub(in crate::api::api_utils::tests) fn create_test_app_state() -> Arc<AppState> {
    create_test_app_state_for_config(Arc::new(create_test_app_config()))
}

pub(in crate::api::api_utils::tests) fn create_test_provider_app_state() -> Arc<AppState> {
    create_test_app_state_for_config(Arc::new(create_test_provider_app_config()))
}

pub(in crate::api::api_utils::tests) fn create_test_dual_provider_app_state() -> Arc<AppState> {
    create_test_app_state_for_config(Arc::new(create_test_dual_provider_app_config()))
}

pub(in crate::api::api_utils::tests) fn create_test_app_state_for_config(app_cfg: Arc<AppConfig>) -> Arc<AppState> {
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

    let tokens = CancelTokens::default();
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
        playlist_updates: crate::api::model::PlaylistUpdateControl::for_tests(),
    })
}

pub(in crate::api::api_utils::tests) fn create_test_fingerprint(addr: std::net::SocketAddr) -> Fingerprint {
    Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr)
}

pub(in crate::api::api_utils::tests) fn create_test_app_state_with_stream_config(
    stream: crate::model::StreamConfig,
) -> Arc<AppState> {
    let config = Config {
        reverse_proxy: Some(crate::model::ReverseProxyConfig {
            resource_rewrite_disabled: false,
            rewrite_secret: [0; 16],
            resource_retry: crate::model::ResourceRetryConfig::default(),
            disabled_header: None,
            stream: Some(stream),
            cache: None,
            rate_limit: None,
            geoip: None,
            stream_history: None,
            qos_aggregation: None,
            hls_cache: None,
        }),
        user_access_control: true,
        ..Config::default()
    };

    let mut app_cfg = create_test_app_config();
    app_cfg.config = Arc::new(ArcSwap::from_pointee(config));
    create_test_app_state_for_config(Arc::new(app_cfg))
}

pub(in crate::api::api_utils::tests) fn create_test_local_channel(url: &str) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id: 41,
        provider_id: 0,
        input_name: "library".intern(),
        item_type: PlaylistItemType::LocalVideo,
        cluster: XtreamCluster::Video,
        group: "Local Movies".intern(),
        title: "Local Test".intern(),
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

pub(in crate::api::api_utils::tests) fn create_test_live_channel(url: &str) -> StreamChannel {
    StreamChannel {
        target_id: 1,
        virtual_id: 42,
        provider_id: 1,
        input_name: "provider_1".intern(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        group: "Live".intern(),
        title: "Shared Live".intern(),
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

pub(in crate::api::api_utils::tests) fn create_test_shared_target() -> ConfigTarget {
    ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: "shared".to_string(),
        options: Some(ConfigTargetOptions {
            share_live_streams: shared::model::ConfigTargetShareLiveStreams { mpeg_ts: true, ..Default::default() },
            ..ConfigTargetOptions::default()
        }),
        sort: None,
        filter: Filter::default().into(),
        output: Vec::new(),
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::default()),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    }
}

/// `Arc<ArcSwapOption<GeoIp>>` with a mock `GeoIP` that always reports the
/// given country for any lookup.
pub(in crate::api::api_utils::tests) fn mock_geoip(country: &str) -> Arc<ArcSwapOption<GeoIp>> {
    Arc::new(ArcSwapOption::from(Some(Arc::new(GeoIp::test_new(country)))))
}

/// Helper to create a test user with specific network access
pub(in crate::api::api_utils::tests) fn user_with_network_access(
    network_access: Option<NetworkAccess>,
) -> ProxyUserCredentials {
    ProxyUserCredentials {
        username: "test".to_string(),
        password: "test".to_string(),
        token: None,
        proxy: ProxyType::default(),
        server: None,
        epg_timeshift: None,
        epg_request_timeshift: None,
        created_at: None,
        exp_date: None,
        max_connections: 0,
        status: None,
        output_clusters: ClusterFlags::all(),
        ui_enabled: true,
        comment: None,
        priority: 0,
        soft_connections: 0,
        soft_priority: 0,
        t_is_api_user: false,
        network_access,
        plan: None,
        filter: None,
        raw_output_clusters: None,
        raw_max_connections: 0,
        raw_soft_connections: 0,
        raw_proxy: Some(ProxyType::default()),
        t_filter: None,
        t_has_unresolved_plan: false,
        t_has_invalid_filter: false,
    }
}
