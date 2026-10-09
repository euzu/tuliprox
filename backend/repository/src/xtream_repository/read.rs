use super::{
    cluster_flag, get_collection_path, merge_preserved_stream_properties, target_category_lock_path,
    write_playlists_to_file, xtream_cluster_category_collection, xtream_get_file_path, xtream_get_storage_path,
    CategoryEntry, PlaylistStorageState, XtreamClusterEvaluationReport, XtreamClusterPublishBatchResult,
};
use crate::{
    bplustree::{BPlusTreeError, BPlusTreeQuery},
    error_macros::cant_read_result,
    playlist_backend::{iter_raw_playlist, Xtream},
    playlist_scratch::PlaylistScratch,
    storage::{get_input_storage_path, get_target_id_mapping_file, get_target_storage_path},
    storage_const,
    target_id_mapping::VirtualIdRecord,
    xtream_playlist_iterator::XtreamPlaylistJsonIterator,
    LockedReceiverStream,
};
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Value};
use shared::{
    concat_string,
    error::{string_to_io_error, TuliproxError},
    model::{
        xtream_const::XTREAM_CLUSTER, ClusterFlags, PlaylistGroup, PlaylistItem, PlaylistItemType, ProviderId,
        VirtualId, XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::{
    io,
    io::{Error, ErrorKind},
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{AppConfig, ConfigInput, ConfigTarget, PlaylistXtreamCategory, ProxyUserCredentials},
    utils::{file_exists_async, json_write_documents_to_file, FileReadGuard},
};

pub(super) fn get_map_item_as_str(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    if let Some(value) = map.get(key) {
        if let Some(result) = value.as_str() {
            return Some(result.to_string());
        }
    }
    None
}

pub fn xtream_get_epg_file_path_for_target(path: &Path) -> PathBuf {
    path.join(concat_string!("epg.", storage_const::FILE_SUFFIX_DB))
}

pub(super) fn xtream_get_file_path_for_name(storage_path: &Path, name: &str) -> PathBuf {
    storage_path.join(concat_string!(name, ".", storage_const::FILE_SUFFIX_DB))
}

async fn xtream_read_item_for_stream_id(
    cfg: &AppConfig,
    stream_id: u32,
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Result<XtreamPlaylistItem, Error> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    let file_lock = cfg.file_locks.read_lock(&xtream_path).await;
    let xtream_path_clone = xtream_path.clone();
    tokio::task::spawn_blocking(move || -> Result<XtreamPlaylistItem, Error> {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path_clone)?;
        match query.query_zero_copy(&stream_id) {
            Ok(Some(item)) => Ok(item),
            Ok(None) => Err(Error::new(ErrorKind::NotFound, format!("Item {stream_id} not found in {cluster}"))),
            Err(err) => Err(Error::other(format!("Query failed for {stream_id} in {cluster}: {err}"))),
        }
    })
    .await
    .map_err(|err| Error::other(format!("Query task failed for {stream_id} in {cluster}: {err}")))?
}

async fn xtream_read_series_item_for_stream_id(
    cfg: &AppConfig,
    stream_id: u32,
    storage_path: &Path,
) -> Result<XtreamPlaylistItem, Error> {
    let xtream_path = xtream_get_file_path(storage_path, XtreamCluster::Series);
    let file_lock = cfg.file_locks.read_lock(&xtream_path).await;
    let xtream_path_clone = xtream_path.clone();
    tokio::task::spawn_blocking(move || -> Result<XtreamPlaylistItem, Error> {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path_clone)?;
        match query.query_zero_copy(&stream_id) {
            Ok(Some(item)) => Ok(item),
            Ok(None) => Err(Error::new(ErrorKind::NotFound, format!("Item {stream_id} not found in series"))),
            Err(err) => Err(Error::other(format!("Query failed for {stream_id} in series: {err}"))),
        }
    })
    .await
    .map_err(|err| Error::other(format!("Query task failed for {stream_id} in series: {err}")))?
}

async fn xtream_get_item_for_stream_id_from_memory(
    virtual_id: u32,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    xtream_cluster: Option<XtreamCluster>,
) -> Result<Option<(XtreamPlaylistItem, VirtualIdRecord)>, Error> {
    if let Some(playlist) = playlists.data.read().await.get(target.name.as_str()) {
        return match (playlist.xtream.as_ref(), playlist.id_mapping.as_ref()) {
            (Some(xtream_storage), Some(id_mapping)) => {
                let mapping = id_mapping
                    .query(&VirtualId::new(virtual_id))
                    .ok_or_else(|| {
                        string_to_io_error(format!(
                            "Could not find mapping for target {} and id {}",
                            target.name, virtual_id
                        ))
                    })?
                    .clone();
                let result = match mapping.item_type {
                    PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo => Ok(xtream_storage
                        .series
                        .query(&mapping.virtual_id.get())
                        .ok_or_else(|| string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))?
                        .clone()),
                    PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                        log::debug!("In-memory series item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}", virtual_id, mapping.parent_virtual_id, mapping.provider_id);

                        if let Some(item) = xtream_storage.series.query(&virtual_id) {
                            Ok(item.clone())
                        } else if let Some(item) = xtream_storage.series.query(&mapping.parent_virtual_id.get()) {
                            let mut xc_item = item.clone();
                            xc_item.provider_id = mapping.provider_id;
                            xc_item.item_type = PlaylistItemType::Series;
                            xc_item.virtual_id = mapping.virtual_id;
                            Ok(xc_item)
                        } else {
                            Err(string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))
                        }
                    }
                    PlaylistItemType::Catchup => {
                        log::debug!("In-memory catchup item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}", virtual_id, mapping.parent_virtual_id, mapping.provider_id);
                        let cluster = cluster_or_item_type!(xtream_cluster, mapping.item_type);
                        let item = match cluster {
                            XtreamCluster::Live => xtream_storage.live.query(&mapping.parent_virtual_id.get()),
                            XtreamCluster::Video => xtream_storage.vod.query(&mapping.parent_virtual_id.get()),
                            XtreamCluster::Series => xtream_storage.series.query(&mapping.parent_virtual_id.get()),
                        };

                        if let Some(pl_item) = item {
                            let mut xc_item = pl_item.clone();
                            xc_item.provider_id = mapping.provider_id;
                            xc_item.item_type = PlaylistItemType::Catchup;
                            xc_item.virtual_id = mapping.virtual_id;
                            Ok(xc_item)
                        } else {
                            Err(string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))
                        }
                    }
                    _ => {
                        let cluster = cluster_or_item_type!(xtream_cluster, mapping.item_type);
                        Ok((match cluster {
                            XtreamCluster::Live => xtream_storage.live.query(&virtual_id),
                            XtreamCluster::Video => xtream_storage.vod.query(&virtual_id),
                            XtreamCluster::Series => xtream_storage.series.query(&virtual_id),
                        })
                        .ok_or_else(|| string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))?
                        .clone())
                    }
                };

                result.map(|xpli| Some((xpli, mapping)))
            }
            _ => Ok(None),
        };
    }
    //Err(string_to_io_error(format!("Failed to read xtream item for id {virtual_id}. No entry found.")))
    Ok(None)
}

pub async fn xtream_get_item_for_stream_id(
    virtual_id: u32,
    app_config: &Arc<AppConfig>,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    xtream_cluster: Option<XtreamCluster>,
) -> Result<XtreamPlaylistItem, Error> {
    if target.use_memory_cache {
        if let Ok(Some((playlist_item, _virtual_record))) =
            xtream_get_item_for_stream_id_from_memory(virtual_id, playlists, target, xtream_cluster).await
        {
            return Ok(playlist_item);
        }
        // fall through to disk lookup on cache miss
    }

    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str())
        .ok_or_else(|| string_to_io_error(format!("Could not find path for target {}", target.name)))?;
    let storage_path = xtream_get_storage_path(&config, target.name.as_str())
        .ok_or_else(|| string_to_io_error(format!("Could not find path for target {} xtream output", target.name)))?;
    {
        let result = if let Some(cluster) = xtream_cluster {
            xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, cluster).await
        } else {
            let target_id_mapping_file = get_target_id_mapping_file(&target_path);
            let target_name = target.name.clone();
            let file_lock = app_config.file_locks.read_lock(&target_id_mapping_file).await;
            let target_id_mapping_file_clone = target_id_mapping_file.clone();
            let mapping = tokio::task::spawn_blocking(move || -> Result<VirtualIdRecord, Error> {
                let _guard = file_lock;
                let mut target_id_mapping =
                    BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&target_id_mapping_file_clone).map_err(|err| {
                        string_to_io_error(format!("Could not load id mapping for target {target_name} err:{err}"))
                    })?;
                match target_id_mapping.query_zero_copy(&virtual_id) {
                    Ok(Some(record)) => Ok(record),
                    Ok(None) => Err(string_to_io_error(format!(
                        "Could not find mapping for target {target_name} and id {virtual_id}"
                    ))),
                    Err(err) => Err(string_to_io_error(format!("Query failed for id {virtual_id}: {err}"))),
                }
            })
            .await
            .map_err(|err| string_to_io_error(format!("Mapping query task failed for id {virtual_id}: {err}")))??;
            match mapping.item_type {
                PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo => {
                    xtream_read_series_item_for_stream_id(app_config, virtual_id, &storage_path).await
                }
                PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                    log::debug!(
                        "Disk series item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}",
                        virtual_id,
                        mapping.parent_virtual_id,
                        mapping.provider_id
                    );

                    if let Ok(episode) =
                        xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, XtreamCluster::Series)
                            .await
                    {
                        return Ok(episode);
                    }

                    if let Ok(mut item) = xtream_read_series_item_for_stream_id(
                        app_config,
                        mapping.parent_virtual_id.get(),
                        &storage_path,
                    )
                    .await
                    {
                        item.provider_id = mapping.provider_id;
                        item.item_type = PlaylistItemType::Series;
                        item.virtual_id = mapping.virtual_id;
                        return Ok(item);
                    }

                    return Err(Error::other(format!("Failed to find episode item with virtual-id {virtual_id}")));
                }
                PlaylistItemType::Catchup => {
                    log::debug!(
                        "Disk catchup item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}",
                        virtual_id,
                        mapping.parent_virtual_id,
                        mapping.provider_id
                    );
                    let cluster = cluster_or_item_type!(xtream_cluster, mapping.item_type);
                    let mut item = xtream_read_item_for_stream_id(
                        app_config,
                        mapping.parent_virtual_id.get(),
                        &storage_path,
                        cluster,
                    )
                    .await?;
                    item.provider_id = mapping.provider_id;
                    item.item_type = PlaylistItemType::Catchup;
                    item.virtual_id = mapping.virtual_id;
                    Ok(item)
                }
                _ => {
                    let cluster = cluster_or_item_type!(xtream_cluster, mapping.item_type);
                    xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, cluster).await
                }
            }
        };

        result
    }
}

pub async fn xtream_load_rewrite_playlist(
    cluster: XtreamCluster,
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    category_id: Option<u32>,
    user: &ProxyUserCredentials,
) -> Result<XtreamPlaylistJsonIterator, TuliproxError> {
    XtreamPlaylistJsonIterator::new(cluster, app_config, target, category_id, user).await
}

pub async fn iter_raw_xtream_target_playlist(
    app_config: &AppConfig,
    target: &ConfigTarget,
    cluster: XtreamCluster,
) -> Option<LockedReceiverStream<Result<XtreamPlaylistItem, TuliproxError>>> {
    let config = app_config.config.load();
    let storage_path = xtream_get_storage_path(&config, target.name.as_str())?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);

    // Xtream partitions by cluster at the file level, so every item in this
    // database already belongs to `cluster` and no per-item filter is needed.
    iter_raw_playlist::<Xtream, u32, _>(app_config, &xtream_path, |_| true).await
}

pub async fn iter_raw_xtream_input_playlist(
    app_config: &AppConfig,
    input: &ConfigInput,
    cluster: XtreamCluster,
) -> Option<LockedReceiverStream<Result<XtreamPlaylistItem, TuliproxError>>> {
    let config = app_config.config.load();
    let storage_dir = &config.storage_dir;
    let storage_path = get_input_storage_path(&input.name, storage_dir).await.ok()?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);

    iter_raw_playlist::<Xtream, u32, _>(app_config, &xtream_path, |_| true).await
}

/// Counts entries in one active persisted raw input cluster without materializing them.
pub async fn count_input_xtream_cluster(
    app_config: &AppConfig,
    input: &ConfigInput,
    cluster: XtreamCluster,
) -> Result<Option<usize>, TuliproxError> {
    let storage_dir = app_config.config.load().storage_dir.clone();
    let storage_path = get_input_storage_path(&input.name, &storage_dir).await.map_err(|err| {
        TuliproxError::RepositoryXtream(format!("Failed to resolve active input storage for {}: {err}", input.name))
    })?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);
    let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
    let query_path = xtream_path.clone();
    let count = tokio::task::spawn_blocking(move || -> io::Result<Option<usize>> {
        let _guard = file_lock;
        let mut query = match BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&query_path) {
            Ok(query) => query,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        query.len().map(Some).map_err(BPlusTreeError::to_io)
    })
    .await
    .map_err(|err| cant_read_result!(RepositoryXtream, "xtream", &xtream_path, err))?
    .map_err(|err| cant_read_result!(RepositoryXtream, "xtream", &xtream_path, err))?;

    Ok(count)
}

pub fn playlist_iter_to_stream<I, P>(channels: Option<(FileReadGuard, I)>) -> impl Stream<Item = Result<Bytes, String>>
where
    I: Iterator<Item = (P, bool)> + 'static,
    P: Serialize,
{
    match channels {
        Some((_, chans)) => {
            // Convert iterator items to Result<Bytes, String> with minimal allocations
            let mapped = chans.map(move |(item, has_next)| match serde_json::to_string(&item) {
                Ok(mut content) => {
                    if has_next {
                        content.push(',');
                    }
                    Ok(Bytes::from(content))
                }
                Err(_) => Ok(Bytes::from("")),
            });
            stream::iter(mapped).left_stream()
        }
        None => stream::once(async { Ok(Bytes::from("")) }).right_stream(),
    }
}

pub async fn xtream_get_playlist_categories(
    app_config: &AppConfig,
    target_name: &str,
    cluster: XtreamCluster,
) -> Option<Vec<PlaylistXtreamCategory>> {
    let file_path = {
        let config = app_config.config.load();
        let storage_path = xtream_get_storage_path(&config, target_name)?;
        get_collection_path(&storage_path, xtream_cluster_category_collection(cluster))
    };
    let category_lock_path = target_category_lock_path(&file_path);
    let _file_lock = app_config.file_locks.read_lock(&category_lock_path).await;
    let content = tokio::fs::read_to_string(&file_path).await.ok()?;
    serde_json::from_str::<Vec<PlaylistXtreamCategory>>(&content).ok()
}

impl XtreamClusterPublishBatchResult {
    pub(super) fn record_cluster_error(&mut self, cluster: XtreamCluster, error: TuliproxError) {
        self.failed_cluster = Some(cluster);
        self.errors.push(error);
    }

    pub(super) fn record_evaluation(&mut self, evaluation: XtreamClusterEvaluationReport) {
        if let Some(acceptance) = evaluation.quality_acceptance {
            self.quality_acceptances.push(acceptance);
        }
        if let Some(rejection) = evaluation.quality_rejection {
            self.quality_rejections.push(rejection);
        }
        if let Some(force_update) = evaluation.force_update {
            self.force_updates.push(force_update);
        }
    }
}

pub async fn persist_input_xtream_playlist(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    persist_input_xtream_playlist_with_empty_replacements(app_config, storage_path, playlist, ClusterFlags::empty())
        .await
}

#[allow(clippy::too_many_lines)]
pub async fn persist_input_xtream_playlist_with_empty_replacements(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    playlist: Vec<PlaylistGroup>,
    replace_empty_clusters: ClusterFlags,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    let mut errors = Vec::new();

    let mut fetched_categories = PlaylistScratch::<Vec<Value>>::new(1_000);
    let mut fetched_scratch = PlaylistScratch::<Vec<PlaylistItem>>::new(50_000);
    let mut stored_scratch = PlaylistScratch::<IndexMap<u32, XtreamPlaylistItem>>::new(50_000);

    // load
    for cluster in XTREAM_CLUSTER {
        let xtream_path = xtream_get_file_path(storage_path, cluster);
        if file_exists_async(&xtream_path).await {
            let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
            let xtream_path = xtream_path.clone();
            let stored_entries = match tokio::task::spawn_blocking(move || {
                let _guard = file_lock;
                let mut entries = IndexMap::new();
                let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)?;
                for entry in query.iter() {
                    let (_, doc) = entry?;
                    entries.insert(doc.provider_id, doc);
                }
                Ok::<_, std::io::Error>(entries)
            })
            .await
            {
                Ok(Ok(entries)) => Some(entries),
                Ok(Err(err)) => {
                    errors.push(format!("Failed to read stored xtream playlist entries for {cluster}: {err}"));
                    None
                }
                Err(err) => {
                    errors.push(format!("Failed to load stored xtream playlist entries for {cluster}: {err}"));
                    None
                }
            };

            if let Some(entries) = stored_entries {
                *stored_scratch.get_mut(cluster) = entries;
            }
        }
    }

    if !errors.is_empty() {
        return (playlist, Some(TuliproxError::RepositoryXtream(errors.join("\n"))));
    }

    let mut groups = IndexMap::new();

    for mut plg in playlist {
        if !&plg.channels.is_empty() {
            fetched_categories.get_mut(plg.xtream_cluster).push(json!(CategoryEntry {
                category_id: plg.id,
                category_name: plg.title.clone(),
                parent_id: 0
            }));

            let channels = std::mem::take(&mut plg.channels);
            for mut pli in channels {
                let stored_col = stored_scratch.get_mut(plg.xtream_cluster);
                let fetched_col = fetched_scratch.get_mut(plg.xtream_cluster);

                if let Ok(provider_id) = pli.header.id.parse::<u32>() {
                    if let Some(stored_pli) = stored_col.get_mut(&provider_id) {
                        if let (Some(new_stream_props), Some(old_stream_props)) =
                            (&mut pli.header.additional_properties, stored_pli.additional_properties.take())
                        {
                            merge_preserved_stream_properties(new_stream_props, &old_stream_props);
                        }
                    }
                }
                fetched_col.push(pli);
            }
            groups.insert((plg.xtream_cluster, plg.id), plg);
        }
    }

    let mut processed_scratch = PlaylistScratch::<Vec<PlaylistItem>>::new(0);
    for xc in XTREAM_CLUSTER {
        processed_scratch.set(
            xc,
            if !replace_empty_clusters.contains(cluster_flag(xc))
                && !stored_scratch.is_empty(xc)
                && fetched_scratch.is_empty(xc)
            {
                stored_scratch.take(xc).iter().map(|(_, item)| PlaylistItem::from(item)).collect::<Vec<PlaylistItem>>()
            } else {
                fetched_scratch.take(xc)
            },
        );
    }
    drop(stored_scratch);
    drop(fetched_scratch);

    let root_path = storage_path.to_path_buf();
    let app_cfg = app_config.clone();
    for cluster in XTREAM_CLUSTER {
        let col_path = get_collection_path(&root_path, xtream_cluster_category_collection(cluster));
        let data = fetched_categories.get_mut(cluster);
        // if there is no data save only if no file exists! Prevent data loss from failed download attempt
        if !data.is_empty()
            || replace_empty_clusters.contains(cluster_flag(cluster))
            || !file_exists_async(&col_path).await
        {
            let lock = app_cfg.file_locks.write_lock(&col_path).await;
            if let Err(err) = json_write_documents_to_file(&col_path, data).await {
                errors.push(format!("Persisting collection failed: {}: {err}", col_path.display()));
            }
            drop(lock);
        }
    }

    for cluster in XTREAM_CLUSTER {
        let col = processed_scratch.take(cluster);

        // persist playlist
        if let Err(err) = write_playlists_to_file(
            app_config,
            storage_path,
            false,
            |item| ProviderId::new(item.provider_id),
            vec![(cluster, col.iter().map(Into::into).collect::<Vec<XtreamPlaylistItem>>())],
            replace_empty_clusters,
        )
        .await
        {
            errors.push(format!("Persisting collection failed:{err}"));
        }

        for item in col {
            let group_key = (item.header.xtream_cluster, item.header.category_id);
            groups
                .entry(group_key)
                .or_insert_with(|| PlaylistGroup {
                    id: item.header.category_id,
                    title: item.header.group.clone(),
                    channels: Vec::new(),
                    xtream_cluster: item.header.xtream_cluster,
                })
                .channels
                .push(item);
        }
    }

    let result = groups.into_values().collect();

    let err = if errors.is_empty() { None } else { Some(TuliproxError::RepositoryXtream(errors.join("\n"))) };

    (result, err)
}

pub async fn load_input_xtream_playlist(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    clusters: &[XtreamCluster],
) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    let mut groups: IndexMap<(XtreamCluster, u32), PlaylistGroup> = IndexMap::new();

    for &cluster in clusters {
        let xtream_path = xtream_get_file_path(storage_path, cluster);
        if xtream_path.exists() {
            let cat_col_name = xtream_cluster_category_collection(cluster);
            let cat_path = get_collection_path(storage_path, cat_col_name);

            if cat_path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(&cat_path).await {
                    if let Ok(cats) = serde_json::from_str::<Vec<CategoryEntry>>(&content) {
                        for cat in cats {
                            groups.insert(
                                (cluster, cat.category_id),
                                PlaylistGroup {
                                    id: cat.category_id,
                                    title: cat.category_name,
                                    channels: Vec::new(),
                                    xtream_cluster: cluster,
                                },
                            );
                        }
                    }
                }
            }

            // Load Items
            let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
            let xtream_path_err = xtream_path.clone();
            let items = tokio::task::spawn_blocking(move || -> Result<Vec<XtreamPlaylistItem>, TuliproxError> {
                let _guard = file_lock;
                let mut items = Vec::new();
                let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)
                    .map_err(|error| TuliproxError::RepositoryXtream(error.to_string()))?;
                for entry in query.iter() {
                    let (_, item) = entry.map_err(|error| TuliproxError::RepositoryXtream(error.to_string()))?;
                    items.push(item);
                }
                Ok(items)
            })
            .await
            .map_err(|err| cant_read_result!(RepositoryXtream, "xtream", &xtream_path_err, err))??;

            for item in items {
                let cat_id = item.category_id;
                groups
                    .entry((cluster, cat_id))
                    .or_insert_with(|| PlaylistGroup {
                        id: cat_id,
                        title: "Unknown".intern(),
                        channels: Vec::new(),
                        xtream_cluster: cluster,
                    })
                    .channels
                    .push(PlaylistItem::from(&item));
            }
        }
    }

    Ok(groups.into_values().collect())
}
