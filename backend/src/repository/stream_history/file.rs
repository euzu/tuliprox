use serde::{Deserialize, Serialize};
use shared::error::to_io_error;
use std::io::{self, Read, Write};
use shared::model::StreamInfo;
use crate::repository::{current_utc_day, now_utc_secs};

pub const FILE_MAGIC: [u8; 8] = *b"STRHIST\x01";
pub const BLOCK_MAGIC: [u8; 4] = *b"BLK\x01";
pub const CONTAINER_FORMAT_VERSION: u8 = 1;
pub const RECORD_SCHEMA_VERSION: u8 = 1;
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Connect,
    Disconnect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    ClientClosed,
    ServerError,
    Timeout,
    DayRollover,
    Shutdown,
    Unknown,
    ProviderError,
    ProviderClosed,
    Preempted,
    SessionExpired,
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

/// A single stream lifecycle event record (connect or disconnect).
///
/// Privacy: must never contain passwords, tokens, or credential-bearing URLs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamHistoryRecord {
    pub schema_version: u8,
    pub event_type: EventType,
    /// Unix timestamp seconds UTC of when this event occurred.
    pub event_ts_utc: u64,
    /// UTC calendar day of this event, e.g. `"2026-03-22"`.
    pub partition_day_utc: String,
    /// Stable correlation id shared between the connect and disconnect events of the same session.
    pub session_id: u64,
    pub source_addr: Option<String>,
    // User and provider identity (no passwords, no tokens)
    pub api_username: Option<String>,
    pub provider_name: Option<String>,
    pub provider_username: Option<String>,
    // Stream metadata
    pub virtual_id: Option<u32>,
    pub item_type: Option<String>,
    pub title: Option<String>,
    pub group: Option<String>,
    pub country: Option<String>,
    // Session summary — populated on disconnect
    pub connect_ts_utc: Option<u64>,
    pub disconnect_ts_utc: Option<u64>,
    pub session_duration: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub disconnect_reason: Option<DisconnectReason>,
}

impl StreamHistoryRecord {
    /// Build a common base from a `StreamInfo`, leaving event-specific fields to the caller.
    fn base(info: &StreamInfo) -> Self {
        Self {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type: EventType::Connect, // overridden by callers
            event_ts_utc: now_utc_secs(),
            partition_day_utc: current_utc_day(),
            session_id: u64::from(info.uid),
            source_addr: Some(info.client_ip.clone()),
            api_username: Some(info.username.clone()),
            provider_name: Some(info.provider.clone()),
            provider_username: None,
            virtual_id: Some(info.channel.virtual_id),
            item_type: Some(info.channel.item_type.to_string()),
            title: Some(info.channel.title.to_string()),
            group: Some(info.channel.group.to_string()),
            country: info.country.clone(),
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            disconnect_reason: None,
        }
    }

    pub fn from_connect(info: &StreamInfo) -> Self {
        let mut record = Self::base(info);
        record.event_type = EventType::Connect;
        record.connect_ts_utc = Some(info.ts);
        record
    }

    pub fn from_disconnect(info: &StreamInfo, reason: DisconnectReason) -> Self {
        let now_secs = now_utc_secs();
        let connect_secs = info.ts;
        let mut record = Self::base(info);
        record.event_type = EventType::Disconnect;
        record.connect_ts_utc = Some(connect_secs);
        record.disconnect_ts_utc = Some(now_secs);
        record.session_duration = Some(now_secs.saturating_sub(connect_secs));
        record.disconnect_reason = Some(reason);
        record
    }
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
    let expected_crc = u32::from_be_bytes(crc_buf);
    let actual_crc = crc32fast::hash(&payload);
    if actual_crc != expected_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CRC mismatch: expected {expected_crc:#010x}, got {actual_crc:#010x}"),
        ));
    }
    deserialize_named(&payload)
}

/// Write the file magic bytes at the beginning of a `.pending` file.
pub fn write_file_magic<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(&FILE_MAGIC)
}

/// Read and verify the file magic bytes.
pub fn read_and_verify_file_magic<R: Read>(reader: &mut R) -> io::Result<()> {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if magic != FILE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid file magic: {magic:02X?}"),
        ));
    }
    Ok(())
}

/// Write the block magic bytes before a block header.
pub fn write_block_magic<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(&BLOCK_MAGIC)
}

/// Read and verify the block magic bytes.
pub fn read_and_verify_block_magic<R: Read>(reader: &mut R) -> io::Result<()> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if magic != BLOCK_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid block magic: {magic:02X?}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    fn sample_connect_record() -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type: EventType::Connect,
            event_ts_utc: 1_742_600_001,
            partition_day_utc: "2026-03-22".to_string(),
            session_id: 999,
            source_addr: Some("192.0.2.1:12345".to_string()),
            api_username: Some("alice".to_string()),
            provider_name: Some("acme-tv".to_string()),
            provider_username: Some("acme_user".to_string()),
            virtual_id: Some(1234),
            item_type: Some("live".to_string()),
            title: Some("News Channel".to_string()),
            group: Some("News".to_string()),
            country: Some("DE".to_string()),
            connect_ts_utc: Some(1_742_600_001_000),
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            disconnect_reason: None,
        }
    }

    fn sample_disconnect_record(session_id: u64) -> StreamHistoryRecord {
        StreamHistoryRecord {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type: EventType::Disconnect,
            event_ts_utc: 1_742_603_601,
            partition_day_utc: "2026-03-22".to_string(),
            session_id,
            source_addr: Some("192.0.2.1:12345".to_string()),
            api_username: Some("alice".to_string()),
            provider_name: Some("acme-tv".to_string()),
            provider_username: Some("acme_user".to_string()),
            virtual_id: Some(1234),
            item_type: Some("live".to_string()),
            title: Some("News Channel".to_string()),
            group: Some("News".to_string()),
            country: Some("DE".to_string()),
            connect_ts_utc: Some(1_742_600_001),
            disconnect_ts_utc: Some(1_742_603_601),
            session_duration: Some(3600),
            bytes_sent: Some(1_234_567_890),
            disconnect_reason: Some(DisconnectReason::ClientClosed),
        }
    }

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
        assert_eq!(decoded.event_type, EventType::Connect);
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
        assert_eq!(connect.event_type, EventType::Connect);
        assert_eq!(disconnect.event_type, EventType::Disconnect);
        assert_eq!(disconnect.session_duration, Some(3600));
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
}
