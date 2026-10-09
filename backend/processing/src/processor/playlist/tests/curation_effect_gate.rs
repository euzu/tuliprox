use super::*;
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::model::{ConfigPaths, NoopSink};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tempfile::tempdir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use tuliprox_core::{
    model::{ApiProxyConfig, CustomStreamResponse, HdHomeRunConfig, MediaToolCapabilities, SourcesConfig},
    utils::FileLockManager,
};

pub(super) fn app_config(storage_dir: &Path) -> Arc<AppConfig> {
    Arc::new(AppConfig {
        config: Arc::new(ArcSwap::from_pointee(Config {
            storage_dir: storage_dir.to_string_lossy().into_owned(),
            ..Config::default()
        })),
        sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
        hdhomerun: Arc::new(ArcSwapOption::<HdHomeRunConfig>::default()),
        api_proxy: Arc::new(ArcSwapOption::<ApiProxyConfig>::default()),
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
        custom_stream_response: Arc::new(ArcSwapOption::<CustomStreamResponse>::default()),
        access_token_secret: [0; 32],
        encrypt_secret: [0; 16],
        media_tools: Arc::new(MediaToolCapabilities::new()),
    })
}

pub(super) fn processing_context(
    app_config: Arc<AppConfig>,
    playlist_state: Option<Arc<PlaylistStorageState>>,
) -> PlaylistProcessingContext<NoopSink> {
    PlaylistProcessingContext {
        client: reqwest::Client::new(),
        run_id: "curation-effect-gate-run".into(),
        execution_order: PlaylistUpdateRunOrder::from(1),
        config: app_config,
        user_targets: Arc::new(ProcessTargets {
            enabled: false,
            inputs: Vec::new(),
            targets: Vec::new(),
            target_names: Vec::new(),
        }),
        events: NoopSink,
        playlist_state,
        disabled_headers: None,
        processed_inputs: Arc::new(Mutex::new(HashSet::new())),
        input_completions: Arc::new(Mutex::new(HashMap::new())),
        input_locks: Arc::new(Mutex::new(HashMap::new())),
        provider_manager: None,
        metadata_manager: None,
        pre_processed_inputs: None,
        stalker_refresh_mode: StalkerRefreshMode::Complete,
        partial_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        had_quality_rejections: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        input_refresh: None,
        library_update_mode: LibraryUpdateMode::ExistingCatalog,
    }
}

fn curation_persist_options(publication_plan: PlaylistPublicationPlan) -> TargetPlaylistPersistOptions {
    TargetPlaylistPersistOptions { publication_plan, ..TargetPlaylistPersistOptions::default() }
}

pub(super) fn file_snapshot(root: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn collect(root: &Path, path: &Path, snapshot: &mut BTreeMap<std::path::PathBuf, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                collect(root, &entry_path, snapshot);
            } else {
                snapshot.insert(
                    entry_path.strip_prefix(root).expect("snapshot path under root").to_path_buf(),
                    std::fs::read(&entry_path).expect("snapshot file"),
                );
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    collect(root, root, &mut snapshot);
    snapshot
}

async fn empty_trakt_server() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind Trakt test server");
    let address = listener.local_addr().expect("Trakt test address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept Trakt request");
        let mut request = Vec::new();
        loop {
            let mut buffer = [0u8; 1024];
            let read = stream.read(&mut buffer).await.expect("read Trakt request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n[]")
            .await
            .expect("write Trakt response");
    });
    (format!("http://{address}"), server)
}

#[tokio::test]
async fn unavailable_curation_preserves_the_empty_mixed_output_guard() {
    assert_curation_preserves_empty_mixed_output_guard(None).await;
}

#[tokio::test]
async fn tmdb_and_mixed_failures_preserve_the_empty_mixed_output_guard() {
    for policy in ["full", "curated"] {
        for mixed in [false, true] {
            let (url, server) = empty_trakt_server().await;
            let mut value = serde_json::json!({"catalog_selection": policy, "tmdb": {"trending": [{"kind": "movie", "time_window": "week", "limit": 100, "category_name": "TMDB"}]}});
            if mixed {
                value["trakt"] = serde_json::json!({"api": {"api_key": "test-client", "url": url}, "charts": [{"kind": "movies", "chart": "popular", "category_name": "Trakt"}]});
            }
            let mut dto: shared::model::CurationConfigDto = serde_json::from_value(value).unwrap();
            dto.prepare(&[TargetOutputDto::Xtream(XtreamTargetOutputDto::default())]).unwrap();
            assert_curation_preserves_empty_mixed_output_guard(Some(dto)).await;
            if mixed {
                server.await.unwrap();
            } else {
                server.abort();
            }
        }
    }
}

async fn assert_curation_preserves_empty_mixed_output_guard(canonical: Option<shared::model::CurationConfigDto>) {
    let directory = tempdir().expect("tempdir");
    let app_config = app_config(directory.path());
    let playlist_state = Arc::new(PlaylistStorageState::new());
    let mut target = ConfigTarget::from(&ConfigTargetDto {
        name: "curation-effect-gate".to_string(),
        output: vec![
            TargetOutputDto::Xtream(XtreamTargetOutputDto {
                trakt: Some(TraktConfigDto {
                    lists: vec![TraktListConfigDto {
                        user: "alice".to_string(),
                        list_slug: "watchlist".to_string(),
                        category_name: Some("Watchlist".to_string()),
                        create_xtream_category: true,
                        content_type: TraktContentType::Vod,
                        tmdb_only: true,
                        fuzzy_match_threshold: 100,
                    }],
                    ..TraktConfigDto::default()
                }),
                ..XtreamTargetOutputDto::default()
            }),
            TargetOutputDto::M3u(M3uTargetOutputDto {
                filename: Some("curation-effect-gate.m3u".to_string()),
                ..M3uTargetOutputDto::default()
            }),
        ],
        watch: Some(vec![".*".to_string()]),
        use_memory_cache: true,
        ..ConfigTargetDto::default()
    });
    if let Some(canonical) = canonical {
        target.curation = Some(CurationConfig::from(&canonical));
        if let tuliprox_core::model::TargetOutput::Xtream(output) = &mut target.output[0] {
            output.trakt = None;
        }
    }
    let mut seeded = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        channels: vec![catalog_test_item(
            "Seeded movie",
            UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000071"),
            PlaylistItemType::Video,
            XtreamCluster::Video,
            None,
        )],
        xtream_cluster: XtreamCluster::Video,
    }];
    let seeded_result = persist_playlist_views(
        &app_config,
        &mut seeded,
        None,
        None,
        &target,
        Some(&playlist_state),
        curation_persist_options(PlaylistPublicationPlan::Ordinary),
    )
    .await;
    assert!(seeded_result.is_ok(), "seed persist failed: {seeded_result:?}");
    assert!(process_watch(&app_config, &NoopSink, &target, &seeded).await);
    let before_files = file_snapshot(directory.path());
    let before_cache_len = playlist_state
        .data
        .read()
        .await
        .get(&target.name)
        .and_then(|storage| storage.xtream.as_ref())
        .map_or(0, |storage| storage.vod.len());

    let context = processing_context(Arc::clone(&app_config), Some(Arc::clone(&playlist_state)));
    let prepared = PreparedTarget {
        target,
        playlist: Vec::new(),
        epg: Vec::new(),
        processing: PipelineStats::default(),
        accepted_empty_clusters: ClusterFlags::Vod,
        library_empty: tuliprox_repository::LibraryEmptyPublication::None,
    };

    let (result, errors) = finalize_prepared_target(Arc::new(context), prepared).await;

    assert!(result.is_err_and(|errors| errors.iter().any(|error| {
        error.message().contains("non-Xtream outputs that cannot safely publish a fully empty forced result")
    })));
    assert!(errors.is_empty());
    assert_eq!(file_snapshot(directory.path()), before_files);
    let after_cache_len = playlist_state
        .data
        .read()
        .await
        .get("curation-effect-gate")
        .and_then(|storage| storage.xtream.as_ref())
        .map_or(0, |storage| storage.vod.len());
    assert!(before_cache_len > 0);
    assert_eq!(after_cache_len, before_cache_len);
}

#[tokio::test]
async fn complete_empty_curation_publishes_empty_watch_group_state() {
    let directory = tempdir().expect("tempdir");
    let app_config = app_config(directory.path());
    let (base_url, server) = empty_trakt_server().await;
    let target_name = "curation-empty-watch";
    let target = ConfigTarget::from(&ConfigTargetDto {
        name: target_name.to_string(),
        output: vec![TargetOutputDto::Xtream(XtreamTargetOutputDto {
            trakt: Some(TraktConfigDto {
                catalog_selection: TraktCatalogSelection::Curated,
                api: TraktApiConfigDto {
                    api_key: "test-client-id".to_string(),
                    version: "2".to_string(),
                    url: base_url,
                    user_agent: "tuliprox-test".to_string(),
                },
                lists: vec![TraktListConfigDto {
                    user: "alice".to_string(),
                    list_slug: "watchlist".to_string(),
                    category_name: Some("Watchlist".to_string()),
                    create_xtream_category: true,
                    content_type: TraktContentType::Vod,
                    tmdb_only: true,
                    fuzzy_match_threshold: 100,
                }],
                ..TraktConfigDto::default()
            }),
            ..XtreamTargetOutputDto::default()
        })],
        watch: Some(vec![".*".to_string()]),
        ..ConfigTargetDto::default()
    });
    let mut seeded = vec![PlaylistGroup {
        id: 1,
        title: "Movies".intern(),
        channels: vec![catalog_test_item(
            "Seeded movie",
            UUIDType::from_valid_uuid("00000000-0000-4000-8000-000000000072"),
            PlaylistItemType::Video,
            XtreamCluster::Video,
            None,
        )],
        xtream_cluster: XtreamCluster::Video,
    }];
    let seed_result = persist_playlist_views(
        &app_config,
        &mut seeded,
        None,
        None,
        &target,
        None,
        curation_persist_options(PlaylistPublicationPlan::Ordinary),
    )
    .await;
    assert!(seed_result.is_ok(), "seed persist failed: {seed_result:?}");
    assert!(process_watch(&app_config, &NoopSink, &target, &seeded).await);
    let watch_index = directory.path().join(format!("{target_name}.groups.bin"));
    let before: BTreeSet<Arc<str>> =
        tuliprox_core::utils::binary_deserialize(&std::fs::read(&watch_index).expect("seeded watch index"))
            .expect("decode seeded watch index");
    assert_eq!(before.len(), 1);

    let context = processing_context(Arc::clone(&app_config), None);
    let prepared = PreparedTarget {
        target,
        playlist: seeded,
        epg: Vec::new(),
        processing: PipelineStats::default(),
        accepted_empty_clusters: ClusterFlags::empty(),
        library_empty: tuliprox_repository::LibraryEmptyPublication::None,
    };
    let (result, errors) = finalize_prepared_target(Arc::new(context), prepared).await;
    server.await.expect("Trakt server should finish");

    assert!(result.is_ok(), "complete empty finalization failed: {result:?}");
    assert!(errors.is_empty());
    let after: BTreeSet<Arc<str>> =
        tuliprox_core::utils::binary_deserialize(&std::fs::read(watch_index).expect("empty watch index"))
            .expect("decode empty watch index");
    assert!(after.is_empty());
}
