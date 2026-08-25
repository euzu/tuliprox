use crate::repository::stream_history::{StreamHistoryFileReader, AsyncStreamHistoryPendingReader};
use super::viewer::{discover_files, CompiledFilter, TimeRange};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream as TokioStream;
use crate::model::StreamHistoryRecord;

/// Async stream of stream history records.
/// NOTE: The stream implementation is ready but not yet wired to the API.
/// Kept here for future use when we move back to streaming responses.
pub struct StreamHistoryStream {
    inner: ReceiverStream<StreamHistoryRecord>,
}

impl StreamHistoryStream {
    /// Creates a new stream by reading files in a background task.
    ///
    /// Pending files (uncompressed) are read with fully async I/O using
    /// `AsyncStreamHistoryPendingReader` (`tokio::fs::File`).
    /// Archive files (LZ4-compressed) still require `spawn_blocking` because
    /// `lz4_flex::frame::FrameDecoder` is sync-only.
    #[allow(dead_code)]
    // Reserved for a future streaming API path; the current paged endpoints use
    // a synchronous iterator with blocking aggregation.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn new(dir: String, time_range: TimeRange, filters: Arc<CompiledFilter>) -> Self {
        let (tx, rx) = mpsc::channel(1024);
        let filters_clone = Arc::clone(&filters);
        let dir_clone = dir.clone();

        tokio::spawn(async move {
            Self::read_files_async(&dir_clone, &time_range, &filters_clone, tx).await;
        });

        Self {
            inner: ReceiverStream::new(rx),
        }
    }

    /// Reads files and sends records through the channel.
    /// - Pending files: fully async (`tokio::fs::File` + `AsyncStreamHistoryPendingReader`)
    /// - Archive files: sync via `spawn_blocking` (`lz4_flex` is sync-only)
    #[allow(dead_code)]
    // Kept together with `StreamHistoryStream::new` for the future
    // streaming-history endpoint; not currently wired into routing.
    #[allow(clippy::needless_pass_by_value)]
    async fn read_files_async(
        dir: &str,
        time_range: &TimeRange,
        filters: &Arc<CompiledFilter>,
        tx: mpsc::Sender<StreamHistoryRecord>,
    ) {
        let files = match discover_files(Path::new(dir), time_range).await {
            Ok(f) => f,
            Err(e) => {
                log::error!("Failed to discover stream history files: {e}");
                return;
            }
        };
        let (range_start, range_end) = *time_range;

        for file in &files {
            if file.is_archive {
                // Archive files: LZ4 decompression is sync-only — use spawn_blocking.
                // The blocking task sends directly into the async channel via blocking_send(),
                // avoiding the deadlock that would occur with an intermediate std::sync::mpsc
                // channel whose bounded sender blocks before the async side starts draining.
                let file_path = file.path.clone();
                let time_range_val = *time_range;
                let tx_clone = tx.clone();
                let filters_clone = Arc::clone(filters);
                let result = tokio::task::spawn_blocking(move || {
                    Self::read_archive_file(&file_path, time_range_val, &filters_clone, &tx_clone);
                }).await;
                if let Err(e) = result {
                    log::error!("Archive read task failed: {e}");
                }
            } else {
                // Pending files: fully async using AsyncStreamHistoryPendingReader
                match AsyncStreamHistoryPendingReader::open(&file.path, Some(*time_range)).await {
                    Ok((mut reader, _)) => {
                        while let Some(result) = reader.next_record().await {
                            match result {
                                Ok(record) => {
                                    if record.event_ts_utc < range_start || record.event_ts_utc > range_end {
                                        continue;
                                    }
                                    if !filters.matches(&record) {
                                        continue;
                                    }
                                    if tx.send(record).await.is_err() {
                                        return; // Receiver dropped
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Failed to read pending record: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to open pending file {}: {e}", file.path.display());
                    }
                }
            }
        }
    }

    /// Read a LZ4 archive file (runs in `spawn_blocking`).
    /// Streams records incrementally via `blocking_send` into the async channel,
    /// so memory usage stays bounded regardless of archive size.
    #[allow(dead_code)]
    // Companion helper for the reserved async streaming history reader.
    fn read_archive_file(
        path: &Path,
        time_range: TimeRange,
        filters: &CompiledFilter,
        tx: &mpsc::Sender<StreamHistoryRecord>,
    ) {
        let (range_start, range_end) = time_range;
        let (reader, _) = match StreamHistoryFileReader::from_archive(path, Some(time_range)) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed to open archive {}: {e}", path.display());
                return;
            }
        };
        for result in reader {
            match result {
                Ok(record) => {
                    if record.event_ts_utc < range_start || record.event_ts_utc > range_end {
                        continue;
                    }
                    if !filters.matches(&record) {
                        continue;
                    }
                    if tx.blocking_send(record).is_err() {
                        // Receiver dropped, stop sending
                        return;
                    }
                }
                Err(e) => {
                    log::warn!("Failed to read archive record: {e}");
                }
            }
        }
    }
}

impl TokioStream for StreamHistoryStream {
    type Item = StreamHistoryRecord;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}
