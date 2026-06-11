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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::Stream;
use log::warn;
use tokio_stream::wrappers::ReceiverStream;
use shared::error::TuliproxError;
use shared::model::stalker_item::{StalkerPlaylistItem, StalkerSeasonItem};

use crate::utils::network::stalker::epg::StalkerProgramRecord;

use crate::api::model::AppState;
use crate::model::{AppConfig, ConfigInput, ConfigTarget};
use crate::repository::bplustree::{BPlusTreeQuery, BPlusTreeUpdate};
use crate::repository::storage::ensure_input_storage_path;
use crate::repository::storage_const;
use shared::model::stalker::StalkerStreamKind;

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
        StalkerStreamKind::Live | StalkerStreamKind::Archive => "stalker_live",
        StalkerStreamKind::Movie => "stalker_vod",
        StalkerStreamKind::Episode => "stalker_episode",
    };
    stalker_file_path_for_name(storage_path, name)
}

pub fn get_stalker_season_file_path(storage_path: &Path) -> PathBuf {
    stalker_file_path_for_name(storage_path, "stalker_seasons")
}

pub fn get_stalker_series_root_file_path(storage_path: &Path) -> PathBuf {
    stalker_file_path_for_name(storage_path, "stalker_series_roots")
}

/// Path of the Stalker EPG B+Tree file. Stores bulk-fetched program records
/// keyed by `<channel_id>:<start_epoch>` so a given (channel, programme) is unique.
pub fn get_stalker_epg_file_path(storage_path: &Path) -> PathBuf {
    stalker_file_path_for_name(storage_path, "stalker_epg")
}

fn get_stalker_item_file_path(storage_path: &Path, item: &StalkerPlaylistItem) -> PathBuf {
    if item.is_series_root() {
        get_stalker_series_root_file_path(storage_path)
    } else {
        get_stalker_file_path(storage_path, item.stream_kind)
    }
}

pub async fn ensure_stalker_storage_path(
    cfg: &AppConfig,
    input_name: &str,
) -> Result<PathBuf, TuliproxError> {
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

/// Bulk-insert a list of `StalkerPlaylistItem` into the appropriate tree. The
/// caller is expected to have already grouped items by `stream_kind` if it
/// wants to avoid repeated disk opens; we route each item to its own tree
/// internally.
pub async fn persist_stalker_items(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    items: &[StalkerPlaylistItem],
) -> Result<u64, TuliproxError> {
    if items.is_empty() {
        return Ok(0);
    }
    // Group by kind so we open each tree at most once. IndexMap gives us
    // deterministic iteration order and avoids the `Hash` bound on the kind
    // enum (we don't need O(1) lookup, only the grouping).
    let mut by_path: indexmap::IndexMap<PathBuf, Vec<StalkerPlaylistItem>> =
        indexmap::IndexMap::new();
    for item in items {
        by_path.entry(get_stalker_item_file_path(storage_path, item)).or_default().push(item.clone());
    }
    let mut total: u64 = 0;
    for (file_path, batch) in by_path {
        // Per-file write lock held for the duration of the blocking upsert —
        // see `persist_stalker_item` for the rationale.
        let file_lock = app_config.file_locks.write_lock(&file_path).await;
        let key_value_pairs: Vec<(u32, StalkerPlaylistItem)> =
            batch.iter().map(|i| (i.stream_id, i.clone())).collect();
        let file_path_for_blocking = file_path.clone();
        let key_value_pairs_for_blocking: Vec<(u32, StalkerPlaylistItem)> = key_value_pairs.clone();
        let inserted = tokio::task::spawn_blocking(move || -> Result<u64, TuliproxError> {
            let _guard = file_lock;
            let pairs: Vec<(&u32, &StalkerPlaylistItem)> = key_value_pairs_for_blocking
                .iter()
                .map(|(k, v)| (k, v))
                .collect();
            BPlusTreeUpdate::<u32, StalkerPlaylistItem>::upsert_batch_prepared_with_backoff(
                &file_path_for_blocking,
                &pairs,
            )
            .map_err(|err| repo_err!("bulk upsert failed for {}: {err}", file_path_for_blocking.display()))
        })
        .await
        .map_err(|err| repo_err!("blocking task join error: {err}"))??;
        total = total.saturating_add(inserted);
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
    let file_lock = app_config.file_locks.read_lock(&file_path).await;
    let blocking_path = file_path.clone();
    tokio::task::spawn_blocking(move || -> Result<Option<StalkerPlaylistItem>, TuliproxError> {
        let _guard = file_lock;
        let mut query = BPlusTreeQuery::<u32, StalkerPlaylistItem>::try_new(&blocking_path)
            .map_err(|err| repo_err!("open {} failed: {err}", blocking_path.display()))?;
        query
            .query(&stream_id)
            .map_err(|err| repo_err!("query {} failed: {err}", blocking_path.display()))
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
) -> Result<Option<Box<dyn Stream<Item = StalkerPlaylistItem> + Send + Unpin>>, TuliproxError> {
    let file_path = get_stalker_file_path(storage_path, kind);
    iter_stalker_file(app_config, file_path).await
}

pub async fn iter_stalker_series_roots(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
) -> Result<Option<Box<dyn Stream<Item = StalkerPlaylistItem> + Send + Unpin>>, TuliproxError> {
    iter_stalker_file(app_config, get_stalker_series_root_file_path(storage_path)).await
}

async fn iter_stalker_file(
    app_config: &Arc<AppConfig>,
    file_path: PathBuf,
) -> Result<Option<Box<dyn Stream<Item = StalkerPlaylistItem> + Send + Unpin>>, TuliproxError> {
    use tokio::sync::mpsc;

    if !tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
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
            for (_, doc) in query.iter() {
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
    let stream: Box<dyn Stream<Item = StalkerPlaylistItem> + Send + Unpin> =
        Box::new(ReceiverStream::new(rx));
    Ok(Some(stream))
}

/// Drop all persisted items for a given input. Used when the user changes the
/// input configuration and the existing tree is no longer valid. Acquires a
/// per-file write lock for each removal so a concurrent reader cannot see a
/// half-removed tree.
pub async fn clear_stalker_storage(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
) -> Result<(), TuliproxError> {
    if !tokio::fs::try_exists(storage_path).await.unwrap_or(false) {
        return Ok(());
    }
    for kind in [
        StalkerStreamKind::Live,
        StalkerStreamKind::Movie,
        StalkerStreamKind::Episode,
    ] {
        let file_path = get_stalker_file_path(storage_path, kind);
        if tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
            let _lock = app_config.file_locks.write_lock(&file_path).await;
            tokio::fs::remove_file(&file_path).await.map_err(|err| {
                repo_err!("failed to remove {}: {err}", file_path.display())
            })?;
        }
    }
    let season_path = get_stalker_season_file_path(storage_path);
    if tokio::fs::try_exists(&season_path).await.unwrap_or(false) {
        let _lock = app_config.file_locks.write_lock(&season_path).await;
        tokio::fs::remove_file(&season_path).await.map_err(|err| {
            repo_err!("failed to remove {}: {err}", season_path.display())
        })?;
    }
    let series_root_path = get_stalker_series_root_file_path(storage_path);
    if tokio::fs::try_exists(&series_root_path).await.unwrap_or(false) {
        let _lock = app_config.file_locks.write_lock(&series_root_path).await;
        tokio::fs::remove_file(&series_root_path).await.map_err(|err| {
            repo_err!("failed to remove {}: {err}", series_root_path.display())
        })?;
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
        query
            .query(&(series_id, season_number))
            .map_err(|err| repo_err!("query seasons failed: {err}"))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

/// Compute the on-disk path for a Stalker input. The result is `<input_dir>/stalker/`.
pub fn get_stalker_input_path(input_root: &Path) -> PathBuf {
    get_stalker_storage_path(input_root)
}

/// Persist a batch of Stalker EPG program records. The bulk-EPG endpoint can
/// emit hundreds of thousands of records per portal, so callers should batch
/// in memory (e.g. 500 at a time) before calling this helper. Records are
/// keyed by `<channel_id>:<start_epoch>` so re-fetches replace stale entries
/// on the same (channel, programme) slot.
pub async fn persist_stalker_epg_programs(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    programs: &[StalkerProgramRecord],
) -> Result<u64, TuliproxError> {
    if programs.is_empty() {
        return Ok(0);
    }
    let file_path = get_stalker_epg_file_path(storage_path);
    let file_lock = app_config.file_locks.write_lock(&file_path).await;
    let blocking_path = file_path.clone();
    let pairs: Vec<(String, StalkerProgramRecord)> = programs
        .iter()
        .map(|p| {
            let ch = p.channel_id.clone().unwrap_or_default();
            let start = p.start_epoch.unwrap_or(0);
            (format!("{ch}:{start}"), p.clone())
        })
        .collect();
    tokio::task::spawn_blocking(move || -> Result<u64, TuliproxError> {
        let _guard = file_lock;
        let pairs_ref: Vec<(&String, &StalkerProgramRecord)> =
            pairs.iter().map(|(k, v)| (k, v)).collect();
        BPlusTreeUpdate::<String, StalkerProgramRecord>::upsert_batch_prepared_with_backoff(
            &blocking_path,
            &pairs_ref,
        )
        .map_err(|err| repo_err!("stalker EPG bulk upsert failed: {err}"))
    })
    .await
    .map_err(|err| repo_err!("blocking task join error: {err}"))?
}

/// Convenience for the proxy path: resolve the per-input storage path given an
/// `AppState` and a `ConfigInput`. Returns `None` if the input is not named or
/// the input directory cannot be created.
pub async fn resolve_stalker_storage_for_input(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
) -> Option<PathBuf> {
    let cfg = app_state.app_config.config.load();
    let input_path = crate::repository::storage::ensure_input_storage_path(&cfg, &input.name)
        .await
        .ok()?;
    Some(get_stalker_storage_path(&input_path))
}

/// Resolve the per-input storage path for a target's `ConfigInput` lookup.
/// The target itself doesn't have a stalker storage, but the inputs that
/// feed it do; this helper centralises the lookup so callers don't need to
/// duplicate the `ensure_*` / `get_stalker_storage_path` logic.
pub async fn resolve_stalker_storage_for_target(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    inputs: &[Arc<ConfigInput>],
) -> Option<(PathBuf, Arc<ConfigInput>)> {
    // Find the first Stalker input that contributes to the target. A target can
    // pull from multiple inputs but only one of them is allowed to be Stalker
    // per the current config schema; we pick the first match.
    for input in inputs {
        if input.input_type.is_stalker() {
            let path = resolve_stalker_storage_for_input(app_state, input).await?;
            return Some((path, input.clone()));
        }
    }
    let _ = target;
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
