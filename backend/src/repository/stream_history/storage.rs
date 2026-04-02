use serde::{Deserialize, Serialize};
use shared::error::to_io_error;
use std::io::{self, Read, Write};
use shared::model::StreamInfo;
use crate::repository::{now_utc_secs, utc_day_from_secs};
use crate::utils::encode_base64_hash;

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
    ConnectFailed,
    Disconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectFailureReason {
    UserAccountExpired,
    UserConnectionsExhausted,
    ProviderConnectionsExhausted,
    ProviderError,
    ProviderClosed,
    ChannelUnavailable,
    Preempted,
    SessionExpired,
    Provisioning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureStage {
    Admission,
    ProviderOpen,
    FirstByte,
    Streaming,
    SessionReconnect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    ClientClosed,
    ServerError,
    Timeout,
    /// Reserved for future day-split logic: emitted at midnight for sessions that span
    /// two calendar days so the previous day's file gets a closing record.
    /// Not yet emitted — sessions crossing midnight have their disconnect in the new day's file.
    DayRollover,
    Shutdown,
    Unknown,
    ProviderError,
    ProviderClosed,
    Preempted,
    SessionExpired,
    UserConnectionsExhausted,
    ProviderConnectionsExhausted,
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
    /// Provider-side username (e.g. Xtream account). Always `None` until `StreamInfo`
    /// carries the provider credential — at which point populate from there.
    pub provider_username: Option<String>,
    pub input_name: Option<String>,
    // Stream metadata
    pub virtual_id: Option<u32>,
    pub item_type: Option<String>,
    pub title: Option<String>,
    pub group: Option<String>,
    pub country: Option<String>,
    // QoS metadata
    pub user_agent: Option<String>,
    pub shared: Option<bool>,
    pub shared_joined_existing: Option<bool>,
    pub shared_stream_id: Option<u64>,
    pub provider_id: Option<u32>,
    pub cluster: Option<String>,
    pub container: Option<String>,
    pub stream_url_hash: Option<String>,
    pub stream_identity_key: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<String>,
    pub resolution: Option<String>,
    pub fps: Option<String>,
    // Session summary — populated on disconnect.
    // bytes_sent and first_byte_latency_ms are None for shared streams (meter serves
    // multiple clients; per-session totals are not meaningful in that case).
    pub connect_ts_utc: Option<u64>,
    pub disconnect_ts_utc: Option<u64>,
    pub session_duration: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub first_byte_latency_ms: Option<u64>,
    pub provider_reconnect_count: Option<u8>,
    pub failure_stage: Option<FailureStage>,
    pub provider_http_status: Option<u16>,
    pub provider_error_class: Option<String>,
    pub connect_failure_reason: Option<ConnectFailureReason>,
    pub disconnect_reason: Option<DisconnectReason>,
    /// Set when this connect event continues a session that was split by a `DayRollover`.
    /// Always `None` until the `DayRollover` mechanism is implemented.
    pub previous_session_id: Option<u64>,
    /// Target config id — identifies the failover bundle this stream belongs to.
    pub target_id: Option<u16>,
}

/// `QoS` metrics collected at disconnect time, passed as a bundle to avoid growing the signature.
#[derive(Debug, Default)]
pub struct DisconnectQos {
    pub bytes_sent: Option<u64>,
    pub first_byte_latency_ms: Option<u64>,
    pub provider_reconnect_count: Option<u8>,
}

impl StreamHistoryRecord {
    fn build_stream_identity_key(info: &StreamInfo) -> Option<String> {
        if info.channel.input_name.is_empty() {
            return None;
        }
        let mut raw = String::with_capacity(info.channel.input_name.len() + 32);
        raw.push_str(info.channel.input_name.as_ref());
        raw.push('|');
        raw.push_str(&info.channel.target_id.to_string());
        raw.push('|');
        raw.push_str(&info.channel.provider_id.to_string());
        raw.push('|');
        raw.push_str(&info.channel.virtual_id.to_string());
        raw.push('|');
        raw.push_str(&info.channel.item_type.to_string());
        Some(encode_base64_hash(&raw))
    }

    /// Build a common base from a `StreamInfo`, leaving event-specific fields to the caller.
    /// `event_ts` is the authoritative timestamp — `partition_day_utc` is derived from it
    /// so both fields are always consistent.
    fn base(info: &StreamInfo, event_ts: u64) -> Self {
        Self {
            schema_version: RECORD_SCHEMA_VERSION,
            event_type: EventType::Connect, // overridden by callers
            event_ts_utc: event_ts,
            partition_day_utc: utc_day_from_secs(event_ts),
            // Combine connect-timestamp (upper 32 bits) with uid (lower 32 bits).
            // This prevents session_id collision across server restarts and uid wrap-around.
            // Both Connect and Disconnect derive this from the same StreamInfo, so they match.
            session_id: (info.ts << 32) | u64::from(info.uid),
            source_addr: Some(info.client_ip.clone()),
            api_username: Some(info.username.clone()),
            provider_name: Some(info.provider.clone()),
            provider_username: None,
            input_name: if info.channel.input_name.is_empty() {
                None
            } else {
                Some(info.channel.input_name.to_string())
            },
            virtual_id: Some(info.channel.virtual_id),
            item_type: Some(info.channel.item_type.to_string()),
            title: Some(info.channel.title.to_string()),
            group: Some(info.channel.group.to_string()),
            country: info.country_code.clone(),
            user_agent: if info.user_agent.is_empty() { None } else { Some(info.user_agent.clone()) },
            shared: Some(info.channel.shared),
            shared_joined_existing: info.channel.shared_joined_existing,
            shared_stream_id: info.channel.shared_stream_id,
            provider_id: Some(info.channel.provider_id),
            cluster: Some(info.channel.cluster.to_string()),
            container: info.channel.technical.as_ref().and_then(|t| if t.container.is_empty() { None } else { Some(t.container.clone()) }),
            stream_url_hash: if info.channel.url.is_empty() {
                None
            } else {
                Some(encode_base64_hash(info.channel.url.as_ref()))
            },
            stream_identity_key: Self::build_stream_identity_key(info),
            video_codec: info.channel.technical.as_ref().and_then(|t| if t.video_codec.is_empty() { None } else { Some(t.video_codec.clone()) }),
            audio_codec: info.channel.technical.as_ref().and_then(|t| if t.audio_codec.is_empty() { None } else { Some(t.audio_codec.clone()) }),
            audio_channels: info.channel.technical.as_ref().and_then(|t| if t.audio_channels.is_empty() { None } else { Some(t.audio_channels.clone()) }),
            resolution: info.channel.technical.as_ref().and_then(|t| if t.resolution.is_empty() { None } else { Some(t.resolution.clone()) }),
            fps: info.channel.technical.as_ref().and_then(|t| if t.fps.is_empty() { None } else { Some(t.fps.clone()) }),
            connect_ts_utc: None,
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            first_byte_latency_ms: None,
            provider_reconnect_count: None,
            failure_stage: None,
            provider_http_status: None,
            provider_error_class: None,
            connect_failure_reason: None,
            disconnect_reason: None,
            previous_session_id: None,
            target_id: Some(info.channel.target_id),
        }
    }

    pub fn from_connect(info: &StreamInfo) -> Self {
        let mut record = Self::base(info, info.ts);
        record.event_type = EventType::Connect;
        record.connect_ts_utc = Some(info.ts);
        record.previous_session_id = info.previous_session_id;
        record
    }

    pub fn from_connect_failed(
        info: &StreamInfo,
        reason: ConnectFailureReason,
        attempt_uid: u32,
        failure_stage: FailureStage,
    ) -> Self {
        let event_ts = now_utc_secs();
        let mut record = Self::base(info, event_ts);
        record.event_type = EventType::ConnectFailed;
        record.session_id = (event_ts << 32) | u64::from(attempt_uid);
        record.failure_stage = Some(failure_stage);
        record.connect_failure_reason = Some(reason);
        record
    }

    pub fn with_provider_failure(mut self, provider_http_status: Option<u16>, provider_error_class: Option<&str>) -> Self {
        self.provider_http_status = provider_http_status;
        self.provider_error_class = provider_error_class.map(ToString::to_string);
        self
    }

    /// Extra `QoS` fields carried as a struct to keep the signature stable.
    pub fn from_disconnect(
        info: &StreamInfo,
        reason: DisconnectReason,
        qos: &DisconnectQos,
        failure_stage: Option<FailureStage>,
    ) -> Self {
        let now_secs = now_utc_secs();
        let connect_secs = info.ts;
        let mut record = Self::base(info, now_secs);
        record.event_type = EventType::Disconnect;
        record.connect_ts_utc = Some(connect_secs);
        record.disconnect_ts_utc = Some(now_secs);
        record.session_duration = Some(now_secs.saturating_sub(connect_secs));
        record.bytes_sent = qos.bytes_sent;
        record.first_byte_latency_ms = qos.first_byte_latency_ms;
        record.provider_reconnect_count = qos.provider_reconnect_count;
        record.failure_stage = failure_stage;
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
    use shared::{
        model::{PlaylistItemType, StreamChannel, StreamInfo, StreamTechnicalInfo, XtreamCluster},
        utils::Internable,
    };
    use std::net::SocketAddr;
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
            input_name: Some("provider-input".to_string()),
            virtual_id: Some(1234),
            item_type: Some("live".to_string()),
            title: Some("News Channel".to_string()),
            group: Some("News".to_string()),
            country: Some("DE".to_string()),
            user_agent: Some("VLC/3.0".to_string()),
            shared: Some(false),
            shared_joined_existing: None,
            shared_stream_id: None,
            provider_id: Some(1),
            cluster: Some("live".to_string()),
            container: Some("mpegts".to_string()),
            stream_url_hash: Some("abc123".to_string()),
            stream_identity_key: Some("identity123".to_string()),
            video_codec: Some("H.264".to_string()),
            audio_codec: Some("AAC".to_string()),
            audio_channels: Some("STEREO".to_string()),
            resolution: Some("1920x1080".to_string()),
            fps: Some("50".to_string()),
            connect_ts_utc: Some(1_742_600_001),
            disconnect_ts_utc: None,
            session_duration: None,
            bytes_sent: None,
            first_byte_latency_ms: None,
            provider_reconnect_count: None,
            failure_stage: None,
            provider_http_status: None,
            provider_error_class: None,
            connect_failure_reason: None,
            disconnect_reason: None,
            previous_session_id: None,
            target_id: Some(1),
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
            input_name: Some("provider-input".to_string()),
            virtual_id: Some(1234),
            item_type: Some("live".to_string()),
            title: Some("News Channel".to_string()),
            group: Some("News".to_string()),
            country: Some("DE".to_string()),
            user_agent: Some("VLC/3.0".to_string()),
            shared: Some(false),
            shared_joined_existing: None,
            shared_stream_id: None,
            provider_id: Some(1),
            cluster: Some("live".to_string()),
            container: Some("mpegts".to_string()),
            stream_url_hash: Some("abc123".to_string()),
            stream_identity_key: Some("identity123".to_string()),
            video_codec: Some("H.264".to_string()),
            audio_codec: Some("AAC".to_string()),
            audio_channels: Some("STEREO".to_string()),
            resolution: Some("1920x1080".to_string()),
            fps: Some("50".to_string()),
            connect_ts_utc: Some(1_742_600_001),
            disconnect_ts_utc: Some(1_742_603_601),
            session_duration: Some(3600),
            bytes_sent: Some(1_234_567_890),
            first_byte_latency_ms: Some(150),
            provider_reconnect_count: Some(0),
            failure_stage: None,
            provider_http_status: None,
            provider_error_class: None,
            connect_failure_reason: None,
            disconnect_reason: Some(DisconnectReason::ClientClosed),
            previous_session_id: None,
            target_id: Some(1),
        }
    }

    fn sample_stream_info() -> StreamInfo {
        let addr: SocketAddr = "192.0.2.1:12345".parse().unwrap();
        let mut info = StreamInfo::new(
            999,
            1001,
            "alice",
            &addr,
            "192.0.2.1",
            "acme-tv",
            StreamChannel {
                target_id: 1,
                virtual_id: 1234,
                provider_id: 1,
                input_name: "provider-input".intern(),
                item_type: PlaylistItemType::Live,
                cluster: XtreamCluster::Live,
                group: "News".intern(),
                title: "News Channel".intern(),
                url: "http://localhost/stream.ts".intern(),
                shared: false,
                shared_joined_existing: None,
                shared_stream_id: None,
                technical: Some(StreamTechnicalInfo {
                    container: "mpegts".to_string(),
                    resolution: "1920x1080".to_string(),
                    fps: "50".to_string(),
                    video_codec: "H.264".to_string(),
                    audio_codec: "AAC".to_string(),
                    audio_channels: "STEREO".to_string(),
                }),
            },
            String::from("VLC/3.0"),
            Some(String::from("DE")),
            None,
        );
        info.ts = 1_742_600_001;
        info
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
    fn stream_history_from_connect_uses_second_precision_session_times() {
        let info = sample_stream_info();
        let record = StreamHistoryRecord::from_connect(&info);

        assert_eq!(record.connect_ts_utc, Some(info.ts));
        assert!(sample_connect_record().connect_ts_utc == Some(info.ts));
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
        assert_eq!(
            record.stream_url_hash.as_deref(),
            Some(encode_base64_hash("http://localhost/stream.ts").as_str())
        );
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
        );

        assert_eq!(record.event_type, EventType::ConnectFailed);
        assert_eq!(
            record.connect_failure_reason,
            Some(ConnectFailureReason::ProviderConnectionsExhausted)
        );
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
}
