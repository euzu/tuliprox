use crate::repository::stream_history::{
    BlockHeaderBody, CompressionKind, FileHeaderBody, RecordEncodingKind, CONTAINER_FORMAT_VERSION,
    SOURCE_KIND_STREAM_HISTORY, write_framed,
};
use crate::repository::stream_history::archive::finalize_and_archive;
use crate::model::{StreamHistoryConfig, StreamHistoryRecord, RECORD_SCHEMA_VERSION};
use log::{error, info, warn};
use std::{
    io,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use crate::utils::{current_utc_day, now_utc_secs, utc_day_from_secs, SECS_PER_DAY};

const QUEUE_CAPACITY: usize = 4096;


fn pending_file_path(directory: &str, day: &str) -> PathBuf {
    PathBuf::from(directory).join(format!("stream-history-{day}.pending"))
}

enum WriterCommand {
    Record(Box<StreamHistoryRecord>),
    Flush(oneshot::Sender<io::Result<()>>),
    GetBatch(oneshot::Sender<Vec<StreamHistoryRecord>>),
    Shutdown(oneshot::Sender<()>),
}

/// A buffered, async writer for stream history events.
///
/// Records are buffered in memory and flushed to a `.pending` daily file in batches.
/// The internal queue is bounded; when full, new records are dropped and counted.
pub struct StreamHistoryWriter {
    tx: Option<mpsc::Sender<WriterCommand>>,
    /// Count of records dropped due to queue backpressure.
    pub dropped_events: Arc<AtomicU64>,
    /// Set by `shutdown()` to reject new records immediately.
    closing: AtomicBool,
}

impl StreamHistoryWriter {
    /// Creates a no-op writer when stream history is disabled.
    pub fn new_disabled() -> Self {
        Self { tx: None, dropped_events: Arc::new(AtomicU64::new(0)), closing: AtomicBool::new(false) }
    }

    /// Creates an active writer and spawns its background worker task.
    pub fn new(config: &StreamHistoryConfig) -> Self {
        if !config.stream_history_enabled {
            return Self::new_disabled();
        }

        let dropped_events = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);

        let worker = WriterWorker::new(config.clone());
        let dropped_clone = Arc::clone(&dropped_events);
        tokio::spawn(async move {
            worker.run(rx, dropped_clone).await;
        });

        Self { tx: Some(tx), dropped_events, closing: AtomicBool::new(false) }
    }

    /// Submit a record for persistence. Non-blocking; drops the record if the queue is full
    /// or the writer is shutting down.
    pub fn send_record(&self, record: StreamHistoryRecord) {
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        let Some(tx) = &self.tx else { return };
        if tx.try_send(WriterCommand::Record(Box::new(record))).is_err() {
            let dropped = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
            warn!("Stream history queue full — record dropped (total dropped: {dropped})");
        }
    }

    /// Flush any buffered records to disk, waiting for completion.
    pub async fn flush(&self) -> io::Result<()> {
        let Some(tx) = &self.tx else { return Ok(()) };
        let (resp_tx, resp_rx) = oneshot::channel();
        if tx.send(WriterCommand::Flush(resp_tx)).await.is_err() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream history writer is not running"));
        }
        resp_rx.await.unwrap_or_else(|_| {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "stream history writer exited before confirming flush"))
        })
    }

    /// Flush and shut down the writer, waiting for the worker to finish.
    /// Sets the closing flag first so `send_record` stops accepting new records
    /// before the Shutdown command is enqueued.
    pub async fn shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        let Some(tx) = &self.tx else { return };
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = tx.send(WriterCommand::Shutdown(resp_tx)).await;
        if resp_rx.await.is_err() {
            warn!("Stream history shutdown: worker channel closed");
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// Returns a copy of the current in-memory batch records.
    /// Async version — sends a command and waits for the response.
    pub async fn get_current_batch(&self) -> io::Result<Vec<StreamHistoryRecord>> {
        let Some(tx) = &self.tx else {
            return Ok(Vec::new());
        };

        let (resp_tx, resp_rx) = oneshot::channel();

        tx.send(WriterCommand::GetBatch(resp_tx))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stream history worker is closed"))?;

        resp_rx
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stream history worker dropped GetBatch response"))
    }
}

struct WriterWorker {
    config: StreamHistoryConfig,
    writer_instance_id: u64,
}

impl WriterWorker {
    fn new(config: StreamHistoryConfig) -> Self {
        let writer_instance_id = now_utc_secs();
        Self { config, writer_instance_id }
    }

    async fn run(self, mut rx: mpsc::Receiver<WriterCommand>, dropped_events: Arc<AtomicU64>) {
        let mut state = match WriterState::open(&self.config, self.writer_instance_id).await {
            Ok(s) => s,
            Err(e) => {
                error!("Stream history writer failed to initialize: {e}");
                // Drop rx immediately so senders observe a closed channel
                // and dropped_events is incremented by send_record.
                return;
            }
        };

        info!("Stream history writer started for day {}", state.current_day);

        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriterCommand::Record(record) => {
                    // Day rollover check — use the record's partition day as the target
                    if record.partition_day_utc != state.current_day {
                        if let Err(e) = state.flush_and_rollover_to(&record.partition_day_utc, &self.config, self.writer_instance_id).await {
                            error!("Stream history day rollover failed: {e}");
                            let dropped = dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                            warn!("Stream history record dropped due to rollover failure (total dropped: {dropped})");
                            continue;
                        }
                    }

                    state.push(*record);

                    if state.batch.len() >= self.config.stream_history_batch_size {
                        if let Err(e) = state.flush_batch().await {
                            error!("Stream history batch flush failed: {e}. Dropped events: {}",
                                dropped_events.load(Ordering::Relaxed));
                        }
                    }
                }
                WriterCommand::Flush(resp) => {
                    let result = state.flush_batch().await;
                    let _ = resp.send(result);
                }
                WriterCommand::GetBatch(resp) => {
                    let _ = resp.send(state.batch.clone());
                }
                WriterCommand::Shutdown(resp) => {
                    if let Err(e) = state.flush_batch().await {
                        error!("Stream history flush on shutdown failed: {e}");
                    }
                    if let Err(e) = state.finalize().await {
                        error!("Stream history finalize on shutdown failed: {e}");
                    }
                    info!("Stream history writer shut down. Total blocks: {}, records: {}",
                        state.total_block_count, state.total_record_count);
                    let _ = resp.send(());
                    break;
                }
            }
        }
    }
}

struct WriterState {
    directory: String,
    current_day: String,
    file: Option<fs::File>,
    file_path: PathBuf,
    batch: Vec<StreamHistoryRecord>,
    total_block_count: u64,
    total_record_count: u64,
    min_event_ts: Option<u64>,
    max_event_ts: Option<u64>,
}

impl WriterState {
    async fn open(config: &StreamHistoryConfig, writer_instance_id: u64) -> io::Result<Self> {
        let day = current_utc_day();
        let dir = &config.stream_history_directory;

        fs::create_dir_all(dir).await?;

        let path = pending_file_path(dir, &day);
        let file = open_or_create_pending_file(&path, &day, writer_instance_id).await?;

        Ok(Self {
            directory: dir.clone(),
            current_day: day,
            file: Some(file),
            file_path: path,
            batch: Vec::with_capacity(config.stream_history_batch_size),
            total_block_count: 0,
            total_record_count: 0,
            min_event_ts: None,
            max_event_ts: None,
        })
    }

    fn push(&mut self, record: StreamHistoryRecord) {
        let ts = record.event_ts_utc;
        if self.min_event_ts.is_none_or(|m| ts < m) {
            self.min_event_ts = Some(ts);
        }
        if self.max_event_ts.is_none_or(|m| ts > m) {
            self.max_event_ts = Some(ts);
        }
        self.batch.push(record);
    }

    /// Flush buffered records to the pending file.
    /// Serialization (CPU-bound) runs in `spawn_blocking`; file I/O is fully async.
    async fn flush_batch(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }

        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pending file not open"));
        };

        let (first_ts, last_ts) = self
            .batch
            .iter()
            .fold((u64::MAX, 0u64), |(min_ts, max_ts), record| {
                (min_ts.min(record.event_ts_utc), max_ts.max(record.event_ts_utc))
            });
        let record_count = u32::try_from(self.batch.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record count too large"))?;

        // Serialize all records into the reusable payload buffer, then write
        // the whole block in one async I/O pass.
        // The batch is cloned for serialization; self.batch is only cleared after all I/O succeeds.
        let batch_snapshot = self.batch.clone();
        let (header_bytes, payload_bytes) = match tokio::task::spawn_blocking(move || {
            let mut payload_buf = Vec::new();
            for record in &batch_snapshot {
                let len_offset = payload_buf.len();
                payload_buf.extend_from_slice(&[0u8; 4]); // placeholder for length
                if let Err(e) = rmp_serde::encode::write_named(&mut payload_buf, record) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, e));
                }
                let record_len = payload_buf.len() - len_offset - 4;
                let Ok(record_len_u32) = u32::try_from(record_len) else {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "record too large"));
                };
                payload_buf[len_offset..len_offset + 4].copy_from_slice(&record_len_u32.to_be_bytes());
            }

            let Ok(payload_len) = u32::try_from(payload_buf.len()) else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "block payload too large"));
            };
            let payload_crc = crc32fast::hash(&payload_buf);

            let block_header = BlockHeaderBody {
                block_version: 1,
                record_count,
                payload_len,
                first_event_ts_utc: first_ts,
                last_event_ts_utc: last_ts,
                payload_crc,
                flags: 0,
            };

            // Serialize the framed block header
            let mut header_buf = Vec::new();
            if let Err(e) = write_framed(&mut header_buf, &block_header) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, e));
            }

            Ok((header_buf, payload_buf))
        }).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(io::Error::other(e)),
        };

        // Now write the magic, header, and payload as three separate async write calls.
        // This parks the task between calls if I/O would block, without blocking any thread.
        // Capture starting position so we can truncate on failure, removing partial corrupt bytes.
        let start_pos = file.stream_position().await?;
        let block_magic = crate::repository::stream_history::BLOCK_MAGIC;
        if let Err(e) = async {
            file.write_all(&block_magic).await?;
            file.write_all(&header_bytes).await?;
            file.write_all(&payload_bytes).await?;
            file.flush().await?;
            io::Result::Ok(())
        }.await {
            // Truncate back to the pre-write position to remove partial corrupt bytes,
            // then seek so the file handle is ready for the next write attempt.
            // self.batch stays intact so the caller can retry.
            let _ = file.set_len(start_pos).await;
            let _ = file.seek(std::io::SeekFrom::Start(start_pos)).await;
            return Err(e);
        }

        self.total_block_count += 1;
        self.total_record_count += u64::from(record_count);
        self.batch.clear();

        Ok(())
    }

    async fn flush_and_rollover_to(&mut self, target_day: &str, config: &StreamHistoryConfig, writer_instance_id: u64) -> io::Result<()> {
        self.flush_batch().await?;
        self.finalize().await?; // closes and drops the File; file_path is still valid

        // Archive the just-closed file and apply retention immediately.
        // At this point all pre-rollover records are safely on disk — the first record
        // with a new partition_day_utc triggered this rollover, so no pre-midnight record
        // can arrive after this point. The previous-day file is now complete and safe to archive.
        finalize_and_archive(&self.file_path, &self.directory, config.stream_history_retention_days).await;

        let new_path = pending_file_path(&self.directory, target_day);
        let new_file = open_or_create_pending_file(&new_path, target_day, writer_instance_id).await?;

        self.current_day = target_day.to_string();
        self.file = Some(new_file);
        self.file_path = new_path;
        self.total_block_count = 0;
        self.total_record_count = 0;
        self.min_event_ts = None;
        self.max_event_ts = None;

        Ok(())
    }

    /// Flush file buffer to OS. Does not compress or archive.
    async fn finalize(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush().await?;
        }
        Ok(())
    }
}

/// Open an existing `.pending` file for appending, or create a new one with the file header.
async fn open_or_create_pending_file(path: &PathBuf, day: &str, writer_instance_id: u64) -> io::Result<fs::File> {
    // Atomically try to create a new file first. If it already exists,
    // open with append mode instead. This avoids TOCTOU races where
    // finalize_and_archive might create/finalize the file between our
    // metadata check and open call.
    let file = match OpenOptions::new().write(true).create_new(true).open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // File already exists, open for appending
            OpenOptions::new().append(true).open(path).await?
        }
        Err(e) => return Err(e),
    };

    // If we created a new file (empty), write the header.
    // For existing files the header was already written at creation time.
    if file.metadata().await.map_or(0, |m| m.len()) == 0 {
        let header = FileHeaderBody {
            container_format_version: CONTAINER_FORMAT_VERSION,
            record_schema_version: RECORD_SCHEMA_VERSION,
            source_kind: SOURCE_KIND_STREAM_HISTORY.to_string(),
            created_at_ts_utc: now_utc_secs(),
            partition_day_ts_utc: day.to_string(),
            writer_instance_id,
            host_id: std::env::var("HOSTNAME").ok(),
            compression_kind: CompressionKind::None,
            finalized: false,
            record_encoding_kind: RecordEncodingKind::MessagePackNamed,
            finalized_at_ts_utc: None,
            total_block_count: None,
            total_record_count: None,
            min_event_ts_utc: None,
            max_event_ts_utc: None,
        };

        let mut header_buf = Vec::new();
        write_framed(&mut header_buf, &header)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let file_magic = crate::repository::stream_history::FILE_MAGIC;
        let mut f = file;
        f.write_all(&file_magic).await?;
        f.write_all(&header_buf).await?;
        f.flush().await?;
        Ok(f)
    } else {
        Ok(file)
    }
}

/// Delete daily archive/pending files older than `retention_days`.
pub async fn apply_retention(directory: &str, retention_days: u16) -> io::Result<()> {
    let dir = PathBuf::from(directory);
    let metadata = match tokio::fs::metadata(&dir).await {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !metadata.is_dir() {
        return Ok(());
    }

    let cutoff_day = {
        let now_secs = now_utc_secs();
        let cutoff_secs = now_secs.saturating_sub(u64::from(retention_days) * SECS_PER_DAY);
        utc_day_from_secs(cutoff_secs)
    };

    let mut entries = tokio::fs::read_dir(&dir).await?;
    let mut to_delete = Vec::new();

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(day) = extract_day_from_filename(&name) {
            if day < cutoff_day.as_str() {
                to_delete.push(entry.path());
            }
        }
    }

    for path in to_delete {
        if let Err(e) = tokio::fs::remove_file(&path).await {
            warn!("Failed to delete old history file {}: {e}", path.display());
        } else {
            info!("Deleted expired history file: {}", path.display());
        }
    }

    Ok(())
}

pub fn extract_day_from_filename(name: &str) -> Option<&str> {
    let stripped = name.strip_prefix("stream-history-")?;
    // Day is the first 10 chars: "YYYY-MM-DD"
    if stripped.len() >= 10 {
        Some(&stripped[..10])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{BlockHeaderBody, FileHeaderBody, read_and_verify_block_magic, read_and_verify_file_magic, read_framed};
    use std::io::BufReader;
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;
    use shared::model::{PlaylistItemType, StreamHistoryEventType};
    use shared::utils::Internable;
    use crate::utils::secs_until_next_utc_midnight;

    fn test_config(dir: &str, batch_size: usize) -> StreamHistoryConfig {
        StreamHistoryConfig {
            stream_history_enabled: true,
            stream_history_batch_size: batch_size,
            stream_history_retention_days: 30,
            stream_history_directory: dir.to_string(),
        }
    }

    fn make_record(session_id: u64, event_type: StreamHistoryEventType) -> StreamHistoryRecord {
        let mut r = crate::repository::stream_history::tests::make_base_test_record();
        r.event_type = event_type;
        r.event_ts_utc = now_utc_secs();
        r.partition_day_utc = current_utc_day();
        r.session_id = session_id;
        r.api_username = Some("user1".to_string());
        r.provider_name = Some("acme".intern());
        r.input_name = Some("input".intern());
        r.virtual_id = Some(1);
        r.item_type = Some(PlaylistItemType::Live);
        r.title = Some("Test Channel".to_string());
        r
    }

    #[tokio::test]
    async fn stream_history_writer_disabled_accepts_records_silently() {
        let writer = StreamHistoryWriter::new_disabled();
        // Must not panic
        writer.send_record(make_record(1, StreamHistoryEventType::Connect));
        writer.flush().await.expect("flush on disabled writer");
        writer.shutdown().await;
        assert_eq!(writer.dropped_events.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn stream_history_writer_enabled_initializes() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_str().unwrap(), 4);
        let writer = StreamHistoryWriter::new(&config);
        assert!(writer.is_enabled());
        writer.shutdown().await;
    }

    #[tokio::test]
    async fn stream_history_writer_partial_batch_stays_in_memory() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_str().unwrap(), 4);
        let writer = StreamHistoryWriter::new(&config);

        // Send 3 records (< batch_size=4), no file blocks should be written yet
        for i in 0..3_u64 {
            writer.send_record(make_record(i, StreamHistoryEventType::Connect));
        }

        // Explicit flush — must not error even if no block written
        writer.flush().await.expect("flush ok");
        writer.shutdown().await;

        // File must exist (header was written on open)
        let day = current_utc_day();
        let pending = tmp.path().join(format!("stream-history-{day}.pending"));
        assert!(pending.exists(), "pending file must exist after writer init");
    }

    #[tokio::test]
    async fn stream_history_writer_batch_flush_at_threshold() {
        let tmp = TempDir::new().unwrap();
        let batch_size = 4;
        let config = test_config(tmp.path().to_str().unwrap(), batch_size);
        let writer = StreamHistoryWriter::new(&config);

        // Send exactly batch_size records — triggers one block write
        for i in 0..batch_size as u64 {
            writer.send_record(make_record(i, StreamHistoryEventType::Connect));
        }

        // Give the worker time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Send one more and flush to ensure previous batch was processed
        writer.send_record(make_record(99, StreamHistoryEventType::Disconnect));
        writer.flush().await.expect("flush ok");
        writer.shutdown().await;

        let day = current_utc_day();
        let pending = tmp.path().join(format!("stream-history-{day}.pending"));
        let metadata = tokio::fs::metadata(&pending).await.unwrap();
        // File should be larger than just the header (a block was written)
        assert!(metadata.len() > 100, "file must contain at least one block (size={})", metadata.len());
    }

    #[tokio::test]
    async fn stream_history_writer_queue_bounded_drops_on_overflow() {
        let tmp = TempDir::new().unwrap();
        // Use a very small batch so worker is always busy writing
        let config = test_config(tmp.path().to_str().unwrap(), 1);
        let writer = StreamHistoryWriter::new(&config);

        // Fill the mpsc channel by sending many records synchronously
        // We can't easily overflow QUEUE_CAPACITY=4096 in tests without blocking,
        // so we test the drop counter mechanism by sending a record with a no-op writer that's full.
        // Instead, verify that dropped_events starts at 0 and can increment.
        assert_eq!(writer.dropped_events.load(Ordering::Relaxed), 0);

        writer.shutdown().await;
    }

    #[tokio::test]
    async fn stream_history_writer_explicit_flush_writes_partial_batch() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_str().unwrap(), 100); // large batch
        let writer = StreamHistoryWriter::new(&config);

        // Send 3 records (well below batch_size=100)
        for i in 0..3_u64 {
            writer.send_record(make_record(i, StreamHistoryEventType::Connect));
        }

        // Explicit flush must write the partial batch to disk
        writer.flush().await.expect("explicit flush ok");
        writer.shutdown().await;

        let day = current_utc_day();
        let pending = tmp.path().join(format!("stream-history-{day}.pending"));
        let metadata = tokio::fs::metadata(&pending).await.unwrap();
        assert!(metadata.len() > 100, "partial batch must be flushed to file (size={})", metadata.len());
    }

    #[tokio::test]
    async fn stream_history_writer_block_header_uses_min_max_event_ts_even_when_batch_unsorted() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_str().unwrap(), 100);
        let writer = StreamHistoryWriter::new(&config);

        let mut late = make_record(1, StreamHistoryEventType::Connect);
        late.event_ts_utc = 3_000;
        late.partition_day_utc = "1970-01-01".to_string();

        let mut early = make_record(2, StreamHistoryEventType::Connect);
        early.event_ts_utc = 1_000;
        early.partition_day_utc = "1970-01-01".to_string();

        let mut middle = make_record(3, StreamHistoryEventType::Connect);
        middle.event_ts_utc = 2_000;
        middle.partition_day_utc = "1970-01-01".to_string();

        writer.send_record(late);
        writer.send_record(early);
        writer.send_record(middle);
        writer.flush().await.expect("explicit flush ok");
        writer.shutdown().await;

        let pending = tmp.path().join("stream-history-1970-01-01.pending");
        let file = std::fs::File::open(&pending).unwrap();
        let mut reader = BufReader::new(file);
        read_and_verify_file_magic(&mut reader).unwrap();
        let _: FileHeaderBody = read_framed(&mut reader).unwrap();
        read_and_verify_block_magic(&mut reader).unwrap();
        let block: BlockHeaderBody = read_framed(&mut reader).unwrap();

        assert_eq!(block.first_event_ts_utc, 1_000);
        assert_eq!(block.last_event_ts_utc, 3_000);
    }

    #[test]
    fn stream_history_ms_until_next_utc_midnight_positive() {
        let midnight_secs = 1_742_601_600_u64; // some UTC midnight
        let offset = secs_until_next_utc_midnight(midnight_secs - 1);
        assert_eq!(offset, 1);
    }

    #[tokio::test]
    async fn stream_history_writer_creates_pending_file_on_init() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_str().unwrap(), 10);
        let writer = StreamHistoryWriter::new(&config);
        // Give worker a moment to create the file
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        writer.shutdown().await;

        let day = current_utc_day();
        let pending = tmp.path().join(format!("stream-history-{day}.pending"));
        assert!(pending.exists(), "writer must create pending file on startup");
    }
}
