use crate::{model::PlaylistItemType, utils::arc_str_option_serde};
use serde::{Deserialize, Serialize};
use std::{
    fmt::{Display, Formatter},
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamHistoryEventType {
    Connect,
    ConnectFailed,
    Disconnect,
    Failure,
}

impl Display for StreamHistoryEventType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamHistoryEventType::Connect => write!(f, "connect"),
            StreamHistoryEventType::ConnectFailed => write!(f, "connect_failed"),
            StreamHistoryEventType::Disconnect => write!(f, "disconnect"),
            StreamHistoryEventType::Failure => write!(f, "failure"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
    IntermediateFailures(usize),
}

impl Display for DisconnectReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DisconnectReason::Cleanup => write!(f, "cleanup"),
            DisconnectReason::ClientClosed => write!(f, "client_closed"),
            DisconnectReason::ClientKicked => write!(f, "client_kicked"),
            DisconnectReason::Provisioning => write!(f, "provisioning"),
            DisconnectReason::ServerError => write!(f, "server_error"),
            DisconnectReason::Timeout => write!(f, "timeout"),
            DisconnectReason::DayRollover => write!(f, "day_rollover"),
            DisconnectReason::Shutdown => write!(f, "shutdown"),
            DisconnectReason::Unknown => write!(f, "unknown"),
            DisconnectReason::ProviderError => write!(f, "provider_error"),
            DisconnectReason::ProviderClosed => write!(f, "provider_closed"),
            DisconnectReason::Preempted => write!(f, "preempted"),
            DisconnectReason::SessionExpired => write!(f, "session_expired"),
            DisconnectReason::UserConnectionsExhausted => write!(f, "user_connections_exhausted"),
            DisconnectReason::ProviderConnectionsExhausted => write!(f, "provider_connections_exhausted"),
            DisconnectReason::IntermediateFailures(count) => write!(f, "intermediate_failures({count})"),
        }
    }
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

impl Display for ConnectFailureReason {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectFailureReason::UserAccountExpired => write!(f, "user_account_expired"),
            ConnectFailureReason::UserConnectionsExhausted => write!(f, "user_connections_exhausted"),
            ConnectFailureReason::ProviderConnectionsExhausted => write!(f, "provider_connections_exhausted"),
            ConnectFailureReason::ProviderError => write!(f, "provider_error"),
            ConnectFailureReason::ProviderClosed => write!(f, "provider_closed"),
            ConnectFailureReason::ChannelUnavailable => write!(f, "channel_unavailable"),
            ConnectFailureReason::Preempted => write!(f, "preempted"),
            ConnectFailureReason::SessionExpired => write!(f, "session_expired"),
            ConnectFailureReason::Provisioning => write!(f, "provisioning"),
        }
    }
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

impl Display for FailureStage {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FailureStage::Admission => write!(f, "admission"),
            FailureStage::ProviderOpen => write!(f, "provider open"),
            FailureStage::FirstByte => write!(f, "first byte"),
            FailureStage::Streaming => write!(f, "streaming"),
            FailureStage::SessionReconnect => write!(f, "session reconnect"),
        }
    }
}

/// DTO for stream history records exchanged between backend and frontend.
/// Uses string-based enum variants for JSON compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct StreamHistoryRecordDto {
    pub event_type: StreamHistoryEventType,
    pub event_ts_utc: u64,
    pub partition_day_utc: String,
    pub session_id: u64,
    pub source_addr: Option<String>,
    pub api_username: Option<String>,
    #[serde(default, with = "arc_str_option_serde")]
    pub provider_name: Option<Arc<str>>,
    #[serde(default, with = "arc_str_option_serde")]
    pub input_name: Option<Arc<str>>,
    pub virtual_id: Option<u32>,
    pub item_type: Option<PlaylistItemType>,
    pub title: Option<String>,
    pub group: Option<String>,
    pub country: Option<String>,
    pub user_agent: Option<String>,
    pub shared: Option<bool>,
    pub provider_id: Option<u32>,
    pub cluster: Option<String>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub resolution: Option<String>,
    pub failure_stage: Option<FailureStage>,
    pub connect_failure_reason: Option<ConnectFailureReason>,
    pub disconnect_reason: Option<DisconnectReason>,
    pub session_duration: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub first_byte_latency_ms: Option<u64>,
    pub previous_session_id: Option<u64>,
    #[serde(default, with = "arc_str_option_serde")]
    pub target_name: Option<Arc<str>>,
}
