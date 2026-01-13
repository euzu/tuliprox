use crate::api::model::AppState;
use crate::model::PlaylistXtreamCategory;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{Config, ConfigTarget};
use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery, BPlusTreeUpdate};
use crate::repository::playlist_scratch::PlaylistScratch;
use crate::repository::storage::{get_file_path_for_db_index, get_target_id_mapping_file, get_target_storage_path};
use crate::repository::storage_const;
use crate::repository::target_id_mapping::VirtualIdRecord;
use crate::repository::xtream_playlist_iterator::XtreamPlaylistJsonIterator;
use crate::utils::file_reader;
use crate::utils::json_write_documents_to_file;
use crate::utils::FileReadGuard;
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use indexmap::IndexMap;
use log::error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shared::error::{info_err_res, info_err, notify_err, string_to_io_error, TuliproxError};
use shared::model::xtream_const::XTREAM_CLUSTER;
use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemType, SeriesStreamProperties, StreamProperties, VideoStreamProperties, XtreamCluster, XtreamPlaylistItem};
use shared::utils::{arc_str_serde, get_u32_from_serde_value, intern, StringInterner};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use shared::notify_err_res;

macro_rules! cant_write_result {
    ($path:expr, $err:expr) => {
        notify_err!(
            "failed to write xtream playlist: {} - {}",
            $path.display(),
            $err
        )
    };
}

#[inline]
pub fn get_collection_path(path: &Path, collection: &str) -> PathBuf {
    path.join(format!("{collection}.json"))
}

#[inline]
pub fn get_live_cat_collection_path(path: &Path) -> PathBuf {
    get_collection_path(path, storage_const::COL_CAT_LIVE)
}

#[inline]
pub fn get_vod_cat_collection_path(path: &Path) -> PathBuf {
    get_collection_path(path, storage_const::COL_CAT_VOD)
}

#[inline]
pub fn get_series_cat_collection_path(path: &Path) -> PathBuf {
    get_collection_path(path, storage_const::COL_CAT_SERIES)
}

pub fn ensure_xtream_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    if let Some(path) = xtream_get_storage_path(cfg, target_name) {
        if std::fs::create_dir_all(&path).is_err() {
            let msg = format!(
                "Failed to save xtream data, can't create directory {}",
                &path.display()
            );
            return notify_err_res!("{msg}");
        }
        Ok(path)
    } else {
        let msg = format!("Failed to save xtream data, can't create directory for target {target_name}");
        notify_err_res!("{msg}")
    }
}

#[derive(Debug, Copy, Clone)]
enum StorageKey {
    VirtualId,
    ProviderId,
}

async fn write_playlists_to_file(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    with_index: bool,
    storage_key: StorageKey,
    collections: Vec<(XtreamCluster, Vec<XtreamPlaylistItem>)>,
) -> Result<(), TuliproxError> {
    for (cluster, playlist) in collections {
        if playlist.is_empty() {
            continue;
        }
        let xtream_path = xtream_get_file_path(storage_path, cluster);
        {
            let _file_lock = app_config.file_locks.write_lock(&xtream_path).await;
            let mut tree = BPlusTree::new();
            for item in playlist {
                tree.insert(match storage_key {
                    StorageKey::VirtualId => item.virtual_id,
                    StorageKey::ProviderId => item.provider_id,
                }, item);
            }
            if with_index {
                tree.store_with_index(&xtream_path, |pli| pli.source_ordinal).map_err(|err| cant_write_result!(&xtream_path, err))?;
            } else {
                tree.store(&xtream_path).map_err(|err| cant_write_result!(&xtream_path, err))?;
            }
        }
    }
    Ok(())
}

pub async fn write_playlist_item_update(
    app_config: &Arc<AppConfig>,
    target_name: &str,
    pli: &XtreamPlaylistItem,
) -> Result<(), TuliproxError> {
    let storage_path = {
        let config = app_config.config.load();
        ensure_xtream_storage_path(&config, target_name)?
    };
    let xtream_path = xtream_get_file_path(&storage_path, pli.xtream_cluster);
    {
        let _file_lock = app_config.file_locks.write_lock(&xtream_path).await;
        let mut tree = if xtream_path.exists() {
            BPlusTreeUpdate::try_new(&xtream_path).map_err(|err| cant_write_result!(&xtream_path, err))?
        } else {
            // This case should rarely happen as the file is usually pre-created, but for safety:
            return Err(cant_write_result!(&xtream_path, "BPlusTree file not found for append"));
        };

        tree.update(&pli.virtual_id, pli.clone()).map_err(|err| cant_write_result!(&xtream_path, err))?;
    }
    Ok(())
}

pub async fn write_playlist_batch_item_upsert(
    app_config: &Arc<AppConfig>,
    target_name: &str,
    xtream_cluster: XtreamCluster,
    pli_list: &[XtreamPlaylistItem],
) -> Result<(), TuliproxError> {
    let storage_path = {
        let config = app_config.config.load();
        ensure_xtream_storage_path(&config, target_name)?
    };
    let xtream_path = xtream_get_file_path(&storage_path, xtream_cluster);
    {
        let _file_lock = app_config.file_locks.write_lock(&xtream_path).await;
        let mut tree = if xtream_path.exists() {
            BPlusTreeUpdate::try_new(&xtream_path).map_err(|err| cant_write_result!(&xtream_path, err))?
        } else {
            // This case should rarely happen as the file is usually pre-created, but for safety:
            return Err(cant_write_result!(&xtream_path, "BPlusTree file not found for append"));
        };

        let batch: Vec<(&u32, &XtreamPlaylistItem)> = pli_list.iter().map(|pli| (&pli.virtual_id, pli)).collect();
        tree.upsert_batch(&batch).map_err(|err| cant_write_result!(&xtream_path, err))?;
    }
    Ok(())
}

fn get_map_item_as_str(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    if let Some(value) = map.get(key) {
        if let Some(result) = value.as_str() {
            return Some(result.to_string());
        }
    }
    None
}

pub type CategoryKey = (XtreamCluster, Arc<str>);

// Because interner is not thread safe we can't use it currently for interning.
// We leave the argument for later optimizations.
async fn load_old_category_ids(path: &Path, _interner: &mut StringInterner) -> (u32, HashMap<CategoryKey, u32>) {
    let old_path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut result: HashMap<CategoryKey, u32> = HashMap::new();
        let mut max_id: u32 = 0;
        for (cluster, cat) in [
            (XtreamCluster::Live, storage_const::COL_CAT_LIVE),
            (XtreamCluster::Video, storage_const::COL_CAT_VOD),
            (XtreamCluster::Series, storage_const::COL_CAT_SERIES)]
        {
            let col_path = get_collection_path(&old_path, cat);
            if col_path.exists() {
                if let Ok(file) = File::open(&col_path) {
                    let reader = file_reader(file);
                    match serde_json::from_reader(reader) {
                        Ok(value) => {
                            if let Value::Array(list) = value {
                                for entry in list {
                                    if let Some(category_id) = entry.get(crate::model::XC_TAG_CATEGORY_ID).and_then(get_u32_from_serde_value) {
                                        if let Value::Object(item) = entry {
                                            if let Some(category_name) = get_map_item_as_str(&item, crate::model::XC_TAG_CATEGORY_NAME) {
                                                result.insert((cluster, /*interner.*/intern(&category_name)), category_id);
                                                max_id = max_id.max(category_id);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            log::warn!("Failed to parse category file {}: {err}", col_path.display());
                        }
                    }
                }
            }
        }
        (max_id, result)
    }).await.unwrap_or_else(|_| (0, HashMap::new()))
}

pub fn xtream_get_storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> {
    get_target_storage_path(cfg, target_name).map(|target_path| target_path.join(PathBuf::from(storage_const::PATH_XTREAM)))
}

pub fn xtream_get_epg_file_path(path: &Path) -> PathBuf {
    path.join(storage_const::FILE_EPG)
}

fn xtream_get_file_path_for_name(storage_path: &Path, name: &str) -> PathBuf {
    storage_path.join(format!("{name}.{}", storage_const::FILE_SUFFIX_DB))
}

pub fn xtream_get_file_path(storage_path: &Path, cluster: XtreamCluster) -> PathBuf {
    xtream_get_file_path_for_name(storage_path, &cluster.as_str().to_lowercase())
}

#[derive(Serialize, Deserialize)]
pub struct CategoryEntry {
    pub category_id: u32,
    #[serde(with = "arc_str_serde")]
    pub category_name: Arc<str>,
    pub parent_id: u32,
}

pub async fn xtream_write_playlist(
    app_cfg: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
    interner: &mut StringInterner,
) -> Result<(), TuliproxError> {
    let path = {
        let config = app_cfg.config.load();
        ensure_xtream_storage_path(&config, target.name.as_str())?
    };
    let mut errors = Vec::new();
    let mut cat_live_col = Vec::with_capacity(1_000);
    let mut cat_series_col = Vec::with_capacity(1_000);
    let mut cat_vod_col = Vec::with_capacity(1_000);
    let mut live_col = Vec::with_capacity(50_000);
    let mut series_col = Vec::with_capacity(50_000);
    let mut vod_col = Vec::with_capacity(50_000);

    let categories = create_categories(playlist, &path, interner).await;
    {
        for (xtream_cluster, category) in categories {
            match xtream_cluster {
                XtreamCluster::Live => &mut cat_live_col,
                XtreamCluster::Series => &mut cat_series_col,
                XtreamCluster::Video => &mut cat_vod_col,
            }.push(category);
        }
    }

    for plg in playlist.iter_mut() {
        if plg.channels.is_empty() {
            continue;
        }

        for pli in &plg.channels {
            let col = match pli.header.xtream_cluster {
                XtreamCluster::Live => &mut live_col,
                XtreamCluster::Series => &mut series_col,
                XtreamCluster::Video => &mut vod_col,
            };
            col.push(pli);
        }
    }

    let root_path = path.clone();
    let app_config = app_cfg.clone();
    for (col_path, data) in [
        (get_live_cat_collection_path(&root_path), &cat_live_col),
        (get_vod_cat_collection_path(&root_path), &cat_vod_col),
        (get_series_cat_collection_path(&root_path), &cat_series_col),
    ] {
        let lock = app_config.file_locks.write_lock(&col_path).await;
        match json_write_documents_to_file(&col_path, data).await {
            Ok(()) => {}
            Err(err) => {
                errors.push(format!("Persisting collection failed: {}: {err}", col_path.display()));
            }
        }
        drop(lock);
    }

    if let Err(err) = write_playlists_to_file(
        app_cfg,
        &path,
        true,
        StorageKey::VirtualId,
        vec![
            (XtreamCluster::Live, live_col.iter().map(|item| XtreamPlaylistItem::from(&**item)).collect::<Vec<XtreamPlaylistItem>>()),
            (XtreamCluster::Video, vod_col.iter().map(|item| XtreamPlaylistItem::from(&**item)).collect::<Vec<XtreamPlaylistItem>>()),
            (XtreamCluster::Series, series_col.iter().map(|item| XtreamPlaylistItem::from(&**item)).collect::<Vec<XtreamPlaylistItem>>()),
        ],
    ).await {
        errors.push(format!("Persisting collection failed:{err}"));
    }

    if !errors.is_empty() {
        return info_err_res!("{}", errors.join("\n"));
    }

    Ok(())
}

async fn create_categories(playlist: &mut [PlaylistGroup], path: &Path, interner: &mut StringInterner) -> Vec<(XtreamCluster, CategoryEntry)> {
    // preserve category_ids
    let (max_cat_id, existing_cat_ids) = load_old_category_ids(path, interner).await;
    let mut cat_id_counter = max_cat_id;

    let mut new_categories: IndexMap<CategoryKey, CategoryEntry> = IndexMap::new();

    let mut last_cluster: Option<XtreamCluster> = None;
    let mut last_group = intern("");
    let mut last_category_id: u32 = 0;

    for plg in playlist.iter_mut() {
        if plg.channels.is_empty() {
            continue;
        }

        for channel in &mut plg.channels {
            let cluster = channel.header.xtream_cluster;
            let group = &channel.header.group;

            // Fast path
            if last_cluster == Some(cluster) && &last_group == group {
                channel.header.category_id = last_category_id;
                continue;
            }

            let key = (cluster, Arc::clone(group));

            let entry = new_categories.entry(key.clone()).or_insert_with(|| {
                let cat_id = existing_cat_ids.get(&key).copied().unwrap_or_else(|| {
                    cat_id_counter += 1;
                    cat_id_counter
                });

                CategoryEntry {
                    category_id: cat_id,
                    category_name: group.clone(),
                    parent_id: 0,
                }
            });

            last_cluster = Some(cluster);
            last_group = Arc::clone(group);
            last_category_id = entry.category_id;

            channel.header.category_id = last_category_id;
        }
    }
    new_categories.into_iter()
        .map(|((cluster, _group), value)| (cluster, value))
        .collect::<Vec<(XtreamCluster, CategoryEntry)>>()
}

pub fn xtream_get_collection_path(
    cfg: &Config,
    target_name: &str,
    collection_name: &str,
) -> Result<PathBuf, Error> {
    if let Some(path) = xtream_get_storage_path(cfg, target_name) {
        let col_path = get_collection_path(&path, collection_name);
        if col_path.exists() {
            return Ok(col_path);
        }
    }
    Err(string_to_io_error(format!("Can't find collection: {target_name}/{collection_name}")))
}

async fn xtream_read_item_for_stream_id(
    cfg: &AppConfig,
    stream_id: u32,
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Result<XtreamPlaylistItem, Error> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    {
        let _file_lock = cfg.file_locks.read_lock(&xtream_path).await;
        let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)?;
        match query.query(&stream_id) {
            Ok(Some(item)) => Ok(item),
            Ok(None) => Err(Error::new(ErrorKind::NotFound, format!("Item {stream_id} not found in {cluster}"))),
            Err(err) => Err(Error::other(format!("Query failed for {stream_id} in {cluster}: {err}"))),
        }
    }
}

async fn xtream_read_series_item_for_stream_id(
    cfg: &AppConfig,
    stream_id: u32,
    storage_path: &Path,
) -> Result<XtreamPlaylistItem, Error> {
    let xtream_path = xtream_get_file_path(storage_path, XtreamCluster::Series);
    {
        let _file_lock = cfg.file_locks.read_lock(&xtream_path).await;
        let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)?;
        match query.query(&stream_id) {
            Ok(Some(item)) => Ok(item),
            Ok(None) => Err(Error::new(ErrorKind::NotFound, format!("Item {stream_id} not found in series"))),
            Err(err) => Err(Error::other(format!("Query failed for {stream_id} in series: {err}"))),
        }
    }
}


macro_rules! try_cluster {
    ($xtream_cluster:expr, $item_type:expr, $virtual_id:expr) => {
        $xtream_cluster
            .or_else(|| XtreamCluster::try_from($item_type).ok())
            .ok_or_else(|| string_to_io_error(format!("Could not determine cluster for xtream item with stream-id {}",$virtual_id)))
    };
}

async fn xtream_get_item_for_stream_id_from_memory(
    virtual_id: u32,
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    xtream_cluster: Option<XtreamCluster>,
) -> Result<Option<(XtreamPlaylistItem, VirtualIdRecord)>, Error> {
    if let Some(playlist) = app_state.playlists.data.read().await.get(target.name.as_str()) {
        return match playlist.xtream.as_ref() {
            None => {
                Ok(None)
            }
            Some(xtream_storage) => {
                let mapping = xtream_storage.id_mapping.query(&virtual_id).ok_or_else(|| string_to_io_error(format!("Could not find mapping for target {} and id {}", target.name, virtual_id)))?.clone();
                let result = match mapping.item_type {
                    PlaylistItemType::SeriesInfo
                    | PlaylistItemType::LocalSeriesInfo => {
                        Ok(xtream_storage.series.query(&mapping.virtual_id)
                            .ok_or_else(|| string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))?
                            .clone())
                    }
                    PlaylistItemType::Series
                    | PlaylistItemType::LocalSeries => {
                        if let Some(item) = xtream_storage.series.query(&mapping.parent_virtual_id) {
                            let mut xc_item = item.clone();
                            xc_item.provider_id = mapping.provider_id;
                            xc_item.item_type = PlaylistItemType::Series;
                            xc_item.virtual_id = mapping.virtual_id;
                            Ok(xc_item)
                        } else {
                            Ok(xtream_storage.series.query(&virtual_id)
                                .ok_or_else(|| string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))?
                                .clone())
                        }
                    }
                    PlaylistItemType::Catchup => {
                        let cluster = try_cluster!(xtream_cluster, mapping.item_type, virtual_id)?;
                        let item = match cluster {
                            XtreamCluster::Live => xtream_storage.live.query(&mapping.parent_virtual_id),
                            XtreamCluster::Video => xtream_storage.vod.query(&mapping.parent_virtual_id),
                            XtreamCluster::Series => xtream_storage.series.query(&mapping.parent_virtual_id),
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
                        let cluster = try_cluster!(xtream_cluster, mapping.item_type, virtual_id)?;
                        Ok((match cluster {
                            XtreamCluster::Live => xtream_storage.live.query(&virtual_id),
                            XtreamCluster::Video => xtream_storage.vod.query(&virtual_id),
                            XtreamCluster::Series => xtream_storage.series.query(&virtual_id),
                        }).ok_or_else(|| string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))?
                            .clone())
                    }
                };

                result.map(|xpli| Some((xpli, mapping)))
            }
        };
    }
    //Err(string_to_io_error(format!("Failed to read xtream item for id {virtual_id}. No entry found.")))
    Ok(None)
}

pub async fn xtream_get_item_for_stream_id(
    virtual_id: u32,
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    xtream_cluster: Option<XtreamCluster>,
) -> Result<XtreamPlaylistItem, Error> {
    if target.use_memory_cache {
        if let Ok(Some((playlist_item, _virtual_record))) =
            xtream_get_item_for_stream_id_from_memory(virtual_id, app_state, target, xtream_cluster).await {
            return Ok(playlist_item);
        }
        // fall through to disk lookup on cache miss
    }

    let app_config: &AppConfig = &app_state.app_config;
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str()).ok_or_else(|| string_to_io_error(format!("Could not find path for target {}", &target.name)))?;
    let storage_path = xtream_get_storage_path(&config, target.name.as_str()).ok_or_else(|| string_to_io_error(format!("Could not find path for target {} xtream output", &target.name)))?;
    {
        let result = if let Some(cluster) = xtream_cluster {
            xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, cluster).await
        } else {
            let target_id_mapping_file = get_target_id_mapping_file(&target_path);
            let _file_lock = app_config.file_locks.read_lock(&target_id_mapping_file).await;

            let mut target_id_mapping = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&target_id_mapping_file).map_err(|err| string_to_io_error(format!("Could not load id mapping for target {} err:{err}", target.name)))?;
            let mapping = match target_id_mapping.query(&virtual_id) {
                Ok(Some(record)) => Ok(record),
                Ok(None) => Err(string_to_io_error(format!("Could not find mapping for target {} and id {}", target.name, virtual_id))),
                Err(err) => Err(string_to_io_error(format!("Query failed for id {virtual_id}: {err}"))),
            }?;
            match mapping.item_type {
                PlaylistItemType::SeriesInfo
                | PlaylistItemType::LocalSeriesInfo => {
                    xtream_read_series_item_for_stream_id(app_config, virtual_id, &storage_path).await
                }
                PlaylistItemType::Series
                | PlaylistItemType::LocalSeries => {
                    if let Ok(mut item) = xtream_read_series_item_for_stream_id(app_config, mapping.parent_virtual_id, &storage_path).await {
                        item.provider_id = mapping.provider_id;
                        item.item_type = PlaylistItemType::Series;
                        item.virtual_id = mapping.virtual_id;
                        Ok(item)
                    } else {
                        xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, XtreamCluster::Series).await
                    }
                }
                PlaylistItemType::Catchup => {
                    let cluster = try_cluster!(xtream_cluster, mapping.item_type, virtual_id)?;
                    let mut item = xtream_read_item_for_stream_id(app_config, mapping.parent_virtual_id, &storage_path, cluster).await?;
                    item.provider_id = mapping.provider_id;
                    item.item_type = PlaylistItemType::Catchup;
                    item.virtual_id = mapping.virtual_id;
                    Ok(item)
                }
                _ => {
                    let cluster = try_cluster!(xtream_cluster, mapping.item_type, virtual_id)?;
                    xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, cluster).await
                }
            }
        };

        result
    }
}

pub async fn xtream_load_rewrite_playlist(
    cluster: XtreamCluster,
    config: &AppConfig,
    target: &ConfigTarget,
    category_id: Option<u32>,
    user: &ProxyUserCredentials,
) -> Result<XtreamPlaylistJsonIterator, TuliproxError> {
    XtreamPlaylistJsonIterator::new(cluster, config, target, category_id, user).await
}

pub async fn iter_raw_xtream_playlist(app_config: &AppConfig, target: &ConfigTarget, cluster: XtreamCluster) -> Option<(FileReadGuard, impl Iterator<Item=(XtreamPlaylistItem, bool)>)> {
    let config = app_config.config.load();
    if let Some(storage_path) = xtream_get_storage_path(&config, target.name.as_str()) {
        let xtream_path = xtream_get_file_path(&storage_path, cluster);
        if !xtream_path.exists() {
            return None;
        }
        let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
        match BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)
            .map_err(|err| info_err!("Could not open BPlusTreeQuery {xtream_path:?} - {err}")) {
        Ok(mut query) => {
            let index_path = get_file_path_for_db_index(&xtream_path);
            let items: Vec<XtreamPlaylistItem> = if index_path.exists() {
                match query.disk_iter_sorted::<u32>() {
                    Ok(iter) => iter.filter_map(Result::ok).map(|(_, v)| v).collect(),
                    Err(err) => {
                        error!("Sorted index error {}: {err}", xtream_path.display());
                        // Re-open query for fallback
                        match BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                            Ok(mut query) => query.iter().map(|(_, v)| v).collect(),
                            Err(_) => Vec::new(),
                        }
                    }
                }
            } else {
                query.iter().map(|(_, v)| v).collect()
            };

            let len = items.len();
            Some((file_lock, items.into_iter().enumerate().map(move |(i, v)| (v, i < len - 1))))
        }
            Err(_) => None
        }
    } else {
        None
    }
}

pub fn playlist_iter_to_stream<I, P>(channels: Option<(FileReadGuard, I)>) -> impl Stream<Item=Result<Bytes, String>>
where
    I: Iterator<Item=(P, bool)> + 'static,
    P: Serialize,
{
    match channels {
        Some((_, chans)) => {
            // Convert iterator items to Result<Bytes, String> with minimal allocations
            let mapped = chans.map(move |(item, has_next)| {
                match serde_json::to_string(&item) {
                    Ok(mut content) => {
                        if has_next { content.push(','); }
                        Ok(Bytes::from(content))
                    }
                    Err(_) => Ok(Bytes::from("")),
                }
            });
            stream::iter(mapped).left_stream()
        }
        None => {
            stream::once(async { Ok(Bytes::from("")) }).right_stream()
        }
    }
}

pub(crate) async fn xtream_get_playlist_categories(config: &Config, target_name: &str, cluster: XtreamCluster) -> Option<Vec<PlaylistXtreamCategory>> {
    let path = xtream_get_collection_path(config, target_name, match cluster {
        XtreamCluster::Live => storage_const::COL_CAT_LIVE,
        XtreamCluster::Video => storage_const::COL_CAT_VOD,
        XtreamCluster::Series => storage_const::COL_CAT_SERIES,
    });
    if let Ok(file_path) = path {
        if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
            return serde_json::from_str::<Vec<PlaylistXtreamCategory>>(&content).ok();
        }
    }
    None
}

#[allow(clippy::too_many_lines)]
pub async fn persist_input_xtream_playlist(app_config: &Arc<AppConfig>, storage_path: &Path,
                                           playlist: Vec<PlaylistGroup>) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    let mut errors = Vec::new();

    let mut fetched_categories = PlaylistScratch::<Vec<Value>>::new(1_000);
    let mut fetched_scratch = PlaylistScratch::<Vec<PlaylistItem>>::new(50_000);
    let mut stored_scratch = PlaylistScratch::<IndexMap::<u32, XtreamPlaylistItem>>::new(50_000);

    // load
    for cluster in XTREAM_CLUSTER {
        let xtream_path = xtream_get_file_path(storage_path, cluster);
        if let Ok(true) = tokio::fs::try_exists(&xtream_path).await {
            let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
            let stored_entries = stored_scratch.get_mut(cluster);
            if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                for (_, doc) in query.iter() {
                    stored_entries.insert(doc.provider_id, doc);
                }
            }
            drop(file_lock);
        }
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
                        if let (Some(new_stream_props), Some(old_stream_props)) = (&mut pli.header.additional_properties, stored_pli.additional_properties.take()) {
                            if !needs_update_info_details(new_stream_props, &old_stream_props) {
                                match (new_stream_props, old_stream_props) {
                                    (StreamProperties::Video(value_1), StreamProperties::Video(value_2)) => {
                                        value_1.details = value_2.details;
                                    }
                                    (StreamProperties::Series(value_1), StreamProperties::Series(value_2)) => {
                                        value_1.details = value_2.details;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                fetched_col.push(pli);
            }
            groups.insert(plg.id, plg);
        }
    }

    let mut processed_scratch = PlaylistScratch::<Vec<PlaylistItem>>::new(0);
    for xc in XTREAM_CLUSTER {
        processed_scratch.set(xc, if !stored_scratch.is_empty(xc) && fetched_scratch.is_empty(xc) {
            stored_scratch.take(xc).iter().map(|(_, item)| PlaylistItem::from(item)).collect::<Vec<PlaylistItem>>()
        } else {
            fetched_scratch.take(xc)
        });
    }
    drop(stored_scratch);
    drop(fetched_scratch);

    let root_path = storage_path.to_path_buf();
    let app_cfg = app_config.clone();
    for cluster in XTREAM_CLUSTER {
        let col_path = match cluster {
            XtreamCluster::Live => get_collection_path(&root_path, storage_const::COL_CAT_LIVE),
            XtreamCluster::Video => get_collection_path(&root_path, storage_const::COL_CAT_VOD),
            XtreamCluster::Series => get_collection_path(&root_path, storage_const::COL_CAT_SERIES),
        };
        let data = fetched_categories.get_mut(cluster);
        // if there is no data save only if no file exists! Prevent data loss from failed download attempt
        if !data.is_empty() || tokio::fs::try_exists(&col_path).await.is_ok_and(|v| !v) {
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
            StorageKey::ProviderId,
            vec![(cluster, col.iter().map(Into::into).collect::<Vec<XtreamPlaylistItem>>())],
        ).await {
            errors.push(format!("Persisting collection failed:{err}"));
        }

        for item in col {
            groups
                .entry(item.header.category_id)
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

    let result = groups.into_iter().map(|(_, group)| group).collect();

    let err = if errors.is_empty() {
        None
    } else {
        Some(notify_err!("{}", errors.join("\n")))
    };

    (result, err)
}

// Checks if the info has changed after the last update
fn needs_update_info_details(
    new_stream_props: &StreamProperties,
    old_stream_props: &StreamProperties,
) -> bool {
    let new_modified = new_stream_props.get_last_modified();
    let old_modified = old_stream_props.get_last_modified();

    match (new_modified, old_modified) {
        (Some(new_ts), Some(old_ts)) => new_ts > old_ts,
        (None, Some(_)) => true,
        _ => false,
    }
}

async fn persist_input_info(app_config: &Arc<AppConfig>, storage_path: &Path, cluster: XtreamCluster,
                            input_name: &str, provider_id: u32, props: StreamProperties) -> Result<(), Error> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    if xtream_path.exists() {
        {
            let _file_lock = app_config.file_locks.write_lock(&xtream_path).await;
            let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> = BPlusTreeUpdate::try_new(&xtream_path).map_err(|err| Error::other(format!("failed to open BPlusTree for input {input_name}: {err}")))?;
            match tree.query(&provider_id) {
                Ok(Some(mut pli)) => {
                    pli.additional_properties = Some(props);
                    tree.update(&provider_id, pli).map_err(|err| Error::other(format!("failed to write {cluster} info for input {input_name}: {err}")))?;
                }
                Ok(None) => {
                    error!("Could not find input entry for provider_id: {provider_id} and input: {input_name}");
                }
                Err(err) => {
                    error!("Failed to query BPlusTree for provider_id: {provider_id} and input: {input_name}: {err}");
                }
            }
        }
    }
    Ok(())
}

pub async fn persist_input_info_batch(app_config: &Arc<AppConfig>, storage_path: &Path, cluster: XtreamCluster,
                                      input_name: &str, updates: Vec<(u32, StreamProperties)>) -> Result<(), Error> {
    if updates.is_empty() { return Ok(()); }
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    if xtream_path.exists() {
        let _file_lock = app_config.file_locks.write_lock(&xtream_path).await;
        let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> = BPlusTreeUpdate::try_new(&xtream_path)
            .map_err(|err| Error::other(format!("failed to open BPlusTree for input {input_name}: {err}")))?;

        let mut updated_plis = Vec::with_capacity(updates.len());
        for (provider_id, props) in updates {
            match tree.query(&provider_id) {
                Ok(Some(mut pli)) => {
                    pli.additional_properties = Some(props);
                    updated_plis.push((provider_id, pli));
                }
                Ok(None) => {
                    error!("Could not find input entry for provider_id: {provider_id} and input: {input_name}");
                }
                Err(err) => {
                    error!("Failed to query BPlusTree for provider_id: {provider_id} and input: {input_name}: {err}");
                }
            }
        }

        if !updated_plis.is_empty() {
            let refs: Vec<(&u32, &XtreamPlaylistItem)> = updated_plis.iter()
                .map(|(id, pli)| (id, pli))
                .collect();
            tree.update_batch(&refs).map_err(|err| Error::other(format!("failed to write batch {cluster} info for input {input_name}: {err}")))?;
        }
    }
    Ok(())
}


pub async fn persist_input_vod_info(app_config: &Arc<AppConfig>, storage_path: &Path,
                                    cluster: XtreamCluster, input_name: &str, provider_id: u32,
                                    props: &VideoStreamProperties) -> Result<(), Error> {
    persist_input_info(app_config, storage_path, cluster, input_name, provider_id, StreamProperties::Video(Box::new(props.clone()))).await
}

pub async fn persist_input_vod_info_batch(app_config: &Arc<AppConfig>, storage_path: &Path,
                                          cluster: XtreamCluster, input_name: &str,
                                          updates: Vec<(u32, VideoStreamProperties)>) -> Result<(), Error> {
    let batch = updates.into_iter()
        .map(|(id, props)| (id, StreamProperties::Video(Box::new(props))))
        .collect();
    persist_input_info_batch(app_config, storage_path, cluster, input_name, batch).await
}

pub async fn persists_input_series_info(app_config: &Arc<AppConfig>, storage_path: &Path,
                                        cluster: XtreamCluster, input_name: &str, provider_id: u32,
                                        props: &SeriesStreamProperties) -> Result<(), Error> {
    persist_input_info(app_config, storage_path, cluster, input_name, provider_id, StreamProperties::Series(Box::new(props.clone()))).await
}

pub async fn persist_input_series_info_batch(app_config: &Arc<AppConfig>, storage_path: &Path,
                                             cluster: XtreamCluster, input_name: &str,
                                             updates: Vec<(u32, SeriesStreamProperties)>) -> Result<(), Error> {
    let batch = updates.into_iter()
        .map(|(id, props)| (id, StreamProperties::Series(Box::new(props))))
        .collect();
    persist_input_info_batch(app_config, storage_path, cluster, input_name, batch).await
}

pub async fn load_input_xtream_playlist(app_config: &Arc<AppConfig>, storage_path: &Path, clusters: &[XtreamCluster]) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    let mut groups: IndexMap<(XtreamCluster, u32), PlaylistGroup> = IndexMap::new();

    for &cluster in clusters {
        let xtream_path = xtream_get_file_path(storage_path, cluster);
        if xtream_path.exists() {
            let cat_col_name = match cluster {
                XtreamCluster::Live => storage_const::COL_CAT_LIVE,
                XtreamCluster::Video => storage_const::COL_CAT_VOD,
                XtreamCluster::Series => storage_const::COL_CAT_SERIES,
            };
            let cat_path = get_collection_path(storage_path, cat_col_name);

            if cat_path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(&cat_path).await {
                    if let Ok(cats) = serde_json::from_str::<Vec<CategoryEntry>>(&content) {
                        for cat in cats {
                            groups.insert((cluster, cat.category_id), PlaylistGroup {
                                id: cat.category_id,
                                title: cat.category_name,
                                channels: Vec::new(),
                                xtream_cluster: cluster,
                            });
                        }
                    }
                }
            }

            // Load Items
            let _file_lock = app_config.file_locks.read_lock(&xtream_path).await;
            if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                for (_, ref item) in query.iter() {
                    let cat_id = item.category_id;
                    groups.entry((cluster, cat_id))
                        .or_insert_with(|| PlaylistGroup {
                            id: cat_id,
                            title: intern("Unknown"),
                            channels: Vec::new(),
                            xtream_cluster: cluster,
                        })
                        .channels.push(PlaylistItem::from(item));
                }
            }
        }
    }

    Ok(groups.into_values().collect())
}

