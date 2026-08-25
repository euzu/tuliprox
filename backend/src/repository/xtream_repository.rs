use crate::api::model::PlaylistStorageState;
use crate::model::XtreamCategory;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{Config, ConfigTarget};
use crate::model::{ConfigInput, PlaylistXtreamCategory};
use crate::processing::parser::xtream;
use crate::repository::bplustree::{
    ensure_distinct_sidecar_lock_domains, publish_staged_database, BPlusTree, BPlusTreeError,
    BPlusTreeQuery, BPlusTreeStagingArtifacts, BPlusTreeUpdate, FlushPolicy,
};
use crate::utils::{parent_or_dot, remove_file_if_exists, require_same_parent_directory};
use crate::repository::open_playlist_reader;
use crate::repository::playlist_scratch::PlaylistScratch;
use crate::repository::storage::{
    ensure_input_storage_path, ensure_target_storage_subpath, get_file_path_for_db_index,
    get_input_storage_path, get_target_id_mapping_file, get_target_storage_path,
    XtreamRefreshGenerationGuard,
};
use crate::repository::storage_const;
use crate::repository::target_id_mapping::VirtualIdRecord;
use crate::repository::xtream_playlist_iterator::XtreamPlaylistJsonIterator;
use crate::utils::json_write_documents_to_file;
use crate::utils::request::DynReader;
use crate::utils::FileReadGuard;
use crate::utils::{file_exists_async, file_reader};
use bytes::Bytes;
use fs2::FileExt;
use futures::{stream, Stream, StreamExt};
use indexmap::IndexMap;
use log::error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use shared::error::{string_to_io_error, TuliproxError};
use shared::model::xtream_const::XTREAM_CLUSTER;
use shared::model::{LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemType, SeriesStreamProperties, StreamProperties, VideoStreamProperties, XtreamCluster, XtreamPlaylistItem};
use shared::utils::{arc_str_serde, get_u32_from_serde_value, Internable};
use shared::concat_string;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

macro_rules! cant_write_result {
    ($path:expr, $err:expr) => {
        TuliproxError::RepositoryXtream(format!(
            "failed to write xtream playlist: {} - {}",
            $path.display(),
            $err
        ))
    };
}


#[inline]
pub fn get_collection_path(path: &Path, collection: &str) -> PathBuf {
    path.join(format!("{collection}.json"))
}

/// Returns the category-collection base name for an [`XtreamCluster`].
///
/// Centralizes the per-cluster `cat_live` / `cat_vod` / `cat_series` mapping so the
/// path-deriving call sites read a single property instead of re-matching the cluster.
#[inline]
pub(crate) const fn xtream_cluster_category_collection(cluster: XtreamCluster) -> &'static str {
    match cluster {
        XtreamCluster::Live => storage_const::COL_CAT_LIVE,
        XtreamCluster::Video => storage_const::COL_CAT_VOD,
        XtreamCluster::Series => storage_const::COL_CAT_SERIES,
    }
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
    ensure_target_storage_subpath(
        cfg,
        target_name,
        "xtream",
        xtream_get_storage_path,
        TuliproxError::RepositoryXtream,
    )
    .await
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
            .map_err(|e| TuliproxError::RepositoryXtream(format!("Blocking task failed: {e}")))?
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
        return Err(TuliproxError::RepositoryXtream(format!("BPlusTree file not found for update {}", xtream_path.display())));
    }

    // Prepare encoded payload before opening the writer lock.
    let prepared_items = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&[(&pli.virtual_id, pli)])
        .map_err(|e| TuliproxError::RepositoryXtream(format!("Failed to serialize value: {e}")))?;

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
        .map_err(|e| TuliproxError::RepositoryXtream(format!("Blocking task failed: {e}")))?
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
        return Err(TuliproxError::RepositoryXtream(format!("BPlusTree file not found for upsert {}", xtream_path.display())));
    }

    // Prepare encoded payload before opening the writer lock.
    let batch_refs: Vec<(&u32, &XtreamPlaylistItem)> = pli_list
        .iter()
        .map(|pli| (&pli.virtual_id, pli))
        .collect();
    let prepared_items = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch_refs)
        .map_err(|e| TuliproxError::RepositoryXtream(format!("Failed to serialize value: {e}")))?;

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
        .map_err(|e| TuliproxError::RepositoryXtream(format!("Blocking task failed: {e}")))?
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
        return Err(TuliproxError::Config(errors.join("\n")));
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
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    xtream_cluster: Option<XtreamCluster>,
) -> Result<Option<(XtreamPlaylistItem, VirtualIdRecord)>, Error> {
    if let Some(playlist) = playlists.data.read().await.get(target.name.as_str()) {
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

                        if let Some(item) = xtream_storage.series.query(&virtual_id) {
                            Ok(item.clone())
                        } else if let Some(item) = xtream_storage.series.query(&mapping.parent_virtual_id) {
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
    app_config: &Arc<AppConfig>,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
    xtream_cluster: Option<XtreamCluster>,
) -> Result<XtreamPlaylistItem, Error> {
    if target.use_memory_cache {
        if let Ok(Some((playlist_item, _virtual_record))) =
            xtream_get_item_for_stream_id_from_memory(virtual_id, playlists, target, xtream_cluster).await {
            return Ok(playlist_item);
        }
        // fall through to disk lookup on cache miss
    }

    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str()).ok_or_else(|| string_to_io_error(format!("Could not find path for target {}", target.name)))?;
    let storage_path = xtream_get_storage_path(&config, target.name.as_str()).ok_or_else(|| string_to_io_error(format!("Could not find path for target {} xtream output", target.name)))?;
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

                    if let Ok(episode) = xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, XtreamCluster::Series).await {
                        return Ok(episode);
                    }

                    if let Ok(mut item) = xtream_read_series_item_for_stream_id(app_config, mapping.parent_virtual_id, &storage_path).await {
                        item.provider_id = mapping.provider_id;
                        item.item_type = PlaylistItemType::Series;
                        item.virtual_id = mapping.virtual_id;
                        return Ok(item);
                    }

                    return Err(Error::other(format!("Failed to find episode item with virtual-id {virtual_id}")));
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
    app_config: &Arc<AppConfig>,
    target: &ConfigTarget,
    category_id: Option<u32>,
    user: &ProxyUserCredentials,
) -> Result<XtreamPlaylistJsonIterator, TuliproxError> {
    XtreamPlaylistJsonIterator::new(cluster, app_config, target, category_id, user).await
}

pub async fn iter_raw_xtream_target_playlist(app_config: &AppConfig, target: &ConfigTarget, cluster: XtreamCluster) -> Option<Box<dyn Stream<Item=Result<XtreamPlaylistItem, TuliproxError>> + Send + Unpin>> {
    let config = app_config.config.load();
    let storage_path = xtream_get_storage_path(&config, target.name.as_str())?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);
    iter_raw_xtream_playlist(app_config, &xtream_path).await
}

pub async fn iter_raw_xtream_input_playlist(app_config: &AppConfig, input: &ConfigInput, cluster: XtreamCluster) -> Option<Box<dyn Stream<Item=Result<XtreamPlaylistItem, TuliproxError>> + Send + Unpin>> {
    let config = app_config.config.load();
    let storage_dir = &config.storage_dir;
    let storage_path = get_input_storage_path(&input.name, storage_dir).await.ok()?;
    let xtream_path = xtream_get_file_path(&storage_path, cluster);

    iter_raw_xtream_playlist(app_config, &xtream_path).await
}

async fn iter_raw_xtream_playlist(app_config: &AppConfig, xtream_path: &Path) -> Option<Box<dyn Stream<Item=Result<XtreamPlaylistItem, TuliproxError>> + Send + Unpin>> {
    if !file_exists_async(xtream_path).await {
        return None;
    }
    let bg_lock = app_config.file_locks.read_lock(xtream_path).await;

    let xtream_path = xtream_path.to_path_buf();
    let index_path = get_file_path_for_db_index(&xtream_path);
    let (tx, rx) = mpsc::channel::<Result<XtreamPlaylistItem, TuliproxError>>(256);

    let xtream_path_for_log = xtream_path.clone();
    let index_path_for_log = index_path.clone();
    let join_error_tx = tx.clone();
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
                let _ = tx.blocking_send(Err(err));
                return;
            }
        };

        for entry in reader {
            let item = match entry {
                Ok((_, item)) => item,
                Err(err) => {
                    error!("Skipping unreadable Xtream playlist entry: {err}");
                    continue;
                }
            };
            if tx.blocking_send(Ok(item)).is_err() {
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
            let _ = join_error_tx
                .send(Err(TuliproxError::RepositoryXtream(format!(
                    "Xtream playlist producer task failed for {}: {err}",
                    xtream_path_for_log.display()
                ))))
                .await;
        }
    });

    let stream: Box<dyn Stream<Item=Result<XtreamPlaylistItem, TuliproxError>> + Send + Unpin> =
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
    let path = xtream_get_collection_path(config, target_name, xtream_cluster_category_collection(cluster));
    if let Ok(file_path) = path {
        if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
            return serde_json::from_str::<Vec<PlaylistXtreamCategory>>(&content).ok();
        }
    }
    None
}

const BATCH_SIZE: usize = 1000;

#[derive(Debug, Clone, Eq, PartialEq)]
struct XtreamRefreshPaths {
    generation: Uuid,
    published_database: PathBuf,
    staging_database: PathBuf,
    published_categories: PathBuf,
    staging_categories: PathBuf,
}

impl XtreamRefreshPaths {
    fn new(storage_path: &Path, cluster: XtreamCluster) -> Result<Self, TuliproxError> {
        Self::for_generation(storage_path, cluster, Uuid::new_v4())
    }

    fn for_generation(
        storage_path: &Path,
        cluster: XtreamCluster,
        generation: Uuid,
    ) -> Result<Self, TuliproxError> {
        let published_database = xtream_get_file_path(storage_path, cluster);
        let published_categories = get_collection_path(storage_path, xtream_cluster_category_collection(cluster));
        let staging_database = refresh_staging_path(&published_database, generation)?;
        // The lock-domain check is repeated by `XtreamRefreshLease::new` with a stricter
        // aliasing scan; doing it here too would canonicalize the same paths twice.
        Ok(Self {
            generation,
            staging_database,
            staging_categories: refresh_staging_path(&published_categories, generation)?,
            published_database,
            published_categories,
        })
    }
}

fn refresh_staging_path(path: &Path, generation: Uuid) -> Result<PathBuf, TuliproxError> {
    let stem = path.file_stem().ok_or_else(|| {
        TuliproxError::RepositoryXtream(format!("Refresh path has no file stem: {}", path.display()))
    })?;
    let extension = path.extension().ok_or_else(|| {
        TuliproxError::RepositoryXtream(format!("Refresh path has no extension: {}", path.display()))
    })?;
    let mut filename = OsString::from(stem);
    filename.push(".refresh-");
    filename.push(generation.simple().to_string());
    filename.push(".");
    filename.push(extension);
    Ok(path.with_file_name(filename))
}

#[derive(Debug, Clone)]
struct XtreamRefreshLease(Arc<XtreamRefreshLeaseInner>);

#[derive(Debug)]
struct XtreamRefreshLeaseInner {
    paths: XtreamRefreshPaths,
    database_artifacts: BPlusTreeStagingArtifacts,
    _generation_guard: XtreamRefreshGenerationGuard,
}

impl XtreamRefreshLease {
    fn new(paths: XtreamRefreshPaths) -> Result<Self, TuliproxError> {
        let database_artifacts = BPlusTreeStagingArtifacts::new(
            &paths.published_database,
            &paths.staging_database,
        )
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Invalid Xtream staging artifacts for generation {}: {error}",
                paths.generation
            ))
        })?;
        let storage_path = paths.staging_database.parent().ok_or_else(|| {
            TuliproxError::RepositoryXtream(format!(
                "Xtream staging database has no storage directory: {}",
                paths.staging_database.display()
            ))
        })?;
        let generation_guard = XtreamRefreshGenerationGuard::acquire(storage_path, paths.generation).map_err(
            |error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to acquire Xtream refresh generation guard {} in {}: {error}",
                    paths.generation,
                    storage_path.display()
                ))
            },
        )?;
        Ok(Self(Arc::new(XtreamRefreshLeaseInner {
            paths,
            database_artifacts,
            _generation_guard: generation_guard,
        })))
    }

    fn paths(&self) -> &XtreamRefreshPaths { &self.0.paths }

    fn cleanup_staging_artifacts(&self) -> io::Result<()> {
        let database_result = self.0.database_artifacts.remove_owned_staging_artifacts();
        let categories_result = remove_file_if_exists(&self.0.paths.staging_categories);
        match (database_result, categories_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(database_error), Err(categories_error)) => Err(io::Error::new(
                database_error.kind(),
                format!("{database_error}; category staging cleanup also failed: {categories_error}"),
            )),
        }
    }
}

impl Drop for XtreamRefreshLeaseInner {
    fn drop(&mut self) {
        let database_result = self.database_artifacts.remove_owned_staging_artifacts();
        let categories_result = remove_file_if_exists(&self.paths.staging_categories);
        if let Err(error) = database_result {
            log::warn!(
                "Failed to clean Xtream staging database artifacts for generation {}: {error}",
                self.paths.generation
            );
        }
        if let Err(error) = categories_result {
            log::warn!(
                "Failed to clean Xtream staging categories for generation {} at {}: {error}",
                self.paths.generation,
                self.paths.staging_categories.display()
            );
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PreserveDetailsOutcome {
    SourceMissing,
    Merged { scanned: usize, updated: usize },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum DetailPreservationOperation {
    Query,
    BatchWrite,
    Commit,
}

fn write_preserved_detail_batch<F>(
    staging_tree: &mut BPlusTreeUpdate<u32, XtreamPlaylistItem>,
    staging_path: &Path,
    updates: &mut Vec<(u32, XtreamPlaylistItem)>,
    before_operation: &mut F,
) -> Result<usize, TuliproxError>
where
    F: FnMut(DetailPreservationOperation) -> io::Result<()>,
{
    if updates.is_empty() {
        return Ok(0);
    }
    let batch_len = updates.len();
    let refs: Vec<(&u32, &XtreamPlaylistItem)> = updates.iter().map(|(id, item)| (id, item)).collect();
    before_operation(DetailPreservationOperation::BatchWrite)
        .and_then(|()| staging_tree.update_batch(&refs).map(|_| ()).map_err(BPlusTreeError::to_io))
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to update staging Xtream tree {} during detail preservation: {error}",
                staging_path.display()
            ))
        })?;
    updates.clear();
    Ok(batch_len)
}

fn preserve_details_input_xtream_playlist_cluster_to_disk(
    published_path: &Path,
    staging_path: &Path,
) -> Result<PreserveDetailsOutcome, TuliproxError> {
    preserve_details_input_xtream_playlist_cluster_to_disk_with_hook(
        published_path,
        staging_path,
        |_| Ok(()),
    )
}

fn preserve_details_input_xtream_playlist_cluster_to_disk_with_hook<F>(
    published_path: &Path,
    staging_path: &Path,
    mut before_operation: F,
) -> Result<PreserveDetailsOutcome, TuliproxError>
where
    F: FnMut(DetailPreservationOperation) -> io::Result<()>,
{
    ensure_distinct_sidecar_lock_domains(published_path, staging_path)
        .map_err(|error| TuliproxError::RepositoryXtream(error.to_string()))?;

    let mut published_tree = match BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(published_path) {
        Ok(tree) => tree,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PreserveDetailsOutcome::SourceMissing);
        }
        Err(error) => {
            return Err(TuliproxError::RepositoryXtream(format!(
                "Failed to open published Xtream tree {} for detail preservation: {error}",
                published_path.display()
            )));
        }
    };

    let mut staging_tree = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(staging_path)
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to open staging Xtream tree {} for detail preservation: {error}",
                staging_path.display()
            ))
        })?;

    let mut pending_updates: Vec<(u32, XtreamPlaylistItem)> = Vec::with_capacity(BATCH_SIZE);
    let mut scanned_count = 0usize;
    let mut updated_count = 0usize;
    for entry in published_tree.iter() {
        let (_, old_item) = entry.map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to read published Xtream tree {} during detail preservation: {error}",
                published_path.display()
            ))
        })?;
        scanned_count = scanned_count.saturating_add(1);
        if let Some(old_props) = old_item.additional_properties.as_ref() {
            if old_props.has_details() {
                let staging_item = before_operation(DetailPreservationOperation::Query)
                    .and_then(|()| staging_tree.query(&old_item.provider_id).map_err(BPlusTreeError::to_io))
                    .map_err(|error| {
                        TuliproxError::RepositoryXtream(format!(
                            "Failed to query staging Xtream tree {} for provider {}: {error}",
                            staging_path.display(),
                            old_item.provider_id
                        ))
                    })?;
                if let Some(mut new_item) = staging_item {
                    if let Some(new_props) = new_item.additional_properties.as_mut() {
                        if merge_preserved_stream_properties(new_props, old_props) {
                            pending_updates.push((new_item.provider_id, new_item));
                            if pending_updates.len() >= BATCH_SIZE {
                                updated_count = updated_count.saturating_add(write_preserved_detail_batch(
                                    &mut staging_tree,
                                    staging_path,
                                    &mut pending_updates,
                                    &mut before_operation,
                                )?);
                            }
                        }
                    }
                }
            }
        }
    }

    updated_count = updated_count.saturating_add(write_preserved_detail_batch(
        &mut staging_tree,
        staging_path,
        &mut pending_updates,
        &mut before_operation,
    )?);
    before_operation(DetailPreservationOperation::Commit)
        .and_then(|()| staging_tree.commit())
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to commit staging Xtream tree {} after detail preservation: {error}",
                staging_path.display()
            ))
        })?;

    Ok(PreserveDetailsOutcome::Merged {
        scanned: scanned_count,
        updated: updated_count,
    })
}

#[cfg(test)]
fn preserve_details_with_injected_operation_failure(
    published_path: &Path,
    staging_path: &Path,
    failure: DetailPreservationOperation,
) -> Result<PreserveDetailsOutcome, TuliproxError> {
    preserve_details_input_xtream_playlist_cluster_to_disk_with_hook(
        published_path,
        staging_path,
        move |operation| {
            if operation == failure {
                Err(io::Error::other(format!("injected {failure:?} failure")))
            } else {
                Ok(())
            }
        },
    )
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
    drop(cfg);
    let refresh_lease = XtreamRefreshLease::new(XtreamRefreshPaths::new(&storage_path, cluster)?)?;

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
                    return Err(TuliproxError::RepositoryXtream(format!("Channel closed while processing {cluster}")));
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
    let consumer_lease = refresh_lease.clone();
    let consumer_task = tokio::task::spawn_blocking(move || {
        let staging_path = &consumer_lease.paths().staging_database;

        BPlusTree::<u32, XtreamPlaylistItem>::new()
            .store(staging_path)
            .map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to initialize staging Xtream tree {} for {cluster}: {error}",
                    staging_path.display()
                ))
            })?;

        let mut tree: BPlusTreeUpdate<u32, XtreamPlaylistItem> =
            BPlusTreeUpdate::try_new_with_backoff(staging_path).map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to open staging Xtream tree {} for {cluster}: {error}",
                    staging_path.display()
                ))
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
                    .map_err(|error| {
                        TuliproxError::RepositoryXtream(format!(
                            "Failed to prepare staging batch for {cluster} at {}: {error}",
                            staging_path.display()
                        ))
                    })?;
                tree.upsert_batch_encoded(prepared).map_err(|error| {
                    TuliproxError::RepositoryXtream(format!(
                        "Failed to write staging batch for {cluster} at {}: {error}",
                        staging_path.display()
                    ))
                })?;
                // Commit per batch so the write transaction's dirty-page map stays bounded.
                // Holding it open for the whole cluster buffered ~48k pages (196 MB); the
                // import writes into a .tmp file that is renamed on success, so atomicity
                // comes from the rename, not from a single transaction.
                tree.commit().map_err(|e| {
                    error!("Batch commit failed for cluster {cluster} at {}: {e}", staging_path.display());
                    TuliproxError::RepositoryXtream(format!(
                        "Failed to commit staging batch for {cluster} at {}: {e}",
                        staging_path.display()
                    ))
                })?;
                buffer.clear();
            }
        }

        // Final batch processing
        if !buffer.is_empty() {
            let batch: Vec<(&u32, &XtreamPlaylistItem)> =
                buffer.iter().map(|i| (&i.provider_id, i)).collect();
            let prepared = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&batch)
                .map_err(|error| {
                    TuliproxError::RepositoryXtream(format!(
                        "Failed to prepare final staging batch for {cluster} at {}: {error}",
                        staging_path.display()
                    ))
                })?;
            tree.upsert_batch_encoded(prepared).map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to write final staging batch for {cluster} at {}: {error}",
                    staging_path.display()
                ))
            })?;
        }

        tree.commit().map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to commit staging Xtream tree for {cluster} at {}: {error}",
                staging_path.display()
            ))
        })?;
        Ok::<(), TuliproxError>(())
    });

    // 3. Robust Joining of both tasks
    // try_join! returns immediately if any task returns an error or panics.
    let (parse_res, consumer_res) = tokio::try_join!(parse_task, consumer_task)
        .map_err(|e| TuliproxError::RepositoryXtream(format!("Task join error during cluster {cluster} update: {e}")))?;

    // Handle internal errors from the tasks
    let parsed_categories = parse_res?;
    consumer_res?;

    save_xtream_categories_to_file(refresh_lease.clone(), &parsed_categories).await?;

    // Lock order for the publish phase is always FileLockManager(final) followed by B+Tree sidecars. No B+Tree
    // handle escapes its blocking closure, so none is held while this async lock is acquired.
    let publish_lock = Arc::new(
        app_config
            .file_locks
            .write_lock(&refresh_lease.paths().published_database)
            .await,
    );

    let merge_lease = refresh_lease.clone();
    let merge_lock = Arc::clone(&publish_lock);
    let merge_outcome = tokio::task::spawn_blocking(move || {
        let _publish_guard = merge_lock;
        preserve_details_input_xtream_playlist_cluster_to_disk(
            &merge_lease.paths().published_database,
            &merge_lease.paths().staging_database,
        )
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Detail-preservation task failed to join during {cluster} refresh: {error}"
        ))
    })??;
    log::debug!(
        "Xtream cluster detail preservation completed: cluster={cluster} generation={} outcome={merge_outcome:?}",
        refresh_lease.paths().generation
    );

    let compact_lease = refresh_lease.clone();
    let compact_lock = Arc::clone(&publish_lock);
    tokio::task::spawn_blocking(move || {
        let _publish_guard = compact_lock;
        let staging_path = &compact_lease.paths().staging_database;
        let mut tree = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new_with_backoff(staging_path)
            .map_err(|error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to open staging Xtream tree {} for compaction: {error}",
                    staging_path.display()
                ))
            })?;
        tree.compact().map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to compact staging Xtream tree {}: {error}",
                staging_path.display()
            ))
        })
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!("Compaction task failed to join during {cluster} refresh: {error}"))
    })??;

    let database_publish_lease = refresh_lease.clone();
    let database_publish_lock = Arc::clone(&publish_lock);
    tokio::task::spawn_blocking(move || {
        let _publish_guard = database_publish_lock;
        publish_staged_database::<u32, XtreamPlaylistItem>(
            &database_publish_lease.paths().staging_database,
            &database_publish_lease.paths().published_database,
        )
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to publish staging Xtream database for {cluster}: {error}"
            ))
        })
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!("Database publish task failed to join for {cluster}: {error}"))
    })??;

    let category_publish_lease = refresh_lease.clone();
    let category_publish_lock = Arc::clone(&publish_lock);
    tokio::task::spawn_blocking(move || {
        let _publish_guard = category_publish_lock;
        publish_staged_file_same_directory(
            &category_publish_lease.paths().staging_categories,
            &category_publish_lease.paths().published_categories,
        )
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Xtream database for {cluster} was published, but category publication failed: {error}"
            ))
        })
    })
    .await
    .map_err(|error| {
        TuliproxError::RepositoryXtream(format!(
            "Xtream database for {cluster} was published, but the category publish task failed to join: {error}"
        ))
    })??;

    let cleanup_lease = refresh_lease.clone();
    let cleanup_lock = Arc::clone(&publish_lock);
    tokio::task::spawn_blocking(move || {
        let _publish_guard = cleanup_lock;
        cleanup_lease.cleanup_staging_artifacts()
    })
        .await
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Xtream refresh for {cluster} was published, but cleanup task failed to join: {error}"
            ))
        })?
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Xtream refresh for {cluster} was published, but staging cleanup failed: {error}"
            ))
        })?;

    drop(publish_lock);
    log::debug!(
        "Xtream cluster updated successfully: cluster={cluster} generation={}",
        refresh_lease.paths().generation
    );
    Ok(())
}

fn publish_staged_file_same_directory(staging: &Path, published: &Path) -> io::Result<()> {
    require_same_parent_directory(staging, published)?;
    let staging_path = tempfile::TempPath::try_from_path(staging).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to prepare staging file {} for publication: {error}", staging.display()),
        )
    })?;
    publish_staged_file_platform(staging_path, published)
}

#[cfg(not(windows))]
fn publish_staged_file_platform(staging_path: tempfile::TempPath, published: &Path) -> io::Result<()> {
    publish_staged_file_with_parent_sync(staging_path, published, sync_published_file_parent)
}

#[cfg(not(windows))]
fn publish_staged_file_with_parent_sync(
    staging_path: tempfile::TempPath,
    published: &Path,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    staging_path.persist(published).map_err(io::Error::from)?;
    sync_parent(published).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "file {} was published, but its parent directory {} could not be synchronized: {error}",
                published.display(),
                parent_or_dot(published).display()
            ),
        )
    })
}

#[cfg(windows)]
fn publish_staged_file_platform(mut staging_path: tempfile::TempPath, published: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn encode_path(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Windows path contains an embedded NUL: {}", path.display()),
            ));
        }
        encoded.push(0);
        Ok(encoded)
    }

    let staging_encoded = encode_path(staging_path.as_ref())?;
    let published_encoded = encode_path(published)?;

    // SAFETY: both buffers are live, immutable, and NUL-terminated for the
    // duration of the call. The same-directory check above ensures that the
    // operation cannot degrade into a cross-volume copy.
    let result = unsafe {
        MoveFileExW(
            staging_encoded.as_ptr(),
            published_encoded.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "failed to atomically publish staging file {} as {} with a Windows write-through rename: {error}",
                staging_path.display(),
                published.display()
            ),
        ));
    }

    // MoveFileExW consumed the source path. Prevent TempPath from issuing a
    // redundant delete for a path that no longer exists.
    staging_path.disable_cleanup(true);
    Ok(())
}

#[cfg(unix)]
fn sync_published_file_parent(path: &Path) -> io::Result<()> {
    File::open(parent_or_dot(path))?.sync_all()
}

/// There is no supported directory durability barrier for other targets.
/// Callers report this only after the atomic rename has completed.
#[cfg(all(not(unix), not(windows)))]
fn sync_published_file_parent(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "parent-directory synchronization is unsupported on this platform",
    ))
}

/// Owns the staging category file plus its `flock`, so the lock is released
/// even when a later step (`serde_json::to_writer`, `sync_all`) returns an
/// error. The unlock runs in `Drop` and is logged on failure; closing the
/// underlying `File` releases the OS-level lock either way.
struct LockedCategoryFile {
    file: File,
    path: PathBuf,
}

impl LockedCategoryFile {
    fn create(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        file.lock_exclusive()?;
        Ok(Self { file, path: path.to_path_buf() })
    }

    fn sync_all(&self) -> io::Result<()> { self.file.sync_all() }
}

impl Drop for LockedCategoryFile {
    fn drop(&mut self) {
        if let Err(error) = fs2::FileExt::unlock(&self.file) {
            log::warn!(
                "Failed to unlock staging category file {}: {error}; the OS will release it on close",
                self.path.display()
            );
        }
    }
}

async fn save_xtream_categories_to_file(
    refresh_lease: XtreamRefreshLease,
    categories: &[XtreamCategory],
) -> Result<(), TuliproxError> {
    let cat_entries: Vec<CategoryEntry> = categories
        .iter()
        .map(|c| CategoryEntry {
            category_id: c.category_id,
            category_name: c.category_name.clone(),
            parent_id: 0,
        })
        .collect();

    tokio::task::spawn_blocking(move || {
        let staging_path = &refresh_lease.paths().staging_categories;
        let locked = LockedCategoryFile::create(staging_path).map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to create or lock staging category file {}: {error}",
                staging_path.display()
            ))
        })?;
        serde_json::to_writer(&locked.file, &cat_entries).map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to write staging category file {}: {error}",
                staging_path.display()
            ))
        })?;
        locked.sync_all().map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to synchronize staging category file {}: {error}",
                staging_path.display()
            ))
        })?;
        Ok(())
    })
        .await
        .map_err(|e| TuliproxError::RepositoryXtream(format!("Spawn error {e}")))?
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
                        if let (Some(new_stream_props), Some(old_stream_props)) = (&mut pli.header.additional_properties, stored_pli.additional_properties.take()) {
                            merge_preserved_stream_properties(new_stream_props, &old_stream_props);
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
        let col_path = get_collection_path(&root_path, xtream_cluster_category_collection(cluster));
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
        Some(TuliproxError::RepositoryXtream(errors.join("\n")))
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

/// Merges persisted fields from old stream properties into freshly fetched properties.
///
/// This keeps long-lived metadata stable across full playlist rewrites:
/// - VOD/Series `details` are preserved when incoming provider metadata is not newer.
/// - Learned Live fields are merged through [`LiveStreamProperties::merge_learned_metadata_from`].
/// - Live catchup remains separate provider metadata and is copied only when missing.
pub(crate) fn merge_preserved_stream_properties(
    new_stream_props: &mut StreamProperties,
    old_stream_props: &StreamProperties,
) -> bool {
    let preserve_info_details =
        old_stream_props.has_details() && !needs_update_info_details(new_stream_props, old_stream_props);

    match (new_stream_props, old_stream_props) {
        (StreamProperties::Video(v_new), StreamProperties::Video(v_old)) => {
            let mut changed = false;

            if preserve_info_details && v_old.details.is_some() && v_new.details != v_old.details {
                v_new.details.clone_from(&v_old.details);
                changed = true;
            }

            if v_new.tmdb.is_none() && v_old.tmdb.is_some() {
                v_new.tmdb = v_old.tmdb;
                changed = true;
            }

            changed
        }
        (StreamProperties::Series(s_new), StreamProperties::Series(s_old)) => {
            let mut changed = false;

            if preserve_info_details && s_old.details.is_some() && s_new.details != s_old.details {
                s_new.details.clone_from(&s_old.details);
                changed = true;
            }

            if s_new.tmdb.is_none() && s_old.tmdb.is_some() {
                s_new.tmdb = s_old.tmdb;
                changed = true;
            }

            if s_new.release_date.is_none() && s_old.release_date.is_some() {
                s_new.release_date.clone_from(&s_old.release_date);
                changed = true;
            }

            changed
        }
        (StreamProperties::Live(l_new), StreamProperties::Live(l_old)) => {
            let mut changed = l_new.merge_learned_metadata_from(l_old);

            if l_new.catchup.is_none() && l_old.catchup.is_some() {
                l_new.catchup.clone_from(&l_old.catchup);
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
                    //rebuild_source_ordinal_index_if_present(&xtream_path_clone)
                    //    .map_err(|err| Error::other(format!("failed to rebuild sorted index for input {input_name_owned}: {err}")))?;
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
                //rebuild_source_ordinal_index_if_present(&xtream_path_clone)
                //    .map_err(|err| Error::other(format!("failed to rebuild sorted index for input {input_name_owned}: {err}")))?;
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
            let cat_col_name = xtream_cluster_category_collection(cluster);
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
                let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)
                    .map_err(|error| TuliproxError::RepositoryXtream(error.to_string()))?;
                for entry in query.iter() {
                    let (_, item) = entry.map_err(|error| TuliproxError::RepositoryXtream(error.to_string()))?;
                    items.push(item);
                }
                Ok(items)
            })
                .await
            .map_err(|err| TuliproxError::RepositoryXtream(format!(
                "failed to read xtream playlist: {xtream_display} - {err}"
            )))??;

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
    use super::{
        merge_preserved_stream_properties, needs_update_info_details,
        persist_input_xtream_playlist_cluster_to_disk, preserve_details_input_xtream_playlist_cluster_to_disk,
        preserve_details_with_injected_operation_failure, publish_staged_file_same_directory,
        DetailPreservationOperation, PreserveDetailsOutcome, XtreamRefreshLease,
        XtreamRefreshPaths,
    };
    #[cfg(unix)]
    use super::refresh_staging_path;
    use crate::model::{
        ApiProxyConfig, AppConfig, Config, ConfigInput, CustomStreamResponse, HdHomeRunConfig,
        MediaToolCapabilities, SourcesConfig,
    };
    use crate::repository::{
        bplustree::{ensure_distinct_sidecar_lock_domains, sidecar_lock_path},
        build_input_storage_path, cleanup_orphaned_staging_artifacts, get_file_path_for_db_index,
        refresh_generation_guard_path, BPlusTreeQuery, BPlusTreeUpdate,
    };
    use crate::utils::{request::DynReader, FileLockManager};
    use arc_swap::{ArcSwap, ArcSwapOption};
    use fs2::FileExt;
    use shared::model::{
        CatchupProperties, ConfigPaths, InputType, LiveStreamProperties, SeriesStreamProperties, StreamProperties,
        VideoStreamProperties, XtreamCluster, XtreamPlaylistItem,
    };
    use shared::utils::Internable;
    use std::{
        env, fs, io,
        path::Path,
        process::{Child, Command, ExitStatus, Stdio},
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use uuid::Uuid;

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

    fn json_reader(content: &'static str) -> DynReader {
        let (mut writer, reader) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            writer.write_all(content.as_bytes()).await.expect("fixture should fit into duplex reader");
            writer.shutdown().await.expect("fixture writer should shut down");
        });
        Box::pin(reader)
    }

    fn wait_for_child(mut child: Child, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "Xtream refresh child timed out"));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
        }
    }

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

    #[test]
    fn merge_preserves_higher_learned_live_bitrate() {
        let mut new_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            bitrate: 1_500_000,
            ..LiveStreamProperties::default()
        }));
        let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            bitrate: 2_500_000,
            ..LiveStreamProperties::default()
        }));

        assert!(merge_preserved_stream_properties(&mut new_props, &old_props));
        match new_props {
            StreamProperties::Live(live) => assert_eq!(live.bitrate, 2_500_000),
            _ => panic!("expected live properties"),
        }
    }

    #[test]
    fn merge_preserves_missing_live_catchup_properties() {
        let mut new_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            ..LiveStreamProperties::default()
        }));
        let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
            stream_id: 1,
            catchup: Some(CatchupProperties {
                mode: Some("append".into()),
                source: Some("?offset=-${offset}".into()),
                ..CatchupProperties::default()
            }),
            ..LiveStreamProperties::default()
        }));

        let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
        assert!(changed);
        match new_props {
            StreamProperties::Live(live) => {
                let catchup = live.catchup.expect("catchup should be preserved");
                assert_eq!(catchup.mode.as_deref(), Some("append"));
                assert_eq!(catchup.source.as_deref(), Some("?offset=-${offset}"));
            }
            _ => panic!("expected live properties"),
        }
    }

    #[test]
    fn merge_preserves_missing_video_tmdb() {
        let mut new_props = StreamProperties::Video(Box::new(VideoStreamProperties {
            tmdb: None,
            ..VideoStreamProperties::default()
        }));
        let old_props = StreamProperties::Video(Box::new(VideoStreamProperties {
            tmdb: Some(317_981),
            ..VideoStreamProperties::default()
        }));

        let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
        assert!(changed);
        match new_props {
            StreamProperties::Video(video) => assert_eq!(video.tmdb, Some(317_981)),
            _ => panic!("expected video properties"),
        }
    }

    #[test]
    fn merge_preserves_missing_series_tmdb_and_release_date() {
        let mut new_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
            tmdb: None,
            release_date: None,
            ..SeriesStreamProperties::default()
        }));
        let old_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
            tmdb: Some(12345),
            release_date: Some("2015-01-01".into()),
            ..SeriesStreamProperties::default()
        }));

        let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
        assert!(changed);
        match new_props {
            StreamProperties::Series(series) => {
                assert_eq!(series.tmdb, Some(12345));
                assert_eq!(series.release_date.as_deref(), Some("2015-01-01"));
            }
            _ => panic!("expected series properties"),
        }
    }

    fn make_live_item(
        provider_id: u32,
        video: Option<&str>,
        audio: Option<&str>,
        last_probed_timestamp: Option<i64>,
        last_success_timestamp: Option<i64>,
        bitrate: u32,
    ) -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: provider_id,
            provider_id,
            name: "Live".intern(),
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
                bitrate,
                ..Default::default()
            }))),
            item_type: shared::model::PlaylistItemType::Live,
            category_id: 1,
            input_name: "input_a".intern(),
            channel_no: 0,
            source_ordinal: 0,
            input_stream_id: provider_id.to_string().intern(),
            upstream_user_agent: None,
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

    fn fixed_refresh_paths(path: &Path, generation: u128) -> XtreamRefreshPaths {
        XtreamRefreshPaths::for_generation(path, XtreamCluster::Live, Uuid::from_u128(generation))
            .expect("fixed refresh paths should be valid")
    }

    fn write_detail_preservation_fixture(paths: &XtreamRefreshPaths, provider_id: u32) {
        write_single_item(
            &paths.published_database,
            &make_live_item(
                provider_id,
                Some("{\"codec_name\":\"h264\"}"),
                Some("{\"codec_name\":\"aac\"}"),
                Some(1_700_000_000),
                Some(1_700_000_100),
                2_500_000,
            ),
        );
        write_single_item(
            &paths.staging_database,
            &make_live_item(provider_id, None, None, None, None, 0),
        );
    }

    #[test]
    fn refresh_staging_database_uses_distinct_published_lock_domain() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 1);

        assert_eq!(paths.published_database.file_name().and_then(|name| name.to_str()), Some("live.db"));
        assert_ne!(sidecar_lock_path(&paths.published_database), sidecar_lock_path(&paths.staging_database));
    }

    #[test]
    fn refresh_staging_database_and_index_share_one_generation_lock_domain() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 2);
        let staging_index = get_file_path_for_db_index(&paths.staging_database);

        assert_eq!(sidecar_lock_path(&paths.staging_database), sidecar_lock_path(&staging_index));
    }

    #[test]
    fn colliding_staging_path_is_rejected_before_lock_acquisition() {
        let dir = tempdir().expect("temp dir should be created");
        let published = dir.path().join("live.db");
        let colliding = dir.path().join("live.tmp");

        let error = ensure_distinct_sidecar_lock_domains(&published, &colliding)
            .expect_err("colliding sidecar domains must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn preserve_details_for_disk_cluster_copies_missing_live_probe_fields() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 3);
        let provider_id = 100_u32;

        write_single_item(
            &paths.published_database,
            &make_live_item(
                provider_id,
                Some("{\"codec_name\":\"h264\"}"),
                Some("{\"codec_name\":\"aac\"}"),
                Some(1_700_000_000),
                Some(1_700_000_100),
                2_500_000,
            ),
        );
        write_single_item(
            &paths.staging_database,
            &make_live_item(provider_id, None, None, None, None, 0),
        );

        let outcome = preserve_details_input_xtream_playlist_cluster_to_disk(
            &paths.published_database,
            &paths.staging_database,
        )
        .expect("merge should succeed");
        assert_eq!(outcome, PreserveDetailsOutcome::Merged { scanned: 1, updated: 1 });

        let merged = read_live_props(&paths.staging_database, provider_id);
        assert_eq!(merged.video, Some("{\"codec_name\":\"h264\"}".intern()));
        assert_eq!(merged.audio, Some("{\"codec_name\":\"aac\"}".intern()));
        assert_eq!(merged.last_probed_timestamp, Some(1_700_000_000));
        assert_eq!(merged.last_success_timestamp, Some(1_700_000_100));
        assert_eq!(merged.bitrate, 2_500_000);
    }

    #[test]
    fn preserve_details_reports_missing_published_database_explicitly() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 4);
        write_single_item(&paths.staging_database, &make_live_item(101, None, None, None, None, 0));

        let outcome = preserve_details_input_xtream_playlist_cluster_to_disk(
            &paths.published_database,
            &paths.staging_database,
        )
        .expect("a missing published database should not fail the refresh");

        assert_eq!(outcome, PreserveDetailsOutcome::SourceMissing);
    }

    #[test]
    fn preserve_details_propagates_corrupt_published_database() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 5);
        fs::write(&paths.published_database, b"corrupt").expect("corrupt fixture should be written");
        write_single_item(&paths.staging_database, &make_live_item(102, None, None, None, None, 0));

        let error = preserve_details_input_xtream_playlist_cluster_to_disk(
            &paths.published_database,
            &paths.staging_database,
        )
        .expect_err("corrupt published data must fail");

        assert!(error.to_string().contains(&paths.published_database.display().to_string()));
    }

    #[test]
    fn preserve_details_propagates_corrupt_staging_database() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 6);
        write_single_item(&paths.published_database, &make_live_item(103, None, None, None, None, 0));
        fs::write(&paths.staging_database, b"corrupt").expect("corrupt fixture should be written");

        let error = preserve_details_input_xtream_playlist_cluster_to_disk(
            &paths.published_database,
            &paths.staging_database,
        )
        .expect_err("corrupt staging data must fail");

        assert!(error.to_string().contains(&paths.staging_database.display().to_string()));
    }

    #[test]
    fn preserve_details_propagates_missing_staging_database() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 14);
        write_single_item(&paths.published_database, &make_live_item(105, None, None, None, None, 0));

        let error = preserve_details_input_xtream_playlist_cluster_to_disk(
            &paths.published_database,
            &paths.staging_database,
        )
        .expect_err("a missing staging database must fail");
        let message = error.to_string();

        assert!(message.contains("Failed to open staging Xtream tree"));
        assert!(message.contains(&paths.staging_database.display().to_string()));
    }

    #[test]
    fn preserve_details_propagates_staging_query_failure() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 15);
        write_detail_preservation_fixture(&paths, 106);

        let error = preserve_details_with_injected_operation_failure(
            &paths.published_database,
            &paths.staging_database,
            DetailPreservationOperation::Query,
        )
        .expect_err("a staging query failure must fail the merge");
        let message = error.to_string();

        assert!(message.contains("Failed to query staging Xtream tree"));
        assert!(message.contains(&paths.staging_database.display().to_string()));
        assert!(message.contains("injected Query failure"));
    }

    #[test]
    fn preserve_details_propagates_staging_batch_write_failure() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 16);
        write_detail_preservation_fixture(&paths, 107);

        let error = preserve_details_with_injected_operation_failure(
            &paths.published_database,
            &paths.staging_database,
            DetailPreservationOperation::BatchWrite,
        )
        .expect_err("a staging batch write failure must fail the merge");
        let message = error.to_string();

        assert!(message.contains("Failed to update staging Xtream tree"));
        assert!(message.contains(&paths.staging_database.display().to_string()));
        assert!(message.contains("injected BatchWrite failure"));
    }

    #[test]
    fn preserve_details_propagates_staging_commit_failure() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 17);
        write_detail_preservation_fixture(&paths, 108);

        let error = preserve_details_with_injected_operation_failure(
            &paths.published_database,
            &paths.staging_database,
            DetailPreservationOperation::Commit,
        )
        .expect_err("a staging commit failure must fail the merge");
        let message = error.to_string();

        assert!(message.contains("Failed to commit staging Xtream tree"));
        assert!(message.contains(&paths.staging_database.display().to_string()));
        assert!(message.contains("injected Commit failure"));
    }

    #[test]
    fn preserve_details_empty_merge_reports_zero_updates() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 7);
        let item = make_live_item(104, None, None, None, None, 0);
        write_single_item(&paths.published_database, &item);
        write_single_item(&paths.staging_database, &item);

        let outcome = preserve_details_input_xtream_playlist_cluster_to_disk(
            &paths.published_database,
            &paths.staging_database,
        )
        .expect("empty merge should succeed");

        assert_eq!(outcome, PreserveDetailsOutcome::Merged { scanned: 1, updated: 0 });
    }

    #[test]
    fn refresh_lease_cleanup_is_idempotent_and_generation_local() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 8);
        let other_paths = fixed_refresh_paths(dir.path(), 9);
        let lease = XtreamRefreshLease::new(paths.clone()).expect("refresh lease should be valid");

        fs::write(&paths.published_database, b"published").expect("published fixture should be written");
        let published_lock = sidecar_lock_path(&paths.published_database);
        fs::write(&published_lock, b"").expect("published lock fixture should be written");
        for artifact in lease.0.database_artifacts.owned_paths() {
            fs::write(artifact, b"staging").expect("staging artifact should be written");
        }
        fs::write(&paths.staging_categories, b"staging").expect("staging category should be written");
        fs::write(&other_paths.staging_database, b"other generation")
            .expect("other generation fixture should be written");

        lease.cleanup_staging_artifacts().expect("first cleanup should succeed");
        lease.cleanup_staging_artifacts().expect("second cleanup should be idempotent");

        assert!(paths.published_database.exists());
        assert!(published_lock.exists());
        assert!(other_paths.staging_database.exists());
        assert!(!paths.staging_database.exists());
        assert!(!paths.staging_categories.exists());
    }

    #[test]
    fn refresh_lease_defers_cleanup_until_last_worker_clone_drops() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 13);
        let guard_path = refresh_generation_guard_path(dir.path(), paths.generation);
        let parent_lease = XtreamRefreshLease::new(paths.clone()).expect("refresh lease should be valid");
        let worker_lease = parent_lease.clone();
        fs::write(&paths.staging_database, b"staging").expect("staging fixture should be written");
        fs::write(&paths.staging_categories, b"categories").expect("category fixture should be written");

        drop(parent_lease);
        assert!(paths.staging_database.exists());
        assert!(paths.staging_categories.exists());
        assert!(guard_path.exists());

        drop(worker_lease);
        assert!(!paths.staging_database.exists());
        assert!(!paths.staging_categories.exists());
        assert!(!guard_path.exists());
    }

    #[test]
    fn active_refresh_between_btree_batches_survives_orphan_cleanup() {
        let dir = tempdir().expect("temp dir should be created");
        let paths = fixed_refresh_paths(dir.path(), 14);
        let guard_path = refresh_generation_guard_path(dir.path(), paths.generation);
        let lease = XtreamRefreshLease::new(paths.clone()).expect("refresh lease should be valid");
        let sidecar = sidecar_lock_path(&paths.staging_database);
        fs::write(&paths.staging_database, b"staging").expect("staging fixture should be written");
        fs::write(&paths.staging_categories, b"categories").expect("category fixture should be written");
        fs::write(&sidecar, b"").expect("sidecar fixture should be written");

        let between_batch_probe = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&sidecar)
            .expect("open staging sidecar");
        between_batch_probe
            .try_lock_exclusive()
            .expect("staging sidecar should be unlocked between batches");
        fs2::FileExt::unlock(&between_batch_probe).expect("release between-batch probe");
        drop(between_batch_probe);

        cleanup_orphaned_staging_artifacts(dir.path(), Duration::ZERO);

        assert!(paths.staging_database.exists());
        assert!(paths.staging_categories.exists());
        assert!(sidecar.exists());
        assert!(guard_path.exists());

        drop(lease);
        assert!(!paths.staging_database.exists());
        assert!(!paths.staging_categories.exists());
        assert!(!sidecar.exists());
        assert!(!guard_path.exists());
    }

    #[test]
    fn sequential_refresh_generations_use_distinct_stems() {
        let dir = tempdir().expect("temp dir should be created");
        let first = fixed_refresh_paths(dir.path(), 10);
        let second = fixed_refresh_paths(dir.path(), 11);

        assert_ne!(first.staging_database.file_stem(), second.staging_database.file_stem());
        assert_ne!(sidecar_lock_path(&first.staging_database), sidecar_lock_path(&second.staging_database));
    }

    #[test]
    fn category_publish_atomically_replaces_same_directory_file() {
        let dir = tempdir().expect("temp dir should be created");
        let published = dir.path().join("cat_live.json");
        let staging = dir.path().join("cat_live.refresh-fixed.json");
        fs::write(&published, b"old").expect("published fixture should be written");
        fs::write(&staging, b"new").expect("staging fixture should be written");

        publish_staged_file_same_directory(&staging, &published).expect("category publish should succeed");

        assert_eq!(fs::read(&published).expect("published categories should be readable"), b"new");
        assert!(!staging.exists());
    }

    #[test]
    fn category_publish_creates_missing_same_directory_file() {
        let dir = tempdir().expect("temp dir should be created");
        let published = dir.path().join("cat_live.json");
        let staging = dir.path().join("cat_live.refresh-fixed.json");
        fs::write(&staging, b"new").expect("staging fixture should be written");

        publish_staged_file_same_directory(&staging, &published).expect("category publish should succeed");

        assert_eq!(fs::read(&published).expect("published categories should be readable"), b"new");
        assert!(!staging.exists());
    }

    #[test]
    fn category_publish_rejects_different_parent_before_replace() {
        let dir = tempdir().expect("temp dir should be created");
        let staging_dir = dir.path().join("staging");
        let published_dir = dir.path().join("published");
        fs::create_dir_all(&staging_dir).expect("staging directory should be created");
        fs::create_dir_all(&published_dir).expect("published directory should be created");
        let staging = staging_dir.join("cat_live.refresh-fixed.json");
        let published = published_dir.join("cat_live.json");
        fs::write(&staging, b"new").expect("staging fixture should be written");
        fs::write(&published, b"old").expect("published fixture should be written");

        let error = publish_staged_file_same_directory(&staging, &published)
            .expect_err("cross-directory category publication should fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&staging).expect("staging fixture should remain"), b"new");
        assert_eq!(fs::read(&published).expect("published fixture should remain"), b"old");
    }

    #[cfg(not(windows))]
    #[test]
    fn category_publish_reports_post_rename_barrier_failure_truthfully() {
        let dir = tempdir().expect("temp dir should be created");
        let published = dir.path().join("cat_live.json");
        let staging = dir.path().join("cat_live.refresh-fixed.json");
        fs::write(&published, b"old").expect("published fixture should be written");
        fs::write(&staging, b"new").expect("staging fixture should be written");
        let staging_path = tempfile::TempPath::try_from_path(&staging)
            .expect("staging fixture should become an owned temporary path");

        let error = super::publish_staged_file_with_parent_sync(staging_path, &published, |_| {
            Err(io::Error::other("injected parent synchronization failure"))
        })
        .expect_err("post-rename synchronization failure should be reported");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("was published, but its parent directory"));
        assert_eq!(fs::read(&published).expect("published categories should be readable"), b"new");
        assert!(!staging.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_staging_path_preserves_non_utf8_stem() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let mut input_name = std::ffi::OsString::from_vec(vec![b'l', b'i', b'v', b'e', 0xff]);
        input_name.push(".db");
        let published = Path::new("/tmp").join(input_name);
        let generation = Uuid::from_u128(12);

        let staging = refresh_staging_path(&published, generation).expect("non-UTF-8 path should be supported");
        let bytes = staging.file_name().expect("staging file name").as_bytes();
        assert!(bytes.starts_with(&[b'l', b'i', b'v', b'e', 0xff]));
        assert!(bytes.ends_with(b".db"));
        assert!(bytes.windows(b".refresh-".len()).any(|window| window == b".refresh-"));
    }

    #[test]
    fn xtream_refresh_end_to_end_child() -> io::Result<()> {
        let Some(storage_root) = env::var_os("TULIPROX_XTREAM_REFRESH_TEST_ROOT") else {
            return Ok(());
        };
        let storage_root = Path::new(&storage_root);
        let app_config = test_app_config(storage_root);
        let input = ConfigInput {
            name: "deadlock-test".intern(),
            input_type: InputType::Xtream,
            ..ConfigInput::default()
        };
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        runtime.block_on(async {
            for _ in 0..2 {
                persist_input_xtream_playlist_cluster_to_disk(
                    &app_config,
                    &input,
                    XtreamCluster::Live,
                    json_reader(r#"[{"category_id":"1","category_name":"Sports"}]"#),
                    json_reader(r#"[{"name":"Live","stream_id":700,"category_id":"1","added":"0"}]"#),
                )
                .await
                .map_err(|error| io::Error::other(error.to_string()))?;
            }
            Ok::<(), io::Error>(())
        })?;

        let input_storage = build_input_storage_path(&input.name, storage_root.to_string_lossy().as_ref());
        let published = super::xtream_get_file_path(&input_storage, XtreamCluster::Live);
        let learned = read_live_props(&published, 700);
        assert_eq!(learned.video, Some("{\"codec_name\":\"h264\"}".intern()));
        assert_eq!(learned.bitrate, 2_500_000);
        Ok(())
    }

    #[test]
    fn two_sequential_cluster_refreshes_complete_without_generation_artifacts() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let input_name = "deadlock-test".intern();
        let input_storage = build_input_storage_path(&input_name, directory.path().to_string_lossy().as_ref());
        fs::create_dir_all(&input_storage)?;
        let published = super::xtream_get_file_path(&input_storage, XtreamCluster::Live);
        write_single_item(
            &published,
            &make_live_item(
                700,
                Some("{\"codec_name\":\"h264\"}"),
                Some("{\"codec_name\":\"aac\"}"),
                Some(1_700_000_000),
                Some(1_700_000_100),
                2_500_000,
            ),
        );

        let child = Command::new(env::current_exe()?)
            .arg("--exact")
            .arg("repository::xtream_repository::tests::xtream_refresh_end_to_end_child")
            .arg("--nocapture")
            .env("TULIPROX_XTREAM_REFRESH_TEST_ROOT", directory.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let status = wait_for_child(child, Duration::from_secs(20))?;
        assert!(status.success(), "Xtream refresh child failed with {status}");

        let learned = read_live_props(&published, 700);
        assert_eq!(learned.video, Some("{\"codec_name\":\"h264\"}".intern()));
        assert_eq!(learned.bitrate, 2_500_000);
        let entries = fs::read_dir(&input_storage)?.collect::<io::Result<Vec<_>>>()?;
        let generation_artifacts = entries
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".refresh-"))
            .collect::<Vec<_>>();
        assert!(generation_artifacts.is_empty(), "generation artifacts survived successful refreshes");
        assert!(sidecar_lock_path(&published).exists());
        Ok(())
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
                2_500_000,
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
                3_500_000,
            ),
        );

        preserve_details_input_xtream_playlist_cluster_to_disk(&old_path, &tmp_path).expect("merge should succeed");

        let merged = read_live_props(&tmp_path, provider_id);
        assert_eq!(merged.video, Some("{\"codec_name\":\"hevc\"}".intern()));
        assert_eq!(merged.audio, Some("{\"codec_name\":\"ac3\"}".intern()));
        assert_eq!(merged.last_probed_timestamp, Some(1_800_000_000));
        assert_eq!(merged.last_success_timestamp, Some(1_800_000_100));
        assert_eq!(merged.bitrate, 3_500_000);
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
                2_500_000,
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
                3_500_000,
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
