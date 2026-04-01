use crate::services::{get_base_href, request_get};
use serde::Deserialize;
use shared::utils::concat_path_leading_slash;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StreamHistoryRecord {
    pub event_type: String,
    pub event_ts_utc: u64,
    pub partition_day_utc: String,
    pub session_id: u64,
    pub source_addr: Option<String>,
    pub api_username: Option<String>,
    pub provider_name: Option<String>,
    pub virtual_id: Option<u32>,
    pub item_type: Option<String>,
    pub title: Option<String>,
    pub group: Option<String>,
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
    pub target_id: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StreamHistoryProviderSummary {
    pub provider_name: String,
    pub session_count: u64,
    pub disconnect_count: u64,
    pub total_bytes_sent: u64,
    pub avg_session_duration_secs: Option<u64>,
    pub avg_first_byte_latency_ms: Option<u64>,
}

pub struct StreamHistoryService {
    path: String,
}

impl Default for StreamHistoryService {
    fn default() -> Self { Self::new() }
}

impl StreamHistoryService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        Self { path: concat_path_leading_slash(&base_href, "api/v1/stream-history") }
    }

    pub async fn get_history(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Option<Vec<StreamHistoryRecord>>, crate::error::Error> {
        let url = match (from, to) {
            (Some(f), Some(t)) => format!("{}?from={}&to={}", self.path, f, t),
            (Some(f), None) => format!("{}?from={}", self.path, f),
            (None, Some(t)) => format!("{}?to={}", self.path, t),
            (None, None) => self.path.clone(),
        };
        request_get::<Vec<StreamHistoryRecord>>(&url, None, None).await
    }

    pub async fn get_summary(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Option<Vec<StreamHistoryProviderSummary>>, crate::error::Error> {
        let summary_path = format!("{}/summary", self.path);
        let url = match (from, to) {
            (Some(f), Some(t)) => format!("{}?from={}&to={}", summary_path, f, t),
            (Some(f), None) => format!("{}?from={}", summary_path, f),
            (None, Some(t)) => format!("{}?to={}", summary_path, t),
            (None, None) => summary_path,
        };
        request_get::<Vec<StreamHistoryProviderSummary>>(&url, None, None).await
    }
}
