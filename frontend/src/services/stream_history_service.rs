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
    pub disconnect_reason: Option<String>,
    pub session_duration: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub first_byte_latency_ms: Option<u64>,
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
}
