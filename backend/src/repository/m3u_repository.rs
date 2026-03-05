use crate::{
    api::model::AppState,
    model::{AppConfig, Config, ConfigInput, ConfigTarget, M3uTargetOutput, ProxyUserCredentials},
    repository::{
        bplustree::{BPlusTree, BPlusTreeQuery},
        m3u_playlist_iterator::M3uPlaylistM3uTextIterator,
        open_playlist_reader,
        playlist_repository::get_input_m3u_playlist_file_path,
        storage::{get_file_path_for_db_index, get_input_storage_path, get_target_storage_path},
        storage_const,
        xtream_repository::CategoryKey,
        LockedReceiverStream,
    },
    utils,
    utils::{async_file_writer, file_exists_async},
};
use indexmap::IndexMap;
use log::error;
use serde::{Deserialize, Serialize};
use shared::{
    concat_string,
    error::{notify_err, str_to_io_error, string_to_io_error, TuliproxError},
    model::{M3uPlaylistItem, PlaylistGroup, PlaylistItem, PlaylistItemType, XtreamCluster},
    notify_err_res,
    utils::PROVIDER_SCHEME_PREFIX,
};
use std::{
    collections::HashMap,
    io::Error,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use tokio::{fs, io::AsyncWriteExt, task};
use tokio_stream::Stream;

macro_rules! cant_write_result {
    ($path:expr, $err:expr) => {
        notify_err!("failed to write m3u playlist: {} - {}", $path.display(), $err)
    };
}

macro_rules! await_playlist_write {
    ($expr:expr, $fmt:literal $(, $args:expr)* ) => {{
        $expr.await.map_err(|err| {
            notify_err!($fmt $(, $args)*, err)
        })?
    }};
}

pub fn m3u_get_file_path_for_db(target_path: &Path) -> PathBuf {
    target_path.join(storage_const::PATH_M3U).join(concat_string!(
        storage_const::FILE_M3U,
        ".",
        storage_const::FILE_SUFFIX_DB
    ))
}

pub fn m3u_get_epg_file_path_for_target(target_path: &Path) -> PathBuf {
    let path = target_path.join(storage_const::PATH_M3U).join(concat_string!(
        storage_const::FILE_M3U,
        ".",
        storage_const::FILE_SUFFIX_DB
    ));
    utils::add_prefix_to_filename(&path, "epg_", Some(storage_const::FILE_SUFFIX_DB))
}

pub fn m3u_get_storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> {
    get_target_storage_path(cfg, target_name)
        .map(|target_path| target_path.join(PathBuf::from(storage_const::PATH_M3U)))
}

pub async fn ensure_m3u_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    if let Some(path) = m3u_get_storage_path(cfg, target_name) {
        if tokio::fs::create_dir_all(&path).await.is_err() {
            let msg = format!("Failed to save m3u data, can't create directory {}", &path.display());
            return notify_err_res!("{msg}");
        }
        Ok(path)
    } else {
        let msg = format!("Failed to save m3u data, can't create directory for target {target_name}");
        notify_err_res!("{msg}")
    }
}

fn provider_m3u_filename(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    let extension = path.extension().map(|ext| ext.to_string_lossy());
    let new_name = match extension {
        Some(ext) if !ext.is_empty() => concat_string!(&stem, "_provider.", &ext),
        _ => concat_string!(&stem, "_provider"),
    };
    path.with_file_name(new_name)
}

async fn write_m3u_text_file<F>(
    m3u_filename: &Path,
    m3u_playlist: &[M3uPlaylistItem],
    mut build_line: F,
) -> Result<(), TuliproxError>
where
    F: FnMut(&M3uPlaylistItem) -> Result<String, TuliproxError>,
{
    let file = await_playlist_write!(
        fs::File::create(m3u_filename),
        "Can't write m3u plain playlist {} - {}",
        m3u_filename.display()
    );
    // Larger buffer for sequential writes to reduce syscalls
    let mut writer = async_file_writer(file);
    await_playlist_write!(writer.write_all(b"#EXTM3U\n"), "Failed to write header to {} - {}", m3u_filename.display());

    for m3u in m3u_playlist {
        let line = build_line(m3u)?;
        let bytes = line.as_bytes();
        await_playlist_write!(writer.write_all(bytes), "Failed to write entry to {} - {}", m3u_filename.display());
        await_playlist_write!(writer.write_all(b"\n"), "Failed to write newline to {} - {}", m3u_filename.display());
    }

    await_playlist_write!(writer.flush(), "Failed to flush {} - {}", m3u_filename.display());

    Ok(())
}

fn temp_m3u_filename(path: &Path) -> PathBuf {
    let file_name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    path.with_file_name(format!("{file_name}.tmp"))
}

async fn write_m3u_text_file_atomic<F>(
    m3u_filename: &Path,
    m3u_playlist: &[M3uPlaylistItem],
    build_line: F,
) -> Result<(), TuliproxError>
where
    F: FnMut(&M3uPlaylistItem) -> Result<String, TuliproxError>,
{
    let tmp_path = temp_m3u_filename(m3u_filename);
    write_m3u_text_file(&tmp_path, m3u_playlist, build_line).await?;
    await_playlist_write!(
        fs::rename(&tmp_path, m3u_filename),
        "Failed to replace {} - {}",
        m3u_filename.display()
    );
    Ok(())
}

fn replace_m3u_url_line(line: &mut String, url: &str) {
    let trimmed = line.trim_end_matches(|c: char| c == '\n' || c.is_whitespace());
    let mut rebuilt = if let Some((meta, _old_url)) = trimmed.rsplit_once('\n') {
        // Preserve metadata line and a single newline separator.
        let mut out = String::with_capacity(meta.len() + 1 + url.len());
        out.push_str(meta);
        out.push('\n');
        out
    } else {
        let mut out = String::with_capacity(trimmed.len() + 1 + url.len());
        out.push_str(trimmed);
        out.push('\n');
        out
    };

    rebuilt.push_str(url);
    *line = rebuilt;
}

async fn cleanup_temp_file(temp_file: &Path) {
    if let Err(err) = fs::remove_file(temp_file).await {
        if err.kind() != std::io::ErrorKind::NotFound {
            error!("Failed to cleanup temp file {} - {err}", temp_file.display());
        }
    }
}

async fn persist_m3u_playlist_as_text(
    app_config: &AppConfig,
    target: &ConfigTarget,
    target_output: &M3uTargetOutput,
    m3u_playlist: &[M3uPlaylistItem],
) -> Result<(), TuliproxError> {
    let Some(filename) = target_output.filename.as_ref() else {
        return Ok(());
    };
    let storage_dir = &app_config.config.load().storage_dir;
    let Some(m3u_filename) = utils::get_file_path(storage_dir, Some(PathBuf::from(filename))) else {
        return Ok(());
    };

    let target_options = target.options.as_ref();
    let sources = app_config.sources.load();
    let provider_input_by_name: HashMap<Arc<str>, Arc<ConfigInput>> = if let Some(source) =
        sources.sources.iter().find(|source| source.targets.iter().any(|t| t.name == target.name))
    {
        sources
            .inputs
            .iter()
            .filter(|input| input.url.starts_with(PROVIDER_SCHEME_PREFIX))
            .filter(|input| source.inputs.contains(&input.name))
            .map(|input| (Arc::clone(&input.name), Arc::clone(input)))
            .collect()
    } else {
        HashMap::new()
    };

    if provider_input_by_name.is_empty() {
        write_m3u_text_file_atomic(&m3u_filename, m3u_playlist, |m3u| Ok(m3u.to_m3u(target_options, false))).await?;
    } else {
        let provider_filename = provider_m3u_filename(&m3u_filename);
        let provider_tmp = temp_m3u_filename(&provider_filename);
        let m3u_tmp = temp_m3u_filename(&m3u_filename);

        write_m3u_text_file(&provider_tmp, m3u_playlist, |m3u| Ok(m3u.to_m3u(target_options, false))).await?;

        write_m3u_text_file(&m3u_tmp, m3u_playlist, |m3u| {
            let effective_url = if m3u.t_stream_url.is_empty() { &m3u.url } else { &m3u.t_stream_url };
            if !effective_url.starts_with(PROVIDER_SCHEME_PREFIX) {
                return Ok(m3u.to_m3u(target_options, false));
            }

            let input = provider_input_by_name
                .get(&m3u.input_name)
                .ok_or_else(|| notify_err!("Input '{}' not found for provider URL resolution", m3u.input_name))?;

            let resolved = input.resolve_url(effective_url)?;
            if resolved.as_ref() == effective_url.as_ref() {
                return Ok(m3u.to_m3u(target_options, false));
            }

            let mut line = m3u.to_m3u(target_options, false);
            replace_m3u_url_line(&mut line, resolved.as_ref());
            Ok(line)
        })
            .await?;

        if let Err(rename_err) = async {
            await_playlist_write!(
                fs::rename(&provider_tmp, &provider_filename),
                "Failed to replace {} - {}",
                provider_filename.display()
            );
            Ok::<(), TuliproxError>(())
        }
            .await
        {
            cleanup_temp_file(&provider_tmp).await;
            cleanup_temp_file(&m3u_tmp).await;
            return Err(rename_err);
        }

        if let Err(rename_err) = async {
            await_playlist_write!(
                fs::rename(&m3u_tmp, &m3u_filename),
                "Failed to replace {} - {}",
                m3u_filename.display()
            );
            Ok::<(), TuliproxError>(())
        }
            .await
        {
            cleanup_temp_file(&m3u_tmp).await;
            return Err(rename_err);
        }
    }

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

    let config = cfg.config.load();
    let _m3u_path = ensure_m3u_storage_path(&config, target.name.as_str()).await?;

    let m3u_path = m3u_get_file_path_for_db(target_path);
    let m3u_playlist = new_playlist
        .iter()
        .flat_map(|pg| &pg.channels)
        .filter(|&pli| {
            !matches!(pli.header.item_type, PlaylistItemType::SeriesInfo | PlaylistItemType::LocalSeriesInfo)
        })
        .map(M3uPlaylistItem::from)
        .collect::<Vec<M3uPlaylistItem>>();

    let file_lock = cfg.file_locks.write_lock(&m3u_path).await;

    persist_m3u_playlist_as_text(cfg, target, target_output, &m3u_playlist).await?;

    let m3u_path_clone = m3u_path.clone();

    // Move all B+Tree building and I/O to spawn_blocking
    task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        let mut tree = BPlusTree::new();
        for m3u in m3u_playlist {
            tree.insert(m3u.virtual_id, m3u);
        }
        tree.store_with_index(&m3u_path_clone, |pli| pli.source_ordinal)
            .map_err(|err| cant_write_result!(&m3u_path_clone, err))?;
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

pub async fn m3u_get_item_for_stream_id(
    stream_id: u32,
    app_state: &AppState,
    target: &ConfigTarget,
) -> Result<M3uPlaylistItem, Error> {
    if stream_id == 0 {
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
        let target_path = get_target_storage_path(&cfg.config.load(), target.name.as_str())
            .ok_or_else(|| string_to_io_error(format!("Could not find path for target {}", &target.name)))?;
        let m3u_path = m3u_get_file_path_for_db(&target_path);
        let file_lock = cfg.file_locks.read_lock(&m3u_path).await;
        let m3u_path_clone = m3u_path.clone();

        task::spawn_blocking(move || -> Result<M3uPlaylistItem, Error> {
            let _guard = file_lock;
            let mut query = BPlusTreeQuery::<u32, M3uPlaylistItem>::try_new(&m3u_path_clone)?;
            match query.query_zero_copy(&stream_id) {
                Ok(Some(item)) => Ok(item),
                Ok(None) => Err(string_to_io_error(format!("Item not found: {stream_id}"))),
                Err(err) => Err(string_to_io_error(format!("Query failed for {stream_id}: {err}"))),
            }
        })
            .await
            .map_err(|err| string_to_io_error(format!("Query task failed for {stream_id}: {err}")))?
    }
}

pub async fn iter_raw_m3u_target_playlist(
    config: &AppConfig,
    target: &ConfigTarget,
    cluster: Option<XtreamCluster>,
) -> Option<Box<dyn Stream<Item=Result<M3uPlaylistItem, TuliproxError>> + Send + Unpin>> {
    let target_path = get_target_storage_path(&config.config.load(), target.name.as_str())?;
    let m3u_path = m3u_get_file_path_for_db(&target_path);

    iter_raw_m3u_playlist::<u32, u32>(config, &m3u_path, cluster).await
}

pub async fn iter_raw_m3u_input_playlist(
    app_config: &AppConfig,
    input: &ConfigInput,
    cluster: Option<XtreamCluster>,
) -> Option<Box<dyn Stream<Item=Result<M3uPlaylistItem, TuliproxError>> + Send + Unpin>> {
    let storage_dir = &app_config.config.load().storage_dir;
    let storage_path = get_input_storage_path(&input.name, storage_dir).await.ok()?;
    let m3u_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);

    iter_raw_m3u_playlist::<u32, Arc<str>>(app_config, &m3u_path, cluster).await
}

async fn iter_raw_m3u_playlist<SortKey, ItemKey>(
    app_config: &AppConfig,
    m3u_path: &Path,
    cluster: Option<XtreamCluster>,
) -> Option<Box<dyn Stream<Item=Result<M3uPlaylistItem, TuliproxError>> + Send + Unpin>>
where
    ItemKey: Ord + Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static,
    SortKey: for<'de> Deserialize<'de> + Send + 'static,
{
    // Two read locks: iter_lock is held by LockedReceiverStream for the consumer lifetime,
    // while bg_lock is moved into spawn_blocking to guard the on-disk reader.
    let iter_lock = app_config.file_locks.read_lock(m3u_path).await;
    if !file_exists_async(m3u_path).await {
        return None;
    }
    let bg_lock = app_config.file_locks.read_lock(m3u_path).await;

    let m3u_path = m3u_path.to_path_buf();
    let index_path = get_file_path_for_db_index(&m3u_path);
    let (tx, rx) = mpsc::channel::<Result<M3uPlaylistItem, TuliproxError>>(256);

    let m3u_path_for_log = m3u_path.clone();
    let err_tx = tx.clone();
    let join_err_tx = tx.clone();
    let handle = task::spawn_blocking(move || {
        let _guard = bg_lock;
        let reader = match open_playlist_reader::<ItemKey, M3uPlaylistItem, SortKey>(
            &m3u_path,
            &index_path,
            None,
        ) {
            Ok(reader) => reader,
            Err(err) => {
                let _ = err_tx.blocking_send(Err(err));
                return;
            }
        };

        for entry in reader {
            let item = match entry {
                Ok((_, item)) => item,
                Err(err) => {
                    error!("M3U playlist reader error: {err}");
                    continue;
                }
            };
            if cluster.is_none_or(|c| item.item_type.is_cluster(c))
                && tx.blocking_send(Ok(item)).is_err()
            {
                break;
            }
        }
    });
    tokio::spawn(async move {
        if let Err(err) = handle.await {
            error!("M3U playlist reader task failed for {}: {err}", m3u_path_for_log.display());
            let _ = join_err_tx.send(Err(notify_err!(
                "M3U playlist reader task failed for {}: {err}",
                m3u_path_for_log.display()
            )))
                .await;
        }
    });

    let stream: Box<dyn Stream<Item=Result<M3uPlaylistItem, TuliproxError>> + Send + Unpin> =
        Box::new(LockedReceiverStream::new(rx, iter_lock));
    Some(stream)
}

pub async fn persist_input_m3u_playlist(
    app_config: &Arc<AppConfig>,
    m3u_path: &Path,
    playlist: &[PlaylistGroup],
) -> Result<(), TuliproxError> {
    let file_lock = app_config.file_locks.write_lock(m3u_path).await;
    let m3u_path_clone = m3u_path.to_path_buf();

    let playlist_items: Vec<M3uPlaylistItem> =
        playlist.iter().flat_map(|pg| &pg.channels).map(M3uPlaylistItem::from).collect();

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

pub async fn load_input_m3u_playlist(
    app_config: &Arc<AppConfig>,
    m3u_path: &Path,
) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    if !file_exists_async(m3u_path).await {
        return Ok(Vec::new());
    }

    let file_lock = app_config.file_locks.read_lock(m3u_path).await;
    let m3u_path = m3u_path.to_path_buf();
    let m3u_path_err = m3u_path.clone();

    let groups = task::spawn_blocking(move || -> Result<Vec<PlaylistGroup>, TuliproxError> {
        let _guard = file_lock;
        let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
        if let Ok(mut query) = BPlusTreeQuery::<Arc<str>, M3uPlaylistItem>::try_new(&m3u_path) {
            let mut group_cnt = 0;
            for (_, ref item) in query.iter() {
                let cluster = XtreamCluster::try_from(item.item_type).unwrap_or(XtreamCluster::Live);
                let key = (cluster, item.group.clone());
                groups
                    .entry(key)
                    .or_insert_with(|| {
                        group_cnt += 1;
                        PlaylistGroup {
                            id: group_cnt,
                            title: item.group.clone(),
                            channels: Vec::new(),
                            xtream_cluster: cluster,
                        }
                    })
                    .channels
                    .push(PlaylistItem::from(item));
            }
        }
        Ok(groups.into_values().collect())
    })
        .await
        .map_err(|err| notify_err!("failed to read m3u playlist: {} - {err}", m3u_path_err.display()))??;

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::replace_m3u_url_line;

    #[test]
    fn replace_m3u_url_line_trims_trailing_whitespace() {
        let mut line = "#EXTINF:-1 tvg-id=\"1\" tvg-name=\"Test\" group-title=\"G\",Title\nhttp://old\n\n  \t".to_string();
        replace_m3u_url_line(&mut line, "http://new");
        assert_eq!(
            line,
            "#EXTINF:-1 tvg-id=\"1\" tvg-name=\"Test\" group-title=\"G\",Title\nhttp://new"
        );
    }

    #[test]
    fn replace_m3u_url_line_handles_crlf() {
        let mut line = "#EXTINF:-1,Title\r\nhttp://old\r\n".to_string();
        replace_m3u_url_line(&mut line, "http://new");
        assert_eq!(line, "#EXTINF:-1,Title\r\nhttp://new");
    }

    #[test]
    fn replace_m3u_url_line_handles_no_metadata() {
        let mut line = "http://old".to_string();
        replace_m3u_url_line(&mut line, "http://new");
        assert_eq!(line, "http://old\nhttp://new");
    }
}
