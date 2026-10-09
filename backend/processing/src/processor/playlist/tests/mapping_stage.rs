use super::*;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{ConfigPaths, EpgSmartMatchConfigDto};
use std::sync::Arc;
use tempfile::tempdir;
use tokio::runtime::Runtime;
use tuliprox_core::{
    model::{
        EpgConfig, EpgSmartMatchConfig, IcsEpgSourceConfig, MediaToolCapabilities, PersistedEpgSource,
        PersistedEpgSourceKind, SourcesConfig,
    },
    utils::FileLockManager,
};

pub(super) fn build_mapping(id: &str, stage: MappingStage, script: &str) -> CompiledMapping {
    CompiledMapping {
        id: id.to_string(),
        match_as_ascii: false,
        stage,
        rules: vec![CompiledMappingRule {
            name: None,
            filter: get_filter(r#"name ~ ".*""#, None).expect("filter parses"),
            program: MappingProgram::Script(MapperScript::parse(script, None).expect("script parses")),
        }],
        counters: vec![],
        templates: None,
    }
}

pub(super) fn build_target(mappings: Vec<CompiledMapping>, remove_duplicates: bool) -> ConfigTarget {
    let dto = ConfigTargetDto {
        options: if remove_duplicates {
            Some(ConfigTargetOptions { remove_duplicates, ..Default::default() })
        } else {
            None
        },
        ..Default::default()
    };
    let mut target = ConfigTarget::from(&dto);
    target.mapping = Arc::new(ArcSwapOption::from(Some(Arc::new(CompiledTargetMappings::new(
        mappings.into_iter().map(Arc::new).collect(),
    )))));
    target
}

/// Pinned to `NoopSink` rather than staying generic: these tests
/// exercise the pipeline, not the bus, and an inferred sink type
/// would just make every call site name one.
fn processing_context() -> PlaylistProcessingContext<shared::model::NoopSink> {
    let paths = ConfigPaths {
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
    };
    let config = AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config::default())),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::default()),
        api_proxy: Arc::new(ArcSwapOption::default()),
        file_locks: Arc::new(FileLockManager::default()),
        paths: Arc::new(ArcSwap::from_pointee(paths)),
        custom_stream_response: Arc::new(ArcSwapOption::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    };
    PlaylistProcessingContext {
        client: reqwest::Client::new(),
        run_id: "m3u-alias-test-run".into(),
        execution_order: PlaylistUpdateRunOrder::from(1),
        config: Arc::new(config),
        user_targets: Arc::new(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        }),
        events: shared::model::NoopSink,
        playlist_state: None,
        disabled_headers: None,
        processed_inputs: Arc::new(Mutex::new(HashSet::new())),
        input_completions: Arc::new(Mutex::new(HashMap::new())),
        input_locks: Arc::new(Mutex::new(HashMap::new())),
        provider_manager: None,
        metadata_manager: None,
        pre_processed_inputs: None,
        input_refresh: None,
        library_update_mode: LibraryUpdateMode::ExistingCatalog,
        stalker_refresh_mode: StalkerRefreshMode::Complete,
        partial_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        had_quality_rejections: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

#[tokio::test]
async fn m3u_alias_playlist_is_downloaded_and_indexed_separately() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let primary_playlist_path = temp.path().join("primary.m3u");
    let alias_playlist_path = temp.path().join("backup.m3u");
    tokio::fs::write(
            &primary_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=primary-stream-token\n",
        )
        .await
        .expect("primary fixture should be written");
    tokio::fs::write(
            &alias_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=backup-stream-token\n",
        )
        .await
        .expect("alias fixture should be written");

    let ctx = processing_context();
    let config =
        Config { storage_dir: temp.path().join("storage").to_string_lossy().into_owned(), ..Config::default() };
    ctx.config.config.store(Arc::new(config));
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "primary-account".intern(),
        input_type: InputType::M3u,
        url: primary_playlist_path.to_string_lossy().into_owned(),
        enabled: true,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "backup-account".intern(),
            url: alias_playlist_path.to_string_lossy().into_owned(),
            username: None,
            password: None,
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });

    let InputDownloadResult {
        errors, source: mut primary_playlist, storage_error, partial, quality_rejections, ..
    } = download_input(&ctx, &input, false).await;

    assert!(errors.is_empty(), "unexpected download errors: {errors:?}");
    assert!(storage_error.is_none(), "unexpected primary storage error: {storage_error:?}");
    assert!(!partial);
    assert!(quality_rejections.is_empty());
    assert!(!primary_playlist.is_empty());
    let alias_url = tuliprox_repository::load_input_m3u_stream_url(
        &ctx.config,
        &"backup-account".intern(),
        "http://stream.example:4000/323/mono.m3u8?token=primary-stream-token",
    )
    .await
    .expect("alias URL lookup should succeed");
    assert_eq!(alias_url.as_deref(), Some("http://stream.example:4000/323/mono.m3u8?token=backup-stream-token"));
}

#[tokio::test]
async fn failed_m3u_alias_is_retried_after_primary_input_is_processed() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let primary_playlist_path = temp.path().join("main.m3u");
    let alias_playlist_path = temp.path().join("retry.m3u");
    tokio::fs::write(
            &primary_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=main-stream-token\n",
        )
        .await
        .expect("primary fixture should be written");

    let ctx = processing_context();
    let config =
        Config { storage_dir: temp.path().join("storage").to_string_lossy().into_owned(), ..Config::default() };
    ctx.config.config.store(Arc::new(config));
    let input = Arc::new(ConfigInput {
        id: 1,
        name: "main-account".intern(),
        input_type: InputType::M3u,
        url: primary_playlist_path.to_string_lossy().into_owned(),
        enabled: true,
        aliases: Some(vec![ConfigInputAlias {
            id: 2,
            name: "retry-account".intern(),
            url: alias_playlist_path.to_string_lossy().into_owned(),
            username: None,
            password: None,
            priority: 1,
            max_connections: 1,
            exp_date: None,
            enabled: true,
            stalker: None,
        }]),
        ..ConfigInput::default()
    });

    let InputDownloadResult {
        errors: first_errors,
        source: mut first_playlist,
        storage_error: first_storage_error,
        partial: first_partial,
        quality_rejections: first_quality_rejections,
        ..
    } = download_input(&ctx, &input, false).await;

    assert!(!first_errors.is_empty(), "missing alias should report an error");
    assert!(first_storage_error.is_none(), "primary storage should succeed: {first_storage_error:?}");
    assert!(!first_partial);
    assert!(first_quality_rejections.is_empty());
    assert!(!first_playlist.is_empty());
    assert!(ctx.is_input_downloaded("main-account").await);
    assert!(!ctx.is_input_downloaded("retry-account").await);

    tokio::fs::write(
            &alias_playlist_path,
            "#EXTM3U\n#EXTINF:-1 tvg-id=\"323\",Channel\nhttp://stream.example:4000/323/mono.m3u8?token=retry-stream-token\n",
        )
        .await
        .expect("alias fixture should be written");

    let InputDownloadResult {
        errors: second_errors,
        source: mut second_playlist,
        storage_error: second_storage_error,
        partial: second_partial,
        quality_rejections: second_quality_rejections,
        ..
    } = download_input(&ctx, &input, false).await;

    assert!(second_errors.is_empty(), "unexpected retry errors: {second_errors:?}");
    assert!(second_storage_error.is_none(), "primary storage should remain readable: {second_storage_error:?}");
    assert!(!second_partial);
    assert!(second_quality_rejections.is_empty());
    assert!(!second_playlist.is_empty());
    assert!(ctx.is_input_downloaded("retry-account").await);
    let alias_url = tuliprox_repository::load_input_m3u_stream_url(
        &ctx.config,
        &"retry-account".intern(),
        "http://stream.example:4000/323/mono.m3u8?token=main-stream-token",
    )
    .await
    .expect("retried alias URL lookup should succeed");
    assert_eq!(alias_url.as_deref(), Some("http://stream.example:4000/323/mono.m3u8?token=retry-stream-token"));
}

#[test]
fn persist_filter_runs_after_after_epg_mapping() {
    let runtime = Runtime::new().expect("runtime");
    runtime.block_on(async {
        let mut input = ConfigInput::from(ConfigInputDto::default());
        input.name = "input".intern();
        let groups = vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    name: "Before".intern(),
                    group: "Live".intern(),
                    xtream_cluster: XtreamCluster::Live,
                    item_type: PlaylistItemType::Live,
                    ..Default::default()
                },
            }],
            xtream_cluster: XtreamCluster::Live,
        }];
        let mut playlist =
            FetchedPlaylist { input: &input, source: MemoryPlaylistSource::new(groups).into_source(), epg: None };
        let rename = build_mapping("rename", MappingStage::AfterEpg, r#"@Name = "After""#);
        let mut target = build_target(vec![rename], false);
        target.filter.persist = Some(get_filter(r#"Name = "After""#, None).expect("filter parses"));
        let mut stats =
            HashMap::from([(Arc::clone(&input.name), create_input_stat(1, 1, 0, input.input_type, &input.name, 0))]);
        let mut errors = Vec::new();

        let mut prepared = prepare_playlist_for_target(
            &processing_context(),
            std::slice::from_mut(&mut playlist),
            &target,
            &mut stats,
            &mut errors,
            ClusterFlags::empty(),
            false,
        )
        .await
        .expect("target preparation");

        assert!(errors.is_empty());
        apply_persist_filter(&target, &mut prepared.playlist);
        let item = &prepared.playlist[0].channels[0];
        assert_eq!(item.header.name.as_ref(), "After");
    });
}

fn make_channel(name: &str) -> PlaylistItem {
    let mut item = PlaylistItem {
        header: PlaylistItemHeader {
            name: name.intern(),
            group: "Originals".intern(),
            xtream_cluster: XtreamCluster::Live,
            item_type: PlaylistItemType::Live,
            ..Default::default()
        },
    };
    item.header.freeze_input_stream_id();
    item
}

fn memory_source(channels: Vec<PlaylistItem>) -> PlaylistSource {
    MemoryPlaylistSource::new(vec![PlaylistGroup {
        id: 1,
        title: "Live".intern(),
        channels,
        xtream_cluster: XtreamCluster::Live,
    }])
    .into_source()
}

fn channel_count(source: &mut PlaylistSource) -> usize { source.take_groups().iter().map(|g| g.channels.len()).sum() }

#[test]
fn map_playlist_applies_only_the_requested_stage() {
    let processing = build_mapping("processing", MappingStage::Processing, r#"@name = concat(@Name, "-P")"#);
    let after_epg = build_mapping("after_epg", MappingStage::AfterEpg, r#"@name = concat(@Name, "-E")"#);
    let target = build_target(vec![processing, after_epg], false);

    let mut source = memory_source(vec![make_channel("Alpha")]);
    let (groups, _) = execute_pipeline_on_groups(source.take_groups(), &target, &[TransformStage::Map]);
    assert_eq!(groups[0].channels[0].header.name.as_ref(), "Alpha-P");

    let mut source = MemoryPlaylistSource::new(groups).into_source();
    let groups = map_playlist_at_stage(&mut source, &target, MappingStage::AfterEpg, None)
        .expect("after_epg mapping should run");
    assert_eq!(groups[0].channels[0].header.name.as_ref(), "Alpha-P-E");
}

#[test]
fn map_playlist_at_stage_returns_none_without_consuming_source_when_no_match() {
    let target = build_target(Vec::new(), false);
    let mut source = memory_source(vec![make_channel("Alpha")]);

    let result = map_playlist_at_stage(&mut source, &target, MappingStage::AfterEpg, None);
    assert!(result.is_none(), "no matching stage must return None");
    assert_eq!(channel_count(&mut source), 1, "source must remain intact");
}

#[test]
fn prepare_target_applies_after_epg_mapping_before_sampling_stats() {
    let runtime = Runtime::new().expect("runtime");
    runtime.block_on(async {
                let dir = tempdir().expect("tempdir");
                let ics_path = dir.path().join("bbc.ics");
                std::fs::write(
                    &ics_path,
                    "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:News\nDTSTART:20260306T120000Z\nDTEND:20260306T130000Z\nEND:VEVENT\nEND:VCALENDAR",
                )
                .expect("write ics");

                let mut smart_dto = EpgSmartMatchConfigDto {
                    enabled: true,
                    fuzzy_matching: false,
                    ..EpgSmartMatchConfigDto::default()
                };
                smart_dto.prepare().expect("smart config");
                let mut input = ConfigInput::from(ConfigInputDto::default());
                input.name = "input".intern();
                input.epg = Some(EpgConfig {
                    sources: vec![],
                    smart_match: Some(EpgSmartMatchConfig::from(smart_dto)),
                });

                let channels = vec![live_item_for_epg("BBC One")];
                let groups = vec![PlaylistGroup {
                    id: 1,
                    title: "Live".intern(),
                    channels,
                    xtream_cluster: XtreamCluster::Live,
                }];
                let tv_guide = TVGuide::new(vec![PersistedEpgSource {
                    file_path: ics_path,
                    priority: 0,
                    logo_override: false,
                    kind: PersistedEpgSourceKind::Ics {
                        channel_id: "bbc.one".intern(),
                        channel_title: Some("BBC One".intern()),
                        match_names: vec!["BBC One".intern()],
                        config: Box::new(IcsEpgSourceConfig::default()),
                    },
                }]);

                let mut playlist = FetchedPlaylist {
                    input: &input,
                    source: MemoryPlaylistSource::new(groups).into_source(),
                    epg: Some(tv_guide),
                };

                let rename_from_epg = build_mapping(
                    "rename",
                    MappingStage::AfterEpg,
                    r#"epg = @epg_channel_id ~ "(.+)"
match {
  epg => @Name = epg.1
}"#,
                );
                let add_virtual = build_mapping("virtual", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
                let target = build_target(vec![rename_from_epg, add_virtual], false);
                let mut stats = HashMap::from([(
                    Arc::clone(&input.name),
                    create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
                )]);
                let mut errors = Vec::new();
                let prepared = prepare_playlist_for_target(
                    &processing_context(),
                    std::slice::from_mut(&mut playlist),
                    &target,
                    &mut stats,
                    &mut errors,
                    ClusterFlags::empty(),
                    false,
                )
                .await
                .expect("target preparation");

                assert!(errors.is_empty());
                assert_eq!(prepared.playlist.iter().map(|group| group.channels.len()).sum::<usize>(), 2);
                let channel = prepared
                    .playlist
                    .iter()
                    .flat_map(|group| &group.channels)
                    .find(|channel| channel.header.group.as_ref() != "Echo")
                    .expect("original channel");
                assert_eq!(channel.header.epg_channel_id.as_deref(), Some("bbc.one"));
                assert_eq!(
                    channel.header.name.as_ref(),
                    "bbc.one",
                    "after_epg mapper must consume the EPG-enriched field"
                );
                let processed_stats = &stats[&input.name].processed_stats;
                assert_eq!(processed_stats.group_count, 2);
                assert_eq!(processed_stats.channel_count, 2);
            });
}

#[test]
fn clear_invalid_epg_ids_clears_ids_invalidated_by_after_epg_mapping() {
    let runtime = Runtime::new().expect("runtime");
    runtime.block_on(async {
                let dir = tempdir().expect("tempdir");
                let ics_path = dir.path().join("bbc.ics");
                std::fs::write(
                    &ics_path,
                    "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:News\nDTSTART:20260306T120000Z\nDTEND:20260306T130000Z\nEND:VEVENT\nEND:VCALENDAR",
                )
                .expect("write ics");

                let mut input = ConfigInput::from(ConfigInputDto::default());
                input.name = "input".intern();
                input.epg = Some(EpgConfig { sources: vec![], smart_match: None });
                let groups = vec![PlaylistGroup {
                    id: 1,
                    title: "Live".intern(),
                    channels: vec![PlaylistItem {
                        header: PlaylistItemHeader {
                            name: "BBC One".intern(),
                            epg_channel_id: Some("bbc.one".intern()),
                            group: "Live".intern(),
                            xtream_cluster: XtreamCluster::Live,
                            item_type: PlaylistItemType::Live,
                            ..Default::default()
                        },
                    }],
                    xtream_cluster: XtreamCluster::Live,
                }];
                let tv_guide = TVGuide::new(vec![PersistedEpgSource {
                    file_path: ics_path,
                    priority: 0,
                    logo_override: false,
                    kind: PersistedEpgSourceKind::Ics {
                        channel_id: "bbc.one".intern(),
                        channel_title: Some("BBC One".intern()),
                        match_names: vec![],
                        config: Box::new(IcsEpgSourceConfig::default()),
                    },
                }]);
                let mut playlist = FetchedPlaylist {
                    input: &input,
                    source: MemoryPlaylistSource::new(groups).into_source(),
                    epg: Some(tv_guide),
                };

                let rewrite_epg =
                    build_mapping("rewrite", MappingStage::AfterEpg, r#"@epg_channel_id = "missing.epg""#);
                let add_virtual = build_mapping("virtual", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
                let mut target = build_target(vec![rewrite_epg, add_virtual], false);
                target.options = Some(ConfigTargetOptions { clear_invalid_epg_ids: true, ..Default::default() });
                let mut stats = HashMap::from([(
                    Arc::clone(&input.name),
                    create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
                )]);
                let mut errors = Vec::new();

                let prepared = prepare_playlist_for_target(
                    &processing_context(),
                    std::slice::from_mut(&mut playlist),
                    &target,
                    &mut stats,
                    &mut errors,
                    ClusterFlags::empty(),
                    false,
                )
                .await
                .expect("target preparation");

                assert!(errors.is_empty());
                assert!(!prepared.playlist.is_empty());
                assert!(prepared
                    .playlist
                    .iter()
                    .flat_map(|group| &group.channels)
                    .all(|channel| channel.header.epg_channel_id.is_none()));
                assert_eq!(stats[&input.name].processed_stats.channel_count, 2);
            });
}

fn live_item_for_epg(name: &str) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            name: name.intern(),
            group: "Live".intern(),
            xtream_cluster: XtreamCluster::Live,
            item_type: PlaylistItemType::Live,
            ..Default::default()
        },
    }
}

#[test]
fn after_epg_hook_runs_on_source_already_deduplicated_by_processing_pipe() {
    let processing = build_mapping("processing", MappingStage::Processing, r#"@group = "PROCESSED""#);
    let after_epg = build_mapping("after_epg", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
    let target = build_target(vec![processing, after_epg], true);

    let input = ConfigInput::default();
    let channel = make_channel("Alpha");
    let mut fetched =
        FetchedPlaylist { input: &input, source: memory_source(vec![channel.clone(), channel]), epg: None };
    let mut duplicates = HashSet::new();
    let (mut processed, _outcome) =
        execute_pipe(&target, &get_processing_pipe(&target), &mut fetched, &mut duplicates, false, None)
            .expect("processing pipe must run");
    assert_eq!(processed.get_channel_count(), 1, "processing pipe must remove the duplicate");

    let groups = map_playlist_at_stage(&mut processed.source, &target, MappingStage::AfterEpg, None)
        .expect("after_epg hook must run");

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].title.as_ref(), "PROCESSED");
    assert_eq!(groups[0].channels.len(), 1);
    assert_eq!(groups[1].title.as_ref(), "Echo");
    assert_eq!(groups[1].channels.len(), 1);
}

#[test]
fn prepare_target_deduplicates_virtual_items_created_by_after_epg_mappings() {
    let runtime = Runtime::new().expect("runtime");
    runtime.block_on(async {
        let first = build_mapping("first", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
        let second = build_mapping("second", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
        let target = build_target(vec![first, second], true);
        let input = ConfigInput { name: "input".intern(), ..Default::default() };
        let mut playlist =
            FetchedPlaylist { input: &input, source: memory_source(vec![make_channel("Alpha")]), epg: None };
        let mut stats =
            HashMap::from([(Arc::clone(&input.name), create_input_stat(1, 1, 0, input.input_type, &input.name, 0))]);
        let mut errors = Vec::new();

        let prepared = prepare_playlist_for_target(
            &processing_context(),
            std::slice::from_mut(&mut playlist),
            &target,
            &mut stats,
            &mut errors,
            ClusterFlags::empty(),
            false,
        )
        .await
        .expect("target preparation");

        assert!(errors.is_empty());
        assert_eq!(prepared.playlist.iter().map(|group| group.channels.len()).sum::<usize>(), 3);
        assert_eq!(stats[&input.name].processed_stats.channel_count, 3);
    });
}
