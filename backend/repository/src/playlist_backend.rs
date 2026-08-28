//! Static-dispatch description of a target-scoped, B+Tree-backed playlist store.
//!
//! The M3U and Xtream repositories were two transcriptions of one design: the
//! same storage-path derivation, the same two-read-lock iteration over a
//! B+Tree with sorted-index fallback, the same `spawn_blocking` producer
//! feeding a bounded channel, differing only in the item type, the storage
//! subdirectory and which [`TuliproxError`] variant wrapped a failure.
//!
//! This module names those differences as associated types and constants so the
//! shared body can be written once. Implementors are zero-sized markers, so
//! `iter_raw_playlist::<M3u, _>` and `iter_raw_playlist::<Xtream, _>`
//! monomorphize into two independent functions: no trait object, no vtable and
//! no allocation is introduced by the abstraction.
//!
//! Stalker is deliberately **not** a [`PlaylistBackend`]. Its store is keyed by
//! input rather than target, carries no sorted index, and yields bare items
//! rather than `Result`s. Forcing it into this trait would mean methods that
//! exist only to return `None` for one of three implementors, which is how an
//! abstraction stops describing anything.

use crate::{
    open_playlist_reader,
    storage::{ensure_target_storage_subpath, get_file_path_for_db_index, get_target_storage_path},
    storage_const, LockedReceiverStream,
};
use log::error;
use serde::{Deserialize, Serialize};
use shared::{
    error::TuliproxError,
    model::{M3uPlaylistItem, XtreamPlaylistItem},
};
use std::path::{Path, PathBuf};
use tokio::{sync::mpsc, task};
use tuliprox_core::{
    model::{AppConfig, Config},
    utils::file_exists_async,
};

/// Bounded channel depth for the producer task. Large enough to keep the disk
/// reader ahead of the consumer, small enough that a stalled consumer cannot
/// pull an entire playlist into memory.
const PLAYLIST_CHANNEL_CAPACITY: usize = 256;

/// Bounds a B+Tree key must satisfy to be read back by [`iter_raw_playlist`].
///
/// A bound alias only — the blanket impl means naming it costs nothing and adds
/// no indirection. It exists so the key can stay a *function* parameter: the M3U
/// backend keys its target store by `u32` and its input store by `Arc<str>`, so
/// the key is not a property of the backend and cannot be an associated type.
pub trait PlaylistKey: Ord + Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static {}

impl<T> PlaylistKey for T where T: Ord + Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static {}

/// One on-disk, target-scoped playlist backend, described for static dispatch.
pub trait PlaylistBackend {
    /// The item persisted as the B+Tree value.
    type Item: Serialize + for<'de> Deserialize<'de> + Clone + Send + 'static;

    /// The sorted-index key used when a sidecar index is present.
    type SortKey: Ord + for<'de> Deserialize<'de> + Send + 'static;

    /// Storage subdirectory beneath the target directory.
    const SUBDIR: &'static str;

    /// Human-readable backend name, used in log lines and error messages.
    const LABEL: &'static str;

    /// Whether the consumer-visible stream keeps a read lock on the playlist
    /// file for its entire lifetime.
    ///
    /// M3U holds one; Xtream historically does not, and that difference is
    /// preserved here rather than silently normalised — changing when a read
    /// lock is released alters what a concurrent writer can do mid-iteration.
    /// See the note in [`iter_raw_playlist`].
    const HOLD_ITER_LOCK: bool;

    /// Wrap `message` in this backend's error variant.
    fn repo_error(message: String) -> TuliproxError;

    /// Storage directory for `target_name`, if the target resolves at all.
    fn storage_path(cfg: &Config, target_name: &str) -> Option<PathBuf> {
        get_target_storage_path(cfg, target_name).map(|target_path| target_path.join(PathBuf::from(Self::SUBDIR)))
    }
}

/// M3U playlist storage.
pub struct M3u;

/// Xtream playlist storage.
pub struct Xtream;

/// Create this backend's storage directory for `target_name`.
pub async fn ensure_storage_path<B: PlaylistBackend>(
    cfg: &Config,
    target_name: &str,
) -> Result<PathBuf, TuliproxError> {
    ensure_target_storage_subpath(cfg, target_name, B::LABEL, B::storage_path, B::repo_error).await
}

/// Stream every item in the playlist database at `path`, keeping only those
/// `accept` returns `true` for.
///
/// Returns `None` when the database does not exist, which callers treat as "no
/// playlist persisted yet" rather than an error.
///
/// # Locking
///
/// Two read locks are taken when `B::HOLD_ITER_LOCK` is set: `iter_lock` travels
/// with the returned stream so the file cannot be replaced while the consumer is
/// still reading, and `bg_lock` moves into the blocking producer to guard the
/// reader itself. Backends with `HOLD_ITER_LOCK == false` take only `bg_lock`,
/// matching the behaviour they had before this function was extracted.
pub async fn iter_raw_playlist<B, K, F>(
    app_config: &AppConfig,
    path: &Path,
    accept: F,
) -> Option<LockedReceiverStream<Result<B::Item, TuliproxError>>>
where
    B: PlaylistBackend,
    K: PlaylistKey,
    F: Fn(&B::Item) -> bool + Send + 'static,
{
    let iter_lock = if B::HOLD_ITER_LOCK { Some(app_config.file_locks.read_lock(path).await) } else { None };

    if !file_exists_async(path).await {
        return None;
    }

    let bg_lock = app_config.file_locks.read_lock(path).await;

    let path = path.to_path_buf();
    let index_path = get_file_path_for_db_index(&path);
    let (tx, rx) = mpsc::channel::<Result<B::Item, TuliproxError>>(PLAYLIST_CHANNEL_CAPACITY);

    let path_for_log = path.clone();
    let index_path_for_log = index_path.clone();
    let open_err_tx = tx.clone();
    let join_err_tx = tx.clone();

    let handle = task::spawn_blocking(move || {
        let _guard = bg_lock;
        let reader = match open_playlist_reader::<K, B::Item, B::SortKey>(&path, &index_path, None) {
            Ok(reader) => reader,
            Err(err) => {
                error!(
                    "Failed to open {} playlist reader {} (index {}): {err}",
                    B::LABEL,
                    path.display(),
                    index_path.display()
                );
                let _ = open_err_tx.blocking_send(Err(err));
                return;
            }
        };

        for entry in reader {
            let item = match entry {
                Ok((_, item)) => item,
                Err(err) => {
                    error!("Skipping unreadable {} playlist entry: {err}", B::LABEL);
                    continue;
                }
            };
            if accept(&item) && tx.blocking_send(Ok(item)).is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        if let Err(err) = handle.await {
            error!(
                "{} playlist producer task failed for {} (index {}): {err}",
                B::LABEL,
                path_for_log.display(),
                index_path_for_log.display()
            );
            let _ = join_err_tx
                .send(Err(B::repo_error(format!(
                    "{} playlist producer task failed for {}: {err}",
                    B::LABEL,
                    path_for_log.display()
                ))))
                .await;
        }
    });

    Some(LockedReceiverStream::with_optional_guard(rx, iter_lock))
}

impl PlaylistBackend for M3u {
    type Item = M3uPlaylistItem;
    type SortKey = u32;

    const SUBDIR: &'static str = storage_const::PATH_M3U;
    const LABEL: &'static str = "M3U";
    const HOLD_ITER_LOCK: bool = true;

    fn repo_error(message: String) -> TuliproxError {
        TuliproxError::RepositoryM3u(message)
    }
}

impl PlaylistBackend for Xtream {
    type Item = XtreamPlaylistItem;
    type SortKey = u32;

    const SUBDIR: &'static str = storage_const::PATH_XTREAM;
    const LABEL: &'static str = "Xtream";
    const HOLD_ITER_LOCK: bool = false;

    fn repo_error(message: String) -> TuliproxError {
        TuliproxError::RepositoryXtream(message)
    }
}
