use crate::{
    services::{get_base_href, request_get, Encoding},
    utils::encoding_for_query,
};
use serde::Deserialize;
use shared::{
    model::{PagedResponseDto, StreamHistoryRecordDto},
    utils::concat_path_leading_slash,
};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StreamHistoryProviderSummary {
    pub provider_name: String,
    pub session_count: u64,
    pub disconnect_count: u64,
    pub total_bytes_sent: u64,
    pub avg_session_duration_secs: Option<u64>,
    pub avg_first_byte_latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StreamHistoryQosSnapshotWindow {
    pub connect_count: u64,
    pub connect_failed_count: u64,
    pub runtime_abort_count: u64,
    pub provider_closed_count: u64,
    pub avg_first_byte_latency_ms: Option<u64>,
    pub avg_session_duration_secs: Option<u64>,
    pub last_success_ts: Option<u64>,
    pub last_failure_ts: Option<u64>,
    pub score: u8,
    pub confidence: u8,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct StreamHistoryQosSnapshot {
    pub stream_identity_key: String,
    pub input_name: String,
    pub target_name: String,
    pub provider_name: String,
    pub provider_id: u32,
    pub virtual_id: u32,
    pub item_type: String,
    pub updated_at: Option<u64>,
    pub last_event_at: Option<u64>,
    pub window_24h: StreamHistoryQosSnapshotWindow,
    pub window_7d: StreamHistoryQosSnapshotWindow,
    pub window_30d: StreamHistoryQosSnapshotWindow,
}

pub struct StreamHistoryService {
    path: String,
    qos_path: String,
}

impl Default for StreamHistoryService {
    fn default() -> Self { Self::new() }
}

impl StreamHistoryService {
    pub fn new() -> Self {
        let base_href = get_base_href();
        let api = |endpoint: &str| concat_path_leading_slash(&base_href, &format!("api/v1/{endpoint}"));
        Self { path: api("stream-history"), qos_path: api("qos-snapshots") }
    }

    pub async fn get_history(
        &self,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Option<Vec<StreamHistoryRecordDto>>, crate::error::Error> {
        let url = match (from, to) {
            (Some(f), Some(t)) => format!("{}?from={}&to={}", self.path, f, t),
            (Some(f), None) => format!("{}?from={}", self.path, f),
            (None, Some(t)) => format!("{}?to={}", self.path, t),
            (None, None) => self.path.clone(),
        };
        request_get::<Vec<StreamHistoryRecordDto>>(&url, None, Some(Encoding::Cbor)).await
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

    pub async fn get_qos_snapshots(&self) -> Result<Option<Vec<StreamHistoryQosSnapshot>>, crate::error::Error> {
        request_get::<Vec<StreamHistoryQosSnapshot>>(&self.qos_path, None, Some(Encoding::Cbor)).await
    }

    pub async fn get_qos_snapshot_detail(
        &self,
        stream_identity_key: &str,
    ) -> Result<Option<StreamHistoryQosSnapshot>, crate::error::Error> {
        let path = format!("{}/{}", self.qos_path, stream_identity_key);
        request_get::<StreamHistoryQosSnapshot>(&path, None, Some(Encoding::Cbor)).await
    }

    pub async fn get_history_page(
        &self,
        time_range: Option<(&str, &str)>,
        page: u32,
        page_size: u16,
        search: Option<&str>,
        search_mode: Option<&str>,
        search_fields: Option<&[String]>,
    ) -> Result<Option<PagedResponseDto<StreamHistoryRecordDto>>, crate::error::Error> {
        let page_path = &self.path;
        let mut params: Vec<(String, String)> = Vec::new();

        // Base parameters
        if let Some((f, t)) = time_range {
            params.push(("from".to_string(), f.to_string()));
            params.push(("to".to_string(), t.to_string()));
        }
        params.push(("page".to_string(), page.to_string()));
        params.push(("page_size".to_string(), page_size.to_string()));

        // Search parameters
        if let Some(s) = search {
            params.push(("search".to_string(), s.to_string()));
        }
        if let Some(mode) = search_mode {
            params.push(("search_mode".to_string(), mode.to_string()));
        }
        if let Some(fields) = search_fields {
            for field in fields {
                params.push(("search_field".to_string(), field.clone()));
            }
        }

        // Build URL with proper encoding
        let mut url = format!("{page_path}?");
        for (i, (key, value)) in params.iter().enumerate() {
            // Encode key and value manually
            let enc_key = encoding_for_query(key);
            let enc_value = encoding_for_query(value);
            if i > 0 {
                url.push('&');
            }
            url.push_str(&format!("{enc_key}={enc_value}"));
        }

        request_get::<PagedResponseDto<StreamHistoryRecordDto>>(&url, None, Some(Encoding::Cbor)).await
    }
}
