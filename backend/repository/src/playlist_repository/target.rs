#[cfg(test)]
use super::publication::TargetCacheReloadStage;
use super::{publication::TargetPersistenceMode, PlaylistM3uStorage, PlaylistXtreamStorage, TargetPlaylistStorage};
use crate::{
    get_target_id_mapping_file, get_target_storage_path, m3u_get_file_path_for_db, xtream_get_file_path,
    xtream_get_storage_path, BPlusTree, TargetIdMapping, VirtualIdRecord,
};
use log::{debug, warn};
use shared::{
    error::TuliproxError,
    model::{M3uPlaylistItem, PlaylistGroup, VirtualId, XtreamCluster, XtreamPlaylistItem},
};
use std::path::Path;
use tuliprox_core::{
    model::{AppConfig, ConfigTarget, TargetOutput},
    utils,
};

pub async fn get_target_id_mapping(
    cfg: &AppConfig,
    target_path: &Path,
    use_memory_cache: bool,
) -> Result<(TargetIdMapping, utils::FileWriteGuard), TuliproxError> {
    let target_id_mapping_file = get_target_id_mapping_file(target_path);
    let file_lock = cfg.file_locks.write_lock(&target_id_mapping_file).await;
    let mapping_path = target_id_mapping_file.clone();
    let mapping =
        tokio::task::spawn_blocking(move || TargetIdMapping::new(&mapping_path, use_memory_cache)).await.map_err(
            |err| TuliproxError::Config(format!("spawn_blocking failed while creating TargetIdMapping: {err}")),
        )??;

    Ok((mapping, file_lock))
}

async fn load_target_id_mapping_as_tree(
    app_config: &AppConfig,
    target_path: &Path,
    target: &ConfigTarget,
) -> Result<BPlusTree<VirtualId, VirtualIdRecord>, TuliproxError> {
    let target_id_mapping_file = get_target_id_mapping_file(target_path);
    let _file_lock = app_config.file_locks.read_lock(&target_id_mapping_file).await;

    // Move B+Tree load to spawn_blocking to avoid blocking tokio runtime
    let path_clone = target_id_mapping_file.clone();
    let target_name = target.name.clone();
    tokio::task::spawn_blocking(move || BPlusTree::<VirtualId, VirtualIdRecord>::load(&path_clone))
        .await
        .map_err(|e| TuliproxError::Config(format!("Blocking task failed: {e}")))?
        .map_err(|err| TuliproxError::Config(format!("Could not find path for target {target_name} err:{err}")))
}

async fn load_xtream_playlist_as_tree(
    app_config: &AppConfig,
    storage_path: &Path,
    cluster: XtreamCluster,
) -> Result<BPlusTree<u32, XtreamPlaylistItem>, TuliproxError> {
    let xtream_path = xtream_get_file_path(storage_path, cluster);
    let file_lock = app_config.file_locks.read_lock(&xtream_path).await;
    // Move B+Tree query and iteration to spawn_blocking to avoid blocking tokio runtime
    let path_clone = xtream_path.clone();
    match tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        BPlusTree::<u32, XtreamPlaylistItem>::load(&path_clone)
    })
    .await
    {
        Ok(Ok(tree)) => Ok(tree),
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!("No xtream {cluster} storage at {}, serving empty playlist", xtream_path.display());
            Ok(BPlusTree::new())
        }
        Ok(Err(err)) => Err(TuliproxError::RepositoryXtream(format!(
            "Failed to load xtream {cluster} storage {}: {err}",
            xtream_path.display()
        ))),
        Err(join_err) => Err(TuliproxError::RepositoryXtream(format!(
            "Failed to join xtream {cluster} storage load task {}: {join_err}",
            xtream_path.display()
        ))),
    }
}

async fn load_id_mapping_target_storage(
    app_config: &AppConfig,
    target: &ConfigTarget,
) -> Result<BPlusTree<VirtualId, VirtualIdRecord>, TuliproxError> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str())
        .ok_or_else(|| TuliproxError::Config(format!("Could not find path for target {}", target.name)))?;

    load_target_id_mapping_as_tree(app_config, &target_path, target).await
}

pub async fn load_xtream_target_storage(
    app_config: &AppConfig,
    target: &ConfigTarget,
) -> Result<PlaylistXtreamStorage, TuliproxError> {
    let config = app_config.config.load();

    let storage_path = xtream_get_storage_path(&config, target.name.as_str()).ok_or_else(|| {
        TuliproxError::Config(format!("Could not find path for target {} xtream output", target.name))
    })?;

    let live_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Live).await?;
    let vod_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Video).await?;
    let series_storage = load_xtream_playlist_as_tree(app_config, &storage_path, XtreamCluster::Series).await?;

    Ok(PlaylistXtreamStorage { live: live_storage, vod: vod_storage, series: series_storage })
}

pub async fn load_m3u_target_storage(
    app_config: &AppConfig,
    target: &ConfigTarget,
) -> Result<PlaylistM3uStorage, TuliproxError> {
    let config = app_config.config.load();
    let target_path = get_target_storage_path(&config, target.name.as_str())
        .ok_or_else(|| TuliproxError::Config(format!("Could not find path for target {}", target.name)))?;

    let m3u_path = m3u_get_file_path_for_db(&target_path);
    let file_lock = app_config.file_locks.read_lock(&m3u_path).await;

    let path_clone = m3u_path.clone();
    match tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        BPlusTree::<u32, M3uPlaylistItem>::load(&path_clone)
    })
    .await
    {
        Ok(Ok(tree)) => Ok(tree),
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            debug!("No m3u storage at {}, serving empty playlist", m3u_path.display());
            Ok(BPlusTree::new())
        }
        Ok(Err(err)) => {
            Err(TuliproxError::RepositoryM3u(format!("Failed to load m3u storage {}: {err}", m3u_path.display())))
        }
        Err(join_err) => Err(TuliproxError::RepositoryM3u(format!(
            "Failed to join m3u storage load task {}: {join_err}",
            m3u_path.display()
        ))),
    }
}

fn target_cache_reload_error(target: &ConfigTarget, component: &str, error: impl std::fmt::Display) -> TuliproxError {
    TuliproxError::RepositoryPlaylist(format!(
        "Target '{}' was persisted but its {component} could not be reloaded into the memory cache: {error}",
        target.name
    ))
}

pub(super) async fn load_target_memory_cache_snapshot(
    app_config: &AppConfig,
    target: &ConfigTarget,
    mode: TargetPersistenceMode,
) -> Result<TargetPlaylistStorage, TuliproxError> {
    #[cfg(not(test))]
    let _ = mode;

    #[cfg(test)]
    if matches!(mode, TargetPersistenceMode::FailCacheReloadAt(TargetCacheReloadStage::IdMapping)) {
        return Err(target_cache_reload_error(target, "ID mapping", "injected reload failure"));
    }
    let id_mapping = load_id_mapping_target_storage(app_config, target)
        .await
        .map_err(|error| target_cache_reload_error(target, "ID mapping", error))?;

    let xtream = if target.output.iter().any(|output| matches!(output, TargetOutput::Xtream(_))) {
        #[cfg(test)]
        if matches!(mode, TargetPersistenceMode::FailCacheReloadAt(TargetCacheReloadStage::XtreamStorage)) {
            return Err(target_cache_reload_error(target, "Xtream storage", "injected reload failure"));
        }
        Some(
            load_xtream_target_storage(app_config, target)
                .await
                .map_err(|error| target_cache_reload_error(target, "Xtream storage", error))?,
        )
    } else {
        None
    };

    let m3u = if target.output.iter().any(|output| matches!(output, TargetOutput::M3u(_))) {
        Some(
            load_m3u_target_storage(app_config, target)
                .await
                .map_err(|error| target_cache_reload_error(target, "M3U storage", error))?,
        )
    } else {
        None
    };

    Ok(TargetPlaylistStorage { xtream, m3u, id_mapping: Some(id_mapping) })
}

pub(super) async fn publish_all_cluster_catalogs(
    app_config: &AppConfig,
    storage_path: &Path,
    input_name: &str,
    persisted_playlist: Vec<PlaylistGroup>,
) -> (Vec<PlaylistGroup>, Option<TuliproxError>) {
    let mut live_groups = Vec::new();
    let mut vod_groups = Vec::new();
    let mut series_groups = Vec::new();
    for group in &persisted_playlist {
        match group.xtream_cluster {
            XtreamCluster::Live => live_groups.push(group.title.to_string()),
            XtreamCluster::Video => vod_groups.push(group.title.to_string()),
            XtreamCluster::Series => series_groups.push(group.title.to_string()),
        }
    }
    for (cluster, groups) in
        [(XtreamCluster::Live, live_groups), (XtreamCluster::Video, vod_groups), (XtreamCluster::Series, series_groups)]
    {
        if let Err(publish_err) =
            crate::publish_raw_group_catalog(storage_path, input_name, cluster, groups, &app_config.file_locks).await
        {
            warn!(
                "Playlist data for input '{input_name}' was persisted, but publishing its raw group catalog for cluster {cluster:?} failed: {publish_err}"
            );
        }
    }
    (persisted_playlist, None)
}
