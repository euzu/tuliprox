//! Stalker/Ministra playlist persistence backed by the custom B+Tree engine.
//!
//! The on-disk layout mirrors the Xtream layout: one B+Tree file per cluster
//! (live / vod / series / episode), keyed by `stream_id` (or `episode_id` for
//! the episode index). The `stalker_repository` module owns the path helpers
//! and the high-level `persist_*` / `read_*` / `iter_*` entry points used by
//! the processor and the reverse-proxy code path.
//!
//! All write paths acquire a per-file write lock from the `FileLockManager`
//! before opening the B+Tree, mirroring the Xtream repository. Read paths
//! inside `iter_stalker_items` and `read_stalker_item` rely on the reader
//! guard that the disk-based source already holds; `clear_stalker_storage`
//! takes a write lock for each file it removes.

use crate::{
    bplustree::{BPlusTree, BPlusTreeError, BPlusTreeQuery, BPlusTreeUpdate, FlushPolicy},
    storage::ensure_input_storage_path,
    storage_const,
};
use log::warn;
use serde::{de::DeserializeOwned, Serialize};
use shared::{
    error::TuliproxError,
    model::{
        stalker::StalkerStreamKind,
        stalker_item::{StalkerPlaylistItem, StalkerSeasonItem},
    },
};
use std::{
    collections::HashSet,
    io::ErrorKind,
    ops::Bound,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_stream::wrappers::ReceiverStream;
use tuliprox_core::model::{AppConfig, ConfigInput, ConfigTarget, StalkerProgramRecord};

macro_rules! repo_err {
    ($($arg:tt)+) => { TuliproxError::RepositoryStalker(format!($($arg)+)) };
}

/// Build the on-disk directory for a Stalker input. The layout under the input
/// root is `<input_root>/stalker/` so that other input types don't collide.
pub fn get_stalker_storage_path(input_root: &Path) -> PathBuf {
    input_root.join(PathBuf::from(storage_const::PATH_STALKER))
}

/// Path of a single B+Tree file. `name` is the per-cluster filename
/// (e.g. `stalker_live`, `stalker_vod`).
fn stalker_file_path_for_name(storage_path: &Path, name: &str) -> PathBuf {
    storage_path.join(format!("{name}.{}", storage_const::FILE_SUFFIX_DB))
}

pub fn get_stalker_file_path(storage_path: &Path, kind: StalkerStreamKind) -> PathBuf {
    // Archive and Live share the live tree on disk (the cmd pipeline emits them
    // to the same per-portal file).
    let name = match kind {
        StalkerStreamKind::Live | StalkerStreamKind::Archive => storage_const::STALKER_LIVE_FILE,
        StalkerStreamKind::Movie => storage_const::STALKER_VOD_FILE,
        StalkerStreamKind::Episode => storage_const::STALKER_EPISODE_FILE,
    };
    stalker_file_path_for_name(storage_path, name)
}

pub fn get_stalker_season_file_path(storage_path: &Path) -> PathBuf {
    stalker_file_path_for_name(storage_path, storage_const::STALKER_SEASONS_FILE)
}

pub fn get_stalker_series_root_file_path(storage_path: &Path) -> PathBuf {
    stalker_file_path_for_name(storage_path, storage_const::STALKER_SERIES_ROOTS_FILE)
}

/// Path of the Stalker EPG B+Tree file. Stores bulk-fetched program records
/// keyed by `<channel_id>:<start_epoch>` so a given (channel, programme) is unique.
pub fn get_stalker_epg_file_path(storage_path: &Path) -> PathBuf {
    stalker_file_path_for_name(storage_path, storage_const::STALKER_EPG_FILE)
}

fn get_stalker_item_file_path(storage_path: &Path, item: &StalkerPlaylistItem) -> PathBuf {
    if item.is_series_root() {
        get_stalker_series_root_file_path(storage_path)
    } else {
        get_stalker_file_path(storage_path, item.stream_kind)
    }
}

pub async fn ensure_stalker_storage_path(cfg: &AppConfig, input_name: &str) -> Result<PathBuf, TuliproxError> {
    let input_path = ensure_input_storage_path(&cfg.config.load(), input_name).await?;
    let stalker_path = get_stalker_storage_path(&input_path);
    tokio::fs::create_dir_all(&stalker_path).await.map_err(|err| {
        TuliproxError::RepositoryStalker(format!(
            "failed to create stalker storage dir {}: {err}",
            stalker_path.display()
        ))
    })?;
    Ok(stalker_path)
}

/// Insert (or update) a single `StalkerPlaylistItem` in the per-cluster tree.
/// The on-disk write is atomic: we encode the payload, open the tree with an
/// exclusive file lock, upsert, flush, and release.
pub async fn persist_stalker_item(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    item: &StalkerPlaylistItem,
) -> Result<(), TuliproxError> {
    let file_path = get_stalker_item_file_path(storage_path, item);
    // Per-file write lock held for the duration of the blocking upsert so
    // concurrent readers (disk-based source) cannot observe a half-written
    // page and a parallel writer cannot race the B+Tree metadata.
    let file_lock = app_config.file_locks.write_lock(&file_path).await;
    let file_path_for_blocking = file_path.clone();
    let key = item.stream_id;
    let value = item.clone();
    tokio::task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        BPlusTreeUpdate::<u32, StalkerPlaylistItem>::upsert_batch_prepared_with_backoff(
            &file_path_for_blocking,
            &[(&key, &value)],
        )
        .map(|_| ())
        .map_err(|err| repo_err!("upsert failed for {}: {err}", file_path_for_blocking.display()))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

/// Check whether a path exists, logging (instead of silently swallowing)
/// IO errors such as permission failures before treating the path as absent.
async fn stalker_path_exists(path: &Path) -> bool {
    match tokio::fs::try_exists(path).await {
        Ok(exists) => exists,
        Err(err) => {
            warn!("Failed to check existence of {}: {err}", path.display());
            false
        }
    }
}

/// Blocking: write a full snapshot of `pairs` into a fresh ghost tree next to
/// `file_path` and atomically swap it into place (mirrors the Xtream ghost-tree
/// pattern). The caller must hold the per-file write lock for `file_path` for
/// the duration of the call.
fn write_stalker_snapshot_blocking<K, V>(file_path: &Path, pairs: &[(K, V)]) -> Result<u64, TuliproxError>
where
    K: Ord + Serialize + DeserializeOwned + Clone,
    V: Serialize + DeserializeOwned + Clone,
{
    let tmp_path = file_path.with_extension("tmp");
    BPlusTree::<K, V>::new()
        .store(&tmp_path)
        .map_err(|err| repo_err!("failed to create snapshot {}: {err}", tmp_path.display()))?;
    let written = {
        let mut tree = BPlusTreeUpdate::<K, V>::try_new_with_backoff(&tmp_path)
            .map_err(|err| repo_err!("failed to open snapshot {}: {err}", tmp_path.display()))?;
        tree.set_flush_policy(FlushPolicy::Batch);
        let refs: Vec<(&K, &V)> = pairs.iter().map(|(k, v)| (k, v)).collect();
        let prepared = BPlusTreeUpdate::<K, V>::prepare_upsert_batch(&refs)
            .map_err(|err| repo_err!("failed to encode snapshot batch for {}: {err}", tmp_path.display()))?;
        let written = tree
            .upsert_batch_encoded(prepared)
            .map_err(|err| repo_err!("snapshot batch write failed for {}: {err}", tmp_path.display()))?;
        tree.commit().map_err(|err| repo_err!("snapshot commit failed for {}: {err}", tmp_path.display()))?;
        written
    };
    tuliprox_core::utils::rename_or_copy(&tmp_path, file_path, false)
        .map_err(|err| repo_err!("failed to swap snapshot into {}: {err}", file_path.display()))?;
    Ok(written)
}

/// Replace the contents of one tree file with the given items (full snapshot).
async fn snapshot_stalker_file(
    app_config: &Arc<AppConfig>,
    file_path: PathBuf,
    pairs: Vec<(u32, StalkerPlaylistItem)>,
) -> Result<u64, TuliproxError> {
    // Per-file write lock held for the duration of the blocking snapshot swap
    // so concurrent readers cannot observe a half-written tree.
    let file_lock = app_config.file_locks.write_lock(&file_path).await;
    tokio::task::spawn_blocking(move || -> Result<u64, TuliproxError> {
        let _guard = file_lock;
        write_stalker_snapshot_blocking(&file_path, &pairs)
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn snapshot_stalker_items_at(
    app_config: &Arc<AppConfig>,
    file_path: PathBuf,
    items: &[StalkerPlaylistItem],
) -> Result<u64, TuliproxError> {
    let pairs = items.iter().map(|item| (item.stream_id, item.clone())).collect();
    snapshot_stalker_file(app_config, file_path, pairs).await
}

pub async fn upsert_stalker_items_at(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
    items: &[StalkerPlaylistItem],
) -> Result<u64, TuliproxError> {
    let file_lock = app_config.file_locks.write_lock(file_path).await;
    let blocking_path = file_path.to_path_buf();
    let pairs: Vec<(u32, StalkerPlaylistItem)> = items.iter().map(|item| (item.stream_id, item.clone())).collect();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        if !blocking_path.exists() {
            BPlusTree::<u32, StalkerPlaylistItem>::new()
                .store(&blocking_path)
                .map_err(|err| repo_err!("create staging tree {} failed: {err}", blocking_path.display()))?;
        }
        if pairs.is_empty() {
            return Ok(0);
        }
        let refs: Vec<(&u32, &StalkerPlaylistItem)> = pairs.iter().map(|(key, value)| (key, value)).collect();
        BPlusTreeUpdate::<u32, StalkerPlaylistItem>::upsert_batch_prepared_with_backoff(&blocking_path, &refs)
            .map_err(|err| repo_err!("batch upsert failed for {}: {err}", blocking_path.display()))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn load_stalker_items_after(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
    after: Option<u32>,
    limit: usize,
) -> Result<Vec<StalkerPlaylistItem>, TuliproxError> {
    if !stalker_path_exists(file_path).await || limit == 0 {
        return Ok(Vec::new());
    }
    let file_lock = app_config.file_locks.read_lock(file_path).await;
    let blocking_path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<u32, StalkerPlaylistItem>::try_new(&blocking_path)
            .map_err(|err| repo_err!("open {} failed: {err}", blocking_path.display()))?;
        if after.is_none() && limit == usize::MAX {
            return query
                .iter()
                .map(|entry| entry.map(|(_, item)| item))
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|err| repo_err!("scan {} failed: {err}", blocking_path.display()));
        }
        let start = after.as_ref().map_or(Bound::Unbounded, Bound::Excluded);
        let (items, _) = query
            .range_page(start, Bound::Unbounded, 0, limit)
            .map_err(|err| repo_err!("range {} failed: {err}", blocking_path.display()))?;
        Ok(items.into_iter().map(|(_, item)| item).collect())
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn load_stalker_items_at(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
) -> Result<Vec<StalkerPlaylistItem>, TuliproxError> {
    load_stalker_items_after(app_config, file_path, None, usize::MAX).await
}

/// Counts one Stalker generation file without materializing its records.
///
/// The read lock is acquired before opening the tree, so a missing file is a
/// stable `None` result rather than an existence-check race.
pub async fn count_stalker_items_at(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
) -> Result<Option<usize>, TuliproxError> {
    let file_lock = app_config.file_locks.read_lock(file_path).await;
    let blocking_path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        let mut query = match BPlusTreeQuery::<u32, StalkerPlaylistItem>::try_new(&blocking_path) {
            Ok(query) => query,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(repo_err!("open {} failed: {err}", blocking_path.display())),
        };
        query
            .len()
            .map(Some)
            .map_err(BPlusTreeError::to_io)
            .map_err(|err| repo_err!("count {} failed: {err}", blocking_path.display()))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn prepare_stalker_episode_series_at(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
    series_id: u32,
) -> Result<HashSet<u32>, TuliproxError> {
    if !stalker_path_exists(file_path).await {
        return Ok(HashSet::new());
    }
    let file_lock = app_config.file_locks.write_lock(file_path).await;
    let blocking_path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        let (occupied, stale) = {
            let mut query = BPlusTreeQuery::<u32, StalkerPlaylistItem>::try_new(&blocking_path)
                .map_err(|err| repo_err!("open {} failed: {err}", blocking_path.display()))?;
            let mut occupied = HashSet::new();
            let mut stale = Vec::new();
            for entry in query.iter() {
                let (key, item) = entry.map_err(|err| repo_err!("scan {} failed: {err}", blocking_path.display()))?;
                if item.series_id == Some(series_id) {
                    stale.push(key);
                } else {
                    occupied.insert(key);
                }
            }
            (occupied, stale)
        };
        let mut tree = BPlusTreeUpdate::<u32, StalkerPlaylistItem>::try_new_with_backoff(&blocking_path)
            .map_err(|err| repo_err!("open {} failed: {err}", blocking_path.display()))?;
        let stale_refs: Vec<&u32> = stale.iter().collect();
        tree.delete_batch(&stale_refs)
            .map_err(|err| repo_err!("delete stale series from {} failed: {err}", blocking_path.display()))?;
        Ok(occupied)
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

/// Persist the full result of one cluster refresh as a snapshot: every tree
/// file belonging to `kind` is rebuilt from scratch and atomically swapped in,
/// so items removed upstream disappear from the store instead of lingering
/// forever (snapshot, not upsert, semantics).
///
/// For `Episode`, items are split between the episode tree and the series-root
/// tree; both are replaced even when one of the partitions is empty.
pub async fn persist_stalker_items(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    kind: StalkerStreamKind,
    items: &[StalkerPlaylistItem],
) -> Result<u64, TuliproxError> {
    let mut total: u64 = 0;
    match kind {
        StalkerStreamKind::Live | StalkerStreamKind::Archive | StalkerStreamKind::Movie => {
            let pairs: Vec<(u32, StalkerPlaylistItem)> = items.iter().map(|i| (i.stream_id, i.clone())).collect();
            total = total.saturating_add(
                snapshot_stalker_file(app_config, get_stalker_file_path(storage_path, kind), pairs).await?,
            );
        }
        StalkerStreamKind::Episode => {
            let mut episodes: Vec<(u32, StalkerPlaylistItem)> = Vec::new();
            let mut roots: Vec<(u32, StalkerPlaylistItem)> = Vec::new();
            for item in items {
                if item.is_series_root() {
                    roots.push((item.stream_id, item.clone()));
                } else {
                    episodes.push((item.stream_id, item.clone()));
                }
            }
            total = total.saturating_add(
                snapshot_stalker_file(app_config, get_stalker_file_path(storage_path, kind), episodes).await?,
            );
            total = total.saturating_add(
                snapshot_stalker_file(app_config, get_stalker_series_root_file_path(storage_path), roots).await?,
            );
        }
    }
    Ok(total)
}

/// Look up a single item by `stream_id` from the cluster-specific tree.
pub async fn read_stalker_item(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    kind: StalkerStreamKind,
    stream_id: u32,
) -> Result<Option<StalkerPlaylistItem>, TuliproxError> {
    let file_path = get_stalker_file_path(storage_path, kind);
    read_stalker_item_at(app_config, &file_path, stream_id).await
}

pub async fn read_stalker_item_at(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
    stream_id: u32,
) -> Result<Option<StalkerPlaylistItem>, TuliproxError> {
    let file_lock = app_config.file_locks.read_lock(file_path).await;
    let blocking_path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Option<StalkerPlaylistItem>, TuliproxError> {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<u32, StalkerPlaylistItem>::try_new(&blocking_path)
            .map_err(|err| repo_err!("open {} failed: {err}", blocking_path.display()))?;
        query.query(&stream_id).map_err(|err| repo_err!("query {} failed: {err}", blocking_path.display()))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

/// Stream-iterate all items of one kind. Wraps the disk iterator in a tokio
/// channel so callers can consume it asynchronously.
pub async fn iter_stalker_items(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    kind: StalkerStreamKind,
) -> Result<Option<ReceiverStream<StalkerPlaylistItem>>, TuliproxError> {
    let file_path = get_stalker_file_path(storage_path, kind);
    iter_stalker_file(app_config, file_path).await
}

pub async fn iter_stalker_series_roots(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
) -> Result<Option<ReceiverStream<StalkerPlaylistItem>>, TuliproxError> {
    iter_stalker_file(app_config, get_stalker_series_root_file_path(storage_path)).await
}

async fn iter_stalker_file(
    app_config: &Arc<AppConfig>,
    file_path: PathBuf,
) -> Result<Option<ReceiverStream<StalkerPlaylistItem>>, TuliproxError> {
    use tokio::sync::mpsc;

    if !stalker_path_exists(&file_path).await {
        return Ok(None);
    }
    // Bounded channel — large enough to keep the disk iterator ahead of the
    // consumer, small enough to avoid OOM if the consumer stalls.
    let (tx, rx) = mpsc::channel::<StalkerPlaylistItem>(1024);
    let file_lock = app_config.file_locks.read_lock(&file_path).await;
    let blocking_path = file_path.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        let result: Result<(), String> = (|| {
            let mut query = BPlusTreeQuery::<u32, StalkerPlaylistItem>::try_new(&blocking_path)
                .map_err(|err| format!("open {} failed: {err}", blocking_path.display()))?;
            for entry in query.iter() {
                let (_, doc) = match entry {
                    Ok(entry) => entry,
                    Err(err) => {
                        warn!("Skipping unreadable Stalker playlist entry in {}: {err}", blocking_path.display());
                        continue;
                    }
                };
                if tx.blocking_send(doc).is_err() {
                    break;
                }
            }
            Ok(())
        })();
        if let Err(err) = result {
            warn!("Stalker iterator aborted: {err}");
        }
    });
    Ok(Some(ReceiverStream::new(rx)))
}

/// Drop all persisted items for a given input. Used when the user changes the
/// input configuration and the existing tree is no longer valid. Acquires a
/// per-file write lock for each removal so a concurrent reader cannot see a
/// half-removed tree.
pub async fn clear_stalker_storage(app_config: &Arc<AppConfig>, storage_path: &Path) -> Result<(), TuliproxError> {
    if !stalker_path_exists(storage_path).await {
        return Ok(());
    }
    for kind in [StalkerStreamKind::Live, StalkerStreamKind::Movie, StalkerStreamKind::Episode] {
        let file_path = get_stalker_file_path(storage_path, kind);
        if stalker_path_exists(&file_path).await {
            let _lock = app_config.file_locks.write_lock(&file_path).await;
            tokio::fs::remove_file(&file_path)
                .await
                .map_err(|err| repo_err!("failed to remove {}: {err}", file_path.display()))?;
        }
    }
    for file_path in [
        get_stalker_season_file_path(storage_path),
        get_stalker_series_root_file_path(storage_path),
        get_stalker_epg_file_path(storage_path),
    ] {
        if stalker_path_exists(&file_path).await {
            let _lock = app_config.file_locks.write_lock(&file_path).await;
            tokio::fs::remove_file(&file_path)
                .await
                .map_err(|err| repo_err!("failed to remove {}: {err}", file_path.display()))?;
        }
    }
    Ok(())
}

/// Persist a season list. Seasons are a side-table because the parent series
/// item is a different shape than the episode rows.
pub async fn persist_stalker_season(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    season: &StalkerSeasonItem,
) -> Result<(), TuliproxError> {
    let file_path = get_stalker_season_file_path(storage_path);
    let file_lock = app_config.file_locks.write_lock(&file_path).await;
    let blocking_path = file_path.clone();
    let key = (season.series_id, season.season_number);
    let value = season.clone();
    tokio::task::spawn_blocking(move || -> Result<(), TuliproxError> {
        let _guard = file_lock;
        BPlusTreeUpdate::<(u32, i32), StalkerSeasonItem>::upsert_batch_prepared_with_backoff(
            &blocking_path,
            &[(&key, &value)],
        )
        .map(|_| ())
        .map_err(|err| repo_err!("upsert season failed: {err}"))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

/// Look up a single season by `(series_id, season_number)`.
pub async fn read_stalker_season(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    series_id: u32,
    season_number: i32,
) -> Result<Option<StalkerSeasonItem>, TuliproxError> {
    let file_path = get_stalker_season_file_path(storage_path);
    let file_lock = app_config.file_locks.read_lock(&file_path).await;
    let blocking_path = file_path.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<StalkerSeasonItem>, TuliproxError> {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<(u32, i32), StalkerSeasonItem>::try_new(&blocking_path)
            .map_err(|err| repo_err!("open seasons failed: {err}"))?;
        query.query(&(series_id, season_number)).map_err(|err| repo_err!("query seasons failed: {err}"))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

/// Persist the full bulk-EPG fetch result as a snapshot. The bulk-EPG endpoint
/// can emit hundreds of thousands of records per portal; the processor collects
/// the whole fetch and calls this once per refresh. Records are keyed by
/// `<channel_id>:<start_epoch>`; the tree is rebuilt from scratch and atomically
/// swapped in so stale programmes from earlier fetches do not accumulate.
/// An empty fetch is treated as "no data" and leaves the existing store intact.
pub async fn persist_stalker_epg_programs(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    programs: &[StalkerProgramRecord],
) -> Result<u64, TuliproxError> {
    if programs.is_empty() {
        return Ok(0);
    }
    let file_path = get_stalker_epg_file_path(storage_path);
    let pairs: Vec<(String, StalkerProgramRecord)> = programs
        .iter()
        .map(|p| {
            let ch = p.channel_id.clone().unwrap_or_default();
            let start = p.start_epoch.unwrap_or(0);
            (format!("{ch}:{start}"), p.clone())
        })
        .collect();
    let file_lock = app_config.file_locks.write_lock(&file_path).await;
    tokio::task::spawn_blocking(move || -> Result<u64, TuliproxError> {
        let _guard = file_lock;
        write_stalker_snapshot_blocking(&file_path, &pairs)
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn upsert_stalker_epg_at(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
    programs: &[StalkerProgramRecord],
) -> Result<u64, TuliproxError> {
    let pairs: Vec<(String, StalkerProgramRecord)> = programs
        .iter()
        .map(|program| {
            let channel = program.channel_id.clone().unwrap_or_default();
            let start = program.start_epoch.unwrap_or_default();
            (format!("{channel}:{start}"), program.clone())
        })
        .collect();
    let file_lock = app_config.file_locks.write_lock(file_path).await;
    let blocking_path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        if !blocking_path.exists() {
            BPlusTree::<String, StalkerProgramRecord>::new()
                .store(&blocking_path)
                .map_err(|err| repo_err!("create EPG staging tree {} failed: {err}", blocking_path.display()))?;
        }
        if pairs.is_empty() {
            return Ok(0);
        }
        let refs: Vec<(&String, &StalkerProgramRecord)> = pairs.iter().map(|(key, value)| (key, value)).collect();
        BPlusTreeUpdate::<String, StalkerProgramRecord>::upsert_batch_prepared_with_backoff(&blocking_path, &refs)
            .map_err(|err| repo_err!("EPG batch upsert failed for {}: {err}", blocking_path.display()))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn snapshot_stalker_epg_at(
    app_config: &Arc<AppConfig>,
    file_path: &Path,
    programs: &[StalkerProgramRecord],
) -> Result<u64, TuliproxError> {
    let pairs: Vec<(String, StalkerProgramRecord)> = programs
        .iter()
        .map(|program| {
            let channel = program.channel_id.clone().unwrap_or_default();
            let start = program.start_epoch.unwrap_or_default();
            (format!("{channel}:{start}"), program.clone())
        })
        .collect();
    let file_lock = app_config.file_locks.write_lock(file_path).await;
    let blocking_path = file_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _guard = file_lock;
        write_stalker_snapshot_blocking(&blocking_path, &pairs)
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn promote_stalker_file(
    app_config: &Arc<AppConfig>,
    staging_path: &Path,
    published_path: &Path,
) -> Result<(), TuliproxError> {
    let staging_lock = app_config.file_locks.write_lock(staging_path).await;
    let published_lock = app_config.file_locks.write_lock(published_path).await;
    let staging = staging_path.to_path_buf();
    let published = published_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _staging_guard = staging_lock;
        let _published_guard = published_lock;
        tuliprox_core::utils::rename_or_copy(&staging, &published, false)
            .map_err(|err| repo_err!("promote {} to {} failed: {err}", staging.display(), published.display()))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

pub async fn remove_stalker_file(app_config: &Arc<AppConfig>, file_path: &Path) -> Result<(), TuliproxError> {
    let file_lock = app_config.file_locks.write_lock(file_path).await;
    let path = file_path.to_path_buf();
    let _guard = file_lock;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(repo_err!("remove {} failed: {err}", path.display())),
    }
}

/// Convenience for the proxy path: resolve the per-input storage path given an
/// `AppConfig` and a `ConfigInput`. Returns `None` if the input is not named or
/// the input directory cannot be created.
pub async fn resolve_stalker_storage_for_input(app_config: &Arc<AppConfig>, input: &ConfigInput) -> Option<PathBuf> {
    let cfg = app_config.config.load();
    let input_path = crate::storage::ensure_input_storage_path(&cfg, &input.name).await.ok()?;
    Some(get_stalker_storage_path(&input_path))
}

/// Resolve the per-input storage path for a target's `ConfigInput` lookup.
/// The target itself doesn't have a stalker storage, but the inputs that
/// feed it do; this helper centralises the lookup so callers don't need to
/// duplicate the `ensure_*` / `get_stalker_storage_path` logic.
pub async fn resolve_stalker_storage_for_target(
    app_config: &Arc<AppConfig>,
    _target: &ConfigTarget,
    inputs: &[Arc<ConfigInput>],
) -> Option<(PathBuf, Arc<ConfigInput>)> {
    // Find the first Stalker input that contributes to the target. A target can
    // pull from multiple inputs but only one of them is allowed to be Stalker
    // per the current config schema; we pick the first match.
    for input in inputs {
        if input.input_type.is_stalker() {
            let path = resolve_stalker_storage_for_input(app_config, input).await?;
            return Some((path, input.clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_path_appends_stalker_dir() {
        let root = PathBuf::from("/var/lib/tuliprox/input_test");
        let path = get_stalker_storage_path(&root);
        assert_eq!(path, root.join("stalker"));
    }

    #[test]
    fn file_path_per_kind_does_not_collide() {
        let root = PathBuf::from("/tmp/x");
        let live = get_stalker_file_path(&root, StalkerStreamKind::Live);
        let vod = get_stalker_file_path(&root, StalkerStreamKind::Movie);
        let ep = get_stalker_file_path(&root, StalkerStreamKind::Episode);
        let series_root = get_stalker_series_root_file_path(&root);
        assert_ne!(live, vod);
        assert_ne!(live, ep);
        assert_ne!(vod, ep);
        assert_ne!(ep, series_root);
    }
}
