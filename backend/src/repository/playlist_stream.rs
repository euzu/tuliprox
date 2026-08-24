use crate::utils::FileReadGuard;
use futures::Stream;
use log::error;
use serde::{Deserialize, Serialize};
use shared::error::{ TuliproxError};
use std::{io, path::Path};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{BPlusTreeDiskIteratorOwned, BPlusTreeQuery, SortedIndexIterator};

pub(crate) enum PlaylistIteratorReader<K, V, SortKey> {
    Sorted {
        iterator: SortedIndexIterator<K, V, SortKey>,
        fallback_path: std::path::PathBuf,
        yielded: bool,
    },
    Unsorted(BPlusTreeDiskIteratorOwned<K, V>),
}

impl<K, V, SortKey> Iterator for PlaylistIteratorReader<K, V, SortKey>
where
    K: Ord + for<'de> Deserialize<'de>,
    V: for<'de> Deserialize<'de>,
    SortKey: Ord + for<'de> Deserialize<'de>,
{
    type Item = io::Result<(K, V)>;

    fn next(&mut self) -> Option<Self::Item> {
        let fallback = match self {
            Self::Sorted { iterator, fallback_path, yielded } => match iterator.next() {
                Some(Ok(entry)) => {
                    *yielded = true;
                    return Some(Ok(entry));
                }
                Some(Err(error)) if !*yielded => Some((fallback_path.clone(), error)),
                other => return other,
            },
            Self::Unsorted(iterator) => return iterator.next(),
        };
        let (path, index_error) = fallback?;
        match BPlusTreeQuery::try_new(&path) {
            Ok(query) => {
                *self = Self::Unsorted(query.disk_iter());
                self.next()
            }
            Err(tree_error) => Some(Err(io::Error::new(
                tree_error.kind(),
                format!("sorted index failed before its first entry ({index_error}); tree fallback failed: {tree_error}"),
            ))),
        }
    }
}

/// Stream wrapper that holds a file read lock for the lifetime of the stream.
pub struct LockedReceiverStream<T> {
    rx: ReceiverStream<T>,
    _guard: Option<FileReadGuard>,
}

impl<T> LockedReceiverStream<T> {
    pub fn new(rx: mpsc::Receiver<T>, guard: FileReadGuard) -> Self {
        Self {
            rx: ReceiverStream::new(rx),
            _guard: Some(guard),
        }
    }

    /// Creates an intentionally empty stream without a `FileReadGuard`.
    ///
    /// Safety invariant:
    /// This must only be used for receivers whose producer never touches an
    /// on-disk playlist. It is safe for ghost/closed channels where the stream
    /// is semantically empty from the start, for example
    /// `XtreamPlaylistIterator::empty()`.
    pub fn new_empty(rx: mpsc::Receiver<T>) -> Self {
        Self {
            rx: ReceiverStream::new(rx),
            _guard: None,
        }
    }
}

impl<T> Stream for LockedReceiverStream<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

/// Open a playlist reader with sorted-index fallback.
///
/// NOTE: This performs disk I/O and should be used inside `spawn_blocking`.
pub(crate) fn open_playlist_reader<K, V, SortKey>(
    path: &Path,
    index_path: &Path,
    sorted_err_prefix: Option<&str>,
) -> Result<PlaylistIteratorReader<K, V, SortKey>, TuliproxError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
    SortKey: Ord + for<'de> Deserialize<'de>,
{
    let query = BPlusTreeQuery::<K, V>::try_new(path)
        .map_err(|err| TuliproxError::Config(format!(
            "Could not open BPlusTreeQuery {} - {err}",
            path.display()
        )))?;

    if index_path.exists() {
        match SortedIndexIterator::open(query, index_path) {
            Ok(iterator) => {
                return Ok(PlaylistIteratorReader::Sorted {
                    iterator,
                    fallback_path: path.to_path_buf(),
                    yielded: false,
                });
            }
            Err(err) => {
                if let Some(prefix) = sorted_err_prefix {
                    error!("{prefix}: {err}");
                }
            }
        }
    }

    let query = BPlusTreeQuery::<K, V>::try_new(path).map_err(|err| {
        TuliproxError::Config(format!("Could not reopen BPlusTreeQuery {} - {err}", path.display()))
    })?;
    Ok(PlaylistIteratorReader::Unsorted(query.disk_iter()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::bplustree::{sorted_index::v4, BPlusTree, Locator};
    use std::{fs, io};

    #[test]
    fn corrupt_first_index_entry_falls_back_before_yielding() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let database = dir.path().join("playlist.db");
        let index = dir.path().join("playlist.idx");
        let mut tree = BPlusTree::new();
        tree.insert(1u32, String::from("ccc"));
        tree.insert(2u32, String::from("a"));
        tree.store_with_index(&database, String::len)?;
        let mut bytes = fs::read(&index)?;
        *bytes.get_mut(72).ok_or_else(|| io::Error::other("index body missing"))? ^= 1;
        fs::write(&index, bytes)?;

        let reader = open_playlist_reader::<u32, String, usize>(&database, &index, None)
            .map_err(io::Error::other)?;
        assert_eq!(reader.collect::<io::Result<Vec<_>>>()?, vec![(1, String::from("ccc")), (2, String::from("a"))]);
        Ok(())
    }

    #[test]
    fn corrupt_late_index_entry_does_not_hide_following_entries() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let database = dir.path().join("playlist-late.db");
        let index = dir.path().join("playlist-late.idx");
        let mut tree = BPlusTree::new();
        tree.insert(1u32, String::from("one"));
        tree.insert(2u32, String::from("two"));
        tree.insert(3u32, String::from("three"));
        tree.store(&database)?;
        let mut query = BPlusTreeQuery::<u32, String>::try_new(&database)?;
        let entries = query.collect_with_locators()?;
        let (database_id, generation) = query.snapshot_identity();
        drop(query);
        let mut writer = v4::Writer::<u32, u32>::new(&index, database_id, generation)?;
        writer.push(&1, &entries[0].0, entries[0].2)?;
        writer.push(&2, &entries[1].0, Locator { slot_index: u16::MAX, ..entries[1].2 })?;
        writer.push(&3, &entries[2].0, entries[2].2)?;
        writer.finish()?;

        let mut reader = open_playlist_reader::<u32, String, u32>(&database, &index, None)
            .map_err(io::Error::other)?;
        assert_eq!(reader.next().transpose()?, Some((1, String::from("one"))));
        assert!(reader.next().is_some_and(|entry| entry.is_err()));
        assert_eq!(reader.next().transpose()?, Some((3, String::from("three"))));
        assert!(reader.next().is_none());
        Ok(())
    }
}
