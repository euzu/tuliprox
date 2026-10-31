use super::*;
use crate::{
    open_playlist_reader, write_playlist_batch_item_upsert, write_playlist_item_update, PlaylistIteratorReader,
    XtreamPlaylistIterator,
};
use futures::StreamExt;
use shared::model::SeriesListProjection;
use tuliprox_core::model::ProxyUserCredentials;

#[tokio::test]
async fn force_empty_output_filters_reject_m3u_and_strm_before_mutation() {
    let filter = get_filter(r#"EpgId ~ "^Mixed\.Case$""#, None).expect("case-sensitive EPG-ID filter");
    let cases = [
        (
            "filtered-m3u",
            TargetOutput::M3u(M3uTargetOutput {
                filename: Some("client.m3u".to_string()),
                include_type_in_url: false,
                mask_redirect_url: false,
                filter: Some(filter.clone()),
            }),
            "client.m3u",
        ),
        (
            "filtered-strm",
            TargetOutput::Strm(StrmTargetOutput {
                directory: "client-strm".to_string(),
                username: None,
                style: StrmExportStyle::Jellyfin,
                flags: StrmTargetFlagsSet::new(),
                strm_props: None,
                filter: Some(filter),
                probe_probe_size_bytes: None,
                probe_analyze_duration: None,
            }),
            "client-strm/old.strm",
        ),
    ];

    for (target_name, output, artifact) in cases {
        let directory = tempdir().expect("temporary storage");
        let app_config = app_config_in(directory.path());
        let artifact_path = directory.path().join(artifact);
        tokio::fs::create_dir_all(artifact_path.parent().expect("artifact parent")).await.expect("artifact directory");
        tokio::fs::write(&artifact_path, b"previous-client-state").await.expect("previous client artifact");
        let mut target = target_with_options(target_name, vec![output], false);
        target.options = Some(epg_normalization_options(true));
        let target_path = {
            let config = app_config.config.load();
            crate::get_target_storage_path(&config, &target.name).expect("target storage path")
        };
        tokio::fs::create_dir_all(&target_path).await.expect("target storage directory");
        let mapping_path = crate::get_target_id_mapping_file(&target_path);
        let mapping_uuid_path = mapping_path.with_extension("uuid.db");
        let seed_group = target_group(XtreamCluster::Live, 100, "mapping-seed");
        {
            let seed_header = &seed_group.channels[0].header;
            let mut mapping = TargetIdMapping::new(&mapping_path, false).expect("seed target ID mapping");
            mapping.get_and_update_virtual_id(seed_header.get_uuid(), 0, seed_header.item_type, VirtualId::default());
            mapping.persist().expect("persist target ID mapping seed");
        }
        let mapping_before = [
            tokio::fs::read(&mapping_path).await.expect("target ID mapping"),
            tokio::fs::read(&mapping_uuid_path).await.expect("target UUID mapping"),
        ];
        let mut candidate = vec![target_group(XtreamCluster::Live, 101, "filtered-after-normalization")];
        candidate[0].channels[0].header.epg_channel_id = Some("Mixed.Case".intern());

        let errors = persist_playlist_with_mode(
            &app_config,
            &mut candidate,
            None,
            &target,
            None,
            TargetPlaylistPersistOptions {
                accepted_empty_clusters: ClusterFlags::Vod,
                ..TargetPlaylistPersistOptions::default()
            },
            TargetPersistenceMode::Persist,
        )
        .await
        .expect_err("force-empty filtered output must be rejected");

        assert!(errors.iter().any(|error| error.to_string().contains("after its output filter")));
        assert_eq!(candidate[0].channels[0].header.epg_channel_id.as_deref(), Some("mixed.case"));
        assert_eq!(tokio::fs::read(&artifact_path).await.expect("retained client artifact"), b"previous-client-state");
        assert_eq!(tokio::fs::read(&mapping_path).await.expect("retained target ID mapping"), mapping_before[0]);
        assert_eq!(tokio::fs::read(&mapping_uuid_path).await.expect("retained target UUID mapping"), mapping_before[1]);
    }
}

#[test]
fn playlist_without_channels_is_empty_for_persistence() {
    assert!(!playlist_has_items(&[]));
    assert!(!playlist_has_items(&[PlaylistGroup {
        id: 1,
        title: "empty".intern(),
        channels: Vec::new(),
        xtream_cluster: XtreamCluster::Live,
    }]));
}

#[test]
fn media_server_playlist_file_path_uses_separate_prefix() {
    let dir = tempdir().expect("tempdir");
    let path = get_input_media_server_playlist_file_path(dir.path(), &"Media Server Input".intern());

    assert!(path.ends_with("media_server_Media_Server_Input.db"));
}

#[tokio::test]
async fn cache_load_drops_series_details_while_disk_keeps_them() {
    let dir = tempdir().expect("tempdir");
    let app_config = app_config_in(dir.path());
    let target = target_with_options(
        "series-cache-load-target",
        vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        true,
    );
    let storage_path = {
        let config = app_config.config.load();
        xtream_get_storage_path(&config, &target.name).expect("storage path")
    };
    let series_path = crate::xtream_get_file_path(&storage_path, XtreamCluster::Series);
    tokio::fs::create_dir_all(series_path.parent().expect("parent dir")).await.expect("create dir");

    let details = SeriesStreamDetailProperties { year: Some(2024), seasons: None, episodes: None };
    let mut item = crate::test_config::xtream_series_item(55, Some(details));
    item.name = "Test Series".intern();
    item.title = "Test Series".intern();
    let mut tree = crate::BPlusTree::new();
    tree.insert(55u32, item.clone());
    tree.store_with_index(&series_path, |pli| pli.source_ordinal).expect("store series tree");

    // Load storage into memory cache
    let storage = load_xtream_target_storage(&app_config, &target).await.expect("load xtream storage");

    // Cache item has details == None
    let cached_item = storage.series.query(&55).expect("item in cache");
    assert!(!cached_item.has_details(), "cache item must have details == None");
    match &cached_item.additional_properties {
        Some(StreamProperties::Series(props)) => assert_eq!(props.details, None),
        other => panic!("expected Series properties, got {other:?}"),
    }

    // Disk item still has details
    let mut disk_query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&series_path).expect("open disk query");
    let disk_item = disk_query.query(&55).expect("query succeeds").expect("item found on disk");
    assert!(disk_item.has_details(), "disk item must keep details");
    match &disk_item.additional_properties {
        Some(StreamProperties::Series(props)) => assert!(props.details.is_some()),
        other => panic!("expected Series properties, got {other:?}"),
    }
}

#[tokio::test]
async fn xtream_get_item_for_stream_id_with_memory_cache_returns_details_for_series_info() {
    let dir = tempdir().expect("tempdir");
    let app_config = Arc::new(app_config_in(dir.path()));
    let target = target_with_options(
        "series-lookup-target",
        vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        true,
    );
    let (target_path, storage_path) = {
        let config = app_config.config.load();
        (
            get_target_storage_path(&config, &target.name).expect("target path"),
            xtream_get_storage_path(&config, &target.name).expect("storage path"),
        )
    };
    let series_path = crate::xtream_get_file_path(&storage_path, XtreamCluster::Series);
    let mapping_path = crate::get_target_id_mapping_file(&target_path);
    tokio::fs::create_dir_all(series_path.parent().expect("parent dir")).await.expect("create dir");

    // 1. Write series item with details to disk
    let details = SeriesStreamDetailProperties { year: Some(2024), seasons: None, episodes: None };
    let mut item = crate::test_config::xtream_series_item(99, Some(details));
    item.name = "Detailed Series".intern();
    item.title = "Detailed Series".intern();
    let mut tree = crate::BPlusTree::new();
    tree.insert(99u32, item.clone());
    tree.store_with_index(&series_path, |pli| pli.source_ordinal).expect("store series tree");

    // 2. Write id mapping to disk and populate memory cache
    let mut mapping_tree = crate::BPlusTree::new();
    let record = VirtualIdRecord {
        virtual_id: VirtualId::new(99),
        provider_id: 99,
        uuid: UUIDType::default(),
        item_type: PlaylistItemType::SeriesInfo,
        parent_virtual_id: VirtualId::default(),
        last_updated: 0,
    };
    mapping_tree.insert(VirtualId::new(99), record);
    mapping_tree.store(&mapping_path).expect("store id mapping");

    let playlists = Arc::new(PlaylistStorageState::new());
    // Load storage into memory cache (so memory cache has series item WITHOUT details)
    let storage = load_xtream_target_storage(&app_config, &target).await.expect("load xtream storage");
    assert!(!storage.series.query(&99).unwrap().has_details());

    playlists.cache_playlist(&target.name, crate::PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;
    playlists.cache_id_mapping(&target.name, mapping_tree).await;

    // 3. Look up item: with use_memory_cache: true, series cluster falls through to disk and returns full details
    let resolved_no_cluster = crate::xtream_get_item_for_stream_id(99, &app_config, &playlists, &target, None)
        .await
        .expect("xtream_get_item_for_stream_id succeeds without cluster");
    assert!(resolved_no_cluster.has_details(), "series item looked up without cluster must have details");

    let resolved_with_cluster =
        crate::xtream_get_item_for_stream_id(99, &app_config, &playlists, &target, Some(XtreamCluster::Series))
            .await
            .expect("xtream_get_item_for_stream_id succeeds with cluster");
    assert!(resolved_with_cluster.has_details(), "series item looked up with cluster must have details");
}

#[tokio::test]
async fn xtream_get_item_for_stream_id_resolves_series_info_when_id_mapping_only_in_memory() {
    let dir = tempdir().expect("tempdir");
    let app_config = Arc::new(app_config_in(dir.path()));
    let target = target_with_options(
        "series-lookup-mem-mapping-target",
        vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        true,
    );
    let (target_path, storage_path) = {
        let config = app_config.config.load();
        (
            get_target_storage_path(&config, &target.name).expect("target path"),
            xtream_get_storage_path(&config, &target.name).expect("storage path"),
        )
    };
    let series_path = crate::xtream_get_file_path(&storage_path, XtreamCluster::Series);
    let mapping_path = crate::get_target_id_mapping_file(&target_path);
    tokio::fs::create_dir_all(series_path.parent().expect("parent dir")).await.expect("create dir");

    // 1. Write series item with details to disk
    let details = SeriesStreamDetailProperties { year: Some(2024), seasons: None, episodes: None };
    let mut item = crate::test_config::xtream_series_item(99, Some(details));
    item.name = "Detailed Series".intern();
    item.title = "Detailed Series".intern();
    let mut tree = crate::BPlusTree::new();
    tree.insert(99u32, item.clone());
    tree.store_with_index(&series_path, |pli| pli.source_ordinal).expect("store series tree");

    // 2. Populate memory cache with storage and in-memory id mapping, but DO NOT store id mapping to disk
    let mut mapping_tree = crate::BPlusTree::new();
    let record = VirtualIdRecord {
        virtual_id: VirtualId::new(99),
        provider_id: 99,
        uuid: UUIDType::default(),
        item_type: PlaylistItemType::SeriesInfo,
        parent_virtual_id: VirtualId::default(),
        last_updated: 0,
    };
    mapping_tree.insert(VirtualId::new(99), record);

    let playlists = Arc::new(PlaylistStorageState::new());
    let storage = load_xtream_target_storage(&app_config, &target).await.expect("load xtream storage");
    assert!(!storage.series.query(&99).unwrap().has_details());

    playlists.cache_playlist(&target.name, crate::PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;
    playlists.cache_id_mapping(&target.name, mapping_tree).await;

    // Verify on-disk id mapping file is absent
    assert!(!mapping_path.exists(), "on-disk id mapping file must not exist");

    // 3. Series info lookup without cluster must resolve via in-memory mapping and return item from disk
    let resolved = crate::xtream_get_item_for_stream_id(99, &app_config, &playlists, &target, None)
        .await
        .expect("series info lookup succeeds when id mapping exists only in memory");
    assert_eq!(resolved.virtual_id.get(), 99);
    assert!(resolved.has_details(), "series item looked up must have details");
}

#[tokio::test]
async fn xtream_get_item_for_stream_id_disk_resolves_catchup_with_live_cluster() {
    let dir = tempdir().expect("tempdir");
    let app_config = Arc::new(app_config_in(dir.path()));
    let target = target_with_options(
        "catchup-disk-target",
        vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        false,
    );
    let (target_path, storage_path) = {
        let config = app_config.config.load();
        (
            get_target_storage_path(&config, &target.name).expect("target path"),
            xtream_get_storage_path(&config, &target.name).expect("storage path"),
        )
    };
    let live_path = crate::xtream_get_file_path(&storage_path, XtreamCluster::Live);
    let mapping_path = crate::get_target_id_mapping_file(&target_path);
    tokio::fs::create_dir_all(live_path.parent().expect("parent dir")).await.expect("create dir");

    let parent_virtual_id = 100u32;
    let catchup_virtual_id = 200u32;
    let mapping_provider_id = 555u32;

    let mut parent_item = crate::test_config::xtream_playlist_item(parent_virtual_id);
    parent_item.provider_id = 111;
    parent_item.item_type = PlaylistItemType::Live;
    let mut tree = crate::BPlusTree::new();
    tree.insert(parent_virtual_id, parent_item);
    tree.store(&live_path).expect("store live tree");

    let mut mapping_tree = crate::BPlusTree::new();
    let record = VirtualIdRecord {
        virtual_id: VirtualId::new(catchup_virtual_id),
        provider_id: mapping_provider_id,
        uuid: UUIDType::default(),
        item_type: PlaylistItemType::Catchup,
        parent_virtual_id: VirtualId::new(parent_virtual_id),
        last_updated: 0,
    };
    mapping_tree.insert(VirtualId::new(catchup_virtual_id), record);
    mapping_tree.store(&mapping_path).expect("store id mapping");

    let playlists = Arc::new(PlaylistStorageState::new());

    let item = crate::xtream_get_item_for_stream_id(
        catchup_virtual_id,
        &app_config,
        &playlists,
        &target,
        Some(XtreamCluster::Live),
    )
    .await
    .expect("catchup item lookup succeeds on disk path");

    assert_eq!(item.item_type, PlaylistItemType::Catchup);
    assert_eq!(item.provider_id, mapping_provider_id);
    assert_eq!(item.virtual_id, VirtualId::new(catchup_virtual_id));
}

#[tokio::test]
async fn xtream_get_item_for_stream_id_disk_resolves_local_series_episode_via_parent() {
    let dir = tempdir().expect("tempdir");
    let app_config = Arc::new(app_config_in(dir.path()));
    let target = target_with_options(
        "local-series-disk-target",
        vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        false,
    );
    let (target_path, storage_path) = {
        let config = app_config.config.load();
        (
            get_target_storage_path(&config, &target.name).expect("target path"),
            xtream_get_storage_path(&config, &target.name).expect("storage path"),
        )
    };
    let series_path = crate::xtream_get_file_path(&storage_path, XtreamCluster::Series);
    let mapping_path = crate::get_target_id_mapping_file(&target_path);
    tokio::fs::create_dir_all(series_path.parent().expect("parent dir")).await.expect("create dir");

    let parent_virtual_id = 100u32;
    let episode_virtual_id = 200u32;
    let mapping_provider_id = 555u32;

    let mut parent_item = crate::test_config::xtream_playlist_item(parent_virtual_id);
    parent_item.provider_id = 111;
    parent_item.item_type = PlaylistItemType::LocalSeriesInfo;
    let mut tree = crate::BPlusTree::new();
    tree.insert(parent_virtual_id, parent_item);
    tree.store(&series_path).expect("store series tree");

    let mut mapping_tree = crate::BPlusTree::new();
    let record = VirtualIdRecord {
        virtual_id: VirtualId::new(episode_virtual_id),
        provider_id: mapping_provider_id,
        uuid: UUIDType::default(),
        item_type: PlaylistItemType::LocalSeries,
        parent_virtual_id: VirtualId::new(parent_virtual_id),
        last_updated: 0,
    };
    mapping_tree.insert(VirtualId::new(episode_virtual_id), record);
    mapping_tree.store(&mapping_path).expect("store id mapping");

    let playlists = Arc::new(PlaylistStorageState::new());

    let item = crate::xtream_get_item_for_stream_id(
        episode_virtual_id,
        &app_config,
        &playlists,
        &target,
        Some(XtreamCluster::Series),
    )
    .await
    .expect("local series episode item lookup succeeds on disk path");

    assert_eq!(item.item_type, PlaylistItemType::LocalSeries);
    assert_eq!(item.provider_id, mapping_provider_id);
    assert_eq!(item.virtual_id, VirtualId::new(episode_virtual_id));
}

fn seed_test_cluster(storage_path: &Path, cluster: XtreamCluster) {
    let path = crate::xtream_get_file_path(storage_path, cluster);
    let mut tree = crate::BPlusTree::new();
    for (id, ordinal) in [(10, 300), (20, 100), (30, 200)] {
        let mut item = match cluster {
            XtreamCluster::Live => crate::test_config::xtream_playlist_item(id),
            XtreamCluster::Video => {
                let mut it = crate::test_config::xtream_playlist_item(id);
                it.xtream_cluster = XtreamCluster::Video;
                it.item_type = PlaylistItemType::Video;
                it
            }
            XtreamCluster::Series => crate::test_config::xtream_series_item(id, None),
        };
        item.source_ordinal = ordinal;
        tree.insert(id, item);
    }
    tree.store_with_index(&path, |pli| pli.source_ordinal).expect("store with index");
}

async fn apply_test_upserts(
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlists: &PlaylistStorageState,
    cluster: XtreamCluster,
) {
    let mut repl = match cluster {
        XtreamCluster::Live => crate::test_config::xtream_playlist_item(20),
        XtreamCluster::Video => {
            let mut it = crate::test_config::xtream_playlist_item(20);
            it.xtream_cluster = XtreamCluster::Video;
            it.item_type = PlaylistItemType::Video;
            it
        }
        XtreamCluster::Series => crate::test_config::xtream_series_item(20, None),
    };
    repl.source_ordinal = 100;
    repl.title = "Updated Title".intern();

    write_playlist_item_update(app_config, &target.name, &repl).await.expect("disk replacement");
    playlists.update_playlist_items(target, &[&repl]).await;

    let mut new1 = match cluster {
        XtreamCluster::Live => crate::test_config::xtream_playlist_item(40),
        XtreamCluster::Video => {
            let mut it = crate::test_config::xtream_playlist_item(40);
            it.xtream_cluster = XtreamCluster::Video;
            it.item_type = PlaylistItemType::Video;
            it
        }
        XtreamCluster::Series => crate::test_config::xtream_series_item(40, None),
    };
    new1.source_ordinal = 50;

    let mut new2 = match cluster {
        XtreamCluster::Live => crate::test_config::xtream_playlist_item(5),
        XtreamCluster::Video => {
            let mut it = crate::test_config::xtream_playlist_item(5);
            it.xtream_cluster = XtreamCluster::Video;
            it.item_type = PlaylistItemType::Video;
            it
        }
        XtreamCluster::Series => crate::test_config::xtream_series_item(5, None),
    };
    new2.source_ordinal = 250;

    write_playlist_batch_item_upsert(app_config, &target.name, cluster, &[new1.clone(), new2.clone()])
        .await
        .expect("disk batch upsert");
    playlists.update_playlist_items(target, &[&new1, &new2]).await;

    if cluster == XtreamCluster::Series {
        let mut episode = crate::test_config::xtream_playlist_item(500);
        episode.xtream_cluster = XtreamCluster::Series;
        episode.item_type = PlaylistItemType::Series;
        episode.source_ordinal = 10;
        write_playlist_batch_item_upsert(app_config, &target.name, cluster, &[episode.clone()])
            .await
            .expect("disk episode upsert");
        playlists.update_playlist_items(target, &[&episode]).await;
    }
}

async fn assert_cluster_iterators_match(
    app_config: &Arc<AppConfig>,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    target_disk: &ConfigTarget,
    storage_path: &Path,
    cluster: XtreamCluster,
    expected_order: &[u32],
) {
    let path = crate::xtream_get_file_path(storage_path, cluster);
    let index_path = crate::get_file_path_for_db_index(&path);
    let user = ProxyUserCredentials::default();

    // Verify the disk path uses the sorted index (not the unsorted fallback)
    if cluster == XtreamCluster::Series {
        let reader = open_playlist_reader::<u32, SeriesListProjection, u32>(&path, &index_path, None)
            .expect("open series reader");
        assert!(matches!(reader, PlaylistIteratorReader::Sorted { .. }), "disk path fell back to unsorted for Series");
    } else {
        let reader = open_playlist_reader::<u32, XtreamPlaylistItem, u32>(&path, &index_path, None)
            .expect("open cluster reader");
        assert!(
            matches!(reader, PlaylistIteratorReader::Sorted { .. }),
            "disk path fell back to unsorted for {cluster}"
        );
    }

    // Memory iterator
    let mem_iter = XtreamPlaylistIterator::new(cluster, app_config, playlists, target, None, &user)
        .await
        .expect("memory iterator opens");
    let mem_items: Vec<XtreamPlaylistItem> =
        mem_iter.collect::<Vec<_>>().await.into_iter().map(|res| res.expect("item").0).collect();

    // Disk iterator
    let disk_iter = XtreamPlaylistIterator::new(cluster, app_config, playlists, target_disk, None, &user)
        .await
        .expect("disk iterator opens");
    let disk_items: Vec<XtreamPlaylistItem> =
        disk_iter.collect::<Vec<_>>().await.into_iter().map(|res| res.expect("item").0).collect();

    let mem_ids: Vec<u32> = mem_items.iter().map(|item| item.virtual_id.get()).collect();
    let disk_ids: Vec<u32> = disk_items.iter().map(|item| item.virtual_id.get()).collect();

    assert_eq!(mem_ids, expected_order, "memory iterator output matches canonical sort order for cluster {cluster}");
    assert_eq!(disk_ids, expected_order, "disk iterator output matches canonical sort order for cluster {cluster}");
    assert_eq!(disk_ids, mem_ids, "disk iterator equals memory iterator for cluster {cluster}");
    assert_eq!(disk_items.len(), mem_items.len());
    for (disk_item, mem_item) in disk_items.iter().zip(mem_items.iter()) {
        assert_eq!(disk_item.virtual_id, mem_item.virtual_id);
        assert_eq!(disk_item.name, mem_item.name);
        assert_eq!(disk_item.title, mem_item.title);
        assert_eq!(disk_item.source_ordinal, mem_item.source_ordinal);
        assert_eq!(disk_item.item_type, mem_item.item_type);
    }
}

async fn assert_raw_series_contains_all_keys(app_config: &AppConfig, target: &ConfigTarget) {
    let stream = crate::iter_raw_xtream_target_playlist(app_config, target, XtreamCluster::Series)
        .await
        .expect("raw series stream opens");
    let raw_items: Vec<XtreamPlaylistItem> =
        stream.collect::<Vec<_>>().await.into_iter().map(|res| res.expect("item")).collect();
    let raw_ids: Vec<u32> = raw_items.iter().map(|item| item.virtual_id.get()).collect();

    // Raw target iterator iterates the tree through the sorted index; assert every key
    // in the tree (including episode 500) is yielded in (source_ordinal, virtual_id) order.
    assert_eq!(raw_ids, vec![500, 40, 20, 30, 5, 10]);
}

async fn assert_repositioning_moves_item(
    app_config: &Arc<AppConfig>,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    target_disk: &ConfigTarget,
    storage_path: &Path,
) {
    let mut moved = crate::test_config::xtream_series_item(30, None);
    moved.source_ordinal = 20;
    moved.title = "Moved Series Title".intern();

    write_playlist_item_update(app_config, &target.name, &moved).await.expect("disk reposition update");
    playlists.update_playlist_items(target, &[&moved]).await;

    // After moving key 30 from source_ordinal 200 to 20, it precedes key 40 (source_ordinal 50)
    let new_expected_order = [30, 40, 20, 5, 10];
    assert_cluster_iterators_match(
        app_config,
        playlists,
        target,
        target_disk,
        storage_path,
        XtreamCluster::Series,
        &new_expected_order,
    )
    .await;
}

#[tokio::test]
async fn disk_and_memory_list_output_match_canonical_sort_order_after_upserts() {
    let dir = tempdir().expect("tempdir");
    let app_config = Arc::new(app_config_in(dir.path()));
    let target = target_with_options(
        "sort-order-target",
        vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        true,
    );
    let storage_path = {
        let config = app_config.config.load();
        xtream_get_storage_path(&config, &target.name).expect("storage path")
    };
    tokio::fs::create_dir_all(&storage_path).await.expect("create storage dir");

    for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
        seed_test_cluster(&storage_path, cluster);
    }

    let storage = load_xtream_target_storage(&app_config, &target).await.expect("load xtream storage");
    let playlists = Arc::new(PlaylistStorageState::new());
    playlists.cache_playlist(&target.name, crate::PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;

    for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
        apply_test_upserts(&app_config, &target, &playlists, cluster).await;
    }

    let expected_order = [40, 20, 30, 5, 10];
    let mut target_disk = target.clone();
    target_disk.use_memory_cache = false;

    for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
        assert_cluster_iterators_match(
            &app_config,
            &playlists,
            &target,
            &target_disk,
            &storage_path,
            cluster,
            &expected_order,
        )
        .await;
    }

    assert_raw_series_contains_all_keys(&app_config, &target).await;
    assert_repositioning_moves_item(&app_config, &playlists, &target, &target_disk, &storage_path).await;
}
