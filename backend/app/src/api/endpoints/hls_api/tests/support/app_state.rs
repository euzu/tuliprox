use super::{
    super::{
        build_group_lookup, ActiveProviderManager, ActiveUserManager, ApiProxyConfig, ApiProxyServerInfo, AppConfig,
        AppState, Arc, ArcSwap, ArcSwapOption, Body, CacheAccessState, CancelTokens, Config, ConfigInput, ConfigPaths,
        ConfigSource, ConfigTarget, ConfigTargetDto, ConfigTargetOptions, ConfigTargetShareLiveStreams,
        ConnectionManager, EventManager, GeoIp, HlsAccessContext, HlsAccessLeaseId, HlsCacheConfigDto,
        HlsLeaseManifestSnapshot, HlsManifestCommitIdentity, HlsManifestDeliveryMode, HlsMediaContainer,
        HlsPlaybackFamilyKey, HlsProxyManager, HlsPublishedTransientResourceIds, InputType, Internable,
        M3uPlaylistItem, M3uTargetOutputDto, MetadataUpdateManager, OriginSegmentFetchRef, OriginSegmentKey,
        PlaylistItem, PlaylistItemHeader, PlaylistItemType, PlaylistStorageState, ProxySessionId, ProxyUserCredentials,
        Response, ReverseProxyConfig, ReverseProxyConfigDto, SegmentCacheKey, SegmentCacheStatus, SegmentEntry,
        SharedStreamManager, SourcesConfig, TargetOutputDto, TargetUser, TransportStreamBuffer, VirtualId,
        XtreamCluster, XtreamTargetOutputDto,
    },
    response_body, test_fingerprint,
};

pub(in crate::api::endpoints::hls_api::tests) fn path_has_extension(path: &str, extension: &str) -> bool {
    std::path::Path::new(path).extension().is_some_and(|actual| actual.eq_ignore_ascii_case(extension))
}

/// App config with one `hls-user` limited to one connection.
pub(in crate::api::endpoints::hls_api::tests) fn test_app_config() -> Arc<AppConfig> {
    let mut hls_user = ProxyUserCredentials::default();
    hls_user.username = "hls-user".to_string();
    hls_user.password = "hls-pass".to_string();
    hls_user.max_connections = 1;
    let api_proxy = ApiProxyConfig {
        server: vec![ApiProxyServerInfo {
            name: "default".to_string(),
            protocol: "https".to_string(),
            host: "example.test".to_string(),
            port: None,
            timezone: "UTC".to_string(),
            message: String::new(),
            path: Some("iptv".to_string()),
        }],
        user: vec![TargetUser { target: "default".to_string(), credentials: vec![Arc::new(hls_user)] }],
        ..Default::default()
    };
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config { custom_stream_response_enabled: true, ..Default::default() })),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::empty()),
        plugins: Arc::default(),
        api_proxy: Arc::new(ArcSwapOption::from(Some(Arc::new(api_proxy)))),
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
            failover_file_path: None,
            api_proxy_file_path: String::new(),
            custom_stream_response_path: None,
        })),
        custom_stream_response: Arc::new(ArcSwapOption::empty()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(crate::model::MediaToolCapabilities::default()),
        failover: Arc::default(),
    })
}

/// Returns the test app config with `storage_dir` set to the given path.
pub(in crate::api::endpoints::hls_api::tests) fn test_app_config_with_storage_dir(
    storage_dir: &std::path::Path,
) -> Arc<AppConfig> {
    let app_config = test_app_config();
    let mut config = (**app_config.config.load()).clone();
    config.storage_dir = storage_dir.to_string_lossy().to_string();
    app_config.config.store(Arc::new(config));
    app_config
}

pub(in crate::api::endpoints::hls_api::tests) fn hls_custom_video_test_user() -> ProxyUserCredentials {
    let mut user = ProxyUserCredentials::default();
    user.username = "viewer".to_string();
    user.password = "secret".to_string();
    user
}

pub(in crate::api::endpoints::hls_api::tests) fn test_hls_share_target(hls_enabled: bool) -> ConfigTarget {
    ConfigTarget::from(&ConfigTargetDto {
        id: 1,
        name: "default".to_string(),
        options: Some(ConfigTargetOptions {
            share_live_streams: ConfigTargetShareLiveStreams { hls: hls_enabled, mpeg_ts: false },
            ..Default::default()
        }),
        output: vec![TargetOutputDto::Xtream(XtreamTargetOutputDto::default())],
        ..Default::default()
    })
}

pub(in crate::api::endpoints::hls_api::tests) fn test_m3u_hls_share_target() -> ConfigTarget {
    ConfigTarget::from(&ConfigTargetDto {
        id: 2,
        name: "m3u-target".to_string(),
        options: Some(ConfigTargetOptions {
            share_live_streams: ConfigTargetShareLiveStreams { hls: true, mpeg_ts: false },
            ..Default::default()
        }),
        output: vec![TargetOutputDto::M3u(M3uTargetOutputDto::default())],
        use_memory_cache: true,
        ..Default::default()
    })
}

pub(in crate::api::endpoints::hls_api::tests) fn test_m3u_hls_item(
    input: &ConfigInput,
    virtual_id: u32,
    input_stream_id: &str,
    url: &str,
) -> M3uPlaylistItem {
    M3uPlaylistItem::from(&PlaylistItem {
        header: PlaylistItemHeader {
            id: input_stream_id.intern(),
            virtual_id: VirtualId::new(virtual_id),
            input_name: Arc::clone(&input.name),
            url: url.intern(),
            item_type: PlaylistItemType::LiveHls,
            xtream_cluster: XtreamCluster::Live,
            input_stream_id: input_stream_id.intern(),
            ..PlaylistItemHeader::default()
        },
    })
}

pub(in crate::api::endpoints::hls_api::tests) fn test_hls_entry_stream_context(
    virtual_id: u32,
    input_stream_id: &str,
    known_bitrate_bps: Option<u32>,
) -> super::super::super::HlsEntryStreamContext {
    super::super::super::HlsEntryStreamContext {
        identity: super::super::super::HlsEntryStreamIdentity::new(virtual_id, input_stream_id)
            .expect("valid input stream identity"),
        known_bitrate_bps,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn cache_test_m3u_hls_item(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    item: M3uPlaylistItem,
) {
    let mut playlist = crate::repository::BPlusTree::new();
    playlist.insert(item.virtual_id.get(), item);
    app_state
        .playlists
        .cache_playlist(&target.name, crate::api::model::PlaylistStorage::M3uPlaylist(Box::new(playlist)))
        .await;
}

pub(in crate::api::endpoints::hls_api::tests) fn test_hls_input() -> ConfigInput {
    ConfigInput {
        id: 1,
        name: Arc::from("test-input"),
        input_type: InputType::Xtream,
        url: "http://origin.example.com".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        max_connections: 1,
        enabled: true,
        ..ConfigInput::default()
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn store_test_sources_with_target(
    app_state: &Arc<AppState>,
    input: ConfigInput,
    target: ConfigTarget,
) {
    let input = Arc::new(input);
    let inputs = vec![Arc::clone(&input)];
    app_state.app_config.sources.store(Arc::new(SourcesConfig {
        batch_files: vec![],
        provider: vec![],
        group_lookup: build_group_lookup(&inputs),
        inputs,
        sources: vec![ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![Arc::new(target)] }],
        templates: None,
    }));
}

pub(in crate::api::endpoints::hls_api::tests) fn configure_default_test_server(app_state: &Arc<AppState>) {
    let users =
        app_state.app_config.api_proxy.load_full().as_ref().map_or_else(Vec::new, |api_proxy| api_proxy.user.clone());
    app_state.app_config.api_proxy.store(Some(Arc::new(ApiProxyConfig {
        server: vec![ApiProxyServerInfo {
            name: "default".to_string(),
            protocol: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port: Some("8901".to_string()),
            timezone: "UTC".to_string(),
            message: String::new(),
            path: None,
        }],
        user: users,
        ..Default::default()
    })));
}

pub(in crate::api::endpoints::hls_api::tests) fn test_app_state() -> Arc<AppState> {
    test_app_state_with_hls_proxy(Arc::new(HlsProxyManager::new()))
}

pub(in crate::api::endpoints::hls_api::tests) fn test_custom_video_buffer() -> TransportStreamBuffer {
    TransportStreamBuffer::new(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts")).to_vec(),
    )
}

pub(in crate::api::endpoints::hls_api::tests) fn test_app_state_with_inputs(
    inputs: Vec<Arc<ConfigInput>>,
) -> Arc<AppState> {
    test_app_state_with_hls_proxy_and_inputs(Arc::new(HlsProxyManager::new()), inputs)
}

/// Origin budgets that loopback test origins rely on.
///
/// The production default (`origin_manifest_timeout_ms` = 3 s) is a wall-clock budget for real provider links.
/// A loaded or single-threaded test runtime can exceed it while the loopback origin answers in microseconds;
/// the manifest attempt then fails, no cached manifest is published, and the entry request degrades to the
/// failed-start fallback status instead of the manifest.
pub(in crate::api::endpoints::hls_api::tests) const TEST_HLS_ORIGIN_MANIFEST_TIMEOUT_MS: u64 = 30_000;
/// Initial manifest wait written into test configs. It equals today's production default; it is spelled out so a
/// change of that default does not silently change how long HLS tests wait for the bootstrap decision.
pub(in crate::api::endpoints::hls_api::tests) const TEST_HLS_INITIAL_MANIFEST_WAIT_SECS: u64 = 90;

/// HLS cache config for tests: the manager's cache path plus loopback-safe origin budgets.
pub(in crate::api::endpoints::hls_api::tests) fn test_hls_cache_dto(app_state: &Arc<AppState>) -> HlsCacheConfigDto {
    app_state.hls.proxy.keep_cache_path(HlsCacheConfigDto {
        origin_manifest_timeout_ms: shared::model::Millis::new(TEST_HLS_ORIGIN_MANIFEST_TIMEOUT_MS),
        initial_manifest_wait_timeout_secs: shared::model::Secs::new(TEST_HLS_INITIAL_MANIFEST_WAIT_SECS),
        ..HlsCacheConfigDto::default()
    })
}

/// Enables the HLS cache for a test app state.
///
/// Stores the config and lifts the running manager's origin manifest budget, which is also the AES key and MAP
/// fetch deadline, to the loopback test budget. Other manager settings stay untouched: tests build managers with
/// settings of their own that `update_config` would replace. The lift applies only while the manager runs the
/// production default; a test that sets 3000 ms on purpose must set it after this call.
pub(in crate::api::endpoints::hls_api::tests) fn enable_hls_cache(app_state: &Arc<AppState>) {
    let config = Config {
        reverse_proxy: Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
            hls_cache: Some(test_hls_cache_dto(app_state)),
            ..Default::default()
        })),
        ..Default::default()
    };
    app_state.app_config.config.store(Arc::new(config));
    let proxy = &app_state.hls.proxy;
    if proxy.origin_manifest_timeout_ms() == shared::defaults::default_hls_origin_manifest_timeout_ms().get() {
        proxy.update_origin_preferences(
            proxy.min_origin_target_duration_secs(),
            proxy.segment_fetch_policy().timeouts.config().segment,
            TEST_HLS_ORIGIN_MANIFEST_TIMEOUT_MS,
        );
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn configure_test_hls_strip(
    app_state: &Arc<AppState>,
    strip: &tuliprox_core::model::StripConfig,
) {
    let mut config = app_state.app_config.config.load().as_ref().clone();
    config.reverse_proxy.as_mut().unwrap().hls_cache.as_mut().unwrap().strip = strip.clone();
    app_state.app_config.config.store(Arc::new(config));
    app_state.hls.proxy.update_config(&app_state.app_config).await;
}

pub(in crate::api::endpoints::hls_api::tests) fn test_app_state_with_hls_proxy(
    hls_proxy: Arc<HlsProxyManager>,
) -> Arc<AppState> {
    test_app_state_with_hls_proxy_and_inputs(hls_proxy, Vec::new())
}

pub(in crate::api::endpoints::hls_api::tests) fn test_app_state_with_hls_proxy_and_inputs(
    hls_proxy: Arc<HlsProxyManager>,
    inputs: Vec<Arc<ConfigInput>>,
) -> Arc<AppState> {
    let app_config = test_app_config();
    if !inputs.is_empty() {
        app_config.sources.store(Arc::new(SourcesConfig {
            batch_files: vec![],
            provider: vec![],
            group_lookup: build_group_lookup(&inputs),
            inputs,
            sources: vec![],
            templates: None,
        }));
    }
    test_app_state_with_config_and_hls_proxy(app_config, hls_proxy)
}

pub(in crate::api::endpoints::hls_api::tests) fn test_app_state_with_config_and_hls_proxy(
    app_config: Arc<AppConfig>,
    hls_proxy: Arc<HlsProxyManager>,
) -> Arc<AppState> {
    let event_manager = Arc::new(EventManager::new());
    let active_provider = Arc::new(ActiveProviderManager::new(&app_config, &event_manager));
    let shared_stream_manager = Arc::new(SharedStreamManager::new(Arc::clone(&active_provider)));
    active_provider.set_shared_stream_manager(&shared_stream_manager);

    let geoip = Arc::new(ArcSwapOption::<GeoIp>::default());
    let config = app_config.config.load();
    let active_users = Arc::new(ActiveUserManager::new(&config, &geoip, &event_manager));
    let connection_manager =
        Arc::new(ConnectionManager::new(&active_users, &active_provider, &shared_stream_manager, &event_manager, None));
    let cancel_tokens = CancelTokens::default();
    let metadata_manager = Arc::new(MetadataUpdateManager::new(cancel_tokens.metadata.clone()));

    Arc::new(AppState {
        recording: crate::api::model::RecordingState::new(
            Arc::new(crate::api::model::RecordingQueue::new()),
            crate::api::model::recording_runtime::ProviderCapacityAdapter::new(
                Arc::clone(&active_provider),
                Arc::clone(&connection_manager),
                crate::api::model::create_test_maintenance(""),
            ),
        ),
        app_config,
        http_clients: Arc::new(tuliprox_core::model::HttpClients::new(
            reqwest::Client::new(),
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("no-redirect client builds"),
            reqwest::Client::new(),
            reqwest::Client::new(),
            reqwest::Client::new(),
        )),
        cache: Arc::new(ArcSwapOption::default()),
        shared_stream_manager,
        hls: crate::api::model::HlsState::new(hls_proxy),
        stalker_resolve_coordinator: crate::api::model::StalkerResolveCoordinator::default(),
        epg_scans: crate::api::model::EpgScanCoordinator::default(),
        active_users,
        active_provider,
        connection_manager,
        event_manager,
        cancel_tokens: ArcSwap::from_pointee(cancel_tokens),
        playlists: Arc::new(PlaylistStorageState::new()),
        geoip,
        metadata_manager,
        auth: crate::api::model::AuthState::for_tests(),
        playlist_updates: crate::api::model::PlaylistUpdateControl::for_tests(),
        overrides: crate::api::model::OverrideState::default(),
        maintenance: crate::api::model::create_test_maintenance(""),
        member_health: Arc::new(tuliprox_session::MemberHealthIndex::default()),
        failover_switch_registry: Arc::default(),
        plugins: crate::api::model::AppPlugins::default(),
    })
}

pub(in crate::api::endpoints::hls_api::tests) fn test_stalker_app_state(
    input_name: &str,
    portal_url: &str,
    storage_dir: &std::path::Path,
) -> (Arc<AppState>, Arc<ConfigInput>, ConfigTarget) {
    let app_config = test_app_config_with_storage_dir(storage_dir);
    let mut config = (**app_config.config.load()).clone();
    config.reverse_proxy = Some(ReverseProxyConfig::from(&ReverseProxyConfigDto {
        stream: Some(shared::model::StreamConfigDto {
            hls_wrap_media_playlist: false,
            ..shared::model::StreamConfigDto::default()
        }),
        ..ReverseProxyConfigDto::default()
    }));
    app_config.config.store(Arc::new(config));

    let input = Arc::new(ConfigInput {
        id: 7,
        name: input_name.intern(),
        input_type: InputType::Stalker,
        url: portal_url.to_string(),
        enabled: true,
        max_connections: 1,
        options: Some(crate::model::ConfigInputOptions {
            flags: crate::model::ConfigInputFlagsSet::new(),
            update_quality: crate::model::ConfigInputUpdateQuality::default(),
            resolve_delay: shared::defaults::default_resolve_delay_secs(),
            probe_delay: shared::defaults::default_probe_delay_secs(),
            probe_live_interval_hours: 120,
            resolve_filter: None,
            probe_filter: None,
            flussonic_hls_catchup: shared::model::FlussonicHlsCatchup::Native,
            flussonic_hls_catchup_max_duration_secs: shared::model::default_flussonic_hls_catchup_max_duration_secs(),
            default_container_extension: None,
        }),
        stalker: Some(crate::model::StalkerInputConfig {
            device: None,
            auth_mode: shared::model::StalkerAuthMode::Auto,
            mag_preset: shared::model::StalkerMagPreset::GenericSafe,
            endpoint_preference: shared::model::StalkerEndpointPreference::ServerLoad,
            size_caps: None,
            catalog_max_pages: None,
            ..Default::default()
        }),
        ..ConfigInput::default()
    });
    let target = test_hls_share_target(false);
    let inputs = vec![Arc::clone(&input)];
    app_config.sources.store(Arc::new(SourcesConfig {
        batch_files: vec![],
        provider: vec![],
        group_lookup: build_group_lookup(&inputs),
        inputs,
        sources: vec![ConfigSource { inputs: vec![Arc::clone(&input.name)], targets: vec![Arc::new(target.clone())] }],
        templates: None,
    }));
    let app_state = test_app_state_with_config_and_hls_proxy(app_config, Arc::new(HlsProxyManager::new()));
    (app_state, input, target)
}

pub(in crate::api::endpoints::hls_api::tests) fn single_hls_provider_input(name: &str) -> ConfigInput {
    ConfigInput {
        id: 1,
        name: Arc::from(name),
        input_type: InputType::Xtream,
        url: "http://account.example.com".to_string(),
        username: Some("account-user".to_string()),
        password: Some("account-pass".to_string()),
        enabled: true,
        priority: 0,
        max_connections: 1,
        ..ConfigInput::default()
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn overlap_provider_input() -> ConfigInput {
    ConfigInput {
        id: 1,
        name: Arc::from("overlap-input"),
        input_type: InputType::Xtream,
        url: "http://root.example.com".to_string(),
        username: Some("root-user".to_string()),
        password: Some("root-pass".to_string()),
        enabled: true,
        priority: 10,
        max_connections: 1,
        aliases: Some(vec![crate::model::ConfigInputAlias {
            id: 2,
            name: Arc::from("account-a"),
            url: "http://account.example.com".to_string(),
            username: Some("account-user".to_string()),
            password: Some("account-pass".to_string()),
            priority: 0,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn test_segment_entry(
    proxy_session_id: &ProxySessionId,
    proxy_seq: u64,
    status: SegmentCacheStatus,
) -> SegmentEntry {
    SegmentEntry {
        origin_key: OriginSegmentKey {
            origin_epoch: 0,
            effective_host_id: 0,
            host_local_sequence: proxy_seq,
            host_local_index: u32::try_from(proxy_seq).unwrap_or(u32::MAX),
        },
        proxy_seq,
        media_start_ms: proxy_seq * 4_000,
        duration_ms: 4_000,
        proxy_file_ext: "ts".to_string(),
        content_type: "video/mp2t".to_string(),
        cache_key: SegmentCacheKey::new(proxy_session_id.clone(), proxy_seq, "ts"),
        discontinuity_before: false,
        program_date_time: None,
        daterange_tags_before: Vec::new(),
        origin_byte_range: None,
        map_ref: None,
        encryption: None,
        retained_resource_identity: None,
        origin_fetch_ref: Some(OriginSegmentFetchRef {
            resolved_origin_url: format!("http://origin.example.com/{proxy_seq}.ts"),
            byte_range: None,
            valid_until_ms: None,
        }),
        status,
        last_rendered_at_ms: None,
        access: Arc::new(CacheAccessState::new()),
    }
}

pub(in crate::api::endpoints::hls_api::tests) fn test_hls_access_context(
    proxy_session_id: ProxySessionId,
    access_lease_id: HlsAccessLeaseId,
) -> HlsAccessContext {
    test_hls_access_context_with(proxy_session_id, access_lease_id, "hls-session-token", test_fingerprint().key)
}

pub(in crate::api::endpoints::hls_api::tests) fn test_hls_access_context_with(
    proxy_session_id: ProxySessionId,
    access_lease_id: HlsAccessLeaseId,
    user_session_token: &str,
    client_fingerprint: String,
) -> HlsAccessContext {
    HlsAccessContext {
        username: "hls-user".to_string(),
        user_session_token: user_session_token.to_string(),
        proxy_session_id,
        input_id: 1,
        stream_ref: "12345".to_string(),
        virtual_id: 12345,
        known_bitrate_bps: None,
        lease_id: access_lease_id,
        family_key: HlsPlaybackFamilyKey::new("hls-user", client_fingerprint),
        epg_reference_ts: None,
        archive_origin_url: None,
    }
}

pub(in crate::api::endpoints::hls_api::tests) async fn publish_test_transient_resource_membership(
    app_state: &Arc<AppState>,
    proxy_session_id: &str,
    access_lease_id: &str,
    resource_uri: &str,
) {
    let proxy_session_id = ProxySessionId(proxy_session_id.to_string());
    let access_lease_id = HlsAccessLeaseId(access_lease_id.to_string());
    let now_ms = super::super::super::current_time_millis();
    let publication = app_state
        .hls
        .proxy
        .prepare_access_lease_manifest_publication_for_test(&access_lease_id, &proxy_session_id, now_ms)
        .await
        .expect("test resource lease accepts manifest publication");
    let snapshot = HlsLeaseManifestSnapshot {
        startup_revisions: None,
        delivery_mode: HlsManifestDeliveryMode::TransientPassthrough,
        source_commit_identity: HlsManifestCommitIdentity::new(now_ms),
        uri_materialization: None,
        finalized_transient_manifest_generation: None,
        snapshot_generation: 0,
        delivered_at_ms: now_ms,
        first_proxy_seq: 0,
        last_proxy_seq: 0,
        visible_segments: Arc::from([]),
        discontinuity_sequence: 0,
        target_duration_ms: 4_000,
        playlist_duration_ms: 0,
        last_visible_media_end_ms: 0,
        active_map: None,
        active_encryption: None,
        container: HlsMediaContainer::MpegTs,
    };
    let published_resource_ids = HlsPublishedTransientResourceIds::from_manifest_body(resource_uri);
    assert!(app_state
        .hls
        .proxy
        .commit_access_lease_manifest_publication_with_resources(
            &access_lease_id,
            &proxy_session_id,
            publication,
            snapshot,
            published_resource_ids,
            now_ms,
        )
        .await
        .is_committed());
}

pub(in crate::api::endpoints::hls_api::tests) async fn single_variant_master_playlist(
    response: Response<Body>,
) -> (u32, String) {
    let body = response_body(response).await;
    let body = std::str::from_utf8(&body).expect("master playlist should be UTF-8");
    let mut lines = body.lines();
    assert_eq!(lines.next(), Some("#EXTM3U"));
    let bandwidth = lines
        .next()
        .and_then(|line| line.strip_prefix("#EXT-X-STREAM-INF:BANDWIDTH="))
        .and_then(|value| value.parse::<u32>().ok())
        .expect("positive master playlist bandwidth");
    let uri = lines.next().expect("single variant URI").to_string();
    assert!(lines.next().is_none(), "master playlist must contain exactly one variant");
    (bandwidth, uri)
}
