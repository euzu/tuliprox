use super::{
    assign_local_series_info_episode_key, assign_media_server_series_info_episode,
    get_input_media_server_playlist_file_path, materialize_media_server_series_info_episodes,
    normalize_target_playlist_epg_ids, persist_playlist_views, persist_playlist_with_mode,
    publication::{playlist_has_items, validate_target_playlist_persistence, TargetPersistenceMode},
    rewrite_local_series_info_episode_virtual_id, rewrite_series_episode_parent_virtual_ids,
    rewrite_series_info_episode_virtual_id, skipped_clusters, LocalEpisodeKey, PlaylistPublicationPlan,
    ProviderEpisodeKey, TargetCacheReloadStage, TargetPlaylistPersistOptions,
};
use crate::{
    get_series_cat_collection_path, get_target_storage_path, get_vod_cat_collection_path, load_m3u_target_storage,
    load_xtream_target_storage, strm_get_file_paths, xtream_get_storage_path, BPlusTreeQuery, PlaylistStorageState,
    TargetIdMapping, VirtualIdRecord,
};
use arc_swap::{ArcSwap, ArcSwapOption};
use shared::{
    foundation::get_filter,
    model::{
        ClusterFlags, ConfigPaths, ConfigTargetDto, ConfigTargetOptions, EpgOutputOptions, EpisodeStreamProperties,
        M3uPlaylistItem, M3uTargetOutputDto, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemHeader,
        PlaylistItemType, ProcessingOrder, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
        SeriesStreamDetailSeasonProperties, SeriesStreamProperties, StreamProperties, StrmExportStyle,
        StrmTargetOutputDto, TargetOutputDto, UUIDType, VirtualId, XtreamCluster, XtreamPlaylistItem,
        XtreamTargetOutputDto,
    },
    utils::{hash_string_as_hex, Internable},
};
use std::{collections::HashMap, path::Path, sync::Arc};
use tuliprox_core::{
    model::{
        ApiProxyConfig, AppConfig, Config, ConfigTarget, CustomStreamResponse, HdHomeRunConfig, M3uTargetOutput,
        MediaToolCapabilities, SourcesConfig, StagedFilter, StrmTargetFlagsSet, StrmTargetOutput, TargetExecutionPlan,
        TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    utils::{normalize_string_path, FileLockManager},
};

fn target_with_outputs(output: Vec<TargetOutput>) -> ConfigTarget {
    target_with_options("target-empty-guard", output, false)
}

fn target_with_options(name: &str, output: Vec<TargetOutput>, use_memory_cache: bool) -> ConfigTarget {
    ConfigTarget {
        curation: None,
        id: 1,
        enabled: true,
        name: name.to_string(),
        options: None,
        sort: None,
        filter: StagedFilter::default(),
        output,
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::new(None)),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache,
    }
}

fn test_app_config(storage_dir: &Path) -> Arc<AppConfig> {
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

fn target_group(cluster: XtreamCluster, id: u32, name: &str) -> PlaylistGroup {
    let name = name.intern();
    let mut header = PlaylistItemHeader {
        id: id.to_string().intern(),
        input_stream_id: id.to_string().intern(),
        name: Arc::clone(&name),
        title: Arc::clone(&name),
        group: Arc::clone(&name),
        input_name: "target-test-input".intern(),
        item_type: PlaylistItemType::from(cluster),
        xtream_cluster: cluster,
        category_id: id,
        ..PlaylistItemHeader::default()
    };
    header.gen_uuid();
    PlaylistGroup { id, title: name, channels: vec![PlaylistItem { header }], xtream_cluster: cluster }
}

fn xtream_target(name: &str, use_memory_cache: bool) -> ConfigTarget {
    target_with_options(
        name,
        vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        use_memory_cache,
    )
}

async fn cached_target_signature(
    playlist_state: &PlaylistStorageState,
    target_name: &str,
) -> (usize, Vec<(XtreamCluster, u32, Arc<str>)>) {
    let cache = playlist_state.data.read().await;
    let target = cache.get(target_name).expect("target cache");
    let mapping_len = target.id_mapping.as_ref().expect("cached ID mapping").len();
    let xtream = target.xtream.as_ref().expect("cached Xtream storage");
    let mut items = Vec::new();
    for (cluster, storage) in [
        (XtreamCluster::Live, &xtream.live),
        (XtreamCluster::Video, &xtream.vod),
        (XtreamCluster::Series, &xtream.series),
    ] {
        for virtual_id in 1..=16 {
            if let Some(item) = storage.query(&virtual_id) {
                items.push((cluster, virtual_id, Arc::clone(&item.name)));
            }
        }
    }
    (mapping_len, items)
}

async fn seed_memory_cached_xtream_target(
    app_config: &Arc<AppConfig>,
    playlist_state: &Arc<PlaylistStorageState>,
    target: &ConfigTarget,
) {
    let mut baseline = vec![
        target_group(XtreamCluster::Live, 101, "old-live"),
        target_group(XtreamCluster::Video, 201, "old-vod"),
        target_group(XtreamCluster::Series, 301, "old-series"),
    ];
    persist_playlist_with_mode(
        app_config,
        &mut baseline,
        None,
        target,
        Some(playlist_state),
        TargetPlaylistPersistOptions::default(),
        TargetPersistenceMode::Persist,
    )
    .await
    .expect("baseline target persistence");
}

use tempfile::tempdir;

fn curation_persist_options(publication_plan: PlaylistPublicationPlan) -> TargetPlaylistPersistOptions {
    TargetPlaylistPersistOptions { publication_plan, ..TargetPlaylistPersistOptions::default() }
}

fn target_test_app_config(storage_dir: &Path) -> Arc<AppConfig> {
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

fn mixed_output_target() -> ConfigTarget {
    let xtream = XtreamTargetOutputDto {
        t_filter: Some(
            shared::foundation::get_filter(r#"Group = "Curated alias""#, None).expect("Xtream output filter"),
        ),
        ..Default::default()
    };
    let mut m3u = M3uTargetOutputDto { filename: Some("curated.m3u".to_string()), ..Default::default() };
    m3u.t_filter = Some(shared::foundation::get_filter(r#"Group = "Base movie""#, None).expect("M3U output filter"));
    ConfigTarget::from(&ConfigTargetDto {
        name: "curated-output-test".to_string(),
        output: vec![
            TargetOutputDto::Xtream(xtream),
            TargetOutputDto::M3u(m3u),
            TargetOutputDto::Strm(StrmTargetOutputDto {
                directory: "strm".to_string(),
                flat: true,
                cleanup: true,
                ..StrmTargetOutputDto::default()
            }),
        ],
        use_memory_cache: true,
        ..ConfigTargetDto::default()
    })
}

fn target_video_group(title: &str, uuid: UUIDType) -> PlaylistGroup {
    PlaylistGroup {
        id: 1,
        title: title.intern(),
        channels: vec![PlaylistItem {
            header: PlaylistItemHeader {
                id: "1".intern(),
                name: title.intern(),
                title: title.intern(),
                group: title.intern(),
                url: format!("http://example.invalid/{title}").intern(),
                uuid,
                item_type: PlaylistItemType::Video,
                xtream_cluster: XtreamCluster::Video,
                ..PlaylistItemHeader::default()
            },
        }],
        xtream_cluster: XtreamCluster::Video,
    }
}

fn strm_files_below(path: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(path) else { return files };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if entry_path.is_dir() {
            files.extend(strm_files_below(&entry_path));
        } else if entry_path.extension().is_some_and(|extension| extension == "strm") {
            files.push(entry_path);
        }
    }
    files
}

fn epg_normalization_options(enabled: bool) -> ConfigTargetOptions {
    ConfigTargetOptions {
        epg_output: EpgOutputOptions { lowercase_ids: enabled, ..EpgOutputOptions::default() },
        ..ConfigTargetOptions::default()
    }
}

fn epg_normalization_playlist() -> Vec<PlaylistGroup> {
    let channel = |epg_channel_id: Option<&str>, name: &str| PlaylistItem {
        header: PlaylistItemHeader {
            name: name.intern(),
            title: "Visible Title".intern(),
            group: "Visible Group".intern(),
            epg_channel_id: epg_channel_id.map(Internable::intern),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            ..PlaylistItemHeader::default()
        },
    };

    vec![PlaylistGroup {
        id: 1,
        title: "Live".intern(),
        channels: vec![
            channel(Some("Example.Channel"), "Mixed Case"),
            channel(Some("already.lower"), "Lowercase"),
            channel(Some(""), "Empty"),
            channel(None, "Missing"),
        ],
        xtream_cluster: XtreamCluster::Live,
    }]
}

fn make_local_series_info(series_uuid: &str, episodes: Vec<(u32, &str, &str)>) -> PlaylistItem {
    let episode_props = episodes
        .into_iter()
        .map(|(id, title, direct_source)| SeriesStreamDetailEpisodeProperties {
            id,
            episode_num: 0,
            season: 0,
            title: title.intern(),
            container_extension: "".intern(),
            custom_sid: None,
            added: "".intern(),
            direct_source: direct_source.intern(),
            tmdb: None,
            release_date: "".intern(),
            series_release_date: None,
            plot: None,
            crew: None,
            duration_secs: 0,
            duration: "".intern(),
            movie_image: "".intern(),
            bitrate: 0,
            rating: None,
            video: None,
            audio: None,
        })
        .collect();

    PlaylistItem {
        header: PlaylistItemHeader {
            id: series_uuid.intern(),
            item_type: PlaylistItemType::LocalSeriesInfo,
            xtream_cluster: XtreamCluster::Series,
            additional_properties: Some(StreamProperties::Series(Box::new(SeriesStreamProperties {
                name: "Series".intern(),
                details: Some(SeriesStreamDetailProperties {
                    year: None,
                    seasons: None,
                    episodes: Some(episode_props),
                }),
                ..SeriesStreamProperties::default()
            }))),
            ..PlaylistItemHeader::default()
        },
    }
}

fn make_local_series_episode(series_uuid: &str, direct_source: &str, virtual_id: u32) -> PlaylistItemHeader {
    PlaylistItemHeader {
        parent_code: series_uuid.intern(),
        url: direct_source.intern(),
        item_type: PlaylistItemType::LocalSeries,
        xtream_cluster: XtreamCluster::Series,
        virtual_id: VirtualId::new(virtual_id),
        ..PlaylistItemHeader::default()
    }
}

fn make_media_server_episode(
    series_uuid: &str,
    item_id: &str,
    virtual_id: u32,
    season: u32,
    episode: u32,
) -> PlaylistItemHeader {
    PlaylistItemHeader {
        id: format!("media-server:server:shows:episode:{item_id}").intern(),
        name: format!("Episode {episode}").intern(),
        title: format!("Episode {episode}").intern(),
        parent_code: series_uuid.intern(),
        url: format!("media-server://plex/server/{item_id}?part_key=%2Flibrary%2Fparts%2Fredacted").intern(),
        item_type: PlaylistItemType::Series,
        xtream_cluster: XtreamCluster::Series,
        virtual_id: VirtualId::new(virtual_id),
        additional_properties: Some(StreamProperties::Episode(Box::new(EpisodeStreamProperties {
            episode_id: 0,
            episode,
            season,
            added: Some("1700000000".intern()),
            release_date: Some("2024-02-03".intern()),
            series_release_date: Some("2024-01-01".intern()),
            plot: Some("Episode summary".intern()),
            tmdb: Some(67890),
            movie_image: "media-server://image/plex/server/episode?image_path=%2Flibrary%2Fmetadata%2Fredacted%2Fthumb"
                .intern(),
            container_extension: "mkv".intern(),
            video: Some(r#"{"codec_name":"h264"}"#.intern()),
            audio: Some(r#"{"codec_name":"aac"}"#.intern()),
        }))),
        ..PlaylistItemHeader::default()
    }
}

mod lifecycle;
mod playlist;
mod policy;
mod recovery;
mod storage;
mod transport;
