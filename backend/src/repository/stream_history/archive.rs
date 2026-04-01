use crate::repository::stream_history::{
    BlockHeaderBody, CompressionKind, FileHeaderBody, RecordEncodingKind, BLOCK_MAGIC, CONTAINER_FORMAT_VERSION,
    RECORD_SCHEMA_VERSION, SOURCE_KIND_STREAM_HISTORY, read_and_verify_file_magic, read_framed,
    write_file_magic, write_framed,
};
use crate::repository::stream_history::writer::{apply_retention, current_utc_day, now_utc_secs};
use lz4_flex::frame::FrameEncoder;
use log::{info, warn};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Seek},
    path::{Path, PathBuf},
};

struct PendingSummary {
    header: FileHeaderBody,
    total_blocks: u64,
    total_records: u64,
    min_event_ts: Option<u64>,
    max_event_ts: Option<u64>,
    /// Byte offset where block data starts (after file magic + header frame).
    blocks_start: u64,
}

/// Scan a `.pending` file to collect summary metadata without loading blocks into memory.
fn scan_pending_file(path: &Path) -> io::Result<PendingSummary> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    read_and_verify_file_magic(&mut reader)?;
    let header: FileHeaderBody = read_framed(&mut reader)?;

    // Reader position after magic + framed header is exactly blocks_start — no re-serialization needed.
    let blocks_start = reader.stream_position()?;

    let mut total_blocks = 0u64;
    let mut total_records = 0u64;
    let mut min_ts: Option<u64> = None;
    let mut max_ts: Option<u64> = None;

    loop {
        let mut magic_buf = [0u8; 4];
        match reader.read_exact(&mut magic_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        if magic_buf != BLOCK_MAGIC {
            // Partial or corrupted trailing block — stop scanning gracefully
            break;
        }

        let block_header: BlockHeaderBody = match read_framed(&mut reader) {
            Ok(h) => h,
            Err(_) => break, // trailing partial block
        };

        // Skip payload bytes without allocating — BufReader<File> implements Seek.
        if reader.seek(io::SeekFrom::Current(i64::from(block_header.payload_len))).is_err() {
            break; // truncated payload
        }

        total_blocks += 1;
        total_records += u64::from(block_header.record_count);

        if min_ts.is_none_or(|m| block_header.first_event_ts_utc < m) {
            min_ts = Some(block_header.first_event_ts_utc);
        }
        if max_ts.is_none_or(|m| block_header.last_event_ts_utc > m) {
            max_ts = Some(block_header.last_event_ts_utc);
        }
    }

    Ok(PendingSummary { header, total_blocks, total_records, min_event_ts: min_ts, max_event_ts: max_ts, blocks_start })
}

/// Finalize a `.pending` file into a `.archive.lz4` file, then delete the pending.
///
/// The archive is self-describing: it contains a new file header with `finalized=true`
/// and all summary statistics, followed by the original block data, all lz4-compressed.
pub fn archive_pending_file(path: &Path) -> io::Result<PathBuf> {
    let summary = scan_pending_file(path)?;

    let finalized_header = FileHeaderBody {
        container_format_version: CONTAINER_FORMAT_VERSION,
        record_schema_version: RECORD_SCHEMA_VERSION,
        source_kind: SOURCE_KIND_STREAM_HISTORY.to_string(),
        created_at_ts_utc: summary.header.created_at_ts_utc,
        partition_day_ts_utc: summary.header.partition_day_ts_utc.clone(),
        writer_instance_id: summary.header.writer_instance_id,
        host_id: summary.header.host_id.clone(),
        compression_kind: CompressionKind::Lz4,
        finalized: true,
        record_encoding_kind: RecordEncodingKind::MessagePackNamed,
        finalized_at_ts_utc: Some(now_utc_secs()),
        total_block_count: Some(summary.total_blocks),
        total_record_count: Some(summary.total_records),
        min_event_ts_utc: summary.min_event_ts,
        max_event_ts_utc: summary.max_event_ts,
    };

    // Use the partition day from the file header (authoritative) — not the filename,
    // which is user-manipulable and must not be trusted.
    let archive_path = archive_path_for(path, &summary.header.partition_day_ts_utc);
    let archive_file = OpenOptions::new().write(true).create_new(true).open(&archive_path)?;
    let mut lz4 = FrameEncoder::new(archive_file);

    // Write the finalized file header
    write_file_magic(&mut lz4)?;
    write_framed(&mut lz4, &finalized_header)?;

    // Stream the block data from the pending file
    let mut pending = File::open(path)?;
    pending.seek(std::io::SeekFrom::Start(summary.blocks_start))?;
    io::copy(&mut pending, &mut lz4)?;

    lz4.finish().map_err(io::Error::other)?;
    drop(pending);

    fs::remove_file(path)?;

    info!(
        "Archived stream history for {}: {} blocks, {} records → {}",
        summary.header.partition_day_ts_utc,
        summary.total_blocks,
        summary.total_records,
        archive_path.display()
    );

    Ok(archive_path)
}

fn archive_path_for(pending_path: &Path, partition_day: &str) -> PathBuf {
    let name = format!("stream-history-{partition_day}.archive.lz4");
    pending_path.parent().unwrap_or(Path::new(".")).join(name)
}

/// Read the `partition_day_ts_utc` field from a pending file's header.
///
/// The header is the authoritative source for the partition day — the filename is
/// user-manipulable and must not be trusted.
fn read_pending_partition_day(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    read_and_verify_file_magic(&mut reader)?;
    let header: FileHeaderBody = read_framed(&mut reader)?;
    Ok(header.partition_day_ts_utc)
}

/// Scan `directory` on startup and archive any leftover `.pending` files from previous days.
///
/// A `.pending` file for the current UTC day is left untouched for the writer to continue.
/// The partition day is read from the file header (authoritative), not from the filename.
pub fn recover_pending_files(directory: &str) -> io::Result<()> {
    let dir = PathBuf::from(directory);
    if !dir.exists() {
        return Ok(());
    }

    let today = current_utc_day();

    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();

        if !name.ends_with(".pending") {
            continue;
        }

        match read_pending_partition_day(&path) {
            Ok(day) if day.as_str() < today.as_str() => {
                info!("Recovering leftover pending file from {day}: {}", path.display());
                if let Err(e) = archive_pending_file(&path) {
                    warn!("Failed to archive leftover pending file {}: {e}", path.display());
                }
            }
            Ok(_) => {
                // Today's pending file is left for the writer to continue appending.
            }
            Err(e) => {
                warn!("Skipping unreadable pending file {}: {e}", path.display());
            }
        }
    }

    Ok(())
}

/// Called by the writer on day rollover or shutdown: flush, archive, apply retention.
pub fn finalize_and_archive(pending_path: &Path, directory: &str, retention_days: u16) {
    if !pending_path.exists() {
        return;
    }
    match archive_pending_file(pending_path) {
        Ok(p) => info!("Finalized archive: {}", p.display()),
        Err(e) => warn!("Failed to archive pending file {}: {e}", pending_path.display()),
    }
    if let Err(e) = apply_retention(directory, retention_days) {
        warn!("Retention cleanup failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use crate::repository::stream_history::{
        CompressionKind, EventType, RECORD_SCHEMA_VERSION, StreamHistoryRecord, read_and_verify_file_magic,
        read_framed, serialize_named, write_block_magic,
    };
    use crate::repository::stream_history::writer::{StreamHistoryWriter, now_utc_secs, current_utc_day};
    use crate::model::StreamHistoryConfig;
    use lz4_flex::frame::FrameDecoder;
    use tempfile::TempDir;

    fn test_config(dir: &str, batch_size: usize) -> StreamHistoryConfig {
        StreamHistoryConfig {
            stream_history_enabled: true,
            stream_history_batch_size: batch_size,
            stream_history_retention_days: 30,
            stream_history_directory: dir.to_string(),
        }
    }

    fn make_record(session_id: u64) -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type: EventType::Connect,
            event_ts_utc: now_utc_secs(),
            partition_day_utc: current_utc_day(),
            session_id,
            source_addr: None,
            api_username: Some("alice".to_string()),
            provider_name: Some("acme".to_string()),
            provider_username: None,
            virtual_id: Some(1),
            item_type: Some("live".to_string()),
            title: Some("Test".to_string()),
            group: None,
            country: None,
            user_agent: None,
            shared: None,
            provider_id: None,
            cluster: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            resolution: None,
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            first_byte_latency_ms: None,
            provider_reconnect_count: None,
            disconnect_reason: None,
            previous_session_id: None,
            target_id: None,
        }
    }

    /// Write a minimal `.pending` file and return its path.
    fn write_test_pending(dir: &Path, day: &str, records: &[StreamHistoryRecord]) -> PathBuf {
        use crate::repository::stream_history::{
            BlockHeaderBody, CONTAINER_FORMAT_VERSION, RECORD_SCHEMA_VERSION, CompressionKind,
            RecordEncodingKind, SOURCE_KIND_STREAM_HISTORY,
        };

        let path = dir.join(format!("stream-history-{day}.pending"));
        let mut file = File::create(&path).unwrap();

        let header = FileHeaderBody {
            container_format_version: CONTAINER_FORMAT_VERSION,
            record_schema_version: RECORD_SCHEMA_VERSION,
            source_kind: SOURCE_KIND_STREAM_HISTORY.to_string(),
            created_at_ts_utc: now_utc_secs(),
            partition_day_ts_utc: day.to_string(),
            writer_instance_id: 1,
            host_id: None,
            compression_kind: CompressionKind::None,
            finalized: false,
            record_encoding_kind: RecordEncodingKind::MessagePackNamed,
            finalized_at_ts_utc: None,
            total_block_count: None,
            total_record_count: None,
            min_event_ts_utc: None,
            max_event_ts_utc: None,
        };

        write_file_magic(&mut file).unwrap();
        write_framed(&mut file, &header).unwrap();

        if !records.is_empty() {
            let mut payload = Vec::new();
            for r in records {
                let bytes = serialize_named(r).unwrap();
                let len = bytes.len() as u32;
                payload.extend_from_slice(&len.to_be_bytes());
                payload.extend_from_slice(&bytes);
            }
            let payload_crc = crc32fast::hash(&payload);
            let block = BlockHeaderBody {
                block_version: 1,
                record_count: records.len() as u32,
                payload_len: payload.len() as u32,
                first_event_ts_utc: records[0].event_ts_utc,
                last_event_ts_utc: records.last().unwrap().event_ts_utc,
                payload_crc,
                flags: 0,
            };
            write_block_magic(&mut file).unwrap();
            write_framed(&mut file, &block).unwrap();
            file.write_all(&payload).unwrap();
        }

        path
    }

    #[test]
    fn stream_history_archive_pending_creates_archive_and_removes_pending() {
        let tmp = TempDir::new().unwrap();
        let day = "2026-03-21";
        let records: Vec<_> = (0..3).map(make_record).collect();
        let path = write_test_pending(tmp.path(), day, &records);

        let archive_path = archive_pending_file(&path).unwrap();

        assert!(!path.exists(), "pending file must be removed after archiving");
        assert!(archive_path.exists(), "archive file must exist");
        assert!(archive_path.to_string_lossy().ends_with(".archive.lz4"), "archive must have .archive.lz4 extension");
    }

    #[test]
    fn stream_history_archive_is_self_describing_with_finalized_header() {
        let tmp = TempDir::new().unwrap();
        let day = "2026-03-21";
        let records: Vec<_> = (0..5).map(make_record).collect();
        let path = write_test_pending(tmp.path(), day, &records);

        let archive_path = archive_pending_file(&path).unwrap();

        // Decompress and read the header
        let lz4_file = File::open(&archive_path).unwrap();
        let mut lz4 = FrameDecoder::new(lz4_file);
        read_and_verify_file_magic(&mut lz4).expect("file magic must be valid in archive");
        let header: FileHeaderBody = read_framed(&mut lz4).expect("header must decode");

        assert!(header.finalized, "archived header must have finalized=true");
        assert_eq!(header.partition_day_ts_utc, day);
        assert_eq!(header.total_block_count, Some(1));
        assert_eq!(header.total_record_count, Some(5));
        assert_eq!(header.compression_kind, CompressionKind::Lz4);
        assert!(header.finalized_at_ts_utc.is_some());
    }

    #[test]
    fn stream_history_archive_empty_pending_produces_valid_archive() {
        let tmp = TempDir::new().unwrap();
        let day = "2026-03-20";
        let path = write_test_pending(tmp.path(), day, &[]);

        let archive_path = archive_pending_file(&path).unwrap();
        assert!(archive_path.exists());

        let lz4_file = File::open(&archive_path).unwrap();
        let mut lz4 = FrameDecoder::new(lz4_file);
        read_and_verify_file_magic(&mut lz4).expect("magic ok");
        let header: FileHeaderBody = read_framed(&mut lz4).expect("header ok");
        assert!(header.finalized);
        assert_eq!(header.total_block_count, Some(0));
        assert_eq!(header.total_record_count, Some(0));
    }

    #[test]
    fn stream_history_archive_recovery_archives_old_pending_leaves_today() {
        let tmp = TempDir::new().unwrap();
        let old_day = "2026-01-01";
        let today = current_utc_day();

        let old_records: Vec<_> = (0..2).map(make_record).collect();
        let old_path = write_test_pending(tmp.path(), old_day, &old_records);
        let today_path = write_test_pending(tmp.path(), &today, &[]);

        recover_pending_files(tmp.path().to_str().unwrap()).unwrap();

        // Old pending must be archived and removed
        assert!(!old_path.exists(), "old pending must be removed by recovery");
        let old_archive = tmp.path().join(format!("stream-history-{old_day}.archive.lz4"));
        assert!(old_archive.exists(), "old pending must become an archive");

        // Today's pending must be untouched
        assert!(today_path.exists(), "today's pending must remain for writer to continue");
    }

    #[test]
    fn stream_history_archive_recovery_nonexistent_dir_is_noop() {
        let result = recover_pending_files("/tmp/nonexistent-tuliprox-history-xyz");
        assert!(result.is_ok(), "recovery on missing dir must not error");
    }

    #[tokio::test]
    async fn stream_history_archive_writer_shutdown_creates_pending() {
        // Integration test: writer creates a pending file on startup
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_str().unwrap(), 4);
        let writer = StreamHistoryWriter::new(&config);

        // Send 2 records (partial batch) — should be flushed on shutdown
        writer.send_record(make_record(1));
        writer.send_record(make_record(2));
        writer.shutdown().await;

        let day = current_utc_day();
        let pending = tmp.path().join(format!("stream-history-{day}.pending"));
        // Pending should exist (we flushed but didn't archive in the writer directly)
        assert!(pending.exists(), "pending file must exist after shutdown flush");

        // Now archive it manually
        let archive = archive_pending_file(&pending).unwrap();
        assert!(archive.exists(), "archive must be created from shutdown pending");
        assert!(!pending.exists(), "pending removed after archive");
    }
}
