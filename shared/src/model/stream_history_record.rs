use crate::utils::arc_str_option_serde;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// DTO for stream history records exchanged between backend and frontend.
/// Uses string-based enum variants for JSON compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct StreamHistoryRecordDto {
    pub event_type: String,
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
    #[serde(default, with = "arc_str_option_serde")]
    pub item_type: Option<Arc<str>>,
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
    pub disconnect_reason: Option<String>,
    pub session_duration: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub first_byte_latency_ms: Option<u64>,
    pub previous_session_id: Option<u64>,
    #[serde(default, with = "arc_str_option_serde")]
    pub target_name: Option<Arc<str>>,
}
