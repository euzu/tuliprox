use super::{ProgressiveBudgetManager, SegmentRevisionGuard, SegmentRevisionState};
use crate::sync_ext::MutexExt;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use std::{
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::{timeout_at, Instant},
};

/// Largest chunk a reader takes from a completed revision file per read.
const REVISION_FILE_CHUNK_BYTES: u64 = 16 * 1024;

pub struct ProgressiveReplay {
    pub slot: super::progressive_budget::ProgressiveSlot,
    chunks: Vec<(u64, Bytes)>,
}

impl std::fmt::Debug for ProgressiveReplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressiveReplay").field("chunks", &self.chunks.len()).finish_non_exhaustive()
    }
}

impl ProgressiveReplay {
    pub fn new(budget: &Arc<ProgressiveBudgetManager>) -> Option<Self> {
        budget.try_claim().map(|slot| Self { slot, chunks: Vec::new() })
    }
    pub fn push(&mut self, chunk: &[u8]) -> io::Result<()> {
        let start = self
            .chunks
            .last()
            .map_or(Some(0), |(start, bytes)| start.checked_add(u64::try_from(bytes.len()).ok()?))
            .ok_or_else(|| io::Error::other("progressive replay offset overflow"))?;
        if self.chunks.len() == self.chunks.capacity() {
            self.grow_chunk_index()?;
        }
        let chunk = self.slot.copy_chunk(chunk)?;
        self.chunks.push((start, chunk));
        Ok(())
    }
    fn grow_chunk_index(&mut self) -> io::Result<()> {
        let item_size = std::mem::size_of::<(u64, Bytes)>();
        let old_bytes = self
            .chunks
            .capacity()
            .checked_mul(item_size)
            .ok_or_else(|| io::Error::other("replay index capacity overflow"))?;
        let capacity = self
            .chunks
            .capacity()
            .max(2)
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("replay index growth overflow"))?;
        let reserved =
            capacity.checked_mul(item_size).ok_or_else(|| io::Error::other("replay index budget overflow"))?;
        self.slot.reserve_metadata(reserved)?;
        let mut next = Vec::new();
        if let Err(error) = next.try_reserve_exact(capacity) {
            self.slot.release_metadata(reserved);
            return Err(io::Error::other(error));
        }
        let actual = next
            .capacity()
            .checked_mul(item_size)
            .ok_or_else(|| io::Error::other("replay allocation capacity overflow"))?;
        if let Err(error) = self.slot.reserve_metadata(actual.saturating_sub(reserved)) {
            drop(next);
            self.slot.release_metadata(reserved);
            return Err(error);
        }
        next.append(&mut self.chunks);
        drop(std::mem::replace(&mut self.chunks, next));
        self.slot.release_metadata(old_bytes);
        Ok(())
    }

    fn at_offset(&self, offset: u64) -> Option<Bytes> {
        let index = self.chunks.partition_point(|(start, _)| *start <= offset).checked_sub(1)?;
        let (start, chunk) = self.chunks.get(index)?;
        let local_offset = usize::try_from(offset.checked_sub(*start)?).ok()?;
        (local_offset < chunk.len()).then(|| chunk.slice(local_offset..))
    }
}

impl Drop for ProgressiveReplay {
    fn drop(&mut self) {
        // The index allocation is released before its slot-owned metadata budget.
        drop(std::mem::take(&mut self.chunks));
    }
}

/// An open revision file that owns its own pin, so GC cannot retire the revision
/// while any handle to it is open, including one inside an in-flight blocking read.
struct PinnedRevisionFile {
    file: std::fs::File,
    // Fields drop in declaration order: the handle closes before the pin is released.
    _pin: SegmentRevisionGuard,
}

/// Reads one response body on its own task, so the deadline can cancel it (and close
/// its file) while a stalled client keeps the response stream alive unpolled.
struct RevisionReader {
    guard: SegmentRevisionGuard,
    changes: watch::Receiver<SegmentRevisionState>,
    file: Option<Arc<PinnedRevisionFile>>,
    offset: u64,
    end: Option<u64>,
}

impl RevisionReader {
    async fn run(mut self, sender: mpsc::Sender<io::Result<Bytes>>, finished: Arc<AtomicBool>) {
        let failure = loop {
            let next = tokio::select! {
                biased;
                () = sender.closed() => return,
                next = self.next() => next,
            };
            match next {
                Ok(Some(chunk)) => {
                    if sender.send(Ok(chunk)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => break None,
                Err(error) => break Some(error),
            }
        };
        // The file and pin are released before a stalled client receives the last item.
        drop(self);
        finished.store(true, Ordering::Release);
        if let Some(error) = failure {
            let _ = sender.send(Err(error)).await;
        }
    }

    async fn next(&mut self) -> io::Result<Option<Bytes>> {
        loop {
            let state = self.changes.borrow_and_update().clone();
            match state {
                SegmentRevisionState::Failed => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "HLS published origin fill failed"))
                }
                SegmentRevisionState::Complete(metadata) => {
                    let end = self.end.unwrap_or(metadata.size).min(metadata.size);
                    if self.offset >= end {
                        return Ok(None);
                    }
                    let limit = usize::try_from(end.saturating_sub(self.offset).min(REVISION_FILE_CHUNK_BYTES))
                        .map_err(io::Error::other)?;
                    let file = self.file_at_offset(metadata.path).await?;
                    let bytes = run_blocking(move || {
                        let mut bytes = vec![0u8; limit];
                        let count = (&file.file).read(&mut bytes)?;
                        bytes.truncate(count);
                        Ok(bytes)
                    })
                    .await?;
                    if bytes.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "immutable HLS revision was truncated",
                        ));
                    }
                    self.offset = self.offset.saturating_add(u64::try_from(bytes.len()).map_err(io::Error::other)?);
                    return Ok(Some(Bytes::from(bytes)));
                }
                SegmentRevisionState::Pending => {
                    let chunk = self
                        .guard
                        .revision()
                        .replay
                        .lock_unpoisoned()
                        .as_ref()
                        .and_then(|replay| replay.at_offset(self.offset));
                    if let Some(chunk) = chunk {
                        self.offset = self.offset.saturating_add(u64::try_from(chunk.len()).map_err(io::Error::other)?);
                        return Ok(Some(chunk));
                    }
                    self.changes
                        .changed()
                        .await
                        .map_err(|_| io::Error::new(io::ErrorKind::UnexpectedEof, "HLS revision producer ended"))?;
                }
            }
        }
    }

    /// A reused handle is already positioned at `offset`.
    async fn file_at_offset(&mut self, path: PathBuf) -> io::Result<Arc<PinnedRevisionFile>> {
        if let Some(file) = &self.file {
            return Ok(Arc::clone(file));
        }
        let pin = self.guard.clone();
        let offset = self.offset;
        let file = run_blocking(move || {
            let mut file = std::fs::File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            Ok(Arc::new(PinnedRevisionFile { file, _pin: pin }))
        })
        .await?;
        Ok(Arc::clone(self.file.insert(file)))
    }
}

async fn run_blocking<T: Send + 'static>(work: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(io::Error::other)?
}

struct RevisionBodyState {
    items: mpsc::Receiver<io::Result<Bytes>>,
    finished: Arc<AtomicBool>,
    deadline: Instant,
    ended: bool,
}

type RevisionBody = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

pub fn revision_body(guard: SegmentRevisionGuard, lifetime: Duration, start: u64, end: Option<u64>) -> RevisionBody {
    let deadline = Instant::now() + lifetime;
    let (sender, items) = mpsc::channel(1);
    let finished = Arc::new(AtomicBool::new(false));
    let changes = guard.revision().subscribe();
    let reader = RevisionReader { guard, changes, file: None, offset: start, end };
    let mut reader = tokio::spawn(reader.run(sender, Arc::clone(&finished)));
    tokio::spawn(async move {
        if timeout_at(deadline, &mut reader).await.is_err() {
            reader.abort();
            // Awaiting the cancelled task drops its guard and file handle before this task ends.
            let _ = reader.await;
        }
    });
    futures::stream::unfold(RevisionBodyState { items, finished, deadline, ended: false }, |mut state| async move {
        if state.ended {
            return None;
        }
        // The lifetime is absolute: data still buffered at the deadline is not served.
        let next = if Instant::now() < state.deadline {
            timeout_at(state.deadline, state.items.recv()).await.ok()
        } else {
            None
        };
        match next {
            Some(Some(item)) => Some((item, state)),
            Some(None) if state.finished.load(Ordering::Acquire) => None,
            Some(None) => {
                state.ended = true;
                Some((
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "HLS revision reader ended unexpectedly")),
                    state,
                ))
            }
            None => {
                state.ended = true;
                Some((Err(io::Error::new(io::ErrorKind::TimedOut, "HLS revision reader lifetime exceeded")), state))
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_index_growth_and_retained_chunk_have_separate_budget_lifetimes() -> io::Result<()> {
        let budget = ProgressiveBudgetManager::new(tuliprox_core::model::HlsStartupConfig::default());
        let mut replay = ProgressiveReplay::new(&budget).ok_or_else(|| io::Error::other("slot"))?;
        for _ in 0..100 {
            replay.push(b"x")?;
        }
        let retained = replay.at_offset(99).ok_or_else(|| io::Error::other("retained chunk"))?;
        let total = budget.usage().1;
        let index_bytes = replay.chunks.capacity() * std::mem::size_of::<(u64, Bytes)>();
        assert!(total > index_bytes);
        drop(replay);
        assert_eq!(budget.usage().0, 0);
        assert!(budget.usage().1 > 0 && budget.usage().1 < index_bytes);
        assert_eq!(retained.as_ref(), b"x");
        drop(retained);
        assert_eq!(budget.usage(), (0, 0));
        Ok(())
    }

    #[tokio::test]
    async fn unpolled_reader_detaches_at_absolute_deadline() -> io::Result<()> {
        let store = super::super::SegmentRevisionStore::default();
        let revision =
            store.create(super::super::ProxySessionId("deadline".into()), 0, super::super::SegmentRevisionKind::Raw)?;
        let body = revision_body(revision.clone(), Duration::from_millis(10), 0, None);
        drop(revision);
        assert!(store.retire_unpinned()?.is_empty());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(store.retire_unpinned()?.len(), 1);
        drop(body);
        Ok(())
    }
    #[tokio::test]
    async fn stalled_reader_closes_its_revision_file_at_the_deadline() -> io::Result<()> {
        use futures::FutureExt;

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("revision.ts");
        let size = REVISION_FILE_CHUNK_BYTES * 8;
        tokio::fs::write(&path, vec![0x47; usize::try_from(size).map_err(io::Error::other)?]).await?;
        let store = super::super::SegmentRevisionStore::default();
        let revision =
            store.create(super::super::ProxySessionId("stalled".into()), 0, super::super::SegmentRevisionKind::Raw)?;
        assert!(revision.revision().complete(super::super::CachedSegmentMetadata { path, size }));
        let mut body = revision_body(revision.clone(), Duration::from_millis(100), 0, None);

        let first = body.next().await.ok_or_else(|| io::Error::other("first chunk"))??;
        assert_eq!(u64::try_from(first.len()).map_err(io::Error::other)?, REVISION_FILE_CHUNK_BYTES);
        // The client polls the next read once and then stops reading mid-response.
        let _ = body.next().now_or_never();
        // The open file owns its own pin, so the pin count observes the handle on every platform:
        // this test, the reader and the open revision file.
        assert_eq!(revision.revision().pin_count(), 3, "an unfinished reader keeps its revision file open");

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            revision.revision().pin_count(),
            1,
            "the deadline closes the file while the response stream is alive"
        );
        drop(revision);
        assert_eq!(store.retire_unpinned()?.len(), 1, "the deadline releases the revision pin");
        let tail: Vec<_> = body.collect().await;
        assert!(tail
            .last()
            .is_some_and(|item| item.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)));
        Ok(())
    }
}
