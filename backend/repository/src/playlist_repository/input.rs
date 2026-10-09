use super::{
    load_input_media_server_playlist, persist_input_media_server_playlist, publication::playlist_has_items,
    target::publish_all_cluster_catalogs, InputPlaylistPersistOptions,
};
use crate::{
    get_input_storage_path, load_input_local_library_playlist, load_input_m3u_playlist, load_input_xtream_playlist,
    persist_input_library_playlist, persist_input_m3u_playlist, stalker_repository::get_stalker_storage_path,
    LocalLibraryDiskPlaylistSource, M3uDiskPlaylistSource, MediaServerDiskPlaylistSource, MemoryPlaylistSource,
    PlaylistSource, StalkerDiskPlaylistSource, XtreamDiskPlaylistSource, FILE_SUFFIX_DB,
};
use log::{debug, warn};
use shared::{
    error::TuliproxError,
    model::{xtream_const::XTREAM_CLUSTER, InputPersistence, PlaylistGroup, PlaylistItem, XtreamCluster},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{AppConfig, ConfigInput},
    utils::normalized_source_ordinal,
};

pub async fn persist_input_playlist(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    persist_input_playlist_with_options(app_config, input, playlist, InputPlaylistPersistOptions::default()).await
}

pub async fn persist_input_playlist_with_options(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    mut playlist: Vec<PlaylistGroup>,
    options: InputPlaylistPersistOptions,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    let persistence = input.get_download_input_type().persistence();
    let accepts_empty = persistence == InputPersistence::Library
        || (!options.accepted_empty_clusters.is_empty()
            && matches!(persistence, InputPersistence::Xtream | InputPersistence::Stalker));
    if !playlist_has_items(&playlist) && !accepts_empty {
        let empty_error = TuliproxError::RepositoryPlaylist(format!(
            "Refusing to persist empty playlist for input '{}'; existing data was retained",
            input.name
        ));
        warn!("{empty_error}");
        return match load_input_playlist(app_config, input, None).await {
            Ok(mut previous) => {
                if previous.is_empty() {
                    (playlist, Some(empty_error))
                } else {
                    (previous.take_groups(), Some(empty_error))
                }
            }
            Err(load_err) => (
                playlist,
                Some(TuliproxError::RepositoryPlaylist(format!(
                    "{empty_error}; failed to load the retained input playlist: {load_err}"
                ))),
            ),
        };
    }
    if persistence == InputPersistence::Stalker {
        // The Stalker processor (`processor::stalker::download_stalker_playlist`)
        // is the single writer of the per-cluster B+Tree and raw group catalogs.
        // Re-encoding the `PlaylistItem` runtime projection back into `StalkerPlaylistItem`
        // would destroy the canonical `cmd`/`playback_descriptor`/capability
        // flags the processor just persisted — including the field the
        // runtime 4xx-re-resolve hook relies on. The disk layout is
        // already in sync; nothing to do here.
        return (playlist, None);
    }
    playlist.iter_mut().for_each(PlaylistGroup::on_load);
    let cfg = app_config.config.load();
    let storage_path = match get_input_storage_path(&input.name, &cfg.storage_dir).await {
        Ok(storage_path) => storage_path,
        Err(err) => {
            return (
                playlist,
                Some(TuliproxError::Config(format!(
                    "Error creating input storage directory for input '{}' failed: {err}",
                    input.name
                ))),
            );
        }
    };

    let (persisted_playlist, err) = match persistence {
        InputPersistence::Xtream => {
            crate::persist_input_xtream_playlist_with_empty_replacements(
                app_config,
                &storage_path,
                playlist,
                options.accepted_empty_clusters,
            )
            .await
        }

        InputPersistence::M3u => {
            // Persist M3U
            let file_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
            if let Err(err) = persist_input_m3u_playlist(app_config, &file_path, &playlist).await {
                return (playlist, Some(err));
            }
            (playlist, None)
        }
        InputPersistence::Library => {
            // Persist local library playlist
            let file_path = get_input_local_library_playlist_file_path(&storage_path, &input.name);
            let (playlist, result) = persist_input_library_playlist(app_config, &file_path, playlist).await;
            if let Err(err) = result {
                return (playlist, Some(err));
            }
            (playlist, None)
        }
        InputPersistence::MediaServer => {
            let file_path = get_input_media_server_playlist_file_path(&storage_path, &input.name);
            let (playlist, result) = persist_input_media_server_playlist(app_config, &file_path, playlist).await;
            if let Err(err) = result {
                return (playlist, Some(err));
            }
            (playlist, None)
        }
        InputPersistence::Stalker => unreachable!("handled above"),
    };

    if err.is_none() {
        return publish_all_cluster_catalogs(app_config, &storage_path, &input.name, persisted_playlist).await;
    }

    (persisted_playlist, err)
}

pub async fn load_input_playlist(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    clusters: Option<&[XtreamCluster]>,
) -> Result<PlaylistSource, TuliproxError> {
    let cfg = app_config.config.load();
    let storage_path = get_input_storage_path(&input.name, &cfg.storage_dir)
        .await
        .map_err(|e| TuliproxError::Config(format!("Error getting input path: {e}")))?;
    let disk_based_processing = cfg.disk_based_processing;

    match input.get_download_input_type().persistence() {
        InputPersistence::Xtream => {
            let clusters_to_load = clusters.unwrap_or(&XTREAM_CLUSTER);
            if disk_based_processing {
                let source =
                    PlaylistSource::xtream_disk(XtreamDiskPlaylistSource::new(app_config, &storage_path).await?);
                Ok(PlaylistSource::filtered(source, skipped_clusters(clusters_to_load)))
            } else {
                let groups = load_input_xtream_playlist(app_config, &storage_path, clusters_to_load).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::M3u => {
            // Load M3U
            let file_path = get_input_m3u_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(PlaylistSource::m3u_disk(M3uDiskPlaylistSource::new(app_config, &file_path).await?))
            } else {
                let groups = load_input_m3u_playlist(app_config, &file_path).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::Library => {
            let file_path = get_input_local_library_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(PlaylistSource::local_library_disk(
                    LocalLibraryDiskPlaylistSource::new(app_config, &file_path).await?,
                ))
            } else {
                let groups = load_input_local_library_playlist(app_config, &file_path).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::MediaServer => {
            let file_path = get_input_media_server_playlist_file_path(&storage_path, &input.name);
            if disk_based_processing && file_path.exists() {
                Ok(PlaylistSource::media_server_disk(MediaServerDiskPlaylistSource::new(app_config, &file_path).await?))
            } else {
                let groups = load_input_media_server_playlist(app_config, &file_path).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
        InputPersistence::Stalker => {
            let clusters_to_load = clusters.unwrap_or(&XTREAM_CLUSTER);
            let stalker_path = get_stalker_storage_path(&storage_path);
            let stalker_config = input.stalker.as_ref().ok_or_else(|| {
                TuliproxError::ConfigInput(format!("Stalker input '{}' has no Stalker configuration", input.name))
            })?;
            let portal_url = input.resolve_url(&input.url)?.into_owned();
            // A read path: when the published manifest belongs to a different identity the
            // input simply has nothing to serve yet, so fall back to an empty manifest
            // instead of replacing the publication state of the refresh that owns it.
            let (manifest, published) = crate::stalker_generation_repository::readable_active_manifest(
                &stalker_path,
                stalker_config.identity_fingerprint(&portal_url),
            )
            .await?;
            if !published {
                debug!(
                    "Stalker input '{}' has no published catalog for its current identity; serving an empty playlist",
                    input.name
                );
            }
            if disk_based_processing {
                let source = PlaylistSource::stalker_disk(
                    StalkerDiskPlaylistSource::new(app_config, &stalker_path, Arc::clone(&input.name), manifest)
                        .await?,
                );
                Ok(PlaylistSource::filtered(source, skipped_clusters(clusters_to_load)))
            } else {
                let groups = load_input_stalker_playlist(app_config, &input.name, clusters_to_load, &manifest).await?;
                Ok(MemoryPlaylistSource::new(groups).into_source())
            }
        }
    }
}

pub(super) fn skipped_clusters(clusters_to_load: &[XtreamCluster]) -> HashSet<XtreamCluster> {
    XTREAM_CLUSTER.iter().copied().filter(|cluster| !clusters_to_load.contains(cluster)).collect()
}

pub fn get_input_m3u_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    storage_path.join(format!("m3u_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}

pub fn get_input_local_library_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    storage_path.join(format!("lib_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}

pub fn get_input_media_server_playlist_file_path(storage_path: &Path, input_name: &Arc<str>) -> PathBuf {
    let sanitized_input_name: String = input_name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    storage_path.join(format!("media_server_{sanitized_input_name}.{FILE_SUFFIX_DB}"))
}

/// Load a Stalker input's playlist into memory. The on-disk B+Tree is the
/// source of truth; we stream every per-cluster tree and bucket the items
/// by cluster to build the runtime `PlaylistGroup`s. `input_name` seeds the
/// canonical `PlaylistItem::from_stalker` conversion so item identity matches
/// the download path.
pub async fn load_input_stalker_playlist(
    app_config: &Arc<AppConfig>,
    input_name: &str,
    clusters: &[XtreamCluster],
    manifest: &crate::stalker_generation_repository::StalkerActiveManifest,
) -> Result<Vec<PlaylistGroup>, TuliproxError> {
    let mut groups_map: indexmap::IndexMap<(XtreamCluster, u32), PlaylistGroup> = indexmap::IndexMap::new();
    for &cluster in clusters {
        let mut batches = Vec::new();
        match cluster {
            XtreamCluster::Live => {
                if let Some(files) = manifest.live.as_ref() {
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.data).await?);
                }
            }
            XtreamCluster::Video => {
                if let Some(files) = manifest.vod.as_ref() {
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.data).await?);
                }
            }
            XtreamCluster::Series => {
                if let Some(files) = manifest.series.as_ref() {
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.roots).await?);
                    batches.push(crate::stalker_repository::load_stalker_items_at(app_config, &files.episodes).await?);
                }
            }
        }
        for batch in batches {
            for item in batch {
                let category_id = item.category_id;
                let playlist_item = PlaylistItem::from_stalker(&item, input_name);
                groups_map
                    .entry((cluster, category_id))
                    .or_insert_with(|| PlaylistGroup {
                        id: category_id,
                        title: Arc::clone(&playlist_item.header.group),
                        channels: Vec::new(),
                        xtream_cluster: cluster,
                    })
                    .channels
                    .push(playlist_item);
            }
        }
    }
    let mut groups: Vec<PlaylistGroup> = groups_map.into_values().collect();
    for group in &mut groups {
        group.channels.sort_by_key(|item| normalized_source_ordinal(item.header.source_ordinal));
    }
    groups.sort_by_key(|group| {
        group.channels.first().map_or(u32::MAX, |c| normalized_source_ordinal(c.header.source_ordinal))
    });
    Ok(groups)
}
