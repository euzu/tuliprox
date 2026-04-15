use crate::utils::arc_str_option_serde;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use shared::model::{StreamHistoryRecordDto, StreamInfo};
use shared::utils::Internable;
use crate::utils::{encode_base64_hash, now_utc_secs, utc_day_from_secs};

pub const RECORD_SCHEMA_VERSION: u8 = 1;

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
    Cleanup,
    ClientClosed,
    ClientKicked,
    Provisioning,
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
    #[serde(default, with = "arc_str_option_serde")]
    pub provider_name: Option<Arc<str>>,
    /// Provider-side username (e.g. Xtream account). Always `None` until `StreamInfo`
    /// carries the provider credential — at which point populate from there.
    pub provider_username: Option<String>,
    #[serde(default, with = "arc_str_option_serde")]
    pub input_name: Option<Arc<str>>,
    // Stream metadata
    pub virtual_id: Option<u32>,
    #[serde(default, with = "arc_str_option_serde")]
    pub item_type: Option<Arc<str>>,
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
    /// Target config name — stable identifier, does not change across restarts.
    #[serde(default, with = "arc_str_option_serde")]
    pub target_name: Option<Arc<str>>,
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
                Some(info.channel.input_name.clone())
            },
            virtual_id: Some(info.channel.virtual_id),
            item_type: Some(info.channel.item_type.to_string().intern()),
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
            target_name: None,
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
        target_name: Option<Arc<str>>,
    ) -> Self {
        let event_ts = now_utc_secs();
        let mut record = Self::base(info, event_ts);
        record.event_type = EventType::ConnectFailed;
        record.session_id = (event_ts << 32) | u64::from(attempt_uid);
        record.failure_stage = Some(failure_stage);
        record.connect_failure_reason = Some(reason);
        record.target_name = target_name;
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

impl From<&StreamHistoryRecord> for StreamHistoryRecordDto {
    fn from(record: &StreamHistoryRecord) -> Self {
        Self {
            event_type: match record.event_type {
                EventType::Connect => "connect".to_string(),
                EventType::ConnectFailed => "connect_failed".to_string(),
                EventType::Disconnect => "disconnect".to_string(),
            },
            event_ts_utc: record.event_ts_utc,
            partition_day_utc: record.partition_day_utc.clone(),
            session_id: record.session_id,
            source_addr: record.source_addr.clone(),
            api_username: record.api_username.clone(),
            provider_name: record.provider_name.clone(),
            input_name: record.input_name.clone(),
            virtual_id: record.virtual_id,
            item_type: record.item_type.clone(),
            title: record.title.clone(),
            group: record.group.clone(),
            country: record.country.clone(),
            user_agent: record.user_agent.clone(),
            shared: record.shared,
            provider_id: record.provider_id,
            cluster: record.cluster.clone(),
            container: record.container.clone(),
            video_codec: record.video_codec.clone(),
            audio_codec: record.audio_codec.clone(),
            resolution: record.resolution.clone(),
            disconnect_reason: record.disconnect_reason.as_ref().map(|r| match r {
                DisconnectReason::Cleanup => "cleanup".to_string(),
                DisconnectReason::ClientClosed => "client_closed".to_string(),
                DisconnectReason::ClientKicked => "client_kicked".to_string(),
                DisconnectReason::Provisioning => "provisioning".to_string(),
                DisconnectReason::ServerError => "server_error".to_string(),
                DisconnectReason::Timeout => "timeout".to_string(),
                DisconnectReason::DayRollover => "day_rollover".to_string(),
                DisconnectReason::Shutdown => "shutdown".to_string(),
                DisconnectReason::Unknown => "unknown".to_string(),
                DisconnectReason::ProviderError => "provider_error".to_string(),
                DisconnectReason::ProviderClosed => "provider_closed".to_string(),
                DisconnectReason::Preempted => "preempted".to_string(),
                DisconnectReason::SessionExpired => "session_expired".to_string(),
                DisconnectReason::UserConnectionsExhausted => "user_connections_exhausted".to_string(),
                DisconnectReason::ProviderConnectionsExhausted => "provider_connections_exhausted".to_string(),
            }),
            session_duration: record.session_duration,
            bytes_sent: record.bytes_sent,
            first_byte_latency_ms: record.first_byte_latency_ms,
            previous_session_id: record.previous_session_id,
            target_name: record.target_name.clone(),
        }
    }
}