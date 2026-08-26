use serde::{Deserialize, Serialize};
use shared::error::to_io_error;
use std::io::{self, Read, Write};

pub const FILE_MAGIC: [u8; 8] = *b"STRHIST\x01";
pub const BLOCK_MAGIC: [u8; 4] = *b"BLK\x01";
pub const CONTAINER_FORMAT_VERSION: u8 = 1;
pub const SOURCE_KIND_STREAM_HISTORY: &str = "stream_history";
pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024; // 8 MiB
/// Maximum allowed block payload size when reading. Prevents memory exhaustion on corrupt/malicious input during magic-recovery.
pub const MAX_BLOCK_PAYLOAD_SIZE: usize = 2 * MAX_FRAME_SIZE; // 16 MiB

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionKind {
    None,
    Lz4,
    Zstd,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordEncodingKind {
    MessagePackNamed,
}

/// Serialized as `MessagePack` named (map encoding) for schema evolution safety.
///
/// On-disk layout:
///   `[FILE_MAGIC: 8][header_len: u32 BE][header_bytes: N][header_crc: u32 BE]`
///
/// The CRC covers `header_bytes` only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHeaderBody {
    pub container_format_version: u8,
    pub record_schema_version: u8,
    pub source_kind: String,
    /// Unix timestamp seconds UTC when this file was created.
    pub created_at_ts_utc: u64,
    /// Logical partition day, e.g. `"2026-03-22"`.
    pub partition_day_ts_utc: String,
    pub writer_instance_id: u64,
    pub host_id: Option<String>,
    pub compression_kind: CompressionKind,
    /// False while the file is still being appended; true after finalization.
    pub finalized: bool,
    pub record_encoding_kind: RecordEncodingKind,
    // ── Summary fields written at finalization, None while active ──
    pub finalized_at_ts_utc: Option<u64>,
    pub total_block_count: Option<u64>,
    pub total_record_count: Option<u64>,
    pub min_event_ts_utc: Option<u64>,
    pub max_event_ts_utc: Option<u64>,
}

/// Serialized as `MessagePack` named (map encoding).
///
/// On-disk layout after `BLOCK_MAGIC`:
///   `[BLOCK_MAGIC: 4][hdr_len: u32 BE][hdr_bytes: N][hdr_crc: u32 BE][payload_bytes: payload_len]`
///
/// `payload_crc` covers the record payload bytes.
/// `hdr_crc` (external, not a struct field) covers `hdr_bytes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHeaderBody {
    pub block_version: u8,
    pub record_count: u32,
    pub payload_len: u32,
    pub first_event_ts_utc: u64,
    pub last_event_ts_utc: u64,
    /// CRC32 of the record payload bytes that follow this header.
    pub payload_crc: u32,
    pub flags: u8,
}

/// Serialize a value to `MessagePack` using **named (map) encoding** for schema evolution safety.
pub fn serialize_named<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    rmp_serde::to_vec_named(value).map_err(to_io_error)
}

/// Deserialize a value from `MessagePack` bytes.
pub fn deserialize_named<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    rmp_serde::from_slice(bytes).map_err(to_io_error)
}

/// Write a length-prefixed, CRC32-verified `MessagePack` frame.
///
/// Layout: `[payload_len: u32 BE][payload_bytes][crc32: u32 BE]`
pub fn write_framed<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let payload = serialize_named(value)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("payload too large: {} (max {MAX_FRAME_SIZE})", payload.len()),
        ));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "payload too large for framed write"))?;
    let crc = crc32fast::hash(&payload);
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.write_all(&crc.to_be_bytes())?;
    Ok(())
}

/// Read and CRC-verify a length-prefixed `MessagePack` frame, then deserialize.
pub fn read_framed<R: Read, T: for<'de> Deserialize<'de>>(reader: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid frame size: {len} (max {MAX_FRAME_SIZE})"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    let mut crc_buf = [0u8; 4];
    reader.read_exact(&mut crc_buf)?;
    verify_crc(crc_buf, &payload)?;
    deserialize_named(&payload)
}

fn verify_crc(crc_buf: [u8; 4], payload: &[u8]) -> io::Result<()> {
    let expected_crc = u32::from_be_bytes(crc_buf);
    let actual_crc = crc32fast::hash(payload);
    if actual_crc != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CRC mismatch: expected {expected_crc:#010x}, got {actual_crc:#010x}"),
        ));
    }
    Ok(())
}

/// Write the file magic bytes at the beginning of a `.pending` file.
pub fn write_file_magic<W: Write>(writer: &mut W) -> io::Result<()> { writer.write_all(&FILE_MAGIC) }

/// Write the block magic bytes before a block header.
pub fn write_block_magic<W: Write>(writer: &mut W) -> io::Result<()> { writer.write_all(&BLOCK_MAGIC) }

/// Shared body of the four `read_and_verify_*_magic` entry points below.
/// Sync and async are kept as separate macros because the `read_exact` trait
/// is in scope differently for `std::io::Read` and `tokio::io::AsyncRead`.
macro_rules! read_and_verify_magic_sync {
    ($reader:expr, $expected:expr, $kind:literal) => {{
        let mut magic = [0u8; $expected.len()];
        $reader.read_exact(&mut magic)?;
        if magic != $expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("invalid {} magic: {magic:02X?}", $kind)));
        }
        Ok(())
    }};
}

macro_rules! read_and_verify_magic_async {
    ($reader:expr, $expected:expr, $kind:literal) => {{
        let mut magic = [0u8; $expected.len()];
        $reader.read_exact(&mut magic).await?;
        if magic != $expected {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("invalid {} magic: {magic:02X?}", $kind)));
        }
        Ok(())
    }};
}

/// Read and verify the file magic bytes.
#[inline]
pub fn read_and_verify_file_magic<R: Read>(reader: &mut R) -> io::Result<()> {
    read_and_verify_magic_sync!(reader, FILE_MAGIC, "file")
}

/// Read and verify the block magic bytes.
#[inline]
pub fn read_and_verify_block_magic<R: Read>(reader: &mut R) -> io::Result<()> {
    read_and_verify_magic_sync!(reader, BLOCK_MAGIC, "block")
}

// ---------------------------------------------------------------------------
// Async variants for use with `tokio::io::AsyncRead`
// Used by `AsyncStreamHistoryPendingReader` in `reader.rs`
// ---------------------------------------------------------------------------

use tokio::io::{AsyncRead, AsyncReadExt};

/// Async version of `read_and_verify_file_magic` for `tokio::io::AsyncRead`.
#[inline]
pub async fn async_read_and_verify_file_magic<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<()> {
    read_and_verify_magic_async!(reader, FILE_MAGIC, "file")
}

/// Async version of `read_and_verify_block_magic` for `tokio::io::AsyncRead`.
#[inline]
pub async fn async_read_and_verify_block_magic<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<()> {
    read_and_verify_magic_async!(reader, BLOCK_MAGIC, "block")
}

/// Async version of `read_framed` for `tokio::io::AsyncRead`.
/// Reads: [len: u32 BE][payload][crc32: u32 BE], verifies CRC, deserializes.
pub async fn async_read_framed<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(reader: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid frame size: {len} (max {MAX_FRAME_SIZE})"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    let mut crc_buf = [0u8; 4];
    reader.read_exact(&mut crc_buf).await?;
    verify_crc(crc_buf, &payload)?;
    deserialize_named(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::model::{ConnectFailureReason, DisconnectReason, FailureStage, StreamHistoryEventType};
    use std::io::Cursor;
    use tuliprox_core::{
        model::{DisconnectQos, StreamHistoryRecord, RECORD_SCHEMA_VERSION},
        utils::encode_base64_hash,
    };

    fn sample_file_header() -> FileHeaderBody {
        FileHeaderBody {
            container_format_version: CONTAINER_FORMAT_VERSION,
            record_schema_version: RECORD_SCHEMA_VERSION,
            source_kind: SOURCE_KIND_STREAM_HISTORY.to_string(),
            created_at_ts_utc: 1_742_600_000,
            partition_day_ts_utc: "2026-03-22".to_string(),
            writer_instance_id: 42,
            host_id: Some("node-1".to_string()),
            compression_kind: CompressionKind::None,
            finalized: false,
            record_encoding_kind: RecordEncodingKind::MessagePackNamed,
            finalized_at_ts_utc: None,
            total_block_count: None,
            total_record_count: None,
            min_event_ts_utc: None,
            max_event_ts_utc: None,
        }
    }

    fn sample_block_header(record_count: u32, payload_len: u32, payload_crc: u32) -> BlockHeaderBody {
        BlockHeaderBody {
            block_version: 1,
            record_count,
            payload_len,
            first_event_ts_utc: 1_742_600_001,
            last_event_ts_utc: 1_742_600_002,
            payload_crc,
            flags: 0,
        }
    }

    use crate::stream_history::tests::{sample_connect_record, sample_disconnect_record, sample_stream_info};

    #[test]
    fn stream_history_header_round_trip() {
        let original = sample_file_header();
        let bytes = serialize_named(&original).expect("serialize");
        let decoded: FileHeaderBody = deserialize_named(&bytes).expect("deserialize");
        assert_eq!(decoded.container_format_version, CONTAINER_FORMAT_VERSION);
        assert_eq!(decoded.record_schema_version, RECORD_SCHEMA_VERSION);
        assert_eq!(decoded.source_kind, SOURCE_KIND_STREAM_HISTORY);
        assert_eq!(decoded.partition_day_ts_utc, "2026-03-22");
        assert!(!decoded.finalized);
        assert_eq!(decoded.compression_kind, CompressionKind::None);
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn stream_history_block_header_round_trip() {
        let payload = b"test_payload";
        let payload_crc = crc32fast::hash(payload);
        let original = sample_block_header(2, payload.len() as u32, payload_crc);
        let bytes = serialize_named(&original).expect("serialize");
        let decoded: BlockHeaderBody = deserialize_named(&bytes).expect("deserialize");
        assert_eq!(decoded.block_version, 1);
        assert_eq!(decoded.record_count, 2);
        assert_eq!(decoded.payload_len, payload.len() as u32);
        assert_eq!(decoded.payload_crc, payload_crc);
    }

    #[test]
    fn stream_history_record_round_trip() {
        let original = sample_connect_record();
        let bytes = serialize_named(&original).expect("serialize");
        let decoded: StreamHistoryRecord = deserialize_named(&bytes).expect("deserialize");
        assert_eq!(decoded.event_type, StreamHistoryEventType::Connect);
        assert_eq!(decoded.session_id, 999);
        assert_eq!(decoded.api_username.as_deref(), Some("alice"));
        assert!(decoded.disconnect_ts_utc.is_none());
    }

    #[test]
    fn stream_history_connect_disconnect_share_session_id() {
        let session_id = 12345_u64;
        let mut connect = sample_connect_record();
        connect.session_id = session_id;
        let disconnect = sample_disconnect_record(session_id);
        assert_eq!(connect.session_id, disconnect.session_id);
        assert_eq!(connect.event_type, StreamHistoryEventType::Connect);
        assert_eq!(disconnect.event_type, StreamHistoryEventType::Disconnect);
        assert_eq!(disconnect.session_duration, Some(3600));
    }

    #[test]
    fn stream_history_from_connect_uses_second_precision_session_times() {
        let info = sample_stream_info();
        let record = StreamHistoryRecord::from_connect(&info);

        assert_eq!(record.connect_ts_utc, Some(info.ts));
        assert_eq!(sample_connect_record().connect_ts_utc, Some(info.ts));
    }

    #[test]
    fn stream_history_from_connect_carries_previous_session_id() {
        let mut info = sample_stream_info();
        info.previous_session_id = Some(123_456);

        let record = StreamHistoryRecord::from_connect(&info);

        assert_eq!(record.previous_session_id, Some(123_456));
    }

    #[test]
    fn stream_history_from_connect_captures_low_cost_qos_identity_fields() {
        let info = sample_stream_info();

        let record = StreamHistoryRecord::from_connect(&info);

        assert_eq!(record.input_name.as_deref(), Some("provider-input"));
        assert_eq!(record.stream_url_hash.as_deref(), Some(encode_base64_hash("http://localhost/stream.ts").as_str()));
        assert_eq!(
            record.stream_identity_key.as_deref(),
            Some(encode_base64_hash("provider-input|1|1|1234|live").as_str())
        );
        assert_eq!(record.container.as_deref(), Some("mpegts"));
        assert_eq!(record.video_codec.as_deref(), Some("H.264"));
        assert_eq!(record.audio_codec.as_deref(), Some("AAC"));
        assert_eq!(record.audio_channels.as_deref(), Some("STEREO"));
        assert_eq!(record.resolution.as_deref(), Some("1920x1080"));
        assert_eq!(record.fps.as_deref(), Some("50"));
    }

    #[test]
    fn stream_history_from_connect_failed_captures_reason_and_identity() {
        let info = sample_stream_info();

        let record = StreamHistoryRecord::from_connect_failed(
            &info,
            ConnectFailureReason::ProviderConnectionsExhausted,
            77,
            FailureStage::Admission,
            None,
        );

        assert_eq!(record.event_type, StreamHistoryEventType::ConnectFailed);
        assert_eq!(record.connect_failure_reason, Some(ConnectFailureReason::ProviderConnectionsExhausted));
        assert_eq!(record.input_name.as_deref(), Some("provider-input"));
        assert_eq!(
            record.stream_identity_key.as_deref(),
            Some(encode_base64_hash("provider-input|1|1|1234|live").as_str())
        );
        assert!(record.connect_ts_utc.is_none());
        assert!(record.disconnect_ts_utc.is_none());
        assert!(record.session_duration.is_none());
    }

    #[test]
    fn stream_history_from_connect_failed_captures_failure_stage() {
        let info = sample_stream_info();

        let record = StreamHistoryRecord::from_connect_failed(
            &info,
            ConnectFailureReason::ProviderConnectionsExhausted,
            77,
            FailureStage::Admission,
            None,
        );

        assert_eq!(record.failure_stage, Some(FailureStage::Admission));
    }

    #[test]
    fn stream_history_from_connect_failed_can_store_provider_failure_metadata() {
        let info = sample_stream_info();

        let record = StreamHistoryRecord::from_connect_failed(
            &info,
            ConnectFailureReason::ChannelUnavailable,
            77,
            FailureStage::ProviderOpen,
            None,
        )
        .with_provider_failure(Some(503), Some("http_5xx"));

        assert_eq!(record.provider_http_status, Some(503));
        assert_eq!(record.provider_error_class.as_deref(), Some("http_5xx"));
    }

    #[test]
    fn stream_history_from_connect_captures_shared_stream_markers() {
        let mut info = sample_stream_info();
        info.channel.shared = true;
        info.channel.shared_joined_existing = Some(true);
        info.channel.shared_stream_id = Some(77);

        let record = StreamHistoryRecord::from_connect(&info);

        assert_eq!(record.shared, Some(true));
        assert_eq!(record.shared_joined_existing, Some(true));
        assert_eq!(record.shared_stream_id, Some(77));
    }

    #[test]
    fn stream_history_from_disconnect_can_store_failure_stage() {
        let info = sample_stream_info();

        let record = StreamHistoryRecord::from_disconnect(
            &info,
            DisconnectReason::ProviderError,
            &DisconnectQos::default(),
            Some(FailureStage::Streaming),
        );

        assert_eq!(record.failure_stage, Some(FailureStage::Streaming));
    }

    #[test]
    fn stream_history_framed_write_read_round_trip() {
        let header = sample_file_header();
        let mut buf = Vec::new();
        write_file_magic(&mut buf).expect("write magic");
        write_framed(&mut buf, &header).expect("write header");

        let mut cursor = Cursor::new(&buf);
        read_and_verify_file_magic(&mut cursor).expect("read magic");
        let decoded: FileHeaderBody = read_framed(&mut cursor).expect("read framed header");
        assert_eq!(decoded.partition_day_ts_utc, "2026-03-22");
        assert_eq!(decoded.writer_instance_id, 42);
    }

    #[test]
    fn stream_history_framed_crc_detects_corruption() {
        let header = sample_file_header();
        let mut buf = Vec::new();
        write_framed(&mut buf, &header).expect("write");
        // Corrupt a byte in the middle of the payload
        let len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
        buf[4 + len / 2] ^= 0xFF;
        let mut cursor = Cursor::new(&buf);
        let result: io::Result<FileHeaderBody> = read_framed(&mut cursor);
        assert!(result.is_err(), "CRC corruption must be detected");
    }

    #[test]
    fn stream_history_privacy_no_password_or_token_fields() {
        let record = sample_connect_record();
        let bytes = serialize_named(&record).expect("serialize");
        // Named msgpack encodes field names as strings in the payload
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("password"), "password field must not appear in serialized record");
        assert!(!text.contains("token"), "token field must not appear in serialized record");
    }

    #[test]
    fn stream_history_magic_mismatch_is_rejected() {
        let bad_magic = b"BADMAGIC";
        let mut cursor = Cursor::new(bad_magic.as_slice());
        let result = read_and_verify_file_magic(&mut cursor);
        assert!(result.is_err(), "invalid magic must be rejected");
    }

    #[test]
    fn stream_history_version_fields_are_serialized() {
        let header = sample_file_header();
        let bytes = serialize_named(&header).expect("serialize");
        let decoded: FileHeaderBody = deserialize_named(&bytes).expect("deserialize");
        assert_eq!(decoded.container_format_version, 1);
        assert_eq!(decoded.record_schema_version, 1);
    }

    #[test]
    fn read_and_verify_block_magic_accepts_valid_magic() {
        let mut buf = Vec::new();
        write_block_magic(&mut buf).expect("write block magic");
        let mut cursor = Cursor::new(buf.as_slice());
        read_and_verify_block_magic(&mut cursor).expect("valid block magic must be accepted");
    }

    #[test]
    fn read_and_verify_block_magic_rejects_invalid_magic() {
        let bad_magic: [u8; 4] = *b"NOPE";
        let mut cursor = Cursor::new(bad_magic.as_slice());
        let result = read_and_verify_block_magic(&mut cursor);
        assert!(result.is_err(), "invalid block magic must be rejected");
    }

    #[test]
    fn read_and_verify_file_magic_accepts_valid_magic() {
        let mut buf = Vec::new();
        write_file_magic(&mut buf).expect("write file magic");
        let mut cursor = Cursor::new(buf.as_slice());
        read_and_verify_file_magic(&mut cursor).expect("valid file magic must be accepted");
    }

    #[test]
    fn read_and_verify_file_magic_rejects_truncated_input() {
        let truncated: [u8; 4] = *b"STRH";
        let mut cursor = Cursor::new(truncated.as_slice());
        let result = read_and_verify_file_magic(&mut cursor);
        assert!(result.is_err(), "truncated input must surface an io error");
    }

    #[tokio::test]
    async fn async_read_and_verify_file_magic_accepts_valid_magic() {
        use tokio::io::BufReader;
        let mut buf = Vec::new();
        write_file_magic(&mut buf).expect("write file magic");
        let mut reader = BufReader::new(buf.as_slice());
        async_read_and_verify_file_magic(&mut reader)
            .await
            .expect("valid file magic must be accepted via async reader");
    }

    #[tokio::test]
    async fn async_read_and_verify_file_magic_rejects_invalid_magic() {
        use tokio::io::BufReader;
        let bad: [u8; 8] = *b"WRONGMAG";
        let mut reader = BufReader::new(bad.as_slice());
        let result = async_read_and_verify_file_magic(&mut reader).await;
        assert!(result.is_err(), "invalid file magic must be rejected via async reader");
    }

    #[tokio::test]
    async fn async_read_and_verify_block_magic_rejects_invalid_magic() {
        use tokio::io::BufReader;
        let bad: [u8; 4] = *b"NOPE";
        let mut reader = BufReader::new(bad.as_slice());
        let result = async_read_and_verify_block_magic(&mut reader).await;
        assert!(result.is_err(), "invalid block magic must be rejected via async reader");
    }
}
