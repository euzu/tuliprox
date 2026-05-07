use crate::{
    model::PlaylistItemType,
    utils::{default_page, default_page_size},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

mod playlist_item_type_serde {
    use crate::model::PlaylistItemType;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &PlaylistItemType, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PlaylistItemType, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "live" | "Live" => Ok(PlaylistItemType::Live),
            "video" | "Video" => Ok(PlaylistItemType::Video),
            "series" | "Series" => Ok(PlaylistItemType::Series),
            "series-info" | "series_info" | "SeriesInfo" => Ok(PlaylistItemType::SeriesInfo),
            "catchup" | "Catchup" => Ok(PlaylistItemType::Catchup),
            "live_unknown" | "LiveUnknown" => Ok(PlaylistItemType::LiveUnknown),
            "live_hls" | "LiveHls" => Ok(PlaylistItemType::LiveHls),
            "live_dash" | "LiveDash" => Ok(PlaylistItemType::LiveDash),
            "local_video" | "LocalVideo" => Ok(PlaylistItemType::LocalVideo),
            "local_series" | "LocalSeries" => Ok(PlaylistItemType::LocalSeries),
            "local_series_info" | "LocalSeriesInfo" => Ok(PlaylistItemType::LocalSeriesInfo),
            _ => Err(D::Error::custom(format!("Invalid PlaylistItemType: {value}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHistoryQueryRequestDto {
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default)]
    #[serde(flatten)]
    pub filter: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHistoryPageRequestDto {
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default = "default_page")]
    pub page: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u16,
    pub search: Option<String>,
    pub search_mode: Option<String>,
    #[serde(rename = "search_field")]
    pub search_fields: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamHistoryProviderSummaryDto {
    pub provider_name: String,
    pub session_count: u64,
    pub disconnect_count: u64,
    pub total_bytes_sent: u64,
    pub avg_session_duration_secs: Option<u64>,
    pub avg_first_byte_latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosSnapshotWindowDto {
    pub connect_count: u64,
    pub connect_failed_count: u64,
    pub startup_capacity_failure_count: u64,
    pub provider_open_failure_count: u64,
    pub first_byte_failure_count: u64,
    pub runtime_abort_count: u64,
    pub provider_closed_count: u64,
    pub preempt_count: u64,
    pub avg_first_byte_latency_ms: Option<u64>,
    pub avg_session_duration_secs: Option<u64>,
    pub avg_provider_reconnect_count: Option<u64>,
    pub last_success_ts: Option<u64>,
    pub last_failure_ts: Option<u64>,
    pub successive_failure_streak: u32,
    pub sample_size: u64,
    pub score: u8,
    pub confidence: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosSnapshotDailyBucketDto {
    pub connect_count: u64,
    pub connect_failed_count: u64,
    pub startup_capacity_failure_count: u64,
    pub provider_open_failure_count: u64,
    pub first_byte_failure_count: u64,
    pub runtime_abort_count: u64,
    pub provider_closed_count: u64,
    pub preempt_count: u64,
    pub total_first_byte_latency_ms: u64,
    pub total_first_byte_latency_samples: u64,
    pub total_session_duration_secs: u64,
    pub total_session_duration_samples: u64,
    pub total_provider_reconnect_count: u64,
    pub total_provider_reconnect_samples: u64,
    pub last_success_ts: Option<u64>,
    pub last_failure_ts: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QosSnapshotRecordDto {
    pub stream_identity_key: String,
    pub input_name: String,
    pub target_name: String,
    pub provider_name: String,
    pub provider_id: u32,
    pub virtual_id: u32,
    #[serde(with = "playlist_item_type_serde")]
    pub item_type: PlaylistItemType,
    pub updated_at: Option<u64>,
    pub last_event_at: Option<u64>,
    pub window_24h: QosSnapshotWindowDto,
    pub window_7d: QosSnapshotWindowDto,
    pub window_30d: QosSnapshotWindowDto,
    #[serde(default)]
    pub daily_buckets: BTreeMap<String, QosSnapshotDailyBucketDto>,
}
