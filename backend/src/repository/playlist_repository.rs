use crate::api::model::{AppState, PlaylistM3uStorage, PlaylistStorage, PlaylistStorageState, PlaylistXtreamStorage};
use crate::model::Epg;
use crate::model::{AppConfig, ConfigInput, ConfigTarget, TargetOutput};
use crate::processing::processor::playlist::{apply_filter_to_playlist, PlaylistProcessingContext};
use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery};
use crate::repository::epg_repository::epg_write;
use crate::repository::m3u_repository::{load_input_m3u_playlist, m3u_get_file_path_for_db, m3u_write_playlist, persist_input_m3u_playlist};
use crate::repository::storage::{ensure_target_storage_path, get_input_storage_path, get_target_id_mapping_file, get_target_storage_path};
use crate::repository::storage_const::FILE_SUFFIX_DB;
use crate::repository::strm_repository::write_strm_playlist;
use crate::repository::target_id_mapping::{TargetIdMapping, VirtualIdRecord};
use crate::repository::xtream_repository::{load_input_xtream_playlist, persist_input_xtream_playlist, xtream_get_file_path, xtream_get_storage_path, xtream_write_playlist};
use crate::utils;
use log::info;
use crate::repository::{LocalLibraryDiskPlaylistSource, M3uDiskPlaylistSource, MemoryPlaylistSource, PlaylistSource, XtreamDiskPlaylistSource};
use shared::error::{info_err, TuliproxError};
use shared::model::xtream_const::XTREAM_CLUSTER;
use shared::model::{InputType, M3uPlaylistItem, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, StreamProperties, XtreamCluster, XtreamPlaylistItem};
use shared::utils::{is_dash_url, is_hls_url, Internable};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::repository::library_repository::{load_input_local_library_playlist, persist_input_library_playlist};

struct LocalEpisodeKey {
    path: Arc<str>,
    virtual_id: u32,
}

pub struct ProviderEpisodeKey {
    pub(crate) provider_id: u32,
    pub(crate) virtual_id: u32,
}

pub async fn persist_playlist(app_config: &Arc<AppConfig>, playlist: &mut [PlaylistGroup], epg: Option<&Epg>,
                              target: &ConfigTarget, playlist_state: Option<&Arc<PlaylistStorageState>>) -> Result<(), Vec<TuliproxError>> {
    let mut errors = vec![];
    let config = &app_config.config.load();
    let target_path = match ensure_target_storage_path(config, &target.name) {
        Ok(path) => path,
        Err(err) => return Err(vec![err]),
    };

    let (mut target_id_mapping, file_lock) = match get_target_id_mapping(app_config, &target_path, target.use_memory_cache).await {
        Ok(result) => result,
        Err(err) => return Err(vec![err]),
    };

    let mut local_library_series = HashMap::<Arc<str>, Vec<LocalEpisodeKey>>::new();
    let mut provider_series = HashMap::<Arc<str>, Vec<ProviderEpisodeKey>>::new();

    let mut source_ordinal: u32 = 0;
    // Virtual IDs assignment
    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            let header = &mut channel.header;
            source_ordinal += 1;
            header.source_ordinal = source_ordinal;
            let provider_id = header.get_provider_id().unwrap_or_default();
            if provider_id == 0 {
                header.item_type = match (is_hls_url(&header.url), header.item_type) {
                    (true, _) => PlaylistItemType::LiveHls,
                    (false, PlaylistItemType::Live) => {
                        if is_dash_url(&header.url) {
                            PlaylistItemType::LiveDash
                        } else {
                            PlaylistItemType::LiveUnknown
                        }
                    }
                    _ => header.item_type,
                };
            }

            let uuid = header.get_uuid();
            let item_type = header.item_type;
            header.virtual_id = target_id_mapping.get_and_update_virtual_id(uuid, provider_id, item_type, 0);

            if item_type == PlaylistItemType::LocalSeries {
                assign_local_series_info_episode_key(&mut local_library_series, header, item_type);
            } else if item_type == PlaylistItemType::Series {
                assign_provider_series_info_episode_key(&mut provider_series, header, item_type);
            }
        }
    }

    rewrite_series_info_episode_virtual_id(playlist, &local_library_series, &provider_series);

    for output in &target.output {
        let mut filtered: Option<Vec<PlaylistGroup>> = match output {
            TargetOutput::Xtream(out) => out.filter.as_ref().and_then(|flt| apply_filter_to_playlist(playlist, flt)),
            TargetOutput::M3u(out) => out.filter.as_ref().and_then(|flt| apply_filter_to_playlist(playlist, flt)),
            TargetOutput::Strm(out) => out.filter.as_ref().and_then(|flt| apply_filter_to_playlist(playlist, flt)),
            TargetOutput::HdHomeRun(_) => None,
        };

        let pl: &mut [PlaylistGroup] = if let Some(filtered_playlist) = filtered.as_mut() {
            filtered_playlist.as_mut_slice()
        } else {
            playlist
        };

        let result = match output {
            TargetOutput::Xtream(_xtream_output) => xtream_write_playlist(app_config, target, pl).await,
            TargetOutput::M3u(m3u_output) => m3u_write_playlist(app_config, target, m3u_output, &target_path, pl).await,
            TargetOutput::Strm(strm_output) => write_strm_playlist(app_config, target, strm_output, pl).await,
            TargetOutput::HdHomeRun(_hdhomerun_output) => Ok(()),
        };

        match result {
            Ok(()) => {
                if !pl.is_empty() {
                    let epg_pl: &[PlaylistGroup] = pl;
                    if let Err(err) = epg_write(config, target, &target_path, epg, output, Some(epg_pl)).await {
                        errors.push(err);
                    }
                }
            }
            Err(err) => errors.push(err)
        }
    }

    if let Err(err) = target_id_mapping.persist() {
        errors.push(info_err!("{err}"));
    }
    // We must drop target_id_mapping here to release the exclusive B+Tree lock
    // otherwise the subsequent load_xtream_target_storage will deadlock waiting for a shared lock.
    drop(target_id_mapping);
    drop(file_lock);

    if target.use_memory_cache {
        if let Some(playlist_storage) = playlist_state {
            for output in &target.output {
                match output {
                    TargetOutput::Xtream(_) => {
                        if let Ok(storage) = load_xtream_target_storage(app_config, target).await {
                            playlist_storage.cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;
                        }
                    }
                    TargetOutput::M3u(_) => {
                        if let Ok(storage) = load_m3u_target_storage(app_config, target).await {
                            playlist_storage.cache_playlist(&target.name, PlaylistStorage::M3uPlaylist(Box::new(storage))).await;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn assign_local_series_info_episode_key(local_library_series: &mut HashMap<Arc<str>, Vec<LocalEpisodeKey>>, header: &mut PlaylistItemHeader, item_type: PlaylistItemType) {
    // we need to rewrite local series info with the new virtual ids
    if item_type == PlaylistItemType::LocalSeries {
        local_library_series
            .entry(header.parent_code.clone())
            .or_default()
            .push(LocalEpisodeKey {
                path: header.url.clone(),
                virtual_id: header.virtual_id,
            });
    }
}

fn assign_provider_series_info_episode_key(provider_series: &mut HashMap<Arc<str>, Vec<ProviderEpisodeKey>>, header: &mut PlaylistItemHeader, item_type: PlaylistItemType) {
    // we need to rewrite local series info with the new virtual ids
    if item_type == PlaylistItemType::Series {
        provider_series
            .entry(header.parent_code.clone())
            .or_default()
            .push(ProviderEpisodeKey {
                provider_id: header.get_provider_id().unwrap_or_default(),
                virtual_id: header.virtual_id,
            });
    }
}

#[allow(clippy::implicit_hasher)]
fn rewrite_local_series_info_episode_virtual_id(pli: &mut PlaylistItem, local_library_series: &HashMap<Arc<str>, Vec<LocalEpisodeKey>>) {
    let header = &mut pli.header;
    // the local_library_series key is the id of the SeriesInfo. The episodes have their parent SeriesInfo id as parent_code.
    // When we populate  local_library_series, we use the episodes.parent_code. Here we need to use the SeriesInfo.id to get the assigned episodes.
    if let Some(episode_keys) = local_library_series.get(&*header.id) {
        if let Some(StreamProperties::Series(series)) = header.additional_properties.as_mut() {
            if let Some(episodes) =
                series.details.as_mut().and_then(|d| d.episodes.as_mut())
            {
                for episode in episodes.iter_mut() {
                    for episode_key in episode_keys {
                        if episode.direct_source == episode_key.path {
                            episode.id = episode_key.virtual_id;
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::implicit_hasher)]
pub fn rewrite_provider_series_info_episode_virtual_id<P>(pli: &mut P, provider_series: &HashMap<Arc<str>, Vec<ProviderEpisodeKey>>)
where
    P: PlaylistEntry,
{
    if let Some(episode_keys) = provider_series.get(&pli.get_uuid().intern()) {
        if let Some(StreamProperties::Series(series)) = pli.get_additional_properties_mut() {
            if let Some(episodes) =
                series.details.as_mut().and_then(|d| d.episodes.as_mut())
            {
                for episode in episodes.iter_mut() {
                    for episode_key in episode_keys {
                        if episode.id == episode_key.provider_id {
                            episode.id = episode_key.virtual_id;
                            break;
                        }
                    }
                }
            }
        }
    }
}

fn rewrite_series_info_episode_virtual_id(playlist: &mut [PlaylistGroup],
                                          local_library_series: &HashMap<Arc<str>, Vec<LocalEpisodeKey>>,
                                          provider_series: &HashMap<Arc<str>, Vec<ProviderEpisodeKey>>) {
    if local_library_series.is_empty() && provider_series.is_empty() {
        return;
    }
    for group in playlist.iter_mut() {
        for channel in &mut group.channels {
            let item_type = channel.header.item_type;
            if item_type == PlaylistItemType::SeriesInfo {
                rewrite_provider_series_info_episode_virtual_id(channel, provider_series);
            } else if item_type == PlaylistItemType::LocalSeriesInfo {
                rewrite_local_series_info_episode_virtual_id(channel, local_library_series);
            } else if item_type == PlaylistItemType::LocalSeries {
                channel.header.parent_code = "".intern();
            }
        }
    }
}

pub async fn get_target_id_mapping(cfg: &AppConfig, target_path: &Path, use_memory_cache: bool) -> Result<(TargetIdMapping, utils::FileWriteGuard), TuliproxError> {
    let target_id_mapping_file = get_target_id_mapping_file(target_path);
    let file_lock = cfg.file_locks.write_lock(&target_id_mapping_file).await;
    let mapping = TargetIdMapping::new(&target_id_mapping_file, use_memory_cache)?;
    Ok((mapping, file_lock))
}


async fn load_target_id_mapping_as_tree(app_config: &AppConfig, target_path: &Path, target: &ConfigTarget) -> Result<BPlusTree<u32, VirtualIdRecord>, TuliproxError> {
    let target_id_mapping_file = get_target_id_mapping_file(target_path);
    let _file_lock = app_config.file_locks.read_lock(&target_id_mapping_file).await;
    BPlusTree::<u32, VirtualIdRecord>::load(&target_id_mapping_file).map_err(|err|
        info_err!("Could not find path for target {} err:{err}", &target.name))
}

async fn load_xtream_playlist_as_tree(app_config: &AppConfig, storage_path: &Path, cluster: XtreamCluster) -> BPlusTree<u32, XtreamPlaylistItem> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    let _file_lock = app_config.file_locks.read_lock(&xtream_path).await;
    let mut tree = BPlusTree::<u32, XtreamPlaylistItem>::new();
    if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
        for (_, doc) in query.iter() {
            tree.insert(doc.virtual_id, doc);
        }
    }
    tree
}

async fn load_xtream_target_storage(app_config: &AppConfig, target: &ConfigTarget) -> Result<PlaylistXtreamStorage, TuliproxError> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str()).ok_or_else(||
        info_err!("Could not find path for target {}", &target.name))?;

    let storage_path = xtream_get_storage_path(&config, target.name.as_str()).ok_or_else(||
        info_err!("Could not find path for target {} xtream output", &target.name))?;

    let target_id_mapping = load_target_id_mapping_as_tree(app_config, &target_path, target).await?;
    let live_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Live).await;
    let vod_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Video).await;
    let series_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Series).await;

    Ok(PlaylistXtreamStorage {
        id_mapping: target_id_mapping,
        live: live_storage,
        vod: vod_storage,
        series: series_storage,
    })
}

async fn load_m3u_target_storage(app_config: &AppConfig, target: &ConfigTarget) -> Result<PlaylistM3uStorage, TuliproxError> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str()).ok_or_else(||
        info_err!("Could not find path for target {}", &target.name))?;

    let m3u_path = m3u_get_file_path_for_db(&target_path);
    let _file_lock = app_config.file_locks.read_lock(&m3u_path).await;
    let mut tree = BPlusTree::<u32, M3uPlaylistItem>::new();
    if let Ok(mut query) = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path) {
        for (_, doc) in query.iter() {
            tree.insert(doc.virtual_id, doc);
        }
    }
    Ok(tree)
}


pub async fn load_playlists_into_memory_cache(app_state: &AppState) -> Result<(), TuliproxError> {
    for sources in &app_state.app_config.sources.load().sources {
        for target in &sources.targets {
            load_target_into_memory_cache(app_state, target).await;
        }
    }
    Ok(())
}

pub async fn load_target_into_memory_cache(app_state: &AppState, target: &Arc<ConfigTarget>) {
    if target.use_memory_cache {
        info!("Loading target {} into memory cache", target.name);
        for output in &target.output {
            match output {
                TargetOutput::Xtream(_) => {
                    if let Ok(storage) = load_xtream_target_storage(&app_state.app_config, target).await {
                        app_state.cache_playlist(&target.name, PlaylistStorage::XtreamPlaylist(Box::new(storage))).await;
                    }
                }
                TargetOutput::M3u(_) => {
                    if let Ok(storage) = load_m3u_target_storage(&app_state.app_config, target).await {
                        app_state.cache_playlist(&target.name, PlaylistStorage::M3uPlaylist(Box::new(storage))).await;
                    }
                }
                _ => {}
            }
        };
    }
}

pub async fn persist_input_playlist(app_config: &Arc<AppConfig>, input: &ConfigInput, mut playlist: Vec<PlaylistGroup>) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    playlist.iter_mut().for_each(PlaylistGroup::on_load);

    match input.input_type {
        InputType::Xtream | InputType::XtreamBatch => {
            let working_dir = &app_config.config.load().working_dir;
            let storage_path = match get_input_storage_path(&input.name, working_dir) {
                Ok(storage_path) => storage_path,
                Err(err) => {
                    return (playlist, Some(info_err!("Error creating input storage directory for input '{}' failed: {err}", input.name)));
                }
            };
            persist_input_xtream_playlist(app_config, &storage_path, playlist).await
        }

        InputType::M3u | InputType::M3uBatch => {
            // Persist M3U
            let working_dir = &app_config.config.load().working_dir;
            let storage_path = match get_input_storage_path(&input.name, working_dir) {
                Ok(storage_path) => storage_path,
                Err(err) => {
                    return (playlist, Some(info_err!("Error creating input storage directory for input '{}' failed: {err}", input.name)));
                }
            };
            let file_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
            if let Err(err) = persist_input_m3u_playlist(app_config, &file_path, &playlist).await {
                return (playlist, Some(err));
            }
            (playlist, None)
        },
        InputType::Library => {
            // Persist local library playlist
            let working_dir = &app_config.config.load().working_dir;
            let storage_path = match get_input_storage_path(&input.name, working_dir) {
                Ok(storage_path) => storage_path,
                Err(err) => {
                    return (playlist, Some(info_err!("Error creating input storage directory for input '{}' failed: {err}", input.name)));
                }
            };
            let file_path = get_input_local_library_playlist_file_path(&storage_path, &input.name);
            if let Err(err) = persist_input_library_playlist(app_config, &file_path, &playlist).await {
                return (playlist, Some(err));
            }
            (playlist, None)
        }
    }
}

pub async fn load_input_playlist(ctx: &PlaylistProcessingContext, input: &ConfigInput, clusters: Option<&[XtreamCluster]>) -> Result<Box<dyn PlaylistSource>, TuliproxError> {
    let app_config = &ctx.config;
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir)
        .map_err(|e| info_err!("Error getting input path: {e}"))?;

    let disk_based_processing = app_config.config.load().disk_based_processing;

    match input.input_type {
        InputType::Xtream | InputType::XtreamBatch => {
            if disk_based_processing {
                Ok(Box::new(XtreamDiskPlaylistSource::new(app_config, &storage_path).await))
            } else {
                let clusters_to_load = if let Some(c) = clusters {
                    c
                } else {
                    &XTREAM_CLUSTER
                };
                let groups = load_input_xtream_playlist(app_config, &storage_path, clusters_to_load).await?;
                Ok(Box::new(MemoryPlaylistSource::new(groups)))
            }
        }
        InputType::M3u | InputType::M3uBatch => {
            // Load M3U
            let file_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(Box::new(M3uDiskPlaylistSource::new(app_config, &file_path).await))
            } else {
                let groups = load_input_m3u_playlist(app_config, &file_path).await?;
                Ok(Box::new(MemoryPlaylistSource::new(groups)))
            }
        }
        InputType::Library => {
            let file_path = get_input_local_library_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(Box::new(LocalLibraryDiskPlaylistSource::new(app_config, &file_path).await))
            } else {
                let groups = load_input_local_library_playlist(app_config, &file_path).await?;
                Ok(Box::new(MemoryPlaylistSource::new(groups)))
            }
        }
    }
}

pub fn get_input_m3u_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    storage_path.join(format!("m3u_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}

fn get_input_local_library_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    storage_path.join(format!("lib_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}