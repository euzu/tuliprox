use crate::api::model::AppState;
use crate::model::XtreamCategory;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{Config, ConfigTarget};
use crate::model::{ConfigInput, PlaylistXtreamCategory};
use crate::processing::parser::xtream;
use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery, BPlusTreeUpdate, FlushPolicy};
use crate::repository::playlist_scratch::PlaylistScratch;
use crate::repository::storage::{ensure_input_storage_path, get_file_path_for_db_index, get_input_storage_path, get_target_id_mapping_file, get_target_storage_path};
use crate::repository::storage_const;
use crate::repository::target_id_mapping::VirtualIdRecord;
use crate::repository::xtream_playlist_iterator::XtreamPlaylistJsonIterator;
use crate::repository::open_playlist_reader;
use crate::utils::json_write_documents_to_file;
use crate::utils::request::DynReader;
use crate::utils::FileReadGuard;
use crate::utils::{file_exists_async, file_reader};
use bytes::Bytes;
use fs2::FileExt;
use futures::{stream, Stream, StreamExt};
use indexmap::IndexMap;
use log::{error};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shared::error::{info_err_res, notify_err, string_to_io_error, TuliproxError};
use shared::model::xtream_const::XTREAM_CLUSTER;
use shared::model::{LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemType, SeriesStreamProperties, StreamProperties, VideoStreamProperties, XtreamCluster, XtreamPlaylistItem};
use shared::utils::{arc_str_serde, get_u32_from_serde_value, Internable};
use shared::{concat_string, notify_err_res};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

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

pub async fn ensure_xtream_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    if let Some(path) = xtream_get_storage_path(cfg, target_name) {
        if tokio::fs::create_dir_all(&path).await.is_err() {
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

        // Acquire FileLockManager lock (async, in-process coordination)
        let file_lock = app_config.file_locks.write_lock(&xtream_path).await;

        // Move all B+Tree building and I/O to spawn_blocking
        // We take ownership of `playlist` here (no cloning needed)
        let path_clone = xtream_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
            let _guard = file_lock;
            let mut tree = BPlusTree::new();
            for item in playlist {
                tree.insert(match storage_key {
                    StorageKey::VirtualId => item.virtual_id,
                    StorageKey::ProviderId => item.provider_id,
                }, item);
            }
            if with_index {
                tree.store_with_index(&path_clone, |pli| pli.source_ordinal)?;
            } else {
                tree.store(&path_clone)?;
            }
            Ok(())
        })
            .await
            .map_err(|e| notify_err!("Blocking task failed: {e}"))?
            .map_err(|err| cant_write_result!(&xtream_path, err))?;
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
        ensure_xtream_storage_path(&config, target_name).await?
    };
    let xtream_path = xtream_get_file_path(&storage_path, pli.xtream_cluster);

    if !file_exists_async(&xtream_path).await {
        return info_err_res!("BPlusTree file not found for update {}", xtream_path.display());
    }

    // Prepare encoded payload before opening the writer lock.
    let prepared_items = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&[(&pli.virtual_id, pli)])
        .map_err(|e| notify_err!("Failed to serialize value: {e}"))?;

    // Keep FileLockManager lock for cross-operation coordination (e.g. swap + update).
    let file_lock = app_config.file_locks.write_lock(&xtream_path).await;

    let xtream_path_clone = xtream_path.clone();
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        let _guard = file_lock;
        let mut tree = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(&xtream_path_clone)?;
        tree.upsert_batch_encoded(prepared_items)?;
        Ok(())
    })
        .await
        .map_err(|e| notify_err!("Blocking task failed: {e}"))?
        .map_err(|err| cant_write_result!(&xtream_path, err))?;

    Ok(())
}

pub async fn write_playlist_batch_item_upsert(
    app_config: &Arc<AppConfig>,
    target_name: &str,
    xtream_cluster: XtreamCluster,
    pli_list: &[XtreamPlaylistItem],
) -> Result<(), TuliproxError> {
    if pli_list.is_empty() {
        return Ok(());
    }

    let storage_path = {
        let config = app_config.config.load();
        ensure_xtream_storage_path(&config, target_name).await?
    };
    let xtream_path = xtream_get_file_path(&storage_path, xtream_cluster);

    if !file_exists_async(&xtream_path).await {
        return info_err_res!("BPlusTree file not found for upsert {}", xtream_path.display());
    }

    // Prepare encoded payload before opening the writer lock.
    let batch_refs: Vec<(&u32, &XtreamPlaylistItem)> = pli_list
        .iter()
        .map(|pli| (&pli.virtual_id, pli))
        .collect();
    let prepared_items = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch_refs)
        .map_err(|e| notify_err!("Failed to serialize value: {e}"))?;

    // Keep FileLockManager lock for cross-operation coordination (e.g. swap + update).
    let file_lock = app_config.file_locks.write_lock(&xtream_path).await;

    let xtream_path_clone = xtream_path.clone();
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        let _guard = file_lock;
        let mut tree = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(&xtream_path_clone)?;
        tree.upsert_batch_encoded(prepared_items)?;
        Ok(())
    })
        .await
        .map_err(|e| notify_err!("Blocking task failed: {e}"))?
        .map_err(|err| cant_write_result!(&xtream_path, err))?;

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
async fn load_old_category_ids(path: &Path) -> (u32, HashMap<CategoryKey, u32>) {
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
                                                result.insert((cluster, /*interner.*/category_name.intern()), category_id);
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

pub fn xtream_get_epg_file_path_for_target(path: &Path) -> PathBuf {
    path.join(concat_string!("epg.", storage_const::FILE_SUFFIX_DB))
}

fn xtream_get_file_path_for_name(storage_path: &Path, name: &str) -> PathBuf {
    storage_path.join(concat_string!(name, ".", storage_const::FILE_SUFFIX_DB))
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
) -> Result<(), TuliproxError> {
    let path = {
        let config = app_cfg.config.load();
        ensure_xtream_storage_path(&config, target.name.as_str()).await?
    };
    let mut errors = Vec::new();
    let mut cat_live_col = Vec::with_capacity(1_000);
    let mut cat_series_col = Vec::with_capacity(1_000);
    let mut cat_vod_col = Vec::with_capacity(1_000);
    let mut live_col = Vec::with_capacity(50_000);
    let mut series_col = Vec::with_capacity(50_000);
    let mut vod_col = Vec::with_capacity(50_000);

    let categories = create_categories(playlist, &path).await;
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

    // Process each cluster sequentially to avoid holding multiple fully
    // materialized Xtream collections in memory at the same time.
    for (cluster, col) in [
        (XtreamCluster::Live, &live_col),
        (XtreamCluster::Video, &vod_col),
        (XtreamCluster::Series, &series_col),
    ] {
        if col.is_empty() {
            continue;
        }
        let data = col
            .iter()
            .map(|item| XtreamPlaylistItem::from(&**item))
            .collect::<Vec<XtreamPlaylistItem>>();
        if let Err(err) = write_playlists_to_file(
            app_cfg,
            &path,
            true,
            StorageKey::VirtualId,
            vec![(cluster, data)],
        )
            .await
        {
            errors.push(format!("Persisting collection failed:{err}"));
        }
    }

    if !errors.is_empty() {
        return info_err_res!("{}", errors.join("\n"));
    }

    Ok(())
}

async fn create_categories(playlist: &mut [PlaylistGroup], path: &Path) -> Vec<(XtreamCluster, CategoryEntry)> {
    // preserve category_ids
    let (max_cat_id, existing_cat_ids) = load_old_category_ids(path).await;
    let mut cat_id_counter = max_cat_id;

    let mut new_categories: IndexMap<CategoryKey, CategoryEntry> = IndexMap::new();

    for plg in playlist.iter_mut() {
        if plg.channels.is_empty() {
            continue;
        }

        for channel in &mut plg.channels {
            let cluster = channel.header.xtream_cluster;
            let group = &channel.header.group;

            let entry = new_categories.entry((cluster, group.clone()))
                .or_insert_with(|| {
                    let cat_id = existing_cat_ids
                        .get(&(cluster, group.clone()))
                        .copied()
                        .unwrap_or_else(|| {
                            cat_id_counter += 1;
                            cat_id_counter
                        });

                    CategoryEntry {
                        category_id: cat_id,
                        category_name: group.clone(),
                        parent_id: 0,
                    }
                });

            channel.header.category_id = entry.category_id;
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
        return match (playlist.xtream.as_ref(), playlist.id_mapping.as_ref()) {
            (Some(xtream_storage), Some(id_mapping)) => {
                let mapping = id_mapping.query(&virtual_id).ok_or_else(|| string_to_io_error(format!("Could not find mapping for target {} and id {}", target.name, virtual_id)))?.clone();
                let result = match mapping.item_type {
                    PlaylistItemType::SeriesInfo
                    | PlaylistItemType::LocalSeriesInfo => {
                        Ok(xtream_storage.series.query(&mapping.virtual_id)
                            .ok_or_else(|| string_to_io_error(format!("Failed to read xtream item for id {virtual_id}")))?
                            .clone())
                    }
                    PlaylistItemType::Series
                    | PlaylistItemType::LocalSeries => {
                        log::debug!("In-memory series item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}", virtual_id, mapping.parent_virtual_id, mapping.provider_id);
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
                        log::debug!("In-memory catchup item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}", virtual_id, mapping.parent_virtual_id, mapping.provider_id);
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
            _ => Ok(None)
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
            let target_name = target.name.clone();
            let file_lock = app_config.file_locks.read_lock(&target_id_mapping_file).await;
            let target_id_mapping_file_clone = target_id_mapping_file.clone();
            let mapping = tokio::task::spawn_blocking(move || -> Result<VirtualIdRecord, Error> {
                let _guard = file_lock;
                let mut target_id_mapping = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&target_id_mapping_file_clone)
                    .map_err(|err| string_to_io_error(format!("Could not load id mapping for target {target_name} err:{err}")))?;
                match target_id_mapping.query_zero_copy(&virtual_id) {
                    Ok(Some(record)) => Ok(record),
                    Ok(None) => Err(string_to_io_error(format!("Could not find mapping for target {target_name} and id {virtual_id}"))),
                    Err(err) => Err(string_to_io_error(format!("Query failed for id {virtual_id}: {err}"))),
                }
            })
            .await
            .map_err(|err| string_to_io_error(format!("Mapping query task failed for id {virtual_id}: {err}")))??;
            match mapping.item_type {
                PlaylistItemType::SeriesInfo
                | PlaylistItemType::LocalSeriesInfo => {
                    xtream_read_series_item_for_stream_id(app_config, virtual_id, &storage_path).await
                }
                PlaylistItemType::Series
                | PlaylistItemType::LocalSeries => {
                    log::debug!("Disk series item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}", virtual_id, mapping.parent_virtual_id, mapping.provider_id);
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
                    log::debug!("Disk catchup item requested. VirtualID: {}, ParentVirtualID: {}, MappingProviderID: {}", virtual_id, mapping.parent_virtual_id, mapping.provider_id);
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

pub async fn iter_raw_xtream_target_playlist(app_config: &AppConfig, target: &ConfigTarget, cluster: XtreamCluster) -> Option<Box<dyn Stream<Item = XtreamPlaylistItem> + Send + Unpin>> {
    let config = app_config.config.load();
    let storage_path = xtream_get_storage_path(&config, target.name.as_str())?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);
    iter_raw_xtream_playlist(app_config, &xtream_path).await
}

pub async fn iter_raw_xtream_input_playlist(app_config: &AppConfig, input: &ConfigInput, cluster: XtreamCluster) -> Option<Box<dyn Stream<Item = XtreamPlaylistItem> + Send + Unpin>> {
    let config = app_config.config.load();
    let working_dir = &config.working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir).await.ok()?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);

    iter_raw_xtream_playlist(app_config, &xtream_path).await
}

async fn iter_raw_xtream_playlist(app_config: &AppConfig, xtream_path: &Path) -> Option<Box<dyn Stream<Item = XtreamPlaylistItem> + Send + Unpin>> {
    if !file_exists_async(xtream_path).await {
        return None;
    }
    let bg_lock = app_config.file_locks.read_lock(xtream_path).await;

    let xtream_path = xtream_path.to_path_buf();
    let index_path = get_file_path_for_db_index(&xtream_path);
    let (tx, rx) = mpsc::channel::<XtreamPlaylistItem>(256);

    let xtream_path_for_log = xtream_path.clone();
    let index_path_for_log = index_path.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let _guard = bg_lock;
        let reader = match open_playlist_reader::<u32, XtreamPlaylistItem, u32>(
            &xtream_path,
            &index_path,
            None,
        ) {
            Ok(reader) => reader,
            Err(err) => {
                error!(
                    "Failed to open Xtream playlist reader {} (index {}): {err}",
                    xtream_path.display(),
                    index_path.display()
                );
                drop(tx);
                return;
            }
        };

        for entry in reader {
            let item = match entry {
                Ok((_, item)) => item,
                Err(err) => {
                    error!("Xtream playlist reader error: {err}");
                    continue;
                }
            };
            if tx.blocking_send(item).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        if let Err(err) = handle.await {
            error!(
                "Xtream playlist producer task failed for {} (index {}): {err}",
                xtream_path_for_log.display(),
                index_path_for_log.display()
            );
        }
    });

    let stream: Box<dyn Stream<Item = XtreamPlaylistItem> + Send + Unpin> =
        Box::new(ReceiverStream::new(rx));
    Some(stream)
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

const BATCH_SIZE: usize = 1000;

fn preserve_details_input_xtream_playlist_cluster_to_disk(
    old_path: &Path,
    tmp_path: &Path,
) -> Result<(), TuliproxError> {

   let Ok(mut old_tree) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(old_path) else {
       return Ok(())
   };

    let Ok(mut new_tree) = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(tmp_path) else {
        return Ok(())
    };

    let mut updates: Vec<(u32, XtreamPlaylistItem)> = Vec::with_capacity(BATCH_SIZE);
    for (_, old_item) in old_tree.iter() {
        if let Some(old_props) = old_item.additional_properties.as_ref() {
            if old_props.has_details() {
                if let Ok(Some(mut new_item)) = new_tree.query(&old_item.provider_id) {
                    if let Some(new_props) = new_item.additional_properties.as_mut() {
                        if needs_preserved_stream_property_merge(new_props, old_props)
                            && merge_preserved_stream_properties(new_props, old_props) {
                            updates.push((new_item.provider_id, new_item));
                            if updates.len() >= BATCH_SIZE {
                                let refs: Vec<(&u32, &XtreamPlaylistItem)> = updates.iter().map(|(id, pli)| (id, pli)).collect();
                                new_tree.update_batch(&refs).map_err(|e| notify_err!("Failed to update tmp tree during merge: {e}"))?;
                                updates.clear();
                            }
                        }
                    }
                }
            }
        }
    }

    if !updates.is_empty() {
        let refs: Vec<(&u32, &XtreamPlaylistItem)> =
            updates.iter().map(|(id, pli)| (id, pli)).collect();
        new_tree.update_batch(&refs)
            .map_err(|e| notify_err!("Failed to update tmp tree during merge: {e}"))?;
    }

    new_tree.commit()
        .map_err(|e| notify_err!("Failed to commit tmp tree merge: {e}"))?;

    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn persist_input_xtream_playlist_cluster_to_disk(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    cluster: XtreamCluster,
    categories: DynReader,
    streams: DynReader,
) -> Result<(), TuliproxError> {
    let cfg = app_config.config.load();
    let storage_path = ensure_input_storage_path(&cfg, &input.name).await?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);

    // Channel for transferring items from Parser (Async Task) to Consumer (Blocking Task)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<XtreamPlaylistItem>(BATCH_SIZE * 2);
    let input_clone = input.clone();

    // 1. Parser Task: Runs the async parsing logic
    // We move the readers into this task.
    let parse_task = tokio::spawn(async move {
        let tx_for_closure = tx.clone();
        let res = xtream::parse_xtream_streaming(
            &input_clone,
            cluster,
            categories,
            streams,
            move |item| {
                // Copy needed data before moving the item into the channel.
                let item_id = item.virtual_id;

                // We use blocking_send because the closure provided by the parser library is synchronous.
                // This is safe here because it runs within its own tokio::spawn task.
                if let Err(e) = tx_for_closure.blocking_send(item) {
                    error!("Channel closed while processing {cluster} for item {item_id}: {e}");
                    return notify_err_res!("Channel closed while processing {cluster}");
                }
                Ok(())
            },
        )
            .await;

        // CRITICAL: Explicitly drop the sender to signal rx.blocking_recv() to stop.
        // This prevents the consumer from waiting forever if the parser fails.
        drop(tx);
        res
    });

    // 2. Consumer Task: Handles heavy Disk I/O (BPlusTree updates)
    let xtream_path_for_consumer = xtream_path.clone();
    let consumer_task = tokio::task::spawn_blocking(move || {
        let tmp_xtream_path = xtream_path_for_consumer.with_extension("tmp");

        BPlusTree::<u32, XtreamPlaylistItem>::new()
            .store(&tmp_xtream_path)
            .map_err(|e| {
                error!(
                    "Failed to initialize ghost BPlusTree at {}: {e}",
                    tmp_xtream_path.display()
                );
                notify_err!("Init tree error {e}")
            })?;

        let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> =
            BPlusTreeUpdate::try_new_with_backoff(&tmp_xtream_path).map_err(|e| {
                error!("Failed to open ghost tree at {}: {e}", tmp_xtream_path.display());
                notify_err!("Failed to open tree {e}")
            })?;
        tree.set_flush_policy(FlushPolicy::Batch);

        let mut buffer = Vec::with_capacity(BATCH_SIZE);

        // This loop exits when all 'tx' clones are dropped (signaling end of stream)
        while let Some(item) = rx.blocking_recv() {
            buffer.push(item);
            if buffer.len() >= BATCH_SIZE {
                let batch: Vec<(&u32, &XtreamPlaylistItem)> =
                    buffer.iter().map(|i| (&i.provider_id, i)).collect();
                let prepared = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch)
                    .map_err(|e| {
                        error!("Batch prepare failed for cluster {cluster}: {e}");
                        notify_err!("Prepare failed {e}")
                    })?;
                tree.upsert_batch_encoded(prepared).map_err(|e| {
                    error!("Batch upsert failed for cluster {cluster}: {e}");
                    notify_err!("Upsert failed {e}")
                })?;
                buffer.clear();
            }
        }

        // Final batch processing
        if !buffer.is_empty() {
            let batch: Vec<(&u32, &XtreamPlaylistItem)> =
                buffer.iter().map(|i| (&i.provider_id, i)).collect();
            let prepared = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch)
                .map_err(|e| {
                    error!("Final batch prepare failed for cluster {cluster}: {e}");
                    notify_err!("Prepare failed {e}")
                })?;
            tree.upsert_batch_encoded(prepared).map_err(|e| {
                error!("Final batch upsert failed for cluster {cluster}: {e}");
                notify_err!("Upsert failed {e}")
            })?;
        }

        tree.commit().map_err(|e| {
            error!("Commit failed for cluster {cluster}: {e}");
            notify_err!("Commit failed {e}")
        })?;
        Ok::<(), TuliproxError>(())
    });

    // 3. Robust Joining of both tasks
    // try_join! returns immediately if any task returns an error or panics.
    let (parse_res, consumer_res) = tokio::try_join!(parse_task, consumer_task)
        .map_err(|e| notify_err!("Task join error during cluster {cluster} update: {e}"))?;

    // Handle internal errors from the tasks
    let parsed_categories = parse_res?;
    consumer_res?;

    // --- Post-Processing & Atomic Swap ---

    let col_path = match cluster {
        XtreamCluster::Live => get_live_cat_collection_path(&storage_path),
        XtreamCluster::Video => get_vod_cat_collection_path(&storage_path),
        XtreamCluster::Series => get_series_cat_collection_path(&storage_path),
    };

    let tmp_col_path = col_path.with_extension("tmp");

    // Use the parsed_categories returned from the task, not the 'categories' reader
    save_xtream_categories_to_file(&tmp_col_path, &parsed_categories).await?;

    let tmp_xtream_path = xtream_path.with_extension("tmp");

    // Acquire file lock to ensure atomic operations
    let swap_lock = app_config.file_locks.write_lock(&xtream_path).await;

    {
        let old_path = xtream_path.clone();
        let new_tmp_path = tmp_xtream_path.clone();
        tokio::task::spawn_blocking(move || preserve_details_input_xtream_playlist_cluster_to_disk(&old_path, &new_tmp_path))
            .await
            .map_err(|e| notify_err!("Merge task join error during cluster {cluster} update: {e}"))??;
    }

    // Optional compaction to optimize the newly created database file.
    // Run on blocking pool because B+Tree lock backoff uses std::thread::sleep.
    let compact_path = tmp_xtream_path.clone();
    match tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        if let Ok(mut tree_update) = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(&compact_path) {
            tree_update.compact(&compact_path)?;
        }
        Ok(())
    })
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            error!(
                "Failed to compact temporary database for {cluster} at {:?}: {e}",
                tmp_xtream_path.display()
            );
        }
        Err(e) => {
            error!(
                "Compaction task join failed for {cluster} at {:?}: {e}",
                tmp_xtream_path.display()
            );
        }
    }

    // Atomic Swap: Replace old database with new one
    if let Err(e) = crate::utils::rename_or_copy(&tmp_xtream_path, &xtream_path, false) {
        error!(
            "Failed to swap xtream database for {cluster} (from {:?} to {:?}): {e}",
            tmp_xtream_path.display(),
            xtream_path.display()
        );
        return notify_err_res!("Failed to swap database: {e}");
    }

    // Atomic Swap: Replace old categories with new ones
    if let Err(e) = crate::utils::rename_or_copy(&tmp_col_path, &col_path, false) {
        error!(
            "Failed to swap xtream categories for {cluster} (from {:?} to {:?}): {e}",
            tmp_col_path.display(),
            col_path.display()
        );
        return notify_err_res!("Failed to swap categories: {e}");
    }

    // Cleanup: Remove temporary files if they still exist (defensive)
    let _ = tokio::fs::remove_file(&tmp_xtream_path).await;
    let _ = tokio::fs::remove_file(&tmp_col_path).await;

    // Explicitly release the lock (though RAII would handle it too)
    drop(swap_lock);

    log::debug!("Cluster {cluster} updated successfully.");
    Ok(())
}

async fn save_xtream_categories_to_file(
    col_path: &Path,
    categories: &[XtreamCategory],
) -> Result<(), TuliproxError> {
    let col_path_buf = col_path.to_path_buf();
    let cat_entries: Vec<CategoryEntry> = categories
        .iter()
        .map(|c| CategoryEntry {
            category_id: c.category_id,
            category_name: c.category_name.clone(),
            parent_id: 0,
        })
        .collect();

    tokio::task::spawn_blocking(move || {
        if let Ok(file) = File::create(&col_path_buf) {
            if let Err(e) = file.lock_exclusive() {
                log::warn!(
                    "Could not acquire exclusive lock for {}: {e}, proceeding without lock",
                    col_path_buf.display()
                );
            }
            serde_json::to_writer(&file, &cat_entries).map_err(|e| {
                error!("Failed to write categories to file {}: {e}", col_path_buf.display());
                notify_err!("Write failed: {e}")
            })?;
            let _ = file.unlock();
        } else {
            return notify_err_res!("Failed to create category file {}", col_path_buf.display());
        }
        Ok(())
    })
        .await
        .map_err(|e| notify_err!("Spawn error {e}"))?
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
        if file_exists_async(&xtream_path).await {
            let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
            let xtream_path = xtream_path.clone();
            let stored_entries = match tokio::task::spawn_blocking(move || {
                let _guard = file_lock;
                let mut entries = IndexMap::new();
                if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                    for (_, doc) in query.iter() {
                        entries.insert(doc.provider_id, doc);
                    }
                }
                entries
            })
            .await
            {
                Ok(entries) => Some(entries),
                Err(err) => {
                    errors.push(format!(
                        "Failed to load stored xtream playlist entries for {cluster}: {err}"
                    ));
                    None
                }
            };

            if let Some(entries) = stored_entries {
                *stored_scratch.get_mut(cluster) = entries;
            }
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
                            if needs_preserved_stream_property_merge(new_stream_props, &old_stream_props) {
                                merge_preserved_stream_properties(new_stream_props, &old_stream_props);
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
        if !data.is_empty() || !file_exists_async(&col_path).await {
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
pub(crate) fn needs_update_info_details(
    new_stream_props: &StreamProperties,
    old_stream_props: &StreamProperties,
) -> bool {

    let new_modified = new_stream_props.get_last_modified();
    let old_modified = old_stream_props.get_last_modified();

    match (new_modified, old_modified) {
        (Some(new_ts), Some(old_ts)) => new_ts > old_ts,
        (Some(_), None) => true,
        _ => false,
    }
}

fn needs_preserved_stream_property_merge(
    new_stream_props: &StreamProperties,
    old_stream_props: &StreamProperties,
) -> bool {
    let preserve_info_details = old_stream_props.has_details() && !needs_update_info_details(new_stream_props, old_stream_props);

    match (new_stream_props, old_stream_props) {
        (StreamProperties::Video(v_new), StreamProperties::Video(v_old))
            if preserve_info_details && v_old.details.is_some() =>
        {
            v_new.details != v_old.details
        }
        (StreamProperties::Series(s_new), StreamProperties::Series(s_old))
            if preserve_info_details && s_old.details.is_some() =>
        {
            s_new.details != s_old.details
        }
        (StreamProperties::Live(l_new), StreamProperties::Live(l_old)) => {
            (l_new.video.is_none() && l_old.video.is_some())
                || (l_new.audio.is_none() && l_old.audio.is_some())
                || (l_new.last_probed_timestamp.is_none() && l_old.last_probed_timestamp.is_some())
                || (l_new.last_success_timestamp.is_none() && l_old.last_success_timestamp.is_some())
        }
        _ => false,
    }
}

/// Merges persisted fields from old stream properties into freshly fetched properties.
///
/// This keeps long-lived metadata stable across full playlist rewrites:
/// - VOD/Series `details` are preserved when incoming provider metadata is not newer.
/// - Live probe fields (`video`, `audio`, `last_probed_timestamp`, `last_success_timestamp`)
///   are copied only when missing in the incoming payload.
pub(crate) fn merge_preserved_stream_properties(
    new_stream_props: &mut StreamProperties,
    old_stream_props: &StreamProperties,
) -> bool {
    let preserve_info_details =
        old_stream_props.has_details() && !needs_update_info_details(new_stream_props, old_stream_props);

    match (new_stream_props, old_stream_props) {
        (StreamProperties::Video(v_new), StreamProperties::Video(v_old))
        if preserve_info_details && v_old.details.is_some() =>
            {
                if v_new.details == v_old.details {
                    false
                } else {
                    v_new.details.clone_from(&v_old.details);
                    true
                }
            }
        (StreamProperties::Series(s_new), StreamProperties::Series(s_old))
        if preserve_info_details && s_old.details.is_some() =>
            {
                if s_new.details == s_old.details {
                    false
                } else {
                    s_new.details.clone_from(&s_old.details);
                    true
                }
            }
        (StreamProperties::Live(l_new), StreamProperties::Live(l_old)) => {
            let mut changed = false;

            if l_new.video.is_none() && l_old.video.is_some() {
                l_new.video.clone_from(&l_old.video);
                changed = true;
            }

            if l_new.audio.is_none() && l_old.audio.is_some() {
                l_new.audio.clone_from(&l_old.audio);
                changed = true;
            }

            if l_new.last_probed_timestamp.is_none() && l_old.last_probed_timestamp.is_some() {
                l_new.last_probed_timestamp = l_old.last_probed_timestamp;
                changed = true;
            }

            if l_new.last_success_timestamp.is_none() && l_old.last_success_timestamp.is_some() {
                l_new.last_success_timestamp = l_old.last_success_timestamp;
                changed = true;
            }

            changed
        }
        _ => false,
    }
}

async fn persist_input_info(app_config: &Arc<AppConfig>, storage_path: &Path, cluster: XtreamCluster,
                            input_name: &str, provider_id: u32, props: StreamProperties) -> Result<(), Error> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    if xtream_path.exists() {
        let file_lock = app_config.file_locks.write_lock(&xtream_path).await;
        let xtream_path_clone = xtream_path.clone();
        let input_name_owned = input_name.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let _guard = file_lock;
            let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> = BPlusTreeUpdate::try_new_with_backoff(&xtream_path_clone)
                .map_err(|err| Error::other(format!("failed to open BPlusTree for input {input_name_owned}: {err}")))?;
            match tree.query(&provider_id) {
                Ok(Some(mut pli)) => {
                    pli.additional_properties = Some(props);
                    tree.update(&provider_id, pli).map_err(|err| Error::other(format!("failed to write {cluster} info for input {input_name_owned}: {err}")))?;
                }
                Ok(None) => {
                    error!("Could not find input entry for provider_id: {provider_id} and input: {input_name_owned}");
                }
                Err(err) => {
                    error!("Failed to query BPlusTree for provider_id: {provider_id} and input: {input_name_owned}: {err}");
                }
            }
            Ok(())
        }).await.map_err(|err| Error::other(format!("failed to join blocking input info persist for {input_name}: {err}")))??;
    }
    Ok(())
}

pub async fn persist_input_info_batch(app_config: &Arc<AppConfig>, storage_path: &Path, cluster: XtreamCluster,
                                      input_name: &str, updates: Vec<(u32, StreamProperties)>) -> Result<(), Error> {
    if updates.is_empty() { return Ok(()); }
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    if xtream_path.exists() {
        let file_lock = app_config.file_locks.write_lock(&xtream_path).await;
        let xtream_path_clone = xtream_path.clone();
        let input_name_owned = input_name.to_string();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let _guard = file_lock;
            let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> = BPlusTreeUpdate::try_new_with_backoff(&xtream_path_clone)
                .map_err(|err| Error::other(format!("failed to open BPlusTree for input {input_name_owned}: {err}")))?;

            // Keep only the latest update per provider id to avoid duplicate reads/writes.
            let mut deduped_updates: HashMap<u32, StreamProperties> = HashMap::with_capacity(updates.len());
            for (provider_id, props) in updates {
                deduped_updates.insert(provider_id, props);
            }

            let mut updated_plis = Vec::with_capacity(deduped_updates.len());
            for (provider_id, props) in deduped_updates {
                match tree.query(&provider_id) {
                    Ok(Some(mut pli)) => {
                        pli.additional_properties = Some(props);
                        updated_plis.push((provider_id, pli));
                    }
                    Ok(None) => {
                        error!("Could not find input entry for provider_id: {provider_id} and input: {input_name_owned}");
                    }
                    Err(err) => {
                        error!("Failed to query BPlusTree for provider_id: {provider_id} and input: {input_name_owned}: {err}");
                    }
                }
            }

            if !updated_plis.is_empty() {
                let refs: Vec<(&u32, &XtreamPlaylistItem)> = updated_plis.iter()
                    .map(|(id, pli)| (id, pli))
                    .collect();
                tree.update_batch(&refs).map_err(|err| Error::other(format!("failed to write batch {cluster} info for input {input_name_owned}: {err}")))?;
            }
            Ok(())
        }).await.map_err(|err| Error::other(format!("failed to join blocking input info batch persist for {input_name}: {err}")))??;
    }
    Ok(())
}


pub async fn persist_input_vod_info(app_config: &Arc<AppConfig>, storage_path: &Path,
                                    cluster: XtreamCluster, input_name: &str, provider_id: u32,
                                    props: &VideoStreamProperties) -> Result<(), Error> {
    persist_input_info(app_config, storage_path, cluster, input_name, provider_id, StreamProperties::Video(Box::new(props.clone()))).await
}

pub async fn persist_input_live_info(app_config: &Arc<AppConfig>, storage_path: &Path,
                                     cluster: XtreamCluster, input_name: &str, provider_id: u32,
                                     props: &LiveStreamProperties) -> Result<(), Error> {
    persist_input_info(app_config, storage_path, cluster, input_name, provider_id, StreamProperties::Live(Box::new(props.clone()))).await
}

pub async fn persist_input_live_info_batch(app_config: &Arc<AppConfig>, storage_path: &Path,
                                           cluster: XtreamCluster, input_name: &str,
                                           updates: Vec<(u32, LiveStreamProperties)>) -> Result<(), Error> {
    let batch = updates.into_iter()
        .map(|(id, props)| (id, StreamProperties::Live(Box::new(props))))
        .collect();
    persist_input_info_batch(app_config, storage_path, cluster, input_name, batch).await
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
            let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
            let xtream_display = xtream_path.display().to_string();
            let xtream_path = xtream_path.clone();
            let items = tokio::task::spawn_blocking(move || -> Result<Vec<XtreamPlaylistItem>, TuliproxError> {
                let _guard = file_lock;
                let mut items = Vec::new();
                if let Ok(mut query) = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path) {
                    for (_, item) in query.iter() {
                        items.push(item);
                    }
                }
                Ok(items)
            })
            .await
            .map_err(|err| notify_err!("failed to read xtream playlist: {} - {err}", xtream_display))??;

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

#[cfg(test)]
mod tests {
    use super::{merge_preserved_stream_properties, needs_update_info_details, preserve_details_input_xtream_playlist_cluster_to_disk};
    use crate::repository::{BPlusTreeQuery, BPlusTreeUpdate};
    use shared::model::{
        LiveStreamProperties, SeriesStreamProperties, StreamProperties, VideoStreamProperties,
        XtreamCluster, XtreamPlaylistItem,
    };
    use shared::utils::Internable;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn keeps_existing_details_when_new_timestamp_is_missing() {
        let new_props = StreamProperties::Video(Box::new(VideoStreamProperties {
            added: "".into(),
            ..VideoStreamProperties::default()
        }));
        let old_props = StreamProperties::Video(Box::new(VideoStreamProperties {
            added: "1700000000".into(),
            ..VideoStreamProperties::default()
        }));

        assert!(!needs_update_info_details(&new_props, &old_props));
    }

    #[test]
    fn updates_details_when_new_timestamp_is_newer() {
        let new_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
            last_modified: Some("200".into()),
            ..SeriesStreamProperties::default()
        }));
        let old_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
            last_modified: Some("100".into()),
            ..SeriesStreamProperties::default()
        }));

        assert!(needs_update_info_details(&new_props, &old_props));
    }

    #[test]
    fn does_not_update_details_when_new_timestamp_is_older() {
        let new_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
            last_modified: Some("100".into()),
            ..SeriesStreamProperties::default()
        }));
        let old_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
            last_modified: Some("200".into()),
            ..SeriesStreamProperties::default()
        }));

        assert!(!needs_update_info_details(&new_props, &old_props));
    }

    #[test]
    fn merge_preserves_missing_live_probe_timestamps() {
        let mut new_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            ..LiveStreamProperties::default()
        }));
        let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            last_probed_timestamp: Some(1_700_000_000),
            last_success_timestamp: Some(1_700_000_100),
            ..LiveStreamProperties::default()
        }));

        let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
        assert!(changed);

        match new_props {
            StreamProperties::Live(live) => {
                assert_eq!(live.last_probed_timestamp, Some(1_700_000_000));
                assert_eq!(live.last_success_timestamp, Some(1_700_000_100));
            }
            _ => panic!("expected live properties"),
        }
    }

    #[test]
    fn merge_does_not_override_existing_live_probe_timestamps() {
        let mut new_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            last_probed_timestamp: Some(1_800_000_000),
            last_success_timestamp: Some(1_800_000_100),
            ..LiveStreamProperties::default()
        }));
        let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            last_probed_timestamp: Some(1_700_000_000),
            last_success_timestamp: Some(1_700_000_100),
            ..LiveStreamProperties::default()
        }));

        let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
        assert!(!changed);

        match new_props {
            StreamProperties::Live(live) => {
                assert_eq!(live.last_probed_timestamp, Some(1_800_000_000));
                assert_eq!(live.last_success_timestamp, Some(1_800_000_100));
            }
            _ => panic!("expected live properties"),
        }
    }

    fn make_live_item(
        provider_id: u32,
        video: Option<&str>,
        audio: Option<&str>,
        last_probed_timestamp: Option<i64>,
        last_success_timestamp: Option<i64>,
    ) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: provider_id,
            provider_id,
            name: "live".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "group".intern(),
            title: "".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://example.com/live.ts".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Live,
            additional_properties: Some(StreamProperties::Live(Box::new(LiveStreamProperties {
                video: video.map(Internable::intern),
                audio: audio.map(Internable::intern),
                last_probed_timestamp,
                last_success_timestamp,
                ..Default::default()
            }))),
            item_type: shared::model::PlaylistItemType::Live,
            category_id: 1,
            input_name: "input_a".intern(),
            channel_no: 0,
            source_ordinal: 0,
        }
    }

    fn write_single_item(path: &Path, item: &XtreamPlaylistItem) {
        crate::repository::BPlusTree::<u32, XtreamPlaylistItem>::new()
            .store(path)
            .expect("tree creation should succeed");
        let mut tree = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(path)
            .expect("tree open should succeed");
        let batch: Vec<(&u32, &XtreamPlaylistItem)> = vec![(&item.provider_id, item)];
        let prepared = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch)
            .expect("batch preparation should succeed");
        tree.upsert_batch_encoded(prepared)
            .expect("batch upsert should succeed");
        tree.commit().expect("tree commit should succeed");
    }

    fn read_live_props(path: &Path, provider_id: u32) -> LiveStreamProperties {
        let mut query =
            BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(path).expect("query open should succeed");
        let item = query
            .query_zero_copy(&provider_id)
            .expect("query should succeed")
            .expect("item should exist");
        match item.additional_properties {
            Some(StreamProperties::Live(live)) => *live,
            other => panic!("expected live stream properties, got {other:?}"),
        }
    }

    #[test]
    fn preserve_details_for_disk_cluster_copies_missing_live_probe_fields() {
        let dir = tempdir().expect("temp dir should be created");
        let old_path = dir.path().join("old_live.db");
        let tmp_path = dir.path().join("tmp_live.db");
        let provider_id = 100_u32;

        write_single_item(
            &old_path,
            &make_live_item(
                provider_id,
                Some("{\"codec_name\":\"h264\"}"),
                Some("{\"codec_name\":\"aac\"}"),
                Some(1_700_000_000),
                Some(1_700_000_100),
            ),
        );
        write_single_item(&tmp_path, &make_live_item(provider_id, None, None, None, None));

        preserve_details_input_xtream_playlist_cluster_to_disk(&old_path, &tmp_path).expect("merge should succeed");

        let merged = read_live_props(&tmp_path, provider_id);
        assert_eq!(merged.video, Some("{\"codec_name\":\"h264\"}".intern()));
        assert_eq!(merged.audio, Some("{\"codec_name\":\"aac\"}".intern()));
        assert_eq!(merged.last_probed_timestamp, Some(1_700_000_000));
        assert_eq!(merged.last_success_timestamp, Some(1_700_000_100));
    }

    #[test]
    fn preserve_details_for_disk_cluster_does_not_override_existing_live_probe_fields() {
        let dir = tempdir().expect("temp dir should be created");
        let old_path = dir.path().join("old_live_existing.db");
        let tmp_path = dir.path().join("tmp_live_existing.db");
        let provider_id = 200_u32;

        write_single_item(
            &old_path,
            &make_live_item(
                provider_id,
                Some("{\"codec_name\":\"h264\"}"),
                Some("{\"codec_name\":\"aac\"}"),
                Some(1_700_000_000),
                Some(1_700_000_100),
            ),
        );
        write_single_item(
            &tmp_path,
            &make_live_item(
                provider_id,
                Some("{\"codec_name\":\"hevc\"}"),
                Some("{\"codec_name\":\"ac3\"}"),
                Some(1_800_000_000),
                Some(1_800_000_100),
            ),
        );

        preserve_details_input_xtream_playlist_cluster_to_disk(&old_path, &tmp_path).expect("merge should succeed");

        let merged = read_live_props(&tmp_path, provider_id);
        assert_eq!(merged.video, Some("{\"codec_name\":\"hevc\"}".intern()));
        assert_eq!(merged.audio, Some("{\"codec_name\":\"ac3\"}".intern()));
        assert_eq!(merged.last_probed_timestamp, Some(1_800_000_000));
        assert_eq!(merged.last_success_timestamp, Some(1_800_000_100));
    }

    #[test]
    fn preserve_details_for_disk_cluster_fills_only_missing_live_probe_fields() {
        let dir = tempdir().expect("temp dir should be created");
        let old_path = dir.path().join("old_live_partial.db");
        let tmp_path = dir.path().join("tmp_live_partial.db");
        let provider_id = 300_u32;

        write_single_item(
            &old_path,
            &make_live_item(
                provider_id,
                Some("{\"codec_name\":\"h264\"}"),
                Some("{\"codec_name\":\"aac\"}"),
                Some(1_700_000_000),
                Some(1_700_000_100),
            ),
        );
        write_single_item(
            &tmp_path,
            &make_live_item(
                provider_id,
                Some("{\"codec_name\":\"hevc\"}"),
                None,
                Some(1_800_000_000),
                None,
            ),
        );

        preserve_details_input_xtream_playlist_cluster_to_disk(&old_path, &tmp_path).expect("merge should succeed");

        let merged = read_live_props(&tmp_path, provider_id);
        assert_eq!(merged.video, Some("{\"codec_name\":\"hevc\"}".intern()));
        assert_eq!(merged.audio, Some("{\"codec_name\":\"aac\"}".intern()));
        assert_eq!(merged.last_probed_timestamp, Some(1_800_000_000));
        assert_eq!(merged.last_success_timestamp, Some(1_700_000_100));
    }
}
