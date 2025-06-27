use shared::error::{create_tuliprox_error, create_tuliprox_error_result, info_err, notify_err, str_to_io_error, to_io_error, TuliproxError, TuliproxErrorKind};
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{Config, ConfigInput, ConfigTarget, XtreamTargetOutput};
use crate::model::{rewrite_doc_urls, PlaylistXtreamCategory, XtreamMappingOptions, XtreamSeriesEpisode};
use crate::model::normalize_release_date;
use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery, BPlusTreeUpdate};
use crate::repository::indexed_document::{IndexedDocumentDirectAccess, IndexedDocumentGarbageCollector, IndexedDocumentIterator, IndexedDocumentWriter};
use crate::repository::playlist_repository::get_target_id_mapping;
use crate::repository::storage::{get_input_storage_path, get_target_id_mapping_file, get_target_storage_path};
use crate::repository::storage_const;
use crate::repository::target_id_mapping::VirtualIdRecord;
use crate::repository::xtream_playlist_iterator::XtreamPlaylistJsonIterator;
use crate::utils::bincode_deserialize;
use crate::utils::FileReadGuard;
use crate::utils::file_reader;
use crate::utils::open_readonly_file;
use crate::utils::{json_write_documents_to_file};
use bytes::Bytes;
use futures::{stream, Stream, StreamExt};
use log::error;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{BufReader, BufWriter, Error, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use shared::model::{PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemType, XtreamCluster, XtreamPlaylistItem};
use shared::utils::{generate_playlist_uuid, get_u32_from_serde_value, hex_encode, json_iter_array};

macro_rules! cant_write_result {
    ($path:expr, $err:expr) => {
        create_tuliprox_error!(
            TuliproxErrorKind::Notify,
            "failed to write xtream playlist: {} - {}",
            $path.to_str().unwrap(),
            $err
        )
    };
}

macro_rules! try_option_ok {
    ($option:expr) => {
        match $option {
            Some(value) => value,
            None => return Ok(()),
        }
    };
}


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct InputVodInfoRecord {
    pub(crate) tmdb_id: u32,
    pub(crate) ts: u64,
    pub(crate) release_date: Option<String>,
}

fn get_collection_path(path: &Path, collection: &str) -> PathBuf {
    path.join(format!("{collection}.json"))
}

fn ensure_xtream_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    if let Some(path) = xtream_get_storage_path(cfg, target_name) {
        if std::fs::create_dir_all(&path).is_err() {
            let msg = format!(
                "Failed to save xtream data, can't create directory {}",
                &path.to_str().unwrap()
            );
            return Err(notify_err!(msg));
        }
        Ok(path)
    } else {
        let msg = format!("Failed to save xtream data, can't create directory for target {target_name}");
        Err(notify_err!(msg))
    }
}

pub fn xtream_get_info_file_paths(
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Option<(PathBuf, PathBuf)> {
    if cluster == XtreamCluster::Series {
        let xtream_path = storage_path.join(format!("{}.{}", storage_const::FILE_SERIES_INFO, storage_const::FILE_SUFFIX_DB));
        let index_path = storage_path.join(format!("{}.{}", storage_const::FILE_SERIES_INFO, storage_const::FILE_SUFFIX_INDEX));
        return Some((xtream_path, index_path));
    } else if cluster == XtreamCluster::Video {
        let xtream_path = storage_path.join(format!("{}.{}", storage_const::FILE_VOD_INFO, storage_const::FILE_SUFFIX_DB));
        let index_path = storage_path.join(format!("{}.{}", storage_const::FILE_VOD_INFO, storage_const::FILE_SUFFIX_INDEX));
        return Some((xtream_path, index_path));
    }
    None
}

pub fn xtream_get_record_file_path(storage_path: &Path, item_type: PlaylistItemType) -> Option<PathBuf> {
    match item_type {
        PlaylistItemType::Video => Some(storage_path.join(format!("{}.{}", storage_const::FILE_VOD_INFO_RECORD, storage_const::FILE_SUFFIX_DB))),
        PlaylistItemType::SeriesInfo => Some(storage_path.join(format!("{}.{}", storage_const::FILE_SERIES_INFO_RECORD, storage_const::FILE_SUFFIX_DB))),
        PlaylistItemType::Series => Some(storage_path.join(format!("{}.{}", storage_const::FILE_SERIES_EPISODE_RECORD, storage_const::FILE_SUFFIX_DB))),
        _ => None,
    }
}
fn write_playlists_to_file(
    cfg: &AppConfig,
    storage_path: &Path,
    collections: Vec<(XtreamCluster, &[&mut PlaylistItem])>,
) -> Result<(), TuliproxError> {
    for (cluster, playlist) in collections {
        let (xtream_path, idx_path) = xtream_get_file_paths(storage_path, cluster);
        {
            let _file_lock = cfg.file_locks.write_lock(&xtream_path);
            match IndexedDocumentWriter::new(xtream_path.clone(), idx_path) {
                Ok(mut writer) => {
                    for item in playlist {
                        let xtream = item.to_xtream();
                        match writer.write_doc(item.header.virtual_id, &xtream) {
                            Ok(()) => {}
                            Err(err) => return Err(cant_write_result!(&xtream_path, err)),
                        }
                    }
                    writer.store().map_err(|err| cant_write_result!(&xtream_path, err))?;
                }
                Err(err) => return Err(cant_write_result!(&xtream_path, err)),
            }
        }
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

fn load_old_category_ids(path: &Path) -> (u32, HashMap<String, u32>) {
    let mut result: HashMap<String, u32> = HashMap::new();
    let mut max_id: u32 = 0;
    for (cluster, cat) in [(XtreamCluster::Live, storage_const::COL_CAT_LIVE), (XtreamCluster::Video, storage_const::COL_CAT_VOD), (XtreamCluster::Series, storage_const::COL_CAT_SERIES)] {
        let col_path = get_collection_path(path, cat);
        if col_path.exists() {
            if let Ok(file) = File::open(col_path) {
                let reader = file_reader(file);
                for entry in json_iter_array::<Value, BufReader<File>>(reader).flatten() {
                    if let Some(category_id) = entry.get(crate::model::XC_TAG_CATEGORY_ID).and_then(get_u32_from_serde_value) {
                        if let Value::Object(item) = entry {
                            if let Some(category_name) = get_map_item_as_str(&item, crate::model::XC_TAG_CATEGORY_NAME) {
                                result.insert(format!("{cluster}{category_name}"), category_id);
                                max_id = max_id.max(category_id);
                            }
                        }
                    }
                }
            }
        }
    }
    (max_id, result)
}

pub fn xtream_get_storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> {
    get_target_storage_path(cfg, target_name).map(|target_path| target_path.join(PathBuf::from(storage_const::PATH_XTREAM)))
}

pub fn xtream_get_epg_file_path(path: &Path) -> PathBuf {
    path.join(storage_const::FILE_EPG)
}

fn xtream_get_file_paths_for_name(storage_path: &Path, name: &str) -> (PathBuf, PathBuf) {
    let xtream_path = storage_path.join(format!("{name}.{}", storage_const::FILE_SUFFIX_DB));
    let index_path = storage_path.join(format!("{name}.{}", storage_const::FILE_SUFFIX_INDEX));
    (xtream_path, index_path)
}

pub fn xtream_get_file_paths(storage_path: &Path, cluster: XtreamCluster) -> (PathBuf, PathBuf) {
    xtream_get_file_paths_for_name(storage_path, &cluster.as_str().to_lowercase())
}

pub fn xtream_get_file_paths_for_series(storage_path: &Path) -> (PathBuf, PathBuf) {
    xtream_get_file_paths_for_name(storage_path, storage_const::FILE_SERIES)
}

fn xtream_garbage_collect(config: &AppConfig, target_name: &str) -> std::io::Result<()> {
    // Garbage collect series
    let storage_path = {
        let cfg = config.config.load();
        try_option_ok!(xtream_get_storage_path(&cfg, target_name))
    };
    let (info_path, idx_path) = try_option_ok!(xtream_get_info_file_paths(
        &storage_path,
        XtreamCluster::Series
    ));
    {
        let _file_lock = config.file_locks.write_lock(&info_path);
        IndexedDocumentGarbageCollector::<u32>::new(info_path.clone(), idx_path)?.garbage_collect()?;
    }
    Ok(())
}

pub async fn xtream_write_playlist(
    cfg: &AppConfig,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
) -> Result<(), TuliproxError> {
    let path = {
        let config = cfg.config.load();
        ensure_xtream_storage_path(&config, target.name.as_str())?
    };
    let mut errors = Vec::new();
    let mut cat_live_col = Vec::with_capacity(1_000);
    let mut cat_series_col = Vec::with_capacity(1_000);
    let mut cat_vod_col = Vec::with_capacity(1_000);
    let mut live_col = Vec::with_capacity(50_000);
    let mut series_col = Vec::with_capacity(10_000);
    let mut vod_col = Vec::with_capacity(10_000);

    // preserve category_ids
    let (max_cat_id, existing_cat_ids) = load_old_category_ids(&path);
    let mut cat_id_counter = max_cat_id;
    for plg in playlist.iter_mut() {
        if !&plg.channels.is_empty() {
            let cat_key = format!("{}{}", plg.xtream_cluster, &plg.title);
            let cat_id = existing_cat_ids.get(&cat_key).unwrap_or_else(|| {
                cat_id_counter += 1;
                &cat_id_counter
            });
            plg.id = *cat_id;

            match &plg.xtream_cluster {
                XtreamCluster::Live => &mut cat_live_col,
                XtreamCluster::Series => &mut cat_series_col,
                XtreamCluster::Video => &mut cat_vod_col,
            }.push(json!({
              crate::model::XC_TAG_CATEGORY_ID: format!("{}", &cat_id),
              crate::model::XC_TAG_CATEGORY_NAME: plg.title.clone(),
              crate::model::XC_TAG_PARENT_ID: 0
            }));

            for pli in &mut plg.channels {
                let header = &mut pli.header;
                header.category_id = *cat_id;
                let col = match header.xtream_cluster {
                    XtreamCluster::Live => &mut live_col,
                    XtreamCluster::Series => &mut series_col,
                    XtreamCluster::Video => &mut vod_col,
                };
                col.push(pli);
            }
        }
    }

    for (col_path, data) in [
        (get_collection_path(&path, storage_const::COL_CAT_LIVE), &cat_live_col),
        (get_collection_path(&path, storage_const::COL_CAT_VOD), &cat_vod_col),
        (get_collection_path(&path, storage_const::COL_CAT_SERIES), &cat_series_col),
    ] {
        match json_write_documents_to_file(&col_path, data) {
            Ok(()) => {}
            Err(err) => {
                errors.push(format!("Persisting collection failed: {}: {err}", col_path.display()));
            }
        }
    }

    match write_playlists_to_file(
        cfg,
        &path,
        vec![
            (XtreamCluster::Live, &live_col),
            (XtreamCluster::Video, &vod_col),
            (XtreamCluster::Series, &series_col),
        ],
    ) {
        Ok(()) => {
            if let Err(err) = xtream_garbage_collect(cfg, &target.name) {
                if err.kind() != ErrorKind::NotFound {
                    errors.push(format!("Garbage collection failed:{err}"));
                }
            }
        }
        Err(err) => {
            errors.push(format!("Persisting collection failed:{err}"));
        }
    }

    if !errors.is_empty() {
        return create_tuliprox_error_result!(
            TuliproxErrorKind::Notify,
            "{}",
            errors.join("\n")
        );
    }

    Ok(())
}

pub fn xtream_get_collection_path(
    cfg: &Config,
    target_name: &str,
    collection_name: &str,
) -> Result<(Option<PathBuf>, Option<String>), Error> {
    if let Some(path) = xtream_get_storage_path(cfg, target_name) {
        let col_path = get_collection_path(&path, collection_name);
        if col_path.exists() {
            return Ok((Some(col_path), None));
        }
    }
    Err(str_to_io_error(&format!("Cant find collection: {target_name}/{collection_name}")))
}

fn xtream_read_item_for_stream_id(
    cfg: &AppConfig,
    stream_id: u32,
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Result<XtreamPlaylistItem, Error> {
    let (xtream_path, idx_path) = xtream_get_file_paths(storage_path, cluster);
    {
        let _file_lock = cfg.file_locks.read_lock(&xtream_path);
        IndexedDocumentDirectAccess::read_indexed_item::<u32, XtreamPlaylistItem>(&xtream_path, &idx_path, &stream_id)
    }
}

fn xtream_read_series_item_for_stream_id(
    cfg: &AppConfig,
    stream_id: u32,
    storage_path: &Path,
) -> Result<XtreamPlaylistItem, Error> {
    let (xtream_path, idx_path) = xtream_get_file_paths_for_series(storage_path);
    {
        let _file_lock = cfg.file_locks.read_lock(&xtream_path);
        IndexedDocumentDirectAccess::read_indexed_item::<u32, XtreamPlaylistItem>(&xtream_path, &idx_path, &stream_id)
    }
}

macro_rules! try_cluster {
    ($xtream_cluster:expr, $item_type:expr, $virtual_id:expr) => {
        $xtream_cluster
            .or_else(|| XtreamCluster::try_from($item_type).ok())
            .ok_or_else(|| str_to_io_error(&format!("Could not determine cluster for xtream item with stream-id {}",$virtual_id)))
    };
}

pub fn xtream_get_item_for_stream_id(
    virtual_id: u32,
    app_config: &AppConfig,
    target: &ConfigTarget,
    xtream_cluster: Option<XtreamCluster>,
) -> Result<(XtreamPlaylistItem, VirtualIdRecord), Error> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str()).ok_or_else(|| str_to_io_error(&format!("Could not find path for target {}", &target.name)))?;
    let storage_path = xtream_get_storage_path(&config, target.name.as_str()).ok_or_else(|| str_to_io_error(&format!("Could not find path for target {} xtream output", &target.name)))?;
    {
        let target_id_mapping_file = get_target_id_mapping_file(&target_path);
        let _file_lock = app_config.file_locks.read_lock(&target_id_mapping_file);

        let mut target_id_mapping = BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&target_id_mapping_file).map_err(|err| str_to_io_error(&format!("Could not load id mapping for target {} err:{err}", target.name)))?;
        let mapping = target_id_mapping.query(&virtual_id).ok_or_else(|| str_to_io_error(&format!("Could not find mapping for target {} and id {}", target.name, virtual_id)))?;
        let result = match mapping.item_type {
            PlaylistItemType::SeriesInfo => {
                xtream_read_series_item_for_stream_id(app_config, virtual_id, &storage_path)
            }
            PlaylistItemType::Series => {
                if let Ok(mut item) = xtream_read_series_item_for_stream_id(app_config, mapping.parent_virtual_id, &storage_path) {
                    item.provider_id = mapping.provider_id;
                    Ok(item)
                } else {
                    xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, XtreamCluster::Series)
                }
            }
            PlaylistItemType::Catchup => {
                let cluster = try_cluster!(xtream_cluster, mapping.item_type, virtual_id)?;
                let mut item = xtream_read_item_for_stream_id(app_config, mapping.parent_virtual_id, &storage_path, cluster)?;
                item.provider_id = mapping.provider_id;
                Ok(item)
            }
            _ => {
                let cluster = try_cluster!(xtream_cluster, mapping.item_type, virtual_id)?;
                xtream_read_item_for_stream_id(app_config, virtual_id, &storage_path, cluster)
            }
        };

        result.map(|xpli| (xpli, mapping))
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

pub fn xtream_write_series_info(
    app_config: &AppConfig,
    target_name: &str,
    series_info_id: u32,
    content: &str,
) -> Result<(), Error> {
    let config = app_config.config.load();
    let target_path = try_option_ok!(get_target_storage_path(&config, target_name));
    let storage_path = try_option_ok!(xtream_get_storage_path(&config, target_name));
    let (info_path, idx_path) = try_option_ok!(xtream_get_info_file_paths(
        &storage_path,
        XtreamCluster::Series
    ));

    {
        let _file_lock = app_config.file_locks.write_lock(&info_path);
        let mut writer = IndexedDocumentWriter::new_append(info_path.clone(), idx_path)?;
        writer.write_doc(series_info_id, content).map_err(|_| str_to_io_error(&format!("failed to write xtream series info for target {target_name}")))?;
        writer.store()?;
    }
    {
        let target_id_mapping_file = get_target_id_mapping_file(&target_path);
        let _file_lock = app_config.file_locks.write_lock(&target_id_mapping_file);
        if let Ok(mut target_id_mapping) = BPlusTreeUpdate::<u32, VirtualIdRecord>::try_new(&target_id_mapping_file) {
            if let Some(record) = target_id_mapping.query(&series_info_id) {
                let new_record = record.copy_update_timestamp();
                let _ = target_id_mapping.update(&series_info_id, new_record);
            }
        }
    }

    Ok(())
}

pub async fn xtream_write_vod_info(
    app_config: &AppConfig,
    target_name: &str,
    virtual_id: u32,
    content: &str,
) -> Result<(), Error> {
    let config = app_config.config.load();
    let storage_path = try_option_ok!(xtream_get_storage_path(&config, target_name));
    let (info_path, idx_path) = try_option_ok!(xtream_get_info_file_paths(&storage_path, XtreamCluster::Video));
    {
        let _file_lock = app_config.file_locks.write_lock(&info_path).await;
        let mut writer = IndexedDocumentWriter::new_append(info_path.clone(), idx_path)?;
        writer.write_doc(virtual_id, content).map_err(|_| str_to_io_error(&format!("failed to write xtream vod info for target {target_name}")))?;
        writer.store()?;
    }
    Ok(())
}

fn xtream_get_series_info_mapping(
    config: &AppConfig,
    target_name: &str,
    series_id: u32,
) -> Option<VirtualIdRecord> {
    xtream_get_info_mapping(config, target_name, series_id).filter(|id_record| !id_record.is_expired())
}

fn xtream_get_info_mapping(app_config: &AppConfig, target_name: &str, info_id: u32) -> Option<VirtualIdRecord> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target_name)?;

    let target_id_mapping_file = get_target_id_mapping_file(&target_path);
    let _file_lock = app_config.file_locks.read_lock(&target_id_mapping_file);
    BPlusTreeQuery::<u32, VirtualIdRecord>::try_new(&target_id_mapping_file).map_err(|err| {
        error!("Could not load id mapping for target {target_name}: {err}");
        str_to_io_error(&format!("ID mapping load error for target {target_name}"))
    }).ok().map(|mut tree| tree.query(&info_id))?
}

// Reads the series info entry if exists
pub fn xtream_load_series_info(
    app_config: &AppConfig,
    target_name: &str,
    series_id: u32,
) -> Option<String> {
    xtream_get_series_info_mapping(app_config, target_name, series_id)?;
    let config = app_config.config.load();
    let storage_path = xtream_get_storage_path(&config, target_name)?;

    let (info_path, idx_path) = xtream_get_info_file_paths(&storage_path, XtreamCluster::Series)?;

    if info_path.exists() && idx_path.exists() {
        {
            let _file_lock = app_config.file_locks.read_lock(&info_path);
            return match IndexedDocumentDirectAccess::read_indexed_item::<u32, String>(&info_path, &idx_path, &series_id) {
                Ok(content) => Some(content),
                Err(err) => {
                    error!("Failed to read series info for id {series_id} for {target_name}: {err}");
                    None
                }
            };
        }
    }
    None
}
fn xtream_get_vod_info_mapping(
    config: &AppConfig,
    target_name: &str,
    vod_id: u32,
) -> Option<VirtualIdRecord> {
    xtream_get_info_mapping(config, target_name, vod_id)
    //.filter(|id_record| !id_record.is_expired())
}

// Reads the vod info entry if exists
pub fn xtream_load_vod_info(
    config: &AppConfig,
    target_name: &str,
    vod_id: u32,
) -> Option<String> {

    // Check if the entry exists; if not, we don't need to look further.
    xtream_get_vod_info_mapping(config, target_name, vod_id).as_ref()?;
    // Entry exists, read db entry
    let target_storage_path = xtream_get_storage_path(&config.config.load(), target_name)?;

    let (info_path, idx_path) = xtream_get_info_file_paths(&target_storage_path, XtreamCluster::Video)?;

    if info_path.exists() && idx_path.exists() {
        {
            let _file_lock = config.file_locks.read_lock(&info_path);
            return IndexedDocumentDirectAccess::read_indexed_item::<u32, String>(&info_path, &idx_path, &vod_id).ok();
        }
    }
    None
}

fn rewrite_xtream_vod_info<P>(
    config: &AppConfig,
    target: &ConfigTarget,
    xtream_output: &XtreamTargetOutput,
    pli: &P,
    user: &ProxyUserCredentials,
    doc: &mut Map<String, Value>,
) -> Result<String, Error> where
    P: PlaylistEntry,
{
    // we need to update the info data.
    if config.is_reverse_proxy_resource_rewrite_enabled() {
        if let Some(Value::Object(info_data)) = doc.get_mut(crate::model::XC_TAG_INFO_DATA) {
            normalize_release_date(info_data); // info data is muteable, so we do not need to clone it.
            let item_type = pli.get_item_type();
            if user.proxy.is_reverse(item_type) && !target.is_force_redirect(item_type) {
                let server_info = config.get_user_server_info(user);
                let url = server_info.get_base_url();
                let resource_url = Some(format!("{url}/resource/movie/{}/{}/{}", user.username, user.password, pli.get_virtual_id()));
                rewrite_doc_urls(resource_url.as_ref(), info_data, storage_const::INFO_REWRITE_FIELDS, crate::model::XC_INFO_RESOURCE_PREFIX);
                // doc.insert(TAG_INFO_DATA, Value::Object(info_data));
            }
        }
    }

    // we need to update the movie data with virtual ids.
    if let Some(Value::Object(movie_data)) = doc.get_mut(crate::model::XC_TAG_MOVIE_DATA) {
        let stream_id = pli.get_virtual_id();
        let category_id = pli.get_category_id().unwrap_or(0);
        movie_data.insert(crate::model::XC_TAG_STREAM_ID.to_string(), Value::Number(serde_json::value::Number::from(stream_id)));
        movie_data.insert(crate::model::XC_TAG_CATEGORY_ID.to_string(), Value::Number(serde_json::value::Number::from(category_id)));
        movie_data.insert(crate::model::XC_TAG_CATEGORY_IDS.to_string(), Value::Array(vec![Value::Number(serde_json::value::Number::from(category_id))]));
        let options = XtreamMappingOptions::from_target_options(target, xtream_output, config);
        if options.skip_video_direct_source {
            movie_data.insert(crate::model::XC_TAG_DIRECT_SOURCE.to_string(), Value::String(String::new()));
        } else {
            movie_data.insert(
                crate::model::XC_TAG_DIRECT_SOURCE.to_string(),
                Value::String(pli.get_provider_url().to_string()),
            );
        }
    }
    let result = serde_json::to_string(&doc).map_err(|_| str_to_io_error("Failed to serialize vod info"))?;

    Ok(result)
}

pub fn rewrite_xtream_vod_info_content<P>(
    config: &AppConfig,
    target: &ConfigTarget,
    xtream_output: &XtreamTargetOutput,
    pli: &P,
    user: &ProxyUserCredentials,
    content: &str,
) -> Result<String, Error> where
    P: PlaylistEntry,
{
    let mut doc = serde_json::from_str::<Map<String, Value>>(content).map_err(|_| str_to_io_error("Failed to parse JSON content"))?;
    rewrite_xtream_vod_info(config, target, xtream_output, pli, user, &mut doc)
}

pub async fn write_and_get_xtream_vod_info<P>(
    app_config: &AppConfig,
    target: &ConfigTarget,
    xtream_output: &XtreamTargetOutput,
    pli: &P,
    user: &ProxyUserCredentials,
    content: &str,
) -> Result<String, Error> where
    P: PlaylistEntry,
{
    let mut doc = serde_json::from_str::<Map<String, Value>>(content).map_err(|_| str_to_io_error("Failed to parse JSON content"))?;
    xtream_write_vod_info(app_config, target.name.as_str(), pli.get_virtual_id(), content).await.ok();
    rewrite_xtream_vod_info(app_config, target, xtream_output, pli, user, &mut doc)
}

async fn rewrite_xtream_series_info<P>(
    app_config: &AppConfig,
    target: &ConfigTarget,
    xtream_output: &XtreamTargetOutput,
    pli: &P,
    user: &ProxyUserCredentials,
    doc: &mut Map<String, Value>,
) -> Result<String, Error> where
    P: PlaylistEntry,
{
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str()).ok_or_else(|| str_to_io_error(&format!("Could not find path for target {}", target.name)))?;

    let resource_url = if app_config.is_reverse_proxy_resource_rewrite_enabled() {
        let item_type = pli.get_item_type();
        if user.proxy.is_reverse(item_type) && !target.is_force_redirect(item_type) {
            let server_info = app_config.get_user_server_info(user);
            let url = server_info.get_base_url();
            Some(format!("{url}/resource/series/{}/{}/{}", user.username, user.password, pli.get_virtual_id()))
        } else {
            None
        }
    } else {
        None
    };
    if resource_url.is_some() {
        // we need to update the info data.
        if let Some(Value::Object(info_data)) = doc.get_mut(crate::model::XC_TAG_INFO_DATA) {
            rewrite_doc_urls(resource_url.as_ref(), info_data, storage_const::INFO_REWRITE_FIELDS, crate::model::XC_INFO_RESOURCE_PREFIX);
        }
        if let Some(Value::Array(seasons_data)) = doc.get_mut(crate::model::XC_TAG_SEASONS_DATA) {
            for season_value in seasons_data {
                if let Value::Object(season_doc) = season_value {
                    if let Some(season_provider_id) = season_doc.get(crate::model::XC_TAG_ID).and_then(get_u32_from_serde_value) {
                        let field_prefix = format!("{}{season_provider_id}_", crate::model::XC_SEASON_RESOURCE_PREFIX);
                        rewrite_doc_urls(resource_url.as_ref(), season_doc, storage_const::INFO_REWRITE_FIELDS, &field_prefix);
                    }
                }
            }
        }
    }

    let episodes = doc.get_mut(crate::model::XC_TAG_EPISODES).and_then(Value::as_object_mut).ok_or_else(|| str_to_io_error("No episodes found in content"))?;

    let virtual_id = pli.get_virtual_id();
    {
        let (mut target_id_mapping, file_lock) = get_target_id_mapping(app_config, &target_path).await;
        let options = XtreamMappingOptions::from_target_options(target, xtream_output, app_config);

        let provider_url = pli.get_provider_url();
        for episode_list in episodes.values_mut().filter_map(Value::as_array_mut) {
            for episode in episode_list.iter_mut().filter_map(Value::as_object_mut) {
                if let Some(episode_provider_id) = episode.get(crate::model::XC_TAG_ID).and_then(get_u32_from_serde_value)
                {
                    let uuid = generate_playlist_uuid(&hex_encode(&pli.get_uuid()), &episode_provider_id.to_string(), PlaylistItemType::Series, &provider_url);
                    let episode_virtual_id = target_id_mapping.get_and_update_virtual_id(
                        &uuid,
                        episode_provider_id,
                        PlaylistItemType::Series,
                        virtual_id,
                    );
                    episode.insert(crate::model::XC_TAG_ID.to_string(), Value::String(episode_virtual_id.to_string()));
                    if resource_url.is_some() {
                        // we need to update the info data.
                        if let Some(Value::Object(info_data)) = episode.get_mut(crate::model::XC_TAG_INFO_DATA) {
                            let field_prefix = format!("{}{episode_provider_id}_", crate::model::XC_INFO_RESOURCE_PREFIX_EPISODE);
                            rewrite_doc_urls(resource_url.as_ref(), info_data, storage_const::INFO_REWRITE_FIELDS, &field_prefix);
                        }
                    }
                }
                if options.skip_series_direct_source {
                    episode.insert(crate::model::XC_TAG_DIRECT_SOURCE.to_string(), Value::String(String::new()));
                }
            }
        }

        if let Err(err) = target_id_mapping.persist() {
            error!("{err}");
        }
        drop(file_lock);
        drop(target_id_mapping);
    }
    let result = serde_json::to_string(&doc).map_err(|_| str_to_io_error("Failed to serialize updated series info"))?;

    Ok(result)
}

pub async fn rewrite_xtream_series_info_content<P>(
    config: &AppConfig,
    target: &ConfigTarget,
    xtream_output: &XtreamTargetOutput,
    pli_series_info: &P,
    user: &ProxyUserCredentials,
    content: &str,
) -> Result<String, Error> where
    P: PlaylistEntry,
{
    let mut doc = serde_json::from_str::<Map<String, Value>>(content).map_err(|_| str_to_io_error("Failed to parse JSON content"))?;
    rewrite_xtream_series_info(config, target, xtream_output, pli_series_info, user, &mut doc).await
}

pub async fn write_and_get_xtream_series_info<P>(
    config: &AppConfig,
    target: &ConfigTarget,
    xtream_output: &XtreamTargetOutput,
    pli_series_info: &P,
    user: &ProxyUserCredentials,
    content: &str,
) -> Result<String, Error> where
    P: PlaylistEntry,
{
    let mut doc = serde_json::from_str::<Map<String, Value>>(content).map_err(|_| str_to_io_error("Failed to parse JSON content"))?;
    let virtual_id = pli_series_info.get_virtual_id();
    xtream_write_series_info(config, target.name.as_str(), virtual_id, content).ok();
    rewrite_xtream_series_info(config, target, xtream_output, pli_series_info, user, &mut doc).await
}

pub fn xtream_get_input_info(
    cfg: &AppConfig,
    input: &ConfigInput,
    provider_id: u32,
    cluster: XtreamCluster,
) -> Option<String> {
    if let Ok(Some((info_path, idx_path))) = get_input_storage_path(&input.name, &cfg.config.load().working_dir).map(|storage_path| xtream_get_info_file_paths(&storage_path, cluster))
    {
        let _file_lock = cfg.file_locks.read_lock(&info_path);
        if let Ok(content) = IndexedDocumentDirectAccess::read_indexed_item::<u32, String>(&info_path, &idx_path, &provider_id) {
            return Some(content);
        }
    }
    None
}

pub async fn xtream_update_input_info_file(
    cfg: &AppConfig,
    input: &ConfigInput,
    wal_path: &Path,
    cluster: XtreamCluster,
) -> Result<(), TuliproxError> {
    let config = cfg.config.load();
    match get_input_storage_path(&input.name, &config.working_dir).map(|storage_path| xtream_get_info_file_paths(&storage_path, cluster)) {
        Ok(Some((info_path, idx_path))) => {
            {
                let _file_lock = cfg.file_locks.write_lock(&info_path);
                let mut reader = file_reader(open_readonly_file(wal_path).map_err(|err| notify_err!(format!("Could not read {cluster} info {err}")))?);
                match IndexedDocumentWriter::<u32>::new_append(info_path.clone(), idx_path) {
                    Ok(mut writer) => {
                        let mut provider_id_bytes = [0u8; 4];
                        let mut length_bytes = [0u8; 4];
                        loop {
                            if reader.read_exact(&mut provider_id_bytes).is_err() {
                                break; // End of file
                            }
                            let provider_id = u32::from_le_bytes(provider_id_bytes);
                            reader.read_exact(&mut length_bytes).map_err(|err| notify_err!(format!("Could not read temporary {cluster} info {err}")))?;
                            let length = u32::from_le_bytes(length_bytes) as usize;
                            let mut buffer = vec![0u8; length];
                            reader.read_exact(&mut buffer).map_err(|err| notify_err!(format!("Could not read temporary {cluster} info {err}")))?;
                            if let Ok(content) = String::from_utf8(buffer) {
                                let _ = writer.write_doc(provider_id, &content);
                            }
                        }
                        writer.store().map_err(|err| notify_err!(format!("Could not store {cluster} info {err}")))?;
                        drop(reader);
                        if let Err(err) = fs::remove_file(wal_path) {
                            error!("Failed to delete WAL file for {cluster} {err}");
                        }
                        Ok(())
                    }
                    Err(err) => Err(notify_err!(format!("Could not create create indexed document writer for {cluster} info {err}"))),
                }
            }
        }
        Ok(None) => Err(notify_err!(format!("Could not create storage path for input {}", &input.name))),
        Err(err) => Err(notify_err!(format!("Could not create storage path for input {err}"))),
    }
}

pub async fn xtream_update_input_vod_record_from_wal_file(
    cfg: &AppConfig,
    input: &ConfigInput,
    wal_path: &Path,
) -> Result<(), TuliproxError> {
    let config = cfg.config.load();
    let record_path = get_input_storage_path(&input.name, &config.working_dir).map(|storage_path| xtream_get_record_file_path(&storage_path, PlaylistItemType::Video))
        .map_err(|err| notify_err!(format!("Error accessing storage path: {err}")))
        .and_then(|opt| opt.ok_or_else(|| notify_err!(format!("Error accessing storage path for input: {}", &input.name))))?;

    {
        let _file_lock = cfg.file_locks.write_lock(&record_path);
        let mut reader = file_reader(open_readonly_file(wal_path).map_err(|err| notify_err!(format!("Could not read vod wal info {err}")))?);
        let mut provider_id_bytes = [0u8; 4];
        let mut tmdb_id_bytes = [0u8; 4];
        let mut ts_bytes = [0u8; 8];
        let mut tree_record_index: BPlusTree<u32, InputVodInfoRecord> = BPlusTree::load(&record_path).unwrap_or_else(|_| BPlusTree::new());
        
        loop {
            if reader.read_exact(&mut provider_id_bytes).is_err() {
                break; // End of file
            }
            let provider_id = u32::from_le_bytes(provider_id_bytes);
            
            if reader.read_exact(&mut tmdb_id_bytes).is_err() {
                error!("Unexpected EOF after reading provider_id {provider_id} for VOD record.");
                break;
            }
            let tmdb_id = u32::from_le_bytes(tmdb_id_bytes);

            if reader.read_exact(&mut ts_bytes).is_err() {
                error!("Unexpected EOF after reading tmdb_id for VOD record with provider_id {provider_id}.");
                break;
            }
            let ts = u64::from_le_bytes(ts_bytes);

            // Read the date string length as a 4-byte u32.
            let mut len_bytes = [0u8; 4];
            if reader.read_exact(&mut len_bytes).is_err() { 
                error!("Unexpected EOF when reading release_date length for VOD record with provider_id {provider_id}.");
                break; 
            }
            let len = u32::from_le_bytes(len_bytes) as usize;

            let release_date = if len > 0 {
                let mut date_buffer = vec![0u8; len];
                if reader.read_exact(&mut date_buffer).is_err() { 
                    error!("Unexpected EOF when reading release_date string for VOD record with provider_id {provider_id}.");
                    break; 
                }
                String::from_utf8(date_buffer).ok()
            } else {
                // If length is 0, set release_date to None
                None
            };

            tree_record_index.insert(provider_id, InputVodInfoRecord { tmdb_id, ts, release_date });
        }

        tree_record_index.store(&record_path).map_err(|err| notify_err!(format!("Could not store vod record info {err}")))?;
        
        drop(reader);
        if let Err(err) = fs::remove_file(wal_path) {
            error!("Failed to delete record WAL file for vod {err}");
        }
        Ok(())
    }
}

pub async fn xtream_update_input_series_record_from_wal_file(
    cfg: &AppConfig,
    input: &ConfigInput,
    wal_path: &Path,
) -> Result<(), TuliproxError> {
    let config = cfg.config.load();
    let record_path = get_input_storage_path(&input.name, &config.working_dir).map(|storage_path| xtream_get_record_file_path(&storage_path, PlaylistItemType::SeriesInfo))
        .map_err(|err| notify_err!(format!("Error accessing storage path: {err}")))
        .and_then(|opt| opt.ok_or_else(|| notify_err!(format!("Error accessing storage path for input: {}", &input.name))))?;
    {
        let _file_lock = cfg.file_locks.write_lock(&record_path);
        let mut reader = file_reader(open_readonly_file(wal_path).map_err(|err| notify_err!(format!("Could not read series wal info {err}")))?);
        let mut provider_id_bytes = [0u8; 4];
        let mut ts_bytes = [0u8; 8];
        let mut tree_record_index: BPlusTree<u32, u64> = BPlusTree::load(&record_path).unwrap_or_else(|_| BPlusTree::new());
        loop {
            if reader.read_exact(&mut provider_id_bytes).is_err() {
                break; // End of file
            }
            let provider_id = u32::from_le_bytes(provider_id_bytes);
            if reader.read_exact(&mut ts_bytes).is_err() {
                break; // End of file
            }
            let ts = u64::from_le_bytes(ts_bytes);
            tree_record_index.insert(provider_id, ts);
        }
        tree_record_index.store(&record_path).map_err(|err| notify_err!(format!("Could not store series record info {err}")))?;
        drop(reader);
        if let Err(err) = fs::remove_file(wal_path) {
            error!("Failed to delete record WAL file for series {err}");
        }
        Ok(())
    }
}

pub async fn xtream_update_input_series_episodes_record_from_wal_file(
    cfg: &AppConfig,
    input: &ConfigInput,
    wal_path: &Path,
) -> Result<(), TuliproxError> {
    let config = cfg.config.load();
    let record_path = get_input_storage_path(&input.name, &config.working_dir).map(|storage_path| xtream_get_record_file_path(&storage_path, PlaylistItemType::Series))
        .map_err(|err| notify_err!(format!("Error accessing storage path: {err}")))
        .and_then(|opt| opt.ok_or_else(|| notify_err!(format!("Error accessing storage path for input: {}", &input.name))))?;
    {
        let _file_lock = cfg.file_locks.write_lock(&record_path);
        let mut reader = file_reader(open_readonly_file(wal_path).map_err(|err| notify_err!(format!("Could not read series episode wal info {err}")))?);
        let mut provider_id_bytes = [0u8; 4];
        let mut len_bytes = [0u8; 4];
        let mut tree_record_index: BPlusTree<u32, XtreamSeriesEpisode> = BPlusTree::load(&record_path).unwrap_or_else(|_| BPlusTree::new());
        let mut buffer = vec![0u8; 4096];
        loop {
            if reader.read_exact(&mut provider_id_bytes).is_err() {
                break; // End of file
            }
            let provider_id = u32::from_le_bytes(provider_id_bytes);
            if reader.read_exact(&mut len_bytes).is_err() {
                break; // End of file
            }
            let len = usize::try_from(u32::from_le_bytes(len_bytes)).unwrap_or(0);
            if len == 0 {
                break;
            }
            if len > buffer.len() {
                buffer = vec![0u8; len];
            }
            if reader.read_exact(&mut buffer[0..len]).is_err() {
                break;
            }
            match bincode_deserialize(&buffer[0..len]) {
                Ok(episode) => {
                    tree_record_index.insert(provider_id, episode);
                }
                Err(err) => {
                    error!("Failed to delete deserialize record WAL file for series episode {err}");
                }
            }
        }
        tree_record_index.store(&record_path).map_err(|err| notify_err!(format!("Could not store series episode record info {err}")))?;
        drop(reader);
        if let Err(err) = fs::remove_file(wal_path) {
            error!("Failed to delete record WAL file for series episode {err}");
        }
        Ok(())
    }
}

pub async fn iter_raw_xtream_playlist(app_config: &AppConfig, target: &ConfigTarget, cluster: XtreamCluster) -> Option<(FileReadGuard, impl Iterator<Item=(XtreamPlaylistItem, bool)>)> {
    let config = app_config.config.load();
    if let Some(storage_path) = xtream_get_storage_path(&config, target.name.as_str()) {
        let (xtream_path, idx_path) = xtream_get_file_paths(&storage_path, cluster);
        if !xtream_path.exists() || !idx_path.exists() {
            return None;
        }
        let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
        match IndexedDocumentIterator::<u32, XtreamPlaylistItem>::new(&xtream_path, &idx_path)
            .map_err(|err| info_err!(format!("Could not deserialize file {xtream_path:?} - {err}"))) {
            Ok(reader) => Some((file_lock, reader)),
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
            // Convert iterator items to Result<Bytes, String>
            let mapped = chans.map(move |(item, has_next)| {
                match serde_json::to_string(&item) {
                    Ok(content) => {
                        Ok(Bytes::from(if has_next {
                            format!("{content},")
                        } else {
                            content
                        }))
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
    if let Ok((Some(file_path), _content)) = path {
        if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
            return serde_json::from_str::<Vec<PlaylistXtreamCategory>>(&content).ok();
        }
    }
    None
}

pub fn write_series_info_to_wal_file(provider_id: u32, ts: u64, content: &str, content_write: &mut BufWriter<&File>, record_writer: &mut BufWriter<&File>) -> std::io::Result<()> {

    let encoded_content = encode_info_content_for_wal_file(provider_id, content)?;
    let encoded_record = encode_series_info_record_for_wal_file(provider_id, ts);
    content_write.write_all(&encoded_content)?;
    record_writer.write_all(&encoded_record)?;
    Ok(())
}


pub fn write_vod_info_to_wal_file(provider_id: u32, content: &str, info_record: &InputVodInfoRecord, content_write: &mut BufWriter<&File>, record_writer: &mut BufWriter<&File>) -> std::io::Result<()> {

    let encoded_content = encode_info_content_for_wal_file(provider_id, content)?;
    let encoded_record = encode_vod_info_record_for_wal_file(provider_id, info_record)?;
    content_write.write_all(&encoded_content)?;
    record_writer.write_all(&encoded_record)?;
    Ok(())
}


fn encode_info_content_for_wal_file(provider_id: u32, content: &str) -> std::io::Result<Vec<u8>> {
    let length = u32::try_from(content.len()).map_err(to_io_error)?;
    let mut buffer = Vec::with_capacity(8 + content.len());

    buffer.extend_from_slice(&provider_id.to_le_bytes());
    buffer.extend_from_slice(&length.to_le_bytes());
    buffer.extend_from_slice(content.as_bytes());

    Ok(buffer)
}


fn encode_vod_info_record_for_wal_file(provider_id: u32, record: &InputVodInfoRecord) -> std::io::Result<Vec<u8>> {
    let estimated_capacity = 30;
    let mut buffer = Vec::with_capacity(estimated_capacity);

    buffer.extend_from_slice(&provider_id.to_le_bytes());
    buffer.extend_from_slice(&record.tmdb_id.to_le_bytes());
    buffer.extend_from_slice(&record.ts.to_le_bytes());

    match &record.release_date {
        Some(date_str) => {
            // Write the length as a 4-byte u32 to prevent overflow.
            let len = u32::try_from(date_str.len()).map_err(to_io_error)?;
            buffer.extend_from_slice(&len.to_le_bytes());
            buffer.extend_from_slice(date_str.as_bytes());
        }
        None => {
            // Write a length of 0 as 4 bytes.
            buffer.extend_from_slice(&0u32.to_le_bytes());
        }
    }

    Ok(buffer)
}


fn encode_series_info_record_for_wal_file(provider_id: u32, ts: u64) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(12);
    buffer.extend_from_slice(&provider_id.to_le_bytes());
    buffer.extend_from_slice(&ts.to_le_bytes());
    buffer
}
