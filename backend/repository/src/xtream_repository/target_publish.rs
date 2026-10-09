#[cfg(all(not(unix), not(windows)))]
use super::sync_published_file_parent;
#[cfg(unix)]
use super::sync_published_file_parent;
#[cfg(test)]
use super::TargetEmptyReplacementFailure;
use super::{
    cluster_flag, ensure_xtream_storage_path, get_collection_path, get_live_cat_collection_path, get_map_item_as_str,
    get_series_cat_collection_path, get_vod_cat_collection_path, target_category_lock_path,
    xtream_cluster_category_collection, xtream_get_file_path, CategoryEntry, CategoryKey,
};
#[cfg(windows)]
use super::{encode_windows_path, sync_published_file_parent};
use crate::{
    bplustree::{get_file_path_for_db_index, BPlusTree, BPlusTreeStagingArtifacts, BPlusTreeUpdate},
    error_macros::cant_write_result,
    playlist_backend::PlaylistKey,
    storage_const,
};
use indexmap::IndexMap;
use log::warn;
use serde_json::Value;
use shared::{
    error::TuliproxError,
    model::{ClusterFlags, PlaylistGroup, XtreamCluster, XtreamPlaylistItem},
    utils::{get_u32_from_serde_value, Internable},
};
#[cfg(unix)]
use std::fs;
use std::{
    collections::HashMap,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tuliprox_core::{
    model::{AppConfig, ConfigTarget},
    utils::{
        file_exists_async, file_reader, json_write_documents_to_file, remove_file_if_exists,
        require_same_parent_directory,
    },
};
use uuid::Uuid;

/// Persist `collections`, keyed by whatever `key_of` extracts.
///
/// The key space is a type parameter rather than a runtime tag. These stores are
/// keyed by [`VirtualId`] on the target path and by [`ProviderId`] on the input
/// path -- the same file layout and value type, two different id spaces -- and a
/// `StorageKey` enum matched per item used to be the only thing recording which.
/// Both keys are `#[serde(transparent)]` over `u32`, so the on-disk encoding is
/// unchanged either way (see the codec test in `backend/btree`).
pub(super) async fn write_playlists_to_file<K, F>(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    with_index: bool,
    key_of: F,
    collections: Vec<(XtreamCluster, Vec<XtreamPlaylistItem>)>,
    replace_empty_clusters: ClusterFlags,
) -> Result<(), TuliproxError>
where
    K: PlaylistKey,
    F: Fn(&XtreamPlaylistItem) -> K + Copy + Send + 'static,
{
    for (cluster, playlist) in collections {
        if playlist.is_empty() && !replace_empty_clusters.contains(cluster_flag(cluster)) {
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
            let mut tree = BPlusTree::<K, XtreamPlaylistItem>::new();
            for item in playlist {
                let key = key_of(&item);
                tree.insert(key, item);
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
        .map_err(|err| cant_write_result!(RepositoryXtream, "xtream", &xtream_path, err))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub(super) enum TargetEmptyReplacementMode {
    #[default]
    Persist,
    #[cfg(test)]
    FailAt(TargetEmptyReplacementFailure),
    #[cfg(test)]
    PauseDuringPublication(TargetEmptyPublicationHook),
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(super) struct TargetEmptyPublicationHook {
    pub(super) backup_window_entered: Arc<std::sync::Barrier>,
    pub(super) resume_publication: Arc<std::sync::Barrier>,
}

#[cfg(test)]
impl TargetEmptyPublicationHook {
    pub(super) fn new() -> Self {
        Self {
            backup_window_entered: Arc::new(std::sync::Barrier::new(2)),
            resume_publication: Arc::new(std::sync::Barrier::new(2)),
        }
    }
}

struct TargetEmptyClusterPaths {
    published_database: PathBuf,
    published_index: PathBuf,
    published_categories: PathBuf,
    staging_database: PathBuf,
    staging_index: PathBuf,
    staging_categories: PathBuf,
}

impl TargetEmptyClusterPaths {
    pub(super) fn new(storage_path: &Path, cluster: XtreamCluster) -> Self {
        let token = Uuid::new_v4().simple();
        let cluster_name = cluster.as_str().to_lowercase();
        let published_database = xtream_get_file_path(storage_path, cluster);
        let staging_database = storage_path.join(format!(".{cluster_name}.force-empty-{token}.db"));
        Self {
            published_index: get_file_path_for_db_index(&published_database),
            published_categories: get_collection_path(storage_path, xtream_cluster_category_collection(cluster)),
            staging_index: get_file_path_for_db_index(&staging_database),
            staging_categories: storage_path.join(format!(".{cluster_name}.force-empty-{token}.json")),
            published_database,
            staging_database,
        }
    }
}

struct TargetFileReplacement {
    published: PathBuf,
    staging: PathBuf,
    backup: PathBuf,
    previous_moved: bool,
    replacement_published: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetFileMoveMode {
    PreserveDestination,
    ReplaceDestination,
}

#[cfg(unix)]
pub(super) fn move_target_file_platform(source: &Path, destination: &Path, mode: TargetFileMoveMode) -> io::Result<()> {
    require_same_parent_directory(source, destination)?;
    if mode == TargetFileMoveMode::PreserveDestination && destination.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("target transaction destination already exists: {}", destination.display()),
        ));
    }
    fs::rename(source, destination)
}

#[cfg(windows)]
pub(super) fn move_target_file_platform(source: &Path, destination: &Path, mode: TargetFileMoveMode) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    require_same_parent_directory(source, destination)?;
    let source_encoded = encode_windows_path(source)?;
    let destination_encoded = encode_windows_path(destination)?;
    let flags = MOVEFILE_WRITE_THROUGH
        | if mode == TargetFileMoveMode::ReplaceDestination { MOVEFILE_REPLACE_EXISTING } else { 0 };

    // SAFETY: both buffers are live, immutable, and NUL-terminated for the
    // duration of the call. The same-directory check prevents a cross-volume
    // move from degrading into a copy.
    let result = unsafe { MoveFileExW(source_encoded.as_ptr(), destination_encoded.as_ptr(), flags) };
    if result == 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!(
                "failed to move target transaction file {} to {} with Windows write-through semantics: {error}",
                source.display(),
                destination.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(all(not(unix), not(windows)))]
pub(super) fn move_target_file_platform(
    source: &Path,
    destination: &Path,
    _mode: TargetFileMoveMode,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "durable target transaction moves are unsupported on this platform: {} -> {}",
            source.display(),
            destination.display()
        ),
    ))
}

impl TargetFileReplacement {
    pub(super) fn new(published: &Path, staging: &Path, token: uuid::fmt::Simple) -> Self {
        let filename =
            published.file_name().map_or_else(|| "target-artifact".into(), |name| name.to_string_lossy().into_owned());
        Self {
            published: published.to_path_buf(),
            staging: staging.to_path_buf(),
            backup: published.with_file_name(format!(".{filename}.force-empty-backup-{token}")),
            previous_moved: false,
            replacement_published: false,
        }
    }
}

fn cleanup_target_empty_staging(
    staging_artifacts: &BPlusTreeStagingArtifacts,
    staging_categories: &Path,
) -> io::Result<()> {
    let database_result = staging_artifacts.remove_owned_staging_artifacts();
    let category_result = remove_file_if_exists(staging_categories);
    match (database_result, category_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(database_error), Err(category_error)) => Err(io::Error::new(
            database_error.kind(),
            format!("{database_error}; category staging cleanup also failed: {category_error}"),
        )),
    }
}

fn rollback_target_file_replacements(replacements: &mut [TargetFileReplacement]) -> io::Result<()> {
    let mut errors = Vec::new();
    for replacement in replacements.iter_mut().rev() {
        if replacement.previous_moved {
            if let Err(error) = move_target_file_platform(
                &replacement.backup,
                &replacement.published,
                TargetFileMoveMode::ReplaceDestination,
            ) {
                errors.push(format!(
                    "failed to restore {} from {}: {error}",
                    replacement.published.display(),
                    replacement.backup.display()
                ));
            }
        } else if replacement.replacement_published {
            if let Err(error) = move_target_file_platform(
                &replacement.published,
                &replacement.backup,
                TargetFileMoveMode::ReplaceDestination,
            ) {
                errors.push(format!(
                    "failed to withdraw replacement {} during rollback: {error}",
                    replacement.published.display()
                ));
            } else if let Err(error) = remove_file_if_exists(&replacement.backup) {
                errors
                    .push(format!("failed to remove withdrawn replacement {}: {error}", replacement.backup.display()));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("; ")))
    }
}

fn publish_target_file_replacements(
    paths: &TargetEmptyClusterPaths,
    mode: &TargetEmptyReplacementMode,
) -> io::Result<()> {
    #[cfg(not(test))]
    let _ = &mode;
    let staging_artifacts = BPlusTreeStagingArtifacts::new(&paths.published_database, &paths.staging_database)?;
    let token = Uuid::new_v4().simple();
    let mut replacements = vec![
        TargetFileReplacement::new(&paths.published_database, &paths.staging_database, token),
        TargetFileReplacement::new(&paths.published_index, &paths.staging_index, token),
        TargetFileReplacement::new(&paths.published_categories, &paths.staging_categories, token),
    ];

    let publication = (|| -> io::Result<()> {
        for replacement in &replacements {
            require_same_parent_directory(&replacement.staging, &replacement.published)?;
            if !replacement.staging.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing prepared target artifact {}", replacement.staging.display()),
                ));
            }
        }

        for replacement in &mut replacements {
            if replacement.published.exists() {
                move_target_file_platform(
                    &replacement.published,
                    &replacement.backup,
                    TargetFileMoveMode::PreserveDestination,
                )?;
                replacement.previous_moved = true;
            }
        }

        #[cfg(test)]
        if let TargetEmptyReplacementMode::PauseDuringPublication(hook) = mode {
            hook.backup_window_entered.wait();
            hook.resume_publication.wait();
        }

        for (index, replacement) in replacements.iter_mut().enumerate() {
            move_target_file_platform(
                &replacement.staging,
                &replacement.published,
                TargetFileMoveMode::ReplaceDestination,
            )?;
            replacement.replacement_published = true;
            #[cfg(test)]
            if matches!(mode, TargetEmptyReplacementMode::FailAt(TargetEmptyReplacementFailure::Publication))
                && index == 0
            {
                return Err(io::Error::other("injected target empty-replacement publication failure"));
            }
            #[cfg(not(test))]
            let _ = index;
        }
        sync_published_file_parent(&paths.published_database)?;
        cleanup_target_empty_staging(&staging_artifacts, &paths.staging_categories)
    })();

    if let Err(publication_error) = publication {
        let rollback_result = rollback_target_file_replacements(&mut replacements);
        let durability_result = sync_published_file_parent(&paths.published_database);
        return match (rollback_result, durability_result) {
            (Ok(()), Ok(())) => Err(publication_error),
            (rollback, durability) => Err(io::Error::new(
                publication_error.kind(),
                format!(
                    "{publication_error}; rollback result: {}; rollback directory sync result: {}",
                    rollback.map_or_else(|error| error.to_string(), |()| "ok".to_string()),
                    durability.map_or_else(|error| error.to_string(), |()| "ok".to_string())
                ),
            )),
        };
    }

    for replacement in replacements {
        if replacement.previous_moved {
            if let Err(error) = remove_file_if_exists(&replacement.backup) {
                warn!(
                    "Target empty replacement was published, but backup cleanup failed for {}: {error}",
                    replacement.backup.display()
                );
            }
        }
    }
    Ok(())
}

async fn replace_target_xtream_cluster_with_empty(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    cluster: XtreamCluster,
    mode: TargetEmptyReplacementMode,
) -> Result<(), TuliproxError> {
    #[cfg(not(test))]
    let _ = &mode;
    let paths = TargetEmptyClusterPaths::new(storage_path, cluster);
    let staging_artifacts = BPlusTreeStagingArtifacts::new(&paths.published_database, &paths.staging_database)
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!("Failed to prepare empty {cluster} target: {error}"))
        })?;

    let operation = async {
        #[cfg(test)]
        if matches!(&mode, TargetEmptyReplacementMode::FailAt(TargetEmptyReplacementFailure::CategoryPersistence)) {
            return Err(TuliproxError::RepositoryXtream("injected target category persistence failure".to_string()));
        }
        json_write_documents_to_file(&paths.staging_categories, &Vec::<CategoryEntry>::new()).await.map_err(
            |error| {
                TuliproxError::RepositoryXtream(format!(
                    "Failed to prepare empty {cluster} target categories {}: {error}",
                    paths.staging_categories.display()
                ))
            },
        )?;
        let staging_category_file = tokio::fs::File::open(&paths.staging_categories).await.map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to reopen empty {cluster} target categories {}: {error}",
                paths.staging_categories.display()
            ))
        })?;
        staging_category_file.sync_all().await.map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to synchronize empty {cluster} target categories {}: {error}",
                paths.staging_categories.display()
            ))
        })?;

        #[cfg(test)]
        if matches!(&mode, TargetEmptyReplacementMode::FailAt(TargetEmptyReplacementFailure::BTreePersistence)) {
            return Err(TuliproxError::RepositoryXtream("injected target BTree/index persistence failure".to_string()));
        }
        let staging_database = paths.staging_database.clone();
        tokio::task::spawn_blocking(move || {
            BPlusTree::<u32, XtreamPlaylistItem>::new().store_with_index(&staging_database, |item| item.source_ordinal)
        })
        .await
        .map_err(|error| TuliproxError::RepositoryXtream(format!("Empty target BTree task failed: {error}")))?
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to prepare empty {cluster} target BTree/index {}: {error}",
                paths.staging_database.display()
            ))
        })?;

        let category_lock_path = target_category_lock_path(&paths.published_categories);
        let mut lock_paths = [&paths.published_database, &category_lock_path];
        lock_paths.sort_unstable();
        let mut file_locks = Vec::with_capacity(lock_paths.len());
        for path in lock_paths {
            file_locks.push(app_config.file_locks.write_lock(path).await);
        }
        let publication_paths = TargetEmptyClusterPaths {
            published_database: paths.published_database.clone(),
            published_index: paths.published_index.clone(),
            published_categories: paths.published_categories.clone(),
            staging_database: paths.staging_database.clone(),
            staging_index: paths.staging_index.clone(),
            staging_categories: paths.staging_categories.clone(),
        };
        let publication_mode = mode.clone();
        tokio::task::spawn_blocking(move || {
            let result = publish_target_file_replacements(&publication_paths, &publication_mode);
            drop(file_locks);
            result
        })
        .await
        .map_err(|error| TuliproxError::RepositoryXtream(format!("Empty target publish task failed: {error}")))?
        .map_err(|error| {
            TuliproxError::RepositoryXtream(format!(
                "Failed to publish empty {cluster} target cluster atomically: {error}"
            ))
        })
    }
    .await;

    let cleanup = cleanup_target_empty_staging(&staging_artifacts, &paths.staging_categories);
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup_error)) => Err(TuliproxError::RepositoryXtream(format!(
            "Empty {cluster} target cluster was published, but staging cleanup failed: {cleanup_error}"
        ))),
        (Err(error), Err(cleanup_error)) => {
            Err(TuliproxError::RepositoryXtream(format!("{error}; staging cleanup also failed: {cleanup_error}")))
        }
    }
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
        return Err(TuliproxError::RepositoryXtream(format!(
            "BPlusTree file not found for update {}",
            xtream_path.display()
        )));
    }

    // Prepare encoded payload before opening the writer lock.
    let prepared_items =
        BPlusTreeUpdate::<u32, XtreamPlaylistItem>::prepare_upsert_batch(&[(&pli.virtual_id.get(), pli)])
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
    .map_err(|err| cant_write_result!(RepositoryXtream, "xtream", &xtream_path, err))?;

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
        return Err(TuliproxError::RepositoryXtream(format!(
            "BPlusTree file not found for upsert {}",
            xtream_path.display()
        )));
    }

    // Prepare encoded payload before opening the writer lock.
    let virtual_ids: Vec<u32> = pli_list.iter().map(|pli| pli.virtual_id.get()).collect();
    let batch_refs: Vec<(&u32, &XtreamPlaylistItem)> = virtual_ids.iter().zip(pli_list.iter()).collect();
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
    .map_err(|err| cant_write_result!(RepositoryXtream, "xtream", &xtream_path, err))?;

    Ok(())
}

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
            (XtreamCluster::Series, storage_const::COL_CAT_SERIES),
        ] {
            let col_path = get_collection_path(&old_path, cat);
            if col_path.exists() {
                if let Ok(file) = File::open(&col_path) {
                    let reader = file_reader(file);
                    match serde_json::from_reader(reader) {
                        Ok(value) => {
                            if let Value::Array(list) = value {
                                for entry in list {
                                    if let Some(category_id) = entry
                                        .get(tuliprox_core::model::XC_TAG_CATEGORY_ID)
                                        .and_then(get_u32_from_serde_value)
                                    {
                                        if let Value::Object(item) = entry {
                                            if let Some(category_name) =
                                                get_map_item_as_str(&item, tuliprox_core::model::XC_TAG_CATEGORY_NAME)
                                            {
                                                result.insert(
                                                    (cluster, /*interner.*/ category_name.intern()),
                                                    category_id,
                                                );
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
    })
    .await
    .unwrap_or_else(|_| (0, HashMap::new()))
}

pub async fn xtream_write_playlist(
    app_cfg: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
    replace_empty_clusters: ClusterFlags,
) -> Result<(), TuliproxError> {
    xtream_write_playlist_with_mode(
        app_cfg,
        target,
        playlist,
        replace_empty_clusters,
        TargetEmptyReplacementMode::Persist,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn xtream_write_playlist_with_injected_empty_replacement_failure(
    app_cfg: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
    replace_empty_clusters: ClusterFlags,
    failure: TargetEmptyReplacementFailure,
) -> Result<(), TuliproxError> {
    xtream_write_playlist_with_mode(
        app_cfg,
        target,
        playlist,
        replace_empty_clusters,
        TargetEmptyReplacementMode::FailAt(failure),
    )
    .await
}

pub(super) async fn xtream_write_playlist_with_mode(
    app_cfg: &Arc<AppConfig>,
    target: &ConfigTarget,
    playlist: &mut [PlaylistGroup],
    replace_empty_clusters: ClusterFlags,
    empty_replacement_mode: TargetEmptyReplacementMode,
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
            }
            .push(category);
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
    for (cluster, col_path, data) in [
        (XtreamCluster::Live, get_live_cat_collection_path(&root_path), &cat_live_col),
        (XtreamCluster::Video, get_vod_cat_collection_path(&root_path), &cat_vod_col),
        (XtreamCluster::Series, get_series_cat_collection_path(&root_path), &cat_series_col),
    ] {
        if data.is_empty() {
            if replace_empty_clusters.contains(cluster_flag(cluster)) {
                continue;
            }
            if file_exists_async(&col_path).await {
                continue;
            }
        }
        let category_lock_path = target_category_lock_path(&col_path);
        let lock = app_config.file_locks.write_lock(&category_lock_path).await;
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
    for (cluster, col) in
        [(XtreamCluster::Live, &live_col), (XtreamCluster::Video, &vod_col), (XtreamCluster::Series, &series_col)]
    {
        if col.is_empty() && replace_empty_clusters.contains(cluster_flag(cluster)) {
            if let Err(error) =
                replace_target_xtream_cluster_with_empty(app_cfg, &path, cluster, empty_replacement_mode.clone()).await
            {
                errors.push(format!("Persisting empty {cluster} target cluster failed: {error}"));
            }
            continue;
        }
        let data = col.iter().map(|item| XtreamPlaylistItem::from(&**item)).collect::<Vec<XtreamPlaylistItem>>();
        if let Err(err) = write_playlists_to_file(
            app_cfg,
            &path,
            true,
            |item| item.virtual_id,
            vec![(cluster, data)],
            replace_empty_clusters,
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

            let entry = new_categories.entry((cluster, group.clone())).or_insert_with(|| {
                let cat_id = existing_cat_ids.get(&(cluster, group.clone())).copied().unwrap_or_else(|| {
                    cat_id_counter += 1;
                    cat_id_counter
                });

                CategoryEntry { category_id: cat_id, category_name: group.clone(), parent_id: 0 }
            });

            channel.header.category_id = entry.category_id;
        }
    }

    new_categories
        .into_iter()
        .map(|((cluster, _group), value)| (cluster, value))
        .collect::<Vec<(XtreamCluster, CategoryEntry)>>()
}
