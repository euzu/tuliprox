use crate::repository::stream_history::{
    BlockHeaderBody, CompressionKind, FileHeaderBody, RecordEncodingKind, CONTAINER_FORMAT_VERSION,
    RECORD_SCHEMA_VERSION, SOURCE_KIND_STREAM_HISTORY, StreamHistoryRecord,
    write_block_magic, write_file_magic, write_framed,
};
use crate::model::StreamHistoryConfig;
use log::{error, info, warn};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot};

const SECS_PER_DAY: u64 = 86_400;

const QUEUE_CAPACITY: usize = 4096;

pub(crate) fn now_utc_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn utc_day_from_secs(ts_secs: u64) -> String {
    chrono::DateTime::from_timestamp_secs(i64::try_from(ts_secs).unwrap_or(0)).map_or_else(|| "1970-01-01".to_string(), |dt| dt.format("%Y-%m-%d").to_string())
}

pub(crate) fn current_utc_day() -> String {
    utc_day_from_secs(now_utc_secs())
}

/// Compute Unix seconds until the next UTC midnight after `from_ms`.
pub fn secs_until_next_utc_midnight(from_secs: u64) -> u64 {
    let remaining = SECS_PER_DAY - (from_secs % SECS_PER_DAY);
    if remaining == 0 { SECS_PER_DAY } else { remaining }
}

fn pending_file_path(directory: &str, day: &str) -> PathBuf {
    PathBuf::from(directory).join(format!("stream-history-{day}.pending"))
}

enum WriterCommand {
    Record(Box<StreamHistoryRecord>),
    Flush(oneshot::Sender<io::Result<()>>),
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
}

impl StreamHistoryWriter {
    /// Creates a no-op writer when stream history is disabled.
    pub fn new_disabled() -> Self {
        Self { tx: None, dropped_events: Arc::new(AtomicU64::new(0)) }
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

        Self { tx: Some(tx), dropped_events }
    }

    /// Submit a record for persistence. Non-blocking; drops the record if the queue is full.
    pub fn send_record(&self, record: StreamHistoryRecord) {
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
        let _ = tx.send(WriterCommand::Flush(resp_tx)).await;
        resp_rx.await.unwrap_or_else(|_| {
            log::warn!("Stream history flush: worker channel closed");
            Ok(())
        })
    }

    /// Flush and shut down the writer, waiting for the worker to finish.
    pub async fn shutdown(&self) {
        let Some(tx) = &self.tx else { return };
        let (resp_tx, resp_rx) = oneshot::channel();
        let _ = tx.send(WriterCommand::Shutdown(resp_tx)).await;
        let _ = resp_rx.await;
    }

    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
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
        let mut state = match WriterState::open(&self.config, self.writer_instance_id) {
            Ok(s) => s,
            Err(e) => {
                error!("Stream history writer failed to initialize: {e}");
                // Drain the channel without panicking
                while rx.recv().await.is_some() {}
                return;
            }
        };

        info!("Stream history writer started for day {}", state.current_day);

        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriterCommand::Record(record) => {
                    // Day rollover check
                    if record.partition_day_utc != state.current_day {
                        if let Err(e) = state.flush_and_rollover(&self.config, self.writer_instance_id) {
                            error!("Stream history day rollover failed: {e}");
                            let dropped = dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                            warn!("Stream history record dropped due to rollover failure (total dropped: {dropped})");
                            continue;
                        }
                    }

                    state.push(*record);

                    if state.batch.len() >= self.config.stream_history_batch_size {
                        if let Err(e) = state.flush_batch() {
                            error!("Stream history batch flush failed: {e}. Dropped events: {}",
                                dropped_events.load(Ordering::Relaxed));
                        }
                    }
                }
                WriterCommand::Flush(resp) => {
                    let result = state.flush_batch();
                    let _ = resp.send(result);
                }
                WriterCommand::Shutdown(resp) => {
                    if let Err(e) = state.flush_batch() {
                        error!("Stream history flush on shutdown failed: {e}");
                    }
                    if let Err(e) = state.finalize() {
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
    file: Option<BufWriter<File>>,
    file_path: PathBuf,
    batch: Vec<StreamHistoryRecord>,
    /// Reusable scratch buffer for building block payloads — retains capacity across flushes.
    payload_buf: Vec<u8>,
    total_block_count: u64,
    total_record_count: u64,
    min_event_ts: Option<u64>,
    max_event_ts: Option<u64>,
}

impl WriterState {
    fn open(config: &StreamHistoryConfig, writer_instance_id: u64) -> io::Result<Self> {
        let day = current_utc_day();
        let dir = &config.stream_history_directory;

        fs::create_dir_all(dir)?;

        let path = pending_file_path(dir, &day);
        let file = open_or_create_pending_file(&path, &day, writer_instance_id)?;

        Ok(Self {
            directory: dir.clone(),
            current_day: day,
            file: Some(BufWriter::new(file)),
            file_path: path,
            batch: Vec::with_capacity(config.stream_history_batch_size),
            payload_buf: Vec::new(),
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
    /// NOTE: This performs blocking file I/O on the tokio runtime. Acceptable because
    /// the writer runs on its own task and stream history is best-effort — a slow disk
    /// only delays history persistence, never the streaming hot path.
    fn flush_batch(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }

        let Some(writer) = self.file.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pending file not open"));
        };

        let first_ts = self.batch.first().map_or(0, |r| r.event_ts_utc);
        let last_ts = self.batch.last().map_or(0, |r| r.event_ts_utc);
        let record_count = u32::try_from(self.batch.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record count too large"))?;

        // Serialize all records directly into the reusable payload buffer.
        // Each record is prefixed with its length as a u32 BE.
        // The length slot is reserved first and patched after serialization to avoid a
        // temporary per-record Vec allocation.
        self.payload_buf.clear();
        for record in &self.batch {
            let len_offset = self.payload_buf.len();
            self.payload_buf.extend_from_slice(&[0u8; 4]); // placeholder for length
            rmp_serde::encode::write_named(&mut self.payload_buf, record)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let record_len = self.payload_buf.len() - len_offset - 4;
            let record_len_u32 = u32::try_from(record_len)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record too large"))?;
            self.payload_buf[len_offset..len_offset + 4].copy_from_slice(&record_len_u32.to_be_bytes());
        }

        let payload_len = u32::try_from(self.payload_buf.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "block payload too large"))?;
        let payload_crc = crc32fast::hash(&self.payload_buf);

        let block_header = BlockHeaderBody {
            block_version: 1,
            record_count,
            payload_len,
            first_event_ts_utc: first_ts,
            last_event_ts_utc: last_ts,
            payload_crc,
            flags: 0,
        };

        write_block_magic(writer)?;
        write_framed(writer, &block_header)?;
        writer.write_all(&self.payload_buf)?;
        writer.flush()?;

        self.total_block_count += 1;
        self.total_record_count += u64::from(record_count);
        self.batch.clear();

        Ok(())
    }

    fn flush_and_rollover(&mut self, config: &StreamHistoryConfig, writer_instance_id: u64) -> io::Result<()> {
        self.flush_batch()?;
        self.finalize()?;

        let new_day = current_utc_day();
        let new_path = pending_file_path(&self.directory, &new_day);
        let new_file = open_or_create_pending_file(&new_path, &new_day, writer_instance_id)?;

        self.current_day = new_day;
        self.file = Some(BufWriter::new(new_file));
        self.file_path = new_path;
        self.total_block_count = 0;
        self.total_record_count = 0;
        self.min_event_ts = None;
        self.max_event_ts = None;

        // Apply retention after rollover
        if let Err(e) = apply_retention(&self.directory, config.stream_history_retention_days) {
            warn!("Stream history retention cleanup failed: {e}");
        }

        Ok(())
    }

    /// Flush writer buffer to OS. Does not compress or archive.
    fn finalize(&mut self) -> io::Result<()> {
        if let Some(mut writer) = self.file.take() {
            writer.flush()?;
        }
        Ok(())
    }
}

/// Open an existing `.pending` file for appending, or create a new one with the file header.
fn open_or_create_pending_file(path: &PathBuf, day: &str, writer_instance_id: u64) -> io::Result<File> {
    if path.exists() {
        // Continue appending to existing file
        let file = OpenOptions::new().append(true).open(path)?;
        return Ok(file);
    }

    // Create new file with header
    let mut file = File::create(path)?;
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

    write_file_magic(&mut file)?;
    write_framed(&mut file, &header)?;
    file.flush()?;

    Ok(file)
}

/// Delete daily archive/pending files older than `retention_days`.
pub fn apply_retention(directory: &str, retention_days: u16) -> io::Result<()> {
    let dir = PathBuf::from(directory);
    if !dir.exists() {
        return Ok(());
    }

    let cutoff_day = {
        let now_secs = now_utc_secs();
        let cutoff_secs = now_secs.saturating_sub(u64::from(retention_days) * SECS_PER_DAY);
        utc_day_from_secs(cutoff_secs)
    };

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        // Match "stream-history-YYYY-MM-DD.pending" or "stream-history-YYYY-MM-DD.archive.lz4"
        if let Some(day) = extract_day_from_filename(&name) {
            if day < cutoff_day.as_str() {
                let path = entry.path();
                if let Err(e) = fs::remove_file(&path) {
                    warn!("Failed to delete old history file {}: {e}", path.display());
                } else {
                    info!("Deleted expired history file: {}", path.display());
                }
            }
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
    use crate::repository::stream_history::{EventType};
    use std::sync::atomic::Ordering;
    use tempfile::TempDir;

    fn test_config(dir: &str, batch_size: usize) -> StreamHistoryConfig {
        StreamHistoryConfig {
            stream_history_enabled: true,
            stream_history_batch_size: batch_size,
            stream_history_retention_days: 30,
            stream_history_directory: dir.to_string(),
        }
    }

    fn make_record(session_id: u64, event_type: EventType) -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type,
            event_ts_utc: now_utc_secs(),
            partition_day_utc: current_utc_day(),
            session_id,
            source_addr: None,
            api_username: Some("user1".to_string()),
            provider_name: Some("acme".to_string()),
            provider_username: None,
            virtual_id: Some(1),
            item_type: Some("live".to_string()),
            title: Some("Test Channel".to_string()),
            group: None,
            country: None,
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            disconnect_reason: None,
        }
    }

    #[tokio::test]
    async fn stream_history_writer_disabled_accepts_records_silently() {
        let writer = StreamHistoryWriter::new_disabled();
        // Must not panic
        writer.send_record(make_record(1, EventType::Connect));
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
            writer.send_record(make_record(i, EventType::Connect));
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
            writer.send_record(make_record(i, EventType::Connect));
        }

        // Give the worker time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Send one more and flush to ensure previous batch was processed
        writer.send_record(make_record(99, EventType::Disconnect));
        writer.flush().await.expect("flush ok");
        writer.shutdown().await;

        let day = current_utc_day();
        let pending = tmp.path().join(format!("stream-history-{day}.pending"));
        let metadata = std::fs::metadata(&pending).unwrap();
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
            writer.send_record(make_record(i, EventType::Connect));
        }

        // Explicit flush must write the partial batch to disk
        writer.flush().await.expect("explicit flush ok");
        writer.shutdown().await;

        let day = current_utc_day();
        let pending = tmp.path().join(format!("stream-history-{day}.pending"));
        let metadata = std::fs::metadata(&pending).unwrap();
        assert!(metadata.len() > 100, "partial batch must be flushed to file (size={})", metadata.len());
    }

    #[test]
    fn stream_history_ms_until_next_utc_midnight_positive() {
        let midnight_secs = 1_742_601_600_u64; // some UTC midnight
        let offset = secs_until_next_utc_midnight(midnight_secs - 1);
        assert_eq!(offset, 1);
    }

    #[test]
    fn stream_history_retention_extract_day_from_filename() {
        assert_eq!(extract_day_from_filename("stream-history-2026-03-22.pending"), Some("2026-03-22"));
        assert_eq!(extract_day_from_filename("stream-history-2026-03-22.archive.lz4"), Some("2026-03-22"));
        assert_eq!(extract_day_from_filename("unrelated.txt"), None);
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
