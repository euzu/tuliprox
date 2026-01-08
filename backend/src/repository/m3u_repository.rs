use crate::api::model::AppState;
use crate::model::{AppConfig, ProxyUserCredentials};
use crate::model::{Config, ConfigTarget, M3uTargetOutput};
use crate::repository::bplustree::{BPlusTree, BPlusTreeQuery};
use crate::repository::m3u_playlist_iterator::M3uPlaylistM3uTextIterator;
use crate::repository::storage::get_target_storage_path;
use crate::repository::storage_const;
use crate::utils;
use crate::utils::{async_file_writer, IO_BUFFER_SIZE};
use indexmap::IndexMap;
use log::error;
use shared::error::{notify_err, info_err, string_to_io_error, str_to_io_error, TuliproxError};
use shared::model::{M3uPlaylistItem, PlaylistGroup};
use shared::model::{PlaylistItem, PlaylistItemType, XtreamCluster};
use std::io::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::task;

macro_rules! cant_write_result {
    ($path:expr, $err:expr) => {
        notify_err!("failed to write m3u playlist: {} - {}", $path.display() ,$err)
    }
}

pub fn m3u_get_file_path(target_path: &Path) -> PathBuf {
    target_path.join(PathBuf::from(format!("{}.{}", storage_const::FILE_M3U, storage_const::FILE_SUFFIX_DB)))
}

pub fn m3u_get_epg_file_path(target_path: &Path) -> PathBuf {
    let path = target_path.join(PathBuf::from(format!("{}.{}", storage_const::FILE_M3U, storage_const::FILE_SUFFIX_DB)));
    utils::add_prefix_to_filename(&path, "epg_", Some("xml"))
}

macro_rules! await_playlist_write {
    ($expr:expr, $fmt:literal $(, $args:expr)* ) => {{
        $expr.await.map_err(|err| {
            notify_err!($fmt $(, $args)*, err)
        })?
    }};
}

async fn persist_m3u_playlist_as_text(
    cfg: &Config,
    target: &ConfigTarget,
    target_output: &M3uTargetOutput,
    m3u_playlist: Arc<Vec<M3uPlaylistItem>>,
) -> Result<(), TuliproxError> {
    let Some(filename) = target_output.filename.as_ref() else { return Ok(()); };
    let Some(m3u_filename) = utils::get_file_path(&cfg.working_dir, Some(PathBuf::from(filename))) else { return Ok(()); };

    let file = await_playlist_write!(fs::File::create(&m3u_filename), "Can't write m3u plain playlist {} - {}", m3u_filename.display());
    // Larger buffer for sequential writes to reduce syscalls
    let mut writer = async_file_writer(file);
    await_playlist_write!(writer.write_all(b"#EXTM3U\n"), "Failed to write header to {} - {}", m3u_filename.display());

    let mut write_counter = 0usize;

    for m3u in m3u_playlist.iter() {
        let line = m3u.to_m3u(target.options.as_ref(), false);
        let bytes = line.as_bytes();
        await_playlist_write!(writer.write_all(bytes), "Failed to write entry to {} - {}", m3u_filename.display());
        await_playlist_write!(writer.write_all(b"\n"), "Failed to write newline to {} - {}", m3u_filename.display());
        write_counter += bytes.len() + 1;
        if write_counter >= IO_BUFFER_SIZE {
            await_playlist_write!(writer.flush(), "Failed to flush {} - {}", m3u_filename.display());
            write_counter = 0;
        }
    }

    await_playlist_write!(writer.flush(), "Failed to flush {} - {}", m3u_filename.display());

    Ok(())
}

pub async fn m3u_write_playlist(
    cfg: &AppConfig,
    target: &ConfigTarget,
    target_output: &M3uTargetOutput,
    target_path: &Path,
    new_playlist: &[PlaylistGroup],
) -> Result<(), TuliproxError> {
    if new_playlist.is_empty() {
        return Ok(());
    }

    let m3u_path = m3u_get_file_path(target_path);
    let m3u_playlist = Arc::new(
        new_playlist
            .iter()
            .flat_map(|pg| &pg.channels)
            .filter(|&pli| !matches!(pli.header.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo))
            .map(M3uPlaylistItem::from)
            .collect::<Vec<M3uPlaylistItem>>(),
    );

    let file_lock = cfg.file_locks.write_lock(&m3u_path).await;

    if let Err(err) = persist_m3u_playlist_as_text(&cfg.config.load(), target, target_output, Arc::clone(&m3u_playlist)).await {
        error!("Persisting m3u playlist failed: {err}");
    }

    let playlist = Arc::clone(&m3u_playlist);
    let m3u_path_clone = m3u_path.clone();

    task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        let mut tree = BPlusTree::new();
        for m3u in playlist.iter() {
            tree.insert(m3u.virtual_id, m3u.clone());
        }
        tree.store(&m3u_path_clone).map_err(|err| cant_write_result!(&m3u_path_clone, err))?;
        Ok(())
    })
        .await
        .map_err(|err| notify_err!("failed to write m3u playlist: {} - {err}", m3u_path.display()))??;

    Ok(())
}

pub async fn m3u_load_rewrite_playlist(
    cfg: &AppConfig,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
) -> Result<M3uPlaylistM3uTextIterator, TuliproxError> {
    M3uPlaylistM3uTextIterator::new(cfg, target, user).await
}

pub async fn m3u_get_item_for_stream_id(stream_id: u32, app_state: &AppState, target: &ConfigTarget) -> Result<M3uPlaylistItem, Error> {
    if stream_id < 1 {
        return Err(str_to_io_error("id should start with 1"));
    }
    {
        if let Some(playlist) = app_state.playlists.data.read().await.get(target.name.as_str()) {
            if let Some(m3u_playlist) = playlist.m3u.as_ref() {
                if let Some(item) = m3u_playlist.query(&stream_id) {
                    return Ok(item.clone());
                }
                // fall through to disk lookup on cache miss
            }
        }

        let cfg: &AppConfig = &app_state.app_config;
        let target_path = get_target_storage_path(&cfg.config.load(), target.name.as_str()).ok_or_else(|| string_to_io_error(format!("Could not find path for target {}", &target.name)))?;
        let m3u_path = m3u_get_file_path(&target_path);
        let _file_lock = cfg.file_locks.read_lock(&m3u_path).await;

        let mut query = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path)?;
        match query.query(&stream_id) {
            Ok(Some(item)) => Ok(item),
            Ok(None) => Err(string_to_io_error(format!("Item not found: {stream_id}"))),
            Err(err) => Err(string_to_io_error(format!("Query failed for {stream_id}: {err}"))),
        }
    }
}

pub async fn iter_raw_m3u_playlist(config: &AppConfig, target: &ConfigTarget) -> Option<(utils::FileReadGuard, impl Iterator<Item=(M3uPlaylistItem, bool)>)> {
    let target_path = get_target_storage_path(&config.config.load(), target.name.as_str())?;
    let m3u_path = m3u_get_file_path(&target_path);
    if let Ok(false) = tokio::fs::try_exists(&m3u_path).await {
        return None;
    }
    let file_lock = config.file_locks.read_lock(&m3u_path).await;
    match BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path)
        .map_err(|err| info_err!("Could not open BPlusTreeQuery {m3u_path:?} - {err}")) {
        Ok(mut query) => {
            let items: Vec<M3uPlaylistItem> = query.iter().map(|(_, v)| v).collect();
            let len = items.len();
            Some((file_lock, items.into_iter().enumerate().map(move |(i, v)| (v, i < len - 1))))
        }
        Err(_) => None
    }
}

pub async fn persist_input_m3u_playlist(app_config: &Arc<AppConfig>, m3u_path: &Path, playlist: &[PlaylistGroup]) -> Result<(), TuliproxError> {

    let file_lock = app_config.file_locks.write_lock(m3u_path).await;
    let m3u_path_clone = m3u_path.to_path_buf();

    let playlist_items: Vec<M3uPlaylistItem> = playlist
        .iter()
        .flat_map(|pg| &pg.channels)
        .map(M3uPlaylistItem::from)
        .collect();

    task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        let mut tree = BPlusTree::new();
        for m3u in &playlist_items {
            tree.insert(m3u.provider_id.clone(), m3u.clone());
        }
        tree.store(&m3u_path_clone).map_err(|err| cant_write_result!(&m3u_path_clone, err))?;
        Ok(())
    })
    .await
    .map_err(|err| notify_err!("failed to write m3u playlist: {} - {err}", m3u_path.display()))??;

    Ok(())
}

pub async fn load_input_m3u_playlist(app_config: &Arc<AppConfig>, m3u_path: &Path) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    let mut groups: IndexMap<Arc<str>, PlaylistGroup> = IndexMap::new();

    if tokio::fs::try_exists(m3u_path).await.unwrap_or(false) {
        // Load Items
        let _file_lock = app_config.file_locks.read_lock(m3u_path).await;
        if let Ok(mut query) = BPlusTreeQuery::<String, M3uPlaylistItem>::try_new(m3u_path) {
            let mut group_cnt = 0;
            for (_, ref item) in query.iter() {
                let cat_id = item.group.clone();
                groups.entry(cat_id)
                    .or_insert_with(|| {
                        group_cnt += 1;
                        PlaylistGroup {
                            id: group_cnt,
                            title: item.group.clone(),
                            channels: Vec::new(),
                            xtream_cluster: XtreamCluster::try_from(item.item_type).unwrap_or(XtreamCluster::Live),
                        }
                    })
                    .channels.push(PlaylistItem::from(item));
            }
        }
    }

    Ok(groups.into_values().collect())
}
