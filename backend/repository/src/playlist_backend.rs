//! Static-dispatch description of a target-scoped, B+Tree-backed playlist store.
//!
//! The M3U and Xtream repositories were two transcriptions of one design: the
//! same storage-path derivation, the same read-locked iteration over a
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
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
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
/// Exactly one read lock is taken, never two on the same path: a second read
/// behind a queued writer would deadlock the task (the lock is write-preferring).
/// When `B::HOLD_ITER_LOCK` is set the guard is shared between the returned
/// stream and the blocking producer, so the file cannot be replaced while the
/// consumer is still reading and the lock is released once both are done.
/// Backends with `HOLD_ITER_LOCK == false` keep the guard in the producer only,
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
    // The lock is taken before the existence check when the stream holds it, so
    // the file cannot be replaced between the check and the read.
    let stream_lock =
        if B::HOLD_ITER_LOCK { Some(Arc::new(app_config.file_locks.read_lock(path).await)) } else { None };

    if !file_exists_async(path).await {
        return None;
    }

    let producer_lock = match &stream_lock {
        Some(lock) => Arc::clone(lock),
        None => Arc::new(app_config.file_locks.read_lock(path).await),
    };

    let path = path.to_path_buf();
    let index_path = get_file_path_for_db_index(&path);
    let (tx, rx) = mpsc::channel::<Result<B::Item, TuliproxError>>(PLAYLIST_CHANNEL_CAPACITY);

    let path_for_log = path.clone();
    let index_path_for_log = index_path.clone();
    let open_err_tx = tx.clone();
    let join_err_tx = tx.clone();
    let skipped = Arc::new(AtomicUsize::new(0));
    let producer_skipped = Arc::clone(&skipped);

    let handle = task::spawn_blocking(move || {
        let _guard = producer_lock;
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
            // A rejecting `accept` never reaches `blocking_send`, so a dropped
            // receiver would otherwise go unnoticed until the end of the database.
            if tx.is_closed() {
                break;
            }
            let item = match entry {
                Ok((_, item)) => item,
                Err(err) => {
                    error!("Skipping unreadable {} playlist entry: {err}", B::LABEL);
                    producer_skipped.fetch_add(1, Ordering::Release);
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

    Some(LockedReceiverStream::with_optional_shared_guard(rx, stream_lock).with_skip_counter(skipped))
}

impl PlaylistBackend for M3u {
    type Item = M3uPlaylistItem;
    type SortKey = u32;

    const SUBDIR: &'static str = storage_const::PATH_M3U;
    const LABEL: &'static str = "M3U";
    const HOLD_ITER_LOCK: bool = true;

    fn repo_error(message: String) -> TuliproxError { TuliproxError::RepositoryM3u(message) }
}

impl PlaylistBackend for Xtream {
    type Item = XtreamPlaylistItem;
    type SortKey = u32;

    const SUBDIR: &'static str = storage_const::PATH_XTREAM;
    const LABEL: &'static str = "Xtream";
    const HOLD_ITER_LOCK: bool = false;

    fn repo_error(message: String) -> TuliproxError { TuliproxError::RepositoryXtream(message) }
}

#[cfg(test)]
mod tests {
    use super::{iter_raw_playlist, M3u, Xtream};
    use crate::{
        bplustree::BPlusTree,
        persist_input_m3u_playlist,
        test_config::{app_config, xtream_playlist_item},
        LockedReceiverStream,
    };
    use futures::{poll, StreamExt};
    use shared::{
        model::{
            M3uPlaylistItem, PlaylistGroup, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster,
            XtreamPlaylistItem,
        },
        utils::Internable,
    };
    use std::{
        fs,
        path::Path,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc as std_mpsc, Arc,
        },
        time::Duration,
    };
    use tokio::{sync::mpsc, task, time::timeout};

    const ENTRY_COUNT: usize = 500;

    fn live_playlist(entries: usize) -> Vec<PlaylistGroup> { named_live_playlist(entries, |_| "Channel".to_string()) }

    fn named_live_playlist(entries: usize, name: impl Fn(usize) -> String) -> Vec<PlaylistGroup> {
        let channels = (0..entries)
            .map(|index| {
                let id = format!("provider-{index:05}").intern();
                PlaylistItem {
                    header: PlaylistItemHeader {
                        id: id.clone(),
                        input_stream_id: id,
                        name: name(index).intern(),
                        group: "Live".intern(),
                        item_type: PlaylistItemType::Live,
                        xtream_cluster: XtreamCluster::Live,
                        ..PlaylistItemHeader::default()
                    },
                }
            })
            .collect();
        vec![PlaylistGroup { id: 1, title: "Live".intern(), channels, xtream_cluster: XtreamCluster::Live }]
    }

    #[tokio::test]
    async fn producer_stops_when_the_receiver_is_dropped_with_a_rejecting_filter() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(ENTRY_COUNT))
            .await
            .expect("playlist should persist");

        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std_mpsc::channel::<()>();
        let (release_tx, release_rx) = std_mpsc::channel::<()>();
        let accept_calls = Arc::clone(&calls);
        // The producer parks inside the first `accept` call, so the receiver is
        // provably dropped before it looks at the next entry.
        let accept = move |_: &M3uPlaylistItem| {
            if accept_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                entered_tx.send(()).expect("test must still be waiting for the first call");
                release_rx.recv().expect("test must release the first call");
            }
            false
        };

        let stream =
            iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, accept).await.expect("playlist database exists");
        timeout(Duration::from_secs(5), task::spawn_blocking(move || entered_rx.recv()))
            .await
            .expect("producer should reach the first accept call")
            .expect("wait task should not panic")
            .expect("producer should signal the first accept call");

        drop(stream);
        release_tx.send(()).expect("producer must still be parked in accept");

        // A write lock is only grantable once the producer released its read lock,
        // i.e. once the blocking loop has ended.
        let write_guard = timeout(Duration::from_secs(5), app_config.file_locks.write_lock(&path))
            .await
            .expect("producer should release its read lock after the receiver is dropped");
        drop(write_guard);

        assert_eq!(calls.load(Ordering::SeqCst), 1, "producer must stop before decoding the next entry");
    }

    const TEST_PAGE_SIZE: usize = 4096;
    const TEST_PAGE_CHECKSUM_OFFSET: usize = 24;

    /// Flips one payload byte of the stored value that contains `marker` and
    /// re-seals the page checksum. The page therefore stays loadable and only
    /// that value's own checksum fails, so the reader errors for this entry and
    /// continues with the next one.
    fn corrupt_value_containing(path: &std::path::Path, marker: &str) {
        let mut bytes = fs::read(path).expect("database should be readable");
        let position = bytes
            .windows(marker.len())
            .position(|window| window == marker.as_bytes())
            .expect("marker should be stored uncompressed in the database");
        bytes[position] ^= 0x01;
        let page_start = position / TEST_PAGE_SIZE * TEST_PAGE_SIZE;
        let page = &mut bytes[page_start..page_start + TEST_PAGE_SIZE];
        page[TEST_PAGE_CHECKSUM_OFFSET..TEST_PAGE_CHECKSUM_OFFSET + 4].fill(0);
        let checksum = crc32fast::hash(page);
        page[TEST_PAGE_CHECKSUM_OFFSET..TEST_PAGE_CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
        fs::write(path, bytes).expect("database should be writable");
    }

    #[tokio::test]
    async fn unreadable_records_are_counted_and_the_remaining_records_are_delivered() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        let playlist = named_live_playlist(10, |index| match index {
            3 => "CORRUPT-MARKER-AAAA".to_string(),
            7 => "CORRUPT-MARKER-BBBB".to_string(),
            _ => format!("Healthy {index}"),
        });
        persist_input_m3u_playlist(&app_config, &path, &playlist).await.expect("playlist should persist");
        corrupt_value_containing(&path, "CORRUPT-MARKER-AAAA");
        corrupt_value_containing(&path, "CORRUPT-MARKER-BBBB");

        let mut stream = iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, |_: &M3uPlaylistItem| true)
            .await
            .expect("playlist database exists");
        let mut delivered = Vec::new();
        while let Some(item) = stream.next().await {
            delivered.push(item.expect("unreadable records are skipped, not delivered as errors").name.to_string());
        }

        assert_eq!(delivered.len(), 8, "healthy records must still be delivered");
        assert!(delivered.iter().all(|name| name.starts_with("Healthy ")));
        assert_eq!(stream.skipped_records(), 2);
    }

    #[tokio::test]
    async fn healthy_database_reports_no_skipped_records() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(20)).await.expect("playlist should persist");

        let mut stream = iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, |_: &M3uPlaylistItem| true)
            .await
            .expect("playlist database exists");
        let mut delivered = 0usize;
        while stream.next().await.is_some() {
            delivered += 1;
        }

        assert_eq!(delivered, 20);
        assert_eq!(stream.skipped_records(), 0);
    }

    /// Parks the producer inside its first `accept` call until [`Self::release`],
    /// so a test controls exactly when the blocking producer is still active.
    struct ParkedProducer {
        entered_rx: std_mpsc::Receiver<()>,
        release_tx: std_mpsc::Sender<()>,
    }

    impl ParkedProducer {
        /// Returns an accepting filter that parks on its first call, and the handle controlling it.
        fn accept_all<I>() -> (impl Fn(&I) -> bool + Send + 'static, Self) {
            let (entered_tx, entered_rx) = std_mpsc::channel::<()>();
            let (release_tx, release_rx) = std_mpsc::channel::<()>();
            let first_call = AtomicUsize::new(0);
            let accept = move |_: &I| {
                if first_call.fetch_add(1, Ordering::SeqCst) == 0 {
                    let _ = entered_tx.send(());
                    let _ = release_rx.recv();
                }
                true
            };
            (accept, Self { entered_rx, release_tx })
        }

        async fn wait_entered(self) -> Self {
            timeout(Duration::from_secs(5), task::spawn_blocking(move || self.entered_rx.recv().map(|()| self)))
                .await
                .expect("producer should reach the first accept call")
                .expect("wait task should not panic")
                .expect("producer should signal the first accept call")
        }

        fn release(&self) { self.release_tx.send(()).expect("producer must still be parked in accept"); }
    }

    fn persist_xtream_playlist(path: &Path, entries: u32) {
        let mut tree = BPlusTree::new();
        for id in 1..=entries {
            tree.insert(id, xtream_playlist_item(id));
        }
        tree.store(path).expect("xtream playlist should persist");
    }

    /// While the producer is parked the stream is dropped: the single shared lock
    /// must stay held by the producer, and only its end releases it.
    #[tokio::test]
    async fn m3u_lock_is_held_until_both_stream_and_producer_are_done() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(ENTRY_COUNT))
            .await
            .expect("playlist should persist");

        let (accept, producer) = ParkedProducer::accept_all::<M3uPlaylistItem>();
        let stream =
            iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, accept).await.expect("playlist database exists");
        let producer = producer.wait_entered().await;

        let mut write = Box::pin(app_config.file_locks.write_lock(&path));
        assert!(poll!(&mut write).is_pending(), "writer must wait while stream and producer are alive");

        drop(stream);
        assert!(poll!(&mut write).is_pending(), "producer alone must keep the lock after the stream is gone");

        producer.release();
        let guard = timeout(Duration::from_secs(5), write).await.expect("writer must proceed once the producer ended");
        drop(guard);
    }

    /// The other direction: the producer is long finished, the undrained-then-
    /// dropped stream is the last owner of the shared lock.
    #[tokio::test]
    async fn m3u_lock_is_held_by_the_stream_after_the_producer_finished() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(20)).await.expect("playlist should persist");

        let mut stream = iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, |_: &M3uPlaylistItem| true)
            .await
            .expect("playlist database exists");
        let mut delivered = 0usize;
        while stream.next().await.is_some() {
            delivered += 1;
        }
        assert_eq!(delivered, 20);

        let mut write = Box::pin(app_config.file_locks.write_lock(&path));
        assert!(poll!(&mut write).is_pending(), "the open stream must keep the file locked");

        drop(stream);
        let guard = timeout(Duration::from_secs(5), write).await.expect("writer must proceed once the stream is gone");
        drop(guard);
    }

    /// A reader queued after the writer is served after it: no starvation and no
    /// deadlock between the open stream, the queued writer and the late reader.
    #[tokio::test]
    async fn m3u_reader_queued_behind_a_writer_is_served_after_the_writer() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(ENTRY_COUNT))
            .await
            .expect("playlist should persist");

        let (accept, producer) = ParkedProducer::accept_all::<M3uPlaylistItem>();
        let stream =
            iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, accept).await.expect("playlist database exists");
        let producer = producer.wait_entered().await;

        let mut write = Box::pin(app_config.file_locks.write_lock(&path));
        assert!(poll!(&mut write).is_pending());
        let mut late_read = Box::pin(app_config.file_locks.read_lock(&path));
        assert!(poll!(&mut late_read).is_pending(), "a reader must queue behind the waiting writer");

        drop(stream);
        producer.release();

        let write_guard = timeout(Duration::from_secs(5), &mut write).await.expect("writer must not deadlock");
        assert!(poll!(&mut late_read).is_pending(), "the late reader must wait for the writer");
        drop(write_guard);
        let read_guard = timeout(Duration::from_secs(5), late_read).await.expect("late reader must be served");
        drop(read_guard);
    }

    /// Structural regression guard for the old two-guard shape (a stream guard plus
    /// a separate producer guard taken one after the other on the same path): a
    /// writer queued between those two acquisitions deadlocked the task, the
    /// writer and every later reader. Only one guard exists now, so once the
    /// stream is dropped and the producer ended nothing is left outstanding: the
    /// queued writer completes and the path is immediately writable again.
    #[tokio::test]
    async fn m3u_leaves_no_read_guard_outstanding_after_stream_and_producer_end() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("input.db");
        let app_config = app_config();
        persist_input_m3u_playlist(&app_config, &path, &live_playlist(ENTRY_COUNT))
            .await
            .expect("playlist database should persist");

        let mut stream = iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, |_: &M3uPlaylistItem| true)
            .await
            .expect("playlist database exists");
        assert!(stream.next().await.is_some());

        let mut write = Box::pin(app_config.file_locks.write_lock(&path));
        assert!(poll!(&mut write).is_pending());

        drop(stream);
        let guard = timeout(Duration::from_secs(5), write)
            .await
            .expect("queued writer must complete without any further read guard being released");
        drop(guard);
        assert!(
            app_config.file_locks.try_write_lock(&path).await.is_ok(),
            "no read guard may be left behind by the iteration"
        );
    }

    #[tokio::test]
    async fn missing_database_returns_none_and_holds_no_lock() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("missing.db");
        let app_config = app_config();

        let m3u = iter_raw_playlist::<M3u, Arc<str>, _>(&app_config, &path, |_: &M3uPlaylistItem| true).await;
        assert!(m3u.is_none());
        assert!(app_config.file_locks.try_write_lock(&path).await.is_ok(), "M3U must not keep a lock");

        let xtream = iter_raw_playlist::<Xtream, u32, _>(&app_config, &path, |_: &XtreamPlaylistItem| true).await;
        assert!(xtream.is_none());
        assert!(app_config.file_locks.try_write_lock(&path).await.is_ok(), "Xtream must not keep a lock");
    }

    /// `HOLD_ITER_LOCK == false`: the producer alone holds the (single) lock, the
    /// returned stream does not.
    #[tokio::test]
    async fn xtream_lock_is_held_by_the_producer_only() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let path = temp.path().join("live.db");
        // Fewer entries than the channel depth: the producer ends without a consumer.
        persist_xtream_playlist(&path, 20);
        let app_config = app_config();

        let (accept, producer) = ParkedProducer::accept_all::<XtreamPlaylistItem>();
        let stream =
            iter_raw_playlist::<Xtream, u32, _>(&app_config, &path, accept).await.expect("playlist database exists");
        let producer = producer.wait_entered().await;

        let mut write = Box::pin(app_config.file_locks.write_lock(&path));
        assert!(poll!(&mut write).is_pending(), "the producer must keep the file locked");

        producer.release();
        let guard = timeout(Duration::from_secs(5), &mut write)
            .await
            .expect("writer must proceed once the producer ended, even though the stream is still open");
        drop(guard);
        drop(stream);
    }

    #[test]
    fn empty_stream_reports_no_skipped_records() {
        let (_tx, rx) = mpsc::channel::<u8>(1);
        assert_eq!(LockedReceiverStream::new_empty(rx).skipped_records(), 0);
    }
}
