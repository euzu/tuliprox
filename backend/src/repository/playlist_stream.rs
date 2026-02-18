use crate::utils::FileReadGuard;
use futures::Stream;
use log::error;
use serde::{Deserialize, Serialize};
use shared::error::{info_err, TuliproxError};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{BPlusTreeQuery, BPlusTreeSortedIteratorOwned, PlaylistIteratorReader, SortedIndexReader};

/// Stream wrapper that holds a file read lock for the lifetime of the stream.
pub struct LockedReceiverStream<T> {
    rx: ReceiverStream<T>,
    _guard: FileReadGuard,
}

impl<T> LockedReceiverStream<T> {
    pub fn new(rx: mpsc::Receiver<T>, guard: FileReadGuard) -> Self {
        Self {
            rx: ReceiverStream::new(rx),
            _guard: guard,
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
pub fn open_playlist_reader<K, V, SortKey>(
    path: &Path,
    index_path: &Path,
    sorted_err_prefix: Option<&str>,
) -> Result<PlaylistIteratorReader<K, V, SortKey>, TuliproxError>
where
    K: Ord + Serialize + for<'de> Deserialize<'de> + Clone,
    V: Serialize + for<'de> Deserialize<'de> + Clone,
    SortKey: for<'de> Deserialize<'de>,
{
    let query = BPlusTreeQuery::<K, V>::try_new(path)
        .map_err(|err| info_err!("Could not open BPlusTreeQuery {path:?} - {err}"))?;

    if index_path.exists() {
        match SortedIndexReader::<SortKey, K>::open(index_path) {
            Ok(index_reader) => {
                let (filepath, file, mmap) = query.into_sorted_parts();
                let reader = BPlusTreeSortedIteratorOwned::from_index_reader(index_reader, filepath, file, mmap);
                return Ok(PlaylistIteratorReader::Sorted(reader));
            }
            Err(err) => {
                if let Some(prefix) = sorted_err_prefix {
                    error!("{prefix}: {err}");
                }
            }
        }
    }

    Ok(PlaylistIteratorReader::Unsorted(query.disk_iter()))
}
