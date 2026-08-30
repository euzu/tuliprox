use super::playlist_mem_cache::PlaylistStorageState;
use crate::{
    bplustree::{BPlusTree, BPlusTreeQuery},
    error_macros::{await_playlist_write, cant_read_result, cant_write_result},
    m3u_playlist_iterator::M3uPlaylistM3uTextIterator,
    playlist_backend::{ensure_storage_path, iter_raw_playlist, M3u, PlaylistBackend},
    playlist_repository::get_input_m3u_playlist_file_path,
    storage::{get_input_storage_path, get_target_storage_path},
    storage_const,
    xtream_repository::CategoryKey,
    LockedReceiverStream,
};
use indexmap::IndexMap;
use log::{error, warn};
use shared::{
    concat_string,
    error::{str_to_io_error, string_to_io_error, TuliproxError},
    model::{
        LiveStreamProperties, M3uPlaylistItem, PlaylistGroup, PlaylistItem, PlaylistItemType, StreamProperties,
        XtreamCluster,
    },
    utils::PROVIDER_SCHEME_PREFIX,
};
use std::{
    collections::HashMap,
    io::Error,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{fs, io::AsyncWriteExt, task};
use tuliprox_core::{
    model::{AppConfig, Config, ConfigInput, ConfigTarget, M3uTargetOutput, ProxyUserCredentials},
    utils::{async_file_writer, file_exists_async},
};

pub fn m3u_get_file_path_for_db(target_path: &Path) -> PathBuf {
    target_path.join(storage_const::PATH_M3U).join(concat_string!(
        storage_const::FILE_M3U,
        ".",
        storage_const::FILE_SUFFIX_DB
    ))
}

pub fn m3u_get_epg_file_path_for_target(target_path: &Path) -> PathBuf {
    target_path.join(storage_const::PATH_M3U).join(concat_string!("epg.", storage_const::FILE_SUFFIX_DB))
}

#[inline]
pub fn m3u_get_storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> { M3u::storage_path(cfg, target_name) }

#[inline]
pub async fn ensure_m3u_storage_path(cfg: &Config, target_name: &str) -> Result<PathBuf, TuliproxError> {
    ensure_storage_path::<M3u>(cfg, target_name).await
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
        RepositoryM3u,
        fs::File::create(m3u_filename),
        "Can't write m3u plain playlist {} - {}",
        m3u_filename.display()
    );
    // Larger buffer for sequential writes to reduce syscalls
    let mut writer = async_file_writer(file);
    await_playlist_write!(
        RepositoryM3u,
        writer.write_all(b"#EXTM3U\n"),
        "Failed to write header to {} - {}",
        m3u_filename.display()
    );

    for m3u in m3u_playlist {
        let line = build_line(m3u)?;
        let bytes = line.as_bytes();
        await_playlist_write!(
            RepositoryM3u,
            writer.write_all(bytes),
            "Failed to write entry to {} - {}",
            m3u_filename.display()
        );
        await_playlist_write!(
            RepositoryM3u,
            writer.write_all(b"\n"),
            "Failed to write newline to {} - {}",
            m3u_filename.display()
        );
    }

    await_playlist_write!(RepositoryM3u, writer.flush(), "Failed to flush {} - {}", m3u_filename.display());

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
        RepositoryM3u,
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
    let cfg = app_config.config.load();
    let Some(m3u_filename) = tuliprox_core::utils::get_file_path(&cfg.storage_dir, Some(PathBuf::from(filename)))
    else {
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

            let input = provider_input_by_name.get(&m3u.input_name).ok_or_else(|| {
                TuliproxError::RepositoryM3u(format!(
                    "Input '{}' not found for provider URL resolution",
                    m3u.input_name
                ))
            })?;

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
                RepositoryM3u,
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
                RepositoryM3u,
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
            .map_err(|err| cant_write_result!(RepositoryM3u, "m3u", &m3u_path_clone, err))?;
        Ok(())
    })
    .await
    .map_err(|err| {
        TuliproxError::RepositoryM3u(format!("failed to write m3u playlist: {} - {err}", m3u_path.display()))
    })??;

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
    app_config: &AppConfig,
    playlists: &PlaylistStorageState,
    target: &ConfigTarget,
) -> Result<M3uPlaylistItem, Error> {
    if stream_id == 0 {
        return Err(str_to_io_error("id should start with 1"));
    }
    {
        if let Some(playlist) = playlists.data.read().await.get(target.name.as_str()) {
            if let Some(m3u_playlist) = playlist.m3u.as_ref() {
                if let Some(item) = m3u_playlist.query(&stream_id) {
                    return Ok(item.clone());
                }
                // fall through to disk lookup on cache miss
            }
        }

        let cfg: &AppConfig = app_config;
        let target_path = get_target_storage_path(&cfg.config.load(), target.name.as_str())
            .ok_or_else(|| string_to_io_error(format!("Could not find path for target {}", target.name)))?;
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

/// Keep items belonging to `cluster`; keep everything when `None`.
///
/// Returned as an opaque closure so `iter_raw_playlist` monomorphizes over it
/// rather than taking a boxed predicate.
fn m3u_cluster_filter(cluster: Option<XtreamCluster>) -> impl Fn(&M3uPlaylistItem) -> bool + Send + 'static {
    move |item| cluster.is_none_or(|wanted| item.item_type.is_cluster(wanted))
}

pub async fn iter_raw_m3u_target_playlist(
    config: &AppConfig,
    target: &ConfigTarget,
    cluster: Option<XtreamCluster>,
) -> Option<LockedReceiverStream<Result<M3uPlaylistItem, TuliproxError>>> {
    let target_path = get_target_storage_path(&config.config.load(), target.name.as_str())?;
    let m3u_path = m3u_get_file_path_for_db(&target_path);

    iter_raw_playlist::<M3u, u32, _>(config, &m3u_path, m3u_cluster_filter(cluster)).await
}

pub async fn iter_raw_m3u_input_playlist(
    app_config: &AppConfig,
    input: &ConfigInput,
    cluster: Option<XtreamCluster>,
) -> Option<LockedReceiverStream<Result<M3uPlaylistItem, TuliproxError>>> {
    let cfg = app_config.config.load();
    let storage_path = get_input_storage_path(&input.name, &cfg.storage_dir).await.ok()?;
    let m3u_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);

    iter_raw_playlist::<M3u, Arc<str>, _>(app_config, &m3u_path, m3u_cluster_filter(cluster)).await
}

pub async fn persist_input_m3u_playlist(
    app_config: &Arc<AppConfig>,
    m3u_path: &Path,
    playlist: &[PlaylistGroup],
) -> Result<(), TuliproxError> {
    let file_lock = app_config.file_locks.write_lock(m3u_path).await;
    let m3u_path_clone = m3u_path.to_path_buf();

    let mut playlist_items: Vec<M3uPlaylistItem> =
        playlist.iter().flat_map(|pg| &pg.channels).map(M3uPlaylistItem::from).collect();

    task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        if let Err(err) = merge_preserved_m3u_live_metadata(&m3u_path_clone, &mut playlist_items) {
            warn!(
                "Failed to preserve learned Live metadata during M3U playlist rewrite for {}: {err}",
                m3u_path_clone.display()
            );
        }
        let mut tree = BPlusTree::new();
        for m3u in &playlist_items {
            tree.insert(m3u.provider_id.clone(), m3u.clone());
        }
        tree.store(&m3u_path_clone).map_err(|err| cant_write_result!(RepositoryM3u, "m3u", &m3u_path_clone, err))?;
        Ok(())
    })
    .await
    .map_err(|err| {
        TuliproxError::RepositoryM3u(format!("failed to write m3u playlist: {} - {err}", m3u_path.display()))
    })??;

    Ok(())
}

fn merge_preserved_m3u_live_metadata(m3u_path: &Path, playlist_items: &mut [M3uPlaylistItem]) -> Result<(), String> {
    if !m3u_path.exists() {
        return Ok(());
    }

    let mut stored = BPlusTreeQuery::<Arc<str>, M3uPlaylistItem>::try_new(m3u_path)
        .map_err(|err| format!("failed to open existing input playlist: {err}"))?;

    for item in playlist_items {
        if item.provider_id.is_empty() {
            continue;
        }
        let previous = stored
            .query(&item.provider_id)
            .map_err(|err| format!("failed to read existing input playlist item: {err}"))?;
        let Some(StreamProperties::Live(previous)) = previous.and_then(|item| item.additional_properties) else {
            continue;
        };

        match item.additional_properties.as_mut() {
            Some(StreamProperties::Live(current)) => {
                current.merge_learned_metadata_from(&previous);
            }
            None if item.item_type.is_live() => {
                let mut current = LiveStreamProperties::default();
                if current.merge_learned_metadata_from(&previous) {
                    item.additional_properties = Some(StreamProperties::Live(Box::new(current)));
                }
            }
            Some(StreamProperties::Video(_) | StreamProperties::Series(_) | StreamProperties::Episode(_)) | None => {}
        }
    }

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
        let mut query = BPlusTreeQuery::<Arc<str>, M3uPlaylistItem>::try_new(&m3u_path)
            .map_err(|error| TuliproxError::RepositoryM3u(error.to_string()))?;
        let mut group_cnt = 0;
        for entry in query.iter() {
            let (_, item) = entry.map_err(|error| TuliproxError::RepositoryM3u(error.to_string()))?;
            let cluster = item.item_type.cluster();
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
                .push(PlaylistItem::from(&item));
        }
        Ok(groups.into_values().collect())
    })
    .await
    .map_err(|err| cant_read_result!(RepositoryM3u, "m3u", &m3u_path_err, err))??;

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::{persist_input_m3u_playlist, replace_m3u_url_line};
    use crate::BPlusTreeQuery;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{
            ConfigPaths, LiveStreamProperties, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType,
            StreamProperties, XtreamCluster,
        },
        utils::Internable,
    };
    use std::sync::Arc;
    use tuliprox_core::{
        model::{AppConfig, Config, MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };

    fn test_app_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::default()),
            api_proxy: Arc::new(ArcSwapOption::default()),
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
            custom_stream_response: Arc::new(ArcSwapOption::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        })
    }

    fn live_playlist(properties: Option<LiveStreamProperties>) -> Vec<PlaylistGroup> {
        vec![PlaylistGroup {
            id: 1,
            title: "Live".intern(),
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    id: "provider-1".intern(),
                    input_stream_id: "provider-1".intern(),
                    name: "Channel".intern(),
                    group: "Live".intern(),
                    item_type: PlaylistItemType::Live,
                    xtream_cluster: XtreamCluster::Live,
                    additional_properties: properties.map(|properties| StreamProperties::Live(Box::new(properties))),
                    ..PlaylistItemHeader::default()
                },
            }],
            xtream_cluster: XtreamCluster::Live,
        }]
    }

    #[test]
    fn replace_m3u_url_line_trims_trailing_whitespace() {
        let mut line =
            "#EXTINF:-1 tvg-id=\"1\" tvg-name=\"Test\" group-title=\"G\",Title\nhttp://old\n\n  \t".to_string();
        replace_m3u_url_line(&mut line, "http://new");
        assert_eq!(line, "#EXTINF:-1 tvg-id=\"1\" tvg-name=\"Test\" group-title=\"G\",Title\nhttp://new");
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

    #[tokio::test]
    async fn persist_input_m3u_playlist_preserves_learned_live_metadata_across_full_rewrite() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = test_app_config();
        let learned = LiveStreamProperties {
            video: Some("learned-video".intern()),
            audio: Some("learned-audio".intern()),
            last_probed_timestamp: Some(100),
            last_success_timestamp: Some(90),
            bitrate: 2_500_000,
            ..LiveStreamProperties::default()
        };

        persist_input_m3u_playlist(&app_config, &path, &live_playlist(Some(learned)))
            .await
            .expect("initial playlist should persist");
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(None))
            .await
            .expect("rewritten playlist should persist");

        let mut query = BPlusTreeQuery::<Arc<str>, shared::model::M3uPlaylistItem>::try_new(&path)
            .expect("rewritten playlist should open");
        let item = query
            .query(&"provider-1".intern())
            .expect("rewritten playlist should be readable")
            .expect("live item should remain present");
        let Some(StreamProperties::Live(properties)) = item.additional_properties else {
            panic!("expected live properties");
        };

        assert_eq!(properties.video.as_deref(), Some("learned-video"));
        assert_eq!(properties.audio.as_deref(), Some("learned-audio"));
        assert_eq!(properties.last_probed_timestamp, Some(100));
        assert_eq!(properties.last_success_timestamp, Some(90));
        assert_eq!(properties.bitrate, 2_500_000);
    }
}
