use crate::repository::stream_history::{StreamHistoryFileReader, AsyncStreamHistoryPendingReader};
use crate::utils::stream_history_viewer::{discover_files, CompiledFilter, TimeRange};
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
#[allow(dead_code)]
pub struct StreamHistoryStream {
    inner: ReceiverStream<StreamHistoryRecord>,
}

#[allow(dead_code)]
impl StreamHistoryStream {
    /// Creates a new stream by reading files in a background task.
    ///
    /// Pending files (uncompressed) are read with fully async I/O using
    /// `AsyncStreamHistoryPendingReader` (`tokio::fs::File`).
    /// Archive files (LZ4-compressed) still require `spawn_blocking` because
    /// `lz4_flex::frame::FrameDecoder` is sync-only.
    #[allow(clippy::needless_pass_by_value)]
    #[allow(dead_code)]
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
    #[allow(clippy::needless_pass_by_value)]
    #[allow(dead_code)]
    async fn read_files_async(
        dir: &str,
        time_range: &TimeRange,
        filters: &CompiledFilter,
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
                // Archive files: LZ4 decompression is sync-only — use spawn_blocking
                let file_path = file.path.clone();
                let time_range_val = *time_range;
                let result = tokio::task::spawn_blocking(move || {
                    Self::read_archive_file(&file_path, time_range_val)
                }).await;
                match result {
                    Ok(iter) => {
                        for result in iter {
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
                                    log::warn!("Failed to read archive record: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Archive read task panicked: {e}");
                    }
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
    /// Collects into Vec because `dyn Iterator` is not Send, and `lz4_flex` is sync-only anyway.
    #[allow(dead_code)]
    fn read_archive_file(
        path: &Path,
        time_range: TimeRange,
    ) -> Vec<std::io::Result<StreamHistoryRecord>> {
        let (reader, _) = match StreamHistoryFileReader::from_archive(path, Some(time_range)) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("Failed to open archive {}: {e}", path.display());
                return Vec::new();
            }
        };
        reader.collect()
    }
}

impl TokioStream for StreamHistoryStream {
    type Item = StreamHistoryRecord;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}
